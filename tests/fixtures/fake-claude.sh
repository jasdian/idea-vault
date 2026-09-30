#!/usr/bin/env bash
# Fake `claude` CLI for idea-vault backend tests. Speaks just enough `stream-json`.
# Behavior is chosen by the value passed to `--model` (default: "tokens"). Like the real CLI
# (2.1.285), the init event lists exactly the `--tools` it was given and no MCP servers.
mode="tokens"
tools=""
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[$i]}" in
    --version) echo "9.9.9 (fake-claude)"; exit 0 ;;
    --model)   mode="${args[$((i+1))]}" ;;
    --tools)   tools="${args[$((i+1))]}" ;;
  esac
done
# A CLI that never reads its prompt: the client's stdin write must time out, not hang.
[ "$mode" = "stalledstdin" ] && exec sleep 30
# Drain stdin (the client writes one user message then closes it).
cat >/dev/null 2>&1 || true

# The init tools as a JSON array, built from the comma-separated --tools value.
init_tools() {
  local extra="$1" out="" t
  IFS=',' read -ra list <<< "$tools"
  for t in "${list[@]}" $extra; do
    [ -n "$t" ] && out="$out${out:+,}\"$t\""
  done
  printf '[%s]' "$out"
}
init() {
  printf '{"type":"system","subtype":"init","session_id":"x","tools":%s,"mcp_servers":[]}\n' "$(init_tools "$1")"
}

case "$mode" in
  eof)
    init
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}}'
    ;;
  auth)
    init
    printf '%s\n' '{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"401 unauthorized"}]}}'
    ;;
  resulttext)
    init
    printf '%s\n' '{"type":"result","result":"whole answer","session_id":"x"}'
    ;;
  dumpenv)
    # Record what the client really passed, into the cwd it chose (the test's idea dir).
    printf '%s\n' "${args[@]}" > "$PWD/fake-claude.argv"
    env > "$PWD/fake-claude.env"
    init
    printf '%s\n' '{"type":"result","result":"recorded","session_id":"x"}'
    ;;
  leakytools)
    # A CLI that ignored --tools: the client must refuse the session before any output.
    init "Bash"
    printf '%s\n' '{"type":"result","result":"should never be seen","session_id":"x"}'
    ;;
  noinit)
    printf '%s\n' '{"type":"result","result":"unverified","session_id":"x"}'
    ;;
  busytools)
    # Busy forever with tool events, so only the turn deadline can end it.
    echo $$ > "$PWD/fake-claude.pid"
    init
    while true; do
      printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Grep"}}}'
      sleep 0.1
    done
    ;;
  *)
    init
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Grep"}}}'
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello "}}}'
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}}'
    printf '%s\n' '{"type":"result","result":"Hello world","session_id":"x"}'
    ;;
esac
exit 0

# ci-gate-check: an undeclared fixture change
