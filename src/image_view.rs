//! Interactive image viewer with pixel-exact rendering, zoom (mouse wheel at
//! the pointer, + / -), panning (drag, hjkl) and animated GIF/WebP/APNG.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::app::{is_quit, pos_label, read_events, Ctx, Nav, CHROME_ROWS};
use crate::diacritics::DIACRITICS;
use crate::kitty::{next_id, placeholder_row_at, Z_BELOW_BG};
use crate::raster::{render_svg, resize, resize_region, Filter, Picture, Raster};
use crate::term::WinSize;
use crate::ui::{help_screen, hint_bar, status_bar};

const HELP: &[(&str, &str)] = &[
    ("wheel / + / -", "zoom (at pointer)"),
    ("drag / h j k l", "pan"),
    ("0 / f", "fit to window"),
    ("1", "actual pixels (100%)"),
    ("r / R", "rotate ↻ / ↺"),
    ("b", "background: transparent / white / black"),
    ("space", "next / pause animation"),
    ("n / p / → / ←", "next / previous file"),
    ("q / Esc", "quit"),
];

const HINTS: &[(&str, &str)] = &[
    ("wheel/+-", "zoom"),
    ("drag/hjkl", "pan"),
    ("0", "fit"),
    ("1", "100%"),
    ("r", "rotate"),
    ("n/p", "next/prev"),
];

/// Background drawn behind transparent pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Background {
    Transparent,
    White,
    Black,
}

impl Background {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Background::White,
            2 => Background::Black,
            _ => Background::Transparent,
        }
    }

    fn next(self) -> Self {
        match self {
            Background::Transparent => Background::White,
            Background::White => Background::Black,
            Background::Black => Background::Transparent,
        }
    }

    fn color(self) -> Option<[u8; 3]> {
        match self {
            Background::Transparent => None,
            Background::White => Some([255, 255, 255]),
            Background::Black => Some([0, 0, 0]),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Background::Transparent => "transparent",
            Background::White => "white",
            Background::Black => "black",
        }
    }
}

/// The chosen background, kept while browsing to the next / previous file.
static BACKGROUND: AtomicU8 = AtomicU8::new(0);

/// Over ssh, how long input must pause before a sharp frame is sent.
const REMOTE_SETTLE: Duration = Duration::from_millis(300);
/// The same without a network (local tmux): the sharp frame (~15 ms to
/// encode) can follow almost at once.
const LOCAL_SETTLE: Duration = Duration::from_millis(60);
/// Sharp frames over ssh are written in pieces of at least this size, checking
/// for user input in between so a new zoom / drag can cancel the frame.
const INTERRUPT_PIECE: usize = 32 * 1024;
/// Largest proxy image uploaded once for inline zooming / panning, in pixels.
const PROXY_MAX_PIXELS: f64 = 6.0e6;
/// Largest regional proxy (a crop around the view, used under tmux when the
/// zoomed picture is too large to address with placeholder cells), in pixels.
const REGION_MAX_PIXELS: f64 = 3.0e6;
/// Highest row / column index a placeholder cell can name explicitly.
const MAX_INDEX: f64 = DIACRITICS.len() as f64 - 1.0;

/// A downscaled copy of the picture kept in the terminal. Over ssh, zooming
/// and panning only re-place it instead of re-sending pixels; a sharp frame
/// follows once input pauses. With direct placements the terminal crops it
/// with a source rectangle; with Unicode placeholders (tmux) it is a virtual
/// placement spanning the whole zoomed image and only visible cells are printed.
struct Proxy {
    id: u32,
    w: u32,
    h: u32,
    bg: Background,
    /// Cell grid (columns, rows) of the current virtual placement.
    grid: Option<(u32, u32)>,
    /// Part of the picture it shows (x, y, w, h in picture pixels).
    region: (f64, f64, f64, f64),
}

/// Where the visible window falls in a proxy's placeholder grid.
/// A rectangle (x, y, w, h) and an (x, y) pair, in pixels.
type Rect = (f64, f64, f64, f64);
type Pair = (f64, f64);

struct GridLayout {
    /// Grid size in cells (columns, rows).
    grid: (u32, u32),
    /// First visible grid column / row, number of visible ones, and the
    /// screen offset (in cells) of the window.
    sc: u32,
    vc: u32,
    col0: u32,
    sr: u32,
    vr: u32,
    row0: u32,
    /// View centre snapped to the cell grid.
    cx: f64,
    cy: f64,
}

/// User input that should cancel a sharp frame in flight (not plain pointer
/// motion or key releases).
fn is_input(ev: &Event) -> bool {
    match ev {
        Event::Mouse(m) => m.kind != MouseEventKind::Moved,
        Event::Key(k) => k.kind != crossterm::event::KeyEventKind::Release,
        Event::Resize(..) => true,
        _ => false,
    }
}

/// End of the next complete escape sequence (string terminator `ESC \`) at
/// or after `from`. A terminator preceded by another ESC is an escaped one
/// inside a tmux passthrough wrapper, not the end of a sequence.
fn next_boundary(data: &[u8], from: usize) -> usize {
    let mut i = from.max(1);
    while i + 1 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'\\' && data[i - 1] != 0x1b {
            return i + 2;
        }
        i += 1;
    }
    data.len()
}

/// Write `data` in pieces cut at escape-sequence boundaries, stopping early
/// when user input arrives. Returns whether it stopped early and the events
/// read meanwhile (to be handled by the caller).
fn write_interruptible(data: &[u8]) -> (bool, Vec<Event>) {
    use std::io::Write;
    let mut evs = Vec::new();
    let mut pos = 0;
    let mut stdout = std::io::stdout().lock();
    while pos < data.len() {
        let end = next_boundary(data, pos + INTERRUPT_PIECE);
        if stdout.write_all(&data[pos..end]).is_err() || stdout.flush().is_err() {
            break;
        }
        pos = end;
        while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
            match crossterm::event::read() {
                Ok(Event::Key(k)) if k.kind == crossterm::event::KeyEventKind::Release => {}
                Ok(ev) => evs.push(ev),
                Err(_) => break,
            }
        }
        if pos < data.len() && evs.iter().any(is_input) {
            return (true, evs);
        }
    }
    (false, evs)
}

/// Placeholder text for the image area (`area_rows` x `cols` cells): screen
/// rows `row0..row0 + vr` show grid rows from `sr` and grid columns from `sc`
/// (`vc` cells) starting at screen column `col0`. Everything is overwritten in
/// place and only uncovered margins / rows are erased, never the whole screen
/// first: tmux redraws as it reads, and would show the empty screen (flicker).
fn placeholder_frame(
    id: u32,
    area_rows: u32,
    cols: u32,
    (row0, col0): (u32, u32),
    (sr, sc): (u32, u32),
    (vr, vc): (u32, u32),
) -> String {
    let mut s = String::with_capacity((vr * vc * 4 + area_rows * 16) as usize);
    for r in 0..area_rows {
        if r >= row0 && r < row0 + vr {
            s.push_str(&format!("\x1b[{};1H\x1b[0m", r + 1));
            if col0 > 0 {
                s.push_str(&format!("\x1b[{col0}X"));
            }
            s.push_str(&format!(
                "\x1b[{};{}H{}\x1b[0m",
                r + 1,
                col0 + 1,
                placeholder_row_at(id, sr + r - row0, sc, vc)
            ));
            // After the last column the cursor stays on it: erasing from
            // there would wipe the last cell, so only erase a real margin.
            if col0 + vc < cols {
                s.push_str("\x1b[K");
            }
        } else {
            s.push_str(&format!("\x1b[{};1H\x1b[0m\x1b[2K", r + 1));
        }
    }
    s
}

/// Only the first placeholder cell of each row: it names the row and the
/// first column; the rest of the row continues from it. When just the grid
/// offset changes (panning, zooming a picture larger than the window), this is
/// all that needs rewriting (~3 KB instead of the whole ~50 KB grid), which tmux
/// passes on in one piece, so the terminal never shows a half-updated frame.
fn placeholder_heads(id: u32, (row0, col0): (u32, u32), (sr, sc): (u32, u32), vr: u32) -> String {
    let mut s = String::from("\x1b[0m");
    for r in 0..vr {
        s.push_str(&format!(
            "\x1b[{};{}H{}",
            row0 + r + 1,
            col0 + 1,
            placeholder_row_at(id, sr + r, sc, 1)
        ));
    }
    s
}

/// How far the picture can be dragged past the point where it fills the
/// window (or, when smaller, past the centre), as a fraction of the window.
const OVERSCROLL: f64 = 0.2;

/// Clamp the view centre on one axis: a picture `len` long seen through a
/// window `win` long (both in source pixels).
fn clamp_centre(c: f64, len: f64, win: f64) -> f64 {
    let (lo, hi) = if len > win {
        (win / 2.0, len - win / 2.0)
    } else {
        (len / 2.0, len / 2.0)
    };
    let m = OVERSCROLL * win;
    c.clamp(lo - m, hi + m)
}

/// Visible part of a `len` long axis through a `win` long window centred on
/// `c`: (start, end, offset of the start from the window's start).
fn visible_span(c: f64, len: f64, win: f64) -> (f64, f64, f64) {
    let w0 = c - win / 2.0;
    let a = w0.max(0.0);
    let b = (w0 + win).min(len).max(a + 1e-6);
    (a, b, a - w0)
}

/// Visible part of a `grid`-cell long axis in `avail` cells around `center`:
/// (first grid cell, number of cells, screen offset in cells, window start in
/// grid cells, which is negative / past the end when dragged beyond an edge).
fn grid_window(grid: f64, avail: f64, center: f64) -> (u32, u32, u32, f64) {
    let start = (center - avail / 2.0).round();
    let first = start.clamp(0.0, grid);
    let end = (start + avail).clamp(first, grid);
    (
        first as u32,
        (end - first) as u32,
        (first - start) as u32,
        start,
    )
}

/// Delay before re-sending a placeholder frame's placement, and how often.
const NUDGE_DELAY: Duration = Duration::from_millis(150);
const NUDGES: u8 = 2;

/// Zoom factor per mouse-wheel notch / key press.
const WHEEL_STEP: f64 = 1.15;
const KEY_STEP: f64 = 1.25;

struct Viewer<'a> {
    ctx: &'a Ctx,
    pic: Picture,
    name: String,
    meta: String,
    pos: (usize, usize),
    zoom: f64,
    cx: f64,
    cy: f64,
    ids: [u32; 2],
    cur: usize,
    shown: bool,
    frame: usize,
    next_frame: Option<Instant>,
    paused: bool,
    last_render: Duration,
    hq_due: Option<Instant>,
    drag: Option<(u16, u16)>,
    help: bool,
    /// Downscaled copies (1/2, 1/4, …) of the first frame for fast zoomed-out rendering.
    mips: Vec<Raster>,
    /// The picture may contain transparent pixels (RGBA raster or SVG).
    transparent: bool,
    bg: Background,
    /// Pixels travel inline in the terminal stream (`Kitty::inline`).
    inline: bool,
    proxy: Option<Proxy>,
    /// The whole-picture proxy, kept in the terminal while a regional proxy
    /// is in use, so zooming back out needs no upload.
    spare: Option<Proxy>,
    /// The proxy placement is what is currently on screen.
    on_proxy: bool,
    /// Size and background of the shown sharp frame when it covers the whole
    /// picture, so it can be adopted as the proxy without another upload.
    shown_full: Option<(u32, u32, Background)>,
    /// Placeholder text on screen: (image id, screen row, screen column,
    /// rows, columns). Same layout again → only the row heads are rewritten.
    cells: Option<(u32, u32, u32, u32, u32)>,
    /// When to re-send the placement of a placeholder frame, and how many
    /// more times (see `nudge`).
    nudge_due: Option<(Instant, u8)>,
}

impl<'a> Viewer<'a> {
    fn dims(&self) -> (f64, f64) {
        (self.pic.width as f64, self.pic.height as f64)
    }

    fn area(&self, ws: WinSize) -> (f64, f64) {
        let (cw, ch) = ws.cell();
        (
            ws.cols as f64 * cw,
            ws.rows.saturating_sub(CHROME_ROWS).max(1) as f64 * ch,
        )
    }

    fn zoom_limits(&self, ws: WinSize) -> (f64, f64) {
        let min = self.fit(ws).min(1.0);
        let max: f64 = if self.pic.svg.is_some() { 400.0 } else { 64.0 };
        (min, max.max(min))
    }

    fn zoom_by(&mut self, ws: WinSize, factor: f64, anchor: Option<(f64, f64)>) {
        self.set_zoom(ws, self.zoom * factor, anchor);
    }

    /// Pick the smallest mip level that still has at least `z` resolution.
    fn source_for(&mut self, z: f64) -> (usize, f64) {
        if self.pic.is_animated() || self.pic.svg.is_some() {
            return (0, 1.0);
        }
        let mut level = 0;
        let mut scale = 1.0;
        while scale * 0.5 >= z * 1.05 {
            let prev = if level == 0 {
                &self.pic.frames[0].raster
            } else {
                &self.mips[level - 1]
            };
            if prev.w < 64 || prev.h < 64 {
                break;
            }
            if self.mips.len() < level + 1 {
                let half = resize_region(
                    prev,
                    (0.0, 0.0, prev.w as f64, prev.h as f64),
                    prev.w.div_ceil(2),
                    prev.h.div_ceil(2),
                    Filter::Bilinear,
                );
                self.mips.push(half);
            }
            level += 1;
            scale *= 0.5;
        }
        (level, scale)
    }

    fn fit(&self, ws: WinSize) -> f64 {
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        (aw / iw).min(ah / ih)
    }

    fn default_zoom(&self, ws: WinSize) -> f64 {
        let fit = self.fit(ws);
        if self.pic.svg.is_some() || fit < 1.0 {
            fit
        } else if fit >= 2.0 {
            fit.floor()
        } else {
            1.0
        }
    }

    fn reset(&mut self, ws: WinSize) {
        self.zoom = self.default_zoom(ws);
        let (iw, ih) = self.dims();
        self.cx = iw / 2.0;
        self.cy = ih / 2.0;
    }

    fn set_zoom(&mut self, ws: WinSize, z: f64, anchor: Option<(f64, f64)>) {
        let (min, max) = self.zoom_limits(ws);
        let z = z.clamp(min, max);
        let (aw, ah) = self.area(ws);
        // keep the source point under the anchor fixed
        let (dx, dy) = anchor
            .map(|(x, y)| (x - aw / 2.0, y - ah / 2.0))
            .unwrap_or((0.0, 0.0));
        let px = self.cx + dx / self.zoom;
        let py = self.cy + dy / self.zoom;
        self.zoom = z;
        self.cx = px - dx / z;
        self.cy = py - dy / z;
    }

    fn pan(&mut self, dx_px: f64, dy_px: f64) {
        self.cx += dx_px / self.zoom;
        self.cy += dy_px / self.zoom;
    }

    fn can_pan_x(&self, ws: WinSize) -> bool {
        self.dims().0 * self.zoom > self.area(ws).0 + 0.5
    }

    /// Clamp the view centre and return the visible source rectangle, the
    /// display size and the screen offset of that rectangle in the image area
    /// (pixels). The picture may be dragged past its edges by `OVERSCROLL` of
    /// the window, so the visible part need not be centred.
    fn view(&mut self, ws: WinSize) -> (Rect, Pair, Pair) {
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        self.cx = clamp_centre(self.cx, iw, aw / z);
        self.cy = clamp_centre(self.cy, ih, ah / z);
        let (x0, x1, ox) = visible_span(self.cx, iw, aw / z);
        let (y0, y1, oy) = visible_span(self.cy, ih, ah / z);
        let crop = (x0, y0, x1 - x0, y1 - y0);
        (crop, (crop.2 * z, crop.3 * z), (ox * z, oy * z))
    }

    /// Whole cells covering the visible `crop` (shown at screen offset `off`):
    /// (columns, rows), the source rectangle that fills exactly those cells at
    /// the current zoom, and the screen offset of the cells in pixels.
    fn cell_fit(&self, ws: WinSize, crop: Rect, off: Pair) -> (Pair, Rect, Pair) {
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        let ac = (aw / cw).round().max(1.0);
        let ar = (ah / ch).round().max(1.0);
        let cols = (crop.2 * z / cw).round().clamp(1.0, ac);
        let rows = (crop.3 * z / ch).round().clamp(1.0, ar);
        let sw = (cols * cw / z).min(iw);
        let sh = (rows * ch / z).min(ih);
        let sx = (crop.0 + crop.2 / 2.0 - sw / 2.0).clamp(0.0, iw - sw);
        let sy = (crop.1 + crop.3 / 2.0 - sh / 2.0).clamp(0.0, ih - sh);
        // Centre the cells on where the source rectangle sits on screen.
        let ox = (off.0 + (sx - crop.0) * z + (sw * z - cols * cw) / 2.0)
            .clamp(0.0, (aw - cols * cw).max(0.0));
        let oy = (off.1 + (sy - crop.1) * z + (sh * z - rows * ch) / 2.0)
            .clamp(0.0, (ah - rows * ch).max(0.0));
        ((cols, rows), (sx, sy, sw, sh), (ox, oy))
    }

    /// Make sure a proxy of the whole picture exists (adopting the frame on
    /// screen when possible); returns (id, w, h).
    fn ensure_proxy(&mut self, ws: WinSize, out: &mut Vec<u8>) -> (u32, u32, u32) {
        let (iw, ih) = self.dims();
        if let Some(p) = &self.proxy {
            if p.bg == self.bg && p.region == (0.0, 0.0, iw, ih) {
                return (p.id, p.w, p.h);
            }
        }
        self.drop_proxy(out);
        if let Some(p) = self.spare.take() {
            if p.bg == self.bg {
                let r = (p.id, p.w, p.h);
                self.proxy = Some(p);
                return r;
            }
            self.ctx.kitty.delete(out, p.id);
        }
        // The frame on screen shows the whole picture (the usual case when
        // zooming starts): adopt it as the proxy, so nothing is uploaded.
        if let (true, Some((w, h, bg))) = (self.shown, self.shown_full) {
            if bg == self.bg {
                let id = self.ids[self.cur];
                self.ids[self.cur] = next_id();
                self.shown = false;
                self.shown_full = None;
                self.on_proxy = true;
                self.proxy = Some(Proxy {
                    id,
                    w,
                    h,
                    bg,
                    grid: None,
                    region: (0.0, 0.0, iw, ih),
                });
                return (id, w, h);
            }
        }
        // Otherwise upload the picture at its default (whole-picture) size:
        // about as much data as one sharp frame.
        let mut s = self.default_zoom(ws);
        if self.pic.svg.is_none() {
            s = s.min(1.0);
        }
        s = s.min((PROXY_MAX_PIXELS / (iw * ih)).sqrt());
        self.upload_proxy(out, (0.0, 0.0, iw, ih), s)
    }

    /// Render `region` of the picture at scale `s` and upload it as the proxy.
    fn upload_proxy(
        &mut self,
        out: &mut Vec<u8>,
        region: (f64, f64, f64, f64),
        s: f64,
    ) -> (u32, u32, u32) {
        let (iw, ih) = self.dims();
        let whole = (0.0, 0.0, iw, ih);
        // Keep a whole-picture proxy as the spare while a region is shown.
        let keep = region != whole
            && self
                .proxy
                .as_ref()
                .is_some_and(|p| p.region == whole && p.bg == self.bg);
        if keep {
            if let Some(old) = self.spare.take() {
                self.ctx.kitty.delete(out, old.id);
            }
            self.spare = self.proxy.take();
            self.on_proxy = false;
        } else {
            self.drop_proxy(out);
        }
        let (pw, ph) = (
            (region.2 * s).round().max(1.0) as u32,
            (region.3 * s).round().max(1.0) as u32,
        );
        let raster = if let Some(tree) = &self.pic.svg {
            render_svg(tree, region, pw, ph)
        } else {
            let (level, scale) = self.source_for(s);
            let src = if level == 0 {
                &self.pic.frames[0].raster
            } else {
                &self.mips[level - 1]
            };
            let r = (
                region.0 * scale,
                region.1 * scale,
                region.2 * scale,
                region.3 * scale,
            );
            if region == whole && src.w == pw && src.h == ph {
                src.clone()
            } else if region == whole {
                resize(src, pw, ph)
            } else if s == 1.0 && level == 0 {
                // A plain crop: original pixels compress best.
                resize_region(src, r, pw, ph, Filter::Nearest)
            } else {
                resize_region(src, r, pw, ph, Filter::Lanczos)
            }
        };
        let raster = match self.bg.color() {
            Some(c) if raster.ch == 4 => raster.flatten(c),
            _ => raster,
        };
        let id = next_id();
        self.ctx
            .kitty
            .transmit(out, &raster, &format!("a=t,i={id}"));
        crate::app::trace(format!(
            "proxy uploaded: {pw}x{ph} of region {:.0}x{:.0}",
            region.2, region.3
        ));
        self.proxy = Some(Proxy {
            id,
            w: pw,
            h: ph,
            bg: self.bg,
            grid: None,
            region,
        });
        (id, pw, ph)
    }

    /// Show the current view from the proxy. Returns false if that is not
    /// possible (then a sharp frame follows once input pauses).
    fn show_proxy(&mut self, ws: WinSize) -> bool {
        if self.ctx.kitty.placeholders {
            return self.show_proxy_grid(ws);
        }
        self.show_proxy_placed(ws);
        true
    }

    /// Lay the current view out on the placeholder grid of a proxy showing
    /// `region`. `None` if the window cannot be addressed (its first column /
    /// last row index is beyond the diacritics table) or the region does not
    /// cover the view.
    fn grid_layout(&mut self, ws: WinSize, region: (f64, f64, f64, f64)) -> Option<GridLayout> {
        self.view(ws); // clamp the centre
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let z = self.zoom;
        let (rx, ry, rw, rh) = region;
        let gc = (rw * z / cw).round().max(1.0);
        let gr = (rh * z / ch).round().max(1.0);
        let ac = (aw / cw).round().max(1.0);
        let ar = (ah / ch).round().max(1.0);
        let (sc, vc, col0, cs) = grid_window(gc, ac, (self.cx - rx) * z / cw);
        let (sr, vr, row0, rs) = grid_window(gr, ar, (self.cy - ry) * z / ch);
        // Cells after the first in a row continue the column implicitly, so
        // only the first column and the rows need explicit indices.
        if vc == 0 || vr == 0 || sc as f64 > MAX_INDEX || (sr + vr) as f64 - 1.0 > MAX_INDEX {
            return None;
        }
        // Snap the centre to the cell grid so the sharp frame lines up.
        let cx = rx + (cs + ac / 2.0) * cw / z;
        let cy = ry + (rs + ar / 2.0) * ch / z;
        // A region crop ends before the picture does: the part of the picture
        // in the window must lie inside it.
        let (iw, ih) = self.dims();
        let (x0, x1, _) = visible_span(cx, iw, aw / z);
        let (y0, y1, _) = visible_span(cy, ih, ah / z);
        let (ex, ey) = (cw / z, ch / z);
        if x0 < rx - ex || x1 > rx + rw + ex || y0 < ry - ey || y1 > ry + rh + ey {
            return None;
        }
        Some(GridLayout {
            grid: (gc as u32, gr as u32),
            sc,
            vc,
            col0,
            sr,
            vr,
            row0,
            cx,
            cy,
        })
    }

    /// Upload a crop around the view as the proxy, sized so the window sits
    /// in the middle of what placeholder cells can address.
    fn upload_region_proxy(&mut self, ws: WinSize, out: &mut Vec<u8>) {
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        let ac = (aw / cw).round().max(1.0);
        let ar = (ah / ch).round().max(1.0);
        // Up to twice the window wide (first column must stay addressable)
        // and three times as tall (all rows must stay addressable).
        let rc = (2.0 * ac).min(MAX_INDEX + ac);
        let rr = (3.0 * ar).min(MAX_INDEX + 1.0);
        let rw = (rc * cw / z).min(iw).floor().max(1.0);
        let rh = (rr * ch / z).min(ih).floor().max(1.0);
        let rx = (self.cx - rw / 2.0).clamp(0.0, iw - rw).floor();
        let ry = (self.cy - rh / 2.0).clamp(0.0, ih - rh).floor();
        let mut s = z.min(1.0);
        s = s.min((REGION_MAX_PIXELS / (rw * rh)).sqrt());
        self.upload_proxy(out, (rx, ry, rw, rh), s);
    }

    /// Unicode-placeholder variant (tmux): one virtual placement covers the
    /// proxy's region as a grid of cells (a few bytes, sent only when the
    /// zoom changes) and only the visible cells are printed, as text that
    /// tmux redraws like any other. Panning moves by whole cells.
    fn show_proxy_grid(&mut self, ws: WinSize) -> bool {
        let (iw, ih) = self.dims();
        let full = (0.0, 0.0, iw, ih);
        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(b"\x1b[?2026h");
        let current = self
            .proxy
            .as_ref()
            .filter(|p| p.bg == self.bg)
            .map(|p| p.region);
        let layout = match current.and_then(|r| self.grid_layout(ws, r)) {
            Some(l) => l,
            None => {
                if let Some(l) = self.grid_layout(ws, full) {
                    self.ensure_proxy(ws, &mut out);
                    l
                } else {
                    self.upload_region_proxy(ws, &mut out);
                    let region = self.proxy.as_ref().map_or(full, |p| p.region);
                    match self.grid_layout(ws, region) {
                        Some(l) => l,
                        None => return false,
                    }
                }
            }
        };
        self.cx = layout.cx;
        self.cy = layout.cy;
        let Some(id) = self.proxy.as_ref().map(|p| p.id) else {
            return false;
        };
        let k = &self.ctx.kitty;
        if let Some(p) = self.proxy.as_mut() {
            if p.grid != Some(layout.grid) {
                // Replace the virtual placement (same image and placement id)
                // instead of deleting it first: a delete would leave the cells
                // without an image for a moment (flicker).
                k.cmd(
                    &mut out,
                    &format!(
                        "a=p,U=1,i={id},p=1,c={},r={},q=2",
                        layout.grid.0, layout.grid.1
                    ),
                );
                p.grid = Some(layout.grid);
            }
        }
        let (aw, ah) = self.area(ws);
        let (cw, ch) = ws.cell();
        let key = (id, layout.row0, layout.col0, layout.vr, layout.vc);
        let text = if self.cells == Some(key) {
            placeholder_heads(
                id,
                (layout.row0, layout.col0),
                (layout.sr, layout.sc),
                layout.vr,
            )
        } else {
            placeholder_frame(
                id,
                (ah / ch).round().max(1.0) as u32,
                (aw / cw).round().max(1.0) as u32,
                (layout.row0, layout.col0),
                (layout.sr, layout.sc),
                (layout.vr, layout.vc),
            )
        };
        out.extend_from_slice(text.as_bytes());
        self.cells = Some(key);
        if self.shown {
            k.delete(&mut out, self.ids[self.cur]);
            self.shown = false;
        }
        self.on_proxy = true;
        out.extend_from_slice(self.status(ws).as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        self.ctx.write(&out);
        true
    }

    /// Direct-placement variant: re-place the proxy with a source rectangle
    /// (a few bytes); the terminal crops and scales it.
    fn show_proxy_placed(&mut self, ws: WinSize) {
        let mut out: Vec<u8> = Vec::with_capacity(1024);
        out.extend_from_slice(b"\x1b[?2026h");
        let (id, pw, ph) = self.ensure_proxy(ws, &mut out);
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        let (crop, (disp_w, disp_h), voff) = self.view(ws);
        let (rx, ry, rw, rh) = self.proxy.as_ref().map_or((0.0, 0.0, iw, ih), |p| p.region);
        let (kx, ky) = (pw as f64 / rw, ph as f64 / rh);
        let (x, y, w, h, size, off_x, off_y);
        if (z / kx - 1.0).abs() < 1e-3 {
            // The proxy has exactly the display resolution: show its pixels
            // 1:1 (no scaling, pixel-exact like a sharp frame).
            w = (disp_w.round() as u32).clamp(1, pw);
            h = (disp_h.round() as u32).clamp(1, ph);
            x = (((crop.0 - rx) * kx).round().max(0.0) as u32).min(pw - w);
            y = (((crop.1 - ry) * ky).round().max(0.0) as u32).min(ph - h);
            size = String::new();
            off_x = voff.0.min(aw - w as f64).max(0.0);
            off_y = voff.1.min(ah - h as f64).max(0.0);
        } else {
            // The placement covers whole cells; pick the source rectangle that
            // fills exactly that many cells at the current zoom.
            let ((cols, rows), (sx, sy, sw, sh), off) = self.cell_fit(ws, crop, voff);
            x = (((sx - rx) * kx).round().max(0.0) as u32).min(pw - 1);
            y = (((sy - ry) * ky).round().max(0.0) as u32).min(ph - 1);
            w = ((sw * kx).round() as u32).clamp(1, pw - x);
            h = ((sh * ky).round() as u32).clamp(1, ph - y);
            size = format!("c={},r={},", cols as u32, rows as u32);
            (off_x, off_y) = off;
        }
        let col = (off_x / cw).floor();
        let row = (off_y / ch).floor();
        let xo = ((off_x - col * cw).floor() as u32).min(cw as u32 - 1);
        let yo = ((off_y - row * ch).floor() as u32).min(ch as u32 - 1);
        out.extend_from_slice(format!("\x1b[{};{}H", row as u32 + 1, col as u32 + 1).as_bytes());
        let k = &self.ctx.kitty;
        k.cmd(
            &mut out,
            &format!(
                "a=p,i={id},p=1,x={x},y={y},w={w},h={h},{size}X={xo},Y={yo},C=1,{Z_BELOW_BG},q=2"
            ),
        );
        if self.shown {
            k.delete(&mut out, self.ids[self.cur]);
            self.shown = false;
        }
        self.on_proxy = true;
        out.extend_from_slice(self.status(ws).as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        self.ctx.write(&out);
    }

    /// The proxy shown at the current zoom is noticeably soft (upscaled) or
    /// heavily downscaled by the terminal, so a sharp frame should follow.
    fn needs_refine(&self) -> bool {
        let Some(p) = &self.proxy else {
            return true;
        };
        let ps = p.w as f64 / p.region.2;
        self.zoom > ps * 1.01 || ps > self.zoom * 1.25
    }

    /// Remove the proxy placement from the screen, keeping its pixel data.
    /// (Placeholder cells are simply overwritten by the next frame.)
    fn hide_proxy(&mut self, out: &mut Vec<u8>) {
        if let (true, false, Some(p)) = (self.on_proxy, self.ctx.kitty.placeholders, &self.proxy) {
            self.ctx.kitty.cmd(out, &format!("a=d,d=i,i={},q=2", p.id));
        }
        self.on_proxy = false;
    }

    /// Free the proxy image in the terminal.
    fn drop_proxy(&mut self, out: &mut Vec<u8>) {
        if let Some(p) = self.proxy.take() {
            self.ctx.kitty.delete(out, p.id);
        }
        self.on_proxy = false;
    }

    /// Free the proxy and the spare.
    fn drop_all_proxies(&mut self, out: &mut Vec<u8>) {
        self.drop_proxy(out);
        if let Some(p) = self.spare.take() {
            self.ctx.kitty.delete(out, p.id);
        }
    }

    /// Draw a sharp frame of the current view. With `interruptible`, the
    /// frame is cancelled when user input arrives while it is being written
    /// (the proxy stays on screen). Returns whether it was cancelled and the
    /// events read while writing, which the caller must handle.
    fn render(&mut self, ws: WinSize, filter: Filter, interruptible: bool) -> (bool, Vec<Event>) {
        let t0 = Instant::now();
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let z = self.zoom;
        let (crop, (disp_w, disp_h), voff) = self.view(ws);
        let (dw, dh) = (
            disp_w.round().max(1.0) as u32,
            disp_h.round().max(1.0) as u32,
        );
        let (iw0, ih0) = self.dims();
        let mut full = !self.pic.is_animated()
            && crop.0 < 0.5
            && crop.1 < 0.5
            && crop.2 >= iw0 - 0.5
            && crop.3 >= ih0 - 0.5;

        // Over ssh above 100%, send the visible pixels at the picture's own
        // resolution (from 4x enlarged by a whole factor with nearest
        // neighbour, so pixels stay crisp) and let the terminal scale them to
        // whole cells: several times less data than an enlarged frame.
        let scaled = self.inline && z > 1.0 + 1e-9 && self.pic.svg.is_none();
        let mut cells: Option<(u32, u32)> = None;
        // Screen offset of the frame in the image area (pixels).
        let mut off = voff;
        let (level, scale) = self.source_for(z);
        let raster: Cow<Raster> = if scaled {
            let ((cols, rows), (sx, sy, sw, sh), cell_off) = self.cell_fit(ws, crop, voff);
            off = cell_off;
            let x0 = sx.round().min(iw0 - 1.0);
            let y0 = sy.round().min(ih0 - 1.0);
            let w0 = sw.round().clamp(1.0, iw0 - x0);
            let h0 = sh.round().clamp(1.0, ih0 - y0);
            let n = if z >= 4.0 { z.floor() } else { 1.0 };
            cells = Some((cols as u32, rows as u32));
            full = full && x0 == 0.0 && y0 == 0.0 && w0 == iw0 && h0 == ih0;
            Cow::Owned(resize_region(
                &self.pic.frames[self.frame].raster,
                (x0, y0, w0, h0),
                (w0 * n) as u32,
                (h0 * n) as u32,
                Filter::Nearest,
            ))
        } else if let Some(tree) = &self.pic.svg {
            Cow::Owned(render_svg(tree, crop, dw, dh))
        } else {
            let src = if level == 0 {
                &self.pic.frames[self.frame].raster
            } else {
                &self.mips[level - 1]
            };
            let crop = (
                crop.0 * scale,
                crop.1 * scale,
                crop.2 * scale,
                crop.3 * scale,
            );
            let (iw, ih) = (src.w as f64, src.h as f64);
            let f = if z >= 4.0 { Filter::Nearest } else { filter };
            if crop.0 == 0.0 && crop.1 == 0.0 && dw == src.w && dh == src.h {
                Cow::Borrowed(src) // shown 1:1: no copy
            } else if f == Filter::Lanczos && crop.2 >= iw - 0.5 && crop.3 >= ih - 0.5 {
                Cow::Owned(resize(src, dw, dh))
            } else {
                Cow::Owned(resize_region(src, crop, dw, dh, f))
            }
        };
        let raster = match self.bg.color() {
            Some(c) if raster.ch == 4 => Cow::Owned(raster.flatten(c)),
            _ => raster,
        };
        let (rw, rh) = (raster.w, raster.h);
        crate::app::trace(format!(
            "render {rw}x{rh} for {dw}x{dh} {filter:?}: pixels ready"
        ));

        let cells_before = self.cells;
        let k = &self.ctx.kitty;
        let new_id = self.ids[1 - self.cur];
        let old_id = self.ids[self.cur];
        let mut out: Vec<u8> = Vec::with_capacity(raster.data.len() / 2 + 1024);
        out.extend_from_slice(b"\x1b[?2026h");
        if k.placeholders {
            let ac = (aw / cw).round().max(1.0);
            let ar = (ah / ch).round().max(1.0);
            let (cols, rows) = cells.unwrap_or((
                (dw as f64 / cw).round().clamp(1.0, ac) as u32,
                (dh as f64 / ch).round().clamp(1.0, ar) as u32,
            ));
            // Same window offset as the proxy grid uses, so nothing shifts.
            let col0 = (off.0 / cw).round().clamp(0.0, (ac - cols as f64).max(0.0)) as u32;
            let row0 = (off.1 / ch).round().clamp(0.0, (ar - rows as f64).max(0.0)) as u32;
            k.transmit(
                &mut out,
                &raster,
                // p=1 so a later proxy grid can replace this placement.
                &format!("a=T,U=1,i={new_id},p=1,c={cols},r={rows}"),
            );
            out.extend_from_slice(
                placeholder_frame(
                    new_id,
                    ar as u32,
                    ac as u32,
                    (row0, col0),
                    (0, 0),
                    (rows, cols),
                )
                .as_bytes(),
            );
            self.cells = Some((new_id, row0, col0, rows, cols));
        } else {
            // A terminal-scaled frame fills whole cells.
            let (off_x, off_y, size) = match cells {
                Some((c, r)) => (off.0, off.1, format!("c={c},r={r},")),
                None => (
                    off.0.min(aw - dw as f64).max(0.0),
                    off.1.min(ah - dh as f64).max(0.0),
                    String::new(),
                ),
            };
            let col = (off_x / cw).floor();
            let row = (off_y / ch).floor();
            let xo = ((off_x - col * cw).floor() as u32).min(cw as u32 - 1);
            let yo = ((off_y - row * ch).floor() as u32).min(ch as u32 - 1);
            out.extend_from_slice(
                format!("\x1b[{};{}H", row as u32 + 1, col as u32 + 1).as_bytes(),
            );
            k.transmit(
                &mut out,
                &raster,
                &format!(
                    "a=T,i={new_id},p=1,{size}X={xo},Y={yo},C=1,{}",
                    crate::kitty::Z_BELOW_BG
                ),
            );
        }
        let before = (self.cur, self.shown, self.on_proxy);
        if self.shown {
            k.delete(&mut out, old_id);
        }
        self.hide_proxy(&mut out);
        if self.proxy.as_ref().is_some_and(|p| p.bg != self.bg) {
            self.drop_all_proxies(&mut out);
        }
        self.cur = 1 - self.cur;
        self.shown = true;
        out.extend_from_slice(self.status(ws).as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        crate::app::trace(format!("render: encoded {} KB", out.len() / 1024));
        let mut evs = Vec::new();
        if interruptible {
            let (cut, read) = write_interruptible(&out);
            evs = read;
            if cut {
                // A delete aborts the partial upload (kitty spec); the proxy
                // was not touched, so it is still what is on screen.
                let mut tail = Vec::new();
                self.ctx.kitty.delete(&mut tail, new_id);
                tail.extend_from_slice(b"\x1b[?2026l");
                self.ctx.write(&tail);
                (self.cur, self.shown, self.on_proxy) = before;
                self.cells = cells_before;
                return (true, evs);
            }
        } else {
            self.ctx.write(&out);
        }
        crate::app::trace("render: written");
        self.shown_full = full.then_some((rw, rh, self.bg));
        if filter == Filter::Lanczos {
            self.last_render = t0.elapsed();
        }
        (false, evs)
    }

    /// Re-send the virtual placement of the placeholder frame on screen (a
    /// few bytes, no pixels). Ghostty can leave placeholder cells blank when
    /// they are drawn while the image is still being decoded, and only fills
    /// them on its next redraw (e.g. after a mouse click); a placement change
    /// triggers that redraw.
    fn nudge(&mut self) {
        if let (true, false, Some((id, _, _, rows, cols))) = (self.shown, self.on_proxy, self.cells)
        {
            if id == self.ids[self.cur] {
                let mut out = Vec::new();
                self.ctx.kitty.cmd(
                    &mut out,
                    &format!("a=p,U=1,i={id},p=1,c={cols},r={rows},q=2"),
                );
                self.ctx.write(&out);
            }
        }
    }

    fn status(&self, ws: WinSize) -> String {
        let anim = if self.pic.is_animated() {
            format!(
                "  frame {}/{}{}",
                self.frame + 1,
                self.pic.frames.len(),
                if self.paused { " ⏸" } else { "" }
            )
        } else {
            String::new()
        };
        let left = format!("{}{}", self.name, pos_label(self.pos));
        let bg = if self.transparent {
            format!("  bg {}", self.bg.label())
        } else {
            String::new()
        };
        let right = format!(
            "{}×{}  {}  {}{}{}  {:.0}%",
            self.pic.width,
            self.pic.height,
            self.pic.format,
            self.meta,
            anim,
            bg,
            self.zoom * 100.0
        );
        let mut s = status_bar(&self.ctx.theme, ws.rows - 1, ws.cols, "IMG", &left, &right);
        let mut hints: Vec<(&str, &str)> = HINTS.to_vec();
        if self.transparent {
            hints.push(("b", "bg"));
        }
        if self.pic.is_animated() {
            hints.push(("space", "next/pause"));
        }
        s.push_str(&hint_bar(&self.ctx.theme, ws.rows, ws.cols, &hints));
        s
    }

    fn clear(&mut self) {
        let mut out = Vec::new();
        self.drop_all_proxies(&mut out);
        for id in self.ids {
            self.ctx.kitty.delete(&mut out, id);
        }
        out.extend_from_slice(b"\x1b[0m\x1b[2J");
        self.ctx.write(&out);
        self.shown = false;
        self.cells = None;
    }
}

pub fn run(ctx: &Ctx, pic: Picture, name: &str, meta: String, pos: (usize, usize)) -> Result<Nav> {
    let animated = pic.is_animated();
    let transparent = pic.svg.is_some() || pic.frames.iter().any(|f| f.raster.ch == 4);
    let mut v = Viewer {
        ctx,
        pic,
        name: name.to_string(),
        meta,
        pos,
        zoom: 1.0,
        cx: 0.0,
        cy: 0.0,
        ids: [next_id(), next_id()],
        cur: 0,
        shown: false,
        frame: 0,
        next_frame: None,
        paused: false,
        last_render: Duration::ZERO,
        hq_due: None,
        drag: None,
        help: false,
        mips: Vec::new(),
        transparent,
        bg: Background::from_u8(BACKGROUND.load(Ordering::Relaxed)),
        inline: ctx.kitty.inline(),
        proxy: None,
        spare: None,
        on_proxy: false,
        shown_full: None,
        cells: None,
        nudge_due: None,
    };
    // With inline pixels (ssh, tmux), zoom / pan by re-placing an uploaded proxy (pointless for
    // animations, which re-send every frame anyway).
    let use_proxy = v.inline && !animated;
    let settle = if ctx.kitty.network {
        REMOTE_SETTLE
    } else {
        LOCAL_SETTLE
    };
    let mut ws = ctx.size();
    v.reset(ws);
    ctx.write(b"\x1b[0m\x1b[2J");
    let mut dirty = true;
    let mut interactive = false;
    // The last events only zoomed or panned.
    let mut moved = false;
    // The pending render is the sharp frame after inline zooming / panning.
    let mut refine = false;
    // Events read while a cancelled sharp frame was being written.
    let mut pending: Vec<Event> = Vec::new();

    let nav = loop {
        if dirty {
            if v.help {
                v.clear();
                ctx.write(
                    help_screen(&ctx.theme, ws.cols, ws.rows, "shw — image viewer", HELP)
                        .as_bytes(),
                );
            } else if moved && v.inline {
                // Over ssh, never send pixels per wheel notch / drag step: move the
                // proxy (or wait), and send one sharp frame once input pauses.
                if !(use_proxy && v.show_proxy(ws)) {
                    // Nothing new on screen until the sharp frame.
                    v.on_proxy = false;
                }
                v.hq_due = Some(Instant::now() + settle);
            } else {
                // While zooming / dragging, draw with a fast filter; refine when idle.
                let fast = !v.inline && interactive && v.last_render > Duration::from_millis(20);
                let filter = if fast {
                    Filter::Bilinear
                } else {
                    Filter::Lanczos
                };
                v.hq_due = fast.then(|| Instant::now() + Duration::from_millis(120));
                let (cancelled, evs) = v.render(ws, filter, refine && v.on_proxy);
                pending = evs;
                if cancelled {
                    // New input arrived: handle it, retry after the next pause.
                    v.hq_due = Some(Instant::now() + settle);
                } else if ctx.kitty.placeholders && !animated {
                    v.nudge_due = Some((Instant::now() + NUDGE_DELAY, NUDGES));
                }
            }
            if animated && !v.paused && v.next_frame.is_none() {
                v.next_frame = Some(Instant::now() + v.pic.frames[v.frame].delay);
            }
            dirty = false;
            interactive = false;
            moved = false;
            refine = false;
        }

        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(3600);
        if let Some(t) = v.hq_due {
            deadline = deadline.min(t);
        }
        if let Some((t, _)) = v.nudge_due {
            deadline = deadline.min(t);
        }
        if let (Some(t), false, false) = (v.next_frame, v.paused, v.help) {
            deadline = deadline.min(t);
        }
        let evs = if pending.is_empty() {
            read_events(deadline.saturating_duration_since(now))?
        } else {
            std::mem::take(&mut pending)
        };
        let now = Instant::now();
        if evs.is_empty() {
            if let Some((t, n)) = v.nudge_due {
                if now >= t {
                    v.nudge();
                    v.nudge_due = (n > 1).then(|| (now + NUDGE_DELAY * 3, n - 1));
                }
            }
            if v.hq_due.is_some_and(|t| now >= t) {
                v.hq_due = None;
                if ctx.kitty.network && v.drag.is_some() {
                    // Over ssh, mouse button still held mid-drag: no sharp
                    // frame yet (locally it is cheap, and cancelled by the
                    // next drag step anyway).
                    v.hq_due = Some(now + settle);
                } else if !v.on_proxy || v.needs_refine() {
                    dirty = true;
                    refine = v.inline;
                }
            }
            if let Some(t) = v.next_frame {
                if now >= t && !v.paused && !v.help {
                    v.frame = (v.frame + 1) % v.pic.frames.len();
                    v.next_frame =
                        Some(t.max(now - Duration::from_millis(200)) + v.pic.frames[v.frame].delay);
                    dirty = true;
                }
            }
            continue;
        }

        let (cw, ch) = ws.cell();
        let mut nav = None;
        for ev in evs {
            if v.help {
                if matches!(ev, Event::Key(_)) {
                    v.help = false;
                    dirty = true;
                }
                continue;
            }
            if is_quit(&ev) {
                nav = Some(Nav::Quit);
                break;
            }
            match ev {
                Event::Resize(..) => {
                    ws = ctx.size();
                    v.clear();
                    v.reset(ws);
                    dirty = true;
                }
                Event::Mouse(m) => {
                    let anchor = Some((
                        m.column as f64 * cw + cw / 2.0,
                        m.row as f64 * ch + ch / 2.0,
                    ));
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            v.zoom_by(ws, WHEEL_STEP, anchor);
                            dirty = true;
                            moved = true;
                            interactive = true;
                        }
                        MouseEventKind::ScrollDown => {
                            v.zoom_by(ws, 1.0 / WHEEL_STEP, anchor);
                            dirty = true;
                            moved = true;
                            interactive = true;
                        }
                        MouseEventKind::Down(MouseButton::Left) => v.drag = Some((m.column, m.row)),
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some((x, y)) = v.drag {
                                v.pan(
                                    -(m.column as f64 - x as f64) * cw,
                                    -(m.row as f64 - y as f64) * ch,
                                );
                                v.drag = Some((m.column, m.row));
                                dirty = true;
                                moved = true;
                                interactive = true;
                            }
                        }
                        MouseEventKind::Up(_) => v.drag = None,
                        _ => {}
                    }
                }
                Event::Key(k) => {
                    let (aw, ah) = v.area(ws);
                    let big = if k.modifiers.contains(KeyModifiers::SHIFT) {
                        0.5
                    } else {
                        0.15
                    };
                    dirty = true;
                    let pans_x =
                        matches!(k.code, KeyCode::Left | KeyCode::Right) && v.can_pan_x(ws);
                    moved |= pans_x
                        || matches!(
                            k.code,
                            KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::Char(
                                    '+' | '='
                                        | '-'
                                        | '_'
                                        | '0'
                                        | 'f'
                                        | '1'
                                        | '2'
                                        | 'h'
                                        | 'H'
                                        | 'l'
                                        | 'L'
                                        | 'k'
                                        | 'K'
                                        | 'j'
                                        | 'J'
                                )
                        );
                    match k.code {
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            v.zoom_by(ws, KEY_STEP, None);
                            interactive = true;
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            v.zoom_by(ws, 1.0 / KEY_STEP, None)
                        }
                        KeyCode::Char('0') | KeyCode::Char('f') => {
                            let fit = v.fit(ws);
                            v.set_zoom(ws, fit, None);
                        }
                        KeyCode::Char('1') => v.set_zoom(ws, 1.0, None),
                        KeyCode::Char('2') => v.set_zoom(ws, 2.0, None),
                        KeyCode::Char('h') | KeyCode::Char('H') => v.pan(-aw * big, 0.0),
                        KeyCode::Char('l') | KeyCode::Char('L') => v.pan(aw * big, 0.0),
                        KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Up => {
                            v.pan(0.0, -ah * big)
                        }
                        KeyCode::Char('j') | KeyCode::Char('J') | KeyCode::Down => {
                            v.pan(0.0, ah * big)
                        }
                        KeyCode::Left if v.can_pan_x(ws) => v.pan(-aw * big, 0.0),
                        KeyCode::Right if v.can_pan_x(ws) => v.pan(aw * big, 0.0),
                        KeyCode::Char('r') | KeyCode::Char('R') if v.pic.svg.is_none() => {
                            let times = if k.code == KeyCode::Char('r') { 1 } else { 3 };
                            for f in &mut v.pic.frames {
                                for _ in 0..times {
                                    f.raster = f.raster.rotate90();
                                }
                            }
                            std::mem::swap(&mut v.pic.width, &mut v.pic.height);
                            v.mips.clear();
                            v.clear();
                            v.reset(ws);
                        }
                        KeyCode::Char('b') if v.transparent => {
                            v.bg = v.bg.next();
                            BACKGROUND.store(v.bg as u8, Ordering::Relaxed);
                        }
                        KeyCode::Char(' ') if animated => {
                            v.paused = !v.paused;
                            v.next_frame = None;
                        }
                        KeyCode::Char('n')
                        | KeyCode::Char(' ')
                        | KeyCode::Tab
                        | KeyCode::Right
                        | KeyCode::PageDown => {
                            nav = Some(Nav::Next);
                            break;
                        }
                        KeyCode::Char('p')
                        | KeyCode::Backspace
                        | KeyCode::BackTab
                        | KeyCode::Left
                        | KeyCode::PageUp => {
                            nav = Some(Nav::Prev);
                            break;
                        }
                        KeyCode::Char('?') => v.help = true,
                        _ => dirty = false,
                    }
                }
                _ => {}
            }
        }
        if let Some(n) = nav {
            break n;
        }
    };
    v.clear();
    Ok(nav)
}

/// Print an image inline (like `kitty +kitten icat`), fitted into `ws`.
pub fn print_inline(ctx: &Ctx, pic: &Picture, ws: WinSize, upscale: bool) {
    let (cw, ch) = ws.cell();
    let (aw, ah) = (ws.cols as f64 * cw, ws.rows.max(1) as f64 * ch);
    let (iw, ih) = (pic.width.max(1) as f64, pic.height.max(1) as f64);
    let mut z = (aw / iw).min(ah / ih);
    if !upscale && pic.svg.is_none() {
        z = z.min(1.0);
    }
    let (dw, dh) = (
        (iw * z).round().max(1.0) as u32,
        (ih * z).round().max(1.0) as u32,
    );
    let raster = pic.render(dw, dh);
    let cols = (dw as f64 / cw).ceil().max(1.0) as u32;
    let rows = (dh as f64 / ch).ceil().max(1.0) as u32;
    let mut doc = crate::doc::Doc::default();
    doc.image(raster.into(), 0, cols, rows, Vec::new(), String::new());
    crate::pager::print_doc(ctx, &mut doc, ws.cols as usize);
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_centre, grid_window, next_boundary, placeholder_frame, placeholder_heads,
        visible_span,
    };

    #[test]
    fn boundaries_skip_escaped_terminators() {
        // plain: cut after each ST
        let plain = b"\x1b_Ga;AAAA\x1b\\\x1b_Gm=0;BB\x1b\\";
        assert_eq!(next_boundary(plain, 0), 11);
        assert_eq!(next_boundary(plain, 11), plain.len());
        // tmux passthrough: the inner (doubled-ESC) ST is not a boundary
        let wrapped = crate::kitty::tmux_wrap("\x1b_Ga;AAAA\x1b\\");
        let w = wrapped.as_bytes();
        assert_eq!(next_boundary(w, 0), w.len());
        assert_eq!(next_boundary(b"no terminator", 0), 13);
    }

    #[test]
    fn placeholder_frame_overwrites_in_place() {
        // 10x4 area, image 6x2 cells at (row 1, col 2)
        let s = placeholder_frame(7, 4, 10, (1, 2), (0, 0), (2, 6));
        assert!(!s.contains("\x1b[2J"), "never clears the whole screen");
        // rows outside the image are erased, rows with the image are not
        assert!(s.contains("\x1b[1;1H\x1b[0m\x1b[2K"));
        assert!(s.contains("\x1b[4;1H\x1b[0m\x1b[2K"));
        assert!(!s.contains("\x1b[2;1H\x1b[0m\x1b[2K"));
        // left margin blanked, right margin erased
        assert!(s.contains("\x1b[2;1H\x1b[0m\x1b[2X\x1b[2;3H"));
        assert_eq!(s.matches("\x1b[K").count(), 2);
        // full width: no erase after the last cell (it would wipe it)
        let full = placeholder_frame(7, 2, 10, (0, 0), (0, 0), (2, 10));
        assert!(!full.contains("\x1b[K") && !full.contains("X\x1b"));
    }

    #[test]
    fn placeholder_heads_rewrite_only_first_cells() {
        let s = placeholder_heads(7, (1, 2), (5, 9), 3);
        assert_eq!(s.matches('\u{10EEEE}').count(), 3);
        assert!(s.contains("\x1b[2;3H") && s.contains("\x1b[4;3H"));
        use crate::diacritics::DIACRITICS;
        // first head: row 5, column 9
        let first: String = [DIACRITICS[5], DIACRITICS[9]].iter().collect();
        assert!(s.contains(&format!("\u{10EEEE}{first}")));
    }

    #[test]
    fn grid_window_centres_and_clamps() {
        // grid smaller than the view: centred, all cells
        assert_eq!(grid_window(10.0, 20.0, 5.0), (0, 10, 5, -5.0));
        // larger: window around the centre
        assert_eq!(grid_window(100.0, 20.0, 50.0), (40, 20, 0, 40.0));
        // dragged past either end: the window shows empty cells there
        assert_eq!(grid_window(100.0, 20.0, 7.0), (0, 17, 3, -3.0));
        assert_eq!(grid_window(100.0, 20.0, 93.0), (83, 17, 0, 83.0));
    }

    #[test]
    fn centre_overscrolls_by_a_fifth_of_the_window() {
        // larger than the window (100 px window over 1000 px): 20 px past an edge
        assert_eq!(clamp_centre(-500.0, 1000.0, 100.0), 30.0);
        assert_eq!(clamp_centre(5000.0, 1000.0, 100.0), 970.0);
        assert_eq!(clamp_centre(400.0, 1000.0, 100.0), 400.0);
        // smaller: 20 px either side of the centre
        assert_eq!(clamp_centre(0.0, 50.0, 100.0), 5.0);
        assert_eq!(clamp_centre(99.0, 50.0, 100.0), 45.0);
        // fully dragged left: 80 % of the window shows the picture's start
        assert_eq!(visible_span(30.0, 1000.0, 100.0), (0.0, 80.0, 20.0));
    }
}
