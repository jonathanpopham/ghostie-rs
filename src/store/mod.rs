//! store — the on-disk memory format and CRUD.
//!
//! Single responsibility: one memory = one plain Markdown file with typed
//! frontmatter under `<root>/memories/`, readable and hand-editable by the
//! user. Serialization is canonical and byte-stable; the derivable index
//! under `<root>/.index/` is an optimization and never authoritative over
//! the files. Specified by `docs/FORMAT.md`.
//!
//! The [`Store`] operates ONLY on files — it never consults the index for
//! correctness (the index is a read optimization owned by `index.rs` and is
//! never authoritative). All writes are atomic: canonical bytes go to a
//! dot-prefixed temp file in the same directory, then rename into place, so
//! a partially-written memory can never be observed, even on crash.

pub mod frontmatter;
pub mod memory;

use crate::error::{Error, Result, Warning};
use crate::store::memory::{Memory, MemoryType};
use crate::util::Clock;
use std::path::{Path, PathBuf};

/// Fields for a new memory; the store assigns `id` and `created`.
#[derive(Debug, Clone, Default)]
pub struct NewMemory {
    /// The memory type (defaults to fact via `Default` only in tests;
    /// callers set it explicitly).
    pub mtype: Option<MemoryType>,
    /// One-line title (required, non-empty).
    pub title: String,
    /// Tags, order preserved.
    pub tags: Vec<String>,
    /// Ids of related memories.
    pub links: Vec<String>,
    /// Capture provenance (session-summary).
    pub source: Option<String>,
    /// Superseded decision id (decision).
    pub supersedes: Option<String>,
    /// Markdown body (may be empty; stored verbatim modulo LF).
    pub body: String,
}

/// Filters for [`Store::list`]. Empty filter = everything.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only this type.
    pub mtype: Option<MemoryType>,
    /// Only memories carrying this tag.
    pub tag: Option<String>,
}

/// A memory store bound to a root directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Bind to a root directory. No I/O happens until an operation runs;
    /// write operations create `<root>/memories/` on demand.
    pub fn open(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/memories/`.
    pub fn memories_dir(&self) -> PathBuf {
        self.root.join("memories")
    }

    fn memory_path(&self, id: &str) -> PathBuf {
        self.memories_dir().join(format!("{id}.md"))
    }

    /// Create a memory: assign the id per docs/FORMAT.md (`<type>-<slug>-
    /// <n>`, lowest free integer scanned from the filesystem), stamp
    /// `created` from the injected clock, write canonical bytes atomically.
    pub fn create(&self, new: &NewMemory, clock: &dyn Clock) -> Result<Memory> {
        let mtype = new.mtype.ok_or_else(|| Error::Usage {
            message: "memory type is required".to_string(),
        })?;
        if new.title.trim().is_empty() {
            return Err(Error::Usage {
                message: "title must not be empty".to_string(),
            });
        }
        let slug = slugify(&new.title);
        let dir = self.memories_dir();
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io {
            context: "creating memories directory".to_string(),
            path: dir.display().to_string(),
            source: e,
        })?;
        // Lowest free disambiguator, scanned deterministically from files.
        let mut n: u64 = 1;
        loop {
            let candidate = format!("{}-{}-{}", mtype.as_str(), slug, n);
            if !self.memory_path(&candidate).exists() {
                break;
            }
            n += 1;
        }
        let id = format!("{}-{}-{}", mtype.as_str(), slug, n);
        let memory = Memory {
            id: id.clone(),
            mtype,
            created: clock.now_epoch_seconds(),
            title: new.title.clone(),
            tags: new.tags.clone(),
            links: new.links.clone(),
            source: new.source.clone(),
            supersedes: new.supersedes.clone(),
            unknown_keys: Vec::new(),
            body: new.body.clone(),
        };
        self.write_atomic(&memory)?;
        self.refresh_index_best_effort();
        Ok(memory)
    }

    /// Keep the derivable index warm after a write, best-effort. Failures
    /// are swallowed by design: reads verify freshness by content hash, so
    /// a stale or missing index can never change results — only speed.
    /// Skipped when no index exists yet (recall builds it lazily).
    fn refresh_index_best_effort(&self) {
        if crate::store::index::index_path(&self.root).exists() {
            let _ = crate::store::index::Index::ensure_fresh(self);
        }
    }

    /// Read a memory by id.
    pub fn read(&self, id: &str) -> Result<(Memory, Vec<Warning>)> {
        self.read_path(&self.memory_path(id))
    }

    /// Read and validate one memory file. Detects git conflict markers
    /// before parsing (a conflicted file is reported as such, not parsed
    /// as a weird memory). Warns when the frontmatter id disagrees with
    /// the filename; the frontmatter id wins.
    pub fn read_path(&self, path: &Path) -> Result<(Memory, Vec<Warning>)> {
        let origin = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            context: "reading memory file".to_string(),
            path: origin.clone(),
            source: e,
        })?;
        if is_conflicted(&text) {
            return Err(Error::Invalid {
                origin,
                message: "file contains git conflict markers (<<<<<<< / >>>>>>>); \
                          resolve the conflict before ghostie can read it"
                    .to_string(),
            });
        }
        let doc = frontmatter::parse(&text, &origin)?;
        let (memory, mut warnings) = Memory::from_doc(&doc, &origin)?;
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && stem != memory.id
        {
            warnings.push(Warning {
                origin: origin.clone(),
                message: format!(
                    "frontmatter id '{}' disagrees with filename '{stem}.md'; the frontmatter id wins",
                    memory.id
                ),
            });
        }
        Ok((memory, warnings))
    }

    /// Rewrite a memory in canonical form, preserving `created`, unknown
    /// keys and the id (files never rename). A no-op update is
    /// byte-identical.
    pub fn update(&self, memory: &Memory) -> Result<()> {
        let path = self.memory_path(&memory.id);
        if !path.exists() {
            return Err(Error::Invalid {
                origin: path.display().to_string(),
                message: format!("cannot update '{}': no such memory", memory.id),
            });
        }
        self.write_atomic(memory)?;
        self.refresh_index_best_effort();
        Ok(())
    }

    /// Remove a memory file. No tombstones — git history is the tombstone.
    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.memory_path(id);
        std::fs::remove_file(&path).map_err(|e| Error::Io {
            context: format!("deleting memory '{id}'"),
            path: path.display().to_string(),
            source: e,
        })?;
        self.refresh_index_best_effort();
        Ok(())
    }

    /// All memories in deterministic order (id lexicographic ascending),
    /// optionally filtered. Unreadable/invalid files become warnings, and
    /// the listing continues — one typo must not take the store down.
    /// Non-`.md` files and dotfiles are ignored.
    pub fn list(&self, filter: &ListFilter) -> Result<(Vec<Memory>, Vec<Warning>)> {
        let dir = self.memories_dir();
        let mut warnings = Vec::new();
        let mut memories = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A store with no memories/ yet is an empty store, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((memories, warnings));
            }
            Err(e) => {
                return Err(Error::Io {
                    context: "listing memories directory".to_string(),
                    path: dir.display().to_string(),
                    source: e,
                });
            }
        };
        // Directory iteration order is OS-dependent: ALWAYS collect + sort.
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::Io {
                context: "reading directory entry".to_string(),
                path: dir.display().to_string(),
                source: e,
            })?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || !name.ends_with(".md") || !path.is_file() {
                continue;
            }
            paths.push(path);
        }
        paths.sort();
        for path in paths {
            match self.read_path(&path) {
                Ok((memory, mut w)) => {
                    warnings.append(&mut w);
                    if let Some(t) = filter.mtype
                        && memory.mtype != t
                    {
                        continue;
                    }
                    if let Some(tag) = &filter.tag
                        && !memory.tags.iter().any(|x| x == tag)
                    {
                        continue;
                    }
                    memories.push(memory);
                }
                Err(e) => warnings.push(Warning {
                    origin: path.display().to_string(),
                    message: format!("skipped: {e}"),
                }),
            }
        }
        // Paths were filename-sorted; memory ids may disagree with
        // filenames (hand edits) — sort again by id for the output order.
        memories.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((memories, warnings))
    }

    /// Canonical bytes -> dot-prefixed temp file -> rename into place.
    /// Rename within one directory is atomic on POSIX; readers can never
    /// see a partial file, and dotfiles are ignored by `list`.
    fn write_atomic(&self, memory: &Memory) -> Result<()> {
        let path = self.memory_path(&memory.id);
        let dir = self.memories_dir();
        std::fs::create_dir_all(&dir).map_err(|e| Error::Io {
            context: "creating memories directory".to_string(),
            path: dir.display().to_string(),
            source: e,
        })?;
        let bytes = memory.to_doc().serialize();
        let tmp = dir.join(format!(".tmp-{}.md", memory.id));
        std::fs::write(&tmp, bytes.as_bytes()).map_err(|e| Error::Io {
            context: "writing memory file".to_string(),
            path: tmp.display().to_string(),
            source: e,
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io {
            context: "renaming memory file into place".to_string(),
            path: path.display().to_string(),
            source: e,
        })
    }
}

/// Slug per docs/FORMAT.md: lowercase ASCII alnum, everything else
/// collapsed to single hyphens, trimmed, capped at 40 chars (never ending
/// in a hyphen); an all-symbol title becomes `untitled`.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_hyphen = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !slug.is_empty() {
                slug.push('-');
            }
            pending_hyphen = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let mut slug: String = slug.chars().take(40).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug = "untitled".to_string();
    }
    slug
}

/// Git conflict markers: a line starting `<<<<<<<` AND one starting
/// `>>>>>>>`. (`=======` alone is a legal Markdown setext underline, so it
/// does not count by itself.)
fn is_conflicted(text: &str) -> bool {
    let mut has_open = false;
    let mut has_close = false;
    for line in text.lines() {
        if line.starts_with("<<<<<<<") {
            has_open = true;
        } else if line.starts_with(">>>>>>>") {
            has_close = true;
        }
    }
    has_open && has_close
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temp dir removed on drop. std-only (no tempfile crate).
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "ghostie-test-{}-{}-{n}",
                std::process::id(),
                label
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::TempDir;
    use super::*;
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000; // 2026-07-13T12:00:00Z

    fn new_fact(title: &str) -> NewMemory {
        NewMemory {
            mtype: Some(MemoryType::Fact),
            title: title.to_string(),
            body: "body text\n".to_string(),
            ..NewMemory::default()
        }
    }

    #[test]
    fn create_read_round_trip() {
        let tmp = TempDir::new("crud");
        let store = Store::open(tmp.path());
        let clock = FixedClock(T0);
        let m = store
            .create(&new_fact("Configs live in etc"), &clock)
            .unwrap();
        assert_eq!(m.id, "fact-configs-live-in-etc-1");
        let (back, warnings) = store.read(&m.id).unwrap();
        assert_eq!(back, m);
        assert!(warnings.is_empty());
    }

    #[test]
    fn created_comes_from_the_injected_clock() {
        let tmp = TempDir::new("clock");
        let store = Store::open(tmp.path());
        let m = store
            .create(&new_fact("clocked"), &FixedClock(12345))
            .unwrap();
        assert_eq!(m.created, 12345, "no wall clock may leak into create");
        let text =
            std::fs::read_to_string(store.memories_dir().join(format!("{}.md", m.id))).unwrap();
        assert!(
            text.contains("created: 1970-01-01T03:25:45Z"),
            "stamped from clock: {text}"
        );
    }

    #[test]
    fn id_collision_appends_lowest_free_integer() {
        let tmp = TempDir::new("collide");
        let store = Store::open(tmp.path());
        let clock = FixedClock(T0);
        let a = store.create(&new_fact("Same Title"), &clock).unwrap();
        let b = store.create(&new_fact("Same Title"), &clock).unwrap();
        let c = store.create(&new_fact("Same  Title!!"), &clock).unwrap();
        assert_eq!(a.id, "fact-same-title-1");
        assert_eq!(b.id, "fact-same-title-2");
        assert_eq!(c.id, "fact-same-title-3", "slug collapses symbols");
        // Delete the middle one; the gap is refilled deterministically.
        store.delete(&b.id).unwrap();
        let d = store.create(&new_fact("Same Title"), &clock).unwrap();
        assert_eq!(d.id, "fact-same-title-2", "lowest free integer");
    }

    #[test]
    fn update_noop_is_byte_identical() {
        let tmp = TempDir::new("noop");
        let store = Store::open(tmp.path());
        let m = store.create(&new_fact("stable"), &FixedClock(T0)).unwrap();
        let path = store.memories_dir().join(format!("{}.md", m.id));
        let before = std::fs::read(&path).unwrap();
        let (read_back, _) = store.read(&m.id).unwrap();
        store.update(&read_back).unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "no-op update must not change a byte");
    }

    #[test]
    fn update_preserves_unknown_keys_and_created() {
        let tmp = TempDir::new("preserve");
        let store = Store::open(tmp.path());
        let m = store.create(&new_fact("keeper"), &FixedClock(T0)).unwrap();
        let path = store.memories_dir().join(format!("{}.md", m.id));
        // Human adds a key by hand.
        let text = std::fs::read_to_string(&path).unwrap();
        let edited = text.replace("---\nbody", "priority: high\n---\nbody");
        std::fs::write(&path, edited).unwrap();
        // Update through the model.
        let (mut read_back, _) = store.read(&m.id).unwrap();
        read_back.title = "keeper (edited)".to_string();
        store.update(&read_back).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("priority: high"),
            "unknown key kept: {after}"
        );
        assert!(after.contains("created: 2026-07-13T12:00:00Z"), "{after}");
        assert!(after.contains("title: keeper (edited)"), "{after}");
        assert!(
            after.contains("id: fact-keeper-1"),
            "id never changes: {after}"
        );
    }

    #[test]
    fn delete_removes_the_file() {
        let tmp = TempDir::new("delete");
        let store = Store::open(tmp.path());
        let m = store.create(&new_fact("gone"), &FixedClock(T0)).unwrap();
        store.delete(&m.id).unwrap();
        assert!(store.read(&m.id).is_err());
        assert!(store.delete(&m.id).is_err(), "double delete errors");
    }

    #[test]
    fn list_is_deterministic_regardless_of_creation_order() {
        let tmp = TempDir::new("listorder");
        let store = Store::open(tmp.path());
        let clock = FixedClock(T0);
        // Shuffled creation order.
        for title in ["zebra", "apple", "mango", "banana"] {
            store.create(&new_fact(title), &clock).unwrap();
        }
        let (memories, warnings) = store.list(&ListFilter::default()).unwrap();
        let ids: Vec<&str> = memories.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "fact-apple-1",
                "fact-banana-1",
                "fact-mango-1",
                "fact-zebra-1"
            ],
            "id lexicographic ascending"
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn list_filters_by_type_and_tag() {
        let tmp = TempDir::new("filter");
        let store = Store::open(tmp.path());
        let clock = FixedClock(T0);
        store.create(&new_fact("a fact"), &clock).unwrap();
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Rule),
                    title: "a rule".to_string(),
                    tags: vec!["ci".to_string()],
                    ..NewMemory::default()
                },
                &clock,
            )
            .unwrap();
        let (rules, _) = store
            .list(&ListFilter {
                mtype: Some(MemoryType::Rule),
                tag: None,
            })
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "rule-a-rule-1");
        let (tagged, _) = store
            .list(&ListFilter {
                mtype: None,
                tag: Some("ci".to_string()),
            })
            .unwrap();
        assert_eq!(tagged.len(), 1);
        let (none, _) = store
            .list(&ListFilter {
                mtype: Some(MemoryType::Fact),
                tag: Some("ci".to_string()),
            })
            .unwrap();
        assert!(none.is_empty(), "filters compose");
    }

    #[test]
    fn corrupt_file_warns_and_listing_continues() {
        let tmp = TempDir::new("corrupt");
        let store = Store::open(tmp.path());
        store.create(&new_fact("good"), &FixedClock(T0)).unwrap();
        std::fs::write(store.memories_dir().join("broken.md"), "no frontmatter").unwrap();
        let (memories, warnings) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(memories.len(), 1, "good memory still listed");
        assert_eq!(warnings.len(), 1, "bad file warned");
        assert!(warnings[0].origin.contains("broken.md"));
    }

    #[test]
    fn non_md_and_dotfiles_are_ignored() {
        let tmp = TempDir::new("ignore");
        let store = Store::open(tmp.path());
        store
            .create(&new_fact("only one"), &FixedClock(T0))
            .unwrap();
        std::fs::write(store.memories_dir().join("notes.txt"), "human notes").unwrap();
        std::fs::write(store.memories_dir().join(".fact-x-1.md.swp"), "vim swap").unwrap();
        let (memories, warnings) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(memories.len(), 1);
        assert!(warnings.is_empty(), "ignored silently: {warnings:?}");
    }

    #[test]
    fn no_tmp_files_left_behind() {
        let tmp = TempDir::new("atomic");
        let store = Store::open(tmp.path());
        store.create(&new_fact("tidy"), &FixedClock(T0)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(store.memories_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
    }

    #[test]
    fn conflicted_file_is_reported_not_parsed() {
        let tmp = TempDir::new("conflict");
        let store = Store::open(tmp.path());
        store.create(&new_fact("ok"), &FixedClock(T0)).unwrap();
        let conflicted = "<<<<<<< HEAD\n---\nid: fact-x-1\n=======\n---\nid: fact-y-1\n>>>>>>> sync\ntype: fact\n";
        std::fs::write(store.memories_dir().join("fact-x-1.md"), conflicted).unwrap();
        let e = store
            .read_path(&store.memories_dir().join("fact-x-1.md"))
            .unwrap_err();
        assert!(e.to_string().contains("conflict"), "{e}");
        // A setext heading (=== underline) alone is NOT a conflict.
        let (m, _) = store.read("fact-ok-1").unwrap();
        let mut with_setext = m.clone();
        with_setext.body = "Heading\n=======\nbody\n".to_string();
        store.update(&with_setext).unwrap();
        assert!(store.read("fact-ok-1").is_ok(), "setext body reads fine");
    }

    #[test]
    fn slugify_rules() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  spaces   everywhere  "), "spaces-everywhere");
        assert_eq!(slugify("ünïcödé is stripped"), "n-c-d-is-stripped");
        assert_eq!(slugify("!!!"), "untitled");
        assert_eq!(slugify(""), "untitled");
        let long = slugify(&"very long title word ".repeat(10));
        assert!(long.len() <= 40, "capped: {long} ({} chars)", long.len());
        assert!(!long.ends_with('-'), "never ends in hyphen: {long}");
        assert_eq!(slugify("CamelCase123"), "camelcase123");
    }

    #[test]
    fn empty_title_is_a_usage_error() {
        let tmp = TempDir::new("usage");
        let store = Store::open(tmp.path());
        let e = store.create(&new_fact("   "), &FixedClock(T0)).unwrap_err();
        assert!(matches!(e, Error::Usage { .. }), "{e}");
    }
}
pub mod index;
