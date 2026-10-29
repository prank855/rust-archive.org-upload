use std::path::Path;

pub const WINDOW: usize = 512;

#[derive(Debug)]
pub struct ImageType {
    pub mime: &'static str,
    pub ext: &'static str,
    magic: Magic,
}

#[derive(Debug)]
enum Magic {
    Prefix(&'static [u8]),
    RiffWebp,
    Svg,
}

pub const KINDS: &[ImageType] = &[
    ImageType {
        mime: "image/png",
        ext: "png",
        magic: Magic::Prefix(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
    },
    ImageType {
        mime: "image/jpeg",
        ext: "jpg",
        magic: Magic::Prefix(&[0xFF, 0xD8, 0xFF]),
    },
    ImageType {
        mime: "image/gif",
        ext: "gif",
        magic: Magic::Prefix(b"GIF8"),
    },
    ImageType {
        mime: "image/webp",
        ext: "webp",
        magic: Magic::RiffWebp,
    },
    ImageType {
        mime: "image/bmp",
        ext: "bmp",
        magic: Magic::Prefix(b"BM"),
    },
    ImageType {
        mime: "image/tiff",
        ext: "tiff",
        magic: Magic::Prefix(b"II*\0"),
    },
    ImageType {
        mime: "image/tiff",
        ext: "tiff",
        magic: Magic::Prefix(b"MM\0*"),
    },
    ImageType {
        mime: "image/svg+xml",
        ext: "svg",
        magic: Magic::Svg,
    },
];

impl ImageType {
    pub fn matches(&self, data: &[u8]) -> bool {
        match self.magic {
            Magic::Prefix(prefix) => data.starts_with(prefix),
            Magic::RiffWebp => {
                data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
            }
            Magic::Svg => {
                let text =
                    String::from_utf8_lossy(&data[..data.len().min(WINDOW)]).to_ascii_lowercase();
                let text = text.trim_start();
                text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
            }
        }
    }
}

pub fn detect(data: &[u8]) -> Option<&'static ImageType> {
    KINDS.iter().find(|kind| kind.matches(data))
}

pub fn from_extension(path: &Path) -> (&'static str, &'static str) {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        "jpg" | "jpeg" => ("image/jpeg", "jpg"),
        "png" => ("image/png", "png"),
        "gif" => ("image/gif", "gif"),
        "webp" => ("image/webp", "webp"),
        "bmp" => ("image/bmp", "bmp"),
        "tif" | "tiff" => ("image/tiff", "tiff"),
        "svg" => ("image/svg+xml", "svg"),
        _ => ("application/octet-stream", "bin"),
    }
}

pub fn looks_like_html(data: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&data[..data.len().min(WINDOW)]).to_ascii_lowercase();
    let text = text.trim_start();
    text.starts_with("<!doctype html") || text.starts_with("<html") || text.starts_with("<head")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];
    const GIF: &[u8] = b"GIF89a\0\0";
    const WEBP: &[u8] = b"RIFF\0\0\0\0WEBPVP8 ";

    #[test]
    fn detects_common_formats() {
        assert_eq!(detect(PNG).unwrap().mime, "image/png");
        assert_eq!(detect(JPEG).unwrap().mime, "image/jpeg");
        assert_eq!(detect(GIF).unwrap().mime, "image/gif");
        assert_eq!(detect(WEBP).unwrap().mime, "image/webp");
    }

    #[test]
    fn detects_svg_and_rejects_html() {
        assert_eq!(
            detect(b"<svg xmlns=\"...\">").unwrap().mime,
            "image/svg+xml"
        );
        assert!(!looks_like_html(b"<svg xmlns=\"...\">"));
        assert!(looks_like_html(b"\n  <!DOCTYPE html><html></html>"));
        assert!(detect(b"<!DOCTYPE html><html></html>").is_none());
    }

    #[test]
    fn svg_matches_inside_the_shared_window() {
        let mut data = b"<?xml version=\"1.0\"?>".to_vec();
        data.extend(vec![b' '; 200]);
        data.extend_from_slice(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>");

        let svg = KINDS
            .iter()
            .find(|kind| kind.mime == "image/svg+xml")
            .unwrap();

        assert!(!svg.matches(&data[..64]));
        assert!(svg.matches(&data[..data.len().min(WINDOW)]));
    }

    #[test]
    fn extension_fallback_is_case_insensitive() {
        assert_eq!(
            from_extension(Path::new("cover.JPG")),
            ("image/jpeg", "jpg")
        );
        assert_eq!(
            from_extension(Path::new("cover.unknown")),
            ("application/octet-stream", "bin")
        );
    }
}
