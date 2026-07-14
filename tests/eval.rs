//! Recall eval harness: a labeled query set + P@1 / MRR, run with the
//! embedding rerank OFF (BM25 baseline) and ON, so the lift is measured, not
//! asserted by vibes. The near-miss queries are the point: a word the memory
//! never spells the same way (`sovereign` vs `sovereignty`), which BM25 alone
//! cannot reach.
//!
//! Floats are fine HERE (this is a test crate; the no-floats gate scans only
//! `src/recall` and `src/util`). Run it to see the numbers:
//!   cargo test --test eval -- --nocapture

use ghostie::recall::{RecallOpts, recall};
use ghostie::store::memory::MemoryType::{Decision, Fact, Rule};
use ghostie::store::{NewMemory, Store};
use ghostie::util::FixedClock;
use std::path::PathBuf;

const T0: i64 = 1_783_944_000;

/// Seed the eval corpus; return the store and the created ids in order.
fn corpus() -> (PathBuf, Store, Vec<String>) {
    let root = std::env::temp_dir().join(format!("ghostie-eval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = Store::open(&root);
    store.set_redaction(false); // eval measures recall, not redaction
    let clock = FixedClock(T0);
    let items = [
        (
            Rule,
            "Data sovereignty is the guarantee",
            "your context stays yours",
        ),
        (
            Decision,
            "Deterministic byte stable outputs",
            "same inputs produce the same bytes",
        ),
        (
            Fact,
            "The migration orchestrator preserves behavior",
            "a behavior preserving port",
        ),
        (
            Rule,
            "Always run verify before commit",
            "the gate is verify sh",
        ),
        (
            Fact,
            "Sync branch is sync never main",
            "push to the sync branch only",
        ),
        (
            Decision,
            "Chose fixed point over floating point",
            "floats round differently across fpus",
        ),
        (Fact, "Configuration lives in etc", "configs load at boot"),
        (
            Rule,
            "Encrypt secrets before pushing",
            "credentials never land in git",
        ),
        (
            Fact,
            "Tokenizer splits camelCase symbols",
            "parseHttpRequest becomes its parts",
        ),
        (
            Decision,
            "Retrieval uses hashed embeddings",
            "cosine similarity reranks the hits",
        ),
    ];
    let ids = items
        .iter()
        .map(|(ty, title, body)| {
            store
                .create(
                    &NewMemory {
                        mtype: Some(*ty),
                        title: title.to_string(),
                        body: format!("{body}\n"),
                        ..NewMemory::default()
                    },
                    &clock,
                )
                .unwrap()
                .id
        })
        .collect();
    (root, store, ids)
}

struct Report {
    p_at_1: f64,
    mrr: f64,
}

fn evaluate(store: &Store, queries: &[(&str, &str)], rerank: bool) -> Report {
    let n = queries.len() as f64;
    let mut p1 = 0.0;
    let mut mrr = 0.0;
    for (q, expected) in queries {
        let opts = RecallOpts {
            k: 5,
            rerank,
            ..RecallOpts::default()
        };
        let r = recall(store, q, &opts).unwrap();
        let rank = r.hits.iter().position(|h| &h.id == expected).map(|i| i + 1);
        if rank == Some(1) {
            p1 += 1.0;
        }
        if let Some(rk) = rank {
            mrr += 1.0 / rk as f64;
        }
    }
    Report {
        p_at_1: p1 / n,
        mrr: mrr / n,
    }
}

#[test]
fn rerank_lifts_recall_over_bm25_baseline() {
    let (root, store, ids) = corpus();
    // (query, expected memory) — the first six are NEAR-MISS: the query word is
    // never spelled the way the memory spells it, so BM25 alone cannot match.
    let queries: Vec<(&str, &str)> = vec![
        ("sovereign", ids[0].as_str()),   // -> "sovereignty"
        ("determinism", ids[1].as_str()), // -> "deterministic"
        ("migrate", ids[2].as_str()),     // -> "migration"
        ("encryption", ids[7].as_str()),  // -> "encrypt"
        ("configure", ids[6].as_str()),   // -> "configuration"
        ("tokenize", ids[8].as_str()),    // -> "tokenizer"
        ("sync branch", ids[4].as_str()), // exact (regression guards below)
        ("verify before commit", ids[3].as_str()),
        ("fixed point floats", ids[5].as_str()),
        ("hashed embeddings", ids[9].as_str()),
    ];
    let near_miss = &queries[..6];

    let base = evaluate(&store, &queries, false);
    let re = evaluate(&store, &queries, true);
    let base_nm = evaluate(&store, near_miss, false);
    let re_nm = evaluate(&store, near_miss, true);

    eprintln!(
        "\nrecall eval  (n={}, near-miss={})",
        queries.len(),
        near_miss.len()
    );
    eprintln!(
        "  baseline (BM25):   P@1={:.2}  MRR={:.2}",
        base.p_at_1, base.mrr
    );
    eprintln!(
        "  reranked (embed):  P@1={:.2}  MRR={:.2}",
        re.p_at_1, re.mrr
    );
    eprintln!(
        "  near-miss only:    baseline MRR={:.2}  ->  reranked MRR={:.2}\n",
        base_nm.mrr, re_nm.mrr
    );

    // The rerank must never regress overall, and must clearly lift the near-miss
    // subset that BM25 structurally cannot reach.
    assert!(
        re.mrr >= base.mrr,
        "rerank regressed MRR: {:.3} < {:.3}",
        re.mrr,
        base.mrr
    );
    assert!(
        base_nm.mrr < 0.2,
        "sanity: BM25 should mostly whiff on near-miss, got MRR {:.2}",
        base_nm.mrr
    );
    assert!(
        re_nm.mrr >= 0.6,
        "rerank should recover the near-miss queries, got MRR {:.2}",
        re_nm.mrr
    );

    // Regression guard: the exact queries must still be rank-1 with rerank on.
    for (q, expected) in &queries[6..] {
        let r = recall(&store, q, &RecallOpts::default()).unwrap();
        assert_eq!(
            r.hits.first().map(|h| h.id.as_str()),
            Some(*expected),
            "exact query {q:?} lost rank-1 under rerank"
        );
    }
    let _ = std::fs::remove_dir_all(root);
}
