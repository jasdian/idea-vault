# 06 — Concept: Workflows

> A **workflow** is a *deterministic* multi-step orchestration over an idea — a fixed graph of
> [skill](./skills.md)/[agent](./agents.md) steps (fan-out → judge → synthesize), as opposed to
> free-form chat. Home of **D19** (workflow DAG). Module: `concepts::workflows`.

## Model

Where free chat is model-driven (the AI decides what to do next), a workflow is **script-driven**:
the *control flow* is fixed by the workflow definition, and only the *content* of each step is
generated. This makes runs reproducible and debuggable — the same idea through the same workflow
takes the same path.

A workflow is a DAG of steps; each step is either a single skill/agent call or a fan-out over
parallel agents whose results feed a downstream judge/synthesize step.

## D19 — Workflow orchestration (DAG)

The canonical "interrogate an idea" workflow: fan out diverse critics, judge/rank their findings,
then synthesize a single position.

```mermaid
flowchart TD
    START(["workflow start: idea in InDiscussion/Reopened"]) --> FANOUT

    subgraph FANOUT["fan-out (parallel agents, bounded — D21)"]
        A1["Critic · premortem"]
        A2["Critic · cheapest-disproof"]
        A3["Researcher · constraints"]
        A4["Critic · second-order effects"]
    end

    A1 --> JUDGE
    A2 --> JUDGE
    A3 --> JUDGE
    A4 --> JUDGE

    JUDGE["judge — rank/dedupe findings"] --> SYNTH
    SYNTH["Synthesizer — merge into one position"] --> APPEND["append result as assistant turn"]
    APPEND --> END(["workflow end"])
```

## Determinism & failure

- **Deterministic control flow:** the node graph is fixed by the workflow definition; only step
  outputs vary. Contrast with a swarm invoked ad hoc from chat.
- **Bounded fan-out:** the parallel stage runs under the same concurrency semaphore and context
  budget as any swarm ([D21](./swarm.md), [ADR-0006](../adr/0006-bounded-concurrency-swarm.md)).
- **Step failure:** a failed agent step drops to a null result and is skipped by the judge (the
  workflow degrades rather than aborting) — mirrors the swarm failure model ([D14](./swarm.md)).
- **Persistence:** the final synthesized output is appended to `conversation.md`; intermediate agent
  outputs may be logged but are not necessarily persisted as turns (kept out of truth to reduce
  noise).

## UI trigger

The canonical `interrogate` workflow (currently the only entry in `builtin_workflows()`) is
UI-triggerable, not just a library call: `POST /idea/{slug}/workflow/{name}`
([R22](../09-web-ui.md#d17--route-map), `web::routes::memory::run_workflow`) follows the same
claim → spawn → poll background-job shape as the skill (R6) and swarm (R7) routes
([D11](../05-ai-integration.md), [ADR-0010](../adr/0010-ai-turns-as-background-jobs.md)). Two
synchronous guards run before the job is claimed: the idea must be `InDiscussion`/`Reopened` (400
otherwise) and `name` must resolve via `get_workflow` (404 if unknown, so a bad name never becomes
a background job at all). Only the converged synthesis is persisted, as one
`## assistant (workflow: {name})` turn — the transcript label keeps the workflow kind
(`foil · workflow {name}`), so a workflow run stays visually distinct from a same-named skill turn
(`foil · {name}`). `templates/_actions.html` renders one `chip chip--workflow` button per
`builtin_workflows()` entry.

## Workflow vs swarm

They share machinery (bounded parallel agents), but:

| | Workflow | Swarm |
|--|----------|-------|
| Control flow | fixed DAG, deterministic | a fan-out primitive used *within* steps or ad hoc |
| Invocation | run a named pipeline | "swarm this idea" from chat, or a workflow's fan-out stage |
| Reproducibility | high (same path) | high per-wave, but composed freely |

A workflow *uses* the swarm fan-out as its parallel stage; a swarm is the lower-level primitive
([D14](./swarm.md)).

## Mapping to code

- Workflow definitions + runner: `concepts::workflows`.
- Fan-out stage: delegates to `concepts::swarm`.
- Steps: `concepts::agents` applying `concepts::skills`.

## Related

- [swarm](./swarm.md) — D14/D21, the parallel primitive and its limits.
- [agents](./agents.md) — the roles sequenced here.
- The host tool's own Workflow concept is the inspiration; here it is applied to one idea.
