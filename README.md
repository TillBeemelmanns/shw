# shw

[![CI](https://github.com/TillBeemelmanns/shw/actions/workflows/ci.yml/badge.svg)](https://github.com/TillBeemelmanns/shw/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/shw.svg)](https://crates.io/crates/shw)
[![license](https://img.shields.io/crates/l/shw.svg)](#license)

**shw** (*show*) is a very simple terminal viewer for **images**, **videos** and **Markdown** based on Rust.
It renders with pixel precision through the [kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

```sh
shw photo.jpg          # zoomable, pannable image viewer
shw clip.mp4           # video player with audio, seeking and zoom
shw README.md          # GitHub-style markdown with images, tables, alerts, emoji…
shw ~/Pictures         # browse a folder (n / p)
```

## Features

- **Images**: PNG, JPEG, GIF, WebP, BMP, TIFF, ICO, QOI, TGA, DDS, HDR, PNM and **SVG** (rendered as vectors,
  so they stay sharp at any zoom). HEIC, AVIF, JXL and RAW work through `ffmpeg` if it is installed.
  - Images are drawn **1:1 at native pixel resolution** with Lanczos3 SIMD resampling and EXIF orientation.
  - **Zoom** with the mouse wheel (anchored at the pointer) or `+` / `-`. Pan by dragging or with `hjkl`.
    `0` fits the window, `1` shows actual pixels. Above 400 % pixels are drawn crisp (nearest neighbour).
  - Transparent images can be shown over a white or black background (`b` toggles transparent / white / black).
  - Animated GIF, WebP and APNG playback. Neighbouring files are decoded in the background, so `n` / `p` feel instant.
- **Videos** (anything ffmpeg can decode). ffmpeg scales each frame to the exact pixel size of the terminal.
  Frames are sent through shared memory locally, or PNG-compressed over ssh.
  - Play/pause, seeking (arrow keys or a click on the progress bar), frame stepping, loop, mute,
    **zoom and pan inside the video**, and saving a frame as PNG.
  - Audio via `ffplay`; it is turned off automatically over ssh.
- **Markdown**, rendered close to the way github.com renders it:
  - large semibold headings like GitHub's (H1 2×, H2 1.5×, H3 1.25×), typeset with a system font and drawn as
    images so they work in every kitty-graphics terminal; plus bold, italic and strikethrough
  - syntax-highlighted code blocks, tables with alignment and zebra striping, task lists, numbered footnotes,
    and `> [!NOTE]` / `[!TIP]` / `[!IMPORTANT]` / `[!WARNING]` / `[!CAUTION]` alerts
  - `:emoji:` shortcodes and clickable links (OSC 8), including bare URLs
  - **inline images** (local files, remote URLs, SVG badges from shields.io), HTML `<img width=…>` and
    `<p align="center">`
  - **diagrams** in fenced code blocks, as on GitHub, drawn in a few milliseconds without external tools:
    `mermaid` (flowcharts, sequence, class, state, ER, gantt, pie, …), `geojson` / `topojson` maps on a
    base map of country outlines, and ASCII `stl` 3D models
  - the document appears immediately; remote images are downloaded in the background and slot in as they
    arrive
  - search with `/`, `n`, `N`; select text with the mouse to copy it (the terminal's native selection), and
    click links
  - press `e` to edit the file in your `$VISUAL` / `$EDITOR` (falls back to `vi`); the document reloads
    when the editor closes and keeps your reading position
- **Code / text** files with syntax highlighting and line numbers, and a hex view for binaries.
- **Works everywhere kitty graphics work**: kitty, Ghostty, WezTerm and Konsole, **over ssh**, and inside **tmux**.
- **Optimized for ssh + tmux**: zooming and panning images on a remote machine inside tmux stays as responsive as
  locally. Each step moves an image that is already in the terminal (a few bytes), and only the pixels that are
  actually visible are sent once you stop.
- **Scriptable**: `-i/--inline` prints straight into the terminal, like `icat`, which suits fzf previews,
  file managers and scripts. It reads from stdin and URLs, and opens GitHub links (`shw https://github.com/user/repo`
  shows the README).

## Install

**Prebuilt binaries** (Linux x86_64/aarch64 static musl, macOS Intel/Apple Silicon) go into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/TillBeemelmanns/shw/main/install.sh | bash
```

**From crates.io**:

```sh
cargo install shw
```

**From source**:

```sh
git clone https://github.com/TillBeemelmanns/shw && cd shw
cargo install --path .
```

Optional runtime tools: `ffmpeg`/`ffprobe` for videos and HEIC/AVIF, `ffplay` for audio, and
[`mmdc`](https://github.com/mermaid-js/mermaid-cli) as a fallback for mermaid diagrams the built-in renderer
cannot draw.

### Dotfiles

Release assets follow the same naming as `bat`: `shw-v<version>-<arch>-<target>.tar.gz`, with
`unknown-linux-musl` or `apple-darwin` as the target. Each archive holds the binary and shell completions.
A dotfiles install script can therefore be a copy of `install.sh` or simply:

```sh
curl -fsSL https://raw.githubusercontent.com/TillBeemelmanns/shw/main/install.sh | bash
```

## Usage

```
shw [OPTIONS] [FILE]...

  -i, --inline           print inline instead of opening the interactive viewer
  -t, --type <TYPE>      force the type: image, video, markdown, code, binary
  -l, --language <LANG>  syntax for code / stdin (rust, py, json, …)
  -W, --width <COLS>     inline width in cells (default: terminal or fzf preview)
  -H, --height <ROWS>    inline height in cells
      --upscale          enlarge small images to fill the inline area
      --theme <THEME>    auto | dark | light
      --transfer <MODE>  auto | direct | file | shm
      --placeholders     use kitty Unicode placeholders (automatic in tmux / fzf)
  -s, --start <TIME>     video start position (90, 1:30, 1:02:03)
  -m, --mute             play videos without sound
      --loop             loop videos
      --offline          do not download remote images in markdown
  -f, --force            skip terminal detection
      --doctor           print detected terminal capabilities
      --completions <SH> print a completion script (bash, zsh, fish, elvish, powershell)
```

With no arguments, `shw` reads stdin if it is piped, otherwise opens `README.md` in the current directory.

### Keys

| Image viewer | | Video player | | Markdown / code | |
|---|---|---|---|---|---|
| wheel, `+` `-` | zoom | `space` | play / pause | `j` `k` wheel | scroll |
| drag, `hjkl` | pan | `←` `→` | ∓ 5 s | `space` `b` | page |
| `0` / `f` | fit | `↓` `↑` | ∓ 30 s | `d` `u` | half page |
| `1` | 100 % | click bar | seek | `g` `G` | top / bottom |
| `r` `R` | rotate | wheel, `+` `-` `0` | zoom | `/` `n` `N` | search |
| `n` `p` | next / prev file | `m` `l` `.` `s` | mute, loop, step, save frame | `Tab` | next file |
| `b` | background (transparent images) | | | `t` | dark / bright theme |
| | | | | `e` | edit in `$VISUAL` / `$EDITOR` (markdown) |
| `?` | help | `?` | help | `?` | help |

`q` or `Esc` quits everywhere. The bottom line of every viewer lists its keys.

## Integrations

**fzf** file preview (fzf ≥ 0.44 supports kitty images):

```sh
fzf --preview 'shw -i {}'
# or as an alias
alias fzfp="fzf --preview 'shw -i {}' --preview-window=right:60%"
```

**yazi**: open files with shw (`~/.config/yazi/yazi.toml`):

```toml
[opener]
shw = [{ run = 'shw %s', block = true, desc = "shw" }]

[open]
prepend_rules = [
  { mime = "image/*", use = ["shw", "open"] },
  { mime = "video/*", use = ["shw", "open"] },
  { url = "*.md",     use = ["shw", "edit"] },
]
```

**lf** / **ranger**: use `shw -i "$1"` as the previewer, or map a key to `shw "$f"`.

**git**: render the README of any repo with `shw https://github.com/user/repo`.

**Shell completions**:

```sh
shw --completions zsh  > ~/.local/share/zsh/site-functions/_shw
shw --completions bash > ~/.local/share/bash-completion/completions/shw
shw --completions fish > ~/.config/fish/completions/shw.fish
```

## Terminals, ssh and tmux

| Terminal | Status |
|---|---|
| kitty | ✅ full support, including text sizing for headings |
| Ghostty | ✅ |
| WezTerm | ✅ (no Unicode placeholders, so no tmux) |
| Konsole | ✅ basic |
| others | ❌ `shw` reports that the protocol is missing |

- **ssh**: works out of the box. Data is sent inline and PNG-compressed. When you zoom or pan an image,
  shw reuses the frame already on screen and lets the terminal scale and move it (a few bytes per step).
  One sharp frame follows when you stop; above 100 % it carries only the visible pixels at the image's own
  resolution and the terminal scales them, and it is cancelled if you move again. Video frames are capped at
  640×360 and 30 fps for smooth playback over a network, and the terminal scales them up.
- **tmux**: optimized for it, locally and over ssh. Enable passthrough with `set -g allow-passthrough on`; shw then
  uses Unicode placeholders automatically. Zooming and panning only rewrite placeholder cells (plain text that
  tmux redraws like anything else), so they stay smooth even for large images; when you zoom in far, shw uploads
  a crop around the view once instead of re-sending every frame. Optionally add
  `set -as terminal-features ',xterm*:sync'` to your tmux config so tmux also wraps its own redraws in
  synchronized updates.
- Run `shw --doctor` to see what was detected.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- README.md
```

Startup timing: `SHW_TRACE=1 shw README.md` prints how long each phase took (terminal query,
fonts, images, layout, first frame) after you quit.

### Releasing

1. Bump `version` in `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. Commit and tag: `git commit -am "release v0.2.0" && git tag v0.2.0 && git push --follow-tags`.
3. The [release workflow](.github/workflows/release.yml) builds binaries for all targets, creates the GitHub release with
   checksums and completions, and publishes to crates.io. It needs a `CARGO_REGISTRY_TOKEN` repository secret.

## License

Licensed under [MIT](LICENSE)
