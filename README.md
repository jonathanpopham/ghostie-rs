# ghostie: memory you own

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

## What it does today

A memory is a plain Markdown file with typed frontmatter (fact, decision, or
rule) plus a body. Beyond the title and tags it carries provenance, which is
what makes it useful across tools:

- `harness` and `core` record WHERE a memory was made and WHICH model made it,
  so a note taken in one harness arrives in another with its origin attached.
- `rationale` (the `--why` flag) is a one-line reason the memory matters,
  surfaced on the recall card without opening the body.

Recall ranks with a clean-room BM25 (fixed-point integer, no floats), then
seeds a Personalized PageRank walk with those hits and follows `links` between
memories. A memory linked to a match surfaces even when it shares no words with
the query, and it names the edge that carried it. Every hit shows its why.
`--budget N` caps the result in tokens so a context-injection hook never
floods; `--diverse` demotes near-duplicate memories (MMR) so each card earns
its slot; `--scope` keeps recall focused on one project.

```sh
ghostie remember --type decision "Chose DuckDB over Postgres" \
  --why "we'll hit this again porting the ingest service" \
  --harness hermes --core hermes-4-405b
ghostie recall "why did we pick duckdb" --budget 800
```

Retrieval is literal-token today (no stemming or embeddings): near-miss wording
can miss, which the link graph and a future hash-embedding rerank are meant to
cover. Capture-on-a-hook and automatic git sync are the next milestone.

Status: the store, provenance, and graph-aware recall are working and gated.
See `docs/GOAL.md` for the plan and the beads for tracked work.

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
