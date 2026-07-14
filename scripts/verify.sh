#!/usr/bin/env bash
# The gate. One entrypoint; CI runs exactly this (gatekit pattern).
# Steps: fmt, clippy -D warnings, build, test, DOGFOOD (seed fixture
# memories through the real binary, recall labeled queries, assert),
# BYTE-STABILITY (two stores byte-identical under GHOSTIE_TEST_CLOCK;
# canonicalization idempotence; recall/index output determinism), and
# POLICY GUARDS (zero deps, forbid unsafe, robot mode everywhere, no
# floats in scoring, LF discipline).
set -euo pipefail
cd "$(dirname "$0")/.."
pass=0; fail=0
step(){ if "$@"; then echo "PASS: $*"; pass=$((pass+1)); else echo "FAIL: $*"; fail=$((fail+1)); fi; }
run_step(){ local name="$1"; shift; if "$@"; then echo "PASS: $name"; pass=$((pass+1)); else echo "FAIL: $name"; fail=$((fail+1)); fi; }

echo "==> fmt";    step cargo fmt --all -- --check
echo "==> clippy"; step cargo clippy --all-targets -- -D warnings
echo "==> build";  step cargo build --all-targets
echo "==> test";   step cargo test

BIN="${CARGO_TARGET_DIR:-target}/debug/ghostie"
# The hidden test clock makes every run reproducible: created timestamps
# are frozen, so two runs can demand BYTE-IDENTICAL trees.
export GHOSTIE_TEST_CLOCK="2026-07-13T12:00:00Z"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/ghostie-gate.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT

# Seed a store from the dogfood corpus THROUGH the real binary: fact/
# decision/rule via `ghostie remember` (exercising the real write path);
# the two session-summary fixtures are copied verbatim because remember
# rejects that type by design — they are capture's output, and capture is
# post-milestone. Fixture bodies are fed via stdin (--body -).
seed_store(){ # $1 = store dir
  local store="$1" f id title tags type body
  mkdir -p "$store/memories"
  for f in tests/fixtures/dogfood/memories/session-summary-*.md; do
    cp "$f" "$store/memories/"
  done
  for f in tests/fixtures/dogfood/memories/fact-*.md \
           tests/fixtures/dogfood/memories/decision-*.md \
           tests/fixtures/dogfood/memories/rule-*.md; do
    type="$(sed -n 's/^type: //p' "$f" | head -1)"
    title="$(sed -n 's/^title: //p' "$f" | head -1)"
    title="${title#\"}"; title="${title%\"}"; title="${title//\\\"/\"}"
    tags="$(sed -n 's/^tags: \[\(.*\)\]/\1/p' "$f" | head -1 | tr -d ' ')"
    body="$(awk 'c>=2 {print} /^---$/ && c<2 {c++}' "$f")"
    printf '%s' "$body" | "$BIN" --store "$store" remember \
      --type "$type" "$title" ${tags:+--tags "$tags"} --body - --quiet >/dev/null
  done
}

# ---------- DOGFOOD ----------
# Replays the milestone expectations through the compiled binary. The
# exhaustive version (full expectations.json semantics) runs in cargo
# test (tests/dogfood.rs); this step proves the REAL BINARY in a clean
# shell answers the same task-shaped questions.
dogfood(){
  local store="$SCRATCH/dogfood" ok=0 bad=0
  seed_store "$store"
  local n
  n="$("$BIN" --store "$store" list --json | grep -o '"count":[0-9]*' | cut -d: -f2)"
  if [ "$n" != "14" ]; then echo "  seed FAILED: expected 14 memories, got $n"; return 1; fi
  # query|expected-rank1-id  (empty id = expect zero hits)
  local cases=(
    'which branch do we sync to|rule-sync-branch-is-sync-never-main-1'
    'why did we avoid floats|decision-chose-fixed-point-scoring-over-floats-1'
    'what do I run before committing|rule-always-run-verify-sh-before-commit-1'
    'parse event stream|fact-parseeventstream-is-the-hot-path-1'
    'what did the last session change in the tokenizer|session-summary-fixed-tokenizer-bug-dropping-digits-1'
    'where does the default config live|fact-default-config-path-is-etc-lantern-toml-1'
    'rules about logging user data|rule-never-log-raw-user-paths-1'
    'storage engine sqlite decision|decision-sqlite-rejected-for-storage-1'
    'where do built binaries end up|fact-release-artifacts-live-in-dist-1'
    'how do we version a release|rule-tag-releases-with-semver-1'
    'kubernetes ingress warp drive|'
  )
  local case query want out
  for case in "${cases[@]}"; do
    query="${case%%|*}"; want="${case#*|}"
    out="$("$BIN" --store "$store" recall "$query" --json)"
    if [ -z "$want" ]; then
      if [[ "$out" == *'"hits":[]'* ]]; then
        echo "  PASS query: $query (zero hits, as labeled)"; ok=$((ok+1))
      else
        echo "  FAIL query: $query"; echo "    expected zero hits"; echo "    got: $out"; bad=$((bad+1))
      fi
      continue
    fi
    # Rank 1 = first element of the hits array (compact JSON, stable).
    # (Per-hit why validity -- lexical vs graph vs semantic -- is exhaustively
    # checked in cargo tests/dogfood.rs; here we only assert rank-1, since
    # graph and embedding-reached hits legitimately carry empty matched_terms.)
    if [[ "$out" == *"\"hits\":[{\"id\":\"$want\""* ]]; then
      echo "  PASS query: $query -> $want"; ok=$((ok+1))
    else
      echo "  FAIL query: $query"
      echo "    expected rank 1: $want"
      echo "    got order: $(echo "$out" | grep -o '"id":"[^"]*"' | head -3 | tr '\n' ' ')"
      bad=$((bad+1))
    fi
  done
  echo "  dogfood: $ok passed, $bad failed"
  [ "$bad" -eq 0 ]
}
echo "==> dogfood"; run_step "dogfood (seed via CLI + labeled recalls)" dogfood

# ---------- BYTE-STABILITY ----------
byte_stability(){
  # 1. Store determinism: two stores from the same scripted remember
  #    sequence (GHOSTIE_TEST_CLOCK freezes created) -> identical trees.
  seed_store "$SCRATCH/bs-a"
  seed_store "$SCRATCH/bs-b"
  if ! diff -r "$SCRATCH/bs-a/memories" "$SCRATCH/bs-b/memories" >/dev/null; then
    echo "  store trees differ between two identical runs:"
    diff -r "$SCRATCH/bs-a/memories" "$SCRATCH/bs-b/memories" | head -10
    return 1
  fi
  echo "  PASS store determinism (two runs, byte-identical memories/ trees)"
  # 2. Canonicalization idempotence: dogfood corpus is canonical; a
  #    re-canonicalize write pass must change nothing.
  mkdir -p "$SCRATCH/bs-canon/memories"
  cp tests/fixtures/dogfood/memories/*.md "$SCRATCH/bs-canon/memories/"
  "$BIN" --store "$SCRATCH/bs-canon" _recanon --quiet >/dev/null
  if ! diff -r tests/fixtures/dogfood/memories "$SCRATCH/bs-canon/memories" >/dev/null; then
    echo "  re-canonicalization changed canonical files:"
    diff -r tests/fixtures/dogfood/memories "$SCRATCH/bs-canon/memories" | head -10
    return 1
  fi
  echo "  PASS canonicalization idempotence"
  # 3. Recall output determinism: byte-compare two runs of two queries.
  local q
  for q in "which branch do we sync to" "why did we avoid floats"; do
    "$BIN" --store "$SCRATCH/bs-a" recall "$q" --json > "$SCRATCH/r1.json"
    "$BIN" --store "$SCRATCH/bs-a" recall "$q" --json > "$SCRATCH/r2.json"
    if ! cmp -s "$SCRATCH/r1.json" "$SCRATCH/r2.json"; then
      echo "  recall output differs across runs for: $q"; return 1
    fi
  done
  echo "  PASS recall output determinism"
  # 4. Index determinism: force rebuild twice, compare bytes.
  "$BIN" --store "$SCRATCH/bs-a" list --rebuild-index --quiet >/dev/null
  cp "$SCRATCH/bs-a/.index/index.json" "$SCRATCH/i1.json"
  "$BIN" --store "$SCRATCH/bs-a" list --rebuild-index --quiet >/dev/null
  if ! cmp -s "$SCRATCH/i1.json" "$SCRATCH/bs-a/.index/index.json"; then
    echo "  index bytes differ across rebuilds"; return 1
  fi
  echo "  PASS index determinism"
}
echo "==> byte-stability"; run_step "byte-stability (store, canon, recall, index)" byte_stability

# ---------- POLICY GUARDS ----------
guard_zero_deps(){
  # Two independent checks: either file alone can be gamed accidentally.
  local deps
  deps="$(awk '/^\[(dependencies|dev-dependencies|build-dependencies)\]/{s=1;next} /^\[/{s=0} s && NF && $1 !~ /^#/' Cargo.toml)"
  if [ -n "$deps" ]; then echo "  Cargo.toml has dependencies:"; echo "$deps"; return 1; fi
  local pkgs
  pkgs="$(grep -c '^\[\[package\]\]' Cargo.lock)"
  if [ "$pkgs" != "1" ]; then echo "  Cargo.lock has $pkgs packages (expected exactly 1: ghostie)"; return 1; fi
}
echo "==> policy: zero-deps"; run_step "guard zero-deps" guard_zero_deps

guard_forbid_unsafe(){
  grep -q '^#!\[forbid(unsafe_code)\]' src/lib.rs || { echo "  src/lib.rs missing #![forbid(unsafe_code)]"; return 1; }
  grep -q '^#!\[forbid(unsafe_code)\]' src/main.rs || { echo "  src/main.rs missing #![forbid(unsafe_code)]"; return 1; }
}
echo "==> policy: forbid-unsafe"; run_step "guard forbid-unsafe" guard_forbid_unsafe

guard_robot_mode(){
  # Every verb the binary reports must answer --json with exit 0 and a
  # single-line JSON document. Deep validation lives in cargo e2e tests;
  # this proves no verb ships without robot mode.
  # The contract: every verb answers --json with exactly one JSON envelope,
  # even when the args yield a usage error (errors still emit the envelope).
  # HOME is redirected to a scratch dir so setup/hook never touch ~/.claude.
  local store="$SCRATCH/robot" home="$SCRATCH/robot-home" sub out
  mkdir -p "$store" "$home"
  printf '%s\n' '{"type":"user","sessionId":"probe","message":{"role":"user","content":"MEMORY fact: probe"}}' > "$store/t.jsonl"
  for sub in $("$BIN" _subcommands); do
    case "$sub" in
      setup)    out="$(HOME="$home" "$BIN" --store "$store" setup --json || true)" ;;
      remember) out="$("$BIN" --store "$store" remember --type fact "robot probe" --json || true)" ;;
      recall)   out="$("$BIN" --store "$store" recall "probe" --json || true)" ;;
      list)     out="$("$BIN" --store "$store" list --json || true)" ;;
      capture)  out="$("$BIN" --store "$store" capture "$store/t.jsonl" --json || true)" ;;
      sync)     out="$("$BIN" --store "$store" sync --json || true)" ;;
      hook)     out="$(HOME="$home" "$BIN" --store "$store" hook status --json || true)" ;;
      # Bare `mcp` prints a one-shot manifest and exits; NEVER `mcp serve`,
      # which is a long-running stdin loop and would block the audit.
      mcp)      out="$("$BIN" --store "$store" mcp --json || true)" ;;
      *) echo "  new subcommand '$sub' has no robot audit case — add one"; return 1 ;;
    esac
    if [ -z "$out" ] || [ "${out:0:1}" != "{" ] || [ "$(printf '%s\n' "$out" | wc -l)" -ne 1 ]; then
      echo "  $sub --json did not produce one JSON line: $out"; return 1
    fi
    echo "  PASS robot mode: $sub"
  done
}
echo "==> policy: robot-mode"; run_step "guard robot-mode audit" guard_robot_mode

guard_no_floats(){
  # Floats are banned in scoring paths. Allowlist: a same-line
  # `// gate-allow-float: <reason>` comment (expected count today: zero).
  local hits
  hits="$(grep -rn 'f32\|f64' src/recall/ src/util.rs | grep -v 'gate-allow-float' || true)"
  if [ -n "$hits" ]; then echo "  float types in scoring paths:"; echo "$hits"; return 1; fi
}
echo "==> policy: no-floats"; run_step "guard no-floats-in-scoring" guard_no_floats

guard_lf_discipline(){
  # find+grep instead of git grep: the gate must also run in containers
  # where the bind mount trips git's ownership check.
  local crs
  crs="$(grep -rIl $'\r' --exclude-dir=.git --exclude-dir=target --exclude-dir=target-linux --exclude-dir=.beads . || true)"
  if [ -n "$crs" ]; then echo "  CR bytes in tracked files:"; echo "$crs"; return 1; fi
}
echo "==> policy: lf-discipline"; run_step "guard lf-discipline" guard_lf_discipline

echo "=============================="
if [ "$fail" -eq 0 ]; then echo " VERIFY: PASS ($pass steps)"; else echo " VERIFY: FAIL ($fail failed)"; exit 1; fi
