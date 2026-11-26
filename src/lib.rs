//! `loom-shot`: screenshot capture for niri with optional clipboard,
//! annotation, HDR handling, output formats, history, OCR, pins and metadata.
//!
//! Modes: `ui` (niri's screenshot UI), `screen` (focused output), `window`
//! (focused or explicit window). Screenshots are saved to a generated path and
//! can be piped to `wl-copy`, annotated non-interactively, processed through
//! OCR, stitched across outputs, or opened in `satty` (all overridable).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::RgbaImage;
use niri_ipc::{Action, Request, Response};

pub mod annotate;
pub mod cli;
pub mod editor;
pub mod hdr;
pub mod history;
pub mod log;
pub mod metadata;
pub mod ocr;
pub mod output;
pub mod pin;
pub mod pin_view;
pub mod portal;
pub mod record;
pub mod script;
pub mod stitch;

use annotate::Annotation;
use hdr::{DEFAULT_REFERENCE_LUMINANCE, HdrDetection, HdrRequest};
use metadata::CaptureMetadata;
use ocr::OcrResult;
use output::OutputFormat;
use stitch::StitchLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ui,
    Screen,
    Window,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Screen => "screen",
            Self::Window => "window",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub mode: Mode,
    pub path: Option<PathBuf>,
    pub show_pointer: bool,
    pub window_id: Option<u64>,
    pub copy: bool,
    /// Open the final image in satty (interactive).
    pub annotate: bool,
    /// Built-in, non-interactive annotations.
    pub annotations: Vec<Annotation>,
    /// JSON annotation scripts, applied after inline annotations.
    pub annotation_files: Vec<PathBuf>,
    pub hdr: HdrRequest,
    pub format: Option<OutputFormat>,
    pub quality: u8,
    pub metadata: bool,
    /// Capture these outputs instead of the focused one (`screen` only).
    pub outputs: Vec<String>,
    /// Stitch multi-output captures into one image.
    pub stitch: Option<StitchLayout>,
    pub stitch_path: Option<PathBuf>,
    /// Run tesseract on each saved image.
    pub ocr: bool,
    /// Write `<image>.ocr.json` next to each OCR result.
    pub ocr_sidecar: bool,
    /// Do not append to capture history.
    pub no_history: bool,
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Ui,
            path: None,
            show_pointer: true,
            window_id: None,
            copy: false,
            annotate: false,
            annotations: Vec::new(),
            annotation_files: Vec::new(),
            hdr: HdrRequest::Keep,
            format: None,
            quality: 90,
            metadata: false,
            outputs: Vec::new(),
            stitch: None,
            stitch_path: None,
            ocr: false,
            ocr_sidecar: false,
            no_history: false,
            dry_run: false,
        }
    }
}

impl Options {
    /// Inline annotations followed by each JSON file, in command-line order.
    pub fn resolved_annotations(&self) -> Result<Vec<Annotation>, String> {
        let mut annotations = self.annotations.clone();
        for file in &self.annotation_files {
            annotations.extend(script::load_file(file)?);
        }
        Ok(annotations)
    }
}

/// One saved file from a capture (one output, or the stitched result).
#[derive(Debug, Clone)]
pub struct CaptureFile {
    pub path: PathBuf,
    pub output: Option<String>,
    pub format: OutputFormat,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_color: Option<&'static str>,
    pub hdr: HdrDetection,
    pub metadata_path: Option<PathBuf>,
    pub ocr: Option<OcrResult>,
}

#[derive(Debug)]
pub struct Outcome {
    /// Primary file: the stitched image when stitching, otherwise the first capture.
    pub path: PathBuf,
    pub copied: bool,
    pub annotated: bool,
    pub action: Action,
    pub actions: Vec<Action>,
    pub format: OutputFormat,
    pub hdr: HdrDetection,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_color: Option<&'static str>,
    pub metadata_path: Option<PathBuf>,
    pub captures: Vec<CaptureFile>,
    pub stitched: Option<PathBuf>,
    pub ocr: Option<OcrResult>,
    pub history_path: Option<PathBuf>,
}

/// Screenshots directory: `$XDG_PICTURES_DIR/Screenshots`, then
/// `$HOME/Pictures/Screenshots`, then the temp dir.
pub fn default_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_PICTURES_DIR") {
        return PathBuf::from(dir).join("Screenshots");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Pictures/Screenshots");
    }
    std::env::temp_dir()
}

/// Generates `loom-shot-<epoch>.png` for the given time.
pub fn generated_path(unix_secs: u64) -> PathBuf {
    generated_path_with_format(unix_secs, OutputFormat::Png)
}

/// Generates `loom-shot-<epoch>.<ext>` for the chosen format.
pub fn generated_path_with_format(unix_secs: u64, format: OutputFormat) -> PathBuf {
    default_dir().join(format!("loom-shot-{unix_secs}.{}", format.extension()))
}

pub fn build_action(options: &Options, path: &Path) -> Action {
    let path = path.to_string_lossy().into_owned();
    match options.mode {
        Mode::Ui => Action::Screenshot {
            show_pointer: options.show_pointer,
            stdout: false,
            path: Some(path),
        },
        Mode::Screen => Action::ScreenshotScreen {
            write_to_disk: true,
            show_pointer: options.show_pointer,
            stdout: false,
            path: Some(path),
        },
        Mode::Window => Action::ScreenshotWindow {
            id: options.window_id,
            write_to_disk: true,
            show_pointer: options.show_pointer,
            stdout: false,
            path: Some(path),
        },
    }
}

pub(crate) fn send(socket: &Path, request: Request) -> Result<Response, String> {
    use niri_ipc::socket::Socket;
    let mut socket =
        Socket::connect_to(socket).map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    let reply = socket
        .send(request)
        .map_err(|err| format!("niri IPC error: {err}"))?;
    reply.map_err(|err| format!("niri returned an error: {err}"))
}

fn run_program(program: &str, args: &[String]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|err| format!("cannot run {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn copy_png_to_clipboard(bytes: &[u8]) -> Result<(), String> {
    let program = std::env::var("LOOM_SHOT_CLIPBOARD").unwrap_or_else(|_| "wl-copy".to_owned());
    let mut child = Command::new(&program)
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|err| format!("cannot run {program}: {err}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| "clipboard stdin unavailable".to_owned())?
        .write_all(bytes)
        .map_err(|err| format!("cannot write to {program}: {err}"))?;
    let status = child
        .wait()
        .map_err(|err| format!("cannot wait for {program}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn annotate(path: &Path) -> Result<(), String> {
    let program = std::env::var("LOOM_SHOT_ANNOTATE").unwrap_or_else(|_| "satty".to_owned());
    run_program(&program, &["-f".to_owned(), path.display().to_string()])
}

fn detect_hdr(socket: &Path) -> HdrDetection {
    match send(socket, Request::FocusedOutput) {
        Ok(Response::FocusedOutput(Some(output))) => {
            HdrDetection::for_output(Some(output.name), output.max_bpc)
        }
        Ok(Response::FocusedOutput(None)) => HdrDetection::default(),
        Ok(other) => HdrDetection {
            config_error: Some(format!(
                "unexpected niri response for focused output: {other:?}"
            )),
            ..HdrDetection::default()
        },
        Err(err) => HdrDetection {
            config_error: Some(err),
            ..HdrDetection::default()
        },
    }
}

/// niri writes the capture asynchronously after replying, so wait until the
/// file exists and its size has stopped changing before reading it.
fn wait_for_file(path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_size = None;
    let mut stable_checks = 0;
    loop {
        if let Ok(meta) = std::fs::metadata(path)
            && meta.is_file()
            && meta.len() > 0
        {
            if last_size == Some(meta.len()) {
                stable_checks += 1;
                if stable_checks >= 2 {
                    return Ok(());
                }
            } else {
                last_size = Some(meta.len());
                stable_checks = 0;
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for niri to write {}",
                path.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn discard(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .map_err(|err| format!("cannot resolve relative path: {err}"))?
            .join(path))
    }
}

fn sanitize_output(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// Captures one file, applies HDR/annotation/format post-processing, writes
/// metadata and OCR sidecars, and leaves clipboard/satty to the caller.
#[allow(clippy::too_many_arguments)]
fn capture_file(
    socket: &Path,
    options: &Options,
    action: Action,
    path: &Path,
    format: OutputFormat,
    hdr: HdrDetection,
    timestamp: u64,
    annotations: &[Annotation],
    output_name: Option<String>,
) -> Result<CaptureFile, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    match send(socket, Request::Action(action.clone()))? {
        Response::Handled => (),
        other => return Err(format!("unexpected niri response: {other:?}")),
    }

    let tonemap = options.hdr == HdrRequest::Tonemap;
    let rewrite = format != OutputFormat::Png || !annotations.is_empty() || tonemap;
    let read_image = rewrite
        || options.hdr == HdrRequest::Sdr
        || options.metadata
        || options.ocr
        || options.copy
        || options.annotate;

    let mut decoded = None;
    let mut source_color = None;
    if read_image {
        wait_for_file(path)?;
        let image = output::load(path).inspect_err(|_| discard(path))?;
        source_color = Some(output::color_name(&image));
        if options.hdr == HdrRequest::Sdr && output::is_hdr_source(&image) {
            discard(path);
            return Err(
                "capture is high-bit-depth (HDR); use --hdr=tonemap to convert it to SDR"
                    .to_owned(),
            );
        }
        decoded = Some(image);
    }

    let mut processed: Option<RgbaImage> = None;
    if tonemap {
        let image = decoded
            .as_ref()
            .ok_or_else(|| "capture image was not read".to_owned())?;
        let reference_luminance = hdr
            .config
            .as_ref()
            .and_then(|config| config.reference_luminance)
            .unwrap_or(DEFAULT_REFERENCE_LUMINANCE);
        processed = match hdr::tonemap_to_sdr(image, reference_luminance) {
            Ok(image) => Some(image),
            Err(err) => {
                discard(path);
                return Err(err);
            }
        };
    }

    if !annotations.is_empty() {
        let mut canvas = match processed.take() {
            Some(canvas) => canvas,
            None => decoded
                .as_ref()
                .ok_or_else(|| "capture image was not read".to_owned())?
                .to_rgba8(),
        };
        for annotation in annotations {
            canvas = match annotation.apply(canvas) {
                Ok(canvas) => canvas,
                Err(err) => {
                    discard(path);
                    return Err(err);
                }
            };
        }
        processed = Some(canvas);
    }

    if rewrite && processed.is_none() {
        processed = Some(
            decoded
                .as_ref()
                .ok_or_else(|| "capture image was not read".to_owned())?
                .to_rgba8(),
        );
    }

    let dimensions = processed
        .as_ref()
        .map(|image| (image.width(), image.height()))
        .or_else(|| {
            decoded
                .as_ref()
                .map(|image| (image.width(), image.height()))
        });

    if rewrite {
        let image = processed
            .as_ref()
            .ok_or_else(|| "processed image missing".to_owned())?;
        let bytes = match format.encode(image, options.quality) {
            Ok(bytes) => bytes,
            Err(err) => {
                discard(path);
                return Err(err);
            }
        };
        if let Err(err) = std::fs::write(path, bytes) {
            discard(path);
            return Err(format!("cannot write {}: {err}", path.display()));
        }
    }

    let metadata_path = if options.metadata {
        let (width, height) =
            dimensions.ok_or_else(|| "image dimensions unavailable".to_owned())?;
        let reference_luminance = if tonemap {
            Some(
                hdr.config
                    .as_ref()
                    .and_then(|config| config.reference_luminance)
                    .unwrap_or(DEFAULT_REFERENCE_LUMINANCE),
            )
        } else {
            hdr.config
                .as_ref()
                .and_then(|config| config.reference_luminance)
        };
        let metadata = CaptureMetadata {
            mode: options.mode.name(),
            window_id: options.window_id,
            timestamp_unix: timestamp,
            output: path.to_owned(),
            format: format.name(),
            width,
            height,
            source_color: source_color.unwrap_or("unknown"),
            hdr: hdr.clone(),
            hdr_request: options.hdr,
            reference_luminance,
            annotations: annotations.iter().map(Annotation::describe).collect(),
            copied: options.copy,
            dry_run: false,
            command: std::env::args_os()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        };
        Some(metadata.write()?)
    } else {
        None
    };

    let ocr = if options.ocr {
        let result = ocr::recognize(path)?;
        if options.ocr_sidecar {
            result.write_sidecar(path)?;
        }
        Some(result)
    } else {
        None
    };

    Ok(CaptureFile {
        path: path.to_owned(),
        output: output_name,
        format,
        width: dimensions.map(|dims| dims.0),
        height: dimensions.map(|dims| dims.1),
        source_color,
        hdr,
        metadata_path,
        ocr,
    })
}

fn copy_final(path: &Path, format: OutputFormat) -> Result<(), String> {
    // Always put a PNG on the clipboard, whatever the file format is.
    let bytes = if format == OutputFormat::Png {
        std::fs::read(path).map_err(|err| format!("cannot read screenshot: {err}"))?
    } else {
        let image = output::load(path)?;
        OutputFormat::Png.encode(&image.to_rgba8(), 100)?
    };
    copy_png_to_clipboard(&bytes)
}

/// Captures a screenshot and runs the requested post-processing.
pub fn run(socket: &Path, options: &Options) -> Result<Outcome, String> {
    run_inner(socket, options, None)
}

/// `history_path = Some(file)` is used by tests; `None` means the default
/// state-directory history file (unless `--no-history` is set).
fn run_inner(
    socket: &Path,
    options: &Options,
    history_path: Option<PathBuf>,
) -> Result<Outcome, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let annotations = options.resolved_annotations()?;
    if options.outputs.is_empty() {
        run_single(socket, options, timestamp, &annotations, history_path)
    } else {
        run_multi(socket, options, timestamp, &annotations, history_path)
    }
}

fn run_single(
    socket: &Path,
    options: &Options,
    timestamp: u64,
    annotations: &[Annotation],
    history_path: Option<PathBuf>,
) -> Result<Outcome, String> {
    let format = options
        .format
        .or_else(|| options.path.as_deref().and_then(OutputFormat::from_path))
        .unwrap_or(OutputFormat::Png);
    let path = options
        .path
        .clone()
        .unwrap_or_else(|| generated_path_with_format(timestamp, format));
    let path = absolute(path)?;

    let action = build_action(options, &path);
    let hdr = if options.dry_run {
        HdrDetection::default()
    } else {
        detect_hdr(socket)
    };

    if options.dry_run {
        return Ok(Outcome {
            path: path.clone(),
            copied: false,
            annotated: false,
            action: action.clone(),
            actions: vec![action.clone()],
            format,
            hdr: hdr.clone(),
            width: None,
            height: None,
            source_color: None,
            metadata_path: None,
            captures: vec![CaptureFile {
                path,
                output: None,
                format,
                width: None,
                height: None,
                source_color: None,
                hdr,
                metadata_path: None,
                ocr: None,
            }],
            stitched: None,
            ocr: None,
            history_path: None,
        });
    }

    let capture = capture_file(
        socket,
        options,
        action.clone(),
        &path,
        format,
        hdr.clone(),
        timestamp,
        annotations,
        None,
    )?;
    let mut history_file = None;
    if !options.no_history {
        history_file = record_history(
            history_path.as_deref(),
            &capture,
            options.mode.name().to_owned(),
            timestamp,
            options.hdr,
            annotations,
            options.metadata,
        );
    }

    if options.copy {
        copy_final(&capture.path, capture.format)?;
    }
    if options.annotate {
        annotate(&capture.path)?;
    }

    Ok(Outcome {
        path: capture.path.clone(),
        copied: options.copy,
        annotated: options.annotate,
        action,
        actions: vec![build_action(options, &capture.path)],
        format: capture.format,
        hdr: capture.hdr.clone(),
        width: capture.width,
        height: capture.height,
        source_color: capture.source_color,
        metadata_path: capture.metadata_path.clone(),
        captures: vec![capture.clone()],
        stitched: None,
        ocr: capture.ocr.clone(),
        history_path: history_file,
    })
}

fn run_multi(
    socket: &Path,
    options: &Options,
    timestamp: u64,
    annotations: &[Annotation],
    history_path: Option<PathBuf>,
) -> Result<Outcome, String> {
    if options.mode != Mode::Screen {
        return Err(
            "--output is only supported with `screen`; niri's IPC cannot target outputs \
                    while using the UI or a window"
                .to_owned(),
        );
    }
    if options
        .outputs
        .iter()
        .any(|output| output.trim().is_empty())
    {
        return Err("--output needs a non-empty output name".to_owned());
    }
    let mut sanitized = Vec::with_capacity(options.outputs.len());
    for output in &options.outputs {
        let name = sanitize_output(output.trim());
        if name.is_empty() {
            return Err(format!(
                "output name `{output}` has no usable file characters"
            ));
        }
        if name == "." || name == ".." || sanitized.contains(&name) {
            return Err(format!(
                "outputs `{output}` collide or resolve to unsafe filenames; rename one"
            ));
        }
        sanitized.push(name);
    }
    let format = options
        .format
        .or_else(|| options.path.as_deref().and_then(OutputFormat::from_path))
        .or_else(|| {
            options
                .stitch_path
                .as_deref()
                .and_then(OutputFormat::from_path)
        })
        .unwrap_or(OutputFormat::Png);
    if options.stitch.is_some() && options.outputs.len() < 2 {
        return Err("--stitch needs at least two --output values".to_owned());
    }
    if options.stitch_path.is_some() && options.stitch.is_none() {
        return Err("--stitch-path needs --stitch".to_owned());
    }
    if options.stitch.is_none() && options.outputs.len() > 1 && options.copy {
        return Err(
            "--copy with multiple outputs needs --stitch (choose which image to copy)".to_owned(),
        );
    }
    if options.stitch.is_none() && options.outputs.len() > 1 && options.annotate {
        return Err(
            "--annotate with multiple outputs needs --stitch (satty edits one image)".to_owned(),
        );
    }

    let template = options.path.clone();
    let paths = if let Some(template) = &template {
        let text = template.to_string_lossy();
        if options.outputs.len() > 1 && !text.contains("{output}") {
            return Err(
                "--path needs a `{output}` placeholder when capturing several outputs".to_owned(),
            );
        }
        let solved = options
            .outputs
            .iter()
            .zip(&sanitized)
            .map(|(_output, name)| {
                if text.contains("{output}") {
                    PathBuf::from(text.replace("{output}", name))
                } else {
                    template.clone()
                }
            });
        solved.collect::<Vec<_>>()
    } else {
        options
            .outputs
            .iter()
            .zip(&sanitized)
            .map(|(_, name)| {
                default_dir().join(format!(
                    "loom-shot-{timestamp}-{name}.{}",
                    format.extension()
                ))
            })
            .collect()
    };
    let paths = paths
        .into_iter()
        .map(absolute)
        .collect::<Result<Vec<_>, _>>()?;
    let stitched_path = if options.stitch.is_some() {
        Some(absolute(options.stitch_path.clone().unwrap_or_else(
            || {
                default_dir().join(format!(
                    "loom-shot-{timestamp}-stitched.{}",
                    format.extension()
                ))
            },
        ))?)
    } else {
        None
    };

    if options.dry_run {
        let captures = paths
            .iter()
            .zip(&options.outputs)
            .map(|(path, output)| CaptureFile {
                path: path.clone(),
                output: Some(output.clone()),
                format,
                width: None,
                height: None,
                source_color: None,
                hdr: HdrDetection::default(),
                metadata_path: None,
                ocr: None,
            })
            .collect::<Vec<_>>();
        let mut actions = Vec::new();
        for (path, output) in paths.iter().zip(&options.outputs) {
            actions.push(Action::FocusMonitor {
                output: output.clone(),
            });
            actions.push(Action::ScreenshotScreen {
                write_to_disk: true,
                show_pointer: options.show_pointer,
                stdout: false,
                path: Some(path.to_string_lossy().into_owned()),
            });
        }
        let action = actions
            .iter()
            .find(|action| matches!(action, Action::ScreenshotScreen { .. }))
            .cloned()
            .unwrap_or(Action::ScreenshotScreen {
                write_to_disk: true,
                show_pointer: options.show_pointer,
                stdout: false,
                path: None,
            });
        return Ok(Outcome {
            path: stitched_path.clone().unwrap_or_else(|| paths[0].clone()),
            copied: false,
            annotated: false,
            action,
            actions,
            format,
            hdr: HdrDetection::default(),
            width: None,
            height: None,
            source_color: None,
            metadata_path: None,
            captures,
            stitched: stitched_path,
            ocr: None,
            history_path: None,
        });
    }

    let original_output = match send(socket, Request::FocusedOutput) {
        Ok(Response::FocusedOutput(Some(output))) => Some(output.name),
        _ => None,
    };

    let mut captures = Vec::with_capacity(paths.len());
    let mut actions = Vec::new();
    for (path, output) in paths.iter().zip(&options.outputs) {
        let focus = Action::FocusMonitor {
            output: output.clone(),
        };
        send(socket, Request::Action(focus.clone()))?;
        actions.push(focus);
        let hdr = detect_hdr(socket);
        let action = Action::ScreenshotScreen {
            write_to_disk: true,
            show_pointer: options.show_pointer,
            stdout: false,
            path: Some(path.to_string_lossy().into_owned()),
        };
        actions.push(action.clone());
        captures.push(capture_file(
            socket,
            options,
            action,
            path,
            format,
            hdr,
            timestamp,
            annotations,
            Some(output.clone()),
        )?);
    }
    if let Some(output) = &original_output {
        let restore = Action::FocusMonitor {
            output: output.clone(),
        };
        if let Err(err) = send(socket, Request::Action(restore.clone())) {
            eprintln!("loom-shot: could not restore focused output: {err}");
        } else {
            actions.push(restore);
        }
    }

    let mut history_files = Vec::new();
    if !options.no_history {
        for capture in &captures {
            let source = format!("screen:{}", capture.output.as_deref().unwrap_or("unknown"));
            if let Some(path) = record_history(
                history_path.as_deref(),
                capture,
                source,
                timestamp,
                options.hdr,
                annotations,
                options.metadata,
            ) {
                history_files.push(path);
            }
        }
    }

    let mut stitched = None;
    if let (Some(path), Some(layout)) = (&stitched_path, options.stitch) {
        let images = captures
            .iter()
            .map(|capture| output::load(&capture.path).map(|image| image.to_rgba8()))
            .collect::<Result<Vec<_>, _>>()?;
        let composed = stitch::compose(&images, layout)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        let bytes = format.encode(&composed, options.quality)?;
        std::fs::write(path, bytes)
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        let metadata_path = if options.metadata {
            let metadata = CaptureMetadata {
                mode: "stitch",
                window_id: None,
                timestamp_unix: timestamp,
                output: path.clone(),
                format: format.name(),
                width: composed.width(),
                height: composed.height(),
                source_color: "rgba8",
                hdr: captures
                    .first()
                    .map(|capture| capture.hdr.clone())
                    .unwrap_or_default(),
                hdr_request: options.hdr,
                reference_luminance: None,
                annotations: annotations.iter().map(Annotation::describe).collect(),
                copied: options.copy,
                dry_run: false,
                command: std::env::args_os()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            };
            Some(metadata.write()?)
        } else {
            None
        };
        let ocr = if options.ocr {
            let result = ocr::recognize(path)?;
            if options.ocr_sidecar {
                result.write_sidecar(path)?;
            }
            Some(result)
        } else {
            None
        };
        let stitched_capture = CaptureFile {
            path: path.clone(),
            output: Some("stitched".to_owned()),
            format,
            width: Some(composed.width()),
            height: Some(composed.height()),
            source_color: Some("rgba8"),
            hdr: captures
                .first()
                .map(|capture| capture.hdr.clone())
                .unwrap_or_default(),
            metadata_path,
            ocr,
        };
        if !options.no_history
            && let Some(history) = record_history(
                history_path.as_deref(),
                &stitched_capture,
                "stitch".to_owned(),
                timestamp,
                options.hdr,
                annotations,
                options.metadata,
            )
        {
            history_files.push(history);
        }
        stitched = Some(stitched_capture);
    }

    let primary = stitched.as_ref().unwrap_or(&captures[0]).clone();
    if options.copy {
        copy_final(&primary.path, primary.format)?;
    }
    if options.annotate {
        annotate(&primary.path)?;
    }
    let action = captures
        .first()
        .map(|capture| Action::ScreenshotScreen {
            write_to_disk: true,
            show_pointer: options.show_pointer,
            stdout: false,
            path: Some(capture.path.to_string_lossy().into_owned()),
        })
        .expect("at least one output");

    Ok(Outcome {
        path: primary.path.clone(),
        copied: options.copy,
        annotated: options.annotate,
        action,
        actions,
        format: primary.format,
        hdr: primary.hdr.clone(),
        width: primary.width,
        height: primary.height,
        source_color: primary.source_color,
        metadata_path: primary.metadata_path.clone(),
        ocr: primary.ocr.clone(),
        stitched: stitched.map(|capture| capture.path),
        captures,
        history_path: history_files.into_iter().next(),
    })
}

#[allow(clippy::too_many_arguments)]
fn record_history(
    history_path: Option<&Path>,
    capture: &CaptureFile,
    source: String,
    timestamp: u64,
    hdr_request: HdrRequest,
    annotations: &[Annotation],
    metadata: bool,
) -> Option<PathBuf> {
    let entry = history::HistoryEntry {
        path: capture.path.clone(),
        source,
        timestamp_unix: timestamp,
        width: capture.width,
        height: capture.height,
        format: Some(capture.format.name().to_owned()),
        hdr_requested: hdr_request.name().to_owned(),
        hdr_output: capture.hdr.output.clone(),
        max_bpc: capture.hdr.max_bpc.map(u32::from),
        annotations: annotations.iter().map(Annotation::describe).collect(),
        metadata: if metadata {
            capture.metadata_path.clone()
        } else {
            None
        },
    };
    let result = match history_path {
        Some(path) => history::append_to(path, entry).map(|()| path.to_owned()),
        None => history::append(entry),
    };
    match result {
        Ok(path) => Some(path),
        Err(err) => {
            eprintln!("loom-shot: capture saved but history was not recorded: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use niri_ipc::Request;
    use serde_json::json;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-shot-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn white_png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, image::Rgba([255, 255, 255, 255]));
        OutputFormat::Png.encode(&image, 100).unwrap()
    }

    fn fake_output() -> niri_ipc::Output {
        niri_ipc::Output {
            name: "TEST-OUTPUT-1".to_owned(),
            make: "Test".to_owned(),
            model: "Panel".to_owned(),
            serial: None,
            physical_size: None,
            modes: Vec::new(),
            current_mode: None,
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: None,
            max_bpc: Some(10),
            ..Default::default()
        }
    }

    /// Serves `connections` requests: reports a focused output and writes a
    /// synthetic PNG for every screenshot action.
    fn fake_niri(path: &Path, connections: usize) -> std::thread::JoinHandle<Vec<Request>> {
        fake_niri_with(path, connections, white_png(24, 18))
    }

    fn fake_niri_with(
        path: &Path,
        connections: usize,
        capture: Vec<u8>,
    ) -> std::thread::JoinHandle<Vec<Request>> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..connections {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(line.trim()).unwrap();
                let response = match &request {
                    Request::FocusedOutput => json!({
                        "Ok": Response::FocusedOutput(Some(fake_output()))
                    })
                    .to_string(),
                    Request::Action(
                        Action::Screenshot { path, .. }
                        | Action::ScreenshotScreen { path, .. }
                        | Action::ScreenshotWindow { path, .. },
                    ) => {
                        if let Some(path) = path {
                            std::fs::write(path, &capture).unwrap();
                        }
                        json!({"Ok": "Handled"}).to_string()
                    }
                    _ => json!({"Ok": "Handled"}).to_string(),
                };
                let mut stream = stream;
                stream.write_all(response.as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        })
    }

    fn run_test(socket: &Path, options: &Options, history: &Path) -> Result<Outcome, String> {
        run_inner(socket, options, Some(history.to_owned()))
    }

    #[test]
    fn builds_actions_for_each_mode() {
        let path = Path::new("/tmp/shot.png");
        let ui = build_action(&Options::default(), path);
        assert_eq!(
            serde_json::to_value(ui).unwrap(),
            json!({"Screenshot": {"show_pointer": true, "stdout": false, "path": "/tmp/shot.png"}})
        );

        let screen = build_action(
            &Options {
                mode: Mode::Screen,
                ..Options::default()
            },
            path,
        );
        assert!(matches!(screen, Action::ScreenshotScreen { .. }));

        let window = build_action(
            &Options {
                mode: Mode::Window,
                window_id: Some(7),
                show_pointer: false,
                ..Options::default()
            },
            path,
        );
        assert_eq!(
            serde_json::to_value(window).unwrap(),
            json!({"ScreenshotWindow": {"id": 7, "write_to_disk": true, "show_pointer": false, "stdout": false, "path": "/tmp/shot.png"}})
        );
    }

    #[test]
    fn generated_path_uses_the_selected_format() {
        let path = generated_path(1234);
        assert_eq!(path.file_name().unwrap(), "loom-shot-1234.png");
        assert!(path.to_string_lossy().contains("Screenshots"));
        assert_eq!(default_dir().file_name().unwrap(), "Screenshots");

        let webp = generated_path_with_format(99, OutputFormat::WebP);
        assert_eq!(webp.file_name().unwrap(), "loom-shot-99.webp");
    }

    #[test]
    fn sends_capture_to_niri_and_reports_hdr_detection() {
        let dir = temp_dir("niri");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &dir.join("history.json")).unwrap();
        assert_eq!(outcome.path, path);
        assert_eq!(outcome.format, OutputFormat::Png);
        assert_eq!(outcome.hdr.output.as_deref(), Some("TEST-OUTPUT-1"));
        assert_eq!(outcome.hdr.max_bpc, Some(10));
        let requests = server.join().unwrap();
        assert!(matches!(requests[0], Request::FocusedOutput));
        assert!(matches!(
            requests[1],
            Request::Action(Action::ScreenshotScreen { .. })
        ));
    }

    #[test]
    fn applies_annotations_and_writes_metadata() {
        let dir = temp_dir("annotate");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            annotations: vec![Annotation::Rect {
                x: 2,
                y: 3,
                w: 10,
                h: 8,
            }],
            metadata: true,
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &dir.join("history.json")).unwrap();
        server.join().unwrap();

        let image = output::load(&path).unwrap();
        assert_eq!((image.width(), image.height()), (24, 18));
        assert_eq!(
            image.to_rgba8().get_pixel(2, 3),
            &image::Rgba([255, 59, 48, 255])
        );
        assert_eq!(
            image.to_rgba8().get_pixel(6, 6),
            &image::Rgba([255, 255, 255, 255])
        );

        let sidecar = outcome.metadata_path.unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(sidecar).unwrap()).unwrap();
        assert_eq!(value["capture"]["mode"], "screen");
        assert_eq!(value["capture"]["output"], "TEST-OUTPUT-1");
        assert_eq!(value["file"]["format"], "png");
        assert_eq!(value["file"]["width"], 24);
        assert_eq!(value["annotations"][0], "rect:2,3,10,8");
        assert_eq!(value["hdr"]["requested"], "keep");
        assert_eq!(value["hdr"]["detected"]["max_bpc"], 10);
        assert_eq!(outcome.width, Some(24));
    }

    #[test]
    fn writes_jpeg_in_place() {
        let dir = temp_dir("jpeg");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.jpg");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            format: Some(OutputFormat::Jpeg),
            ..Options::default()
        };
        run_test(&socket, &options, &dir.join("history.json")).unwrap();
        server.join().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], b"\xff\xd8");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries
                .iter()
                .all(|name| !name.contains("loom-shot-capture")),
            "temporary capture left behind: {entries:?}"
        );
    }

    #[test]
    fn rejects_tonemap_for_eight_bit_niri_captures() {
        let dir = temp_dir("tonemap");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            hdr: HdrRequest::Tonemap,
            ..Options::default()
        };
        let err = run_test(&socket, &options, &dir.join("history.json")).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("niri tone-maps"), "{err}");
        assert!(!path.exists());
    }

    #[test]
    fn tone_maps_a_sixteen_bit_capture_end_to_end() {
        let dir = temp_dir("hdr16");
        let socket = dir.join("niri.sock");
        let source = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(
            2,
            1,
            vec![
                0, 0, 0, 0, // black
                65535, 65535, 65535, 65535, // PQ white
            ],
        )
        .unwrap();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba16(source)
            .write_to(std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let server = fake_niri_with(&socket, 2, png);

        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            hdr: HdrRequest::Tonemap,
            metadata: true,
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &dir.join("history.json")).unwrap();
        server.join().unwrap();

        let image = output::load(&path).unwrap();
        assert_eq!(output::color_name(&image), "rgba8");
        let rgba = image.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0), &image::Rgba([0, 0, 0, 0]));
        assert!(rgba.get_pixel(1, 0)[0] >= 245);
        let metadata: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(outcome.metadata_path.unwrap()).unwrap())
                .unwrap();
        assert_eq!(metadata["capture"]["source_color"], "rgba16");
        assert_eq!(metadata["hdr"]["requested"], "tonemap");
        assert_eq!(metadata["hdr"]["reference_luminance"], 203.0);
    }

    #[test]
    fn sdr_mode_accepts_niri_captures() {
        let dir = temp_dir("sdr");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            hdr: HdrRequest::Sdr,
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &dir.join("history.json")).unwrap();
        server.join().unwrap();
        assert_eq!(outcome.hdr.output.as_deref(), Some("TEST-OUTPUT-1"));
        assert!(path.exists());
    }

    #[test]
    fn copy_and_annotate_use_configured_commands() {
        let dir = temp_dir("hooks");
        let image = dir.join("shot.png");
        std::fs::write(&image, b"png-bytes").unwrap();

        let copy_script = dir.join("copy");
        std::fs::write(
            &copy_script,
            format!("#!/bin/sh\ncat > {}\n", dir.join("copied").display()),
        )
        .unwrap();
        std::fs::set_permissions(&copy_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: tests in this crate only mutate these variables locally.
        unsafe { std::env::set_var("LOOM_SHOT_CLIPBOARD", &copy_script) };
        copy_png_to_clipboard(b"png-bytes").unwrap();
        assert_eq!(std::fs::read(dir.join("copied")).unwrap(), b"png-bytes");

        let annotate_script = dir.join("annotate");
        std::fs::write(
            &annotate_script,
            format!(
                "#!/bin/sh\nprintf '%s' \"$2\" > {}\n",
                dir.join("annotated").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&annotate_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("LOOM_SHOT_ANNOTATE", &annotate_script) };
        annotate(&image).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("annotated")).unwrap(),
            image.display().to_string()
        );

        unsafe { std::env::remove_var("LOOM_SHOT_CLIPBOARD") };
        unsafe { std::env::remove_var("LOOM_SHOT_ANNOTATE") };
    }

    #[test]
    fn applies_annotation_files_and_redacts_the_saved_file() {
        let dir = temp_dir("annotation-file");
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 2);
        let script = dir.join("ops.json");
        std::fs::write(
            &script,
            r#"{"version":1,"ops":[
                {"op":"solid-redaction","x":0,"y":0,"w":8,"h":8},
                {"op":"blur","x":10,"y":10,"w":8,"h":8,"radius":2}
            ]}"#,
        )
        .unwrap();
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            annotation_files: vec![script],
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &dir.join("history.json")).unwrap();
        server.join().unwrap();

        let image = output::load(&path).unwrap().to_rgba8();
        assert_eq!(*image.get_pixel(3, 3), image::Rgba([0, 0, 0, 255]));
        assert_eq!(
            outcome.captures[0].path, path,
            "redaction is baked into the final file"
        );
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("history.json")).unwrap())
                .unwrap();
        assert_eq!(
            value["entries"][0]["annotations"][0],
            "solid-redaction:0,0,8,8,#000000"
        );
    }

    #[test]
    fn records_history_and_can_opt_out() {
        let dir = temp_dir("history");
        let socket = dir.join("niri.sock");
        let history = dir.join("history.json");

        let server = fake_niri(&socket, 2);
        let path = dir.join("shot.png");
        let options = Options {
            mode: Mode::Screen,
            path: Some(path.clone()),
            annotations: vec![Annotation::parse("spotlight:0,0,5,5,50").unwrap()],
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &history).unwrap();
        server.join().unwrap();
        assert_eq!(outcome.history_path.as_deref(), Some(history.as_path()));
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&history).unwrap()).unwrap();
        assert_eq!(value["entries"][0]["path"], path.display().to_string());
        assert_eq!(value["entries"][0]["source"], "screen");
        assert_eq!(
            value["entries"][0]["annotations"][0],
            "spotlight:0,0,5,5,50"
        );
        assert_eq!(value["entries"][0]["hdr"]["max_bpc"], 10);

        let socket = dir.join("niri2.sock");
        let server = fake_niri(&socket, 2);
        let options = Options {
            mode: Mode::Screen,
            path: Some(dir.join("second.png")),
            no_history: true,
            ..Options::default()
        };
        run_test(&socket, &options, &history).unwrap();
        server.join().unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&history).unwrap()).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn captures_and_stitches_multiple_outputs() {
        let dir = temp_dir("multi");
        let socket = dir.join("niri.sock");
        // FocusedOutput before the loop, 3 requests per output, then restore.
        let server = fake_niri(&socket, 1 + 2 * 3 + 1);
        let history = dir.join("history.json");
        let options = Options {
            mode: Mode::Screen,
            path: Some(dir.join("shot-{output}.png")),
            outputs: vec!["DP-1".to_owned(), "HDMI-A-1".to_owned()],
            stitch: Some(StitchLayout::Horizontal),
            stitch_path: Some(dir.join("stitched.png")),
            ..Options::default()
        };
        let outcome = run_test(&socket, &options, &history).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(outcome.captures.len(), 2);
        assert_eq!(outcome.captures[0].output.as_deref(), Some("DP-1"));
        assert_eq!(outcome.captures[1].output.as_deref(), Some("HDMI-A-1"));
        assert!(dir.join("shot-DP-1.png").exists());
        assert!(dir.join("shot-HDMI-A-1.png").exists());
        assert_eq!(
            outcome.stitched.as_deref(),
            Some(dir.join("stitched.png").as_path())
        );
        let stitched = output::load(&dir.join("stitched.png")).unwrap();
        assert_eq!((stitched.width(), stitched.height()), (48, 18));
        assert_eq!(outcome.path, dir.join("stitched.png"));
        assert_eq!(outcome.width, Some(48));
        assert!(matches!(requests[0], Request::FocusedOutput));
        assert!(requests.iter().any(|request| matches!(
            request,
            Request::Action(Action::FocusMonitor { output }) if output == "DP-1"
        )));
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&history).unwrap()).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 3);
        assert_eq!(value["entries"][2]["source"], "stitch");
    }

    #[test]
    fn multi_output_dry_run_plans_paths_without_niri() {
        let dir = temp_dir("multi-dry");
        let options = Options {
            mode: Mode::Screen,
            path: Some(dir.join("shot-{output}.webp")),
            outputs: vec!["DP-1".to_owned(), "DP-2".to_owned()],
            stitch: Some(StitchLayout::Vertical),
            dry_run: true,
            ..Options::default()
        };
        let outcome = run_test(Path::new("/nonexistent/socket"), &options, &dir.join("h")).unwrap();
        assert_eq!(outcome.captures.len(), 2);
        assert_eq!(outcome.captures[0].path, dir.join("shot-DP-1.webp"));
        assert_eq!(outcome.format, OutputFormat::WebP);
        let stitched = outcome.stitched.as_ref().unwrap();
        assert!(
            stitched.to_string_lossy().ends_with("-stitched.webp"),
            "unexpected stitch path {stitched:?}"
        );
        assert_eq!(outcome.actions.len(), 4);
    }

    #[test]
    fn multi_output_validation_is_explicit() {
        let dir = temp_dir("multi-invalid");
        let base = Options {
            mode: Mode::Screen,
            path: Some(dir.join("shot.png")),
            outputs: vec!["DP-1".to_owned(), "DP-2".to_owned()],
            dry_run: true,
            ..Options::default()
        };
        let err = run_test(Path::new("/nonexistent"), &base, &dir.join("h")).unwrap_err();
        assert!(err.contains("{output}"), "{err}");

        let err = run_test(
            Path::new("/nonexistent"),
            &Options {
                path: Some(dir.join("{output}.png")),
                copy: true,
                ..base.clone()
            },
            &dir.join("h"),
        )
        .unwrap_err();
        assert!(err.contains("--stitch"), "{err}");

        let err = run_test(
            Path::new("/nonexistent"),
            &Options {
                path: Some(dir.join("{output}.png")),
                stitch: Some(StitchLayout::Horizontal),
                outputs: vec!["DP-1".to_owned()],
                ..base.clone()
            },
            &dir.join("h"),
        )
        .unwrap_err();
        assert!(err.contains("at least two"), "{err}");

        let err = run_test(
            Path::new("/nonexistent"),
            &Options {
                mode: Mode::Window,
                outputs: vec!["DP-1".to_owned()],
                dry_run: true,
                ..Options::default()
            },
            &dir.join("h"),
        )
        .unwrap_err();
        assert!(err.contains("screen"), "{err}");
    }
}
