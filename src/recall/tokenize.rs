//! The single tokenizer shared by indexing and querying. One tokenizer,
//! used everywhere — if index-time and query-time tokenization ever
//! diverge, recall silently rots; this module owns preventing that.
//!
//! Behavior (pinned exactly; golden tests freeze it):
//! - Split on any char that is not alphanumeric or `_`.
//! - Code-aware subtoken expansion: camelCase, PascalCase, snake_case and
//!   digit boundaries yield BOTH the compound token and its parts —
//!   `parseHttpRequest` -> `[parsehttprequest, parse, http, request]` —
//!   so a query "http request" hits a memory naming the symbol.
//!   (kebab-case is already split by `-`.)
//! - Lowercasing: ASCII fast path; non-ASCII via `char::to_lowercase`
//!   (std, Unicode-aware). Unicode tables can shift between Rust releases;
//!   the golden tests fail visibly if they ever do.
//! - Stopwords: a fixed English list, embedded as a const sorted slice and
//!   binary-searched. Dropped by [`tokenize`]; [`tokenize_all`] keeps them
//!   (explanations show them as ignored).
//! - Integer-only; a pure function of its input.

/// Fixed English stopword list. MUST stay sorted (binary search; asserted
/// by a test).
pub const STOPWORDS: [&str; 54] = [
    "a", "about", "all", "an", "and", "any", "are", "as", "at", "be", "been", "but", "by", "can",
    "did", "do", "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "into", "is",
    "it", "its", "just", "no", "not", "of", "on", "or", "our", "so", "than", "that", "the",
    "their", "then", "there", "these", "they", "this", "to", "was", "we", "were", "what", "when",
    "with",
];

/// Is this (already-lowercased) term a stopword?
pub fn is_stopword(term: &str) -> bool {
    STOPWORDS.binary_search(&term).is_ok()
}

/// Tokenize, dropping stopwords. This is the scoring tokenizer.
pub fn tokenize(text: &str) -> Vec<String> {
    tokenize_all(text)
        .into_iter()
        .filter(|t| !is_stopword(t))
        .collect()
}

/// Tokenize, keeping stopwords (for "why did my word do nothing"
/// explanations). Document order, lowercase, subtoken-expanded.
pub fn tokenize_all(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.is_empty() {
            continue;
        }
        let compound = lowercase(raw);
        let parts = split_subtokens(raw);
        out.push(compound.clone());
        if parts.len() > 1 {
            for p in parts {
                let p = lowercase(&p);
                if p != compound {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn lowercase(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.chars().flat_map(|c| c.to_lowercase()).collect()
    }
}

/// Split one raw token at code boundaries: `_`, lower->Upper transitions,
/// Upper->Upper-lower transitions (`IRBuilder` -> `IR`, `Builder`), and
/// letter<->digit boundaries.
fn split_subtokens(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() {
            let prev = chars[i - 1];
            let boundary =
                // camelCase / snake -> next word starts.
                (prev.is_lowercase() && c.is_uppercase())
                // ABCWord: last upper of an acronym starts a new word.
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
                // letter <-> digit boundaries.
                || (prev.is_ascii_digit() != c.is_ascii_digit()
                    && (prev.is_ascii_digit() || c.is_ascii_digit()));
            if boundary {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopword_list_is_sorted_for_binary_search() {
        let mut sorted = STOPWORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(STOPWORDS.to_vec(), sorted, "STOPWORDS must stay sorted");
    }

    #[test]
    fn golden_plain_english() {
        assert_eq!(
            tokenize("The quick brown fox jumps over the lazy dog"),
            ["quick", "brown", "fox", "jumps", "over", "lazy", "dog"]
        );
    }

    #[test]
    fn golden_code_identifiers() {
        assert_eq!(
            tokenize("parseHttpRequest"),
            ["parsehttprequest", "parse", "http", "request"]
        );
        assert_eq!(
            tokenize("parse_rfc3339_utc"),
            ["parse_rfc3339_utc", "parse", "rfc", "3339", "utc"]
        );
        assert_eq!(
            tokenize("kebab-case-name"),
            ["kebab", "case", "name"],
            "kebab is split by the delimiter pass"
        );
        assert_eq!(
            tokenize("HTMLParser v2"),
            ["htmlparser", "html", "parser", "v2", "v", "2"]
        );
        assert_eq!(tokenize("utf8"), ["utf8", "utf", "8"]);
        assert_eq!(
            tokenize("FixedClock(1234)"),
            ["fixedclock", "fixed", "clock", "1234"]
        );
    }

    #[test]
    fn golden_unicode() {
        assert_eq!(tokenize("Größe MATTERS"), ["größe", "matters"]);
        assert_eq!(tokenize("Ünïcödé"), ["ünïcödé"]);
        // CJK has no case or internal boundaries we split.
        assert_eq!(tokenize("日本語 text"), ["日本語", "text"]);
        // Emoji are not alphanumeric: they drop as delimiters.
        assert_eq!(tokenize("hello 😀 world"), ["hello", "world"]);
    }

    #[test]
    fn golden_mixed_markdown() {
        assert_eq!(
            tokenize("## Fix `verify.sh` in CI (see #42)"),
            ["fix", "verify", "sh", "ci", "see", "42"]
        );
    }

    #[test]
    fn stopwords_dropped_but_kept_in_tokenize_all() {
        assert_eq!(tokenize("the store is a file"), ["store", "file"]);
        assert_eq!(
            tokenize_all("the store is a file"),
            ["the", "store", "is", "a", "file"]
        );
    }

    #[test]
    fn empty_and_whitespace_only_inputs() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("   \t\n  ").is_empty());
        assert!(tokenize("!!! ... ???").is_empty());
    }

    #[test]
    fn pure_function_double_run() {
        let text = "MixedCase_input with ünïcödé, digits42 and parseHttpRequest";
        assert_eq!(tokenize(text), tokenize(text));
        assert_eq!(tokenize_all(text), tokenize_all(text));
    }
}
