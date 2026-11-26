//! Bounded capture history.
//!
//! Entries live in `$XDG_STATE_HOME/loom-shot/history.json` (falling back to
//! `~/.local/state/loom-shot/history.json`). Each successful capture appends
//! one entry per saved file, including the path, source, timestamp, dimensions,
//! HDR detection, annotation specs, and metadata sidecar path. The newest entry
//! is listed first. The log is bounded to [`MAX_ENTRIES`] entries.
//!
//! `--no-history` on a capture opts out. History is bookkeeping only: it never
//! stores pixels.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub const MAX_ENTRIES: usize = 200;
const VERSION: u64 = 1;

pub const USAGE: &str = r#"loom-shot history [list|show|prune|clear] [options]

  list                    List captures, newest first (default)
    --limit N             Show at most N entries
  show INDEX|PATH         Show one entry (INDEX 1 = newest)
  prune                   Remove old entries
    --keep N              Keep the newest N entries (default: the 200-entry bound)
    --older-than DAYS     Remove entries older than DAYS
    --missing             Remove entries whose image no longer exists
    --dry-run             Report what would be removed
  clear                   Remove every entry
    --dry-run             Report what would be removed
  --json                  Print JSON instead of a text summary
  --help                  Show this help

  History file: $XDG_STATE_HOME/loom-shot/history.json
                (~/.local/state/loom-shot/history.json when unset)
"#;

/// One saved capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub path: PathBuf,
    pub source: String,
    pub timestamp_unix: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
    pub hdr_requested: String,
    pub hdr_output: Option<String>,
    pub max_bpc: Option<u32>,
    pub annotations: Vec<String>,
    pub metadata: Option<PathBuf>,
}

impl HistoryEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.display().to_string(),
            "source": self.source,
            "timestamp_unix": self.timestamp_unix,
            "width": self.width,
            "height": self.height,
            "format": self.format,
            "hdr": {
                "requested": self.hdr_requested,
                "output": self.hdr_output,
                "max_bpc": self.max_bpc,
            },
            "annotations": self.annotations,
            "metadata": self.metadata.as_ref().map(|path| path.display().to_string()),
        })
    }

    fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "history entry must be an object".to_owned())?;
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "history entry is missing `path`".to_owned())?;
        let source = object
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| "history entry is missing `source`".to_owned())?;
        let timestamp_unix = object
            .get("timestamp_unix")
            .and_then(Value::as_u64)
            .ok_or_else(|| "history entry is missing `timestamp_unix`".to_owned())?;
        let hdr = object.get("hdr").and_then(Value::as_object);
        let annotations = object
            .get("annotations")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            path: PathBuf::from(path),
            source: source.to_owned(),
            timestamp_unix,
            width: object
                .get("width")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            height: object
                .get("height")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            format: object
                .get("format")
                .and_then(Value::as_str)
                .map(str::to_owned),
            hdr_requested: hdr
                .and_then(|hdr| hdr.get("requested"))
                .and_then(Value::as_str)
                .unwrap_or("keep")
                .to_owned(),
            hdr_output: hdr
                .and_then(|hdr| hdr.get("output"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            max_bpc: hdr
                .and_then(|hdr| hdr.get("max_bpc"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            annotations,
            metadata: object
                .get("metadata")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        })
    }
}

/// `$XDG_STATE_HOME/loom-shot`, then `~/.local/state/loom-shot`.
pub fn state_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("loom-shot"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/state/loom-shot"));
    }
    Err("cannot locate a state directory: XDG_STATE_HOME and HOME are unset".to_owned())
}

/// Default history file.
pub fn default_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("history.json"))
}

/// Appends an entry to the default history file and returns its path.
pub fn append(entry: HistoryEntry) -> Result<PathBuf, String> {
    let path = default_path()?;
    append_to(&path, entry)?;
    Ok(path)
}

/// Appends an entry to an explicit file, enforcing the bound.
pub fn append_to(path: &Path, entry: HistoryEntry) -> Result<(), String> {
    let mut entries = load_entries(path)?;
    entries.push(entry);
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    save_entries(path, &entries)
}

fn load_entries(path: &Path) -> Result<Vec<HistoryEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("cannot read history {}: {err}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("invalid history {}: {err}", path.display()))?;
    let version = value.get("version").and_then(Value::as_u64);
    if version != Some(VERSION) {
        return Err(format!(
            "unsupported history version {version:?} in {}",
            path.display()
        ));
    }
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("history {} is missing `entries`", path.display()))?;
    entries.iter().map(HistoryEntry::from_json).collect()
}

fn save_entries(path: &Path, entries: &[HistoryEntry]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let value = json!({
        "version": VERSION,
        "entries": entries.iter().map(HistoryEntry::to_json).collect::<Vec<_>>(),
    });
    let mut text = serde_json::to_string_pretty(&value)
        .map_err(|err| format!("cannot encode history: {err}"))?;
    text.push('\n');
    let temporary = path.with_file_name(format!(".history.tmp-{}", std::process::id()));
    std::fs::write(&temporary, text)
        .map_err(|err| format!("cannot write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|err| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace {}: {err}", path.display())
    })
}

/// Actions accepted by `loom-shot history`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    List,
    Show,
    Prune,
    Clear,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryOptions {
    pub action: HistoryAction,
    pub target: Option<String>,
    pub json: bool,
    pub limit: Option<usize>,
    pub keep: Option<usize>,
    pub older_than_days: Option<u64>,
    pub missing: bool,
    pub dry_run: bool,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            action: HistoryAction::List,
            target: None,
            json: false,
            limit: None,
            keep: None,
            older_than_days: None,
            missing: false,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HistoryCli {
    Run(HistoryOptions),
    Help,
}

/// Parses arguments after the `history` token.
pub fn parse_cli<I, S>(args: I) -> Result<HistoryCli, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into).peekable();
    let mut options = HistoryOptions::default();
    let mut action = None;
    while let Some(raw) = args.next() {
        let (flag, inline) = match raw.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (raw, None),
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(HistoryCli::Help),
            "--json" => {
                no_value(&flag, inline)?;
                options.json = true;
            }
            "--dry-run" => {
                no_value(&flag, inline)?;
                options.dry_run = true;
            }
            "--missing" => {
                no_value(&flag, inline)?;
                options.missing = true;
            }
            "--limit" => options.limit = Some(parse_usize(&flag, inline, &mut args)?),
            "--keep" => options.keep = Some(parse_usize(&flag, inline, &mut args)?),
            "--older-than" => {
                options.older_than_days = Some(parse_usize(&flag, inline, &mut args)? as u64)
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown history option `{other}`\n\n{USAGE}"));
            }
            other => {
                if action.is_some() {
                    if options.target.is_none() {
                        options.target = Some(other.to_owned());
                        continue;
                    }
                    return Err(format!("unexpected history argument `{other}`\n\n{USAGE}"));
                }
                action = match other {
                    "list" => Some(HistoryAction::List),
                    "show" => Some(HistoryAction::Show),
                    "prune" => Some(HistoryAction::Prune),
                    "clear" => Some(HistoryAction::Clear),
                    _ => {
                        return Err(format!(
                            "unknown history command `{other}` (expected list, show, prune or \
                             clear)\n\n{USAGE}"
                        ));
                    }
                };
            }
        }
    }
    options.action = action.unwrap_or(HistoryAction::List);
    validate(&options)?;
    Ok(HistoryCli::Run(options))
}

fn validate(options: &HistoryOptions) -> Result<(), String> {
    match options.action {
        HistoryAction::List => {
            if options.target.is_some() {
                return Err("`history list` takes no positional argument".to_owned());
            }
            if options.keep.is_some() || options.older_than_days.is_some() || options.missing {
                return Err(
                    "--keep/--older-than/--missing only apply to `history prune`".to_owned(),
                );
            }
            if options.dry_run {
                return Err("--dry-run only applies to `history prune` and `clear`".to_owned());
            }
        }
        HistoryAction::Show => {
            if options.target.is_none() {
                return Err("`history show` needs an index or a path".to_owned());
            }
            if options.limit.is_some()
                || options.keep.is_some()
                || options.older_than_days.is_some()
                || options.missing
                || options.dry_run
            {
                return Err("`history show` only accepts --json".to_owned());
            }
        }
        HistoryAction::Prune => {
            if options.target.is_some() {
                return Err("`history prune` takes no positional argument".to_owned());
            }
            if options.limit.is_some() {
                return Err("--limit only applies to `history list`".to_owned());
            }
        }
        HistoryAction::Clear => {
            if options.target.is_some() {
                return Err("`history clear` takes no positional argument".to_owned());
            }
            if options.limit.is_some()
                || options.keep.is_some()
                || options.older_than_days.is_some()
                || options.missing
            {
                return Err("`history clear` only accepts --dry-run and --json".to_owned());
            }
        }
    }
    Ok(())
}

fn parse_usize<I>(flag: &str, inline: Option<String>, iter: &mut I) -> Result<usize, String>
where
    I: Iterator<Item = String>,
{
    let raw = match inline {
        Some(value) if !value.is_empty() => value,
        Some(_) => return Err(format!("{flag} needs a number")),
        None => iter
            .next()
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("{flag} needs a number"))?,
    };
    raw.parse()
        .map_err(|_| format!("{flag} needs a non-negative number, got `{raw}`"))
}

fn no_value(flag: &str, inline: Option<String>) -> Result<(), String> {
    if inline.is_some() {
        Err(format!("{flag} does not take a value"))
    } else {
        Ok(())
    }
}

/// Runs a history command against the default file and returns printable text.
pub fn run(options: &HistoryOptions) -> Result<String, String> {
    let path = default_path()?;
    run_with_path(&path, options)
}

/// Runs a history command against an explicit file (used by tests).
pub fn run_with_path(path: &Path, options: &HistoryOptions) -> Result<String, String> {
    match options.action {
        HistoryAction::List => {
            let entries = load_entries(path)?;
            let limit = options.limit.unwrap_or(usize::MAX);
            let listed: Vec<&HistoryEntry> = entries.iter().rev().take(limit).collect();
            if options.json {
                let value = json!({
                    "path": path.display().to_string(),
                    "count": entries.len(),
                    "entries": listed.iter().map(|entry| entry.to_json()).collect::<Vec<_>>(),
                });
                Ok(pretty(&value))
            } else if listed.is_empty() {
                Ok("history is empty".to_owned())
            } else {
                let mut text = String::new();
                for (index, entry) in listed.iter().enumerate() {
                    text.push_str(&format!(
                        "{:>3}  {:>10}  {:<18}  {}\n",
                        index + 1,
                        entry.timestamp_unix,
                        entry.source,
                        entry.path.display()
                    ));
                }
                Ok(text.trim_end().to_owned())
            }
        }
        HistoryAction::Show => {
            let entries = load_entries(path)?;
            let target = options
                .target
                .as_deref()
                .ok_or_else(|| "`history show` needs an index or path".to_owned())?;
            let entry = if let Ok(index) = target.parse::<usize>() {
                if index == 0 {
                    return Err("history indexes start at 1 (newest)".to_owned());
                }
                entries
                    .iter()
                    .rev()
                    .nth(index - 1)
                    .ok_or_else(|| format!("history has no entry {index}"))?
            } else {
                entries
                    .iter()
                    .rev()
                    .find(|entry| entry.path == Path::new(target))
                    .ok_or_else(|| format!("no history entry for `{target}`"))?
            };
            if options.json {
                Ok(pretty(&entry.to_json()))
            } else {
                let mut text = format!(
                    "{}\nsource: {}\ntimestamp_unix: {}\n",
                    entry.path.display(),
                    entry.source,
                    entry.timestamp_unix
                );
                if let (Some(width), Some(height)) = (entry.width, entry.height) {
                    text.push_str(&format!("size: {width}x{height}\n"));
                }
                if let Some(format) = &entry.format {
                    text.push_str(&format!("format: {format}\n"));
                }
                text.push_str(&format!(
                    "hdr: requested={} output={} max_bpc={}\n",
                    entry.hdr_requested,
                    entry.hdr_output.as_deref().unwrap_or("-"),
                    entry
                        .max_bpc
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "-".to_owned())
                ));
                if entry.annotations.is_empty() {
                    text.push_str("annotations: none\n");
                } else {
                    text.push_str(&format!("annotations: {}\n", entry.annotations.join(", ")));
                }
                if let Some(metadata) = &entry.metadata {
                    text.push_str(&format!("metadata: {}\n", metadata.display()));
                }
                Ok(text.trim_end().to_owned())
            }
        }
        HistoryAction::Prune => {
            let entries = load_entries(path)?;
            let now = now_unix();
            let keep = options.keep.unwrap_or(MAX_ENTRIES);
            let total = entries.len();
            let mut kept = Vec::new();
            let mut removed = 0usize;
            for entry in entries {
                let mut drop = false;
                if options.missing && !entry.path.exists() {
                    drop = true;
                }
                if let Some(days) = options.older_than_days {
                    let cutoff = days.saturating_mul(86_400);
                    if now.saturating_sub(entry.timestamp_unix) > cutoff {
                        drop = true;
                    }
                }
                if drop {
                    removed += 1;
                } else {
                    kept.push(entry);
                }
            }
            if kept.len() > keep {
                removed += kept.len() - keep;
                kept.drain(..kept.len() - keep);
            }
            if !options.dry_run {
                save_entries(path, &kept)?;
            }
            let value = json!({
                "path": path.display().to_string(),
                "total": total,
                "removed": removed,
                "remaining": kept.len(),
                "dry_run": options.dry_run,
            });
            if options.json {
                Ok(pretty(&value))
            } else {
                Ok(format!(
                    "{} {} entries ({} removed, {} remaining)",
                    if options.dry_run {
                        "would prune"
                    } else {
                        "pruned"
                    },
                    total,
                    removed,
                    kept.len()
                ))
            }
        }
        HistoryAction::Clear => {
            let entries = load_entries(path)?;
            let removed = entries.len();
            if !options.dry_run {
                save_entries(path, &[])?;
            }
            let value = json!({
                "path": path.display().to_string(),
                "removed": removed,
                "dry_run": options.dry_run,
            });
            if options.json {
                Ok(pretty(&value))
            } else {
                Ok(format!(
                    "{} {removed} history entries",
                    if options.dry_run {
                        "would clear"
                    } else {
                        "cleared"
                    }
                ))
            }
        }
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

/// Convenience constructor used by capture.
pub fn entry(
    path: PathBuf,
    source: String,
    width: Option<u32>,
    height: Option<u32>,
    format: &'static str,
    annotations: Vec<String>,
) -> HistoryEntry {
    HistoryEntry {
        path,
        source,
        timestamp_unix: now_unix(),
        width,
        height,
        format: Some(format.to_owned()),
        hdr_requested: "keep".to_owned(),
        hdr_output: None,
        max_bpc: None,
        annotations,
        metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_history(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-shot-history-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.json")
    }

    fn sample(path: &str, timestamp: u64, source: &str) -> HistoryEntry {
        HistoryEntry {
            path: PathBuf::from(path),
            source: source.to_owned(),
            timestamp_unix: timestamp,
            width: Some(640),
            height: Some(480),
            format: Some("png".to_owned()),
            hdr_requested: "keep".to_owned(),
            hdr_output: Some("DP-1".to_owned()),
            max_bpc: Some(8),
            annotations: vec!["rect:1,2,3,4".to_owned()],
            metadata: Some(PathBuf::from(format!("{path}.json"))),
        }
    }

    #[test]
    fn parses_list_show_prune_clear() {
        let HistoryCli::Run(options) = parse_cli(["show", "2"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.action, HistoryAction::Show);
        assert_eq!(options.target.as_deref(), Some("2"));

        let HistoryCli::Run(options) = parse_cli([
            "prune",
            "--keep",
            "5",
            "--older-than=7",
            "--missing",
            "--json",
        ])
        .unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.action, HistoryAction::Prune);
        assert_eq!(options.keep, Some(5));
        assert_eq!(options.older_than_days, Some(7));
        assert!(options.missing);
        assert!(options.json);

        let HistoryCli::Run(options) = parse_cli(["clear", "--dry-run"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.action, HistoryAction::Clear);
        assert!(options.dry_run);

        let HistoryCli::Run(options) = parse_cli(Vec::<String>::new()).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(options.action, HistoryAction::List);
        assert_eq!(parse_cli(["--help"]).unwrap(), HistoryCli::Help);
    }

    #[test]
    fn rejects_invalid_combinations() {
        for args in [
            vec!["show"],
            vec!["show", "1", "--keep", "2"],
            vec!["list", "--keep", "2"],
            vec!["clear", "--missing"],
            vec!["prune", "extra"],
            vec!["bogus"],
            vec!["--bogus"],
        ] {
            assert!(parse_cli(args.clone()).is_err(), "accepted {args:?}");
        }
    }

    #[test]
    fn appends_lists_and_shows_entries_newest_first() {
        let path = temp_history("list");
        append_to(&path, sample("/tmp/a.png", 10, "screen")).unwrap();
        append_to(&path, sample("/tmp/b.png", 20, "window")).unwrap();
        append_to(&path, sample("/tmp/c.png", 30, "screen:DP-1")).unwrap();

        let options = HistoryOptions {
            json: true,
            ..HistoryOptions::default()
        };
        let text = run_with_path(&path, &options).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["count"], 3);
        assert_eq!(value["entries"][0]["path"], "/tmp/c.png");
        assert_eq!(value["entries"][2]["path"], "/tmp/a.png");
        assert_eq!(value["entries"][0]["hdr"]["output"], "DP-1");
        assert_eq!(value["entries"][0]["annotations"][0], "rect:1,2,3,4");

        let mut show = HistoryOptions {
            action: HistoryAction::Show,
            target: Some("2".to_owned()),
            json: true,
            ..HistoryOptions::default()
        };
        let value: Value = serde_json::from_str(&run_with_path(&path, &show).unwrap()).unwrap();
        assert_eq!(value["path"], "/tmp/b.png");
        show.target = Some("/tmp/c.png".to_owned());
        let value: Value = serde_json::from_str(&run_with_path(&path, &show).unwrap()).unwrap();
        assert_eq!(value["source"], "screen:DP-1");
        show.target = Some("9".to_owned());
        assert!(run_with_path(&path, &show).is_err());
    }

    #[test]
    fn history_is_bounded() {
        let path = temp_history("bound");
        for index in 0..(MAX_ENTRIES + 5) {
            append_to(
                &path,
                sample(&format!("/tmp/{index}.png"), index as u64, "screen"),
            )
            .unwrap();
        }
        let entries = load_entries(&path).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries.first().unwrap().timestamp_unix, 5);
        assert_eq!(
            entries.last().unwrap().timestamp_unix,
            (MAX_ENTRIES + 4) as u64
        );
    }

    #[test]
    fn prunes_by_keep_age_and_missing_files() {
        let path = temp_history("prune");
        let dir = path.parent().unwrap();
        let existing = dir.join("existing.png");
        std::fs::write(&existing, b"png").unwrap();
        append_to(&path, sample(&existing.display().to_string(), 1, "screen")).unwrap();
        append_to(
            &path,
            sample(
                &dir.join("missing.png").display().to_string(),
                now_unix(),
                "screen",
            ),
        )
        .unwrap();
        let newer = dir.join("newer.png");
        std::fs::write(&newer, b"png").unwrap();
        append_to(
            &path,
            sample(&newer.display().to_string(), now_unix(), "screen"),
        )
        .unwrap();

        let options = HistoryOptions {
            action: HistoryAction::Prune,
            missing: true,
            older_than_days: Some(1),
            dry_run: true,
            json: true,
            ..HistoryOptions::default()
        };
        let value: Value = serde_json::from_str(&run_with_path(&path, &options).unwrap()).unwrap();
        assert_eq!(value["removed"], 2);
        assert_eq!(value["remaining"], 1);
        assert_eq!(load_entries(&path).unwrap().len(), 3);

        let options = HistoryOptions {
            action: HistoryAction::Prune,
            keep: Some(0),
            ..HistoryOptions::default()
        };
        run_with_path(&path, &options).unwrap();
        assert!(load_entries(&path).unwrap().is_empty());

        let options = HistoryOptions {
            action: HistoryAction::Clear,
            ..HistoryOptions::default()
        };
        append_to(&path, sample("/tmp/x.png", 1, "screen")).unwrap();
        run_with_path(&path, &options).unwrap();
        assert!(load_entries(&path).unwrap().is_empty());
    }

    #[test]
    fn corrupt_history_is_an_error_not_a_silent_reset() {
        let path = temp_history("corrupt");
        std::fs::write(&path, "{ nope").unwrap();
        assert!(load_entries(&path).is_err());
        let options = HistoryOptions::default();
        assert!(run_with_path(&path, &options).is_err());
    }
}
