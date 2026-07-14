//! sync — cross-device sync via the user's own git remote.
//!
//! Single responsibility: commit / fetch / rebase / push of the store by
//! shelling to the system `git` binary (an external *tool*, present on every
//! developer machine, NOT a crate dependency, so crate-level zero-dependency
//! holds). This mirrors how Lockstep shells to POSIX `kill`: the OS's own
//! utilities are fair game; the toolbox stays std-only.
//!
//! Conflicts are detected and reported, never auto-resolved: a rebase that
//! collides is aborted cleanly and surfaced as [`Error::Conflict`] (exit code
//! 3), leaving the working tree exactly as it was for the human to resolve.
//! The derived `.index/` is never synced (it is rebuildable and would only
//! manufacture conflicts).

use crate::crypt;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::util::{Clock, format_rfc3339_utc};
use std::path::Path;
use std::process::Command;

/// Ignore rules for the PLAINTEXT store repo: the rebuildable index, the
/// unapproved review candidates, and the ciphertext mirror all stay local and
/// out of the plaintext remote.
const STORE_IGNORE: [&str; 3] = [".index/", ".pending/", ".enc/"];

/// The result of a successful sync, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Local changes were committed this run.
    pub committed: bool,
    /// Remote changes were rebased in.
    pub pulled: bool,
    /// The commit(s) were pushed.
    pub pushed: bool,
    /// The branch synced.
    pub branch: String,
}

/// Captured output of one `git` invocation.
struct GitRun {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `git -C <root> <args>`. `Err` only when git cannot be launched at all
/// (not installed / not on PATH); a non-zero git exit is reported via `ok`.
fn git(root: &Path, args: &[&str]) -> Result<GitRun> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| Error::Io {
            context: "running git (is it installed and on PATH?)".to_string(),
            path: "git".to_string(),
            source: e,
        })?;
    Ok(GitRun {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Is a git binary available? (Callers that must degrade gracefully use this;
/// tests skip when git is absent from the CI image.)
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Initialise the store as a git repo pointed at the user's own `remote`.
/// Idempotent: safe to re-run to change the remote. Sets a local identity so
/// commits never fail on a machine with no global git identity, and ignores
/// the derived index, the pending candidates, and the ciphertext mirror.
pub fn sync_init(store: &Store, remote: &str) -> Result<()> {
    git_init_at(store.root(), remote, &STORE_IGNORE)
}

/// Initialise `root` as a git repo pointed at `remote`, writing `ignore` lines
/// into its `.gitignore`. Shared by the plaintext store ([`sync_init`]) and the
/// encrypted mirror ([`sync_encrypted_init`]); the latter passes no ignores so
/// every ciphertext file is committed.
fn git_init_at(root: &Path, remote: &str, ignore: &[&str]) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| Error::Io {
        context: "creating directory for sync".to_string(),
        path: root.display().to_string(),
        source: e,
    })?;
    if !root.join(".git").exists() {
        let r = git(root, &["init"])?;
        if !r.ok {
            return Err(Error::Invalid {
                origin: "sync".to_string(),
                message: format!("git init failed: {}", r.stderr.trim()),
            });
        }
    }
    // Pin the unborn branch to `main` regardless of this machine's
    // init.defaultBranch, so every ghostie store agrees on one branch and a
    // second device never pushes an empty `master` past the remote's `main`.
    if is_unborn(root) {
        let _ = git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    }
    // A local identity, only if the repo has none (never clobber a real one).
    if git(root, &["config", "user.email"])?
        .stdout
        .trim()
        .is_empty()
    {
        let _ = git(root, &["config", "user.email", "ghostie@localhost"]);
        let _ = git(root, &["config", "user.name", "ghostie"]);
    }
    if !ignore.is_empty() {
        ensure_gitignore(root, ignore)?;
    }
    let has_origin = git(root, &["remote"])?
        .stdout
        .lines()
        .any(|l| l.trim() == "origin");
    let r = if has_origin {
        git(root, &["remote", "set-url", "origin", remote])?
    } else {
        git(root, &["remote", "add", "origin", remote])?
    };
    if !r.ok {
        return Err(Error::Invalid {
            origin: "sync".to_string(),
            message: format!("could not set remote: {}", r.stderr.trim()),
        });
    }
    Ok(())
}

/// Ensure `.gitignore` excludes each of `lines` (append any missing, create if
/// absent). These are rebuildable or unapproved artifacts (`.index/`,
/// `.pending/`, `.enc/`), so syncing them only makes conflicts or leaks
/// unapproved candidates.
fn ensure_gitignore(root: &Path, lines: &[&str]) -> Result<()> {
    let path = root.join(".gitignore");
    // Only a genuinely-absent file is treated as empty. A decode/permission
    // error must NOT be silently overwritten (that would drop the user's
    // existing ignore rules and could stage secrets on the next `git add`).
    let existing = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(Error::Io {
                context: "reading existing .gitignore (refusing to overwrite)".to_string(),
                path: path.display().to_string(),
                source: e,
            });
        }
    };
    let mut next = existing.clone();
    let mut changed = false;
    for line in lines {
        if next.lines().any(|l| l.trim() == *line) {
            continue;
        }
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(line);
        next.push('\n');
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    std::fs::write(&path, next).map_err(|e| Error::Io {
        context: "writing .gitignore".to_string(),
        path: path.display().to_string(),
        source: e,
    })
}

/// The current branch, or `main` when HEAD is unborn (fresh repo).
fn current_branch(root: &Path) -> String {
    let r = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    match r {
        Ok(run) if run.ok && run.stdout.trim() != "HEAD" && !run.stdout.trim().is_empty() => {
            run.stdout.trim().to_string()
        }
        _ => "main".to_string(),
    }
}

/// Is HEAD unborn (no commits yet)?
fn is_unborn(root: &Path) -> bool {
    !git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .map(|r| r.ok)
        .unwrap_or(false)
}

/// The branch to sync: the remote's default branch when it advertises one (so
/// two devices agree), otherwise this store's local branch.
fn sync_branch(root: &Path) -> String {
    if let Ok(run) = git(root, &["ls-remote", "--symref", "origin", "HEAD"])
        && run.ok
    {
        for line in run.stdout.lines() {
            if let Some(rest) = line.trim().strip_prefix("ref: refs/heads/")
                && let Some(name) = rest.split_whitespace().next()
            {
                return name.to_string();
            }
        }
    }
    current_branch(root)
}

/// Commit local changes, rebase in anything on the remote, and push. A rebase
/// conflict is aborted and surfaced (exit 3), never auto-resolved.
pub fn sync(store: &Store, clock: &dyn Clock) -> Result<SyncOutcome> {
    if !store.root().join(".git").exists() {
        return Err(Error::Usage {
            message: "store is not initialised for sync; run `ghostie sync --init <remote>` first"
                .to_string(),
        });
    }
    sync_git(store.root(), clock)
}

/// The commit / fetch / rebase / push loop for a git repo rooted at `root`.
/// Shared by the plaintext store ([`sync`]) and the encrypted ciphertext mirror
/// ([`sync_encrypted`]); assumes `root/.git` already exists.
fn sync_git(root: &Path, clock: &dyn Clock) -> Result<SyncOutcome> {
    // Agree on one branch across devices: follow the remote's default branch
    // when it advertises one, and pin our unborn HEAD to it so the first
    // commit lands there rather than on this machine's local default.
    let branch = sync_branch(root);
    if is_unborn(root) {
        let _ = git(
            root,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        );
    }

    // Stage everything, then commit only if something changed.
    let add = git(root, &["add", "-A"])?;
    if !add.ok {
        return Err(Error::Invalid {
            origin: "sync".to_string(),
            message: format!("git add failed: {}", add.stderr.trim()),
        });
    }
    let dirty = !git(root, &["status", "--porcelain"])?
        .stdout
        .trim()
        .is_empty();
    let mut committed = false;
    if dirty {
        let msg = format!(
            "ghostie sync {}",
            format_rfc3339_utc(clock.now_epoch_seconds())
        );
        let c = git(root, &["commit", "-m", &msg])?;
        if !c.ok {
            return Err(Error::Invalid {
                origin: "sync".to_string(),
                message: format!("git commit failed: {}", c.stderr.trim()),
            });
        }
        committed = true;
    }

    // Fetch, then rebase onto the remote branch if it exists. A fresh remote
    // has no such ref yet, which is fine (nothing to pull).
    let _ = git(root, &["fetch", "origin"])?;
    let remote_ref = format!("origin/{branch}");
    let mut pulled = false;
    if git(root, &["rev-parse", "--verify", "--quiet", &remote_ref])?.ok {
        if is_unborn(root) {
            // An empty local repo (nothing to commit, e.g. a fresh device with
            // an empty encrypted mirror) cannot rebase; adopt the remote branch
            // wholesale. There is no local work to conflict with.
            let r = git(root, &["reset", "--hard", &remote_ref])?;
            if !r.ok {
                return Err(Error::Invalid {
                    origin: "sync".to_string(),
                    message: format!("adopting {remote_ref} failed: {}", r.stderr.trim()),
                });
            }
        } else {
            let rb = git(root, &["rebase", &remote_ref])?;
            if !rb.ok {
                let _ = git(root, &["rebase", "--abort"]);
                return Err(Error::Conflict {
                    message: format!(
                        "rebase onto {remote_ref} hit a conflict; the working tree is unchanged. \
                         Resolve in {} and re-run `ghostie sync`.",
                        root.display()
                    ),
                });
            }
        }
        pulled = true;
    }

    // Push (sets upstream on first push).
    let push = git(root, &["push", "-u", "origin", &branch])?;
    if !push.ok {
        return Err(Error::Invalid {
            origin: "sync".to_string(),
            message: format!("git push failed: {}", push.stderr.trim()),
        });
    }

    Ok(SyncOutcome {
        committed,
        pulled,
        pushed: true,
        branch,
    })
}

/// The result of an encrypted sync, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptOutcome {
    /// Memory files encrypted into the ciphertext mirror this run.
    pub encrypted: usize,
    /// Memory files decrypted back from the mirror (fresh device / after pull).
    pub decrypted: usize,
    /// The ciphertext mirror was pushed to its remote.
    pub pushed: bool,
    /// The tool used (`age` or `gpg`), for the human/robot report.
    pub tool: &'static str,
}

/// Initialise the ciphertext mirror `<store>/.enc/` as its own git repo pointed
/// at the user's own *encrypted* `remote`. Kept separate from the plaintext
/// remote so the two never mix: the plaintext store repo (if any) ignores
/// `.enc/`, and only ciphertext lands on this remote.
pub fn sync_encrypted_init(store: &Store, remote: &str) -> Result<()> {
    let enc = crypt::enc_dir(store);
    std::fs::create_dir_all(enc.join("memories")).map_err(|e| Error::Io {
        context: "creating encrypted mirror directory".to_string(),
        path: enc.display().to_string(),
        source: e,
    })?;
    // No ignores: every ciphertext file is committed to the encrypted remote.
    git_init_at(&enc, remote, &[])
}

/// Does the store hold at least one plaintext memory file?
fn has_any_memory(store: &Store) -> bool {
    std::fs::read_dir(store.memories_dir())
        .map(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.ends_with(".md") && !n.starts_with('.')
            })
        })
        .unwrap_or(false)
}

/// Encrypted sync: encrypt the store's memory files into the `<store>/.enc/`
/// ciphertext mirror before commit/push, and decrypt on pull. The plaintext
/// store on disk is never touched by this beyond a fresh-device restore, and
/// the DEFAULT [`sync`] path is entirely independent.
///
/// Flow: on a fresh device (ciphertext present, no local plaintext) decrypt
/// first to restore. Encrypt the current plaintext into the mirror. If the
/// mirror is a git repo, commit / rebase / push it (a rebase conflict on
/// ciphertext surfaces as [`Error::Conflict`], exit 3, like the plaintext
/// path); after a pull, decrypt anything new so a second device sees it.
pub fn sync_encrypted(store: &Store, clock: &dyn Clock) -> Result<EncryptOutcome> {
    let cfg = crypt::CryptConfig::from_env()?;
    if !crypt::available(cfg.tool) {
        return Err(Error::Usage {
            message: format!(
                "encrypted sync needs the `{}` tool on PATH (not found)",
                cfg.tool.binary()
            ),
        });
    }
    sync_encrypted_with(store, &cfg, clock)
}

/// The encrypted-sync core with an explicit [`crypt::CryptConfig`] (so it is
/// testable without mutating process env, which is `unsafe` under edition
/// 2024). [`sync_encrypted`] is the thin env-reading wrapper.
pub fn sync_encrypted_with(
    store: &Store,
    cfg: &crypt::CryptConfig,
    clock: &dyn Clock,
) -> Result<EncryptOutcome> {
    let enc = crypt::enc_dir(store);
    let mut decrypted = 0usize;
    // Fresh-device restore: ciphertext exists but no local plaintext yet.
    if enc.join("memories").exists() && !has_any_memory(store) {
        decrypted += crypt::decrypt_store(store, cfg)?;
    }
    let encrypted = crypt::encrypt_store(store, cfg)?;
    let mut pushed = false;
    if enc.join(".git").exists() {
        let outcome = sync_git(&enc, clock)?;
        pushed = outcome.pushed;
        // A pull may have brought a peer's ciphertext; decrypt it into place.
        if outcome.pulled {
            decrypted += crypt::decrypt_store(store, cfg)?;
        }
    }
    Ok(EncryptOutcome {
        encrypted,
        decrypted,
        pushed,
        tool: cfg.tool.binary(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryType;
    use crate::store::testutil::TempDir;
    use crate::store::{NewMemory, Store};
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000;

    fn bare_remote(dir: &Path) -> String {
        let _ = git(dir, &["init", "--bare"]);
        dir.display().to_string()
    }

    fn remember(store: &Store, title: &str) {
        store
            .create(
                &NewMemory {
                    mtype: Some(MemoryType::Fact),
                    title: title.to_string(),
                    ..NewMemory::default()
                },
                &FixedClock(T0),
            )
            .unwrap();
    }

    #[test]
    fn init_push_then_pull_on_a_second_device_round_trips() {
        if !git_available() {
            eprintln!("SKIP: git not available");
            return;
        }
        let remote_dir = TempDir::new("sync-remote");
        std::fs::create_dir_all(remote_dir.path()).unwrap();
        let remote = bare_remote(remote_dir.path());

        // Device A: init, remember, sync (push).
        let a = TempDir::new("sync-a");
        let store_a = Store::open(a.path());
        remember(&store_a, "shared across devices");
        sync_init(&store_a, &remote).unwrap();
        let out = sync(&store_a, &FixedClock(T0)).unwrap();
        assert!(out.committed && out.pushed);

        // Device B: init against the same remote, sync (pull).
        let b = TempDir::new("sync-b");
        let store_b = Store::open(b.path());
        sync_init(&store_b, &remote).unwrap();
        let out = sync(&store_b, &FixedClock(T0)).unwrap();
        assert!(out.pulled, "device B pulled A's memory");
        let (mems, _) = store_b.list(&crate::store::ListFilter::default()).unwrap();
        assert!(
            mems.iter().any(|m| m.title == "shared across devices"),
            "the memory synced to device B"
        );
    }

    #[test]
    fn sync_before_init_is_a_usage_error() {
        let tmp = TempDir::new("sync-noinit");
        let store = Store::open(tmp.path());
        // create the dir but no .git
        remember(&store, "x");
        let e = sync(&store, &FixedClock(T0)).unwrap_err();
        assert!(matches!(e, Error::Usage { .. }), "got {e:?}");
    }

    fn gpg_cfg() -> crypt::CryptConfig {
        crypt::CryptConfig {
            tool: crypt::Tool::Gpg,
            recipient: None,
            identity: None,
            passphrase: Some("ghostie-sync-test-pass".to_string()),
        }
    }

    #[test]
    fn encrypted_sync_pushes_ciphertext_and_restores_on_a_second_device() {
        if !git_available() || !crypt::gpg_available() {
            eprintln!("SKIP: git or gpg not available");
            return;
        }
        let cfg = gpg_cfg();
        let remote_dir = TempDir::new("enc-remote");
        std::fs::create_dir_all(remote_dir.path()).unwrap();
        let remote = bare_remote(remote_dir.path());

        // Device A: remember, init the encrypted mirror, encrypted-sync (push).
        let a = TempDir::new("enc-a");
        let store_a = Store::open(a.path());
        remember(&store_a, "a secret only for me");
        sync_encrypted_init(&store_a, &remote).unwrap();
        let out = sync_encrypted_with(&store_a, &cfg, &FixedClock(T0)).unwrap();
        assert!(out.encrypted >= 1 && out.pushed, "{out:?}");
        // The remote holds only ciphertext: the plaintext title is not in the
        // committed .enc tree.
        let enc_mem = crypt::enc_dir(&store_a).join("memories");
        for e in std::fs::read_dir(&enc_mem).unwrap().flatten() {
            let bytes = std::fs::read(e.path()).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("a secret only for me"),
                "ciphertext must not leak the plaintext"
            );
        }

        // Device B: init against the same encrypted remote, encrypted-sync;
        // it pulls ciphertext and decrypts the memory into its plaintext store.
        let b = TempDir::new("enc-b");
        let store_b = Store::open(b.path());
        sync_encrypted_init(&store_b, &remote).unwrap();
        let out = sync_encrypted_with(&store_b, &cfg, &FixedClock(T0)).unwrap();
        assert!(
            out.decrypted >= 1,
            "device B restored from ciphertext: {out:?}"
        );
        let (mems, _) = store_b.list(&crate::store::ListFilter::default()).unwrap();
        assert!(
            mems.iter().any(|m| m.title == "a secret only for me"),
            "the encrypted memory decrypted onto device B"
        );
    }

    #[test]
    fn gitignore_excludes_the_derived_index() {
        if !git_available() {
            eprintln!("SKIP: git not available");
            return;
        }
        let remote_dir = TempDir::new("sync-gi-remote");
        std::fs::create_dir_all(remote_dir.path()).unwrap();
        let remote = bare_remote(remote_dir.path());
        let tmp = TempDir::new("sync-gi");
        let store = Store::open(tmp.path());
        sync_init(&store, &remote).unwrap();
        let gi = std::fs::read_to_string(store.root().join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == ".index/"));
    }
}
