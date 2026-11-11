//! File type detection by extension and magic bytes.

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Image,
    Video,
    Markdown,
    Code,
    Binary,
}

const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "webp", "bmp", "tif", "tiff", "ico", "pnm", "ppm",
    "pgm", "pbm", "pam", "qoi", "tga", "dds", "hdr", "svg", "svgz", "heic", "heif", "avif", "jxl",
    "exr", "cr2", "nef", "arw", "dng",
];
const VIDEO: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "m2ts", "mts", "ts",
    "3gp", "ogv", "vob", "gifv", "y4m",
];
const MARKDOWN: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn", "mdx", "mdwn"];

fn ext_of(name: &str) -> String {
    let clean = name.split(['?', '#']).next().unwrap_or(name);
    std::path::Path::new(clean)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn by_extension(name: &str) -> Option<Kind> {
    let ext = ext_of(name);
    if ext.is_empty() {
        let base = std::path::Path::new(name)
            .file_name()
            .map(|f| f.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if base == "readme" || base == "changelog" {
            return Some(Kind::Markdown);
        }
        return None;
    }
    let e = ext.as_str();
    if IMAGE.contains(&e) {
        Some(Kind::Image)
    } else if VIDEO.contains(&e) {
        Some(Kind::Video)
    } else if MARKDOWN.contains(&e) {
        Some(Kind::Markdown)
    } else {
        None
    }
}

pub fn by_content(head: &[u8]) -> Kind {
    if image::guess_format(head).is_ok() {
        return Kind::Image;
    }
    let s = String::from_utf8_lossy(&head[..head.len().min(512)]).to_ascii_lowercase();
    if s.contains("<svg") {
        return Kind::Image;
    }
    if head.len() >= 12 {
        if &head[4..8] == b"ftyp" {
            let brand = &head[8..12];
            if brand == b"heic" || brand == b"heix" || brand == b"mif1" || brand == b"avif" {
                return Kind::Image;
            }
            return Kind::Video;
        }
        if head[..4] == [0x1a, 0x45, 0xdf, 0xa3]
            || (&head[..4] == b"RIFF" && &head[8..12] == b"AVI ")
            || head[..4] == [0, 0, 1, 0xba]
        {
            return Kind::Video;
        }
    }
    if head.contains(&0) {
        return Kind::Binary;
    }
    Kind::Code
}

pub fn detect(name: &str, head: &[u8]) -> Kind {
    if let Some(k) = by_extension(name) {
        return k;
    }
    by_content(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(detect("a/b.PNG", b""), Kind::Image);
        assert_eq!(detect("clip.mkv", b""), Kind::Video);
        assert_eq!(detect("README.md", b""), Kind::Markdown);
        assert_eq!(detect("main.rs", b"fn main() {}"), Kind::Code);
        assert_eq!(detect("https://x.org/i.jpg?s=2", b""), Kind::Image);
        assert_eq!(
            detect("blob", &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Kind::Image
        );
        assert_eq!(detect("blob", &[0, 1, 2, 3, 0]), Kind::Binary);
        assert_eq!(detect("x", b"\0\0\0\x18ftypisom\0\0"), Kind::Video);
    }
}
