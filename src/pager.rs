//! Scrollable document viewer (markdown, code, text) with inline images,
//! search and mouse wheel scrolling; plus the non-interactive printer.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};

use crate::app::{
    is_quit, pos_label, read_events, Ctx, Nav, CHROME_ROWS, MOUSE_TO_APP, MOUSE_TO_TERMINAL,
};
use crate::doc::{render_text, Doc, DocImage, Row};
use crate::kitty::placeholder_row;
use crate::term::WinSize;
use crate::ui::{
    help_screen, hint_bar, set_theme_override, status_bar, theme_override, write_spans, Theme,
};

pub trait Source {
    fn layout(&mut self, ctx: &Ctx, ws: WinSize) -> Doc;
    /// True if content loaded in the background has arrived since the last
    /// layout: the document should be laid out again.
    fn poll(&mut self) -> bool {
        false
    }
    /// Number of things still loading in the background.
    fn loading(&self) -> usize {
        0
    }
    fn badge(&self) -> &str;
    fn title(&self) -> String;
}

const HELP: &[(&str, &str)] = &[
    ("j / k / ↓ / ↑", "scroll line"),
    ("wheel", "scroll"),
    ("mouse drag", "select text (copy with ⌘C / Ctrl+Shift+C)"),
    ("space / b", "page down / up"),
    ("d / u", "half page down / up"),
    ("g / G", "top / bottom"),
    ("/", "search"),
    ("n / N", "next / previous match"),
    ("t", "toggle dark / bright theme"),
    ("Tab / S-Tab", "next / previous file"),
    ("q / Esc", "quit"),
];

const HINTS: &[(&str, &str)] = &[
    ("j/k", "scroll"),
    ("space/b", "page"),
    ("d/u", "½ page"),
    ("g/G", "top/end"),
    ("/", "search"),
    ("n/N", "match"),
    ("t", "theme"),
    ("Tab", "next file"),
];

/// Free every image the terminal holds for this document. Image data is
/// always re-sent when it becomes visible again, so nothing relies on the
/// terminal keeping images that have no placement (Ghostty frees those).
fn free_images(ctx: &Ctx, doc: &mut Doc, out: &mut Vec<u8>) {
    for im in &mut doc.images {
        if im.sent {
            ctx.kitty.delete(out, im.id);
            im.sent = false;
        }
    }
}

/// Transmit the image data unless the terminal already has it.
fn send_image(ctx: &Ctx, out: &mut Vec<u8>, im: &mut DocImage) {
    if im.sent {
        return;
    }
    let control = if ctx.kitty.placeholders {
        format!("a=T,U=1,i={},c={},r={}", im.id, im.cols, im.rows)
    } else {
        format!("a=t,i={}", im.id)
    };
    ctx.kitty.transmit(out, &im.raster, &control);
    im.sent = true;
}

/// Show the image from its row `row` on, at screen row `r` (0-based) of a
/// view with left margin `m`, cropped to the `room` rows left on screen.
fn place_image(
    ctx: &Ctx,
    out: &mut Vec<u8>,
    im: &mut DocImage,
    (r, m): (usize, usize),
    row: u32,
    room: usize,
    ch: f64,
) {
    send_image(ctx, out, im);
    let col = im.col as usize + m + 1;
    let vis_rows = (im.rows - row).min(room as u32);
    if ctx.kitty.placeholders {
        for k in 0..vis_rows {
            let cells = placeholder_row(im.id, row + k, im.cols);
            out.extend_from_slice(format!("\x1b[{};{col}H{cells}", r + k as usize + 1).as_bytes());
        }
        return;
    }
    let y = ((row as f64 * ch).round() as u32).min(im.raster.h.saturating_sub(1));
    let y_end = (((row + vis_rows) as f64) * ch)
        .round()
        .min(im.raster.h as f64) as u32;
    let h = y_end.saturating_sub(y).max(1);
    // same placement id: the terminal moves/re-crops the existing placement
    let s = format!(
        "\x1b[{};{col}H\x1b_Ga=p,i={},p=1,x=0,y={y},w={},h={h},C=1,q=2\x1b\\",
        r + 1,
        im.id,
        im.raster.w
    );
    if ctx.kitty.tmux {
        out.extend_from_slice(crate::kitty::tmux_wrap(&s).as_bytes());
    } else {
        out.extend_from_slice(s.as_bytes());
    }
}

/// Lay the document out again (new size, theme or content) and keep the
/// reading position. The old images are freed with the next frame.
fn relayout(ctx: &Ctx, src: &mut dyn Source, doc: &mut Doc, st: &mut State, ws: WinSize) {
    free_images(ctx, doc, &mut st.stale);
    let position = doc.locate(st.top);
    let fraction = st.top as f64 / doc.rows.len().max(1) as f64;
    *doc = src.layout(ctx, ws);
    st.top = match position {
        Some(p) => doc.row_of(p),
        None => (fraction * doc.rows.len() as f64) as usize,
    };
}

struct State {
    top: usize,
    search: Option<String>,
    input: Option<String>,
    message: Option<String>,
    help: bool,
    /// Escape sequences that free the images of a replaced layout.
    stale: Vec<u8>,
}

pub fn run(ctx: &Ctx, src: &mut dyn Source, pos: (usize, usize)) -> Result<Nav> {
    // Local copy so the theme can be toggled; the choice persists across files.
    let mut ctx = ctx.clone();
    if let Some(dark) = theme_override() {
        ctx.theme = Theme::for_bg(dark, ctx.caps.dark);
    }
    let ctx = &mut ctx;
    // Native text selection: the terminal handles the mouse in documents.
    ctx.write(MOUSE_TO_TERMINAL);
    let mut ws = ctx.size();
    let mut doc = src.layout(ctx, ws);
    let mut first_draw = true;
    let mut st = State {
        top: 0,
        search: None,
        input: None,
        message: None,
        help: false,
        stale: Vec::new(),
    };
    let mut dirty = true;
    let nav = loop {
        let view = ws.rows.saturating_sub(CHROME_ROWS).max(1) as usize;
        let max_top = doc.rows.len().saturating_sub(view);
        st.top = st.top.min(max_top);
        if dirty {
            draw(ctx, &mut doc, &mut st, ws, src, pos);
            if std::mem::take(&mut first_draw) {
                crate::app::trace("first frame drawn");
            }
            dirty = false;
        }
        // while pictures load in the background, look for them regularly
        let timeout = match src.loading() {
            0 => Duration::from_secs(3600),
            _ => Duration::from_millis(40),
        };
        let evs = read_events(timeout)?;
        if src.poll() {
            relayout(ctx, src, &mut doc, &mut st, ws);
            dirty = true;
        }
        let mut nav = None;
        for ev in evs {
            dirty = true;
            if let Some(input) = &mut st.input {
                if let Event::Key(k) = &ev {
                    match k.code {
                        KeyCode::Esc => st.input = None,
                        KeyCode::Enter => {
                            let q = st.input.take().unwrap_or_default();
                            if !q.is_empty() {
                                st.search = Some(q);
                                find(&doc, &mut st, true, true);
                            }
                        }
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    }
                }
                continue;
            }
            if st.help {
                if let Event::Key(_) = ev {
                    st.help = false;
                }
                continue;
            }
            st.message = None;
            match ev {
                Event::Resize(..) => {
                    ws = ctx.size();
                    relayout(ctx, src, &mut doc, &mut st, ws);
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollDown => st.top = (st.top + 3).min(max_top),
                    MouseEventKind::ScrollUp => st.top = st.top.saturating_sub(3),
                    _ => dirty = false,
                },
                Event::Key(k) => {
                    if is_quit(&ev) {
                        if k.code == KeyCode::Esc && st.search.is_some() {
                            st.search = None;
                            continue;
                        }
                        nav = Some(Nav::Quit);
                        break;
                    }
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('j') | KeyCode::Down | KeyCode::Enter => {
                            st.top = (st.top + 1).min(max_top)
                        }
                        KeyCode::Char('k') | KeyCode::Up => st.top = st.top.saturating_sub(1),
                        KeyCode::Char('d') if ctrl => st.top = (st.top + view / 2).min(max_top),
                        KeyCode::Char('u') if ctrl => st.top = st.top.saturating_sub(view / 2),
                        KeyCode::Char('f') if ctrl => st.top = (st.top + view).min(max_top),
                        KeyCode::Char('b') if ctrl => st.top = st.top.saturating_sub(view),
                        KeyCode::Char('d') => st.top = (st.top + view / 2).min(max_top),
                        KeyCode::Char('u') => st.top = st.top.saturating_sub(view / 2),
                        KeyCode::Char(' ') | KeyCode::PageDown | KeyCode::Char('f') => {
                            st.top = (st.top + view.saturating_sub(1)).min(max_top)
                        }
                        KeyCode::Char('b') | KeyCode::PageUp => {
                            st.top = st.top.saturating_sub(view.saturating_sub(1))
                        }
                        KeyCode::Char('g') | KeyCode::Home => st.top = 0,
                        KeyCode::Char('G') | KeyCode::End => st.top = max_top,
                        KeyCode::Char('/') => st.input = Some(String::new()),
                        KeyCode::Char('n') => find(&doc, &mut st, true, false),
                        KeyCode::Char('N') => find(&doc, &mut st, false, false),
                        KeyCode::Char('?') => st.help = true,
                        KeyCode::Char('t') => {
                            let dark = !ctx.theme.dark;
                            set_theme_override(dark);
                            ctx.theme = Theme::for_bg(dark, ctx.caps.dark);
                            relayout(ctx, src, &mut doc, &mut st, ws);
                            st.message = Some(if dark {
                                "dark theme".into()
                            } else {
                                "bright theme".into()
                            });
                        }
                        KeyCode::Tab | KeyCode::Char(']') => {
                            nav = Some(Nav::Next);
                            break;
                        }
                        KeyCode::BackTab | KeyCode::Char('[') => {
                            nav = Some(Nav::Prev);
                            break;
                        }
                        _ => dirty = false,
                    }
                }
                _ => dirty = false,
            }
        }
        if let Some(n) = nav {
            break n;
        }
    };
    let mut out = Vec::new();
    free_images(ctx, &mut doc, &mut out);
    out.extend_from_slice(b"\x1b[0m\x1b[2J");
    out.extend_from_slice(MOUSE_TO_APP);
    ctx.write(&out);
    Ok(nav)
}

fn find(doc: &Doc, st: &mut State, forward: bool, include_current: bool) {
    let Some(q) = st.search.clone().map(|q| q.to_ascii_lowercase()) else {
        return;
    };
    let n = doc.rows.len();
    if n == 0 {
        return;
    }
    let start = if include_current {
        st.top
    } else if forward {
        st.top + 1
    } else {
        st.top + n - 1
    };
    for k in 0..n {
        let i = if forward {
            (start + k) % n
        } else {
            (start + n - k % n) % n
        };
        if doc.plain(i).to_ascii_lowercase().contains(&q) {
            st.top = i;
            return;
        }
    }
    st.message = Some(format!("Pattern not found: {q}"));
}

fn draw(
    ctx: &Ctx,
    doc: &mut Doc,
    st: &mut State,
    ws: WinSize,
    src: &dyn Source,
    pos: (usize, usize),
) {
    let t = &ctx.theme;
    let (_, ch) = ws.cell();
    let view = ws.rows.saturating_sub(CHROME_ROWS).max(1) as usize;
    let width = if doc.width > 0 {
        doc.width
    } else {
        ws.cols as usize
    };
    let m = doc.margin as usize;
    let page = t.page;
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"\x1b[?2026h\x1b[0m");
    out.append(&mut st.stale);
    if st.help {
        // the help screen clears the display: drop images, they are re-sent afterwards
        free_images(ctx, doc, &mut out);
        out.extend_from_slice(
            help_screen(t, ws.cols, ws.rows, "shw — document viewer", HELP).as_bytes(),
        );
        out.extend_from_slice(b"\x1b[?2026l");
        ctx.write(&out);
        return;
    }

    // Images that scrolled out of view are freed (and re-sent if they come back);
    // visible ones keep their data and get their placement replaced in place.
    let (top, bottom) = (st.top, st.top + view);
    for im in &mut doc.images {
        let visible = im.start < bottom && im.start + im.rows as usize > top;
        if im.sent && !visible {
            ctx.kitty.delete(&mut out, im.id);
            im.sent = false;
        }
    }

    // Clear the text area line by line (never ED 2: some terminals drop image
    // data on a full clear). All lines are cleared before drawing so that
    // double-height text is not erased by the next line's clear.
    let clear_sgr = match page {
        Some((fg, bg)) => format!(
            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
            fg.0, fg.1, fg.2, bg.0, bg.1, bg.2
        ),
        None => String::new(),
    };
    for r in 0..view {
        out.extend_from_slice(format!("\x1b[{};1H{clear_sgr}\x1b[2K", r + 1).as_bytes());
    }
    out.extend_from_slice(b"\x1b[0m");

    let hl = st.search.as_deref().map(|q| (q, t.search_bg));
    let mut line = String::new();
    for r in 0..view {
        let i = st.top + r;
        if i >= doc.rows.len() {
            break;
        }
        line.clear();
        match &doc.rows[i] {
            Row::Text { spans, fill, scale } => {
                if *scale > 1 && r + 1 >= view {
                    continue;
                }
                line.push_str(&format!("\x1b[{};{}H", r + 1, m + 1));
                render_text(&mut line, spans, *fill, *scale, width, hl, page);
                out.extend_from_slice(line.as_bytes());
            }
            Row::Cont => {}
            Row::Image { img, row, prefix } => {
                let (img, row) = (*img, *row);
                line.push_str(&format!("\x1b[{};{}H", r + 1, m + 1));
                write_spans(&mut line, prefix, 1, None, page);
                out.extend_from_slice(line.as_bytes());
                let im = &mut doc.images[img];
                if i == im.start.max(st.top) {
                    place_image(ctx, &mut out, im, (r, m), row, view - r, ch);
                }
            }
        }
    }

    // Overlays (checkboxes, pictures in table cells) are drawn over their text rows.
    for im in doc.images.iter_mut().filter(|im| im.overlay) {
        let first = im.start.max(top);
        if first < bottom && first < im.start + im.rows as usize {
            let (r, row) = (first - top, (first - im.start) as u32);
            place_image(ctx, &mut out, im, (r, m), row, view - r, ch);
        }
    }

    // status bar
    let total = doc.rows.len();
    let pct = if total <= view {
        100
    } else {
        ((st.top + view).min(total) * 100) / total
    };
    let left = if let Some(inp) = &st.input {
        format!("/{inp}█")
    } else if let Some(m) = &st.message {
        m.clone()
    } else {
        format!("{}{}", src.title(), pos_label(pos))
    };
    let loading = match src.loading() {
        0 => String::new(),
        n => format!("loading {n} image{}…  ", if n == 1 { "" } else { "s" }),
    };
    let right = format!(
        "{loading}{}/{}  {pct}%",
        (st.top + 1).min(total.max(1)),
        total
    );
    out.extend_from_slice(
        status_bar(t, ws.rows - 1, ws.cols, src.badge(), &left, &right).as_bytes(),
    );
    out.extend_from_slice(hint_bar(t, ws.rows, ws.cols, HINTS).as_bytes());
    out.extend_from_slice(b"\x1b[?2026l");
    ctx.write(&out);
}

/// Print a document to stdout in the normal screen flow (no alternate screen).
pub fn print_doc(ctx: &Ctx, doc: &mut Doc, width: usize) {
    let mut out: Vec<u8> = Vec::new();
    let mut line = String::new();
    let mut i = 0;
    let width = if doc.width > 0 { doc.width } else { width };
    while i < doc.rows.len() {
        line.clear();
        match &doc.rows[i] {
            Row::Text { spans, fill, scale } => {
                render_text(&mut line, spans, *fill, *scale, width, None, None);
                out.extend_from_slice(line.as_bytes());
                // overlays: placeholder cells row by row; otherwise the image is
                // drawn from its last row, once the rows it covers exist
                let over = |im: &&mut DocImage| {
                    im.overlay && (im.start..im.start + im.rows as usize).contains(&i)
                };
                for im in doc.images.iter_mut().filter(over) {
                    let row = (i - im.start) as u32;
                    out.extend_from_slice(format!("\x1b[{}G", im.col + 1).as_bytes());
                    if ctx.kitty.placeholders {
                        send_image(ctx, &mut out, im);
                        out.extend_from_slice(placeholder_row(im.id, row, im.cols).as_bytes());
                    } else if row + 1 == im.rows {
                        if row > 0 {
                            out.extend_from_slice(format!("\x1b[{row}A").as_bytes());
                        }
                        ctx.kitty
                            .transmit(&mut out, &im.raster, &format!("a=T,i={},C=1", im.id));
                        if row > 0 {
                            out.extend_from_slice(format!("\x1b[{row}B").as_bytes());
                        }
                        im.sent = true;
                    }
                }
                out.push(b'\n');
            }
            Row::Cont => out.push(b'\n'),
            Row::Image { img, prefix, .. } => {
                let im = &mut doc.images[*img];
                let prefix = prefix.clone();
                if ctx.kitty.placeholders {
                    ctx.kitty.transmit(
                        &mut out,
                        &im.raster,
                        &format!("a=T,U=1,i={},c={},r={}", im.id, im.cols, im.rows),
                    );
                    for r in 0..im.rows {
                        line.clear();
                        write_spans(&mut line, &prefix, 1, None, None);
                        line.push_str(&format!(
                            "\x1b[{}G{}\n",
                            im.col + 1,
                            placeholder_row(im.id, r, im.cols)
                        ));
                        out.extend_from_slice(line.as_bytes());
                    }
                } else {
                    // Reserve the rows first (scrolling if needed), then go back up and draw.
                    for _ in 0..im.rows {
                        line.clear();
                        write_spans(&mut line, &prefix, 1, None, None);
                        line.push('\n');
                        out.extend_from_slice(line.as_bytes());
                    }
                    out.extend_from_slice(
                        format!("\x1b[{}A\x1b[{}G", im.rows, im.col + 1).as_bytes(),
                    );
                    ctx.kitty
                        .transmit(&mut out, &im.raster, &format!("a=T,i={},C=1", im.id));
                    out.extend_from_slice(format!("\x1b[{}B\r", im.rows).as_bytes());
                }
                im.sent = true;
                i += im.rows as usize;
                continue;
            }
        }
        i += 1;
    }
    ctx.write(&out);
}
