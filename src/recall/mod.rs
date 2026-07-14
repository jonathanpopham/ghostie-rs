//! recall — deterministic retrieval with why-explanations.
//!
//! Single responsibility: given a task or query, surface the relevant
//! memories. Clean-room BM25 in fixed-point i64 (floats are banned here; the
//! gate greps for them), one shared tokenizer for index and query time, and
//! a per-hit explanation whose term contributions sum exactly to the score.
//!
//! Pipeline: tokenize query -> corpus stats (index, hash-verified fresh;
//! a deleted or unusable index is rebuilt from the files, so results are
//! IDENTICAL with or without it) -> BM25 -> seed Personalized PageRank with
//! those hits and walk the `links` graph, so memories linked to a hit surface
//! by association (each naming the edge that carried it) -> type/tag/scope
//! filters (BEFORE truncation, so filters don't starve results) -> total
//! order (score desc, id asc) -> pack under the token budget -> top-k.

pub mod bm25;
pub mod embed;
pub mod explain;
pub mod ppr;
pub mod tokenize;

use crate::error::{Result, Warning};
use crate::json::Value;
use crate::recall::bm25::bm25_scores;
use crate::recall::explain::{Explanation, RerankExplanation, ignored_terms};
use crate::store::Store;
use crate::store::index::Index;
use crate::store::memory::MemoryType;
use crate::util::format_rfc3339_utc;
use std::collections::{BTreeMap, BTreeSet};

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
    /// Only memories in this scope (`global` or `project:<name>`). A memory
    /// with no scope is treated as `global` and always eligible.
    pub scope: Option<String>,
    /// Approximate token budget for the whole result: recall packs the
    /// highest-value cards greedily until the budget is spent, so the caller
    /// (a hook injecting context) never floods. `None` = only `k` bounds it.
    pub budget_tokens: Option<usize>,
    /// Diversify with Maximal Marginal Relevance: trade a little relevance for
    /// novelty so recall does not return several near-duplicate memories.
    /// Off by default (pure relevance order); most useful with `budget_tokens`
    /// when a hook injects a handful of cards and each one should earn its slot.
    pub diversify: bool,
    /// Blend a hashed-subword embedding cosine into the ranking: boosts hits
    /// that are semantically close and surfaces near-miss / concept matches
    /// that share no whole token (`sovereign` -> `sovereignty`). On by default;
    /// the eval harness flips it off to measure the lift.
    pub rerank: bool,
    /// Blend a confidence-decay prior into the ranking: memories untouched for a
    /// long time sink a little, so a growing store does not flood recall with
    /// stale cruft. OFF by default so the default output is byte-identical to a
    /// no-decay build (the gate's goldens rely on that); `--decay` turns it on.
    pub decay: bool,
    /// Max score demotion (micro-units) a fully-decayed memory receives when
    /// `decay` is on. Small by design: decay reorders near-ties, never
    /// overpowers BM25 / embedding relevance.
    pub decay_weight_micros: i64,
    /// Confidence half-life in seconds for the decay prior.
    pub half_life_secs: i64,
    /// Query-time instant (epoch seconds) decay is measured against, taken from
    /// the injected clock so recall stays deterministic under
    /// `GHOSTIE_TEST_CLOCK`. `None` (the default) disables decay regardless of
    /// the `decay` flag, so recall never depends on an ambient wall clock.
    pub now_epoch: Option<i64>,
}

impl Default for RecallOpts {
    fn default() -> Self {
        RecallOpts {
            k: 10,
            mtype: None,
            tag: None,
            min_score_micros: 0,
            scope: None,
            budget_tokens: None,
            diversify: false,
            rerank: true,
            decay: false,
            decay_weight_micros: crate::decay::DEFAULT_DECAY_WEIGHT_MICROS,
            half_life_secs: crate::decay::DEFAULT_HALF_LIFE_SECS,
            now_epoch: None,
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
    /// The blended ranking score, micro-units: BM25 for a lexical hit, the
    /// Personalized-PageRank mass for a graph-reached hit.
    pub score_micros: i64,
    /// Graph-reach mass (micro-units), > 0 only when the memory surfaced via
    /// the link graph rather than a lexical match.
    pub graph_micros: i64,
    /// For a graph-reached hit, the seed memory on the other end of the edge:
    /// "reached via link from `<graph_via>`". `None` for lexical hits.
    pub graph_via: Option<String>,
    /// Provenance: the why-line, surfaced on the card without the body.
    pub rationale: Option<String>,
    /// Provenance: harness that created it (the WHERE, cross-provider).
    pub harness: Option<String>,
    /// Provenance: model/core that produced it (the WHICH, cross-provider).
    pub core: Option<String>,
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
    /// Robot rendering, fixed key order, byte-stable. Provenance and
    /// graph-reach keys are always present (null/0 when absent) so the schema
    /// is stable for the hooks that consume it.
    pub fn to_json(&self) -> Value {
        let opt = |s: &Option<String>| match s {
            Some(v) => Value::string(v.clone()),
            None => Value::Null,
        };
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
            ("graph_micros".to_string(), Value::int(self.graph_micros)),
            ("graph_via".to_string(), opt(&self.graph_via)),
            ("rationale".to_string(), opt(&self.rationale)),
            ("harness".to_string(), opt(&self.harness)),
            ("core".to_string(), opt(&self.core)),
            ("why".to_string(), self.explanation.to_json()),
        ])
    }

    /// The compact one-line "why is this here" for human mode: the lexical
    /// why for a match, or the edge that carried a graph-reached hit.
    pub fn why_line(&self) -> String {
        if let Some(via) = &self.graph_via {
            return format!(
                "why: reached via link from {} (graph {})",
                via,
                render_micros(self.graph_micros)
            );
        }
        // Semantic-only hit: no lexical terms, reached by embedding similarity.
        if self.explanation.matched_terms.is_empty()
            && let Some(r) = &self.explanation.rerank
            && r.embed_sim_micros > 0
        {
            let via = if r.shared_subtokens.is_empty() {
                String::new()
            } else {
                format!(" via {}", r.shared_subtokens.join(", "))
            };
            return format!(
                "why: semantically similar (embed {}){}",
                render_micros(r.embed_sim_micros),
                via
            );
        }
        self.explanation.render_human()
    }

    /// The provenance tag for the card, e.g. ` [hermes/hermes-4-405b]`, or
    /// empty when the memory carries no provenance.
    pub fn provenance_tag(&self) -> String {
        match (&self.harness, &self.core) {
            (Some(h), Some(c)) => format!(" [{h}/{c}]"),
            (Some(h), None) => format!(" [{h}]"),
            (None, Some(c)) => format!(" [{c}]"),
            (None, None) => String::new(),
        }
    }
}

/// Render micro-units as a fixed 2-decimal string without floats (the gate
/// bans floats in recall): `240000` -> `0.24`.
fn render_micros(micros: i64) -> String {
    let whole = micros / ppr::SCALE;
    let frac = (micros % ppr::SCALE).abs() / (ppr::SCALE / 100); // hundredths
    format!("{whole}.{frac:02}")
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

/// Estimate the context cost of one card in tokens (~4 chars/token), so the
/// budget packer can bound how much recall injects. Title plus the surfaced
/// why-line (rationale), plus a small fixed overhead for the id/score line.
fn card_token_cost(hit: &RecallHit) -> usize {
    let chars = hit.title.chars().count()
        + hit.rationale.as_deref().map(str::len).unwrap_or(0)
        + hit.id.len();
    chars / 4 + 8
}

/// Integer Jaccard similarity between two memories' indexed term sets, in
/// micro-units: `|A ∩ B| * SCALE / |A ∪ B|`, or 0 when both are empty. Floats
/// are banned in recall, so this is the deterministic novelty measure MMR uses.
fn jaccard_micros(a: &BTreeSet<&str>, b: &BTreeSet<&str>) -> i64 {
    if a.is_empty() && b.is_empty() {
        return 0;
    }
    let inter = a.intersection(b).count() as i64;
    let union = a.union(b).count() as i64;
    if union == 0 {
        0
    } else {
        inter.saturating_mul(ppr::SCALE) / union
    }
}

/// Reorder hits for diversity (Maximal Marginal Relevance): greedily choose the
/// hit that best trades relevance against novelty relative to what is already
/// chosen, so recall does not surface several near-duplicate memories. Ties
/// resolve to the earlier (higher-ranked) hit, so the order stays total and
/// deterministic. Relevance is each hit's score normalised against the top hit;
/// novelty is `1 - max Jaccard` to the already-selected set.
fn mmr_reorder(hits: Vec<RecallHit>, index: &Index) -> Vec<RecallHit> {
    if hits.len() <= 2 {
        return hits;
    }
    // λ = 1/2: relevance and novelty weighted equally, so an exact duplicate
    // of an already-chosen hit (redundancy 1) nets zero and loses its slot to
    // any distinct memory that is still relevant. This is what "do not return
    // several near-duplicates" requires; pure-relevance order stays the
    // default (this pass only runs under --diverse).
    const LAMBDA_NUM: i64 = 1;
    const LAMBDA_DEN: i64 = 2;
    let sets: Vec<BTreeSet<&str>> = hits
        .iter()
        .map(|h| {
            index
                .docs
                .get(&h.id)
                .map(|e| e.tf.keys().map(String::as_str).collect())
                .unwrap_or_default()
        })
        .collect();
    let max_rel = hits
        .iter()
        .map(|h| h.score_micros)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut remaining: Vec<usize> = (0..hits.len()).collect();
    let mut order: Vec<usize> = Vec::new();
    while !remaining.is_empty() {
        let mut best_rp = 0usize;
        let mut best_score = i64::MIN;
        for (rp, &i) in remaining.iter().enumerate() {
            let rel = hits[i].score_micros.saturating_mul(ppr::SCALE) / max_rel;
            let mut maxsim = 0i64;
            for &j in &order {
                let s = jaccard_micros(&sets[i], &sets[j]);
                if s > maxsim {
                    maxsim = s;
                }
            }
            // MMR = λ·relevance − (1−λ)·redundancy.
            let mmr = (LAMBDA_NUM * rel - (LAMBDA_DEN - LAMBDA_NUM) * maxsim) / LAMBDA_DEN;
            if mmr > best_score {
                best_score = mmr;
                best_rp = rp;
            }
        }
        order.push(remaining.remove(best_rp));
    }
    order.into_iter().map(|i| hits[i].clone()).collect()
}

/// Embedding boost weight on an existing hit: `score += cosine * NUM/DEN`.
/// Half-weight keeps a strong BM25 hit on top while letting the embedding
/// reorder near-ties and lift semantically-close memories.
const BETA_NUM: i64 = 1;
const BETA_DEN: i64 = 2;
/// A memory reached ONLY by embedding similarity must clear this cosine floor
/// (0.35) to enter the results, so faint semantic echoes do not flood.
const SEMANTIC_FLOOR: i64 = 350_000;

/// Assemble the rerank breakdown for the explanation schema.
fn rerank_expl(bm25_micros: i64, embed_sim_micros: i64, shared: Vec<String>) -> RerankExplanation {
    RerankExplanation {
        bm25_micros,
        embed_sim_micros,
        alpha_micros: embed::SCALE, // BM25 kept at full weight
        beta_micros: embed::SCALE * BETA_NUM / BETA_DEN,
        shared_subtokens: shared,
    }
}

/// The query terms that overlap this memory's terms as subwords (a query term
/// that contains, or is contained by, a doc term, both length >= 3), up to
/// three: the words that carried the semantic match, for the explanation.
fn shared_subtokens(query_terms: &[String], entry: &crate::store::index::DocEntry) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for qt in query_terms {
        if qt.len() < 3 {
            continue;
        }
        let overlaps = entry
            .tf
            .keys()
            .any(|dt| dt.len() >= 3 && (dt.contains(qt.as_str()) || qt.contains(dt.as_str())));
        if overlaps && !out.contains(qt) {
            out.push(qt.clone());
            if out.len() == 3 {
                break;
            }
        }
    }
    out
}

/// Does this memory pass the mtype/tag/scope filters? Applied while
/// assembling, so filters never starve a later result. A memory with no
/// scope is `global` and passes any scope filter.
fn passes_filters(entry: &crate::store::index::DocEntry, opts: &RecallOpts) -> bool {
    if let Some(t) = opts.mtype
        && entry.mtype != t
    {
        return false;
    }
    if let Some(tag) = &opts.tag
        && !entry.tags.iter().any(|x| x == tag)
    {
        return false;
    }
    if let Some(scope) = &opts.scope {
        // A scope filter admits memories in that scope PLUS globally-scoped
        // ones (no scope, or explicit `global`), so project isolation never
        // hides the rules you want everywhere. A memory scoped to a DIFFERENT
        // project is excluded, which is what stops cross-project leakage.
        let entry_scope = entry.scope.as_deref().unwrap_or("global");
        if entry_scope != scope && entry_scope != "global" {
            return false;
        }
    }
    true
}

/// The library-level entry point the CLI fronts.
///
/// Pipeline: BM25 lexical scores -> seed Personalized PageRank with them ->
/// surface both the lexical hits and their linked neighbours (reached "by
/// association", each naming the edge that carried it) -> filter -> rank in
/// a total order (score desc, id asc) -> pack under the token budget -> k.
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
    let scored = bm25_scores(&query_terms, &index);
    let ignored = ignored_terms(query, &index);

    // Seed PPR with the lexical hits (id -> BM25 micros), then walk the links.
    let mut seeds: BTreeMap<String, i64> = BTreeMap::new();
    for s in &scored {
        if s.score_micros > 0 {
            seeds.insert(s.id.clone(), s.score_micros);
        }
    }
    let ppr_mass = ppr::personalized_pagerank(&index, &seeds);
    let direct: BTreeSet<&str> = scored.iter().map(|s| s.id.as_str()).collect();

    let mut hits: Vec<RecallHit> = Vec::new();

    // 1. Lexical hits, scored and explained by BM25 exactly as before.
    for s in &scored {
        let Some(entry) = index.docs.get(&s.id) else {
            continue; // unreachable: scorer only sees indexed docs
        };
        if !passes_filters(entry, opts) {
            continue;
        }
        if opts.min_score_micros > 0 && s.score_micros < opts.min_score_micros {
            continue;
        }
        hits.push(RecallHit {
            id: s.id.clone(),
            mtype: entry.mtype,
            title: entry.title.clone(),
            created: entry.created,
            path: entry.path.clone(),
            score_micros: s.score_micros,
            graph_micros: 0,
            graph_via: None,
            rationale: entry.rationale.clone(),
            harness: entry.harness.clone(),
            core: entry.core.clone(),
            explanation: Explanation::for_hit(s, &ignored),
        });
    }

    // 2. Graph-reached hits: memories linked to a seed that did not match
    //    lexically. Bounded to those with a direct seed neighbour (1-hop, so
    //    the "reached via link from X" edge is real and nameable) and above
    //    the mass floor (so faint echoes do not flood).
    for (id, mass) in &ppr_mass {
        if direct.contains(id.as_str()) || *mass < ppr::GRAPH_FLOOR {
            continue;
        }
        let Some(entry) = index.docs.get(id) else {
            continue;
        };
        if !passes_filters(entry, opts) {
            continue;
        }
        if opts.min_score_micros > 0 && *mass < opts.min_score_micros {
            continue;
        }
        let Some(via) = ppr::strongest_seed_neighbor(&index, id, &seeds) else {
            continue;
        };
        hits.push(RecallHit {
            id: id.clone(),
            mtype: entry.mtype,
            title: entry.title.clone(),
            created: entry.created,
            path: entry.path.clone(),
            score_micros: *mass,
            graph_micros: *mass,
            graph_via: Some(via),
            rationale: entry.rationale.clone(),
            harness: entry.harness.clone(),
            core: entry.core.clone(),
            explanation: Explanation::graph_reached(&ignored),
        });
    }

    // 3. Semantic rerank via hashed subword embeddings (deterministic, no
    //    model). A memory's semantic score is the average, over query tokens,
    //    of the BEST cosine to any single memory term, so a near-miss word
    //    (`sovereign` -> `sovereignty`) matches regardless of memory length.
    //    Boosts existing hits and pulls in memories reached by meaning alone.
    if opts.rerank && !query_terms.is_empty() {
        // Embed each query token once (query terms are ephemeral, not corpus
        // members, so they are not cached).
        let q_embeds: Vec<embed::Embedding> = query_terms
            .iter()
            .map(|t| embed::embed_terms(std::slice::from_ref(t)))
            .collect();
        // Per-term memory embeddings come straight from the index cache
        // (computed once at index-build time). An embedding is a pure function
        // of its term, so this is byte-identical to recomputing here; the cache
        // holds one entry per indexed term, so every `tf` key resolves.
        let term_embed = &index.term_embed;
        // Max-sim semantic score per memory, computed once.
        let mut sim: BTreeMap<&str, i64> = BTreeMap::new();
        for (id, entry) in &index.docs {
            let mut total: i64 = 0;
            for qe in &q_embeds {
                let best = entry
                    .tf
                    .keys()
                    .filter_map(|t| term_embed.get(t.as_str()))
                    .map(|te| embed::cosine_micros(qe, te))
                    .max()
                    .unwrap_or(0);
                total += best;
            }
            let c = total / q_embeds.len() as i64;
            if c > 0 {
                sim.insert(id.as_str(), c);
            }
        }
        let present: BTreeSet<String> = hits.iter().map(|h| h.id.clone()).collect();
        // Boost the hits already found, and record the blend breakdown.
        for h in &mut hits {
            let c = sim.get(h.id.as_str()).copied().unwrap_or(0);
            let bm25 = if h.graph_via.is_some() {
                0
            } else {
                h.score_micros
            };
            h.score_micros = h.score_micros.saturating_add(c * BETA_NUM / BETA_DEN);
            let shared = index
                .docs
                .get(&h.id)
                .map(|e| shared_subtokens(&query_terms, e))
                .unwrap_or_default();
            h.explanation.rerank = Some(rerank_expl(bm25, c, shared));
        }
        // New semantic candidates: reached by meaning alone, above the floor.
        for (id, &c) in &sim {
            if present.contains(*id) || c < SEMANTIC_FLOOR {
                continue;
            }
            let entry = &index.docs[*id];
            if !passes_filters(entry, opts) {
                continue;
            }
            if opts.min_score_micros > 0 && c < opts.min_score_micros {
                continue;
            }
            let mut explanation = Explanation::graph_reached(&ignored);
            explanation.rerank = Some(rerank_expl(0, c, shared_subtokens(&query_terms, entry)));
            hits.push(RecallHit {
                id: id.to_string(),
                mtype: entry.mtype,
                title: entry.title.clone(),
                created: entry.created,
                path: entry.path.clone(),
                score_micros: c,
                graph_micros: 0,
                graph_via: None,
                rationale: entry.rationale.clone(),
                harness: entry.harness.clone(),
                core: entry.core.clone(),
                explanation,
            });
        }
    }

    // Confidence-decay prior (opt-in). A memory untouched for a long time is
    // demoted a little; a fresh one is untouched. Deterministic: the reference
    // clock is injected (`now_epoch`), the math is pure integer (`decay`), and
    // when the flag is off (the default) NOTHING here runs, so the default
    // output is byte-identical to a build without decay. The lifecycle fields
    // are not in the derivable index, so we read them from the files — only on
    // the handful of already-found hits, and only under `--decay`.
    if opts.decay
        && opts.decay_weight_micros > 0
        && let Some(now) = opts.now_epoch
    {
        for h in &mut hits {
            let (base, reference) = match store.read(&h.id) {
                Ok((m, _)) => (
                    m.confidence.unwrap_or(crate::decay::FULL_CONFIDENCE_MICROS),
                    m.last_used.unwrap_or(m.created),
                ),
                Err(_) => (crate::decay::FULL_CONFIDENCE_MICROS, h.created),
            };
            let decayed =
                crate::decay::decayed_confidence_micros(base, reference, now, opts.half_life_secs);
            let deficit = crate::decay::FULL_CONFIDENCE_MICROS - decayed; // 0..=FULL
            let penalty = opts.decay_weight_micros.saturating_mul(deficit)
                / crate::decay::FULL_CONFIDENCE_MICROS;
            h.score_micros = h.score_micros.saturating_sub(penalty);
        }
    }

    // Total order: score desc, id asc (stable, deterministic).
    hits.sort_by(|a, b| {
        b.score_micros
            .cmp(&a.score_micros)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Optional diversity pass: demote near-duplicates before packing.
    if opts.diversify {
        hits = mmr_reorder(hits, &index);
    }

    // Pack under k and (optionally) the token budget.
    let mut out = Vec::new();
    let mut spent = 0usize;
    for hit in hits {
        if out.len() == opts.k {
            break;
        }
        if let Some(budget) = opts.budget_tokens {
            let cost = card_token_cost(&hit);
            // Always admit the first card even if it alone exceeds budget, so
            // a tight budget still answers rather than returning nothing.
            if !out.is_empty() && spent + cost > budget {
                continue;
            }
            spent += cost;
        }
        out.push(hit);
    }
    Ok(RecallResult {
        hits: out,
        warnings,
    })
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
    fn cached_and_recomputed_rerank_are_byte_equal() {
        // The semantic rerank reads per-term embeddings from the index cache.
        // A first recall materializes the cache on disk; deleting the index
        // forces the from-scratch recompute path. Because an embedding is a
        // pure function of its term, both must produce byte-identical output —
        // this is the guard that the cache never diverges from recompute.
        let tmp = TempDir::new("recall-embcache-eq");
        let store = Store::open(tmp.path());
        seed(&store);
        // A near-miss query that leans on the rerank (no whole-token match).
        let q = "determinismic floating point";
        let cached = recall(&store, q, &RecallOpts::default()).unwrap();
        // The cache is on disk now; drop it and rebuild from files.
        std::fs::remove_dir_all(store.root().join(".index")).unwrap();
        let recomputed = recall(&store, q, &RecallOpts::default()).unwrap();
        assert_eq!(
            cached.to_json().emit(),
            recomputed.to_json().emit(),
            "cached term embeddings must match the recompute path byte-for-byte"
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
    fn diversify_demotes_a_near_duplicate() {
        let tmp = TempDir::new("recall-mmr");
        let store = Store::open(tmp.path());
        let clock = FixedClock(T0);
        let mk = |title: &str, body: &str| {
            store
                .create(
                    &NewMemory {
                        mtype: Some(MemoryType::Rule),
                        title: title.to_string(),
                        body: format!("{body}\n"),
                        ..NewMemory::default()
                    },
                    &clock,
                )
                .unwrap()
                .id
        };
        // a and b are content-identical (same term set): a perfect duplicate
        // pair. c is distinct but still matches the query.
        let a = mk("sync branch primary", "sync branch details here");
        let b = mk("sync branch primary", "sync branch details here");
        let c = mk(
            "sync over git remote",
            "sync branch travels over your git remote",
        );
        let q = "sync branch";

        // Pure relevance keeps the identical twins adjacent at the top.
        let plain = recall(&store, q, &RecallOpts::default()).unwrap();
        assert_eq!(plain.hits.len(), 3);
        assert_eq!(plain.hits[0].id, a);
        assert_eq!(
            plain.hits[1].id, b,
            "plain order buries the distinct memory"
        );

        // Diversity pushes the exact duplicate out of rank 2 for the novel one.
        let diverse = recall(
            &store,
            q,
            &RecallOpts {
                diversify: true,
                ..RecallOpts::default()
            },
        )
        .unwrap();
        assert_eq!(diverse.hits[0].id, a, "top hit unchanged");
        assert_ne!(diverse.hits[1].id, b, "MMR demotes the near-duplicate");
        assert_eq!(diverse.hits[1].id, c, "the distinct memory takes rank 2");
        assert!(
            diverse.hits.iter().any(|h| h.id == b),
            "the duplicate is demoted, not dropped"
        );
    }

    #[test]
    fn decay_is_off_by_default_and_selective_when_on() {
        let tmp = TempDir::new("recall-decay");
        let store = Store::open(tmp.path());
        seed(&store); // four memories, all created at T0
        let q = "sync floats configs";
        let base = recall(&store, q, &RecallOpts::default()).unwrap();
        assert!(base.hits.len() >= 2);
        let stale_id = base.hits[0].id.clone();
        let fresh_id = base.hits[1].id.clone();
        let base_stale = base.hits[0].score_micros;
        let base_fresh = base.hits[1].score_micros;

        // "now" is a year past creation: well over a 90-day half-life.
        let now = T0 + 365 * 24 * 3600;
        // Revalidate the second hit so it is fresh AS OF `now`.
        store.mark_used(&fresh_id, &FixedClock(now)).unwrap();

        // Default recall is byte-identical whether or not the decay code exists:
        // the decay branch is gated off, so re-running proves no leak.
        let base_again = recall(&store, q, &RecallOpts::default()).unwrap();
        let re_fresh = base_again
            .hits
            .iter()
            .find(|h| h.id == fresh_id)
            .unwrap()
            .score_micros;

        let opts = RecallOpts {
            decay: true,
            now_epoch: Some(now),
            ..RecallOpts::default()
        };
        let decayed = recall(&store, q, &opts).unwrap();
        let d_stale = decayed
            .hits
            .iter()
            .find(|h| h.id == stale_id)
            .unwrap()
            .score_micros;
        let d_fresh = decayed
            .hits
            .iter()
            .find(|h| h.id == fresh_id)
            .unwrap()
            .score_micros;

        assert!(
            d_stale < base_stale,
            "a stale memory is demoted under --decay ({d_stale} < {base_stale})"
        );
        assert_eq!(
            d_fresh, re_fresh,
            "a just-revalidated memory keeps full confidence: no penalty"
        );
        let _ = base_fresh;
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
