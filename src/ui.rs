//! Colours, styled text spans and status bar rendering.

use std::rc::Rc;

use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

/// GitHub Primer inspired palette for dark and light backgrounds.
#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    pub muted: Rgb,
    pub border: Rgb,
    pub link: Rgb,
    pub code_bg: Rgb,
    pub inline_code_bg: Rgb,
    pub bar_bg: Rgb,
    pub bar_fg: Rgb,
    pub accent: Rgb,
    pub note: Rgb,
    pub tip: Rgb,
    pub important: Rgb,
    pub warning: Rgb,
    pub caution: Rgb,
    pub search_bg: Rgb,
    pub syntect: &'static str,
}

impl Theme {
    pub fn new(dark: bool) -> Theme {
        if dark {
            Theme {
                dark,
                muted: Rgb::hex(0x9198a1),
                border: Rgb::hex(0x3d444d),
                link: Rgb::hex(0x4493f8),
                code_bg: Rgb::hex(0x151b23),
                inline_code_bg: Rgb::hex(0x2a313c),
                bar_bg: Rgb::hex(0x212830),
                bar_fg: Rgb::hex(0xd1d7e0),
                accent: Rgb::hex(0x4493f8),
                note: Rgb::hex(0x4493f8),
                tip: Rgb::hex(0x3fb950),
                important: Rgb::hex(0xab7df8),
                warning: Rgb::hex(0xd29922),
                caution: Rgb::hex(0xf85149),
                search_bg: Rgb::hex(0x9e6a03),
                syntect: "base16-ocean.dark",
            }
        } else {
            Theme {
                dark,
                muted: Rgb::hex(0x59636e),
                border: Rgb::hex(0xd1d9e0),
                link: Rgb::hex(0x0969da),
                code_bg: Rgb::hex(0xf6f8fa),
                inline_code_bg: Rgb::hex(0xeff1f3),
                bar_bg: Rgb::hex(0xe6eaef),
                bar_fg: Rgb::hex(0x1f2328),
                accent: Rgb::hex(0x0969da),
                note: Rgb::hex(0x0969da),
                tip: Rgb::hex(0x1a7f37),
                important: Rgb::hex(0x8250df),
                warning: Rgb::hex(0x9a6700),
                caution: Rgb::hex(0xd1242f),
                search_bg: Rgb::hex(0xfff8c5),
                syntect: "InspiredGitHub",
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub dim: bool,
    pub link: Option<Rc<str>>,
}

impl Style {
    pub fn fg(c: Rgb) -> Style {
        Style {
            fg: Some(c),
            ..Default::default()
        }
    }

    pub fn sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        if self.bold {
            out.push_str(";1");
        }
        if self.dim {
            out.push_str(";2");
        }
        if self.italic {
            out.push_str(";3");
        }
        if self.underline {
            out.push_str(";4");
        }
        if self.strike {
            out.push_str(";9");
        }
        if let Some(Rgb(r, g, b)) = self.fg {
            out.push_str(&format!(";38;2;{r};{g};{b}"));
        }
        if let Some(Rgb(r, g, b)) = self.bg {
            out.push_str(&format!(";48;2;{r};{g};{b}"));
        }
        out.push('m');
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Span {
        Span {
            text: text.into(),
            style,
        }
    }
    pub fn plain(text: impl Into<String>) -> Span {
        Span {
            text: text.into(),
            style: Style::default(),
        }
    }
    pub fn width(&self) -> usize {
        self.text.width()
    }
}

pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.width()).sum()
}

/// Write spans as ANSI text. `scale` > 1 uses the kitty text sizing protocol.
pub fn write_spans(out: &mut String, spans: &[Span], scale: u8, highlight: Option<(&str, Rgb)>) {
    let owned;
    let spans = match highlight {
        Some((q, bg)) if !q.is_empty() => {
            owned = highlight_spans(spans, q, bg);
            &owned[..]
        }
        _ => spans,
    };
    let mut cur_link: Option<&str> = None;
    for s in spans {
        let link = s.style.link.as_deref();
        if link != cur_link {
            match link {
                Some(url) => out.push_str(&format!("\x1b]8;;{url}\x1b\\")),
                None => out.push_str("\x1b]8;;\x1b\\"),
            }
            cur_link = link;
        }
        s.style.sgr(out);
        if scale > 1 {
            // OSC 66 payloads are limited to 4096 bytes; split long spans.
            let mut buf = String::new();
            for ch in s.text.chars() {
                buf.push(ch);
                if buf.len() > 3000 {
                    out.push_str(&format!("\x1b]66;s={scale};{buf}\x07"));
                    buf.clear();
                }
            }
            if !buf.is_empty() {
                out.push_str(&format!("\x1b]66;s={scale};{buf}\x07"));
            }
        } else {
            out.push_str(&s.text);
        }
    }
    if cur_link.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
    out.push_str("\x1b[0m");
}

/// Split spans so that ASCII-case-insensitive matches of `q` get a background.
fn highlight_spans(spans: &[Span], q: &str, bg: Rgb) -> Vec<Span> {
    let full: String = spans.iter().map(|s| s.text.as_str()).collect();
    let lower = full.to_ascii_lowercase();
    let ql = q.to_ascii_lowercase();
    let mut marks = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find(&ql) {
        marks.push((from + i, from + i + ql.len()));
        from += i + ql.len().max(1);
        if from >= lower.len() {
            break;
        }
    }
    if marks.is_empty() {
        return spans.to_vec();
    }
    let mut out = Vec::new();
    let mut pos = 0;
    for s in spans {
        let (start, end) = (pos, pos + s.text.len());
        let mut cut = start;
        for &(a, b) in &marks {
            if b <= cut || a >= end {
                continue;
            }
            let a = a.max(cut);
            let b = b.min(end);
            if a > cut {
                out.push(Span::new(&full[cut..a], s.style.clone()));
            }
            let mut st = s.style.clone();
            st.bg = Some(bg);
            st.bold = true;
            out.push(Span::new(&full[a..b], st));
            cut = b;
        }
        if cut < end {
            out.push(Span::new(&full[cut..end], s.style.clone()));
        }
        pos = end;
    }
    out
}

/// Truncate a string to `max` display columns, adding an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Render the bottom status bar at screen row `row` (1-based).
pub fn status_bar(
    theme: &Theme,
    row: u16,
    cols: u16,
    badge: &str,
    left: &str,
    right: &str,
) -> String {
    let cols = cols as usize;
    let mut s = format!("\x1b[{row};1H\x1b[0m\x1b[2K");
    let badge_txt = format!(" {badge} ");
    let bw = badge_txt.width();
    let rw = right.width();
    let avail = cols.saturating_sub(bw + rw + 3);
    let left = truncate(left, avail);
    let pad = cols.saturating_sub(bw + 1 + left.width() + rw + 1);
    let mut b = String::new();
    Style {
        fg: Some(if theme.dark {
            Rgb::hex(0x0d1117)
        } else {
            Rgb::hex(0xffffff)
        }),
        bg: Some(theme.accent),
        bold: true,
        ..Default::default()
    }
    .sgr(&mut b);
    b.push_str(&badge_txt);
    Style {
        fg: Some(theme.bar_fg),
        bg: Some(theme.bar_bg),
        ..Default::default()
    }
    .sgr(&mut b);
    b.push(' ');
    b.push_str(&left);
    b.push_str(&" ".repeat(pad));
    Style {
        fg: Some(theme.muted),
        bg: Some(theme.bar_bg),
        ..Default::default()
    }
    .sgr(&mut b);
    b.push_str(right);
    b.push(' ');
    b.push_str("\x1b[0m");
    s.push_str(&b);
    s
}

/// Render a centered help box listing key bindings (full screen, no image).
pub fn help_screen(
    theme: &Theme,
    cols: u16,
    rows: u16,
    title: &str,
    keys: &[(&str, &str)],
) -> String {
    let mut s = String::from("\x1b[0m\x1b[2J");
    let kw = keys.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
    let dw = keys.iter().map(|(_, d)| d.width()).max().unwrap_or(0);
    let bw = (kw + dw + 7).max(title.width() + 4);
    let bh = keys.len() + 4;
    let x0 = (cols as usize).saturating_sub(bw) / 2 + 1;
    let y0 = (rows as usize).saturating_sub(bh) / 2 + 1;
    let mut border = String::new();
    Style::fg(theme.border).sgr(&mut border);
    s.push_str(&format!("\x1b[{y0};{x0}H{border}╭{}╮", "─".repeat(bw - 2)));
    let mut t = String::new();
    Style {
        fg: Some(theme.accent),
        bold: true,
        ..Default::default()
    }
    .sgr(&mut t);
    s.push_str(&format!(
        "\x1b[{};{x0}H{border}│{t} {title}{}{border}│",
        y0 + 1,
        " ".repeat(bw - 3 - title.width())
    ));
    s.push_str(&format!(
        "\x1b[{};{x0}H{border}│{}│",
        y0 + 2,
        " ".repeat(bw - 2)
    ));
    for (i, (k, d)) in keys.iter().enumerate() {
        let mut ks = String::new();
        Style {
            fg: Some(theme.link),
            bold: true,
            ..Default::default()
        }
        .sgr(&mut ks);
        let mut ds = String::new();
        Style::default().sgr(&mut ds);
        let line = format!(
            " {ks}{k}{}  {ds}{d}{}",
            " ".repeat(kw - k.width()),
            " ".repeat(bw - 5 - kw - d.width())
        );
        s.push_str(&format!(
            "\x1b[{};{x0}H{border}│{line}{border}│",
            y0 + 3 + i
        ));
    }
    s.push_str(&format!(
        "\x1b[{};{x0}H{border}╰{}╯\x1b[0m",
        y0 + 3 + keys.len(),
        "─".repeat(bw - 2)
    ));
    s
}

pub fn fmt_duration(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_splits() {
        let spans = vec![Span::plain("Hello "), Span::plain("World")];
        let h = highlight_spans(&spans, "o w", Rgb(1, 2, 3));
        let texts: Vec<_> = h.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["Hell", "o ", "W", "orld"]);
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_duration(75.0), "1:15");
        assert_eq!(fmt_duration(3725.0), "1:02:05");
    }
}
