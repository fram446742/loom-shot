//! File-backed pins, with an optional layer-shell overlay.
//!
//! Every pin copies the image to a stable path under
//! `$XDG_STATE_HOME/loom-shot/pins/`. By default the copy is opened in an
//! external viewer (`$LOOM_SHOT_VIEWER`, then `xdg-open`); that window is
//! managed normally and can be covered. `pin add --layer` instead spawns a
//! detached `loom-shot pin-view PATH` child that renders the copy on a
//! click-through `zwlr_layer_shell_v1` overlay. `pin list` / `pin unpin` manage
//! the stable copies and terminate overlay children.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::history::state_dir;

const VERSION: u64 = 1;
const PINS_DIR: &str = "pins";
const REGISTRY: &str = "pins.json";

pub const USAGE: &str = r#"loom-shot pin PATH [options]
loom-shot pin list [--json]
loom-shot pin unpin ID [--json]

  PATH                  Image to pin (copied to a stable path)
  --id ID               Pin id (default: pin-<epoch>-<pid>)
  --no-open             Register the pin without launching a viewer
  --viewer CMD          Viewer program (default: $LOOM_SHOT_VIEWER, then xdg-open)
  --layer               Pin on a click-through layer-shell overlay instead of a viewer
  --json                Print a JSON report

  Without --layer a pin is a stable copy plus an external viewer window.
  `pin unpin` removes the registry entry and the copy, and terminates a layer
  overlay child if one is running.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub id: String,
    pub path: PathBuf,
    pub source: Option<PathBuf>,
    pub created_unix: u64,
    pub viewer: Option<String>,
    /// PID of the layer overlay child, if the pin was added with `--layer`.
    pub pid: Option<u64>,
}

impl Pin {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "path": self.path.display().to_string(),
            "source": self.source.as_ref().map(|path| path.display().to_string()),
            "created_unix": self.created_unix,
            "viewer": self.viewer,
            "pid": self.pid,
            "pid_alive": self.pid.is_some_and(pid_alive),
        })
    }

    fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "pin entry must be an object".to_owned())?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "pin entry is missing `id`".to_owned())?;
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pin `{id}` is missing `path`"))?;
        Ok(Self {
            id: id.to_owned(),
            path: PathBuf::from(path),
            source: object
                .get("source")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            created_unix: object
                .get("created_unix")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            viewer: object
                .get("viewer")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pid: object.get("pid").and_then(Value::as_u64),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinCommand {
    Add,
    List,
    Unpin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinOptions {
    pub command: PinCommand,
    pub path: Option<PathBuf>,
    pub id: Option<String>,
    pub no_open: bool,
    pub viewer: Option<String>,
    pub layer: bool,
    pub json: bool,
}

impl Default for PinOptions {
    fn default() -> Self {
        Self {
            command: PinCommand::Add,
            path: None,
            id: None,
            no_open: false,
            viewer: None,
            layer: false,
            json: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PinCli {
    Run(PinOptions),
    Help,
}

/// Parses arguments after the `pin` token.
pub fn parse_cli<I, S>(args: I) -> Result<PinCli, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into).peekable();
    let mut options = PinOptions::default();
    let mut words: Vec<String> = Vec::new();
    while let Some(raw) = args.next() {
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (raw, None),
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(PinCli::Help),
            "--json" => {
                no_value(&flag, inline)?;
                options.json = true;
            }
            "--no-open" => {
                no_value(&flag, inline)?;
                options.no_open = true;
            }
            "--layer" => {
                no_value(&flag, inline)?;
                options.layer = true;
            }
            "--no-layer" => {
                no_value(&flag, inline)?;
                options.layer = false;
            }
            "--id" => options.id = Some(value(&flag, inline, &mut args)?),
            "--viewer" => options.viewer = Some(value(&flag, inline, &mut args)?),
            other if other.starts_with('-') => {
                return Err(format!("unknown pin option `{other}`\n\n{USAGE}"));
            }
            other => words.push(other.to_owned()),
        }
    }

    match words.first().map(String::as_str) {
        None => return Err(format!("`pin` needs a path, `list` or `unpin`\n\n{USAGE}")),
        Some("list") => {
            require_words(&words, 1, "`pin list`")?;
            options.command = PinCommand::List;
            reject_add_only(&options, "`pin list`")?;
        }
        Some("unpin") => {
            require_words(&words, 2, "`pin unpin ID`")?;
            options.command = PinCommand::Unpin;
            if options.id.is_some() {
                return Err("`pin unpin` takes the id as its argument, not --id".to_owned());
            }
            options.id = Some(words[1].clone());
            if options.path.is_some()
                || options.no_open
                || options.viewer.is_some()
                || options.layer
            {
                return Err("`pin unpin` only accepts --json".to_owned());
            }
        }
        Some(_) => {
            require_words(&words, 1, "`pin PATH`")?;
            options.command = PinCommand::Add;
            options.path = Some(PathBuf::from(&words[0]));
            if let Some(id) = &options.id {
                validate_id(id)?;
            }
        }
    }
    Ok(PinCli::Run(options))
}

fn require_words(words: &[String], expected: usize, usage: &str) -> Result<(), String> {
    if words.len() != expected {
        return Err(format!("{usage} takes {expected} argument(s)\n\n{USAGE}"));
    }
    Ok(())
}

fn reject_add_only(options: &PinOptions, command: &str) -> Result<(), String> {
    if options.path.is_some()
        || options.id.is_some()
        || options.no_open
        || options.viewer.is_some()
        || options.layer
    {
        return Err(format!("{command} only accepts --json"));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(format!(
            "pin id `{id}` must contain only letters, digits, `-` and `_`"
        ));
    }
    Ok(())
}

fn value<I>(flag: &str, inline: Option<String>, iter: &mut I) -> Result<String, String>
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

fn registry_path(state: &Path) -> PathBuf {
    state.join(REGISTRY)
}

fn pins_dir(state: &Path) -> PathBuf {
    state.join(PINS_DIR)
}

fn load(state: &Path) -> Result<Vec<Pin>, String> {
    let path = registry_path(state);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read pin registry {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("invalid pin registry {}: {err}", path.display()))?;
    let version = value.get("version").and_then(Value::as_u64);
    if version != Some(VERSION) {
        return Err(format!("unsupported pin registry version {version:?}"));
    }
    value
        .get("pins")
        .and_then(Value::as_array)
        .ok_or_else(|| "pin registry is missing `pins`".to_owned())?
        .iter()
        .map(Pin::from_json)
        .collect()
}

fn save(state: &Path, pins: &[Pin]) -> Result<(), String> {
    std::fs::create_dir_all(state)
        .map_err(|err| format!("cannot create {}: {err}", state.display()))?;
    let value = json!({
        "version": VERSION,
        "pins": pins.iter().map(Pin::to_json).collect::<Vec<_>>(),
    });
    let mut text =
        serde_json::to_string_pretty(&value).map_err(|err| format!("cannot encode pins: {err}"))?;
    text.push('\n');
    let path = registry_path(state);
    let temporary = state.join(format!(".pins.tmp-{}", std::process::id()));
    std::fs::write(&temporary, text)
        .map_err(|err| format!("cannot write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|err| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace {}: {err}", path.display())
    })
}

/// Runs a pin command against the default state directory.
pub fn run(options: &PinOptions) -> Result<String, String> {
    let state = state_dir()?;
    run_with_state(&state, options)
}

/// Runs a pin command against an explicit state directory (used by tests).
pub fn run_with_state(state: &Path, options: &PinOptions) -> Result<String, String> {
    match options.command {
        PinCommand::Add => add(state, options),
        PinCommand::List => {
            let pins = load(state)?;
            if options.json {
                let value = json!({
                    "registry": registry_path(state).display().to_string(),
                    "count": pins.len(),
                    "pins": pins.iter().map(Pin::to_json).collect::<Vec<_>>(),
                });
                Ok(pretty(&value))
            } else if pins.is_empty() {
                Ok("no pins".to_owned())
            } else {
                Ok(pins
                    .iter()
                    .map(|pin| {
                        let state = match (pin.pid, &pin.viewer) {
                            (Some(pid), _) => match pid_alive(pid) {
                                true => format!("layer pid {pid}"),
                                false => format!("layer pid {pid} (not running)"),
                            },
                            (None, Some(viewer)) => viewer.clone(),
                            (None, None) => "not opened".to_owned(),
                        };
                        format!("{:<24}  {}  {state}", pin.id, pin.path.display())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
        PinCommand::Unpin => {
            let id = options
                .id
                .as_deref()
                .ok_or_else(|| "`pin unpin` needs a pin id".to_owned())?;
            let mut pins = load(state)?;
            let index = pins
                .iter()
                .position(|pin| pin.id == id)
                .ok_or_else(|| format!("no pin with id `{id}`"))?;
            let pin = pins.remove(index);
            if let Some(pid) = pin.pid {
                terminate(pid);
            }
            save(state, &pins)?;
            remove_pinned_copy(&pin.path);
            let value = json!({
                "id": pin.id,
                "path": pin.path.display().to_string(),
                "removed": true,
            });
            Ok(if options.json {
                pretty(&value)
            } else {
                format!("unpinned {} ({})", pin.id, pin.path.display())
            })
        }
    }
}

fn add(state: &Path, options: &PinOptions) -> Result<String, String> {
    let source = options
        .path
        .as_ref()
        .ok_or_else(|| "`pin` needs a path".to_owned())?;
    if !source.is_file() {
        return Err(format!("{} is not a file", source.display()));
    }
    let mut pins = load(state)?;
    let id = match &options.id {
        Some(id) => {
            validate_id(id)?;
            if pins.iter().any(|pin| pin.id == *id) {
                return Err(format!("a pin with id `{id}` already exists"));
            }
            id.clone()
        }
        None => unique_id(&pins),
    };
    let file_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("cannot use {} as a pin name", source.display()))?;
    let directory = pins_dir(state).join(&id);
    std::fs::create_dir_all(&directory)
        .map_err(|err| format!("cannot create {}: {err}", directory.display()))?;
    let stable = directory.join(file_name);
    if let Err(err) = std::fs::copy(source, &stable) {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(format!(
            "cannot copy {} to {}: {err}",
            source.display(),
            stable.display()
        ));
    }
    let pin = Pin {
        id: id.clone(),
        path: stable.clone(),
        source: Some(source.clone()),
        created_unix: now_unix(),
        viewer: None,
        pid: None,
    };
    pins.push(pin);
    let index = pins.len() - 1;
    if let Err(err) = save(state, &pins) {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(err);
    }

    if options.layer {
        let pid = match spawn_overlay(&stable) {
            Ok(pid) => pid,
            Err(err) => {
                pins.remove(index);
                save(state, &pins)?;
                remove_pinned_copy(&stable);
                return Err(err);
            }
        };
        pins[index].pid = Some(pid);
        if let Err(err) = save(state, &pins) {
            terminate(pid);
            pins.remove(index);
            let _ = save(state, &pins);
            remove_pinned_copy(&stable);
            return Err(err);
        }
    } else if !options.no_open {
        let viewer = options
            .viewer
            .clone()
            .or_else(|| std::env::var("LOOM_SHOT_VIEWER").ok())
            .unwrap_or_else(|| "xdg-open".to_owned());
        let mut command = Command::new(&viewer);
        command
            .arg(&stable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match spawn_viewer(&mut command) {
            Ok(_) => {
                pins[index].viewer = Some(viewer);
                save(state, &pins)?;
            }
            Err(err) => {
                // Roll back so a failed open does not leave a registered pin.
                pins.remove(index);
                save(state, &pins)?;
                remove_pinned_copy(&stable);
                return Err(format!(
                    "cannot launch pin viewer `{viewer}`: {err}; install it, set \
                     LOOM_SHOT_VIEWER, or pass --no-open"
                ));
            }
        }
    }

    let pin = &pins[index];
    let value = pin.to_json();
    Ok(if options.json {
        pretty(&value)
    } else if let Some(pid) = pin.pid {
        format!(
            "pinned {} as {} on the layer overlay (pid {pid})",
            pin.path.display(),
            pin.id
        )
    } else {
        match &pin.viewer {
            Some(viewer) => format!(
                "pinned {} as {} via {} ({})",
                pin.path.display(),
                pin.id,
                viewer,
                pin.source
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            ),
            None => format!(
                "pinned {} as {} (not opened; use `pin list` or open it manually)",
                pin.path.display(),
                pin.id
            ),
        }
    })
}

fn spawn_overlay(path: &Path) -> Result<u64, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err(
            "cannot launch layer pin: WAYLAND_DISPLAY is unset; run inside a Wayland \
             session or drop --layer"
                .to_owned(),
        );
    }
    let executable = std::env::current_exe()
        .map_err(|err| format!("cannot locate the loom-shot executable: {err}"))?;
    let child = Command::new(executable)
        .arg("pin-view")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot launch layer pin overlay: {err}"))?;
    Ok(u64::from(child.id()))
}

fn spawn_viewer(command: &mut Command) -> std::io::Result<std::process::Child> {
    // Package updates and freshly written helper scripts can leave the viewer
    // open for writing for a moment; ETXTBSY clears as soon as that closes.
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(err) if err.raw_os_error() == Some(26) && attempts < 5 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    }
}

fn pid_arg(pid: u64) -> Option<i32> {
    // Never pass 0 or -1: those signal a process group or every process.
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

fn pid_alive(pid: u64) -> bool {
    // Signal 0 only checks whether the process exists.
    pid_arg(pid).is_some_and(|pid| unsafe { libc::kill(pid, 0) == 0 })
}

fn terminate(pid: u64) {
    // Best effort: an already-dead (ESRCH) child is not an error.
    if let Some(pid) = pid_arg(pid) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

fn unique_id(pins: &[Pin]) -> String {
    let base = format!("pin-{}-{}", now_unix(), std::process::id());
    if !pins.iter().any(|pin| pin.id == base) {
        return base;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{base}-{suffix}");
        if !pins.iter().any(|pin| pin.id == candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn remove_pinned_copy(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn temp_state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-shot-pin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_image(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-shot-pin-img-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, b"png-bytes").unwrap();
        path
    }

    #[test]
    fn parses_add_list_unpin() {
        let PinCli::Run(options) = parse_cli(["a.png", "--id", "one", "--no-open"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.command, PinCommand::Add);
        assert_eq!(options.id.as_deref(), Some("one"));
        assert!(options.no_open);

        let PinCli::Run(options) = parse_cli(["list", "--json"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.command, PinCommand::List);
        assert!(options.json);

        let PinCli::Run(options) = parse_cli(["unpin", "one"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.command, PinCommand::Unpin);
        assert_eq!(options.id.as_deref(), Some("one"));

        assert_eq!(parse_cli(["--help"]).unwrap(), PinCli::Help);
    }

    #[test]
    fn parses_layer_flags() {
        let PinCli::Run(options) = parse_cli(["a.png", "--layer"]).unwrap() else {
            panic!("expected run");
        };
        assert!(options.layer);
        let PinCli::Run(options) = parse_cli(["a.png", "--layer", "--no-layer"]).unwrap() else {
            panic!("expected run");
        };
        assert!(!options.layer);
        for args in [
            vec!["list", "--layer"],
            vec!["unpin", "one", "--layer"],
            vec!["a.png", "--layer=1"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn registry_roundtrips_pid_and_tolerates_missing() {
        let state = temp_state("pid");
        let path = state.join("shot.png");
        std::fs::write(&path, b"x").unwrap();
        let registry = json!({
            "version": VERSION,
            "pins": [{"id": "old", "path": path.display().to_string(), "created_unix": 1}],
        });
        std::fs::write(
            registry_path(&state),
            serde_json::to_string(&registry).unwrap(),
        )
        .unwrap();
        let pins = load(&state).unwrap();
        assert_eq!(pins[0].pid, None);
        assert_eq!(pins[0].to_json()["pid"], Value::Null);
        assert_eq!(pins[0].to_json()["pid_alive"], false);

        let live = Pin {
            id: "new".to_owned(),
            path,
            source: None,
            created_unix: 2,
            viewer: None,
            pid: Some(u64::from(std::process::id())),
        };
        save(&state, std::slice::from_ref(&live)).unwrap();
        let pins = load(&state).unwrap();
        assert_eq!(pins, vec![live]);
        assert_eq!(pins[0].to_json()["pid_alive"], true);
    }

    #[test]
    fn unpin_tolerates_a_dead_pid() {
        let state = temp_state("dead-pid");
        let image = temp_image("dead-pid");
        let options = PinOptions {
            path: Some(image),
            no_open: true,
            ..PinOptions::default()
        };
        run_with_state(&state, &options).unwrap();
        let mut pins = load(&state).unwrap();
        pins[0].pid = Some(0x7fff_ffff);
        save(&state, &pins).unwrap();
        let unpin = PinOptions {
            command: PinCommand::Unpin,
            id: Some(pins[0].id.clone()),
            ..PinOptions::default()
        };
        run_with_state(&state, &unpin).unwrap();
        assert!(load(&state).unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_arguments() {
        for args in [
            vec![],
            vec!["list", "extra"],
            vec!["list", "--id", "x"],
            vec!["unpin"],
            vec!["unpin", "a", "b"],
            vec!["unpin", "a", "--no-open"],
            vec!["a.png", "--id", "bad id!"],
            vec!["--bogus"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn add_no_open_copies_to_a_stable_path_and_unpin_removes_it() {
        let state = temp_state("add");
        let image = temp_image("add");
        let options = PinOptions {
            path: Some(image.clone()),
            no_open: true,
            json: true,
            ..PinOptions::default()
        };
        let value: Value =
            serde_json::from_str(&run_with_state(&state, &options).unwrap()).unwrap();
        let stable = PathBuf::from(value["path"].as_str().unwrap());
        assert!(stable.exists());
        assert_ne!(stable, image);
        assert_eq!(std::fs::read(&stable).unwrap(), b"png-bytes");
        assert!(stable.starts_with(pins_dir(&state)));

        let list = PinOptions {
            command: PinCommand::List,
            json: true,
            ..PinOptions::default()
        };
        let value: Value = serde_json::from_str(&run_with_state(&state, &list).unwrap()).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["pins"][0]["source"], image.display().to_string());

        let unpin = PinOptions {
            command: PinCommand::Unpin,
            id: Some(value["pins"][0]["id"].as_str().unwrap().to_owned()),
            ..PinOptions::default()
        };
        run_with_state(&state, &unpin).unwrap();
        assert!(!stable.exists());
        let value: Value = serde_json::from_str(&run_with_state(&state, &list).unwrap()).unwrap();
        assert_eq!(value["count"], 0);
        assert!(run_with_state(&state, &unpin).is_err());
    }

    #[test]
    fn duplicate_ids_and_non_files_are_rejected() {
        let state = temp_state("duplicate");
        let image = temp_image("duplicate");
        let options = PinOptions {
            path: Some(image.clone()),
            id: Some("same".to_owned()),
            no_open: true,
            ..PinOptions::default()
        };
        run_with_state(&state, &options).unwrap();
        assert!(run_with_state(&state, &options).is_err());

        let missing = PinOptions {
            path: Some(state.join("missing.png")),
            no_open: true,
            ..PinOptions::default()
        };
        assert!(run_with_state(&state, &missing).is_err());
    }

    #[test]
    fn missing_viewer_rolls_back_the_pin() {
        let state = temp_state("viewer");
        let image = temp_image("viewer");
        let options = PinOptions {
            path: Some(image),
            viewer: Some("loom-shot-definitely-missing-viewer".to_owned()),
            ..PinOptions::default()
        };
        let err = run_with_state(&state, &options).unwrap_err();
        assert!(err.contains("cannot launch pin viewer"), "{err}");
        assert!(err.contains("--no-open"), "{err}");
        let value: Value = serde_json::from_str(
            &run_with_state(
                &state,
                &PinOptions {
                    command: PinCommand::List,
                    json: true,
                    ..PinOptions::default()
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(value["count"], 0);
        assert!(
            !pins_dir(&state).exists() || std::fs::read_dir(pins_dir(&state)).unwrap().count() == 0
        );
    }

    #[test]
    fn viewer_receives_the_stable_path() {
        let state = temp_state("open");
        let image = temp_image("open");
        let marker = state.join("opened");
        let viewer = state.join("viewer");
        std::fs::write(
            &viewer,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&viewer, std::fs::Permissions::from_mode(0o755)).unwrap();
        let options = PinOptions {
            path: Some(image),
            viewer: Some(viewer.display().to_string()),
            ..PinOptions::default()
        };
        run_with_state(&state, &options).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let opened = std::fs::read_to_string(&marker).unwrap();
        assert!(opened.starts_with(&pins_dir(&state).display().to_string()));
    }
}
