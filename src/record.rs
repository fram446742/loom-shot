//! Screen recording: backend selection, argument mapping and lifecycle.
//!
//! Recording is deliberately built out of existing tools instead of linking a
//! capture or PipeWire stack:
//!
//! * `wf-recorder` (wlr-screencopy) is preferred when it is installed. It records outputs and
//!   regions.
//! * the xdg-desktop-portal `ScreenCast` route negotiates a stream through the portal and hands its
//!   PipeWire node to `gst-launch-1.0` (x264/x265/av1), so no encoder or PipeWire client is linked
//!   in.
//! * a generic command template runs any recorder through `sh -c`. It is the escape hatch for
//!   portal-capable recorders, window capture and HDR tone-mapping filters.
//!
//! A running recording leaves a PID/state file in
//! `$XDG_STATE_HOME/loom-shot/recordings/` so `--stop` can find and signal it
//! even from another terminal. State files are pruned when the process is gone.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use niri_ipc::{Request, Response};
use serde_json::{Value, json};

use crate::send;

pub const USAGE: &str = r#"loom-shot record [options]

  Target (mutually exclusive, default: focused output via niri IPC)
    --output NAME       Record one output, e.g. DP-1
    --region x,y,w,h    Record a rectangle in compositor pixels
    --window ID|focused Record a window (generic command backend only;
                        `focused` uses niri's focused window)

  Recording:
    --fps N|auto        Constant frame rate 1-240 (default 60; auto = damage-driven)
    --codec CODEC       h264 | hevc | av1 | auto (default: auto)
    --format FORMAT     mp4 | mkv | webm (default: from --path, else mp4)
    --audio SOURCE      none (default) | default | DEVICE
                        sink:SINK (appends .monitor) | source:SOURCE
    --hdr MODE          keep (default) | tonemap
    --duration SECONDS  Stop automatically after this many seconds
    --path PATH         Output file (default: $XDG_VIDEOS_DIR/Recordings/
                        loom-shot-<epoch>.<ext>)
    --backend NAME      auto | wf-recorder | portal | command
    --command TEMPLATE  Generic backend command; requires {path}. Placeholders:
                        {path} {output} {region} {geometry} {window} {fps}
                        {codec} {encoder} {format} {audio} {hdr} {backend}
    --stop              Stop the newest running recording from its state file
    --json              Print a JSON report
    --dry-run           Print the planned recorder without starting it
    --socket PATH       niri socket ($NIRI_SOCKET)

  Environment:
    LOOM_SHOT_RECORD_COMMAND  Default command template
    LOOM_SHOT_WF_RECORDER     Explicit wf-recorder path
    XDG_STATE_HOME              PID/state files under loom-shot/recordings

  Backends:
    wf-recorder  Outputs and regions only. No window capture and no HDR
                 tone-map switch (wlr-screencopy has neither).
    command      Any recorder, run through sh -c with the placeholders above.
    portal       xdg-desktop-portal ScreenCast for outputs or windows. Source
                 selection happens in the portal dialog; the stream is encoded
                 with gst-launch-1.0 (x264/x265/av1). No audio.

  `--stop` accepts only --json, --dry-run and --socket.
"#;

pub const DEFAULT_FPS: u32 = 60;
const MAX_FPS: u32 = 240;
const STOP_INT_GRACE: Duration = Duration::from_secs(5);
const STOP_TERM_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_DURATION_SECONDS: f64 = 86_400.0;

/// A rectangle in compositor pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// Parses `x,y,w,h`. Width and height must be positive.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(',').map(str::trim);
        let x = parts.next()?.parse().ok()?;
        let y = parts.next()?.parse().ok()?;
        let w = parts.next()?.parse().ok()?;
        let h = parts.next()?.parse().ok()?;
        if parts.next().is_some() || w == 0 || h == 0 {
            return None;
        }
        Some(Self { x, y, w, h })
    }

    /// wf-recorder's `-g` syntax.
    pub fn wf_geometry(self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.w, self.h)
    }

    /// Compact `x,y,w,h` used in JSON, state files and `{region}`.
    pub fn compact(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.w, self.h)
    }
}

/// What the recorder should capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The output niri reports as focused; resolved at run time.
    FocusedOutput,
    Output(String),
    Region(Region),
    Window(u64),
    /// The window niri reports as focused; resolved at run time.
    FocusedWindow,
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Self::FocusedOutput => "focused output".to_owned(),
            Self::Output(name) => format!("output {name}"),
            Self::Region(region) => format!("region {}", region.compact()),
            Self::Window(id) => format!("window {id}"),
            Self::FocusedWindow => "focused window".to_owned(),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::FocusedOutput => json!({"kind": "focused-output"}),
            Self::Output(name) => json!({"kind": "output", "output": name}),
            Self::Region(region) => json!({
                "kind": "region",
                "x": region.x,
                "y": region.y,
                "width": region.w,
                "height": region.h,
            }),
            Self::Window(id) => json!({"kind": "window", "id": id}),
            Self::FocusedWindow => json!({"kind": "focused-window"}),
        }
    }
}

/// Video codec requested by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "h264" | "avc" | "x264" => Some(Self::H264),
            "hevc" | "h265" | "x265" => Some(Self::Hevc),
            "av1" => Some(Self::Av1),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }

    /// ffmpeg encoder name passed to wf-recorder's `-c`.
    pub fn encoder(self) -> &'static str {
        match self {
            Self::Auto | Self::H264 => "libx264",
            Self::Hevc => "libx265",
            Self::Av1 => "libaom-av1",
        }
    }

    /// Resolves `auto` and rejects combinations the container cannot carry.
    pub fn resolve(self, container: Container) -> Result<Self, String> {
        let resolved = match self {
            Self::Auto => match container {
                Container::WebM => Self::Av1,
                Container::Mp4 | Container::Mkv => Self::H264,
            },
            explicit => explicit,
        };
        if container == Container::WebM && resolved != Self::Av1 {
            return Err(format!(
                "{} in WebM is not supported; WebM needs AV1, use --codec=av1 or \
                 --codec=auto (or --format=mp4/mkv)",
                resolved.name()
            ));
        }
        Ok(resolved)
    }
}

/// Container written by the recorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
    WebM,
}

impl Container {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "mp4" => Some(Self::Mp4),
            "mkv" | "matroska" => Some(Self::Mkv),
            "webm" => Some(Self::WebM),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(OsStr::to_str)
            .and_then(Self::parse)
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::WebM => "webm",
        }
    }

    pub fn name(self) -> &'static str {
        self.extension()
    }
}

/// Audio capture request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audio {
    None,
    /// The recorder's default input (wf-recorder: `-a` without a device).
    Default,
    /// A specific source device name.
    Device(String),
}

impl Audio {
    /// Parses `none`, `default`, a bare device, `source:NAME` or `sink:NAME`.
    ///
    /// `sink:` follows PipeWire/PulseAudio's monitor-source convention and
    /// appends `.monitor` unless the name already has it.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match value.to_ascii_lowercase().as_str() {
            "none" | "off" => return Some(Self::None),
            "default" | "on" => return Some(Self::Default),
            _ => {}
        }
        if let Some(sink) = value.strip_prefix("sink:") {
            let sink = sink.trim();
            if sink.is_empty() {
                return None;
            }
            return Some(Self::Device(if sink.ends_with(".monitor") {
                sink.to_owned()
            } else {
                format!("{sink}.monitor")
            }));
        }
        if let Some(source) = value.strip_prefix("source:") {
            let source = source.trim();
            return (!source.is_empty()).then(|| Self::Device(source.to_owned()));
        }
        Some(Self::Device(value.to_owned()))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::Default => "default".to_owned(),
            Self::Device(device) => device.clone(),
        }
    }

    fn wf_recorder_args(&self) -> Vec<String> {
        match self {
            Self::None => Vec::new(),
            Self::Default => vec!["-a".to_owned()],
            Self::Device(device) => vec![format!("--audio={device}")],
        }
    }
}

/// HDR handling for recordings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordHdr {
    Keep,
    Tonemap,
}

impl RecordHdr {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "keep" => Some(Self::Keep),
            "tonemap" => Some(Self::Tonemap),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Tonemap => "tonemap",
        }
    }
}

/// Available recording backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    WfRecorder,
    Portal,
    Command,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::WfRecorder => "wf-recorder",
            Self::Portal => "portal",
            Self::Command => "command",
        }
    }
}

/// Backend override from the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Auto,
    WfRecorder,
    Portal,
    Command,
}

impl BackendChoice {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "wf-recorder" | "wf" | "wlr" => Some(Self::WfRecorder),
            "portal" | "pipewire" => Some(Self::Portal),
            "command" | "cmd" | "generic" => Some(Self::Command),
            _ => None,
        }
    }
}

/// Result of checking whether a backend can be used here.
#[derive(Debug, Clone)]
pub struct BackendProbe {
    pub kind: BackendKind,
    pub available: bool,
    pub detail: String,
    pub path: Option<PathBuf>,
}

/// Environment values that recording reads.
///
/// Keeping them in a struct makes backend selection and path generation
/// testable without mutating the process environment (and without racing
/// other tests).
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub path: Option<OsString>,
    pub wf_recorder: Option<PathBuf>,
    pub gst_launch: Option<PathBuf>,
    pub command: Option<String>,
    pub state_home: Option<PathBuf>,
    pub videos_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub niri_socket: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        Self {
            path: std::env::var_os("PATH"),
            wf_recorder: std::env::var_os("LOOM_SHOT_WF_RECORDER").map(PathBuf::from),
            gst_launch: std::env::var_os("LOOM_SHOT_GST_LAUNCH").map(PathBuf::from),
            command: std::env::var("LOOM_SHOT_RECORD_COMMAND")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            state_home: std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
            videos_dir: std::env::var_os("XDG_VIDEOS_DIR").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            niri_socket: std::env::var_os("NIRI_SOCKET").map(PathBuf::from),
        }
    }
}

/// Parsed `record` options.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordOptions {
    pub path: Option<PathBuf>,
    pub target: Target,
    /// `None` means damage-driven/variable frame rate.
    pub fps: Option<u32>,
    pub codec: Codec,
    /// `None` means infer from `--path`, else MP4.
    pub container: Option<Container>,
    pub audio: Audio,
    pub hdr: RecordHdr,
    pub duration: Option<Duration>,
    pub backend: BackendChoice,
    pub command: Option<String>,
    pub socket: Option<PathBuf>,
    pub stop: bool,
    pub json: bool,
    pub dry_run: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            path: None,
            target: Target::FocusedOutput,
            fps: Some(DEFAULT_FPS),
            codec: Codec::Auto,
            container: None,
            audio: Audio::None,
            hdr: RecordHdr::Keep,
            duration: None,
            backend: BackendChoice::Auto,
            command: None,
            socket: None,
            stop: false,
            json: false,
            dry_run: false,
        }
    }
}

impl RecordOptions {
    /// Effective container, checking `--path` against `--format`.
    pub fn resolve_container(&self) -> Result<Container, String> {
        let extension = self
            .path
            .as_deref()
            .and_then(Path::extension)
            .and_then(OsStr::to_str);
        let from_path = match extension {
            Some(extension) => Some(Container::parse(extension).ok_or_else(|| {
                format!("unsupported recording extension `.{extension}`; use mp4, mkv or webm")
            })?),
            None => None,
        };
        match (self.container, from_path) {
            (Some(chosen), Some(inferred)) if chosen != inferred => Err(format!(
                "--path ends in .{} but --format={} was requested",
                inferred.extension(),
                chosen.name()
            )),
            (Some(chosen), _) => Ok(chosen),
            (None, Some(inferred)) => Ok(inferred),
            (None, None) => Ok(Container::Mp4),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(fps) = self.fps
            && !(1..=MAX_FPS).contains(&fps)
        {
            return Err(format!("--fps needs 1-{MAX_FPS} or auto, got `{fps}`"));
        }
        if let Some(duration) = self.duration
            && duration.is_zero()
        {
            return Err("--duration needs a positive number of seconds".to_owned());
        }
        let container = self.resolve_container()?;
        self.codec.resolve(container)?;
        Ok(())
    }
}

/// Result of the record subcommand's parser.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordCli {
    Run(RecordOptions),
    Help,
}

/// Parses `record` arguments (without the `record` token itself).
pub fn parse_cli<I, S>(args: I) -> Result<RecordCli, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into).peekable();
    let mut options = RecordOptions::default();
    let mut target = None;
    let mut extra_flags = Vec::new();

    while let Some(raw) = iter.next() {
        let (flag, inline) = split_flag(raw);
        match flag.as_str() {
            "--help" | "-h" => return Ok(RecordCli::Help),
            "--path" => {
                extra_flags.push(flag.clone());
                options.path = Some(PathBuf::from(value(&flag, inline, &mut iter)?));
            }
            "--output" => {
                extra_flags.push(flag.clone());
                set_target(
                    &mut target,
                    Target::Output(value(&flag, inline, &mut iter)?),
                )?;
            }
            "--region" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                let region = Region::parse(&raw).ok_or_else(|| {
                    format!("--region needs x,y,w,h with positive size, got `{raw}`")
                })?;
                set_target(&mut target, Target::Region(region))?;
            }
            "--window" => {
                extra_flags.push(flag.clone());
                let raw = match inline {
                    Some(value) if value.is_empty() => {
                        return Err(format!("{flag} needs a window id or `focused`"));
                    }
                    Some(value) => value,
                    None => match iter.peek() {
                        Some(next) if !next.starts_with("--") => iter.next().unwrap(),
                        _ => "focused".to_owned(),
                    },
                };
                if raw.eq_ignore_ascii_case("focused") {
                    set_target(&mut target, Target::FocusedWindow)?;
                } else {
                    let id: u64 = raw.parse().map_err(|_| {
                        format!("--window needs a window id or `focused`, got `{raw}`")
                    })?;
                    set_target(&mut target, Target::Window(id))?;
                }
            }
            "--fps" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.fps = parse_fps(&raw)?;
            }
            "--codec" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.codec = Codec::parse(&raw)
                    .ok_or_else(|| format!("--codec needs h264, hevc, av1 or auto, got `{raw}`"))?;
            }
            "--format" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.container = Some(
                    Container::parse(&raw)
                        .ok_or_else(|| format!("--format needs mp4, mkv or webm, got `{raw}`"))?,
                );
            }
            "--audio" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.audio = Audio::parse(&raw).ok_or_else(|| {
                    format!(
                        "--audio needs none, default, a device, sink:SINK or source:SOURCE, \
                         got `{raw}`"
                    )
                })?;
            }
            "--hdr" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.hdr = RecordHdr::parse(&raw)
                    .ok_or_else(|| format!("--hdr needs keep or tonemap, got `{raw}`"))?;
            }
            "--duration" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.duration = Some(parse_duration(&raw)?);
            }
            "--backend" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.backend = BackendChoice::parse(&raw).ok_or_else(|| {
                    format!("--backend needs auto, wf-recorder, portal or command, got `{raw}`")
                })?;
            }
            "--command" => {
                extra_flags.push(flag.clone());
                let raw = value(&flag, inline, &mut iter)?;
                options.command = Some(raw);
            }
            "--stop" => {
                no_value(&flag, inline)?;
                options.stop = true;
            }
            "--json" => {
                no_value(&flag, inline)?;
                options.json = true;
            }
            "--dry-run" => {
                no_value(&flag, inline)?;
                options.dry_run = true;
            }
            "--socket" => {
                options.socket = Some(PathBuf::from(value(&flag, inline, &mut iter)?));
            }
            "--id" | "--copy" | "--annotate" | "--no-pointer" | "--pointer" | "--quality"
            | "--metadata" => {
                return Err(format!(
                    "`{flag}` is a screenshot option and does not apply to `record`\n\n{USAGE}"
                ));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown record option `{other}`\n\n{USAGE}"));
            }
            other => {
                return Err(format!("unexpected record argument `{other}`\n\n{USAGE}"));
            }
        }
    }

    if options.stop {
        if let Some(flag) = extra_flags.first() {
            return Err(format!(
                "--stop only accepts --json, --dry-run and --socket; `{flag}` is a recording \
                 option\n\n{USAGE}"
            ));
        }
    } else {
        options.validate()?;
    }
    if let Some(target) = target {
        options.target = target;
    }
    Ok(RecordCli::Run(options))
}

fn split_flag(raw: String) -> (String, Option<String>) {
    match raw.split_once('=') {
        Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
        None => (raw, None),
    }
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
    match iter.next() {
        Some(value) if !value.starts_with("--") => Ok(value),
        _ => Err(format!("{flag} needs a value")),
    }
}

fn no_value(flag: &str, inline: Option<String>) -> Result<(), String> {
    if inline.is_some() {
        Err(format!("{flag} does not take a value"))
    } else {
        Ok(())
    }
}

fn set_target(slot: &mut Option<Target>, target: Target) -> Result<(), String> {
    if slot.is_some() {
        return Err("--output, --region and --window are mutually exclusive".to_owned());
    }
    *slot = Some(target);
    Ok(())
}

fn parse_fps(raw: &str) -> Result<Option<u32>, String> {
    if raw.eq_ignore_ascii_case("auto") || raw == "0" {
        return Ok(None);
    }
    let fps: u32 = raw
        .parse()
        .map_err(|_| format!("--fps needs 1-{MAX_FPS} or auto, got `{raw}`"))?;
    if !(1..=MAX_FPS).contains(&fps) {
        return Err(format!("--fps needs 1-{MAX_FPS} or auto, got `{raw}`"));
    }
    Ok(Some(fps))
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let seconds: f64 = raw
        .parse()
        .map_err(|_| format!("--duration needs a number of seconds, got `{raw}`"))?;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > MAX_DURATION_SECONDS {
        return Err(format!(
            "--duration needs a number between 0 and {MAX_DURATION_SECONDS} seconds, got `{raw}`"
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// Probes every backend and explains why each one is or is not usable.
pub fn probe_backends(env: &Env, cli_command: Option<&str>) -> Vec<BackendProbe> {
    vec![
        probe_wf_recorder(env),
        probe_portal(env),
        probe_command(env, cli_command),
    ]
}

fn probe_wf_recorder(env: &Env) -> BackendProbe {
    if let Some(path) = &env.wf_recorder {
        return if is_executable(path) {
            BackendProbe {
                kind: BackendKind::WfRecorder,
                available: true,
                detail: format!("found via LOOM_SHOT_WF_RECORDER at {}", path.display()),
                path: Some(path.clone()),
            }
        } else {
            BackendProbe {
                kind: BackendKind::WfRecorder,
                available: false,
                detail: format!(
                    "LOOM_SHOT_WF_RECORDER points at {}, which is not an executable file",
                    path.display()
                ),
                path: None,
            }
        };
    }
    match find_in_path("wf-recorder", env) {
        Some(path) => BackendProbe {
            kind: BackendKind::WfRecorder,
            available: true,
            detail: format!("found at {}", path.display()),
            path: Some(path),
        },
        None => BackendProbe {
            kind: BackendKind::WfRecorder,
            available: false,
            detail: "not found in PATH; install wf-recorder to record without a template"
                .to_owned(),
            path: None,
        },
    }
}

fn probe_portal(env: &Env) -> BackendProbe {
    if let Some(path) = &env.gst_launch {
        return if is_executable(path) {
            BackendProbe {
                kind: BackendKind::Portal,
                available: true,
                detail: format!("found via LOOM_SHOT_GST_LAUNCH at {}", path.display()),
                path: Some(path.clone()),
            }
        } else {
            BackendProbe {
                kind: BackendKind::Portal,
                available: false,
                detail: format!(
                    "LOOM_SHOT_GST_LAUNCH points at {}, which is not an executable file",
                    path.display()
                ),
                path: None,
            }
        };
    }
    match find_in_path("gst-launch-1.0", env) {
        Some(path) => BackendProbe {
            kind: BackendKind::Portal,
            available: true,
            detail: format!(
                "gst-launch-1.0 found at {}; xdg-desktop-portal ScreenCast is negotiated at \
                 record time and needs a running portal and PipeWire",
                path.display()
            ),
            path: Some(path),
        },
        None => BackendProbe {
            kind: BackendKind::Portal,
            available: false,
            detail: "gst-launch-1.0 not found in PATH; the portal backend uses it to encode \
                     the portal's PipeWire stream (install gstreamer and its plugins)"
                .to_owned(),
            path: None,
        },
    }
}

fn probe_command(env: &Env, cli_command: Option<&str>) -> BackendProbe {
    if cli_command.is_some() {
        return BackendProbe {
            kind: BackendKind::Command,
            available: true,
            detail: "configured with --command".to_owned(),
            path: None,
        };
    }
    if env.command.is_some() {
        return BackendProbe {
            kind: BackendKind::Command,
            available: true,
            detail: "configured with LOOM_SHOT_RECORD_COMMAND".to_owned(),
            path: None,
        };
    }
    BackendProbe {
        kind: BackendKind::Command,
        available: false,
        detail: "no --command or LOOM_SHOT_RECORD_COMMAND template was provided".to_owned(),
        path: None,
    }
}

fn describe_probes(probes: &[BackendProbe]) -> String {
    probes
        .iter()
        .map(|probe| format!("  - {}: {}", probe.kind.name(), probe.detail))
        .collect::<Vec<_>>()
        .join("\n")
}

fn command_template<'a>(options: &'a RecordOptions, env: &'a Env) -> Option<&'a str> {
    options
        .command
        .as_deref()
        .or(env.command.as_deref())
        .map(str::trim)
        .filter(|template| !template.is_empty())
}

fn select_backend(
    env: &Env,
    options: &RecordOptions,
    probes: &[BackendProbe],
) -> Result<(BackendKind, PathBuf), String> {
    let wf = probes
        .iter()
        .find(|probe| probe.kind == BackendKind::WfRecorder)
        .expect("wf-recorder probe is always present");
    let portal = probes
        .iter()
        .find(|probe| probe.kind == BackendKind::Portal)
        .expect("portal probe is always present");
    let command = probes
        .iter()
        .find(|probe| probe.kind == BackendKind::Command)
        .expect("command probe is always present");

    match options.backend {
        BackendChoice::Auto => {
            if wf.available {
                Ok((
                    BackendKind::WfRecorder,
                    wf.path.clone().expect("available wf-recorder has a path"),
                ))
            } else if portal.available {
                Ok((
                    BackendKind::Portal,
                    portal.path.clone().expect("available portal has a path"),
                ))
            } else if command.available {
                Ok((BackendKind::Command, shell_path(env)?))
            } else {
                Err(format!(
                    "no recording backend is available; checked:\n{}\nInstall wf-recorder or \
                     pass --command TEMPLATE (LOOM_SHOT_RECORD_COMMAND also works)",
                    describe_probes(probes)
                ))
            }
        }
        BackendChoice::WfRecorder => {
            if wf.available {
                Ok((
                    BackendKind::WfRecorder,
                    wf.path.clone().expect("available wf-recorder has a path"),
                ))
            } else {
                Err(format!("wf-recorder is unavailable: {}", wf.detail))
            }
        }
        BackendChoice::Portal => {
            if portal.available {
                Ok((
                    BackendKind::Portal,
                    portal.path.clone().expect("available portal has a path"),
                ))
            } else {
                Err(format!(
                    "the portal backend is not available: {}\nInstall gst-launch-1.0 or use \
                     --backend=wf-recorder/--backend=command",
                    portal.detail
                ))
            }
        }
        BackendChoice::Command => {
            if command_template(options, env).is_none() {
                return Err("the command backend needs --command TEMPLATE or \
                     LOOM_SHOT_RECORD_COMMAND"
                    .to_owned());
            }
            Ok((BackendKind::Command, shell_path(env)?))
        }
    }
}

fn shell_path(env: &Env) -> Result<PathBuf, String> {
    let bin_sh = PathBuf::from("/bin/sh");
    if is_executable(&bin_sh) {
        return Ok(bin_sh);
    }
    find_in_path("sh", env)
        .ok_or_else(|| "cannot find a POSIX shell (`/bin/sh` or sh in PATH)".to_owned())
}

fn find_in_path(program: &str, env: &Env) -> Option<PathBuf> {
    let path = env
        .path
        .clone()
        .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Directory where recordings are generated.
pub fn recordings_dir(env: &Env) -> PathBuf {
    if let Some(dir) = &env.videos_dir {
        return dir.join("Recordings");
    }
    if let Some(home) = &env.home {
        return home.join("Videos/Recordings");
    }
    std::env::temp_dir().join("Recordings")
}

/// Generates `loom-shot-<epoch>.<ext>`.
pub fn generated_recording_path(env: &Env, unix_secs: u64, container: Container) -> PathBuf {
    recordings_dir(env).join(format!("loom-shot-{unix_secs}.{}", container.extension()))
}

/// `$XDG_STATE_HOME/loom-shot/recordings` (or the `~/.local/state` fallback).
pub fn state_dir(env: &Env) -> PathBuf {
    let base = env
        .state_home
        .clone()
        .or_else(|| env.home.as_ref().map(|home| home.join(".local/state")))
        .unwrap_or_else(|| std::env::temp_dir().join("loom-shot-state"));
    base.join("loom-shot/recordings")
}

/// A fully resolved recording invocation.
#[derive(Debug, Clone)]
pub struct Plan {
    pub backend: BackendKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub child_env: Vec<(&'static str, String)>,
    pub path: PathBuf,
    pub target: Target,
    pub fps: Option<u32>,
    pub codec: Codec,
    pub container: Container,
    pub audio: Audio,
    pub hdr: RecordHdr,
    pub resolved_output: Option<String>,
    /// Portal source kind when the backend is `portal`.
    pub portal_source: Option<crate::portal::Source>,
    pub state_dir: PathBuf,
    pub log_path: PathBuf,
}

impl Plan {
    pub fn command_line(&self) -> Vec<String> {
        let mut line = vec![self.program.display().to_string()];
        line.extend(self.args.iter().cloned());
        line
    }

    fn report(
        &self,
        pid: Option<u32>,
        duration_seconds: Option<f64>,
        stopped_by: Option<&'static str>,
        dry_run: bool,
    ) -> Report {
        Report {
            path: self.path.clone(),
            backend: self.backend,
            target: self.target.clone(),
            pid,
            fps: self.fps,
            codec: self.codec,
            container: self.container,
            audio: self.audio.clone(),
            hdr: self.hdr,
            duration_seconds,
            stopped_by,
            command: self.command_line(),
            dry_run,
        }
    }
}

/// Builds the resolved plan without starting anything.
pub fn build_plan(
    env: &Env,
    options: &RecordOptions,
    timestamp: u64,
    resolve_target: bool,
) -> Result<Plan, String> {
    options.validate()?;
    let container = options.resolve_container()?;
    let codec = options.codec.resolve(container)?;
    let probes = probe_backends(env, options.command.as_deref());
    let (backend, program) = select_backend(env, options, &probes)?;

    if backend == BackendKind::WfRecorder
        && matches!(options.target, Target::Window(_) | Target::FocusedWindow)
    {
        return Err(
            "wf-recorder cannot record a single window: wlr-screencopy has no per-window \
             capture. Use --output/--region, or --backend=command with a portal-capable \
             recorder"
                .to_owned(),
        );
    }
    if backend == BackendKind::WfRecorder && options.hdr == RecordHdr::Tonemap {
        return Err(
            "wf-recorder has no HDR tone-map switch: wlr-screencopy delivers the compositor's \
             pixels as-is. Record with --hdr=keep, or use --backend=command with a recorder or \
             ffmpeg filter that tone-maps"
                .to_owned(),
        );
    }
    if backend == BackendKind::Portal {
        if matches!(options.target, Target::Region(_)) {
            return Err(
                "the portal ScreenCast backend has no region source: the portal offers outputs \
                 and windows only. Use wf-recorder or --backend=command for regions"
                    .to_owned(),
            );
        }
        if options.audio != Audio::None {
            return Err(
                "the portal ScreenCast backend does not record audio: xdg-desktop-portal \
                 ScreenCast carries video only. Use wf-recorder for an output monitor source, or \
                 --backend=command with a recorder that handles audio"
                    .to_owned(),
            );
        }
        if options.hdr == RecordHdr::Tonemap {
            return Err(
                "the portal gst pipeline does not tone-map: record with --hdr=keep, or use \
                 --backend=command with an ffmpeg tone-mapping filter"
                    .to_owned(),
            );
        }
    }

    let socket = options.socket.clone().or_else(|| env.niri_socket.clone());
    let mut resolved_output = None;
    let mut portal_source = None;
    let mut resolved_window = match &options.target {
        Target::Window(id) => Some(*id),
        _ => None,
    };
    match &options.target {
        Target::FocusedOutput => {
            if let Some(socket) = &socket
                && resolve_target
            {
                resolved_output = Some(focused_output_name(socket)?);
            }
            if resolved_output.is_none() && resolve_target {
                return Err(
                    "cannot resolve the focused output: NIRI_SOCKET is not set. Pass \
                     --output NAME, --socket PATH, or set NIRI_SOCKET"
                        .to_owned(),
                );
            }
        }
        Target::Output(name) => resolved_output = Some(name.clone()),
        Target::Region(_) => {}
        Target::Window(id) => {
            if let Some(socket) = &socket
                && resolve_target
            {
                ensure_window_recordable(socket, *id)?;
            }
        }
        Target::FocusedWindow => {
            if resolve_target {
                let socket = socket.as_ref().ok_or_else(|| {
                    "cannot resolve the focused window: NIRI_SOCKET is not set. Pass \
                     --window ID, --socket PATH, or set NIRI_SOCKET"
                        .to_owned()
                })?;
                let window = focused_window(socket)?;
                if looks_like_niri_ui(&window) {
                    return Err(format!(
                        "refusing to record the focused window {}: it is niri's \
                         screenshot/screencast UI",
                        window
                            .title
                            .as_deref()
                            .or(window.app_id.as_deref())
                            .unwrap_or("untitled")
                    ));
                }
                resolved_window = Some(window.id);
            }
        }
    }

    let path = resolve_output_path(env, options, container, timestamp)?;
    let values = placeholder_values(
        &path,
        options,
        resolved_output.as_deref().unwrap_or(""),
        resolved_window,
        codec,
        container,
        backend,
    );

    let mut args = Vec::new();
    match backend {
        BackendKind::WfRecorder => {
            args.push("-f".to_owned());
            args.push(path.to_string_lossy().into_owned());
            if let Some(fps) = options.fps {
                args.push("-r".to_owned());
                args.push(fps.to_string());
            }
            args.push("-c".to_owned());
            args.push(codec.encoder().to_owned());
            match &options.target {
                Target::FocusedOutput | Target::Output(_) => {
                    let output = resolved_output.as_deref().unwrap_or("");
                    if output.is_empty() {
                        return Err(
                            "wf-recorder needs an output name, but the focused output could not \
                             be resolved; pass --output NAME"
                                .to_owned(),
                        );
                    }
                    args.push("-o".to_owned());
                    args.push(output.to_owned());
                }
                Target::Region(region) => {
                    args.push("-g".to_owned());
                    args.push(region.wf_geometry());
                }
                Target::Window(_) | Target::FocusedWindow => unreachable!("rejected above"),
            }
            args.extend(options.audio.wf_recorder_args());
        }
        BackendKind::Command => {
            let template = command_template(options, env)
                .expect("command backend is only selected with a template");
            args.push("-c".to_owned());
            args.push(render_template(template, &values)?);
        }
        BackendKind::Portal => {
            portal_source = Some(match &options.target {
                Target::FocusedOutput | Target::Output(_) => crate::portal::Source::Monitor,
                Target::FocusedWindow | Target::Window(_) => crate::portal::Source::Window,
                Target::Region(_) => unreachable!("rejected above"),
            });
            args = crate::portal::pipeline_args(
                &crate::portal::Stream::placeholder(),
                codec,
                container,
                options.fps,
                &path,
            );
        }
    }

    let state_dir = state_dir(env);
    let log_path = state_dir.join(format!("record-{}-{timestamp}.log", std::process::id()));

    Ok(Plan {
        backend,
        program,
        args,
        child_env: backend_env(&values),
        path,
        target: options.target.clone(),
        fps: options.fps,
        codec,
        container,
        audio: options.audio.clone(),
        hdr: options.hdr,
        resolved_output,
        portal_source,
        state_dir,
        log_path,
    })
}

fn resolve_output_path(
    env: &Env,
    options: &RecordOptions,
    container: Container,
    timestamp: u64,
) -> Result<PathBuf, String> {
    let Some(path) = &options.path else {
        return Ok(generated_recording_path(env, timestamp, container));
    };
    let path = if path.is_absolute() {
        path.clone()
    } else {
        std::env::current_dir()
            .map_err(|err| format!("cannot resolve relative --path: {err}"))?
            .join(path)
    };
    Ok(if path.extension().is_none() {
        path.with_extension(container.extension())
    } else {
        path
    })
}

fn focused_output_name(socket: &Path) -> Result<String, String> {
    match send(socket, Request::FocusedOutput)? {
        Response::FocusedOutput(Some(output)) => Ok(output.name),
        Response::FocusedOutput(None) => Err("niri reports no focused output".to_owned()),
        other => Err(format!(
            "unexpected niri response for focused output: {other:?}"
        )),
    }
}

fn focused_window(socket: &Path) -> Result<niri_ipc::Window, String> {
    match send(socket, Request::FocusedWindow)? {
        Response::FocusedWindow(Some(window)) => Ok(window),
        Response::FocusedWindow(None) => Err("niri reports no focused window".to_owned()),
        other => Err(format!(
            "unexpected niri response for focused window: {other:?}"
        )),
    }
}

/// Refuses windows that are niri's own screenshot/screencast UI.
///
/// Current niri renders the screenshot UI as an internal overlay rather than
/// an IPC-visible window, so this is defence in depth for builds or portals
/// that do expose it as a window.
fn ensure_window_recordable(socket: &Path, id: u64) -> Result<(), String> {
    let windows = match send(socket, Request::Windows)? {
        Response::Windows(windows) => windows,
        other => return Err(format!("unexpected niri response for windows: {other:?}")),
    };
    let Some(window) = windows.into_iter().find(|window| window.id == id) else {
        return Err(format!("window {id} is not mapped in niri"));
    };
    if looks_like_niri_ui(&window) {
        return Err(format!(
            "refusing to record window {id} ({}): it is niri's screenshot/screencast UI",
            window
                .title
                .as_deref()
                .or(window.app_id.as_deref())
                .unwrap_or("untitled")
        ));
    }
    Ok(())
}

fn looks_like_niri_ui(window: &niri_ipc::Window) -> bool {
    let app = window.app_id.as_deref().unwrap_or("").to_ascii_lowercase();
    let title = window.title.as_deref().unwrap_or("").to_ascii_lowercase();
    let niri_app = app == "niri" || app == "rs.bxt.niri" || app.starts_with("niri-");
    let ui_word = title.contains("screenshot")
        || title.contains("screen shot")
        || title.contains("screencast")
        || app.contains("screenshot");
    niri_app && ui_word
}

fn placeholder_values(
    path: &Path,
    options: &RecordOptions,
    output: &str,
    window_id: Option<u64>,
    codec: Codec,
    container: Container,
    backend: BackendKind,
) -> Vec<(&'static str, String)> {
    let region = match &options.target {
        Target::Region(region) => region.compact(),
        _ => String::new(),
    };
    let geometry = match &options.target {
        Target::Region(region) => region.wf_geometry(),
        _ => String::new(),
    };
    let window = window_id.map(|id| id.to_string()).unwrap_or_default();
    vec![
        ("path", path.to_string_lossy().into_owned()),
        ("output", output.to_owned()),
        ("region", region),
        ("geometry", geometry),
        ("window", window),
        (
            "fps",
            options.fps.map(|fps| fps.to_string()).unwrap_or_default(),
        ),
        ("codec", codec.name().to_owned()),
        ("encoder", codec.encoder().to_owned()),
        ("format", container.name().to_owned()),
        ("audio", options.audio.describe()),
        ("hdr", options.hdr.name().to_owned()),
        ("backend", backend.name().to_owned()),
    ]
}

/// Substitutes known `{placeholder}`s with shell-quoted values.
///
/// Unknown braces (for example shell's `${HOME}`) are left untouched. The
/// template must contain `{path}` so loom-shot knows where the file lands.
pub fn render_template(
    template: &str,
    values: &[(&'static str, String)],
) -> Result<String, String> {
    if !template.contains("{path}") {
        return Err(
            "the command template must contain {path} so the recording file is known".to_owned(),
        );
    }
    let mut rendered = template.to_owned();
    for (key, value) in values {
        rendered = rendered.replace(&format!("{{{key}}}"), &shell_quote(value));
    }
    Ok(rendered)
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn backend_env(values: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    values
        .iter()
        .map(|(key, value)| {
            let name = match *key {
                "path" => "LOOM_SHOT_RECORD_PATH",
                "output" => "LOOM_SHOT_RECORD_OUTPUT",
                "region" => "LOOM_SHOT_RECORD_REGION",
                "geometry" => "LOOM_SHOT_RECORD_GEOMETRY",
                "window" => "LOOM_SHOT_RECORD_WINDOW",
                "fps" => "LOOM_SHOT_RECORD_FPS",
                "codec" => "LOOM_SHOT_RECORD_CODEC",
                "encoder" => "LOOM_SHOT_RECORD_ENCODER",
                "format" => "LOOM_SHOT_RECORD_FORMAT",
                "audio" => "LOOM_SHOT_RECORD_AUDIO",
                "hdr" => "LOOM_SHOT_RECORD_HDR",
                "backend" => "LOOM_SHOT_RECORD_BACKEND",
                other => unreachable!("unexpected placeholder {other}"),
            };
            (name, value.clone())
        })
        .collect()
}

/// Final report for a started or planned recording.
#[derive(Debug, Clone)]
pub struct Report {
    pub path: PathBuf,
    pub backend: BackendKind,
    pub target: Target,
    pub pid: Option<u32>,
    pub fps: Option<u32>,
    pub codec: Codec,
    pub container: Container,
    pub audio: Audio,
    pub hdr: RecordHdr,
    pub duration_seconds: Option<f64>,
    pub stopped_by: Option<&'static str>,
    pub command: Vec<String>,
    pub dry_run: bool,
}

impl Report {
    pub fn to_json(&self) -> Value {
        json!({
            "tool": "loom-shot",
            "version": env!("CARGO_PKG_VERSION"),
            "recording": {
                "path": self.path,
                "backend": self.backend.name(),
                "target": self.target.to_json(),
                "pid": self.pid,
                "fps": self.fps,
                "codec": self.codec.name(),
                "encoder": self.codec.encoder(),
                "format": self.container.name(),
                "audio": self.audio.describe(),
                "hdr": self.hdr.name(),
                "duration_seconds": self.duration_seconds,
                "stopped_by": self.stopped_by,
                "command": self.command,
                "dry_run": self.dry_run,
                "stopped": false,
            }
        })
    }

    pub fn summary(&self) -> String {
        if self.dry_run {
            return format!(
                "Would record {} to {} with {}: {}",
                self.target.describe(),
                self.path.display(),
                self.backend.name(),
                self.command.join(" ")
            );
        }
        format!(
            "Recorded {} ({:.2}s, {}/{}, backend {}, pid {})",
            self.path.display(),
            self.duration_seconds.unwrap_or_default(),
            self.codec.name(),
            self.container.name(),
            self.backend.name(),
            self.pid.unwrap_or_default(),
        )
    }
}

/// Report for `--stop`.
#[derive(Debug, Clone)]
pub struct StopReport {
    pub path: PathBuf,
    pub pid: u32,
    pub duration_seconds: f64,
    pub dry_run: bool,
}

impl StopReport {
    pub fn to_json(&self) -> Value {
        json!({
            "tool": "loom-shot",
            "version": env!("CARGO_PKG_VERSION"),
            "recording": {
                "path": self.path,
                "pid": self.pid,
                "duration_seconds": self.duration_seconds,
                "dry_run": self.dry_run,
                "stopped": !self.dry_run,
            }
        })
    }

    pub fn summary(&self) -> String {
        if self.dry_run {
            format!(
                "Would stop recording {} (pid {}, {:.2}s)",
                self.path.display(),
                self.pid,
                self.duration_seconds
            )
        } else {
            format!(
                "Stopped recording {} (pid {}, {:.2}s)",
                self.path.display(),
                self.pid,
                self.duration_seconds
            )
        }
    }
}

/// Result of `loom-shot record`.
#[derive(Debug, Clone)]
pub enum RecordOutcome {
    Recorded(Report),
    Stopped(StopReport),
}

impl RecordOutcome {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Recorded(report) => report.to_json(),
            Self::Stopped(report) => report.to_json(),
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Recorded(report) => report.summary(),
            Self::Stopped(report) => report.summary(),
        }
    }
}

/// Runs the record subcommand (or `--stop`).
pub fn run(env: &Env, options: &RecordOptions) -> Result<RecordOutcome, String> {
    if options.stop {
        return Ok(RecordOutcome::Stopped(stop_recording(env, options)?));
    }
    let timestamp = unix_secs();
    let plan = build_plan(env, options, timestamp, !options.dry_run)?;
    if options.dry_run {
        return Ok(RecordOutcome::Recorded(plan.report(None, None, None, true)));
    }
    execute(options, plan)
}

fn execute(options: &RecordOptions, mut plan: Plan) -> Result<RecordOutcome, String> {
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    std::fs::create_dir_all(&plan.state_dir)
        .map_err(|err| format!("cannot create {}: {err}", plan.state_dir.display()))?;
    prune_stale(&plan.state_dir);

    let mut portal_session = None;
    if plan.backend == BackendKind::Portal {
        let source = plan
            .portal_source
            .ok_or_else(|| "the portal backend has no source kind".to_owned())?;
        let session = crate::portal::open(source)?;
        plan.args = crate::portal::pipeline_args(
            &session.stream,
            plan.codec,
            plan.container,
            plan.fps,
            &plan.path,
        );
        // Keep the portal session (and its PipeWire node) alive until the
        // recorder has been supervised and stopped.
        portal_session = Some(session);
    }

    let log = File::create(&plan.log_path)
        .map_err(|err| format!("cannot create {}: {err}", plan.log_path.display()))?;
    // Install handlers before spawning: otherwise a SIGINT disposition
    // inherited from a background shell would also be inherited by the
    // recorder, which may then ignore SIGINT.
    let _signals = SignalGuard::install();
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    for (key, value) in &plan.child_env {
        command.env(key, value);
    }
    apply_parent_death_signal(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = std::fs::remove_file(&plan.log_path);
            return Err(format!("cannot start {}: {err}", plan.program.display()));
        }
    };
    let pid = child.id();
    let state_path = plan.state_dir.join(format!("record-{pid}.json"));
    if let Err(err) = write_state(&state_path, &plan, pid) {
        let _ = send_signal(pid, libc::SIGINT);
        let _ = child.wait();
        let _ = std::fs::remove_file(&plan.log_path);
        return Err(err);
    }
    let _state_guard = StateGuard { path: state_path };

    let started = Instant::now();
    let (status, reason) = supervise(&mut child, options.duration, started)?;
    let elapsed = started.elapsed();
    let stderr = read_log_tail(&plan.log_path);

    if !status.success() {
        return Err(format_backend_failure(&plan, &status, &stderr));
    }
    if !file_has_content(&plan.path) {
        return Err(format!(
            "{} exited successfully but wrote no data to {}{} (log: {})",
            plan.backend.name(),
            plan.path.display(),
            stderr_suffix(&stderr),
            plan.log_path.display()
        ));
    }
    if let (Some(limit), ExitReason::Backend) = (options.duration, reason)
        && elapsed + Duration::from_millis(100) < limit
    {
        return Err(format!(
            "{} exited before --duration elapsed ({:.2}s of {:.2}s){} (log: {})",
            plan.backend.name(),
            elapsed.as_secs_f64(),
            limit.as_secs_f64(),
            stderr_suffix(&stderr),
            plan.log_path.display()
        ));
    }

    let stopped_by = match reason {
        ExitReason::Duration => "duration",
        ExitReason::Signal => "signal",
        ExitReason::Backend => {
            if status.signal().is_some() {
                "signal"
            } else {
                "backend"
            }
        }
    };
    drop(portal_session);
    let _ = std::fs::remove_file(&plan.log_path);
    Ok(RecordOutcome::Recorded(plan.report(
        Some(pid),
        Some(elapsed.as_secs_f64()),
        Some(stopped_by),
        false,
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    Duration,
    Signal,
    Backend,
}

fn supervise(
    child: &mut Child,
    duration: Option<Duration>,
    started: Instant,
) -> Result<(ExitStatus, ExitReason), String> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("cannot wait for recorder: {err}"))?
        {
            return Ok((status, ExitReason::Backend));
        }
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            stop_child(child)?;
            let status = child
                .wait()
                .map_err(|err| format!("cannot wait for recorder: {err}"))?;
            return Ok((status, ExitReason::Signal));
        }
        if let Some(limit) = duration
            && started.elapsed() >= limit
        {
            stop_child(child)?;
            let status = child
                .wait()
                .map_err(|err| format!("cannot wait for recorder: {err}"))?;
            return Ok((status, ExitReason::Duration));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn stop_child(child: &mut Child) -> Result<(), String> {
    let pid = child.id();
    send_signal(pid, libc::SIGINT)?;
    if wait_for_child(child, STOP_INT_GRACE)? {
        return Ok(());
    }
    send_signal(pid, libc::SIGTERM)?;
    if wait_for_child(child, STOP_TERM_GRACE)? {
        return Ok(());
    }
    send_signal(pid, libc::SIGKILL)?;
    wait_for_child(child, STOP_TERM_GRACE)?;
    Ok(())
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|err| format!("cannot wait for recorder: {err}"))?
            .is_some()
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn apply_parent_death_signal(command: &mut Command) {
    // SAFETY: `pre_exec` runs between fork and exec; `prctl` is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn file_has_content(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

fn format_backend_failure(plan: &Plan, status: &ExitStatus, stderr: &str) -> String {
    let mut message = format!("{} failed ({})", plan.backend.name(), status);
    if let Some(category) = classify_backend_error(stderr) {
        message.push_str(": ");
        message.push_str(category);
    }
    if let Some(signal) = status.signal() {
        message.push_str(&format!("; terminated by signal {signal}"));
    }
    message.push_str(&stderr_suffix(stderr));
    message.push_str(&format!(" (log: {})", plan.log_path.display()));
    message
}

fn stderr_suffix(stderr: &str) -> String {
    let tail = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ");
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    }
}

/// Maps recorder stderr to a user-actionable failure category.
pub fn classify_backend_error(stderr: &str) -> Option<&'static str> {
    let text = stderr.to_ascii_lowercase();
    if text.contains("permission denied")
        || text.contains("not authorized")
        || text.contains("access denied")
        || text.contains("portal")
    {
        Some(
            "screen recording permission was denied; accept the desktop portal prompt or fix \
             compositor permissions",
        )
    } else if text.contains("failed to connect")
        || text.contains("cannot connect")
        || text.contains("wayland")
        || text.contains("compositor")
    {
        Some("the recorder could not connect to the Wayland compositor")
    } else if text.contains("no such file") || text.contains("not found") {
        Some("a recorder dependency is missing")
    } else {
        None
    }
}

fn read_log_tail(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(metadata) = file.metadata() else {
        return String::new();
    };
    let length = metadata.len();
    let start = length.saturating_sub(4096);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    text
}

fn write_state(path: &Path, plan: &Plan, pid: u32) -> Result<(), String> {
    let value = json!({
        "tool": "loom-shot",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": pid,
        "path": plan.path,
        "target": plan.target.to_json(),
        "backend": plan.backend.name(),
        "codec": plan.codec.name(),
        "encoder": plan.codec.encoder(),
        "format": plan.container.name(),
        "fps": plan.fps,
        "audio": plan.audio.describe(),
        "hdr": plan.hdr.name(),
        "command": plan.command_line(),
        "started_unix_ms": unix_millis(),
        "log": plan.log_path,
    });
    let mut text = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("cannot encode recording state: {err}"))?;
    text.push('\n');
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .map_err(|err| format!("cannot write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|err| format!("cannot move recording state to {}: {err}", path.display()))
}

struct StateGuard {
    path: PathBuf,
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone)]
struct RunningState {
    file: PathBuf,
    pid: u32,
    path: PathBuf,
    started_unix_ms: u64,
    log: Option<PathBuf>,
}

fn live_states(directory: &Path) -> Result<Vec<RunningState>, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "cannot read recording state directory {}: {err}",
                directory.display()
            ));
        }
    };
    let mut states = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot read state entry: {err}"))?;
        let file = entry.path();
        let name = file.file_name().and_then(OsStr::to_str).unwrap_or("");
        if !name.starts_with("record-") || !name.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(pid) = value["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
        else {
            let _ = std::fs::remove_file(&file);
            continue;
        };
        let log = value["log"]
            .as_str()
            .filter(|log| !log.is_empty())
            .map(PathBuf::from);
        if process_alive(pid) {
            states.push(RunningState {
                file,
                pid,
                path: value["path"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                started_unix_ms: value["started_unix_ms"].as_u64().unwrap_or(0),
                log,
            });
        } else {
            let _ = std::fs::remove_file(&file);
            if let Some(log) = log {
                let _ = std::fs::remove_file(log);
            }
        }
    }
    Ok(states)
}

fn prune_stale(directory: &Path) {
    let _ = live_states(directory);
}

fn stop_recording(env: &Env, options: &RecordOptions) -> Result<StopReport, String> {
    let directory = state_dir(env);
    let states = live_states(&directory)?;
    let Some(state) = states.into_iter().max_by_key(|state| state.started_unix_ms) else {
        return Err(format!(
            "no running recording found in {}; start one with `loom-shot record`",
            directory.display()
        ));
    };
    let duration_seconds = (unix_millis().saturating_sub(state.started_unix_ms)) as f64 / 1000.0;
    if options.dry_run {
        return Ok(StopReport {
            path: state.path,
            pid: state.pid,
            duration_seconds,
            dry_run: true,
        });
    }
    stop_pid(state.pid)?;
    let _ = std::fs::remove_file(&state.file);
    if let Some(log) = &state.log {
        let _ = std::fs::remove_file(log);
    }
    Ok(StopReport {
        path: state.path,
        pid: state.pid,
        duration_seconds,
        dry_run: false,
    })
}

fn stop_pid(pid: u32) -> Result<(), String> {
    if !process_alive(pid) {
        return Ok(());
    }
    send_signal(pid, libc::SIGINT)?;
    if wait_for_pid_exit(pid, STOP_INT_GRACE) {
        return Ok(());
    }
    send_signal(pid, libc::SIGTERM)?;
    if wait_for_pid_exit(pid, STOP_TERM_GRACE) {
        return Ok(());
    }
    send_signal(pid, libc::SIGKILL)?;
    if wait_for_pid_exit(pid, STOP_TERM_GRACE) {
        Ok(())
    } else {
        Err(format!("recorder pid {pid} did not exit after SIGKILL"))
    }
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    !process_alive(pid)
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only performs error checking.
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn send_signal(pid: u32, signal: libc::c_int) -> Result<(), String> {
    // SAFETY: `kill` is a plain syscall wrapper; the pid comes from a child process.
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("cannot signal recorder pid {pid}: {err}"))
}

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_stop_signal(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Installs SIGINT/SIGTERM handlers and restores the previous ones on drop.
struct SignalGuard {
    previous_int: libc::sighandler_t,
    previous_term: libc::sighandler_t,
}

impl SignalGuard {
    fn install() -> Self {
        STOP_REQUESTED.store(false, Ordering::SeqCst);
        // SAFETY: installing a handler for a signal is process-global but the
        // handler only touches an atomic.
        unsafe {
            Self {
                previous_int: libc::signal(
                    libc::SIGINT,
                    handle_stop_signal as *const () as libc::sighandler_t,
                ),
                previous_term: libc::signal(
                    libc::SIGTERM,
                    handle_stop_signal as *const () as libc::sighandler_t,
                ),
            }
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        // SAFETY: restores the handlers that were installed before `run`.
        unsafe {
            libc::signal(libc::SIGINT, self.previous_int);
            libc::signal(libc::SIGTERM, self.previous_term);
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    use niri_ipc::{Window, WindowLayout};

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-shot-record-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_exe(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    struct Fixture {
        dir: PathBuf,
        bin: PathBuf,
        state: PathBuf,
        videos: PathBuf,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = temp_dir(name);
        let bin = dir.join("bin");
        let state = dir.join("state");
        let videos = dir.join("videos");
        std::fs::create_dir_all(&bin).unwrap();
        Fixture {
            dir,
            bin,
            state,
            videos,
        }
    }

    impl Fixture {
        fn env(&self) -> Env {
            Env {
                path: Some(self.bin.as_os_str().to_os_string()),
                wf_recorder: None,
                gst_launch: None,
                command: None,
                state_home: Some(self.state.clone()),
                videos_dir: Some(self.videos.clone()),
                home: Some(self.dir.join("home")),
                niri_socket: None,
            }
        }

        fn wf_recorder(&self, body: &str) -> PathBuf {
            write_exe(&self.bin, "wf-recorder", body)
        }

        fn gst_launch(&self, body: &str) -> PathBuf {
            write_exe(&self.bin, "gst-launch-1.0", body)
        }

        fn state_dir(&self) -> PathBuf {
            state_dir(&self.env())
        }

        fn state_files(&self) -> Vec<PathBuf> {
            let directory = self.state_dir();
            let Ok(entries) = std::fs::read_dir(directory) else {
                return Vec::new();
            };
            entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| name.ends_with(".json"))
                })
                .collect()
        }
    }

    /// A recorder that creates the output immediately and exits on SIGINT.
    const HOLDING_RECORDER: &str = "#!/bin/sh\n\
        printf x > \"$LOOM_SHOT_RECORD_PATH\"\n\
        trap 'printf y >> \"$LOOM_SHOT_RECORD_PATH\"; exit 0' INT TERM\n\
        while :; do sleep 0.02; done\n";

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

    fn fake_window(id: u64, app_id: &str, title: &str) -> Window {
        Window {
            id,
            title: Some(title.to_owned()),
            app_id: Some(app_id.to_owned()),
            pid: Some(1000),
            workspace_id: Some(1),
            is_focused: false,
            is_floating: false,
            is_sticky: false,
            is_urgent: false,
            is_minimized: false,
            is_agent: false,
            content_type: None,
            tearing_allowed: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn fake_niri(path: &Path, responses: Vec<Value>) -> std::thread::JoinHandle<Vec<Request>> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: Request = serde_json::from_str(line.trim()).unwrap();
                let mut stream = stream;
                stream.write_all(response.to_string().as_bytes()).unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
                requests.push(request);
            }
            requests
        })
    }

    #[test]
    fn parses_record_defaults() {
        let RecordCli::Run(options) = parse_cli(std::iter::empty::<&str>()).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.target, Target::FocusedOutput);
        assert_eq!(options.fps, Some(60));
        assert_eq!(options.codec, Codec::Auto);
        assert_eq!(options.container, None);
        assert_eq!(options.audio, Audio::None);
        assert_eq!(options.hdr, RecordHdr::Keep);
        assert_eq!(options.backend, BackendChoice::Auto);
        assert!(!options.stop);
        assert!(!options.json);
        assert!(!options.dry_run);
        assert_eq!(options.duration, None);
    }

    #[test]
    fn parses_every_record_flag() {
        let RecordCli::Run(options) = parse_cli([
            "--output",
            "DP-1",
            "--fps=30",
            "--codec",
            "av1",
            "--format=webm",
            "--audio",
            "sink:alsa_output.pci",
            "--hdr",
            "tonemap",
            "--duration=1.5",
            "--backend=command",
            "--command=rec {path}",
            "--path=/tmp/a.webm",
            "--socket",
            "/run/niri.sock",
            "--json",
            "--dry-run",
        ])
        .unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.target, Target::Output("DP-1".to_owned()));
        assert_eq!(options.fps, Some(30));
        assert_eq!(options.codec, Codec::Av1);
        assert_eq!(options.container, Some(Container::WebM));
        assert_eq!(
            options.audio,
            Audio::Device("alsa_output.pci.monitor".to_owned())
        );
        assert_eq!(options.hdr, RecordHdr::Tonemap);
        assert_eq!(options.duration, Some(Duration::from_millis(1500)));
        assert_eq!(options.backend, BackendChoice::Command);
        assert_eq!(options.command.as_deref(), Some("rec {path}"));
        assert_eq!(options.socket.as_deref(), Some(Path::new("/run/niri.sock")));
        assert!(options.json);
        assert!(options.dry_run);
    }

    #[test]
    fn parses_regions_windows_audio_and_fps() {
        let RecordCli::Run(options) =
            parse_cli(["--region", "10,20,640,480", "--fps", "auto"]).unwrap()
        else {
            panic!("expected run");
        };
        assert_eq!(
            options.target,
            Target::Region(Region {
                x: 10,
                y: 20,
                w: 640,
                h: 480
            })
        );
        assert_eq!(options.fps, None);

        let RecordCli::Run(options) = parse_cli(["--window=42"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.target, Target::Window(42));

        let RecordCli::Run(options) = parse_cli(["--window=focused"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.target, Target::FocusedWindow);

        let RecordCli::Run(options) = parse_cli(["--window"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.target, Target::FocusedWindow);

        assert_eq!(Audio::parse("none"), Some(Audio::None));
        assert_eq!(Audio::parse("default"), Some(Audio::Default));
        assert_eq!(
            Audio::parse("source:mic"),
            Some(Audio::Device("mic".to_owned()))
        );
        assert_eq!(
            Audio::parse("sink:speakers"),
            Some(Audio::Device("speakers.monitor".to_owned()))
        );
        assert_eq!(
            Audio::parse("sink:speakers.monitor"),
            Some(Audio::Device("speakers.monitor".to_owned()))
        );
        assert_eq!(
            Audio::parse("alsa_input.foo"),
            Some(Audio::Device("alsa_input.foo".to_owned()))
        );
    }

    #[test]
    fn rejects_bad_record_input() {
        for args in [
            vec!["--output", "DP-1", "--region=0,0,1,1"],
            vec!["--region=0,0,0,1"],
            vec!["--region=0,0,1"],
            vec!["--window=nope"],
            vec!["--fps=0.5"],
            vec!["--fps=241"],
            vec!["--codec=vp9"],
            vec!["--format=avi"],
            vec!["--audio="],
            vec!["--hdr=sdr"],
            vec!["--duration=0"],
            vec!["--duration=999999"],
            vec!["--backend=x"],
            vec!["--copy"],
            vec!["--id=3"],
            vec!["--stop", "--output", "DP-1"],
            vec!["--stop", "--duration=2"],
            vec!["--format=webm", "--codec=h264"],
            vec!["--format=webm", "--codec=hevc"],
            vec!["--format=mp4", "--path=/tmp/a.mkv"],
            vec!["--path=/tmp/a.avi"],
            vec!["--nope"],
            vec!["extra"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
        assert_eq!(
            parse_cli(["--help"]).unwrap(),
            RecordCli::Help,
            "help should be recognized"
        );
    }

    #[test]
    fn accepts_stop_with_json_and_dry_run_only() {
        let RecordCli::Run(options) = parse_cli(["--stop", "--json", "--dry-run"]).unwrap() else {
            panic!("expected run");
        };
        assert!(options.stop);
        assert!(options.json);
        assert!(options.dry_run);
    }

    #[test]
    fn detects_backends_and_explains_unavailability() {
        let fixture = fixture("probes");
        let env = fixture.env();

        let probes = probe_backends(&env, None);
        let wf = probes
            .iter()
            .find(|probe| probe.kind == BackendKind::WfRecorder)
            .unwrap();
        assert!(!wf.available);
        assert!(wf.detail.contains("not found in PATH"), "{}", wf.detail);
        let portal = probes
            .iter()
            .find(|probe| probe.kind == BackendKind::Portal)
            .unwrap();
        assert!(!portal.available);
        assert!(
            portal.detail.contains("gst-launch-1.0"),
            "{}",
            portal.detail
        );
        let command = probes
            .iter()
            .find(|probe| probe.kind == BackendKind::Command)
            .unwrap();
        assert!(!command.available);
        assert!(command.detail.contains("--command"), "{}", command.detail);

        let non_executable = fixture.bin.join("wf-recorder");
        std::fs::write(&non_executable, "not executable").unwrap();
        let env = Env {
            wf_recorder: Some(non_executable),
            ..fixture.env()
        };
        assert!(
            probe_backends(&env, None)[0]
                .detail
                .contains("not an executable")
        );

        let executable = fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let probes = probe_backends(&fixture.env(), None);
        assert!(probes[0].available);
        assert!(probes[0].detail.contains(&executable.display().to_string()));

        let env = Env {
            wf_recorder: Some(executable),
            path: Some(fixture.dir.as_os_str().to_os_string()),
            ..fixture.env()
        };
        assert!(probe_backends(&env, None)[0].available);

        let command = probe_backends(&fixture.env(), Some("rec {path}"));
        assert!(command[2].available);
        assert!(command[2].detail.contains("--command"));
    }

    #[test]
    fn selects_wf_recorder_then_command_and_rejects_missing() {
        let fixture = fixture("select");
        let env = fixture.env();
        let options = RecordOptions {
            path: Some(fixture.dir.join("a.mp4")),
            target: Target::Output("DP-1".to_owned()),
            dry_run: true,
            ..RecordOptions::default()
        };
        let error = build_plan(&env, &options, 1, false).unwrap_err();
        assert!(
            error.contains("no recording backend is available"),
            "{error}"
        );
        assert!(error.contains("wf-recorder"), "{error}");
        assert!(error.contains("command"), "{error}");

        let command_env = Env {
            command: Some("rec {path}".to_owned()),
            ..fixture.env()
        };
        let plan = build_plan(&command_env, &options, 1, false).unwrap();
        assert_eq!(plan.backend, BackendKind::Command);

        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let plan = build_plan(&fixture.env(), &options, 1, false).unwrap();
        assert_eq!(plan.backend, BackendKind::WfRecorder);

        let explicit = RecordOptions {
            backend: BackendChoice::Command,
            ..options.clone()
        };
        assert!(build_plan(&fixture.env(), &explicit, 1, false).is_err());

        let portal = RecordOptions {
            backend: BackendChoice::Portal,
            ..options.clone()
        };
        let error = build_plan(&fixture.env(), &portal, 1, false).unwrap_err();
        assert!(error.contains("portal backend is not available"), "{error}");
        assert!(error.contains("gst-launch-1.0"), "{error}");

        let gst = fixture.gst_launch("#!/bin/sh\nexit 0\n");
        let plan = build_plan(&fixture.env(), &portal, 1, false).unwrap();
        assert_eq!(plan.backend, BackendKind::Portal);
        assert_eq!(plan.program, gst);
        assert!(
            plan.args.join(" ").contains("pipewiresrc path=PORTAL"),
            "{:?}",
            plan.args
        );
        let probe = probe_backends(&fixture.env(), None)
            .into_iter()
            .find(|probe| probe.kind == BackendKind::Portal)
            .unwrap();
        assert!(probe.available);
        assert!(probe.detail.contains(&gst.display().to_string()));
    }

    #[test]
    fn portal_rejects_region_audio_and_tonemap() {
        let fixture = fixture("portal-limits");
        fixture.gst_launch("#!/bin/sh\nexit 0\n");
        let env = fixture.env();
        let base = RecordOptions {
            path: Some(fixture.dir.join("a.mp4")),
            backend: BackendChoice::Portal,
            dry_run: true,
            ..RecordOptions::default()
        };

        let region = RecordOptions {
            target: Target::Region(Region {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            }),
            ..base.clone()
        };
        assert!(
            build_plan(&env, &region, 1, false)
                .unwrap_err()
                .contains("no region source")
        );

        let audio = RecordOptions {
            audio: Audio::Device("sink.monitor".to_owned()),
            ..base.clone()
        };
        assert!(
            build_plan(&env, &audio, 1, false)
                .unwrap_err()
                .contains("does not record audio")
        );

        let tonemap = RecordOptions {
            hdr: RecordHdr::Tonemap,
            ..base
        };
        assert!(
            build_plan(&env, &tonemap, 1, false)
                .unwrap_err()
                .contains("does not tone-map")
        );
    }

    #[test]
    fn auto_prefers_portal_over_a_command_template() {
        let fixture = fixture("auto-portal");
        fixture.gst_launch("#!/bin/sh\nexit 0\n");
        let env = Env {
            command: Some("rec {path}".to_owned()),
            ..fixture.env()
        };
        let options = RecordOptions {
            path: Some(fixture.dir.join("a.mp4")),
            target: Target::Output("DP-1".to_owned()),
            dry_run: true,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, false).unwrap();
        assert_eq!(plan.backend, BackendKind::Portal);
    }

    #[test]
    fn maps_every_codec_and_container_for_wf_recorder() {
        let fixture = fixture("matrix");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let env = fixture.env();
        let valid = [
            (Codec::Auto, Container::Mp4, Codec::H264),
            (Codec::Auto, Container::Mkv, Codec::H264),
            (Codec::Auto, Container::WebM, Codec::Av1),
            (Codec::H264, Container::Mp4, Codec::H264),
            (Codec::H264, Container::Mkv, Codec::H264),
            (Codec::Hevc, Container::Mp4, Codec::Hevc),
            (Codec::Hevc, Container::Mkv, Codec::Hevc),
            (Codec::Av1, Container::Mp4, Codec::Av1),
            (Codec::Av1, Container::Mkv, Codec::Av1),
            (Codec::Av1, Container::WebM, Codec::Av1),
        ];
        for (requested, container, resolved) in valid {
            let options = RecordOptions {
                path: Some(fixture.dir.join(format!("clip.{}", container.extension()))),
                target: Target::Output("DP-1".to_owned()),
                codec: requested,
                container: Some(container),
                dry_run: true,
                ..RecordOptions::default()
            };
            let plan = build_plan(&env, &options, 1, false).unwrap();
            assert_eq!(plan.codec, resolved, "{requested:?} in {container:?}");
            assert!(plan.args.contains(&resolved.encoder().to_owned()));
        }
        for codec in [Codec::H264, Codec::Hevc] {
            let options = RecordOptions {
                path: Some(fixture.dir.join("clip.webm")),
                target: Target::Output("DP-1".to_owned()),
                codec,
                container: Some(Container::WebM),
                dry_run: true,
                ..RecordOptions::default()
            };
            let error = build_plan(&env, &options, 1, false).unwrap_err();
            assert!(error.contains("WebM"), "{error}");
        }
    }

    #[test]
    fn maps_wf_recorder_output_region_fps_and_audio() {
        let fixture = fixture("wf-args");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let env = fixture.env();

        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            fps: Some(30),
            codec: Codec::Hevc,
            container: Some(Container::Mp4),
            audio: Audio::Device("mic".to_owned()),
            dry_run: true,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, false).unwrap();
        assert_eq!(
            plan.args,
            vec![
                "-f",
                fixture.dir.join("clip.mp4").to_str().unwrap(),
                "-r",
                "30",
                "-c",
                "libx265",
                "-o",
                "DP-1",
                "--audio=mic"
            ]
        );

        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mkv")),
            target: Target::Region(Region {
                x: 10,
                y: 20,
                w: 640,
                h: 480,
            }),
            fps: None,
            codec: Codec::Auto,
            container: Some(Container::Mkv),
            audio: Audio::Default,
            dry_run: true,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, false).unwrap();
        assert_eq!(
            plan.args,
            vec![
                "-f",
                fixture.dir.join("clip.mkv").to_str().unwrap(),
                "-c",
                "libx264",
                "-g",
                "10,20 640x480",
                "-a"
            ]
        );
        assert!(!plan.args.contains(&"-r".to_owned()));
    }

    #[test]
    fn rejects_unsupported_wf_recorder_combinations() {
        let fixture = fixture("wf-limits");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let env = fixture.env();

        let window = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Window(7),
            dry_run: true,
            ..RecordOptions::default()
        };
        let error = build_plan(&env, &window, 1, false).unwrap_err();
        assert!(error.contains("single window"), "{error}");
        assert!(error.contains("--backend=command"), "{error}");

        let tonemap = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            hdr: RecordHdr::Tonemap,
            dry_run: true,
            ..RecordOptions::default()
        };
        let error = build_plan(&env, &tonemap, 1, false).unwrap_err();
        assert!(error.contains("HDR tone-map"), "{error}");
        assert!(error.contains("--backend=command"), "{error}");
    }

    #[test]
    fn resolves_focused_output_through_niri() {
        let fixture = fixture("focused");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let socket = fixture.dir.join("niri.sock");
        let server = fake_niri(
            &socket,
            vec![json!({"Ok": Response::FocusedOutput(Some(fake_output()))})],
        );
        let env = Env {
            niri_socket: Some(socket),
            ..fixture.env()
        };
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            dry_run: false,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, true).unwrap();
        assert_eq!(plan.resolved_output.as_deref(), Some("TEST-OUTPUT-1"));
        assert!(
            plan.args
                .windows(2)
                .any(|pair| pair == ["-o", "TEST-OUTPUT-1"])
        );
        let requests = server.join().unwrap();
        assert!(matches!(requests[0], Request::FocusedOutput));
    }

    #[test]
    fn requires_socket_to_resolve_focused_output_for_wf_recorder() {
        let fixture = fixture("focused-missing");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            ..RecordOptions::default()
        };
        let error = build_plan(&fixture.env(), &options, 1, true).unwrap_err();
        assert!(error.contains("NIRI_SOCKET"), "{error}");
        assert!(error.contains("--output"), "{error}");
    }

    #[test]
    fn refuses_the_niri_screenshot_ui_window() {
        let fixture = fixture("ui-window");
        let socket = fixture.dir.join("niri.sock");
        let server = fake_niri(
            &socket,
            vec![json!({
                "Ok": Response::Windows(vec![fake_window(42, "niri", "Screenshot UI")])
            })],
        );
        let env = Env {
            niri_socket: Some(socket),
            command: Some("rec {path} {window}".to_owned()),
            ..fixture.env()
        };
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Window(42),
            backend: BackendChoice::Command,
            ..RecordOptions::default()
        };
        let error = build_plan(&env, &options, 1, true).unwrap_err();
        assert!(error.contains("screenshot/screencast UI"), "{error}");
        server.join().unwrap();

        let socket = fixture.dir.join("niri2.sock");
        let server = fake_niri(
            &socket,
            vec![json!({
                "Ok": Response::Windows(vec![fake_window(7, "firefox", "Docs")])
            })],
        );
        let env = Env {
            niri_socket: Some(socket),
            command: Some("rec {path} {window}".to_owned()),
            ..fixture.env()
        };
        let window_options = RecordOptions {
            target: Target::Window(7),
            ..options.clone()
        };
        let plan = build_plan(&env, &window_options, 1, true).unwrap();
        assert!(plan.args[1].contains("'7'"), "{:?}", plan.args);
        server.join().unwrap();
    }

    #[test]
    fn resolves_focused_window_and_refuses_the_ui() {
        let fixture = fixture("focused-window");
        let socket = fixture.dir.join("niri.sock");
        let server = fake_niri(
            &socket,
            vec![json!({
                "Ok": Response::FocusedWindow(Some(fake_window(7, "firefox", "Docs")))
            })],
        );
        let env = Env {
            niri_socket: Some(socket),
            command: Some("rec {path} --window {window}".to_owned()),
            ..fixture.env()
        };
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::FocusedWindow,
            backend: BackendChoice::Command,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, true).unwrap();
        assert!(plan.args[1].contains("--window '7'"), "{:?}", plan.args);
        assert!(matches!(
            server.join().unwrap().as_slice(),
            [Request::FocusedWindow]
        ));

        let socket = fixture.dir.join("niri-ui.sock");
        let server = fake_niri(
            &socket,
            vec![json!({
                "Ok": Response::FocusedWindow(Some(fake_window(42, "niri", "Screenshot UI")))
            })],
        );
        let env = Env {
            niri_socket: Some(socket),
            command: Some("rec {path}".to_owned()),
            ..fixture.env()
        };
        let error = build_plan(&env, &options, 1, true).unwrap_err();
        assert!(error.contains("screenshot/screencast UI"), "{error}");
        server.join().unwrap();

        let error = build_plan(
            &Env {
                command: Some("rec {path}".to_owned()),
                ..fixture.env()
            },
            &options,
            1,
            true,
        )
        .unwrap_err();
        assert!(
            error.contains("cannot resolve the focused window"),
            "{error}"
        );
        assert!(error.contains("--window ID"), "{error}");
    }

    #[test]
    fn generates_timestamped_paths_and_infers_containers() {
        let fixture = fixture("paths");
        let env = fixture.env();
        let path = generated_recording_path(&env, 1234, Container::Mp4);
        assert_eq!(
            path,
            fixture.videos.join("Recordings").join("loom-shot-1234.mp4")
        );
        let webm = generated_recording_path(&env, 99, Container::WebM);
        assert_eq!(webm.file_name().unwrap(), "loom-shot-99.webm");

        let no_videos = Env {
            videos_dir: None,
            home: Some(fixture.dir.join("home")),
            ..fixture.env()
        };
        assert_eq!(
            generated_recording_path(&no_videos, 5, Container::Mkv),
            fixture.dir.join("home/Videos/Recordings/loom-shot-5.mkv")
        );

        let options = RecordOptions {
            path: Some(fixture.dir.join("clip")),
            target: Target::Output("DP-1".to_owned()),
            dry_run: true,
            ..RecordOptions::default()
        };
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let plan = build_plan(&fixture.env(), &options, 1, false).unwrap();
        assert_eq!(plan.path, fixture.dir.join("clip.mp4"));

        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mkv")),
            target: Target::Output("DP-1".to_owned()),
            container: Some(Container::Mp4),
            ..RecordOptions::default()
        };
        let error = build_plan(&fixture.env(), &options, 1, false).unwrap_err();
        assert!(
            error.contains("--format=mp4") || error.contains("ends in .mkv"),
            "{error}"
        );
    }

    #[test]
    fn renders_command_templates_with_quoting_and_env() {
        let fixture = fixture("template");
        let env = Env {
            command: Some(
                "rec --out {path} --fps {fps} --codec {encoder} --fmt {format} \
                 --audio {audio} --hdr {hdr} --out-name {output} --win {window} \
                 --region {region} --geom {geometry} --backend {backend} ${HOME} {unknown}"
                    .to_owned(),
            ),
            ..fixture.env()
        };
        let options = RecordOptions {
            path: Some(fixture.dir.join("with space.mp4")),
            target: Target::Window(7),
            fps: Some(24),
            codec: Codec::Av1,
            container: Some(Container::Mp4),
            audio: Audio::Device("mic".to_owned()),
            hdr: RecordHdr::Tonemap,
            backend: BackendChoice::Command,
            dry_run: true,
            ..RecordOptions::default()
        };
        let plan = build_plan(&env, &options, 1, false).unwrap();
        let script = &plan.args[1];
        assert!(script.contains("'/"), "{script}");
        assert!(script.contains("with space.mp4'"), "{script}");
        assert!(script.contains("--fps '24'"), "{script}");
        assert!(script.contains("--codec 'libaom-av1'"), "{script}");
        assert!(script.contains("--fmt 'mp4'"), "{script}");
        assert!(script.contains("--audio 'mic'"), "{script}");
        assert!(script.contains("--hdr 'tonemap'"), "{script}");
        assert!(script.contains("--win '7'"), "{script}");
        assert!(script.contains("--backend 'command'"), "{script}");
        assert!(script.contains("${HOME}"), "{script}");
        assert!(script.contains("{unknown}"), "{script}");
        assert_eq!(
            plan.child_env
                .iter()
                .find(|(key, _)| *key == "LOOM_SHOT_RECORD_ENCODER")
                .map(|(_, value)| value.as_str()),
            Some("libaom-av1")
        );

        let error = render_template("rec {output}", &[("path", "x".to_owned())]).unwrap_err();
        assert!(error.contains("{path}"), "{error}");
    }

    #[test]
    fn records_for_a_duration_cleans_state_and_reports() {
        let fixture = fixture("duration");
        fixture.wf_recorder(HOLDING_RECORDER);
        let env = fixture.env();
        let path = fixture.dir.join("clip.mp4");
        let options = RecordOptions {
            path: Some(path.clone()),
            target: Target::Output("DP-1".to_owned()),
            duration: Some(Duration::from_millis(250)),
            ..RecordOptions::default()
        };
        let outcome = run(&env, &options).unwrap();
        let RecordOutcome::Recorded(report) = outcome else {
            panic!("expected a recording");
        };
        assert_eq!(report.path, path);
        assert_eq!(report.stopped_by, Some("duration"));
        assert!(report.duration_seconds.unwrap() >= 0.2);
        assert_eq!(report.backend, BackendKind::WfRecorder);
        assert_eq!(std::fs::read(&path).unwrap(), b"xy");
        assert!(
            fixture.state_files().is_empty(),
            "state file was not cleaned"
        );
        assert!(
            !std::fs::read_dir(fixture.state_dir())
                .unwrap()
                .any(|entry| entry.unwrap().path().extension() == Some(OsStr::new("log"))),
            "log file was not cleaned"
        );

        let json = report.to_json();
        assert_eq!(json["recording"]["path"], path.display().to_string());
        assert_eq!(json["recording"]["codec"], "h264");
        assert_eq!(json["recording"]["format"], "mp4");
        assert_eq!(json["recording"]["stopped_by"], "duration");
        assert_eq!(json["recording"]["dry_run"], false);
        assert_eq!(json["recording"]["stopped"], false);
        assert_eq!(json["recording"]["target"]["kind"], "output");
    }

    #[test]
    fn stop_command_terminates_a_running_recording() {
        let fixture = fixture("stop");
        fixture.wf_recorder(HOLDING_RECORDER);
        let env = fixture.env();
        let path = fixture.dir.join("clip.mp4");
        let options = RecordOptions {
            path: Some(path.clone()),
            target: Target::Output("DP-1".to_owned()),
            ..RecordOptions::default()
        };

        let thread_env = env.clone();
        let thread_options = options.clone();
        let handle = std::thread::spawn(move || run(&thread_env, &thread_options));

        let state = wait_for_state(&fixture, Duration::from_secs(5));
        let state_text = std::fs::read_to_string(&state).unwrap();
        let value: Value = serde_json::from_str(&state_text).unwrap();
        let pid = value["pid"].as_u64().unwrap() as u32;
        assert!(process_alive(pid));
        assert_eq!(value["path"], path.display().to_string());
        assert_eq!(value["backend"], "wf-recorder");

        let stop_options = RecordOptions {
            stop: true,
            ..RecordOptions::default()
        };
        let stopped = run(&env, &stop_options).unwrap();
        let RecordOutcome::Stopped(report) = stopped else {
            panic!("expected stop");
        };
        assert_eq!(report.pid, pid);
        assert_eq!(report.path, path);
        assert!(!report.dry_run);
        assert!(!process_alive(pid));
        assert!(fixture.state_files().is_empty());
        let json = report.to_json();
        assert_eq!(json["recording"]["stopped"], true);
        assert_eq!(json["recording"]["pid"], pid);
        assert!(json["recording"]["duration_seconds"].as_f64().unwrap() >= 0.0);

        let outcome = handle.join().unwrap().unwrap();
        let RecordOutcome::Recorded(report) = outcome else {
            panic!("expected recording");
        };
        assert!(report.duration_seconds.unwrap() >= 0.0);
        assert!(std::fs::read(&path).unwrap().starts_with(b"x"));
    }

    fn wait_for_state(fixture: &Fixture, timeout: Duration) -> PathBuf {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(path) = fixture.state_files().first() {
                return path.clone();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no recording state file appeared");
    }

    #[test]
    fn stop_without_a_recording_is_an_error() {
        let fixture = fixture("stop-empty");
        let error = run(
            &fixture.env(),
            &RecordOptions {
                stop: true,
                ..RecordOptions::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("no running recording found"), "{error}");
    }

    #[test]
    fn stop_dry_run_reports_without_signalling() {
        let fixture = fixture("stop-dry");
        fixture.wf_recorder(HOLDING_RECORDER);
        let env = fixture.env();
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            ..RecordOptions::default()
        };
        let thread_env = env.clone();
        let handle = std::thread::spawn(move || run(&thread_env, &options));
        let state = wait_for_state(&fixture, Duration::from_secs(5));
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        let pid = value["pid"].as_u64().unwrap() as u32;

        let stopped = run(
            &env,
            &RecordOptions {
                stop: true,
                dry_run: true,
                ..RecordOptions::default()
            },
        )
        .unwrap();
        let RecordOutcome::Stopped(report) = stopped else {
            panic!("expected stop");
        };
        assert!(report.dry_run);
        assert!(process_alive(pid), "dry run must not stop the recorder");
        assert_eq!(report.to_json()["recording"]["stopped"], false);

        run(
            &env,
            &RecordOptions {
                stop: true,
                ..RecordOptions::default()
            },
        )
        .unwrap();
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn reports_spawn_failures_without_leaving_a_log() {
        let fixture = fixture("spawn-failure");
        fixture.wf_recorder("#!/no/such/interpreter\n");
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            ..RecordOptions::default()
        };
        let error = run(&fixture.env(), &options).unwrap_err();
        assert!(error.contains("cannot start"), "{error}");
        assert!(fixture.state_files().is_empty());
        assert!(
            !std::fs::read_dir(fixture.state_dir())
                .unwrap()
                .any(|entry| entry.unwrap().path().extension() == Some(OsStr::new("log"))),
            "failed spawn left a log behind"
        );
    }

    #[test]
    fn maps_early_exits_and_cleans_state() {
        let fixture = fixture("early");
        fixture.wf_recorder(
            "#!/bin/sh\n\
             printf 'failed to connect to the Wayland compositor\\n' >&2\n\
             exit 4\n",
        );
        let env = fixture.env();
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            duration: Some(Duration::from_secs(30)),
            ..RecordOptions::default()
        };
        let error = run(&env, &options).unwrap_err();
        assert!(error.contains("failed (exit status: 4)"), "{error}");
        assert!(error.contains("Wayland compositor"), "{error}");
        assert!(error.contains("log:"), "{error}");
        assert!(fixture.state_files().is_empty());
        assert!(
            std::fs::read_dir(fixture.state_dir())
                .unwrap()
                .any(|entry| entry.unwrap().path().extension() == Some(OsStr::new("log"))),
            "failure log should be preserved for diagnostics"
        );
    }

    #[test]
    fn maps_permission_failures() {
        let fixture = fixture("permission");
        fixture.wf_recorder("#!/bin/sh\nprintf 'portal: permission denied\\n' >&2\nexit 1\n");
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            ..RecordOptions::default()
        };
        let error = run(&fixture.env(), &options).unwrap_err();
        assert!(error.contains("permission was denied"), "{error}");
    }

    #[test]
    fn rejects_early_success_when_duration_was_requested() {
        let fixture = fixture("early-success");
        fixture.wf_recorder("#!/bin/sh\nprintf x > \"$LOOM_SHOT_RECORD_PATH\"\nexit 0\n");
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            duration: Some(Duration::from_secs(2)),
            ..RecordOptions::default()
        };
        let error = run(&fixture.env(), &options).unwrap_err();
        assert!(error.contains("before --duration elapsed"), "{error}");
        assert!(fixture.state_files().is_empty());
    }

    #[test]
    fn reports_when_a_successful_recorder_writes_nothing() {
        let fixture = fixture("no-output");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            ..RecordOptions::default()
        };
        let error = run(&fixture.env(), &options).unwrap_err();
        assert!(error.contains("wrote no data"), "{error}");
        assert!(fixture.state_files().is_empty());
    }

    #[test]
    fn accepts_a_successful_command_backend_and_its_json() {
        let fixture = fixture("command");
        let recorder = write_exe(
            &fixture.bin,
            "recorder",
            "#!/bin/sh\nprintf hello > \"$LOOM_SHOT_RECORD_PATH\"\nexit 0\n",
        );
        let env = Env {
            command: Some(format!("{} --out {{path}}", recorder.display())),
            ..fixture.env()
        };
        let path = fixture.dir.join("clip.mkv");
        let options = RecordOptions {
            path: Some(path.clone()),
            target: Target::Output("DP-1".to_owned()),
            container: Some(Container::Mkv),
            codec: Codec::Hevc,
            backend: BackendChoice::Command,
            json: true,
            ..RecordOptions::default()
        };
        let outcome = run(&env, &options).unwrap();
        let RecordOutcome::Recorded(report) = outcome else {
            panic!("expected recording");
        };
        assert_eq!(report.backend, BackendKind::Command);
        assert_eq!(report.stopped_by, Some("backend"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let json = report.to_json();
        assert_eq!(json["recording"]["backend"], "command");
        assert_eq!(json["recording"]["encoder"], "libx265");
        assert_eq!(json["recording"]["format"], "mkv");
        assert_eq!(json["recording"]["audio"], "none");
        assert_eq!(json["recording"]["hdr"], "keep");
        assert!(
            !json["recording"]["command"][2]
                .as_str()
                .unwrap()
                .contains("{path}")
        );
    }

    #[test]
    fn dry_run_does_not_start_or_touch_the_filesystem() {
        let fixture = fixture("dry");
        fixture.wf_recorder("#!/bin/sh\nexit 0\n");
        let env = fixture.env();
        let options = RecordOptions {
            path: Some(fixture.dir.join("clip.mp4")),
            target: Target::Output("DP-1".to_owned()),
            dry_run: true,
            ..RecordOptions::default()
        };
        let outcome = run(&env, &options).unwrap();
        let RecordOutcome::Recorded(report) = outcome else {
            panic!("expected report");
        };
        assert!(report.dry_run);
        assert_eq!(report.pid, None);
        assert_eq!(report.duration_seconds, None);
        assert!(!fixture.dir.join("clip.mp4").exists());
        assert!(!fixture.state_dir().exists());
        let json = report.to_json();
        assert_eq!(json["recording"]["dry_run"], true);
        assert_eq!(json["recording"]["pid"], Value::Null);
        assert!(report.summary().contains("Would record"));
    }

    #[test]
    fn prunes_stale_state_and_ignores_foreign_files() {
        let fixture = fixture("stale");
        let directory = fixture.state_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let stale = directory.join("record-999999.json");
        std::fs::write(
            &stale,
            json!({"pid": 4_000_000_000_u64, "path": "/tmp/old.mp4", "started_unix_ms": 1})
                .to_string(),
        )
        .unwrap();
        std::fs::write(directory.join("notes.txt"), "keep me").unwrap();
        let states = live_states(&directory).unwrap();
        assert!(states.is_empty());
        assert!(!stale.exists());
        assert!(directory.join("notes.txt").exists());
    }

    #[test]
    fn region_and_state_helpers_are_stable() {
        assert_eq!(
            Region::parse(" 1, 2, 3, 4 "),
            Some(Region {
                x: 1,
                y: 2,
                w: 3,
                h: 4
            })
        );
        assert_eq!(Region::parse("1,2,3"), None);
        assert_eq!(Region::parse("1,2,0,4"), None);
        assert_eq!(
            Region {
                x: -5,
                y: 0,
                w: 10,
                h: 20
            }
            .compact(),
            "-5,0,10,20"
        );
        assert_eq!(
            Container::from_path(Path::new("/tmp/a.WEBM")),
            Some(Container::WebM)
        );
        assert_eq!(Container::from_path(Path::new("/tmp/a")), None);
        assert_eq!(Codec::parse("HEVC"), Some(Codec::Hevc));
        assert_eq!(Codec::parse("vp9"), None);
        assert_eq!(RecordHdr::parse("Tonemap"), Some(RecordHdr::Tonemap));
        assert_eq!(BackendChoice::parse("wf"), Some(BackendChoice::WfRecorder));
        assert_eq!(BackendChoice::parse("cmd"), Some(BackendChoice::Command));
    }
}
