use clap::{Parser, ValueEnum};

use crate::detect::Kind;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TransferArg {
    Auto,
    Direct,
    File,
    Shm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ThemeArg {
    Auto,
    Dark,
    Light,
}

const AFTER_HELP: &str = "\
\x1b[1mExamples:\x1b[0m
  shw photo.jpg                 open the image viewer (wheel / + - to zoom)
  shw ~/Pictures                browse every image/video/markdown file in a folder
  shw clip.mp4 --start 1:30     play a video from 1m30s
  shw README.md                 GitHub-style markdown pager
  shw -i diagram.svg            print inline into the terminal (like icat)
  curl -sL https://…/x.png | shw -
  fzf --preview 'shw -i {}'     preview files in fzf

\x1b[1mKeys:\x1b[0m press ? inside any viewer.

Requires a terminal implementing the kitty graphics protocol
(kitty, Ghostty, WezTerm, Konsole, …). Works over ssh and in tmux
(with `set -g allow-passthrough on`).";

/// Blazing fast viewer for images, videos and Markdown in kitty-compatible terminals.
#[derive(Parser, Debug)]
#[command(name = "shw", version, about, after_help = AFTER_HELP, max_term_width = 100)]
pub struct Cli {
    /// Files, directories or URLs to show. Use `-` for stdin.
    #[arg(value_name = "FILE")]
    pub files: Vec<String>,

    /// Print inline into the terminal instead of opening the interactive viewer.
    #[arg(short, long, env = "SHW_INLINE")]
    pub inline: bool,

    /// Force the file type instead of detecting it.
    #[arg(short = 't', long = "type", value_enum, value_name = "TYPE")]
    pub kind: Option<Kind>,

    /// Syntax used for highlighting code / stdin (e.g. rust, py, json).
    #[arg(short, long, value_name = "LANG")]
    pub language: Option<String>,

    /// Width in cells for inline output (defaults to the terminal or fzf preview width).
    #[arg(short = 'W', long, value_name = "COLS")]
    pub width: Option<u16>,

    /// Height in cells for inline output.
    #[arg(short = 'H', long, value_name = "ROWS")]
    pub height: Option<u16>,

    /// Upscale small images to fill the inline area.
    #[arg(long)]
    pub upscale: bool,

    /// Colour theme for markdown and code.
    #[arg(long, value_enum, default_value = "auto", env = "SHW_THEME")]
    pub theme: ThemeArg,

    /// How pixel data is sent to the terminal.
    #[arg(long, value_enum, default_value = "auto", value_name = "MODE")]
    pub transfer: TransferArg,

    /// Use kitty Unicode placeholders (automatic inside tmux and fzf).
    #[arg(long)]
    pub placeholders: bool,

    /// Video: start position in seconds or [hh:]mm:ss.
    #[arg(short, long, value_name = "TIME", value_parser = parse_time, default_value = "0")]
    pub start: f64,

    /// Video: play without sound.
    #[arg(short, long)]
    pub mute: bool,

    /// Video: loop playback.
    #[arg(long = "loop")]
    pub looping: bool,

    /// Do not download remote images referenced by markdown.
    #[arg(long, env = "SHW_OFFLINE")]
    pub offline: bool,

    /// Skip terminal capability detection and assume kitty graphics support.
    #[arg(short, long)]
    pub force: bool,

    /// Print detected terminal capabilities and exit.
    #[arg(long)]
    pub doctor: bool,

    /// Print a shell completion script and exit.
    #[arg(long, value_enum, value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,
}

fn parse_time(s: &str) -> Result<f64, String> {
    let mut total = 0.0;
    for part in s.split(':') {
        let v: f64 = part
            .trim()
            .parse()
            .map_err(|_| format!("invalid time `{s}`"))?;
        total = total * 60.0 + v;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("90").unwrap(), 90.0);
        assert_eq!(parse_time("1:30").unwrap(), 90.0);
        assert_eq!(parse_time("1:00:05").unwrap(), 3605.0);
        assert!(parse_time("x").is_err());
    }

    #[test]
    fn cli_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
