//! Terminal environment: size queries, environment detection and capability probing.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::kitty;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub px_w: u32,
    pub px_h: u32,
}

impl WinSize {
    /// Cell size in pixels. Falls back to a sane default if the terminal does not report pixels.
    pub fn cell(&self) -> (f64, f64) {
        if self.px_w > 0 && self.px_h > 0 && self.cols > 0 && self.rows > 0 {
            (
                self.px_w as f64 / self.cols as f64,
                self.px_h as f64 / self.rows as f64,
            )
        } else {
            (10.0, 20.0)
        }
    }
}

fn ioctl_size(fd: i32) -> Option<WinSize> {
    // SAFETY: TIOCGWINSZ fills a plain-old-data winsize struct.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            Some(WinSize {
                cols: ws.ws_col,
                rows: ws.ws_row,
                px_w: ws.ws_xpixel as u32,
                px_h: ws.ws_ypixel as u32,
            })
        } else {
            None
        }
    }
}

pub fn open_tty() -> Option<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()
}

/// Raw terminal size (cells + pixels) as reported by the kernel.
pub fn raw_size() -> Option<WinSize> {
    ioctl_size(libc::STDOUT_FILENO)
        .or_else(|| ioctl_size(libc::STDIN_FILENO))
        .or_else(|| open_tty().and_then(|f| ioctl_size(f.as_raw_fd())))
}

pub fn stdout_is_tty() -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Facts about the environment that do not need a terminal round trip.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub tmux: bool,
    pub ssh: bool,
    /// fzf preview window size (columns, lines) when running as an fzf previewer.
    pub fzf: Option<(u16, u16)>,
    pub term: String,
    pub term_program: String,
}

impl Env {
    pub fn detect() -> Self {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let fzf = match (
            var("FZF_PREVIEW_COLUMNS").parse::<u16>(),
            var("FZF_PREVIEW_LINES").parse::<u16>(),
        ) {
            (Ok(c), Ok(l)) => Some((c, l)),
            _ => None,
        };
        Env {
            tmux: !var("TMUX").is_empty() || var("TERM").starts_with("tmux"),
            ssh: !var("SSH_CONNECTION").is_empty()
                || !var("SSH_TTY").is_empty()
                || !var("SSH_CLIENT").is_empty(),
            fzf,
            term: var("TERM"),
            term_program: var("TERM_PROGRAM"),
        }
    }

    /// Heuristic for terminals known to implement the kitty graphics protocol.
    pub fn known_kitty_terminal(&self) -> bool {
        let t = self.term.to_ascii_lowercase();
        let p = self.term_program.to_ascii_lowercase();
        t.contains("kitty")
            || t.contains("ghostty")
            || p.contains("ghostty")
            || p.contains("wezterm")
            || p.contains("kitty")
            || std::env::var_os("KITTY_WINDOW_ID").is_some()
            || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
            || std::env::var_os("KONSOLE_VERSION").is_some()
    }
}

/// Terminal capabilities discovered by querying the terminal.
#[derive(Clone, Debug)]
pub struct Caps {
    pub graphics: bool,
    pub file_transfer: bool,
    pub shm_transfer: bool,
    pub text_sizing: bool,
    pub dark: bool,
    /// Default foreground colour reported via OSC 10.
    pub fg: Option<(u8, u8, u8)>,
    /// Cell size reported via `CSI 16 t` (width, height).
    pub cell_px: Option<(u32, u32)>,
    /// Whether the terminal answered at all (DA1).
    pub responded: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            graphics: false,
            file_transfer: false,
            shm_transfer: false,
            text_sizing: false,
            dark: true,
            fg: None,
            cell_px: None,
            responded: false,
        }
    }
}

struct RawGuard {
    fd: i32,
    orig: libc::termios,
}

impl RawGuard {
    fn new(fd: i32) -> Option<Self> {
        // SAFETY: standard termios calls on a valid tty descriptor.
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(fd, libc::TCSANOW, &raw);
            Some(RawGuard { fd, orig })
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the attributes we saved.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

fn poll_read(fd: i32, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: single pollfd on the stack.
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

/// Query the terminal for kitty graphics support, transfer modes, background
/// colour, cell size and the text sizing protocol. Uses DA1 as a sentinel so
/// unsupported queries never cause a long wait.
pub fn probe(env: &Env, timeout: Duration) -> Caps {
    let mut caps = Caps::default();
    let Some(mut tty) = open_tty() else {
        return caps;
    };
    let fd = tty.as_raw_fd();
    let Some(_raw) = RawGuard::new(fd) else {
        return caps;
    };

    let wrap = |s: String| if env.tmux { kitty::tmux_wrap(&s) } else { s };
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());

    let mut q = String::new();
    q += &wrap("\x1b_Gi=4000001,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\".to_string());

    // Temp-file and shared-memory transfer only make sense when the terminal
    // shares our filesystem (not over ssh, not through tmux).
    let local = !env.ssh && !env.tmux;
    let tmp_path = kitty::temp_path("probe");
    let file_probe = local && std::fs::write(&tmp_path, [0u8; 3]).is_ok();
    if file_probe {
        q += &format!(
            "\x1b_Gi=4000002,s=1,v=1,a=q,t=t,f=24;{}\x1b\\",
            b64(&tmp_path.to_string_lossy())
        );
    }
    let shm_name = kitty::shm_name();
    let shm_probe = local && kitty::shm_write(&shm_name, &[0u8; 3]).is_ok();
    if shm_probe {
        q += &format!(
            "\x1b_Gi=4000003,s=1,v=1,a=q,t=s,f=24,S=3;{}\x1b\\",
            b64(&shm_name)
        );
    }
    q += "\x1b]10;?\x1b\\";
    q += "\x1b]11;?\x1b\\";
    q += "\x1b[16t";
    if !env.tmux {
        // Text sizing protocol: if supported, a w=2 space advances the cursor two cells.
        q += "\r\x1b]66;w=2; \x07\x1b[6n\r\x1b[2K";
    }
    q += "\x1b[c";

    if tty.write_all(q.as_bytes()).is_err() {
        return caps;
    }
    let _ = tty.flush();

    let start = Instant::now();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while start.elapsed() < timeout {
        if !poll_read(fd, timeout.saturating_sub(start.elapsed())) {
            break;
        }
        match tty.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if has_da1(&buf) {
            caps.responded = true;
            break;
        }
    }
    let _ = std::fs::remove_file(&tmp_path);
    if shm_probe {
        kitty::shm_unlink(&shm_name);
    }

    let s = String::from_utf8_lossy(&buf);
    caps.graphics = s.contains("\x1b_Gi=4000001;OK");
    caps.file_transfer = s.contains("\x1b_Gi=4000002;OK");
    caps.shm_transfer = s.contains("\x1b_Gi=4000003;OK");
    if let Some(dark) = parse_bg(&s) {
        caps.dark = dark;
    }
    caps.fg = parse_osc_rgb(&s, "10")
        .map(|(r, g, b)| ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8));
    caps.cell_px = parse_cell_size(&s);
    caps.text_sizing = parse_cpr_col(&s) == Some(3);
    if env.tmux {
        // tmux swallows graphics replies; trust the outer terminal.
        caps.graphics = true;
    }
    caps
}

fn has_da1(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j < buf.len() && buf[j] == b'c' {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn parse_bg(s: &str) -> Option<bool> {
    let (r, g, b) = parse_osc_rgb(s, "11")?;
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(lum < 0.5)
}

/// Parse an `OSC <code> ; rgb:RRRR/GGGG/BBBB` colour reply into 0..1 components.
fn parse_osc_rgb(s: &str, code: &str) -> Option<(f64, f64, f64)> {
    let pat = format!("]{code};rgb:");
    let i = s.find(&pat)? + pat.len();
    let rest = &s[i..];
    let parts: Vec<&str> = rest
        .split(|c: char| !c.is_ascii_hexdigit() && c != '/')
        .next()?
        .split('/')
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let comp = |p: &str| -> Option<f64> {
        let v = u32::from_str_radix(p, 16).ok()?;
        let max = (1u32 << (4 * p.len() as u32)) - 1;
        Some(v as f64 / max as f64)
    };
    Some((comp(parts[0])?, comp(parts[1])?, comp(parts[2])?))
}

fn parse_cell_size(s: &str) -> Option<(u32, u32)> {
    let i = s.find("\x1b[6;")? + 4;
    let rest = &s[i..];
    let end = rest.find('t')?;
    let mut it = rest[..end].split(';');
    let h: u32 = it.next()?.parse().ok()?;
    let w: u32 = it.next()?.parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

fn parse_cpr_col(s: &str) -> Option<u32> {
    // Find "\x1b[<row>;<col>R"
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let rest = &s[i + 2..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != ';')
                .unwrap_or(rest.len());
            if rest[end..].starts_with('R') {
                let mut it = rest[..end].split(';');
                let _row = it.next()?;
                return it.next()?.parse().ok();
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses() {
        let s = "\x1b_Gi=4000001;OK\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b[6;36;17t\x1b[1;3R\x1b[?62;22c";
        assert!(has_da1(s.as_bytes()));
        assert_eq!(parse_bg(s), Some(true));
        assert_eq!(parse_cell_size(s), Some((17, 36)));
        assert_eq!(parse_cpr_col(s), Some(3));
        assert_eq!(parse_bg("\x1b]11;rgb:ffff/ffff/ffff\x07"), Some(false));
        assert_eq!(
            parse_osc_rgb("\x1b]10;rgb:ffff/8080/0000\x1b\\", "10"),
            Some((1.0, 128.0 / 255.0, 0.0))
        );
    }
}
