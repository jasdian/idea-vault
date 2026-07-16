#!/usr/bin/env bash
# gate.sh — the idea-vault no-mistakes gate (td-bot convention, adapted).
#
# Runs the fixed pipeline, in order, stopping on the first red step:
#   1. intent      — docs/INTENT.md exists and is non-empty (written before the code)
#   2. invariants  — scripts/check-invariants.sh (greps that protect the ADRs)
#   3. build       — cargo build
#   4. tests       — cargo test
#   5. fmt         — cargo fmt --check
#   6. lint        — cargo clippy -D warnings
#
# None of the steps need the network — Ollama does not have to be running.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
fail() { printf '\033[31mGATE FAILED at: %s\033[0m\n' "$1"; exit 1; }

step "1/6 intent"
[ -s docs/INTENT.md ] && echo "  docs/INTENT.md present and non-empty" || fail "intent (docs/INTENT.md missing or empty)"

step "2/6 invariants (ADR-protecting checks)"
bash scripts/check-invariants.sh || fail "invariants"

step "3/6 build"
cargo build --quiet || fail "cargo build"

step "4/6 tests"
cargo test --quiet || fail "cargo test"

step "5/6 fmt"
cargo fmt --check || fail "cargo fmt --check"

step "6/6 lint (clippy -D warnings)"
cargo clippy --all-targets --quiet -- -D warnings || fail "cargo clippy"

printf '\n\033[32mGATE PASSED — intent + invariants + build + tests + fmt + lint green.\033[0m\n'
