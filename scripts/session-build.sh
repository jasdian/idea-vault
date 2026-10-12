#!/usr/bin/env bash
# Fetch the locked dependencies and build the release binary in a sandbox whose egress may block
# crates.io's sparse index (index.crates.io) while still allowing GitHub and static.crates.io, as a
# Claude Code on the web environment's network policy can. scripts/session-up.sh runs it when the
# binary is missing; it also works as a cloud environment's setup script (docs/12 "Agent
# sessions"). Harmless anywhere else.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if curl -fsS -m 8 -o /dev/null https://index.crates.io/config.json 2>/dev/null; then
  echo "crates.io sparse index reachable"
else
  # Fall back to the git index on GitHub; the crate files themselves still come from
  # static.crates.io. The git CLI honours the sandbox's proxy and CA settings.
  echo "crates.io sparse index unreachable; using the git index"
  export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=git
  export CARGO_NET_GIT_FETCH_WITH_CLI=true
fi

cargo fetch --locked
cargo build --release --locked
