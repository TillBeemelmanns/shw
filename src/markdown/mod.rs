//! GitHub-flavoured Markdown renderer: typography, tables, alerts, task lists,
//! emoji shortcodes, syntax highlighted code, images (local, remote, SVG,
//! HTML `<img>`), mermaid diagrams (via `mmdc`) and clickable links.

mod html;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use unicode_width::UnicodeWidthStr;

use crate::code;
use crate::doc::{hard_wrap, wrap, Doc};
use crate::fetch;
use crate::raster::{compose_row, Picture, Raster};
use crate::ui::{spans_width, Rgb, Span, Style, Theme};
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
    /// Maximum image height in pixels.
    pub max_img_h: f64,
}

type Cache = HashMap<String, Option<Arc<Picture>>>;

pub struct Markdown {
    src: String,
    base: Base,
    cache: Cache,
}

impl Markdown {
    pub fn new(src: String, base: Base) -> Self {
        Markdown {
            src,
            base,
            cache: HashMap::new(),
        }
    }

    fn options() -> Options {
        Options::ENABLE_TABLES
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_GFM
            | Options::ENABLE_MATH
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
    }

    pub fn layout(&mut self, o: &LayoutOpts) -> Doc {
        let events: Vec<Event> = Parser::new_ext(&self.src, Self::options()).collect();
        if o.graphics {
            preload(&events, &self.base, &mut self.cache, o);
        }
        let mut r = Renderer::new(o, &self.cache, &self.base);
        for ev in events {
            r.event(ev);
        }
        r.finish()
    }
}

// ---------------------------------------------------------------------------
// Image loading

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

fn link_target(base: &Base, url: &str) -> Option<Rc<str>> {
    if url.starts_with('#') || url.is_empty() {
        return None;
    }
    if fetch::is_url(url) || url.contains("://") || url.starts_with("mailto:") {
        return Some(url.into());
    }
    let r = resolve(base, url)?;
    if fetch::is_url(&r) {
        Some(r.into())
    } else {
        let abs = std::fs::canonicalize(&r)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(r);
        Some(format!("file://{abs}").into())
    }
}

fn mermaid_key(src: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    format!("mermaid:{:x}", h.finish())
}

fn render_mermaid(src: &str, dark: bool) -> Option<Picture> {
    let dir = std::env::temp_dir();
    let id = format!("{}-{}", std::process::id(), crate::kitty::next_id());
    let input = dir.join(format!("shw-mermaid-{id}.mmd"));
    let output = dir.join(format!("shw-mermaid-{id}.png"));
    std::fs::write(&input, src).ok()?;
    let status = std::process::Command::new("mmdc")
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args([
            "-b",
            "transparent",
            "-s",
            "2",
            "-q",
            "-t",
            if dark { "dark" } else { "default" },
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::fs::remove_file(&input);
    let data = std::fs::read(&output).ok();
    let _ = std::fs::remove_file(&output);
    if !status.ok()?.success() {
        return None;
    }
    let mut pic = crate::raster::decode(data?, "diagram.png", None).ok()?;
    // rendered at scale 2: report CSS size
    pic.width = (pic.width / 2).max(1);
    pic.height = (pic.height / 2).max(1);
    Some(pic)
}

/// Collect every image referenced by the document and load the missing ones in parallel.
fn preload(events: &[Event], base: &Base, cache: &mut Cache, o: &LayoutOpts) {
    let mut wanted: Vec<String> = Vec::new();
    let mut mermaid: Vec<String> = Vec::new();
    let mut in_mermaid: Option<String> = None;
    for ev in events {
        match ev {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if let Some(k) = resolve(base, dest_url) {
                    wanted.push(k);
                }
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                for t in html::tokenize(h) {
                    if let Token::Open { name, attrs, .. } = t {
                        if name == "img" {
                            if let Some(k) =
                                Token::attr(&attrs, "src").and_then(|s| resolve(base, s))
                            {
                                wanted.push(k);
                            }
                        }
                    }
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.split_whitespace().next() == Some("mermaid") =>
            {
                in_mermaid = Some(String::new());
            }
            Event::Text(t) => {
                if let Some(m) = &mut in_mermaid {
                    m.push_str(t);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(m) = in_mermaid.take() {
                    mermaid.push(m);
                }
            }
            _ => {}
        }
    }
    wanted.retain(|k| !cache.contains_key(k) && !(o.offline && fetch::is_url(k)));
    wanted.sort();
    wanted.dedup();
    let have_mmdc = !mermaid.is_empty() && fetch::which("mmdc");
    mermaid.retain(|m| !cache.contains_key(&mermaid_key(m)));
    if !have_mmdc {
        for m in &mermaid {
            cache.insert(mermaid_key(m), None);
        }
        mermaid.clear();
    }

    let dark = o.theme.dark;
    let results: Vec<(String, Option<Arc<Picture>>)> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for chunk in wanted.chunks(wanted.len().div_ceil(16).max(1)) {
            handles.push(s.spawn(move || {
                chunk
                    .iter()
                    .map(|k| {
                        let pic = fetch::read_source(k).ok().and_then(|data| {
                            let dir = Path::new(k).parent().filter(|_| !fetch::is_url(k));
                            crate::raster::decode(data, k, dir).ok()
                        });
                        (k.clone(), pic.map(Arc::new))
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for m in &mermaid {
            handles.push(
                s.spawn(move || vec![(mermaid_key(m), render_mermaid(m, dark).map(Arc::new))]),
            );
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    cache.extend(results);
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
    link: Option<Rc<str>>,
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

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span>>>,
    cur: Vec<Vec<Span>>,
    header_rows: usize,
}

struct Renderer<'a> {
    o: &'a LayoutOpts<'a>,
    t: &'a Theme,
    cache: &'a Cache,
    base: &'a Base,
    doc: Doc,
    blocks: Vec<Block>,
    inline: Vec<Inline>,
    styles: Vec<Style>,
    html_styles: Vec<String>,
    links: Vec<Option<Rc<str>>>,
    heading: Option<u8>,
    code: Option<(String, String)>,
    table: Option<Table>,
    image: Option<ImgRef>,
    in_code_span: bool,
    center: Vec<bool>,
    /// Suppress the next block gap (directly after an alert title).
    no_gap: bool,
}

impl<'a> Renderer<'a> {
    fn new(o: &'a LayoutOpts<'a>, cache: &'a Cache, base: &'a Base) -> Self {
        Renderer {
            o,
            t: o.theme,
            cache,
            base,
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
        let border = self.t.border;
        for b in self.blocks.iter_mut() {
            match b {
                Block::Quote { color } => out.push(Span::new("▎ ", Style::fg(*color))),
                Block::Item { marker, indent } => {
                    if consume {
                        if let Some(m) = marker.take() {
                            let pad = indent.saturating_sub(m.width());
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
        let _ = border;
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
        let text = if self.in_code_span {
            text.to_string()
        } else {
            emojify(text)
        };
        let st = self.style();
        self.inline.push(Inline::Text(Span::new(text, st)));
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
                self.push_style(|s| s.fg = Some(link));
                self.push_text(&superscript(&label));
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
                    CodeBlockKind::Fenced(info) => {
                        info.split([' ', ',', '{']).next().unwrap_or("").to_string()
                    }
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
                let m = format!("{}. ", label);
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
                let items = std::mem::take(&mut self.inline);
                let spans: Vec<Span> = items
                    .into_iter()
                    .map(|i| match i {
                        Inline::Text(s) => s,
                        Inline::Img(r) => {
                            Span::new(format!("🖼 {}", r.alt), Style::fg(self.t.muted))
                        }
                    })
                    .collect();
                if let Some(t) = &mut self.table {
                    t.cur.push(spans);
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
        let mut group: Vec<Raster> = Vec::new();
        let gap = (cw_px / 2.0).round() as u32;
        let mut group_w = 0u32;

        for r in refs {
            let pic = if self.o.graphics {
                r.key
                    .as_ref()
                    .and_then(|k| self.cache.get(k))
                    .cloned()
                    .flatten()
            } else {
                None
            };
            let Some(pic) = pic else {
                if !fallback.is_empty() {
                    fallback.push(Span::plain(" "));
                }
                fallback.push(self.alt_span(&r));
                continue;
            };
            let (w, h) = self.img_size(&pic, &r, avail_px);
            let raster = pic.render(w, h);
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

    fn place_group(&mut self, group: Vec<Raster>, gap: u32, ch_px: f64, cw_px: f64) {
        let raster = if group.len() == 1 {
            group.into_iter().next().unwrap()
        } else {
            compose_row(&group, gap)
        };
        let cols = (raster.w as f64 / cw_px).ceil().max(1.0) as u32;
        let rows = (raster.h as f64 / ch_px).ceil().max(1.0) as u32;
        let cw = self.content_width() as u32;
        let mut col = self.prefix_width() as u32;
        if self.centered() && cols < cw {
            col += (cw - cols) / 2;
        }
        let p = self.prefix(true);
        // keep quote bars, drop list markers for image rows
        let prefix: Vec<Span> = p.into_iter().filter(|s| s.text.contains('▎')).collect();
        self.doc.image(raster, col, cols, rows, prefix);
    }

    // -- code ----------------------------------------------------------------

    fn code_block(&mut self, lang: &str, text: &str) {
        if lang == "mermaid" {
            if let Some(Some(pic)) = self.cache.get(&mermaid_key(text)) {
                let pic = pic.clone();
                let avail = self.content_width() as f64 * self.o.cell.0;
                let r = ImgRef {
                    key: None,
                    alt: String::new(),
                    width: None,
                    height: None,
                    link: None,
                };
                let (w, h) = self.img_size(&pic, &r, avail);
                self.center.push(true);
                let raster = pic.render(w, h);
                self.place_group(vec![raster], 0, self.o.cell.1, self.o.cell.0);
                self.center.pop();
                return;
            }
        }
        let bg = self.t.code_bg;
        let cw = self.content_width();
        let inner = cw.saturating_sub(2).max(4);
        let text = text.strip_suffix('\n').unwrap_or(text);
        let syntax = code::find_syntax(lang);
        let lines = code::highlight(text, syntax, self.t, 5000);

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

        for line in lines {
            for part in hard_wrap(&line, inner) {
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
                t.rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| spans_width(cell))
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
            let wrapped: Vec<Vec<Vec<Span>>> = (0..ncols)
                .map(|c| {
                    let cell = row.get(c).cloned().unwrap_or_default();
                    let mut cell = cell;
                    if let Some(f) = cell.first_mut() {
                        f.text = f.text.trim_start().to_string();
                    }
                    wrap(&cell, widths[c])
                })
                .collect();
            let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
            for li in 0..height {
                let mut p = self.prefix(false);
                p.push(Span::new("│", b.clone()));
                for c in 0..ncols {
                    let content = wrapped[c].get(li).cloned().unwrap_or_default();
                    let w = spans_width(&content);
                    let free = widths[c].saturating_sub(w);
                    let align = t.aligns.get(c).copied().unwrap_or(Alignment::None);
                    let (lp, rp) = match align {
                        Alignment::Right => (free, 0),
                        Alignment::Center => (free / 2, free - free / 2),
                        _ if is_head => (free / 2, free - free / 2),
                        _ => (0, free),
                    };
                    let pad_style = Style {
                        bg: cell_bg,
                        ..Default::default()
                    };
                    p.push(Span::new(" ".repeat(lp + 1), pad_style.clone()));
                    for mut s in content {
                        if cell_bg.is_some() && s.style.bg.is_none() {
                            s.style.bg = cell_bg;
                        }
                        p.push(s);
                    }
                    p.push(Span::new(" ".repeat(rp + 1), pad_style));
                    p.push(Span::new("│", b.clone()));
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
            "summary" | "li" | "tr" | "details" | "ul" | "ol" | "table" => {
                if name == "summary" {
                    self.close_html_style(name);
                }
                self.flush_inline();
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
        if let Some(level) = self.heading {
            let _ = level;
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

fn superscript(label: &str) -> String {
    const SUP: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
    if label.chars().all(|c| c.is_ascii_digit()) {
        label
            .chars()
            .map(|c| SUP[c as usize - '0' as usize])
            .collect()
    } else {
        format!("[{label}]")
    }
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
            cell: (10.0, 20.0),
            theme: &theme,
            text_sizing: false,
            graphics: false,
            offline: true,
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
    fn columns_fit() {
        assert_eq!(fit_columns(&[5, 5], 20), vec![5, 5]);
        let w = fit_columns(&[3, 100], 40);
        assert_eq!(w[0], 3);
        assert_eq!(w[1], 37);
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
