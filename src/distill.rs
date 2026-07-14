//! distill — turn a session transcript into candidate memories.
//!
//! Single responsibility: given the raw transcript text plus the parsed
//! [`SessionRecord`], produce a `Vec<`[`Candidate`]`>` — decisions, rules, and
//! facts worth keeping that the session never flagged with an explicit
//! `MEMORY <type>:` marker. Capture then writes each candidate linked to the
//! session summary, redaction applied on the write path (see below).
//!
//! # The impurity boundary (honesty framing, non-negotiable)
//!
//! The shipped DEFAULT binary is airgap-pure: it never calls a model. The
//! always-compiled [`HeuristicDistiller`] is a deterministic, std-only
//! extractor — it beats bare markers by pulling decision/rule-shaped sentences
//! straight out of the transcript, so auto-capture is useful even offline.
//!
//! Richer, model-driven distillation is a deliberately impure node, compiled
//! ONLY behind the `distill` cargo feature (default OFF). It shells out to a
//! configurable external agent CLI (a *tool*, invoked via `sh -c`, NOT a crate
//! dependency), exactly as sync shells to `git` — so crate-level
//! zero-dependency is preserved. On any failure (spawn error, timeout,
//! non-zero exit, unparseable output) it falls back to the heuristic, so the
//! feature can only ever add memories, never lose the offline baseline.
//!
//! # Redaction ordering
//!
//! Distillation runs FIRST, over the un-redacted transcript, so the extractor
//! (or the model) sees the real text. The candidates it returns are written
//! through the store's write-path choke point, which scrubs secrets by default
//! (see [`crate::redact`]). Redaction therefore runs AFTER distillation: a
//! secret echoed in the transcript is scrubbed out of any stored candidate
//! before it can reach disk or sync to a git remote.

use crate::capture::SessionRecord;
use crate::store::memory::MemoryType;

/// A candidate memory produced by a [`Distiller`], before it is written. The
/// store assigns the id and applies redaction; a distiller only proposes
/// content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Proposed type (never `session-summary` — that is capture's own record).
    pub mtype: MemoryType,
    /// One-line title.
    pub title: String,
    /// Memory body.
    pub body: String,
    /// Why this was kept / when to apply it (the recall card's why-line).
    pub rationale: Option<String>,
    /// Tags, order preserved.
    pub tags: Vec<String>,
}

/// A distiller turns a transcript into candidate memories. Implementations must
/// be deterministic for the same input (the heuristic is; the model path is
/// best-effort and falls back to the heuristic, which is).
pub trait Distiller {
    /// Distill `transcript` (raw, un-redacted) with the parsed `rec` for
    /// provenance. Returns candidates in a stable, first-seen order.
    fn distill(&self, transcript: &str, rec: &SessionRecord) -> Vec<Candidate>;
}

/// Pick the distiller for a capture run. Under the default (feature-off) build
/// this is always the heuristic. With `--features distill`, an explicit
/// `cmd` (from `--distill-cmd`) or the `GHOSTIE_DISTILL_CMD` environment
/// variable selects the model-backed distiller; otherwise the heuristic.
pub fn build_distiller(_cmd: Option<&str>) -> Box<dyn Distiller> {
    #[cfg(feature = "distill")]
    {
        if let Some(c) = _cmd.filter(|c| !c.trim().is_empty()) {
            return Box::new(ModelDistiller::new(c.to_string()));
        }
        if let Some(m) = ModelDistiller::from_env() {
            return Box::new(m);
        }
    }
    Box::new(HeuristicDistiller)
}

// ---------------------------------------------------------------------------
// Heuristic distiller (always compiled, deterministic, std-only).
// ---------------------------------------------------------------------------

/// Longest a distilled sentence may be to be kept (chars); longer lines are
/// prose paragraphs, not crisp decisions/rules.
const MAX_SENTENCE_CHARS: usize = 200;
/// Shortest a distilled sentence may be (chars); shorter is noise.
const MIN_SENTENCE_CHARS: usize = 20;
/// Fewest words a distilled sentence may carry.
const MIN_WORDS: usize = 4;
/// Cap on how many candidates one session yields; keeps auto-capture bounded.
const MAX_CANDIDATES: usize = 12;
/// Title cap (chars), matching capture's marker titles.
const TITLE_CHARS: usize = 100;

/// A deterministic, std-only distiller. It reduces the transcript to natural
/// language, keeps only decision/rule/imperative-shaped sentences, dedupes
/// them, and caps the count. Pure function of the input: same transcript ->
/// byte-identical candidates.
#[derive(Debug, Default, Clone)]
pub struct HeuristicDistiller;

impl Distiller for HeuristicDistiller {
    fn distill(&self, transcript: &str, _rec: &SessionRecord) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for block in prose_blocks(transcript) {
            for raw in block.split('\n') {
                if out.len() >= MAX_CANDIDATES {
                    return out;
                }
                let line = strip_lead(raw.trim());
                let Some(mtype) = classify(line) else {
                    continue;
                };
                let key = normalize(line);
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                out.push(Candidate {
                    mtype,
                    title: cap_chars(line, TITLE_CHARS),
                    body: format!("{line}\n"),
                    rationale: Some(
                        "auto-distilled from the session transcript (heuristic)".to_string(),
                    ),
                    tags: vec!["distilled".to_string()],
                });
            }
        }
        out
    }
}

/// Reduce a transcript to natural-language blocks. Each line that parses as
/// JSON contributes every string stored under a `text` / `content` / `message`
/// key (covers Claude Code and Codex content blocks); a non-JSON line is kept
/// verbatim. Order-preserving and deterministic (object pairs and arrays are
/// walked in source order).
fn prose_blocks(transcript: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in transcript.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match crate::json::parse(t) {
            Ok(v) => collect_text(&v, &mut out),
            Err(_) => out.push(t.to_string()),
        }
    }
    out
}

/// Recursively collect human text from a parsed JSON value.
fn collect_text(v: &crate::json::Value, out: &mut Vec<String>) {
    use crate::json::Value;
    match v {
        Value::Object(pairs) => {
            for (k, val) in pairs {
                if matches!(k.as_str(), "text" | "content" | "message")
                    && let Value::String(s) = val
                {
                    out.push(s.clone());
                }
                collect_text(val, out);
            }
        }
        Value::Array(items) => {
            for it in items {
                collect_text(it, out);
            }
        }
        _ => {}
    }
}

/// Strip a leading list/heading marker so the sentence, not its bullet, is
/// classified: `- `, `* `, `+ `, `> `, `# `, and `N.`/`N)` numeric prefixes.
fn strip_lead(s: &str) -> &str {
    let t = s.trim_start_matches(['#', '>', '-', '*', '+']).trim_start();
    // Numeric list prefix: "12. " or "3) ".
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && matches!(bytes[i], b'.' | b')') {
        return t[i + 1..].trim_start();
    }
    t
}

/// Normalize for dedup: lowercase, whitespace collapsed to single spaces.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Cap to `max` chars on a char boundary.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Decision cues win over rule cues (an explicit choice is the higher-value
/// memory even when phrased with an "always"/"never" qualifier).
const DECISION_CUES: [&str; 12] = [
    "chose ",
    "we chose",
    "decided ",
    "decide to",
    "going with",
    "we'll use",
    "we will use",
    "settled on",
    "opted for",
    "switch to",
    "switched to",
    "instead of",
];

const RULE_CUES: [&str; 11] = [
    "always ",
    "never ",
    " must ",
    "you must",
    "we must",
    "make sure",
    "be sure to",
    "ensure ",
    "don't ",
    "do not ",
    "avoid ",
];

/// Classify a sentence as a decision/rule candidate, or `None` to skip. Skips
/// too-short/too-long lines, lines with too few words, and existing `MEMORY`
/// markers (capture harvests those directly, so distilling them would double).
fn classify(line: &str) -> Option<MemoryType> {
    let chars = line.chars().count();
    if !(MIN_SENTENCE_CHARS..=MAX_SENTENCE_CHARS).contains(&chars) {
        return None;
    }
    if line.split_whitespace().count() < MIN_WORDS {
        return None;
    }
    let lower = format!(" {} ", line.to_ascii_lowercase());
    if lower.trim_start().starts_with("memory") {
        return None;
    }
    if DECISION_CUES.iter().any(|c| lower.contains(c)) {
        return Some(MemoryType::Decision);
    }
    if RULE_CUES.iter().any(|c| lower.contains(c)) {
        return Some(MemoryType::Rule);
    }
    None
}

// ---------------------------------------------------------------------------
// Model-backed distiller (feature-gated, impure — shells to an external tool).
// ---------------------------------------------------------------------------

/// The fixed instruction prepended to the transcript on the model's stdin. The
/// model is asked to answer in the same `MEMORY <type>: ...` marker convention
/// capture already understands, so the contract is small and shell-friendly.
#[cfg(feature = "distill")]
const PROMPT: &str = "\
You are distilling an AI coding session into durable memories. Read the \
transcript below and emit the decisions, rules, and facts worth remembering, \
one per line, each in exactly this form:\n\
  MEMORY decision: <text>\n\
  MEMORY rule: <text>\n\
  MEMORY fact: <text>\n\
Use only the types decision, rule, fact. Output nothing else. Transcript:";

/// Model-backed distiller: shells to a configurable agent CLI, feeds it the
/// prompt + transcript on stdin, parses `MEMORY <type>: ...` lines from its
/// stdout, and falls back to the heuristic on any failure. Compiled only with
/// `--features distill`.
#[cfg(feature = "distill")]
pub struct ModelDistiller {
    command: String,
    timeout: std::time::Duration,
    fallback: HeuristicDistiller,
}

#[cfg(feature = "distill")]
impl ModelDistiller {
    /// Build for a shell `command` (run via `sh -c`). The timeout comes from
    /// `GHOSTIE_DISTILL_TIMEOUT_SECS` (default 60s), so a wedged agent can
    /// never hang capture — it is killed and the heuristic takes over.
    pub fn new(command: String) -> Self {
        let secs = std::env::var("GHOSTIE_DISTILL_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(60);
        ModelDistiller {
            command,
            timeout: std::time::Duration::from_secs(secs),
            fallback: HeuristicDistiller,
        }
    }

    /// The distiller configured by `GHOSTIE_DISTILL_CMD`, if set and non-empty.
    pub fn from_env() -> Option<Self> {
        std::env::var("GHOSTIE_DISTILL_CMD")
            .ok()
            .filter(|c| !c.trim().is_empty())
            .map(Self::new)
    }

    /// Run the agent, returning parsed candidates or `None` on any failure.
    fn run(&self, transcript: &str) -> Option<Vec<Candidate>> {
        use std::io::{Read, Write};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        let input = format!("{PROMPT}\n\n{transcript}");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Feed stdin on its own thread so a large transcript filling the pipe
        // buffer cannot deadlock against us reading stdout.
        if let Some(mut stdin) = child.stdin.take() {
            let data = input.into_bytes();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
                // dropping `stdin` here closes it, signalling EOF to the child
            });
        }

        // Read stdout on a thread and wait with a timeout: a wedged agent is
        // killed rather than hanging the capture.
        let mut stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stdout.read_to_string(&mut buf);
            let _ = tx.send(buf);
        });
        let out = match rx.recv_timeout(self.timeout) {
            Ok(s) => s,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        let status = child.wait().ok()?;
        if !status.success() {
            return None;
        }
        let cands = parse_model_output(&out);
        if cands.is_empty() { None } else { Some(cands) }
    }
}

#[cfg(feature = "distill")]
impl Distiller for ModelDistiller {
    fn distill(&self, transcript: &str, rec: &SessionRecord) -> Vec<Candidate> {
        self.run(transcript)
            .unwrap_or_else(|| self.fallback.distill(transcript, rec))
    }
}

/// Parse `MEMORY <type>: text` (or a bare `<type>: text`) lines from a model's
/// stdout into candidates. Unknown types and `session-summary` are ignored;
/// duplicates are dropped. Deterministic. Also used by the feature-off test
/// build to validate the parser, so it is not feature-gated.
#[cfg(any(feature = "distill", test))]
fn parse_model_output(out: &str) -> Vec<Candidate> {
    let mut cands: Vec<Candidate> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        // Strip an optional leading "memory" keyword.
        let after = lower
            .strip_prefix("memory")
            .map(str::trim_start)
            .unwrap_or(lower.as_str());
        let Some((head, _)) = after.split_once(':') else {
            continue;
        };
        let mtype = match head.trim() {
            "fact" => MemoryType::Fact,
            "decision" => MemoryType::Decision,
            "rule" => MemoryType::Rule,
            _ => continue,
        };
        // Recover the body from the original-cased line at the same colon.
        let body = match line.split_once(':') {
            Some((_, b)) => b.trim(),
            None => continue,
        };
        if body.is_empty() {
            continue;
        }
        let key = normalize(body);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        cands.push(Candidate {
            mtype,
            title: cap_chars(body, TITLE_CHARS),
            body: format!("{body}\n"),
            rationale: Some("auto-distilled from the session transcript (model)".to_string()),
            tags: vec!["distilled".to_string()],
        });
    }
    cands
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> SessionRecord {
        SessionRecord {
            harness: "hermes".to_string(),
            ..SessionRecord::default()
        }
    }

    // A small, realistic Claude Code JSONL transcript with decision/rule prose
    // and no explicit MEMORY markers — the case bare-marker capture misses.
    fn transcript() -> String {
        [
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"help me set up the store"}}"#,
            r#"{"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"We chose DuckDB over Postgres for the analytics store because columnar scans are faster.\n- Always run verify.sh before you commit any change.\nHere is some ordinary prose that decides nothing at all today."}]}}"#,
            r#"{"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"Never log raw user paths in the ingest pipeline.\nWe chose DuckDB over Postgres for the analytics store because columnar scans are faster."}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn heuristic_extracts_decisions_and_rules_deterministically() {
        let d = HeuristicDistiller;
        let a = d.distill(&transcript(), &rec());
        let b = d.distill(&transcript(), &rec());
        assert_eq!(a, b, "heuristic is a pure function of its input");

        // The duplicate DuckDB decision appears once; both rules are kept.
        assert_eq!(a.len(), 3, "{a:#?}");
        assert_eq!(a[0].mtype, MemoryType::Decision);
        assert!(a[0].title.starts_with("We chose DuckDB"));
        assert_eq!(a[1].mtype, MemoryType::Rule);
        assert!(a[1].title.contains("verify.sh"));
        assert_eq!(a[2].mtype, MemoryType::Rule);
        assert!(a[2].title.contains("Never log raw user paths"));
        assert!(a.iter().all(|c| c.tags == vec!["distilled".to_string()]));
    }

    #[test]
    fn heuristic_golden_titles_are_byte_stable() {
        let got: Vec<String> = HeuristicDistiller
            .distill(&transcript(), &rec())
            .into_iter()
            .map(|c| format!("{}|{}", c.mtype.as_str(), c.title))
            .collect();
        assert_eq!(
            got,
            vec![
                "decision|We chose DuckDB over Postgres for the analytics store because columnar scans are faster.".to_string(),
                "rule|Always run verify.sh before you commit any change.".to_string(),
                "rule|Never log raw user paths in the ingest pipeline.".to_string(),
            ]
        );
    }

    #[test]
    fn plain_prose_without_json_still_distills() {
        let t = "# notes\nWe decided to use parquet everywhere.\njust chatting here\nAlways pin the toolchain version.\n";
        let out = HeuristicDistiller.distill(t, &rec());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].mtype, MemoryType::Decision);
        assert_eq!(out[1].mtype, MemoryType::Rule);
    }

    #[test]
    fn markers_are_not_re_distilled() {
        // A MEMORY marker line is capture's job; the distiller skips it so the
        // same insight is not stored twice.
        let t = "MEMORY decision: we chose duckdb\nthis line just says hello there\n";
        let out = HeuristicDistiller.distill(t, &rec());
        assert!(out.is_empty(), "{out:#?}");
    }

    #[test]
    fn cap_bounds_the_candidate_count() {
        let mut t = String::new();
        for i in 0..50 {
            t.push_str(&format!(
                "Always keep invariant number {i} stable across runs.\n"
            ));
        }
        let out = HeuristicDistiller.distill(&t, &rec());
        assert_eq!(out.len(), MAX_CANDIDATES);
    }

    #[test]
    fn model_output_parser_reads_marker_lines() {
        let out = "MEMORY decision: chose duckdb\nrule: always run verify.sh\nnoise line\nMEMORY opinion: ignored type\nMEMORY decision: chose duckdb\n";
        let cands = parse_model_output(out);
        assert_eq!(cands.len(), 2, "{cands:#?}");
        assert_eq!(cands[0].mtype, MemoryType::Decision);
        assert_eq!(cands[0].title, "chose duckdb");
        assert_eq!(cands[1].mtype, MemoryType::Rule);
        assert_eq!(cands[1].title, "always run verify.sh");
    }

    #[cfg(feature = "distill")]
    #[test]
    fn model_distiller_uses_stub_command_stdout() {
        // A stub "agent": echoes two marker lines, needs no real model.
        let d = ModelDistiller::new(
            "printf 'MEMORY decision: chose duckdb over postgres\\nMEMORY rule: always run verify.sh\\n'"
                .to_string(),
        );
        let out = d.distill("irrelevant transcript body", &rec());
        assert_eq!(out.len(), 2, "{out:#?}");
        assert_eq!(out[0].mtype, MemoryType::Decision);
        assert_eq!(out[1].mtype, MemoryType::Rule);
    }

    #[cfg(feature = "distill")]
    #[test]
    fn model_distiller_falls_back_to_heuristic_on_failure() {
        // A command that exits non-zero -> fall back to the heuristic, which
        // still finds the decision in the transcript.
        let d = ModelDistiller::new("exit 7".to_string());
        let out = d.distill(&transcript(), &rec());
        assert!(!out.is_empty(), "fell back to heuristic");
        assert_eq!(out[0].mtype, MemoryType::Decision);
    }
}
