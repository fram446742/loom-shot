//! Stitch multiple output captures into one image.
//!
//! Layouts are deliberately simple and documented:
//!
//! * `horizontal` — left to right, top-aligned, canvas height is the tallest capture.
//! * `vertical` — top to bottom, left-aligned, canvas width is the widest capture.
//!
//! The canvas is opaque black where captures do not cover it. Images keep their
//! original pixel size (physical pixels, after scale/transform).

use image::{Rgba, RgbaImage, imageops};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StitchLayout {
    Horizontal,
    Vertical,
}

impl StitchLayout {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "horizontal" | "h" | "row" => Some(Self::Horizontal),
            "vertical" | "v" | "column" => Some(Self::Vertical),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

/// Composes captures in the requested layout.
pub fn compose(images: &[RgbaImage], layout: StitchLayout) -> Result<RgbaImage, String> {
    if images.is_empty() {
        return Err("cannot stitch an empty capture list".to_owned());
    }
    let (width, height) = match layout {
        StitchLayout::Horizontal => (
            images.iter().try_fold(0u32, |total, image| {
                total
                    .checked_add(image.width())
                    .ok_or_else(|| "stitched width overflows".to_owned())
            })?,
            images.iter().map(RgbaImage::height).max().unwrap_or(0),
        ),
        StitchLayout::Vertical => (
            images.iter().map(RgbaImage::width).max().unwrap_or(0),
            images.iter().try_fold(0u32, |total, image| {
                total
                    .checked_add(image.height())
                    .ok_or_else(|| "stitched height overflows".to_owned())
            })?,
        ),
    };
    if width == 0 || height == 0 {
        return Err("cannot stitch zero-sized captures".to_owned());
    }
    let mut canvas = RgbaImage::from_pixel(width, height, Rgba([0, 0, 0, 255]));
    let mut offset = 0u32;
    for image in images {
        let (x, y) = match layout {
            StitchLayout::Horizontal => (offset, 0),
            StitchLayout::Vertical => (0, offset),
        };
        imageops::overlay(&mut canvas, image, i64::from(x), i64::from(y));
        offset += match layout {
            StitchLayout::Horizontal => image.width(),
            StitchLayout::Vertical => image.height(),
        };
    }
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, value: u8) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([value, value, value, 255]))
    }

    #[test]
    fn parses_layout_names() {
        assert_eq!(
            StitchLayout::parse("horizontal"),
            Some(StitchLayout::Horizontal)
        );
        assert_eq!(StitchLayout::parse("v"), Some(StitchLayout::Vertical));
        assert_eq!(StitchLayout::parse("grid"), None);
        assert_eq!(StitchLayout::Vertical.name(), "vertical");
    }

    #[test]
    fn composes_horizontal_with_top_alignment() {
        let images = [solid(3, 2, 10), solid(2, 4, 20)];
        let stitched = compose(&images, StitchLayout::Horizontal).unwrap();
        assert_eq!((stitched.width(), stitched.height()), (5, 4));
        assert_eq!(*stitched.get_pixel(0, 0), Rgba([10, 10, 10, 255]));
        assert_eq!(*stitched.get_pixel(2, 1), Rgba([10, 10, 10, 255]));
        assert_eq!(*stitched.get_pixel(3, 0), Rgba([20, 20, 20, 255]));
        // Below the shorter left capture the canvas is black.
        assert_eq!(*stitched.get_pixel(1, 3), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn composes_vertical_with_left_alignment() {
        let images = [solid(4, 1, 30), solid(2, 3, 40)];
        let stitched = compose(&images, StitchLayout::Vertical).unwrap();
        assert_eq!((stitched.width(), stitched.height()), (4, 4));
        assert_eq!(*stitched.get_pixel(3, 0), Rgba([30, 30, 30, 255]));
        assert_eq!(*stitched.get_pixel(1, 2), Rgba([40, 40, 40, 255]));
        assert_eq!(*stitched.get_pixel(3, 2), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(compose(&[], StitchLayout::Horizontal).is_err());
    }
}
