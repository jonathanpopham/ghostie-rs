# ghostie store format (normative)

This document is the authoritative spec for the on-disk memory format. The
store code implements THIS document; if code and spec disagree, the spec wins
and the code is a bug. The golden fixtures under `tests/fixtures/store/`
(one per memory type, exercising tags/links/unknown keys/quoting) are
**normative examples** of canonical form: `tests/store_goldens.rs` asserts
they survive parse -> serialize byte-identically, so any codec drift fails
the gate loudly.

## Store layout

- Store root: `$GHOSTIE_HOME` if set, else `~/.ghostie`. The `--store <path>`
  CLI flag overrides both (precedence: flag > env > default).
- Memories live in `<root>/memories/`, one file per memory, filename
  `<id>.md`.
- The derived index lives in `<root>/.index/index.json`. It is derivable
  data: rebuildable at any time from the memory files, never authoritative,
  and excluded from sync. Its `corpus.term_embed` block caches, per unique
  indexed term, the hashed-subword embedding the semantic rerank uses (a map
  of bucket to integer weight). Embeddings are a pure function of the term
  string, so the cache is a speed optimization only: recomputing it yields
  byte-identical vectors, and deleting the index never changes recall output.
  The index `format_version` is bumped whenever this schema changes so older
  indexes rebuild automatically.
- Non-`.md` files and dotfiles inside `memories/` are ignored (humans keep
  notes; editors drop swap files).

## File shape

A memory file is UTF-8 text: a frontmatter block delimited by two `---`
lines, then a Markdown body.

```
---
id: rule-verify-before-commit-1
type: rule
created: 2026-07-13T12:00:00Z
title: Always run verify.sh before committing
tags: [ci, discipline]
---
The gate is scripts/verify.sh. Run it locally before every commit.
```

The body is everything after the closing `---` line's newline, verbatim
(canonical form normalizes line endings to LF). A file with no frontmatter
block at all is an error: the first line of a memory file MUST be exactly
`---`.

## Frontmatter grammar (restricted, self-defined, NOT full YAML)

Full YAML is a determinism tarpit and would need an external parser. The
frontmatter is a deliberately tiny language:

- One `key: value` pair per line. No nesting, no multi-line values.
- **Keys**: `[a-z0-9_]+` (lowercase ASCII, digits, underscore).
- **Duplicate keys are an error** (silent last-wins would eat human data).
- **Scalar values**: written bare when safe (see quoting rule), otherwise
  double-quoted.
- **List values**: inline only: `key: [a, b, c]`. Elements follow the same
  scalar quoting rule. The empty list `[]` never appears in canonical form
  (empty/absent fields are omitted); readers accept it as "absent".

### Quoting rule (exact)

A scalar is written **bare** iff it is non-empty and:

- contains none of: `"` `[` `]` `,` `#`, and no control characters
  (including newline/tab), and
- has no leading or trailing space or tab.

(`:` inside a value is fine bare: the key/value split is at the *first*
colon on the line, and keys are `[a-z0-9_]+`, so `created:
2026-07-13T12:00:00Z` is unambiguous.)

Otherwise it is written **double-quoted** with exactly two escapes: `\"` for
a double quote and `\\` for a backslash. (No other escapes exist; a literal
newline cannot appear in a frontmatter value.) Readers accept a quoted form
even where bare would have been legal; writers quote only when required.

## Memory types and fields

`type` is one of: `fact` | `decision` | `rule` | `session-summary`.

Required for all memories:

| key | meaning |
|---|---|
| `id` | the memory's identity and filename stem; see ID scheme |
| `type` | one of the four types |
| `created` | UTC RFC3339, second precision, `Z` suffix only (`YYYY-MM-DDTHH:MM:SSZ`), stamped from the injected Clock at creation and never changed |
| `title` | one-line human title |

Optional:

| key | meaning |
|---|---|
| `tags` | list of tags (bare-safe strings by convention) |
| `links` | list of memory ids this memory relates to |
| `source` | for captured memories: `<harness>:<session_id>` (meaningful on `session-summary`) |
| `supersedes` | id of the decision this decision replaces (meaningful on `decision`) |
| `harness` | provenance: which harness created the memory (`claude-code`, `hermes`) |
| `core` | provenance: which model produced it (`opus-4.8`, `hermes-4-405b`) |
| `rationale` | why the memory is necessary; the why-line surfaced on the recall card |
| `scope` | retrieval scope: `global` (default when absent) or `project:<name>` |
| `confidence` | lifecycle: confidence in micro-units (0..1000000; full when absent). Decays on a half-life without reuse; `recall --touch` restores it |
| `last_used` | lifecycle: UTC RFC3339 instant the memory was last used/revalidated; the reference point for decay (falls back to `created` when absent) |

Type-scoping is warn-not-error: `supersedes` on a non-decision or `source`
on a non-session-summary parses fine and is preserved, but validation
surfaces a structured warning. Be liberal, warn, never destroy.

**Unknown keys are preserved verbatim on rewrite.** Humans will add keys;
ghostie must never eat them. See canonical form for their position.

## Canonical form (the byte-stability contract)

Writers ALWAYS emit canonical form. Readers are tolerant (CRLF, reordered
keys, extra blank lines, trailing whitespace, missing final newline, quoted
scalars where bare would do). Normalization happens only on write, never as
a side effect of reading.

Canonical form is:

1. Line 1: `---` exactly.
2. Frontmatter keys in **schema order**: `id`, `type`, `created`, `title`,
   `tags`, `links`, `source`, `supersedes`, `harness`, `core`, `rationale`,
   `scope`, `confidence`, `last_used`, then unknown keys, in first-seen order.
   Absent/empty optional fields are omitted entirely (no `tags: []` noise).
3. Exactly one space after the colon: `key: value`. List form
   `key: [a, b]`, one space after each comma, none inside the brackets.
4. Closing `---` line immediately after the last key line (no blank lines
   inside the frontmatter block).
5. The body follows, verbatim except line endings normalized to LF.
6. LF endings everywhere, no trailing whitespace on any line, and the file
   ends with **exactly one** trailing newline. An empty body means the file
   ends right after the closing `---` line's newline.

Contracts, encoded as tests:

- **Idempotence**: `serialize(parse(serialize(d))) == serialize(d)`.
- **Tolerant-read canonical-write**: parsing any accepted variant then
  serializing yields the canonical bytes.
- **Same inputs, same bytes**: two stores built from the same definitions
  with the same clock are byte-identical trees.

## ID scheme

`<type>-<slug>-<disambiguator>`

- `slug`: derived from the title at creation: lowercase ASCII letters and
  digits, everything else collapsed to single hyphens, trimmed of leading/
  trailing hyphens, capped at 40 characters (never ending in a hyphen).
  An empty slug (all-symbol title) becomes `untitled`.
- `disambiguator`: the lowest positive integer that makes the id free in
  this store, scanned from the filesystem deterministically (`-1`, `-2`, ...).
- Ids are assigned once at creation and are **stable across title edits**:
  files never rename on update.
- Captured session memories (capture phase, post-milestone) derive their
  disambiguator from FNV-1a 64 of `harness:session_id` instead, so
  re-capturing the same session is idempotent. Recorded here; implemented
  in the capture phase.

## Edge cases (defined behavior)

| case | behavior |
|---|---|
| missing required field (`id`, `type`, `created`, `title`) | typed error naming file + field; store operations skip the file with a structured warning |
| unknown `type` value | error (same handling as above) |
| malformed `created` | error naming file + expected shape |
| duplicate key | error naming file + line + key |
| empty body | valid; canonical file ends after closing `---` |
| no frontmatter (first line not `---`) | error: "not a memory file" |
| unclosed frontmatter (one `---` only) | error naming the file |
| git conflict markers (`<<<<<<<`/`=======`/`>>>>>>>`) anywhere in the file | reported as "conflicted", not parsed as a memory |
| `id` in frontmatter disagrees with filename | warning; the frontmatter `id` wins for content, the mismatch is surfaced |
| non-`.md` files / dotfiles in `memories/` | ignored |

## How to hand-edit safely

The files are yours. Edit them with anything. To stay friction-free:

- Keep the first line `---` and one `key: value` per line above the second
  `---`. Don't repeat a key.
- Quote a value only if it needs `"`, `[`, `]`, `,`, `#`, or edges of
  whitespace; inside quotes escape `\"` and `\\`.
- Add your own keys freely (`priority: high`); ghostie preserves them.
- Don't edit `id` (it's the filename and what other memories link to) or
  `created`.
- CRLF, stray blank lines, and reordered keys are all tolerated; the next
  time ghostie writes the file it will come out canonical, with your
  content intact.
