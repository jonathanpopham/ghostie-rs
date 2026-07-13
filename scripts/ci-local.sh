#!/usr/bin/env bash
# Local Linux CI: run the IDENTICAL gate (scripts/verify.sh) inside Linux
# on the dev machine. Docker is the only extra requirement.
#
# - The toolchain comes from rust-toolchain.toml; this script derives the
#   image tag from the same file and refuses to run on a mismatch, so the
#   two can never drift apart silently.
# - target/ is NEVER shared between macOS and Linux (mixed-platform
#   artifacts poison builds): the container builds into target-linux/.
# - Prefers the long-running `geist-ci` container when present (asserting
#   its rustc matches the pin); falls back to a throwaway `rust:<pin>-slim`
#   container otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."

PIN="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
if [ -z "$PIN" ]; then
  echo "ci-local: cannot read the pinned channel from rust-toolchain.toml" >&2
  exit 1
fi

if docker ps --format '{{.Names}}' | grep -qx geist-ci; then
  GOT="$(docker exec geist-ci rustc --version | awk '{print $2}')"
  if [ "$GOT" != "$PIN" ]; then
    echo "ci-local: geist-ci has rustc $GOT but rust-toolchain.toml pins $PIN — refusing to run" >&2
    exit 1
  fi
  docker exec geist-ci bash -c 'command -v git >/dev/null' \
    || { echo "ci-local: git missing in geist-ci (sync e2e tests need it)" >&2; exit 1; }
  exec docker exec -e CARGO_TARGET_DIR=/work/ghostie-rs/target-linux \
    -w /work/ghostie-rs geist-ci bash scripts/verify.sh
fi

IMAGE="rust:${PIN}-slim"
echo "ci-local: geist-ci not running; using throwaway container $IMAGE"
exec docker run --rm -v "$PWD":/work -w /work \
  -e CARGO_TARGET_DIR=/work/target-linux \
  "$IMAGE" bash -c '
    set -euo pipefail
    command -v git >/dev/null || { echo "git missing in image"; exit 1; }
    rustup component add rustfmt clippy >/dev/null 2>&1 || true
    ./scripts/verify.sh
  '
