//! Optional OCR through an installed `tesseract`.
//!
//! loom-shot does not link an OCR engine. `--ocr` (on a capture) or
//! `loom-shot ocr PATH` invokes `tesseract IMAGE stdout`, using
//! `$LOOM_SHOT_TESSERACT` when set and `tesseract` from `PATH` otherwise.
//! Missing tesseract is a clear, actionable error; nothing is silently skipped.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

pub const USAGE: &str = r#"loom-shot ocr PATH [options]

  PATH          Image to recognise (PNG, JPEG or WebP)
  --json        Print the JSON result (default)
  --text        Print only the recognised text
  --sidecar     Write <PATH>.ocr.json and report its path

  Engine: $LOOM_SHOT_TESSERACT, else `tesseract` from PATH.
  If tesseract is missing, the command fails with an install hint instead of
  pretending OCR succeeded. The sidecar contains the same JSON as --json.
"#;

/// Structured OCR output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<String>,
    pub program: String,
}

impl OcrResult {
    pub fn to_json(&self, image: &Path) -> Value {
        json!({
            "engine": "tesseract",
            "program": self.program,
            "image": image.display().to_string(),
            "text": self.text,
            "lines": self.lines,
        })
    }

    /// `<image>.ocr.json`, next to the image.
    pub fn sidecar_path(image: &Path) -> PathBuf {
        let mut name = image.as_os_str().to_os_string();
        name.push(".ocr.json");
        PathBuf::from(name)
    }

    pub fn write_sidecar(&self, image: &Path) -> Result<PathBuf, String> {
        let path = Self::sidecar_path(image);
        let mut text = serde_json::to_string_pretty(&self.to_json(image))
            .map_err(|err| format!("cannot encode OCR sidecar: {err}"))?;
        text.push('\n');
        std::fs::write(&path, text)
            .map_err(|err| format!("cannot write OCR sidecar {}: {err}", path.display()))?;
        Ok(path)
    }
}

/// Resolves the tesseract program: `$LOOM_SHOT_TESSERACT`, then `tesseract`.
pub fn program_from_env() -> String {
    std::env::var("LOOM_SHOT_TESSERACT")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "tesseract".to_owned())
}

/// Recognises text using the configured tesseract.
pub fn recognize(image: &Path) -> Result<OcrResult, String> {
    recognize_with(image, &program_from_env())
}

/// Recognises text with an explicit program (used by tests and integrations).
pub fn recognize_with(image: &Path, program: &str) -> Result<OcrResult, String> {
    if !image.is_file() {
        return Err(format!("{} is not a file", image.display()));
    }
    let output = Command::new(program)
        .arg(image)
        .arg("stdout")
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "tesseract is not available (`{program}` not found); install tesseract or \
                     set LOOM_SHOT_TESSERACT"
                )
            } else {
                format!("cannot run tesseract `{program}`: {err}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("tesseract exited with {}", output.status)
        } else {
            format!("tesseract exited with {}: {stderr}", output.status)
        };
        return Err(detail);
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|err| format!("tesseract returned non-UTF-8 output: {err}"))?;
    let lines = text.lines().map(str::to_owned).collect();
    Ok(OcrResult {
        text,
        lines,
        program: program.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrOptions {
    pub path: PathBuf,
    pub json: bool,
    pub text: bool,
    pub sidecar: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OcrCli {
    Run(OcrOptions),
    Help,
}

/// Parses arguments after the `ocr` token.
pub fn parse_cli<I, S>(args: I) -> Result<OcrCli, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut path = None;
    let mut options = OcrOptions {
        path: PathBuf::new(),
        json: false,
        text: false,
        sidecar: false,
    };
    for raw in args.into_iter().map(Into::into) {
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (raw, None),
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(OcrCli::Help),
            "--json" => {
                no_value(&flag, inline)?;
                options.json = true;
            }
            "--text" => {
                no_value(&flag, inline)?;
                options.text = true;
            }
            "--sidecar" => {
                no_value(&flag, inline)?;
                options.sidecar = true;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown ocr option `{other}`\n\n{USAGE}"));
            }
            other => {
                if path.is_some() {
                    return Err(format!("unexpected ocr argument `{other}`\n\n{USAGE}"));
                }
                path = Some(PathBuf::from(other));
            }
        }
    }
    let path = path.ok_or_else(|| format!("`ocr` needs an image path\n\n{USAGE}"))?;
    if options.json && options.text {
        return Err("--json and --text are mutually exclusive".to_owned());
    }
    options.path = path;
    Ok(OcrCli::Run(options))
}

fn no_value(flag: &str, inline: Option<String>) -> Result<(), String> {
    if inline.is_some() {
        Err(format!("{flag} does not take a value"))
    } else {
        Ok(())
    }
}

/// Runs `loom-shot ocr` and returns printable text.
pub fn run(options: &OcrOptions) -> Result<String, String> {
    let result = recognize(&options.path)?;
    let sidecar = if options.sidecar {
        Some(result.write_sidecar(&options.path)?)
    } else {
        None
    };
    if options.text {
        return Ok(result.text.trim_end().to_owned());
    }
    let mut value = result.to_json(&options.path);
    if let (Some(sidecar), Some(object)) = (sidecar, value.as_object_mut()) {
        object.insert(
            "sidecar".to_owned(),
            Value::String(sidecar.display().to_string()),
        );
    }
    Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-shot-ocr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Other tests in this process fork while a freshly written script can still
    /// be open for writing in another thread, which yields ETXTBSY. Retrying is
    /// the portable way to avoid that test-only race.
    fn recognize_retry(image: &Path, program: &str) -> Result<OcrResult, String> {
        let mut last = None;
        for _ in 0..50 {
            match recognize_with(image, program) {
                Err(err) if err.contains("Text file busy") => {
                    last = Some(err);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => return other,
            }
        }
        Err(last.unwrap_or_else(|| "tesseract did not run".to_owned()))
    }

    #[test]
    fn parses_ocr_arguments() {
        let OcrCli::Run(options) = parse_cli(["shot.png", "--json", "--sidecar"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.path, PathBuf::from("shot.png"));
        assert!(options.json);
        assert!(options.sidecar);
        assert!(!options.text);

        let OcrCli::Run(options) = parse_cli(["--text", "shot.png"]).unwrap() else {
            panic!("expected run");
        };
        assert!(options.text);
        assert_eq!(parse_cli(["--help"]).unwrap(), OcrCli::Help);

        for args in [
            vec![],
            vec!["a.png", "b.png"],
            vec!["a.png", "--json", "--text"],
            vec!["--bogus", "a.png"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn fake_tesseract_returns_text_and_writes_a_sidecar() {
        let dir = temp_dir("fake");
        let image = dir.join("shot.png");
        std::fs::write(&image, b"png").unwrap();
        let tesseract = script(&dir, "tesseract", "#!/bin/sh\nprintf 'hello\\nworld\\n'\n");
        let result = recognize_retry(&image, tesseract.to_str().unwrap()).unwrap();
        assert_eq!(result.text, "hello\nworld\n");
        assert_eq!(result.lines, vec!["hello", "world"]);

        let sidecar = result.write_sidecar(&image).unwrap();
        assert_eq!(sidecar, dir.join("shot.png.ocr.json"));
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(value["engine"], "tesseract");
        assert_eq!(value["text"], "hello\nworld\n");
        assert_eq!(value["image"], image.display().to_string());
    }

    #[test]
    fn missing_tesseract_degrades_with_an_install_hint() {
        let dir = temp_dir("missing");
        let image = dir.join("shot.png");
        std::fs::write(&image, b"png").unwrap();
        let err = recognize_retry(&image, "loom-shot-definitely-missing-tesseract").unwrap_err();
        assert!(err.contains("install tesseract"), "{err}");
        assert!(err.contains("LOOM_SHOT_TESSERACT"), "{err}");
    }

    #[test]
    fn non_zero_exit_includes_stderr() {
        let dir = temp_dir("failure");
        let image = dir.join("shot.png");
        std::fs::write(&image, b"png").unwrap();
        let tesseract = script(
            &dir,
            "tesseract",
            "#!/bin/sh\necho 'bad image' >&2\nexit 3\n",
        );
        let err = recognize_retry(&image, tesseract.to_str().unwrap()).unwrap_err();
        assert!(err.contains("exited with"), "{err}");
        assert!(err.contains("bad image"), "{err}");
    }

    #[test]
    fn rejects_a_missing_image_before_running_tesseract() {
        let dir = temp_dir("no-image");
        let err = recognize_with(&dir.join("nope.png"), "tesseract").unwrap_err();
        assert!(err.contains("is not a file"), "{err}");
    }
}
