//! Video player: ffmpeg decodes and scales frames to the exact pixel size of
//! the terminal, frames are streamed with the kitty graphics protocol
//! (shared memory / temp file locally, PNG-compressed direct over ssh) and
//! audio plays through ffplay.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyCode, MouseButton, MouseEventKind};

use crate::app::{is_quit, pos_label, read_events, Ctx, Nav, CHROME_ROWS};
use crate::fetch::{tool, which};
use crate::kitty::{next_id, placeholder_row};
use crate::raster::Raster;
use crate::term::WinSize;
use crate::ui::{fmt_duration, help_screen, hint_bar, truncate, Rgb, Style};

#[derive(Clone, Debug)]
pub struct Info {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub has_audio: bool,
    pub codec: String,
}

pub fn probe(path: &str) -> Result<Info> {
    let out = Command::new(tool("ffprobe").ok_or_else(|| anyhow!("ffprobe not found — install ffmpeg to play videos"))?)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name,width,height,avg_frame_rate,r_frame_rate:stream_side_data=rotation:stream_tags=rotate:format=duration",
            "-of",
            "json",
            path,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|_| anyhow!("ffprobe not found — install ffmpeg to play videos"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("invalid ffprobe output")?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| anyhow!("no video stream"))?;
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let rate = |k: &str| -> f64 {
        let s = video[k].as_str().unwrap_or("0/0");
        let mut it = s.split('/');
        let n: f64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
        let d: f64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1.0);
        if d > 0.0 {
            n / d
        } else {
            0.0
        }
    };
    let mut fps = rate("avg_frame_rate");
    if !(1.0..=480.0).contains(&fps) {
        fps = rate("r_frame_rate");
    }
    if !(1.0..=480.0).contains(&fps) {
        fps = 25.0;
    }
    let mut width = video["width"].as_u64().unwrap_or(0) as u32;
    let mut height = video["height"].as_u64().unwrap_or(0) as u32;
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|l| l.iter().find_map(|d| d["rotation"].as_f64()))
        .or_else(|| {
            video["tags"]["rotate"]
                .as_str()
                .and_then(|r| r.parse().ok())
        })
        .unwrap_or(0.0);
    if (rotation.abs() - 90.0).abs() < 1.0 || (rotation.abs() - 270.0).abs() < 1.0 {
        std::mem::swap(&mut width, &mut height);
    }
    if width == 0 || height == 0 {
        return Err(anyhow!("cannot determine video size"));
    }
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    Ok(Info {
        width,
        height,
        fps,
        duration,
        has_audio,
        codec: video["codec_name"].as_str().unwrap_or("?").to_string(),
    })
}

/// Grab a single frame at `at` seconds, scaled to `w` x `h`.
pub fn thumbnail(path: &str, at: f64, w: u32, h: u32) -> Result<Raster> {
    let out = Command::new(
        tool("ffmpeg")
            .ok_or_else(|| anyhow!("ffmpeg not found — install ffmpeg to play videos"))?,
    )
    .args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-ss",
        &format!("{at:.3}"),
        "-i",
        path,
    ])
    .args([
        "-frames:v",
        "1",
        "-an",
        "-sn",
        "-vf",
        &format!("scale={w}:{h}:flags=lanczos"),
    ])
    .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
    .stdin(Stdio::null())
    .stderr(Stdio::null())
    .output()
    .map_err(|_| anyhow!("ffmpeg not found — install ffmpeg to preview videos"))?;
    let need = (w * h * 3) as usize;
    if out.stdout.len() < need {
        if at > 0.0 {
            return thumbnail(path, 0.0, w, h);
        }
        return Err(anyhow!("could not decode a frame"));
    }
    let mut data = out.stdout;
    data.truncate(need);
    Ok(Raster { w, h, ch: 3, data })
}

/// Viewport in source pixels (zoom / pan).
#[derive(Clone, Copy, Debug, PartialEq)]
struct View {
    zoom: f64,
    cx: f64,
    cy: f64,
}

struct Pipeline {
    child: Child,
    rx: Receiver<Vec<u8>>,
    /// Frame buffers that are no longer shown go back to the reader thread:
    /// allocating a fresh one per frame costs megabytes of page faults.
    spare: Sender<Vec<u8>>,
    fw: u32,
    fh: u32,
    fps: f64,
    start: f64,
    frames: u64,
    eof: bool,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Layout {
    fw: u32,
    fh: u32,
    /// display rect in cells: col, row, cols, rows (0-based)
    col: u32,
    row: u32,
    cols: u32,
    rows: u32,
    xo: u32,
    yo: u32,
    scaled: bool,
    crop: Option<(u32, u32, u32, u32)>,
}

fn compute_layout(info: &Info, view: View, ws: WinSize, max_px: f64) -> Layout {
    let (cw, ch) = ws.cell();
    let aw = ws.cols as f64 * cw;
    let ah = ws.rows.saturating_sub(CHROME_ROWS).max(1) as f64 * ch;
    let (iw, ih) = (info.width as f64, info.height as f64);
    let fit = (aw / iw).min(ah / ih);
    let z = fit * view.zoom;
    let disp_w = (iw * z).min(aw);
    let disp_h = (ih * z).min(ah);
    let src_w = (disp_w / z).min(iw);
    let src_h = (disp_h / z).min(ih);
    let cx = view.cx.clamp(src_w / 2.0, iw - src_w / 2.0);
    let cy = view.cy.clamp(src_h / 2.0, ih - src_h / 2.0);
    let crop = if view.zoom > 1.0001 {
        Some((
            src_w.round().max(2.0) as u32,
            src_h.round().max(2.0) as u32,
            (cx - src_w / 2.0).max(0.0).round() as u32,
            (cy - src_h / 2.0).max(0.0).round() as u32,
        ))
    } else {
        None
    };
    // cap the frame resolution for throughput
    let px = disp_w * disp_h;
    let s = if px > max_px {
        (max_px / px).sqrt()
    } else {
        1.0
    };
    let fw = ((disp_w * s).round() as u32).max(2) & !1;
    let fh = ((disp_h * s).round() as u32).max(2) & !1;
    let off_x = ((aw - disp_w) / 2.0).max(0.0);
    let off_y = ((ah - disp_h) / 2.0).max(0.0);
    let col = (off_x / cw).floor();
    let row = (off_y / ch).floor();
    Layout {
        fw,
        fh,
        col: col as u32,
        row: row as u32,
        cols: (disp_w / cw).round().max(1.0) as u32,
        rows: (disp_h / ch).round().max(1.0) as u32,
        xo: ((off_x - col * cw) as u32).min(cw as u32 - 1),
        yo: ((off_y - row * ch) as u32).min(ch as u32 - 1),
        scaled: s < 1.0,
        crop,
    }
}

fn spawn_pipeline(
    path: &str,
    at: f64,
    info: &Info,
    lay: &Layout,
    max_fps: f64,
) -> Result<Pipeline> {
    let mut vf = String::new();
    if let Some((w, h, x, y)) = lay.crop {
        vf.push_str(&format!("crop={w}:{h}:{x}:{y},"));
    }
    vf.push_str(&format!("scale={}:{}:flags=lanczos", lay.fw, lay.fh));
    let fps = if info.fps > max_fps {
        vf.push_str(&format!(",fps={max_fps}"));
        max_fps
    } else {
        info.fps
    };
    let mut child = Command::new(
        tool("ffmpeg")
            .ok_or_else(|| anyhow!("ffmpeg not found — install ffmpeg to play videos"))?,
    )
    .args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-threads",
        "0",
    ])
    .args(["-ss", &format!("{:.3}", at.max(0.0)), "-i", path])
    .args([
        "-an", "-sn", "-dn", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .map_err(|_| anyhow!("ffmpeg not found — install ffmpeg to play videos"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no ffmpeg stdout"))?;
    let frame_len = (lay.fw * lay.fh * 3) as usize;
    let (tx, rx) = sync_channel::<Vec<u8>>(3);
    let (spare, spares) = channel::<Vec<u8>>();
    std::thread::spawn(move || loop {
        let reused = spares.try_recv().ok().filter(|b| b.len() == frame_len);
        let mut buf = reused.unwrap_or_else(|| vec![0u8; frame_len]);
        if stdout.read_exact(&mut buf).is_err() {
            break;
        }
        if tx.send(buf).is_err() {
            break;
        }
    });
    Ok(Pipeline {
        child,
        rx,
        spare,
        fw: lay.fw,
        fh: lay.fh,
        fps,
        start: at,
        frames: 0,
        eof: false,
    })
}

fn spawn_audio(path: &str, at: f64) -> Option<Child> {
    Command::new(tool("ffplay")?)
        .args([
            "-nodisp",
            "-autoexit",
            "-vn",
            "-sn",
            "-loglevel",
            "quiet",
            "-ss",
            &format!("{:.3}", at.max(0.0)),
            path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

struct Clock {
    base: f64,
    since: Instant,
    paused: bool,
}

impl Clock {
    fn now(&self) -> f64 {
        if self.paused {
            self.base
        } else {
            self.base + self.since.elapsed().as_secs_f64()
        }
    }
    fn set(&mut self, t: f64) {
        self.base = t;
        self.since = Instant::now();
    }
}

const HINTS: &[(&str, &str)] = &[
    ("space", "play/pause"),
    ("←/→", "±5s"),
    ("↓/↑", "±30s"),
    ("click", "seek"),
    ("wheel/+-", "zoom"),
    ("0", "reset"),
    ("m", "mute"),
    ("l", "loop"),
    (".", "step"),
    ("s", "snapshot"),
    ("n/p", "next/prev"),
];

const HELP: &[(&str, &str)] = &[
    ("space / k", "play / pause"),
    ("← / →", "seek -5s / +5s"),
    ("↓ / ↑", "seek -30s / +30s"),
    ("click bar", "seek to position"),
    ("wheel / + / -", "zoom (at pointer)"),
    ("drag / 0", "pan / reset zoom"),
    ("m", "mute / unmute"),
    ("l", "toggle loop"),
    (".", "step frame (paused)"),
    ("s", "save frame as PNG"),
    ("n / p", "next / previous file"),
    ("q / Esc", "quit"),
];

#[allow(clippy::too_many_arguments)]
fn status(
    ctx: &Ctx,
    ws: WinSize,
    name: &str,
    pos: (usize, usize),
    t: f64,
    info: &Info,
    paused: bool,
    muted: bool,
    looping: bool,
    msg: &Option<String>,
) -> (String, (u16, u16)) {
    let th = &ctx.theme;
    let cols = ws.cols as usize;
    let mut s = format!("\x1b[{};1H\x1b[0m\x1b[2K", ws.rows - 1);
    let mut badge = String::new();
    Style {
        fg: Some(if th.dark {
            Rgb::hex(0x0d1117)
        } else {
            Rgb::hex(0xffffff)
        }),
        bg: Some(th.accent),
        bold: true,
        ..Default::default()
    }
    .sgr(&mut badge);
    let icon = if paused { "⏸" } else { "▶" };
    s.push_str(&format!("{badge} {icon} "));
    let mut bar = String::new();
    Style {
        fg: Some(th.bar_fg),
        bg: Some(th.bar_bg),
        ..Default::default()
    }
    .sgr(&mut bar);
    s.push_str(&bar);
    let time = format!(" {} ", fmt_duration(t));
    let dur = if info.duration > 0.0 {
        format!(" {} ", fmt_duration(info.duration))
    } else {
        String::new()
    };
    let flags = format!(
        "{}{}",
        if muted { " 🔇" } else { "" },
        if looping { " 🔁" } else { "" }
    );
    let label = msg
        .clone()
        .unwrap_or_else(|| format!("{}{}", name, pos_label(pos)));
    let label = truncate(&label, (cols / 3).max(10));
    let fixed = 4
        + time.len()
        + dur.len()
        + unicode_width::UnicodeWidthStr::width(flags.as_str())
        + unicode_width::UnicodeWidthStr::width(label.as_str())
        + 3;
    let bar_w = cols.saturating_sub(fixed).max(4);
    let start_col = 4 + time.len() + 1;
    s.push_str(&time);
    let frac = if info.duration > 0.0 {
        (t / info.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (frac * bar_w as f64).round() as usize;
    let mut on = String::new();
    Style {
        fg: Some(th.accent),
        bg: Some(th.bar_bg),
        ..Default::default()
    }
    .sgr(&mut on);
    let mut off = String::new();
    Style {
        fg: Some(th.border),
        bg: Some(th.bar_bg),
        ..Default::default()
    }
    .sgr(&mut off);
    s.push_str(&format!(
        "{on}{}{off}{}",
        "━".repeat(filled),
        "─".repeat(bar_w - filled)
    ));
    s.push_str(&bar);
    s.push_str(&dur);
    s.push_str(&flags);
    s.push_str("  ");
    let mut muted_st = String::new();
    Style {
        fg: Some(th.muted),
        bg: Some(th.bar_bg),
        ..Default::default()
    }
    .sgr(&mut muted_st);
    s.push_str(&muted_st);
    s.push_str(&label);
    let used = fixed - 3 + bar_w;
    if used < cols {
        s.push_str(&" ".repeat(cols - used));
    }
    s.push_str("\x1b[0m");
    s.push_str(&hint_bar(th, ws.rows, ws.cols, HINTS));
    (s, (start_col as u16, (start_col + bar_w) as u16))
}

pub fn run(ctx: &Ctx, path: &str, name: &str, pos: (usize, usize)) -> Result<Nav> {
    let info = probe(path)?;
    let remote = ctx.kitty.network || ctx.kitty.inline();
    // Pixel budget per frame: generous with shared memory, modest when pixels
    // go inline (ssh, tmux).
    let max_px = if remote {
        640.0 * 360.0
    } else {
        2560.0 * 1440.0
    };
    let max_fps = if remote { 30.0 } else { 60.0 };
    let audio_ok = info.has_audio && which("ffplay") && !ctx.env.ssh;
    let mut muted = ctx.opts.mute || !audio_ok;
    let mut looping = ctx.opts.looping;

    let mut ws = ctx.size();
    let mut view = View {
        zoom: 1.0,
        cx: info.width as f64 / 2.0,
        cy: info.height as f64 / 2.0,
    };
    let mut lay = compute_layout(&info, view, ws, max_px);
    let start = ctx.opts.start.clamp(0.0, (info.duration - 0.5).max(0.0));
    let mut pipe = spawn_pipeline(path, start, &info, &lay, max_fps)?;
    let mut clock = Clock {
        base: start,
        since: Instant::now(),
        paused: false,
    };
    let mut audio = if muted {
        None
    } else {
        spawn_audio(path, start)
    };
    let ids = [next_id(), next_id()];
    let mut cur = 0usize;
    let mut shown = false;
    let mut last_frame: Option<Vec<u8>> = None;
    let mut pending_restart: Option<Instant> = None;
    let mut bar_cols = (0u16, 0u16);
    let mut msg: Option<(String, Instant)> = None;
    let mut help = false;
    let mut drag: Option<(u16, u16)> = None;
    let mut need_status = true;
    // second shown in the status line, and whether ffmpeg is behind the clock
    let mut shown_second = -1i64;
    let mut starved = false;
    let mut step = false;
    ctx.write(b"\x1b[0m\x1b[2J");

    let kill_audio = |a: &mut Option<Child>| {
        if let Some(mut c) = a.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    };

    let nav = loop {
        // ---- video frames
        let mut frame: Option<Vec<u8>> = None;
        if !help && (!clock.paused || step) && !pipe.eof {
            let now_t = clock.now();
            loop {
                let due = pipe.start + pipe.frames as f64 / pipe.fps;
                if due > now_t && !(step || (frame.is_none() && !shown)) {
                    break;
                }
                match pipe.rx.try_recv() {
                    Ok(f) => {
                        starved = false;
                        pipe.frames += 1;
                        if let Some(skipped) = frame.replace(f) {
                            let _ = pipe.spare.send(skipped);
                        }
                        if step {
                            step = false;
                            clock.set(pipe.start + pipe.frames as f64 / pipe.fps);
                            break;
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        starved = true;
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        pipe.eof = true;
                        break;
                    }
                }
            }
        }
        if pipe.eof && !clock.paused {
            if looping {
                kill_audio(&mut audio);
                pipe = spawn_pipeline(path, 0.0, &info, &lay, max_fps)?;
                clock.set(0.0);
                if !muted {
                    audio = spawn_audio(path, 0.0);
                }
            } else {
                clock.base = info.duration.max(clock.now().min(info.duration.max(0.0)));
                clock.paused = true;
                kill_audio(&mut audio);
                need_status = true;
            }
        }
        if msg
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(3))
        {
            msg = None;
            need_status = true;
        }

        if frame.is_some() || need_status {
            let mut out: Vec<u8> = Vec::new();
            out.extend_from_slice(b"\x1b[?2026h");
            if let Some(f) = frame {
                let raster = Raster {
                    w: pipe.fw,
                    h: pipe.fh,
                    ch: 3,
                    data: f,
                };
                let new_id = ids[1 - cur];
                let k = &ctx.kitty;
                if k.placeholders {
                    k.transmit(
                        &mut out,
                        &raster,
                        &format!("a=T,U=1,i={new_id},c={},r={}", lay.cols, lay.rows),
                    );
                    for r in 0..lay.rows {
                        out.extend_from_slice(
                            format!(
                                "\x1b[{};{}H{}",
                                lay.row + r + 1,
                                lay.col + 1,
                                placeholder_row(new_id, r, lay.cols)
                            )
                            .as_bytes(),
                        );
                    }
                } else {
                    out.extend_from_slice(
                        format!("\x1b[{};{}H", lay.row + 1, lay.col + 1).as_bytes(),
                    );
                    let size = if lay.scaled {
                        format!(",c={},r={}", lay.cols, lay.rows)
                    } else {
                        String::new()
                    };
                    k.transmit(
                        &mut out,
                        &raster,
                        &format!(
                            "a=T,i={new_id},p=1,X={},Y={},C=1{size},{}",
                            lay.xo,
                            lay.yo,
                            crate::kitty::Z_BELOW_BG
                        ),
                    );
                }
                if shown {
                    k.delete(&mut out, ids[cur]);
                }
                cur = 1 - cur;
                shown = true;
                if let Some(old) = last_frame.replace(raster.data) {
                    let _ = pipe.spare.send(old);
                }
            }
            let m = msg.as_ref().map(|(s, _)| s.clone());
            let (s, bc) = status(
                ctx,
                ws,
                name,
                pos,
                clock.now(),
                &info,
                clock.paused,
                muted,
                looping,
                &m,
            );
            bar_cols = bc;
            shown_second = clock.now() as i64;
            out.extend_from_slice(s.as_bytes());
            out.extend_from_slice(b"\x1b[?2026l");
            ctx.write(&out);
            need_status = false;
        }

        // ---- input
        let timeout = if let Some(t) = pending_restart {
            t.saturating_duration_since(Instant::now())
        } else if clock.paused || help {
            Duration::from_millis(500)
        } else if starved {
            // the next frame is due but not decoded yet: look again shortly
            Duration::from_millis(4)
        } else {
            let due = pipe.start + pipe.frames as f64 / pipe.fps;
            Duration::from_secs_f64((due - clock.now()).clamp(0.001, 0.25))
        };
        let evs = read_events(timeout)?;
        // The status line clock ticks every second.
        let mut seek_to: Option<f64> = None;
        let mut nav = None;
        let (cw, ch) = ws.cell();
        for ev in evs {
            if help {
                if matches!(ev, Event::Key(_)) {
                    help = false;
                    ctx.write(b"\x1b[0m\x1b[2J");
                    shown = false;
                    seek_to = Some(clock.now());
                }
                continue;
            }
            if is_quit(&ev) {
                nav = Some(Nav::Quit);
                break;
            }
            need_status = true;
            match ev {
                Event::Resize(..) => {
                    ws = ctx.size();
                    ctx.write(b"\x1b[0m\x1b[2J");
                    pending_restart = Some(Instant::now());
                }
                Event::Mouse(m) => {
                    let (aw, ah) = (
                        ws.cols as f64 * cw,
                        ws.rows.saturating_sub(CHROME_ROWS) as f64 * ch,
                    );
                    let fit = (aw / info.width as f64).min(ah / info.height as f64);
                    let z = fit * view.zoom;
                    let anchor = (
                        m.column as f64 * cw - aw / 2.0,
                        m.row as f64 * ch - ah / 2.0,
                    );
                    let zoom_to = |nz: f64, view: &mut View| {
                        let nz = nz.clamp(1.0, 32.0);
                        let px = view.cx + anchor.0 / z;
                        let py = view.cy + anchor.1 / z;
                        view.zoom = nz;
                        view.cx = px - anchor.0 / (fit * nz);
                        view.cy = py - anchor.1 / (fit * nz);
                    };
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            zoom_to(view.zoom * 1.25, &mut view);
                            pending_restart = Some(Instant::now() + Duration::from_millis(180));
                        }
                        MouseEventKind::ScrollDown => {
                            zoom_to(view.zoom / 1.25, &mut view);
                            pending_restart = Some(Instant::now() + Duration::from_millis(180));
                        }
                        MouseEventKind::Down(MouseButton::Left) if m.row + 2 == ws.rows => {
                            if info.duration > 0.0
                                && m.column >= bar_cols.0
                                && m.column < bar_cols.1
                            {
                                let f = (m.column - bar_cols.0) as f64
                                    / (bar_cols.1 - bar_cols.0).max(1) as f64;
                                seek_to = Some(f * info.duration);
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => drag = Some((m.column, m.row)),
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let (Some((x, y)), true) = (drag, view.zoom > 1.0) {
                                view.cx -= (m.column as f64 - x as f64) * cw / z;
                                view.cy -= (m.row as f64 - y as f64) * ch / z;
                                drag = Some((m.column, m.row));
                                pending_restart = Some(Instant::now() + Duration::from_millis(120));
                            }
                        }
                        MouseEventKind::Up(_) => drag = None,
                        _ => need_status = false,
                    }
                }
                Event::Key(k) => match k.code {
                    KeyCode::Char(' ') | KeyCode::Char('k') => {
                        if clock.paused {
                            if pipe.eof {
                                seek_to = Some(0.0);
                            } else {
                                clock.since = Instant::now();
                                clock.paused = false;
                                if !muted {
                                    audio = spawn_audio(path, clock.base);
                                }
                            }
                        } else {
                            clock.base = clock.now();
                            clock.paused = true;
                            kill_audio(&mut audio);
                        }
                    }
                    KeyCode::Right => seek_to = Some(clock.now() + 5.0),
                    KeyCode::Left => seek_to = Some(clock.now() - 5.0),
                    KeyCode::Up => seek_to = Some(clock.now() + 30.0),
                    KeyCode::Down => seek_to = Some(clock.now() - 30.0),
                    KeyCode::Home | KeyCode::Char('g') => seek_to = Some(0.0),
                    KeyCode::Char('m') => {
                        if !audio_ok {
                            msg = Some((
                                if info.has_audio {
                                    "audio needs ffplay (local only)".into()
                                } else {
                                    "no audio track".into()
                                },
                                Instant::now(),
                            ));
                        } else {
                            muted = !muted;
                            if muted {
                                kill_audio(&mut audio);
                            } else if !clock.paused {
                                audio = spawn_audio(path, clock.now());
                            }
                        }
                    }
                    KeyCode::Char('l') => looping = !looping,
                    KeyCode::Char('.') if clock.paused => step = true,
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        view.zoom = (view.zoom * 1.25).min(32.0);
                        pending_restart = Some(Instant::now() + Duration::from_millis(150));
                    }
                    KeyCode::Char('-') => {
                        view.zoom = (view.zoom / 1.25).max(1.0);
                        pending_restart = Some(Instant::now() + Duration::from_millis(150));
                    }
                    KeyCode::Char('0') => {
                        view = View {
                            zoom: 1.0,
                            cx: info.width as f64 / 2.0,
                            cy: info.height as f64 / 2.0,
                        };
                        pending_restart = Some(Instant::now());
                    }
                    KeyCode::Char('s') => {
                        let m = match &last_frame {
                            Some(f) => save_frame(name, clock.now(), pipe.fw, pipe.fh, f),
                            None => "no frame yet".into(),
                        };
                        msg = Some((m, Instant::now()));
                    }
                    KeyCode::Char('?') => {
                        help = true;
                        let mut out = Vec::new();
                        for id in ids {
                            ctx.kitty.delete(&mut out, id);
                        }
                        out.extend_from_slice(
                            help_screen(&ctx.theme, ws.cols, ws.rows, "shw — video player", HELP)
                                .as_bytes(),
                        );
                        ctx.write(&out);
                        clock.base = clock.now();
                        clock.paused = true;
                        kill_audio(&mut audio);
                    }
                    KeyCode::Char('n') | KeyCode::Tab => {
                        nav = Some(Nav::Next);
                        break;
                    }
                    KeyCode::Char('p') | KeyCode::BackTab => {
                        nav = Some(Nav::Prev);
                        break;
                    }
                    _ => need_status = false,
                },
                _ => need_status = false,
            }
        }
        if let Some(n) = nav {
            break n;
        }

        if pending_restart.is_some_and(|t| Instant::now() >= t) && seek_to.is_none() {
            pending_restart = None;
            seek_to = Some(clock.now());
            ctx.write(b"\x1b[0m\x1b[2J");
        }
        if let Some(t) = seek_to {
            let t = if info.duration > 0.0 {
                t.clamp(0.0, (info.duration - 0.05).max(0.0))
            } else {
                t.max(0.0)
            };
            lay = compute_layout(&info, view, ws, max_px);
            drop(pipe);
            pipe = spawn_pipeline(path, t, &info, &lay, max_fps)?;
            clock.set(t);
            kill_audio(&mut audio);
            if !clock.paused && !muted {
                audio = spawn_audio(path, t);
            }
            if clock.paused {
                step = true;
            }
            need_status = true;
        }
        // the time display ticks even when no frame arrives
        if !clock.paused && !help && clock.now() as i64 != shown_second {
            need_status = true;
        }
    };
    kill_audio(&mut audio);
    let mut out = Vec::new();
    for id in ids {
        ctx.kitty.delete(&mut out, id);
    }
    out.extend_from_slice(b"\x1b[0m\x1b[2J");
    ctx.write(&out);
    Ok(nav)
}

fn save_frame(name: &str, t: f64, w: u32, h: u32, data: &[u8]) -> String {
    let stem = std::path::Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "frame".into());
    let file = format!("{stem}-{:.2}s.png", t);
    match image::RgbImage::from_raw(w, h, data.to_vec()).map(|img| img.save(&file)) {
        Some(Ok(())) => format!("saved {file}"),
        _ => "failed to save frame".into(),
    }
}

/// Inline preview: a poster frame plus a one-line summary.
pub fn print_inline(ctx: &Ctx, path: &str, name: &str, ws: WinSize) -> Result<()> {
    let info = probe(path)?;
    let (cw, ch) = ws.cell();
    let rows_avail = ws.rows.saturating_sub(1).max(1);
    let (aw, ah) = (ws.cols as f64 * cw, rows_avail as f64 * ch);
    let z = (aw / info.width as f64)
        .min(ah / info.height as f64)
        .min(1.0);
    let (w, h) = (
        ((info.width as f64 * z) as u32).max(2) & !1,
        ((info.height as f64 * z) as u32).max(2) & !1,
    );
    let at = if info.duration > 0.0 {
        (info.duration * 0.1).min(30.0)
    } else {
        0.0
    };
    let raster = thumbnail(path, at, w, h)?;
    let mut doc = crate::doc::Doc::default();
    let cols = (w as f64 / cw).ceil() as u32;
    let rows = (h as f64 / ch).ceil() as u32;
    doc.image(raster.into(), 0, cols, rows, Vec::new(), String::new());
    let mut line = String::new();
    Style {
        fg: Some(ctx.theme.muted),
        ..Default::default()
    }
    .sgr(&mut line);
    line.push_str(&format!(
        "▶ {name}  {}×{}  {:.3} fps  {}  {}{}",
        info.width,
        info.height,
        info.fps,
        info.codec,
        fmt_duration(info.duration),
        if info.has_audio { "  ♪" } else { "" }
    ));
    line.push_str("\x1b[0m");
    doc.text(vec![crate::ui::Span::plain(String::new())]);
    crate::pager::print_doc(ctx, &mut doc, ws.cols as usize);
    println!("{line}");
    Ok(())
}
