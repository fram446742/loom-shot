//! Non-interactive annotations drawn onto a captured image.
//!
//! Specs are pixel-based and repeatable. Draw operations:
//! `rect`, `text`, `arrow`, `line`, `ellipse`, `highlight`, `blur`, `pixelate`,
//! `solid-redaction`, `spotlight`, `zoom`. `crop` changes the canvas and must be
//! applied with [`Annotation::apply`].
//!
//! Everything is implemented with integer pixel work on an RGBA canvas, so
//! annotations need no fonts, GUI toolkit, display, or font server. Text uses a
//! built-in 5x7 bitmap font scaled 3x. `satty` remains available separately.

use std::f64::consts::TAU;

use image::{Rgba, RgbaImage, imageops};

const RED: Rgba<u8> = Rgba([255, 59, 48, 255]);
const SHADOW: Rgba<u8> = Rgba([0, 0, 0, 255]);
const HIGHLIGHT: Rgba<u8> = Rgba([255, 235, 59, 110]);
const STROKE: i32 = 3;
const TEXT_SCALE: u32 = 3;
const GLYPH_ADVANCE: i32 = (5 * TEXT_SCALE + 2) as i32;
const DEFAULT_BLUR_RADIUS: u32 = 8;
const MAX_BLUR_RADIUS: u32 = 64;
const DEFAULT_PIXELATE_BLOCK: u32 = 12;
const MAX_PIXELATE_BLOCK: u32 = 256;
const DEFAULT_SPOTLIGHT_DIM: u8 = 60;
const DEFAULT_ZOOM_SCALE: u32 = 2;
const MAX_ZOOM_SCALE: u32 = 8;
const ZOOM_MARGIN: u32 = 8;

/// Human-readable list used by `loom-shot annotate --list-ops`.
pub const OPS_HELP: &str = r#"Supported annotation operations (coordinates are pixels in the
image they are applied to; operations run in the order given):

  rect:x,y,w,h                    3 px red outline
  text:x,y,STRING                 built-in 5x7 font, 3x scale, red with shadow
  arrow:x1,y1,x2,y2               3 px shaft with a spread head
  line:x1,y1,x2,y2                3 px red line without a head
  ellipse:cx,cy,rx,ry             3 px red outline
  highlight:x,y,w,h               translucent yellow marker fill
  blur:x,y,w,h[,RADIUS]           box blur inside the rect (radius 1-64, default 8)
  pixelate:x,y,w,h[,BLOCK]        average-colour blocks (block 1-256, default 12)
  solid-redaction:x,y,w,h[,RRGGBB]
                                  opaque filled rectangle (default black #000000)
  spotlight:x,y,w,h[,DIM]         dim everything outside the rect (1-100%, default 60)
  zoom:x,y,w,h[,CORNER[,SCALE]]   magnify a region into a corner
                                  CORNER: top-left|top-right|bottom-left|bottom-right
                                  (default top-right), SCALE 2-8 (default 2)
  crop:x,y,w,h                    crop the canvas to the rect (changes dimensions)

Aliases: `redact` for `solid-redaction`. Every draw operation validates that
its geometry fits the current image; `text` may be clipped at the right/bottom.
"#;

/// Corner used by `zoom`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "top-left" | "topleft" | "tl" => Some(Self::TopLeft),
            "top-right" | "topright" | "tr" => Some(Self::TopRight),
            "bottom-left" | "bottomleft" | "bl" => Some(Self::BottomLeft),
            "bottom-right" | "bottomright" | "br" => Some(Self::BottomRight),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
        }
    }
}

/// A single annotation parsed from `--annotate=SPEC`, `--op`, or JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    Rect {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    },
    Text {
        x: u32,
        y: u32,
        text: String,
    },
    Arrow {
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
    },
    Line {
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
    },
    Ellipse {
        cx: u32,
        cy: u32,
        rx: u32,
        ry: u32,
    },
    Highlight {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    },
    Blur {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        radius: u32,
    },
    Pixelate {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        block: u32,
    },
    SolidRedaction {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        color: [u8; 3],
    },
    Crop {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    },
    Zoom {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        corner: Corner,
        scale: u32,
    },
    Spotlight {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        dim: u8,
    },
}

impl Annotation {
    /// Parses a `kind:...` spec.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (kind, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("annotation `{spec}` needs a `kind:...` prefix"))?;
        match kind {
            "rect" => {
                let [x, y, w, h] = exact_rect(rest, spec)?;
                Ok(Self::Rect { x, y, w, h })
            }
            "highlight" => {
                let [x, y, w, h] = exact_rect(rest, spec)?;
                Ok(Self::Highlight { x, y, w, h })
            }
            "crop" => {
                let [x, y, w, h] = exact_rect(rest, spec)?;
                Ok(Self::Crop { x, y, w, h })
            }
            "arrow" | "line" => {
                let [x1, y1, x2, y2] = parse_numbers::<4>(rest, spec)?;
                if x1 == x2 && y1 == y2 {
                    return Err(format!("annotation `{spec}` has zero length"));
                }
                if kind == "arrow" {
                    Ok(Self::Arrow { x1, y1, x2, y2 })
                } else {
                    Ok(Self::Line { x1, y1, x2, y2 })
                }
            }
            "ellipse" => {
                let [cx, cy, rx, ry] = parse_numbers::<4>(rest, spec)?;
                if rx == 0 || ry == 0 {
                    return Err(format!(
                        "annotation `{spec}` needs a non-zero ellipse radius"
                    ));
                }
                Ok(Self::Ellipse { cx, cy, rx, ry })
            }
            "blur" => {
                let fields = fields(rest);
                require_count(&fields, 4, 5, spec)?;
                let (x, y, w, h) = rect_fields(&fields, spec)?;
                let radius = match fields.get(4) {
                    Some(raw) => parse_u32(raw, spec, "radius")?,
                    None => DEFAULT_BLUR_RADIUS,
                };
                if !(1..=MAX_BLUR_RADIUS).contains(&radius) {
                    return Err(format!(
                        "annotation `{spec}` needs a blur radius 1-{MAX_BLUR_RADIUS}"
                    ));
                }
                Ok(Self::Blur { x, y, w, h, radius })
            }
            "pixelate" => {
                let fields = fields(rest);
                require_count(&fields, 4, 5, spec)?;
                let (x, y, w, h) = rect_fields(&fields, spec)?;
                let block = match fields.get(4) {
                    Some(raw) => parse_u32(raw, spec, "block")?,
                    None => DEFAULT_PIXELATE_BLOCK,
                };
                if !(1..=MAX_PIXELATE_BLOCK).contains(&block) {
                    return Err(format!(
                        "annotation `{spec}` needs a block size 1-{MAX_PIXELATE_BLOCK}"
                    ));
                }
                Ok(Self::Pixelate { x, y, w, h, block })
            }
            "solid-redaction" | "redact" => {
                let fields = fields(rest);
                require_count(&fields, 4, 5, spec)?;
                let (x, y, w, h) = rect_fields(&fields, spec)?;
                let color = match fields.get(4) {
                    Some(raw) => parse_color(raw, spec)?,
                    None => [0, 0, 0],
                };
                Ok(Self::SolidRedaction { x, y, w, h, color })
            }
            "spotlight" => {
                let fields = fields(rest);
                require_count(&fields, 4, 5, spec)?;
                let (x, y, w, h) = rect_fields(&fields, spec)?;
                let dim = match fields.get(4) {
                    Some(raw) => {
                        let dim = parse_u32(raw, spec, "dim")?;
                        u8::try_from(dim)
                            .ok()
                            .filter(|dim| (1..=100).contains(dim))
                            .ok_or_else(|| {
                                format!("annotation `{spec}` needs a dim percentage 1-100")
                            })?
                    }
                    None => DEFAULT_SPOTLIGHT_DIM,
                };
                Ok(Self::Spotlight { x, y, w, h, dim })
            }
            "zoom" => {
                let fields = fields(rest);
                require_count(&fields, 4, 6, spec)?;
                let (x, y, w, h) = rect_fields(&fields, spec)?;
                let mut corner = Corner::TopRight;
                let mut scale = None;
                if let Some(extra) = fields.get(4) {
                    if let Some(parsed) = Corner::parse(extra) {
                        corner = parsed;
                    } else {
                        scale = Some(parse_u32(extra, spec, "scale")?);
                    }
                }
                if let Some(extra) = fields.get(5) {
                    if scale.is_some() {
                        return Err(format!("annotation `{spec}` repeats the zoom scale"));
                    }
                    scale = Some(parse_u32(extra, spec, "scale")?);
                }
                let scale = scale.unwrap_or(DEFAULT_ZOOM_SCALE);
                if !(2..=MAX_ZOOM_SCALE).contains(&scale) {
                    return Err(format!(
                        "annotation `{spec}` needs a zoom scale 2-{MAX_ZOOM_SCALE}"
                    ));
                }
                Ok(Self::Zoom {
                    x,
                    y,
                    w,
                    h,
                    corner,
                    scale,
                })
            }
            "text" => {
                let mut parts = rest.splitn(3, ',');
                let x = parse_number(parts.next(), spec)?;
                let y = parse_number(parts.next(), spec)?;
                let text = parts
                    .next()
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| format!("annotation `{spec}` needs text after x,y"))?;
                Ok(Self::Text {
                    x,
                    y,
                    text: text.to_owned(),
                })
            }
            other => Err(format!(
                "unknown annotation kind `{other}` (expected one of: rect, text, arrow, line, \
                 ellipse, highlight, blur, pixelate, solid-redaction, spotlight, zoom, crop)"
            )),
        }
    }

    /// Canonical form used in metadata, logs, and JSON history.
    pub fn describe(&self) -> String {
        match self {
            Self::Rect { x, y, w, h } => format!("rect:{x},{y},{w},{h}"),
            Self::Text { x, y, text } => format!("text:{x},{y},{text}"),
            Self::Arrow { x1, y1, x2, y2 } => format!("arrow:{x1},{y1},{x2},{y2}"),
            Self::Line { x1, y1, x2, y2 } => format!("line:{x1},{y1},{x2},{y2}"),
            Self::Ellipse { cx, cy, rx, ry } => format!("ellipse:{cx},{cy},{rx},{ry}"),
            Self::Highlight { x, y, w, h } => format!("highlight:{x},{y},{w},{h}"),
            Self::Blur { x, y, w, h, radius } => format!("blur:{x},{y},{w},{h},{radius}"),
            Self::Pixelate { x, y, w, h, block } => format!("pixelate:{x},{y},{w},{h},{block}"),
            Self::SolidRedaction { x, y, w, h, color } => format!(
                "solid-redaction:{x},{y},{w},{h},#{:02x}{:02x}{:02x}",
                color[0], color[1], color[2]
            ),
            Self::Crop { x, y, w, h } => format!("crop:{x},{y},{w},{h}"),
            Self::Zoom {
                x,
                y,
                w,
                h,
                corner,
                scale,
            } => format!("zoom:{x},{y},{w},{h},{},{}", corner.name(), scale),
            Self::Spotlight { x, y, w, h, dim } => format!("spotlight:{x},{y},{w},{h},{dim}"),
        }
    }

    /// Applies the annotation, returning the (possibly resized) canvas.
    ///
    /// `crop` replaces the canvas. All other operations draw in place.
    pub fn apply(&self, image: RgbaImage) -> Result<RgbaImage, String> {
        if let Self::Crop { x, y, w, h } = self {
            validate_rect("crop", *x, *y, *w, *h, image.width(), image.height())?;
            return Ok(imageops::crop_imm(&image, *x, *y, *w, *h).to_image());
        }
        let mut image = image;
        self.draw(&mut image)?;
        Ok(image)
    }

    /// Draws the annotation, validating it against the image size.
    pub fn draw(&self, image: &mut RgbaImage) -> Result<(), String> {
        let (width, height) = (image.width(), image.height());
        match self {
            Self::Crop { .. } => Err("crop changes the image dimensions; use apply".to_owned()),
            Self::Rect { x, y, w, h } => {
                validate_rect("rectangle", *x, *y, *w, *h, width, height)?;
                draw_rect_outline(image, *x as i32, *y as i32, *w as i32, *h as i32);
                Ok(())
            }
            Self::Highlight { x, y, w, h } => {
                validate_rect("highlight", *x, *y, *w, *h, width, height)?;
                fill_rect(image, *x, *y, *w, *h, HIGHLIGHT);
                Ok(())
            }
            Self::Arrow { x1, y1, x2, y2 } => {
                validate_points(*x1, *y1, *x2, *y2, width, height, "arrow")?;
                draw_arrow(image, *x1 as i32, *y1 as i32, *x2 as i32, *y2 as i32);
                Ok(())
            }
            Self::Line { x1, y1, x2, y2 } => {
                validate_points(*x1, *y1, *x2, *y2, width, height, "line")?;
                draw_thick_line(image, *x1 as i32, *y1 as i32, *x2 as i32, *y2 as i32);
                Ok(())
            }
            Self::Ellipse { cx, cy, rx, ry } => {
                validate_ellipse(*cx, *cy, *rx, *ry, width, height)?;
                draw_ellipse(image, *cx as i32, *cy as i32, *rx as i32, *ry as i32);
                Ok(())
            }
            Self::Text { x, y, text } => {
                if *x >= width || *y >= height {
                    return Err(format!(
                        "text origin {x},{y} lies outside the {width}x{height} capture"
                    ));
                }
                draw_text(image, *x as i32, *y as i32, text);
                Ok(())
            }
            Self::Blur { x, y, w, h, radius } => {
                validate_rect("blur", *x, *y, *w, *h, width, height)?;
                blur_region(image, *x, *y, *w, *h, *radius);
                Ok(())
            }
            Self::Pixelate { x, y, w, h, block } => {
                validate_rect("pixelate", *x, *y, *w, *h, width, height)?;
                pixelate_region(image, *x, *y, *w, *h, *block);
                Ok(())
            }
            Self::SolidRedaction { x, y, w, h, color } => {
                validate_rect("solid redaction", *x, *y, *w, *h, width, height)?;
                fill_rect(
                    image,
                    *x,
                    *y,
                    *w,
                    *h,
                    Rgba([color[0], color[1], color[2], 255]),
                );
                Ok(())
            }
            Self::Spotlight { x, y, w, h, dim } => {
                validate_rect("spotlight", *x, *y, *w, *h, width, height)?;
                spotlight(image, *x, *y, *w, *h, *dim);
                Ok(())
            }
            Self::Zoom {
                x,
                y,
                w,
                h,
                corner,
                scale,
            } => {
                validate_rect("zoom region", *x, *y, *w, *h, width, height)?;
                draw_zoom(image, *x, *y, *w, *h, *corner, *scale)
            }
        }
    }
}

fn exact_rect(rest: &str, spec: &str) -> Result<[u32; 4], String> {
    let [x, y, w, h] = parse_numbers::<4>(rest, spec)?;
    if w == 0 || h == 0 {
        return Err(format!(
            "annotation `{spec}` needs a non-zero width and height"
        ));
    }
    Ok([x, y, w, h])
}

fn fields(rest: &str) -> Vec<&str> {
    rest.split(',').map(str::trim).collect()
}

fn require_count(fields: &[&str], min: usize, max: usize, spec: &str) -> Result<(), String> {
    if fields.len() < min || fields.len() > max {
        return Err(format!(
            "annotation `{spec}` needs {min} to {max} comma-separated values"
        ));
    }
    Ok(())
}

fn rect_fields(fields: &[&str], spec: &str) -> Result<(u32, u32, u32, u32), String> {
    let x = parse_u32(fields[0], spec, "x")?;
    let y = parse_u32(fields[1], spec, "y")?;
    let w = parse_u32(fields[2], spec, "width")?;
    let h = parse_u32(fields[3], spec, "height")?;
    if w == 0 || h == 0 {
        return Err(format!(
            "annotation `{spec}` needs a non-zero width and height"
        ));
    }
    Ok((x, y, w, h))
}

fn parse_u32(value: &str, spec: &str, what: &str) -> Result<u32, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("annotation `{spec}` has a non-numeric {what} `{value}`"))
}

fn parse_color(value: &str, spec: &str) -> Result<[u8; 3], String> {
    match value.to_ascii_lowercase().as_str() {
        "black" => return Ok([0, 0, 0]),
        "white" => return Ok([255, 255, 255]),
        "red" => return Ok([255, 0, 0]),
        _ => {}
    }
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "annotation `{spec}` needs a colour as RRGGBB or #RRGGBB"
        ));
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16)
            .map_err(|_| format!("annotation `{spec}` has an invalid colour `{value}`"))
    };
    Ok([channel(0..2)?, channel(2..4)?, channel(4..6)?])
}

fn validate_rect(
    what: &str,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if w == 0 || h == 0 {
        return Err(format!("{what} has a zero width or height"));
    }
    if x as u64 + w as u64 > width as u64 || y as u64 + h as u64 > height as u64 {
        return Err(format!(
            "{what} {x},{y},{w},{h} does not fit in the {width}x{height} capture"
        ));
    }
    Ok(())
}

fn validate_points(
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
    width: u32,
    height: u32,
    what: &str,
) -> Result<(), String> {
    if x1 >= width || y1 >= height || x2 >= width || y2 >= height {
        return Err(format!(
            "{what} {x1},{y1} -> {x2},{y2} lies outside the {width}x{height} capture"
        ));
    }
    Ok(())
}

fn validate_ellipse(
    cx: u32,
    cy: u32,
    rx: u32,
    ry: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    if rx == 0 || ry == 0 {
        return Err("ellipse has a zero radius".to_owned());
    }
    if cx < rx || cy < ry || cx + rx >= width || cy + ry >= height {
        return Err(format!(
            "ellipse {cx},{cy},{rx},{ry} lies outside the {width}x{height} capture"
        ));
    }
    Ok(())
}

fn parse_numbers<const N: usize>(rest: &str, spec: &str) -> Result<[u32; N], String> {
    let parts: Vec<&str> = rest.split(',').collect();
    if parts.len() != N {
        return Err(format!(
            "annotation `{spec}` needs {N} comma-separated numbers"
        ));
    }
    let mut values = [0u32; N];
    for (slot, part) in values.iter_mut().zip(parts) {
        *slot = part
            .trim()
            .parse()
            .map_err(|_| format!("annotation `{spec}` has a non-numeric value `{part}`"))?;
    }
    Ok(values)
}

fn parse_number(value: Option<&str>, spec: &str) -> Result<u32, String> {
    value
        .ok_or_else(|| format!("annotation `{spec}` is missing coordinates"))?
        .trim()
        .parse()
        .map_err(|_| format!("annotation `{spec}` has a non-numeric coordinate"))
}

fn put(image: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
        image.put_pixel(x as u32, y as u32, color);
    }
}

fn fill_block(image: &mut RgbaImage, x: i32, y: i32, size: i32, color: Rgba<u8>) {
    for dy in 0..size {
        for dx in 0..size {
            put(image, x + dx, y + dy, color);
        }
    }
}

fn fill_rect(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    if color[3] == 255 {
        for py in y..y + h {
            for px in x..x + w {
                image.put_pixel(px, py, color);
            }
        }
        return;
    }
    for py in y..y + h {
        for px in x..x + w {
            let base = *image.get_pixel(px, py);
            image.put_pixel(px, py, blend(base, color));
        }
    }
}

fn blend(base: Rgba<u8>, over: Rgba<u8>) -> Rgba<u8> {
    let alpha = u32::from(over[3]);
    let inverse = 255 - alpha;
    let channel = |index: usize| {
        ((u32::from(over[index]) * alpha + u32::from(base[index]) * inverse + 127) / 255) as u8
    };
    Rgba([channel(0), channel(1), channel(2), 255])
}

fn draw_rect_outline(image: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32) {
    let thickness = STROKE.min(w).min(h);
    for px in x..x + w {
        for offset in 0..thickness {
            put(image, px, y + offset, RED);
            put(image, px, y + h - 1 - offset, RED);
        }
    }
    for py in y..y + h {
        for offset in 0..thickness {
            put(image, x + offset, py, RED);
            put(image, x + w - 1 - offset, py, RED);
        }
    }
}

fn draw_arrow(image: &mut RgbaImage, x1: i32, y1: i32, x2: i32, y2: i32) {
    draw_thick_line(image, x1, y1, x2, y2);

    let dx = f64::from(x2 - x1);
    let dy = f64::from(y2 - y1);
    let length = dx.hypot(dy).max(1.0);
    let (ux, uy) = (dx / length, dy / length);
    let head_length = 14.0_f64.min(length * 0.6).max(6.0);
    let (bx, by) = (
        f64::from(x2) - ux * head_length,
        f64::from(y2) - uy * head_length,
    );
    // Perpendicular offset for a ~30 degree head.
    let spread = head_length * 0.55;
    let (px, py) = (-uy * spread, ux * spread);
    let h1 = ((bx + px).round() as i32, (by + py).round() as i32);
    let h2 = ((bx - px).round() as i32, (by - py).round() as i32);
    draw_thick_line(image, x2, y2, h1.0, h1.1);
    draw_thick_line(image, x2, y2, h2.0, h2.1);
}

fn draw_thick_line(image: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32) {
    let radius = STROKE / 2;
    let (mut x, mut y) = (x0, y0);
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    loop {
        fill_block(image, x - radius, y - radius, STROKE, RED);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

fn draw_ellipse(image: &mut RgbaImage, cx: i32, cy: i32, rx: i32, ry: i32) {
    let steps = ((rx + ry) as f64 * 2.0).max(64.0) as i32;
    let mut previous = None;
    for step in 0..=steps {
        let angle = TAU * f64::from(step) / f64::from(steps);
        let point = (
            cx + (f64::from(rx) * angle.cos()).round() as i32,
            cy + (f64::from(ry) * angle.sin()).round() as i32,
        );
        if let Some((px, py)) = previous {
            draw_thick_line(image, px, py, point.0, point.1);
        }
        previous = Some(point);
    }
}

fn blur_region(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, radius: u32) {
    let region = imageops::crop_imm(image, x, y, w, h).to_image();
    let blurred = box_blur(&region, radius);
    imageops::overlay(image, &blurred, i64::from(x), i64::from(y));
}

fn box_blur(source: &RgbaImage, radius: u32) -> RgbaImage {
    let (width, height) = (source.width(), source.height());
    let radius = i64::from(radius);
    let mut horizontal = RgbaImage::new(width, height);
    for y in 0..height {
        let mut sums = [0u64; 4];
        let mut count = 0u64;
        for offset in -radius..=radius {
            let x = clamp_index(offset, width);
            add_pixel(&mut sums, *source.get_pixel(x, y));
            count += 1;
        }
        for x in 0..width {
            horizontal.put_pixel(x, y, average(&sums, count));
            let leaving = clamp_index(i64::from(x) - radius, width);
            let entering = clamp_index(i64::from(x) + radius + 1, width);
            subtract_pixel(&mut sums, *source.get_pixel(leaving, y));
            add_pixel(&mut sums, *source.get_pixel(entering, y));
        }
    }

    let mut output = RgbaImage::new(width, height);
    for x in 0..width {
        let mut sums = [0u64; 4];
        let mut count = 0u64;
        for offset in -radius..=radius {
            let y = clamp_index(offset, height);
            add_pixel(&mut sums, *horizontal.get_pixel(x, y));
            count += 1;
        }
        for y in 0..height {
            output.put_pixel(x, y, average(&sums, count));
            let leaving = clamp_index(i64::from(y) - radius, height);
            let entering = clamp_index(i64::from(y) + radius + 1, height);
            subtract_pixel(&mut sums, *horizontal.get_pixel(x, leaving));
            add_pixel(&mut sums, *horizontal.get_pixel(x, entering));
        }
    }
    output
}

fn clamp_index(value: i64, limit: u32) -> u32 {
    value.clamp(0, i64::from(limit) - 1) as u32
}

fn add_pixel(sums: &mut [u64; 4], pixel: Rgba<u8>) {
    for channel in 0..4 {
        sums[channel] += u64::from(pixel[channel]);
    }
}

fn subtract_pixel(sums: &mut [u64; 4], pixel: Rgba<u8>) {
    for channel in 0..4 {
        sums[channel] -= u64::from(pixel[channel]);
    }
}

fn average(sums: &[u64; 4], count: u64) -> Rgba<u8> {
    let channel = |index: usize| ((sums[index] + count / 2) / count) as u8;
    Rgba([channel(0), channel(1), channel(2), channel(3)])
}

fn pixelate_region(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, block: u32) {
    let region = imageops::crop_imm(image, x, y, w, h).to_image();
    let mut block_y = 0;
    while block_y < h {
        let block_height = block.min(h - block_y);
        let mut block_x = 0;
        while block_x < w {
            let block_width = block.min(w - block_x);
            let mut sums = [0u64; 4];
            let count = u64::from(block_width) * u64::from(block_height);
            for py in block_y..block_y + block_height {
                for px in block_x..block_x + block_width {
                    add_pixel(&mut sums, *region.get_pixel(px, py));
                }
            }
            let color = average(&sums, count);
            for py in block_y..block_y + block_height {
                for px in block_x..block_x + block_width {
                    image.put_pixel(x + px, y + py, color);
                }
            }
            block_x += block_width;
        }
        block_y += block_height;
    }
}

fn spotlight(image: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, dim: u8) {
    let factor = 100 - u32::from(dim);
    for py in 0..image.height() {
        for px in 0..image.width() {
            if px >= x && px < x + w && py >= y && py < y + h {
                continue;
            }
            let pixel = *image.get_pixel(px, py);
            let channel = |index: usize| ((u32::from(pixel[index]) * factor + 50) / 100) as u8;
            image.put_pixel(px, py, Rgba([channel(0), channel(1), channel(2), pixel[3]]));
        }
    }
}

fn draw_zoom(
    image: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    corner: Corner,
    scale: u32,
) -> Result<(), String> {
    let zoomed_width = w
        .checked_mul(scale)
        .ok_or_else(|| "zoom dimensions overflow".to_owned())?;
    let zoomed_height = h
        .checked_mul(scale)
        .ok_or_else(|| "zoom dimensions overflow".to_owned())?;
    let (width, height) = (image.width(), image.height());
    if zoomed_width + 2 * ZOOM_MARGIN > width || zoomed_height + 2 * ZOOM_MARGIN > height {
        return Err(format!(
            "zoom region {w}x{h} at scale {scale} becomes {zoomed_width}x{zoomed_height}, which \
             does not fit in the {width}x{height} capture"
        ));
    }

    // Capture the source before drawing its outline so the magnified copy stays clean.
    let source = imageops::crop_imm(image, x, y, w, h).to_image();
    let magnified = imageops::resize(
        &source,
        zoomed_width,
        zoomed_height,
        imageops::FilterType::Nearest,
    );
    draw_rect_outline(image, x as i32, y as i32, w as i32, h as i32);

    let max_x = width - zoomed_width - ZOOM_MARGIN;
    let max_y = height - zoomed_height - ZOOM_MARGIN;
    let (dx, dy) = match corner {
        Corner::TopLeft => (ZOOM_MARGIN, ZOOM_MARGIN),
        Corner::TopRight => (max_x, ZOOM_MARGIN),
        Corner::BottomLeft => (ZOOM_MARGIN, max_y),
        Corner::BottomRight => (max_x, max_y),
    };
    imageops::overlay(image, &magnified, i64::from(dx), i64::from(dy));
    draw_rect_outline(
        image,
        dx as i32,
        dy as i32,
        zoomed_width as i32,
        zoomed_height as i32,
    );
    Ok(())
}

fn draw_text(image: &mut RgbaImage, x: i32, y: i32, text: &str) {
    let mut cursor = x;
    for ch in text.chars() {
        let glyph = glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) == 0 {
                    continue;
                }
                let px = cursor + column * TEXT_SCALE as i32;
                let py = y + row as i32 * TEXT_SCALE as i32;
                // One-pixel drop shadow keeps red text readable on light UI.
                fill_block(image, px + 1, py + 1, TEXT_SCALE as i32, SHADOW);
                fill_block(image, px, py, TEXT_SCALE as i32, RED);
            }
        }
        cursor += GLYPH_ADVANCE;
    }
}

/// 5x7 glyph, one byte per row, bit 4 = leftmost pixel.
#[allow(clippy::match_same_arms)]
fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01111, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b01111,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01111, 0b10000, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '!' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
        ],
        '?' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        '_' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100,
        ],
        ',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b01000,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        ';' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b01000, 0b00000,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        '+' => [
            0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
        ],
        '=' => [
            0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000,
        ],
        '#' => [
            0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
        ],
        '%' => [
            0b11001, 0b11010, 0b00010, 0b00100, 0b01000, 0b01011, 0b10011,
        ],
        '\'' => [
            0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        '"' => [
            0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        '(' => [
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => [
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        '<' => [
            0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010,
        ],
        '>' => [
            0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000,
        ],
        '*' => [
            0b00000, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0b00000,
        ],
        _ => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> RgbaImage {
        RgbaImage::from_pixel(64, 48, Rgba([255, 255, 255, 255]))
    }

    fn is_red(pixel: Rgba<u8>) -> bool {
        pixel[0] > 200 && pixel[1] < 100 && pixel[2] < 100
    }

    #[test]
    fn parses_all_specs() {
        assert_eq!(
            Annotation::parse("rect:1,2,30,40").unwrap(),
            Annotation::Rect {
                x: 1,
                y: 2,
                w: 30,
                h: 40
            }
        );
        assert_eq!(
            Annotation::parse("arrow:1,2,30,40").unwrap(),
            Annotation::Arrow {
                x1: 1,
                y1: 2,
                x2: 30,
                y2: 40
            }
        );
        assert_eq!(
            Annotation::parse("text:3,4,hello, world").unwrap(),
            Annotation::Text {
                x: 3,
                y: 4,
                text: "hello, world".to_owned()
            }
        );
        assert_eq!(
            Annotation::parse("text:0,0,x").unwrap().describe(),
            "text:0,0,x"
        );
        assert_eq!(
            Annotation::parse("line:1,2,30,40").unwrap().describe(),
            "line:1,2,30,40"
        );
        assert_eq!(
            Annotation::parse("ellipse:20,20,10,5").unwrap().describe(),
            "ellipse:20,20,10,5"
        );
        assert_eq!(
            Annotation::parse("highlight:1,2,3,4").unwrap().describe(),
            "highlight:1,2,3,4"
        );
        assert_eq!(
            Annotation::parse("blur:1,2,3,4").unwrap().describe(),
            "blur:1,2,3,4,8"
        );
        assert_eq!(
            Annotation::parse("pixelate:1,2,3,4,20").unwrap().describe(),
            "pixelate:1,2,3,4,20"
        );
        assert_eq!(
            Annotation::parse("redact:1,2,3,4,#ff0000")
                .unwrap()
                .describe(),
            "solid-redaction:1,2,3,4,#ff0000"
        );
        assert_eq!(
            Annotation::parse("solid-redaction:1,2,3,4")
                .unwrap()
                .describe(),
            "solid-redaction:1,2,3,4,#000000"
        );
        assert_eq!(
            Annotation::parse("crop:1,2,30,40").unwrap().describe(),
            "crop:1,2,30,40"
        );
        assert_eq!(
            Annotation::parse("zoom:1,2,30,40").unwrap().describe(),
            "zoom:1,2,30,40,top-right,2"
        );
        assert_eq!(
            Annotation::parse("zoom:1,2,30,40,bottom-left,3")
                .unwrap()
                .describe(),
            "zoom:1,2,30,40,bottom-left,3"
        );
        assert_eq!(
            Annotation::parse("zoom:1,2,30,40,3").unwrap().describe(),
            "zoom:1,2,30,40,top-right,3"
        );
        assert_eq!(
            Annotation::parse("spotlight:1,2,3,4").unwrap().describe(),
            "spotlight:1,2,3,4,60"
        );
    }

    #[test]
    fn rejects_bad_specs() {
        for spec in [
            "rect:1,2,3",
            "rect:1,2,0,4",
            "rect:-1,2,3,4",
            "arrow:1,1,1,1",
            "arrow:1,2,3",
            "line:1,1,1,1",
            "ellipse:1,2,0,4",
            "text:1,2",
            "text:1,2,",
            "blur:1,2,3,4,0",
            "blur:1,2,3,4,99",
            "blur:1,2,3,4,8,9",
            "pixelate:1,2,3,4,0",
            "pixelate:1,2,3,4,300",
            "solid-redaction:1,2,3,4,zzzzzz",
            "spotlight:1,2,3,4,0",
            "spotlight:1,2,3,4,101",
            "zoom:1,2,3,4,1",
            "zoom:1,2,3,4,nope,2",
            "zoom:1,2,3,4,top-right,2,3",
            "crop:1,2,0,4",
            "circle:1,2,3,4",
            "nonsense",
        ] {
            assert!(Annotation::parse(spec).is_err(), "accepted {spec}");
        }
    }

    #[test]
    fn draws_rectangle_outline_only() {
        let mut image = canvas();
        Annotation::parse("rect:5,6,20,15")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert!(is_red(*image.get_pixel(5, 6)));
        assert!(is_red(*image.get_pixel(24, 20)));
        assert!(is_red(*image.get_pixel(5, 20)));
        assert!(is_red(*image.get_pixel(24, 6)));
        assert_eq!(*image.get_pixel(15, 13), Rgba([255, 255, 255, 255]));
        assert_eq!(*image.get_pixel(4, 6), Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn rejects_out_of_bounds_geometry() {
        let mut image = canvas();
        let err = Annotation::parse("rect:60,40,10,10")
            .unwrap()
            .draw(&mut image)
            .unwrap_err();
        assert!(err.contains("does not fit"));
        let err = Annotation::parse("arrow:1,1,100,1")
            .unwrap()
            .draw(&mut image)
            .unwrap_err();
        assert!(err.contains("outside"));
        let err = Annotation::parse("text:100,1,x")
            .unwrap()
            .draw(&mut image)
            .unwrap_err();
        assert!(err.contains("outside"));
    }

    #[test]
    fn draws_arrow_shaft_and_head() {
        let mut image = canvas();
        Annotation::parse("arrow:5,20,40,20")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert!(is_red(*image.get_pixel(5, 20)));
        assert!(is_red(*image.get_pixel(20, 20)));
        assert!(is_red(*image.get_pixel(40, 20)));
        // The head spreads away from the 3 px shaft (y = 19..=21).
        let head_pixels = (0..48)
            .flat_map(|y| (25..=40).map(move |x| (x, y)))
            .filter(|&(x, y)| !(19..=21).contains(&y) && is_red(*image.get_pixel(x, y)))
            .count();
        assert!(head_pixels > 10, "arrow head too small: {head_pixels}");
    }

    #[test]
    fn draws_text_with_builtin_font() {
        let mut image = canvas();
        Annotation::parse("text:2,2,Hi 42!")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        let red_pixels = image.pixels().filter(|pixel| is_red(**pixel)).count();
        assert!(red_pixels > 40, "text drew only {red_pixels} pixels");
        // Unknown characters fall back to `?` instead of panicking.
        let mut fallback = canvas();
        Annotation::parse("text:2,2,§")
            .unwrap()
            .draw(&mut fallback)
            .unwrap();
        assert!(fallback.pixels().any(|pixel| is_red(*pixel)));
    }

    #[test]
    fn crops_the_canvas() {
        let mut image = RgbaImage::new(10, 8);
        for y in 0..8 {
            for x in 0..10 {
                image.put_pixel(x, y, Rgba([x as u8, y as u8, 0, 255]));
            }
        }
        let cropped = Annotation::parse("crop:2,3,4,5")
            .unwrap()
            .apply(image)
            .unwrap();
        assert_eq!((cropped.width(), cropped.height()), (4, 5));
        assert_eq!(*cropped.get_pixel(0, 0), Rgba([2, 3, 0, 255]));
        assert_eq!(*cropped.get_pixel(3, 4), Rgba([5, 7, 0, 255]));

        let err = Annotation::parse("crop:8,0,4,4")
            .unwrap()
            .apply(cropped)
            .unwrap_err();
        assert!(err.contains("does not fit"), "{err}");
    }

    #[test]
    fn blur_softens_inside_and_preserves_outside() {
        let mut image = RgbaImage::new(40, 40);
        for y in 0..40 {
            for x in 0..40 {
                let value = if (x / 2 + y / 2) % 2 == 0 { 0 } else { 255 };
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        let before = *image.get_pixel(10, 10);
        Annotation::parse("blur:5,5,30,30,6")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        let after = *image.get_pixel(10, 10);
        assert_ne!(before, after, "blur did not change the region");
        assert!(!(after[0] == 0 || after[0] == 255), "blur was not blended");
        assert_eq!(*image.get_pixel(1, 1), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn pixelate_averages_blocks() {
        let mut image = RgbaImage::new(20, 20);
        for y in 0..20 {
            for x in 0..20 {
                let value = (x * 10) as u8;
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        Annotation::parse("pixelate:0,0,20,20,5")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        // Each 5 px block takes the average of its source values.
        assert_eq!(*image.get_pixel(0, 0), Rgba([20, 20, 20, 255]));
        assert_eq!(*image.get_pixel(5, 0), Rgba([70, 70, 70, 255]));
        assert_eq!(*image.get_pixel(15, 0), Rgba([170, 170, 170, 255]));
    }

    #[test]
    fn redaction_is_opaque_and_coloured() {
        let mut image = RgbaImage::from_pixel(10, 10, Rgba([10, 20, 30, 255]));
        Annotation::parse("solid-redaction:2,2,3,3,#ff0000")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert_eq!(*image.get_pixel(2, 2), Rgba([255, 0, 0, 255]));
        assert_eq!(*image.get_pixel(4, 4), Rgba([255, 0, 0, 255]));
        assert_eq!(*image.get_pixel(5, 5), Rgba([10, 20, 30, 255]));
    }

    #[test]
    fn spotlight_dims_outside_only() {
        let mut image = RgbaImage::from_pixel(20, 20, Rgba([200, 200, 200, 255]));
        Annotation::parse("spotlight:5,5,10,10,50")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert_eq!(*image.get_pixel(10, 10), Rgba([200, 200, 200, 255]));
        assert_eq!(*image.get_pixel(0, 0), Rgba([100, 100, 100, 255]));
    }

    #[test]
    fn highlight_is_translucent() {
        let mut image = RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 255]));
        Annotation::parse("highlight:2,2,4,4")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        let pixel = *image.get_pixel(3, 3);
        assert!(pixel[0] > 80 && pixel[1] > 70 && pixel[2] < 60);
    }

    #[test]
    fn line_and_ellipse_draw_outlines() {
        let mut image = canvas();
        Annotation::parse("line:5,5,40,5")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert!(is_red(*image.get_pixel(20, 5)));
        assert!(!is_red(*image.get_pixel(20, 15)));

        let mut image = canvas();
        Annotation::parse("ellipse:32,24,20,10")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        assert!(is_red(*image.get_pixel(12, 24)));
        assert!(is_red(*image.get_pixel(52, 24)));
        assert!(!is_red(*image.get_pixel(32, 24)));
    }

    #[test]
    fn zoom_copies_region_into_a_corner() {
        let mut image = RgbaImage::new(80, 60);
        for y in 0..60 {
            for x in 0..80 {
                let value = if (10..20).contains(&x) && (10..20).contains(&y) {
                    255
                } else {
                    0
                };
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        Annotation::parse("zoom:10,10,10,10,bottom-right,2")
            .unwrap()
            .draw(&mut image)
            .unwrap();
        // Source outlined and not covered by the bottom-right destination.
        assert!(is_red(*image.get_pixel(10, 10)));
        // Magnified destination starts at (52, 32) and contains the white block.
        assert!(is_red(*image.get_pixel(52, 32)));
        assert_eq!(image.get_pixel(60, 40)[0], 255);
        assert_eq!(image.get_pixel(5, 5)[0], 0);
    }

    #[test]
    fn zoom_rejects_regions_that_do_not_fit() {
        let mut image = RgbaImage::from_pixel(40, 30, Rgba([0, 0, 0, 255]));
        let err = Annotation::parse("zoom:0,0,30,20,top-right,2")
            .unwrap()
            .draw(&mut image)
            .unwrap_err();
        assert!(err.contains("does not fit"), "{err}");
    }

    #[test]
    fn apply_chains_operations_deterministically() {
        let base = RgbaImage::from_pixel(30, 30, Rgba([9, 9, 9, 255]));
        let ops = [
            Annotation::parse("solid-redaction:0,0,10,10").unwrap(),
            Annotation::parse("crop:0,0,20,20").unwrap(),
            Annotation::parse("rect:0,0,20,20").unwrap(),
        ];
        let render = |ops: &[Annotation]| {
            let mut image = base.clone();
            for op in ops {
                image = op.apply(image).unwrap();
            }
            image
        };
        let first = render(&ops);
        let second = render(&ops);
        assert_eq!(first, second);
        assert_eq!((first.width(), first.height()), (20, 20));
        assert_eq!(*first.get_pixel(5, 5), Rgba([0, 0, 0, 255]));
        assert!(is_red(*first.get_pixel(0, 0)));
    }
}
