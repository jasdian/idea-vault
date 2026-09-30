#!/usr/bin/env bash
# check-invariants.sh — greps that protect the architecture decisions (ADR-0041, D41).
#
#   check-invariants.sh [--strict] [--root <dir>] | --list
#
# Collect-all: every rule in the catalog below runs, in catalog order, and nothing exits early,
# so one run shows every violation instead of the first. Each rule prints
#   [ok] <id> — <zero-state> (<ADR/D>)
# or one line per finding
#   ERROR|WARN|INFO <id>: <target> — <message>
# which tests/gate_invariants.rs parses. --strict promotes WARN to ERROR at emit time (gate step 2
# and the pre-push hook run it); INFO is never counted or promoted. Exit 0 with no error, 1 with
# any, 2 on a usage error. --root checks another tree, so the tests can seed violations in a
# tempdir; every catalog id has a seeded violation there, and a rule added without one goes red.
#
# Rust-source checks drop comment lines (content starting with // after optional whitespace):
# doc comments in src/ai legitimately MENTION localhost:11434 while documenting why it is never
# hardcoded. Compose checks likewise skip YAML # comment lines — the vault bind's comment block
# quotes the forbidden short form to explain the pitfall.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Ratchet floors: clippy -D warnings counts none of these, so a grep holds the line. A rise needs
# a same-line justification, a floor bump here and a `## Expectation changes` entry (gate step 7);
# a fall lowers the floor in the same commit (ratchet-slack, red under --strict).
CLIPPY_ALLOW_FLOOR=5
UNSAFE_FLOOR=0
IGNORE_FLOOR=0

# id|severity|ADR/D|zero-state — the order is the report order. checklist-mirror is an error,
# downgraded to info only when .claude/ is absent (a fresh clone or a worktree without it).
CATALOG='ollama-url|error|CLAUDE.md, docs/12-deployment.md|no hardcoded localhost:11434 in src/ outside src/config.rs
bind-addr|error|CLAUDE.md|no hardcoded 127.0.0.1:3000 or 0.0.0.0:3000 in src/ outside src/config.rs
restart-no|error|ADR-0020|every compose restart: directive is exactly restart: "no"
vault-bind-long|error|ADR-0019|no short-form ./vault: bind in compose files
no-docker-exec|error|ADR-0020|no Command::new("docker…") in src/
doc-links|error|CLAUDE.md, D-catalog|every ADR path in CLAUDE.md and every relative .md link in docs/08-diagrams.md resolves
ratchet|error|HTC-9|clippy allows and unsafe blocks at or under their floors
d4-config|error|D4|ai, domain, mcp and sources never import crate::config
d4-web-app|error|D4|nothing under src/web imports crate::app
ratchet-slack|warn|HTC-9|no count below its floor (a fall lowers the floor in the same commit)
ignore-ratchet|error|TST-6|#[ignore] count in src/ and tests/ at or under IGNORE_FLOOR
doc-ranges|error|D-catalog, ADR index|CLAUDE.md D1–Dnn and ADRs 0001–NNNN name the highest D and ADR
doc-range-gaps|info|D-catalog, ADR index|no unused D or ADR number below the highest
checklist-mirror|error|ADR-0041|every [dev] phrase of the docs/14 checklist is mirrored verbatim in .claude/
intent-archive|info|ADR-0041|docs/INTENT.md holds one intent block'

usage() {
    printf 'check-invariants.sh: %s\n' "$1" >&2
    printf 'usage: check-invariants.sh [--strict] [--root <dir>] | --list\n' >&2
    printf '  --strict      promote WARN findings to ERROR\n' >&2
    printf '  --root <dir>  check <dir> instead of the repository holding this script\n' >&2
    printf '  --list        print the catalog as id|severity|ADR/D|zero-state and exit (alone)\n' >&2
    exit 2
}

strict=0
list=0
root=""
nargs=$#
while [ $# -gt 0 ]; do
    case "$1" in
        --strict) strict=1 ;;
        --list) list=1 ;;
        --root)
            [ $# -ge 2 ] || usage "--root needs a directory"
            root="$2"
            shift
            ;;
        *) usage "unknown argument: $1" ;;
    esac
    shift
done
if [ "$list" = 1 ]; then
    [ "$nargs" = 1 ] || usage "--list takes no other flag"
    printf '%s\n' "$CATALOG"
    exit 0
fi
[ -n "$root" ] || root="$SCRIPT_DIR/.."
[ -d "$root" ] || usage "--root: not a directory: $root"
cd "$root" || usage "--root: cannot enter $root"

errors=0
warns=0
infos=0
cur=()

# found SEVERITY TARGET MESSAGE — record one finding of the rule being checked.
found() { cur+=("$1"$'\t'"$2"$'\t'"$3"); }

# found_hits SEVERITY MESSAGE <<< "file:line:content…" — one finding per grep hit.
found_hits() {
    local h file rest
    while IFS= read -r h; do
        [ -n "$h" ] || continue
        file=${h%%:*}
        rest=${h#*:}
        found "$1" "$file:${rest%%:*}" "$2"
    done
}

catalog_field() { printf '%s\n' "$CATALOG" | awk -F'|' -v id="$1" -v n="$2" '$1 == id { print $n }'; }

# report ID — print the rule's [ok] line or its findings, then reset for the next rule.
report() {
    local id=$1 line sev target msg
    if [ ${#cur[@]} -eq 0 ]; then
        printf '[ok] %s — %s (%s)\n' "$id" "$(catalog_field "$id" 4)" "$(catalog_field "$id" 3)"
    else
        for line in "${cur[@]}"; do
            IFS=$'\t' read -r sev target msg <<<"$line"
            [ "$strict" = 1 ] && [ "$sev" = WARN ] && sev=ERROR
            case "$sev" in
                ERROR) errors=$((errors + 1)) ;;
                WARN) warns=$((warns + 1)) ;;
                *) infos=$((infos + 1)) ;;
            esac
            printf '%s %s: %s — %s\n' "$sev" "$id" "$target" "$msg"
        done
    fi
    cur=()
}

# grep -rn output is file:line:content; drop lines whose content starts with // after optional
# whitespace (covers //, //!, ///).
strip_rust_comments() { grep -vE '^[^:]*:[0-9]+:[[:space:]]*//' || true; }

# The existing ones of the given paths, so a partial tree (a seeded tempdir) greps quietly.
existing() {
    local p
    for p in "$@"; do [ -e "$p" ] && printf '%s\n' "$p"; done
}

shopt -s nullglob
compose=(docker-compose*.yml)
adr_files=(docs/adr/[0-9][0-9][0-9][0-9]-*.md)
shopt -u nullglob

rs_grep() { # rs_grep <grep -E pattern> <path>… — Rust hits outside comments
    local pat=$1
    shift
    local paths
    mapfile -t paths < <(existing "$@")
    [ ${#paths[@]} -gt 0 ] || return 0
    grep -rnE "$pat" "${paths[@]}" --include='*.rs' 2>/dev/null | strip_rust_comments
}

# ollama-url: the containerized run reaches Ollama at http://ollama:11434, so the URL is config.
found_hits ERROR "hardcoded Ollama URL; read IDEA_VAULT_OLLAMA_URL in config.rs" \
    < <(rs_grep 'localhost:11434' src | grep -v '^src/config\.rs:')
report ollama-url

# bind-addr: IDEA_VAULT_BIND is env-driven; a literal bind breaks the in-container 0.0.0.0 run.
found_hits ERROR "hardcoded bind address; read IDEA_VAULT_BIND in config.rs" \
    < <(rs_grep '127\.0\.0\.1:3000|0\.0\.0\.0:3000' src | grep -v '^src/config\.rs:')
report bind-addr

# restart-no (ADR-0020): restart posture is manual bring-up, quoted so YAML does not read false.
if [ ${#compose[@]} -gt 0 ]; then
    found_hits ERROR 'restart: must be exactly restart: "no"' \
        < <(grep -Hn '^[[:space:]]*restart:' "${compose[@]}" |
            grep -vE ':[0-9]+:[[:space:]]*restart:[[:space:]]*"no"[[:space:]]*$')
fi
report restart-no

# vault-bind-long (ADR-0019): the short './vault:' form lets the daemon auto-create a root-owned
# ghost dir on a boot race and silently serve an empty vault.
if [ ${#compose[@]} -gt 0 ]; then
    found_hits ERROR "short-form vault bind; use long syntax with create_host_path: false" \
        < <(grep -Hn '\./vault:' "${compose[@]}" | grep -vE ':[0-9]+:[[:space:]]*#')
fi
report vault-bind-long

# no-docker-exec (ADR-0020): detects INVOCATION, not mention — the app must TELL the owner to run
# `docker compose up -d`, so the command text is legitimate owner-facing copy.
found_hits ERROR "the app never runs docker; the owner applies compose overrides" \
    < <(rs_grep 'Command::new\([^)]*docker' src)
report no-docker-exec

# doc-links: every ADR path in CLAUDE.md and relative .md link in docs/08-diagrams.md resolves.
if [ -f CLAUDE.md ]; then
    for target in $(grep -oE 'docs/adr/[0-9]{4}[A-Za-z0-9._-]*\.md' CLAUDE.md | sort -u); do
        [ -f "$target" ] || found ERROR CLAUDE.md "links $target, which does not exist"
    done
fi
if [ -f docs/08-diagrams.md ]; then
    for target in $(grep -oE '\]\([^)]*\.md(#[^)]*)?\)' docs/08-diagrams.md |
        sed -E 's/^\]\(//; s/\)$//; s/#.*$//' | sort -u); do
        case "$target" in http*) continue ;; esac
        [ -f "docs/$target" ] || found ERROR docs/08-diagrams.md "links $target, which does not exist"
    done
fi
report doc-links

# The three ratchet counts, shared by ratchet, ratchet-slack and ignore-ratchet.
allows=$(rs_grep '#\[allow\(clippy::' src | wc -l)
unsafes=$(rs_grep '\bunsafe[[:space:]]*(\{|fn\b|impl\b)' src | wc -l)
ignore_hits=$(rs_grep '#\[ignore(\]|[[:space:]]*=)' src tests)
ignores=$(printf '%s' "$ignore_hits" | grep -c . || true)

# ratchet (HTC-9): a count above its floor.
[ "$allows" -le "$CLIPPY_ALLOW_FLOOR" ] ||
    found ERROR src/ "clippy allows: $allows > CLIPPY_ALLOW_FLOOR $CLIPPY_ALLOW_FLOOR"
[ "$unsafes" -le "$UNSAFE_FLOOR" ] ||
    found ERROR src/ "unsafe blocks: $unsafes > UNSAFE_FLOOR $UNSAFE_FLOOR"
report ratchet

# d4-config (D4, docs/02-module-reference.md): the lower layers never reach up into `config`.
found_hits ERROR "imports crate::config (D4: config is a bin-level leaf)" \
    < <(rs_grep '\bconfig::|crate::\{[^}]*\bconfig\b' src/ai src/domain src/mcp.rs src/sources.rs)
report d4-config

# d4-web-app (D4): only app → web, never back.
found_hits ERROR "imports crate::app (D4: only app → web)" \
    < <(rs_grep '\bapp::|crate::\{[^}]*\bapp\b' src/web)
report d4-web-app

# ratchet-slack: a count below its floor means the floor was not lowered with the fix.
[ "$allows" -ge "$CLIPPY_ALLOW_FLOOR" ] || found WARN scripts/check-invariants.sh \
    "clippy allows: $allows < CLIPPY_ALLOW_FLOOR $CLIPPY_ALLOW_FLOOR; lower the floor in this commit"
[ "$unsafes" -ge "$UNSAFE_FLOOR" ] || found WARN scripts/check-invariants.sh \
    "unsafe blocks: $unsafes < UNSAFE_FLOOR $UNSAFE_FLOOR; lower the floor in this commit"
[ "$ignores" -ge "$IGNORE_FLOOR" ] || found WARN scripts/check-invariants.sh \
    "#[ignore]: $ignores < IGNORE_FLOOR $IGNORE_FLOOR; lower the floor in this commit"
report ratchet-slack

# ignore-ratchet (TST-6): a skipped test is a weakened check.
if [ "$ignores" -gt "$IGNORE_FLOOR" ]; then
    first=$(printf '%s\n' "$ignore_hits" | head -1)
    rest=${first#*:}
    found ERROR "${first%%:*}:${rest%%:*}" "#[ignore] count $ignores > IGNORE_FLOOR $IGNORE_FLOOR"
fi
report ignore-ratchet

# doc-ranges: CLAUDE.md's ranges name the highest diagram in the D-catalog and the highest ADR.
top_d=""
d_present=""
if [ -f docs/08-diagrams.md ]; then
    d_present=$(grep -oE '^\| \*\*D[0-9]+\*\* \|' docs/08-diagrams.md | grep -oE '[0-9]+' | sort -n -u)
    top_d=$(printf '%s\n' "$d_present" | tail -1)
fi
adr_present=$(for f in "${adr_files[@]}"; do n=${f#docs/adr/}; n=${n%%-*}; [ "$n" = 0000 ] || echo "$n"; done | sort -u)
top_adr=$(printf '%s\n' "$adr_present" | tail -1)
if [ ! -f CLAUDE.md ]; then
    found ERROR CLAUDE.md "missing, so its D and ADR ranges cannot be checked"
else
    claude_text=$(tr '\n' ' ' <CLAUDE.md)
    d_claims=$(grep -oE 'D1–D[0-9]+' <<<"$claude_text" | sort -u)
    adr_claims=$(grep -oE 'ADRs 0001–[0-9]{4}' <<<"$claude_text" | sort -u)
    [ -n "$d_claims" ] || found ERROR CLAUDE.md "no \"D1–Dnn\" range sentence"
    [ -n "$adr_claims" ] || found ERROR CLAUDE.md "no \"ADRs 0001–NNNN\" range sentence"
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        [ "${c#D1–D}" = "$top_d" ] || found ERROR CLAUDE.md \
            "fix the sentence \"$(grep -oE "[^.(]{0,40}\\(?$c" <<<"$claude_text" | head -1 | sed 's/^ *//')\": the highest diagram in docs/08-diagrams.md is D${top_d:-?}"
    done <<<"$d_claims"
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        [ "${c#ADRs 0001–}" = "$top_adr" ] || found ERROR CLAUDE.md \
            "fix the sentence \"$(grep -oE "[^.(]{0,40}$c" <<<"$claude_text" | head -1 | sed 's/^ *//')\": the highest ADR in docs/adr/ is ${top_adr:-none}"
    done <<<"$adr_claims"
fi
report doc-ranges

# doc-range-gaps: numbers reserved by work not yet landed. Informational, never an error.
if [ -n "$top_d" ]; then
    gaps=$(for ((i = 1; i < top_d; i++)); do grep -qx "$i" <<<"$d_present" || printf 'D%s ' "$i"; done)
    [ -z "$gaps" ] || found INFO docs/08-diagrams.md "unused below D$top_d: ${gaps% }"
fi
if [ -n "$top_adr" ]; then
    gaps=$(for ((i = 1; i < 10#$top_adr; i++)); do
        n=$(printf '%04d' "$i")
        grep -qx "$n" <<<"$adr_present" || printf 'ADR-%s ' "$n"
    done)
    [ -z "$gaps" ] || found INFO docs/adr "unused below ADR-$top_adr: ${gaps% }"
fi
report doc-range-gaps

# checklist-mirror: the docs/14 [dev] checklist is mirrored verbatim in the unversioned .claude/
# skill and rules (scripts/skill-check.sh). .claude/ is gitignored, so a fresh clone or a
# worktree has none: that is reported, never an error.
if [ ! -d .claude ]; then
    found INFO .claude "absent (fresh clone or worktree): the [dev] checklist mirrors are unchecked"
else
    out=$(bash "$SCRIPT_DIR/skill-check.sh" docs/14-no-mistakes-gate.md \
        .claude/skills/attack/SKILL.md .claude/rules/core.md 2>&1)
    code=$?
    if [ "$code" != 0 ]; then
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            case "$line" in
                *" — "*) found ERROR "${line%% — *}" "${line#* — }" ;;
                *) found ERROR scripts/skill-check.sh "$line" ;;
            esac
        done <<<"$out"
        [ ${#cur[@]} -gt 0 ] || found ERROR scripts/skill-check.sh "exited $code"
    fi
fi
report checklist-mirror

# intent-archive: gate step 1 reads only the top intent block; older ones belong in the archive.
if [ -f docs/INTENT.md ]; then
    blocks=$(grep -c '^# Intent' docs/INTENT.md || true)
    [ "$blocks" -le 1 ] ||
        found INFO docs/INTENT.md "$blocks intent blocks: move all but the top one to docs/intent-archive.md"
fi
report intent-archive

printf 'summary: %d error(s), %d warning(s), %d info\n' "$errors" "$warns" "$infos"
[ "$errors" -eq 0 ]
