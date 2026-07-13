# Goal: ghostie, rebuilt — memory you own, in Rust

## The mission

A local-first, provider-agnostic memory system for AI coding work. Your
context (facts, decisions, session history, working rules) lives in plain
files you own and can read, and it is portable across sessions, across coding
agents, and across devices. This is the "remembers" node of the ghostie stack.

## Why a clean start

The earlier ghostie was a Node/npm tool built around one hosted agent. That
made it unable to honestly claim the stack's core properties (sovereign,
airgap-capable, provider-agnostic). This rewrite makes those true:

- **Owned**: plain human-readable files on disk, not a vendor database.
- **Provider-agnostic**: not tied to any single model or agent harness.
- **Airgap-capable**: runs fully offline; sync is optional and uses the user's
  own git remote, so data never lands on someone else's server.

## Engineering religion (non-negotiable)

- Rust, **std-only, zero external dependencies**, `#![forbid(unsafe_code)]`.
- Deterministic and byte-stable: the same inputs produce byte-identical store
  files and outputs.
- Plain-file storage the user can read, diff, and edit by hand (Markdown with
  frontmatter, or an equally transparent format). No opaque binary store.
- A robot mode (`--json`) on every command so agents can drive it.
- Dogfood the stack: the `gatekit` verification-gate pattern for CI, `beads`
  for planning, the deterministic clean-room search approach used elsewhere in
  the stack (BM25 + hash embeddings, integer math, why-explainable) for recall.
- CI runs on a local Linux box (Docker) as well as GitHub, proving it builds
  the same on the dev machine and in CI.

## Capabilities (the shape of the work — expand into beads)

1. **Store** — the on-disk memory format and CRUD. A memory is a file with
   typed frontmatter (a fact, a decision, a rule, a session summary) plus body.
   Deterministic serialization; an index for fast lookup that is derivable and
   never authoritative over the files themselves.
2. **Capture** — ingest agent session logs from multiple harnesses (Claude
   Code JSONL, Codex, others) into a unified, provider-agnostic session record.
   The parsers are pluggable; adding a harness does not change the core.
3. **Recall** — given a task or query, surface the relevant memories.
   Deterministic retrieval (clean-room BM25 + hash-embedding rerank, std-only),
   with an explanation of *why* each result matched. This is the value: the
   right context, resurfaced, without a cloud.
4. **Sync** — cross-device via the user's own git remote. Deterministic,
   conflict-aware at the file level, provider-agnostic. No proprietary sync
   service. Implemented by shelling to the system `git` binary (an external
   *tool*, present everywhere, NOT a crate dependency), so crate-level
   zero-dependency is preserved. This mirrors how Lockstep shells to POSIX
   `kill`: the toolbox stays std-only; the OS's own utilities are fair game.
5. **CLI + robot mode** — `ghostie remember`, `recall`, `capture`, `sync`,
   `list`, each with `--json`. Ergonomic for humans, scriptable for agents.
6. **Gate** — the gatekit-pattern `verify.sh` grown into a real gate: fmt,
   clippy (deny warnings), build, test, a dogfood run (capture a fixture
   session, recall against it, assert the expected memory surfaces), and a
   byte-stability check on the store format. CI is a thin wrapper that runs it.

## First milestone (make it demoable, not theoretical)

The Store plus a minimal Recall, dogfooded end to end: write a handful of
memories, run `ghostie recall "<task>"`, and get the right ones back with a
why. Then Capture one real session and recall against it. That single loop,
green in the local Linux CI box, is the proof the rewrite works.

## Positioning (must match the site)

Memory you own, provider-agnostic, portable across sessions/agents/devices.
Not "airgap everything" (it composes with hosted agents); rather, *your
context stays yours*, readable and local, wherever the model runs.
