//! Interactive image viewer with pixel-exact rendering, zoom (mouse wheel at
//! the pointer, + / -), panning (drag, hjkl) and animated GIF/WebP/APNG.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::app::{is_quit, pos_label, read_events, Ctx, Nav, CHROME_ROWS};
use crate::diacritics::DIACRITICS;
use crate::kitty::{next_id, placeholder_row, placeholder_row_at, Transfer, Z_BELOW_BG};
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
const REMOTE_SETTLE: Duration = Duration::from_millis(150);
/// Largest proxy image uploaded once for remote zooming / panning, in pixels.
const PROXY_MAX_PIXELS: f64 = 6.0e6;

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
}

/// Visible part of a `grid`-cell long axis in `avail` cells around `center`:
/// (first grid cell, number of cells, screen offset in cells).
fn grid_window(grid: f64, avail: f64, center: f64) -> (u32, u32, u32) {
    if grid <= avail {
        (0, grid as u32, ((avail - grid) / 2.0).floor() as u32)
    } else {
        let start = (center - avail / 2.0).round().clamp(0.0, grid - avail);
        (start as u32, avail as u32, 0)
    }
}

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
    /// Pixels travel over the network (ssh, direct transfer).
    remote: bool,
    proxy: Option<Proxy>,
    /// The proxy placement is what is currently on screen.
    on_proxy: bool,
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

    /// Clamp the view centre and return the visible source rectangle and the
    /// display size in pixels.
    fn view(&mut self, ws: WinSize) -> ((f64, f64, f64, f64), f64, f64) {
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        let disp_w = (iw * z).min(aw);
        let disp_h = (ih * z).min(ah);
        let src_w = disp_w / z;
        let src_h = disp_h / z;
        self.cx = self
            .cx
            .clamp(src_w / 2.0, (iw - src_w / 2.0).max(src_w / 2.0));
        self.cy = self
            .cy
            .clamp(src_h / 2.0, (ih - src_h / 2.0).max(src_h / 2.0));
        let crop = (self.cx - src_w / 2.0, self.cy - src_h / 2.0, src_w, src_h);
        (crop, disp_w, disp_h)
    }

    /// Upload the proxy image if it is missing or stale; returns (id, w, h).
    fn ensure_proxy(&mut self, ws: WinSize, out: &mut Vec<u8>) -> (u32, u32, u32) {
        if let Some(p) = &self.proxy {
            if p.bg == self.bg {
                return (p.id, p.w, p.h);
            }
        }
        self.drop_proxy(out);
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        // Twice the fitted size covers moderate zoom without a re-send.
        let mut s = (2.0 * aw / iw).min(2.0 * ah / ih);
        if self.pic.svg.is_none() {
            s = s.min(1.0);
        }
        s = s.min((PROXY_MAX_PIXELS / (iw * ih)).sqrt());
        let (pw, ph) = (
            (iw * s).round().max(1.0) as u32,
            (ih * s).round().max(1.0) as u32,
        );
        let raster = if let Some(tree) = &self.pic.svg {
            render_svg(tree, (0.0, 0.0, iw, ih), pw, ph)
        } else {
            let (level, _) = self.source_for(s);
            let src = if level == 0 {
                &self.pic.frames[0].raster
            } else {
                &self.mips[level - 1]
            };
            if src.w == pw && src.h == ph {
                src.clone()
            } else {
                resize(src, pw, ph)
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
        self.proxy = Some(Proxy {
            id,
            w: pw,
            h: ph,
            bg: self.bg,
            grid: None,
        });
        (id, pw, ph)
    }

    /// Show the current view from the proxy. Returns false if that is not
    /// possible at this zoom (the placeholder grid would be too large).
    fn show_proxy(&mut self, ws: WinSize) -> bool {
        if self.ctx.kitty.placeholders {
            return self.show_proxy_grid(ws);
        }
        self.show_proxy_placed(ws);
        true
    }

    /// Unicode-placeholder variant (tmux): one virtual placement covers the
    /// whole zoomed image as a grid of cells (a few bytes, sent only when the
    /// zoom changes) and only the visible cells are printed, as text that
    /// tmux redraws like any other. Panning moves by whole cells.
    fn show_proxy_grid(&mut self, ws: WinSize) -> bool {
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let (iw, ih) = self.dims();
        let z = self.zoom;
        let gc = (iw * z / cw).round().max(1.0);
        let gr = (ih * z / ch).round().max(1.0);
        // Row / column indices are limited by the diacritics table.
        let max = DIACRITICS.len() as f64;
        if gc > max || gr > max {
            return false;
        }
        let ac = (aw / cw).round().max(1.0);
        let ar = (ah / ch).round().max(1.0);
        let (sc, vc, col0) = grid_window(gc, ac, self.cx * z / cw);
        let (sr, vr, row0) = grid_window(gr, ar, self.cy * z / ch);
        // Snap the centre to the cell grid so the sharp frame lines up.
        self.cx = if gc > ac {
            (sc as f64 + ac / 2.0) * cw / z
        } else {
            iw / 2.0
        };
        self.cy = if gr > ar {
            (sr as f64 + ar / 2.0) * ch / z
        } else {
            ih / 2.0
        };

        let mut out: Vec<u8> = Vec::with_capacity((vc * vr * 4 + 4096) as usize);
        out.extend_from_slice(b"\x1b[?2026h");
        let (id, _, _) = self.ensure_proxy(ws, &mut out);
        let grid = (gc as u32, gr as u32);
        let k = &self.ctx.kitty;
        if let Some(p) = self.proxy.as_mut() {
            if p.grid != Some(grid) {
                // Drop the old virtual placement (keeping the data), then create
                // one for the new grid; exactly one exists for the image.
                k.cmd(&mut out, &format!("a=d,d=i,i={id},q=2"));
                k.cmd(
                    &mut out,
                    &format!("a=p,U=1,i={id},p=1,c={},r={},q=2", grid.0, grid.1),
                );
                p.grid = Some(grid);
            }
        }
        out.extend_from_slice(b"\x1b[0m\x1b[2J");
        for r in 0..vr {
            out.extend_from_slice(
                format!(
                    "\x1b[{};{}H{}",
                    row0 + r + 1,
                    col0 + 1,
                    placeholder_row_at(id, sr + r, sc, vc)
                )
                .as_bytes(),
            );
        }
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
        let (_, disp_w, disp_h) = self.view(ws);
        // The placement covers whole cells; pick the source rectangle that
        // fills exactly that many cells at the current zoom.
        let cols = (disp_w / cw).round().clamp(1.0, (aw / cw).round().max(1.0));
        let rows = (disp_h / ch).round().clamp(1.0, (ah / ch).round().max(1.0));
        let sw = (cols * cw / z).min(iw);
        let sh = (rows * ch / z).min(ih);
        let sx = (self.cx - sw / 2.0).clamp(0.0, iw - sw);
        let sy = (self.cy - sh / 2.0).clamp(0.0, ih - sh);
        let (kx, ky) = (pw as f64 / iw, ph as f64 / ih);
        let x = ((sx * kx).round() as u32).min(pw - 1);
        let y = ((sy * ky).round() as u32).min(ph - 1);
        let w = ((sw * kx).round() as u32).clamp(1, pw - x);
        let h = ((sh * ky).round() as u32).clamp(1, ph - y);
        let off_x = ((aw - cols * cw) / 2.0).max(0.0);
        let off_y = ((ah - rows * ch) / 2.0).max(0.0);
        let col = (off_x / cw).floor();
        let row = (off_y / ch).floor();
        let xo = ((off_x - col * cw).floor() as u32).min(cw as u32 - 1);
        let yo = ((off_y - row * ch).floor() as u32).min(ch as u32 - 1);
        out.extend_from_slice(format!("\x1b[{};{}H", row as u32 + 1, col as u32 + 1).as_bytes());
        let k = &self.ctx.kitty;
        k.cmd(
            &mut out,
            &format!(
                "a=p,i={id},p=1,x={x},y={y},w={w},h={h},c={},r={},X={xo},Y={yo},C=1,{Z_BELOW_BG},q=2",
                cols as u32, rows as u32
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
        let ps = p.w as f64 / self.pic.width as f64;
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

    fn render(&mut self, ws: WinSize, filter: Filter) {
        let t0 = Instant::now();
        let (cw, ch) = ws.cell();
        let (aw, ah) = self.area(ws);
        let z = self.zoom;
        let (crop, disp_w, disp_h) = self.view(ws);
        let (dw, dh) = (
            disp_w.round().max(1.0) as u32,
            disp_h.round().max(1.0) as u32,
        );

        let (level, scale) = self.source_for(z);
        let raster: Cow<Raster> = if let Some(tree) = &self.pic.svg {
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

        let k = &self.ctx.kitty;
        let new_id = self.ids[1 - self.cur];
        let old_id = self.ids[self.cur];
        let mut out: Vec<u8> = Vec::with_capacity(raster.data.len() / 2 + 1024);
        out.extend_from_slice(b"\x1b[?2026h");
        let off_x = ((aw - dw as f64) / 2.0).max(0.0);
        let off_y = ((ah - dh as f64) / 2.0).max(0.0);
        if k.placeholders {
            let cols = (dw as f64 / cw).round().max(1.0) as u32;
            let rows = (dh as f64 / ch).round().max(1.0) as u32;
            let col0 = (off_x / cw).round() as u32;
            let row0 = (off_y / ch).round() as u32;
            k.transmit(
                &mut out,
                &raster,
                &format!("a=T,U=1,i={new_id},c={cols},r={rows}"),
            );
            out.extend_from_slice(b"\x1b[0m\x1b[2J");
            for r in 0..rows {
                out.extend_from_slice(
                    format!(
                        "\x1b[{};{}H{}",
                        row0 + r + 1,
                        col0 + 1,
                        placeholder_row(new_id, r, cols)
                    )
                    .as_bytes(),
                );
            }
        } else {
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
                    "a=T,i={new_id},p=1,X={xo},Y={yo},C=1,{}",
                    crate::kitty::Z_BELOW_BG
                ),
            );
        }
        if self.shown {
            k.delete(&mut out, old_id);
        }
        self.hide_proxy(&mut out);
        if self.proxy.as_ref().is_some_and(|p| p.bg != self.bg) {
            self.drop_proxy(&mut out);
        }
        self.cur = 1 - self.cur;
        self.shown = true;
        out.extend_from_slice(self.status(ws).as_bytes());
        out.extend_from_slice(b"\x1b[?2026l");
        self.ctx.write(&out);
        if filter == Filter::Lanczos {
            self.last_render = t0.elapsed();
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
        self.drop_proxy(&mut out);
        for id in self.ids {
            self.ctx.kitty.delete(&mut out, id);
        }
        out.extend_from_slice(b"\x1b[0m\x1b[2J");
        self.ctx.write(&out);
        self.shown = false;
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
        remote: ctx.kitty.compress && ctx.kitty.transfer == Transfer::Direct,
        proxy: None,
        on_proxy: false,
    };
    // Over ssh, zoom / pan by re-placing an uploaded proxy (pointless for
    // animations, which re-send every frame anyway).
    let use_proxy = v.remote && !animated;
    let mut ws = ctx.size();
    v.reset(ws);
    ctx.write(b"\x1b[0m\x1b[2J");
    let mut dirty = true;
    let mut interactive = false;
    // The last events only zoomed or panned.
    let mut moved = false;

    let nav = loop {
        if dirty {
            if v.help {
                v.clear();
                ctx.write(
                    help_screen(&ctx.theme, ws.cols, ws.rows, "shw — image viewer", HELP)
                        .as_bytes(),
                );
            } else if moved && v.remote {
                // Over ssh, never send pixels per wheel notch / drag step: move the
                // proxy (or wait), and send one sharp frame once input pauses.
                if !(use_proxy && v.show_proxy(ws)) {
                    // Nothing new on screen until the sharp frame.
                    v.on_proxy = false;
                }
                v.hq_due = Some(Instant::now() + REMOTE_SETTLE);
            } else {
                // While zooming / dragging, draw with a fast filter; refine when idle.
                let fast = !v.remote && interactive && v.last_render > Duration::from_millis(20);
                v.render(
                    ws,
                    if fast {
                        Filter::Bilinear
                    } else {
                        Filter::Lanczos
                    },
                );
                v.hq_due = fast.then(|| Instant::now() + Duration::from_millis(120));
            }
            if animated && !v.paused && v.next_frame.is_none() {
                v.next_frame = Some(Instant::now() + v.pic.frames[v.frame].delay);
            }
            dirty = false;
            interactive = false;
            moved = false;
        }

        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(3600);
        if let Some(t) = v.hq_due {
            deadline = deadline.min(t);
        }
        if let (Some(t), false, false) = (v.next_frame, v.paused, v.help) {
            deadline = deadline.min(t);
        }
        let evs = read_events(deadline.saturating_duration_since(now))?;
        let now = Instant::now();
        if evs.is_empty() {
            if v.hq_due.is_some_and(|t| now >= t) {
                v.hq_due = None;
                if !v.on_proxy || v.needs_refine() {
                    dirty = true;
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
    use super::grid_window;

    #[test]
    fn grid_window_centres_and_clamps() {
        // grid smaller than the view: centred, all cells
        assert_eq!(grid_window(10.0, 20.0, 5.0), (0, 10, 5));
        // larger: window around the centre
        assert_eq!(grid_window(100.0, 20.0, 50.0), (40, 20, 0));
        // clamped at both ends
        assert_eq!(grid_window(100.0, 20.0, 3.0), (0, 20, 0));
        assert_eq!(grid_window(100.0, 20.0, 99.0), (80, 20, 0));
    }
}
