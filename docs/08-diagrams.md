# 08 — Diagram Registry

> The canonical index of every diagram in the documentation. Each diagram is **authored once** in its
> home document; this registry only catalogs and links to it (no diagram is copied here). If a
> reference elsewhere says "see D14", this table says where D14 lives.

## Conventions

- **ID** — stable `D1`…`D42` (see Coverage below for why the range runs past D25). References
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
| **D6** | ER | SQLite index schema (ideas, tags, memory_facts, backlinks, search_fts, fact_links, edges) | [03-data-model](./03-data-model.md) |
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
| **D12** | Sequence | Store → memory extraction (consolidate then distil; evidence gate + quarantine; ADD/UPDATE/NOOP — ADR-0023) | [06-concepts/memory](./06-concepts/memory.md) |
| **D13** | Sequence | Reopen → load memory as context | [06-concepts/memory](./06-concepts/memory.md) |
| **D14** | Sequence | Subagent swarm fan-out → judge → factored audit → converge/synthesize, run as a background job (ADR-0010, ADR-0023) | [06-concepts/swarm](./06-concepts/swarm.md) |
| **D30** | Sequence | Knowledge extraction — per-lens artifacts + synthesis, run as a background job (ADR-0015) | [06-concepts/swarm](./06-concepts/swarm.md) |
| **D15** | Sequence | Reindex — rebuild SQLite from markdown | [03-data-model](./03-data-model.md) |
| **D16** | Activity | HTTP request / middleware pipeline — AI-driven routes branch into a background job, not an SSE stream | [09-web-ui](./09-web-ui.md) |
| **D18** | Sequence | Skill invocation with output-contract validation + at most one retry, run as a background job when interactive; a build_plan skill persists via build_plan::finish (ADR-0010, ADR-0023, ADR-0030) | [06-concepts/skills](./06-concepts/skills.md) |
| **D25** | Sequence | Startup / boot | [01-architecture](./01-architecture.md) |
| **D39** | Sequence | Run-journal lifecycle: claim → `open_run` (`RunStarted`, prune to 50) → `spawn_job` → `LlmCall` / `ToolCall` / `Contract` / `Verdict` → `RunFinished` (done, failed, cancelled by the writer's `Drop`, panicked); an open failure runs unjournaled (ADR-0037) | [05-ai-integration](./05-ai-integration.md) |
| **D40** | Flowchart | Parser corpus and regrade: journaled verdict → haystack recovery or skip → today's parser → same / flip → exit; hand-run `--export` into the committed corpus and the snapshot test (ADR-0038) | [10-testing-strategy](./10-testing-strategy.md) |
| **D41** | Flowchart | The no-mistakes gate: the seven fixed steps, the bless refusal, and the findings protocol (no-op / auto-fix / ask-user) on a red step (ADR-0041) | [14-no-mistakes-gate](./14-no-mistakes-gate.md) |

### Structure of the web + orchestration

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D17** | Route graph | Every route (including `/settings`, `/pending`, `/history`, `/fork`, turn/memory delete) → response shape → template | [09-web-ui](./09-web-ui.md) |
| **D19** | DAG (activity) | The interrogate workflow (fan-out → judge → audit → synthesize) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D32** | Flowchart | Workflow stage model: the eight stage kinds (FanOut / Chain / Audit / Synthesize, plus Ground / Panel / Loop / Refine expanded in D35–D37), the call budget, failure paths, the all-or-nothing persist tail with stage artifacts, the build-plan persist branch (ADR-0022, ADR-0023, ADR-0030, ADR-0034) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D33** | Flowchart | Plan workbench: answer → validate → owner turns → reset/apply/re-gate → new linked version (+ superseded, busy and re-plan branches) (ADR-0032) | [06-concepts/skills](./06-concepts/skills.md) |
| **D34** | Flowchart | MCP replay decision: in-flight reattach → explicit key / args hash + turn count + idea stamp → replay or fresh run; what is cached (ADR-0033) | [13-mcp-server-inbound](./13-mcp-server-inbound.md) |
| **D35** | Flowchart | Ground stage: skip with no sources → code map → readers → parse/dedupe → verify each anchor in code → carry only verified anchors, stage a `ground_map` (ADR-0034) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D36** | Flowchart | Panel stage: proposals → no contest or cold one-proposal scoring by the Auditor role → pure aggregate (totals, tie-break, grafts) → findings + scorecard → graft-mode synthesis (ADR-0034) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D37** | State machine | Loop and Refine: per-round precheck, absorb in step order, stop reasons Dry/Cap/Failed, then audit-gated Refine rounds by id (ADR-0034) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D38** | Flowchart | Workflow registry: built-ins + `vault/.workflows/` → parse by hand-dispatched `kind:` → validate against the skill registry → issue or register → one `Book` pair, swapped whole on reload (ADR-0035) | [06-concepts/workflows](./06-concepts/workflows.md) |
| **D42** | Flowchart | Make skill: button → R51 guard (not Draft, ≥ 2 owner turns, ≥ 1 move) → claim → distil job (≤ 2 calls, `skill_draft` contract) → finalize + evidence → `skill_draft` artifact + notice, no turn → R19 review panel → R52 save check (loads, not built-in/internal, fresh digest) → `write_owner_skill` → reload → skill book (ADR-0042) | [06-concepts/skills](./06-concepts/skills.md) |
| **D20** | State machine | Ollama-unavailable degradation | [05-ai-integration](./05-ai-integration.md) |
| **D21** | Sequence | Concurrency & context-budget model | [06-concepts/swarm](./06-concepts/swarm.md) |
| **D22** | Activity | Slug lifecycle & collision handling | [03-data-model](./03-data-model.md) |
| **D23** | Data-flow | `[[slug]]` backlink resolution and `[[idea#fact]]` → `fact_links` | [06-concepts/memory](./06-concepts/memory.md) |
| **D24** | Taxonomy (flowchart) | Error/failure domains → user outcomes | [05-ai-integration](./05-ai-integration.md) |

### Deployment (containers)

| ID | Type | Depicts | Home |
|----|------|---------|------|
| **D26** | Deployment | Container topology: app + ollama, network, volumes, bind mount | [12-deployment](./12-deployment.md) |
| **D27** | Flowchart | Multi-stage image build (cargo-chef → runtime) | [12-deployment](./12-deployment.md) |
| **D28** | Flowchart | CPU vs GPU compose composition (override merge) | [12-deployment](./12-deployment.md) |
| **D29** | Deployment | claude-code container topology: host CLI bind-mount, `CLAUDE_CODE_OAUTH_TOKEN`, `claude-state` volume | [12-deployment](./12-deployment.md) |
| **D31** | Deployment | Reference-source topology: registry dotfile → generated override → owner `docker compose up -d` → ro binds `/mnt/sources/<name>` → app resolve/probe → deterministic tool leaves (Ollama) + `--add-dir` (claude-code) (ADR-0021) | [12-deployment](./12-deployment.md) |

## Coverage

- **42 IDs, D1–D42** (D17 is used but note that D1–D25 was the originally-stated range; D26–D29
  were added for containerized deployment, D30 for knowledge extraction, D31 for reference
  sources, D32 for the workflow stage model, D33/D34 for the plan workbench and MCP replay, and
  D35–D38 for the grounded, ranked and bounded workflow stages and the workflow registry, and
  D39–D41 for the run journal, the parser corpus and the no-mistakes gate, and D42 for make skill, all
  without renumbering — the range is D1–D42 in
  practice, not D1–D25), each authored exactly once.
  **D1–D15** are the mandatory core (they cover every flow named in [CLAUDE.md](../CLAUDE.md));
  **D16–D25** complete the SOTA set; **D26–D29** cover containerized deployment; **D30** covers
  knowledge extraction ([ADR-0015](./adr/0015-knowledge-extraction-artifacts.md)); **D31** covers
  reference sources ([ADR-0021](./adr/0021-reference-sources.md)); **D32** covers the staged
  workflow model ([ADR-0022](./adr/0022-skills-as-markdown-and-the-skill-book.md),
  [ADR-0023](./adr/0023-verification-layer.md)); **D33** covers the plan workbench
  ([ADR-0032](./adr/0032-plan-workbench-answers-and-versions.md)); **D34** covers MCP idempotent
  replay ([ADR-0033](./adr/0033-mcp-idempotent-replay-and-plan-tools.md)); **D35–D37** cover the
  Ground, Panel, and Loop/Refine stages ([ADR-0034](./adr/0034-grounded-ranked-and-bounded-workflow-stages.md)),
  **D38** the workflow registry ([ADR-0035](./adr/0035-workflows-as-markdown-and-the-workflow-book.md));
  **D39** the run journal ([ADR-0037](./adr/0037-run-journal-diagnostics-only-call-record.md)); **D40**
  the parser corpus and regrade ([ADR-0038](./adr/0038-parser-corpus-and-read-only-regrade.md));
  **D41** the no-mistakes gate ([ADR-0041](./adr/0041-no-mistakes-gate.md)); **D42** make skill
  ([ADR-0042](./adr/0042-make-skill-distil-owner-skills.md)).
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
