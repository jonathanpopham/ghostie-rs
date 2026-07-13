//! Why-explanations: every recall hit carries a machine-checkable,
//! human-readable WHY. For humans it answers "why is this here?"; for
//! agents in robot mode it is justification they can act on or discard.
//!
//! The INVARIANT (unit-tested here, re-checked by the gate dogfood):
//! the matched-term contributions sum to the total score, exactly, in
//! integer micros. Explanations that don't sum are lies.

use crate::json::Value;
use crate::recall::bm25::{ScoredDoc, TermBreakdown};
use crate::recall::tokenize::{is_stopword, tokenize_all};
use crate::store::index::{FIELDS, Index};

/// Why a query token contributed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoredReason {
    /// On the fixed stopword list.
    Stopword,
    /// Not a stopword, but no memory contains it.
    NotInCorpus,
}

impl IgnoredReason {
    /// Stable robot-mode spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            IgnoredReason::Stopword => "stopword",
            IgnoredReason::NotInCorpus => "not-in-corpus",
        }
    }
}

/// A query token that did nothing, and why — so "why did my word do
/// nothing" is answerable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredTerm {
    /// The token (lowercased).
    pub term: String,
    /// Why it was ignored.
    pub reason: IgnoredReason,
}

/// Rerank breakdown, designed now so the post-milestone hash-embedding
/// rerank bead extends rather than reshapes the schema. Always `None`
/// until that bead lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankExplanation {
    /// BM25 component, micro-units.
    pub bm25_micros: i64,
    /// Embedding cosine similarity component, micro-units.
    pub embed_sim_micros: i64,
    /// Blend weight for BM25, micro-units.
    pub alpha_micros: i64,
    /// Blend weight for the embedding, micro-units.
    pub beta_micros: i64,
    /// Top contributing shared subtokens.
    pub shared_subtokens: Vec<String>,
}

/// One hit's full explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    /// Matched terms, contribution desc then term asc (a total order).
    pub matched_terms: Vec<TermBreakdown>,
    /// Query tokens that were stopwords or absent from the corpus.
    pub ignored_terms: Vec<IgnoredTerm>,
    /// Total score; equals the sum of contributions, exactly.
    pub score_micros: i64,
    /// Present once the rerank stage exists (post-milestone).
    pub rerank: Option<RerankExplanation>,
}

impl Explanation {
    /// Assemble the explanation for one scored document. The breakdown
    /// arrives pre-sorted from the scorer; `ignored` comes from
    /// [`ignored_terms`] once per query.
    pub fn for_hit(scored: &ScoredDoc, ignored: &[IgnoredTerm]) -> Explanation {
        Explanation {
            matched_terms: scored.terms.clone(),
            ignored_terms: ignored.to_vec(),
            score_micros: scored.score_micros,
            rerank: None,
        }
    }

    /// The invariant, as a checkable predicate.
    pub fn contributions_sum_exactly(&self) -> bool {
        self.matched_terms
            .iter()
            .map(|t| t.contribution_micros)
            .sum::<i64>()
            == self.score_micros
    }

    /// Human rendering: one compact line under a hit, e.g.
    /// `why: title~"sync"(tf 1) body~"branch"(tf 3) — 2 matched, 1 ignored (the)`.
    pub fn render_human(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for t in &self.matched_terms {
            let fields: Vec<&str> = FIELDS
                .iter()
                .zip(t.tf_per_field.iter())
                .filter(|(_, tf)| **tf > 0)
                .map(|(name, _)| *name)
                .collect();
            let total_tf: i64 = t.tf_per_field.iter().sum();
            parts.push(format!(
                "{}~\"{}\"(tf {})",
                fields.join("+"),
                t.term,
                total_tf
            ));
        }
        let mut line = format!("why: {}", parts.join(" "));
        line.push_str(&format!(" — {} matched", self.matched_terms.len()));
        if !self.ignored_terms.is_empty() {
            let names: Vec<&str> = self.ignored_terms.iter().map(|i| i.term.as_str()).collect();
            line.push_str(&format!(
                ", {} ignored ({})",
                self.ignored_terms.len(),
                names.join(", ")
            ));
        }
        line
    }

    /// Robot rendering: the full structure, stable schema, fixed key
    /// order, byte-stable via the JSON module's ordered builders.
    pub fn to_json(&self) -> Value {
        let matched: Vec<Value> = self
            .matched_terms
            .iter()
            .map(|t| {
                let fields_hit: Vec<Value> = FIELDS
                    .iter()
                    .zip(t.tf_per_field.iter())
                    .filter(|(_, tf)| **tf > 0)
                    .map(|(name, _)| Value::string(*name))
                    .collect();
                let tf_obj: Vec<(String, Value)> = FIELDS
                    .iter()
                    .zip(t.tf_per_field.iter())
                    .map(|(name, &tf)| ((*name).to_string(), Value::int(tf)))
                    .collect();
                Value::Object(vec![
                    ("term".to_string(), Value::string(t.term.clone())),
                    ("fields_hit".to_string(), Value::Array(fields_hit)),
                    ("tf_per_field".to_string(), Value::Object(tf_obj)),
                    ("df".to_string(), Value::int(t.df)),
                    ("idf_micros".to_string(), Value::int(t.idf_micros)),
                    (
                        "contribution_micros".to_string(),
                        Value::int(t.contribution_micros),
                    ),
                ])
            })
            .collect();
        let ignored: Vec<Value> = self
            .ignored_terms
            .iter()
            .map(|i| {
                Value::Object(vec![
                    ("term".to_string(), Value::string(i.term.clone())),
                    ("reason".to_string(), Value::string(i.reason.as_str())),
                ])
            })
            .collect();
        let mut pairs = vec![
            ("score_micros".to_string(), Value::int(self.score_micros)),
            ("matched_terms".to_string(), Value::Array(matched)),
            ("ignored_terms".to_string(), Value::Array(ignored)),
        ];
        if let Some(r) = &self.rerank {
            pairs.push((
                "rerank".to_string(),
                Value::Object(vec![
                    ("bm25_micros".to_string(), Value::int(r.bm25_micros)),
                    (
                        "embed_sim_micros".to_string(),
                        Value::int(r.embed_sim_micros),
                    ),
                    ("alpha_micros".to_string(), Value::int(r.alpha_micros)),
                    ("beta_micros".to_string(), Value::int(r.beta_micros)),
                    (
                        "shared_subtokens".to_string(),
                        Value::Array(r.shared_subtokens.iter().map(Value::string).collect()),
                    ),
                ]),
            ));
        }
        Value::Object(pairs)
    }
}

/// Compute the query's ignored terms once: every unique query token that
/// is a stopword or absent from the corpus, in first-seen order.
pub fn ignored_terms(query: &str, index: &Index) -> Vec<IgnoredTerm> {
    let df = index.df();
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for tok in tokenize_all(query) {
        if seen.contains(&tok) {
            continue;
        }
        seen.push(tok.clone());
        if is_stopword(&tok) {
            out.push(IgnoredTerm {
                term: tok,
                reason: IgnoredReason::Stopword,
            });
        } else if !df.contains_key(&tok) {
            out.push(IgnoredTerm {
                term: tok,
                reason: IgnoredReason::NotInCorpus,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::bm25::bm25_scores;
    use crate::store::index::DocEntry;
    use crate::store::memory::MemoryType;
    use std::collections::BTreeMap;

    fn corpus() -> Index {
        let mut docs = BTreeMap::new();
        let mut tf = BTreeMap::new();
        tf.insert("sync".to_string(), [1i64, 0, 3]);
        tf.insert("branch".to_string(), [0i64, 0, 2]);
        docs.insert(
            "rule-sync-branch-1".to_string(),
            DocEntry {
                id: "rule-sync-branch-1".to_string(),
                path: "memories/rule-sync-branch-1.md".to_string(),
                mtype: MemoryType::Rule,
                title: "sync".to_string(),
                tags: vec![],
                created: 0,
                content_hash: "0".repeat(16),
                tf,
                field_len: [1, 0, 5],
            },
        );
        Index { docs }
    }

    fn explain_query(q: &str) -> Explanation {
        let idx = corpus();
        let toks = crate::recall::tokenize::tokenize(q);
        let hits = bm25_scores(&toks, &idx);
        assert_eq!(hits.len(), 1);
        Explanation::for_hit(&hits[0], &ignored_terms(q, &idx))
    }

    #[test]
    fn contribution_sum_invariant_holds() {
        let e = explain_query("the sync branch");
        assert!(e.contributions_sum_exactly());
        assert_eq!(
            e.matched_terms
                .iter()
                .map(|t| t.contribution_micros)
                .sum::<i64>(),
            e.score_micros
        );
    }

    #[test]
    fn ignored_terms_cover_stopwords_and_absentees() {
        let e = explain_query("the sync branch unicorns");
        assert_eq!(
            e.ignored_terms,
            vec![
                IgnoredTerm {
                    term: "the".to_string(),
                    reason: IgnoredReason::Stopword
                },
                IgnoredTerm {
                    term: "unicorns".to_string(),
                    reason: IgnoredReason::NotInCorpus
                },
            ]
        );
        // Disjoint from matched terms, by construction.
        for i in &e.ignored_terms {
            assert!(e.matched_terms.iter().all(|m| m.term != i.term));
        }
    }

    #[test]
    fn ordering_is_deterministic_and_total() {
        let a = explain_query("sync branch");
        let b = explain_query("sync branch");
        assert_eq!(a, b, "double run identical");
        // Contribution desc then term asc.
        let contribs: Vec<i64> = a
            .matched_terms
            .iter()
            .map(|t| t.contribution_micros)
            .collect();
        let mut sorted = contribs.clone();
        sorted.sort_unstable_by(|x, y| y.cmp(x));
        assert_eq!(contribs, sorted);
    }

    #[test]
    fn human_rendering_snapshot() {
        let e = explain_query("the sync branch");
        assert_eq!(
            e.render_human(),
            "why: title+body~\"sync\"(tf 4) body~\"branch\"(tf 2) — 2 matched, 1 ignored (the)"
        );
        // Zero ignored renders without the ignored clause.
        let e2 = explain_query("sync branch");
        assert!(
            !e2.render_human().contains("ignored"),
            "{}",
            e2.render_human()
        );
    }

    #[test]
    fn json_golden_for_one_explanation() {
        let e = explain_query("the sync");
        let json = e.to_json().emit();
        // Full golden: stable schema is a compatibility promise.
        let want = format!(
            "{{\"score_micros\":{},\"matched_terms\":[{{\"term\":\"sync\",\"fields_hit\":[\"title\",\"body\"],\"tf_per_field\":{{\"title\":1,\"tags\":0,\"body\":3}},\"df\":1,\"idf_micros\":{},\"contribution_micros\":{}}}],\"ignored_terms\":[{{\"term\":\"the\",\"reason\":\"stopword\"}}]}}",
            e.score_micros, e.matched_terms[0].idf_micros, e.matched_terms[0].contribution_micros
        );
        assert_eq!(json, want);
        assert_eq!(json, e.to_json().emit(), "byte-stable");
    }
}
