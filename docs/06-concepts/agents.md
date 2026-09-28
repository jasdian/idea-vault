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

The roles the "run it into the ground" loop leans on (`concepts::agents::AgentRole`). A skill's
`role` frontmatter field ([skills](./skills.md)) names which of the first five it runs under when an
orchestrator fans it out.

| Role | Persona | Typical input | Typical output |
|------|---------|---------------|----------------|
| **Critic** | Adversarial: find the strongest objections and failure modes | idea body + memory + a critical skill (premortem, cheapest-disproof) | ranked objections / risks |
| **Researcher** | Gather relevant considerations, precedents, constraints | idea body + focused question (constraints, market-size) | notes / considerations (from model knowledge) |
| **Advocate** | Make the strongest honest case *for* the idea; no attack, no hedging | idea body + the steelman skill | the idea's best version and why it could win |
| **Harvester** | Extract only what the material already says; add nothing | the discussion + an `extract-*` lens | bullets of decisions / facts / questions / risks / actions |
| **Synthesizer** | Neutral: merge many agent outputs into one coherent view | the idea statement + labelled findings (with audit verdicts) | consolidated position, tensions surfaced |
| **Auditor** | Sceptical by default, no stake in the findings | only the numbered findings + idea/memory/discussion | one `F<n>: CONFIRMED\|UNCERTAIN\|REFUTED — reason` line per finding ([ADR-0023](../adr/0023-verification-layer.md)) |

Roles are prompt configurations, so adding one (an "estimator", an "ethicist") is additive in
spirit. In code they form a closed enum: a new role means a new variant plus a persona, not a data
file.

## I/O contract

```text
AgentTask {
  role:     Critic | Researcher | Advocate | Harvester | Synthesizer | Auditor
  skill?:   <skill name to apply>          // optional lens
  context:  <budgeted block>               // from ai::budget (D21)
}
      │  concepts::agents runs persona + (skill prompt with {context} | bare context) via the
      │  active LlmBackend (under the semaphore), then repairs the answer against the skill's
      │  output contract (ai::contract — repair only, never a retry)
      ▼
AgentResult {
  role:     <role>
  lens:     <skill name, if any>           // provenance: "premortem · critic"
  content:  <text / list>                  // split into findings, audited, synthesized
}
```

The orchestrator (`concepts::swarm` / `concepts::workflows` / `concepts::knowledge`) is responsible
for building `AgentTask`s and consuming `AgentResult`s; the agent module only knows how to *run one
role well*.

## Relationships

- A **[swarm](./swarm.md)** ([D14](./swarm.md)) dispatches many `AgentTask`s in parallel (often the
  same idea, different roles/skills → diverse lenses), an Auditor judges the findings, then a
  Synthesizer agent converges them.
- A **[workflow](./workflows.md)** ([D19](./workflows.md), [D32](./workflows.md)) stages agents
  deterministically (e.g. Advocate → Critics ∥ → Auditor → Synthesizer).
- Agents apply **[skills](./skills.md)** as their lens.

## Mapping to code

- Role definitions + `AgentTask`/`AgentResult`: `concepts::agents`.
- Execution boundary: `ai::backend::LlmBackend` — the live router over Ollama/claude-code
  ([ADR-0011](../adr/0011-live-switchable-llm-backend.md)); all calls acquire the concurrency
  semaphore ([ADR-0006](../adr/0006-bounded-concurrency-swarm.md)).
- Orchestration: `concepts::swarm`, `concepts::workflows`, `concepts::knowledge`; the audit stage:
  `concepts::audit`.
