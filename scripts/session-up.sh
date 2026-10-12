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

# HTTP status of /admin/health, or 000 if nothing answers. The handler probes the LLM backend
# before replying (the claude-code probe may take up to 5s) and returns 503 when the vault is
# unusable, so any status other than 000 means an idea-vault is listening on the port.
health_status() {
  curl -s --noproxy '*' -m 10 -o /dev/null -w '%{http_code}' "$BASE/admin/health" 2>/dev/null || true
}
listening() { [ "$(health_status)" != 000 ]; }

# The idea-vault this checkout started on $PORT: its exe is $BIN and its environment binds this
# port. Looked up from /proc rather than taken from `$!`, which is setsid's own pid whenever
# setsid has to fork.
pid_on_port() {
  local p bin
  bin="$(readlink -f "$BIN")"
  for p in /proc/[0-9]*; do
    [ "$(readlink -f "$p/exe" 2>/dev/null)" = "$bin" ] || continue
    tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -qx "IDEA_VAULT_BIND=127.0.0.1:$PORT" &&
      { echo "${p#/proc/}"; return 0; }
  done
  return 1
}

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
    code="$(health_status)"
    if [ "$code" != 000 ]; then echo "up: $BASE (health HTTP $code)"; else echo "down"; exit 1; fi
    exit 0 ;;
  "") ;;
  *) echo "usage: $0 [--down|--status]" >&2; exit 2 ;;
esac

mkdir -p "$STATE"
chmod 700 "$STATE"
if [ ! -s "$STATE/mcp-token" ]; then
  (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$STATE/mcp-token")
fi

if listening; then
  backend="$(curl -sS --noproxy '*' -m 10 "$BASE/admin/health" | sed -n 's/.*"backend":"\([^"]*\)".*/\1/p')"
  if our_pid >/dev/null; then owner="started by this script"; else owner="NOT started by this script"; fi
  code="$(mcp_status)"
  if [ "$code" != 200 ]; then
    case "$code" in
      404) why="its /api/mcp is off (started without IDEA_VAULT_MCP_TOKEN)" ;;
      401) why="it was started with a different IDEA_VAULT_MCP_TOKEN" ;;
      *)   why="/api/mcp answered HTTP $code (health HTTP $(health_status))" ;;
    esac
    echo "an idea-vault on $BASE ($owner, backend: ${backend:-unknown}) can't be used by this session:" \
         "$why. Stop it, or pick a free port with IDEA_VAULT_SESSION_PORT." >&2
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
  spawned=$!
  for _ in $(seq 1 60); do
    listening && break
    if ! kill -0 "$spawned" 2>/dev/null && ! pid_on_port >/dev/null; then
      echo "idea-vault exited during boot; last log lines:" >&2
      tail -n 20 "$LOG" >&2
      exit 1
    fi
    sleep 0.5
  done
  listening || { echo "idea-vault did not answer $BASE/admin/health within 30s; see $LOG" >&2; exit 1; }
  # Recorded only once the server answers, so a failed boot never overwrites a good pid file.
  pid="$(pid_on_port)" || { echo "idea-vault answers on $BASE but its pid was not found; see $LOG" >&2; exit 1; }
  echo "$pid" > "$PIDFILE"
  echo "idea-vault up at $BASE (backend: $IDEA_VAULT_LLM_BACKEND, pid $pid, log $LOG)"
fi

cat <<EOF
Drive it:  python3 scripts/session-mcp.py tools
           python3 scripts/session-mcp.py call list_ideas
Web UI:    $BASE  (forward the port to view it)
Stop:      bash scripts/session-up.sh --down
EOF
