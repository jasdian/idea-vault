# ADR-0003 — Ollama local models as the only AI backend

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

The whole product is AI-driven: interrogating ideas, extracting memory, swarming subagents. We must
choose the AI backend. The owner prioritizes **privacy and offline operation** — ideas are personal
and should not leave the machine — over raw model capability.

## Decision

We will talk **only to a local Ollama server** at `http://localhost:11434` (`ai::ollama`). No cloud
AI provider is integrated. The subagent-swarm and skills features run against local models, which
constrains us to **bounded concurrency and careful context budgeting** (see
[ADR-0006](./0006-bounded-concurrency-swarm.md)).

## Consequences

- The tool is fully usable offline; no API keys, no per-token cost, no data leaving the machine.
- Model capability is whatever the user has pulled locally; prompts and workflows must be robust to
  smaller/weaker models than frontier cloud models.
- Concurrency is limited by one machine's resources — swarms must cap parallelism and budget context.
- Ollama being absent/slow is an expected runtime state; the app must **degrade gracefully**
  ([D20](../05-ai-integration.md)), never hang.
- The `ai` module is the single boundary to the model; swapping providers later would be localized
  there, but doing so is explicitly out of scope unless the owner asks.

## Alternatives considered

- **Anthropic Claude API** — strongest fit for multi-agent orchestration and the harness concepts,
  but sends personal ideas to the cloud and requires network + keys + cost. Rejected: violates the
  privacy/offline goal.
- **Provider-agnostic trait (Anthropic/OpenAI/Ollama)** — maximum flexibility, but real upfront
  abstraction cost for a solo tool with one chosen backend. Rejected as premature; the `ai` module
  boundary keeps the door open without paying for it now.
