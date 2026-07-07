# 06 — Concept: Skills

> A **skill** is a named, reusable ideation move — a parameterized prompt template the AI can apply
> to the current idea on demand (premortem, cheapest-disproof, market-size, devil's advocate…).
> Home of **D18** (skill invocation). Module: `concepts::skills`.

## Model

A skill is data, not code: a name, a description, and a prompt template with slots for idea context.
Skills live in a **registry** loaded at startup; they are the reusable vocabulary of "moves" the
owner (or a workflow/swarm) can invoke against an idea.

```yaml
# conceptual shape of a skill definition
name: premortem
description: Assume the idea failed; enumerate the most likely causes.
inputs: [idea_body, memory, recent_conversation]
prompt: |
  The idea below has failed badly 12 months from now. Working backwards,
  list the most likely causes of failure, ranked by probability × impact.
  {context}
```

Skills are:

- **Composable** — a [workflow](./workflows.md) is often a sequence of skills; a [swarm](./swarm.md)
  can assign a different skill to each agent (diverse lenses).
- **Budget-aware** — the `{context}` slot is filled by `ai::budget` ([D21](./swarm.md)), not the raw
  full history.
- **Stateless** — applying a skill appends its output as an assistant turn; it does not itself change
  idea state.

## D18 — Skill invocation flow

An interactive skill run (`POST /idea/:slug/skill/:name`) is a **background job**
([ADR-0010](../adr/0010-ai-turns-as-background-jobs.md)), the same claim → spawn → poll shape as
chat: the route claims the per-idea job slot and returns a "thinking" indicator immediately; the
owner sees the appended turn only once `GET /idea/:slug/pending` reports the job done. There is no
token streaming to the UI — the whole skill output lands in one swap. A workflow/swarm invoking a
skill internally does not claim the job registry itself (its *caller* — R6 or R7 — already owns the
one job for that idea); it just calls `invoke` directly.

```mermaid
sequenceDiagram
    autonumber
    participant U as Owner / workflow / swarm
    participant J as web::jobs (interactive case only)
    participant Reg as concepts::skills (registry)
    participant Bud as ai::budget
    participant L as ai::backend::LlmBackend
    participant V as vault::store

    U->>J: POST /idea/:slug/skill/:name — claim job, return indicator immediately
    J->>Reg: invoke(skill_name, idea)
    Reg->>Reg: look up skill template
    Reg->>Bud: fill {context} for skill.inputs (under budget)
    Bud-->>Reg: hydrated prompt
    Reg->>L: chat(prompt) [semaphore, active backend]
    L-->>Reg: result
    Reg->>V: append result as assistant turn to conversation.md (only if non-empty)
    Reg-->>J: skill output
    J-->>U: mark_done; next poll returns the finished transcript
```

## Registry & discovery

- Built-in skills ship with the binary; the registry is populated at boot
  (`SkillRegistry::builtin`).
- A skill is selected in the UI (a menu of moves) or named by a workflow/swarm step.
- Extensibility: skills being plain templates means new ones are additive — no code path changes to
  add a "move".

### Built-in skills

| Name | Move |
|------|------|
| `premortem` | Assume the idea failed; enumerate the most likely causes. |
| `cheapest-disproof` | Find the fastest, cheapest experiment that could disprove the idea. |
| `devils-advocate` | Argue against the idea as persuasively as possible. |
| `constraints` | Map the practical constraints, prerequisites, and precedents bearing on the idea. |
| `second-order-effects` | Assume the idea works; trace the second-order and knock-on effects. |
| `build-prompt` | The **capstone move**: fold the entire discussion into a single, ready-to-paste build prompt for a coding agent (e.g. Claude Code) — settled decisions/constraints/disproofs extracted (not transcribed), an ordered plan, explicit fan-out-vs-sequential guidance, and acceptance criteria. |

`premortem`, `cheapest-disproof`, `constraints`, and `second-order-effects` are also the default
angle set a swarm run uses when the owner doesn't specify angles ([D14](./swarm.md)).

## Distinction from adjacent concepts

| Concept | What it is | Relation to skills |
|---------|-----------|--------------------|
| **Skill** | one reusable prompt move | the atomic unit |
| **[Agent](./agents.md)** | a scoped role (critic/researcher/…) | an agent *applies* skills within its role |
| **[Workflow](./workflows.md)** | deterministic multi-step pipeline | a sequence/DAG of skills+agents |
| **[Swarm](./swarm.md)** | parallel fan-out | assigns different skills to parallel agents |

## Mapping to code

- Registry + invocation: `concepts::skills`.
- Context hydration: `ai::budget`.
- Output persistence: `vault::store` (append to `conversation.md`).

## Related

- [workflows](./workflows.md) — D19, how skills are sequenced.
- [swarm](./swarm.md) — D14, how skills are parallelized across agents.
- [ADR-0010](../adr/0010-ai-turns-as-background-jobs.md) — interactive skill runs are background jobs.
- [ADR-0011](../adr/0011-live-switchable-llm-backend.md) — the `LlmBackend` router skills call through.
