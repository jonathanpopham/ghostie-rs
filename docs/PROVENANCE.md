# Provenance thread: hash-chained, replayable memory lineage

Every memory write appends one record to an append-only, hash-chained log, so a
memory's origin is verifiable and the whole history can be replayed and checked
for tampering. This is ghostie's "prove" surface and the stack's safe IP lane:
a clean, Lockstep-style lineage built with zero dependencies and zero parsing of
anyone else's program.

## Where it lives

`<store>/.provenance/log.jsonl`, one JSON object per line, in write order.

## The record

Each line is a byte-stable canonical JSON object with these fields, in this
fixed order:

| field          | meaning                                                          |
| -------------- | ---------------------------------------------------------------- |
| `seq`          | 1-based monotonic sequence across the whole log                  |
| `prev_hash`    | the previous record's `entry_hash` (the chain link); genesis is sixteen hex zeros |
| `memory_id`    | which memory the record is about                                 |
| `event`        | `created`, `updated`, or `captured`                              |
| `content_hash` | FNV-1a of the memory's exact canonical file bytes at write time  |
| `source`       | the memory's `source` field, or `null`                          |
| `harness`      | the memory's `harness` field, or `null`                         |
| `core`         | the memory's `core` field, or `null`                            |
| `created`      | the event instant, RFC3339 UTC, from the injected clock          |
| `entry_hash`   | `fnv1a64_hex(prev_hash + canonical_payload)`                     |

The `canonical_payload` is every field above EXCEPT `prev_hash` and
`entry_hash`, emitted by the crate's deterministic JSON codec (fixed order, no
whitespace). Because the entry hash folds in `prev_hash`, each record commits to
the entire history before it.

## Events

- `created` comes from `ghostie remember` (a new memory from the CLI).
- `captured` comes from `ghostie capture` (a provider-agnostic session record).
- `updated` comes from any rewrite of an existing memory through the store.

The emit is best-effort on the write path: a provenance-log failure never loses
a memory write, mirroring how the derived index is refreshed. The log is
surfaced and checked by `ghostie provenance verify`.

## Verifying

```
ghostie provenance <memory-id>   # show one memory's lineage
ghostie provenance verify        # replay the whole chain
```

`verify` reports `INTACT` (exit 0) or the first `BROKEN` link by sequence
number (exit 1), so a script or a CI gate fails on tampering. It runs two
independent checks:

1. Chain integrity. In sequence order, each record's `prev_hash` must equal the
   previous record's `entry_hash`, and its recomputed `entry_hash` must equal
   the stored one. Editing any field of any past record (including a historical
   `content_hash`) breaks this.
2. Content tamper. For each memory's latest record, the live file's bytes must
   still hash to the recorded `content_hash`. A memory edited outside the store
   API (a raw hand-edit, not a re-`update`) is caught here. A deleted memory
   (file absent) is legitimate, not tampering: git history is the tombstone.

To re-bless a deliberate hand-edit, write it back through an update so a fresh
`updated` record chains onto the tail.

## Determinism

The canonical payload is compact deterministic JSON, the timestamp comes from
the injected clock (frozen under `GHOSTIE_TEST_CLOCK`), and the content hash is
FNV-1a of the exact stored bytes. Two identical runs produce byte-identical
logs. The gate asserts this: it seeds two stores from the same scripted
sequence and byte-compares their logs, then verifies the chain is INTACT through
the real binary.

## Sync decision: the log syncs

Unlike the rebuildable `.index/` (gitignored, an optimization that is never
authoritative), the provenance log is the evidence, so it travels with your
memories through your own git remote and is NOT gitignored. `ghostie sync`
stages it with the memories. Lineage is portable across every device that syncs
the store.

## The Lockstep-lineage framing

The provenance thread is the same shape as a Lockstep behavioral-equivalence
certificate: deterministic evidence, black-box verifiable, tamper-evident. It
records what was written and when, chains each record to the last, and lets any
party replay the chain and confirm nothing was forged, all with std-only Rust
and no parsing of anyone else's program. That keeps the lineage clean IP and a
portfolio-grade artifact.
