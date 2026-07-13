//! CLI e2e (bead ghostie-rs-zya.4.7): drive the REAL compiled binary the
//! way users and agents will, via std::process::Command and
//! env!("CARGO_BIN_EXE_ghostie").
//!
//! GOLDEN REGENERATION: the exact-stdout goldens below are inline string
//! literals. To regenerate after an intended output change, run the
//! failing test, copy the printed `got` value, and update the literal —
//! deliberately, in a reviewed diff.
//!
//! Extension contract: the capture and sync CLI beads MUST add their
//! cases here when they land.

use ghostie::json::{self, Value};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ghostie");
/// Every e2e store is stamped with this instant via GHOSTIE_TEST_CLOCK so
/// stdout goldens are exact.
const CLOCK: &str = "2026-07-13T12:00:00Z";

fn temp_store(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ghostie-e2e-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn ghostie(store: &PathBuf, args: &[&str]) -> Output {
    Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", CLOCK)
        .env_remove("GHOSTIE_HOME")
        // Isolate HOME to the store so `setup`/`hook` never touch the real
        // ~/.claude during tests.
        .env("HOME", store)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("binary runs")
}

fn ghostie_stdin(store: &PathBuf, args: &[&str], stdin: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", CLOCK)
        .env_remove("GHOSTIE_HOME")
        .arg("--store")
        .arg(store)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).unwrap()
}

fn seed_five(store: &PathBuf) {
    for (ty, title, tags, body) in [
        (
            "rule",
            "Sync branch is sync never main",
            "git,sync",
            "Push to the sync branch only.",
        ),
        (
            "rule",
            "Always run verify.sh before commit",
            "ci",
            "The gate is verify.sh.",
        ),
        (
            "decision",
            "Chose fixed-point over floats",
            "determinism",
            "Floats round differently.",
        ),
        (
            "fact",
            "Configs live in etc",
            "layout",
            "All configs live in etc/.",
        ),
        (
            "fact",
            "parseEventStream is the hot path",
            "performance",
            "Optimize parseEventStream first.",
        ),
    ] {
        let o = ghostie(
            store,
            &[
                "remember", "--type", ty, title, "--tags", tags, "--body", body,
            ],
        );
        assert!(o.status.success(), "seed failed: {}", stderr(&o));
    }
}

// ---------- the milestone loop ----------

#[test]
fn milestone_loop_remember_list_recall_exact_goldens() {
    let store = temp_store("milestone");
    seed_five(&store);

    // list: exact human golden (clock is fixed, so dates are exact).
    let o = ghostie(&store, &["list"]);
    assert!(o.status.success());
    assert_eq!(
        stdout(&o),
        "\
decision-chose-fixed-point-over-floats-1   decision  2026-07-13  Chose fixed-point over floats  [determinism]
fact-configs-live-in-etc-1                 fact      2026-07-13  Configs live in etc  [layout]
fact-parseeventstream-is-the-hot-path-1    fact      2026-07-13  parseEventStream is the hot path  [performance]
rule-always-run-verify-sh-before-commit-1  rule      2026-07-13  Always run verify.sh before commit  [ci]
rule-sync-branch-is-sync-never-main-1      rule      2026-07-13  Sync branch is sync never main  [git, sync]
"
    );

    // recall: expected memory at rank 1 with a non-empty why (human).
    let o = ghostie(&store, &["recall", "which branch do we sync to"]);
    assert!(o.status.success());
    let human = stdout(&o);
    assert!(
        human.starts_with(" 1. rule-sync-branch-is-sync-never-main-1  [rule]  score "),
        "{human}"
    );
    assert!(human.contains("why: "), "{human}");

    // recall --json: parse with our own json module (dogfooding).
    let o = ghostie(&store, &["recall", "which branch do we sync to", "--json"]);
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    assert_eq!(doc.get("ok").and_then(Value::as_bool), Some(true));
    let hits = doc
        .get("data")
        .and_then(|d| d.get("hits"))
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(
        hits[0].get("id").and_then(Value::as_str),
        Some("rule-sync-branch-is-sync-never-main-1")
    );
    assert!(
        !hits[0]
            .get("why")
            .and_then(|w| w.get("matched_terms"))
            .and_then(Value::as_array)
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(store);
}

// ---------- robot-mode contract, mechanically ----------

#[test]
fn robot_mode_contract_for_every_subcommand() {
    let store = temp_store("robot");
    seed_five(&store);
    // The dispatcher's own list is the source of truth: a new verb is
    // auto-covered here (and must ship safe audit args below).
    let o = ghostie(&store, &["_subcommands", "--json"]);
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    let subcommands: Vec<String> = doc
        .get("data")
        .and_then(|d| d.get("subcommands"))
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    assert!(!subcommands.is_empty());
    // A transcript so `capture` has something real to read.
    let transcript = store.join("audit.jsonl");
    std::fs::write(
        &transcript,
        r#"{"type":"user","sessionId":"audit","message":{"role":"user","content":"MEMORY fact: audit probe"}}"#,
    )
    .unwrap();
    let tpath = transcript.to_str().unwrap();
    for sub in &subcommands {
        // Safe args per verb. Some verbs (sync/hook with no target) legitimately
        // return a usage error; the robot CONTRACT is that they still emit
        // exactly one valid JSON envelope, which is what we assert.
        let safe_args: Vec<&str> = match sub.as_str() {
            "setup" => vec!["setup"], // local-only (HOME is the temp store)
            "remember" => vec!["remember", "--type", "fact", "robot audit memory"],
            "recall" => vec!["recall", "sync branch"],
            "list" => vec!["list"],
            "capture" => vec!["capture", tpath],
            "sync" => vec!["sync"],
            "hook" => vec!["hook", "status"],
            other => panic!("new subcommand '{other}' has no audit args — add them"),
        };
        let mut args = safe_args.clone();
        args.push("--json");
        let o = ghostie(&store, &args);
        let out = stdout(&o);
        assert_eq!(
            out.lines().count(),
            1,
            "{sub}: exactly one JSON line on stdout, got: {out}"
        );
        let doc = json::parse(out.trim_end())
            .unwrap_or_else(|e| panic!("{sub}: stdout is not JSON: {e}"));
        let ok = doc.get("ok").and_then(Value::as_bool);
        assert!(ok.is_some(), "{sub}: envelope has an ok flag");
        assert_eq!(
            doc.get("command").and_then(Value::as_str),
            Some(sub.as_str()),
            "{sub}"
        );
        // Success carries data; a usage error carries error. Either is a valid
        // envelope; both always carry warnings.
        if ok == Some(true) {
            assert!(doc.get("data").is_some(), "{sub}: ok envelope has data");
        } else {
            assert!(
                doc.get("error").is_some(),
                "{sub}: error envelope has error"
            );
        }
        assert!(
            doc.get("warnings").is_some(),
            "{sub}: envelope has warnings"
        );
        assert!(
            !stderr(&o).contains('{'),
            "{sub}: no JSON on stderr: {}",
            stderr(&o)
        );
    }
    let _ = std::fs::remove_dir_all(store);
}

// ---------- process-level byte-stability ----------

#[test]
fn recall_and_list_stdout_bytes_identical_across_runs() {
    let store = temp_store("stable");
    seed_five(&store);
    for args in [
        vec!["recall", "sync branch", "--json"],
        vec!["recall", "which branch do we sync to"],
        vec!["list", "--json"],
        vec!["list"],
    ] {
        let a = ghostie(&store, &args);
        let b = ghostie(&store, &args);
        assert_eq!(
            stdout(&a),
            stdout(&b),
            "{args:?}: stdout must be byte-identical across runs"
        );
    }
    let _ = std::fs::remove_dir_all(store);
}

// ---------- exit codes ----------

#[test]
fn exit_codes_match_the_contract() {
    let store = temp_store("exits");
    // 0: success.
    let o = ghostie(&store, &["list"]);
    assert_eq!(o.status.code(), Some(0));
    // 2: usage errors.
    for args in [
        vec!["frobnicate"],
        vec!["recall"],
        vec!["recall", "two", "words"],
        vec!["remember", "--type", "opinion", "t"],
        vec!["list", "--wat"],
    ] {
        let o = ghostie(&store, &args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
    }
    // 1: operational failure — store root under a regular FILE.
    let blocker = std::env::temp_dir().join(format!("ghostie-e2e-blocker-{}", std::process::id()));
    std::fs::write(&blocker, "i am a file").unwrap();
    let bad_store = blocker.join("sub");
    let o = ghostie(&bad_store, &["remember", "--type", "fact", "t"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    // Robot mode still emits the error envelope AND exits non-zero.
    let o = ghostie(&bad_store, &["remember", "--type", "fact", "t", "--json"]);
    assert_eq!(o.status.code(), Some(1));
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    assert_eq!(doc.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        doc.get("error")
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str),
        Some("io")
    );
    let _ = std::fs::remove_file(blocker);
    let _ = std::fs::remove_dir_all(store);
}

// ---------- stdin body ----------

#[test]
fn remember_body_from_stdin_via_pipe() {
    let store = temp_store("stdin");
    let o = ghostie_stdin(
        &store,
        &[
            "remember",
            "--type",
            "fact",
            "piped body",
            "--body",
            "-",
            "--json",
        ],
        "first line\nsecond line\n",
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let text = std::fs::read_to_string(store.join("memories/fact-piped-body-1.md")).unwrap();
    assert!(text.ends_with("---\nfirst line\nsecond line\n"), "{text}");
    let _ = std::fs::remove_dir_all(store);
}

// ---------- env precedence ----------

#[test]
fn store_flag_beats_ghostie_home_env() {
    let flag_store = temp_store("flagwins");
    let env_store = temp_store("envloses");
    let o = Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", CLOCK)
        .env("GHOSTIE_HOME", &env_store)
        .args(["--store"])
        .arg(&flag_store)
        .args(["remember", "--type", "fact", "flag wins"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(flag_store.join("memories/fact-flag-wins-1.md").exists());
    assert!(!env_store.exists(), "env store must be untouched");
    // Without the flag, GHOSTIE_HOME is used.
    let o = Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", CLOCK)
        .env("GHOSTIE_HOME", &env_store)
        .args(["remember", "--type", "fact", "env used"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(env_store.join("memories/fact-env-used-1.md").exists());
    let _ = std::fs::remove_dir_all(flag_store);
    let _ = std::fs::remove_dir_all(env_store);
}

// ---------- test-clock hook ----------

#[test]
fn malformed_test_clock_is_a_hard_error_not_wall_time() {
    let store = temp_store("badclock");
    let o = Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", "yesterday-ish")
        .arg("--store")
        .arg(&store)
        .args(["remember", "--type", "fact", "t"])
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(1),
        "silent fallback to wall time would break byte-stability mysteriously"
    );
    assert!(
        stderr(&o).contains("timestamp") || stderr(&o).contains("GHOSTIE"),
        "{}",
        stderr(&o)
    );
    let _ = std::fs::remove_dir_all(store);
}
