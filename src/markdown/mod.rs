//! GitHub-flavoured Markdown renderer: typography, tables, alerts, task lists,
//! footnotes, emoji shortcodes, syntax highlighted code, images (local, remote,
//! SVG, HTML `<img>`), diagrams (mermaid, GeoJSON, TopoJSON, STL) and
//! clickable links.

mod assets;
mod diagram;
mod html;

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use unicode_width::UnicodeWidthStr;

use crate::code;
use crate::doc::{hard_wrap, wrap, Doc, Row};
use crate::fetch;
use crate::raster::{compose_row, Picture, Raster};
use crate::ui::{spans_width, Rgb, Span, Style, Theme};
use assets::{diagram_key, Assets, Job};
use html::Token;

/// Where relative links and images are resolved from.
#[derive(Clone, Debug)]
pub enum Base {
    Dir(PathBuf),
    Url(String),
}

pub struct LayoutOpts<'a> {
    pub width: usize,
    pub cell: (f64, f64),
    pub theme: &'a Theme,
    pub text_sizing: bool,
    pub graphics: bool,
    pub offline: bool,
    /// Wait for downloads instead of showing the document without them first.
    pub wait: bool,
    /// Maximum image height in pixels.
    pub max_img_h: f64,
    /// Text colour used for typeset (image) headings.
    pub fg: Rgb,
}

pub struct Markdown {
    src: String,
    base: Base,
    assets: Assets,
}

/// First word of a fenced code block's info string.
fn fence_lang(info: &str) -> &str {
    info.split([' ', ',', '{']).next().unwrap_or("")
}

impl Markdown {
    /// Replace the text (after the file was edited), keeping loaded pictures.
    pub fn set_source(&mut self, src: String) {
        self.src = src;
    }

    pub fn new(src: String, base: Base) -> Self {
        Markdown {
            src,
            base,
            assets: Assets::default(),
        }
    }

    /// True once pictures loaded in the background have arrived: lay out again.
    pub fn poll(&mut self) -> bool {
        self.assets.poll()
    }

    /// Number of pictures still loading in the background.
    pub fn loading(&self) -> usize {
        self.assets.loading()
    }

    pub fn layout(&mut self, o: &LayoutOpts) -> Doc {
        let (events, footnotes) = parse(&self.src);
        crate::app::trace("markdown parsed");
        self.assets.validate(format!(
            "{} {:?} {} {:?}",
            o.width, o.cell, o.theme.dark, o.fg
        ));
        // Highlight the code blocks on other threads while the pictures load:
        // compiling a grammar on first use takes tens of milliseconds.
        let assets = &mut self.assets;
        let code: Vec<(u64, &str, String)> = code_blocks(&events)
            .into_iter()
            .filter(|b| !assets.code.contains_key(&b.0))
            .collect();
        std::thread::scope(|s| {
            let highlight = |(key, lang, text): &(u64, &str, String)| {
                let lines = code::highlight(text, code::find_syntax(lang), o.theme, 5000);
                (*key, Arc::new(lines))
            };
            let chunks = code.chunks(code.len().div_ceil(8).max(1));
            let threads: Vec<_> = chunks
                .map(|c| s.spawn(move || c.iter().map(highlight).collect::<Vec<_>>()))
                .collect();
            if o.graphics {
                assets.load(jobs(&events, &self.base, o), o.wait);
            }
            for t in threads {
                assets.code.extend(t.join().unwrap_or_default());
            }
        });
        crate::app::trace("pictures loaded, code highlighted");
        let mut r = Renderer::new(o, &mut self.assets, &self.base, footnotes);
        for (ev, offset) in events {
            let block_start = matches!(ev, Event::Start(_));
            r.event(ev);
            if block_start {
                r.doc.anchor(offset);
            }
        }
        let doc = r.finish();
        crate::app::trace(format!(
            "markdown laid out ({} rows, {} images)",
            doc.rows.len(),
            doc.images.len()
        ));
        doc
    }
}

/// Parse into events with their source offsets. Footnote definitions move
/// to the end, as on GitHub; also returns each footnote's number.
fn parse(src: &str) -> (Vec<(Event<'_>, usize)>, HashMap<String, usize>) {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_MATH
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut body = Vec::new();
    let mut notes: Vec<(String, Vec<(Event, usize)>)> = Vec::new();
    let mut numbers = HashMap::new();
    let mut in_note = false;
    for (ev, range) in Parser::new_ext(src, options).into_offset_iter() {
        match &ev {
            Event::Start(Tag::FootnoteDefinition(label)) => {
                in_note = true;
                notes.push((label.to_string(), Vec::new()));
            }
            Event::FootnoteReference(label) => {
                let n = numbers.len() + 1;
                numbers.entry(label.to_string()).or_insert(n);
            }
            _ => {}
        }
        let end = matches!(ev, Event::End(TagEnd::FootnoteDefinition));
        match notes.last_mut() {
            Some(note) if in_note => note.1.push((ev, range.start)),
            _ => body.push((ev, range.start)),
        }
        in_note &= !end;
    }
    if !notes.is_empty() {
        for (label, _) in &notes {
            let n = numbers.len() + 1;
            numbers.entry(label.clone()).or_insert(n);
        }
        notes.sort_by_key(|(label, _)| numbers[label]);
        body.push((Event::Rule, src.len()));
        body.extend(notes.into_iter().flat_map(|n| n.1));
    }
    (body, numbers)
}

// ---------------------------------------------------------------------------
// Links and image sources

fn resolve(base: &Base, src: &str) -> Option<String> {
    let src = src.trim();
    if src.is_empty()
        || src.starts_with('#')
        || src.starts_with("data:")
        || src.starts_with("mailto:")
    {
        return None;
    }
    if fetch::is_url(src) {
        return Some(src.to_string());
    }
    if let Some(p) = src.strip_prefix("file://") {
        return Some(p.to_string());
    }
    let clean = src.split(['?', '#']).next().unwrap_or(src);
    match base {
        Base::Dir(d) => {
            let rel = clean.trim_start_matches('/');
            let p = if Path::new(clean).is_absolute() && Path::new(clean).exists() {
                PathBuf::from(clean)
            } else {
                d.join(rel)
            };
            Some(p.to_string_lossy().into_owned())
        }
        Base::Url(u) => {
            let dir = &u[..u.rfind('/').map(|i| i + 1).unwrap_or(u.len())];
            if let Some(stripped) = clean.strip_prefix('/') {
                // scheme://host/
                let host_end = u
                    .find("://")
                    .map(|i| i + 3)
                    .and_then(|s| u[s..].find('/').map(|j| s + j))
                    .unwrap_or(u.len());
                Some(format!("{}/{}", &u[..host_end], stripped))
            } else {
                Some(format!("{dir}{}", clean.trim_start_matches("./")))
            }
        }
    }
}

/// Local files a link may point at: ones the OS opens in a viewer or editor.
/// Anything else (scripts, `.command`, apps, …) could execute when clicked.
const SAFE_LINK_EXT: &[&str] = &[
    "md", "markdown", "mdown", "mkd", "txt", "rst", "adoc", "png", "jpg", "jpeg", "gif", "webp",
    "svg", "bmp", "pdf",
];

/// Target for a clickable (OSC 8) link, or None if it should not be clickable.
/// Only http(s) and mailto URLs and harmless local file types are allowed;
/// other schemes (`javascript:`, custom app schemes, `file:` to scripts) are dropped.
fn link_target(base: &Base, url: &str) -> Option<Arc<str>> {
    let url = url.trim();
    if url.starts_with('#') || url.is_empty() || url.chars().any(char::is_control) {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
    {
        return Some(url.into());
    }
    let has_scheme = url.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if has_scheme && !lower.starts_with("file:") {
        return None;
    }
    let r = resolve(base, url)?;
    if fetch::is_url(&r) {
        return Some(r.into());
    }
    let ext = Path::new(&r)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !SAFE_LINK_EXT.contains(&ext.as_str()) {
        return None;
    }
    let abs = std::fs::canonicalize(&r)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(r);
    Some(format!("file://{abs}").into())
}

fn code_key(lang: &str, text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (lang, text).hash(&mut h);
    h.finish()
}

/// The code blocks to highlight: (cache key, language, text).
fn code_blocks<'a>(events: &'a [(Event, usize)]) -> Vec<(u64, &'a str, String)> {
    let mut out = Vec::new();
    let mut block: Option<(&str, String)> = None;
    for (ev, _) in events {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => fence_lang(info),
                    CodeBlockKind::Indented => "",
                };
                block = (!diagram::is_diagram(lang)).then_some((lang, String::new()));
            }
            Event::Text(t) => {
                if let Some((_, text)) = &mut block {
                    text.push_str(t);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, mut text)) = block.take() {
                    text.truncate(text.strip_suffix('\n').unwrap_or(&text).len());
                    out.push((code_key(lang, &text), lang, text));
                }
            }
            _ => {}
        }
    }
    out
}

/// Every picture the document refers to: images and diagram code blocks.
fn jobs(events: &[(Event, usize)], base: &Base, o: &LayoutOpts) -> Vec<Job> {
    let mut jobs = Vec::new();
    let image = |src: &str| {
        resolve(base, src)
            .filter(|k| !(o.offline && fetch::is_url(k)))
            .map(Job::Image)
    };
    let mut diagram: Option<(&str, String)> = None;
    for (ev, _) in events {
        match ev {
            Event::Start(Tag::Image { dest_url, .. }) => jobs.extend(image(dest_url)),
            Event::Html(h) | Event::InlineHtml(h) => {
                for t in html::tokenize(h) {
                    if let Token::Open { name, attrs, .. } = t {
                        if name == "img" {
                            jobs.extend(Token::attr(&attrs, "src").and_then(image));
                        }
                    }
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if diagram::is_diagram(fence_lang(info)) =>
            {
                diagram = Some((fence_lang(info), String::new()));
            }
            Event::Text(t) => {
                if let Some((_, src)) = &mut diagram {
                    src.push_str(t);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, src)) = diagram.take() {
                    jobs.push(Job::Diagram {
                        lang: lang.to_string(),
                        src,
                        dark: o.theme.dark,
                    });
                }
            }
            _ => {}
        }
    }
    jobs
}

// ---------------------------------------------------------------------------
// Layout

#[derive(Clone, Copy, Debug)]
enum Dim {
    Px(f64),
    Pct(f64),
}

fn parse_dim(s: Option<&str>) -> Option<Dim> {
    let s = s?.trim();
    if let Some(p) = s.strip_suffix('%') {
        return p.trim().parse().ok().map(Dim::Pct);
    }
    s.trim_end_matches("px")
        .trim()
        .parse()
        .ok()
        .filter(|v: &f64| *v > 0.0)
        .map(Dim::Px)
}

#[derive(Clone, Debug)]
struct ImgRef {
    key: Option<String>,
    alt: String,
    width: Option<Dim>,
    height: Option<Dim>,
    link: Option<Arc<str>>,
}

enum Inline {
    Text(Span),
    Img(ImgRef),
}

enum Block {
    Quote { color: Rgb },
    List { next: Option<u64>, depth: usize },
    Item { marker: Option<Span>, indent: usize },
}

/// A table cell: text, and pictures shown in a row above or below it.
#[derive(Default)]
struct Cell {
    text: Vec<Span>,
    images: Vec<(String, Arc<Picture>, ImgRef)>,
    images_first: bool,
}

/// A laid-out cell: wrapped text lines, the picture with its size in cells
/// (columns, rows), and whether the picture comes first.
type CellLayout = (Vec<Vec<Span>>, Option<Arc<Raster>>, usize, usize, bool);

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Cell>>,
    cur: Vec<Cell>,
    header_rows: usize,
}

struct Renderer<'a> {
    o: &'a LayoutOpts<'a>,
    t: &'a Theme,
    assets: &'a mut Assets,
    base: &'a Base,
    /// Footnote label -> number.
    footnotes: HashMap<String, usize>,
    doc: Doc,
    blocks: Vec<Block>,
    inline: Vec<Inline>,
    styles: Vec<Style>,
    html_styles: Vec<String>,
    links: Vec<Option<Arc<str>>>,
    heading: Option<u8>,
    code: Option<(String, String)>,
    table: Option<Table>,
    image: Option<ImgRef>,
    in_code_span: bool,
    center: Vec<bool>,
    /// Suppress the next block gap (directly after an alert title).
    no_gap: bool,
    /// Task list checkboxes to draw as icons: (row, column, checked).
    checkboxes: Vec<(usize, u32, bool)>,
    /// Open HTML `<dd>` elements (each one is an indented block).
    html_dd: usize,
}

impl<'a> Renderer<'a> {
    fn new(
        o: &'a LayoutOpts<'a>,
        assets: &'a mut Assets,
        base: &'a Base,
        footnotes: HashMap<String, usize>,
    ) -> Self {
        Renderer {
            o,
            t: o.theme,
            assets,
            base,
            footnotes,
            doc: Doc::default(),
            blocks: Vec::new(),
            inline: Vec::new(),
            styles: vec![Style::default()],
            html_styles: Vec::new(),
            links: Vec::new(),
            heading: None,
            code: None,
            table: None,
            image: None,
            in_code_span: false,
            center: Vec::new(),
            no_gap: false,
            checkboxes: Vec::new(),
            html_dd: 0,
        }
    }

    fn style(&self) -> Style {
        self.styles.last().cloned().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(&mut Style)) {
        let mut s = self.style();
        f(&mut s);
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn centered(&self) -> bool {
        self.center.last().copied().unwrap_or(false)
    }

    fn dpr(&self) -> f64 {
        (self.o.cell.1 / 18.0).round().clamp(1.0, 4.0)
    }

    /// Prefix spans for the current block nesting. With `consume`, the
    /// innermost pending list marker is emitted (once).
    fn prefix(&mut self, consume: bool) -> Vec<Span> {
        let mut out = Vec::new();
        for b in self.blocks.iter_mut() {
            match b {
                Block::Quote { color } => out.push(Span::new("▎ ", Style::fg(*color))),
                Block::Item { marker, indent } => {
                    if consume {
                        if let Some(mut m) = marker.take() {
                            let pad = indent.saturating_sub(m.width());
                            if self.o.graphics && (m.text == "☐ " || m.text == "☑ ") {
                                let col =
                                    out.iter().map(|s: &Span| s.width()).sum::<usize>() as u32;
                                self.checkboxes
                                    .push((self.doc.rows.len(), col, m.text == "☑ "));
                                m.text = "  ".into();
                            }
                            out.push(m);
                            if pad > 0 {
                                out.push(Span::plain(" ".repeat(pad)));
                            }
                            continue;
                        }
                    }
                    out.push(Span::plain(" ".repeat(*indent)));
                }
                Block::List { .. } => {}
            }
        }
        out
    }

    fn prefix_width(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| match b {
                Block::Quote { .. } => 2,
                Block::Item { indent, .. } => *indent,
                Block::List { .. } => 0,
            })
            .sum()
    }

    fn content_width(&self) -> usize {
        self.o.width.saturating_sub(self.prefix_width()).max(8)
    }

    fn at_item_start(&self) -> bool {
        matches!(
            self.blocks.last(),
            Some(Block::Item {
                marker: Some(_),
                ..
            })
        )
    }

    fn block_gap(&mut self) {
        if std::mem::take(&mut self.no_gap) {
            return;
        }
        if self.at_item_start() || self.doc.rows.is_empty() || self.doc.last_is_blank() {
            return;
        }
        let p = self.prefix(false);
        self.doc.blank(p);
    }

    fn push_text(&mut self, text: &str) {
        if let Some(img) = &mut self.image {
            img.alt.push_str(text);
            return;
        }
        if let Some((_, buf)) = &mut self.code {
            buf.push_str(text);
            return;
        }
        let st = self.style();
        if self.in_code_span {
            self.inline.push(Inline::Text(Span::new(text, st)));
            return;
        }
        // bare URLs become links, like GitHub's autolinks
        let linkify = self.links.is_empty();
        for (part, url) in autolinks(text, linkify) {
            let mut st = st.clone();
            if let Some(target) = url.and_then(|u| link_target(self.base, &u)) {
                st.fg = Some(self.t.link);
                st.link = Some(target);
            }
            self.inline.push(Inline::Text(Span::new(emojify(part), st)));
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.push_text(&t),
            Event::Code(c) => {
                let bg = self.t.inline_code_bg;
                self.push_style(|s| {
                    s.bg = Some(bg);
                });
                self.in_code_span = true;
                self.push_text(&format!(" {c} "));
                self.in_code_span = false;
                self.pop_style();
            }
            Event::InlineMath(m) => {
                self.push_style(|s| s.italic = true);
                self.in_code_span = true;
                self.push_text(&m);
                self.in_code_span = false;
                self.pop_style();
            }
            Event::DisplayMath(m) => {
                self.flush_inline();
                self.block_gap();
                self.center.push(true);
                self.push_style(|s| s.italic = true);
                self.in_code_span = true;
                self.push_text(&m);
                self.in_code_span = false;
                self.pop_style();
                self.flush_inline();
                self.center.pop();
            }
            Event::Html(h) | Event::InlineHtml(h) => self.html(&h),
            Event::FootnoteReference(label) => {
                let link = self.t.link;
                let n = self.footnotes.get(&*label).copied().unwrap_or(0);
                self.push_style(|s| s.fg = Some(link));
                self.in_code_span = true; // literal text
                self.push_text(&superscript(n));
                self.in_code_span = false;
                self.pop_style();
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_text("\n"),
            Event::Rule => {
                self.flush_inline();
                self.block_gap();
                let mut p = self.prefix(true);
                p.push(Span::new(
                    "─".repeat(self.content_width()),
                    Style::fg(self.t.border),
                ));
                self.doc.text(p);
            }
            Event::TaskListMarker(done) => {
                let (txt, color) = if done {
                    ("☑ ", self.t.tip)
                } else {
                    ("☐ ", self.t.muted)
                };
                if let Some(Block::Item { marker, .. }) = self.blocks.last_mut() {
                    *marker = Some(Span::new(
                        txt,
                        Style {
                            fg: Some(color),
                            bold: true,
                            ..Default::default()
                        },
                    ));
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                self.block_gap();
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.block_gap();
                self.begin_heading(heading_num(level));
            }
            Tag::BlockQuote(kind) => {
                self.flush_inline();
                self.block_gap();
                let (color, title) = match kind {
                    None => (self.t.border, None),
                    Some(BlockQuoteKind::Note) => (self.t.note, Some("ⓘ  Note")),
                    Some(BlockQuoteKind::Tip) => (self.t.tip, Some("💡 Tip")),
                    Some(BlockQuoteKind::Important) => (self.t.important, Some("📢 Important")),
                    Some(BlockQuoteKind::Warning) => (self.t.warning, Some("⚠  Warning")),
                    Some(BlockQuoteKind::Caution) => (self.t.caution, Some("⛔ Caution")),
                };
                self.blocks.push(Block::Quote { color });
                if kind.is_none() {
                    let muted = self.t.muted;
                    self.push_style(|s| s.fg = Some(muted));
                } else {
                    self.push_style(|_| {});
                }
                if let Some(title) = title {
                    let mut p = self.prefix(true);
                    p.push(Span::new(
                        title,
                        Style {
                            fg: Some(color),
                            bold: true,
                            ..Default::default()
                        },
                    ));
                    self.doc.text(p);
                    self.no_gap = true;
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.block_gap();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => fence_lang(&info).to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::HtmlBlock => {}
            Tag::List(start) => {
                self.flush_inline();
                let nested = self.blocks.iter().any(|b| matches!(b, Block::Item { .. }));
                if !nested {
                    self.block_gap();
                }
                let depth = self
                    .blocks
                    .iter()
                    .filter(|b| matches!(b, Block::List { .. }))
                    .count();
                self.blocks.push(Block::List { next: start, depth });
            }
            Tag::Item => {
                self.flush_inline();
                let (marker, indent) = match self.blocks.last_mut() {
                    Some(Block::List { next: Some(n), .. }) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        let w = m.width().max(3);
                        (Span::plain(m), w)
                    }
                    Some(Block::List { depth, .. }) => {
                        let b = ["• ", "◦ ", "▪ "][*depth % 3];
                        (Span::new(b, Style::fg(self.t.muted)), 2)
                    }
                    _ => (Span::plain("• "), 2),
                };
                self.blocks.push(Block::Item {
                    marker: Some(marker),
                    indent,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_inline();
                self.block_gap();
                let m = format!("{}. ", self.footnotes.get(&*label).copied().unwrap_or(0));
                let w = m.width();
                self.blocks.push(Block::Item {
                    marker: Some(Span::new(m, Style::fg(self.t.muted))),
                    indent: w,
                });
                let muted = self.t.muted;
                self.push_style(|s| s.fg = Some(muted));
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.flush_inline();
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.block_gap();
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    cur: Vec::new(),
                    header_rows: 0,
                });
            }
            Tag::TableHead => self.push_style(|s| s.bold = true),
            Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.push_style(|s| s.italic = true),
            Tag::Strong => self.push_style(|s| s.bold = true),
            Tag::Strikethrough => self.push_style(|s| s.strike = true),
            Tag::Superscript | Tag::Subscript => self.push_style(|_| {}),
            Tag::Link { dest_url, .. } => {
                let target = link_target(self.base, &dest_url);
                let link = self.t.link;
                let t2 = target.clone();
                self.push_style(|s| {
                    s.fg = Some(link);
                    s.link = t2;
                });
                self.links.push(target);
            }
            Tag::Image { dest_url, .. } => {
                self.image = Some(ImgRef {
                    key: resolve(self.base, &dest_url),
                    alt: String::new(),
                    width: None,
                    height: None,
                    link: self.links.last().cloned().flatten(),
                });
            }
            Tag::MetadataBlock(_) => {
                self.flush_inline();
                self.code = Some(("yaml".into(), String::new()));
            }
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.pop_style();
                self.blocks.pop();
            }
            TagEnd::CodeBlock | TagEnd::MetadataBlock(_) => {
                if let Some((lang, text)) = self.code.take() {
                    self.code_block(&lang, &text);
                }
            }
            TagEnd::HtmlBlock => self.flush_inline(),
            TagEnd::List(_) => {
                self.flush_inline();
                self.blocks.pop();
            }
            TagEnd::Item => {
                self.flush_inline();
                if self.at_item_start() {
                    let p = self.prefix(true);
                    self.doc.text(p);
                }
                self.blocks.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.pop_style();
                self.blocks.pop();
            }
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => self.flush_inline(),
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
            }
            TagEnd::TableHead => {
                self.pop_style();
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                    t.header_rows = t.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let mut cell = Cell::default();
                for item in std::mem::take(&mut self.inline) {
                    match item {
                        Inline::Text(s) => cell.text.push(s),
                        Inline::Img(r) => {
                            let key = r.key.clone().filter(|_| self.o.graphics);
                            match key.and_then(|k| Some((self.assets.get(&k)?, k))) {
                                Some((pic, key)) => {
                                    cell.images_first |= cell.text.is_empty();
                                    cell.images.push((key, pic, r));
                                }
                                // not loaded (yet): the alt text stands in
                                None => cell.text.push(self.alt_span(&r)),
                            }
                        }
                    }
                }
                if let Some(t) = &mut self.table {
                    t.cur.push(cell);
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                self.links.pop();
            }
            TagEnd::Image => {
                if let Some(img) = self.image.take() {
                    self.inline.push(Inline::Img(img));
                }
            }
        }
    }

    // -- headings ----------------------------------------------------------

    fn begin_heading(&mut self, level: u8) {
        self.heading = Some(level);
        let muted = self.t.muted;
        self.push_style(|s| {
            s.bold = true;
            if level == 6 {
                s.fg = Some(muted);
            }
        });
    }

    fn end_heading(&mut self) {
        let Some(level) = self.heading.take() else {
            return;
        };
        let items = std::mem::take(&mut self.inline);
        self.pop_style();
        let mut spans = Vec::new();
        let mut imgs = Vec::new();
        for i in items {
            match i {
                Inline::Text(s) => spans.push(s),
                Inline::Img(r) => imgs.push(r),
            }
        }
        if !imgs.is_empty() {
            self.emit_images(imgs);
        }
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        if level <= 3 && self.o.graphics && self.heading_image(&spans, level) {
            return;
        }
        let top_level = self.blocks.is_empty();
        let scale: u8 = if level == 1 && self.o.text_sizing && top_level {
            2
        } else {
            1
        };
        let cw = self.content_width();
        for line in wrap(&spans, cw / scale as usize) {
            let mut p = self.prefix(true);
            let lw = spans_width(&line) * scale as usize;
            if self.centered() && lw < cw {
                p.push(Span::plain(" ".repeat((cw - lw) / 2)));
            }
            p.extend(line);
            if scale > 1 {
                self.doc.scaled(p, scale);
            } else {
                self.doc.text(p);
            }
        }
        if level <= 2 {
            let mut p = self.prefix(false);
            let ch = if level == 1 { "━" } else { "─" };
            p.push(Span::new(ch.repeat(cw), Style::fg(self.t.border)));
            self.doc.text(p);
        }
    }

    /// GitHub-style large heading typeset with a real font and shown as an
    /// image (works in every kitty-graphics terminal, unlike text sizing).
    fn heading_image(&mut self, spans: &[Span], level: u8) -> bool {
        let text: String = spans.iter().map(|s| s.text.as_str()).collect::<String>();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return false;
        }
        let (cw_px, ch_px) = self.o.cell;
        // terminal body text is roughly cell height / 1.22; GitHub: h1 2em, h2 1.5em, h3 1.25em
        let body = ch_px / 1.22;
        let size = body * [2.0, 1.5, 1.25][(level - 1) as usize];
        let rows = ((size * 1.32) / ch_px).ceil().max(1.0) as u32;
        let h = (rows as f64 * ch_px).round() as u32;
        let cw = self.content_width();
        let avail = cw as f64 * cw_px - 2.0;

        let color = (self.o.fg.0, self.o.fg.1, self.o.fg.2);
        let key = format!("{level} {cw} {text}");
        let lines = match self.assets.headings.get(&key) {
            Some(lines) => lines.clone(),
            None => {
                let lines = crate::raster::typeset(&text, size, h, avail, color).map(|lines| {
                    Arc::new(lines.into_iter().map(|(r, t)| (Arc::new(r), t)).collect())
                });
                self.assets.headings.insert(key, lines.clone());
                lines
            }
        };
        let Some(lines) = lines else {
            return false;
        };
        for (raster, alt) in lines.iter() {
            let cols = ((raster.w as f64 / cw_px).ceil() as u32)
                .min(cw as u32)
                .max(1);
            self.place(raster.clone(), cols, rows, alt.clone());
        }
        if level <= 2 {
            let mut p = self.prefix(false);
            p.push(Span::new("─".repeat(cw), Style::fg(self.t.border)));
            self.doc.text(p);
        }
        true
    }

    /// Put an image on its own rows, centred if the context asks for it.
    fn place(&mut self, raster: Arc<Raster>, cols: u32, rows: u32, alt: String) {
        let cw = self.content_width() as u32;
        let mut col = self.prefix_width() as u32;
        if self.centered() && cols < cw {
            col += (cw - cols) / 2;
        }
        // keep quote bars, drop list markers for image rows
        let prefix = self.prefix(true);
        let prefix = prefix
            .into_iter()
            .filter(|s| s.text.contains('▎'))
            .collect();
        self.doc.image(raster, col, cols, rows, prefix, alt);
    }

    // -- inline flow --------------------------------------------------------

    fn flush_inline(&mut self) {
        if self.heading.is_some() {
            return;
        }
        let items = std::mem::take(&mut self.inline);
        let mut text: Vec<Span> = Vec::new();
        let mut imgs: Vec<ImgRef> = Vec::new();
        for it in items {
            match it {
                Inline::Text(s) => {
                    if !imgs.is_empty() {
                        if s.text.trim().is_empty() {
                            continue;
                        }
                        self.emit_images(std::mem::take(&mut imgs));
                    }
                    text.push(s);
                }
                Inline::Img(r) => {
                    if text.iter().any(|s| !s.text.trim().is_empty()) {
                        self.emit_text(std::mem::take(&mut text));
                    }
                    text.clear();
                    imgs.push(r);
                }
            }
        }
        if !imgs.is_empty() {
            self.emit_images(imgs);
        }
        self.emit_text(text);
    }

    fn emit_text(&mut self, spans: Vec<Span>) {
        if spans.iter().all(|s| s.text.trim().is_empty()) {
            return;
        }
        // Trim leading whitespace of the paragraph (common after HTML tags).
        let mut spans = spans;
        while let Some(f) = spans.first_mut() {
            let t = f.text.trim_start_matches([' ', '\n']).to_string();
            if t.is_empty() {
                spans.remove(0);
            } else {
                f.text = t;
                break;
            }
        }
        let cw = self.content_width();
        for line in wrap(&spans, cw) {
            let mut p = self.prefix(true);
            let lw = spans_width(&line);
            if self.centered() && lw < cw {
                p.push(Span::plain(" ".repeat((cw - lw) / 2)));
            }
            p.extend(line);
            self.doc.text(p);
        }
    }

    fn alt_span(&self, r: &ImgRef) -> Span {
        let alt = if r.alt.trim().is_empty() {
            "image"
        } else {
            r.alt.trim()
        };
        Span::new(
            format!("🖼  {alt}"),
            Style {
                fg: Some(self.t.muted),
                italic: true,
                link: r.link.clone(),
                ..Default::default()
            },
        )
    }

    fn img_size(&self, pic: &Picture, r: &ImgRef, avail_px: f64) -> (u32, u32) {
        let dpr = self.dpr();
        let (nw, nh) = (pic.width.max(1) as f64, pic.height.max(1) as f64);
        let (mut w, mut h) = match (r.width, r.height) {
            (Some(w), Some(Dim::Px(h))) => (dim_px(w, avail_px, dpr), h * dpr),
            (Some(w), _) => {
                let w = dim_px(w, avail_px, dpr);
                (w, w * nh / nw)
            }
            (None, Some(Dim::Px(h))) => (h * dpr * nw / nh, h * dpr),
            _ => (nw * dpr, nh * dpr),
        };
        if w > avail_px {
            h *= avail_px / w;
            w = avail_px;
        }
        if h > self.o.max_img_h {
            w *= self.o.max_img_h / h;
            h = self.o.max_img_h;
        }
        (w.round().max(1.0) as u32, h.round().max(1.0) as u32)
    }

    fn emit_images(&mut self, refs: Vec<ImgRef>) {
        let (cw_px, ch_px) = self.o.cell;
        let cw = self.content_width();
        let avail_px = cw as f64 * cw_px;
        let mut fallback: Vec<Span> = Vec::new();
        let mut group: Vec<Arc<Raster>> = Vec::new();
        let gap = (cw_px / 2.0).round() as u32;
        let mut group_w = 0u32;

        for r in refs {
            let key = r.key.as_ref().filter(|_| self.o.graphics);
            let Some((key, pic)) = key.and_then(|k| Some((k, self.assets.get(k)?))) else {
                if !fallback.is_empty() {
                    fallback.push(Span::plain(" "));
                }
                fallback.push(self.alt_span(&r));
                continue;
            };
            let (w, h) = self.img_size(&pic, &r, avail_px);
            let raster = self.scaled(key, &pic, w, h);
            if !group.is_empty() && (group_w + gap + raster.w) as f64 > avail_px {
                self.place_group(std::mem::take(&mut group), gap, ch_px, cw_px);
                group_w = 0;
            }
            group_w += if group.is_empty() {
                raster.w
            } else {
                gap + raster.w
            };
            group.push(raster);
        }
        if !group.is_empty() {
            self.place_group(group, gap, ch_px, cw_px);
        }
        if !fallback.is_empty() {
            self.emit_text(fallback);
        }
    }

    /// The picture resampled to `w` x `h` px; kept for the next layout.
    fn scaled(&mut self, key: &str, pic: &Picture, w: u32, h: u32) -> Arc<Raster> {
        let cached = self.assets.scaled.entry((key.to_string(), w, h));
        cached.or_insert_with(|| Arc::new(pic.render(w, h))).clone()
    }

    fn place_group(&mut self, group: Vec<Arc<Raster>>, gap: u32, ch_px: f64, cw_px: f64) {
        let raster = match &group[..] {
            [one] => one.clone(),
            _ => Arc::new(compose_row(&group, gap)),
        };
        let cols = (raster.w as f64 / cw_px).ceil().max(1.0) as u32;
        let rows = (raster.h as f64 / ch_px).ceil().max(1.0) as u32;
        self.place(raster, cols, rows, String::new());
    }

    // -- code ----------------------------------------------------------------

    fn code_block(&mut self, lang: &str, text: &str) {
        if diagram::is_diagram(lang) {
            let key = diagram_key(lang, text, self.t.dark);
            if let Some(pic) = self.assets.get(&key).filter(|_| self.o.graphics) {
                let avail = self.content_width() as f64 * self.o.cell.0;
                let r = ImgRef {
                    key: None,
                    alt: String::new(),
                    width: None,
                    height: None,
                    link: None,
                };
                let (w, h) = self.img_size(&pic, &r, avail);
                let raster = self.scaled(&key, &pic, w, h);
                self.center.push(true);
                self.place_group(vec![raster], 0, self.o.cell.1, self.o.cell.0);
                self.center.pop();
                return;
            }
        }
        let bg = self.t.code_bg;
        let cw = self.content_width();
        let inner = cw.saturating_sub(2).max(4);
        let text = text.strip_suffix('\n').unwrap_or(text);
        let theme = self.t;
        let highlighted = self.assets.code.entry(code_key(lang, text));
        let lines = highlighted
            .or_insert_with(|| {
                Arc::new(code::highlight(text, code::find_syntax(lang), theme, 5000))
            })
            .clone();

        let label = if lang.is_empty() {
            String::new()
        } else {
            lang.to_string()
        };
        let mut top = self.prefix(true);
        let pad = cw.saturating_sub(label.width() + 1);
        top.push(Span::new(
            " ".repeat(pad),
            Style {
                bg: Some(bg),
                ..Default::default()
            },
        ));
        top.push(Span::new(
            format!("{label} "),
            Style {
                bg: Some(bg),
                fg: Some(self.t.muted),
                italic: true,
                ..Default::default()
            },
        ));
        self.push_code_row(top, cw);

        for line in lines.iter() {
            for part in hard_wrap(line, inner) {
                let mut row = self.prefix(false);
                row.push(Span::new(
                    " ",
                    Style {
                        bg: Some(bg),
                        ..Default::default()
                    },
                ));
                for mut s in part {
                    s.style.bg = Some(bg);
                    row.push(s);
                }
                self.push_code_row(row, cw);
            }
        }
        let p = self.prefix(false);
        self.push_code_row(p, cw);
    }

    fn push_code_row(&mut self, mut spans: Vec<Span>, cw: usize) {
        let pw = self.prefix_width();
        let w = spans_width(&spans);
        let target = pw + cw;
        if w < target {
            spans.push(Span::new(
                " ".repeat(target - w),
                Style {
                    bg: Some(self.t.code_bg),
                    ..Default::default()
                },
            ));
        }
        self.doc.text(spans);
    }

    // -- tables ----------------------------------------------------------------

    fn render_table(&mut self, t: Table) {
        let ncols = t
            .rows
            .iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(0)
            .max(t.aligns.len());
        if ncols == 0 {
            return;
        }
        let cw = self.content_width();
        let overhead = ncols * 3 + 1;
        let avail = cw.saturating_sub(overhead).max(ncols * 3);
        let natural: Vec<usize> = (0..ncols)
            .map(|c| {
                let cells = t.rows.iter().filter_map(|r| r.get(c));
                cells
                    .map(|cell| spans_width(&cell.text).max(self.cell_images(cell, avail).1))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let widths = fit_columns(&natural, avail);

        let b = Style::fg(self.t.border);
        let stripe = self.t.code_bg;
        let line = |l: &str, m: &str, r: &str, widths: &[usize]| -> Span {
            let mut s = String::from(l);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 < widths.len() { m } else { r });
            }
            Span::new(s, b.clone())
        };

        let mut p = self.prefix(true);
        p.push(line("╭", "┬", "╮", &widths));
        self.doc.text(p);
        for (ri, row) in t.rows.iter().enumerate() {
            let is_head = ri < t.header_rows;
            let striped = !is_head && (ri - t.header_rows) % 2 == 1;
            let cell_bg = if striped { Some(stripe) } else { None };
            // each cell: its wrapped text and the rows its pictures take
            let empty = Cell::default();
            let cells: Vec<CellLayout> = (0..ncols)
                .map(|c| {
                    let cell = row.get(c).unwrap_or(&empty);
                    let mut text = cell.text.clone();
                    if let Some(f) = text.first_mut() {
                        f.text = f.text.trim_start().to_string();
                    }
                    let blank = text.iter().all(|s| s.text.trim().is_empty());
                    let (picture, cols) = self.cell_images(cell, widths[c]);
                    let rows = picture
                        .as_ref()
                        .map_or(0, |p| (p.h as f64 / self.o.cell.1).ceil().max(1.0) as usize);
                    let lines = match blank && picture.is_some() {
                        true => Vec::new(),
                        false => wrap(&text, widths[c]),
                    };
                    (lines, picture, cols, rows, cell.images_first)
                })
                .collect();
            let height = cells.iter().map(|c| c.0.len() + c.3).max().unwrap_or(1);
            for li in 0..height {
                let mut p = self.prefix(false);
                p.push(Span::new("│", b.clone()));
                let mut x = self.prefix_width() + 1;
                for (c, (lines, picture, pic_cols, pic_rows, pic_first)) in cells.iter().enumerate()
                {
                    // rows `pic_at..pic_at + pic_rows` of the cell are left blank for the picture
                    let pic_at = if *pic_first { 0 } else { lines.len() };
                    let in_picture = (pic_at..pic_at + pic_rows).contains(&li);
                    let text_line = if li < pic_at {
                        li
                    } else {
                        li.saturating_sub(*pic_rows)
                    };
                    let content = match in_picture {
                        true => Vec::new(),
                        false => lines.get(text_line).cloned().unwrap_or_default(),
                    };
                    let w = match in_picture {
                        true => *pic_cols,
                        false => spans_width(&content),
                    };
                    let free = widths[c].saturating_sub(w);
                    let align = t.aligns.get(c).copied().unwrap_or(Alignment::None);
                    let (lp, rp) = match align {
                        Alignment::Right => (free, 0),
                        Alignment::Center => (free / 2, free - free / 2),
                        _ if is_head => (free / 2, free - free / 2),
                        _ => (0, free),
                    };
                    if let Some(picture) = picture.as_ref().filter(|_| li == pic_at) {
                        let (row, col) = (self.doc.rows.len(), (x + 1 + lp) as u32);
                        self.doc.overlay(
                            picture.clone(),
                            row,
                            col,
                            *pic_cols as u32,
                            *pic_rows as u32,
                        );
                    }
                    let pad_style = Style {
                        bg: cell_bg,
                        ..Default::default()
                    };
                    p.push(Span::new(" ".repeat(lp + 1), pad_style.clone()));
                    if in_picture {
                        p.push(Span::new(" ".repeat(w), pad_style.clone()));
                    }
                    for mut s in content {
                        if cell_bg.is_some() && s.style.bg.is_none() {
                            s.style.bg = cell_bg;
                        }
                        p.push(s);
                    }
                    p.push(Span::new(" ".repeat(rp + 1), pad_style));
                    p.push(Span::new("│", b.clone()));
                    x += widths[c] + 3;
                }
                self.doc.text(p);
            }
            if is_head && ri + 1 == t.header_rows {
                let mut p = self.prefix(false);
                p.push(line("├", "┼", "┤", &widths));
                self.doc.text(p);
            }
        }
        let mut p = self.prefix(false);
        p.push(line("╰", "┴", "╯", &widths));
        self.doc.text(p);
    }

    /// The pictures of a table cell side by side, scaled to fit `width`
    /// cells, and the number of cells they take.
    fn cell_images(&mut self, cell: &Cell, width: usize) -> (Option<Arc<Raster>>, usize) {
        if cell.images.is_empty() {
            return (None, 0);
        }
        let cw_px = self.o.cell.0;
        let avail = width as f64 * cw_px;
        let gap = (cw_px / 2.0).round() as u32;
        let gaps = (gap as usize * (cell.images.len() - 1)) as f64;
        let sizes: Vec<(u32, u32)> = cell
            .images
            .iter()
            .map(|(_, pic, r)| self.img_size(pic, r, avail))
            .collect();
        let total: f64 = sizes.iter().map(|s| s.0 as f64).sum();
        let shrink = ((avail - gaps).max(1.0) / total).min(1.0);
        let scale = |v: u32| ((v as f64 * shrink).round() as u32).max(1);
        let rasters: Vec<Arc<Raster>> = cell
            .images
            .iter()
            .zip(&sizes)
            .map(|((key, pic, _), &(w, h))| self.scaled(key, pic, scale(w), scale(h)))
            .collect();
        let raster = match &rasters[..] {
            [one] => one.clone(),
            _ => Arc::new(compose_row(&rasters, gap)),
        };
        let cols = ((raster.w as f64 / cw_px).ceil() as usize).clamp(1, width.max(1));
        (Some(raster), cols)
    }

    // -- html --------------------------------------------------------------------

    fn html(&mut self, h: &str) {
        for tok in html::tokenize(h) {
            match tok {
                Token::Text(t) => {
                    let collapsed = collapse_ws(&t);
                    if !collapsed.is_empty() {
                        self.push_text(&collapsed);
                    }
                }
                Token::Open {
                    name,
                    attrs,
                    self_closing,
                } => self.html_open(&name, &attrs, self_closing),
                Token::Close(name) => self.html_close(&name),
            }
        }
    }

    fn html_open(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        let align_center = Token::attr(attrs, "align")
            .map(|a| a.eq_ignore_ascii_case("center"))
            .unwrap_or(false);
        match name {
            "img" => {
                let r = ImgRef {
                    key: Token::attr(attrs, "src").and_then(|s| resolve(self.base, s)),
                    alt: Token::attr(attrs, "alt").unwrap_or("").to_string(),
                    width: parse_dim(Token::attr(attrs, "width")),
                    height: parse_dim(Token::attr(attrs, "height")),
                    link: self.links.last().cloned().flatten(),
                };
                if let Some(img) = &mut self.image {
                    // nested inside markdown image alt; ignore
                    img.alt.push_str(&r.alt);
                } else {
                    self.inline.push(Inline::Img(r));
                }
            }
            "br" => self.push_text("\n"),
            "hr" => self.event(Event::Rule),
            "p" | "div" | "center" | "section" | "picture" | "figure" => {
                if name == "picture" || name == "figure" {
                    return;
                }
                self.flush_inline();
                self.block_gap();
                if !self_closing {
                    let c = align_center || name == "center" || self.centered();
                    self.center.push(c);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_inline();
                self.block_gap();
                self.center.push(align_center || self.centered());
                self.begin_heading(name[1..].parse().unwrap_or(1));
            }
            "a" => {
                let target = Token::attr(attrs, "href").and_then(|h| link_target(self.base, h));
                let link = self.t.link;
                let t2 = target.clone();
                self.push_style(|s| {
                    s.fg = Some(link);
                    s.link = t2;
                });
                self.html_styles.push("a".into());
                self.links.push(target);
            }
            "b" | "strong" => self.html_style(name, |s| s.bold = true),
            "i" | "em" => self.html_style(name, |s| s.italic = true),
            "s" | "del" | "strike" => self.html_style(name, |s| s.strike = true),
            "u" | "ins" => self.html_style(name, |s| s.underline = true),
            "code" | "kbd" | "samp" => {
                let bg = self.t.inline_code_bg;
                self.html_style(name, |s| s.bg = Some(bg));
            }
            "summary" => {
                self.flush_inline();
                self.push_text("▸ ");
                self.html_style(name, |s| s.bold = true);
            }
            "details" | "ul" | "ol" | "table" | "tr" => self.flush_inline(),
            "dl" => {
                self.flush_inline();
                self.block_gap();
            }
            "dt" => {
                self.flush_inline();
                self.html_style(name, |s| s.bold = true);
            }
            "dd" => {
                self.flush_inline();
                self.html_dd += 1;
                self.blocks.push(Block::Item {
                    marker: None,
                    indent: 4,
                });
            }
            "li" => {
                self.flush_inline();
                self.push_text("• ");
            }
            "td" | "th" => self.push_text(" "),
            _ => {}
        }
    }

    fn html_style(&mut self, name: &str, f: impl FnOnce(&mut Style)) {
        self.push_style(f);
        self.html_styles.push(name.to_string());
    }

    fn html_close(&mut self, name: &str) {
        match name {
            "p" | "div" | "center" | "section" => {
                self.flush_inline();
                self.center.pop();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.end_heading();
                self.center.pop();
            }
            "summary" | "dt" | "li" | "tr" | "details" | "ul" | "ol" | "table" | "dl" => {
                if name == "summary" || name == "dt" {
                    self.close_html_style(name);
                }
                self.flush_inline();
            }
            "dd" if self.html_dd > 0 => {
                self.flush_inline();
                self.html_dd -= 1;
                if matches!(self.blocks.last(), Some(Block::Item { .. })) {
                    self.blocks.pop();
                }
            }
            _ => self.close_html_style(name),
        }
    }

    fn close_html_style(&mut self, name: &str) {
        if self.html_styles.last().map(|s| s == name).unwrap_or(false) {
            self.html_styles.pop();
            self.pop_style();
            if name == "a" {
                self.links.pop();
            }
        }
    }

    fn finish(mut self) -> Doc {
        self.flush_inline();
        let (cw, ch) = self.o.cell;
        let (w, h) = ((cw * 2.0).round() as u32, ch.round() as u32);
        let rgb = |c: Rgb| (c.0, c.1, c.2);
        for (row, col, checked) in std::mem::take(&mut self.checkboxes) {
            if matches!(self.doc.rows.get(row), Some(Row::Text { .. })) {
                let raster =
                    crate::raster::checkbox(checked, w, h, rgb(self.t.accent), rgb(self.t.muted));
                self.doc.overlay(Arc::new(raster), row, col, 2, 1);
            }
        }
        if self.heading.is_some() {
            self.end_heading();
        }
        while self.doc.rows.len() > 1 && self.doc.last_is_blank() {
            self.doc.rows.pop();
        }
        self.doc
    }
}

fn heading_num(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn dim_px(d: Dim, avail: f64, dpr: f64) -> f64 {
    match d {
        Dim::Px(v) => v * dpr,
        Dim::Pct(p) => avail * p / 100.0,
    }
}

/// Water-filling column widths: narrow columns keep their natural width,
/// wide ones share the remaining space.
fn fit_columns(natural: &[usize], avail: usize) -> Vec<usize> {
    let total: usize = natural.iter().sum();
    if total <= avail {
        return natural.to_vec();
    }
    let n = natural.len();
    let mut widths = vec![0; n];
    let mut fixed = vec![false; n];
    let mut remaining = avail;
    loop {
        let open: Vec<usize> = (0..n).filter(|&i| !fixed[i]).collect();
        if open.is_empty() {
            break;
        }
        let share = (remaining / open.len()).max(3);
        let mut changed = false;
        for &i in &open {
            if natural[i] <= share {
                widths[i] = natural[i];
                fixed[i] = true;
                remaining = remaining.saturating_sub(natural[i]);
                changed = true;
            }
        }
        if !changed {
            for &i in &open {
                widths[i] = share;
            }
            break;
        }
    }
    widths
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut ws = false;
    for c in s.chars() {
        if c.is_whitespace() && c != '\u{a0}' {
            ws = true;
        } else {
            if ws {
                out.push(' ');
            }
            ws = false;
            out.push(c);
        }
    }
    if ws && !out.is_empty() {
        out.push(' ');
    }
    if out.trim().is_empty() {
        return String::new();
    }
    out
}

fn superscript(n: usize) -> String {
    const SUP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    n.to_string()
        .bytes()
        .map(|d| SUP[(d - b'0') as usize])
        .collect()
}

/// Split text into plain parts and bare URLs (`https://…`, `www.…`), each
/// with the link target for URLs. With `linkify` off the text stays whole.
fn autolinks(text: &str, linkify: bool) -> Vec<(&str, Option<String>)> {
    let mut out = Vec::new();
    let mut rest = text;
    if !linkify {
        rest = "";
        out.push((text, None));
    }
    loop {
        let found = ["https://", "http://", "www."]
            .iter()
            .filter_map(|p| {
                rest.match_indices(p)
                    .map(|m| m.0)
                    .find(|&i| word_start(rest, i))
            })
            .min();
        let Some(start) = found else {
            break;
        };
        let len = rest[start..]
            .find(|c: char| c.is_whitespace() || c == '<')
            .unwrap_or(rest.len() - start);
        let mut url = &rest[start..start + len];
        // trailing punctuation belongs to the sentence, not the URL
        while let Some(last) = url.chars().last() {
            let unbalanced = last == ')' && url.matches(')').count() > url.matches('(').count();
            if !unbalanced && !".,;:!?'\"*_~".contains(last) {
                break;
            }
            url = &url[..url.len() - last.len_utf8()];
        }
        let host = url
            .split("//")
            .last()
            .unwrap_or("")
            .trim_start_matches("www.");
        if host.is_empty() {
            // not a link after all: keep the prefix as text
            let cut = start + rest[start..].chars().next().map_or(1, char::len_utf8);
            out.push((&rest[..cut], None));
            rest = &rest[cut..];
            continue;
        }
        if start > 0 {
            out.push((&rest[..start], None));
        }
        let target = match url.starts_with("www.") {
            true => format!("http://{url}"),
            false => url.to_string(),
        };
        out.push((url, Some(target)));
        rest = &rest[start + url.len()..];
    }
    if !rest.is_empty() {
        out.push((rest, None));
    }
    out
}

/// Is byte `i` the start of a word (not preceded by a letter or digit)?
fn word_start(s: &str, i: usize) -> bool {
    !s[..i]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
}

/// Replace GitHub emoji shortcodes such as `:rocket:`.
pub fn emojify(s: &str) -> String {
    if !s.contains(':') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(':') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(j) = after.find(':') {
            let name = &after[..j];
            if !name.is_empty()
                && name.len() <= 40
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-')
            {
                if let Some(e) = emojis::get_by_shortcode(name) {
                    out.push_str(e.as_str());
                    rest = &after[j + 1..];
                    continue;
                }
            }
        }
        out.push(':');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(md: &str, width: usize) -> Vec<String> {
        let theme = Theme::new(true);
        let o = LayoutOpts {
            width,
            fg: Rgb(255, 255, 255),
            cell: (10.0, 20.0),
            theme: &theme,
            text_sizing: false,
            graphics: false,
            offline: true,
            wait: true,
            max_img_h: 400.0,
        };
        let mut m = Markdown::new(md.to_string(), Base::Dir(".".into()));
        let doc = m.layout(&o);
        (0..doc.rows.len()).map(|i| doc.plain(i)).collect()
    }

    #[test]
    fn emoji() {
        assert_eq!(emojify("ship it :rocket: now: ok"), "ship it 🚀 now: ok");
        assert_eq!(emojify("12:30:45"), "12:30:45");
    }

    #[test]
    fn basic_blocks() {
        let rows = render(
            "# Title\n\nHello *world* :tada:\n\n- [x] done\n- [ ] todo\n\n> [!NOTE]\n> careful\n",
            40,
        );
        assert_eq!(rows[0], "Title");
        assert!(rows[1].starts_with("━━━"));
        assert!(rows.iter().any(|r| r == "Hello world 🎉"));
        assert!(rows.iter().any(|r| r == "☑ done"));
        assert!(rows.iter().any(|r| r == "☐ todo"));
        assert!(rows.iter().any(|r| r.contains("Note")));
        assert!(rows.iter().any(|r| r == "▎ careful"));
    }

    #[test]
    fn table_layout() {
        let rows = render("| a | b |\n|---|--:|\n| 1 | 22 |\n", 40);
        assert_eq!(rows[0], "╭───┬────╮");
        assert_eq!(rows[2], "├───┼────┤");
        assert_eq!(rows[3], "│ 1 │ 22 │");
        assert_eq!(rows[4], "╰───┴────╯");
    }

    #[test]
    fn html_center_and_img_alt() {
        let rows = render("<p align=\"center\"><img src=\"missing.png\" alt=\"Logo\"></p>\n\n<h1 align=\"center\">Proj</h1>\n", 20);
        assert!(rows.iter().any(|r| r.contains("Logo")));
        assert!(rows
            .iter()
            .any(|r| r.trim() == "Proj" && r.starts_with(' ')));
    }

    #[test]
    fn footnotes_are_numbered_and_last() {
        let rows = render(
            "A[^x] and B[^y].\n\n[^y]: second\n[^x]: first\n\nAfter.\n",
            40,
        );
        assert_eq!(rows[0], "A¹ and B².");
        let after = rows.iter().position(|r| r == "After.").unwrap();
        let first = rows.iter().position(|r| r == "1. first").unwrap();
        let second = rows.iter().position(|r| r == "2. second").unwrap();
        assert!(after < first && first < second);
    }

    #[test]
    fn bare_urls_become_links() {
        let parts = autolinks("see https://a.io/x(1), www.b.org. or http:// x", true);
        let urls: Vec<_> = parts.iter().filter_map(|p| p.1.as_deref()).collect();
        assert_eq!(urls, ["https://a.io/x(1)", "http://www.b.org"]);
        let text: String = parts.iter().map(|p| p.0).collect();
        assert_eq!(text, "see https://a.io/x(1), www.b.org. or http:// x");
        assert_eq!(autolinks("xhttps://a.io", true).len(), 1);
        assert_eq!(autolinks("https://a.io", false), [("https://a.io", None)]);
    }

    #[test]
    fn html_definition_list() {
        let rows = render(
            "<dl>\n<dt>Term</dt>\n<dd>Meaning of it.</dd>\n</dl>\n\nAfter\n",
            40,
        );
        assert_eq!(rows[0], "Term");
        assert_eq!(rows[1], "    Meaning of it.");
        assert_eq!(rows.last().unwrap(), "After");
    }

    #[test]
    fn diagrams_without_graphics_stay_code() {
        let rows = render("```stl\nsolid x\nendsolid\n```\n", 40);
        assert!(rows.iter().any(|r| r.contains("solid x")));
    }

    #[test]
    fn table_cell_images() {
        let dir = std::env::temp_dir().join(format!("shw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="50"><rect width="60" height="50" fill="red"/></svg>"#;
        std::fs::write(dir.join("p.svg"), svg).unwrap();
        let theme = Theme::new(true);
        let o = LayoutOpts {
            width: 60,
            fg: Rgb(255, 255, 255),
            cell: (10.0, 20.0),
            theme: &theme,
            text_sizing: false,
            graphics: true,
            offline: true,
            wait: true,
            max_img_h: 400.0,
        };
        let md = "| a | b |\n|---|---|\n| ![x](p.svg) | text |\n| cap ![x](p.svg) | ![m](missing.png) |\n";
        let doc = Markdown::new(md.into(), Base::Dir(dir.clone())).layout(&o);
        let _ = std::fs::remove_dir_all(&dir);
        let rows: Vec<String> = (0..doc.rows.len()).map(|i| doc.plain(i)).collect();
        // 60x50 px at 10x20 px cells: 6 columns, 3 rows
        let overlays: Vec<_> = doc.images.iter().filter(|im| im.overlay).collect();
        assert_eq!(overlays.len(), 2);
        assert!(overlays
            .iter()
            .all(|im| (im.cols, im.rows, im.col) == (6, 3, 2)));
        assert_eq!(overlays[0].start, 3);
        assert_eq!(rows[3], "│        │ text │");
        assert_eq!(rows[5], "│        │      │");
        // second row: caption above the picture, alt text for the missing one
        assert_eq!(overlays[1].start, 7);
        assert!(rows[6].starts_with("│ cap    │") && rows[6].contains('m'));
        assert_eq!(rows.len(), 11);
    }

    #[test]
    fn columns_fit() {
        assert_eq!(fit_columns(&[5, 5], 20), vec![5, 5]);
        let w = fit_columns(&[3, 100], 40);
        assert_eq!(w[0], 3);
        assert_eq!(w[1], 37);
    }

    #[test]
    fn link_allowlist() {
        let b = Base::Dir("/repo".into());
        assert!(link_target(&b, "https://example.com").is_some());
        assert!(link_target(&b, "mailto:a@b.c").is_some());
        assert_eq!(
            link_target(&b, "docs/guide.md").as_deref(),
            Some("file:///repo/docs/guide.md")
        );
        for bad in [
            "./setup.command",
            "run.sh",
            "javascript:alert(1)",
            "vscode://x",
            "file:///Applications/X.app",
            "https://x/\x1b]52;c;x\x07",
        ] {
            assert!(link_target(&b, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn resolves_urls() {
        let b = Base::Url("https://example.com/a/README.md".into());
        assert_eq!(
            resolve(&b, "img/x.png").unwrap(),
            "https://example.com/a/img/x.png"
        );
        assert_eq!(resolve(&b, "/x.png").unwrap(), "https://example.com/x.png");
    }
}
