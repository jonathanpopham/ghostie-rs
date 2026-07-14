//! Provenance e2e (bead ghostie-rs-l2g): drive the REAL compiled binary the
//! way an agent or a CI gate will. The clean Lockstep-lineage story, end to
//! end at the CLI boundary: every write leaves a hash-chained record, the
//! chain replays to INTACT, `provenance <id>` shows a memory's origin, and a
//! single silent edit is caught with a non-zero exit and the exact broken seq.

use ghostie::json::{self, Value};
use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_ghostie");
const CLOCK: &str = "2026-07-13T12:00:00Z";

fn temp_store(label: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("ghostie-prov-e2e-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn ghostie(store: &PathBuf, args: &[&str]) -> Output {
    Command::new(BIN)
        .env("GHOSTIE_TEST_CLOCK", CLOCK)
        .env_remove("GHOSTIE_HOME")
        .env("HOME", store)
        .arg("--store")
        .arg(store)
        .args(args)
        .output()
        .expect("binary runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn remember(store: &PathBuf, ty: &str, title: &str, body: &str) {
    let o = ghostie(store, &["remember", "--type", ty, title, "--body", body]);
    assert!(o.status.success(), "remember failed");
}

#[test]
fn every_write_records_lineage_and_verify_is_intact() {
    let store = temp_store("intact");
    remember(
        &store,
        "fact",
        "Configs live in etc",
        "All configs live in etc/.",
    );
    remember(
        &store,
        "rule",
        "Run verify before commit",
        "The gate is verify.sh.",
    );

    // A provenance record exists for a memory, tagged `created`.
    let o = ghostie(
        &store,
        &["provenance", "fact-configs-live-in-etc-1", "--json"],
    );
    assert!(o.status.success());
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    let entries = doc
        .get("data")
        .and_then(|d| d.get("entries"))
        .and_then(Value::as_array)
        .unwrap();
    assert_eq!(entries.len(), 1, "one record for a freshly-created memory");
    assert_eq!(
        entries[0].get("event").and_then(Value::as_str),
        Some("created")
    );
    // The chain replays clean.
    let o = ghostie(&store, &["provenance", "verify", "--json"]);
    assert!(o.status.success(), "verify exits 0 when INTACT");
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    assert_eq!(
        doc.get("data")
            .and_then(|d| d.get("status"))
            .and_then(Value::as_str),
        Some("intact")
    );
    assert_eq!(
        doc.get("data")
            .and_then(|d| d.get("entries"))
            .and_then(Value::as_i64),
        Some(2)
    );
}

#[test]
fn tampering_with_a_memory_file_makes_verify_fail() {
    let store = temp_store("tamper");
    remember(
        &store,
        "fact",
        "Configs live in etc",
        "All configs live in etc/.",
    );
    remember(
        &store,
        "fact",
        "Artifacts live in dist",
        "Built binaries end up in dist/.",
    );

    // Forge the second memory behind the store's back.
    let p = store
        .join("memories")
        .join("fact-artifacts-live-in-dist-1.md");
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("forged line\n");
    std::fs::write(&p, text).unwrap();

    let o = ghostie(&store, &["provenance", "verify", "--json"]);
    assert!(
        !o.status.success(),
        "verify exits non-zero on a broken chain (a CI gate must fail)"
    );
    let doc = json::parse(stdout(&o).trim_end()).unwrap();
    assert_eq!(doc.get("ok").and_then(Value::as_bool), Some(false));
    let msg = doc
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap();
    assert!(msg.contains("BROKEN"), "message names the break: {msg}");
    assert!(msg.contains("seq 2"), "and the exact record: {msg}");
}
