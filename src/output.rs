//! Output formats for captured screenshots.
//!
//! PNG is the native format niri writes. We additionally support JPEG and
//! WebP through the `image` crate; JPEG is lossy (quality configurable) and
//! WebP is encoded losslessly so annotations stay crisp.

use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbaImage};

/// Formats loom-shot can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Png,
    Jpeg,
    WebP,
}

impl OutputFormat {
    /// Parses a user-facing format name.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::WebP),
            _ => None,
        }
    }

    /// Infers a format from a path extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::parse)
    }

    /// File extension used for generated paths.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }

    /// Stable name used in JSON output and metadata.
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::WebP => "webp",
        }
    }

    /// MIME type used for the clipboard.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }

    /// Encodes an RGBA image into this format.
    ///
    /// `quality` is only used by JPEG. WebP is always encoded losslessly.
    pub fn encode(self, image: &RgbaImage, quality: u8) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        let width = image.width();
        let height = image.height();
        let result = match self {
            Self::Png => PngEncoder::new(&mut bytes).write_image(
                image.as_raw(),
                width,
                height,
                ExtendedColorType::Rgba8,
            ),
            Self::Jpeg => {
                // JPEG has no alpha channel; screenshots are opaque in practice.
                let rgb = DynamicImage::ImageRgba8(image.clone()).into_rgb8();
                JpegEncoder::new_with_quality(&mut bytes, quality.clamp(1, 100)).encode_image(&rgb)
            }
            Self::WebP => WebPEncoder::new_lossless(&mut bytes).encode(
                image.as_raw(),
                width,
                height,
                ExtendedColorType::Rgba8,
            ),
        };
        result.map_err(|err| format!("cannot encode {} image: {err}", self.name()))?;
        Ok(bytes)
    }
}

/// Decodes an image from disk, guessing the format from content.
pub fn load(path: &Path) -> Result<DynamicImage, String> {
    image::ImageReader::open(path)
        .map_err(|err| format!("cannot open capture {}: {err}", path.display()))?
        .with_guessed_format()
        .map_err(|err| format!("cannot inspect capture {}: {err}", path.display()))?
        .decode()
        .map_err(|err| format!("cannot decode capture {}: {err}", path.display()))
}

/// Whether a decoded image carries more than 8 bits per channel, which is our
/// signal that it may be HDR-encoded (niri's own captures are always 8-bit
/// sRGB after tone mapping).
pub fn is_hdr_source(image: &DynamicImage) -> bool {
    use image::ColorType;
    matches!(
        image.color(),
        ColorType::L16
            | ColorType::La16
            | ColorType::Rgb16
            | ColorType::Rgba16
            | ColorType::Rgb32F
            | ColorType::Rgba32F
    )
}

/// Human-readable colour type for metadata.
pub fn color_name(image: &DynamicImage) -> &'static str {
    use image::ColorType;
    match image.color() {
        ColorType::L8 => "gray8",
        ColorType::La8 => "gray-alpha8",
        ColorType::Rgb8 => "rgb8",
        ColorType::Rgba8 => "rgba8",
        ColorType::L16 => "gray16",
        ColorType::La16 => "gray-alpha16",
        ColorType::Rgb16 => "rgb16",
        ColorType::Rgba16 => "rgba16",
        ColorType::Rgb32F => "rgb32f",
        ColorType::Rgba32F => "rgba32f",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    fn sample() -> RgbaImage {
        RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255]))
    }

    #[test]
    fn parses_format_names_and_extensions() {
        assert_eq!(OutputFormat::parse("PNG"), Some(OutputFormat::Png));
        assert_eq!(OutputFormat::parse("jpg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("jpeg"), Some(OutputFormat::Jpeg));
        assert_eq!(OutputFormat::parse("webp"), Some(OutputFormat::WebP));
        assert_eq!(OutputFormat::parse("bmp"), None);
        assert_eq!(
            OutputFormat::from_path(Path::new("/tmp/a.webp")),
            Some(OutputFormat::WebP)
        );
        assert_eq!(OutputFormat::from_path(Path::new("/tmp/a.txt")), None);
    }

    #[test]
    fn encodes_each_format_with_expected_magic() {
        let image = sample();
        let png = OutputFormat::Png.encode(&image, 90).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

        let jpeg = OutputFormat::Jpeg.encode(&image, 90).unwrap();
        assert_eq!(&jpeg[..2], b"\xff\xd8");

        let webp = OutputFormat::WebP.encode(&image, 90).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
    }

    #[test]
    fn detects_high_bit_depth_sources() {
        let eight = DynamicImage::ImageRgba8(sample());
        assert!(!is_hdr_source(&eight));
        assert_eq!(color_name(&eight), "rgba8");

        let sixteen = DynamicImage::ImageRgba16(
            image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_pixel(
                1,
                1,
                image::Rgba([0, 0, 0, 65535]),
            ),
        );
        assert!(is_hdr_source(&sixteen));
        assert_eq!(color_name(&sixteen), "rgba16");
    }
}
