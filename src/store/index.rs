//! The derivable index: `<root>/.index/index.json` — a read optimization
//! for recall and list, generated FROM the memory files and disposable at
//! all times. Deleting it never loses data or changes observable results
//! (only speed). Never authoritative; the tests bead proves it mechanically.
//!
//! Freshness: an entry is stale when its `content_hash` (FNV-1a 64 of the
//! file bytes) no longer matches the file — mtimes are not trusted (hand
//! edits and git checkouts lie). [`Index::ensure_fresh`] re-indexes stale
//! or missing entries and drops deleted ones. CRUD writes refresh
//! incrementally, best-effort: an index write failure is a warning, never
//! data loss.
//!
//! The file is one deterministic JSON document (sorted-object builders,
//! everything integer or string): byte-stable given the same memory files.

use crate::error::{Error, Result, Warning};
use crate::json::{self, Value};
use crate::recall::tokenize::tokenize;
use crate::store::Store;
use crate::store::memory::{Memory, MemoryType};
use crate::util::{fnv1a64_hex, format_rfc3339_utc, parse_rfc3339_utc};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bump when the on-disk index schema changes (old indexes are rebuilt).
pub const FORMAT_VERSION: i64 = 1;

/// The three scored fields, in canonical order. Array positions in
/// [`DocEntry::tf`] and [`DocEntry::field_len`] follow this order.
pub const FIELDS: [&str; 3] = ["title", "tags", "body"];

/// Per-memory index entry: identity, freshness hash, and token statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocEntry {
    /// Memory id (also the map key).
    pub id: String,
    /// Path relative to the store root (portable across machines).
    pub path: String,
    /// Memory type.
    pub mtype: MemoryType,
    /// Title (list rendering without re-reading files).
    pub title: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Created, epoch seconds.
    pub created: i64,
    /// FNV-1a 64 hex of the file bytes at index time.
    pub content_hash: String,
    /// term -> tf per field `[title, tags, body]`.
    pub tf: BTreeMap<String, [i64; 3]>,
    /// Token count per field `[title, tags, body]` (stopwords dropped).
    pub field_len: [i64; 3],
}

/// The in-memory index: doc entries by id. Corpus aggregates (doc count,
/// per-term df, total field lengths) are always recomputed from the
/// entries — they are written to the file for humans and debuggers, but
/// never trusted on load (they are derivable; deriving is cheap and
/// removes a staleness class).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Index {
    /// Entries keyed by memory id (BTreeMap: deterministic iteration).
    pub docs: BTreeMap<String, DocEntry>,
}

impl Index {
    /// Index one memory (tokenizes title/tags/body with THE tokenizer).
    pub fn entry_for(memory: &Memory, file_bytes: &[u8]) -> DocEntry {
        let title_toks = tokenize(&memory.title);
        let tags_toks = tokenize(&memory.tags.join(" "));
        let body_toks = tokenize(&memory.body);
        let mut tf: BTreeMap<String, [i64; 3]> = BTreeMap::new();
        for (slot, toks) in [(0, &title_toks), (1, &tags_toks), (2, &body_toks)] {
            for t in toks {
                tf.entry(t.clone()).or_insert([0, 0, 0])[slot] += 1;
            }
        }
        DocEntry {
            id: memory.id.clone(),
            path: format!("memories/{}.md", memory.id),
            mtype: memory.mtype,
            title: memory.title.clone(),
            tags: memory.tags.clone(),
            created: memory.created,
            content_hash: fnv1a64_hex(file_bytes),
            tf,
            field_len: [
                title_toks.len() as i64,
                tags_toks.len() as i64,
                body_toks.len() as i64,
            ],
        }
    }

    /// Build a fresh index from every readable memory file. Unreadable
    /// files become warnings (and are simply absent from the index).
    pub fn build(store: &Store) -> Result<(Index, Vec<Warning>)> {
        let (memories, warnings) = store.list(&crate::store::ListFilter::default())?;
        let mut docs = BTreeMap::new();
        for m in &memories {
            let path = store.memories_dir().join(format!("{}.md", m.id));
            let bytes = std::fs::read(&path).map_err(|e| Error::Io {
                context: "reading memory file for indexing".to_string(),
                path: path.display().to_string(),
                source: e,
            })?;
            docs.insert(m.id.clone(), Index::entry_for(m, &bytes));
        }
        Ok((Index { docs }, warnings))
    }

    /// Load the index and re-index anything stale (hash mismatch), missing
    /// (new file) or deleted. Returns the fresh index plus warnings. Saves
    /// back to disk best-effort when anything changed (a save failure is a
    /// warning, never an error).
    pub fn ensure_fresh(store: &Store) -> Result<(Index, Vec<Warning>)> {
        let mut warnings = Vec::new();
        let loaded = match Index::load(store.root()) {
            Ok(Some(idx)) => idx,
            Ok(None) => Index::default(),
            Err(e) => {
                warnings.push(Warning {
                    origin: index_path(store.root()).display().to_string(),
                    message: format!("unreadable index, rebuilding: {e}"),
                });
                Index::default()
            }
        };
        let mut fresh = Index::default();
        let mut changed = false;
        let (memories, mut list_warnings) = store.list(&crate::store::ListFilter::default())?;
        warnings.append(&mut list_warnings);
        for m in &memories {
            let path = store.memories_dir().join(format!("{}.md", m.id));
            let bytes = std::fs::read(&path).map_err(|e| Error::Io {
                context: "reading memory file for freshness check".to_string(),
                path: path.display().to_string(),
                source: e,
            })?;
            let hash = fnv1a64_hex(&bytes);
            match loaded.docs.get(&m.id) {
                Some(entry) if entry.content_hash == hash => {
                    fresh.docs.insert(m.id.clone(), entry.clone());
                }
                _ => {
                    fresh.docs.insert(m.id.clone(), Index::entry_for(m, &bytes));
                    changed = true;
                }
            }
        }
        if loaded.docs.len() != fresh.docs.len() {
            changed = true; // deletions
        }
        if changed && let Err(e) = fresh.save(store.root()) {
            warnings.push(Warning {
                origin: index_path(store.root()).display().to_string(),
                message: format!("could not write index (results unaffected): {e}"),
            });
        }
        Ok((fresh, warnings))
    }

    /// Corpus size.
    pub fn doc_count(&self) -> i64 {
        self.docs.len() as i64
    }

    /// Per-term document frequency (a doc counts once regardless of field).
    pub fn df(&self) -> BTreeMap<String, i64> {
        let mut df: BTreeMap<String, i64> = BTreeMap::new();
        for entry in self.docs.values() {
            for term in entry.tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
        }
        df
    }

    /// Total token count per field `[title, tags, body]` across the corpus.
    pub fn total_field_len(&self) -> [i64; 3] {
        let mut totals = [0i64; 3];
        for entry in self.docs.values() {
            for (i, l) in entry.field_len.iter().enumerate() {
                totals[i] += l;
            }
        }
        totals
    }

    /// Serialize to the deterministic JSON document (sorted keys, compact).
    pub fn to_json(&self) -> String {
        let mut memories: Vec<(String, Value)> = Vec::new();
        for (id, e) in &self.docs {
            let mut tf_fields: [Vec<(String, Value)>; 3] = [vec![], vec![], vec![]];
            for (term, counts) in &e.tf {
                for (slot, &c) in counts.iter().enumerate() {
                    if c > 0 {
                        tf_fields[slot].push((term.clone(), Value::int(c)));
                    }
                }
            }
            let fields_obj: Vec<(String, Value)> = FIELDS
                .iter()
                .enumerate()
                .map(|(slot, name)| {
                    (
                        (*name).to_string(),
                        Value::sorted_object(vec![
                            ("len".to_string(), Value::int(e.field_len[slot])),
                            (
                                "terms".to_string(),
                                Value::sorted_object(std::mem::take(&mut tf_fields[slot])),
                            ),
                        ]),
                    )
                })
                .collect();
            let entry = Value::sorted_object(vec![
                ("path".to_string(), Value::string(e.path.clone())),
                ("type".to_string(), Value::string(e.mtype.as_str())),
                ("title".to_string(), Value::string(e.title.clone())),
                (
                    "tags".to_string(),
                    Value::Array(e.tags.iter().map(Value::string).collect()),
                ),
                (
                    "created".to_string(),
                    Value::string(format_rfc3339_utc(e.created)),
                ),
                (
                    "content_hash".to_string(),
                    Value::string(e.content_hash.clone()),
                ),
                ("fields".to_string(), Value::sorted_object(fields_obj)),
            ]);
            memories.push((id.clone(), entry));
        }
        let df_pairs: Vec<(String, Value)> = self
            .df()
            .into_iter()
            .map(|(t, n)| (t, Value::int(n)))
            .collect();
        let totals = self.total_field_len();
        let corpus = Value::sorted_object(vec![
            ("doc_count".to_string(), Value::int(self.doc_count())),
            ("df".to_string(), Value::Object(df_pairs)), // BTreeMap: sorted
            (
                "field_len_totals".to_string(),
                Value::sorted_object(
                    FIELDS
                        .iter()
                        .enumerate()
                        .map(|(i, name)| ((*name).to_string(), Value::int(totals[i])))
                        .collect(),
                ),
            ),
        ]);
        let doc = Value::sorted_object(vec![
            ("format_version".to_string(), Value::int(FORMAT_VERSION)),
            ("memories".to_string(), Value::Object(memories)), // BTreeMap: sorted
            ("corpus".to_string(), corpus),
        ]);
        doc.emit()
    }

    /// Write `<root>/.index/index.json` atomically.
    pub fn save(&self, root: &Path) -> Result<()> {
        let dir = root.join(".index");
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io {
            context: "creating index directory".to_string(),
            path: dir.display().to_string(),
            source: e,
        })?;
        let tmp = dir.join(".tmp-index.json");
        let path = index_path(root);
        let mut bytes = self.to_json();
        bytes.push('\n'); // one trailing newline, like every ghostie file
        std::fs::write(&tmp, bytes.as_bytes()).map_err(|e| Error::Io {
            context: "writing index".to_string(),
            path: tmp.display().to_string(),
            source: e,
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io {
            context: "renaming index into place".to_string(),
            path: path.display().to_string(),
            source: e,
        })
    }

    /// Read `<root>/.index/index.json`. `Ok(None)` when absent; `Err` when
    /// present but unusable (caller rebuilds).
    pub fn load(root: &Path) -> Result<Option<Index>> {
        let path = index_path(root);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::Io {
                    context: "reading index".to_string(),
                    path: path.display().to_string(),
                    source: e,
                });
            }
        };
        let origin = path.display().to_string();
        let v = json::parse_with_origin(text.trim_end(), &origin)?;
        let bad = |message: &str| Error::Invalid {
            origin: origin.clone(),
            message: message.to_string(),
        };
        if v.get("format_version").and_then(Value::as_i64) != Some(FORMAT_VERSION) {
            return Err(bad("unsupported or missing format_version"));
        }
        let mut docs = BTreeMap::new();
        let memories = v
            .get("memories")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("missing 'memories' object"))?;
        for (id, entry) in memories {
            let str_field = |key: &str| -> Result<String> {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| bad(&format!("memory '{id}': missing '{key}'")))
            };
            let mtype = MemoryType::parse(&str_field("type")?)
                .ok_or_else(|| bad(&format!("memory '{id}': bad type")))?;
            let created = parse_rfc3339_utc(&str_field("created")?)?;
            let tags = entry
                .get("tags")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let fields = entry
                .get("fields")
                .and_then(Value::as_object)
                .ok_or_else(|| bad(&format!("memory '{id}': missing 'fields'")))?;
            let mut tf: BTreeMap<String, [i64; 3]> = BTreeMap::new();
            let mut field_len = [0i64; 3];
            for (slot, name) in FIELDS.iter().enumerate() {
                let f = fields
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v)
                    .ok_or_else(|| bad(&format!("memory '{id}': missing field '{name}'")))?;
                field_len[slot] = f
                    .get("len")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| bad(&format!("memory '{id}': field '{name}' missing len")))?;
                if let Some(terms) = f.get("terms").and_then(Value::as_object) {
                    for (term, count) in terms {
                        let c = count.as_i64().ok_or_else(|| {
                            bad(&format!("memory '{id}': non-integer tf for '{term}'"))
                        })?;
                        tf.entry(term.clone()).or_insert([0, 0, 0])[slot] = c;
                    }
                }
            }
            docs.insert(
                id.clone(),
                DocEntry {
                    id: id.clone(),
                    path: str_field("path")?,
                    mtype,
                    title: str_field("title")?,
                    tags,
                    created,
                    content_hash: str_field("content_hash")?,
                    tf,
                    field_len,
                },
            );
        }
        Ok(Some(Index { docs }))
    }
}

/// `<root>/.index/index.json`.
pub fn index_path(root: &Path) -> PathBuf {
    root.join(".index").join("index.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;
    use crate::store::{ListFilter, NewMemory, Store};
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

    fn seed(store: &Store) {
        let clock = FixedClock(T0);
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Fact),
                    title: "Configs live in etc".to_string(),
                    tags: vec!["layout".to_string()],
                    body: "All configs live in etc/ and load at boot.\n".to_string(),
                    ..NewMemory::default()
                },
                &clock,
            )
            .unwrap();
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Rule),
                    title: "Run verifyGate before commit".to_string(),
                    tags: vec!["ci".to_string(), "discipline".to_string()],
                    body: "Always run the verifyGate script.\n".to_string(),
                    ..NewMemory::default()
                },
                &clock,
            )
            .unwrap();
    }

    #[test]
    fn build_produces_byte_identical_json_across_runs() {
        let tmp = TempDir::new("idx-stable");
        let store = Store::open(tmp.path());
        seed(&store);
        let (a, _) = Index::build(&store).unwrap();
        let (b, _) = Index::build(&store).unwrap();
        assert_eq!(a.to_json(), b.to_json(), "same files -> same index bytes");
        // And across two separate identically-seeded stores.
        let tmp2 = TempDir::new("idx-stable-2");
        let store2 = Store::open(tmp2.path());
        seed(&store2);
        let (c, _) = Index::build(&store2).unwrap();
        assert_eq!(a.to_json(), c.to_json(), "paths are relative: portable");
    }

    #[test]
    fn golden_index_shape() {
        let tmp = TempDir::new("idx-golden");
        let store = Store::open(tmp.path());
        seed(&store);
        let (idx, _) = Index::build(&store).unwrap();
        let json = idx.to_json();
        // Structural spot-checks (full goldens live in the store test bead).
        assert!(
            json.starts_with(r#"{"corpus":{"df":{"always":1,"#),
            "{json}"
        );
        assert!(json.contains(r#""doc_count":2"#), "{json}");
        assert!(
            json.contains(r#""verifygate":1"#),
            "df counts docs (rule only), not occurrences: {json}"
        );
        assert!(
            json.contains(r#""path":"memories/fact-configs-live-in-etc-1.md""#),
            "{json}"
        );
        // Subtokens indexed: verifyGate -> verify + gate.
        assert!(json.contains(r#""verify":"#), "{json}");
    }

    #[test]
    fn save_load_round_trips() {
        let tmp = TempDir::new("idx-roundtrip");
        let store = Store::open(tmp.path());
        seed(&store);
        let (idx, _) = Index::build(&store).unwrap();
        idx.save(store.root()).unwrap();
        let loaded = Index::load(store.root()).unwrap().expect("index exists");
        assert_eq!(idx, loaded, "load(save(idx)) == idx");
        assert_eq!(idx.to_json(), loaded.to_json());
    }

    #[test]
    fn ensure_fresh_detects_hand_edits_by_hash() {
        let tmp = TempDir::new("idx-stale");
        let store = Store::open(tmp.path());
        seed(&store);
        let (idx, _) = Index::ensure_fresh(&store).unwrap();
        assert!(
            idx.docs["fact-configs-live-in-etc-1"]
                .tf
                .contains_key("boot")
        );
        // Hand-edit out-of-band (no mtime games needed: hash is the truth).
        let path = store.memories_dir().join("fact-configs-live-in-etc-1.md");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("boot", "reindexplease")).unwrap();
        let (idx2, _) = Index::ensure_fresh(&store).unwrap();
        assert!(
            idx2.docs["fact-configs-live-in-etc-1"]
                .tf
                .contains_key("reindexplease"),
            "stale entry refreshed from file content"
        );
        assert!(
            !idx2.docs["fact-configs-live-in-etc-1"]
                .tf
                .contains_key("boot")
        );
    }

    #[test]
    fn ensure_fresh_handles_new_and_deleted_files() {
        let tmp = TempDir::new("idx-newdel");
        let store = Store::open(tmp.path());
        seed(&store);
        let (idx, _) = Index::ensure_fresh(&store).unwrap();
        assert_eq!(idx.doc_count(), 2);
        store.delete("fact-configs-live-in-etc-1").unwrap();
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Decision),
                    title: "New decision".to_string(),
                    ..NewMemory::default()
                },
                &FixedClock(T0),
            )
            .unwrap();
        let (idx2, _) = Index::ensure_fresh(&store).unwrap();
        assert_eq!(idx2.doc_count(), 2);
        assert!(idx2.docs.contains_key("decision-new-decision-1"));
        assert!(!idx2.docs.contains_key("fact-configs-live-in-etc-1"));
    }

    #[test]
    fn deleting_the_index_never_changes_results() {
        let tmp = TempDir::new("idx-derivable");
        let store = Store::open(tmp.path());
        seed(&store);
        let (idx, _) = Index::ensure_fresh(&store).unwrap();
        let before = idx.to_json();
        std::fs::remove_dir_all(store.root().join(".index")).unwrap();
        let (rebuilt, _) = Index::ensure_fresh(&store).unwrap();
        assert_eq!(rebuilt.to_json(), before, "rebuild == original, byte-exact");
        // And list (files-only path) is obviously unaffected.
        let (memories, _) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(memories.len(), 2);
    }

    #[test]
    fn crud_writes_keep_an_existing_index_warm() {
        let tmp = TempDir::new("idx-warm");
        let store = Store::open(tmp.path());
        seed(&store);
        let (_, _) = Index::ensure_fresh(&store).unwrap(); // materialize it
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Fact),
                    title: "warm entry".to_string(),
                    ..NewMemory::default()
                },
                &FixedClock(T0),
            )
            .unwrap();
        // Loaded straight from disk: create refreshed it best-effort.
        let on_disk = Index::load(store.root()).unwrap().expect("index exists");
        assert!(on_disk.docs.contains_key("fact-warm-entry-1"));
        store.delete("fact-warm-entry-1").unwrap();
        let on_disk = Index::load(store.root()).unwrap().expect("index exists");
        assert!(!on_disk.docs.contains_key("fact-warm-entry-1"));
    }

    #[test]
    fn corrupt_index_is_rebuilt_with_a_warning() {
        let tmp = TempDir::new("idx-corrupt");
        let store = Store::open(tmp.path());
        seed(&store);
        let dir = store.root().join(".index");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(index_path(store.root()), "{ not json").unwrap();
        let (idx, warnings) = Index::ensure_fresh(&store).unwrap();
        assert_eq!(idx.doc_count(), 2, "rebuilt from files");
        assert!(
            warnings.iter().any(|w| w.message.contains("rebuilding")),
            "warned: {warnings:?}"
        );
    }
}
