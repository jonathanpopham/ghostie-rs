//! Hand-editability + index derivability suite (bead ghostie-rs-zya.2.7).
//! "Plain files you own and can read" is only true if a human editing
//! with vim can't brick the store. Table-driven; failures print the
//! mutated input.

use ghostie::recall::{RecallOpts, recall};
use ghostie::store::frontmatter;
use ghostie::store::index::{Index, index_path};
use ghostie::store::memory::Memory;
use ghostie::store::{ListFilter, NewMemory, Store};
use ghostie::util::FixedClock;
use std::path::PathBuf;

const T0: i64 = 1_783_944_000;

const CANONICAL: &str = "---\nid: fact-editable-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: An editable fact\ntags: [alpha, beta]\n---\nOriginal body line.\n";

fn temp_store(label: &str) -> (PathBuf, Store) {
    let root =
        std::env::temp_dir().join(format!("ghostie-handedit-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("memories")).unwrap();
    let store = Store::open(&root);
    (root, store)
}

#[test]
fn human_edit_tolerance_matrix() {
    // Each case: (name, mutation of the canonical text). Parse must
    // succeed and the meaning must be preserved.
    let cases: Vec<(&str, String)> = vec![
        (
            "reordered keys",
            "---\ntitle: An editable fact\nid: fact-editable-1\ntags: [alpha, beta]\ntype: fact\ncreated: 2026-07-13T12:00:00Z\n---\nOriginal body line.\n"
                .to_string(),
        ),
        ("crlf line endings", CANONICAL.replace('\n', "\r\n")),
        (
            "trailing whitespace",
            CANONICAL.replace("type: fact\n", "type: fact   \n"),
        ),
        (
            "extra blank lines around delimiters",
            CANONICAL.replace("---\nid:", "---\n\nid:").replace(
                "tags: [alpha, beta]\n---",
                "tags: [alpha, beta]\n\n---",
            ),
        ),
        (
            "missing final newline",
            CANONICAL.trim_end().to_string(),
        ),
        (
            "added unknown key",
            CANONICAL.replace("---\nOriginal", "mood: curious\n---\nOriginal"),
        ),
        (
            "edited body",
            CANONICAL.replace("Original body line.", "A human rewrote this."),
        ),
        (
            "edited tags list",
            CANONICAL.replace("[alpha, beta]", "[alpha, beta, gamma]"),
        ),
    ];
    for (name, mutated) in cases {
        let doc = frontmatter::parse(&mutated, "<edit>")
            .unwrap_or_else(|e| panic!("{name}: parse failed: {e}\ninput: {mutated:?}"));
        let (m, _) = Memory::from_doc(&doc, "<edit>")
            .unwrap_or_else(|e| panic!("{name}: validation failed: {e}\ninput: {mutated:?}"));
        assert_eq!(m.id, "fact-editable-1", "{name}: id preserved");
        assert_eq!(m.title, "An editable fact", "{name}: title preserved");
        assert!(
            m.tags
                .starts_with(&["alpha".to_string(), "beta".to_string()]),
            "{name}"
        );
    }
}

#[test]
fn canonicalization_preserves_the_humans_changes() {
    let (root, store) = temp_store("canon");
    let path = root.join("memories/fact-editable-1.md");
    // A hand-edit with CRLF, reordered keys, an added key and a new tag.
    let edited = "---\r\ntitle: An editable fact\r\nmood: curious\r\nid: fact-editable-1\r\ntype: fact\r\ncreated: 2026-07-13T12:00:00Z\r\ntags: [alpha, beta, gamma]\r\n---\r\nA human rewrote this.\r\n";
    std::fs::write(&path, edited).unwrap();
    let (m, _) = store.read("fact-editable-1").unwrap();
    store.update(&m).unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after,
        "---\nid: fact-editable-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: An editable fact\ntags: [alpha, beta, gamma]\nmood: curious\n---\nA human rewrote this.\n",
        "canonical bytes with every human change intact, unknown key in spec position"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn destructive_edits_error_with_file_and_line_and_store_continues() {
    let (root, store) = temp_store("destructive");
    // One good memory.
    store
        .create(
            &NewMemory {
                mtype: Some(ghostie::store::memory::MemoryType::Fact),
                title: "survivor".to_string(),
                body: "healthy sync content\n".to_string(),
                ..NewMemory::default()
            },
            &FixedClock(T0),
        )
        .unwrap();
    let cases: Vec<(&str, &str, &str)> = vec![
        (
            "unclosed delimiter",
            "---\nid: fact-broken-1\ntype: fact\n",
            "unclosed",
        ),
        (
            "duplicate key",
            "---\nid: fact-broken-1\nid: fact-double-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\n",
            "duplicate key 'id'",
        ),
        (
            "invalid type",
            "---\nid: fact-broken-1\ntype: opinion\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\n",
            "unknown type 'opinion'",
        ),
    ];
    for (name, content, needle) in cases {
        let path = root.join("memories/fact-broken-1.md");
        std::fs::write(&path, content).unwrap();
        let e = store.read_path(&path).unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("fact-broken-1.md"),
            "{name}: file named in {msg}"
        );
        assert!(msg.contains(needle), "{name}: expected {needle:?} in {msg}");
        // list continues over the remaining files with a warning.
        let (memories, warnings) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(memories.len(), 1, "{name}: survivor still listed");
        assert_eq!(warnings.len(), 1, "{name}");
        // recall continues too.
        let r = recall(&store, "healthy sync content", &RecallOpts::default()).unwrap();
        assert_eq!(r.hits.len(), 1, "{name}: recall not held hostage");
        assert!(!r.warnings.is_empty(), "{name}: warning surfaced");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn git_conflict_markers_are_reported_as_conflicted() {
    let (root, store) = temp_store("conflict");
    let path = root.join("memories/fact-conflicted-1.md");
    std::fs::write(
        &path,
        "<<<<<<< HEAD\n---\nid: fact-conflicted-1\n=======\n---\nid: fact-conflicted-2\n>>>>>>> sync\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\n",
    )
    .unwrap();
    let e = store.read_path(&path).unwrap_err();
    assert!(
        e.to_string().contains("conflict"),
        "conflicted, not parsed as a weird memory: {e}"
    );
    // A setext '=======' underline alone must NOT trip the detector.
    let setext = "---\nid: fact-setext-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\nHeading\n=======\nbody\n";
    std::fs::write(root.join("memories/fact-setext-1.md"), setext).unwrap();
    assert!(
        store
            .read_path(&root.join("memories/fact-setext-1.md"))
            .is_ok()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn index_stays_fresh_under_out_of_band_edits() {
    let (root, store) = temp_store("idx-fresh");
    store
        .create(
            &NewMemory {
                mtype: Some(ghostie::store::memory::MemoryType::Fact),
                title: "editable".to_string(),
                body: "original searchable text\n".to_string(),
                ..NewMemory::default()
            },
            &FixedClock(T0),
        )
        .unwrap();
    // Materialize the index, then hand-edit the file out-of-band.
    let _ = Index::ensure_fresh(&store).unwrap();
    let path = root.join("memories/fact-editable-1.md");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("original searchable", "replacement findable"),
    )
    .unwrap();
    // Next recall serves fresh data (hash detected the change).
    let r = recall(&store, "replacement findable", &RecallOpts::default()).unwrap();
    assert_eq!(r.hits.len(), 1, "stale index would have missed this");
    let r_old = recall(&store, "original searchable", &RecallOpts::default()).unwrap();
    assert!(r_old.hits.is_empty(), "old tokens must be gone");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn index_is_never_authoritative_delete_and_rebuild_identical() {
    let (root, store) = temp_store("idx-derivable");
    for title in ["one", "two", "three"] {
        store
            .create(
                &NewMemory {
                    mtype: Some(ghostie::store::memory::MemoryType::Rule),
                    title: format!("rule {title}"),
                    body: format!("body {title}\n"),
                    ..NewMemory::default()
                },
                &FixedClock(T0),
            )
            .unwrap();
    }
    let (_, _) = Index::ensure_fresh(&store).unwrap();
    let before_index = std::fs::read(index_path(&root)).unwrap();
    let before_list = {
        let (m, _) = store.list(&ListFilter::default()).unwrap();
        m
    };
    let before_recall = recall(&store, "rule two", &RecallOpts::default())
        .unwrap()
        .to_json()
        .emit();
    // rm -rf .index/
    std::fs::remove_dir_all(root.join(".index")).unwrap();
    let (after_list, _) = store.list(&ListFilter::default()).unwrap();
    assert_eq!(before_list, after_list, "list identical without the index");
    let after_recall = recall(&store, "rule two", &RecallOpts::default())
        .unwrap()
        .to_json()
        .emit();
    assert_eq!(before_recall, after_recall, "recall identical (rebuilt)");
    let rebuilt_index = std::fs::read(index_path(&root)).unwrap();
    assert_eq!(
        before_index, rebuilt_index,
        "rebuilt index bytes match the pre-deletion index bytes"
    );
    let _ = std::fs::remove_dir_all(root);
}
