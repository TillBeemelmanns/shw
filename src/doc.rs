//! A laid-out document: rows of styled text and image slices. Produced by the
//! markdown / code renderers and consumed by the pager or inline printer.

use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use crate::raster::Raster;
use crate::ui::{spans_width, write_spans, Rgb, Span, Style};

pub enum Row {
    Text {
        spans: Vec<Span>,
        /// Fill the remainder of the line with this background colour.
        fill: Option<Rgb>,
        /// Text sizing scale (2 = double height, occupies this and the next row).
        scale: u8,
    },
    /// Lower half of a scaled text row.
    Cont,
    Image {
        img: usize,
        row: u32,
        prefix: Vec<Span>,
    },
}

pub struct DocImage {
    pub raster: Arc<Raster>,
    pub cols: u32,
    pub rows: u32,
    /// Column (0-based) where the image starts.
    pub col: u32,
    /// Index of the first row of this image in `Doc::rows`.
    pub start: usize,
    pub id: u32,
    pub sent: bool,
    /// Text represented by the image (headings), used for search.
    pub alt: String,
    /// Drawn over text rows that leave room for it (task list checkbox,
    /// picture in a table cell) instead of on rows of its own.
    pub overlay: bool,
}

#[derive(Default)]
pub struct Doc {
    pub rows: Vec<Row>,
    pub images: Vec<DocImage>,
    /// Layout width in cells (0 = full terminal width).
    pub width: usize,
    /// Left margin in cells applied when drawing.
    pub margin: u16,
    /// (source byte offset, row) of block starts, both ascending. They keep
    /// the reading position when the document is laid out again.
    anchors: Vec<(usize, usize)>,
}

impl Doc {
    pub fn text(&mut self, spans: Vec<Span>) {
        self.rows.push(Row::Text {
            spans,
            fill: None,
            scale: 1,
        });
    }

    pub fn scaled(&mut self, spans: Vec<Span>, scale: u8) {
        self.rows.push(Row::Text {
            spans,
            fill: None,
            scale,
        });
        for _ in 1..scale {
            self.rows.push(Row::Cont);
        }
    }

    pub fn blank(&mut self, prefix: Vec<Span>) {
        self.rows.push(Row::Text {
            spans: prefix,
            fill: None,
            scale: 1,
        });
    }

    pub fn last_is_blank(&self) -> bool {
        match self.rows.last() {
            None => true,
            Some(Row::Text {
                spans, fill: None, ..
            }) => spans
                .iter()
                .all(|s| s.text.trim().is_empty() || s.text.trim() == "▎" || s.text.trim() == "│"),
            _ => false,
        }
    }

    /// Note that the next row starts the block at source byte `offset`.
    pub fn anchor(&mut self, offset: usize) {
        let row = self.rows.len();
        match self.anchors.last() {
            Some(&(o, r)) if offset <= o || row <= r => {}
            _ => self.anchors.push((offset, row)),
        }
    }

    /// Source position of `row`: the block it belongs to and the rows past its start.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        let i = self.anchors.partition_point(|a| a.1 <= row);
        let (offset, start) = *self.anchors.get(i.checked_sub(1)?)?;
        Some((offset, row - start))
    }

    /// Row of a position returned by `locate` (possibly from another layout).
    pub fn row_of(&self, (offset, delta): (usize, usize)) -> usize {
        let i = self.anchors.partition_point(|a| a.0 < offset);
        let anchor = self.anchors.get(i).or(self.anchors.last());
        anchor.map_or(0, |a| a.1 + delta)
    }

    /// Add an image with given cell geometry; `prefix` is drawn left of each
    /// row and `alt` is the text it stands for (used by search).
    pub fn image(
        &mut self,
        raster: Arc<Raster>,
        col: u32,
        cols: u32,
        rows: u32,
        prefix: Vec<Span>,
        alt: String,
    ) {
        let idx = self.images.len();
        let start = self.rows.len();
        self.images.push(DocImage {
            raster,
            cols,
            rows,
            col,
            start,
            id: crate::kitty::next_id(),
            sent: false,
            alt,
            overlay: false,
        });
        for r in 0..rows {
            self.rows.push(Row::Image {
                img: idx,
                row: r,
                prefix: prefix.clone(),
            });
        }
    }

    /// Draw an image over the text rows `row..row + rows`, starting at `col`.
    pub fn overlay(&mut self, raster: Arc<Raster>, row: usize, col: u32, cols: u32, rows: u32) {
        self.images.push(DocImage {
            raster,
            cols,
            rows,
            col,
            start: row,
            id: crate::kitty::next_id(),
            sent: false,
            alt: String::new(),
            overlay: true,
        });
    }

    pub fn plain(&self, i: usize) -> String {
        match &self.rows[i] {
            Row::Text { spans, .. } => spans.iter().map(|s| s.text.as_str()).collect(),
            Row::Image { img, row: 0, .. } => self.images[*img].alt.clone(),
            _ => String::new(),
        }
    }
}

/// Render a text row (without cursor positioning).
pub fn render_text(
    out: &mut String,
    spans: &[Span],
    fill: Option<Rgb>,
    scale: u8,
    width: usize,
    hl: Option<(&str, Rgb)>,
    base: Option<(Rgb, Rgb)>,
) {
    write_spans(out, spans, scale, hl, base);
    if let Some(Rgb(r, g, b)) = fill {
        let w = spans_width(spans) * scale as usize;
        if w < width {
            out.push_str(&format!(
                "\x1b[48;2;{r};{g};{b}m{}\x1b[0m",
                " ".repeat(width - w)
            ));
        }
    }
}

fn push_merge(line: &mut Vec<Span>, text: &str, style: &Style) {
    if let Some(last) = line.last_mut() {
        if &last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    line.push(Span::new(text, style.clone()));
}

/// Split text into (token, kind) where kind: 0 = word, 1 = space, 2 = newline.
fn tokenize(s: &str) -> Vec<(&str, u8)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind: Option<u8> = None;
    for (i, c) in s.char_indices() {
        let k = if c == '\n' {
            2
        } else if c.is_whitespace() {
            1
        } else {
            0
        };
        match kind {
            Some(prev) if prev == k && k != 2 => {}
            Some(prev) => {
                out.push((&s[start..i], prev));
                start = i;
                kind = Some(k);
            }
            None => {
                start = i;
                kind = Some(k);
            }
        }
    }
    if let Some(k) = kind {
        out.push((&s[start..], k));
    }
    out
}

/// Greedy word wrap over styled spans. `\n` in span text forces a break.
pub fn wrap(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span>> = Vec::new();
    let mut line: Vec<Span> = Vec::new();
    let mut w = 0usize;
    let mut pending: Option<(String, Style)> = None;

    for s in spans {
        for (tok, kind) in tokenize(&s.text) {
            match kind {
                2 => {
                    lines.push(std::mem::take(&mut line));
                    w = 0;
                    pending = None;
                }
                1 => {
                    if w > 0 {
                        pending = Some((" ".to_string(), s.style.clone()));
                    }
                }
                _ => {
                    let tw = tok.width();
                    let sw = if pending.is_some() { 1 } else { 0 };
                    if w + sw + tw <= width {
                        if let Some((p, st)) = pending.take() {
                            push_merge(&mut line, &p, &st);
                            w += 1;
                        }
                        push_merge(&mut line, tok, &s.style);
                        w += tw;
                    } else if tw <= width {
                        if !line.is_empty() {
                            lines.push(std::mem::take(&mut line));
                        }
                        pending = None;
                        push_merge(&mut line, tok, &s.style);
                        w = tw;
                    } else {
                        // Hard-split an overlong word.
                        if let Some((p, st)) = pending.take() {
                            if w + 1 < width {
                                push_merge(&mut line, &p, &st);
                                w += 1;
                            }
                        }
                        for ch in tok.chars() {
                            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                            if w + cw > width && w > 0 {
                                lines.push(std::mem::take(&mut line));
                                w = 0;
                            }
                            let mut b = [0u8; 4];
                            push_merge(&mut line, ch.encode_utf8(&mut b), &s.style);
                            w += cw;
                        }
                    }
                }
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Hard-wrap a line by display width without word boundaries (for code).
pub fn hard_wrap(spans: &[Span], width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut lines = vec![Vec::new()];
    let mut w = 0;
    for s in spans {
        for ch in s.text.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + cw > width && w > 0 {
                lines.push(Vec::new());
                w = 0;
            }
            let mut b = [0u8; 4];
            push_merge(lines.last_mut().unwrap(), ch.encode_utf8(&mut b), &s.style);
            w += cw;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Vec<Span>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn wraps_words() {
        let spans = vec![Span::plain("the quick brown fox jumps")];
        assert_eq!(
            text(&wrap(&spans, 10)),
            vec!["the quick", "brown fox", "jumps"]
        );
        let spans = vec![Span::plain("abcdefghijkl")];
        assert_eq!(text(&wrap(&spans, 5)), vec!["abcde", "fghij", "kl"]);
        let spans = vec![Span::plain("a\nb")];
        assert_eq!(text(&wrap(&spans, 5)), vec!["a", "b"]);
    }

    #[test]
    fn anchors_keep_position() {
        let mut a = Doc::default();
        for (offset, rows) in [(0, 3), (40, 5), (90, 2)] {
            a.anchor(offset);
            (0..rows).for_each(|_| a.text(Vec::new()));
        }
        assert_eq!(a.locate(4), Some((40, 1)));
        // the same document with two more rows in the first block
        let mut b = Doc::default();
        for (offset, rows) in [(0, 5), (40, 5), (90, 2)] {
            b.anchor(offset);
            (0..rows).for_each(|_| b.text(Vec::new()));
        }
        assert_eq!(b.row_of((40, 1)), 6);
        assert_eq!(Doc::default().locate(0), None);
    }

    #[test]
    fn keeps_styles() {
        let bold = Style {
            bold: true,
            ..Default::default()
        };
        let spans = vec![
            Span::plain("hello "),
            Span::new("bold", bold.clone()),
            Span::plain(" end"),
        ];
        let l = wrap(&spans, 80);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0][1].text, "bold");
        assert!(l[0][1].style.bold);
    }
}
