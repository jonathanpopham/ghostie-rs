#![forbid(unsafe_code)]
//! ghostie — memory you own. Local-first, provider-agnostic memory for AI
//! coding work: plain files on your disk, deterministic tooling around them.
//!
//! # Engineering religion (non-negotiable, enforced by the gate)
//!
//! - Rust, std-only, **zero external dependencies**.
//! - `#![forbid(unsafe_code)]` in both crate roots.
//! - Deterministic and byte-stable: same inputs -> byte-identical store
//!   files and outputs. No floats in scoring (fixed-point i64). No map
//!   iteration order or wall clock on any output path (see [`util`]).
//! - Plain human-readable storage: Markdown + frontmatter (`docs/FORMAT.md`).
//! - A `--json` robot mode on every command.

pub mod capture;
pub mod cli;
pub mod codex;
pub mod distill;
mod error;
pub mod hook;
pub mod json;
pub mod mcp;
pub mod recall;
pub mod redact;
pub mod store;
pub mod sync;
pub mod util;

pub use error::{Error, Result};
