//! json — the crate's one JSON codec: std-only recursive-descent parser and
//! deterministic compact emitter.
//!
//! Single responsibility: (a) `--json` robot output on every CLI command,
//! (b) parsing session JSONL logs, (c) the derivable store index file.
//!
//! Determinism by construction:
//! - Objects are **ordered vectors**, not maps. The parser preserves input
//!   order; builders append in fixed code order; therefore emission is
//!   deterministic. For map-like data whose canonical form is lexicographic
//!   key order (the index), use [`Value::sorted_object`].
//! - Numbers: i64 fast path for integral values; anything non-integral or
//!   out of range keeps its raw source text ([`Number::Raw`]) so re-emission
//!   is byte-exact and no float ever enters our own logic. Documents ghostie
//!   itself emits only ever contain i64.
//! - The emitter is compact (no whitespace) with minimal RFC 8259 escaping:
//!   same [`Value`] -> identical bytes, every run, every platform.

use crate::error::{Error, Result};
use std::io::BufRead;

/// Hard recursion depth limit for the parser: bounds stack use on
/// adversarial input (arrays/objects nested deeper than this error cleanly).
pub const MAX_DEPTH: usize = 128;

/// A JSON number: integral fast path, raw text otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Number {
    /// An integer that fits i64 and whose source text is the canonical
    /// decimal rendering (so emission reproduces the input exactly).
    Int(i64),
    /// Any other syntactically valid number (fractional, exponent form,
    /// out-of-range, `-0`): the raw source text, re-emitted verbatim.
    Raw(String),
}

/// A parsed JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// A number (see [`Number`]).
    Number(Number),
    /// A string (fully unescaped).
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object as an **ordered** list of key/value pairs.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// Integer convenience constructor.
    pub fn int(v: i64) -> Value {
        Value::Number(Number::Int(v))
    }

    /// String convenience constructor.
    pub fn string(s: impl Into<String>) -> Value {
        Value::String(s.into())
    }

    /// Build an object whose canonical form is lexicographic key order
    /// (used for map-like data such as the store index). Sorting is by
    /// Unicode scalar values (Rust `str` ordering), stable for equal keys.
    pub fn sorted_object(mut pairs: Vec<(String, Value)>) -> Value {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Value::Object(pairs)
    }

    /// Look up a key in an object (first match). `None` for non-objects.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string payload, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The integer payload, if this is an integral number.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(Number::Int(v)) => Some(*v),
            _ => None,
        }
    }

    /// The boolean payload, if this is a bool.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The elements, if this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The pairs, if this is an object.
    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    /// Emit compact JSON: no whitespace, minimal escaping, deterministic.
    pub fn emit(&self) -> String {
        let mut out = String::new();
        self.emit_into(&mut out);
        out
    }

    fn emit_into(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Number(Number::Int(v)) => out.push_str(&v.to_string()),
            Value::Number(Number::Raw(s)) => out.push_str(s),
            Value::String(s) => emit_string(s, out),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.emit_into(out);
                }
                out.push(']');
            }
            Value::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    emit_string(k, out);
                    out.push(':');
                    v.emit_into(out);
                }
                out.push('}');
            }
        }
    }
}

/// Minimal RFC 8259 escaping: `"`, `\`, and control chars only. Non-ASCII
/// is emitted as raw UTF-8 (valid JSON, byte-stable, human-readable).
fn emit_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse one JSON document from a string. Origin `<json>` in errors.
pub fn parse(input: &str) -> Result<Value> {
    parse_with_origin(input, "<json>")
}

/// Parse one JSON document, naming `origin` (a file path, `<stdin>`, ...)
/// in any error. Leading/trailing whitespace is allowed; anything else
/// after the document is an error. Never panics on any input.
pub fn parse_with_origin(input: &str, origin: &str) -> Result<Value> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
        origin,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing garbage after JSON document"));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    origin: &'a str,
}

impl<'a> Parser<'a> {
    fn err(&self, message: impl Into<String>) -> Error {
        // 1-based line number from newlines seen so far; byte offset for
        // precision within the line.
        let line = 1 + self.bytes[..self.pos.min(self.bytes.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        Error::Parse {
            origin: self.origin.to_string(),
            line,
            message: format!("byte {}: {}", self.pos, message.into()),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected '{}'", b as char)))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(self.err(format!("nesting deeper than {MAX_DEPTH}")));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input, expected a value")),
            Some(b'n') => self.keyword("null", Value::Null),
            Some(b't') => self.keyword("true", Value::Bool(true)),
            Some(b'f') => self.keyword("false", Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(c) => Err(self.err(format!("unexpected byte 0x{c:02x}, expected a value"))),
        }
    }

    fn keyword(&mut self, word: &str, v: Value) -> Result<Value> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err(format!("invalid literal, expected '{word}'")))
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key in object"));
            }
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let v = self.value(depth + 1)?;
            pairs.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let esc = self
                        .peek()
                        .ok_or_else(|| self.err("unterminated escape sequence"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: require a low surrogate.
                                if self.peek() == Some(b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(self
                                            .err("high surrogate not followed by low surrogate"));
                                    }
                                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    char::from_u32(cp)
                                        .ok_or_else(|| self.err("invalid surrogate pair"))?
                                } else {
                                    return Err(self.err("lone high surrogate in \\u escape"));
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(self.err("lone low surrogate in \\u escape"));
                            } else {
                                char::from_u32(hi)
                                    .ok_or_else(|| self.err("invalid \\u code point"))?
                            };
                            out.push(c);
                        }
                        other => {
                            return Err(self.err(format!("invalid escape '\\{}'", other as char)));
                        }
                    }
                }
                Some(c) if c < 0x20 => {
                    return Err(self.err("unescaped control character in string"));
                }
                Some(_) => {
                    // Consume one UTF-8 encoded char. Input is &str, so
                    // byte boundaries are already valid UTF-8.
                    let start = self.pos;
                    let mut end = start + 1;
                    while end < self.bytes.len() && (self.bytes[end] & 0xC0) == 0x80 {
                        end += 1;
                    }
                    // Safe because the parser input came from &str.
                    let s = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| self.err("invalid UTF-8 in string"))?;
                    out.push_str(s);
                    self.pos = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self
                .peek()
                .ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = match c {
                b'0'..=b'9' => u32::from(c - b'0'),
                b'a'..=b'f' => u32::from(c - b'a') + 10,
                b'A'..=b'F' => u32::from(c - b'A') + 10,
                _ => return Err(self.err("non-hex digit in \\u escape")),
            };
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: 0 | [1-9][0-9]*
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err("leading zero in number"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number: expected digit")),
        }
        let mut integral = true;
        if self.peek() == Some(b'.') {
            integral = false;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: expected digit after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            integral = false;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number: expected digit in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // The slice is ASCII by construction.
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number bytes"))?;
        if integral {
            // Int only when the canonical i64 rendering reproduces the
            // source text exactly (rules out -0 and out-of-range), keeping
            // emit(parse(s)) byte-exact for every number.
            if let Ok(v) = text.parse::<i64>()
                && v.to_string() == text
            {
                return Ok(Value::Number(Number::Int(v)));
            }
        }
        Ok(Value::Number(Number::Raw(text.to_string())))
    }
}

/// Iterate JSONL: one JSON document per line, with per-line error recovery.
///
/// Yields `(line_number, Result<Value>)` (1-based). Empty and
/// whitespace-only lines are skipped. A bad line yields an `Err` naming the
/// line; iteration continues — capture streams large session files and must
/// skip bad lines without aborting. Invalid UTF-8 on a line is a per-line
/// error, not a stream abort. A final line without a trailing newline is
/// handled.
pub struct JsonlReader<R: BufRead> {
    reader: R,
    origin: String,
    line: usize,
}

impl<R: BufRead> JsonlReader<R> {
    /// Wrap a buffered reader; `origin` names the source in errors.
    pub fn new(reader: R, origin: impl Into<String>) -> Self {
        JsonlReader {
            reader,
            origin: origin.into(),
            line: 0,
        }
    }
}

impl<R: BufRead> Iterator for JsonlReader<R> {
    type Item = (usize, Result<Value>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut buf = Vec::new();
            match self.reader.read_until(b'\n', &mut buf) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    self.line += 1;
                    return Some((
                        self.line,
                        Err(Error::Io {
                            context: "reading JSONL line".to_string(),
                            path: self.origin.clone(),
                            source: e,
                        }),
                    ));
                }
            }
            self.line += 1;
            // Strip the newline (and a CR before it, for CRLF input).
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let text = match std::str::from_utf8(&buf) {
                Ok(t) => t,
                Err(_) => {
                    return Some((
                        self.line,
                        Err(Error::Parse {
                            origin: self.origin.clone(),
                            line: self.line,
                            message: "invalid UTF-8 on line".to_string(),
                        }),
                    ));
                }
            };
            if text.trim().is_empty() {
                continue; // skip blank lines
            }
            let result = parse_with_origin(text.trim(), &self.origin).map_err(|e| {
                // Rewrite the (per-line) line number into the stream's.
                match e {
                    Error::Parse {
                        origin, message, ..
                    } => Error::Parse {
                        origin,
                        line: self.line,
                        message,
                    },
                    other => other,
                }
            });
            return Some((self.line, result));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_scalars() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("42").unwrap(), Value::int(42));
        assert_eq!(parse("-7").unwrap(), Value::int(-7));
        assert_eq!(parse("\"hi\"").unwrap(), Value::string("hi"));
    }

    #[test]
    fn objects_preserve_input_order() {
        let v = parse(r#"{"zebra":1,"apple":2,"zebra2":3}"#).unwrap();
        let keys: Vec<&str> = v
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(keys, ["zebra", "apple", "zebra2"], "input order preserved");
    }

    #[test]
    fn sorted_object_is_lexicographic() {
        let v = Value::sorted_object(vec![
            ("zebra".into(), Value::int(1)),
            ("apple".into(), Value::int(2)),
        ]);
        assert_eq!(v.emit(), r#"{"apple":2,"zebra":1}"#);
    }

    #[test]
    fn emit_is_compact_and_stable() {
        let v = parse(r#"{ "a" : [ 1 , 2 ] , "b" : { "c" : null } }"#).unwrap();
        assert_eq!(v.emit(), r#"{"a":[1,2],"b":{"c":null}}"#);
        assert_eq!(v.emit(), v.emit(), "double emit identical");
    }

    #[test]
    fn non_integral_numbers_keep_raw_text() {
        for raw in [
            "1.5",
            "-0.25",
            "1e10",
            "2.5E-3",
            "-0",
            "123456789012345678901234567890",
        ] {
            let v = parse(raw).unwrap();
            assert_eq!(v.emit(), raw, "raw number text re-emitted byte-exact");
        }
    }

    #[test]
    fn surrogate_pair_escapes_decode() {
        // 😀 must combine to U+1F600 exactly.
        let v = parse(r#""\ud83d\ude00""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "\u{1F600}");
        // Literal (non-escaped) emoji also passes through.
        let v = parse(r#""😀""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "\u{1F600}");
    }

    #[test]
    fn emit_escapes_minimally_and_round_trips() {
        let s = "quote:\" backslash:\\ newline:\n tab:\t bell:\u{07} text";
        let v = Value::string(s);
        assert_eq!(
            v.emit(),
            r#""quote:\" backslash:\\ newline:\n tab:\t bell:\u0007 text""#
        );
        assert_eq!(parse(&v.emit()).unwrap(), v, "parse(emit(v)) == v");
    }

    #[test]
    fn lone_surrogates_are_rejected() {
        assert!(parse(r#""\ud83d""#).is_err(), "lone high surrogate");
        assert!(parse(r#""\ude00""#).is_err(), "lone low surrogate");
        assert!(parse(r#""\ud83dx""#).is_err(), "high surrogate then text");
    }

    #[test]
    fn depth_bomb_errors_cleanly() {
        let bomb = "[".repeat(MAX_DEPTH + 10) + &"]".repeat(MAX_DEPTH + 10);
        assert!(parse(&bomb).is_err(), "must error, not overflow the stack");
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok(), "at the limit still parses");
    }

    #[test]
    fn trailing_garbage_is_an_error() {
        assert!(parse("{} x").is_err());
        assert!(parse("1 2").is_err());
        assert!(parse("  {\"a\":1}  ").is_ok(), "plain whitespace is fine");
    }

    #[test]
    fn errors_carry_line_numbers() {
        let e = parse("{\"a\": 1,\n\"b\": }").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains(":2:"), "line 2 named in: {msg}");
    }

    #[test]
    fn jsonl_recovers_per_line() {
        let data = "{\"a\":1}\n\nnot json\n{\"b\":2}";
        let items: Vec<_> = JsonlReader::new(data.as_bytes(), "<test>").collect();
        assert_eq!(items.len(), 3, "blank line skipped");
        assert!(items[0].1.is_ok());
        assert_eq!(items[1].0, 3, "bad line numbered");
        assert!(items[1].1.is_err());
        assert!(items[2].1.is_ok(), "recovery after bad line");
        assert_eq!(items[2].0, 4, "final line without newline handled");
    }
}
