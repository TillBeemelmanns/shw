//! Scrollable document viewer (markdown, code, text) with inline images,
//! search and mouse wheel scrolling; plus the non-interactive printer.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};

use crate::app::{is_quit, pos_label, read_events, Ctx, Nav};
use crate::doc::{render_text, Doc, Row};
use crate::kitty::placeholder_row;
use crate::term::WinSize;
use crate::ui::{help_screen, status_bar, write_spans};

pub trait Source {
    fn layout(&mut self, ctx: &Ctx, ws: WinSize) -> Doc;
    fn badge(&self) -> &str;
    fn title(&self) -> String;
}

const HELP: &[(&str, &str)] = &[
    ("j / k / ↓ / ↑", "scroll line"),
    ("wheel", "scroll"),
    ("space / b", "page down / up"),
    ("d / u", "half page down / up"),
    ("g / G", "top / bottom"),
    ("/", "search"),
    ("n / N", "next / previous match"),
    ("Tab / S-Tab", "next / previous file"),
    ("q / Esc", "quit"),
];

fn free_images(ctx: &Ctx, doc: &Doc, out: &mut Vec<u8>) {
    for im in &doc.images {
        if im.sent {
            ctx.kitty.delete(out, im.id);
        }
    }
}

struct State {
    top: usize,
    search: Option<String>,
    input: Option<String>,
    message: Option<String>,
    help: bool,
}

pub fn run(ctx: &Ctx, src: &mut dyn Source, pos: (usize, usize)) -> Result<Nav> {
    let mut ws = ctx.size();
    let mut doc = src.layout(ctx, ws);
    let mut st = State {
        top: 0,
        search: None,
        input: None,
        message: None,
        help: false,
    };
    let mut dirty = true;
    let nav = loop {
        let view = ws.rows.saturating_sub(1).max(1) as usize;
        let max_top = doc.rows.len().saturating_sub(view);
        st.top = st.top.min(max_top);
        if dirty {
            draw(ctx, &mut doc, &st, ws, src, pos);
            dirty = false;
        }
        let evs = read_events(Duration::from_secs(3600))?;
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
                    let mut out = Vec::new();
                    free_images(ctx, &doc, &mut out);
                    ctx.write(&out);
                    let frac = st.top as f64 / doc.rows.len().max(1) as f64;
                    ws = ctx.size();
                    doc = src.layout(ctx, ws);
                    st.top = (frac * doc.rows.len() as f64) as usize;
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
    free_images(ctx, &doc, &mut out);
    ctx.kitty.clear_placements(&mut out);
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

fn draw(ctx: &Ctx, doc: &mut Doc, st: &State, ws: WinSize, src: &dyn Source, pos: (usize, usize)) {
    let t = &ctx.theme;
    let (cw, ch) = ws.cell();
    let view = ws.rows.saturating_sub(1).max(1) as usize;
    let width = if doc.width > 0 {
        doc.width
    } else {
        ws.cols as usize
    };
    let m = doc.margin as usize;
    let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"\x1b[?2026h\x1b[H\x1b[0m\x1b[2J");
    if !ctx.kitty.placeholders {
        ctx.kitty.clear_placements(&mut out);
    }
    if st.help {
        out.extend_from_slice(
            help_screen(t, ws.cols, ws.rows, "shw — document viewer", HELP).as_bytes(),
        );
        out.extend_from_slice(b"\x1b[?2026l");
        ctx.write(&out);
        return;
    }
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
                render_text(&mut line, spans, *fill, *scale, width, hl);
                out.extend_from_slice(line.as_bytes());
            }
            Row::Cont => {}
            Row::Image { img, row, prefix } => {
                let (img, row) = (*img, *row);
                line.push_str(&format!("\x1b[{};{}H", r + 1, m + 1));
                write_spans(&mut line, prefix, 1, None);
                out.extend_from_slice(line.as_bytes());
                let im = &mut doc.images[img];
                if ctx.kitty.placeholders {
                    if !im.sent {
                        ctx.kitty.transmit(
                            &mut out,
                            &im.raster,
                            &format!("a=T,U=1,i={},c={},r={}", im.id, im.cols, im.rows),
                        );
                        im.sent = true;
                    }
                    let s = format!(
                        "\x1b[{};{}H{}",
                        r + 1,
                        im.col as usize + m + 1,
                        placeholder_row(im.id, row, im.cols)
                    );
                    out.extend_from_slice(s.as_bytes());
                } else if i == im.start.max(st.top) {
                    if !im.sent {
                        ctx.kitty
                            .transmit(&mut out, &im.raster, &format!("a=t,i={}", im.id));
                        im.sent = true;
                    }
                    let vis_rows = (im.rows - row).min((view - r) as u32);
                    let y = (row as f64 * ch).round() as u32;
                    let y = y.min(im.raster.h.saturating_sub(1));
                    let y_end = (((row + vis_rows) as f64) * ch)
                        .round()
                        .min(im.raster.h as f64) as u32;
                    let h = y_end.saturating_sub(y).max(1);
                    let _ = cw;
                    let s = format!(
                        "\x1b[{};{}H\x1b_Ga=p,i={},p=1,x=0,y={y},w={},h={h},C=1,q=2\x1b\\",
                        r + 1,
                        im.col as usize + m + 1,
                        im.id,
                        im.raster.w
                    );
                    if ctx.kitty.tmux {
                        out.extend_from_slice(crate::kitty::tmux_wrap(&s).as_bytes());
                    } else {
                        out.extend_from_slice(s.as_bytes());
                    }
                }
            }
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
    let right = format!(
        "{}/{}  {pct}%  ? help",
        (st.top + 1).min(total.max(1)),
        total
    );
    out.extend_from_slice(status_bar(t, ws.rows, ws.cols, src.badge(), &left, &right).as_bytes());
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
                render_text(&mut line, spans, *fill, *scale, width, None);
                line.push('\n');
                out.extend_from_slice(line.as_bytes());
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
                        write_spans(&mut line, &prefix, 1, None);
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
                        write_spans(&mut line, &prefix, 1, None);
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
