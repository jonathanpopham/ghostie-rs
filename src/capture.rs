//! capture — pluggable session-log ingestion.
//!
//! Single responsibility: parse an agent session log into one
//! provider-agnostic [`SessionRecord`], then distill it into memories: a
//! `session-summary` carrying provenance (harness / core / session id) plus
//! one memory per explicit `MEMORY <type>: ...` marker the session left
//! behind. Parsers are pluggable; the Claude Code JSONL reader is the first.
//!
//! Distillation here is deterministic and airgap-pure: it never calls a model.
//! Richer, model-driven distillation is a separate, feature-gated step (the
//! same "one impure node" discipline the rest of the stack follows), so the
//! shipped binary stays offline and byte-stable.

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::store::memory::{Memory, MemoryType};
use crate::store::{NewMemory, Store};
use crate::util::Clock;

/// The harness a session is assumed to come from when the log does not name
/// one and the caller does not override it.
pub const DEFAULT_HARNESS: &str = "claude-code";

/// A provider-agnostic distillation of one agent session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRecord {
    /// Where the session ran (`claude-code`, `hermes`, ...).
    pub harness: String,
    /// The model that drove it, when the log records one.
    pub core: Option<String>,
    /// The harness's session id, when present.
    pub session_id: Option<String>,
    /// The first user message, trimmed to one line: the task, heuristically.
    pub task: Option<String>,
    /// Count of user + assistant messages seen.
    pub message_count: usize,
    /// Explicit `MEMORY <type>: text` markers, in first-seen order.
    pub markers: Vec<(MemoryType, String)>,
}

/// Parse a Claude Code JSONL transcript. Tolerant: unparseable or
/// unrecognised lines are skipped, never fatal (a transcript is an external
/// artifact and may carry lines we do not model).
pub fn parse_claude_code(text: &str, harness: Option<&str>, core: Option<&str>) -> SessionRecord {
    let mut rec = SessionRecord {
        harness: harness.unwrap_or(DEFAULT_HARNESS).to_string(),
        core: core.map(str::to_string),
        ..SessionRecord::default()
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = json::parse(line) else {
            continue;
        };
        if rec.session_id.is_none()
            && let Some(sid) = v.get("sessionId").and_then(Value::as_str)
        {
            rec.session_id = Some(sid.to_string());
        }
        let msg = v.get("message");
        let role = msg
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .or_else(|| v.get("type").and_then(Value::as_str));
        if rec.core.is_none()
            && let Some(model) = msg.and_then(|m| m.get("model")).and_then(Value::as_str)
        {
            rec.core = Some(model.to_string());
        }
        let is_message = matches!(role, Some("user") | Some("assistant"));
        if is_message {
            rec.message_count += 1;
        }
        let text_content = msg
            .map(|m| collect_text(m.get("content")))
            .unwrap_or_default();
        if rec.task.is_none() && role == Some("user") && !text_content.trim().is_empty() {
            rec.task = Some(one_line(&text_content));
        }
        for marker in scan_markers(&text_content) {
            rec.markers.push(marker);
        }
    }
    rec
}

/// Extract the human-readable text from a `message.content` value: a bare
/// string, or the concatenation of the `text` blocks in a content array.
fn collect_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::new();
            for b in blocks {
                if b.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = b.get("text").and_then(Value::as_str)
                {
                    parts.push(t);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// First non-empty line, trimmed and capped so a title stays a title.
fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    cap_chars(line, 100)
}

/// Cap a string to `max` characters (not bytes), on a char boundary.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Scan text for `MEMORY <type>: body` markers (case-insensitive on the word
/// MEMORY and the type). `MEMORY: body` with no type defaults to a fact.
/// This is the deterministic way an agent or human flags something worth
/// keeping mid-session; capture harvests them.
fn scan_markers(text: &str) -> Vec<(MemoryType, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        let Some(after) = lower.strip_prefix("memory") else {
            continue;
        };
        // Recover the original-case remainder aligned to `after`.
        let rest_orig = &line[line.len() - after.len()..];
        // `MEMORY: body`  or  `MEMORY <type>: body`.
        let Some((head, body)) = rest_orig.split_once(':') else {
            continue;
        };
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let head = head.trim();
        let mtype = if head.is_empty() {
            MemoryType::Fact
        } else {
            match MemoryType::parse(&head.to_ascii_lowercase()) {
                Some(t) if t != MemoryType::SessionSummary => t,
                _ => continue, // unknown/reserved type word: not a marker
            }
        };
        out.push((mtype, body.to_string()));
    }
    out
}

/// Distill a session record into memories: always a `session-summary` (the
/// cross-provider breadcrumb: what you were doing, where, with which model),
/// plus one memory per marker, each linked back to the summary. Returns the
/// created memories in creation order.
pub fn capture(store: &Store, rec: &SessionRecord, clock: &dyn Clock) -> Result<Vec<Memory>> {
    let mut created = Vec::new();
    let session_id = rec
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let title = rec
        .task
        .clone()
        .unwrap_or_else(|| "agent session".to_string());
    let body = format!(
        "Session on {}{}. {} message(s).{}\n",
        rec.harness,
        rec.core
            .as_deref()
            .map(|c| format!(" via {c}"))
            .unwrap_or_default(),
        rec.message_count,
        rec.task
            .as_deref()
            .map(|t| format!(" Task: {t}"))
            .unwrap_or_default(),
    );
    let summary = store.create(
        &NewMemory {
            mtype: Some(MemoryType::SessionSummary),
            title,
            source: Some(format!("{}:{}", rec.harness, session_id)),
            harness: Some(rec.harness.clone()),
            core: rec.core.clone(),
            body,
            ..NewMemory::default()
        },
        clock,
    )?;
    let summary_id = summary.id.clone();
    created.push(summary);

    for (mtype, text) in &rec.markers {
        let m = store.create(
            &NewMemory {
                mtype: Some(*mtype),
                title: cap_chars(text, 100),
                harness: Some(rec.harness.clone()),
                core: rec.core.clone(),
                links: vec![summary_id.clone()],
                body: format!("{text}\n"),
                ..NewMemory::default()
            },
            clock,
        )?;
        created.push(m);
    }
    Ok(created)
}

/// Read a transcript file and capture it. `harness`/`core` override what the
/// log reports (a hook knows its own harness even when the log is terse).
pub fn capture_file(
    store: &Store,
    path: &str,
    harness: Option<&str>,
    core: Option<&str>,
    clock: &dyn Clock,
) -> Result<Vec<Memory>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        context: "reading session transcript".to_string(),
        path: path.to_string(),
        source: e,
    })?;
    let rec = parse_claude_code(&text, harness, core);
    capture(store, &rec, clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

    fn transcript() -> String {
        // A minimal Claude Code JSONL: a user task, an assistant reply that
        // drops a marker, and a model-bearing line.
        [
            r#"{"type":"user","sessionId":"abc123","message":{"role":"user","content":"help me pick a storage engine"}}"#,
            r#"{"type":"assistant","sessionId":"abc123","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"Let's go with DuckDB.\nMEMORY decision: chose DuckDB over Postgres for analytics"}]}}"#,
            r#"{"type":"summary","summary":"noise line we ignore"}"#,
        ]
        .join("\n")
    }

    #[test]
    fn parses_task_model_session_and_markers() {
        let rec = parse_claude_code(&transcript(), None, None);
        assert_eq!(rec.harness, "claude-code");
        assert_eq!(rec.session_id.as_deref(), Some("abc123"));
        assert_eq!(rec.core.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(rec.task.as_deref(), Some("help me pick a storage engine"));
        assert_eq!(rec.message_count, 2);
        assert_eq!(
            rec.markers,
            vec![(
                MemoryType::Decision,
                "chose DuckDB over Postgres for analytics".to_string()
            )]
        );
    }

    #[test]
    fn capture_writes_summary_and_marker_memories_linked() {
        let tmp = TempDir::new("capture");
        let store = Store::open(tmp.path());
        let rec = parse_claude_code(&transcript(), None, None);
        let created = capture(&store, &rec, &FixedClock(T0)).unwrap();
        assert_eq!(created.len(), 2, "one summary + one marker");
        let summary = &created[0];
        assert_eq!(summary.mtype, MemoryType::SessionSummary);
        assert_eq!(summary.source.as_deref(), Some("claude-code:abc123"));
        assert_eq!(summary.harness.as_deref(), Some("claude-code"));
        assert_eq!(summary.core.as_deref(), Some("claude-opus-4-8"));
        let marker = &created[1];
        assert_eq!(marker.mtype, MemoryType::Decision);
        assert!(
            marker.links.contains(&summary.id),
            "marker links the summary"
        );
        assert_eq!(marker.harness.as_deref(), Some("claude-code"));
    }

    #[test]
    fn capture_is_deterministic() {
        let a = TempDir::new("capture-det-a");
        let b = TempDir::new("capture-det-b");
        let rec = parse_claude_code(&transcript(), None, None);
        let sa = Store::open(a.path());
        let sb = Store::open(b.path());
        let ca = capture(&sa, &rec, &FixedClock(T0)).unwrap();
        let cb = capture(&sb, &rec, &FixedClock(T0)).unwrap();
        let ids_a: Vec<_> = ca.iter().map(|m| &m.id).collect();
        let ids_b: Vec<_> = cb.iter().map(|m| &m.id).collect();
        assert_eq!(ids_a, ids_b, "same transcript + clock -> same memory ids");
    }

    #[test]
    fn harness_override_wins_over_default() {
        let rec = parse_claude_code(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            Some("hermes"),
            Some("hermes-4-405b"),
        );
        assert_eq!(rec.harness, "hermes");
        assert_eq!(rec.core.as_deref(), Some("hermes-4-405b"));
    }

    #[test]
    fn markers_default_to_fact_and_reject_unknown_types() {
        let rec = parse_claude_code(
            &[
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"MEMORY: configs live in etc"}]}}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"MEMORY opinion: not a real type"}]}}"#,
            ]
            .join("\n"),
            None,
            None,
        );
        assert_eq!(
            rec.markers,
            vec![(MemoryType::Fact, "configs live in etc".to_string())],
            "bare MEMORY is a fact; unknown type is not a marker"
        );
    }
}
