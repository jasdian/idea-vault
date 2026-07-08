# ADR-0017 — Web access tools, gated by one live setting

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

## Context

[ADR-0003](./0003-ollama-local-only-ai.md) chose local-only Ollama specifically for **privacy and
offline operation** — no cloud AI provider, nothing leaving the machine. That decision is about the
*model* the owner talks to, not about whether the foil may read the live web while reasoning. The
owner now wants the foil — on either backend — to be able to pull in current external facts (market
numbers, prior art, competitors, news) when stress-testing an idea, without abandoning the ability to
run idea-vault fully offline whenever they choose. The two backends need this capability delivered
differently: Ollama's `/api/chat` has no built-in web tools, while the `claude` CLI already ships its
own `WebSearch`/`WebFetch` tools that only need to be allowed or denied.

## Decision

We will add **one live setting, `web_access`** (boot env `IDEA_VAULT_WEB_ACCESS`, default `true`;
Settings-page checkbox, following the [ADR-0011](./0011-live-switchable-llm-backend.md) live-tunable
pattern — no restart to flip it), that gates web crawling on both backends identically in spirit,
differently in mechanism:

- **Ollama path — a new `ai::web` module.** Two keyless tools: `web_search` (GETs DuckDuckGo's no-JS
  HTML endpoint, no API key; the endpoint is env-overridable via `IDEA_VAULT_SEARCH_URL`, e.g. to
  point at a self-hosted SearXNG instance) and `fetch_url` (GET + tag-strip, truncated to 12,000
  characters so one fetch can never blow the context budget). `LlmBackend::chat` runs a **bounded
  tool-calling loop** on top of `/api/chat` with `stream: false` and a `tools` array: at most
  `MAX_TOOL_ROUNDS = 4` rounds of "model may call tools", at most `MAX_CALLS_PER_ROUND = 3` executed
  calls per round, then one final forced tool-free call so the loop always terminates in a plain
  answer. **Tool errors are returned as readable tool-result text, never fail the turn** — a dead
  network or a 404 becomes something the model can read and route around, per the existing [D20](../05-ai-integration.md)
  degrade-not-die posture. A model that doesn't support tool calling (Ollama's `400 does not support
  tools`) falls back to the plain, non-tool offline call rather than erroring the turn. Non-streaming
  tool rounds get a wall-clock bound of `token_timeout × TOOL_ROUND_TIMEOUT_FACTOR` (4×) instead of
  the streaming inactivity timeout — a `stream:false` round waits for one whole generation, not
  token-to-token gaps, and a thinking model can generate for minutes without ever being "inactive".
- **claude-code path — allow/deny, not a new tool.** When `web_access` is on, the router allows the
  CLI's own `WebSearch`/`WebFetch` tools and appends a system-prompt hint nudging the model to use
  them and cite sources. When off, it passes `--disallowedTools WebSearch,WebFetch` — a **deny that
  holds even under `--dangerously-skip-permissions`** (the full-agentic default,
  [ADR-0009](./0009-pluggable-llm-backend-claude-code.md)), rather than merely omitting them from an
  allow-list that skipped-permissions would ignore anyway. `ClaudeCodeConfig` gains a
  `disallowed_tools: Vec<String>` field carrying this.

`IDEA_VAULT_WEB_ACCESS=false` (or unchecking the Settings checkbox) restores a fully offline run on
either backend — this is the owner's explicit, opt-outable choice, not a silent revision of
[ADR-0003](./0003-ollama-local-only-ai.md)'s privacy stance. The **default is on** because the owner
asked for it; the escape hatch back to fully-offline is one env var or one checkbox away, so the
original privacy guarantee remains available, just no longer the unconditional default.

## Consequences

- The foil can ground an interrogation in live external facts on either backend, with one place
  (`web_access`) to reason about whether that's happening.
- **New module `ai::web`** (search + fetch + Ollama tool-loop leaves) sits inside the existing `ai`
  boundary — callers still never talk to a concrete client directly (docs/02-module-reference.md).
- **Ollama turns are no longer purely single-shot when tools fire.** `LlmBackend::chat` now
  internally makes up to `MAX_TOOL_ROUNDS + 1` non-streaming calls instead of one streaming call
  when `web_access` is on and the model exercises tools; callers (chat/skill/swarm/workflow) are
  unaffected — they still see one `chat()` call in, one reply out.
- **A model without tool support degrades silently to plain chat** rather than failing the turn —
  consistent with D20, at the cost of that model never getting web access even when the setting is
  on; there is no UI signal distinguishing "model ignored the tools" from "model has none", which is
  an accepted gap, not a new failure mode (the reply itself is unaffected either way).
- **Outbound internet is now a real dependency** when `web_access` is on: a containerized deployment
  needs outbound network reachability for `IDEA_VAULT_SEARCH_URL`/DuckDuckGo and arbitrary fetch
  targets — noted in [12-deployment](../12-deployment.md)'s env-var table. This is a new posture for
  a tool whose containers previously only needed to reach the Ollama service on the compose network.
- **Tension with ADR-0003 is explicit, not silently absorbed.** ADR-0003's privacy/offline stance is
  about *not sending idea content to a cloud AI provider by default*; it never claimed the app would
  never make outbound HTTP calls at all. `web_access` is a separate, owner-visible, opt-outable axis
  — this ADR does not amend or supersede ADR-0003, it adds a capability ADR-0003 didn't rule on, and
  documents the seam between the two explicitly so the two decisions don't read as contradictory.

## Alternatives considered

- **API-key search providers (Bing/Google/Brave Search APIs)** — better result quality and stability
  than scraping DuckDuckGo's HTML, but requires a signup, a key, and a network dependency on a paid
  third party. Rejected: violates the keyless/local ethos the rest of the app holds to — every other
  external dependency (Ollama, the `claude` CLI) needs no API key either.
- **Always-on, no toggle** — simpler (one less setting, one less code path to test), but removes the
  owner's ability to run idea-vault fully offline, which is the whole point of
  [ADR-0003](./0003-ollama-local-only-ai.md). Rejected: the owner must be able to opt back into a
  fully offline posture at any time, not just at first boot.
- **Streaming tool rounds** (keep `/api/chat` `stream: true` and parse tool calls out of the token
  stream) — would preserve the existing token-by-token UX during a tool-calling turn, but is
  materially more protocol work (partial-JSON tool-call accumulation across chunks, mid-stream
  branching between "more tokens" and "a tool call needs a synchronous round-trip before more
  tokens can be produced") for a feature that already has a working non-streaming path. Deferred,
  not rejected outright — revisit if the non-streaming round's coarser "thinking…" experience turns
  out to matter in practice.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
