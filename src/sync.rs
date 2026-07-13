//! sync — cross-device sync via the user's own git remote
//! (post-Milestone-1).
//!
//! Single responsibility: commit/fetch/merge/push of the store by shelling
//! to the system `git` binary (an external *tool*, not a crate dependency).
//! File-level conflict awareness: detect and report, never auto-resolve.
