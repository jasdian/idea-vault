#!/usr/bin/env bash
# Build a Graphify map of a reference source or a codebase, the way docs/15 says it may be built
# (ADR-0043): beside the source, never inside it or inside the vault, on a backend chosen here and
# never auto-picked from an API key, with no model call at all in the default --code-only mode.
#
#   bash scripts/graphify-map.sh <source-dir> <map-dir>                       # code only, offline
#   bash scripts/graphify-map.sh <source-dir> <map-dir> --backend ollama      # + docs via local Ollama
#   bash scripts/graphify-map.sh <source-dir> <map-dir> --backend claude-cli  # + docs via local claude (sends the corpus to Anthropic)
#   add --viz to also write graph.html (slow and large past ~5,000 nodes)
#
# Writes <map-dir>/graphify-out/{graph.json,GRAPH_REPORT.md}, <map-dir>/COMMUNITIES.md (communities
# named by their files, deterministic; scripts/graphify-communities.py) and <map-dir>/BUILT.txt.
# Register <map-dir> on /sources next to the source to let the foil read it (docs/15 Layer 2/3).
# Needs `graphify` on PATH (uv tool install graphifyy). Read-only on <source-dir>.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
die() { echo "graphify-map: $*" >&2; exit 1; }

[ $# -ge 2 ] || die "usage: $0 <source-dir> <map-dir> [--backend ollama|claude-cli] [--viz]"
# Portable absolute path with symlinks resolved, for a path that may not exist yet (GNU realpath's
# -e/-m flags are missing on macOS; python3 is already needed for the community index).
abspath() { python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$1"; }
[ -e "$1" ] || die "no such source dir: $1"
SRC="$(abspath "$1")"
MAP="$(abspath "$2")"
shift 2
MODE=code-only
VIZ=--no-viz
while [ $# -gt 0 ]; do
  case "$1" in
    --backend)
      case "${2:-}" in
        ollama|claude-cli) MODE="$2"; shift 2 ;;
        *) die "--backend must be ollama or claude-cli (other backends are cloud APIs; ADR-0043 E)" ;;
      esac ;;
    --viz) VIZ=; shift ;;
    *) die "unknown option: $1" ;;
  esac
done
[ -d "$SRC" ] || die "source is not a directory: $SRC"
command -v graphify >/dev/null || die "graphify not on PATH (uv tool install graphifyy)"

# Graphify's extract and its community labeling both auto-select a backend from whichever API key
# is set (Gemini first). Refuse rather than trust every sub-pass to honour --backend.
for k in GEMINI_API_KEY GOOGLE_API_KEY MOONSHOT_API_KEY KIMI_API_KEY ANTHROPIC_API_KEY \
         OPENAI_API_KEY DEEPSEEK_API_KEY AZURE_OPENAI_API_KEY; do
  [ -z "${!k:-}" ] || die "refusing: $k is set (graphify would auto-pick a cloud backend); unset it for this run"
done

# Placement: the map is derived and rebuildable. Inside the source it would be fed back into the
# next build (and the source is read-only, ADR-0021); inside the vault it would sit next to truth.
inside() { case "$1/" in "$2"/*) return 0 ;; *) return 1 ;; esac; }
VAULT="$(abspath "${IDEA_VAULT_VAULT_DIR:-$ROOT/vault}")"
inside "$MAP" "$SRC" && die "map dir $MAP is inside the source; put it beside it (e.g. ${SRC}-map)"
inside "$SRC" "$MAP" && die "source $SRC is inside the map dir"
inside "$MAP" "$VAULT" && die "map dir $MAP is inside the vault ($VAULT); Graphify output is never vault content"
inside "$SRC" "$VAULT" && die "source $SRC is inside the vault; Graphify runs on reference sources or code, never on vault/"
# A codebase that holds the vault (this repo) may be mapped only while git ignores the vault, which
# graphify honours by default.
if inside "$VAULT" "$SRC" && [ -e "$VAULT" ]; then
  git -C "$SRC" check-ignore -q "$VAULT" 2>/dev/null ||
    die "the vault $VAULT is under the source and not git-ignored there; graphify would read it"
fi

export GRAPHIFY_QUERY_LOG_DISABLE=1
mkdir -p "$MAP"
case "$MODE" in
  code-only)  extract=(--code-only) ;;
  ollama)     extract=(--backend ollama --max-concurrency 1)
              # Same Ollama as the app when it is configured; never a hardcoded host.
              [ -z "${OLLAMA_BASE_URL:-}" ] && [ -n "${IDEA_VAULT_OLLAMA_URL:-}" ] &&
                export OLLAMA_BASE_URL="$IDEA_VAULT_OLLAMA_URL" ;;
  claude-cli) extract=(--backend claude-cli --max-concurrency 1)
              echo "graphify-map: claude-cli sends every non-code file in $SRC to Anthropic (ADR-0043 E)" >&2 ;;
esac

graphify extract "$SRC" "${extract[@]}" --out "$MAP"
# extract stops at graph.json; the report comes from cluster-only. --no-label keeps community
# naming off any model (it would auto-pick a backend too); COMMUNITIES.md names them instead.
graphify cluster-only "$MAP" --no-label ${VIZ:+"$VIZ"}
python3 -I "$ROOT/scripts/graphify-communities.py" "$MAP/graphify-out/graph.json" > "$MAP/COMMUNITIES.md"

rev="$(git -C "$SRC" rev-parse --short HEAD 2>/dev/null || echo "not a git checkout")"
{
  date -u +"built %Y-%m-%dT%H:%M:%SZ"
  echo "source $SRC @ $rev"
  echo "mode $MODE, $(graphify --version 2>/dev/null | head -1)"
} > "$MAP/BUILT.txt"

report="$MAP/graphify-out/GRAPH_REPORT.md"
echo "graphify-map: $(wc -c < "$report") B report, $(wc -c < "$MAP/COMMUNITIES.md") B community index in $MAP"
