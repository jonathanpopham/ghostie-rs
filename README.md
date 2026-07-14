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
cover.

## One button, and it just works

The point of provider-agnostic memory is that it moves with you. One command
wires that up:

```sh
ghostie setup <your-git-remote>   # any git host; omit the remote for local-only
```

That wires the store to your own remote, installs the hooks, and does the first
push. After it, in Claude Code: relevant memories are recalled and injected on
each prompt (bounded by a token budget), and when a session ends the transcript
is captured into memories, then committed and pushed to your remote. Sit down at
another machine, run the same `ghostie setup <remote>`, and your context is
there. `ghostie hook status` shows what is wired; `ghostie hook uninstall`
removes it, leaving your other settings untouched.

Under the button are the parts, usable on their own: `ghostie sync --init
<remote>` then `ghostie sync`, and `ghostie hook install`.

Codex auto-capture works too. Codex has no pre-prompt hook, but it has a
`notify` program it runs after each turn, so `ghostie hook install --harness
codex [--sync]` sets that program in `~/.codex/config.toml` (backing the config
up first). On each completed turn it captures the just-finished rollout from
`~/.codex/sessions`, deduped by session id so a repeated notify is a no-op. If
you already have a `notify` configured, ghostie will not overwrite it; it prints
the exact line to merge yourself. `ghostie hook status --harness codex` and
`ghostie hook uninstall --harness codex` manage it. You can also capture the
latest Codex session on demand with `ghostie capture --latest codex`.

`ghostie capture <transcript>` distills a session by hand: a session-summary
carrying provenance plus one memory per `MEMORY <type>: ...` marker left in the
transcript. Sync shells to the system `git` binary (a tool, not a crate), so
crate-level zero-dependency holds; conflicts are reported, never auto-resolved.

Status: the store, provenance, graph-aware recall, capture, sync, and the hook
installer are working and gated. Richer model-driven distillation is the one
deliberately impure, feature-gated step still ahead. See `docs/GOAL.md`.

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
