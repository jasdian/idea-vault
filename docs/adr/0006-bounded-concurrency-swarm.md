# ADR-0006 — Bounded concurrency and context budgeting for swarms

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

Subagent swarming fans out many agents against one idea. Against a **single local Ollama server**
([ADR-0003](./0003-ollama-local-only-ai.md)), naive parallelism would thrash: Ollama serializes or
slows under concurrent requests, memory blows up, and the machine becomes unusable. We also cannot
assume large context windows on local models.

## Decision

We will cap swarm parallelism with a **semaphore (bounded concurrency)** and give each agent a
**bounded context budget** (`ai::budget`). The concurrency limit is configurable (`config.rs`) with a
conservative default; excess agent tasks **queue** rather than run. Every agent prompt is assembled
to fit the budget (idea body + selected memory + trimmed conversation), not the full history.

## Consequences

- The machine stays responsive during a swarm; throughput is predictable, latency is bounded per
  wave rather than exploding.
- Swarm code fans out N tasks but only K run at once (K = limit); the orchestrator must handle
  queueing and backpressure ([D21](../06-concepts/swarm.md)).
- Context budgeting means agents may not see everything; workflows must select the *relevant* slice,
  which is a deliberate design constraint, not a bug.
- Determinism improves: with a fixed limit and budget, runs are more reproducible and debuggable.

## Alternatives considered

- **Unbounded fan-out** — simplest to code, but melts a single local machine and gives worse latency
  than a bounded queue. Rejected outright.
- **Fixed small pool with no budgeting** — caps concurrency but still overflows context on long
  ideas. Rejected: both limits are needed.
- **Offload swarm to cloud for scale** — sidesteps the machine limit but violates
  [ADR-0003](./0003-ollama-local-only-ai.md). Rejected.
