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

use crate::error::{Error, Result};
use crate::store::Store;
use crate::util::{Clock, format_rfc3339_utc};
use std::path::Path;
use std::process::Command;

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
/// the derived index.
pub fn sync_init(store: &Store, remote: &str) -> Result<()> {
    let root = store.root();
    std::fs::create_dir_all(root).map_err(|e| Error::Io {
        context: "creating store directory for sync".to_string(),
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
    ensure_gitignore(root)?;
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

/// Ensure `.gitignore` excludes the derived index (append if missing, create
/// if absent). The index is rebuildable, so syncing it only makes conflicts.
fn ensure_gitignore(root: &Path) -> Result<()> {
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
    if existing.lines().any(|l| l.trim() == ".index/") {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".index/\n");
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
    let root = store.root();
    if !root.join(".git").exists() {
        return Err(Error::Usage {
            message: "store is not initialised for sync; run `ghostie sync --init <remote>` first"
                .to_string(),
        });
    }

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
