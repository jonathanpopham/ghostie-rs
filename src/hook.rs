//! hook — wire ghostie into a harness so recall and capture happen
//! automatically, plus the runners the harness invokes.
//!
//! The "two buttons" live here. `install` merges two entries into the
//! harness's settings (backing the file up first, never clobbering other
//! keys): a `UserPromptSubmit` hook that recalls relevant memories and injects
//! them, and a `SessionEnd` hook that captures the session (and optionally
//! syncs). `run_recall` / `run_capture` are what those entries call: they read
//! the harness's hook payload on stdin and act. Claude Code is the first
//! harness; the shape (payload in, action out) is provider-agnostic.

use crate::capture;
use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::recall::{RecallOpts, recall};
use crate::store::Store;
use crate::sync;
use crate::util::{Clock, format_rfc3339_utc};
use std::path::{Path, PathBuf};

/// Default token budget for the recall-on-prompt injection (bounded so it
/// never floods a prompt).
pub const DEFAULT_BUDGET: usize = 600;

/// Substrings that mark a hook command as ours (for idempotent install,
/// status, and uninstall).
const RECALL_TAIL: &str = "hook run recall";
const CAPTURE_TAIL: &str = "hook run capture";

// ---------------------------------------------------------------------------
// Runners: invoked by the harness with the hook payload on stdin.
// ---------------------------------------------------------------------------

/// Render recalled memories as compact injection text, confined to `scope`
/// (globally-scoped memories still surface).
fn render_context(
    store: &Store,
    prompt: &str,
    budget: usize,
    scope: Option<String>,
) -> Result<String> {
    let opts = RecallOpts {
        budget_tokens: Some(budget),
        diversify: true,
        scope,
        ..RecallOpts::default()
    };
    let res = recall(store, prompt, &opts)?;
    if res.hits.is_empty() {
        return Ok(String::new());
    }
    let mut s = String::from("Relevant memories (ghostie):\n");
    for h in &res.hits {
        s.push_str(&format!("- {}{}", h.title, h.provenance_tag()));
        if let Some(r) = &h.rationale {
            s.push_str(&format!(" (why: {r})"));
        }
        s.push('\n');
    }
    Ok(s)
}

/// UserPromptSubmit runner: recall against the prompt from the hook payload
/// and emit the `additionalContext` document Claude Code injects. Empty output
/// (no JSON) when there is nothing to add, so a no-hit prompt is untouched.
pub fn run_recall(store: &Store, stdin: &str, budget: usize) -> Result<String> {
    let payload = json::parse(stdin.trim()).ok();
    let prompt = payload
        .as_ref()
        .and_then(|v| v.get("prompt").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    if prompt.trim().is_empty() {
        return Ok(String::new());
    }
    let scope = scope_from_cwd(
        payload
            .as_ref()
            .and_then(|v| v.get("cwd").and_then(Value::as_str)),
    );
    let ctx = render_context(store, &prompt, budget, scope)?;
    if ctx.is_empty() {
        return Ok(String::new());
    }
    let out = Value::Object(vec![(
        "hookSpecificOutput".to_string(),
        Value::Object(vec![
            (
                "hookEventName".to_string(),
                Value::string("UserPromptSubmit"),
            ),
            ("additionalContext".to_string(), Value::string(ctx)),
        ]),
    )]);
    Ok(out.emit())
}

/// SessionEnd runner: capture the transcript named in the payload, and
/// optionally sync. Returns a short status line (SessionEnd ignores stdout;
/// the line is for logs and tests).
pub fn run_capture(
    store: &Store,
    stdin: &str,
    do_sync: bool,
    distill: bool,
    pending: bool,
    clock: &dyn Clock,
) -> Result<String> {
    let v = json::parse(stdin.trim()).unwrap_or(Value::Null);
    let Some(path) = v.get("transcript_path").and_then(Value::as_str) else {
        return Ok("ghostie: no transcript_path in payload; nothing captured".to_string());
    };
    let scope = scope_from_cwd(v.get("cwd").and_then(Value::as_str));
    // Trust gate: with --pending, capture into <store>/.pending for review, and
    // never sync (candidates are not memory until approved).
    let pending_store;
    let target: &Store = if pending {
        pending_store = crate::review::pending_store(store);
        &pending_store
    } else {
        store
    };
    // Auto-detect the format (a Claude Code SessionEnd transcript detects as
    // claude-code and stamps that harness).
    let created = if distill {
        // Feature-off builds get the heuristic distiller here (still airgap-
        // pure); a `--features distill` build may shell to the configured agent.
        let distiller = crate::distill::build_distiller(None);
        capture::capture_file_with_distill(
            target,
            path,
            None,
            None,
            None,
            scope.as_deref(),
            distiller.as_ref(),
            clock,
        )?
    } else {
        capture::capture_file(target, path, None, None, None, scope.as_deref(), clock)?
    };
    if pending {
        return Ok(format!(
            "ghostie: captured {} candidate(s) to the review queue",
            created.len()
        ));
    }
    let mut msg = format!("ghostie: captured {} memory(ies)", created.len());
    if do_sync && sync::git_available() {
        match sync::sync(store, clock) {
            Ok(_) => {
                let _ = std::fs::remove_file(sync_error_path(store));
                msg.push_str("; synced");
            }
            // SessionEnd ignores stdout, so a swallowed error would leave the
            // user silently un-backed-up. Persist a visible failure marker and
            // shout on stderr so it cannot rot unnoticed.
            Err(e) => {
                let note = format!(
                    "ghostie sync FAILED at {}: {e}",
                    format_rfc3339_utc(clock.now_epoch_seconds())
                );
                let _ = std::fs::write(sync_error_path(store), format!("{note}\n"));
                eprintln!("{note}");
                msg.push_str(&format!("; SYNC FAILED ({e})"));
            }
        }
    }
    Ok(msg)
}

/// Codex `notify` runner: Codex has no transcript path in its event, so find
/// the just-finished rollout ourselves and capture it. `notify_arg` is the JSON
/// Codex appends as the last argv argument (stdin is a fallback). Idempotent —
/// agent-turn-complete fires per turn, but capture dedups by session id.
/// Mirrors [`run_capture`]'s sync-failure surfacing when `do_sync`.
pub fn run_capture_codex_notify(
    store: &Store,
    notify_arg: Option<&str>,
    stdin: &str,
    do_sync: bool,
    clock: &dyn Clock,
) -> Result<String> {
    let home = crate::codex::codex_home()?;
    let created = crate::codex::run_notify_capture(store, &home, notify_arg, stdin, clock)?;
    let mut msg = format!("ghostie: captured {} memory(ies) from Codex", created.len());
    if do_sync && sync::git_available() {
        match sync::sync(store, clock) {
            Ok(_) => {
                let _ = std::fs::remove_file(sync_error_path(store));
                msg.push_str("; synced");
            }
            Err(e) => {
                let note = format!(
                    "ghostie sync FAILED at {}: {e}",
                    format_rfc3339_utc(clock.now_epoch_seconds())
                );
                let _ = std::fs::write(sync_error_path(store), format!("{note}\n"));
                eprintln!("{note}");
                msg.push_str(&format!("; SYNC FAILED ({e})"));
            }
        }
    }
    Ok(msg)
}

/// Where the last sync failure is recorded (cleared on the next success);
/// `hook status` surfaces it so a stalled backup is visible.
pub fn sync_error_path(store: &Store) -> std::path::PathBuf {
    store.root().join(".sync-error")
}

// ---------------------------------------------------------------------------
// Installer: merge our entries into Claude Code settings.json.
// ---------------------------------------------------------------------------

/// Where Claude Code user settings live.
pub fn claude_settings_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| Error::Usage {
        message: "cannot locate $HOME to find Claude Code settings".to_string(),
    })?;
    Ok(PathBuf::from(home).join(".claude").join("settings.json"))
}

/// Outcome of an install, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Settings file written.
    pub path: PathBuf,
    /// A backup was written alongside it.
    pub backed_up: bool,
}

/// POSIX single-quote a string so shell metacharacters (`$()`, backticks,
/// `"`, `\`) in a store path can never be interpreted by the hook shell. A
/// literal single quote becomes `'\''`.
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn recall_command(store_root: &Path, budget: usize) -> String {
    format!(
        "ghostie --store {} hook run recall --budget {budget}",
        sh_quote(&store_root.display().to_string())
    )
}

fn capture_command(store_root: &Path, do_sync: bool, distill: bool, review: bool) -> String {
    let mut c = format!(
        "ghostie --store {} hook run capture",
        sh_quote(&store_root.display().to_string())
    );
    // The trust gate wins over sync: pending candidates never sync until
    // approved, so --pending suppresses --sync on the wired command.
    if do_sync && !review {
        c.push_str(" --sync");
    }
    if distill {
        c.push_str(" --distill");
    }
    if review {
        c.push_str(" --pending");
    }
    c
}

/// Derive a retrieval scope from the hook payload's working directory, so
/// recall and capture are confined to the active project (globally-scoped
/// memories still pass). `~/work/acme` -> `project:acme`.
fn scope_from_cwd(cwd: Option<&str>) -> Option<String> {
    let name = std::path::Path::new(cwd?)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(format!("project:{name}"))
    }
}

/// Load a settings file into a top-level object's pairs (empty when the file
/// is absent or blank). Errors if present but not a JSON object.
fn load_settings(path: &Path) -> Result<(String, Vec<(String, Value)>)> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.trim().is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    match json::parse(raw.trim()) {
        Ok(Value::Object(pairs)) => Ok((raw, pairs)),
        Ok(_) => Err(Error::Invalid {
            origin: path.display().to_string(),
            message: "settings.json is not a JSON object".to_string(),
        }),
        Err(e) => Err(Error::Invalid {
            origin: path.display().to_string(),
            message: format!("existing settings.json is not valid JSON: {e}"),
        }),
    }
}

/// Get (creating if needed) the object stored under `key`.
fn ensure_object<'a>(
    pairs: &'a mut Vec<(String, Value)>,
    key: &str,
) -> &'a mut Vec<(String, Value)> {
    let idx = match pairs.iter().position(|(k, _)| k == key) {
        Some(i) => {
            if !matches!(pairs[i].1, Value::Object(_)) {
                pairs[i].1 = Value::Object(Vec::new());
            }
            i
        }
        None => {
            pairs.push((key.to_string(), Value::Object(Vec::new())));
            pairs.len() - 1
        }
    };
    match &mut pairs[idx].1 {
        Value::Object(o) => o,
        _ => unreachable!("just set to object"),
    }
}

/// Get (creating if needed) the array stored under `key`.
fn ensure_array<'a>(pairs: &'a mut Vec<(String, Value)>, key: &str) -> &'a mut Vec<Value> {
    let idx = match pairs.iter().position(|(k, _)| k == key) {
        Some(i) => {
            if !matches!(pairs[i].1, Value::Array(_)) {
                pairs[i].1 = Value::Array(Vec::new());
            }
            i
        }
        None => {
            pairs.push((key.to_string(), Value::Array(Vec::new())));
            pairs.len() - 1
        }
    };
    match &mut pairs[idx].1 {
        Value::Array(a) => a,
        _ => unreachable!("just set to array"),
    }
}

/// Does a hook entry carry a command containing `tail` (i.e. is it ours)?
fn entry_has_tail(entry: &Value, tail: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains(tail))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Replace any prior ghostie entry for this event with a fresh one (idempotent
/// install that also updates a changed command).
fn upsert_hook(hooks: &mut Vec<(String, Value)>, event: &str, command: &str, tail: &str) {
    let arr = ensure_array(hooks, event);
    arr.retain(|entry| !entry_has_tail(entry, tail));
    arr.push(Value::Object(vec![(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(vec![
            ("type".to_string(), Value::string("command")),
            ("command".to_string(), Value::string(command)),
        ])]),
    )]));
}

/// Install the recall + capture hooks into `settings_path`, baking the store
/// root into the commands. Backs up any existing file first.
pub fn install_at(
    settings_path: &Path,
    store_root: &Path,
    budget: usize,
    do_sync: bool,
    distill: bool,
    review: bool,
) -> Result<InstallReport> {
    let (raw, mut top) = load_settings(settings_path)?;
    let hooks = ensure_object(&mut top, "hooks");
    upsert_hook(
        hooks,
        "UserPromptSubmit",
        &recall_command(store_root, budget),
        RECALL_TAIL,
    );
    upsert_hook(
        hooks,
        "SessionEnd",
        &capture_command(store_root, do_sync, distill, review),
        CAPTURE_TAIL,
    );

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Io {
            context: "creating settings directory".to_string(),
            path: parent.display().to_string(),
            source: e,
        })?;
    }
    let backed_up = !raw.trim().is_empty();
    if backed_up {
        let bak = settings_path.with_extension("json.ghostie-bak");
        std::fs::write(&bak, &raw).map_err(|e| Error::Io {
            context: "backing up settings.json".to_string(),
            path: bak.display().to_string(),
            source: e,
        })?;
    }
    let mut out = Value::Object(top).emit();
    out.push('\n');
    std::fs::write(settings_path, out).map_err(|e| Error::Io {
        context: "writing settings.json".to_string(),
        path: settings_path.display().to_string(),
        source: e,
    })?;
    Ok(InstallReport {
        path: settings_path.to_path_buf(),
        backed_up,
    })
}

/// Are the recall / capture hooks currently installed in `settings_path`?
pub fn status_at(settings_path: &Path) -> Result<(bool, bool)> {
    let (_, top) = load_settings(settings_path)?;
    let hooks = top
        .iter()
        .find(|(k, _)| k == "hooks")
        .and_then(|(_, v)| v.as_object());
    let installed = |event: &str, tail: &str| -> bool {
        hooks
            .and_then(|h| h.iter().find(|(k, _)| k == event))
            .and_then(|(_, v)| v.as_array())
            .map(|arr| arr.iter().any(|e| entry_has_tail(e, tail)))
            .unwrap_or(false)
    };
    Ok((
        installed("UserPromptSubmit", RECALL_TAIL),
        installed("SessionEnd", CAPTURE_TAIL),
    ))
}

/// Remove our hooks from `settings_path`, leaving every other setting intact.
/// Returns how many entries were removed.
pub fn uninstall_at(settings_path: &Path) -> Result<usize> {
    let (raw, mut top) = load_settings(settings_path)?;
    if raw.trim().is_empty() {
        return Ok(0);
    }
    let mut removed = 0usize;
    if let Some((_, Value::Object(hooks))) = top.iter_mut().find(|(k, _)| k == "hooks") {
        for (event, tail) in [
            ("UserPromptSubmit", RECALL_TAIL),
            ("SessionEnd", CAPTURE_TAIL),
        ] {
            if let Some((_, Value::Array(arr))) = hooks.iter_mut().find(|(k, _)| k == event) {
                let before = arr.len();
                arr.retain(|e| !entry_has_tail(e, tail));
                removed += before - arr.len();
            }
        }
        // Drop now-empty event arrays.
        hooks.retain(|(_, v)| !matches!(v, Value::Array(a) if a.is_empty()));
    }
    // Drop the hooks object if we emptied it.
    top.retain(|(k, v)| !(k == "hooks" && matches!(v, Value::Object(o) if o.is_empty())));

    let mut out = Value::Object(top).emit();
    out.push('\n');
    std::fs::write(settings_path, out).map_err(|e| Error::Io {
        context: "writing settings.json".to_string(),
        path: settings_path.display().to_string(),
        source: e,
    })?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryType;
    use crate::store::testutil::TempDir;
    use crate::store::{NewMemory, Store};
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000;

    #[test]
    fn recall_runner_injects_context_for_a_prompt() {
        let tmp = TempDir::new("hook-recall");
        let store = Store::open(tmp.path());
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Decision),
                    title: "Chose DuckDB for analytics".to_string(),
                    harness: Some("hermes".to_string()),
                    core: Some("hermes-4-405b".to_string()),
                    rationale: Some("faster columnar scans".to_string()),
                    ..NewMemory::default()
                },
                &FixedClock(T0),
            )
            .unwrap();
        let stdin = r#"{"prompt":"what did we choose for duckdb analytics"}"#;
        let out = run_recall(&store, stdin, DEFAULT_BUDGET).unwrap();
        assert!(
            out.contains("additionalContext"),
            "emits injection JSON: {out}"
        );
        assert!(out.contains("Chose DuckDB"), "surfaces the memory");
        assert!(out.contains("hermes"), "carries provenance");
        // No-hit prompt injects nothing.
        let none = run_recall(&store, r#"{"prompt":"quantum unicorns"}"#, DEFAULT_BUDGET).unwrap();
        assert!(none.is_empty(), "no hits -> no injection");
    }

    #[test]
    fn capture_runner_reads_transcript_path() {
        let tmp = TempDir::new("hook-capture");
        let store = Store::open(tmp.path());
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"do a thing"}}"#,
        )
        .unwrap();
        let stdin = format!(r#"{{"transcript_path":"{}"}}"#, transcript.display());
        let msg = run_capture(&store, &stdin, false, false, false, &FixedClock(T0)).unwrap();
        assert!(msg.contains("captured 1"), "{msg}");
        let (mems, _) = store.list(&crate::store::ListFilter::default()).unwrap();
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0].mtype, MemoryType::SessionSummary);
    }

    #[test]
    fn capture_runner_with_distill_pulls_extra_memories() {
        let tmp = TempDir::new("hook-capture-distill");
        let store = Store::open(tmp.path());
        let transcript = tmp.path().join("t.jsonl");
        // Prose carrying a decision the session never flagged with a marker.
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"We chose DuckDB over Postgres for the analytics store today."}]}}"#,
        )
        .unwrap();
        let stdin = format!(r#"{{"transcript_path":"{}"}}"#, transcript.display());
        // distill on -> summary + a distilled decision (feature-off heuristic).
        let msg = run_capture(&store, &stdin, false, true, false, &FixedClock(T0)).unwrap();
        assert!(msg.contains("captured 2"), "{msg}");
        let (mems, _) = store.list(&crate::store::ListFilter::default()).unwrap();
        assert!(mems.iter().any(|m| m.mtype == MemoryType::Decision));
    }

    #[test]
    fn install_status_uninstall_round_trip_preserves_other_settings() {
        let tmp = TempDir::new("hook-install");
        let settings = tmp.path().join("settings.json");
        std::fs::create_dir_all(tmp.path()).unwrap();
        // Pre-existing unrelated settings must survive.
        std::fs::write(
            &settings,
            r#"{"model":"opus","permissions":{"allow":["Bash"]}}"#,
        )
        .unwrap();

        let store_root = tmp.path().join("store");
        let rep = install_at(&settings, &store_root, 800, true, false, false).unwrap();
        assert!(rep.backed_up, "existing settings backed up");
        assert!(settings.with_extension("json.ghostie-bak").exists());

        let (recall_on, capture_on) = status_at(&settings).unwrap();
        assert!(recall_on && capture_on, "both hooks report installed");

        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(
            text.contains("\"model\":\"opus\""),
            "unrelated setting kept"
        );
        assert!(text.contains("hook run recall --budget 800"));
        assert!(text.contains("hook run capture --sync"));

        // Idempotent: installing again does not duplicate entries.
        install_at(&settings, &store_root, 800, true, false, false).unwrap();
        let v = json::parse(std::fs::read_to_string(&settings).unwrap().trim()).unwrap();
        let ups = v
            .get("hooks")
            .and_then(|h| h.get("UserPromptSubmit"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(ups.len(), 1, "install is idempotent, not additive");

        // Uninstall removes ours and leaves the rest.
        let removed = uninstall_at(&settings).unwrap();
        assert_eq!(removed, 2);
        let (recall_on, capture_on) = status_at(&settings).unwrap();
        assert!(!recall_on && !capture_on);
        let text = std::fs::read_to_string(&settings).unwrap();
        assert!(
            text.contains("\"model\":\"opus\""),
            "unrelated setting still kept"
        );
    }
}
