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

On top of BM25, a deterministic hashed-subword embedding reranks the results:
each token is hashed into character n-grams, so `sovereign` reaches a memory
that only ever says `sovereignty`, and a concept phrased differently still
surfaces. No model, no stemming, no lexicon; std-only integer cosine, so the
binary stays offline and byte-stable. A labeled eval (`cargo test --test eval
-- --nocapture`) measures the lift: on its near-miss set, mean reciprocal rank
goes from 0.00 (BM25 alone) to 1.00 with the rerank on. `recall --no-rerank`
turns it off.

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

## Use ghostie as an MCP server

Any MCP client (Codex, Cursor, Claude, Windsurf, Zed) can use your store as its
memory. `ghostie mcp serve` speaks the Model Context Protocol over stdio:
newline-delimited JSON-RPC 2.0, one request per line. It exposes four tools:

- `recall` (query, budget?, k?, scope?): the ranked memories for a task, each
  with its why and provenance.
- `remember` (type, title, body?, tags?, harness?, core?, rationale?, scope?):
  create a memory, returns the new id.
- `capture` (path, format?, harness?): distill a session transcript into memories.
- `list`: every memory in the store.

Point your client's MCP config at the `ghostie` binary. For a client that reads
`~/.config` style JSON (adapt the exact path to your client):

```json
{
  "mcpServers": {
    "ghostie": {
      "command": "ghostie",
      "args": ["mcp", "serve"]
    }
  }
}
```

`ghostie mcp` with no argument prints a one-shot manifest (server name, version,
and the tool list); `ghostie mcp --json` emits it as a single JSON envelope, so
a tool can discover the surface without starting the server.

## Secret-redaction gate

Because your memory syncs to your own git remote, nothing secret should ever
land in a memory file. Every write runs through a deterministic, std-only
redaction pass first: it scans the free-text fields (title, tags, rationale,
source, and the body) and replaces anything shaped like a credential with
`[REDACTED:<kind>]`. This matters most for `capture`, which ingests arbitrary
agent transcripts that routinely echo API keys and tokens.

The scan is a small hand-rolled matcher (no regex crate) covering AWS access
key ids, GitHub tokens, OpenAI/Anthropic keys, Slack tokens, Google API keys,
`Bearer` / `Authorization:` credentials, PEM private-key blocks, and
`password=` / `token=` / `secret=` style assignments. It is deliberately
conservative: every matcher keys off a specific vendor prefix or an explicit
assignment context, and there is no blunt "long random string" rule. The
tradeoff is precision over recall. Ordinary prose, memory ids like
`rule-foo-1`, short git shas, and plain URLs are never mangled; the cost is
that a bare, prefixless secret can slip through. A false positive silently
corrupts your own memory, so the gate errs toward leaving text alone.

Redaction runs at a single choke point (`Store::build_memory`), so both
`remember` and `capture` are covered and the scrubbed bytes are what get
written and synced. It is deterministic and byte-stable: the same input always
produces the same output. For the rare case where content must be stored
verbatim, `remember --no-redact` and `capture --no-redact` turn the gate off.

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
