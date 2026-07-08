# idea-vault — Documentation

The design foundation for **idea-vault**: a single-user, localhost, offline web tool where you bring
a raw idea, **run it into the ground** with a local AI, and store it in a markdown vault for later
resumption. It mirrors an LLM agent harness — memory, skills, agents, workflows, subagent swarming —
applied to interrogating one idea.

> **Status: built out.** The codebase implements the core loop these docs describe — see the
> top-level [CLAUDE.md](../CLAUDE.md) Status section for the current build state. CLAUDE.md is the
> north-star spec; everything here elaborates it. If a doc and CLAUDE.md ever disagree, that's
> drift — fix both in one change, in whichever direction is correct.

## How to read the diagrams

Every diagram is authored in a `mermaid` fenced code block and **renders inline on GitHub** — no build
step. Each has a stable ID (**D1**–**D30**) catalogued in [08-diagrams](./08-diagrams.md). To render
locally, use any Mermaid-aware markdown previewer.

## Reading order

New here? Read top to bottom:

1. [00-vision](./00-vision.md) — what the product is and is not.
2. [11-glossary](./11-glossary.md) — precise terms (used verbatim everywhere else).
3. [01-architecture](./01-architecture.md) — system context, containers, boot (D1, D2, D25).
4. [02-module-reference](./02-module-reference.md) — the single-crate module graph + rules (D4, D5).
5. [03-data-model](./03-data-model.md) — vault-on-disk truth + SQLite index + reindex (D6–D8, D15, D22).
6. [04-state-machine](./04-state-machine.md) — the idea lifecycle (D9).
7. [05-ai-integration](./05-ai-integration.md) — Ollama + claude-code, background-job flow, degradation, errors (D3, D11, D20, D24).
8. [06-concepts/](./06-concepts/) — the harness primitives:
   [memory](./06-concepts/memory.md) (D12, D13, D23),
   [skills](./06-concepts/skills.md) (D18),
   [agents](./06-concepts/agents.md),
   [workflows](./06-concepts/workflows.md) (D19),
   [swarm](./06-concepts/swarm.md) (D14, D21, D30).
9. [07-flows](./07-flows.md) — index of runtime flows (authors D10).
10. [09-web-ui](./09-web-ui.md) — routes, middleware, templates (D16, D17).
11. [12-deployment](./12-deployment.md) — containerized local hosting, with/without GPU (D26–D28).
12. [08-diagrams](./08-diagrams.md) — the full diagram registry.
13. [10-testing-strategy](./10-testing-strategy.md) — invariants and how they're tested.

For running the stack, the top-level [README](../README.md) has the Docker quickstart.

Decision records are in [adr/](./adr/) — read these for the *why* behind any choice.

## Document map

| Doc | Purpose | Diagrams |
|-----|---------|----------|
| [00-vision](./00-vision.md) | Product intent, the core loop, non-goals | — |
| [01-architecture](./01-architecture.md) | C4 context/container, boot, request topology | D1, D2, D25 |
| [02-module-reference](./02-module-reference.md) | Single-crate modules + one-way deps | D4, D5 |
| [03-data-model](./03-data-model.md) | Vault contract + SQLite index + reindex | D6, D7, D8, D15, D22 |
| [04-state-machine](./04-state-machine.md) | Idea lifecycle | D9 |
| [05-ai-integration](./05-ai-integration.md) | Ollama + claude-code boundary (live router), background-job flow, degradation, errors | D3, D11, D20, D24 |
| [06-concepts/memory](./06-concepts/memory.md) | Extract on Store, load on Reopen, backlinks | D12, D13, D23 |
| [06-concepts/skills](./06-concepts/skills.md) | Reusable ideation moves | D18 |
| [06-concepts/agents](./06-concepts/agents.md) | Subagent roles + I/O contract | — |
| [06-concepts/workflows](./06-concepts/workflows.md) | Deterministic orchestration | D19 |
| [06-concepts/swarm](./06-concepts/swarm.md) | Bounded fan-out/converge, budgets, knowledge extraction | D14, D21, D30 |
| [07-flows](./07-flows.md) | Runtime flow index | D10 |
| [09-web-ui](./09-web-ui.md) | Routes, middleware, templates, HTMX (background-job polling) | D16, D17 |
| [12-deployment](./12-deployment.md) | Containerized local hosting, GPU/no-GPU, claude-code in containers | D26, D27, D28, D29 |
| [08-diagrams](./08-diagrams.md) | Diagram registry (D1–D30) | (catalog) |
| [10-testing-strategy](./10-testing-strategy.md) | Invariants + test approach | — |
| [11-glossary](./11-glossary.md) | Canonical vocabulary | — |
| [adr/](./adr/) | Architecture Decision Records 0001–0015 | — |

## Locked decisions (at a glance)

- **UI:** axum + Askama + HTMX, single binary, no JS build ([ADR-0001](./adr/0001-server-rendered-htmx-over-spa.md)).
- **AI turns:** detached background jobs polled via `GET /idea/:slug/pending`, not SSE ([ADR-0010](./adr/0010-ai-turns-as-background-jobs.md), supersedes [ADR-0004](./adr/0004-sse-token-streaming.md)).
- **Storage:** markdown = truth, SQLite = rebuildable index ([ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md)).
- **AI backend:** Ollama local by default, `:11434`, plus an optional live-switchable claude-code backend ([ADR-0003](./adr/0003-ollama-local-only-ai.md), [ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md), [ADR-0011](./adr/0011-live-switchable-llm-backend.md)).
- **Code:** single crate, strict one-way module deps ([ADR-0005](./adr/0005-single-crate-vs-workspace.md)).
- **Swarm:** bounded concurrency + context budget ([ADR-0006](./adr/0006-bounded-concurrency-swarm.md)).
- **State:** canonical in frontmatter ([ADR-0007](./adr/0007-state-in-frontmatter-not-db.md)).
- **Auto-compact:** a fingerprinted, deletable `compacted.md` sidecar rolls up the conversation head, folded pre-emptively and best-effort before each reply ([ADR-0012](./adr/0012-auto-compact.md)).
- **Deployment:** app + Ollama in containers, GPU optional (override), env-driven config ([ADR-0008](./adr/0008-containerized-local-deployment.md)).
- **claude-code in containers:** host CLI bind-mounted read-only, auth via `claude setup-token` → `CLAUDE_CODE_OAUTH_TOKEN`, CLI state on a `claude-state` volume via `HOME=/claude` ([ADR-0013](./adr/0013-containerized-claude-code.md)).
- **Context budget:** derived live per backend/model (`/api/show` for Ollama, model-name mapping for claude-code), overridable per backend, no longer a fixed constant ([ADR-0014](./adr/0014-dynamic-context-budget.md)).
- **Knowledge extraction:** per-lens findings persisted as `artifacts/*.md` truth files alongside a converged synthesis, a deliberate divergence from the swarm's discard-intermediates rule ([ADR-0015](./adr/0015-knowledge-extraction-artifacts.md)).

## Beyond these docs

The container files ([`Dockerfile`](../Dockerfile), [`docker-compose.yml`](../docker-compose.yml),
[`docker-compose.gpu.yml`](../docker-compose.gpu.yml)) implement the deployment contract
([12-deployment](./12-deployment.md)). The `src/` layout follows
[02-module-reference](./02-module-reference.md); see [CLAUDE.md](../CLAUDE.md) for the current
build status and the real, runnable commands.

## Contributing to the docs

- Keep [11-glossary](./11-glossary.md) terms authoritative; use them verbatim.
- Author each diagram **once** in its home doc; reference by ID elsewhere. Register new diagrams in
  [08-diagrams](./08-diagrams.md) ([maintenance rule](./08-diagrams.md#maintenance-rule)).
- Record decisions as ADRs (template: [adr/0000](./adr/0000-adr-template.md)); ADRs are immutable once
  Accepted — supersede, don't edit.
