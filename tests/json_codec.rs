//! Comprehensive JSON codec suite (bead ghostie-rs-zya.1.3): round-trip,
//! escapes, adversarial input. Table-driven; failures print the offending
//! input. These tests are the reason capture can trust arbitrary session
//! logs later.

use ghostie::json::{self, JsonlReader, Number, Value};

// ---------- round-trip ----------

#[test]
fn round_trip_parse_emit_parse() {
    // parse(emit(v)) == v for a battery of values.
    let cases = [
        "null",
        "true",
        "false",
        "0",
        "-1",
        "9223372036854775807",
        "-9223372036854775808",
        r#""""#,
        r#""plain""#,
        "[]",
        "{}",
        r#"[1,[2,[3,[4]]],null]"#,
        r#"{"a":{"b":{"c":[true,false,null]}}}"#,
        r#"{"mixed":[1,"two",3.5,{"four":4}]}"#,
    ];
    for src in cases {
        let v = json::parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        let emitted = v.emit();
        let v2 = json::parse(&emitted).unwrap_or_else(|e| panic!("re-parse {emitted}: {e}"));
        assert_eq!(v, v2, "parse(emit(v)) == v for {src}");
    }
}

#[test]
fn canonical_inputs_emit_byte_identical() {
    // emit(parse(s)) byte-identical for canonical (compact) inputs.
    let cases = [
        "null",
        "true",
        "-42",
        r#""hi""#,
        r#"[1,2,3]"#,
        r#"{"a":1,"b":[true,null],"c":"x"}"#,
        r#"{"n":1.5,"e":1e10,"neg":-0.25}"#,
        "-0",
    ];
    for src in cases {
        let v = json::parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(v.emit(), src, "emit(parse(s)) byte-identical for {src}");
    }
}

#[test]
fn byte_stability_across_separately_constructed_values() {
    let build = || {
        Value::Object(vec![
            ("id".to_string(), Value::string("fact-a-1")),
            ("n".to_string(), Value::int(3)),
            (
                "tags".to_string(),
                Value::Array(vec![Value::string("x"), Value::string("y")]),
            ),
        ])
    };
    assert_eq!(build().emit(), build().emit());
    let one = build();
    assert_eq!(one.emit(), one.emit(), "same value emits identical bytes");
}

// ---------- escapes / unicode ----------

#[test]
fn all_rfc8259_escapes_decode() {
    let cases: &[(&str, &str)] = &[
        (r#""\"""#, "\""),
        (r#""\\""#, "\\"),
        (r#""\/""#, "/"),
        (r#""\b""#, "\u{08}"),
        (r#""\f""#, "\u{0C}"),
        (r#""\n""#, "\n"),
        (r#""\r""#, "\r"),
        (r#""\t""#, "\t"),
        (r#""\u0041""#, "A"),
        (r#""\u00e9""#, "é"),
        (r#""\u2603""#, "☃"),
        (r#""\u0000""#, "\u{0}"),    // embedded NUL via escape
        (r#""\ud83d\ude00""#, "😀"), // surrogate pair
        (r#""\uD83D\uDE00""#, "😀"), // uppercase hex
    ];
    for (src, want) in cases {
        let v = json::parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert_eq!(v.as_str().unwrap(), *want, "decoding {src}");
    }
}

#[test]
fn embedded_nul_round_trips() {
    let v = Value::string("a\u{0}b");
    let emitted = v.emit();
    assert_eq!(emitted, "\"a\\u0000b\"");
    assert_eq!(json::parse(&emitted).unwrap(), v);
}

#[test]
fn lone_surrogates_rejected() {
    for src in [
        r#""\ud800""#,
        r#""\udfff""#,
        r#""\ud83d""#,
        r#""\ud83dtail""#,
        r#""\ud83d\n""#,
        r#""\ud83dA""#,
        r#""\ude00\ud83d""#, // reversed order
    ] {
        assert!(json::parse(src).is_err(), "{src} must be rejected");
    }
}

#[test]
fn multibyte_utf8_in_keys_and_values() {
    let src = r#"{"schlüssel":"wert","日本語":"値","🔑":["emoji key",1]}"#;
    let v = json::parse(src).unwrap();
    assert_eq!(v.get("schlüssel").unwrap().as_str().unwrap(), "wert");
    assert_eq!(v.get("日本語").unwrap().as_str().unwrap(), "値");
    assert!(v.get("🔑").is_some());
    assert_eq!(v.emit(), src, "raw UTF-8 preserved byte-exact");
}

// ---------- numbers ----------

#[test]
fn i64_bounds_are_int_beyond_is_raw() {
    assert_eq!(
        json::parse("9223372036854775807").unwrap(),
        Value::Number(Number::Int(i64::MAX))
    );
    assert_eq!(
        json::parse("-9223372036854775808").unwrap(),
        Value::Number(Number::Int(i64::MIN))
    );
    // One past the boundary: preserved as raw text, re-emitted byte-exact.
    for src in ["9223372036854775808", "-9223372036854775809"] {
        let v = json::parse(src).unwrap();
        assert!(matches!(v, Value::Number(Number::Raw(_))), "{src} is Raw");
        assert_eq!(v.emit(), src);
    }
}

#[test]
fn negative_zero_is_raw_and_byte_exact() {
    let v = json::parse("-0").unwrap();
    assert!(matches!(v, Value::Number(Number::Raw(_))), "-0 must be Raw");
    assert_eq!(v.emit(), "-0");
}

#[test]
fn leading_zeros_rejected() {
    for src in ["01", "-01", "007", "0.1.2"] {
        assert!(json::parse(src).is_err(), "{src} must be rejected");
    }
}

#[test]
fn exponent_forms_preserved_as_raw_text() {
    for src in ["1e10", "1E10", "1e+10", "1e-10", "2.5e3", "-1.25E-7", "1e0"] {
        let v = json::parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
        assert!(matches!(v, Value::Number(Number::Raw(_))), "{src} is Raw");
        assert_eq!(v.emit(), src, "re-emitted byte-exact");
    }
}

#[test]
fn huge_digit_strings_survive() {
    let huge = "9".repeat(10_000);
    let v = json::parse(&huge).unwrap();
    assert_eq!(v.emit(), huge, "10k-digit number preserved verbatim");
    let huge_frac = format!("0.{}", "3".repeat(10_000));
    let v = json::parse(&huge_frac).unwrap();
    assert_eq!(v.emit(), huge_frac);
}

// ---------- adversarial ----------

#[test]
fn adversarial_inputs_error_cleanly() {
    // Must error, never panic, never hang.
    let cases = [
        "",
        " ",
        "\n\t ",
        "nul",
        "tru",
        "falsey",
        "nulll",
        "{",
        "}",
        "[",
        "]",
        "{\"a\"",
        "{\"a\":",
        "{\"a\":1",
        "{\"a\":1,",
        "{\"a\":1,}",
        "{,}",
        "{\"a\" 1}",
        "{1:2}",
        "[1",
        "[1,",
        "[1,]",
        "[,]",
        "\"",
        "\"abc",
        "\"abc\\",
        "\"abc\\u",
        "\"abc\\u00",
        "\"abc\\q\"",
        "-",
        "-.",
        "1.",
        ".5",
        "1e",
        "1e+",
        "+1",
        "{} {}",
        "[] []",
        "1 2",
        "null true",
    ];
    for src in cases {
        assert!(json::parse(src).is_err(), "{src:?} must error cleanly");
    }
}

#[test]
fn truncation_at_every_position_never_panics() {
    let doc = r#"{"key":[1,-2.5,"str ☃ 😀",{"n":null,"b":true}]}"#;
    for cut in 0..doc.len() {
        if !doc.is_char_boundary(cut) {
            continue;
        }
        let truncated = &doc[..cut];
        // Any result is fine except a panic; full doc must parse.
        let _ = json::parse(truncated);
    }
    assert!(json::parse(doc).is_ok());
}

#[test]
fn depth_bomb_beyond_limit_errors() {
    let bomb = "[".repeat(json::MAX_DEPTH * 4);
    let e = json::parse(&bomb).unwrap_err();
    assert!(
        e.to_string().contains("nesting"),
        "depth error is explicit: {e}"
    );
    // Alternating object/array bomb too.
    let bomb2 = "{\"a\":[".repeat(json::MAX_DEPTH * 2);
    assert!(json::parse(&bomb2).is_err());
}

// ---------- JSONL ----------

#[test]
fn jsonl_mixed_good_and_bad_lines() {
    let data = b"{\"a\":1}\n\n   \nnot json\n{\"b\":2}\n\xff\xfe invalid utf8\n{\"c\":3}";
    let items: Vec<_> = JsonlReader::new(&data[..], "session.jsonl").collect();
    // Lines: 1 good, 2+3 blank (skipped), 4 bad, 5 good, 6 invalid utf8, 7 good (no trailing newline).
    assert_eq!(items.len(), 5, "3 good + 2 bad, blanks skipped");
    assert_eq!(items[0].0, 1);
    assert!(items[0].1.is_ok());
    assert_eq!(items[1].0, 4);
    let bad = items[1].1.as_ref().unwrap_err().to_string();
    assert!(
        bad.contains("session.jsonl:4"),
        "bad line names origin+line: {bad}"
    );
    assert!(items[2].1.is_ok());
    assert_eq!(items[3].0, 6);
    let utf8_err = items[3].1.as_ref().unwrap_err().to_string();
    assert!(utf8_err.contains("UTF-8"), "utf8 error named: {utf8_err}");
    assert!(items[4].1.is_ok(), "iteration completed to the last line");
    assert_eq!(items[4].0, 7);
    assert_eq!(
        items[4].1.as_ref().unwrap().get("c").unwrap().as_i64(),
        Some(3),
        "final line without newline parsed"
    );
}

#[test]
fn jsonl_crlf_lines_parse() {
    let data = b"{\"a\":1}\r\n{\"b\":2}\r\n";
    let items: Vec<_> = JsonlReader::new(&data[..], "<crlf>").collect();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|(_, r)| r.is_ok()), "CRLF tolerated");
}

#[test]
fn jsonl_empty_input_yields_nothing() {
    let items: Vec<_> = JsonlReader::new(&b""[..], "<empty>").collect();
    assert!(items.is_empty());
}
