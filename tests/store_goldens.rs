//! Store round-trip + byte-stability golden suite (bead ghostie-rs-zya.2.6).
//! These are the tests the gate's byte-stability step leans on; the
//! goldens under tests/fixtures/store/ ARE canonical form (documented as
//! normative examples in docs/FORMAT.md) — any codec drift fails loudly.

use ghostie::store::frontmatter;
use ghostie::store::memory::{Memory, MemoryType};
use ghostie::store::{ListFilter, NewMemory, Store};
use ghostie::util::FixedClock;
use std::path::{Path, PathBuf};

const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

fn goldens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/store")
}

fn golden_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(goldens_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 4, "one golden per memory type");
    paths
}

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("ghostie-goldens-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

#[test]
fn goldens_cover_all_four_types() {
    let mut seen = Vec::new();
    for path in golden_paths() {
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = frontmatter::parse(&text, &path.display().to_string()).unwrap();
        let (m, _) = Memory::from_doc(&doc, &path.display().to_string()).unwrap();
        seen.push(m.mtype);
    }
    for t in MemoryType::ALL {
        assert!(seen.contains(&t), "missing golden for {}", t.as_str());
    }
}

#[test]
fn parse_then_serialize_is_byte_identical_to_the_golden() {
    for path in golden_paths() {
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = frontmatter::parse(&text, &path.display().to_string()).unwrap();
        assert_eq!(
            doc.serialize(),
            text,
            "{}: golden is not canonical (codec drift?)",
            path.display()
        );
    }
}

#[test]
fn typed_round_trip_second_bytes_equal_first() {
    for path in golden_paths() {
        let origin = path.display().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        // Memory -> file bytes -> Memory -> file bytes.
        let (m1, _) =
            Memory::from_doc(&frontmatter::parse(&text, &origin).unwrap(), &origin).unwrap();
        let bytes1 = m1.to_doc().serialize();
        let (m2, _) =
            Memory::from_doc(&frontmatter::parse(&bytes1, &origin).unwrap(), &origin).unwrap();
        let bytes2 = m2.to_doc().serialize();
        assert_eq!(m1, m2, "{origin}: parsed structs equal");
        assert_eq!(bytes1, bytes2, "{origin}: second bytes == first bytes");
        assert_eq!(bytes1, text, "{origin}: typed model preserves the golden");
    }
}

#[test]
fn double_serialize_idempotence_over_every_fixture() {
    for path in golden_paths() {
        let origin = path.display().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = frontmatter::parse(&text, &origin).unwrap();
        let once = doc.serialize();
        let twice = frontmatter::parse(&once, &origin).unwrap().serialize();
        assert_eq!(once, twice, "{origin}");
    }
}

#[test]
fn update_noop_is_byte_identical_for_every_golden() {
    let root = temp_root("noop");
    let store = Store::open(&root);
    std::fs::create_dir_all(root.join("memories")).unwrap();
    for path in golden_paths() {
        std::fs::copy(&path, root.join("memories").join(path.file_name().unwrap())).unwrap();
    }
    let (memories, _) = store.list(&ListFilter::default()).unwrap();
    assert_eq!(memories.len(), 4);
    for m in &memories {
        let file = root.join("memories").join(format!("{}.md", m.id));
        let before = std::fs::read(&file).unwrap();
        store.update(m).unwrap();
        let after = std::fs::read(&file).unwrap();
        assert_eq!(before, after, "{}: no-op update changed bytes", m.id);
    }
    let _ = std::fs::remove_dir_all(root);
}

/// The exact check the gate's byte-stability step promotes into
/// scripts/verify.sh: same definitions + same FixedClock -> byte-identical
/// memories/ trees. Kept callable as a plain function so both this test
/// and future helpers reuse the logic.
pub fn build_store_from_definitions(root: &Path) -> Store {
    let store = Store::open(root);
    let clock = FixedClock(T0);
    for (mtype, title, tags, body) in [
        (MemoryType::Fact, "Alpha fact", vec!["a"], "Alpha body.\n"),
        (MemoryType::Rule, "Beta rule", vec!["b", "ci"], ""),
        (
            MemoryType::Decision,
            "Gamma decision",
            vec![],
            "Chose gamma over delta.\n",
        ),
    ] {
        store
            .create(
                &NewMemory {
                    mtype: Some(mtype),
                    title: title.to_string(),
                    tags: tags.iter().map(|s| s.to_string()).collect(),
                    body: body.to_string(),
                    ..NewMemory::default()
                },
                &clock,
            )
            .unwrap();
    }
    store
}

fn tree_bytes(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn same_inputs_same_bytes_across_two_stores() {
    let root_a = temp_root("same-a");
    let root_b = temp_root("same-b");
    build_store_from_definitions(&root_a);
    build_store_from_definitions(&root_b);
    let a = tree_bytes(&root_a.join("memories"));
    let b = tree_bytes(&root_b.join("memories"));
    assert!(!a.is_empty());
    assert_eq!(
        a, b,
        "two stores from the same definitions and clock must be byte-identical trees"
    );
    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

#[test]
fn lf_discipline_no_cr_exactly_one_trailing_newline() {
    // Goldens AND freshly-created files.
    let root = temp_root("lf");
    build_store_from_definitions(&root);
    let mut files: Vec<PathBuf> = golden_paths();
    files.extend(
        std::fs::read_dir(root.join("memories"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path()),
    );
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.contains(&b'\r'), "{}: CR byte found", path.display());
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "{}: must end with newline",
            path.display()
        );
        assert_ne!(
            bytes.get(bytes.len().saturating_sub(2)),
            Some(&b'\n'),
            "{}: exactly ONE trailing newline",
            path.display()
        );
        // No trailing whitespace on any line.
        let text = String::from_utf8(bytes).unwrap();
        for (i, line) in text.lines().enumerate() {
            assert_eq!(
                line,
                line.trim_end(),
                "{}:{}: trailing whitespace",
                path.display(),
                i + 1
            );
        }
    }
    let _ = std::fs::remove_dir_all(root);
}
