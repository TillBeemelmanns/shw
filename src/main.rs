//! shw — show images, videos and Markdown in kitty-compatible terminals.

mod app;
mod cli;
mod code;
mod detect;
mod diacritics;
mod doc;
mod fetch;
mod image_view;
mod kitty;
mod markdown;
mod pager;
mod raster;
mod term;
mod ui;
mod video;

use std::collections::HashMap;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser};

use app::{Ctx, Nav, Opts, Screen};
use cli::{Cli, ThemeArg, TransferArg};
use detect::Kind;
use doc::Doc;
use kitty::{Kitty, Transfer};
use markdown::{Base, LayoutOpts, Markdown};
use pager::Source;
use raster::Picture;
use term::{Caps, Env, WinSize};
use ui::{Span, Style, Theme};

#[derive(Clone)]
struct Item {
    src: String,
    name: String,
    kind: Kind,
    data: Option<Arc<Vec<u8>>>,
}

impl Item {
    fn bytes(&self) -> Result<Arc<Vec<u8>>> {
        match &self.data {
            Some(d) => Ok(d.clone()),
            None => Ok(Arc::new(fetch::read_source(&self.src)?)),
        }
    }

    fn base(&self) -> Base {
        if fetch::is_url(&self.src) {
            Base::Url(self.src.clone())
        } else if self.src == "-" {
            Base::Dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        } else {
            let p = std::fs::canonicalize(&self.src).unwrap_or_else(|_| PathBuf::from(&self.src));
            Base::Dir(
                p.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from(".")),
            )
        }
    }

    fn size_label(&self) -> String {
        if let Some(d) = &self.data {
            return ui::fmt_bytes(d.len() as u64);
        }
        std::fs::metadata(&self.src)
            .map(|m| ui::fmt_bytes(m.len()))
            .unwrap_or_default()
    }
}

fn main() {
    app::install_panic_hook();
    if let Err(e) = run() {
        eprintln!("\x1b[1;31mshw:\x1b[0m {}", ui::sanitize(&format!("{e:#}")));
        std::process::exit(1);
    }
}

fn read_head(path: &Path) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(_) => Vec::new(),
    }
}

fn collect(files: &[String], forced: Option<Kind>, lang: bool) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for f in files {
        if f == "-" {
            let data = fetch::read_source("-")?;
            let kind = forced.unwrap_or_else(|| match detect::by_content(&data) {
                Kind::Code if !lang => Kind::Markdown,
                k => k,
            });
            items.push(Item {
                src: "-".into(),
                name: "stdin".into(),
                kind,
                data: Some(Arc::new(data)),
            });
            continue;
        }
        if fetch::is_url(f) {
            let url = fetch::normalize_url(f);
            let name = url
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(&url)
                .to_string();
            let kind = match forced.or_else(|| detect::by_extension(&url)) {
                Some(k) => k,
                None => {
                    let data = fetch::fetch(&url)?;
                    let k = detect::by_content(&data);
                    let k = if k == Kind::Code { Kind::Markdown } else { k };
                    items.push(Item {
                        src: url,
                        name,
                        kind: k,
                        data: Some(Arc::new(data)),
                    });
                    continue;
                }
            };
            items.push(Item {
                src: url,
                name,
                kind,
                data: None,
            });
            continue;
        }
        let p = Path::new(f);
        if p.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(p)
                .with_context(|| format!("cannot read directory {f}"))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .filter(|p| {
                    !p.file_name()
                        .map(|n| n.to_string_lossy().starts_with('.'))
                        .unwrap_or(true)
                })
                .collect();
            entries.sort_by_key(|p| p.to_string_lossy().to_lowercase());
            for e in entries {
                let s = e.to_string_lossy().into_owned();
                if let Some(kind) = forced.or_else(|| detect::by_extension(&s)) {
                    let name = e
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    items.push(Item {
                        src: s,
                        name,
                        kind,
                        data: None,
                    });
                }
            }
            continue;
        }
        if !p.exists() {
            bail!("no such file: {f}");
        }
        let kind = forced.unwrap_or_else(|| detect::detect(f, &read_head(p)));
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| f.clone());
        items.push(Item {
            src: f.clone(),
            name,
            kind,
            data: None,
        });
    }
    Ok(items)
}

fn load_picture(item: &Item) -> Result<Picture> {
    let data = item.bytes()?;
    let dir = Path::new(&item.src)
        .parent()
        .filter(|_| !fetch::is_url(&item.src))
        .map(|p| p.to_path_buf());
    raster::decode(&data, &item.src, dir.as_deref())
}

fn run() -> Result<()> {
    app::trace("start");
    let cli = Cli::parse();
    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "shw", &mut std::io::stdout());
        return Ok(());
    }
    let env = Env::detect();

    let mut files = cli.files.clone();
    if files.is_empty() && !cli.doctor {
        if !std::io::stdin().is_terminal() {
            files.push("-".into());
        } else if let Some(readme) = ["README.md", "readme.md", "Readme.md"]
            .iter()
            .find(|f| Path::new(f).exists())
        {
            files.push(readme.to_string());
        } else {
            Cli::command().print_help()?;
            return Ok(());
        }
    }
    let items = collect(&files, cli.kind, cli.language.is_some())?;
    if items.is_empty() && !cli.doctor {
        bail!("nothing to show (no supported files found)");
    }

    let tty_out = term::stdout_is_tty();
    let interactive = !cli.inline && tty_out && !cli.doctor;
    let can_probe = (tty_out || cli.doctor) && env.fzf.is_none() && !cli.force;

    app::trace("inputs collected");
    if items
        .iter()
        .any(|i| matches!(i.kind, Kind::Markdown | Kind::Code))
    {
        code::warm(); // overlaps with probing the terminal
    }
    let screen = if interactive {
        Some(Screen::enter()?)
    } else {
        None
    };
    app::trace("screen entered");
    let mut caps = if can_probe {
        term::probe(
            &env,
            if env.ssh {
                Duration::from_millis(2500)
            } else {
                Duration::from_millis(500)
            },
        )
    } else {
        Caps {
            graphics: true,
            ..Default::default()
        }
    };
    app::trace(format!(
        "terminal probed (answered: {}, graphics: {}, file: {}, shm: {}, cell: {:?})",
        caps.responded, caps.graphics, caps.file_transfer, caps.shm_transfer, caps.cell_px
    ));
    if cli.force {
        caps.graphics = true;
    }
    if !can_probe {
        if let Ok(fgbg) = std::env::var("COLORFGBG") {
            if let Some(bg) = fgbg.rsplit(';').next().and_then(|b| b.parse::<u8>().ok()) {
                caps.dark = !matches!(bg, 7 | 15);
            }
        }
    }
    let caps_dark = caps.dark;
    let dark = match cli.theme {
        ThemeArg::Auto => caps.dark,
        ThemeArg::Dark => true,
        ThemeArg::Light => false,
    };
    let transfer = match cli.transfer {
        TransferArg::Auto if caps.shm_transfer => Transfer::Shm,
        TransferArg::Auto if caps.file_transfer => Transfer::File,
        TransferArg::Auto | TransferArg::Direct => Transfer::Direct,
        TransferArg::File => Transfer::File,
        TransferArg::Shm => Transfer::Shm,
    };
    let kitty = Kitty {
        transfer,
        tmux: env.tmux,
        compress: env.ssh,
        placeholders: cli.placeholders || env.tmux || env.fzf.is_some(),
    };
    let ctx = Ctx {
        env,
        caps,
        kitty,
        theme: Theme::for_bg(dark, caps_dark),
        opts: Opts {
            width: cli.width,
            height: cli.height,
            offline: cli.offline,
            mute: cli.mute,
            start: cli.start,
            looping: cli.looping,
            lang: cli.language.clone(),
        },
    };

    if cli.doctor {
        drop(screen);
        doctor(&ctx);
        return Ok(());
    }

    if !ctx.caps.graphics
        && items
            .iter()
            .any(|i| matches!(i.kind, Kind::Image | Kind::Video))
    {
        drop(screen);
        bail!(
            "this terminal does not support the kitty graphics protocol.\n  \
             Use kitty, Ghostty, WezTerm or Konsole (run `shw --doctor` for details, `--force` to try anyway)."
        );
    }

    let r = if interactive {
        let r = run_interactive(&ctx, &items);
        drop(screen);
        r
    } else {
        run_inline(&ctx, &items, cli.upscale)
    };
    app::print_trace();
    r
}

// ---------------------------------------------------------------------------

struct MdSource {
    md: Markdown,
    title: String,
}

/// `wait`: block until downloads have finished (non-interactive output).
fn md_opts<'a>(ctx: &'a Ctx, ws: WinSize, width: usize, wait: bool) -> LayoutOpts<'a> {
    let (_, ch) = ws.cell();
    LayoutOpts {
        width,
        cell: ws.cell(),
        theme: &ctx.theme,
        text_sizing: ctx.caps.text_sizing && !ctx.kitty.placeholders,
        graphics: ctx.caps.graphics,
        offline: ctx.opts.offline,
        wait,
        max_img_h: (ws.rows.saturating_sub(3).max(4) as f64) * ch,
        fg: match (ctx.theme.page, ctx.caps.fg) {
            (Some((fg, _)), _) => fg,
            (None, Some((r, g, b))) => ui::Rgb(r, g, b),
            (None, None) if ctx.theme.dark => ui::Rgb::hex(0xf0f6fc),
            (None, None) => ui::Rgb::hex(0x1f2328),
        },
    }
}

/// Readable line length like GitHub's content column, centered on wide terminals.
fn md_geometry(cols: u16) -> (usize, u16) {
    let cols = cols as usize;
    let max = 120;
    if cols > max + 4 {
        (max, ((cols - max) / 2) as u16)
    } else {
        (cols.saturating_sub(2).max(10), 1)
    }
}

impl Source for MdSource {
    fn layout(&mut self, ctx: &Ctx, ws: WinSize) -> Doc {
        let (width, margin) = md_geometry(ws.cols);
        let mut doc = self.md.layout(&md_opts(ctx, ws, width, false));
        doc.width = width;
        doc.margin = margin;
        doc
    }
    fn poll(&mut self) -> bool {
        self.md.poll()
    }
    fn loading(&self) -> usize {
        self.md.loading()
    }
    fn badge(&self) -> &str {
        "MD"
    }
    fn title(&self) -> String {
        self.title.clone()
    }
}

struct CodeSource {
    text: String,
    name: String,
    lang: Option<String>,
}

impl Source for CodeSource {
    fn layout(&mut self, ctx: &Ctx, ws: WinSize) -> Doc {
        code::layout_file(
            &self.text,
            &self.name,
            self.lang.as_deref(),
            ws.cols as usize,
            &ctx.theme,
        )
    }
    fn badge(&self) -> &str {
        "CODE"
    }
    fn title(&self) -> String {
        format!(
            "{}  ·  {}",
            self.name,
            code::syntax_name(&self.name, self.lang.as_deref())
        )
    }
}

struct TextSource {
    rows: Vec<Vec<Span>>,
    title: String,
    badge: &'static str,
}

impl Source for TextSource {
    fn layout(&mut self, _ctx: &Ctx, _ws: WinSize) -> Doc {
        let mut d = Doc::default();
        for r in &self.rows {
            d.text(r.clone());
        }
        d.margin = 1;
        d
    }
    fn badge(&self) -> &str {
        self.badge
    }
    fn title(&self) -> String {
        self.title.clone()
    }
}

fn hexdump(data: &[u8], theme: &Theme, max: usize) -> Vec<Vec<Span>> {
    let mut rows = Vec::new();
    for (i, chunk) in data.chunks(16).take(max / 16).enumerate() {
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '·'
                }
            })
            .collect();
        rows.push(vec![
            Span::new(format!("{:08x}  ", i * 16), Style::fg(theme.muted)),
            Span::plain(format!("{hex:<48} ")),
            Span::new(ascii, Style::fg(theme.accent)),
        ]);
    }
    rows
}

fn source_for(ctx: &Ctx, item: &Item) -> Result<Box<dyn Source>> {
    let data = item.bytes()?;
    Ok(match item.kind {
        Kind::Markdown => Box::new(MdSource {
            md: Markdown::new(String::from_utf8_lossy(&data).into_owned(), item.base()),
            title: item.name.clone(),
        }),
        Kind::Code => Box::new(CodeSource {
            text: String::from_utf8_lossy(&data).into_owned(),
            name: item.name.clone(),
            lang: ctx.opts.lang.clone(),
        }),
        _ => {
            let mut rows = vec![
                vec![Span::new(
                    item.name.clone(),
                    Style {
                        bold: true,
                        ..Default::default()
                    },
                )],
                vec![Span::new(
                    format!("binary file · {}", ui::fmt_bytes(data.len() as u64)),
                    Style::fg(ctx.theme.muted),
                )],
                vec![],
            ];
            rows.extend(hexdump(&data, &ctx.theme, 64 * 1024));
            Box::new(TextSource {
                rows,
                title: item.name.clone(),
                badge: "BIN",
            })
        }
    })
}

fn error_source(ctx: &Ctx, item: &Item, err: &anyhow::Error) -> TextSource {
    let red = Style {
        fg: Some(ctx.theme.caution),
        bold: true,
        ..Default::default()
    };
    let mut rows = vec![
        vec![],
        vec![Span::new(format!("  ✗ cannot show {}", item.name), red)],
        vec![],
    ];
    for line in format!("{err:#}").lines() {
        rows.push(vec![Span::new(
            format!("    {line}"),
            Style::fg(ctx.theme.muted),
        )]);
    }
    TextSource {
        rows,
        title: item.name.clone(),
        badge: "ERR",
    }
}

type Prefetch = HashMap<usize, JoinHandle<Result<Picture>>>;

fn prefetch(items: &[Item], idx: usize, cache: &mut Prefetch) {
    if idx >= items.len() || cache.contains_key(&idx) || items[idx].kind != Kind::Image {
        return;
    }
    let item = items[idx].clone();
    cache.insert(idx, std::thread::spawn(move || load_picture(&item)));
}

fn view(ctx: &Ctx, items: &[Item], i: usize, cache: &mut Prefetch) -> Result<Nav> {
    let item = &items[i];
    let pos = (i, items.len());
    match item.kind {
        Kind::Image => {
            let ws = ctx.size();
            ctx.write(
                ui::status_bar(
                    &ctx.theme,
                    ws.rows,
                    ws.cols,
                    "IMG",
                    &format!("loading {}…", item.name),
                    "",
                )
                .as_bytes(),
            );
            let pic = match cache.remove(&i) {
                Some(h) => h
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("decoder crashed"))),
                None => load_picture(item),
            }?;
            if items.len() > 1 {
                prefetch(items, (i + 1) % items.len(), cache);
                prefetch(items, (i + items.len() - 1) % items.len(), cache);
            }
            image_view::run(ctx, pic, &item.name, item.size_label(), pos)
        }
        Kind::Video => {
            let path = if item.src == "-" {
                bail!("videos cannot be read from stdin");
            } else {
                item.src.clone()
            };
            video::run(ctx, &path, &item.name, pos)
        }
        _ => {
            let mut src = source_for(ctx, item)?;
            pager::run(ctx, src.as_mut(), pos)
        }
    }
}

fn run_interactive(ctx: &Ctx, items: &[Item]) -> Result<()> {
    let n = items.len();
    let mut i = 0;
    let mut cache: Prefetch = HashMap::new();
    loop {
        let nav = match view(ctx, items, i, &mut cache) {
            Ok(nav) => nav,
            Err(e) => {
                let mut src = error_source(ctx, &items[i], &e);
                pager::run(ctx, &mut src, (i, n))?
            }
        };
        match nav {
            Nav::Quit => break,
            Nav::Next => i = (i + 1) % n,
            Nav::Prev => i = (i + n - 1) % n,
        }
        // drop prefetched pictures that are far away to bound memory
        let keep = [(i + 1) % n, (i + n - 1) % n, i];
        cache.retain(|k, _| keep.contains(k));
    }
    Ok(())
}

fn run_inline(ctx: &Ctx, items: &[Item], upscale: bool) -> Result<()> {
    let multi = items.len() > 1;
    let mut first_err = None;
    for item in items {
        if multi {
            let mut h = String::new();
            Style {
                fg: Some(ctx.theme.accent),
                bold: true,
                ..Default::default()
            }
            .sgr(&mut h);
            println!("{h}── {} ──\x1b[0m", ui::sanitize(&item.name));
        }
        let ws = ctx.inline_size();
        let r: Result<()> = (|| {
            match item.kind {
                Kind::Image => {
                    let pic = load_picture(item)?;
                    image_view::print_inline(ctx, &pic, ws, upscale);
                }
                Kind::Video => video::print_inline(ctx, &item.src, &item.name, ws)?,
                Kind::Markdown => {
                    let data = item.bytes()?;
                    let mut md =
                        Markdown::new(String::from_utf8_lossy(&data).into_owned(), item.base());
                    let mut doc = md.layout(&md_opts(ctx, ws, ws.cols as usize, true));
                    pager::print_doc(ctx, &mut doc, ws.cols as usize);
                }
                Kind::Code => {
                    let data = item.bytes()?;
                    let text = String::from_utf8_lossy(&data);
                    let mut doc = code::layout_file(
                        &text,
                        &item.name,
                        ctx.opts.lang.as_deref(),
                        ws.cols as usize,
                        &ctx.theme,
                    );
                    if let Some(h) = ctx.opts.height {
                        doc.rows.truncate(h as usize);
                    } else if let Some((_, l)) = ctx.env.fzf {
                        doc.rows.truncate(l as usize);
                    }
                    pager::print_doc(ctx, &mut doc, ws.cols as usize);
                }
                Kind::Binary => {
                    let data = item.bytes()?;
                    let mut doc = Doc::default();
                    for r in hexdump(&data, &ctx.theme, 16 * ws.rows.max(4) as usize) {
                        doc.text(r);
                    }
                    pager::print_doc(ctx, &mut doc, ws.cols as usize);
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            eprintln!(
                "\x1b[1;31mshw:\x1b[0m {}",
                ui::sanitize(&format!("{}: {e:#}", item.name))
            );
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) if items.len() == 1 => {
            Err(e.context("failed")).map_err(|_| anyhow::anyhow!("could not display file"))
        }
        _ => Ok(()),
    }
}

fn doctor(ctx: &Ctx) {
    let ws = ctx.size();
    let (cw, ch) = ws.cell();
    let yes = |b: bool| {
        if b {
            "\x1b[32myes\x1b[0m"
        } else {
            "\x1b[31mno\x1b[0m"
        }
    };
    println!("\x1b[1mshw {}\x1b[0m", env!("CARGO_PKG_VERSION"));
    println!(
        "  TERM / TERM_PROGRAM  {} / {}",
        ctx.env.term, ctx.env.term_program
    );
    println!("  terminal responded   {}", yes(ctx.caps.responded));
    println!("  kitty graphics       {}", yes(ctx.caps.graphics));
    println!("  temp-file transfer   {}", yes(ctx.caps.file_transfer));
    println!("  shm transfer         {}", yes(ctx.caps.shm_transfer));
    println!("  text sizing (OSC 66) {}", yes(ctx.caps.text_sizing));
    println!(
        "  transfer mode        {}{}",
        ctx.kitty.transfer,
        if ctx.kitty.compress { " + png" } else { "" }
    );
    println!("  unicode placeholders {}", yes(ctx.kitty.placeholders));
    println!(
        "  background           {}",
        if ctx.caps.dark { "dark" } else { "light" }
    );
    println!(
        "  window               {}x{} cells, {}x{} px, cell {:.1}x{:.1} px",
        ws.cols, ws.rows, ws.px_w, ws.px_h, cw, ch
    );
    println!(
        "  ssh / tmux / fzf     {} / {} / {}",
        yes(ctx.env.ssh),
        yes(ctx.env.tmux),
        yes(ctx.env.fzf.is_some())
    );
    println!(
        "  ffmpeg / ffprobe     {} / {}",
        yes(fetch::which("ffmpeg")),
        yes(fetch::which("ffprobe"))
    );
    println!("  ffplay (audio)       {}", yes(fetch::which("ffplay")));
    println!("  mmdc (mermaid, opt.) {}", yes(fetch::which("mmdc")));
    if !ctx.caps.graphics && ctx.env.known_kitty_terminal() {
        println!("\n  note: this looks like a kitty-compatible terminal but it did not answer; try --force");
    }
}
