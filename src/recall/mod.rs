//! recall — deterministic retrieval with why-explanations.
//!
//! Single responsibility: given a task or query, surface the relevant
//! memories. Clean-room BM25 in fixed-point i64 (floats are banned here; the
//! gate greps for them), one shared tokenizer for index and query time, and
//! a per-hit explanation whose term contributions sum exactly to the score.
//!
//! Pipeline: tokenize query -> corpus stats (index, hash-verified fresh;
//! a deleted or unusable index is rebuilt from the files, so results are
//! IDENTICAL with or without it) -> BM25 -> type/tag filters (BEFORE
//! truncation, so filters don't starve results) -> rerank stage (identity
//! until the post-milestone hash-embedding bead) -> explanations -> top-k
//! in the total order (score desc, id asc).

pub mod bm25;
pub mod explain;
pub mod tokenize;

use crate::error::{Result, Warning};
use crate::json::Value;
use crate::recall::bm25::{ScoredDoc, bm25_scores};
use crate::recall::explain::{Explanation, ignored_terms};
use crate::store::Store;
use crate::store::index::Index;
use crate::store::memory::MemoryType;
use crate::util::format_rfc3339_utc;

/// Options for [`recall`].
#[derive(Debug, Clone)]
pub struct RecallOpts {
    /// Maximum hits returned.
    pub k: usize,
    /// Only memories of this type.
    pub mtype: Option<MemoryType>,
    /// Only memories carrying this tag.
    pub tag: Option<String>,
    /// Score floor in micro-units; 0 = no cutoff (default; revisit after
    /// dogfooding).
    pub min_score_micros: i64,
}

impl Default for RecallOpts {
    fn default() -> Self {
        RecallOpts {
            k: 10,
            mtype: None,
            tag: None,
            min_score_micros: 0,
        }
    }
}

/// One recall hit: identity for rendering plus the full why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallHit {
    /// Memory id.
    pub id: String,
    /// Memory type.
    pub mtype: MemoryType,
    /// Title.
    pub title: String,
    /// Created, epoch seconds.
    pub created: i64,
    /// Path relative to the store root (agents open the file directly).
    pub path: String,
    /// BM25 score, micro-units.
    pub score_micros: i64,
    /// The why.
    pub explanation: Explanation,
}

/// The pipeline output: ranked hits plus structured warnings (skipped
/// unparseable files, index problems) so `--json` surfaces store problems
/// instead of hiding them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallResult {
    /// Ranked hits, best first.
    pub hits: Vec<RecallHit>,
    /// Non-fatal problems encountered on the way.
    pub warnings: Vec<Warning>,
}

impl RecallHit {
    /// Robot rendering, fixed key order, byte-stable.
    pub fn to_json(&self) -> Value {
        Value::Object(vec![
            ("id".to_string(), Value::string(self.id.clone())),
            ("type".to_string(), Value::string(self.mtype.as_str())),
            ("title".to_string(), Value::string(self.title.clone())),
            (
                "created".to_string(),
                Value::string(format_rfc3339_utc(self.created)),
            ),
            ("path".to_string(), Value::string(self.path.clone())),
            ("score_micros".to_string(), Value::int(self.score_micros)),
            ("why".to_string(), self.explanation.to_json()),
        ])
    }
}

impl RecallResult {
    /// Robot rendering of the whole result (the CLI wraps this in the
    /// standard envelope).
    pub fn to_json(&self) -> Value {
        Value::Object(vec![
            (
                "hits".to_string(),
                Value::Array(self.hits.iter().map(RecallHit::to_json).collect()),
            ),
            (
                "warnings".to_string(),
                Value::Array(
                    self.warnings
                        .iter()
                        .map(|w| {
                            Value::Object(vec![
                                ("origin".to_string(), Value::string(w.origin.clone())),
                                ("message".to_string(), Value::string(w.message.clone())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// The rerank slot: identity until the post-milestone hash-embedding
/// rerank bead fills it. The pipeline shape will not change — only this
/// stage.
fn rerank_stage(hits: Vec<ScoredDoc>) -> Vec<ScoredDoc> {
    hits
}

/// The library-level entry point the CLI fronts.
///
/// Empty query, empty store and zero hits all return cleanly with empty
/// hits (and a warning where helpful) — never an error, never a panic.
pub fn recall(store: &Store, query: &str, opts: &RecallOpts) -> Result<RecallResult> {
    let (index, mut warnings) = Index::ensure_fresh(store)?;
    let query_terms = tokenize::tokenize(query);
    if query_terms.is_empty() && !query.trim().is_empty() {
        warnings.push(Warning {
            origin: "<query>".to_string(),
            message: "query contains only stopwords/punctuation; nothing to match".to_string(),
        });
    }
    let scored = rerank_stage(bm25_scores(&query_terms, &index));
    let ignored = ignored_terms(query, &index);
    let mut hits = Vec::new();
    for s in scored {
        let Some(entry) = index.docs.get(&s.id) else {
            continue; // unreachable: scorer only sees indexed docs
        };
        // Filter BEFORE truncating to k, so filters don't starve results.
        if let Some(t) = opts.mtype
            && entry.mtype != t
        {
            continue;
        }
        if let Some(tag) = &opts.tag
            && !entry.tags.iter().any(|x| x == tag)
        {
            continue;
        }
        if opts.min_score_micros > 0 && s.score_micros < opts.min_score_micros {
            continue;
        }
        let explanation = Explanation::for_hit(&s, &ignored);
        hits.push(RecallHit {
            id: s.id.clone(),
            mtype: entry.mtype,
            title: entry.title.clone(),
            created: entry.created,
            path: entry.path.clone(),
            score_micros: s.score_micros,
            explanation,
        });
        if hits.len() == opts.k {
            break;
        }
    }
    Ok(RecallResult { hits, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::TempDir;
    use crate::store::{NewMemory, Store};
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

    fn seed(store: &Store) {
        let clock = FixedClock(T0);
        let mems = [
            (
                MemoryType::Rule,
                "Sync branch is sync never main",
                vec!["git", "sync"],
                "Always push the memory store to the sync branch, never to main.",
            ),
            (
                MemoryType::Decision,
                "Chose fixed-point over floats",
                vec!["determinism"],
                "Floats round differently across FPUs; scores are i64 micros.",
            ),
            (
                MemoryType::Fact,
                "Configs live in etc",
                vec!["layout"],
                "All configs live in etc/ and load at boot.",
            ),
            (
                MemoryType::Fact,
                "Tokenizer splits parseHttpRequest",
                vec!["recall"],
                "Code-aware subtokens: compound plus parts.",
            ),
        ];
        for (mtype, title, tags, body) in mems {
            store
                .create(
                    &NewMemory {
                        mtype: Some(mtype),
                        title: title.to_string(),
                        tags: tags.iter().map(|s| s.to_string()).collect(),
                        body: format!("{body}\n"),
                        ..NewMemory::default()
                    },
                    &clock,
                )
                .unwrap();
        }
    }

    #[test]
    fn right_memory_surfaces_with_a_why() {
        let tmp = TempDir::new("recall-basic");
        let store = Store::open(tmp.path());
        seed(&store);
        let r = recall(&store, "which branch do we sync to", &RecallOpts::default()).unwrap();
        assert!(!r.hits.is_empty());
        assert_eq!(r.hits[0].id, "rule-sync-branch-is-sync-never-main-1");
        assert!(r.hits[0].explanation.contributions_sum_exactly());
        assert!(!r.hits[0].explanation.matched_terms.is_empty());
    }

    #[test]
    fn indexed_and_indexless_results_are_byte_equal() {
        let tmp = TempDir::new("recall-indexless");
        let store = Store::open(tmp.path());
        seed(&store);
        let q = "why did we avoid floats";
        let with_index = recall(&store, q, &RecallOpts::default()).unwrap();
        std::fs::remove_dir_all(store.root().join(".index")).unwrap();
        let without_index = recall(&store, q, &RecallOpts::default()).unwrap();
        assert_eq!(
            with_index.to_json().emit(),
            without_index.to_json().emit(),
            "the index is never authoritative — byte-equal results"
        );
    }

    #[test]
    fn filters_apply_before_truncation() {
        let tmp = TempDir::new("recall-filter");
        let store = Store::open(tmp.path());
        seed(&store);
        // Probed unfiltered order for this query: the fact outranks the
        // rule. So k=1 + type=rule ONLY works if the filter runs before
        // truncation — truncate-first starves the rule (sabotage-checked).
        let r = recall(
            &store,
            "sync configs etc",
            &RecallOpts {
                k: 1,
                mtype: Some(MemoryType::Rule),
                ..RecallOpts::default()
            },
        )
        .unwrap();
        assert_eq!(r.hits.len(), 1, "filter-then-truncate must find the rule");
        assert_eq!(r.hits[0].mtype, MemoryType::Rule);
        // Tag filter.
        let r = recall(
            &store,
            "sync branch",
            &RecallOpts {
                tag: Some("git".to_string()),
                ..RecallOpts::default()
            },
        )
        .unwrap();
        assert!(r.hits.iter().all(|h| h.id.starts_with("rule-sync")));
    }

    #[test]
    fn k_truncates() {
        let tmp = TempDir::new("recall-k");
        let store = Store::open(tmp.path());
        seed(&store);
        // "live" matches configs; "sync" matches the rule; "floats" the
        // decision — broad query, k=2 keeps the top two only.
        let all = recall(&store, "sync floats configs", &RecallOpts::default()).unwrap();
        assert!(all.hits.len() >= 3);
        let two = recall(
            &store,
            "sync floats configs",
            &RecallOpts {
                k: 2,
                ..RecallOpts::default()
            },
        )
        .unwrap();
        assert_eq!(two.hits.len(), 2);
        assert_eq!(two.hits[0].id, all.hits[0].id);
        assert_eq!(two.hits[1].id, all.hits[1].id);
    }

    #[test]
    fn determinism_double_run_byte_equal() {
        let tmp = TempDir::new("recall-determinism");
        let store = Store::open(tmp.path());
        seed(&store);
        let a = recall(&store, "sync branch", &RecallOpts::default()).unwrap();
        let b = recall(&store, "sync branch", &RecallOpts::default()).unwrap();
        assert_eq!(a.to_json().emit(), b.to_json().emit());
    }

    #[test]
    fn corrupt_file_warning_propagates() {
        let tmp = TempDir::new("recall-warn");
        let store = Store::open(tmp.path());
        seed(&store);
        std::fs::write(store.memories_dir().join("broken.md"), "not a memory").unwrap();
        let r = recall(&store, "sync branch", &RecallOpts::default()).unwrap();
        assert!(!r.hits.is_empty(), "recall not held hostage by one typo");
        assert!(
            r.warnings.iter().any(|w| w.origin.contains("broken.md")),
            "warning surfaced: {:?}",
            r.warnings
        );
    }

    #[test]
    fn empty_query_empty_store_and_no_hits_return_cleanly() {
        let tmp = TempDir::new("recall-empty");
        let store = Store::open(tmp.path());
        // Empty store.
        let r = recall(&store, "anything", &RecallOpts::default()).unwrap();
        assert!(r.hits.is_empty());
        seed(&store);
        // Empty query.
        let r = recall(&store, "", &RecallOpts::default()).unwrap();
        assert!(r.hits.is_empty());
        // Stopword-only query warns.
        let r = recall(&store, "the of and", &RecallOpts::default()).unwrap();
        assert!(r.hits.is_empty());
        assert!(r.warnings.iter().any(|w| w.message.contains("stopword")));
        // No overlap.
        let r = recall(&store, "xylophone quantum", &RecallOpts::default()).unwrap();
        assert!(r.hits.is_empty());
    }

    #[test]
    fn code_aware_subtokens_reach_across_symbols() {
        let tmp = TempDir::new("recall-subtok");
        let store = Store::open(tmp.path());
        seed(&store);
        // Query words only exist inside the camelCase symbol.
        let r = recall(&store, "http request", &RecallOpts::default()).unwrap();
        assert!(!r.hits.is_empty(), "subtoken match must fire");
        assert_eq!(r.hits[0].id, "fact-tokenizer-splits-parsehttprequest-1");
    }
}
