# ADR-0011 — LLM backend is a live-switchable router, not fixed at boot

- **Status:** Accepted (**amends** [ADR-0009](./0009-pluggable-llm-backend-claude-code.md)'s
  "selected at boot" language)
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

[ADR-0009](./0009-pluggable-llm-backend-claude-code.md) introduced a second LLM backend
(claude-code, alongside Ollama) as an `LlmBackend` enum with two variants, dispatched by `match`,
with `AppState` holding **one** `LlmBackend` selected once at boot from `IDEA_VAULT_LLM_BACKEND`
and never changed for the life of the process.

In practice the owner wants to compare backends and retune parameters (Ollama sampling temperature;
claude-code model and reasoning effort) within a single running session — e.g. try a discussion
turn on the local Ollama model, then immediately retry the same idea against the agentic claude-code
foil, without restarting the server and losing the in-flight session/AppState. A boot-fixed enum
variant cannot support that; it would require a process restart (and re-probing, re-binding, etc.)
for every backend or parameter change.

## Decision

We will make `LlmBackend` (`ai::backend`) a **live router**, not a closed enum selected once:

- `LlmBackend` is a struct holding **both** backends always constructed — an `OllamaClient` and a
  base `ClaudeCodeConfig` — plus `settings: Arc<RwLock<LlmSettings>>`.
- `LlmSettings` carries the runtime-tunable knobs: `backend: LlmBackendKind` (which backend answers
  right now), `temperature: f32` (Ollama sampling temperature), `claude_model: String` (empty = the
  CLI's own default), and `claude_effort: String` (`low`/`medium`/`high`).
- Every call (`probe`, `model`, `chat`, `chat_stream`) reads a fresh snapshot of `LlmSettings` and
  dispatches to the currently-selected backend with the currently-tuned parameters — there is no
  cached "the backend" decision baked in at construction time.
- `claude_effort` has no CLI flag to carry it, so it is injected as a system-prompt hint
  ("Reasoning effort: {effort}. Match the depth of your analysis to it.") appended to the
  claude-code system prompt on every call that uses that backend.
- A new **Settings page** (`GET /settings` renders the current values; `POST /settings` writes a
  new `LlmSettings` via `state.llm.set_settings(...)`) lets the owner toggle backend and retune
  temperature/model/effort with **no restart**; the change is effective on the very next AI call.
- **New config** (initial/boot values only — the Settings page can retune both live):
  `IDEA_VAULT_OLLAMA_TEMPERATURE` (default `0.7`, validated to the range `0.0..=2.0`, falls back to
  the default outside that range or if unparsable) and `IDEA_VAULT_CLAUDE_EFFORT` (default `high`).
- `IDEA_VAULT_LLM_BACKEND` (`ollama` default | `claude-code`/`claude`) still exists and still picks
  which backend is active **at boot** — that part of ADR-0009 is unchanged — but it is now only the
  *initial* value of `LlmSettings.backend`, not a permanent choice; the Settings page can flip it at
  any time afterward.

This supersedes only the single sentence in ADR-0009's Decision section that reads *"`AppState`
holds one `LlmBackend`, selected at boot ... "* and the framing of `LlmBackend` as a two-variant
enum dispatched by `match`. Everything else in ADR-0009 — the four-method client surface
(`probe`/`chat`/`chat_stream`/`model`), the claude-code process-spawn mechanics, external context
(`--add-dir`), the importer, the safety posture (`cwd` defaults to the vault, never the app source),
and the new `AiError::Backend` variant — is unchanged and still authoritative.

## Consequences

- **No restart to compare backends or retune sampling.** The owner can A/B a discussion turn across
  Ollama and claude-code, or nudge temperature/effort, from the running UI.
- **Both backends are always constructed**, even if only one is ever selected in a given session —
  `ollama_only()` remains available as a lighter constructor for tests and Ollama-only runs (a
  placeholder, never-invoked `ClaudeCodeConfig`).
- **Settings are process-lifetime, not persisted.** `LlmSettings` lives only in the in-memory
  `Arc<RwLock<...>>`; a restart reverts to the `IDEA_VAULT_*` env defaults. (No durability
  requirement was raised for this — it is a live *tuning* control, not vault state.)
- **`RwLock` contention is negligible.** Every AI call takes a short read lock to snapshot settings
  before starting; the Settings POST takes a short write lock. Neither is held across an await
  point that touches the network.
- **Health/model-label reporting follows the toggle.** `probe()` and `model()` now report whichever
  backend is *currently* active, not whichever was active at boot — the degraded-AI banner
  ([D20](../05-ai-integration.md)) and the model label shown in the UI stay accurate after a live
  switch.

## Alternatives considered

- **Keep the boot-fixed enum, require a restart to switch backends** — simplest, but directly
  blocks the owner's stated workflow of comparing backends mid-session; rejected.
- **A `dyn LlmBackendTrait` with hot-swappable trait objects** — more "pluggable" in the abstract,
  but ADR-0009 already rejected `dyn` for a closed, small backend set on cost/dependency grounds;
  that reasoning still holds, and the struct-router shape achieves live-switching without it.
  Rejected.
- **Persist `LlmSettings` to disk (e.g. a dotfile) so the toggle survives a restart** — adds a
  second, out-of-vault durability concern for a single-owner localhost tool where a restart is rare
  and cheap; rejected as unnecessary scope for now. Revisit if the owner asks for persistence.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
