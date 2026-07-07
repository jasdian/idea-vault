# ADR-0009 — Pluggable LLM backend; claude-code as an agentic option

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

idea-vault shipped with a single LLM backend: local Ollama over HTTP ([ADR-0003](./0003-ollama-local-only-ai.md)),
a pure text model with no file access. Two owner needs pushed past that:

1. **Run without installing Ollama.** The owner's machine has an authenticated `claude` CLI (Claude
   Code) but no Ollama and no pulled model. The sibling repo `ai-automation/claude-remote-chat`
   already proves a clean pattern: shell out to `claude --output-format stream-json`, write one
   user-message JSON line on stdin, and parse the newline-delimited JSON on stdout, forwarding
   `text_delta` chunks as tokens.
2. **Ground ideation in the owner's own notes.** The foil should be able to read the owner's Obsidian
   vault and prior Claude Code artifacts while interrogating an idea — which a pure text model cannot
   do, but an agentic `claude` process (Read/Grep/Glob/Bash) can, pointed at those directories.

The `ai` module was already a narrow seam ([D4](./../02-module-reference.md)): callers assemble
prompts and hand them in; `ai` never reads the vault. `OllamaClient` exposes exactly four
load-bearing methods (`probe`, `chat`, `chat_stream`, `model`), and the persist boundaries, the
shared concurrency semaphore ([ADR-0006](./0006-bounded-concurrency-swarm.md)), and the SSE pump all
sit *above* the client. So a second backend drops in without disturbing any invariant.

## Decision

Introduce an **`LlmBackend` enum** (`ai::backend`) with two variants — `Ollama(OllamaClient)` and
`ClaudeCode(ClaudeCodeClient)` — carrying the same four-method surface and dispatching by `match`.
An enum (not a `dyn` trait) keeps this zero-cost and dependency-free; the backend set is closed and
small. `AppState` holds one `LlmBackend`, selected at boot from `IDEA_VAULT_LLM_BACKEND`
(`ollama` default | `claude-code`).

The **claude-code backend** (`ai::claude_code`) spawns a fresh one-shot `claude` process per turn
(idea-vault reassembles the full budgeted context every turn, so no `--resume`/session state is
needed), streams `text_delta` chunks as tokens, ends on the `result` event, and maps
spawn/parse/EOF/auth failures to a terminal `AiError::Backend` (so a partial reply is never
persisted — the D11 boundary is unchanged). Tool activity the foil performs (Grep/Read/Bash) is
consumed but **not** streamed as chat tokens; only its prose reaches the transcript.

Two adjacent capabilities land with it:

- **External context** (`IDEA_VAULT_CLAUDE_ADD_DIRS` → `--add-dir`, plus a system-prompt note naming
  those dirs) so the agentic foil knows to grep the owner's vault/artifacts.
- **An importer** (`idea-vault import <dir>`, `import.rs`) that converts flat Obsidian `.md` notes
  into ideas (path-derived slug for idempotent re-runs; `[[Wiki Links]]` rewritten to `[[slug]]`).
  Known v1 limitation: two *distinct* source paths that slugify to the same base slug collide — the
  first wins and the second is reported as `skipped` (logged, never silently dropped without a
  count). Rare in practice; a provenance field would remove it.

## Consequences

- **New error variant.** `AiError::Backend(String)` for non-Ollama failures; `Http`/`Timeout`/`Protocol`
  stay Ollama-specific.
- **New config.** `IDEA_VAULT_LLM_BACKEND`, `IDEA_VAULT_CLAUDE_BIN`, `_MODEL`, `_CWD`, `_ADD_DIRS`,
  `_ALLOWED_TOOLS`, `_SKIP_PERMISSIONS`, `_TIMEOUT_SECS` (see [12-deployment](../12-deployment.md)).
- **Safety of a full-agentic foil.** With `--dangerously-skip-permissions` the foil can run
  Bash/Write/Edit unattended. Mitigation: the foil's working dir defaults to the **vault dir, never
  the idea-vault source tree**, so it cannot rewrite the app; the behavior is a single documented
  config flag.
- **Latency.** A `claude` turn is seconds (process spawn + agentic tool use), so its hard timeout
  defaults higher (`IDEA_VAULT_CLAUDE_TIMEOUT_SECS`, 300s) than Ollama's.
- **Testing.** The Ollama mock does not apply; the claude backend is tested against a fake `claude`
  shell script emitting canned `stream-json` (`tests/fixtures/fake-claude.sh`,
  `tests/claude_backend.rs`). The persist boundaries above the seam remain covered by the Ollama-path
  web tests.
- **Ollama stays the default** — the offline local option is unchanged.
