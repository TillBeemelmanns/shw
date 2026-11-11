//! Image decoding (raster + SVG + ffmpeg fallback) and high quality resampling.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use fast_image_resize as fr;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};

/// A packed 8-bit RGB or RGBA image (straight alpha).
#[derive(Clone, Debug)]
pub struct Raster {
    pub w: u32,
    pub h: u32,
    pub ch: u8,
    pub data: Vec<u8>,
}

impl Raster {
    pub fn from_dynamic(img: DynamicImage) -> Raster {
        let (w, h) = (img.width(), img.height());
        if img.color().has_alpha() {
            let mut rgba = img.into_rgba8();
            // Fully opaque images are sent as RGB: 25% less data.
            if rgba.pixels().all(|p| p.0[3] == 255) {
                let rgb = DynamicImage::ImageRgba8(std::mem::take(&mut rgba)).into_rgb8();
                return Raster {
                    w,
                    h,
                    ch: 3,
                    data: rgb.into_raw(),
                };
            }
            Raster {
                w,
                h,
                ch: 4,
                data: rgba.into_raw(),
            }
        } else {
            Raster {
                w,
                h,
                ch: 3,
                data: img.into_rgb8().into_raw(),
            }
        }
    }

    pub fn to_rgba(&self) -> Raster {
        if self.ch == 4 {
            return self.clone();
        }
        let mut data = Vec::with_capacity((self.w * self.h * 4) as usize);
        for p in self.data.as_chunks::<3>().0 {
            data.extend_from_slice(&[p[0], p[1], p[2], 255]);
        }
        Raster {
            w: self.w,
            h: self.h,
            ch: 4,
            data,
        }
    }

    pub fn rotate90(&self) -> Raster {
        let (w, h, ch) = (self.w as usize, self.h as usize, self.ch as usize);
        let mut data = vec![0u8; self.data.len()];
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * ch;
                // (x, y) -> (h - 1 - y, x) in a h-wide image
                let dst = (x * h + (h - 1 - y)) * ch;
                data[dst..dst + ch].copy_from_slice(&self.data[src..src + ch]);
            }
        }
        Raster {
            w: self.h,
            h: self.w,
            ch: self.ch,
            data,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    Bilinear,
    Lanczos,
}

/// Resample the source region `crop = (x, y, w, h)` (sub-pixel precise) into a
/// `dw` x `dh` raster.
pub fn resize_region(
    src: &Raster,
    crop: (f64, f64, f64, f64),
    dw: u32,
    dh: u32,
    filter: Filter,
) -> Raster {
    let dw = dw.max(1);
    let dh = dh.max(1);
    let pt = if src.ch == 4 {
        fr::PixelType::U8x4
    } else {
        fr::PixelType::U8x3
    };
    let Ok(src_ref) = fr::images::ImageRef::new(src.w, src.h, &src.data, pt) else {
        return Raster {
            w: dw,
            h: dh,
            ch: src.ch,
            data: vec![0; (dw * dh * src.ch as u32) as usize],
        };
    };
    let mut dst = fr::images::Image::new(dw, dh, pt);
    let alg = match filter {
        Filter::Nearest => fr::ResizeAlg::Nearest,
        Filter::Bilinear => fr::ResizeAlg::Convolution(fr::FilterType::Bilinear),
        Filter::Lanczos => fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3),
    };
    let (cx, cy, cw, chh) = crop;
    let opts = fr::ResizeOptions::new().resize_alg(alg).crop(
        cx.max(0.0),
        cy.max(0.0),
        cw.max(1e-3),
        chh.max(1e-3),
    );
    let mut resizer = fr::Resizer::new();
    if resizer.resize(&src_ref, &mut dst, &opts).is_err() {
        return Raster {
            w: dw,
            h: dh,
            ch: src.ch,
            data: vec![0; (dw * dh * src.ch as u32) as usize],
        };
    }
    Raster {
        w: dw,
        h: dh,
        ch: src.ch,
        data: dst.into_vec(),
    }
}

/// Resize the whole raster.
pub fn resize(src: &Raster, dw: u32, dh: u32) -> Raster {
    if src.w == dw && src.h == dh {
        return src.clone();
    }
    resize_region(
        src,
        (0.0, 0.0, src.w as f64, src.h as f64),
        dw,
        dh,
        Filter::Lanczos,
    )
}

pub struct Frame {
    pub raster: Raster,
    pub delay: Duration,
}

/// A decoded picture: raster frames (one for stills) or a vector document.
pub struct Picture {
    pub frames: Vec<Frame>,
    pub svg: Option<Arc<resvg::usvg::Tree>>,
    pub width: u32,
    pub height: u32,
    pub format: String,
}

impl Picture {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Render the picture (first frame) at exactly `w` x `h`.
    pub fn render(&self, w: u32, h: u32) -> Raster {
        if let Some(tree) = &self.svg {
            return render_svg(
                tree,
                (0.0, 0.0, self.width as f64, self.height as f64),
                w,
                h,
            );
        }
        resize(&self.frames[0].raster, w, h)
    }
}

fn fontdb() -> Arc<resvg::usvg::fontdb::Database> {
    use std::sync::OnceLock;
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

pub fn parse_svg(data: &[u8], resources_dir: Option<&Path>) -> Result<resvg::usvg::Tree> {
    let opt = resvg::usvg::Options {
        resources_dir: resources_dir.map(|p| p.to_path_buf()),
        fontdb: fontdb(),
        ..Default::default()
    };
    resvg::usvg::Tree::from_data(data, &opt).map_err(|e| anyhow!("invalid SVG: {e}"))
}

/// Render the region `crop` of an SVG document into a `dw` x `dh` RGBA raster.
pub fn render_svg(
    tree: &resvg::usvg::Tree,
    crop: (f64, f64, f64, f64),
    dw: u32,
    dh: u32,
) -> Raster {
    let dw = dw.max(1);
    let dh = dh.max(1);
    let Some(mut pixmap) = resvg::tiny_skia::Pixmap::new(dw, dh) else {
        return Raster {
            w: dw,
            h: dh,
            ch: 4,
            data: vec![0; (dw * dh * 4) as usize],
        };
    };
    let sx = dw as f64 / crop.2.max(1e-6);
    let sy = dh as f64 / crop.3.max(1e-6);
    let ts = resvg::tiny_skia::Transform::from_row(
        sx as f32,
        0.0,
        0.0,
        sy as f32,
        (-crop.0 * sx) as f32,
        (-crop.1 * sy) as f32,
    );
    resvg::render(tree, ts, &mut pixmap.as_mut());
    let mut data = pixmap.take();
    // tiny-skia is premultiplied; kitty wants straight alpha.
    for p in data.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            p[0] = ((p[0] as u32 * 255 + a / 2) / a).min(255) as u8;
            p[1] = ((p[1] as u32 * 255 + a / 2) / a).min(255) as u8;
            p[2] = ((p[2] as u32 * 255 + a / 2) / a).min(255) as u8;
        }
    }
    Raster {
        w: dw,
        h: dh,
        ch: 4,
        data,
    }
}

fn looks_like_svg(data: &[u8]) -> bool {
    let head = &data[..data.len().min(1024)];
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    s.contains("<svg") || (s.trim_start().starts_with("<?xml") && s.contains("svg"))
}

/// Decode an image from memory. `hint` is a file name or URL used for format guessing.
pub fn decode(data: Vec<u8>, hint: &str, resources_dir: Option<&Path>) -> Result<Picture> {
    let lower = hint.to_ascii_lowercase();
    let is_svg_ext = lower.ends_with(".svg") || lower.ends_with(".svgz");
    if is_svg_ext || looks_like_svg(&data) {
        let tree = parse_svg(&data, resources_dir)?;
        let size = tree.size();
        return Ok(Picture {
            frames: Vec::new(),
            width: size.width().ceil().max(1.0) as u32,
            height: size.height().ceil().max(1.0) as u32,
            svg: Some(Arc::new(tree)),
            format: "SVG".into(),
        });
    }

    let guessed = image::guess_format(&data)
        .ok()
        .or_else(|| ImageFormat::from_path(hint).ok());
    match guessed {
        Some(fmt) => {
            decode_raster(data, fmt).or_else(|e| decode_with_ffmpeg_bytes(hint).map_err(|_| e))
        }
        None => decode_with_ffmpeg_bytes(hint),
    }
}

fn decode_raster(data: Vec<u8>, fmt: ImageFormat) -> Result<Picture> {
    let format = format!("{fmt:?}").to_uppercase();

    // Animated formats
    let frames: Option<Vec<image::Frame>> = match fmt {
        ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(&data))
            .ok()
            .and_then(|d| d.into_frames().collect_frames().ok()),
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(&data))
            .ok()
            .filter(|d| d.has_animation())
            .and_then(|d| d.into_frames().collect_frames().ok()),
        ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(&data))
            .ok()
            .filter(|d| d.is_apng().unwrap_or(false))
            .and_then(|d| d.apng().ok())
            .and_then(|d| d.into_frames().collect_frames().ok()),
        _ => None,
    };
    if let Some(frames) = frames.filter(|f| f.len() > 1) {
        let (w, h) = frames[0].buffer().dimensions();
        let frames = frames
            .into_iter()
            .map(|f| {
                let (n, d) = f.delay().numer_denom_ms();
                let ms = n.checked_div(d).map(|v| v.max(20)).unwrap_or(100);
                Frame {
                    delay: Duration::from_millis(ms as u64),
                    raster: Raster::from_dynamic(DynamicImage::ImageRgba8(f.into_buffer())),
                }
            })
            .collect();
        return Ok(Picture {
            frames,
            svg: None,
            width: w,
            height: h,
            format,
        });
    }

    let reader = ImageReader::with_format(Cursor::new(&data), fmt);
    let mut reader = reader;
    reader.no_limits();
    let mut decoder = reader.into_decoder().context("unsupported image")?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).context("failed to decode image")?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let raster = Raster::from_dynamic(img);
    Ok(Picture {
        width: raster.w,
        height: raster.h,
        frames: vec![Frame {
            raster,
            delay: Duration::ZERO,
        }],
        svg: None,
        format,
    })
}

/// Fallback for formats the image crate cannot decode (HEIC, AVIF, JXL, RAW, ...):
/// let ffmpeg convert the first frame to PNG.
fn decode_with_ffmpeg_bytes(path: &str) -> Result<Picture> {
    if path.is_empty() || path == "-" {
        return Err(anyhow!("unsupported image format"));
    }
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-i",
            path,
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| {
            anyhow!("unsupported image format (install ffmpeg for HEIC/AVIF/JXL support)")
        })?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(anyhow!("unsupported image format"));
    }
    let ext = Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_else(|| "IMAGE".into());
    let mut pic = decode_raster(out.stdout, ImageFormat::Png)?;
    pic.format = ext;
    Ok(pic)
}

/// Horizontal composition of images (badges in a row), aligned at the bottom.
pub fn compose_row(items: &[Raster], gap: u32) -> Raster {
    let h = items.iter().map(|r| r.h).max().unwrap_or(1).max(1);
    let w = items.iter().map(|r| r.w).sum::<u32>() + gap * items.len().saturating_sub(1) as u32;
    let w = w.max(1);
    let mut data = vec![0u8; (w * h * 4) as usize];
    let mut x0 = 0u32;
    for it in items {
        let it = it.to_rgba();
        let y0 = (h - it.h) / 2;
        for y in 0..it.h {
            let src = &it.data[(y * it.w * 4) as usize..((y + 1) * it.w * 4) as usize];
            let off = (((y0 + y) * w + x0) * 4) as usize;
            data[off..off + src.len()].copy_from_slice(src);
        }
        x0 += it.w + gap;
    }
    Raster { w, h, ch: 4, data }
}

// ---------------------------------------------------------------------------
// Typeset text (large headings)

/// GitHub's font stack, restricted to families fontdb can resolve.
const HEADING_FONTS: &str = "'Helvetica Neue', Helvetica, 'Segoe UI', 'Noto Sans', 'Liberation Sans', 'DejaVu Sans', Arial, sans-serif";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn text_svg(text: &str, size: f64, w: f64, h: f64, baseline: f64, color: (u8, u8, u8)) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}"><text x="0" y="{baseline:.2}" font-family="{HEADING_FONTS}" font-weight="600" font-size="{size:.2}" fill="#{:02x}{:02x}{:02x}">{}</text></svg>"##,
        color.0,
        color.1,
        color.2,
        xml_escape(text)
    )
}

/// Rendered width of `text` at `size` px (semibold), or None if no font is available.
pub fn text_width(text: &str, size: f64) -> Option<f64> {
    let svg = text_svg(text, size, 20000.0, size * 2.0, size * 1.5, (0, 0, 0));
    let tree = parse_svg(svg.as_bytes(), None).ok()?;
    let bb = tree.root().bounding_box()?;
    (bb.width() > 0.0).then(|| bb.right() as f64)
}

/// Render one line of text into an RGBA raster exactly `h` px tall,
/// vertically centred on the cap height.
pub fn render_text_line(text: &str, size: f64, h: u32, color: (u8, u8, u8)) -> Option<Raster> {
    let w = text_width(text, size)?.ceil() + 2.0;
    let baseline = h as f64 / 2.0 + size * 0.36;
    let svg = text_svg(text, size, w, h as f64, baseline, color);
    let tree = parse_svg(svg.as_bytes(), None).ok()?;
    Some(render_svg(&tree, (0.0, 0.0, w, h as f64), w as u32, h))
}

/// GitHub-style task list checkbox drawn into a `w` x `h` (2 x 1 cells) raster.
pub fn checkbox(
    checked: bool,
    w: u32,
    h: u32,
    accent: (u8, u8, u8),
    border: (u8, u8, u8),
) -> Raster {
    let body = h as f64 / 1.22;
    let size = (body * 0.82).round().max(6.0);
    let x = (w as f64 * 0.08).round();
    let y = ((h as f64 - size) / 2.0).round();
    let r = (size * 0.2).max(1.5);
    let stroke = (size / 11.0).max(1.0);
    let hex = |c: (u8, u8, u8)| format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2);
    let shape = if checked {
        // tick inside a filled box
        let (x0, y0, s) = (x, y, size);
        format!(
            r#"<rect x="{x0}" y="{y0}" width="{s}" height="{s}" rx="{r}" fill="{a}"/><path d="M {} {} L {} {} L {} {}" fill="none" stroke="white" stroke-width="{}" stroke-linecap="round" stroke-linejoin="round"/>"#,
            x0 + s * 0.24,
            y0 + s * 0.52,
            x0 + s * 0.43,
            y0 + s * 0.70,
            x0 + s * 0.77,
            y0 + s * 0.32,
            s * 0.13,
            a = hex(accent)
        )
    } else {
        let half = stroke / 2.0;
        format!(
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{r}" fill="none" stroke="{}" stroke-width="{stroke}"/>"#,
            x + half,
            y + half,
            size - stroke,
            size - stroke,
            hex(border)
        )
    };
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}">{shape}</svg>"#
    );
    match parse_svg(svg.as_bytes(), None) {
        Ok(tree) => render_svg(&tree, (0.0, 0.0, w as f64, h as f64), w, h),
        Err(_) => Raster {
            w,
            h,
            ch: 4,
            data: vec![0; (w * h * 4) as usize],
        },
    }
}

/// Load the system font database in the background so the first heading is fast.
pub fn warm_fonts() {
    std::thread::spawn(|| {
        let _ = fontdb();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_and_rotate() {
        let r = Raster {
            w: 4,
            h: 2,
            ch: 3,
            data: (0..24).map(|v| v as u8).collect(),
        };
        let rot = r.rotate90();
        assert_eq!((rot.w, rot.h), (2, 4));
        let s = resize(&r, 8, 4);
        assert_eq!((s.w, s.h, s.data.len()), (8, 4, 96));
    }

    #[test]
    fn decodes_svg() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><rect width="20" height="10" fill="red"/></svg>"#;
        let pic = decode(svg.to_vec(), "x.svg", None).unwrap();
        assert_eq!((pic.width, pic.height), (20, 10));
        let r = pic.render(40, 20);
        assert_eq!(&r.data[0..4], &[255, 0, 0, 255]);
    }
}
