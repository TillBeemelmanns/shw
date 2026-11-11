//! Shared application context and interactive screen handling.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::kitty::Kitty;
use crate::term::{Caps, Env, WinSize};
use crate::ui::Theme;

#[derive(Clone, Debug)]
pub struct Opts {
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub offline: bool,
    pub mute: bool,
    pub start: f64,
    pub looping: bool,
    pub lang: Option<String>,
}

#[derive(Clone)]
pub struct Ctx {
    pub env: Env,
    pub caps: Caps,
    pub kitty: Kitty,
    pub theme: Theme,
    pub opts: Opts,
}

/// Rows reserved at the bottom of interactive views: status line + key hints.
pub const CHROME_ROWS: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Next,
    Prev,
    Quit,
}

impl Ctx {
    /// Terminal size with pixel dimensions filled in when the kernel does not
    /// report them (e.g. some ssh setups), and inline overrides applied.
    pub fn size(&self) -> WinSize {
        let mut ws = crate::term::raw_size().unwrap_or(WinSize {
            cols: 80,
            rows: 24,
            px_w: 0,
            px_h: 0,
        });
        // Some terminals (e.g. Ghostty) include window padding in the pixel size,
        // which overestimates the cell size. The terminal's own cell size report
        // (CSI 16 t) is exact; take the smaller value, so a stale report after a
        // font size change can only make images slightly smaller, never overlap.
        if let Some((cw, ch)) = self.caps.cell_px {
            if ws.px_w == 0 || cw * (ws.cols as u32) < ws.px_w {
                ws.px_w = cw * ws.cols as u32;
            }
            if ws.px_h == 0 || ch * (ws.rows as u32) < ws.px_h {
                ws.px_h = ch * ws.rows as u32;
            }
        }
        ws
    }

    /// Size of the drawing area for inline output (fzf preview, --width/--height).
    pub fn inline_size(&self) -> WinSize {
        let full = self.size();
        let (cw, ch) = full.cell();
        let (mut cols, mut rows) = (full.cols, full.rows.saturating_sub(1).max(1));
        if let Some((c, l)) = self.env.fzf {
            cols = c;
            rows = l;
        }
        if let Some(w) = self.opts.width {
            cols = w;
        }
        if let Some(h) = self.opts.height {
            rows = h;
        }
        WinSize {
            cols,
            rows,
            px_w: (cols as f64 * cw) as u32,
            px_h: (rows as f64 * ch) as u32,
        }
    }

    pub fn write(&self, data: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(data);
        let _ = out.flush();
    }
}

/// Alternate screen + raw mode + mouse reporting for the lifetime of the value.
pub struct Screen;

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?7l";
const LEAVE: &str =
    "\x1b[?7h\x1b[?1007l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l";

impl Screen {
    pub fn enter() -> Result<Screen> {
        crossterm::terminal::enable_raw_mode()?;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = std::io::stdout().write_all(LEAVE.as_bytes());
            let _ = std::io::stdout().flush();
            let _ = crossterm::terminal::disable_raw_mode();
            prev(info);
        }));
        let mut out = std::io::stdout().lock();
        out.write_all(ENTER.as_bytes())?;
        out.flush()?;
        Ok(Screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(LEAVE.as_bytes());
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Hand the mouse back to the terminal so text can be selected and copied
/// natively; the wheel still scrolls because alternate-scroll mode (1007)
/// turns it into arrow keys.
pub const MOUSE_TO_TERMINAL: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1007h";
/// Report mouse events (clicks, drags, wheel) to shw again.
pub const MOUSE_TO_APP: &[u8] = b"\x1b[?1007l\x1b[?1000h\x1b[?1002h\x1b[?1006h";

pub fn is_quit(ev: &Event) -> bool {
    match ev {
        Event::Key(KeyEvent {
            code: KeyCode::Char('q'),
            ..
        })
        | Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) => true,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) => modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

/// Read all immediately available events (coalesces mouse-wheel bursts).
pub fn read_events(timeout: std::time::Duration) -> Result<Vec<Event>> {
    let mut evs = Vec::new();
    if crossterm::event::poll(timeout)? {
        evs.push(crossterm::event::read()?);
        while evs.len() < 256 && crossterm::event::poll(std::time::Duration::ZERO)? {
            evs.push(crossterm::event::read()?);
        }
    }
    // Ignore key releases (kitty keyboard protocol)
    evs.retain(
        |e| !matches!(e, Event::Key(k) if k.kind == crossterm::event::KeyEventKind::Release),
    );
    Ok(evs)
}

pub fn pos_label(pos: (usize, usize)) -> String {
    if pos.1 > 1 {
        format!("  [{}/{}]", pos.0 + 1, pos.1)
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Startup timing trace: `SHW_TRACE=1 shw file` prints phase timings on exit.

struct Trace {
    start: Instant,
    marks: Mutex<Vec<(f64, String)>>,
}

fn tracer() -> Option<&'static Trace> {
    static T: OnceLock<Option<Trace>> = OnceLock::new();
    T.get_or_init(|| {
        std::env::var_os("SHW_TRACE").map(|_| Trace {
            start: Instant::now(),
            marks: Mutex::new(Vec::new()),
        })
    })
    .as_ref()
}

/// Record a timing mark (no-op unless SHW_TRACE is set).
pub fn trace(label: impl Into<String>) {
    if let Some(t) = tracer() {
        let ms = t.start.elapsed().as_secs_f64() * 1000.0;
        if let Ok(mut m) = t.marks.lock() {
            m.push((ms, label.into()));
        }
    }
}

/// Print the collected marks to stderr (call after leaving the alternate screen).
pub fn print_trace() {
    if let Some(t) = tracer() {
        if let Ok(m) = t.marks.lock() {
            let mut prev = 0.0;
            for (ms, label) in m.iter() {
                eprintln!("shw trace {ms:8.1} ms  (+{:7.1})  {label}", ms - prev);
                prev = *ms;
            }
        }
    }
}
