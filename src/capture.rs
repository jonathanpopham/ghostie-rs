//! capture — pluggable session-log ingestion (post-Milestone-1).
//!
//! Single responsibility: parse agent session logs from multiple harnesses
//! (Claude Code JSONL, Codex, ...) into one provider-agnostic
//! `SessionRecord`, then distill it into a `session-summary` memory.
//! Parsers are pluggable; adding a harness must not change the core.
