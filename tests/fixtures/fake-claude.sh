#!/usr/bin/env bash
# Fake `claude` CLI for idea-vault backend tests. Speaks just enough `stream-json`.
# Behavior is chosen by the value passed to `--model` (default: "tokens").
mode="tokens"
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[$i]}" in
    --version) echo "9.9.9 (fake-claude)"; exit 0 ;;
    --model)   mode="${args[$((i+1))]}" ;;
  esac
done
# Drain stdin (the client writes one user message then closes it).
cat >/dev/null 2>&1 || true
case "$mode" in
  eof)
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}}'
    ;;
  auth)
    printf '%s\n' '{"type":"assistant","error":"authentication_failed","message":{"content":[{"type":"text","text":"401 unauthorized"}]}}'
    ;;
  resulttext)
    printf '%s\n' '{"type":"result","result":"whole answer","session_id":"x"}'
    ;;
  *)
    printf '%s\n' '{"type":"system","subtype":"init","session_id":"x"}'
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Grep"}}}'
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello "}}}'
    printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"world"}}}'
    printf '%s\n' '{"type":"result","result":"Hello world","session_id":"x"}'
    ;;
esac
exit 0
