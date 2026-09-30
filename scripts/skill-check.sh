#!/usr/bin/env bash
# skill-check.sh — the checklist drift gate (ADR-0041, docs/14-no-mistakes-gate.md "Checklist").
#
#   skill-check.sh [doc] [skill] [rules]
#
# docs/14 is the canonical home of the no-mistakes checklist; `N. **phrase** [dev]` items are
# mirrored verbatim in the attack skill and the core rules (both under the unversioned .claude/),
# `[product]` items in RUN_PROTOCOL (checked by cargo, plan::tests). This checks that the items
# are numbered 1..n without a gap and that every [dev] phrase appears verbatim in both mirrors.
# Prints one `<target> — <message>` line per problem; exit 0 or 1. It runs as the
# checklist-mirror rule of scripts/check-invariants.sh, so gate step 2 and the pre-push hook
# carry it.
set -uo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
doc=${1:-$root/docs/14-no-mistakes-gate.md}
skill=${2:-$root/.claude/skills/attack/SKILL.md}
rules=${3:-$root/.claude/rules/core.md}

problems=0
problem() {
    printf '%s — %s\n' "$1" "$2"
    problems=$((problems + 1))
}

if [ ! -f "$doc" ]; then
    problem "$doc" "missing: it is the canonical home of the checklist"
    exit 1
fi

# The `## Checklist` section: from its heading to the next `## ` heading.
section=$(awk '/^## Checklist[[:space:]]*$/ { on = 1; next } /^## / { on = 0 } on' "$doc")
expected=1
dev=()
while IFS= read -r line; do
    case "$line" in
        [0-9]*.\ *) ;;
        *) continue ;;
    esac
    if [[ "$line" =~ ^([0-9]+)\.\ \*\*(.+)\*\*\ \[(dev|product)\][[:space:]]*$ ]]; then
        n=${BASH_REMATCH[1]}
        [ "$n" = "$expected" ] || problem "$doc" "checklist item $n should be numbered $expected"
        expected=$((expected + 1))
        [ "${BASH_REMATCH[3]}" = dev ] && dev+=("${BASH_REMATCH[2]}")
    else
        problem "$doc" "checklist line is not \`N. **phrase** [dev|product]\`: $line"
    fi
done <<<"$section"
[ "$expected" -gt 1 ] || problem "$doc" "no \`## Checklist\` items"

for mirror in "$skill" "$rules"; do
    if [ ! -f "$mirror" ]; then
        problem "$mirror" "missing: it mirrors the [dev] checklist of $doc"
        continue
    fi
    for phrase in "${dev[@]+"${dev[@]}"}"; do
        grep -qF -- "$phrase" "$mirror" || problem "$mirror" "missing [dev] phrase: $phrase"
    done
done

[ "$problems" -eq 0 ]
