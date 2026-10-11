#!/usr/bin/env bash
# Boot idea-vault for an agent session (Claude Code on the web, a terminal agent): build it if
# needed, start it in the background on the claude-code backend with the inbound MCP server on, and
# wait until /admin/health answers. Idempotent: an instance already answering on the port is reused.
#
#   bash scripts/session-up.sh            # start (or reuse) and print how to drive it
#   bash scripts/session-up.sh --down     # stop the instance this script started
#   bash scripts/session-up.sh --status   # is it up?
#
# Then drive it with scripts/session-mcp.py (docs/12-deployment.md "Agent sessions").
#
# Env (all optional): IDEA_VAULT_SESSION_PORT (default 3000), IDEA_VAULT_LLM_BACKEND (default
# claude-code here; set `ollama` when an Ollama is reachable), IDEA_VAULT_VAULT_DIR (default ./vault).
# Every other IDEA_VAULT_* setting passes through to the server unchanged (docs/12).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE="$ROOT/target/session"
PORT="${IDEA_VAULT_SESSION_PORT:-3000}"
BASE="http://127.0.0.1:$PORT"
BIN="$ROOT/target/release/idea-vault"
PIDFILE="$STATE/idea-vault.pid"
LOG="$STATE/idea-vault.log"

healthy() { curl -fsS --noproxy '*' -m 3 "$BASE/admin/health" >/dev/null 2>&1; }

# The pid in $PIDFILE, only if that process is still this script's idea-vault binary (a pid can be
# reused by an unrelated process after a crash or container restart).
our_pid() {
  local pid
  pid="$(cat "$PIDFILE" 2>/dev/null)" || return 1
  [ -n "$pid" ] && [ "$(readlink -f "/proc/$pid/exe" 2>/dev/null)" = "$(readlink -f "$BIN")" ] &&
    echo "$pid"
}

# HTTP status of an MCP `initialize` with our token: 200 means /api/mcp is on and accepts it.
mcp_status() {
  curl -s --noproxy '*' -m 5 -o /dev/null -w '%{http_code}' -X POST "$BASE/api/mcp" \
    -H "Authorization: Bearer $(cat "$STATE/mcp-token")" -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"session-up","version":"1"}}}'
}

case "${1:-}" in
  --down)
    if pid="$(our_pid)" && kill "$pid" 2>/dev/null; then
      echo "stopped idea-vault (pid $pid)"
    else
      echo "no idea-vault started by this script is running"
    fi
    rm -f "$PIDFILE"
    exit 0 ;;
  --status)
    if healthy; then echo "up: $BASE"; else echo "down"; exit 1; fi
    exit 0 ;;
  "") ;;
  *) echo "usage: $0 [--down|--status]" >&2; exit 2 ;;
esac

mkdir -p "$STATE"
chmod 700 "$STATE"
if [ ! -s "$STATE/mcp-token" ]; then
  (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$STATE/mcp-token")
fi

if healthy; then
  backend="$(curl -fsS --noproxy '*' -m 3 "$BASE/admin/health" | sed -n 's/.*"backend":"\([^"]*\)".*/\1/p')"
  if our_pid >/dev/null; then owner="started by this script"; else owner="NOT started by this script"; fi
  code="$(mcp_status)"
  if [ "$code" != 200 ]; then
    echo "an idea-vault on $BASE ($owner, backend: ${backend:-unknown}) rejects this session's MCP" \
         "token (HTTP $code from /api/mcp): it was started without IDEA_VAULT_MCP_TOKEN or with another" \
         "one. Stop it, or pick a free port with IDEA_VAULT_SESSION_PORT." >&2
    exit 1
  fi
  echo "idea-vault already up at $BASE ($owner, backend: ${backend:-unknown})"
else
  if [ ! -x "$BIN" ]; then
    echo "building idea-vault (release)…"
    "$ROOT/scripts/session-build.sh"
  fi
  # A per-session vault under the repo's own vault/ (the gitignored owner content, ADR-0019).
  export IDEA_VAULT_VAULT_DIR="${IDEA_VAULT_VAULT_DIR:-$ROOT/vault}"
  export IDEA_VAULT_INDEX_PATH="${IDEA_VAULT_INDEX_PATH:-$ROOT/index.db}"
  export IDEA_VAULT_BIND="127.0.0.1:$PORT"
  export IDEA_VAULT_LLM_BACKEND="${IDEA_VAULT_LLM_BACKEND:-claude-code}"
  IDEA_VAULT_MCP_TOKEN="$(cat "$STATE/mcp-token")"
  export IDEA_VAULT_MCP_TOKEN
  # setsid detaches it from this shell so it outlives the command that started it.
  setsid nohup "$BIN" >>"$LOG" 2>&1 < /dev/null &
  echo $! > "$PIDFILE"
  for _ in $(seq 1 60); do
    healthy && break
    if ! kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
      echo "idea-vault exited during boot; last log lines:" >&2
      tail -n 20 "$LOG" >&2
      exit 1
    fi
    sleep 0.5
  done
  healthy || { echo "idea-vault did not answer $BASE/admin/health within 30s; see $LOG" >&2; exit 1; }
  echo "idea-vault up at $BASE (backend: $IDEA_VAULT_LLM_BACKEND, pid $(cat "$PIDFILE"), log $LOG)"
fi

cat <<EOF
Drive it:  python3 scripts/session-mcp.py tools
           python3 scripts/session-mcp.py call list_ideas
Web UI:    $BASE  (forward the port to view it)
Stop:      bash scripts/session-up.sh --down
EOF
