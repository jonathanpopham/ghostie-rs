# ghostie — memory you own

Provider-agnostic memory for AI coding work. Your context lives in a store you
own and can read: portable across sessions, across agents, and across devices.

- **Own it.** Plain files on your disk, human-readable, not a vendor's database.
- **Provider-agnostic.** Works with any coding agent; your memory is not locked
  to one model or tool.
- **Local-first, airgap-capable.** Runs fully offline. Sync is optional and
  goes through your own git remote, so your data never sits on someone else's
  server.
- **Rust, std-only, deterministic.** Zero external dependencies,
  `#![forbid(unsafe_code)]`, byte-stable outputs.

The "remembers" node of the ghostie stack, and the piece that makes the stack's
sovereignty real: a clean-start rewrite of an earlier Node prototype that
depended on a hosted agent. This version does not.

Status: early. See `docs/GOAL.md` for the plan and the beads for tracked work.

## CI

`scripts/verify.sh` is the gate: fmt, clippy (deny warnings), build, test,
a dogfood run (seed fixture memories through the real binary, recall
labeled queries, assert the expected memory surfaces with a why), a
byte-stability check on the store format, and policy guards (zero deps,
forbid unsafe, robot mode on every verb, no floats in scoring, LF only).

GitHub Actions runs exactly that script. `scripts/ci-local.sh` runs the
identical gate inside Linux on the dev machine (Docker is the only extra
requirement); the toolchain is pinned by `rust-toolchain.toml` and the
script refuses to run against a mismatched container.
