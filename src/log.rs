//! Persistent annotation log with undo/redo.
//!
//! A log lives next to the image (`shot.png.annotations.json`) and keeps the
//! untouched baseline (`shot.png.annotations.baseline`). Every render replays
//! the log from that baseline, so undo and redo are exact rather than
//! approximate. Adding a new operation after an undo discards the redo tail.
//!
//! The baseline is the pre-annotation image. That is what makes undo possible,
//! but it also means a redacted capture still has an unredacted baseline until
//! `loom-shot annotate PATH --clear-log` is run. `clear` keeps the current
//! (redacted) image and removes both sidecars.

use std::path::{Path, PathBuf};

use image::RgbaImage;
use serde_json::{Value, json};

use crate::annotate::Annotation;
use crate::output::{self, OutputFormat};

pub const LOG_SUFFIX: &str = ".annotations.json";
pub const BASELINE_SUFFIX: &str = ".annotations.baseline";
const VERSION: u64 = 1;

/// An on-disk annotation log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationLog {
    pub image: PathBuf,
    pub baseline: PathBuf,
    pub ops: Vec<Annotation>,
    /// Number of leading `ops` baked into `image`.
    pub applied: usize,
}

impl AnnotationLog {
    pub fn to_json(&self) -> Value {
        json!({
            "version": VERSION,
            "image": self.image.display().to_string(),
            "baseline": self.baseline.display().to_string(),
            "ops": self.ops.iter().map(Annotation::describe).collect::<Vec<_>>(),
            "applied": self.applied,
            "pending_redo": self.ops.len().saturating_sub(self.applied),
        })
    }

    /// `shot.png` -> `shot.png.annotations.json`.
    pub fn path_for(image: &Path) -> PathBuf {
        with_suffix(image, LOG_SUFFIX)
    }

    /// `shot.png` -> `shot.png.annotations.baseline`.
    pub fn baseline_for(image: &Path) -> PathBuf {
        with_suffix(image, BASELINE_SUFFIX)
    }

    /// `shot.png` -> `shot.preview.png`.
    pub fn preview_for(image: &Path) -> PathBuf {
        let extension = image
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("png");
        let stem = image
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("image");
        image.with_file_name(format!("{stem}.preview.{extension}"))
    }

    fn from_json(value: &Value, image: &Path) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "annotation log must be a JSON object".to_owned())?;
        let version = object.get("version").and_then(Value::as_u64);
        if version != Some(VERSION) {
            return Err(format!("unsupported annotation log version {version:?}"));
        }
        let recorded_image = object
            .get("image")
            .and_then(Value::as_str)
            .ok_or_else(|| "annotation log is missing `image`".to_owned())?;
        if Path::new(recorded_image) != image {
            return Err(format!(
                "annotation log belongs to {recorded_image}, not {}",
                image.display()
            ));
        }
        let baseline = object
            .get("baseline")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| Self::baseline_for(image));
        let raw_ops = object
            .get("ops")
            .and_then(Value::as_array)
            .ok_or_else(|| "annotation log is missing its `ops` array".to_owned())?;
        let mut ops = Vec::with_capacity(raw_ops.len());
        for (index, value) in raw_ops.iter().enumerate() {
            let spec = value
                .as_str()
                .ok_or_else(|| format!("annotation log op {} is not a string", index + 1))?;
            ops.push(
                Annotation::parse(spec)
                    .map_err(|err| format!("annotation log op {}: {err}", index + 1))?,
            );
        }
        let applied = object
            .get("applied")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "annotation log is missing `applied`".to_owned())?;
        if applied > ops.len() {
            return Err(format!(
                "annotation log applies {applied} of {} operations",
                ops.len()
            ));
        }
        Ok(Self {
            image: image.to_owned(),
            baseline,
            ops,
            applied,
        })
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path_for(&self.image);
        let mut text = serde_json::to_string_pretty(&self.to_json())
            .map_err(|err| format!("cannot encode annotation log: {err}"))?;
        text.push('\n');
        write_atomic(&path, text.as_bytes())
    }

    /// Replays `applied` operations from the baseline and writes the image.
    fn render(&self) -> Result<(), String> {
        let baseline = std::fs::read(&self.baseline).map_err(|err| {
            format!(
                "cannot read annotation baseline {}: {err}; use --clear-log to start over",
                self.baseline.display()
            )
        })?;
        if self.applied == 0 {
            return write_atomic(&self.image, &baseline);
        }
        let decoded = image::load_from_memory(&baseline).map_err(|err| {
            format!(
                "cannot decode annotation baseline {}: {err}",
                self.baseline.display()
            )
        })?;
        let mut canvas = decoded.to_rgba8();
        for op in &self.ops[..self.applied] {
            canvas = op.apply(canvas)?;
        }
        let format = OutputFormat::from_path(&self.image).unwrap_or(OutputFormat::Png);
        let bytes = format.encode(&canvas, 90)?;
        write_atomic(&self.image, &bytes)
    }
}

/// Loads the log for `image`, or `None` when no log exists.
pub fn load(image: &Path) -> Result<Option<AnnotationLog>, String> {
    let path = AnnotationLog::path_for(image);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read annotation log {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("invalid annotation log {}: {err}", path.display()))?;
    AnnotationLog::from_json(&value, image).map(Some)
}

/// Ensures the untouched baseline exists for `image`.
fn ensure_baseline(image: &Path, existing: Option<&AnnotationLog>) -> Result<PathBuf, String> {
    if !image.is_file() {
        return Err(format!("{} is not a file", image.display()));
    }
    let baseline = AnnotationLog::baseline_for(image);
    if baseline.exists() {
        return Ok(baseline);
    }
    if existing.is_some() {
        return Err(format!(
            "annotation log exists but its baseline {} is missing; use --clear-log",
            baseline.display()
        ));
    }
    std::fs::copy(image, &baseline).map_err(|err| {
        format!(
            "cannot create annotation baseline {}: {err}",
            baseline.display()
        )
    })?;
    Ok(baseline)
}

/// Applies `ops` and records them in the persistent log.
///
/// Operations are validated and rendered in memory before anything is written,
/// so a failing batch leaves the image and log untouched.
pub fn append(image: &Path, ops: &[Annotation]) -> Result<AnnotationLog, String> {
    let existing = load(image)?;
    let baseline_existed = AnnotationLog::baseline_for(image).exists();
    let baseline = ensure_baseline(image, existing.as_ref())?;
    let previous = existing
        .as_ref()
        .map(|log| log.ops[..log.applied].to_vec())
        .unwrap_or_default();
    let mut all = previous;
    all.extend_from_slice(ops);
    let log = AnnotationLog {
        image: image.to_owned(),
        baseline,
        ops: all,
        applied: 0,
    };
    let mut candidate = log.clone();
    candidate.applied = candidate.ops.len();
    if let Err(err) = render_candidate(&candidate) {
        if !baseline_existed {
            let _ = std::fs::remove_file(AnnotationLog::baseline_for(image));
        }
        return Err(err);
    }
    candidate.render()?;
    candidate.save()?;
    Ok(candidate)
}

/// Undoes the last `count` applied operations.
pub fn undo(image: &Path, count: usize) -> Result<AnnotationLog, String> {
    let mut log = load(image)?.ok_or_else(|| {
        format!(
            "no annotation log for {}; there is nothing to undo",
            image.display()
        )
    })?;
    log.applied = log.applied.saturating_sub(count);
    log.render()?;
    log.save()?;
    Ok(log)
}

/// Redoes up to `count` operations.
pub fn redo(image: &Path, count: usize) -> Result<AnnotationLog, String> {
    let mut log = load(image)?.ok_or_else(|| {
        format!(
            "no annotation log for {}; there is nothing to redo",
            image.display()
        )
    })?;
    log.applied = log.applied.saturating_add(count).min(log.ops.len());
    log.render()?;
    log.save()?;
    Ok(log)
}

/// Removes the log and baseline, leaving the current image untouched.
///
/// Returns whether anything was removed.
pub fn clear(image: &Path) -> Result<bool, String> {
    let mut removed = false;
    for path in [
        AnnotationLog::path_for(image),
        AnnotationLog::baseline_for(image),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!("cannot remove {}: {err}", path.display()));
            }
        }
    }
    Ok(removed)
}

/// Renders `ops` on top of the current image to `shot.preview.<ext>`.
///
/// The image and log are not modified.
pub fn preview(image: &Path, ops: &[Annotation]) -> Result<PathBuf, String> {
    let source = output::load(image)?;
    let mut canvas: RgbaImage = source.to_rgba8();
    for op in ops {
        canvas = op.apply(canvas)?;
    }
    let path = AnnotationLog::preview_for(image);
    let format = OutputFormat::from_path(&path).unwrap_or(OutputFormat::Png);
    let bytes = format.encode(&canvas, 90)?;
    write_atomic(&path, &bytes)?;
    Ok(path)
}

fn render_candidate(log: &AnnotationLog) -> Result<(), String> {
    // Validation pass: decode the baseline and apply every operation without
    // writing anything. `render` then repeats the work, which keeps the write
    // path simple and guarantees a failure cannot leave a half-written image.
    let baseline = std::fs::read(&log.baseline)
        .map_err(|err| format!("cannot read {}: {err}", log.baseline.display()))?;
    let decoded = image::load_from_memory(&baseline)
        .map_err(|err| format!("cannot decode {}: {err}", log.baseline.display()))?;
    let mut canvas = decoded.to_rgba8();
    for op in &log.ops {
        canvas = op.apply(canvas)?;
    }
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("loom-shot"),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes)
        .map_err(|err| format!("cannot write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|err| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace {}: {err}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;

    fn temp_image(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-shot-log-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        let image = RgbaImage::from_pixel(20, 20, Rgba([200, 200, 200, 255]));
        std::fs::write(&path, OutputFormat::Png.encode(&image, 100).unwrap()).unwrap();
        path
    }

    fn load_rgba(path: &Path) -> RgbaImage {
        output::load(path).unwrap().to_rgba8()
    }

    #[test]
    fn append_creates_log_and_baseline_and_bakes_ops() {
        let path = temp_image("append");
        let log = append(
            &path,
            &[
                Annotation::parse("solid-redaction:0,0,10,10").unwrap(),
                Annotation::parse("rect:0,0,20,20").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(log.applied, 2);
        assert_eq!(log.ops.len(), 2);
        assert!(AnnotationLog::path_for(&path).exists());
        assert!(AnnotationLog::baseline_for(&path).exists());
        let image = load_rgba(&path);
        assert_eq!(*image.get_pixel(5, 5), Rgba([0, 0, 0, 255]));
        assert_eq!(*image.get_pixel(0, 0), Rgba([255, 59, 48, 255]));

        let reloaded = load(&path).unwrap().unwrap();
        assert_eq!(reloaded.applied, 2);
        assert_eq!(
            reloaded.ops[0].describe(),
            "solid-redaction:0,0,10,10,#000000"
        );
    }

    #[test]
    fn append_rejects_a_bad_batch_without_writing() {
        let path = temp_image("reject");
        let before = std::fs::read(&path).unwrap();
        let err = append(
            &path,
            &[
                Annotation::parse("rect:0,0,5,5").unwrap(),
                Annotation::parse("crop:0,0,500,500").unwrap(),
            ],
        )
        .unwrap_err();
        assert!(err.contains("does not fit"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!AnnotationLog::path_for(&path).exists());
        assert!(!AnnotationLog::baseline_for(&path).exists());
    }

    #[test]
    fn undo_and_redo_replay_from_the_baseline() {
        let path = temp_image("undo");
        let baseline = std::fs::read(&path).unwrap();
        append(
            &path,
            &[
                Annotation::parse("solid-redaction:0,0,10,10").unwrap(),
                Annotation::parse("rect:0,0,20,20").unwrap(),
            ],
        )
        .unwrap();

        let log = undo(&path, 1).unwrap();
        assert_eq!(log.applied, 1);
        let image = load_rgba(&path);
        assert_eq!(*image.get_pixel(0, 0), Rgba([0, 0, 0, 255]));
        assert_eq!(*image.get_pixel(19, 19), Rgba([200, 200, 200, 255]));

        let log = undo(&path, 5).unwrap();
        assert_eq!(log.applied, 0);
        assert_eq!(std::fs::read(&path).unwrap(), baseline);

        let log = redo(&path, 1).unwrap();
        assert_eq!(log.applied, 1);
        let log = redo(&path, 9).unwrap();
        assert_eq!(log.applied, 2);
        assert_eq!(*load_rgba(&path).get_pixel(0, 0), Rgba([255, 59, 48, 255]));

        assert!(undo(&temp_image("no-log"), 1).is_err());
    }

    #[test]
    fn new_ops_after_undo_discard_the_redo_tail() {
        let path = temp_image("truncate");
        append(&path, &[Annotation::parse("rect:0,0,20,20").unwrap()]).unwrap();
        undo(&path, 1).unwrap();
        let log = append(&path, &[Annotation::parse("highlight:0,0,5,5").unwrap()]).unwrap();
        assert_eq!(log.ops.len(), 1);
        assert_eq!(log.ops[0].describe(), "highlight:0,0,5,5");
        assert_eq!(log.applied, 1);
    }

    #[test]
    fn preview_does_not_touch_the_image_or_log() {
        let path = temp_image("preview");
        append(&path, &[Annotation::parse("rect:0,0,20,20").unwrap()]).unwrap();
        let image_before = std::fs::read(&path).unwrap();
        let log_before = std::fs::read(AnnotationLog::path_for(&path)).unwrap();
        let preview_path = preview(
            &path,
            &[Annotation::parse("solid-redaction:2,2,4,4").unwrap()],
        )
        .unwrap();
        assert_eq!(
            preview_path,
            path.with_file_name("shot.preview.png"),
            "preview path is derived from the image"
        );
        assert_eq!(std::fs::read(&path).unwrap(), image_before);
        assert_eq!(
            std::fs::read(AnnotationLog::path_for(&path)).unwrap(),
            log_before
        );
        assert_eq!(
            *load_rgba(&preview_path).get_pixel(3, 3),
            Rgba([0, 0, 0, 255])
        );
    }

    #[test]
    fn clear_removes_sidecars_but_keeps_the_image() {
        let path = temp_image("clear");
        append(&path, &[Annotation::parse("rect:0,0,20,20").unwrap()]).unwrap();
        let image = std::fs::read(&path).unwrap();
        assert!(clear(&path).unwrap());
        assert!(!AnnotationLog::path_for(&path).exists());
        assert!(!AnnotationLog::baseline_for(&path).exists());
        assert_eq!(std::fs::read(&path).unwrap(), image);
        assert!(!clear(&path).unwrap());
    }

    #[test]
    fn missing_baseline_is_an_explicit_error() {
        let path = temp_image("missing-baseline");
        append(&path, &[Annotation::parse("rect:0,0,20,20").unwrap()]).unwrap();
        std::fs::remove_file(AnnotationLog::baseline_for(&path)).unwrap();
        let err = undo(&path, 1).unwrap_err();
        assert!(err.contains("baseline"), "{err}");
    }

    #[test]
    fn malformed_log_is_rejected() {
        let path = temp_image("malformed");
        std::fs::write(AnnotationLog::path_for(&path), "{ nope").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("invalid annotation log"), "{err}");
    }
}
