//! Dogfood corpus tests (bead ghostie-rs-zya.3.7): the curated lantern
//! corpus plus labeled task-shaped queries. This test is the truth the
//! gate's DOGFOOD step re-proves through the compiled binary.
//!
//! Fixture files are copied into a temp store (never touched in place, so
//! no .index dir pollutes the checked-in fixtures).

use ghostie::json::{self, Value};
use ghostie::recall::{RecallOpts, recall};
use ghostie::store::frontmatter;
use ghostie::store::{ListFilter, Store};
use std::path::{Path, PathBuf};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dogfood")
}

/// Copy the corpus into a fresh temp store; caller cleans up via the
/// returned guard-ish path (best-effort removal at end of test).
fn corpus_store(label: &str) -> (PathBuf, Store) {
    let root = std::env::temp_dir().join(format!("ghostie-dogfood-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mem_dir = root.join("memories");
    std::fs::create_dir_all(&mem_dir).unwrap();
    for entry in std::fs::read_dir(fixtures_dir().join("memories")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, mem_dir.join(path.file_name().unwrap())).unwrap();
    }
    let store = Store::open(&root);
    (root, store)
}

#[test]
fn corpus_files_are_canonical_form() {
    // The corpus doubles as store goldens: every file must round-trip
    // byte-identically (parse -> serialize == original bytes).
    let dir = fixtures_dir().join("memories");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = frontmatter::parse(&text, &path.display().to_string())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            doc.serialize(),
            text,
            "{} is not canonical form",
            path.display()
        );
        checked += 1;
    }
    assert_eq!(checked, 14, "corpus size pinned");
}

#[test]
fn corpus_parses_clean_with_all_four_types() {
    let (root, store) = corpus_store("types");
    let (memories, warnings) = store.list(&ListFilter::default()).unwrap();
    assert_eq!(memories.len(), 14);
    assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
    for t in ghostie::store::memory::MemoryType::ALL {
        assert!(
            memories.iter().any(|m| m.mtype == t),
            "corpus must cover type {}",
            t.as_str()
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

/// The reusable assertion core: run every milestone-phase expectation
/// against the recall pipeline. The gate re-runs the same expectations
/// through the compiled binary.
#[test]
fn labeled_queries_hit_their_expected_memories() {
    let (root, store) = corpus_store("queries");
    let text = std::fs::read_to_string(fixtures_dir().join("expectations.json")).unwrap();
    let spec = json::parse_with_origin(&text, "expectations.json").unwrap();
    let entries = spec.get("entries").and_then(Value::as_array).unwrap();
    let mut ran = 0;
    for entry in entries {
        let phase = entry.get("phase").and_then(Value::as_str).unwrap();
        if phase != "milestone" {
            continue; // post-milestone cases belong to the rerank bead
        }
        let query = entry.get("query").and_then(Value::as_str).unwrap();
        let r = recall(&store, query, &RecallOpts::default()).unwrap();
        if entry
            .get("expect_zero_hits")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            assert!(
                r.hits.is_empty(),
                "{query:?}: expected zero hits, got {:?}",
                r.hits.iter().map(|h| &h.id).collect::<Vec<_>>()
            );
            ran += 1;
            continue;
        }
        if let Some(tops) = entry.get("expect_top").and_then(Value::as_array) {
            let want: Vec<&str> = tops.iter().filter_map(Value::as_str).collect();
            let got = r.hits.first().map(|h| h.id.as_str()).unwrap_or("<none>");
            assert!(
                want.contains(&got),
                "{query:?}: rank 1 is {got}, expected one of {want:?}; full order: {:?}",
                r.hits.iter().map(|h| &h.id).collect::<Vec<_>>()
            );
        }
        if let Some(in_top) = entry.get("expect_in_top_k").and_then(Value::as_array) {
            for case in in_top {
                let id = case.get("id").and_then(Value::as_str).unwrap();
                let k = case.get("k").and_then(Value::as_i64).unwrap() as usize;
                assert!(
                    r.hits.iter().take(k).any(|h| h.id == id),
                    "{query:?}: {id} not in top {k}; order: {:?}",
                    r.hits.iter().map(|h| &h.id).collect::<Vec<_>>()
                );
            }
        }
        if let Some(forbid) = entry.get("forbid_in_top_k").and_then(Value::as_array) {
            for case in forbid {
                let id = case.get("id").and_then(Value::as_str).unwrap();
                let k = case.get("k").and_then(Value::as_i64).unwrap() as usize;
                assert!(
                    r.hits.iter().take(k).all(|h| h.id != id),
                    "{query:?}: forbidden {id} appeared in top {k}"
                );
            }
        }
        // Every hit must carry a why. A lexical hit's why is its matched
        // terms (non-empty, sum-exact). A graph-reached hit's why is the edge
        // it came in on: no lexical terms, but a named seed and real mass.
        for h in &r.hits {
            if h.graph_via.is_some() {
                assert!(
                    h.graph_micros > 0,
                    "{query:?}: graph hit {} has no graph mass",
                    h.id
                );
                assert!(
                    h.explanation.matched_terms.is_empty()
                        && h.explanation.contributions_sum_exactly(),
                    "{query:?}: graph hit {} must have an empty, consistent lexical why",
                    h.id
                );
                continue;
            }
            assert!(
                !h.explanation.matched_terms.is_empty(),
                "{query:?}: hit {} has an empty why",
                h.id
            );
            assert!(
                h.explanation.contributions_sum_exactly(),
                "{query:?}: hit {} why does not sum",
                h.id
            );
        }
        ran += 1;
    }
    assert_eq!(ran, 11, "all milestone-phase expectations ran");
    let _ = std::fs::remove_dir_all(root);
}
