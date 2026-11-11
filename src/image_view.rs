//! Interactive image viewer with pixel-exact rendering, zoom (mouse wheel at
//! the pointer, + / -), panning (drag, hjkl) and animated GIF/WebP/APNG.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::app::{is_quit, pos_label, read_events, Ctx, Nav, CHROME_ROWS};
use crate::kitty::{next_id, placeholder_row};
use crate::raster::{render_svg, resize, resize_region, Filter, Picture, Raster};
use crate::term::WinSize;
use crate::ui::{help_screen, hint_bar, status_bar};

const HELP: &[(&str, &str)] = &[
    ("wheel / + / -", "zoom (at pointer)"),
    ("drag / h j k l", "pan"),
    ("0 / f", "fit to window"),
    ("1", "actual pixels (100%)"),
    ("r / R", "rotate ↻ / ↺"),
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
    ("space", "next/pause"),
];

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

    fn render(&mut self, ws: WinSize, filter: Filter) {
        let t0 = Instant::now();
        let (cw, ch) = ws.cell();
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
        let (dw, dh) = (
            disp_w.round().max(1.0) as u32,
            disp_h.round().max(1.0) as u32,
        );

        let (level, scale) = self.source_for(z);
        let raster: Raster = if let Some(tree) = &self.pic.svg {
            render_svg(tree, crop, dw, dh)
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
                src.clone()
            } else if f == Filter::Lanczos && crop.2 >= iw - 0.5 && crop.3 >= ih - 0.5 {
                resize(src, dw, dh)
            } else {
                resize_region(src, crop, dw, dh, f)
            }
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
        let right = format!(
            "{}×{}  {}  {}{}  {:.0}%",
            self.pic.width,
            self.pic.height,
            self.pic.format,
            self.meta,
            anim,
            self.zoom * 100.0
        );
        let mut s = status_bar(&self.ctx.theme, ws.rows - 1, ws.cols, "IMG", &left, &right);
        let hints: &[(&str, &str)] = if self.pic.is_animated() {
            HINTS
        } else {
            &HINTS[..HINTS.len() - 1]
        };
        s.push_str(&hint_bar(&self.ctx.theme, ws.rows, ws.cols, hints));
        s
    }

    fn clear(&mut self) {
        let mut out = Vec::new();
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
    };
    let mut ws = ctx.size();
    v.reset(ws);
    ctx.write(b"\x1b[0m\x1b[2J");
    let mut dirty = true;
    let mut interactive = false;

    let nav = loop {
        if dirty {
            if v.help {
                v.clear();
                ctx.write(
                    help_screen(&ctx.theme, ws.cols, ws.rows, "shw — image viewer", HELP)
                        .as_bytes(),
                );
            } else {
                // While zooming / dragging, draw with a fast filter; refine when idle.
                let fast = interactive && v.last_render > Duration::from_millis(20);
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
                dirty = true;
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
                            interactive = true;
                        }
                        MouseEventKind::ScrollDown => {
                            v.zoom_by(ws, 1.0 / WHEEL_STEP, anchor);
                            dirty = true;
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
    let id = next_id();
    let mut doc = crate::doc::Doc::default();
    let _ = id;
    doc.image(raster, 0, cols, rows, Vec::new());
    crate::pager::print_doc(ctx, &mut doc, ws.cols as usize);
}
