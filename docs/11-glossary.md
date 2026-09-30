# 11 — Glossary

> Canonical vocabulary for idea-vault. Every other doc, and eventually the code, uses these terms
> with exactly these meanings. Where a term maps to a code identifier, the identifier is given
> verbatim so docs and code never drift.

## Core domain

- **Idea** — the unit of work. One idea = one folder in the vault. Modeled in code as
  `domain::idea::Idea`. Has exactly one **state** at any time.
- **Vault** — the on-disk directory (`vault/`) containing all ideas. The **source of truth** for
  everything the user reads. Never derived; always authoritative.
- **Slug** — the URL- and filesystem-safe identifier for an idea, derived from its title
  (`domain::slug`). Unique within the vault; collisions are suffix-disambiguated. Also the target of
  `[[slug]]` links. Example: `distributed-idea-market`.
- **Idea state** — one of exactly four values, represented by the `domain::idea::IdeaState` enum.
  The names are canonical and must match verbatim in docs and code:
  - **`Draft`** — just created; not yet interrogated.
  - **`InDiscussion`** — the active interrogation loop.
  - **`Stored`** — the user has finished; memory has been extracted; the idea is dormant but complete.
  - **`Reopened`** — a previously `Stored` idea brought back into discussion with its memory reloaded.
  - (Written in frontmatter in lower-kebab as `state: in_discussion`, etc. — see [03-data-model](./03-data-model.md).)

## Storage artifacts

- **`idea.md`** — per-idea file: YAML **frontmatter** (state, slug, timestamps, tags) plus the body,
  which holds the *current best statement* of the idea. Rewritten on Store.
- **`conversation.md`** — per-idea **append-only** transcript of the discussion (user and assistant
  turns). Never rewritten, only appended.
- **Memory fact** — one durable, distilled conclusion about an idea, stored as a single file in the
  idea's `memory/` directory. Modeled as `domain::memory::MemoryFact`. One fact per file.
- **`MEMORY.md`** — per-idea one-line index of the files in `memory/`, loaded as context when the
  idea is reopened. Mirrors the agent-harness memory-index convention.
- **Frontmatter** — the YAML block at the top of `idea.md` (and of each memory fact file) carrying
  structured, indexable fields. Parsed by `domain::frontmatter`.
- **`[[slug]]` link / backlink** — a cross-reference from one idea or memory fact to another idea by
  slug. Resolved on reindex into the `backlinks` index table. See [D23](./06-concepts/memory.md).
- **Artifact** — a persisted knowledge-extraction output stored under `vault/<slug>/artifacts/`.
  Modeled as `domain::artifact::Artifact` (frontmatter + body). `.md` is **truth**, indexed into
  `search_fts` as kind `'artifact'`; `.html` is a **derived, unindexed export** — a standalone,
  self-contained report the owner can open or share, not a knowledge source. See
  [ADR-0015](./adr/0015-knowledge-extraction-artifacts.md).

## Index

- **Index** — the **SQLite** database (`index.db`) holding search, tags, and backlink tables. It is
  **derived and rebuildable** — never a source of truth.
- **Reindex** — the operation that rebuilds the entire index by walking `vault/**` and re-parsing
  markdown (`index::reindex`). The **reindex invariant**: the index must always be fully
  reconstructable from the vault alone. See [ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md).
- **FTS5** — SQLite's full-text search extension, used for search over idea bodies and conversations.

## AI

- **Ollama** — the local LLM server (default `http://localhost:11434`) idea-vault talks to by
  default. See [ADR-0003](./adr/0003-ollama-local-only-ai.md).
- **claude-code backend** — the second, agentic LLM backend: idea-vault spawns the owner's local,
  authenticated `claude` CLI as a one-shot process per turn. Not a cloud API call. See
  [ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md).
- **`LlmBackend`** (`ai::backend::LlmBackend`) — the **live router** holding both backends plus
  runtime-tunable `LlmSettings`; every AI call re-reads the current settings to pick the active
  backend and its tuned parameters. Switchable via the Settings page (`GET`/`POST /settings`) with
  no restart. See [ADR-0011](./adr/0011-live-switchable-llm-backend.md).
- **Background job** (`web::jobs`) — the detached async task an AI-driven route (chat/skill/swarm)
  spawns to run the model call; one job per idea. The browser polls
  `GET /idea/:slug/pending` for a server-driven "thinking… Ns" indicator until it resolves. Replaced
  SSE token streaming. See [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md).
- **SSE (Server-Sent Events)** — the one-way streaming channel originally used to push AI tokens to
  the browser token-by-token. **Superseded** by the background-job + poll model above; see
  [ADR-0004](./adr/0004-sse-token-streaming.md) (superseded) and
  [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md) (current).
- **Context budget** — the bounded amount of text (idea body + selected memory + recent
  conversation) assembled into a prompt, kept within the model's limits by `ai::budget`.
- **Degradation** — the defined behavior when the active LLM backend is slow or absent: the UI
  surfaces a clear state and never hangs. See [D20](./05-ai-integration.md).

## Harness primitives (the first-class concepts)

- **Memory (concept)** — the feature of extracting facts on Store and reloading them on Reopen.
  Doc: [06-concepts/memory](./06-concepts/memory.md).
- **Skill** — a named, reusable ideation move (a parameterized prompt template) applied to an idea.
  Skills are markdown files (`domain::frontmatter::parse_skill`), not code: built-ins ship compiled
  into the binary (`concepts::skills::BUILTIN`, `include_str!`), and the owner may add or override
  any of them under `vault/.skills/` (**owner skills**). See
  [ADR-0022](./adr/0022-skills-as-markdown-and-the-skill-book.md) and
  [06-concepts/skills](./06-concepts/skills.md).
- **Spine / stage** (the ideation spine) — the order an idea is best run through: `steelman` →
  `attack` → `consequence` → `converge` → `capstone`, plus the off-spine `extract` stage
  (orchestrator-only knowledge-harvest lenses, never an offered move). Each skill declares its
  `stage` (`domain::skill::SkillStage`); `concepts::coverage::coverage` derives which stages a
  discussion has actually covered from its turn headings, purely from `conversation.md`, and
  suggests the next move.
- **Skill book** — the `GET /skills` page: every registered skill grouped by spine stage, with its
  `use_when`/`avoid_when` guidance, role, output contract, and source (`built-in` / `vault
  override` / `vault`), plus any owner file that failed to load; `POST /skills/reload` refreshes it
  live. See [09-web-ui](./09-web-ui.md).
- **Output contract** — the shape a skill's output must take (`domain::skill::OutputContract`:
  `Free`, `BulletsOrEmpty`, `RankedList`, `FencedMarkdown`, `BuildPlan`, and the workflow engine's
  `GroundClaims`, `Proposal`, `Scorecard`), validated and, for a single
  interactive call, repaired by `ai::contract`. See
  [ADR-0023](./adr/0023-verification-layer.md).
- **Factored audit** — the verification pass a swarm's or workflow's converge step runs over every
  candidate finding before synthesis (`concepts::audit`): each finding is labeled `CONFIRMED`,
  `UNCERTAIN`, or `REFUTED` against the idea, its memory, and the discussion, preferring
  `UNCERTAIN` over `CONFIRMED` when in doubt. Refuted findings are kept, struck through, under
  **Disproven objections**; a **uniform pass** (over 90% of at least 4 findings confirmed) is flagged
  as a warning sign; a garbled or failed audit leaves every finding `UNCERTAIN` and marks the run
  "unverified" rather than aborting it. Toggled by `IDEA_VAULT_AUDIT_FINDINGS` / the Settings page's
  audit checkbox. See [ADR-0023](./adr/0023-verification-layer.md).
- **Quarantined facts** (evidence gate) — durable facts memory extraction holds back rather than
  writing to `memory/`, because they lack a `QUOTE` from the discussion supporting them; written
  instead to an `artifacts/<stamp>-quarantined-facts.md` file the owner can read and promote by
  hand (`memory::extract`).
- **Agent** — a scoped subagent role (e.g. critic, researcher, synthesizer) with a specific prompt
  and I/O contract. `domain::skill::SkillRole` — `Critic`, `Researcher`, **Advocate**,
  **Harvester**, `Synthesizer` — maps 1:1 onto `concepts::agents::AgentRole` for skills fanned out
  by an orchestrator (swarm, workflow, extraction); a direct interactive skill run ignores it.
  `AgentRole` adds the **Auditor**, which only the factored audit and the Panel stage's scorers use. Doc:
  [06-concepts/agents](./06-concepts/agents.md).
- **Workflow** — a deterministic, staged orchestration over an idea, defined as a markdown file
  (fan-out / chained step / audit / synthesize, plus Ground / Panel / Loop / Refine; a chained
  step's output is carried forward). Contrast with free-form chat. Doc:
  [06-concepts/workflows](./06-concepts/workflows.md).
- **Workflow book** — the workflow half of the `GET /skills` page (each workflow's stages, worst-case
  call ceiling and source, plus any owner file that failed validation) and its per-workflow detail
  page `GET /skills/workflow/{name}` (R49). Built-ins are compiled in from
  `src/concepts/workflows/*.md`; owner files live in `vault/.workflows/`
  (`IDEA_VAULT_WORKFLOWS_DIR`), app config, not vault truth. The registry and the skill registry are
  held as one `Book` pair so a job never sees them disagree
  ([ADR-0035](./adr/0035-workflows-as-markdown-and-the-workflow-book.md), D38).
- **Workflow stage** (not a spine stage) — one step of a workflow; one of eight kinds (`domain::workflow::StageKind`): `fan_out`,
  `chain`, `audit`, `synthesize`, **Ground**, **Panel**, **Loop**, **Refine**.
- **Ground** — the stage that maps an idea's attached reference sources and verifies every anchor
  its readers cite in code (verified / moved / disproved / unverified), carrying only verified
  anchors; skipped with no model call when no source is attached. Verifies existence, not meaning.
  Emits a `ground_map` artifact ([ADR-0034](./adr/0034-grounded-ranked-and-bounded-workflow-stages.md), D35).
- **Panel** — the stage where 2–4 proposers compete: each proposal is scored alone, cold, by the
  Auditor role running the hidden `panel-score` skill against a weighted rubric; code picks the winner
  and the grafts. Distinct from `swarm::judge`, the deterministic dedupe. Emits a `scorecard`
  artifact (D36).
- **Loop / Refine** — bounded repetition stages: a Loop reruns its steps until a round finds nothing
  new (**dry**) or a cap is hit; a Refine rewrites the audit's REFUTED and UNCERTAIN findings by id and
  re-audits (D37).
- **Call ceiling** — a workflow's exact worst-case number of model calls, repair retries included
  (`Workflow::call_ceiling`), at most `WORKFLOW_MAX_CALLS` (32) and shown before a run; a **wave** is
  `⌈widest stage / K⌉` batches at the shared concurrency bound K The run is charged by billed requests, not steps
  ([ADR-0037](./adr/0037-run-journal-diagnostics-only-call-record.md)).
- **Capstone** — a workflow that chains a build-plan skill; derived, and allowed only under the name
  `ready-to-build` (owners fork it by overriding that name).
- **Stage artifact / run record** — an `artifacts/*.md` file a Ground, Panel or Loop stage writes
  (`ground_map`, `scorecard`, a `finding` with lens `loop`) and the `workflow_run` record listing every
  stage's status and calls; written all-or-nothing after the final stage succeeds, never turns and
  never memory evidence.
- **Swarm / swarming** — running many agents concurrently against one idea, under **bounded
  concurrency**, then converging their outputs. Doc: [06-concepts/swarm](./06-concepts/swarm.md).
- **Bounded concurrency** — the hard cap (a semaphore) on how many AI calls (to whichever backend is
  active) run at once during a swarm, protecting a single local machine. See
  [ADR-0006](./adr/0006-bounded-concurrency-swarm.md).
- **Converge / synthesize** — the final step of a swarm/workflow where multiple agent outputs are
  judged and merged into one result. The `converge` skill is the single-turn version: it judges the
  findings already in the transcript and commits to one verdict.

## Observability, provenance and the gate

- **Run journal** — the append-only `vault/<slug>/.runs/<run_id>.jsonl` one AI job writes: `RunStarted`,
  each `LlmCall` (verbatim response and its call meta), `ToolCall`, `Contract`, `Verdict`, `RunFinished`
  (`journal::JournalEntry`). Diagnostics, **not truth**: never indexed, never read into a prompt, never
  forked, newest 50 runs per idea kept. Unrelated to MCP replay (D34) and never called "replay"
  ([ADR-0037](./adr/0037-run-journal-diagnostics-only-call-record.md), D39).
- **Run inspector** — `GET /idea/{slug}/runs/{run_id}` (R50), the read-only page over one run journal.
- **Call meta** (`ai::call::CallMeta`) — what one model call cost and how it stopped: prompt and output
  tokens, `api_calls`, stop reason, `num_ctx`, milliseconds. **Output truncated** is
  `stop_reason == "length"`; **input truncated** is a prompt at or over 98% of `num_ctx`, judged
  for a tool loop by its largest single round (`peak_prompt_tokens`), never by the summed usage.
- **Contract outcome** (`ai::contract::ContractOutcome`) — `Clean`, `Repaired`, `Retried` or
  `OffContract(violation)`: how an answer met its output contract.
- **Verdict line** — a parser's canonical one-line judgement of a model answer (`pass=<n> key=value …`),
  written by `regrade::summarize`, the single definition; journaled and replayed
  ([ADR-0038](./adr/0038-parser-corpus-and-read-only-regrade.md)).
- **Regrade** — `idea-vault regrade`, the read-only replay of today's parsers over the journaled answers,
  printing one line per **flip**; **haystack** is the idea body and conversation prefix a grounding
  verdict was checked against, and a verdict whose haystack changed is skipped. Replay covers parse,
  detector and gate code, never prompts (D40).
- **Parser corpus** — the hand-curated `tests/fixtures/raw-outputs/<parser>/<case>.md` outputs plus
  `parser-corpus.snap`, filled only by an explicit `regrade --export`.
- **Recipe** (`domain::Recipe`) — the optional frontmatter block on an AI-written artifact naming its
  skill or workflow, digest, prompt templates, build id and off-contract lenses. **Digest** is 12 hex
  digits of a file's SHA-256. An artifact without one is "provenance unknown", never "stale"
  ([ADR-0040](./adr/0040-recipe-provenance-and-audit-re-ask.md)).
- **Audit re-ask** — the single extra Auditor call, naming only the findings still without a verdict,
  made when an audit answer is malformed or partial; merged first verdict wins.
- **Foil lockdown** — the claude-code foil's fixed launch: `--restricted --tools Read,Grep,Glob` (plus
  web tools while web access is on), `--strict-mcp-config`, the idea's folder as cwd, no
  `--dangerously-skip-permissions`, a pass-list environment that never includes `IDEA_VAULT_*`, a
  checked `init` event and an 1800 s turn deadline. **Fence** (`ai::untrusted::fence_untrusted`) wraps
  every Ollama tool result as untrusted data ([ADR-0039](./adr/0039-foil-hygiene-and-lockdown.md)).
- **No-mistakes gate** — `scripts/gate.sh`, the fixed seven-step shipping gate, with the invariant
  **catalog** of `scripts/check-invariants.sh` (each rule an id and a severity, each with a seeded test),
  the **honesty** step and the **findings protocol** (**no-op**, **auto-fix**, **ask-user**), shared by
  the developer and every build plan's `RUN_PROTOCOL` ([ADR-0041](./adr/0041-no-mistakes-gate.md), D41,
  [14-no-mistakes-gate](./14-no-mistakes-gate.md)).

## System / code

- **Single crate** — idea-vault ships as one binary Cargo crate with strict internal modules; not a
  workspace (yet). See [ADR-0005](./adr/0005-single-crate-vs-workspace.md) and [02-module-reference](./02-module-reference.md).
- **AppState** — the shared, cloneable application state (`web::state`: config, db, llm, ai_semaphore, skills,
  jobs, queues, mcp, sources) held by axum handlers.
- **Askama** — the compile-time HTML templating library used for server-rendered pages.
- **HTMX** — the client-side library that turns HTML attributes into AJAX/polling interactions,
  avoiding a JS SPA.
- **Partial** — an HTML fragment (not a full page) returned to HTMX to swap into the DOM.

## Diagram vocabulary

- **Diagram ID (Dn)** — every diagram in the docs has a stable ID in `D1`…`D41`, catalogued in
  [08-diagrams](./08-diagrams.md). References elsewhere use the ID.
- **Home doc** — the single document a diagram is authored in; the registry only links to it.
