# 06 — Concept: Agents

> An **agent** is a scoped subagent *role* with a specific prompt persona and a defined input/output
> contract — the unit that a [swarm](./swarm.md) fans out and a [workflow](./workflows.md) sequences.
> Module: `concepts::agents`. (No dedicated diagram of its own; agents appear inside D3, D14, D19.)

## Model

An agent = **role prompt** + **I/O contract**. It is not a long-lived process; it is a configured way
of calling `ai` for one bounded task. Each agent:

- is given a **scoped persona** (what it is responsible for, what to ignore),
- receives a **budgeted context** ([D21](./swarm.md)) plus optionally a [skill](./skills.md) to apply,
- returns a **structured-ish result** the orchestrator can rank/merge (a critique, a finding list, a
  synthesis).

## Standard roles

The three roles that the "run it into the ground" loop leans on:

| Role | Persona | Typical input | Typical output |
|------|---------|---------------|----------------|
| **Critic** | Adversarial; find the strongest objections and failure modes | idea body + memory + a critical skill (premortem, cheapest-disproof) | ranked objections / risks |
| **Researcher** | Gather relevant considerations, precedents, constraints | idea body + focused question | notes / considerations (from model knowledge; offline) |
| **Synthesizer** | Neutral; merge many agent outputs into one coherent view | the set of prior agent outputs | consolidated position, tensions surfaced |

Roles are extensible — they are prompt configurations, so adding a role (e.g. "estimator",
"ethicist") is additive, like [skills](./skills.md).

## I/O contract

```text
AgentTask {
  role:     Critic | Researcher | Synthesizer | <custom>
  skill?:   <skill name to apply>          // optional lens
  context:  <budgeted block>               // from ai::budget (D21)
}
      │  concepts::agents runs the role prompt via ai::ollama (under the semaphore)
      ▼
AgentResult {
  role:     <role>
  content:  <text / list>                  // consumed by judge/synthesizer
}
```

The orchestrator (`concepts::swarm` / `concepts::workflows`) is responsible for building `AgentTask`s
and consuming `AgentResult`s; the agent module only knows how to *run one role well*.

## Relationships

- A **[swarm](./swarm.md)** ([D14](./swarm.md)) dispatches many `AgentTask`s in parallel (often the
  same idea, different roles/skills → diverse lenses), then a Synthesizer agent converges them.
- A **[workflow](./workflows.md)** ([D19](./workflows.md)) sequences agents deterministically
  (e.g. Critic → Researcher → Synthesizer).
- Agents apply **[skills](./skills.md)** as their lens.

## Mapping to code

- Role definitions + `AgentTask`/`AgentResult`: `concepts::agents`.
- Execution boundary: `ai::ollama` (all calls acquire the concurrency semaphore, [ADR-0006](../adr/0006-bounded-concurrency-swarm.md)).
- Orchestration: `concepts::swarm`, `concepts::workflows`.
