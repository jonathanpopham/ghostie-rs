//! provenance — a deterministic, hash-chained, append-only lineage log.
//!
//! Single responsibility: every memory write appends one record to
//! `<root>/.provenance/log.jsonl` so a memory's origin is verifiable and
//! tamper-evident, and the whole chain can be replayed and checked. This is
//! the stack's clean Lockstep-style lineage: each record commits to the one
//! before it, so any edit to a past record (or to a memory file after its
//! last recorded write) breaks a hash and is caught by `verify`.
//!
//! # The record
//!
//! One JSON object per line (JSONL), byte-stable canonical form:
//!
//! ```text
//! {seq, prev_hash, memory_id, event, content_hash, source, harness, core,
//!  created, entry_hash}
//! ```
//!
//! - `seq` — 1-based monotonic sequence number across the whole log.
//! - `prev_hash` — the previous record's `entry_hash`; [`GENESIS_HASH`] for
//!   the first. This is the chain link.
//! - `memory_id` — which memory this record is about.
//! - `event` — `created` | `updated` | `captured`.
//! - `content_hash` — FNV-1a of the memory's canonical file bytes at the
//!   instant of the write (identical to the bytes `write_atomic` persisted).
//! - `source` / `harness` / `core` — the memory's provenance fields (the
//!   where/which of a cross-provider memory), `null` when absent.
//! - `created` — the event instant, stamped from the injected [`Clock`] so two
//!   identical runs under `GHOSTIE_TEST_CLOCK` produce byte-identical logs.
//! - `entry_hash` — `fnv1a64_hex(prev_hash ++ canonical_payload)`, where the
//!   canonical payload is every field above EXCEPT `prev_hash` and
//!   `entry_hash`, emitted by [`crate::json`] in fixed order.
//!
//! # Determinism (crate law)
//!
//! The canonical payload is compact deterministic JSON (fixed field order, no
//! whitespace, no map iteration), the timestamp comes from the [`Clock`], and
//! the content hash is FNV-1a of the exact stored bytes. Same writes, same
//! bytes, same log, forever.
//!
//! # Sync decision: the log DOES sync
//!
//! Unlike the derived `.index/` (rebuildable, gitignored), the provenance log
//! is the point: it is the evidence. It travels with the memories through the
//! user's own git remote so lineage is portable across devices, so it is NOT
//! added to `.gitignore`. `sync` stages it with `git add -A`.

use crate::error::{Error, Result};
use crate::json::{self, Value};
use crate::store::memory::Memory;
use crate::util::{Clock, fnv1a64_hex, format_rfc3339_utc, parse_rfc3339_utc};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The `prev_hash` of the first record: no predecessor. Sixteen hex zeros,
/// matching the FNV-1a 64-bit hex width used everywhere else.
pub const GENESIS_HASH: &str = "0000000000000000";

/// `<root>/.provenance/log.jsonl`.
pub fn log_path(root: &Path) -> PathBuf {
    root.join(".provenance").join("log.jsonl")
}

/// What kind of write produced a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `Store::create` — a brand-new memory from the command line.
    Created,
    /// `Store::update` — a rewrite of an existing memory.
    Updated,
    /// `Store::create_with_id` — capture's provider-agnostic session record.
    Captured,
}

impl Event {
    /// The on-disk spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Event::Created => "created",
            Event::Updated => "updated",
            Event::Captured => "captured",
        }
    }

    /// Parse the on-disk spelling.
    pub fn parse(s: &str) -> Option<Event> {
        match s {
            "created" => Some(Event::Created),
            "updated" => Some(Event::Updated),
            "captured" => Some(Event::Captured),
            _ => None,
        }
    }
}

/// One provenance record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 1-based monotonic sequence across the whole log.
    pub seq: i64,
    /// The previous record's `entry_hash` (or [`GENESIS_HASH`]).
    pub prev_hash: String,
    /// Which memory this record is about.
    pub memory_id: String,
    /// The write that produced this record.
    pub event: Event,
    /// FNV-1a of the memory's canonical file bytes at write time.
    pub content_hash: String,
    /// The memory's `source` field, if any.
    pub source: Option<String>,
    /// The memory's `harness` field, if any.
    pub harness: Option<String>,
    /// The memory's `core` field, if any.
    pub core: Option<String>,
    /// The event instant, epoch seconds UTC (from the injected clock).
    pub created: i64,
    /// `fnv1a64_hex(prev_hash ++ canonical_payload)`.
    pub entry_hash: String,
}

fn opt_value(o: &Option<String>) -> Value {
    match o {
        Some(s) => Value::string(s.clone()),
        None => Value::Null,
    }
}

impl Entry {
    /// The canonical payload bytes that `entry_hash` commits to: every field
    /// EXCEPT `prev_hash` and `entry_hash`, in fixed order, compact JSON.
    fn canonical_payload(&self) -> String {
        Value::Object(vec![
            ("seq".to_string(), Value::int(self.seq)),
            (
                "memory_id".to_string(),
                Value::string(self.memory_id.clone()),
            ),
            ("event".to_string(), Value::string(self.event.as_str())),
            (
                "content_hash".to_string(),
                Value::string(self.content_hash.clone()),
            ),
            ("source".to_string(), opt_value(&self.source)),
            ("harness".to_string(), opt_value(&self.harness)),
            ("core".to_string(), opt_value(&self.core)),
            (
                "created".to_string(),
                Value::string(format_rfc3339_utc(self.created)),
            ),
        ])
        .emit()
    }

    /// Recompute `entry_hash` from `prev_hash` and the canonical payload. The
    /// chain check: a record is intact iff this equals its stored hash.
    pub fn compute_hash(&self) -> String {
        let mut buf = self.prev_hash.clone();
        buf.push_str(&self.canonical_payload());
        fnv1a64_hex(buf.as_bytes())
    }

    /// The full stored line: canonical payload plus `prev_hash` and
    /// `entry_hash`, in fixed field order.
    pub fn to_line(&self) -> String {
        Value::Object(vec![
            ("seq".to_string(), Value::int(self.seq)),
            (
                "prev_hash".to_string(),
                Value::string(self.prev_hash.clone()),
            ),
            (
                "memory_id".to_string(),
                Value::string(self.memory_id.clone()),
            ),
            ("event".to_string(), Value::string(self.event.as_str())),
            (
                "content_hash".to_string(),
                Value::string(self.content_hash.clone()),
            ),
            ("source".to_string(), opt_value(&self.source)),
            ("harness".to_string(), opt_value(&self.harness)),
            ("core".to_string(), opt_value(&self.core)),
            (
                "created".to_string(),
                Value::string(format_rfc3339_utc(self.created)),
            ),
            (
                "entry_hash".to_string(),
                Value::string(self.entry_hash.clone()),
            ),
        ])
        .emit()
    }

    /// A public JSON view for CLI robot output (same shape as the stored line).
    pub fn to_json(&self) -> Value {
        json::parse(&self.to_line()).unwrap_or(Value::Null)
    }

    /// Parse one stored line back into an [`Entry`].
    fn from_value(v: &Value, origin: &str) -> Result<Entry> {
        let invalid = |what: &str| Error::Invalid {
            origin: origin.to_string(),
            message: format!("provenance record: {what}"),
        };
        let req_str = |k: &str| -> Result<String> {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| invalid(&format!("missing or non-string field '{k}'")))
        };
        let opt = |k: &str| -> Option<String> {
            match v.get(k) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            }
        };
        let seq = v
            .get("seq")
            .and_then(Value::as_i64)
            .ok_or_else(|| invalid("missing or non-integer 'seq'"))?;
        let event_str = req_str("event")?;
        let event = Event::parse(&event_str)
            .ok_or_else(|| invalid(&format!("unknown event '{event_str}'")))?;
        let created_str = req_str("created")?;
        let created = parse_rfc3339_utc(&created_str)
            .map_err(|e| invalid(&format!("field 'created': {e}")))?;
        Ok(Entry {
            seq,
            prev_hash: req_str("prev_hash")?,
            memory_id: req_str("memory_id")?,
            event,
            content_hash: req_str("content_hash")?,
            source: opt("source"),
            harness: opt("harness"),
            core: opt("core"),
            created,
            entry_hash: req_str("entry_hash")?,
        })
    }
}

/// On Unix, keep the log and its directory private to the owner (0600/0700):
/// it records what the user remembered and where it came from. A no-op else.
#[cfg(unix)]
fn set_private(path: &Path, dir: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if dir { 0o700 } else { 0o600 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_private(_path: &Path, _dir: bool) {}

/// Read the whole log in order. A genuinely-absent log is an empty chain
/// (`Ok(vec![])`); a decode/permission error is surfaced.
pub fn read_all(root: &Path) -> Result<Vec<Entry>> {
    let path = log_path(root);
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Io {
                context: "reading provenance log".to_string(),
                path: origin,
                source: e,
            });
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v = json::parse_with_origin(line, &origin)?;
        out.push(Entry::from_value(&v, &origin)?);
    }
    Ok(out)
}

/// The last record (for chaining a new one), without parsing the whole log.
fn read_tail(root: &Path) -> Result<Option<Entry>> {
    let path = log_path(root);
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Io {
                context: "reading provenance log tail".to_string(),
                path: origin,
                source: e,
            });
        }
    };
    match text.lines().rev().find(|l| !l.trim().is_empty()) {
        None => Ok(None),
        Some(line) => {
            let v = json::parse_with_origin(line, &origin)?;
            Ok(Some(Entry::from_value(&v, &origin)?))
        }
    }
}

/// Append one record for a just-written memory. Chains onto the current tail
/// (or genesis), hashing the memory's canonical bytes so the record commits to
/// exactly what was persisted. Returns the appended [`Entry`].
pub fn append(root: &Path, memory: &Memory, event: Event, clock: &dyn Clock) -> Result<Entry> {
    let (seq, prev_hash) = match read_tail(root)? {
        Some(last) => (last.seq + 1, last.entry_hash),
        None => (1, GENESIS_HASH.to_string()),
    };
    let content = memory.to_doc().serialize();
    let mut entry = Entry {
        seq,
        prev_hash,
        memory_id: memory.id.clone(),
        event,
        content_hash: fnv1a64_hex(content.as_bytes()),
        source: memory.source.clone(),
        harness: memory.harness.clone(),
        core: memory.core.clone(),
        created: clock.now_epoch_seconds(),
        entry_hash: String::new(),
    };
    entry.entry_hash = entry.compute_hash();

    let path = log_path(root);
    if let Some(dir) = path.parent() {
        let existed = dir.exists();
        std::fs::create_dir_all(dir).map_err(|e| Error::Io {
            context: "creating provenance directory".to_string(),
            path: dir.display().to_string(),
            source: e,
        })?;
        if !existed {
            set_private(dir, true);
        }
    }
    let new_file = !path.exists();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Error::Io {
            context: "opening provenance log for append".to_string(),
            path: path.display().to_string(),
            source: e,
        })?;
    let mut line = entry.to_line();
    line.push('\n');
    f.write_all(line.as_bytes()).map_err(|e| Error::Io {
        context: "appending provenance record".to_string(),
        path: path.display().to_string(),
        source: e,
    })?;
    if new_file {
        set_private(&path, false);
    }
    Ok(entry)
}

/// The verdict of a full chain replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyReport {
    /// The whole chain replays and every live memory still matches its last
    /// recorded content hash.
    Intact {
        /// Records replayed.
        entries: usize,
        /// Distinct memories covered.
        memories: usize,
    },
    /// The first broken link, by sequence number.
    Broken {
        /// Where the break is.
        seq: i64,
        /// What broke, in plain words.
        reason: String,
    },
}

/// Replay the whole chain and report [`VerifyReport::Intact`] or the first
/// [`VerifyReport::Broken`] link. Two independent checks:
///
/// 1. **Chain integrity** — in sequence order, each record's `prev_hash` must
///    equal the previous record's `entry_hash`, and its recomputed
///    `entry_hash` must equal the stored one. Editing any field of any past
///    record (including a historical `content_hash`) breaks this.
/// 2. **Content tamper** — for each memory's LATEST record, the memory file's
///    bytes must still hash to the recorded `content_hash`. A memory file
///    changed outside the store API (a raw hand-edit, not a re-`update`) is
///    caught here. A deleted memory (file absent) is legitimate, not broken.
///
/// The lowest broken `seq` wins, so the report names the first bad link.
pub fn verify(root: &Path) -> Result<VerifyReport> {
    let entries = read_all(root)?;

    // Pass 1: chain integrity, strictly in order.
    let mut prev = GENESIS_HASH.to_string();
    let mut last_seq = 0i64;
    for e in &entries {
        if e.seq != last_seq + 1 {
            return Ok(VerifyReport::Broken {
                seq: e.seq,
                reason: format!(
                    "sequence gap: expected seq {}, found {} (a record was inserted or removed)",
                    last_seq + 1,
                    e.seq
                ),
            });
        }
        if e.prev_hash != prev {
            return Ok(VerifyReport::Broken {
                seq: e.seq,
                reason: "prev_hash does not match the previous entry_hash (chain broken)"
                    .to_string(),
            });
        }
        if e.compute_hash() != e.entry_hash {
            return Ok(VerifyReport::Broken {
                seq: e.seq,
                reason: "entry_hash does not match the record contents (record tampered)"
                    .to_string(),
            });
        }
        prev = e.entry_hash.clone();
        last_seq = e.seq;
    }

    // Pass 2: content tamper on each memory's latest record. Track the highest
    // seq per memory, then rehash the live file.
    let mut latest: std::collections::BTreeMap<String, (i64, String)> =
        std::collections::BTreeMap::new();
    for e in &entries {
        let slot = latest
            .entry(e.memory_id.clone())
            .or_insert((0, String::new()));
        if e.seq >= slot.0 {
            *slot = (e.seq, e.content_hash.clone());
        }
    }
    let mut first_broken: Option<(i64, String)> = None;
    for (id, (seq, hash)) in &latest {
        let mpath = root.join("memories").join(format!("{id}.md"));
        match std::fs::read(&mpath) {
            Ok(bytes) => {
                if &fnv1a64_hex(&bytes) != hash {
                    let candidate = (
                        *seq,
                        format!(
                            "memory '{id}' changed since its last provenance record \
                             (content hash mismatch); re-record it via an update"
                        ),
                    );
                    match &first_broken {
                        Some((s, _)) if *s <= candidate.0 => {}
                        _ => first_broken = Some(candidate),
                    }
                }
            }
            // A deleted memory leaves its history in the log; git history is the
            // tombstone. Absence is not tampering.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Error::Io {
                    context: "reading memory file for provenance verify".to_string(),
                    path: mpath.display().to_string(),
                    source: e,
                });
            }
        }
    }
    match first_broken {
        Some((seq, reason)) => Ok(VerifyReport::Broken { seq, reason }),
        None => Ok(VerifyReport::Intact {
            entries: entries.len(),
            memories: latest.len(),
        }),
    }
}

/// Every record for one memory, in log order.
pub fn lineage(root: &Path, memory_id: &str) -> Result<Vec<Entry>> {
    Ok(read_all(root)?
        .into_iter()
        .filter(|e| e.memory_id == memory_id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::{Memory, MemoryType};
    use crate::util::FixedClock;

    fn tmpdir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let d =
            std::env::temp_dir().join(format!("ghostie-prov-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mem(id: &str, title: &str, body: &str) -> Memory {
        Memory {
            id: id.to_string(),
            mtype: MemoryType::Fact,
            created: 1_783_944_000,
            title: title.to_string(),
            tags: vec![],
            links: vec![],
            source: None,
            supersedes: None,
            harness: Some("claude-code".to_string()),
            core: Some("opus-4.8".to_string()),
            rationale: None,
            scope: None,
            unknown_keys: vec![],
            body: body.to_string(),
        }
    }

    /// Simulate the store write path: persist the canonical bytes AND append a
    /// provenance record, so verify's content check sees a real file.
    fn write_and_record(root: &Path, m: &Memory, event: Event, clock: &dyn Clock) {
        let dir = root.join("memories");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{}.md", m.id)), m.to_doc().serialize()).unwrap();
        append(root, m, event, clock).unwrap();
    }

    #[test]
    fn append_n_then_verify_intact() {
        let root = tmpdir("intact");
        let clock = FixedClock(1_783_944_000);
        for i in 1..=5 {
            let m = mem(&format!("fact-x-{i}"), &format!("title {i}"), "body\n");
            write_and_record(&root, &m, Event::Created, &clock);
        }
        let entries = read_all(&root).unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[0].prev_hash, GENESIS_HASH);
        // Each record chains onto the last.
        for w in entries.windows(2) {
            assert_eq!(w[1].prev_hash, w[0].entry_hash, "prev links to predecessor");
            assert_eq!(w[1].seq, w[0].seq + 1, "seq is monotonic");
        }
        assert_eq!(
            verify(&root).unwrap(),
            VerifyReport::Intact {
                entries: 5,
                memories: 5
            }
        );
    }

    #[test]
    fn updates_chain_and_carry_events() {
        let root = tmpdir("events");
        let clock = FixedClock(1_783_944_000);
        let mut m = mem("fact-note-1", "note", "first\n");
        write_and_record(&root, &m, Event::Created, &clock);
        m.body = "second\n".to_string();
        write_and_record(&root, &m, Event::Updated, &clock);
        let hist = lineage(&root, "fact-note-1").unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].event, Event::Created);
        assert_eq!(hist[1].event, Event::Updated);
        // The two content hashes differ because the body changed.
        assert_ne!(hist[0].content_hash, hist[1].content_hash);
        assert_eq!(
            verify(&root).unwrap(),
            VerifyReport::Intact {
                entries: 2,
                memories: 1
            }
        );
    }

    #[test]
    fn tampering_with_a_memory_file_is_detected() {
        let root = tmpdir("tamper-file");
        let clock = FixedClock(1_783_944_000);
        write_and_record(&root, &mem("fact-a-1", "a", "aa\n"), Event::Created, &clock);
        write_and_record(&root, &mem("fact-b-1", "b", "bb\n"), Event::Created, &clock);
        // Hand-edit the second memory's file behind the store's back.
        let p = root.join("memories").join("fact-b-1.md");
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("sneaky extra line\n");
        std::fs::write(&p, text).unwrap();
        match verify(&root).unwrap() {
            VerifyReport::Broken { seq, reason } => {
                assert_eq!(seq, 2, "break reported at the tampered memory's record");
                assert!(reason.contains("content hash mismatch"), "{reason}");
            }
            other => panic!("expected BROKEN, got {other:?}"),
        }
    }

    #[test]
    fn tampering_with_a_log_entry_is_detected() {
        let root = tmpdir("tamper-entry");
        let clock = FixedClock(1_783_944_000);
        for i in 1..=3 {
            write_and_record(
                &root,
                &mem(&format!("fact-x-{i}"), &format!("t{i}"), "b\n"),
                Event::Created,
                &clock,
            );
        }
        // Rewrite the middle record's content_hash without fixing its
        // entry_hash: the recomputed hash no longer matches.
        let path = log_path(&root);
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let mut v = json::parse(&lines[1]).unwrap();
        if let Value::Object(pairs) = &mut v {
            for (k, val) in pairs.iter_mut() {
                if k == "content_hash" {
                    *val = Value::string("deadbeefdeadbeef");
                }
            }
        }
        let tampered = format!("{}\n{}\n{}\n", lines[0], v.emit(), lines[2]);
        std::fs::write(&path, tampered).unwrap();
        match verify(&root).unwrap() {
            VerifyReport::Broken { seq, reason } => {
                assert_eq!(seq, 2, "break reported at the edited record");
                assert!(
                    reason.contains("tampered") || reason.contains("chain"),
                    "{reason}"
                );
            }
            other => panic!("expected BROKEN, got {other:?}"),
        }
    }

    #[test]
    fn log_is_byte_stable_under_a_fixed_clock() {
        // Two identical runs must produce byte-identical logs.
        let build = || {
            let root = tmpdir("stable");
            let clock = FixedClock(1_783_944_000);
            for (id, title, body) in [
                ("fact-one-1", "one", "b1\n"),
                ("fact-two-1", "two", "b2\n"),
                ("fact-three-1", "three", "b3\n"),
            ] {
                write_and_record(&root, &mem(id, title, body), Event::Created, &clock);
            }
            std::fs::read(log_path(&root)).unwrap()
        };
        assert_eq!(build(), build(), "identical writes -> identical log bytes");
    }

    #[test]
    fn deleted_memory_does_not_break_the_chain() {
        let root = tmpdir("deleted");
        let clock = FixedClock(1_783_944_000);
        write_and_record(
            &root,
            &mem("fact-keep-1", "k", "k\n"),
            Event::Created,
            &clock,
        );
        write_and_record(
            &root,
            &mem("fact-gone-1", "g", "g\n"),
            Event::Created,
            &clock,
        );
        // Delete one memory file; its history stays in the log.
        std::fs::remove_file(root.join("memories").join("fact-gone-1.md")).unwrap();
        assert_eq!(
            verify(&root).unwrap(),
            VerifyReport::Intact {
                entries: 2,
                memories: 2
            },
            "a deleted memory is not tampering (git history is the tombstone)"
        );
    }

    #[test]
    fn empty_log_verifies_intact() {
        let root = tmpdir("empty");
        assert_eq!(
            verify(&root).unwrap(),
            VerifyReport::Intact {
                entries: 0,
                memories: 0
            }
        );
        assert!(read_all(&root).unwrap().is_empty());
    }

    #[test]
    fn the_lockstep_lineage_demo() {
        // The clean Lockstep-lineage narrative in one path: every write leaves
        // a hash-chained certificate of origin, the chain replays to INTACT,
        // and a single silent edit is caught with the exact broken link. This
        // is the same shape as a Lockstep behavioral-equivalence certificate:
        // deterministic evidence, black-box verifiable, tamper-evident, with
        // zero parsing of anyone else's program (clean IP).
        let root = tmpdir("lockstep-demo");
        let clock = FixedClock(1_783_944_000);
        write_and_record(
            &root,
            &mem(
                "fact-decision-1",
                "chose fnv over sha",
                "std-only keeps zero deps\n",
            ),
            Event::Created,
            &clock,
        );
        write_and_record(
            &root,
            &mem(
                "fact-branch-1",
                "sync branch is main",
                "one branch across devices\n",
            ),
            Event::Captured,
            &clock,
        );
        // The whole lineage replays clean.
        assert!(matches!(
            verify(&root).unwrap(),
            VerifyReport::Intact { .. }
        ));
        // Now forge one memory. The certificate no longer holds, and verify
        // points at the exact record.
        let p = root.join("memories").join("fact-decision-1.md");
        std::fs::write(&p, "forged\n").unwrap();
        assert!(matches!(
            verify(&root).unwrap(),
            VerifyReport::Broken { seq: 1, .. }
        ));
    }
}
