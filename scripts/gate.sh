#!/usr/bin/env bash
# gate.sh — the idea-vault no-mistakes gate (ADR-0041, D41, docs/14-no-mistakes-gate.md).
#
#   gate.sh | gate.sh --list | gate.sh --install-hook
#
# Runs a fixed pipeline, in order, stopping on the first red step:
#   1. intent      — the top docs/INTENT.md block has acceptance bullets and an ADR/D token, and
#                    on a branch it changed since merge-base(HEAD, main)
#   2. invariants  — scripts/check-invariants.sh --strict (collect-all, WARN promoted)
#   3. build       — cargo build
#   4. tests       — cargo test, refused while PARSER_CORPUS_BLESS is set
#   5. fmt         — cargo fmt --check
#   6. clippy      — cargo clippy --all-targets -D warnings
#   7. honesty     — every changed fixture/snapshot, rising floor and removed or downgraded
#                    invariant id is declared under `## Expectation changes`
# Budget is deliberately not a step: there is no metered spend (Ollama is local, claude runs on a
# subscription) and the runtime cap is the workflow CallBudget (ADR-0034).
#
# There is no run-time option: no --skip, no --from, no env seam. A green gate means every step
# ran; after any fix the whole gate re-runs from step 1. None of the steps needs the network.
set -euo pipefail
cd "$(dirname "$0")/.."

HOOK_MARKER='# idea-vault gate pre-push hook v1'

STEPS='1|intent|top intent block: acceptance bullets, an ADR/D token, changed since main on a branch
2|invariants|scripts/check-invariants.sh --strict
3|build|cargo build
4|tests|cargo test (red first if PARSER_CORPUS_BLESS is set)
5|fmt|cargo fmt --check
6|clippy|cargo clippy --all-targets -- -D warnings
7|honesty|changed fixtures, snapshots, rising floors and removed/downgraded rules are declared'

usage() {
    printf 'gate.sh: %s\n' "$1" >&2
    printf 'usage: gate.sh | gate.sh --list | gate.sh --install-hook\n' >&2
    printf '  (no flag)       run the fixed 7-step pipeline\n' >&2
    printf '  --list          print the step table and exit\n' >&2
    printf '  --install-hook  install the pre-push hook (runs check-invariants.sh --strict)\n' >&2
    exit 2
}

step() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
fail() {
    printf '\033[31mGATE FAILED at: %s\033[0m\n' "$1"
    exit 1
}

install_hook() {
    local hooks hook
    hooks=$(git rev-parse --git-path hooks) || fail "install-hook (not inside a git repository)"
    [ -d "$hooks" ] || fail "install-hook ($hooks is not a directory)"
    hook="$hooks/pre-push"
    if [ -e "$hook" ] && ! grep -qxF "$HOOK_MARKER" "$hook"; then
        fail "install-hook ($hook exists and is not the idea-vault hook; refusing to overwrite it)"
    fi
    # --strict matches gate step 2 (owner decision D-d): a push never carries a WARN either.
    cat >"$hook" <<EOF
#!/usr/bin/env bash
$HOOK_MARKER
# Installed by scripts/gate.sh --install-hook (ADR-0041); re-run that command to update it.
exec bash "\$(git rev-parse --show-toplevel)/scripts/check-invariants.sh" --strict
EOF
    chmod 0755 "$hook"
    echo "  installed $hook"
}

case $# in
    0) ;;
    1)
        case "$1" in
            --list)
                printf '%s\n' "$STEPS"
                exit 0
                ;;
            --install-hook)
                install_hook
                exit 0
                ;;
            *) usage "unknown argument: $1" ;;
        esac
        ;;
    *) usage "flags are used alone: $*" ;;
esac

# The top intent block: from the first `# ` heading to the next one.
intent_block() { awk '/^# / { n++ } n == 1' docs/INTENT.md; }

# The `- <item>: <why>` items under a `## <heading>` of the top intent block.
intent_items() {
    intent_block | awk -v h="## $1" '$0 == h { on = 1; next } /^## / { on = 0 } on && /^- /'
}

step "1/7 intent"
[ -s docs/INTENT.md ] || fail "intent (docs/INTENT.md missing or empty)"
block=$(intent_block)
head -1 <<<"$block" | grep -q '^# Intent' || fail "intent (the first heading of docs/INTENT.md is not a '# Intent' block)"
[ -n "$(intent_items 'Acceptance criteria')" ] ||
    fail "intent (the top intent block has no '## Acceptance criteria' with a '- ' bullet)"
grep -qE 'ADR-[0-9]{4}|\bD[0-9]+\b' <<<"$block" ||
    fail "intent (the top intent block names no ADR-NNNN or D<n>)"
git rev-parse --verify -q main >/dev/null ||
    fail "intent (no 'main' ref to check freshness against; run: git fetch origin main:main)"
base=$(git merge-base HEAD main) || fail "intent (no merge-base between HEAD and main)"
# "On main" is the main branch itself (or a detached HEAD at the merge-base), not merely HEAD ==
# merge-base: a new branch before its first commit sits at the base, and the gate runs before
# that commit, so freshness and honesty diff the working tree against the base there too.
on_main=0
branch=$(git symbolic-ref -q --short HEAD || true)
if [ "$branch" = main ] || { [ -z "$branch" ] && [ "$(git rev-parse HEAD)" = "$base" ]; }; then
    on_main=1
fi
if [ "$on_main" = 1 ]; then
    echo "  top intent block well-formed; freshness skipped on main"
else
    if git diff --quiet "$base" -- docs/INTENT.md; then
        fail "intent (docs/INTENT.md unchanged since main: write this branch's intent first)"
    fi
    echo "  top intent block well-formed and changed since main"
fi

step "2/7 invariants (collect-all, --strict)"
bash scripts/check-invariants.sh --strict || fail "invariants"

step "3/7 build"
cargo build --quiet || fail "cargo build"

step "4/7 tests"
[ -z "${PARSER_CORPUS_BLESS+x}" ] ||
    fail "tests (PARSER_CORPUS_BLESS set: blessing is an ask-user change, never inside the gate)"
cargo test --quiet || fail "cargo test"

step "5/7 fmt"
cargo fmt --check || fail "cargo fmt --check"

step "6/7 clippy (-D warnings)"
cargo clippy --all-targets --quiet -- -D warnings || fail "cargo clippy"

step "7/7 honesty (expectation changes declared)"
if [ "$on_main" = 1 ]; then
    echo "  skipped on main: there is no merge-base to diff against"
else
    surface=()
    # Changed or new fixtures and snapshots since the merge-base, working tree included.
    while IFS= read -r p; do
        if [ -n "$p" ]; then surface+=("$p"); fi
    done < <({
        git diff --name-only "$base" -- tests/fixtures ':(glob)tests/**/*.snap'
        git ls-files --others --exclude-standard -- tests/fixtures ':(glob)tests/**/*.snap'
    } | sort -u)
    # Rising ratchet floors. A floor new since the base is a new rule, not a rise.
    old_script=$(git show "$base:scripts/check-invariants.sh" 2>/dev/null || true)
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        name=${line%%=*}
        old=$(grep -oE "^${name}=[0-9]+" <<<"$old_script" | head -1 || true)
        if [ -n "$old" ] && [ "${line#*=}" -gt "${old#*=}" ]; then surface+=("$name"); fi
    done < <(grep -oE '^[A-Z_]+_FLOOR=[0-9]+' scripts/check-invariants.sh || true)
    # Catalog ids removed or downgraded; skipped when the base script predates --list.
    if grep -qF -- '--list)' <<<"$old_script"; then
        tmp=$(mktemp)
        printf '%s\n' "$old_script" >"$tmp"
        old_list=$(bash "$tmp" --list) || fail "honesty (the base check-invariants.sh --list failed)"
        rm -f "$tmp"
        new_list=$(bash scripts/check-invariants.sh --list) || fail "honesty (check-invariants.sh --list failed)"
        rank() { case "$1" in error) echo 3 ;; warn) echo 2 ;; *) echo 1 ;; esac; }
        while IFS='|' read -r id sev _; do
            [ -n "$id" ] || continue
            now=$(awk -F'|' -v id="$id" '$1 == id { print $2 }' <<<"$new_list")
            if [ -z "$now" ] || [ "$(rank "$now")" -lt "$(rank "$sev")" ]; then
                surface+=("$id")
            fi
        done <<<"$old_list"
    else
        echo "  catalog diff skipped: the base check-invariants.sh has no --list"
    fi
    declared=()
    while IFS= read -r item; do
        item=${item#- }
        name=${item%%: *}
        if [ "$name" != "$item" ] && [ -n "${item#*: }" ]; then declared+=("${name//\`/}"); fi
    done < <(intent_items 'Expectation changes')
    missing=()
    for s in "${surface[@]+"${surface[@]}"}"; do
        ok=0
        for d in "${declared[@]+"${declared[@]}"}"; do
            if [ "$s" = "$d" ] || { [ "${d%/}" != "$d" ] && [ "${s#"$d"}" != "$s" ]; }; then
                ok=1
                break
            fi
        done
        if [ "$ok" = 0 ]; then missing+=("$s"); fi
    done
    if [ ${#missing[@]} -gt 0 ]; then
        printf '  undeclared expectation change: %s\n' "${missing[@]}"
        fail "honesty (list each under '## Expectation changes' in the top intent block as '- <path or id>: <why>')"
    fi
    echo "  ${#surface[@]} expectation change(s), all declared"
fi

printf '\n\033[32mGATE PASSED — intent + invariants + build + tests + fmt + clippy + honesty green.\033[0m\n'
