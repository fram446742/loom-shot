//! Optional JSON sidecar metadata for a capture.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::hdr::{HdrDetection, HdrRequest};

/// Everything recorded next to a screenshot when `--metadata` is used.
#[derive(Debug, Clone)]
pub struct CaptureMetadata {
    pub mode: &'static str,
    pub window_id: Option<u64>,
    pub timestamp_unix: u64,
    pub output: PathBuf,
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_color: &'static str,
    pub hdr: HdrDetection,
    pub hdr_request: HdrRequest,
    pub reference_luminance: Option<f64>,
    pub annotations: Vec<String>,
    pub copied: bool,
    pub dry_run: bool,
    pub command: Vec<String>,
}

impl CaptureMetadata {
    /// Sidecar path: `shot.png` -> `shot.png.json`.
    pub fn sidecar_path(output: &Path) -> PathBuf {
        let mut name = output.as_os_str().to_os_string();
        name.push(".json");
        PathBuf::from(name)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "tool": "loom-shot",
            "version": env!("CARGO_PKG_VERSION"),
            "timestamp_unix": self.timestamp_unix,
            "capture": {
                "mode": self.mode,
                "window_id": self.window_id,
                "output": self.hdr.output,
                "max_bpc": self.hdr.max_bpc,
                "source_color": self.source_color,
                "width": self.width,
                "height": self.height,
            },
            "hdr": {
                "requested": self.hdr_request.name(),
                "detected": self.hdr.to_json(),
                "reference_luminance": self.reference_luminance,
            },
            "file": {
                "path": self.output.display().to_string(),
                "format": self.format,
                "width": self.width,
                "height": self.height,
            },
            "annotations": self.annotations,
            "copied": self.copied,
            "dry_run": self.dry_run,
            "command": self.command,
        })
    }

    /// Writes `<output>.json` and returns its path.
    pub fn write(&self) -> Result<PathBuf, String> {
        let path = Self::sidecar_path(&self.output);
        let mut text = serde_json::to_string_pretty(&self.to_json())
            .map_err(|err| format!("cannot encode metadata: {err}"))?;
        text.push('\n');
        std::fs::write(&path, text)
            .map_err(|err| format!("cannot write metadata {}: {err}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(path: PathBuf) -> CaptureMetadata {
        CaptureMetadata {
            mode: "screen",
            window_id: None,
            timestamp_unix: 1234,
            output: path,
            format: "png",
            width: 640,
            height: 480,
            source_color: "rgba8",
            hdr: HdrDetection {
                output: Some("eDP-1".to_owned()),
                max_bpc: Some(10),
                ..HdrDetection::default()
            },
            hdr_request: HdrRequest::Keep,
            reference_luminance: Some(203.0),
            annotations: vec!["rect:1,2,3,4".to_owned()],
            copied: true,
            dry_run: false,
            command: vec!["loom-shot".to_owned(), "screen".to_owned()],
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-shot-meta-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("shot.png")
    }

    #[test]
    fn sidecar_appends_json_to_the_full_name() {
        assert_eq!(
            CaptureMetadata::sidecar_path(Path::new("/tmp/shot.webp")),
            PathBuf::from("/tmp/shot.webp.json")
        );
    }

    #[test]
    fn json_records_capture_source_hdr_and_command() {
        let metadata = sample(temp_path("json"));
        let value = metadata.to_json();
        assert_eq!(value["tool"], "loom-shot");
        assert_eq!(value["timestamp_unix"], 1234);
        assert_eq!(value["capture"]["mode"], "screen");
        assert_eq!(value["capture"]["output"], "eDP-1");
        assert_eq!(value["capture"]["max_bpc"], 10);
        assert_eq!(value["hdr"]["requested"], "keep");
        assert_eq!(value["hdr"]["detected"]["niri_tone_maps_capture"], true);
        assert_eq!(value["file"]["format"], "png");
        assert_eq!(value["annotations"][0], "rect:1,2,3,4");
        assert_eq!(value["command"][1], "screen");
    }

    #[test]
    fn writes_sidecar_next_to_the_image() {
        let path = temp_path("write");
        std::fs::write(&path, b"png").unwrap();
        let sidecar = sample(path.clone()).write().unwrap();
        assert_eq!(sidecar, CaptureMetadata::sidecar_path(&path));
        let text = std::fs::read_to_string(&sidecar).unwrap();
        assert!(text.ends_with('\n'));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["file"]["path"], path.display().to_string());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
