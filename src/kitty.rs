//! Kitty graphics protocol encoder.
//!
//! Supports direct (base64 + optional zlib, works over ssh), temp-file and
//! shared-memory transmission, tmux passthrough, and Unicode placeholders.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use base64::Engine;
use flate2::write::ZlibEncoder;
use flate2::Compression;

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

pub fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // The name must contain "tty-graphics-protocol" so terminals agree to delete it.
    std::env::temp_dir().join(format!(
        "shw-tty-graphics-protocol-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ))
}

pub fn shm_name() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // macOS limits POSIX shm names to 31 characters.
    format!("/shw{}-{}", std::process::id() % 100000, n % 100000)
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

#[derive(Clone, Debug)]
pub struct Kitty {
    pub transfer: Transfer,
    pub tmux: bool,
    pub compress: bool,
    /// Use Unicode placeholders instead of direct placements.
    pub placeholders: bool,
}

impl Kitty {
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
                if std::fs::write(&path, &img.data).is_ok() {
                    let ctrl = format!("{base},t=t");
                    let p = b64.encode(path.to_string_lossy().as_bytes());
                    self.push(out, &format!("\x1b_G{ctrl};{p}\x1b\\"));
                    return;
                }
            }
            Transfer::Direct => {}
        }
        let (ctrl, payload) = if self.compress && img.data.len() > 4096 {
            let mut enc =
                ZlibEncoder::new(Vec::with_capacity(img.data.len() / 2), Compression::fast());
            let _ = enc.write_all(&img.data);
            (
                format!("{base},o=z"),
                enc.finish().unwrap_or_else(|_| img.data.clone()),
            )
        } else {
            (base, img.data.clone())
        };
        let encoded = b64.encode(&payload);
        self.write_chunks(out, &ctrl, encoded.as_bytes());
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

    /// Delete all visible placements but keep image data.
    pub fn clear_placements(&self, out: &mut Vec<u8>) {
        self.cmd(out, "a=d,d=a,q=2");
    }
}

/// Text for one row of Unicode placeholder cells referencing image `id`.
pub fn placeholder_row(id: u32, row: u32, cols: u32) -> String {
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    let mut s = format!("\x1b[38;2;{r};{g};{b}m");
    let rd = DIACRITICS[(row as usize).min(DIACRITICS.len() - 1)];
    for c in 0..cols {
        s.push('\u{10EEEE}');
        if c == 0 {
            s.push(rd);
            s.push(DIACRITICS[0]);
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
    fn ids_fit_24_bits() {
        for _ in 0..1000 {
            let id = next_id();
            assert!(id > 0 && id < (1 << 24));
        }
    }
}
