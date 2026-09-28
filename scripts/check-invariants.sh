#!/usr/bin/env bash
# check-invariants.sh — greps that protect the architecture decisions.
#
# Each check is one grep (or one test -f loop) with a failure message naming
# the ADR / house rule it protects. Exits nonzero on the first violation;
# prints "ok — <rule>" per green check. Called as step 2 of scripts/gate.sh.
#
# Rust-source checks exclude comment lines (content starting with // after
# optional whitespace): two doc comments in src/ai/mod.rs and src/ai/ollama.rs
# legitimately MENTION localhost:11434 while documenting why it is never
# hardcoded. Compose checks likewise skip YAML # comment lines — the vault
# bind's comment block quotes the forbidden short form to explain the pitfall.
set -euo pipefail
cd "$(dirname "$0")/.."

fail() { printf '\033[31mINVARIANT VIOLATED — %s\033[0m\n' "$1"; [ -n "${2:-}" ] && printf '%s\n' "$2"; exit 1; }
ok() { printf 'ok — %s\n' "$1"; }

# grep -rn output is file:line:content; drop lines whose content starts with //
# after optional whitespace (covers //, //!, ///).
strip_rust_comments() { grep -vE '^[^:]*:[0-9]+:[[:space:]]*//' || true; }

# 1. Never hardcode the Ollama endpoint (CLAUDE.md / docs/12-deployment.md):
#    it breaks the containerized run, where the URL is http://ollama:11434.
rule="no hardcoded 'localhost:11434' in src/ outside src/config.rs (CLAUDE.md, docs/12-deployment.md)"
hits=$(grep -rn 'localhost:11434' src --include='*.rs' | grep -v '^src/config\.rs:' | strip_rust_comments) || true
[ -z "$hits" ] || fail "$rule" "$hits"
ok "$rule"

# 2. Never hardcode a bind address — IDEA_VAULT_BIND is env-driven (CLAUDE.md).
rule="no hardcoded '127.0.0.1:3000' / '0.0.0.0:3000' in src/ outside src/config.rs (CLAUDE.md)"
hits=$(grep -rnE '127\.0\.0\.1:3000|0\.0\.0\.0:3000' src --include='*.rs' | grep -v '^src/config\.rs:' | strip_rust_comments) || true
[ -z "$hits" ] || fail "$rule" "$hits"
ok "$rule"

# 3. ADR-0020: restart posture is manual bring-up — every restart: directive
#    must be exactly restart: "no" (quoted, so YAML does not read it as false).
rule='every compose restart: directive is exactly restart: "no" (ADR-0020)'
hits=$(grep -Hn '^[[:space:]]*restart:' docker-compose*.yml | grep -vE ':[0-9]+:[[:space:]]*restart:[[:space:]]*"no"[[:space:]]*$' || true)
[ -z "$hits" ] || fail "$rule" "$hits"
ok "$rule"

# 4. ADR-0019: the vault bind must use long syntax + create_host_path:false —
#    the short './vault:' form lets the daemon auto-create a root-owned ghost
#    dir on a boot race and silently serve an empty vault.
rule="no short-form vault bind './vault:' in compose files (ADR-0019)"
hits=$(grep -Hn '\./vault:' docker-compose*.yml | grep -vE ':[0-9]+:[[:space:]]*#' || true)
[ -z "$hits" ] || fail "$rule" "$hits"
ok "$rule"

# 5. ADR-0020: the app never runs docker — the owner applies compose overrides.
#    Detects INVOCATION (Command::new), not mention: ADR-0020 itself requires the
#    app to TELL the owner to run `docker compose up -d` (the generated override's
#    header, the health warn, the Sources banner), so the command text is
#    legitimate owner-facing copy in string literals.
rule="app never invokes docker: no Command::new(\"docker...\") in src/ (ADR-0020)"
hits=$(grep -rnE 'Command::new\([^)]*docker' src --include='*.rs' | strip_rust_comments) || true
[ -z "$hits" ] || fail "$rule" "$hits"
ok "$rule"

# 6. Docs stay navigable: every ADR path referenced from CLAUDE.md and every
#    relative .md link target in docs/08-diagrams.md must resolve to a file.
rule="all ADR paths in CLAUDE.md and relative .md links in docs/08-diagrams.md resolve"
missing=""
for target in $(grep -oE 'docs/adr/[0-9]{4}[A-Za-z0-9._-]*\.md' CLAUDE.md | sort -u); do
    [ -f "$target" ] || missing="${missing}  ${target} (referenced from CLAUDE.md)"$'\n'
done
for target in $(grep -oE '\]\([^)]*\.md(#[^)]*)?\)' docs/08-diagrams.md | sed -E 's/^\]\(//; s/\)$//; s/#.*$//' | sort -u); do
    case "$target" in http*) continue ;; esac
    [ -f "docs/$target" ] || missing="${missing}  docs/${target} (referenced from docs/08-diagrams.md)"$'\n'
done
[ -z "$missing" ] || fail "$rule" "$missing"
ok "$rule"

# 7. Ratchet: clippy -D warnings counts none of these, so a grep holds the line.
#    A rise needs a same-line justification and a floor bump in this block; a
#    fall lowers the floor in the same commit. Floors measured 2026-09-28.
CLIPPY_ALLOW_FLOOR=6
UNSAFE_FLOOR=0
rule="ratchet: #[allow(clippy::…)] <= $CLIPPY_ALLOW_FLOOR and unsafe blocks <= $UNSAFE_FLOOR in src/"
allows=$({ grep -rn '#\[allow(clippy::' src --include='*.rs' || true; } | strip_rust_comments | wc -l)
unsafes=$({ grep -rnE '\bunsafe[[:space:]]*(\{|fn\b|impl\b)' src --include='*.rs' || true; } | strip_rust_comments | wc -l)
[ "$allows" -le "$CLIPPY_ALLOW_FLOOR" ] || fail "$rule" "  clippy allows: $allows (floor $CLIPPY_ALLOW_FLOOR)"
[ "$unsafes" -le "$UNSAFE_FLOOR" ] || fail "$rule" "  unsafe blocks: $unsafes (floor $UNSAFE_FLOOR)"
ok "$rule"

printf '\033[32mall invariants hold.\033[0m\n'
