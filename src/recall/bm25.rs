//! Clean-room Okapi BM25 in fixed-point i64 micro-units, implemented from
//! the published formula — no code consulted or ported from any search
//! library. Floats are BANNED in this module (the gate greps for them):
//! floats round differently across FPUs and optimization levels, and
//! byte-stable scores are a headline property.
//!
//! # Fixed-point scheme
//!
//! - All scores are i64 **micro-units** (`util::SCALE` = 1_000_000).
//! - Parameters: `k1 = 1.2` (`K1_MICROS`), `b = 0.75` (`B_MICROS`).
//! - Rounding: round-half-up via `util::mul_div_round` (i128 inside).
//! - IDF: `ln((N - df + 0.5) / (df + 0.5) + 1)`, which simplifies to
//!   `ln((2N + 2) / (2df + 1))` — an integer rational, fed to
//!   `util::ln_micros` (atanh series, documented there). Precomputed once
//!   per query term.
//! - Field weighting (weighted-field BM25): integer multipliers applied to
//!   tf and doc length before saturation — title x2, tags x2, body x1.
//!   Simple, explainable, adjustable later.
//! - Average length stays a rational `(total_weighted_len, doc_count)`;
//!   it is never pre-divided.
//!
//! Per document: `score = Σ_t idf(t) * tf_w(t) * (k1 + 1) /
//! (tf_w(t) + k1 * (1 - b + b * len_w / avglen_w))`.
//!
//! # Ranking contract
//!
//! Score descending, ties broken by memory id lexicographic ascending — a
//! total order, always, so output is byte-stable.

use crate::store::index::Index;
use crate::util::{SCALE, ln_micros, mul_div_round};

/// k1 = 1.2 in micro-units.
pub const K1_MICROS: i64 = 1_200_000;
/// b = 0.75 in micro-units.
pub const B_MICROS: i64 = 750_000;
/// Field weights `[title, tags, body]` (plain integers, not micros).
pub const FIELD_WEIGHTS: [i64; 3] = [2, 2, 1];

/// Per-term scoring breakdown for one document. The explanations layer
/// consumes this; it is produced here so whys are never reconstructed
/// after the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermBreakdown {
    /// The (already tokenized, lowercased) query term.
    pub term: String,
    /// Term frequency per field `[title, tags, body]`.
    pub tf_per_field: [i64; 3],
    /// Corpus document frequency of the term.
    pub df: i64,
    /// Precomputed idf, micro-units.
    pub idf_micros: i64,
    /// This term's exact contribution to the document score, micro-units.
    pub contribution_micros: i64,
}

/// One scored document: total score plus its exact per-term breakdown.
/// Invariant (unit-tested and gate-checked): the contributions sum to
/// `score_micros`, exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoredDoc {
    /// Memory id.
    pub id: String,
    /// BM25 score, micro-units.
    pub score_micros: i64,
    /// Per-term breakdown, sorted contribution desc then term asc.
    pub terms: Vec<TermBreakdown>,
}

/// Score every document in the corpus against the query terms.
///
/// - Query terms are deduplicated (first occurrence wins) so repeated
///   words don't double-score.
/// - Terms absent from the corpus (df = 0) contribute nothing; the
///   pipeline reports them as ignored.
/// - Documents matching no term are omitted.
/// - Result is in the total order: score desc, id asc.
pub fn bm25_scores(query_terms: &[String], index: &Index) -> Vec<ScoredDoc> {
    let n = index.doc_count();
    if n == 0 || query_terms.is_empty() {
        return Vec::new();
    }
    // Dedup, preserving first-seen order (determinism).
    let mut terms: Vec<&String> = Vec::new();
    for t in query_terms {
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    let df_map = index.df();
    // idf per term, precomputed once: ln((2N + 2) / (2df + 1)).
    let with_idf: Vec<(&String, i64, i64)> = terms
        .iter()
        .filter_map(|t| {
            let df = *df_map.get(*t)?;
            (df > 0).then(|| (*t, df, ln_micros(2 * n + 2, 2 * df + 1)))
        })
        .collect();
    if with_idf.is_empty() {
        return Vec::new();
    }
    let totals = index.total_field_len();
    // Weighted total corpus length; average stays rational (total_w, n).
    let total_w: i64 = FIELD_WEIGHTS
        .iter()
        .zip(totals.iter())
        .map(|(w, l)| w * l)
        .sum();

    let mut out: Vec<ScoredDoc> = Vec::new();
    for (id, doc) in &index.docs {
        let len_w: i64 = FIELD_WEIGHTS
            .iter()
            .zip(doc.field_len.iter())
            .map(|(w, l)| w * l)
            .sum();
        // norm = 1 - b + b * len_w / avglen_w, in micros. With the average
        // as the rational total_w / n: b * len_w * n / total_w.
        let norm_micros = if total_w == 0 {
            SCALE // degenerate corpus of empty docs: no length penalty
        } else {
            (SCALE - B_MICROS) + mul_div_round(B_MICROS, len_w * n, total_w)
        };
        let mut breakdown: Vec<TermBreakdown> = Vec::new();
        let mut score: i64 = 0;
        for (term, df, idf_micros) in &with_idf {
            let Some(tf) = doc.tf.get(*term) else {
                continue;
            };
            let tf_w: i64 = FIELD_WEIGHTS
                .iter()
                .zip(tf.iter())
                .map(|(w, f)| w * f)
                .sum();
            if tf_w == 0 {
                continue;
            }
            // numerator: tf_w * (k1 + 1); denominator: tf_w + k1 * norm.
            let numer_micros = tf_w * (K1_MICROS + SCALE);
            let denom_micros = tf_w * SCALE + mul_div_round(K1_MICROS, norm_micros, SCALE);
            let contribution = mul_div_round(*idf_micros, numer_micros, denom_micros);
            score += contribution;
            breakdown.push(TermBreakdown {
                term: (*term).clone(),
                tf_per_field: *tf,
                df: *df,
                idf_micros: *idf_micros,
                contribution_micros: contribution,
            });
        }
        if score > 0 {
            // Total order inside the breakdown too.
            breakdown.sort_by(|a, b| {
                b.contribution_micros
                    .cmp(&a.contribution_micros)
                    .then_with(|| a.term.cmp(&b.term))
            });
            out.push(ScoredDoc {
                id: id.clone(),
                score_micros: score,
                terms: breakdown,
            });
        }
    }
    out.sort_by(|a, b| {
        b.score_micros
            .cmp(&a.score_micros)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::index::DocEntry;
    use crate::store::memory::MemoryType;
    use std::collections::BTreeMap;

    /// Hand-built two-doc corpus for arithmetic checks.
    ///
    /// Doc A "fact-sync-1": title tokens [sync] (len 1), no tags, no body.
    /// Doc B "fact-other-1": title [other] (len 1), body [unrelated, words]
    /// (len 2).
    fn tiny_corpus() -> Index {
        let mut docs = BTreeMap::new();
        let mut tf_a = BTreeMap::new();
        tf_a.insert("sync".to_string(), [1i64, 0, 0]);
        docs.insert(
            "fact-sync-1".to_string(),
            DocEntry {
                id: "fact-sync-1".to_string(),
                path: "memories/fact-sync-1.md".to_string(),
                mtype: MemoryType::Fact,
                title: "sync".to_string(),
                tags: vec![],
                created: 0,
                links: vec![],
                harness: None,
                core: None,
                rationale: None,
                scope: None,
                content_hash: "0".repeat(16),
                tf: tf_a,
                field_len: [1, 0, 0],
            },
        );
        let mut tf_b = BTreeMap::new();
        tf_b.insert("other".to_string(), [1i64, 0, 0]);
        tf_b.insert("unrelated".to_string(), [0i64, 0, 1]);
        tf_b.insert("words".to_string(), [0i64, 0, 1]);
        docs.insert(
            "fact-other-1".to_string(),
            DocEntry {
                id: "fact-other-1".to_string(),
                path: "memories/fact-other-1.md".to_string(),
                mtype: MemoryType::Fact,
                title: "other".to_string(),
                tags: vec![],
                created: 0,
                links: vec![],
                harness: None,
                core: None,
                rationale: None,
                scope: None,
                content_hash: "0".repeat(16),
                tf: tf_b,
                field_len: [1, 0, 2],
            },
        );
        Index { docs }
    }

    #[test]
    fn hand_computed_micro_score() {
        // Query "sync" against tiny_corpus. The arithmetic, in full:
        //   N = 2 docs, df(sync) = 1.
        //   idf = ln((2N+2)/(2df+1)) = ln(6/3) = ln 2 = 693147 micros.
        //   Doc A: tf_w = 2*1 (title weight 2) = 2.
        //     len_w(A) = 2*1 = 2; len_w(B) = 2*1 + 1*2 = 4; total_w = 6.
        //     norm = 1 - 0.75 + 0.75 * len_w * N / total_w
        //          = 0.25 + 0.75 * 2 * 2 / 6 = 0.25 + 0.5 = 0.75
        //          -> 250000 + 750000*4/6 = 250000 + 500000 = 750000.
        //     numer = tf_w * (k1+1) = 2 * 2200000 = 4400000.
        //     denom = tf_w * SCALE + k1*norm/SCALE
        //           = 2000000 + 1200000*750000/1000000 = 2900000.
        //     score = idf * numer / denom = 693147 * 4400000 / 2900000
        //           = 30498468/29 = 1051671.31... -> 1051671 (half-up).
        let hits = bm25_scores(&["sync".to_string()], &tiny_corpus());
        assert_eq!(hits.len(), 1, "only doc A matches");
        assert_eq!(hits[0].id, "fact-sync-1");
        assert_eq!(hits[0].score_micros, 1_051_671);
        let t = &hits[0].terms[0];
        assert_eq!(t.term, "sync");
        assert_eq!(t.df, 1);
        assert_eq!(t.idf_micros, 693_147);
        assert_eq!(t.tf_per_field, [1, 0, 0]);
        assert_eq!(t.contribution_micros, 1_051_671);
    }

    #[test]
    fn contributions_sum_to_score_exactly() {
        let hits = bm25_scores(
            &["sync".to_string(), "words".to_string(), "other".to_string()],
            &tiny_corpus(),
        );
        for hit in &hits {
            let sum: i64 = hit.terms.iter().map(|t| t.contribution_micros).sum();
            assert_eq!(sum, hit.score_micros, "whys that don't sum are lies");
        }
    }

    #[test]
    fn ties_break_by_id_ascending() {
        // Two identical docs must produce identical scores; order by id.
        let mut idx = tiny_corpus();
        let mut clone = idx.docs["fact-sync-1"].clone();
        clone.id = "fact-async-1".to_string(); // sorts before fact-sync-1
        clone.path = "memories/fact-async-1.md".to_string();
        idx.docs.insert(clone.id.clone(), clone);
        let hits = bm25_scores(&["sync".to_string()], &idx);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].score_micros, hits[1].score_micros, "true tie");
        assert_eq!(hits[0].id, "fact-async-1");
        assert_eq!(hits[1].id, "fact-sync-1");
    }

    #[test]
    fn zero_hit_and_unknown_term_queries_return_empty() {
        assert!(bm25_scores(&["absent".to_string()], &tiny_corpus()).is_empty());
        assert!(bm25_scores(&[], &tiny_corpus()).is_empty());
        assert!(bm25_scores(&["sync".to_string()], &Index::default()).is_empty());
    }

    #[test]
    fn repeated_query_terms_do_not_double_score() {
        let once = bm25_scores(&["sync".to_string()], &tiny_corpus());
        let twice = bm25_scores(&["sync".to_string(), "sync".to_string()], &tiny_corpus());
        assert_eq!(once, twice, "query term dedup");
    }

    #[test]
    fn determinism_double_run() {
        let q = vec!["sync".to_string(), "unrelated".to_string()];
        assert_eq!(
            bm25_scores(&q, &tiny_corpus()),
            bm25_scores(&q, &tiny_corpus())
        );
    }

    #[test]
    fn title_weighting_beats_body_for_same_term() {
        // Same term once in A's title vs once in B's body, equal lengths.
        let mut docs = BTreeMap::new();
        for (id, slot) in [("fact-a-1", 0usize), ("fact-b-1", 2usize)] {
            let mut tf = BTreeMap::new();
            let mut tfv = [0i64; 3];
            tfv[slot] = 1;
            tf.insert("gate".to_string(), tfv);
            let mut field_len = [0i64; 3];
            field_len[slot] = 1;
            docs.insert(
                id.to_string(),
                DocEntry {
                    id: id.to_string(),
                    path: format!("memories/{id}.md"),
                    mtype: MemoryType::Fact,
                    title: String::new(),
                    tags: vec![],
                    created: 0,
                    links: vec![],
                    harness: None,
                    core: None,
                    rationale: None,
                    scope: None,
                    content_hash: "0".repeat(16),
                    tf,
                    field_len,
                },
            );
        }
        let hits = bm25_scores(&["gate".to_string()], &Index { docs });
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "fact-a-1", "title hit outranks body hit");
        assert!(hits[0].score_micros > hits[1].score_micros);
    }
}
