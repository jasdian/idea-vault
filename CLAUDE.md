# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status: built out — core loop implemented, not a stub scaffold

The crate matches the docs and is implemented past the original stub milestone: `cargo run` boots
the D25 sequence and serves the full route set — idea list/view, chat, skills, swarm, store/reopen,
fork, history, and a live Settings page. `domain/`, `vault`, `index` (schema + queries + reindex),
`ai` (Ollama client + claude-code client behind a live-switchable `LlmBackend` router, see
[ADR-0009](docs/adr/0009-pluggable-llm-backend-claude-code.md)/[ADR-0011](docs/adr/0011-live-switchable-llm-backend.md)),
`memory`, `concepts` (skills/agents/workflows/swarm), and `web` are all implemented, not typed
501 stubs. The "Commands" section below is real. Do not invent build/test output — run the
commands and report real results. Any remaining `TODO(<milestone>)` markers in the source are
narrow (e.g. individual skill prompt templates), not whole-module stubs — check the source before
assuming a gap.

## What this is

**idea-vault** is a localhost web tool for solo ideation. The owner brings a raw idea, then
"runs it into the ground" in conversation with an AI — pushing it through every stage of thought,
however ridiculous — and when finished, tells the AI to **store the idea in the vault** so it can
be reopened and continued later.

It is deliberately modeled on how an LLM agent harness works: the same primitives — **memory**,
**skills**, **agents**, **workflows**, and **subagent swarming** — are first-class product
concepts here, applied to interrogating one idea rather than editing a codebase.

## Confirmed architecture decisions

These were chosen explicitly by the owner. Do not silently revise them; if you believe one is
wrong, raise it rather than working around it.

- **Backend / UI:** Rust, **axum** HTTP server. Server-rendered HTML via **Askama** templates,
  enhanced with **HTMX**; No JS build step, no SPA — ships as a single binary. AI turns (chat,
  skills, swarm, extract, compact, store) run as **detached background jobs** (`web::jobs`), not SSE — the model call
  outlives the request so navigating away can't kill it; the browser polls
  `GET /idea/{slug}/pending` for a server-driven "thinking… Ns" indicator and swaps in the finished
  transcript on completion. See [ADR-0010](docs/adr/0010-ai-turns-as-background-jobs.md), which
  supersedes the earlier SSE-streaming decision ([ADR-0004](docs/adr/0004-sse-token-streaming.md)).
  New chat-adjacent routes should follow this same claim → spawn → poll pattern, not block the
  request thread on a model call.
- **Storage: hybrid.** Markdown files on disk are the **source of truth**; **SQLite is a
  rebuildable index** for search/tags/backlinks only. Never store canonical idea content only in
  SQLite — anything in the DB must be reconstructable by re-scanning the vault. A `reindex`
  path that rebuilds the DB from markdown must always exist and stay correct.
- **AI backend: local models via Ollama** (`http://localhost:11434`) by default, offline/private,
  with an optional agentic **claude-code** backend (the local, authenticated `claude` CLI — not a
  cloud API call) for owners who want the foil to read their own notes/artifacts. The two backends
  sit behind a **live-switchable router** (`ai::backend::LlmBackend`): a Settings page
  (`GET`/`POST /settings`) toggles the active backend and tunes Ollama temperature / claude-code
  model+effort — globally and per agent role ([ADR-0026](docs/adr/0026-per-role-call-profiles.md))
  — with no restart. See [ADR-0009](docs/adr/0009-pluggable-llm-backend-claude-code.md)
  and [ADR-0011](docs/adr/0011-live-switchable-llm-backend.md). The subagent-swarm and skills
  concepts run against whichever backend is active — keep prompt/context budgets modest and degrade
  gracefully when a model is slow or unavailable.
- No cloud AI provider is in scope beyond the local `claude` CLI above. Do not add a networked
  Anthropic/OpenAI API call without the owner asking.

## The ideation lifecycle (the core product loop)

An idea moves through explicit states — this state machine is the heart of the app. The canonical
names (the `IdeaState` enum, used verbatim in code and docs) are in parentheses; see
[docs/04-state-machine.md](docs/04-state-machine.md) for the full transition table:

1. **Draft** (`Draft`) — owner enters a new idea; a vault folder is created.
2. **In discussion** (`InDiscussion`) — the working loop. Owner and AI converse; the AI is expected
   to be a rigorous foil: steelman, then stress-test, from many angles. This is where "run it into
   the ground" happens and where subagent swarming is triggered.
3. **Stored** (`Stored`) — owner signals they're done ("store it in the vault"). The AI produces a
   consolidated writeup and extracts **memory** (durable facts/decisions) for the idea. The idea
   is now dormant but complete.
4. **Reopened** (`Reopened`) — a stored idea can re-enter discussion later, carrying its memory
   forward as context, exactly like an LLM resuming with prior memory loaded.

The state must be persisted in the idea's markdown frontmatter, not only in SQLite.

## Design docs (build against these)

The full design foundation lives in [`docs/`](docs/README.md): architecture (C4), the
single-crate module graph, the vault/SQLite data model, the lifecycle state machine, AI backend
integration (Ollama + claude-code), the five harness concepts (memory/skills/agents/workflows/swarm),
the web-UI routes, a Mermaid diagram catalog (D1–D42), and ADRs 0001–0042. Start at
[docs/README.md](docs/README.md). The code is built against these docs; when a doc and the code
disagree, treat it as drift to fix (in whichever direction is correct), not as license to ignore
either.

## Vault layout (source of truth)

Each idea is a folder of markdown. Proposed shape (finalize during scaffolding):

```
vault/
  <idea-slug>/
    idea.md          # frontmatter (state, created, tags) + the current best statement of the idea
    conversation.md  # append-only transcript of the discussion
    memory/          # one durable fact/decision per file, LLM-memory style
      *.md           # frontmatter + body; link related memories with [[slug]]
    MEMORY.md        # one-line index of memory/ files, loaded as context when the idea reopens
    .runs/           # diagnostics, NOT truth: one append-only <run_id>.jsonl per AI job (ADR-0037); never indexed, forked or read into a prompt, newest 50 kept
index.db             # SQLite: rebuildable search/tag/backlink index over vault/**
```

The `memory/` + `MEMORY.md` convention intentionally mirrors an LLM agent's file-based memory:
small single-fact files, a loaded index, `[[slug]]` cross-links.

## LLM-inspired concepts → how they map here

When implementing these, keep the mental model close to a real agent harness:

- **Memory** — per-idea durable facts extracted at "store" time and reloaded on reopen. Files,
  not a monolith. This is what makes an idea resumable. A fact must quote the discussion verbatim
  to be remembered; the rest are quarantined in an artifact ([ADR-0023](docs/adr/0023-verification-layer.md)).
- **Skills** — reusable, named ideation moves the AI can apply to an idea (e.g. "premortem",
  "find the cheapest disproof", "market-size it", "devil's advocate"). Each is a markdown file
  (built-ins in `src/concepts/skills/*.md`, owner overrides in `vault/.skills/`) with a spine
  stage, role, output contract and use-when guidance, browsed on the `/skills` skill book
  ([ADR-0022](docs/adr/0022-skills-as-markdown-and-the-skill-book.md)). The **make skill** button
  distils a discussion into a `skill_draft` artifact (a job, never a turn); the owner edits and saves
  it into `vault/.skills/` with no model call, never over a built-in
  ([ADR-0042](docs/adr/0042-make-skill-distil-owner-skills.md), D42, R51–R52).
- **Agents** — specialized subagent roles (critic, researcher, advocate, harvester, synthesizer,
  auditor) with scoped prompts.
- **Workflows** — deterministic staged orchestrations over an idea, as opposed to free-form chat.
  A workflow is a markdown file (built-ins in `src/concepts/workflows/*.md`, owner additions and
  overrides in `vault/.workflows/`, listed with their worst-case call ceiling on the skill book and
  R49; only `ready-to-build` may be the build-plan capstone —
  [ADR-0035](docs/adr/0035-workflows-as-markdown-and-the-workflow-book.md)) of stages: fan-out,
  chained step, audit, synthesize, and the code-decided **Ground** (verify the attached sources'
  anchors in code, skipped free with no sources), **Panel** (proposals scored alone against a
  weighted rubric, winner and grafts chosen in code), **Loop** (rounds until dry or capped) and
  **Refine** (rewrite REFUTED/UNCERTAIN findings by id, re-audit). Every workflow has an exact call
  ceiling of at most 32, shown before a run, and its Ground/Panel/Loop stages keep artifacts plus a
  run record, written only after the final stage succeeds and never as turns or evidence
  ([ADR-0034](docs/adr/0034-grounded-ranked-and-bounded-workflow-stages.md)). Swarm and workflow
  findings are audited by default (CONFIRMED/UNCERTAIN/REFUTED) before they are synthesized.
  Build plans are versioned: on the plan page the owner answers open questions and owner-held
  tasks in their own words, and each submission appends plain `## user` turns and makes a new
  linked plan version deterministically (no model call, no job slot; the base is never modified,
  and an answered question is never re-asked by a later plan) — see
  [ADR-0032](docs/adr/0032-plan-workbench-answers-and-versions.md).
  Over MCP, `build_plan`/`get_plan`/`answer_plan` expose the same workbench, and a retry of a
  served long-running MCP call replays its result instead of re-running
  ([ADR-0033](docs/adr/0033-mcp-idempotent-replay-and-plan-tools.md)). `list_workflows` and
  `run_workflow` put the workflow book on MCP; a capstone points to `build_plan`
  ([ADR-0036](docs/adr/0036-mcp-list-workflows-and-run-workflow.md)).
- **Run journal** — every AI job writes an append-only `vault/<slug>/.runs/<run_id>.jsonl` (each call's
  verbatim response, tokens, stop reason, tool rounds, contract outcome and parser verdict): diagnostics,
  not truth, never indexed, forked or read into a prompt, newest 50 runs kept, and a journal failure
  never fails a turn; R50 (`GET /idea/{slug}/runs/{run_id}`) inspects one
  ([ADR-0037](docs/adr/0037-run-journal-diagnostics-only-call-record.md), D39). It is unrelated to
  ADR-0033's MCP replay.
- **Regrade and the parser corpus** — `idea-vault regrade` re-runs today's parsers, detectors and gates
  over the journaled answers and prints one line per flip (read-only; a verdict whose haystack changed
  is skipped), and `tests/parser_corpus.rs` checks hand-exported fixtures against a committed snapshot;
  replay never covers prompt changes ([ADR-0038](docs/adr/0038-parser-corpus-and-read-only-regrade.md), D40).
- **Foil hygiene and lockdown** — the claude-code foil runs `--restricted --tools Read,Grep,Glob`
  (+ `WebSearch,WebFetch` only while web access is on), always `--strict-mcp-config`, in the idea's own
  folder, never under `--dangerously-skip-permissions`, with a checked stream-json `init` event, an env
  pass-list, and a turn deadline; every Ollama tool result is fenced as untrusted data
  ([ADR-0039](docs/adr/0039-foil-hygiene-and-lockdown.md)).
- **Recipe provenance** — every AI-written artifact carries a `recipe:` (skill or workflow digest,
  parse-coupled template refs, build id, off-contract lenses); an old artifact reads "provenance
  unknown", an edited skill shows "recipe changed since", and a malformed or partial audit gets at most
  one targeted re-ask ([ADR-0040](docs/adr/0040-recipe-provenance-and-audit-re-ask.md)).
- **No-mistakes gate** — `scripts/gate.sh` is a fixed pipeline with no skip whose invariant catalog
  collects every finding with an id and severity (each rule has a seeded test), and whose honesty step
  is red when a fixture, snapshot, floor or rule change is not declared under `## Expectation changes`;
  findings are acted on by what a fix would change (no-op, auto-fix, ask-user), and `RUN_PROTOCOL` in
  every `PROMPT.md` says the same ([ADR-0041](docs/adr/0041-no-mistakes-gate.md), D41,
  [docs/14](docs/14-no-mistakes-gate.md)).
- **Subagent swarming** — fan out N agents in parallel to attack one idea from independent angles,
  then converge/synthesize. Against local Ollama models this means bounded concurrency and careful
  context budgeting — do not naively spawn unbounded parallel calls.

## Intended commands (once scaffolded)

Standard Cargo — reflect the real state of the repo, don't fabricate:

```bash
cargo run                 # start the localhost web UI
cargo build --release     # single-binary release build
cargo test                # run tests
cargo test <name>         # run a single test by name substring
cargo fmt && cargo clippy # format + lint before finishing a change
bash scripts/gate.sh      # the fixed 7-step shipping gate, no skip: intent (+freshness) → strict invariants → build → tests → fmt → clippy -D warnings → honesty (ADR-0041)
bash scripts/gate.sh --list           # print the step table
bash scripts/gate.sh --install-hook   # install the pre-push hook (runs check-invariants.sh --strict); use alone
bash scripts/check-invariants.sh --list   # the invariant catalog: id|severity|ADR/D|zero-state
cargo run -- regrade [--idea <slug>] [--parser audit|facts|contract|plan-gates] [--strict]   # replay today's parsers over vault/*/.runs (read-only; ADR-0038)
```

Ollama must be running locally (`ollama serve`, model pulled) for AI features to work; the app
should detect its absence and surface a clear UI state rather than hanging.

### Containers (the primary way to host it)

The whole stack (app + Ollama) runs in containers, locally, with or without a GPU. The
`Dockerfile` and Compose files already exist as the deployment contract (they build once the crate
is scaffolded). Full topology, build pipeline, and pitfalls are in
[docs/12-deployment.md](docs/12-deployment.md) ([ADR-0008](docs/adr/0008-containerized-local-deployment.md)).

```bash
cp .env.example .env                                                   # set IDEA_VAULT_UID/GID to your id -u / id -g
docker compose up -d --build                                           # CPU mode (default)
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d   # GPU mode (nvidia; needs nvidia-container-toolkit)
docker compose -f docker-compose.yml -f docker-compose.claude.yml up -d --build   # claude-code backend in-container (ADR-0013; needs a one-time host `claude setup-token` → CLAUDE_CODE_OAUTH_TOKEN in .env)
docker compose --profile tools run --rm ollama-pull                    # first run: pull the model
# open http://localhost:3000
docker compose down
```

**Config is env-driven, not hardcoded.** `config.rs` reads `IDEA_VAULT_BIND` (default
`127.0.0.1:3000`; `0.0.0.0:3000` in-container), `IDEA_VAULT_VAULT_DIR`, `IDEA_VAULT_INDEX_PATH`,
`IDEA_VAULT_OLLAMA_URL` (default `http://localhost:11434`; `http://ollama:11434` in-compose),
`IDEA_VAULT_OLLAMA_MODEL`, `IDEA_VAULT_AI_CONCURRENCY` (default `2`, the shared Ollama-call bound K,
ADR-0006), `IDEA_VAULT_OLLAMA_TIMEOUT_SECS` (default `120`, the hard inactivity timeout),
`IDEA_VAULT_OLLAMA_TEMPERATURE` (default `0.7`, range `0.0..=2.0`), `IDEA_VAULT_CLAUDE_EFFORT`
(default `high`; injected as a system-prompt hint, since the claude CLI has no per-call effort
flag), and the dynamic-context-budget overrides `IDEA_VAULT_OLLAMA_CTX_TOKENS` /
`IDEA_VAULT_CLAUDE_CTX_TOKENS` (default `0` = auto-derive from the model, else clamped
`1024..=2_000_000` tokens; [ADR-0014](docs/adr/0014-dynamic-context-budget.md)),
`IDEA_VAULT_CLAUDE_TURN_TIMEOUT_SECS` (default `1800`, the wall-clock ceiling on one whole claude-code
turn) and `IDEA_VAULT_CLAUDE_ENV_PASS` (comma-separated extra env names the claude child may inherit,
on top of a fixed pass-list; `IDEA_VAULT_*` keys are never passed;
[ADR-0039](docs/adr/0039-foil-hygiene-and-lockdown.md)), and
`IDEA_VAULT_WORKFLOWS_DIR` (default `<vault>/.workflows`, owner workflow files, app config not vault
truth; [ADR-0035](docs/adr/0035-workflows-as-markdown-and-the-workflow-book.md)). All of the above
are only the **initial** values — the live Settings page (`GET`/`POST /settings`,
[ADR-0011](docs/adr/0011-live-switchable-llm-backend.md)) can retune
backend/temperature/model/effort/context-window at runtime with no restart.
`IDEA_VAULT_BUILD_SHA` is different: a **build-time** value (a Dockerfile `ARG`, read by `option_env!`;
unset under `cargo run`) that stamps artifact recipes as `<version>+<sha>`
([ADR-0040](docs/adr/0040-recipe-provenance-and-audit-re-ask.md)).
**Never hardcode `localhost:11434` or a localhost bind** — it breaks the
containerized run. `vault/` is a host bind mount (truth you own); the SQLite index and Ollama models
are named volumes (rebuildable / re-pullable). GPU touches only the Ollama service. The **claude-code
container override** (`docker-compose.claude.yml`, [ADR-0013](docs/adr/0013-containerized-claude-code.md))
bind-mounts the host's own `claude` CLI read-only and persists its state (`.claude/`, `.claude.json`)
on a `claude-state` named volume via `HOME=/claude`; it needs `IDEA_VAULT_CLAUDE_HOST_BIN` (host CLI
path, default `~/.local/bin/claude`) and `CLAUDE_CODE_OAUTH_TOKEN` (from a one-time host
`claude setup-token`) in `.env` — see [docs/12-deployment.md](docs/12-deployment.md) for the
run commands and pitfalls (rebuild-before-first-volume-creation, restart-for-CLI-update,
probe-green-but-auth-broken).

**Reference sources** ([ADR-0021](docs/adr/0021-reference-sources.md)): register named read-only
source dirs on `/sources`; the app regenerates `vault/.docker-compose.sources.yml` (ro binds at
`/mnt/sources/<name>`) but **never self-ups** — add the one-time
`COMPOSE_FILE=docker-compose.yml:vault/.docker-compose.sources.yml` line to `.env` (sources entry
last when combined with other overrides) and apply each registry change yourself with
`docker compose up -d`. Sources are attached per-idea via frontmatter `sources: [name]` and reach
the model as deterministic `source_*` tools (Ollama) / `--add-dir` (claude-code) — never as
model-authored paths.

## Conventions

- Markdown is the durable format everywhere the owner reads content; render it in the UI.
- Frontmatter carries structured state (idea state, tags, timestamps) so the vault is
  self-describing and the SQLite index is always regenerable from disk.
- Run anything AI-generated as a detached background job with a visible, server-driven "thinking"
  indicator ([ADR-0010](docs/adr/0010-ai-turns-as-background-jobs.md)) so long local-model
  responses feel live without tying a model call to the request that started it.
- Keep it a single self-contained binary + a `vault/` directory the owner can read, back up,
  and version with git independently of the app.
