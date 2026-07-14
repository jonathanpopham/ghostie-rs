//! codex — auto-capture Codex sessions when a turn completes.
//!
//! Codex has no pre-prompt hook, but it has a `notify` mechanism: a program
//! it spawns after each agent turn, passing a single JSON string describing the
//! event as the LAST argv argument (NOT stdin). Verified against codex 0.144.3:
//! the event is `{"type":"agent-turn-complete", "thread-id":..., "turn-id":...,
//! "cwd":..., "input-messages":[...], "last-assistant-message":...}` and it does
//! NOT carry a transcript path. So capture must find the just-finished rollout
//! itself: Codex writes rollout JSONL to `~/.codex/sessions/YYYY/MM/DD/
//! rollout-*.jsonl` (live) and `~/.codex/archived_sessions/rollout-*.jsonl`
//! (older), and the newest-by-mtime file is the session that just ran.
//!
//! Everything here is std-only and deterministic; the actual capture reuses
//! [`crate::capture::capture_file`] with `Format::Codex` + harness `codex`, so a
//! repeated notify (agent-turn-complete fires per turn) is idempotent — capture
//! dedups by `harness:session_id`.

use crate::capture::{self, Format};
use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::store::Store;
use crate::store::memory::Memory;
use crate::util::Clock;
use std::path::{Path, PathBuf};

/// Locate the Codex home: `$CODEX_HOME` when set, else `$HOME/.codex`.
pub fn codex_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        let p = PathBuf::from(dir);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| Error::Usage {
        message: "cannot locate $HOME to find ~/.codex".to_string(),
    })?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Path to the Codex config file under a given home.
pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// Is `name` a Codex rollout file (`rollout-*.jsonl`)?
fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// Recursively collect every `rollout-*.jsonl` under `dir` (bounded depth so a
/// pathological tree cannot spin). Silently skips unreadable entries.
fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_rollouts(&path, depth - 1, out);
        } else if ft.is_file()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_rollout(name)
        {
            out.push(path);
        }
    }
}

/// Find the most-recently-modified `rollout-*.jsonl` under `home` (searching
/// both `sessions/` recursively and `archived_sessions/`). Ties break on the
/// path (lexically greatest wins), which for rollout files means the latest
/// timestamp in the name, so the result is deterministic. Returns `None` when
/// no rollout exists.
pub fn newest_rollout(home: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    // `sessions/` nests YYYY/MM/DD; `archived_sessions/` is flat. Cap depth.
    collect_rollouts(&home.join("sessions"), 8, &mut candidates);
    collect_rollouts(&home.join("archived_sessions"), 2, &mut candidates);

    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for path in candidates {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else { continue };
        match &best {
            Some((bt, bp)) if (mtime, &path) <= (*bt, bp) => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// A parsed Codex `notify` event. We only need the working directory (to scope
/// the captured memory to its project); everything else is informational.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyEvent {
    /// The event type, e.g. `agent-turn-complete`.
    pub kind: Option<String>,
    /// The session working directory, when Codex reports it.
    pub cwd: Option<String>,
}

/// Parse the notify event from the argv last-arg (Codex's real transport) and,
/// failing that, from stdin. Returns `None` only when neither is valid JSON;
/// unknown-shape JSON yields an empty event (we still capture the newest
/// rollout — the event body is a hint, not a requirement).
pub fn parse_notify(arg: Option<&str>, stdin: &str) -> Option<NotifyEvent> {
    let raw = arg
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| Some(stdin.trim()).filter(|s| !s.is_empty()))?;
    let v = json::parse(raw).ok()?;
    let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(NotifyEvent {
        kind: get("type"),
        // Newer Codex uses hyphenated keys; accept the underscore spelling too.
        cwd: get("cwd"),
    })
}

/// Derive a retrieval scope (`project:<name>`) from a working directory, so a
/// Codex capture is confined to its project the same way the Claude hook is.
pub fn scope_from_cwd(cwd: Option<&str>) -> Option<String> {
    let name = Path::new(cwd?).file_name().and_then(|n| n.to_str())?;
    if name.is_empty() {
        None
    } else {
        Some(format!("project:{name}"))
    }
}

/// The notify capture, factored out so it can be tested against an injected
/// `home` without touching the user's real `~/.codex`. Parses the notify event
/// (for scope), finds the newest rollout under `home`, and captures it as
/// Codex. Returns the created memories (empty when nothing new — already
/// captured, or no rollout found).
pub fn run_notify_capture(
    store: &Store,
    home: &Path,
    notify_arg: Option<&str>,
    stdin: &str,
    clock: &dyn Clock,
) -> Result<Vec<Memory>> {
    let event = parse_notify(notify_arg, stdin).unwrap_or_default();
    let scope = scope_from_cwd(event.cwd.as_deref());
    let Some(path) = newest_rollout(home) else {
        return Ok(Vec::new());
    };
    capture::capture_file(
        store,
        &path.display().to_string(),
        Some(Format::Codex),
        Some("codex"),
        None,
        scope.as_deref(),
        clock,
    )
}

// ---------------------------------------------------------------------------
// config.toml notify installer — careful, line-based, std-only (no toml crate).
// ---------------------------------------------------------------------------

/// The argv array ghostie installs as Codex's `notify` program.
pub fn notify_argv(store_root: &Path, do_sync: bool) -> Vec<String> {
    let mut v = vec![
        "ghostie".to_string(),
        "--store".to_string(),
        store_root.display().to_string(),
        "hook".to_string(),
        "run".to_string(),
        "capture".to_string(),
        "--codex-notify".to_string(),
    ];
    if do_sync {
        v.push("--sync".to_string());
    }
    v
}

/// The marker that identifies a `notify` line as ours.
const NOTIFY_MARKER: &str = "--codex-notify";

/// TOML-quote a string as a basic string (double quotes, backslash escapes).
fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Serialize an argv into a single-line TOML array: `["a", "b", ...]`.
fn toml_array(argv: &[String]) -> String {
    let parts: Vec<String> = argv.iter().map(|s| toml_quote(s)).collect();
    format!("[{}]", parts.join(", "))
}

/// Where a top-level `notify = ...` sits, and whether we can safely edit it.
enum NotifyLine {
    /// No top-level `notify` key exists.
    Absent,
    /// A clean single-line `notify = [...]` at index `idx`; `ours` is true when
    /// it already carries our marker.
    Single { idx: usize, ours: bool },
    /// A `notify` key we cannot safely rewrite (multi-line array, non-array
    /// value, or a foreign single-line one we refuse to clobber).
    Unsafe,
}

/// Scan `lines` for a top-level `notify` key (before the first `[table]`
/// header). Only a clean single-line array is safe to auto-edit.
fn find_notify(lines: &[&str]) -> NotifyLine {
    for (idx, raw) in lines.iter().enumerate() {
        let line = raw.trim_start();
        // A `[table]` / `[[array-of-tables]]` header ends the top-level scope.
        if line.starts_with('[') {
            return NotifyLine::Absent;
        }
        // Match a bare `notify` key: `notify` optionally spaced, then `=`.
        let Some(rest) = line.strip_prefix("notify") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(value) = rest.strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let ours = raw.contains(NOTIFY_MARKER);
        // Safe only if it is a single-line array literal on this line.
        if value.starts_with('[') && value.ends_with(']') && value.len() >= 2 {
            return NotifyLine::Single { idx, ours };
        }
        return NotifyLine::Unsafe;
    }
    NotifyLine::Absent
}

/// Outcome of a config install attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigReport {
    /// The config file targeted.
    pub path: PathBuf,
    /// A backup (`config.toml.ghostie-bak`) was written.
    pub backed_up: bool,
    /// The edit was applied to the file. When false, `manual_line` carries the
    /// exact line for the user to paste (a foreign or multi-line `notify` was
    /// found and we refuse to clobber it).
    pub applied: bool,
    /// The `notify = [...]` line to paste when `applied` is false.
    pub manual_line: Option<String>,
}

/// Install (or update, idempotently) the ghostie `notify` line in the Codex
/// config at `path`, preserving every other key. Backs up an existing file
/// first. If a foreign or multi-line `notify` is present we do NOT overwrite
/// it; instead we return `applied=false` with the exact line to paste.
pub fn install_notify_at(path: &Path, argv: &[String]) -> Result<ConfigReport> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let notify_line = format!("notify = {}", toml_array(argv));

    let lines: Vec<&str> = raw.split('\n').collect();
    let decision = find_notify(&lines);

    let new_raw = match decision {
        // A foreign or multi-line notify: refuse to clobber it, hand the user
        // the exact line to paste instead.
        NotifyLine::Unsafe | NotifyLine::Single { ours: false, .. } => {
            return Ok(ConfigReport {
                path: path.to_path_buf(),
                backed_up: false,
                applied: false,
                manual_line: Some(notify_line),
            });
        }
        // Ours already: replace in place (idempotent, updates flags).
        NotifyLine::Single { idx, ours: true } => {
            let mut out = lines.clone();
            out[idx] = &notify_line;
            out.join("\n")
        }
        NotifyLine::Absent => {
            // Insert as a new top-level key. Prepend so it is unambiguously
            // above any `[table]` header (top-level scope).
            if raw.trim().is_empty() {
                format!("{notify_line}\n")
            } else {
                format!("{notify_line}\n{raw}")
            }
        }
    };

    let backed_up = !raw.trim().is_empty();
    if backed_up {
        let bak = backup_path(path);
        std::fs::write(&bak, &raw).map_err(|e| Error::Io {
            context: "backing up Codex config.toml".to_string(),
            path: bak.display().to_string(),
            source: e,
        })?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            context: "creating Codex config directory".to_string(),
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    std::fs::write(path, &new_raw).map_err(|e| Error::Io {
        context: "writing Codex config.toml".to_string(),
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(ConfigReport {
        path: path.to_path_buf(),
        backed_up,
        applied: true,
        manual_line: None,
    })
}

/// Where the config backup is written.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config.toml".to_string());
    name.push_str(".ghostie-bak");
    path.with_file_name(name)
}

/// Is ghostie's `notify` line currently installed in the config at `path`?
pub fn status_notify_at(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let lines: Vec<&str> = raw.split('\n').collect();
    matches!(find_notify(&lines), NotifyLine::Single { ours: true, .. })
}

/// Remove ghostie's `notify` line from the config at `path`, leaving every
/// other key intact. Returns whether a line was removed. A foreign `notify` is
/// never touched.
pub fn uninstall_notify_at(path: &Path) -> Result<bool> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let lines: Vec<&str> = raw.split('\n').collect();
    let idx = match find_notify(&lines) {
        NotifyLine::Single { idx, ours: true } => idx,
        _ => return Ok(false),
    };
    let bak = backup_path(path);
    std::fs::write(&bak, &raw).map_err(|e| Error::Io {
        context: "backing up Codex config.toml".to_string(),
        path: bak.display().to_string(),
        source: e,
    })?;
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx)
        .map(|(_, l)| *l)
        .collect();
    let new_raw = kept.join("\n");
    std::fs::write(path, &new_raw).map_err(|e| Error::Io {
        context: "writing Codex config.toml".to_string(),
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::memory::MemoryType;
    use crate::store::testutil::TempDir;
    use crate::util::FixedClock;
    use std::io::Write;

    const T0: i64 = 1_783_944_000;

    fn write_rollout(path: &Path, session_id: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        writeln!(
            f,
            r#"{{"type":"session_meta","payload":{{"id":"{session_id}","cwd":"/tmp/proj","cli_version":"0.144.3"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"port the ingest service"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"turn_context","payload":{{"cwd":"/tmp/proj","model":"gpt-5.6-sol"}}}}"#
        )
        .unwrap();
    }

    #[test]
    fn newest_rollout_picks_the_most_recent() {
        let tmp = TempDir::new("codex-newest");
        let home = tmp.path();
        // Older, in archived_sessions (written first → older mtime).
        let old = home.join("archived_sessions/rollout-2025-09-30T15-48-11-aaaa.jsonl");
        write_rollout(&old, "old-session");
        // Newer, in sessions/YYYY/MM/DD (written second; also lexically greater
        // so the mtime tiebreak agrees).
        let new = home.join("sessions/2026/07/14/rollout-2026-07-14T06-39-00-bbbb.jsonl");
        write_rollout(&new, "new-session");

        let got = newest_rollout(home).expect("finds a rollout");
        assert_eq!(
            got, new,
            "the newest (by mtime, tiebreak path) rollout is chosen"
        );
    }

    #[test]
    fn newest_rollout_none_when_empty() {
        let tmp = TempDir::new("codex-empty");
        assert_eq!(newest_rollout(tmp.path()), None);
    }

    #[test]
    fn parse_notify_reads_argv_then_stdin() {
        let arg = r#"{"type":"agent-turn-complete","cwd":"/home/x/proj","last-assistant-message":"done"}"#;
        let ev = parse_notify(Some(arg), "").expect("parses argv json");
        assert_eq!(ev.kind.as_deref(), Some("agent-turn-complete"));
        assert_eq!(ev.cwd.as_deref(), Some("/home/x/proj"));
        // Falls back to stdin when argv is empty.
        let ev2 = parse_notify(None, arg).expect("parses stdin json");
        assert_eq!(ev2.cwd.as_deref(), Some("/home/x/proj"));
        // Neither present → None.
        assert!(parse_notify(None, "").is_none());
        // Unknown-shape JSON still yields an (empty) event.
        let ev3 = parse_notify(Some("{}"), "").expect("valid json, empty event");
        assert_eq!(ev3.cwd, None);
    }

    #[test]
    fn scope_from_cwd_derives_project() {
        assert_eq!(
            scope_from_cwd(Some("/home/x/acme")).as_deref(),
            Some("project:acme")
        );
        assert_eq!(scope_from_cwd(None), None);
    }

    #[test]
    fn run_notify_capture_captures_newest_as_codex() {
        let store_tmp = TempDir::new("codex-cap-store");
        let store = Store::open(store_tmp.path());
        let home_tmp = TempDir::new("codex-cap-home");
        let home = home_tmp.path();
        let roll = home.join("sessions/2026/07/14/rollout-2026-07-14T06-39-00-zzzz.jsonl");
        write_rollout(&roll, "cx-session-1");

        let arg = r#"{"type":"agent-turn-complete","cwd":"/tmp/proj"}"#;
        let created = run_notify_capture(&store, home, Some(arg), "", &FixedClock(T0)).unwrap();
        assert!(!created.is_empty(), "captured something");
        assert_eq!(created[0].mtype, MemoryType::SessionSummary);
        assert_eq!(created[0].harness.as_deref(), Some("codex"));
        assert_eq!(
            created[0].scope.as_deref(),
            Some("project:proj"),
            "scope derived from notify cwd"
        );

        // Idempotent: a repeated notify (agent-turn-complete fires per turn)
        // creates nothing new for the same session.
        let again = run_notify_capture(&store, home, Some(arg), "", &FixedClock(T0)).unwrap();
        assert!(
            again.is_empty(),
            "second notify is a no-op for same session"
        );
    }

    #[test]
    fn run_notify_capture_no_rollout_is_noop() {
        let store_tmp = TempDir::new("codex-cap-none");
        let store = Store::open(store_tmp.path());
        let home_tmp = TempDir::new("codex-home-none");
        let created =
            run_notify_capture(&store, home_tmp.path(), Some("{}"), "", &FixedClock(T0)).unwrap();
        assert!(created.is_empty(), "no rollout → nothing captured");
    }

    #[test]
    fn install_notify_inserts_and_preserves_config() {
        let tmp = TempDir::new("codex-install");
        let cfg = tmp.path().join("config.toml");
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(
            &cfg,
            "model = \"gpt-5.6-sol\"\nmodel_reasoning_effort = \"xhigh\"\n\n[features]\ngoals = true\n",
        )
        .unwrap();

        let argv = notify_argv(Path::new("/store/root"), true);
        let rep = install_notify_at(&cfg, &argv).unwrap();
        assert!(rep.applied, "clean install applied");
        assert!(rep.backed_up, "existing config backed up");
        assert!(backup_path(&cfg).exists());

        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("model = \"gpt-5.6-sol\""), "kept model key");
        assert!(text.contains("[features]"), "kept table");
        assert!(text.contains("goals = true"), "kept nested key");
        assert!(text.contains(NOTIFY_MARKER), "notify installed");
        assert!(text.contains("--sync"), "sync flag baked in");
        assert!(status_notify_at(&cfg), "status reports installed");

        // Idempotent: reinstall updates in place, does not duplicate.
        let argv2 = notify_argv(Path::new("/store/root"), false);
        install_notify_at(&cfg, &argv2).unwrap();
        let text2 = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(
            text2.matches(NOTIFY_MARKER).count(),
            1,
            "exactly one notify line after reinstall"
        );
        assert!(!text2.contains("--sync"), "reinstall dropped sync flag");

        // Uninstall removes ours, keeps the rest.
        assert!(uninstall_notify_at(&cfg).unwrap());
        let text3 = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text3.contains(NOTIFY_MARKER), "notify removed");
        assert!(text3.contains("[features]"), "other config intact");
        assert!(!status_notify_at(&cfg));
    }

    #[test]
    fn install_notify_refuses_to_clobber_foreign_notify() {
        let tmp = TempDir::new("codex-foreign");
        let cfg = tmp.path().join("config.toml");
        std::fs::create_dir_all(tmp.path()).unwrap();
        std::fs::write(&cfg, "notify = [\"my-own-notifier\", \"--flag\"]\n").unwrap();

        let argv = notify_argv(Path::new("/store/root"), false);
        let rep = install_notify_at(&cfg, &argv).unwrap();
        assert!(!rep.applied, "foreign notify not overwritten");
        assert!(
            rep.manual_line.as_deref().unwrap().contains(NOTIFY_MARKER),
            "hands back the exact line to paste"
        );
        // The user's config is untouched.
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("my-own-notifier"), "foreign notify preserved");
        assert!(!text.contains(NOTIFY_MARKER));
    }

    #[test]
    fn install_notify_into_empty_config() {
        let tmp = TempDir::new("codex-empty-cfg");
        let cfg = tmp.path().join("config.toml");
        let argv = notify_argv(Path::new("/s"), false);
        let rep = install_notify_at(&cfg, &argv).unwrap();
        assert!(rep.applied);
        assert!(!rep.backed_up, "no backup for a fresh file");
        assert!(status_notify_at(&cfg));
    }
}
