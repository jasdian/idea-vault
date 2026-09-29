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
step. Each has a stable ID (**D1**–**D31**) catalogued in [08-diagrams](./08-diagrams.md). To render
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
11. [12-deployment](./12-deployment.md) — containerized local hosting, with/without GPU (D26–D29, D31).
12. [13-mcp-server-inbound](./13-mcp-server-inbound.md) — exposing idea-vault itself as an MCP server.
13. [08-diagrams](./08-diagrams.md) — the full diagram registry.
14. [10-testing-strategy](./10-testing-strategy.md) — invariants and how they're tested.

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
| [06-concepts/skills](./06-concepts/skills.md) | Reusable ideation moves as markdown files, the skill book, the spine | D18 |
| [06-concepts/agents](./06-concepts/agents.md) | Subagent roles + I/O contract | — |
| [06-concepts/workflows](./06-concepts/workflows.md) | Deterministic staged orchestration | D19, D32 |
| [06-concepts/swarm](./06-concepts/swarm.md) | Bounded fan-out/converge, budgets, knowledge extraction | D14, D21, D30 |
| [07-flows](./07-flows.md) | Runtime flow index | D10 |
| [09-web-ui](./09-web-ui.md) | Routes, middleware, templates, HTMX (background-job polling) | D16, D17 |
| [12-deployment](./12-deployment.md) | Containerized local hosting, GPU/no-GPU, claude-code in containers, reference sources | D26, D27, D28, D29, D31 |
| [13-mcp-server-inbound](./13-mcp-server-inbound.md) | Inbound MCP server (`/api/mcp`), the Task↔Job bridge, and the reusable cookbook | — |
| [08-diagrams](./08-diagrams.md) | Diagram registry (D1–D32) | (catalog) |
| [10-testing-strategy](./10-testing-strategy.md) | Invariants + test approach | — |
| [11-glossary](./11-glossary.md) | Canonical vocabulary | — |
| [adr/](./adr/) | Architecture Decision Records 0001–0028 | — |

## Locked decisions (at a glance)

- **UI:** axum + Askama + HTMX, single binary, no JS build ([ADR-0001](./adr/0001-server-rendered-htmx-over-spa.md)).
- **AI turns:** detached background jobs polled via `GET /idea/:slug/pending`, not SSE ([ADR-0010](./adr/0010-ai-turns-as-background-jobs.md), supersedes [ADR-0004](./adr/0004-sse-token-streaming.md)).
- **Storage:** markdown = truth, SQLite = rebuildable index ([ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md)).
- **AI backend:** Ollama local by default, `:11434`, plus an optional live-switchable claude-code backend ([ADR-0003](./adr/0003-ollama-local-only-ai.md), [ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md), [ADR-0011](./adr/0011-live-switchable-llm-backend.md)).
- **Code:** single crate, strict one-way module deps ([ADR-0005](./adr/0005-single-crate-vs-workspace.md)).
- **Swarm:** bounded concurrency + context budget ([ADR-0006](./adr/0006-bounded-concurrency-swarm.md)).
- **State:** canonical in frontmatter ([ADR-0007](./adr/0007-state-in-frontmatter-not-db.md)).
- **Auto-compact:** a fingerprinted, deletable `compacted.md` sidecar rolls up the conversation head, folded pre-emptively and best-effort before each reply ([ADR-0012](./adr/0012-auto-compact.md)); a forced "compact now" fold targets a zero verbatim tail (not the automatic path's 0.40 target) and a genuine no-op surfaces as a one-shot notice ([ADR-0016](./adr/0016-forced-compact-folds-fully.md)).
- **Deployment:** app + Ollama in containers, GPU optional (override), env-driven config ([ADR-0008](./adr/0008-containerized-local-deployment.md)).
- **claude-code in containers:** host CLI bind-mounted read-only, auth via `claude setup-token` → `CLAUDE_CODE_OAUTH_TOKEN`, CLI state on a `claude-state` volume via `HOME=/claude` ([ADR-0013](./adr/0013-containerized-claude-code.md)).
- **Vault mount is verified, not created:** a `.idea-vault-root` marker (`vault::store::VAULT_MARKER`) lets `vault::store::ensure_vault_dir` tell a genuine first run from an unmounted ghost dir (`VaultInit::Suspect`: logged, never blessed), reindex refuses to wipe a populated index from an empty walk (`IndexError::RefusingEmptyRebuild`; `index::reindex::reindex_forced` is the override), and `/admin/health` returns 503 on an unusable vault ([ADR-0019](./adr/0019-vault-mount-verified-not-created.md)).
- **Nothing auto-starts:** every compose service is `restart: "no"` and the stack is brought up by hand, so the boot race with a late host mount never opens; `create_host_path: false` refuses to create a missing bind source but is not a boot-race guard ([ADR-0020](./adr/0020-boot-order-and-ghost-binds.md), amends ADR-0019 guard 1).
- **Context budget:** derived live per backend/model (`/api/show` for Ollama, model-name mapping for claude-code), overridable per backend, no longer a fixed constant ([ADR-0014](./adr/0014-dynamic-context-budget.md)).
- **Knowledge extraction:** per-lens findings persisted as `artifacts/*.md` truth files alongside a converged synthesis, a deliberate divergence from the swarm's discard-intermediates rule ([ADR-0015](./adr/0015-knowledge-extraction-artifacts.md)).
- **Web access:** one live `web_access` setting (default on) lets either backend crawl the internet — a bounded `ai::web` tool-calling loop on Ollama, allow/deny of the CLI's own WebSearch/WebFetch on claude-code; off restores a fully offline run ([ADR-0017](./adr/0017-web-access-tools.md)).
- **MCP servers:** an owner-managed registry of MCP Streamable-HTTP endpoints (`crate::mcp`, persisted app config at `<vault>/.mcp-servers.json`, not vault truth) is bridged to either backend by `ai::backend` alone, keeping the `mcp`/`ai::mcp` split one-way and acyclic; managed live from the `/mcp` page, with probes run inline rather than as background jobs ([ADR-0018](./adr/0018-mcp-servers.md)).
- **Reference sources:** an owner-managed registry of named read-only source dirs (`crate::sources`, app config at `<vault>/.sources.json`, no enabled flag — per-idea frontmatter `sources: [name]` is the opt-in) generates a compose override (`<vault>/.docker-compose.sources.yml`, ro binds at `/mnt/sources/<name>`) that the **owner** applies with `docker compose up -d` — the app never runs docker; attached sources reach the model per turn as deterministic `source_list`/`source_grep`/`source_read` leaves (Ollama) or `--add-dir` roots (claude-code), never as model-authored paths ([ADR-0021](./adr/0021-reference-sources.md)).

- **Skills as markdown + the skill book:** every ideation move is a markdown file (frontmatter: stage, role, output contract, use_when/avoid_when, hidden; body: the prompt) — built-ins compiled in from `src/concepts/skills/*.md`, owner additions/overrides in `vault/.skills/` (app config, not truth) reloaded live from the `/skills` skill book, which groups moves along the spine (steelman → attack → consequence → converge → capstone); coverage of the spine is derived from transcript headings, never stored ([ADR-0022](./adr/0022-skills-as-markdown-and-the-skill-book.md)).
- **Verification layer:** skill answers are held to an output contract (repair, then at most one retry for a single interactive call); swarms and workflows run a factored audit by default (one Auditor call, CONFIRMED/UNCERTAIN/REFUTED, refuted findings kept as "disproven objections", live toggle); store-time facts must quote the discussion verbatim or go to a quarantined-facts artifact, and UPDATE appends rather than rewrites ([ADR-0023](./adr/0023-verification-layer.md)).
- **Inbound MCP server:** idea-vault exposes itself as an MCP server at `POST /api/mcp` (`web::mcp_server`, `rmcp` `ServerHandler` over Streamable HTTP), gated by a single Bearer token (`IDEA_VAULT_MCP_TOKEN`, unset = not mounted) — the mirror image of the outbound `crate::mcp` registry above. `chat`/`store_idea` run as MCP Tasks (SEP-1686) bridged onto the existing `web::jobs` background-job machinery rather than a held-open connection ([ADR-0024](./adr/0024-mcp-server-inbound.md), [docs/13](./13-mcp-server-inbound.md)).
- **Registry leaves may use `domain`:** `crate::mcp` and `crate::sources` both validate owner-supplied names against the shared crate-wide slug alphabet (both key their entries by `domain::Name`, valid by construction iff `domain::slug::is_valid`), so both leaves depend on `domain` alone — never on `ai` or `web` — rather than duplicating the check ([ADR-0025](./adr/0025-registry-leaves-may-use-domain.md), amends ADR-0018, ADR-0021).
- **Per-role call profiles:** swarm, workflow and skill calls run under their agent role's profile — an Ollama temperature plus an optional claude model and effort, blank inheriting the global — overlaid per call on the live settings via `ai::backend::LlmBackend::for_role`. On by default, switchable off on Settings; one Ollama model for every role; free chat, compaction and extraction keep the global settings ([ADR-0026](./adr/0026-per-role-call-profiles.md), amends ADR-0011).
- **Cross-idea retrieval:** `[[idea#fact]]` resolves into `fact_links`, and reindex derives one `edges` graph (link 1.0, IDF-weighted exact tags, top-2 lexical word overlap ≤ 0.19) walked two hops by `index::queries::related_ideas`; a separately budgeted, leftover-only "Related ideas" block is pushed into chat, skill, swarm-angle and workflow-stage prompts (never audit, synthesis or extraction) and shown as a panel on the idea page; tag drift is surfaced, never merged; no vector/graph DB and no model call at reindex. Phase 2 (embeddings) was KILLED by its pre-registered criterion (margin 1 TP, needed 2) until ~30–50 ideas or a named real-use miss ([ADR-0027](./adr/0027-cross-idea-retrieval-and-the-phase-2-verdict.md)).
- **MCP tasks are optional:** `chat`/`store_idea` accept either the Task lifecycle or a plain call that waits a short, fixed budget, so Task-unaware MCP clients can use them; the Task path is unchanged ([ADR-0028](./adr/0028-optional-task-support-bounded-wait.md), amends ADR-0024).

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
