//! capture — provider-agnostic session-log ingestion.
//!
//! Single responsibility: parse an agent session log from ANY harness into one
//! [`SessionRecord`], then distill it into memories: a `session-summary`
//! carrying provenance (harness / core / session id) plus one memory per
//! explicit `MEMORY <type>: ...` marker the session left behind.
//!
//! Three parsers, chosen by [`detect_format`] or forced with `--format`:
//! - **claude-code**: Claude Code JSONL (objects with `sessionId` + `message`).
//! - **codex**: structured JSONL from Codex and similar (role + content, with
//!   `input_text` / `output_text` blocks).
//! - **generic**: ANY text or markdown. The `MEMORY <type>:` marker convention
//!   is just text, so this makes capture work for a harness we have no bespoke
//!   parser for: paste a transcript, or point at a log, and markers are
//!   harvested with the first line taken as the task.
//!
//! Distillation is deterministic and airgap-pure: it never calls a model.
//! Richer, model-driven distillation is a separate feature-gated step, so the
//! shipped binary stays offline and byte-stable.

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::store::memory::{Memory, MemoryType};
use crate::store::{NewMemory, Store};
use crate::util::Clock;

/// A session-log format. `Auto` means "sniff it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Claude Code JSONL.
    ClaudeCode,
    /// Codex (and similar) structured JSONL.
    Codex,
    /// Any text / markdown (marker + first-line heuristic).
    Generic,
}

impl Format {
    /// Parse a `--format` value; `auto` returns `None` (detect at read time).
    pub fn parse(s: &str) -> Option<Option<Format>> {
        match s {
            "auto" => Some(None),
            "claude-code" | "claude" => Some(Some(Format::ClaudeCode)),
            "codex" => Some(Some(Format::Codex)),
            "generic" | "text" => Some(Some(Format::Generic)),
            _ => None,
        }
    }

    /// The harness label a format implies when the caller does not override it.
    pub fn default_harness(self) -> &'static str {
        match self {
            Format::ClaudeCode => "claude-code",
            Format::Codex => "codex",
            Format::Generic => "unknown",
        }
    }
}

/// A provider-agnostic distillation of one agent session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRecord {
    /// Where the session ran (`claude-code`, `codex`, `hermes`, ...).
    pub harness: String,
    /// The model that drove it, when the log records one.
    pub core: Option<String>,
    /// The harness's session id, when present.
    pub session_id: Option<String>,
    /// The first user message, trimmed to one line: the task, heuristically.
    pub task: Option<String>,
    /// Count of user + assistant messages seen.
    pub message_count: usize,
    /// Retrieval scope to stamp on captured memories (`project:<name>`), so a
    /// hook-driven capture in one project does not later leak into another.
    pub scope: Option<String>,
    /// Explicit `MEMORY <type>: text` markers, in first-seen order.
    pub markers: Vec<(MemoryType, String)>,
}

/// Sniff the format from the first lines: JSONL with `sessionId` is Claude
/// Code; other structured JSONL (role/message/content) is Codex-style; a
/// non-JSON line means plain text, so Generic.
pub fn detect_format(text: &str) -> Format {
    let mut saw_structured = false;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(25)
    {
        match json::parse(line) {
            Ok(v) => {
                if v.get("sessionId").is_some() {
                    return Format::ClaudeCode;
                }
                // Real Codex rollout: `{type: session_meta|response_item|
                // turn_context|event_msg, payload: {...}}`.
                let kind = v.get("type").and_then(Value::as_str);
                if v.get("payload").is_some()
                    && matches!(
                        kind,
                        Some("session_meta")
                            | Some("response_item")
                            | Some("turn_context")
                            | Some("event_msg")
                    )
                {
                    return Format::Codex;
                }
                if v.get("message").is_some()
                    || v.get("role").is_some()
                    || v.get("content").is_some()
                {
                    saw_structured = true;
                }
            }
            Err(_) => return Format::Generic,
        }
    }
    if saw_structured {
        Format::Codex
    } else {
        Format::Generic
    }
}

/// Parse with an explicit format. `harness`/`core` override what the log
/// reports (a hook knows its own harness even when the log is terse).
pub fn parse(
    text: &str,
    format: Format,
    harness: Option<&str>,
    core: Option<&str>,
) -> SessionRecord {
    match format {
        Format::ClaudeCode => parse_structured(text, format, harness, core, &["sessionId"]),
        Format::Codex => parse_codex(text, harness, core),
        Format::Generic => parse_generic(text, format, harness, core),
    }
}

/// Codex injects `<user_instructions>` and `<environment_context>` as the
/// first "user" messages; skip them when choosing the task.
fn is_real_user_text(t: &str) -> bool {
    let s = t.trim_start();
    !s.is_empty()
        && !s.starts_with("<user_instructions>")
        && !s.starts_with("<environment_context>")
}

/// Parse a Codex rollout JSONL (verified against real `~/.codex` output).
/// Each line wraps a `payload`: `session_meta` carries the session id,
/// `turn_context` the model, and `response_item` a message (role + content
/// blocks of `input_text` / `output_text`). The duplicate `event_msg` /
/// `user_message` lines are ignored so a prompt is not counted twice. Lines
/// with a top-level `role`/`content` (simpler tools) are handled too.
fn parse_codex(text: &str, harness: Option<&str>, core: Option<&str>) -> SessionRecord {
    let mut rec = SessionRecord {
        harness: harness
            .unwrap_or(Format::Codex.default_harness())
            .to_string(),
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
        let kind = v.get("type").and_then(Value::as_str);
        let payload = v.get("payload");
        match kind {
            Some("session_meta") => {
                if rec.session_id.is_none()
                    && let Some(id) = payload.and_then(|p| p.get("id")).and_then(Value::as_str)
                {
                    rec.session_id = Some(id.to_string());
                }
            }
            Some("turn_context") => {
                if rec.core.is_none()
                    && let Some(m) = payload.and_then(|p| p.get("model")).and_then(Value::as_str)
                {
                    rec.core = Some(m.to_string());
                }
            }
            // `response_item` (payload-wrapped) OR a bare top-level message.
            _ => {
                let scope = payload.unwrap_or(&v);
                if scope.get("type").and_then(Value::as_str) == Some("message")
                    || scope.get("role").is_some()
                {
                    let role = scope.get("role").and_then(Value::as_str);
                    if rec.core.is_none()
                        && let Some(m) = scope.get("model").and_then(Value::as_str)
                    {
                        rec.core = Some(m.to_string());
                    }
                    if rec.session_id.is_none()
                        && let Some(id) = v
                            .get("session_id")
                            .or_else(|| v.get("id"))
                            .and_then(Value::as_str)
                    {
                        rec.session_id = Some(id.to_string());
                    }
                    let content = collect_text(scope.get("content"));
                    if matches!(role, Some("user") | Some("assistant")) {
                        rec.message_count += 1;
                    }
                    if rec.task.is_none() && role == Some("user") && is_real_user_text(&content) {
                        rec.task = Some(one_line(&content));
                    }
                    for marker in scan_markers(&content) {
                        rec.markers.push(marker);
                    }
                }
            }
        }
    }
    rec
}

/// Parse structured JSONL (Claude Code, Codex, and lookalikes). Role, model,
/// and text are read from a `message` object or, failing that, the top-level
/// object, so both layouts work; the session id comes from `session_keys`.
fn parse_structured(
    text: &str,
    format: Format,
    harness: Option<&str>,
    core: Option<&str>,
    session_keys: &[&str],
) -> SessionRecord {
    let mut rec = SessionRecord {
        harness: harness.unwrap_or(format.default_harness()).to_string(),
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
        if rec.session_id.is_none() {
            for key in session_keys {
                if let Some(sid) = v.get(key).and_then(Value::as_str) {
                    rec.session_id = Some(sid.to_string());
                    break;
                }
            }
        }
        // Prefer a `message` object; fall back to the line itself.
        let scope = v.get("message").unwrap_or(&v);
        let role = scope
            .get("role")
            .and_then(Value::as_str)
            .or_else(|| v.get("type").and_then(Value::as_str));
        if rec.core.is_none()
            && let Some(model) = scope
                .get("model")
                .and_then(Value::as_str)
                .or_else(|| v.get("model").and_then(Value::as_str))
        {
            rec.core = Some(model.to_string());
        }
        if matches!(role, Some("user") | Some("assistant")) {
            rec.message_count += 1;
        }
        let text_content = collect_text(scope.get("content"));
        if rec.task.is_none() && role == Some("user") && !text_content.trim().is_empty() {
            rec.task = Some(one_line(&text_content));
        }
        for marker in scan_markers(&text_content) {
            rec.markers.push(marker);
        }
    }
    rec
}

/// Parse any text / markdown: the whole document is prose. The first non-empty
/// line is the task; `MEMORY <type>:` markers anywhere are harvested. This is
/// the universal path for a harness we have no bespoke parser for.
fn parse_generic(
    text: &str,
    format: Format,
    harness: Option<&str>,
    core: Option<&str>,
) -> SessionRecord {
    SessionRecord {
        harness: harness.unwrap_or(format.default_harness()).to_string(),
        core: core.map(str::to_string),
        session_id: None,
        task: text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .filter(|l| !l.is_empty())
            .map(cap_line),
        message_count: 0,
        scope: None,
        markers: scan_markers(text),
    }
}

/// Extract human text from a `content` value: a bare string, or the joined
/// `text` of blocks whose type is `text` / `input_text` / `output_text`
/// (covers Claude Code and Codex content arrays).
fn collect_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<&str> = Vec::new();
            for b in blocks {
                let ty = b.get("type").and_then(Value::as_str);
                if matches!(
                    ty,
                    Some("text") | Some("input_text") | Some("output_text") | None
                ) && let Some(t) = b.get("text").and_then(Value::as_str)
                {
                    parts.push(t);
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// First non-empty line, capped so a title stays a title.
fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    cap_line(line)
}

fn cap_line(line: &str) -> String {
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

/// Scan text for `MEMORY <type>: body` markers (case-insensitive on MEMORY and
/// the type). `MEMORY: body` with no type defaults to a fact. This is how an
/// agent or human flags something worth keeping, in ANY harness.
fn scan_markers(text: &str) -> Vec<(MemoryType, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        let Some(after) = lower.strip_prefix("memory") else {
            continue;
        };
        let rest_orig = &line[line.len() - after.len()..];
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
                _ => continue,
            }
        };
        out.push((mtype, body.to_string()));
    }
    out
}

/// Distill a session record into memories: always a `session-summary` (the
/// cross-provider breadcrumb), plus one memory per marker, each linked back to
/// the summary. Returns the created memories in creation order.
pub fn capture(store: &Store, rec: &SessionRecord, clock: &dyn Clock) -> Result<Vec<Memory>> {
    let mut created = Vec::new();
    // Scrub secrets from any content that flows into an id/slug BEFORE it is
    // slugified: the id is both the filename and the frontmatter id, both of
    // which sync to the remote, and `create_with_id` takes the id verbatim
    // (unlike `create`, it does not recompute the slug). The stored title/body
    // are scrubbed again in `build_memory`; re-redaction is idempotent. Honors
    // the store's `--no-redact` toggle.
    let scrub = |s: &str| {
        if store.redaction_enabled() {
            crate::redact::redact(s).0
        } else {
            s.to_string()
        }
    };
    let session_id = rec
        .session_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let title = scrub(
        &rec.task
            .clone()
            .unwrap_or_else(|| "agent session".to_string()),
    );
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
    // Deterministic identity from harness:session_id, so re-capturing the same
    // session (a retried SessionEnd hook, or a manual re-run) is idempotent and
    // never duplicates the summary or its markers.
    let sig = crate::util::fnv1a64_hex(format!("{}:{}", rec.harness, session_id).as_bytes());
    let summary_id = format!(
        "session-summary-{}-{}",
        crate::store::slugify(&title),
        &sig[..8]
    );
    let summary = match store.create_with_id(
        &summary_id,
        &NewMemory {
            mtype: Some(MemoryType::SessionSummary),
            title,
            source: Some(format!("{}:{}", rec.harness, session_id)),
            harness: Some(rec.harness.clone()),
            core: rec.core.clone(),
            scope: rec.scope.clone(),
            body,
            ..NewMemory::default()
        },
        clock,
    )? {
        Some(m) => m,
        // Already captured this session: nothing new to add.
        None => return Ok(created),
    };
    let summary_id = summary.id.clone();
    created.push(summary);

    for (mtype, text) in &rec.markers {
        let text = scrub(text);
        let mhash = crate::util::fnv1a64_hex(format!("{summary_id}:{text}").as_bytes());
        let marker_id = format!(
            "{}-{}-{}",
            mtype.as_str(),
            crate::store::slugify(&text),
            &mhash[..8]
        );
        if let Some(m) = store.create_with_id(
            &marker_id,
            &NewMemory {
                mtype: Some(*mtype),
                title: cap_chars(&text, 100),
                harness: Some(rec.harness.clone()),
                core: rec.core.clone(),
                scope: rec.scope.clone(),
                links: vec![summary_id.clone()],
                body: format!("{text}\n"),
                ..NewMemory::default()
            },
            clock,
        )? {
            created.push(m);
        }
    }
    Ok(created)
}

/// Read a transcript file and capture it. `format` forces a parser; `None`
/// auto-detects. `harness`/`core` override the log.
#[allow(clippy::too_many_arguments)]
pub fn capture_file(
    store: &Store,
    path: &str,
    format: Option<Format>,
    harness: Option<&str>,
    core: Option<&str>,
    scope: Option<&str>,
    clock: &dyn Clock,
) -> Result<Vec<Memory>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        context: "reading session transcript".to_string(),
        path: path.to_string(),
        source: e,
    })?;
    let format = format.unwrap_or_else(|| detect_format(&text));
    let mut rec = parse(&text, format, harness, core);
    rec.scope = scope.map(str::to_string);
    capture(store, &rec, clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000;

    fn claude_transcript() -> String {
        [
            r#"{"type":"user","sessionId":"abc123","message":{"role":"user","content":"help me pick a storage engine"}}"#,
            r#"{"type":"assistant","sessionId":"abc123","message":{"role":"assistant","model":"claude-opus-4-8","content":[{"type":"text","text":"Done.\nMEMORY decision: chose DuckDB over Postgres for analytics"}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn detects_and_parses_claude_code() {
        let t = claude_transcript();
        assert_eq!(detect_format(&t), Format::ClaudeCode);
        let rec = parse(&t, Format::ClaudeCode, None, None);
        assert_eq!(rec.harness, "claude-code");
        assert_eq!(rec.session_id.as_deref(), Some("abc123"));
        assert_eq!(rec.core.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(rec.task.as_deref(), Some("help me pick a storage engine"));
        assert_eq!(rec.message_count, 2);
        assert_eq!(rec.markers.len(), 1);
    }

    #[test]
    fn parses_real_codex_rollout_schema() {
        // The verified ~/.codex rollout shape: payload-wrapped, session_meta /
        // turn_context / response_item, with a duplicate event_msg line.
        let t = [
            r#"{"type":"session_meta","payload":{"id":"cx-1","cwd":"/tmp/proj","cli_version":"0.38.0"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_instructions>house rules</user_instructions>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"port the ingest service"}]}}"#,
            r#"{"type":"turn_context","payload":{"cwd":"/tmp/proj","model":"gpt-5.5"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok\nMEMORY rule: keep the contract stable across the port"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"port the ingest service"}}"#,
        ]
        .join("\n");
        assert_eq!(detect_format(&t), Format::Codex);
        let rec = parse(&t, Format::Codex, None, None);
        assert_eq!(rec.harness, "codex");
        assert_eq!(
            rec.session_id.as_deref(),
            Some("cx-1"),
            "session id from session_meta.payload.id"
        );
        assert_eq!(
            rec.core.as_deref(),
            Some("gpt-5.5"),
            "model from turn_context.payload.model"
        );
        assert_eq!(
            rec.task.as_deref(),
            Some("port the ingest service"),
            "the <user_instructions> injection is skipped when picking the task"
        );
        assert_eq!(
            rec.message_count, 3,
            "2 user + 1 assistant response_items; the duplicate event_msg is not counted"
        );
        assert_eq!(
            rec.markers,
            vec![(
                MemoryType::Rule,
                "keep the contract stable across the port".to_string()
            )]
        );
    }

    #[test]
    fn codex_parser_also_handles_bare_top_level_messages() {
        // A simpler tool that puts role/content at the top level.
        let t = r#"{"session_id":"s1","role":"user","model":"gpt-5.5","content":[{"type":"input_text","text":"do the thing"}]}"#;
        let rec = parse(t, Format::Codex, None, None);
        assert_eq!(rec.session_id.as_deref(), Some("s1"));
        assert_eq!(rec.core.as_deref(), Some("gpt-5.5"));
        assert_eq!(rec.task.as_deref(), Some("do the thing"));
    }

    #[test]
    fn generic_parser_works_on_any_text_for_an_unknown_harness() {
        // A plain markdown note from some harness we have no parser for.
        let t = "# Research notes\n\nfirst line is the task really\n\nMEMORY fact: Hermes runs the 405B model locally\nsome prose\nMEMORY decision: we standardize on parquet\n";
        assert_eq!(detect_format(t), Format::Generic);
        let rec = parse(t, Format::Generic, Some("hermes"), Some("hermes-4-405b"));
        assert_eq!(rec.harness, "hermes", "harness override honored");
        assert_eq!(rec.core.as_deref(), Some("hermes-4-405b"));
        assert_eq!(rec.task.as_deref(), Some("# Research notes"));
        assert_eq!(rec.markers.len(), 2, "markers harvested from plain text");
    }

    #[test]
    fn capture_writes_summary_and_markers_linked_for_any_harness() {
        let tmp = TempDir::new("capture-multi");
        let store = Store::open(tmp.path());
        let rec = parse(
            "MEMORY fact: sync uses your own git remote",
            Format::Generic,
            Some("hermes"),
            None,
        );
        let created = capture(&store, &rec, &FixedClock(T0)).unwrap();
        assert_eq!(created.len(), 2);
        assert_eq!(created[0].harness.as_deref(), Some("hermes"));
        assert_eq!(created[0].mtype, MemoryType::SessionSummary);
        assert!(created[1].links.contains(&created[0].id));
    }

    #[test]
    fn capture_is_deterministic() {
        let a = TempDir::new("cap-det-a");
        let b = TempDir::new("cap-det-b");
        let rec = parse(&claude_transcript(), Format::ClaudeCode, None, None);
        let ca = capture(&Store::open(a.path()), &rec, &FixedClock(T0)).unwrap();
        let cb = capture(&Store::open(b.path()), &rec, &FixedClock(T0)).unwrap();
        let ia: Vec<_> = ca.iter().map(|m| &m.id).collect();
        let ib: Vec<_> = cb.iter().map(|m| &m.id).collect();
        assert_eq!(ia, ib);
    }

    #[test]
    fn captured_secret_is_redacted_before_it_reaches_disk() {
        // A transcript whose marker body echoes a fake GitHub token. After
        // capture, no stored memory file may contain the token; each must show
        // the redaction marker instead. Fake, obviously-non-real secret.
        let tmp = TempDir::new("capture-redact");
        let store = Store::open(tmp.path());
        let fake = format!("ghp_{}", "x".repeat(36));
        let transcript = format!("MEMORY fact: the leaked key was {fake} do not reuse it");
        let rec = parse(&transcript, Format::Generic, Some("hermes"), None);
        let created = capture(&store, &rec, &FixedClock(T0)).unwrap();
        assert!(!created.is_empty());
        for m in &created {
            let path = store.memories_dir().join(format!("{}.md", m.id));
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains("ghp_"),
                "secret leaked into {}: {text}",
                m.id
            );
        }
        // The marker memory carries the redaction marker in title and body.
        let marker = created
            .iter()
            .find(|m| m.mtype == MemoryType::Fact)
            .expect("a fact marker was created");
        assert!(
            marker.body.contains("[REDACTED:github-token]"),
            "{:?}",
            marker.body
        );
        assert!(
            marker.title.contains("[REDACTED:github-token]"),
            "{:?}",
            marker.title
        );
    }

    #[test]
    fn markers_default_to_fact_and_reject_unknown_types() {
        let rec = parse(
            "MEMORY: configs live in etc\nMEMORY opinion: not a real type",
            Format::Generic,
            None,
            None,
        );
        assert_eq!(
            rec.markers,
            vec![(MemoryType::Fact, "configs live in etc".to_string())]
        );
    }
}
