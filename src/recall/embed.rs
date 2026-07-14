//! recall::embed — deterministic hashed subword embeddings for a
//! semantic-ish rerank. No model, std-only, integer math (floats are banned
//! in recall and the gate greps for them).
//!
//! Each token is expanded into character n-grams bounded with `<` `>` word
//! markers (fastText style), and every gram is hashed into a fixed number of
//! buckets. A document or query vector is the tf-weighted bucket-count sum
//! over its tokens' grams. Similarity is fixed-point cosine.
//!
//! Why this fixes near-miss recall: `sovereign` and `sovereignty` share almost
//! all of their character 3-to-5-grams (`sov`, `ove`, `ver`, `eig`, `ign`,
//! ...), so their vectors are close even though they share no whole token.
//! Concept queries reach documents that phrase the idea differently. All
//! without stemming, a lexicon, or a model, so the shipped binary stays
//! offline and byte-stable.

use crate::util::fnv1a64;
use std::collections::BTreeMap;

/// Embedding dimension (hash buckets). A personal store is small, so 1024
/// balances collision rate against per-query cost.
const DIM: u64 = 1024;
/// Character n-gram sizes (inclusive).
const NGRAM_MIN: usize = 3;
const NGRAM_MAX: usize = 5;
/// Similarity scale (micro-units), matching the rest of recall.
pub const SCALE: i64 = 1_000_000;

/// A sparse embedding: bucket -> weight. `BTreeMap` keeps iteration (and thus
/// every derived number) deterministic.
pub type Embedding = BTreeMap<u32, i64>;

/// Accumulate one token's bounded char n-grams (plus the whole token as a
/// unigram feature, which rewards exact matches) into `vec`, each `weight`
/// times so term frequency carries through.
fn add_token(vec: &mut Embedding, token: &str, weight: i64) {
    if token.is_empty() || weight <= 0 {
        return;
    }
    let bounded: Vec<char> = std::iter::once('<')
        .chain(token.chars())
        .chain(std::iter::once('>'))
        .collect();
    let len = bounded.len();
    let mut bump = |bytes: &[u8]| {
        let bucket = (fnv1a64(bytes) % DIM) as u32;
        *vec.entry(bucket).or_insert(0) += weight;
    };
    for n in NGRAM_MIN..=NGRAM_MAX {
        if n > len {
            break;
        }
        for start in 0..=(len - n) {
            let gram: String = bounded[start..start + n].iter().collect();
            bump(gram.as_bytes());
        }
    }
    // The whole token as its own feature (a distinct hash space via a prefix).
    let mut whole = String::from("=");
    whole.push_str(token);
    bump(whole.as_bytes());
}

/// Build an embedding from `(token, weight)` pairs (weight = term frequency).
pub fn embed<'a>(tokens: impl IntoIterator<Item = (&'a str, i64)>) -> Embedding {
    let mut v = Embedding::new();
    for (tok, w) in tokens {
        add_token(&mut v, tok, w);
    }
    v
}

/// Build an embedding from a slice of tokens, each counted once.
pub fn embed_terms(tokens: &[String]) -> Embedding {
    let mut v = Embedding::new();
    for t in tokens {
        add_token(&mut v, t, 1);
    }
    v
}

/// Cosine similarity in micro-units, in `[0, SCALE]`. All-integer: the square
/// is computed exactly in `i128`, then an integer square root brings it back.
/// `cos * SCALE = isqrt( dot^2 * SCALE^2 / (|a|^2 * |b|^2) )`.
pub fn cosine_micros(a: &Embedding, b: &Embedding) -> i64 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let (small, big) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut dot: i128 = 0;
    for (k, &va) in small {
        if let Some(&vb) = big.get(k) {
            dot += (va as i128) * (vb as i128);
        }
    }
    if dot <= 0 {
        return 0;
    }
    let norm_sq = |e: &Embedding| -> i128 { e.values().map(|&x| (x as i128) * (x as i128)).sum() };
    let den = norm_sq(a).saturating_mul(norm_sq(b));
    if den <= 0 {
        return 0;
    }
    let scale = SCALE as i128;
    let num = dot
        .saturating_mul(dot)
        .saturating_mul(scale)
        .saturating_mul(scale);
    let cos = isqrt_i128(num / den) as i64;
    cos.min(SCALE)
}

/// Deterministic integer square root of a non-negative `i128` (Newton).
fn isqrt_i128(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(word: &str) -> Embedding {
        embed_terms(&[word.to_string()])
    }

    #[test]
    fn near_miss_wording_is_close() {
        // The headline case: no shared whole token, but shared subword grams.
        let a = tok("sovereign");
        let b = tok("sovereignty");
        let sim = cosine_micros(&a, &b);
        assert!(
            sim > 550_000,
            "sovereign~sovereignty should be highly similar, got {sim}"
        );
    }

    #[test]
    fn identical_is_full_similarity() {
        let a = tok("determinism");
        assert_eq!(cosine_micros(&a, &a), SCALE, "cos(x,x) == 1.0");
    }

    #[test]
    fn unrelated_words_are_far() {
        let a = tok("sovereignty");
        let b = tok("kubernetes");
        let sim = cosine_micros(&a, &b);
        assert!(sim < 200_000, "unrelated words should be far, got {sim}");
    }

    #[test]
    fn morphological_family_clusters() {
        // "migrate" is closer to "migration" than to "banana".
        let base = tok("migrate");
        let kin = cosine_micros(&base, &tok("migration"));
        let stranger = cosine_micros(&base, &tok("banana"));
        assert!(
            kin > stranger,
            "migrate~migration {kin} > migrate~banana {stranger}"
        );
    }

    #[test]
    fn empty_and_determinism() {
        let empty = Embedding::new();
        assert_eq!(cosine_micros(&empty, &tok("x")), 0);
        let a = embed([("sync", 2), ("branch", 1)]);
        let b = embed([("sync", 2), ("branch", 1)]);
        assert_eq!(a, b, "same inputs -> byte-identical embedding");
        assert_eq!(cosine_micros(&a, &b), SCALE);
    }

    #[test]
    fn isqrt_is_exact_on_squares() {
        for k in [0i128, 1, 4, 9, 1_000_000, 1_000_000_000_000] {
            assert_eq!(isqrt_i128(k * k), k);
        }
        assert_eq!(isqrt_i128(8), 2); // floor
    }
}
