//! HDR detection and PQ -> SDR tone mapping.
//!
//! niri renders screenshots through its `ScreenCapture` target and explicitly
//! converts the HDR blend space back to SDR while capturing
//! (`set_sdr_capture_blend`). Every 8-bit PNG written by niri is therefore
//! already tone-mapped; running another PQ -> sRGB pass would double-map it.
//! `--hdr=tonemap` is only accepted for genuine high-bit-depth captures (which
//! current niri does not produce) and rejected with an explanation otherwise.
//!
//! HDR *state* is detected from the focused output reported by niri IPC
//! (`max_bpc`) plus the output's `hdr` block in niri's config. IPC itself has
//! no HDR flag, so an output configured with `hdr mode="auto"` only says HDR
//! *may* engage; niri does not expose whether it is currently engaged.

use std::path::PathBuf;

use image::{DynamicImage, Rgba, RgbaImage};
use serde_json::{Value, json};

use crate::output;

/// BT.2408 reference white used by niri when no explicit value is configured.
pub const DEFAULT_REFERENCE_LUMINANCE: f64 = 203.0;

/// HDR handling requested on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HdrRequest {
    /// Leave niri's output untouched (already SDR).
    #[default]
    Keep,
    /// Require SDR output; reject high-bit-depth captures.
    Sdr,
    /// Tone-map a high-bit-depth PQ capture to SDR.
    Tonemap,
}

impl HdrRequest {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "keep" => Some(Self::Keep),
            "sdr" => Some(Self::Sdr),
            "tonemap" | "tone-map" | "tonemap-sdr" => Some(Self::Tonemap),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Sdr => "sdr",
            Self::Tonemap => "tonemap",
        }
    }
}

/// `hdr mode` from niri's output config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfiguredHdrMode {
    /// HDR only while an HDR fullscreen surface is shown.
    #[default]
    Auto,
    /// HDR is always signalled on the output.
    On,
}

impl ConfiguredHdrMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
        }
    }
}

/// The `hdr` block of an output in niri's config.
#[derive(Debug, Clone, PartialEq)]
pub struct HdrConfig {
    pub mode: ConfiguredHdrMode,
    pub reference_luminance: Option<f64>,
}

/// Best-effort HDR state for the output a capture targets.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HdrDetection {
    pub output: Option<String>,
    pub max_bpc: Option<u8>,
    pub config: Option<HdrConfig>,
    pub config_path: Option<PathBuf>,
    pub config_error: Option<String>,
}

impl HdrDetection {
    /// Detects HDR configuration for the named output.
    pub fn for_output(output: Option<String>, max_bpc: Option<u8>) -> Self {
        let mut detection = Self {
            output: output.clone(),
            max_bpc,
            ..Self::default()
        };
        let Some(output_name) = output else {
            return detection;
        };
        let Some(path) = existing_config_path() else {
            return detection;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                detection.config_error = Some(format!("cannot read {}: {err}", path.display()));
                return detection;
            }
        };
        detection.config_path = Some(path);
        match parse_output_hdr(&text, &output_name) {
            Ok(config) => detection.config = config,
            Err(err) => detection.config_error = Some(err),
        }
        detection
    }

    /// Whether the output has an `hdr` block at all.
    pub fn hdr_configured(&self) -> bool {
        self.config.is_some()
    }

    /// niri always converts captures back to SDR. Kept as a named fact so it
    /// shows up in metadata and error messages instead of being implicit.
    pub fn niri_tone_maps_capture(&self) -> bool {
        true
    }

    pub fn to_json(&self) -> Value {
        json!({
            "output": self.output,
            "max_bpc": self.max_bpc,
            "configured": self.hdr_configured(),
            "mode": self.config.as_ref().map(|c| c.mode.name()),
            "reference_luminance": self.config.as_ref().and_then(|c| c.reference_luminance),
            "niri_tone_maps_capture": self.niri_tone_maps_capture(),
            "config_path": self.config_path.as_ref().map(|p| p.display().to_string()),
            "error": self.config_error,
        })
    }
}

/// Config candidates in niri's own precedence order.
pub fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(explicit) = std::env::var_os("NIRI_CONFIG").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(explicit);
        paths.push(if path.is_dir() {
            path.join("config.kdl")
        } else {
            path
        });
        return paths;
    }
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(config_home).join("niri/config.kdl"));
    } else if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(home).join(".config/niri/config.kdl"));
    }
    paths.push(PathBuf::from("/etc/niri/config.kdl"));
    paths.dedup();
    paths
}

fn existing_config_path() -> Option<PathBuf> {
    config_paths().into_iter().find(|path| path.is_file())
}

/// Extracts the `hdr` block for one output from a niri KDL config.
///
/// This is a deliberately small KDL reader: it understands nodes, children,
/// quoted/bare values, properties and comments, which is everything niri's
/// output/HDR syntax uses. `include` files are not followed (documented
/// limitation).
pub fn parse_output_hdr(text: &str, output_name: &str) -> Result<Option<HdrConfig>, String> {
    let nodes = parse_document(text)?;
    let Some(output) = nodes
        .iter()
        .find(|node| node.name == "output" && node.args.first().is_some_and(|a| a == output_name))
    else {
        return Ok(None);
    };
    let Some(hdr) = output.children.iter().find(|node| node.name == "hdr") else {
        return Ok(None);
    };

    let mut mode = ConfiguredHdrMode::Auto;
    let mut reference_luminance = None;
    if let Some(value) = hdr.prop("mode") {
        mode = parse_mode(value)?;
    }
    if let Some(value) = hdr.prop("reference-luminance") {
        reference_luminance = Some(parse_luminance(value)?);
    }
    for child in &hdr.children {
        match child.name.as_str() {
            "mode" => {
                if let Some(value) = child.args.first() {
                    mode = parse_mode(value)?;
                }
            }
            "reference-luminance" => {
                if let Some(value) = child.args.first() {
                    reference_luminance = Some(parse_luminance(value)?);
                }
            }
            _ => (),
        }
    }
    Ok(Some(HdrConfig {
        mode,
        reference_luminance,
    }))
}

fn parse_mode(value: &str) -> Result<ConfiguredHdrMode, String> {
    match value {
        "auto" => Ok(ConfiguredHdrMode::Auto),
        "on" => Ok(ConfiguredHdrMode::On),
        other => Err(format!(
            "invalid hdr mode `{other}` in niri config (expected `auto` or `on`)"
        )),
    }
}

fn parse_luminance(value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .map_err(|_| format!("invalid hdr reference-luminance `{value}` in niri config"))
}

#[derive(Debug, Default)]
struct KdlNode {
    name: String,
    args: Vec<String>,
    props: Vec<(String, String)>,
    children: Vec<KdlNode>,
}

impl KdlNode {
    fn prop(&self, name: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    LBrace,
    RBrace,
    Equals,
    Newline,
    Semicolon,
}

fn parse_document(text: &str) -> Result<Vec<KdlNode>, String> {
    let tokens = tokenize(text)?;
    let mut parser = Parser { tokens, pos: 0 };
    parser.parse_nodes(false)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn parse_nodes(&mut self, nested: bool) -> Result<Vec<KdlNode>, String> {
        let mut nodes = Vec::new();
        loop {
            match self.peek().cloned() {
                None => {
                    if nested {
                        return Err("unexpected end of config inside `{`".to_owned());
                    }
                    return Ok(nodes);
                }
                Some(Token::Newline | Token::Semicolon) => {
                    self.bump();
                }
                Some(Token::RBrace) => {
                    if nested {
                        self.bump();
                        return Ok(nodes);
                    }
                    return Err("unexpected `}` in niri config".to_owned());
                }
                Some(Token::Word(_)) => nodes.push(self.parse_node()?),
                Some(other) => return Err(format!("unexpected token {other:?} in niri config")),
            }
        }
    }

    fn parse_node(&mut self) -> Result<KdlNode, String> {
        let Some(Token::Word(name)) = self.bump() else {
            return Err("expected a node name in niri config".to_owned());
        };
        let mut node = KdlNode {
            name,
            ..KdlNode::default()
        };
        loop {
            match self.peek().cloned() {
                None | Some(Token::RBrace) => return Ok(node),
                Some(Token::Newline | Token::Semicolon) => {
                    self.bump();
                    return Ok(node);
                }
                Some(Token::LBrace) => {
                    self.bump();
                    node.children = self.parse_nodes(true)?;
                    // A block ends the node; consume a trailing terminator.
                    if matches!(self.peek(), Some(Token::Newline | Token::Semicolon)) {
                        self.bump();
                    }
                    return Ok(node);
                }
                Some(Token::Equals) => {
                    self.bump();
                    let Some(Token::Word(value)) = self.bump() else {
                        return Err("expected a value after `=` in niri config".to_owned());
                    };
                    let Some((key, _)) = node.props.pop() else {
                        return Err("unexpected `=` in niri config".to_owned());
                    };
                    node.props.push((key, value));
                }
                Some(Token::Word(word)) => {
                    self.bump();
                    if matches!(self.peek(), Some(Token::Equals)) {
                        self.bump();
                        let Some(Token::Word(value)) = self.bump() else {
                            return Err("expected a value after `=` in niri config".to_owned());
                        };
                        node.props.push((word, value));
                    } else {
                        node.args.push(word);
                    }
                }
            }
        }
    }
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                tokens.push(Token::Newline);
                i += 1;
            }
            '\r' => {
                i += 1;
                if chars.get(i) == Some(&'\n') {
                    i += 1;
                }
                tokens.push(Token::Newline);
            }
            ' ' | '\t' | '\u{000b}' | '\u{000c}' => i += 1,
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Equals);
                i += 1;
            }
            ';' => {
                tokens.push(Token::Semicolon);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                let mut closed = false;
                while i + 1 < chars.len() {
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err("unterminated block comment in niri config".to_owned());
                }
            }
            '"' => {
                i += 1;
                let mut value = String::new();
                let mut closed = false;
                while i < chars.len() {
                    match chars[i] {
                        '"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        '\\' => {
                            i += 1;
                            let Some(escaped) = chars.get(i).copied() else {
                                return Err("unterminated escape in niri config".to_owned());
                            };
                            value.push(match escaped {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                '"' => '"',
                                '\\' => '\\',
                                other => other,
                            });
                            i += 1;
                        }
                        other => {
                            value.push(other);
                            i += 1;
                        }
                    }
                }
                if !closed {
                    return Err("unterminated string in niri config".to_owned());
                }
                tokens.push(Token::Word(value));
            }
            _ => {
                let mut value = String::new();
                while i < chars.len() {
                    let ch = chars[i];
                    if ch.is_whitespace() || matches!(ch, '{' | '}' | '=' | ';' | '"') {
                        break;
                    }
                    if ch == '/' && matches!(chars.get(i + 1), Some('/' | '*')) {
                        break;
                    }
                    value.push(ch);
                    i += 1;
                }
                if value.is_empty() {
                    return Err(format!("unexpected character `{c}` in niri config"));
                }
                tokens.push(Token::Word(value));
            }
        }
    }
    Ok(tokens)
}

/// Decodes a normalized PQ electrical value (0..=1) to linear light where
/// 1.0 corresponds to 10 000 cd/m².
pub fn pq_eotf(encoded: f64) -> f64 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;

    let encoded = encoded.clamp(0.0, 1.0);
    let p = encoded.powf(1.0 / M2);
    let numerator = (p - C1).max(0.0);
    let denominator = C2 - C3 * p;
    if denominator <= 0.0 {
        return 1.0;
    }
    (numerator / denominator).powf(1.0 / M1).clamp(0.0, 1.0)
}

/// Encodes linear light (0..=1) as an sRGB electrical value.
pub fn linear_to_srgb(linear: f64) -> f64 {
    let linear = linear.clamp(0.0, 1.0);
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// Maps one PQ-encoded BT.2020 pixel to 8-bit sRGB.
///
/// Steps: PQ EOTF -> absolute linear (10 000 cd/m² scale) -> relative to the
/// configured SDR reference white -> BT.2020 to BT.709 primaries -> Reinhard
/// highlight roll-off -> sRGB transfer. This is intentionally simple and
/// per-channel; it is a display-referred conversion for still screenshots,
/// not a colour-managed HDR pipeline.
pub fn tonemap_pq_rgb(r: f64, g: f64, b: f64, reference_luminance: f64) -> [u8; 3] {
    let reference_luminance = sane_reference_luminance(reference_luminance);
    let to_relative = |encoded: f64| pq_eotf(encoded) * 10_000.0 / reference_luminance;

    let r = to_relative(r);
    let g = to_relative(g);
    let b = to_relative(b);

    // BT.2020 -> BT.709, linear light, D65.
    let r709 = 1.660_491 * r - 0.587_641 * g - 0.072_850 * b;
    let g709 = -0.124_550 * r + 1.132_900 * g - 0.008_349 * b;
    let b709 = -0.018_151 * r - 0.100_579 * g + 1.118_730 * b;

    let roll_off = |value: f64| {
        let value = value.max(0.0);
        value / (1.0 + value)
    };
    [
        encode_srgb(roll_off(r709)),
        encode_srgb(roll_off(g709)),
        encode_srgb(roll_off(b709)),
    ]
}

fn encode_srgb(linear: f64) -> u8 {
    (linear_to_srgb(linear) * 255.0).round().clamp(0.0, 255.0) as u8
}

fn sane_reference_luminance(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value.clamp(1.0, 10_000.0)
    } else {
        DEFAULT_REFERENCE_LUMINANCE
    }
}

/// Tone-maps a high-bit-depth PQ capture to an 8-bit SDR image.
///
/// Returns an error for 8-bit captures: niri has already tone-mapped those,
/// and a second pass would crush the image.
pub fn tonemap_to_sdr(image: &DynamicImage, reference_luminance: f64) -> Result<RgbaImage, String> {
    if !output::is_hdr_source(image) {
        return Err(
            "capture is already 8-bit SDR; niri tone-maps screenshots while capturing, so \
             --hdr=tonemap would apply a second pass. Use --hdr=keep (default) or --hdr=sdr."
                .to_owned(),
        );
    }
    if matches!(
        image.color(),
        image::ColorType::Rgb32F | image::ColorType::Rgba32F
    ) {
        return Err(
            "32-bit float captures are not supported for tone mapping; only 16-bit integer \
             PQ captures are"
                .to_owned(),
        );
    }
    let reference_luminance = sane_reference_luminance(reference_luminance);
    let source = image.to_rgba16();
    let mut output = RgbaImage::new(source.width(), source.height());
    for (dst, src) in output.pixels_mut().zip(source.pixels()) {
        let [r, g, b] = tonemap_pq_rgb(
            f64::from(src[0]) / 65535.0,
            f64::from(src[1]) / 65535.0,
            f64::from(src[2]) / 65535.0,
            reference_luminance,
        );
        let alpha = ((u32::from(src[3]) * 255 + 32_767) / 65_535) as u8;
        *dst = Rgba([r, g, b, alpha]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
// A comment
input {
    keyboard {
        xkb { layout "us" }
    }
}

output "eDP-1" {
    scale 2
    max-bpc 10
    hdr mode="on" {
        reference-luminance 203
    }
}

output "HDMI-A-1" {
    mode "1920x1080@60"
}

output "DP-1" {
    hdr {
        mode "auto"
    }
}
"#;

    #[test]
    fn parses_hdr_blocks_from_config() {
        let hdr = parse_output_hdr(CONFIG, "eDP-1").unwrap().unwrap();
        assert_eq!(hdr.mode, ConfiguredHdrMode::On);
        assert_eq!(hdr.reference_luminance, Some(203.0));

        assert!(parse_output_hdr(CONFIG, "HDMI-A-1").unwrap().is_none());

        let auto = parse_output_hdr(CONFIG, "DP-1").unwrap().unwrap();
        assert_eq!(auto.mode, ConfiguredHdrMode::Auto);
        assert_eq!(auto.reference_luminance, None);

        assert!(parse_output_hdr(CONFIG, "missing").unwrap().is_none());
    }

    #[test]
    fn tolerates_comments_and_inline_blocks() {
        let text = r#"
            output "DP-2" { hdr mode="on" { reference-luminance 400 } }
            /* block
               comment */
        "#;
        let hdr = parse_output_hdr(text, "DP-2").unwrap().unwrap();
        assert_eq!(hdr.mode, ConfiguredHdrMode::On);
        assert_eq!(hdr.reference_luminance, Some(400.0));
    }

    #[test]
    fn rejects_invalid_hdr_values() {
        let text = r#"output "eDP-1" { hdr mode="sometimes" }"#;
        assert!(parse_output_hdr(text, "eDP-1").is_err());
        let text = r#"output "eDP-1" { hdr mode="on" { reference-luminance nope } }"#;
        assert!(parse_output_hdr(text, "eDP-1").is_err());
    }

    #[test]
    fn pq_endpoints_are_sane() {
        assert_eq!(pq_eotf(0.0), 0.0);
        assert!((pq_eotf(1.0) - 1.0).abs() < 1e-9);
        // PQ 0.5 is roughly 100 cd/m², i.e. 1% of the 10 000 cd/m² scale.
        assert!((pq_eotf(0.5) - 0.010).abs() < 0.002);
    }

    #[test]
    fn tone_maps_pq_to_expected_sdr_range() {
        assert_eq!(tonemap_pq_rgb(0.0, 0.0, 0.0, 203.0), [0, 0, 0]);

        let white = tonemap_pq_rgb(1.0, 1.0, 1.0, 203.0);
        assert!(white.iter().all(|&v| v >= 245), "white -> {white:?}");

        let mid = tonemap_pq_rgb(0.5, 0.5, 0.5, 203.0);
        assert!(
            mid.iter().all(|&v| (100..=200).contains(&v)),
            "PQ 0.5 -> {mid:?}"
        );

        let quarter = tonemap_pq_rgb(0.25, 0.25, 0.25, 203.0);
        let three_quarter = tonemap_pq_rgb(0.75, 0.75, 0.75, 203.0);
        assert!(quarter[1] < mid[1] && mid[1] < three_quarter[1]);
        assert!(three_quarter[1] < white[1]);
    }

    #[test]
    fn higher_reference_white_darkens_sdr_mapping() {
        let pq = 0.5;
        let dim = tonemap_pq_rgb(pq, pq, pq, 100.0);
        let bright = tonemap_pq_rgb(pq, pq, pq, 400.0);
        assert!(bright[1] < dim[1]);
    }

    #[test]
    fn tone_maps_sixteen_bit_image_and_preserves_alpha() {
        let source = DynamicImage::ImageRgba16(
            image::ImageBuffer::<Rgba<u16>, Vec<u16>>::from_raw(
                2,
                1,
                vec![
                    0, 0, 0, 0, // black, transparent
                    65535, 65535, 65535, 32768, // white, half alpha
                ],
            )
            .unwrap(),
        );
        let mapped = tonemap_to_sdr(&source, 203.0).unwrap();
        assert_eq!(mapped.get_pixel(0, 0)[3], 0);
        assert_eq!(mapped.get_pixel(1, 0)[3], 128);
        assert!(mapped.get_pixel(1, 0)[0] >= 245);
    }

    #[test]
    fn rejects_tonemapping_eight_bit_capture() {
        let source =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, Rgba([128, 128, 128, 255])));
        let err = tonemap_to_sdr(&source, 203.0).unwrap_err();
        assert!(err.contains("niri tone-maps"));
    }
}
