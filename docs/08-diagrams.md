# 08 — Diagram Registry

> The canonical index of every diagram in the documentation. Each diagram is **authored once** in its
> home document; this registry only catalogs and links to it (no diagram is copied here). If a
> reference elsewhere says "see D14", this table says where D14 lives.

## Conventions

- **ID** — stable `D1`…`D29` (see Coverage below for why the range runs past D25). References
  across docs use the ID.
- **Tool** — all diagrams are **Mermaid** in `mermaid` fenced code blocks, rendering inline on GitHub with no
  build step (see [ADR-0001](./adr/0001-server-rendered-htmx-over-spa.md) ethos; escape hatches below).
- **Home doc** — the single file the diagram is authored in.

## Registry

### Structural

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D1** | C4 Context | Owner ↔ idea-vault ↔ Ollama ↔ filesystem; offline boundary | [01-architecture](./01-architecture.md) |
| **D2** | C4 Container (flowchart) | Modules inside the binary + browser/disk/Ollama | [01-architecture](./01-architecture.md) |
| **D3** | C4 Component (flowchart) | Inside `concepts::swarm` + `ai`, routed through the live `LlmBackend` (not a fixed Ollama client) | [05-ai-integration](./05-ai-integration.md) |
| **D4** | Dependency graph | Module dependencies, allowed one-way direction | [02-module-reference](./02-module-reference.md) |
| **D5** | Layout (flowchart) | Crate module/file layout | [02-module-reference](./02-module-reference.md) |

### Data

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D6** | ER | SQLite index schema (ideas, tags, memory_facts, backlinks, search_fts) | [03-data-model](./03-data-model.md) |
| **D7** | ER | Vault on-disk entity map (idea.md, conversation.md, memory/) | [03-data-model](./03-data-model.md) |
| **D8** | Class | Frontmatter schema + IdeaState enum | [03-data-model](./03-data-model.md) |

### State

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D9** | State machine | Idea lifecycle Draft→InDiscussion→Stored→Reopened | [04-state-machine](./04-state-machine.md) |

### Flows (sequence / activity)

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D10** | Sequence | New-idea creation | [07-flows](./07-flows.md) |
| **D11** | Sequence | Chat turn → `LlmBackend` → detached background job → poll (`/pending`); no SSE (ADR-0010 supersedes ADR-0004) | [05-ai-integration](./05-ai-integration.md) |
| **D12** | Sequence | Store → memory extraction | [06-concepts/memory](./06-concepts/memory.md) |
| **D13** | Sequence | Reopen → load memory as context | [06-concepts/memory](./06-concepts/memory.md) |
| **D14** | Sequence | Subagent swarm fan-out → converge/synthesize, run as a background job (ADR-0010) | [06-concepts/swarm](./06-concepts/swarm.md) |
| **D15** | Sequence | Reindex — rebuild SQLite from markdown | [03-data-model](./03-data-model.md) |
| **D16** | Activity | HTTP request / middleware pipeline — AI-driven routes branch into a background job, not an SSE stream | [09-web-ui](./09-web-ui.md) |
| **D18** | Sequence | Skill invocation, run as a background job when interactive (ADR-0010) | [06-concepts/skills](./06-concepts/skills.md) |
| **D25** | Sequence | Startup / boot | [01-architecture](./01-architecture.md) |

### Structure of the web + orchestration

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D17** | Route graph | Every route (including `/settings`, `/pending`, `/history`, `/fork`, turn/memory delete) → response shape → template | [09-web-ui](./09-web-ui.md) |
| **D19** | DAG (activity) | Workflow orchestration (fan-out → judge → synthesize) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D20** | State machine | Ollama-unavailable degradation | [05-ai-integration](./05-ai-integration.md) |
| **D21** | Sequence | Concurrency & context-budget model | [06-concepts/swarm](./06-concepts/swarm.md) |
| **D22** | Activity | Slug lifecycle & collision handling | [03-data-model](./03-data-model.md) |
| **D23** | Data-flow | `[[slug]]` backlink resolution | [06-concepts/memory](./06-concepts/memory.md) |
| **D24** | Taxonomy (flowchart) | Error/failure domains → user outcomes | [05-ai-integration](./05-ai-integration.md) |

### Deployment (containers)

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D26** | Deployment | Container topology: app + ollama, network, volumes, bind mount | [12-deployment](./12-deployment.md) |
| **D27** | Flowchart | Multi-stage image build (cargo-chef → runtime) | [12-deployment](./12-deployment.md) |
| **D28** | Flowchart | CPU vs GPU compose composition (override merge) | [12-deployment](./12-deployment.md) |
| **D29** | Deployment | claude-code container topology: host CLI bind-mount, `CLAUDE_CODE_OAUTH_TOKEN`, `claude-state` volume | [12-deployment](./12-deployment.md) |

## Coverage

- **29 IDs, D1–D29** (D17 is used but note that D1–D25 was the originally-stated range; D26–D29
  were added for containerized deployment without renumbering — the range is D1–D29 in practice,
  not D1–D25), each authored exactly once. **D1–D15** are the mandatory core (they cover every
  flow named in [CLAUDE.md](../CLAUDE.md)); **D16–D25** complete the SOTA set; **D26–D29** cover
  containerized deployment.
- The six core flows from CLAUDE.md map to: new idea **D10**, chat (background job + poll, not SSE
  — ADR-0010) **D11**, store+memory **D12**, reopen+memory **D13**, swarm **D14**, reindex **D15**.

## Tooling notes & escape hatches

- **Default: Mermaid.** Text-based, git-diffable, renders on GitHub — consistent with the
  markdown-first product ethos.
- **C4 fallback:** if a renderer lacks Mermaid's `C4Context`, D1 is expressed as a plain flowchart
  (D2/D3 already are). No diagram depends on exotic renderer features.
- **Large-graph escape hatch:** if Mermaid auto-layout ever mangles a specific graph (most likely
  D4 as modules grow), escalate *that one diagram* to Graphviz DOT or Structurizr — keep the rest in
  Mermaid. Record any such exception in this section.
- **Literal function-level call graphs are intentionally NOT hand-drawn.** Once code exists, generate
  them with `cargo-modules` (module graph → DOT/SVG) into `docs/generated/`. Hand-authored diagrams
  cover architecture and flows; tooling covers exhaustive call graphs. This split is deliberate.

## Maintenance rule

When adding a diagram: give it the next ID, author it in the relevant topical doc, and add one row
here. Never paste a diagram into two files — reference the ID instead.
