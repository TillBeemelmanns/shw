//! Kitty graphics protocol encoder.
//!
//! Supports direct (base64, optionally PNG-compressed, works over ssh), temp-file and
//! shared-memory transmission, tmux passthrough, and Unicode placeholders.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};

use crate::diacritics::DIACRITICS;
use crate::raster::Raster;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    Direct,
    File,
    Shm,
}

impl std::fmt::Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Transfer::Direct => "direct",
            Transfer::File => "temp-file",
            Transfer::Shm => "shared-memory",
        })
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);
static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// Allocate an image id. Ids stay below 2^24 so they can be encoded in a
/// true-colour foreground for Unicode placeholders, and are randomised per
/// process so inline images of earlier runs are not replaced.
/// z-index that puts an image below cells with a non-default background, so
/// status bars (which have a background colour) always cover it.
pub const Z_BELOW_BG: &str = "z=-1073741825";

pub fn next_id() -> u32 {
    let mut cur = NEXT_ID.load(Ordering::Relaxed);
    if cur == 0 {
        let seed = std::process::id()
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(7);
        let start = (seed.wrapping_mul(2654435761) % 0xE0_0000) + 0x10_0000;
        let _ = NEXT_ID.compare_exchange(0, start, Ordering::Relaxed, Ordering::Relaxed);
        cur = NEXT_ID.load(Ordering::Relaxed);
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id >= 0xFF_FFF0 {
        NEXT_ID.store(0x10_0000, Ordering::Relaxed);
    }
    let _ = cur;
    id
}

/// Unpredictable hex token for temp file / shm names.
pub fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // fallback: time, pid and a counter mixed together
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut x = t
            ^ ((std::process::id() as u128) << 64)
            ^ COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
        for b in buf.iter_mut() {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 64) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn temp_path(tag: &str) -> PathBuf {
    // The name must contain "tty-graphics-protocol" so terminals agree to delete it.
    std::env::temp_dir().join(format!(
        "shw-tty-graphics-protocol-{tag}-{}",
        random_hex(12)
    ))
}

/// Create a new file readable only by the user. Fails if anything (including a
/// symlink planted by another user) already exists at `path`.
pub fn write_private_new(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

pub fn shm_name() -> String {
    // macOS limits POSIX shm names to 31 characters.
    format!("/shw-{}", random_hex(10))
}

pub fn shm_write(name: &str, data: &[u8]) -> std::io::Result<()> {
    use std::ffi::CString;
    let cname = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: plain POSIX shared memory calls; every resource is released on all paths.
    unsafe {
        #[cfg(target_os = "macos")]
        let fd = libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600 as libc::c_uint,
        );
        #[cfg(not(target_os = "macos"))]
        let fd = libc::shm_open(
            cname.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
            0o600 as libc::mode_t,
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let len = data.len().max(1);
        if libc::ftruncate(fd, len as libc::off_t) != 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
            return Err(e);
        }
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            libc::shm_unlink(cname.as_ptr());
            return Err(e);
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        libc::munmap(ptr, len);
        libc::close(fd);
    }
    Ok(())
}

pub fn shm_unlink(name: &str) {
    if let Ok(c) = std::ffi::CString::new(name) {
        // SAFETY: unlinking a name we created.
        unsafe {
            libc::shm_unlink(c.as_ptr());
        }
    }
}

/// Wrap an escape sequence for tmux passthrough (`set -g allow-passthrough on`).
pub fn tmux_wrap(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    o.push_str("\x1bPtmux;");
    for c in s.chars() {
        if c == '\x1b' {
            o.push('\x1b');
        }
        o.push(c);
    }
    o.push_str("\x1b\\");
    o
}

/// How images reach the terminal. Decided once at startup from the
/// environment (ssh, tmux, fzf) and the command line; views only ask these
/// questions, never about the environment itself.
#[derive(Clone, Debug)]
pub struct Kitty {
    pub transfer: Transfer,
    /// Wrap escape codes in tmux passthrough.
    pub tmux: bool,
    /// Send inline pixels as PNG.
    pub compress: bool,
    /// Use Unicode placeholders instead of direct placements.
    pub placeholders: bool,
    /// Pixels cross a network link (ssh): every frame costs bandwidth and
    /// latency, so views send fewer, smaller frames.
    pub network: bool,
}

impl Kitty {
    /// Pixels travel inline in the terminal stream (base64 escape codes)
    /// instead of through a temp file or shared memory, so every frame costs
    /// its full size to write and for the terminal (and tmux) to parse.
    pub fn inline(&self) -> bool {
        self.transfer == Transfer::Direct
    }

    fn push(&self, out: &mut Vec<u8>, esc: &str) {
        if self.tmux {
            out.extend_from_slice(tmux_wrap(esc).as_bytes());
        } else {
            out.extend_from_slice(esc.as_bytes());
        }
    }

    /// A control-only graphics command, e.g. `a=d,d=I,i=5`.
    pub fn cmd(&self, out: &mut Vec<u8>, ctrl: &str) {
        self.push(out, &format!("\x1b_G{ctrl}\x1b\\"));
    }

    /// Transmit pixel data. `keys` contains the action and placement keys,
    /// e.g. `a=T,i=7,C=1`. Responses are always suppressed.
    pub fn transmit(&self, out: &mut Vec<u8>, img: &Raster, keys: &str) {
        let f = if img.ch == 4 { 32 } else { 24 };
        let base = format!("{keys},q=2,f={f},s={},v={}", img.w, img.h);
        let b64 = base64::engine::general_purpose::STANDARD;
        match self.transfer {
            Transfer::Shm => {
                let name = shm_name();
                if shm_write(&name, &img.data).is_ok() {
                    let ctrl = format!("{base},t=s,S={}", img.data.len());
                    self.push(out, &format!("\x1b_G{ctrl};{}\x1b\\", b64.encode(&name)));
                    return;
                }
            }
            Transfer::File => {
                let path = temp_path("img");
                if write_private_new(&path, &img.data).is_ok() {
                    let ctrl = format!("{base},t=t");
                    let p = b64.encode(path.to_string_lossy().as_bytes());
                    self.push(out, &format!("\x1b_G{ctrl};{p}\x1b\\"));
                    return;
                }
            }
            Transfer::Direct => {}
        }
        // Over ssh, send PNG: its per-row prediction filters shrink photos far
        // more than zlib on raw pixels (~40% less), and fast PNG encodes quicker.
        if self.compress && img.data.len() > 4096 {
            if let Some(png) = encode_png(img) {
                let ctrl = format!("{keys},q=2,f=100");
                self.write_chunks(out, &ctrl, b64.encode(png).as_bytes());
                return;
            }
        }
        self.write_chunks(out, &base, b64.encode(&img.data).as_bytes());
    }

    fn write_chunks(&self, out: &mut Vec<u8>, ctrl: &str, data: &[u8]) {
        const CHUNK: usize = 4096;
        if data.is_empty() {
            self.push(out, &format!("\x1b_G{ctrl};\x1b\\"));
            return;
        }
        let n = data.len().div_ceil(CHUNK);
        for (i, c) in data.chunks(CHUNK).enumerate() {
            let m = if i + 1 < n { 1 } else { 0 };
            let head = if i == 0 {
                format!("{ctrl},m={m}")
            } else {
                format!("m={m},q=2")
            };
            // chunks are ASCII base64
            let body = std::str::from_utf8(c).unwrap_or("");
            self.push(out, &format!("\x1b_G{head};{body}\x1b\\"));
        }
    }

    /// Delete an image and free its data.
    pub fn delete(&self, out: &mut Vec<u8>, id: u32) {
        self.cmd(out, &format!("a=d,d=I,i={id},q=2"));
    }
}

/// Fast, filtered PNG encoding of a raster (`None` if it fails).
pub fn encode_png(img: &Raster) -> Option<Vec<u8>> {
    let color = if img.ch == 4 {
        ExtendedColorType::Rgba8
    } else {
        ExtendedColorType::Rgb8
    };
    let mut png = Vec::with_capacity(img.data.len() / 2);
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&img.data, img.w, img.h, color)
        .ok()?;
    Some(png)
}

/// Text for one row of Unicode placeholder cells referencing image `id`.
pub fn placeholder_row(id: u32, row: u32, cols: u32) -> String {
    placeholder_row_at(id, row, 0, cols)
}

/// Like [`placeholder_row`], but showing `cols` cells of the image's cell
/// grid starting at column `col` (following cells continue the column).
pub fn placeholder_row_at(id: u32, row: u32, col: u32, cols: u32) -> String {
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    let mut s = format!("\x1b[38;2;{r};{g};{b}m");
    let last = DIACRITICS.len() - 1;
    let rd = DIACRITICS[(row as usize).min(last)];
    let cd = DIACRITICS[(col as usize).min(last)];
    for c in 0..cols {
        s.push('\u{10EEEE}');
        if c == 0 {
            s.push(rd);
            s.push(cd);
        }
    }
    s.push_str("\x1b[39m");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_and_wrap() {
        let k = Kitty {
            transfer: Transfer::Direct,
            tmux: false,
            compress: false,
            placeholders: false,
            network: false,
        };
        let img = Raster {
            w: 64,
            h: 64,
            ch: 4,
            data: vec![0; 64 * 64 * 4],
        };
        let mut out = Vec::new();
        k.transmit(&mut out, &img, "a=T,i=1");
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b_Ga=T,i=1,q=2,f=32,s=64,v=64,m=1;"));
        assert!(s.ends_with("\x1b\\"));
        assert!(s.contains("\x1b_Gm=0,q=2;"));
        assert_eq!(
            tmux_wrap("\x1b_Gx\x1b\\"),
            "\x1bPtmux;\x1b\x1b_Gx\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn compressed_transfer_uses_png() {
        let k = Kitty {
            transfer: Transfer::Direct,
            tmux: false,
            compress: true,
            placeholders: false,
            network: false,
        };
        let img = Raster {
            w: 64,
            h: 64,
            ch: 3,
            data: (0..64 * 64 * 3).map(|i| (i % 251) as u8).collect(),
        };
        let mut out = Vec::new();
        k.transmit(&mut out, &img, "a=t,i=9");
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b_Ga=t,i=9,q=2,f=100,m="));
        let png = encode_png(&img).unwrap();
        let back = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(back.into_raw(), img.data);
    }

    #[test]
    fn placeholder_row_offsets() {
        let s = placeholder_row_at(0x010203, 2, 5, 3);
        assert!(s.starts_with("\x1b[38;2;1;2;3m"));
        let cells: Vec<char> = s.chars().filter(|c| !c.is_ascii()).collect();
        assert_eq!(
            cells,
            vec![
                '\u{10EEEE}',
                DIACRITICS[2],
                DIACRITICS[5],
                '\u{10EEEE}',
                '\u{10EEEE}'
            ]
        );
        assert_eq!(placeholder_row(7, 1, 4), placeholder_row_at(7, 1, 0, 4));
    }

    #[test]
    fn ids_fit_24_bits() {
        for _ in 0..1000 {
            let id = next_id();
            assert!(id > 0 && id < (1 << 24));
        }
    }
}
