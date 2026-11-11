# shw

[![CI](https://github.com/TillBeemelmanns/shw/actions/workflows/ci.yml/badge.svg)](https://github.com/TillBeemelmanns/shw/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/shw.svg)](https://crates.io/crates/shw)
[![license](https://img.shields.io/crates/l/shw.svg)](#license)

**shw** (*show*) is a very fast terminal viewer for **images**, **videos** and **GitHub-flavored Markdown**.
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
  - Animated GIF, WebP and APNG playback. Neighbouring files are decoded in the background, so `n` / `p` feel instant.
- **Videos** (anything ffmpeg can decode). ffmpeg scales each frame to the exact pixel size of the terminal.
  Frames are sent through shared memory locally, or zlib-compressed over ssh.
  - Play/pause, seeking (arrow keys or a click on the progress bar), frame stepping, loop, mute,
    **zoom and pan inside the video**, and saving a frame as PNG.
  - Audio via `ffplay`; it is turned off automatically over ssh.
- **Markdown**, rendered close to the way github.com renders it:
  - headings with rules (H1 at double size on terminals with the
    [text sizing protocol](https://sw.kovidgoyal.net/kitty/text-sizing-protocol/)), bold, italic and strikethrough
  - syntax-highlighted code blocks, tables with alignment and zebra striping, task lists, footnotes, and
    `> [!NOTE]` / `[!TIP]` / `[!IMPORTANT]` / `[!WARNING]` / `[!CAUTION]` alerts
  - `:emoji:` shortcodes and clickable links (OSC 8)
  - **inline images** (local files, remote URLs, SVG badges from shields.io), HTML `<img width=…>` and
    `<p align="center">`
  - mermaid diagrams, if [`mmdc`](https://github.com/mermaid-js/mermaid-cli) is installed
  - search with `/`, `n`, `N`
- **Code / text** files with syntax highlighting and line numbers, and a hex view for binaries.
- **Works everywhere kitty graphics work**: kitty, Ghostty, WezTerm and Konsole, **over ssh**, and inside **tmux**
  (Unicode placeholders + passthrough).
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

Optional runtime tools: `ffmpeg`/`ffprobe` for videos and HEIC/AVIF, `ffplay` for audio, `mmdc` for mermaid diagrams.

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
| `?` | help | `?` | help | `?` | help |

`q` or `Esc` quits everywhere.

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
shw = [{ run = 'shw "$@"', block = true, desc = "shw" }]

[open]
prepend_rules = [
  { mime = "image/*", use = ["shw", "open"] },
  { mime = "video/*", use = ["shw", "open"] },
  { name = "*.md",    use = ["shw", "edit"] },
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

- **ssh**: works out of the box. Data is sent inline and zlib-compressed. Video frames are capped
  at 640×360 and 30 fps for smooth playback over a network, and the terminal scales them up.
- **tmux**: enable passthrough with `set -g allow-passthrough on`. shw then uses Unicode placeholders automatically.
- Run `shw --doctor` to see what was detected.

## How it works

On startup shw sends a handful of queries, terminated by a DA1 sentinel, so detection never hangs. They check for kitty
graphics support, which transfer modes work (direct, temp file, shared memory), the background colour (for the light
or dark theme), the cell size in pixels and text-sizing support. Every frame is resampled to the exact number of device
pixels the terminal will show, so the terminal never rescales the image. Updates are wrapped in synchronized-output
mode and swap between two image ids, so they don't flicker.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo run -- README.md
```

### Releasing

1. Bump `version` in `Cargo.toml` and run `cargo check` to update `Cargo.lock`.
2. Commit and tag: `git commit -am "release v0.2.0" && git tag v0.2.0 && git push --follow-tags`.
3. The [release workflow](.github/workflows/release.yml) builds binaries for all targets, creates the GitHub release with
   checksums and completions, and publishes to crates.io. It needs a `CARGO_REGISTRY_TOKEN` repository secret
   (in a `crates-io` environment).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
