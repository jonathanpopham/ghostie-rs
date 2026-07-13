//! Personalized PageRank (random-walk-with-restart) over the memory link
//! graph, in fixed-point i64 — floats are banned in recall, and the gate
//! greps for them. The seeds are the BM25 hits; the walk spreads their
//! relevance across `links` edges, so a memory linked to a hit surfaces even
//! when it shares no query terms. That is the deterministic, zero-dependency
//! substitute for semantic search: reach by association, explained by the
//! edge that carried it ("reached via link from X").
//!
//! Determinism: every step is integer arithmetic over `BTreeMap`s, so
//! iteration order is fixed and two runs are byte-identical.

use crate::store::index::Index;
use std::collections::BTreeMap;

/// Probability scale: masses are i64 micro-units, a full distribution sums
/// to roughly `SCALE`.
pub const SCALE: i64 = 1_000_000;
/// Damping numerator/denominator: the walk keeps `DAMP/DAMP_DEN` of the mass
/// each step and restarts with the complement. 1/2 converges fast and keeps
/// reach local (association, not the whole graph lighting up).
const DAMP: i64 = 1;
const DAMP_DEN: i64 = 2;
/// Power-iteration rounds. With d = 1/2 the mass past ~20 hops is negligible;
/// 30 is comfortably past convergence for any realistic store.
const ITERS: usize = 30;
/// A non-seed memory surfaces as a graph hit only when its stationary mass
/// clears this floor (`SCALE`/50 = 0.02), so faint multi-hop echoes do not
/// flood recall.
pub const GRAPH_FLOOR: i64 = SCALE / 50;

/// Undirected adjacency over ids that exist in the index (links pointing at
/// absent memories are dropped). A link is treated as an association edge in
/// both directions: if a decision links its rationale fact, a query hitting
/// either should be able to reach the other.
fn adjacency(index: &Index) -> BTreeMap<String, Vec<String>> {
    let mut adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in index.docs.keys() {
        adj.entry(id.clone()).or_default();
    }
    for (id, entry) in &index.docs {
        for l in &entry.links {
            if index.docs.contains_key(l) {
                adj.get_mut(id).expect("seeded above").push(l.clone());
                adj.get_mut(l).expect("both ids present").push(id.clone());
            }
        }
    }
    for neighbors in adj.values_mut() {
        neighbors.sort();
        neighbors.dedup();
    }
    adj
}

/// Run PPR seeded by `seeds` (id -> non-negative weight, e.g. BM25 micros).
/// Returns stationary mass per id in micro-units. Empty or all-zero seeds
/// yield an empty map (recall then behaves exactly as pure BM25).
pub fn personalized_pagerank(
    index: &Index,
    seeds: &BTreeMap<String, i64>,
) -> BTreeMap<String, i64> {
    let total: i64 = seeds.values().copied().filter(|w| *w > 0).sum();
    if total <= 0 || index.docs.is_empty() {
        return BTreeMap::new();
    }
    // Restart distribution p[i], proportional to seed weight, summing ~SCALE.
    let mut p: BTreeMap<String, i64> = BTreeMap::new();
    for (id, w) in seeds {
        if *w > 0 && index.docs.contains_key(id) {
            p.insert(id.clone(), w.saturating_mul(SCALE) / total);
        }
    }
    let adj = adjacency(index);
    let mut r = p.clone();
    for _ in 0..ITERS {
        let mut next: BTreeMap<String, i64> = BTreeMap::new();
        // Restart term: (1 - d) * p.
        for (id, pv) in &p {
            *next.entry(id.clone()).or_insert(0) += pv.saturating_mul(DAMP_DEN - DAMP) / DAMP_DEN;
        }
        // Walk term: d * sum over neighbors of r[j] / deg(j). Dangling nodes
        // (no neighbors) drop their walk mass — a small, bounded leak that
        // never affects relative ranking.
        for (j, rv) in &r {
            if *rv == 0 {
                continue;
            }
            let neighbors = &adj[j];
            let deg = neighbors.len() as i64;
            if deg == 0 {
                continue;
            }
            let share = (rv.saturating_mul(DAMP) / DAMP_DEN) / deg;
            if share == 0 {
                continue;
            }
            for nb in neighbors {
                *next.entry(nb.clone()).or_insert(0) += share;
            }
        }
        r = next;
    }
    r
}

/// Of a graph-reached memory's neighbors, the seed (a BM25 hit) with the
/// strongest lexical score — the edge to name in "reached via link from X".
/// `None` when the memory has no direct seed neighbor (reached multi-hop).
pub fn strongest_seed_neighbor(
    index: &Index,
    id: &str,
    seeds: &BTreeMap<String, i64>,
) -> Option<String> {
    let entry = index.docs.get(id)?;
    // Undirected: this memory's own links, plus memories that link to it.
    let mut neighbors: Vec<&String> = entry.links.iter().collect();
    for (other, e) in &index.docs {
        if e.links.iter().any(|l| l == id) {
            neighbors.push(other);
        }
    }
    neighbors
        .into_iter()
        .filter(|n| seeds.get(*n).copied().unwrap_or(0) > 0)
        // Highest BM25, then id asc for a total, stable order.
        .max_by(|a, b| {
            let wa = seeds[*a];
            let wb = seeds[*b];
            wa.cmp(&wb).then_with(|| b.cmp(a))
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::index::DocEntry;
    use crate::store::memory::MemoryType;

    fn node(id: &str, links: &[&str]) -> DocEntry {
        DocEntry {
            id: id.to_string(),
            path: format!("memories/{id}.md"),
            mtype: MemoryType::Fact,
            title: id.to_string(),
            tags: vec![],
            created: 0,
            links: links.iter().map(|s| s.to_string()).collect(),
            harness: None,
            core: None,
            rationale: None,
            scope: None,
            content_hash: "0".repeat(16),
            tf: BTreeMap::new(),
            field_len: [0, 0, 0],
        }
    }

    fn index_of(nodes: Vec<DocEntry>) -> Index {
        let mut docs = BTreeMap::new();
        for n in nodes {
            docs.insert(n.id.clone(), n);
        }
        Index { docs }
    }

    #[test]
    fn walk_reaches_a_linked_neighbor_of_a_seed() {
        // a (seed) -> b -> c. Only a matches lexically; b and c must light up.
        let index = index_of(vec![node("a", &["b"]), node("b", &["c"]), node("c", &[])]);
        let mut seeds = BTreeMap::new();
        seeds.insert("a".to_string(), 2_000_000);
        let r = personalized_pagerank(&index, &seeds);
        assert!(r["a"] > 0, "seed keeps mass");
        assert!(r["b"] > 0, "direct neighbor of the seed is reached");
        assert!(
            r["b"] > r["c"],
            "closer neighbor outranks the farther one: b={} c={}",
            r["b"],
            r["c"]
        );
    }

    #[test]
    fn no_edges_means_no_spread() {
        let index = index_of(vec![node("a", &[]), node("b", &[])]);
        let mut seeds = BTreeMap::new();
        seeds.insert("a".to_string(), 1_000_000);
        let r = personalized_pagerank(&index, &seeds);
        // b is never reached; it stays absent or zero.
        assert_eq!(r.get("b").copied().unwrap_or(0), 0);
        assert!(r["a"] > 0);
    }

    #[test]
    fn empty_seeds_is_empty_and_deterministic() {
        let index = index_of(vec![node("a", &["b"]), node("b", &[])]);
        let empty = BTreeMap::new();
        assert!(personalized_pagerank(&index, &empty).is_empty());
        let mut seeds = BTreeMap::new();
        seeds.insert("a".to_string(), 5);
        let one = personalized_pagerank(&index, &seeds);
        let two = personalized_pagerank(&index, &seeds);
        assert_eq!(one, two, "integer PPR is byte-identical across runs");
    }

    #[test]
    fn strongest_seed_neighbor_names_the_edge() {
        let index = index_of(vec![
            node("hit", &["neighbor"]),
            node("neighbor", &[]),
            node("other", &[]),
        ]);
        let mut seeds = BTreeMap::new();
        seeds.insert("hit".to_string(), 1_000_000);
        assert_eq!(
            strongest_seed_neighbor(&index, "neighbor", &seeds).as_deref(),
            Some("hit"),
            "the seed on the other end of the edge is named"
        );
        assert_eq!(
            strongest_seed_neighbor(&index, "other", &seeds),
            None,
            "a memory with no seed neighbor reports no edge"
        );
    }
}
