//! redact — deterministic, std-only secret scrubbing for the write path.
//!
//! Single responsibility: given free-form text (a transcript body, a title, a
//! rationale), replace anything that looks like a credential with
//! `[REDACTED:<kind>]` BEFORE it is ever written to disk (and therefore before
//! it can sync to the user's git remote). Capture ingests arbitrary agent
//! transcripts, which routinely echo API keys and tokens; scrubbing at the
//! write choke point ([`crate::store::Store::build_memory`]) means nothing
//! secret ever lands in a memory file.
//!
//! # Design religion
//!
//! - **std-only, no regex crate.** A small hand-rolled left-to-right scanner
//!   walks the bytes once; at each position it tries a fixed, ordered set of
//!   shape matchers anchored there.
//! - **Deterministic and byte-stable.** Pure function of the input: same input
//!   always yields the same output and the same count. No map iteration, no
//!   clock, no floats.
//! - **Conservative by construction.** Every matcher keys off a specific vendor
//!   prefix or an explicit `keyword=` / `Bearer ` / `Authorization:` context.
//!   There is deliberately **no** blunt "long high-entropy string" fallback:
//!   the tradeoff is that a bare, prefixless secret can slip through, but in
//!   exchange ordinary prose, memory ids (`rule-foo-1`), short git shas, and
//!   URLs are never mangled. Precision over recall, because a false positive
//!   silently corrupts the user's own memory and a missed prefixless blob is
//!   rare in practice.

/// One matched secret span. `keep` bytes at the anchor are copied verbatim
/// (e.g. the `Bearer ` prefix, or the `token=` key), then the replacement
/// marker is emitted, then scanning resumes at `end`.
struct Match {
    keep: usize,
    end: usize,
    kind: &'static str,
}

/// Replace detected secrets in `input` with `[REDACTED:<kind>]`.
///
/// Returns the scrubbed text and the number of redactions performed. Pure and
/// deterministic. Byte offsets are handled so the result is always valid UTF-8:
/// every matcher only ever spans ASCII bytes, and any non-ASCII byte is copied
/// through untouched.
pub fn redact(input: &str) -> (String, usize) {
    let b = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut count = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        if let Some(m) = match_at(b, i) {
            out.extend_from_slice(&b[i..i + m.keep]);
            out.extend_from_slice(b"[REDACTED:");
            out.extend_from_slice(m.kind.as_bytes());
            out.push(b']');
            count += 1;
            i = m.end;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // out is valid UTF-8 by construction (see the doc comment); the lossy
    // fallback exists only so a hypothetical bug degrades gracefully instead
    // of panicking on the write path.
    let text = String::from_utf8(out)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    (text, count)
}

/// Try every matcher at position `i`, in priority order (most specific and
/// longest-anchored first). The private-key block spans lines and must win
/// over any prefix that might appear inside it; the `Authorization:` header is
/// tried before the bare `Bearer ` so the header keeps its full prefix.
fn match_at(b: &[u8], i: usize) -> Option<Match> {
    match_private_key(b, i)
        .or_else(|| match_authorization(b, i))
        .or_else(|| match_bearer(b, i))
        .or_else(|| match_aws(b, i))
        .or_else(|| match_google(b, i))
        .or_else(|| match_github(b, i))
        .or_else(|| match_slack(b, i))
        .or_else(|| match_openai(b, i))
        .or_else(|| match_assignment(b, i))
}

// ---------- byte predicates ----------

fn is_base62(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// Key-material alphabet used by GitHub/Google/OpenAI style tokens.
fn is_key_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// Credential alphabet for `Bearer`/`Authorization` values: base64url plus the
/// JWT/base64 punctuation. A run outside this set is treated as prose, not a
/// token, which keeps `Bearer of bad news` from being redacted.
fn is_cred_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+' | b'/' | b'=')
}

/// Value alphabet for `keyword=value`: everything up to whitespace or a
/// separator (`&`, `;`) or a quote. Stops at `&` so a URL query string only
/// loses the one secret parameter.
fn is_value_char(c: u8) -> bool {
    !c.is_ascii_whitespace() && !matches!(c, b'&' | b';' | b',' | b'"' | b'\'')
}

/// A left word boundary: start of input, or a non-alphanumeric byte before the
/// anchor. Prevents matching a vendor prefix embedded inside a larger token
/// (e.g. the `AKIA` inside `XAKIA...`).
fn left_boundary(b: &[u8], i: usize) -> bool {
    i == 0 || !b[i - 1].is_ascii_alphanumeric()
}

fn starts_with(b: &[u8], i: usize, p: &[u8]) -> bool {
    b.len() >= i + p.len() && &b[i..i + p.len()] == p
}

fn starts_with_ci(b: &[u8], i: usize, p: &[u8]) -> bool {
    b.len() >= i + p.len()
        && b[i..i + p.len()]
            .iter()
            .zip(p)
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Length of the maximal run of bytes satisfying `pred`, starting at `i`.
fn run_len(b: &[u8], i: usize, pred: fn(u8) -> bool) -> usize {
    let mut j = i;
    while j < b.len() && pred(b[j]) {
        j += 1;
    }
    j - i
}

/// Advance past spaces and tabs (never a newline: values stay on their line).
fn skip_inline_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

/// First index at/after `from` where the substring `needle` begins, if any.
fn find(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from > b.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= b.len() {
        if &b[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---------- matchers ----------

/// PEM private-key block: `-----BEGIN ... PRIVATE KEY-----` through the closing
/// `-----END ... PRIVATE KEY-----`, redacted whole. Only fires when a matching
/// END is found, so a lone BEGIN line in prose is left alone.
fn match_private_key(b: &[u8], i: usize) -> Option<Match> {
    if !starts_with(b, i, b"-----BEGIN ") {
        return None;
    }
    let line_end = find(b, i, b"\n").unwrap_or(b.len());
    // The BEGIN line must actually declare a PRIVATE KEY.
    find(&b[..line_end], i, b"PRIVATE KEY-----")?;
    let end_marker = find(b, i, b"-----END")?;
    let after = end_marker + b"-----END".len();
    let close = find(b, after, b"-----")?;
    Some(Match {
        keep: 0,
        end: close + b"-----".len(),
        kind: "private-key",
    })
}

/// `Authorization: <scheme> <token>` (case-insensitive header). Keeps the
/// header, scheme, and spacing; redacts only the credential token.
fn match_authorization(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || !starts_with_ci(b, i, b"authorization:") {
        return None;
    }
    let mut j = i + "authorization:".len();
    j = skip_inline_ws(b, j);
    let scheme = run_len(b, j, |c| c.is_ascii_alphabetic());
    if scheme == 0 {
        return None;
    }
    j += scheme;
    j = skip_inline_ws(b, j);
    let tok = run_len(b, j, is_cred_char);
    if tok < 8 {
        return None;
    }
    Some(Match {
        keep: j - i,
        end: j + tok,
        kind: "bearer",
    })
}

/// `Bearer <token>`: keeps the `Bearer ` prefix, redacts the token. Requires a
/// token of credential chars at least 12 long so ordinary prose after the word
/// `Bearer` is never touched.
fn match_bearer(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || !starts_with(b, i, b"Bearer") {
        return None;
    }
    let mut j = i + "Bearer".len();
    // Require at least one space between `Bearer` and the token.
    if j >= b.len() || (b[j] != b' ' && b[j] != b'\t') {
        return None;
    }
    j = skip_inline_ws(b, j);
    let tok = run_len(b, j, is_cred_char);
    if tok < 12 {
        return None;
    }
    Some(Match {
        keep: j - i,
        end: j + tok,
        kind: "bearer",
    })
}

/// AWS access key id: `AKIA` + 16 uppercase-alnum.
fn match_aws(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || !starts_with(b, i, b"AKIA") {
        return None;
    }
    let start = i + 4;
    let run = run_len(b, start, |c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if run < 16 {
        return None;
    }
    Some(Match {
        keep: 0,
        end: start + 16,
        kind: "aws-key",
    })
}

/// Google API key: `AIza` + 35 of `[A-Za-z0-9_-]`.
fn match_google(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || !starts_with(b, i, b"AIza") {
        return None;
    }
    let start = i + 4;
    let run = run_len(b, start, is_key_char);
    if run < 35 {
        return None;
    }
    Some(Match {
        keep: 0,
        end: start + 35,
        kind: "google-key",
    })
}

/// GitHub token: `ghp_|gho_|ghu_|ghs_|ghr_` + 36 or more base62.
fn match_github(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || b.len() < i + 4 || &b[i..i + 2] != b"gh" {
        return None;
    }
    if !matches!(b[i + 2], b'p' | b'o' | b'u' | b's' | b'r') || b[i + 3] != b'_' {
        return None;
    }
    let start = i + 4;
    let run = run_len(b, start, is_base62);
    if run < 36 {
        return None;
    }
    Some(Match {
        keep: 0,
        end: start + run,
        kind: "github-token",
    })
}

/// Slack token: `xox[baprs]-` + a long `[A-Za-z0-9-]` tail.
fn match_slack(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) || b.len() < i + 5 || &b[i..i + 3] != b"xox" {
        return None;
    }
    if !matches!(b[i + 3], b'b' | b'a' | b'p' | b'r' | b's') || b[i + 4] != b'-' {
        return None;
    }
    let start = i + 5;
    let run = run_len(b, start, |c| c.is_ascii_alphanumeric() || c == b'-');
    if run < 10 {
        return None;
    }
    Some(Match {
        keep: 0,
        end: start + run,
        kind: "slack-token",
    })
}

/// OpenAI / Anthropic keys: `sk-ant-` + long key material, or `sk-` + 20 or
/// more base62.
fn match_openai(b: &[u8], i: usize) -> Option<Match> {
    if !left_boundary(b, i) {
        return None;
    }
    if starts_with(b, i, b"sk-ant-") {
        let start = i + "sk-ant-".len();
        let run = run_len(b, start, is_key_char);
        if run >= 20 {
            return Some(Match {
                keep: 0,
                end: start + run,
                kind: "api-key",
            });
        }
    }
    if starts_with(b, i, b"sk-") {
        let start = i + 3;
        let run = run_len(b, start, is_base62);
        if run >= 20 {
            return Some(Match {
                keep: 0,
                end: start + run,
                kind: "api-key",
            });
        }
    }
    None
}

/// `keyword = value` assignment (case-insensitive keyword): redact the value.
/// Keeps the key, the `=`, and surrounding spacing; a quoted value is redacted
/// whole (including its quotes). Requires a value of at least 6 characters so
/// `secret=true` style config booleans are not disturbed.
fn match_assignment(b: &[u8], i: usize) -> Option<Match> {
    const KEYS: [&[u8]; 6] = [
        b"password",
        b"passwd",
        b"secret",
        b"token",
        b"api_key",
        b"apikey",
    ];
    if !left_boundary(b, i) {
        return None;
    }
    let key_len = KEYS
        .iter()
        .find(|k| starts_with_ci(b, i, k))
        .map(|k| k.len())?;
    let mut j = i + key_len;
    j = skip_inline_ws(b, j);
    if j >= b.len() || b[j] != b'=' {
        return None;
    }
    j += 1;
    j = skip_inline_ws(b, j);
    if j >= b.len() {
        return None;
    }
    let value_start = j;
    // Idempotence: never re-redact a value that is already a redaction marker
    // (its `[REDACTED:...]` chars would otherwise read as a fresh secret).
    if starts_with(b, value_start, b"[REDACTED:") {
        return None;
    }
    if b[j] == b'"' || b[j] == b'\'' {
        let quote = b[j];
        let close = find(b, j + 1, &[quote])?;
        if close - (j + 1) < 1 {
            return None;
        }
        return Some(Match {
            keep: value_start - i,
            end: close + 1,
            kind: "secret-assign",
        });
    }
    let run = run_len(b, j, is_value_char);
    if run < 6 {
        return None;
    }
    Some(Match {
        keep: value_start - i,
        end: value_start + run,
        kind: "secret-assign",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic, obviously-fake credentials only. Never a real secret.
    const AWS: &str = "AKIAIOSFODNN7EXAMPLE";

    fn ghp() -> String {
        format!("ghp_{}", "x".repeat(36))
    }

    #[test]
    fn aws_key_is_redacted() {
        let (out, n) = redact(&format!("key is {AWS} ok"));
        assert_eq!(out, "key is [REDACTED:aws-key] ok");
        assert_eq!(n, 1);
    }

    #[test]
    fn github_token_is_redacted() {
        let (out, n) = redact(&format!("token {} done", ghp()));
        assert_eq!(out, "token [REDACTED:github-token] done");
        assert_eq!(n, 1);
        // Every gh* prefix variant.
        for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let (o, c) = redact(&format!("{p}{}", "a".repeat(36)));
            assert_eq!(o, "[REDACTED:github-token]", "prefix {p}");
            assert_eq!(c, 1);
        }
    }

    #[test]
    fn openai_and_anthropic_keys_are_redacted() {
        let (out, n) = redact(&format!("sk-ant-{}", "A".repeat(40)));
        assert_eq!(out, "[REDACTED:api-key]");
        assert_eq!(n, 1);
        let (out, n) = redact(&format!("use sk-{} here", "b".repeat(32)));
        assert_eq!(out, "use [REDACTED:api-key] here");
        assert_eq!(n, 1);
    }

    #[test]
    fn slack_token_is_redacted() {
        let (out, n) = redact(&format!("xoxb-{}", "1234567890abcdef".repeat(2)));
        assert_eq!(out, "[REDACTED:slack-token]");
        assert_eq!(n, 1);
    }

    #[test]
    fn google_key_is_redacted() {
        let (out, n) = redact("AIzaA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r");
        assert_eq!(out, "[REDACTED:google-key]");
        assert_eq!(n, 1);
    }

    #[test]
    fn bearer_and_authorization_redact_only_the_token() {
        let (out, n) = redact("Authorization: Bearer abc123.def456.ghi789xyz");
        assert_eq!(out, "Authorization: Bearer [REDACTED:bearer]");
        assert_eq!(n, 1);
        let (out, n) = redact("Bearer abc123DEF456ghi789");
        assert_eq!(out, "Bearer [REDACTED:bearer]");
        assert_eq!(n, 1);
        // Non-Bearer scheme.
        let (out, n) = redact("authorization: Basic dXNlcjpwYXNzd29yZA==");
        assert_eq!(out, "authorization: Basic [REDACTED:bearer]");
        assert_eq!(n, 1);
    }

    #[test]
    fn private_key_block_is_redacted_whole() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAK\nfakefakefake\n-----END RSA PRIVATE KEY-----";
        let (out, n) = redact(&format!("before\n{pem}\nafter"));
        assert_eq!(out, "before\n[REDACTED:private-key]\nafter");
        assert_eq!(n, 1);
    }

    #[test]
    fn assignment_style_redacts_the_value() {
        let (out, n) = redact("password=hunter2seekrit");
        assert_eq!(out, "password=[REDACTED:secret-assign]");
        assert_eq!(n, 1);
        let (out, _) = redact("API_KEY = longsecretvalue123");
        assert_eq!(out, "API_KEY = [REDACTED:secret-assign]");
        let (out, _) = redact(r#"token="quotedsecret""#);
        assert_eq!(out, "token=[REDACTED:secret-assign]");
        // URL query: only the token param is scrubbed, the rest survives.
        let (out, _) = redact("https://h/cb?token=abcdefghijkl&next=/home");
        assert_eq!(
            out,
            "https://h/cb?token=[REDACTED:secret-assign]&next=/home"
        );
    }

    #[test]
    fn multiple_secrets_counted_and_replaced() {
        let (out, n) = redact(&format!("a {AWS} b {} c", ghp()));
        assert_eq!(out, "a [REDACTED:aws-key] b [REDACTED:github-token] c");
        assert_eq!(n, 2);
    }

    #[test]
    fn false_positives_are_not_mangled() {
        // A memory id, a short git sha, ordinary prose, a plain URL, and a
        // config boolean must all pass through byte-identical.
        for s in [
            "rule-foo-1",
            "see commit a1b2c3d for the fix",
            "The quick brown fox jumps over the lazy dog.",
            "https://example.com/path/to/page?ref=home",
            "secret=true",
            "token=yes",
            "the risk-averse task-runner asked a question",
            "Bearer of bad news arrived at noon",
            "session-summary-fixed-tokenizer-bug-abc12345",
        ] {
            let (out, n) = redact(s);
            assert_eq!(out, s, "must not change: {s:?}");
            assert_eq!(n, 0, "no redactions for: {s:?}");
        }
    }

    #[test]
    fn determinism_double_run_identical() {
        let input = format!(
            "log: {AWS}\nAuthorization: Bearer tok.tok.tokabc123\npassword=supersecretvalue\n{}",
            ghp()
        );
        let (a, na) = redact(&input);
        let (b, nb) = redact(&input);
        assert_eq!(a, b);
        assert_eq!(na, nb);
        // Redacting the already-redacted text finds nothing new.
        let (c, nc) = redact(&a);
        assert_eq!(c, a);
        assert_eq!(nc, 0);
    }

    #[test]
    fn non_ascii_is_preserved() {
        let (out, n) = redact("café ünïcödé stays intact");
        assert_eq!(out, "café ünïcödé stays intact");
        assert_eq!(n, 0);
        let (out, n) = redact(&format!("café {AWS} ünï"));
        assert_eq!(out, "café [REDACTED:aws-key] ünï");
        assert_eq!(n, 1);
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let (out, n) = redact("");
        assert_eq!(out, "");
        assert_eq!(n, 0);
    }
}
