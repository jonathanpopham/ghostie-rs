//! Frontmatter codec: tolerant parse + canonical byte-stable serialize.
//!
//! Implements `docs/FORMAT.md` exactly. This is the byte-stability
//! keystone: every store guarantee reduces to this codec being right.
//!
//! The codec is schema-agnostic — it moves ordered key/value pairs plus a
//! body; the typed memory model layers on top. It does know the canonical
//! *key order* (schema keys first, unknown keys last in first-seen order),
//! because "tolerant read, canonical write" must hold even for a codec-only
//! round trip of a hand-reordered file.
//!
//! Contracts (doc-tested below):
//! 1. Idempotence: `serialize(parse(serialize(d))) == serialize(d)`.
//! 2. Tolerant-read canonical-write: parsing any accepted variant then
//!    serializing yields the canonical bytes.

use crate::error::{Error, Result};

/// Canonical frontmatter key order per docs/FORMAT.md. Unknown keys follow,
/// in first-seen order.
pub const SCHEMA_KEY_ORDER: [&str; 12] = [
    "id",
    "type",
    "created",
    "title",
    "tags",
    "links",
    "source",
    "supersedes",
    "harness",
    "core",
    "rationale",
    "scope",
];

/// A frontmatter value: bare/quoted scalar or inline list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FmValue {
    /// A single scalar (unquoted content; quoting is a serialization detail).
    Scalar(String),
    /// An inline list of scalars.
    List(Vec<String>),
}

/// A parsed memory file: ordered key/value pairs + body. Schema-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterDoc {
    /// Frontmatter pairs in parse (or construction) order.
    pub pairs: Vec<(String, FmValue)>,
    /// The Markdown body, as read. Normalized only on write.
    pub body: String,
}

impl FrontmatterDoc {
    /// First value for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&FmValue> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Serialize to canonical bytes per docs/FORMAT.md:
    /// schema key order (unknown keys last, first-seen), `key: value` with
    /// one space after the colon, LF endings, no trailing whitespace,
    /// exactly one trailing newline, empty lists omitted.
    ///
    /// ```
    /// use ghostie::store::frontmatter::{parse, FrontmatterDoc};
    /// // Idempotence: serialize(parse(serialize(d))) == serialize(d).
    /// let d = parse("---\ntitle: t\nid: x-1\ntype: fact\ncreated: 2026-01-01T00:00:00Z\n---\nbody\n", "<mem>").unwrap();
    /// let once = d.serialize();
    /// let twice = parse(&once, "<mem>").unwrap().serialize();
    /// assert_eq!(once, twice);
    /// // Canonical write reordered the hand-edited keys into schema order.
    /// assert!(once.starts_with("---\nid: x-1\ntype: fact\n"));
    /// ```
    pub fn serialize(&self) -> String {
        let mut out = String::from("---\n");
        // Schema keys first, in schema order; then unknowns, first-seen.
        for want in SCHEMA_KEY_ORDER {
            for (k, v) in &self.pairs {
                if k == want {
                    emit_pair(&mut out, k, v);
                }
            }
        }
        for (k, v) in &self.pairs {
            if !SCHEMA_KEY_ORDER.contains(&k.as_str()) {
                emit_pair(&mut out, k, v);
            }
        }
        out.push_str("---\n");
        // Body: LF endings, no trailing whitespace on any line, exactly one
        // trailing newline; an empty body ends the file right here.
        let body = self.body.replace("\r\n", "\n").replace('\r', "\n");
        let trimmed: Vec<&str> = body.lines().map(|l| l.trim_end()).collect();
        // Drop trailing blank lines so the file ends with exactly one \n.
        let last_content = trimmed.iter().rposition(|l| !l.is_empty());
        if let Some(last) = last_content {
            for line in &trimmed[..=last] {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

fn emit_pair(out: &mut String, key: &str, value: &FmValue) {
    match value {
        FmValue::List(items) if items.is_empty() => {} // omitted
        FmValue::List(items) => {
            out.push_str(key);
            out.push_str(": [");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_scalar(out, item);
            }
            out.push_str("]\n");
        }
        FmValue::Scalar(s) => {
            out.push_str(key);
            out.push_str(": ");
            emit_scalar(out, s);
            out.push('\n');
        }
    }
}

/// The exact bare-vs-quoted rule from docs/FORMAT.md.
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.starts_with([' ', '\t'])
        || s.ends_with([' ', '\t'])
        || s.chars()
            .any(|c| matches!(c, '"' | '[' | ']' | ',' | '#') || c.is_control())
}

fn emit_scalar(out: &mut String, s: &str) {
    if needs_quoting(s) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// Tolerant parse of a memory file per docs/FORMAT.md.
///
/// Accepts CRLF, reordered keys, extra blank lines, trailing whitespace and
/// a missing final newline. Duplicate keys, missing/unclosed frontmatter and
/// malformed values are typed errors naming `origin` and the 1-based line.
pub fn parse(text: &str, origin: &str) -> Result<FrontmatterDoc> {
    let err = |line: usize, message: String| Error::Parse {
        origin: origin.to_string(),
        line,
        message,
    };
    // Split into lines without consuming the body's exact content: we track
    // byte offsets so the body can be sliced verbatim.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_at = |idx: usize| -> &str {
        let start = line_starts[idx];
        let end = line_starts.get(idx + 1).map_or(text.len(), |&e| e - 1);
        text[start..end]
            .strip_suffix('\r')
            .unwrap_or(&text[start..end])
    };
    let total_lines = if text.is_empty() {
        0
    } else {
        line_starts.len() - usize::from(text.ends_with('\n'))
    };

    // Opening delimiter: first non-blank line must be exactly `---`.
    let mut idx = 0;
    while idx < total_lines && line_at(idx).trim().is_empty() {
        idx += 1;
    }
    if idx >= total_lines || line_at(idx).trim_end() != "---" {
        return Err(err(
            idx + 1,
            "not a memory file: expected `---` frontmatter delimiter on the first line".to_string(),
        ));
    }
    idx += 1;

    let mut pairs: Vec<(String, FmValue)> = Vec::new();
    let mut closed = false;
    while idx < total_lines {
        let raw = line_at(idx);
        let line = raw.trim_end();
        if line == "---" {
            closed = true;
            idx += 1;
            break;
        }
        if line.trim().is_empty() {
            idx += 1; // tolerated blank line inside frontmatter
            continue;
        }
        let Some(colon) = line.find(':') else {
            return Err(err(
                idx + 1,
                format!("expected `key: value` or a closing `---` delimiter, got {line:?}"),
            ));
        };
        let key = line[..colon].trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(err(
                idx + 1,
                format!("invalid key {key:?}: keys are [a-z0-9_]+"),
            ));
        }
        if pairs.iter().any(|(k, _)| k == key) {
            return Err(err(idx + 1, format!("duplicate key '{key}'")));
        }
        let value_text = line[colon + 1..].trim();
        let value = parse_value(value_text, idx + 1, origin)?;
        pairs.push((key.to_string(), value));
        idx += 1;
    }
    if !closed {
        return Err(err(
            total_lines.max(1),
            "unclosed frontmatter: missing the second `---` delimiter".to_string(),
        ));
    }

    // Body: everything after the closing delimiter line, verbatim.
    let body = line_starts
        .get(idx)
        .map_or(String::new(), |&off| text[off..].to_string());
    Ok(FrontmatterDoc { pairs, body })
}

fn parse_value(text: &str, line: usize, origin: &str) -> Result<FmValue> {
    let err = |message: String| Error::Parse {
        origin: origin.to_string(),
        line,
        message,
    };
    if let Some(rest) = text.strip_prefix('[') {
        let Some(inner) = rest.strip_suffix(']') else {
            return Err(err(format!("unclosed list value {text:?}")));
        };
        let mut items = Vec::new();
        let mut in_quotes = false;
        let mut escaped = false;
        let mut boundaries = Vec::new();
        for (i, c) in inner.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_quotes => escaped = true,
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => boundaries.push(i),
                _ => {}
            }
        }
        if in_quotes {
            return Err(err(format!("unterminated quote in list {text:?}")));
        }
        let mut prev = 0usize;
        let mut segments: Vec<&str> = Vec::new();
        for b in boundaries {
            segments.push(&inner[prev..b]);
            prev = b + 1;
        }
        segments.push(&inner[prev..]);
        // `[]` (or all-blank interior) is the empty list.
        if segments.iter().all(|s| s.trim().is_empty()) && segments.len() == 1 {
            return Ok(FmValue::List(items));
        }
        for seg in segments {
            let seg = seg.trim();
            if seg.is_empty() {
                return Err(err(format!("empty element in list {text:?}")));
            }
            items.push(parse_scalar(seg, line, origin)?);
        }
        Ok(FmValue::List(items))
    } else {
        Ok(FmValue::Scalar(parse_scalar(text, line, origin)?))
    }
}

fn parse_scalar(text: &str, line: usize, origin: &str) -> Result<String> {
    let err = |message: String| Error::Parse {
        origin: origin.to_string(),
        line,
        message,
    };
    if let Some(rest) = text.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        loop {
            match chars.next() {
                None => return Err(err(format!("unterminated quoted scalar {text:?}"))),
                Some('\\') => match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    other => {
                        return Err(err(format!(
                            "invalid escape in quoted scalar: \\{}",
                            other.map_or(String::from("<end>"), |c| c.to_string())
                        )));
                    }
                },
                Some('"') => {
                    let trailing: String = chars.collect();
                    if !trailing.trim().is_empty() {
                        return Err(err(format!(
                            "unexpected text after closing quote: {trailing:?}"
                        )));
                    }
                    return Ok(out);
                }
                Some(c) => out.push(c),
            }
        }
    } else {
        // Bare scalar: the trimmed text, whatever it contains (tolerant).
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL: &str = "---\nid: fact-store-root-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: Store root defaults to ~/.ghostie\ntags: [store, layout]\n---\nThe store root is $GHOSTIE_HOME or ~/.ghostie.\n";

    #[test]
    fn canonical_file_round_trips_byte_identical() {
        let d = parse(CANONICAL, "<t>").unwrap();
        assert_eq!(d.serialize(), CANONICAL);
    }

    #[test]
    fn tolerant_read_canonical_write() {
        // CRLF, reordered keys, extra blank lines, trailing whitespace,
        // missing final newline, quoted-where-bare-would-do.
        let messy = "---\r\n\r\ntitle: \"Store root defaults to ~/.ghostie\"   \r\ntags: [store, layout]\r\ncreated: 2026-07-13T12:00:00Z\r\nid: fact-store-root-1\r\n\r\ntype: fact\r\n---\r\nThe store root is $GHOSTIE_HOME or ~/.ghostie.";
        let d = parse(messy, "<t>").unwrap();
        assert_eq!(
            d.serialize(),
            CANONICAL,
            "any accepted variant -> canonical bytes"
        );
    }

    #[test]
    fn serialize_is_idempotent_through_parse() {
        let d = parse(CANONICAL, "<t>").unwrap();
        let once = d.serialize();
        let twice = parse(&once, "<t>").unwrap().serialize();
        assert_eq!(once, twice);
    }

    #[test]
    fn quoting_rule_corners() {
        let cases: &[(&str, &str)] = &[
            ("plain", "k: plain\n"),
            ("has space inside", "k: has space inside\n"),
            ("", "k: \"\"\n"),
            ("colon: inside stays bare", "k: colon: inside stays bare\n"),
            ("2026-07-13T12:00:00Z", "k: 2026-07-13T12:00:00Z\n"),
            ("bracket[y]", "k: \"bracket[y]\"\n"),
            ("comma, here", "k: \"comma, here\"\n"),
            ("hash # here", "k: \"hash # here\"\n"),
            (" leading space", "k: \" leading space\"\n"),
            ("trailing tab\t", "k: \"trailing tab\t\"\n"),
            (
                "quote \" and back \\ slash",
                "k: \"quote \\\" and back \\\\ slash\"\n",
            ),
        ];
        for (value, want_line) in cases {
            let d = FrontmatterDoc {
                pairs: vec![("k".to_string(), FmValue::Scalar(value.to_string()))],
                body: String::new(),
            };
            let out = d.serialize();
            let want = format!("---\n{want_line}---\n");
            assert_eq!(out, want, "serializing scalar {value:?}");
            // And it parses back to the same value.
            let back = parse(&out, "<t>").unwrap();
            assert_eq!(back.get("k"), Some(&FmValue::Scalar(value.to_string())));
        }
    }

    #[test]
    fn inline_lists_parse_and_canonicalize() {
        let d = parse("---\ntags: [ a ,b,  \"c, with comma\" ]\n---\n", "<t>").unwrap();
        assert_eq!(
            d.get("tags"),
            Some(&FmValue::List(vec![
                "a".to_string(),
                "b".to_string(),
                "c, with comma".to_string()
            ]))
        );
        assert_eq!(d.serialize(), "---\ntags: [a, b, \"c, with comma\"]\n---\n");
    }

    #[test]
    fn empty_list_parses_and_is_omitted_on_write() {
        let d = parse("---\ntags: []\nid: x-1\n---\n", "<t>").unwrap();
        assert_eq!(d.get("tags"), Some(&FmValue::List(vec![])));
        assert_eq!(d.serialize(), "---\nid: x-1\n---\n", "empty list omitted");
    }

    #[test]
    fn empty_body_ends_after_closing_delimiter() {
        let d = parse("---\nid: x-1\n---\n", "<t>").unwrap();
        assert_eq!(d.body, "");
        assert_eq!(d.serialize(), "---\nid: x-1\n---\n");
        // Body of only blank lines collapses to empty.
        let d2 = parse("---\nid: x-1\n---\n\n\n  \n", "<t>").unwrap();
        assert_eq!(d2.serialize(), "---\nid: x-1\n---\n");
    }

    #[test]
    fn no_frontmatter_is_an_error() {
        for bad in ["just text\n", "", "# heading\n---\n"] {
            let e = parse(bad, "memories/x.md").unwrap_err();
            let msg = e.to_string();
            assert!(msg.contains("memories/x.md"), "origin named in: {msg}");
            assert!(msg.contains("---"), "delimiter explained in: {msg}");
        }
    }

    #[test]
    fn unclosed_frontmatter_is_an_error() {
        // EOF while still inside the frontmatter block.
        let e = parse("---\nid: x-1\n", "<t>").unwrap_err();
        assert!(e.to_string().contains("unclosed"), "{e}");
        // Body-looking line while unclosed: error mentions the delimiter.
        let e2 = parse("---\nid: x-1\nbody text\n", "<t>").unwrap_err();
        assert!(e2.to_string().contains("---"), "{e2}");
    }

    #[test]
    fn duplicate_key_is_an_error_naming_file_and_line() {
        let e = parse("---\nid: x-1\ntags: [a]\nid: y-2\n---\n", "memories/x.md").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("memories/x.md:4"), "file:line in: {msg}");
        assert!(msg.contains("duplicate key 'id'"), "key named in: {msg}");
    }

    #[test]
    fn invalid_keys_and_values_error_with_lines() {
        assert!(
            parse("---\nBadKey: x\n---\n", "<t>").is_err(),
            "uppercase key"
        );
        assert!(
            parse("---\nno colon here\n---\n", "<t>").is_err(),
            "no colon"
        );
        assert!(
            parse("---\nk: [a, b\n---\n", "<t>").is_err(),
            "unclosed list"
        );
        assert!(parse("---\nk: \"unterminated\n---\n", "<t>").is_err());
        assert!(
            parse("---\nk: [a,,b]\n---\n", "<t>").is_err(),
            "empty element"
        );
        assert!(
            parse("---\nk: \"x\" tail\n---\n", "<t>").is_err(),
            "text after quote"
        );
    }

    #[test]
    fn body_is_verbatim_on_parse() {
        let src = "---\nid: x-1\n---\nline one\n\n  indented\ncode `fence`\n";
        let d = parse(src, "<t>").unwrap();
        assert_eq!(d.body, "line one\n\n  indented\ncode `fence`\n");
        assert_eq!(d.serialize(), src, "already-canonical body untouched");
    }

    #[test]
    fn unknown_keys_preserved_in_first_seen_order_after_schema_keys() {
        let src = "---\nzeta: 1\nid: x-1\nalpha: 2\ntype: fact\n---\n";
        let d = parse(src, "<t>").unwrap();
        assert_eq!(
            d.serialize(),
            "---\nid: x-1\ntype: fact\nzeta: 1\nalpha: 2\n---\n",
            "schema keys in schema order, unknowns last in first-seen order"
        );
    }
}
