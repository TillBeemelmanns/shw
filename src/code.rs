//! Syntax highlighting (syntect) for code blocks and source files.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::doc::{hard_wrap, Doc};
use crate::ui::{Rgb, Span, Style, Theme};

pub fn syntaxes() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn themes() -> &'static ThemeSet {
    static T: OnceLock<ThemeSet> = OnceLock::new();
    T.get_or_init(ThemeSet::load_defaults)
}

/// Load the grammars and themes in the background, so that they are ready
/// by the time the first document is laid out.
pub fn warm() {
    std::thread::spawn(|| {
        syntaxes();
        themes();
    });
}

/// Resolve a fenced-code language token or file name to a syntax.
pub fn find_syntax(token: &str) -> Option<&'static SyntaxReference> {
    let ss = syntaxes();
    let t = token.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    let alias = match t.as_str() {
        "sh" | "shell" | "zsh" | "console" | "shellsession" | "bash" => "bash",
        "js" | "javascript" | "jsx" | "mjs" | "cjs" => "js",
        "ts" | "typescript" | "tsx" => "js",
        "py" | "python" | "python3" => "py",
        "rs" | "rust" => "rs",
        "yml" | "yaml" => "yaml",
        "md" | "markdown" => "md",
        "c++" | "cpp" | "cxx" | "hpp" => "cpp",
        "cs" | "csharp" | "c#" => "cs",
        "toml" | "ini" | "cfg" | "conf" => "ini",
        "dockerfile" | "docker" => "sh",
        "json" | "jsonc" | "json5" => "json",
        "golang" | "go" => "go",
        "objc" | "objective-c" => "m",
        other => other,
    };
    ss.find_syntax_by_token(alias)
        .or_else(|| ss.find_syntax_by_extension(alias))
        .or_else(|| ss.find_syntax_by_name(token))
        .or_else(|| {
            std::path::Path::new(&t)
                .extension()
                .and_then(|e| ss.find_syntax_by_extension(&e.to_string_lossy()))
        })
}

/// Highlight text into one span list per line (tabs expanded, newlines stripped).
pub fn highlight(
    text: &str,
    syntax: Option<&SyntaxReference>,
    theme: &Theme,
    max_lines: usize,
) -> Vec<Vec<Span>> {
    let ss = syntaxes();
    let syntax = syntax.unwrap_or_else(|| ss.find_syntax_plain_text());
    let th = themes()
        .themes
        .get(theme.syntect)
        .or_else(|| themes().themes.values().next())
        .expect("syntect ships default themes");
    let mut h = HighlightLines::new(syntax, th);
    let mut out = Vec::new();
    for (i, line) in LinesWithEndings::from(text).enumerate() {
        let line_trim = line.trim_end_matches(['\n', '\r']);
        if i >= max_lines {
            out.push(vec![Span::plain(line_trim.replace('\t', "    "))]);
            continue;
        }
        let ranges = match h.highlight_line(line, ss) {
            Ok(r) => r,
            Err(_) => {
                out.push(vec![Span::plain(line_trim.replace('\t', "    "))]);
                continue;
            }
        };
        let mut spans = Vec::new();
        for (st, piece) in ranges {
            let piece = piece.trim_end_matches(['\n', '\r']);
            if piece.is_empty() {
                continue;
            }
            let c = st.foreground;
            spans.push(Span::new(
                piece.replace('\t', "    "),
                Style {
                    fg: Some(Rgb(c.r, c.g, c.b)),
                    bold: st.font_style.contains(FontStyle::BOLD),
                    italic: st.font_style.contains(FontStyle::ITALIC),
                    ..Default::default()
                },
            ));
        }
        out.push(spans);
    }
    if text.is_empty() {
        out.push(Vec::new());
    }
    out
}

/// Lay out a whole source file with a line number gutter.
pub fn layout_file(text: &str, name: &str, lang: Option<&str>, width: usize, theme: &Theme) -> Doc {
    let ss = syntaxes();
    let syntax = lang
        .and_then(find_syntax)
        .or_else(|| find_syntax(name))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|l| ss.find_syntax_by_first_line(l))
        });
    let lines = highlight(text, syntax, theme, 50_000);
    let gw = lines.len().to_string().len().max(3);
    let mut doc = Doc::default();
    let content_w = width.saturating_sub(gw + 3).max(10);
    for (i, line) in lines.iter().enumerate() {
        for (j, part) in hard_wrap(line, content_w).into_iter().enumerate() {
            let num = if j == 0 {
                format!("{:>gw$}", i + 1)
            } else {
                " ".repeat(gw)
            };
            let mut spans = vec![
                Span::new(num, Style::fg(theme.muted)),
                Span::new(" │ ", Style::fg(theme.border)),
            ];
            spans.extend(part);
            doc.text(spans);
        }
    }
    doc
}

pub fn syntax_name(name: &str, lang: Option<&str>) -> String {
    lang.and_then(find_syntax)
        .or_else(|| find_syntax(name))
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "Text".into())
}
