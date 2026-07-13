//! Recall capability-level safety net (bead ghostie-rs-zya.3.6):
//! determinism, checked-in goldens, edge cases.
//!
//! GOLDEN REGENERATION (a conscious, reviewed act — never automatic):
//!   cargo test --test recall_goldens regenerate_goldens -- --ignored
//! then review the diff under tests/fixtures/recall/expected/ before
//! committing. Any scoring change fails these tests loudly first.
//!
//! Cross-platform honesty: this suite runs on macOS locally AND Linux in
//! the Docker gate — fixed-point integer math is the reason the same
//! golden bytes can match on both; any divergence is a P0 bug in the
//! fixed-point layer.

use ghostie::json::{self, Value};
use ghostie::recall::{RecallOpts, recall};
use ghostie::store::memory::MemoryType;
use ghostie::store::{NewMemory, Store};
use ghostie::util::FixedClock;
use std::path::{Path, PathBuf};

const T0: i64 = 1_783_944_000;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn expected_dir() -> PathBuf {
    fixtures_dir().join("recall/expected")
}

fn corpus_store(label: &str) -> (PathBuf, Store) {
    let root = std::env::temp_dir().join(format!(
        "ghostie-recall-goldens-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mem_dir = root.join("memories");
    std::fs::create_dir_all(&mem_dir).unwrap();
    for entry in std::fs::read_dir(fixtures_dir().join("dogfood/memories")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, mem_dir.join(path.file_name().unwrap())).unwrap();
    }
    (root.clone(), Store::open(root))
}

fn milestone_queries() -> Vec<String> {
    let text = std::fs::read_to_string(fixtures_dir().join("dogfood/expectations.json")).unwrap();
    let spec = json::parse(&text).unwrap();
    spec.get("entries")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter(|e| e.get("phase").and_then(Value::as_str) == Some("milestone"))
        .map(|e| e.get("query").and_then(Value::as_str).unwrap().to_string())
        .collect()
}

fn query_slug(q: &str) -> String {
    q.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Regenerates the goldens. Ignored by default; see the module doc.
#[test]
#[ignore = "writes goldens; run deliberately and review the diff"]
fn regenerate_goldens() {
    let (root, store) = corpus_store("regen");
    std::fs::create_dir_all(expected_dir()).unwrap();
    for q in milestone_queries() {
        let r = recall(&store, &q, &RecallOpts::default()).unwrap();
        let mut bytes = r.to_json().emit();
        bytes.push('\n');
        std::fs::write(
            expected_dir().join(format!("{}.json", query_slug(&q))),
            bytes,
        )
        .unwrap();
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn golden_results_for_every_labeled_query() {
    let (root, store) = corpus_store("golden");
    for q in milestone_queries() {
        let golden_path = expected_dir().join(format!("{}.json", query_slug(&q)));
        let want = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}\nregenerate deliberately: cargo test --test recall_goldens regenerate_goldens -- --ignored",
                golden_path.display()
            )
        });
        let r = recall(&store, &q, &RecallOpts::default()).unwrap();
        let mut got = r.to_json().emit();
        got.push('\n');
        assert_eq!(
            got, want,
            "{q:?}: scoring changed — if intended, regenerate goldens deliberately"
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn byte_exact_determinism_fresh_stores_and_indexless() {
    for q in milestone_queries() {
        let (root_a, store_a) = corpus_store("det-a");
        let (root_b, store_b) = corpus_store("det-b");
        let a = recall(&store_a, &q, &RecallOpts::default())
            .unwrap()
            .to_json()
            .emit();
        let b = recall(&store_b, &q, &RecallOpts::default())
            .unwrap()
            .to_json()
            .emit();
        assert_eq!(a, b, "{q:?}: fresh stores diverged");
        // Indexless: delete the index and repeat.
        std::fs::remove_dir_all(root_a.join(".index")).unwrap();
        let c = recall(&store_a, &q, &RecallOpts::default())
            .unwrap()
            .to_json()
            .emit();
        assert_eq!(a, c, "{q:?}: indexless path diverged");
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }
}

#[test]
fn explanation_invariants_corpus_wide() {
    let (root, store) = corpus_store("invariants");
    for q in milestone_queries() {
        let r = recall(&store, &q, &RecallOpts::default()).unwrap();
        for h in &r.hits {
            assert!(
                h.explanation.contributions_sum_exactly(),
                "{q:?}/{}: contributions must sum to score",
                h.id
            );
            // Graph-reached hits have no lexical terms by design (their why is
            // the edge they came in on); the sum invariant above still holds.
            assert!(
                !h.explanation.matched_terms.is_empty() || h.graph_via.is_some(),
                "{q:?}/{}: every lexical hit has a non-empty why",
                h.id
            );
            // ignored ∩ matched = ∅
            for ig in &h.explanation.ignored_terms {
                assert!(
                    h.explanation
                        .matched_terms
                        .iter()
                        .all(|m| m.term != ig.term),
                    "{q:?}/{}: '{}' both ignored and matched",
                    h.id,
                    ig.term
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

fn scratch_store(label: &str) -> (PathBuf, Store) {
    let root = std::env::temp_dir().join(format!(
        "ghostie-recall-edge-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    (root.clone(), Store::open(root))
}

#[test]
fn edge_single_memory_and_empty_body() {
    let (root, store) = scratch_store("single");
    store
        .create(
            &NewMemory {
                mtype: Some(MemoryType::Fact),
                title: "title only memory".to_string(),
                ..NewMemory::default() // empty body is valid
            },
            &FixedClock(T0),
        )
        .unwrap();
    let r = recall(&store, "title only", &RecallOpts::default()).unwrap();
    assert_eq!(r.hits.len(), 1, "single-memory store, empty body");
    assert!(r.hits[0].explanation.contributions_sum_exactly());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn edge_very_long_body_is_fast_and_correct() {
    let (root, store) = scratch_store("long");
    // 12k tokens of filler + one needle.
    let mut body = String::new();
    for i in 0..12_000 {
        body.push_str(&format!("filler{} ", i % 977));
    }
    body.push_str("needleterm\n");
    store
        .create(
            &NewMemory {
                mtype: Some(MemoryType::Fact),
                title: "the haystack".to_string(),
                body,
                ..NewMemory::default()
            },
            &FixedClock(T0),
        )
        .unwrap();
    let started = std::time::Instant::now();
    let r = recall(&store, "needleterm", &RecallOpts::default()).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r.hits.len(), 1);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "perf smoke: 12k-token body took {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn edge_unicode_query_against_unicode_memory() {
    let (root, store) = scratch_store("unicode");
    store
        .create(
            &NewMemory {
                mtype: Some(MemoryType::Fact),
                title: "Größenordnung der Läufe".to_string(),
                body: "Die GRÖSSE spielt eine Rolle für die Läufe.\n".to_string(),
                ..NewMemory::default()
            },
            &FixedClock(T0),
        )
        .unwrap();
    let r = recall(&store, "größenordnung läufe", &RecallOpts::default()).unwrap();
    assert_eq!(r.hits.len(), 1, "unicode lowercasing must line up");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn edge_filter_combinatorics() {
    let (root, store) = corpus_store("filters");
    // type+tag together.
    let r = recall(
        &store,
        "sync branch",
        &RecallOpts {
            mtype: Some(MemoryType::Rule),
            tag: Some("git".to_string()),
            ..RecallOpts::default()
        },
    )
    .unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].id, "rule-sync-branch-is-sync-never-main-1");
    // Filter matching nothing.
    let r = recall(
        &store,
        "sync branch",
        &RecallOpts {
            mtype: Some(MemoryType::Rule),
            tag: Some("no-such-tag".to_string()),
            ..RecallOpts::default()
        },
    )
    .unwrap();
    assert!(r.hits.is_empty(), "empty is clean, not an error");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn edge_empty_and_stopword_queries() {
    let (root, store) = corpus_store("empties");
    for q in ["", "   ", "the of and to"] {
        let r = recall(&store, q, &RecallOpts::default()).unwrap();
        assert!(r.hits.is_empty(), "{q:?} must return no hits, cleanly");
    }
    let _ = std::fs::remove_dir_all(root);
}
