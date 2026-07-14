//! review — the trust / approval gate for captured memories.
//!
//! Single responsibility: hold candidate memories in a PENDING area
//! (`<store>/.pending/`) so a human approves what enters the live store before
//! auto-capture and sync ever land it. Opt-in: capture writes to pending only
//! with `--pending` (or a hook installed with `--review`); the default
//! auto-capture flow is unchanged.
//!
//! The pending area is itself a full [`Store`] rooted at `<store>/.pending/`,
//! so capture writes candidates through the identical redacting, byte-stable
//! write path, and the ids it assigns there are exactly the ids the live store
//! will use on approval. `.pending/` never syncs: it is gitignored by
//! [`crate::sync`], so an unapproved candidate can never reach the remote.
//!
//! - [`list_pending`]  — enumerate candidates (deterministic id order).
//! - [`approve`] / [`approve_all`] — promote a candidate into the live store
//!   (then normal sync carries it), and drop it from pending.
//! - [`reject`] / [`reject_all`] — drop a candidate, unpromoted.

use crate::error::{Error, Result, Warning};
use crate::store::memory::Memory;
use crate::store::{ListFilter, NewMemory, Store};
use crate::util::Clock;
use std::path::PathBuf;

/// `<store>/.pending` — where candidates wait for approval.
pub fn pending_root(store: &Store) -> PathBuf {
    store.root().join(".pending")
}

/// A [`Store`] rooted at the pending area. A full store (same on-disk format),
/// so capture can write candidates into it unchanged.
pub fn pending_store(store: &Store) -> Store {
    Store::open(pending_root(store))
}

/// The candidates awaiting review, in deterministic id order. An absent
/// pending area is simply "nothing pending" (empty, no error).
pub fn list_pending(store: &Store) -> Result<(Vec<Memory>, Vec<Warning>)> {
    pending_store(store).list(&ListFilter::default())
}

/// Rebuild the `NewMemory` inputs from a stored candidate so it can be created
/// verbatim in the live store. `created` is re-stamped by the live store from
/// the injected clock at approval time (approval is when it enters memory).
fn to_new(mem: &Memory) -> NewMemory {
    NewMemory {
        mtype: Some(mem.mtype),
        title: mem.title.clone(),
        tags: mem.tags.clone(),
        links: mem.links.clone(),
        source: mem.source.clone(),
        supersedes: mem.supersedes.clone(),
        harness: mem.harness.clone(),
        core: mem.core.clone(),
        rationale: mem.rationale.clone(),
        scope: mem.scope.clone(),
        body: mem.body.clone(),
    }
}

/// Approve one candidate by id: promote it into the live store under the SAME
/// id, then drop it from pending. Idempotent: if the live store already has
/// that id (a re-approval), the pending copy is still cleared. Returns the id
/// promoted.
pub fn approve(store: &Store, id: &str, clock: &dyn Clock) -> Result<String> {
    let pending = pending_store(store);
    let (mem, _warnings) = pending.read(id).map_err(|_| Error::Usage {
        message: format!("no pending candidate '{id}' to approve"),
    })?;
    // The candidate was already written through the redacting write path in the
    // pending store, so a second redaction pass in the live store is an
    // idempotent no-op. create_with_id is a no-op when the id already exists.
    store.create_with_id(id, &to_new(&mem), clock)?;
    pending.delete(id)?;
    Ok(id.to_string())
}

/// Reject one candidate by id: drop it, unpromoted. Errors if there is no such
/// pending candidate.
pub fn reject(store: &Store, id: &str) -> Result<String> {
    let pending = pending_store(store);
    pending.read(id).map_err(|_| Error::Usage {
        message: format!("no pending candidate '{id}' to reject"),
    })?;
    pending.delete(id)?;
    Ok(id.to_string())
}

/// Approve every pending candidate. Returns the ids promoted, in id order.
pub fn approve_all(store: &Store, clock: &dyn Clock) -> Result<Vec<String>> {
    let (mems, _w) = list_pending(store)?;
    let pending = pending_store(store);
    let mut done = Vec::new();
    for mem in &mems {
        store.create_with_id(&mem.id, &to_new(mem), clock)?;
        pending.delete(&mem.id)?;
        done.push(mem.id.clone());
    }
    Ok(done)
}

/// Reject every pending candidate. Returns the ids dropped, in id order.
pub fn reject_all(store: &Store) -> Result<Vec<String>> {
    let (mems, _w) = list_pending(store)?;
    let pending = pending_store(store);
    let mut done = Vec::new();
    for mem in &mems {
        pending.delete(&mem.id)?;
        done.push(mem.id.clone());
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture;
    use crate::store::memory::MemoryType;
    use crate::store::testutil::TempDir;
    use crate::util::FixedClock;

    const T0: i64 = 1_783_944_000;

    fn capture_into_pending(store: &Store) -> Vec<Memory> {
        let pending = pending_store(store);
        let rec = capture::parse(
            "MEMORY fact: candidate worth keeping\nMEMORY rule: gate before commit",
            capture::Format::Generic,
            Some("hermes"),
            None,
        );
        capture::capture(&pending, &rec, &FixedClock(T0)).unwrap()
    }

    #[test]
    fn capture_to_pending_does_not_touch_the_live_store() {
        let tmp = TempDir::new("review-pending-write");
        let store = Store::open(tmp.path());
        let created = capture_into_pending(&store);
        assert!(!created.is_empty(), "candidates written to pending");
        // The live store stays empty.
        let (live, _) = store.list(&ListFilter::default()).unwrap();
        assert!(live.is_empty(), "nothing landed live: {live:?}");
        // The candidates are listable from pending.
        let (pending, _) = list_pending(&store).unwrap();
        assert_eq!(pending.len(), created.len());
    }

    #[test]
    fn approve_promotes_into_the_live_store_and_clears_pending() {
        let tmp = TempDir::new("review-approve");
        let store = Store::open(tmp.path());
        capture_into_pending(&store);
        let (pending, _) = list_pending(&store).unwrap();
        // Approve the fact marker.
        let fact = pending
            .iter()
            .find(|m| m.mtype == MemoryType::Fact)
            .expect("a fact candidate");
        let id = approve(&store, &fact.id, &FixedClock(T0)).unwrap();
        assert_eq!(id, fact.id);
        // It is now live...
        let (back, _) = store.read(&fact.id).unwrap();
        assert_eq!(back.id, fact.id);
        // ...and gone from pending.
        let (still, _) = list_pending(&store).unwrap();
        assert!(
            !still.iter().any(|m| m.id == fact.id),
            "cleared from pending"
        );
    }

    #[test]
    fn reject_drops_without_promoting() {
        let tmp = TempDir::new("review-reject");
        let store = Store::open(tmp.path());
        capture_into_pending(&store);
        let (pending, _) = list_pending(&store).unwrap();
        let victim = &pending[0];
        let id = reject(&store, &victim.id).unwrap();
        assert_eq!(id, victim.id);
        // Not live, not pending.
        assert!(store.read(&victim.id).is_err(), "never promoted");
        let (still, _) = list_pending(&store).unwrap();
        assert!(
            !still.iter().any(|m| m.id == victim.id),
            "gone from pending"
        );
    }

    #[test]
    fn approve_all_and_reject_all() {
        let tmp = TempDir::new("review-all");
        let store = Store::open(tmp.path());
        capture_into_pending(&store);
        let (pending, _) = list_pending(&store).unwrap();
        let total = pending.len();
        assert!(total >= 2);
        // Approve all: pending empties, live fills.
        let promoted = approve_all(&store, &FixedClock(T0)).unwrap();
        assert_eq!(promoted.len(), total);
        let (still, _) = list_pending(&store).unwrap();
        assert!(still.is_empty());
        let (live, _) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(live.len(), total);

        // Fresh batch, reject all: live count unchanged.
        capture_into_pending(&store);
        // A re-captured identical session is idempotent, so pending may be
        // empty; capture a distinct one to be sure there is something to reject.
        let rec = capture::parse(
            "MEMORY fact: a different candidate",
            capture::Format::Generic,
            Some("hermes"),
            None,
        );
        capture::capture(&pending_store(&store), &rec, &FixedClock(T0)).unwrap();
        let dropped = reject_all(&store).unwrap();
        assert!(!dropped.is_empty());
        let (still, _) = list_pending(&store).unwrap();
        assert!(still.is_empty(), "reject_all clears pending");
        let (live_after, _) = store.list(&ListFilter::default()).unwrap();
        assert_eq!(live_after.len(), total, "reject promoted nothing");
    }

    #[test]
    fn approve_or_reject_unknown_id_is_a_usage_error() {
        let tmp = TempDir::new("review-unknown");
        let store = Store::open(tmp.path());
        let e = approve(&store, "fact-nope-1", &FixedClock(T0)).unwrap_err();
        assert!(matches!(e, Error::Usage { .. }), "{e:?}");
        let e = reject(&store, "fact-nope-1").unwrap_err();
        assert!(matches!(e, Error::Usage { .. }), "{e:?}");
    }
}
