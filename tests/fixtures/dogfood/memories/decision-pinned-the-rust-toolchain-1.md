---
id: decision-pinned-the-rust-toolchain-1
type: decision
created: 2026-06-29T14:00:00Z
title: Pinned the Rust toolchain
tags: [ci, determinism]
supersedes: decision-float-with-stable-1
---
Unicode tables and lint sets drift across compiler releases; goldens need
one compiler everywhere.
