//! `loom-shot annotate PATH`: a display-free annotation editor.
//!
//! Operations are applied in the order given: each `--op` and each
//! `--annotations FILE` contributes at that position. The persistent log makes
//! `--undo` / `--redo` exact (the original image is kept as a baseline), and
//! `--preview` renders to `PATH.preview.EXT` without touching either the image
//! or the log. `--clear-log` removes the baseline for privacy; the current
//! (possibly redacted) image is kept.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::annotate::{Annotation, OPS_HELP};
use crate::log::{self, AnnotationLog};
use crate::script;

pub const USAGE: &str = r#"loom-shot annotate PATH [options]
loom-shot annotate --list-ops

  PATH                    Image to edit (in place)
  --op SPEC               Add an operation (repeatable, applied in order)
  --annotations FILE      Add operations from a JSON file (repeatable)
  --undo [N]              Undo the last N operations (default 1)
  --redo [N]              Redo up to N operations (default 1)
  --preview               Render to PATH.preview.EXT without changing image/log
  --history               Print the persistent annotation log
  --clear-log             Delete log and baseline; keep the current image
  --list-ops              List supported operations and syntax
  --json                  Print JSON (results are JSON by default)

  Operations are validated and rendered in memory before the image is
  replaced, so a failing batch writes nothing. The log lives next to the image
  (PATH.annotations.json) with PATH.annotations.baseline. The baseline is the
  pre-annotation image; run --clear-log after redacting if that copy must not
  remain. See `--list-ops` for operation syntax.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpSource {
    Op(Annotation),
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnnotateOptions {
    pub path: Option<PathBuf>,
    pub sources: Vec<OpSource>,
    pub undo: Option<usize>,
    pub redo: Option<usize>,
    pub preview: bool,
    pub history: bool,
    pub list_ops: bool,
    pub clear_log: bool,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AnnotateCli {
    Run(AnnotateOptions),
    Help,
}

/// Parses arguments after the `annotate` token.
pub fn parse_cli<I, S>(args: I) -> Result<AnnotateCli, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into).peekable();
    let mut options = AnnotateOptions::default();
    while let Some(raw) = args.next() {
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (raw, None),
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(AnnotateCli::Help),
            "--json" => {
                no_value(&flag, inline)?;
                options.json = true;
            }
            "--preview" => {
                no_value(&flag, inline)?;
                options.preview = true;
            }
            "--history" => {
                no_value(&flag, inline)?;
                options.history = true;
            }
            "--clear-log" => {
                no_value(&flag, inline)?;
                options.clear_log = true;
            }
            "--list-ops" => {
                no_value(&flag, inline)?;
                options.list_ops = true;
            }
            "--op" => {
                let spec = value(&flag, inline, &mut args)?;
                options
                    .sources
                    .push(OpSource::Op(Annotation::parse(&spec)?));
            }
            "--annotations" => {
                options.sources.push(OpSource::File(PathBuf::from(value(
                    &flag, inline, &mut args,
                )?)));
            }
            "--undo" => options.undo = Some(optional_count(&flag, inline, &mut args)?),
            "--redo" => options.redo = Some(optional_count(&flag, inline, &mut args)?),
            other if other.starts_with('-') => {
                return Err(format!("unknown annotate option `{other}`\n\n{USAGE}"));
            }
            other => {
                if options.path.is_some() {
                    return Err(format!("unexpected annotate argument `{other}`\n\n{USAGE}"));
                }
                options.path = Some(PathBuf::from(other));
            }
        }
    }
    validate(&options)?;
    Ok(AnnotateCli::Run(options))
}

fn validate(options: &AnnotateOptions) -> Result<(), String> {
    if options.list_ops {
        if options.path.is_some()
            || !options.sources.is_empty()
            || options.undo.is_some()
            || options.redo.is_some()
            || options.preview
            || options.history
            || options.clear_log
        {
            return Err("--list-ops must be used on its own".to_owned());
        }
        return Ok(());
    }
    if options.path.is_none() {
        return Err(format!("`annotate` needs an image path\n\n{USAGE}"));
    }
    if options.undo.is_some() && options.redo.is_some() {
        return Err("--undo and --redo are mutually exclusive".to_owned());
    }
    if let Some(count) = options.undo.or(options.redo)
        && count == 0
    {
        return Err("--undo/--redo need a positive count".to_owned());
    }
    if options.history
        && (!options.sources.is_empty()
            || options.undo.is_some()
            || options.redo.is_some()
            || options.preview
            || options.clear_log)
    {
        return Err("--history cannot be combined with edits".to_owned());
    }
    if options.clear_log
        && (!options.sources.is_empty()
            || options.undo.is_some()
            || options.redo.is_some()
            || options.preview
            || options.history)
    {
        return Err("--clear-log cannot be combined with edits".to_owned());
    }
    if options.preview && (options.undo.is_some() || options.redo.is_some()) {
        return Err("--preview cannot be combined with --undo/--redo".to_owned());
    }
    if (options.undo.is_some() || options.redo.is_some()) && !options.sources.is_empty() {
        return Err("--undo/--redo cannot be combined with new operations".to_owned());
    }
    Ok(())
}

fn optional_count<I>(
    flag: &str,
    inline: Option<String>,
    iter: &mut std::iter::Peekable<I>,
) -> Result<usize, String>
where
    I: Iterator<Item = String>,
{
    let raw = match inline {
        Some(value) if value.is_empty() => return Ok(1),
        Some(value) => value,
        None => match iter.peek() {
            Some(value) if !value.starts_with('-') => iter.next().unwrap(),
            _ => return Ok(1),
        },
    };
    raw.parse()
        .map_err(|_| format!("{flag} needs a positive number, got `{raw}`"))
}

fn value<I>(
    flag: &str,
    inline: Option<String>,
    iter: &mut std::iter::Peekable<I>,
) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(format!("{flag} needs a value"));
        }
        return Ok(value);
    }
    iter.next()
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn no_value(flag: &str, inline: Option<String>) -> Result<(), String> {
    if inline.is_some() {
        Err(format!("{flag} does not take a value"))
    } else {
        Ok(())
    }
}

impl AnnotateOptions {
    /// Resolves `--op` and `--annotations` sources in command-line order.
    pub fn operations(&self) -> Result<Vec<Annotation>, String> {
        let mut ops = Vec::new();
        for source in &self.sources {
            match source {
                OpSource::Op(annotation) => ops.push(annotation.clone()),
                OpSource::File(path) => ops.extend(script::load_file(path)?),
            }
        }
        Ok(ops)
    }
}

/// Runs the annotate command and returns printable text.
pub fn run(options: &AnnotateOptions) -> Result<String, String> {
    if options.list_ops {
        return Ok(OPS_HELP.trim_end().to_owned());
    }
    let path = options
        .path
        .as_ref()
        .ok_or_else(|| "`annotate` needs an image path".to_owned())?;

    if options.clear_log {
        let cleared = log::clear(path)?;
        return Ok(pretty(&json!({
            "image": path.display().to_string(),
            "cleared": cleared,
            "log": AnnotationLog::path_for(path).display().to_string(),
            "baseline": AnnotationLog::baseline_for(path).display().to_string(),
        })));
    }

    if options.history {
        return Ok(pretty(&history_json(path)?));
    }

    if let Some(count) = options.undo {
        let log = log::undo(path, count)?;
        return Ok(pretty(&log.to_json()));
    }
    if let Some(count) = options.redo {
        let log = log::redo(path, count)?;
        return Ok(pretty(&log.to_json()));
    }

    let ops = options.operations()?;
    if options.preview {
        let preview = log::preview(path, &ops)?;
        return Ok(pretty(&json!({
            "image": path.display().to_string(),
            "preview": preview.display().to_string(),
            "operations": ops.iter().map(Annotation::describe).collect::<Vec<_>>(),
        })));
    }
    if ops.is_empty() {
        return Ok(pretty(&history_json(path)?));
    }
    let log = log::append(path, &ops)?;
    Ok(pretty(&log.to_json()))
}

fn history_json(path: &std::path::Path) -> Result<Value, String> {
    match log::load(path)? {
        Some(log) => {
            let mut value = log.to_json();
            if let Some(object) = value.as_object_mut() {
                object.insert("exists".to_owned(), Value::Bool(true));
            }
            Ok(value)
        }
        None => Ok(json!({
            "image": path.display().to_string(),
            "baseline": AnnotationLog::baseline_for(path).display().to_string(),
            "ops": [],
            "applied": 0,
            "pending_redo": 0,
            "exists": false,
        })),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;
    use crate::output::{self, OutputFormat};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-shot-editor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_image(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("shot.png");
        let image = image::RgbaImage::from_pixel(20, 20, Rgba([200, 200, 200, 255]));
        std::fs::write(&path, OutputFormat::Png.encode(&image, 100).unwrap()).unwrap();
        path
    }

    fn parse_options(args: &[&str]) -> AnnotateOptions {
        match parse_cli(args.iter().copied()).unwrap() {
            AnnotateCli::Run(options) => options,
            AnnotateCli::Help => panic!("expected run, got help"),
        }
    }

    fn load(path: &std::path::Path) -> image::RgbaImage {
        output::load(path).unwrap().to_rgba8()
    }

    #[test]
    fn parses_operations_undo_preview_and_history() {
        let options = parse_options(&[
            "shot.png",
            "--op",
            "rect:0,0,5,5",
            "--annotations",
            "ops.json",
            "--op=blur:1,1,5,5,2",
            "--json",
        ]);
        assert_eq!(
            options.path.as_deref(),
            Some(std::path::Path::new("shot.png"))
        );
        assert_eq!(options.sources.len(), 3);
        assert_eq!(options.undo, None);
        assert!(options.json);

        let options = parse_options(&["shot.png", "--undo", "2"]);
        assert_eq!(options.undo, Some(2));
        let options = parse_options(&["shot.png", "--redo"]);
        assert_eq!(options.redo, Some(1));
        let options = parse_options(&["shot.png", "--preview", "--op", "crop:0,0,5,5"]);
        assert!(options.preview);

        let options = parse_options(&["--list-ops"]);
        assert!(options.list_ops);
    }

    #[test]
    fn rejects_invalid_combinations() {
        for args in [
            vec![],
            vec!["shot.png", "--undo", "--redo"],
            vec!["shot.png", "--undo", "0"],
            vec!["shot.png", "--history", "--op", "rect:0,0,5,5"],
            vec!["shot.png", "--clear-log", "--preview"],
            vec!["shot.png", "--undo", "1", "--op", "rect:0,0,5,5"],
            vec!["shot.png", "--preview", "--undo"],
            vec!["--list-ops", "shot.png"],
            vec!["shot.png", "--bogus"],
            vec!["shot.png", "extra.png"],
            vec!["shot.png", "--op"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn commands_resolve_sources_in_order() {
        let dir = temp_dir("order");
        let ops_file = dir.join("ops.json");
        std::fs::write(&ops_file, r#"["highlight:0,0,2,2"]"#).unwrap();
        let options = AnnotateOptions {
            path: Some(dir.join("shot.png")),
            sources: vec![
                OpSource::Op(Annotation::parse("rect:0,0,5,5").unwrap()),
                OpSource::File(ops_file),
                OpSource::Op(Annotation::parse("line:1,1,5,5").unwrap()),
            ],
            ..AnnotateOptions::default()
        };
        let ops = options.operations().unwrap();
        assert_eq!(
            ops.iter().map(Annotation::describe).collect::<Vec<_>>(),
            ["rect:0,0,5,5", "highlight:0,0,2,2", "line:1,1,5,5"]
        );
    }

    #[test]
    fn append_history_undo_redo_preview_and_clear() {
        let dir = temp_dir("flow");
        let path = temp_image(&dir);
        let baseline = std::fs::read(&path).unwrap();

        let append = AnnotateOptions {
            path: Some(path.clone()),
            sources: vec![OpSource::Op(
                Annotation::parse("solid-redaction:0,0,10,10").unwrap(),
            )],
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&append).unwrap()).unwrap();
        assert_eq!(value["applied"], 1);
        assert_eq!(*load(&path).get_pixel(5, 5), Rgba([0, 0, 0, 255]));

        let history = AnnotateOptions {
            path: Some(path.clone()),
            history: true,
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&history).unwrap()).unwrap();
        assert_eq!(value["exists"], true);
        assert_eq!(value["ops"][0], "solid-redaction:0,0,10,10,#000000");

        let image_before = std::fs::read(&path).unwrap();
        let preview = AnnotateOptions {
            path: Some(path.clone()),
            sources: vec![OpSource::Op(Annotation::parse("rect:0,0,20,20").unwrap())],
            preview: true,
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&preview).unwrap()).unwrap();
        let preview_path = PathBuf::from(value["preview"].as_str().unwrap());
        assert!(preview_path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), image_before);

        let undo = AnnotateOptions {
            path: Some(path.clone()),
            undo: Some(1),
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&undo).unwrap()).unwrap();
        assert_eq!(value["applied"], 0);
        assert_eq!(std::fs::read(&path).unwrap(), baseline);

        let redo = AnnotateOptions {
            path: Some(path.clone()),
            redo: Some(1),
            ..AnnotateOptions::default()
        };
        run(&redo).unwrap();
        assert_eq!(*load(&path).get_pixel(5, 5), Rgba([0, 0, 0, 255]));

        let clear = AnnotateOptions {
            path: Some(path.clone()),
            clear_log: true,
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&clear).unwrap()).unwrap();
        assert_eq!(value["cleared"], true);
        assert!(!AnnotationLog::path_for(&path).exists());
        assert!(!AnnotationLog::baseline_for(&path).exists());
        assert_eq!(*load(&path).get_pixel(5, 5), Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn list_ops_and_empty_history() {
        let options = AnnotateOptions {
            list_ops: true,
            ..AnnotateOptions::default()
        };
        let text = run(&options).unwrap();
        for name in [
            "blur",
            "pixelate",
            "solid-redaction",
            "crop",
            "zoom",
            "spotlight",
        ] {
            assert!(text.contains(name), "missing {name}");
        }

        let dir = temp_dir("empty");
        let path = temp_image(&dir);
        let history = AnnotateOptions {
            path: Some(path),
            history: true,
            ..AnnotateOptions::default()
        };
        let value: Value = serde_json::from_str(&run(&history).unwrap()).unwrap();
        assert_eq!(value["exists"], false);
        assert_eq!(value["applied"], 0);
    }
}
