//! JSON annotation scripts.
//!
//! `--annotations FILE` accepts either a bare array or an object with an `ops`
//! array. Each item is a canonical `kind:...` string or an object whose `op`
//! field names the operation:
//!
//! ```json
//! {
//!   "version": 1,
//!   "ops": [
//!     "crop:0,0,1280,720",
//!     {"op": "blur", "x": 40, "y": 60, "w": 320, "h": 80, "radius": 12},
//!     {"op": "solid-redaction", "x": 0, "y": 700, "w": 1280, "h": 20}
//!   ]
//! }
//! ```
//!
//! Object forms are strict: unknown fields are rejected so a typo cannot
//! silently produce a different redaction. Values are normalised through
//! [`Annotation::parse`], so the same range and geometry checks apply.

use std::path::Path;

use serde_json::Value;

use crate::annotate::Annotation;

/// Loads and validates all operations from a JSON file.
pub fn load_file(path: &Path) -> Result<Vec<Annotation>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read annotations {}: {err}", path.display()))?;
    parse(&text).map_err(|err| format!("{}: {err}", path.display()))
}

/// Parses a JSON annotations document.
pub fn parse(text: &str) -> Result<Vec<Annotation>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|err| format!("invalid annotations JSON: {err}"))?;
    let ops = match value {
        Value::Array(ops) => ops,
        Value::Object(mut object) => {
            if let Some(version) = object.get("version") {
                match version.as_u64() {
                    Some(1) => {}
                    _ => return Err("annotations JSON supports version 1 only".to_owned()),
                }
            }
            check_keys(&object, &["version", "ops"], "annotations document")?;
            match object.remove("ops") {
                Some(Value::Array(ops)) => ops,
                Some(_) => return Err("`ops` must be an array".to_owned()),
                None => return Err("annotations JSON needs an `ops` array".to_owned()),
            }
        }
        _ => return Err("annotations JSON must be an array or an object with `ops`".to_owned()),
    };
    if ops.is_empty() {
        return Err("annotations JSON contains no operations".to_owned());
    }
    ops.iter()
        .enumerate()
        .map(|(index, value)| {
            parse_item(value).map_err(|err| format!("operation {}: {err}", index + 1))
        })
        .collect()
}

fn parse_item(value: &Value) -> Result<Annotation, String> {
    match value {
        Value::String(spec) => Annotation::parse(spec),
        Value::Object(object) => parse_object(object),
        _ => Err("each operation must be a string or an object".to_owned()),
    }
}

fn parse_object(object: &serde_json::Map<String, Value>) -> Result<Annotation, String> {
    let kind = object
        .get("op")
        .or_else(|| object.get("kind"))
        .and_then(Value::as_str)
        .ok_or_else(|| "operation object needs a string `op` field".to_owned())?;
    let spec = match kind {
        "rect" | "highlight" | "crop" => {
            check_keys(object, &["op", "kind", "x", "y", "w", "h"], kind)?;
            let (x, y, w, h) = rect(object, kind)?;
            format!("{kind}:{x},{y},{w},{h}")
        }
        "blur" => {
            check_keys(object, &["op", "kind", "x", "y", "w", "h", "radius"], kind)?;
            let (x, y, w, h) = rect(object, kind)?;
            match optional_u32(object, "radius", kind)? {
                Some(radius) => format!("blur:{x},{y},{w},{h},{radius}"),
                None => format!("blur:{x},{y},{w},{h}"),
            }
        }
        "pixelate" => {
            check_keys(object, &["op", "kind", "x", "y", "w", "h", "block"], kind)?;
            let (x, y, w, h) = rect(object, kind)?;
            match optional_u32(object, "block", kind)? {
                Some(block) => format!("pixelate:{x},{y},{w},{h},{block}"),
                None => format!("pixelate:{x},{y},{w},{h}"),
            }
        }
        "solid-redaction" | "redact" => {
            check_keys(object, &["op", "kind", "x", "y", "w", "h", "color"], kind)?;
            let (x, y, w, h) = rect(object, kind)?;
            match object.get("color") {
                Some(value) => {
                    let color = json_color(value)?;
                    format!("solid-redaction:{x},{y},{w},{h},{color}")
                }
                None => format!("solid-redaction:{x},{y},{w},{h}"),
            }
        }
        "spotlight" => {
            check_keys(object, &["op", "kind", "x", "y", "w", "h", "dim"], kind)?;
            let (x, y, w, h) = rect(object, kind)?;
            match optional_u32(object, "dim", kind)? {
                Some(dim) => format!("spotlight:{x},{y},{w},{h},{dim}"),
                None => format!("spotlight:{x},{y},{w},{h}"),
            }
        }
        "zoom" => {
            check_keys(
                object,
                &["op", "kind", "x", "y", "w", "h", "corner", "scale"],
                kind,
            )?;
            let (x, y, w, h) = rect(object, kind)?;
            let corner = optional_str(object, "corner", kind)?;
            let scale = optional_u32(object, "scale", kind)?;
            match (corner, scale) {
                (Some(corner), Some(scale)) => {
                    format!("zoom:{x},{y},{w},{h},{corner},{scale}")
                }
                (Some(corner), None) => format!("zoom:{x},{y},{w},{h},{corner}"),
                (None, Some(scale)) => format!("zoom:{x},{y},{w},{h},{scale}"),
                (None, None) => format!("zoom:{x},{y},{w},{h}"),
            }
        }
        "text" => {
            check_keys(object, &["op", "kind", "x", "y", "text"], kind)?;
            let x = required_u32(object, "x", kind)?;
            let y = required_u32(object, "y", kind)?;
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "text operation needs a non-empty `text`".to_owned())?;
            format!("text:{x},{y},{text}")
        }
        "arrow" | "line" => {
            check_keys(object, &["op", "kind", "x1", "y1", "x2", "y2"], kind)?;
            let x1 = required_u32(object, "x1", kind)?;
            let y1 = required_u32(object, "y1", kind)?;
            let x2 = required_u32(object, "x2", kind)?;
            let y2 = required_u32(object, "y2", kind)?;
            format!("{kind}:{x1},{y1},{x2},{y2}")
        }
        "ellipse" => {
            check_keys(object, &["op", "kind", "cx", "cy", "rx", "ry"], kind)?;
            let cx = required_u32(object, "cx", kind)?;
            let cy = required_u32(object, "cy", kind)?;
            let rx = required_u32(object, "rx", kind)?;
            let ry = required_u32(object, "ry", kind)?;
            format!("ellipse:{cx},{cy},{rx},{ry}")
        }
        other => {
            return Err(format!(
                "unknown op `{other}` (see `loom-shot annotate --list-ops`)"
            ));
        }
    };
    Annotation::parse(&spec)
}

fn rect(
    object: &serde_json::Map<String, Value>,
    kind: &str,
) -> Result<(u32, u32, u32, u32), String> {
    Ok((
        required_u32(object, "x", kind)?,
        required_u32(object, "y", kind)?,
        required_u32(object, "w", kind)?,
        required_u32(object, "h", kind)?,
    ))
}

fn required_u32(
    object: &serde_json::Map<String, Value>,
    key: &str,
    kind: &str,
) -> Result<u32, String> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("`{kind}` needs a non-negative integer `{key}`"))
}

fn optional_u32(
    object: &serde_json::Map<String, Value>,
    key: &str,
    kind: &str,
) -> Result<Option<u32>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| format!("`{kind}` needs a non-negative integer `{key}`")),
    }
}

fn optional_str<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    kind: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("`{kind}` needs a string `{key}`")),
    }
}

fn json_color(value: &Value) -> Result<String, String> {
    match value {
        Value::String(color) => Ok(color.clone()),
        Value::Array(channels) if channels.len() == 3 => {
            let mut bytes = [0u8; 3];
            for (slot, channel) in bytes.iter_mut().zip(channels) {
                *slot = channel
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .ok_or_else(|| "colour channels must be integers 0-255".to_owned())?;
            }
            Ok(format!("#{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2]))
        }
        _ => Err("colour must be a string like `#ff0000` or three 0-255 integers".to_owned()),
    }
}

fn check_keys(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
    kind: &str,
) -> Result<(), String> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(format!("`{kind}` does not accept the field `{key}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_arrays_of_strings_and_objects() {
        let ops =
            parse(r#"["rect:1,2,3,4", {"op":"blur","x":5,"y":6,"w":7,"h":8,"radius":2}]"#).unwrap();
        assert_eq!(
            ops[0],
            Annotation::Rect {
                x: 1,
                y: 2,
                w: 3,
                h: 4
            }
        );
        assert_eq!(ops[1].describe(), "blur:5,6,7,8,2");
    }

    #[test]
    fn parses_documented_object_document() {
        let ops = parse(
            r#"{
                "version": 1,
                "ops": [
                    "crop:0,0,40,30",
                    {"op": "solid-redaction", "x": 0, "y": 20, "w": 40, "h": 5},
                    {"op": "zoom", "x": 1, "y": 2, "w": 10, "h": 10, "corner": "bottom-left", "scale": 3},
                    {"op": "text", "x": 1, "y": 1, "text": "hi, there"},
                    {"op": "ellipse", "cx": 10, "cy": 10, "rx": 4, "ry": 3}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(ops.len(), 5);
        assert_eq!(ops[1].describe(), "solid-redaction:0,20,40,5,#000000");
        assert_eq!(ops[2].describe(), "zoom:1,2,10,10,bottom-left,3");
    }

    #[test]
    fn accepts_rgb_colour_arrays() {
        let ops = parse(r#"[{"op":"solid-redaction","x":0,"y":0,"w":2,"h":2,"color":[255,0,16]}]"#)
            .unwrap();
        assert_eq!(ops[0].describe(), "solid-redaction:0,0,2,2,#ff0010");
    }

    #[test]
    fn rejects_typos_and_invalid_ranges() {
        for text in [
            r#"[]"#,
            r#"{"ops":[]}"#,
            r#"{"version":2,"ops":["rect:0,0,1,1"]}"#,
            r#"[{"op":"blur","x":0,"y":0,"w":2,"h":2,"radus":1}]"#,
            r#"[{"op":"blur","x":0,"y":0,"w":2,"h":2,"radius":0}]"#,
            r#"[{"op":"solid-redaction","x":0,"y":0,"w":2,"h":2,"color":[1,2]}]"#,
            r#"[{"op":"unknown","x":0}]"#,
            r#"[42]"#,
            r#"{"ops":{}}"#,
            "not json",
        ] {
            assert!(parse(text).is_err(), "accepted {text}");
        }
    }
}
