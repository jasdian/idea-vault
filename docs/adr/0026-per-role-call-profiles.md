# ADR-0026 — Per-role call profiles over the live settings snapshot

- **Status:** Accepted
- **Date:** 2026-09-28
- **Deciders:** owner
- **Amends:** [ADR-0011](./0011-live-switchable-llm-backend.md) — "every call reads one
  `LlmSettings` snapshot" becomes "every call reads one snapshot, overlaid by its role's profile".

## Context

[ADR-0011](./0011-live-switchable-llm-backend.md) made the LLM backend a live router: every call
reads a single `LlmSettings` snapshot (backend, Ollama temperature, claude model, claude effort).
Swarms, workflows and skills run six agent roles
([06-concepts/agents](../06-concepts/agents.md)) that differ only in their persona prompt, so a
`ready-to-build` workflow runs five harvesters, the auditor and the build-prompt synthesizer at
the same temperature and on the same claude model.

The roles want opposite sampling. A harvester extracts only what the discussion said and should
run cold; so should the auditor, whose job is a sceptical label. Critics and the advocate are
there to produce diverse angles and should run hot. The synthesizer sits in between. On
claude-code the owner may also want the auditor and synthesizer on a stronger model or effort
than the fan-out critics.

## Decision

- `ai::backend::LlmSettings` gains `role_tuning: bool` and `role_profiles`, a map from a role
  name to a `RoleProfile { temperature, claude_model, claude_effort }`. A blank model or effort
  inherits the global value. The keys are plain strings: `ai` stays role-agnostic and never
  imports `concepts` (D4).
- `LlmBackend::for_role(name)` returns a cheap scoped clone, the same pattern as
  `with_turn_sources` ([ADR-0021](./0021-reference-sources.md)). When role tuning is on and the
  role has a profile, its calls overlay that profile on the live snapshot; otherwise they use the
  snapshot unchanged. The overlay is read per call, so Settings edits stay live.
- `concepts` owns the defaults table and scopes every role-bearing call: agent runs (swarm
  fan-out, swarm synthesis, audit), a workflow's chained step, and a single skill invocation
  (under the skill's own role). Free chat with the foil, compaction and store-time extraction
  have no role and keep the global settings.
- One Ollama model serves every role; only its temperature varies. A per-role Ollama model would
  make a fan-out swap models in VRAM between calls.
- Role tuning is on by default. A Settings checkbox turns it off, which restores the single
  global setting. Like every other setting it is runtime-only.

## Consequences

- Prompts are sized from the global context window before the call
  ([ADR-0014](./0014-dynamic-context-budget.md)). With role tuning on, the claude-code window is
  the smallest window across the global model and every per-role model override, so a smaller
  per-role model is never sent an over-budget prompt. The Ollama window is unchanged.
- Temperature has no effect on claude-code; the CLI has no temperature flag, as before.
- The shared concurrency bound ([ADR-0006](./0006-bounded-concurrency-swarm.md)) is untouched: a scoped
  clone changes call parameters, never the permit logic.

## Alternatives considered

- **Per-role Ollama model.** Rejected by the owner: model load/unload thrash under fan-out on one
  GPU.
- **Temperature only.** Smaller, but leaves claude-code with no way to put the auditor and
  synthesizer on a stronger model.
- **Profiles on `AgentRole` in `concepts`, passed down per call.** Would thread sampling
  parameters through every orchestrator signature; the scoped clone keeps the call sites to one
  `for_role` each.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
