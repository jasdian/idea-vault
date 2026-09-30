# Intent — claude-code foil lockdown and tool-output fence (P4)

The claude-code foil ran with `--dangerously-skip-permissions`, about 27 built-in tools (Bash, Write,
Edit, Task and more), the owner's user-level MCP servers (idea-vault's own among them), the vault root
as its cwd and the server's full environment, `IDEA_VAULT_MCP_TOKEN` included. A turn busy with tool
calls had no end, because only the per-line timeout bounded it. On the Ollama path, fetched pages,
source files and MCP answers reached the model raw, so text inside them could pass for instructions.
The fix: the foil runs `--restricted --tools Read,Grep,Glob` (plus WebSearch and WebFetch only while
web access is on), always with `--strict-mcp-config`, in the idea's own folder with sources as
`--add-dir`, and never skips permissions. It gets an allowlisted environment and a 1800s wall-clock
turn deadline, its init event is checked against the allowlist before any output is accepted, and every
Ollama tool result is fenced as untrusted data. Design: ADR-0039, with the owner decisions of
2026-09-30 (spec §7.2).

## Acceptance criteria

- No code path passes `--dangerously-skip-permissions`; every foil argv carries `--restricted`,
  `--tools` equal to the allowlist, and `--strict-mcp-config` with an `--mcp-config` file (an empty
  `{"mcpServers":{}}` when none is registered, removed after the turn).
- An idea turn's foil cwd is that idea's folder; extraction and compaction run there too, source-free.
- The child environment is the pass-list plus `IDEA_VAULT_CLAUDE_ENV_PASS`; no `IDEA_VAULT_*` key ever
  reaches it, even when listed.
- The init tools the app accepts are exactly the allowlist, plus `mcp__<server>__*` tools of registered
  servers; one extra tool, an unregistered MCP server, or output before the init event fails the turn.
- A turn still busy at `IDEA_VAULT_CLAUDE_TURN_TIMEOUT_SECS` (default 1800) fails with a wall-clock
  error, and its process is killed.
- Every `role: "tool"` message in the Ollama loop is fenced, with smuggled markers escaped (CR counts as
  a line break), the turn's first message carries the fence note, and no fence reaches
  `conversation.md`.
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

## Expectation changes

- `tests/fixtures/fake-claude.sh` now emits an init event listing its `--tools` in every mode, and gains
  the `dumpenv`, `leakytools`, `noinit` and `busytools` modes.
- `tests/support/mod.rs` gains `ChatScript::ToolCall`; test `ClaudeSettings` literals drop
  `skip_permissions` and gain `turn_timeout` and `env_pass`.

# Intent — grounded, ranked and bounded workflow stages; workflows as markdown; MCP workflows

A workflow could argue about code it had never looked at, merge competing designs without ranking
them, stop after one pass, and move on from findings the audit rejected. Owners could add a skill but
not a workflow, and an MCP client could not run one. The fix is four code-decided stages (Ground,
Panel, Loop, Refine) under an exact call ceiling shown before a run, workflows as markdown files in a
workflow book with a detail page, and `list_workflows` / `run_workflow` over MCP.
Design: ADR-0034 (stages, ceiling, stage artifacts), ADR-0035 (markdown, book, R49), ADR-0036 (MCP);
diagrams D35 to D38. The owner accepted the decisions on 2026-09-30: the scoped exception to D14 (stage
artifacts and a run record, all-or-nothing, never turns or evidence), only `ready-to-build` may be a
capstone, and the cost defaults (`design-panel` at most 12 calls, `exhaust` within 16, at most 32 per
workflow, the ceiling shown before running).

## Acceptance criteria

- Ground verifies anchors in code: a moved anchor is re-anchored, a missing file or symbol is
  disproved, a capped or unreadable probe is unverified and never disproved; only verified anchors are
  carried, and with no source attached Ground makes no model call and carries nothing.
- A Panel scores each proposal alone and cold, as the Auditor role, with no other proposal and no
  related block in the prompt; the winner, tie-break and grafts are the same for shuffled score lines; a
  graft naming a missing proposal or the winner is stripped; fewer than two proposals is no contest.
- A Loop stops on a dry round, a cap or a failed first round, never runs a round its call budget cannot
  fund, and a fully failed round does not extend the dry streak; Refine replaces findings by id and is
  skipped with no call when the audit is off or clean.
- Every workflow's call ceiling is exact and at most 32; one over the limit is rejected at load; the
  ceiling and waves show on the chips and the book before a run.
- Stage artifacts and the run record are written only after the final stage succeeds: a cancel or a
  failed final stage persists nothing, they are never memory evidence, and reindex stays idempotent
  with the new kinds.
- `ready-to-build` without sources builds the same prompts and makes the same calls as before Ground.
- A workflow file is validated against the skill registry it is paired with; an invalid owner file is
  a book issue and the built-in of that name stays active; a reload revalidates workflows against the
  fresh skills and a job never sees a mismatched pair; only `ready-to-build` may be a capstone.
- Over MCP, `list_workflows` shows each workflow's ceiling, `run_workflow` returns the turn and the
  stage-artifact slugs, an unknown or invalid name claims no job, a capstone points to `build_plan`,
  and an identical retry replays.
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

# Intent — plan workbench (answer → version) + MCP idempotent collect

The owner answered a build plan's open questions in chat, pressed "build again", and got an
unrelated plan that asked the same questions; over MCP, a plain-call retry after the result was
served started a second model run and wrote a second plan. The fix is a workbench on the plan page
where answers make a new linked version deterministically, and a replay of served MCP results.
Design: ADR-0032 (workbench), ADR-0033 (replay and the plan tools); diagrams D33 and D34.

## Acceptance criteria

- An answered Q# never reappears: not in the answered version, not in any later re-plan, whether
  the model renumbers it, re-asks it or drops the owner's answer.
- A version never mutates its base: answering writes a new `<stamp>-build-plan.md` with `revises`,
  and the base file's bytes are unchanged.
- An identical MCP retry creates no job, no turn and no artifact: it replays the served result.
  The same `idempotency_key` with different arguments is `invalid_params`.
- G10 does not fire on word fragments (`commonly if`) or on foil turns.
- Answering makes no model call, takes no job slot and is refused while a job runs.
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

# Intent — build-plan fixes from the first live run

"fix bugs on `main` branch, including task dependency and anything worth adjusting, based on
your report." Both build chips ran live on `platform-map` with the claude-code backend; the
report found commands that fail when copied from `plan.md`, a dropped fence, noisy kill flags and
tasks that depend on no premise.

## Acceptance criteria

- A command copied from a `plan.md` cell can be run correctly: the header says `\|` is `|`.
- A Fence item is never quarantined or moved; an unproven one stays fenced, marked unverified.
- An absolute path at or under an attached source root counts as inside that source, for paths
  and anchors alike, at that exact place only; a path climbing out with `..` never does.
- A wired kill row is not flagged for lacking a stop word.
- The template asks each task to list the `P#` it relies on, and premise wiring also matches
  record ids such as `ADR-002` (not `UTF-8`, `SHA-256` or `ISO-4217`).
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

# Intent — query-driven fact retrieval with snippets (experiment first)

"add query-driven fact retrieval with snippets alongside the current graph, not instead of it,
and skip PageRank for now … experiment that, yes." The Google pattern: the latest turn is the
query, other ideas' facts are the documents, bm25 ranks them and a snippet shows the passage.

## Acceptance criteria

- A pre-registered experiment, frozen before any turn-level run, decides whether it ships; its
  thresholds are not re-tuned after the results.
- If it passes, the section is pushed beside the ADR-0027 graph block inside the same leftover
  budget. If it fails, nothing reaches a prompt and the retriever stays an offline instrument that
  no model-facing tool and no module outside `index` may reference.
- No PageRank, no new store, no model call.
- ADR-0031 records the verdict with its numbers.
- Every commit ships through this gate: `bash scripts/gate.sh` green.

# Intent — per-role call profiles (role tuning)

A living per-gated-change file (td-bot convention): rewritten before each gated change to state,
in the owner's words, what the change must do. `scripts/gate.sh` step 1 requires it to exist and
be non-empty; the rest of the gate proves the tree still honors the ADRs.

Swarms, workflows and skills run six agent roles, but every call uses the same temperature, claude
model and effort. Harvesters and the auditor should run cold and faithful, critics hot and
diverse, the synthesizer in between; on claude-code the auditor and synthesizer may deserve a
stronger model. I want each role to carry its own call profile.

## Acceptance criteria

- Each role (critic, researcher, advocate, harvester, synthesizer, auditor) has a temperature, and
  optionally a claude model and a claude effort; blank model/effort inherit the global value.
- Role tuning is on by default with sensible defaults (harvester/auditor cold, critic/advocate
  hot), and a Settings checkbox turns it off, which restores the single global setting.
- The Settings page shows and edits the six role rows live, with no restart.
- Role profiles apply to swarm, workflow and skill calls. Free chat with the foil, compaction and
  store-time extraction keep the global settings.
- There is one Ollama model for every role; only its temperature varies.
- A per-role claude model can never push a prompt over its model's context window.
- `ai` stays role-agnostic: it never imports `concepts`.
- ADR-0026 records the decision; ADR-0011 is not rewritten.
- Every commit ships through this gate: `bash scripts/gate.sh` green.

# Intent — cross-idea retrieval (in flight alongside per-role call profiles)

When I interrogate one idea, the foil sees nothing from the other ideas in the vault. I want the
vault's own links, tags and shared vocabulary pushed into each idea's context as a small
"Related ideas" block, without a new store, a vector DB or a model call at reindex/boot. I also
want an honest experiment that decides whether embeddings are worth building at all.

## Acceptance criteria

- `[[fact]]` and `[[idea#fact]]` links resolve into a queryable fact-to-fact table in `index.db`.
- Idea-to-idea `edges` (explicit links, exact shared tags, lexical word overlap) are derived from
  `vault/**` by reindex alone; deleting `index.db` and reindexing reproduces them exactly.
- Chat, skill, swarm-angle and workflow-stage prompts carry an auto-injected "Related ideas" block
  that never includes the idea itself and only uses budget left over after the idea's own context,
  which stays byte-identical. Audit, synthesis and knowledge extraction never receive it.
- Tag near-duplicates (`system-design`/`systems-design`) are surfaced, never silently merged.
- `vault_search` exists for the offline experiment only and is never exposed to the model.
- The phase-2 embeddings verdict follows the pre-registered kill criterion and lands in ADR-0027.
- No `unsafe`; `scripts/check-invariants.sh` stays green.

# Intent — optional Tasks support for chat/store_idea (MCP inbound)

I want weaker MCP clients — ones that don't speak the Tasks primitive (SEP-1686), like Claude
Code's own MCP client — to still be able to call `chat` and `store_idea` over `/api/mcp`, instead
of being rejected outright. Task-capable clients keep using `task:{}` exactly as today.

## Acceptance criteria

- `chat` and `store_idea` are `TaskSupport::Optional`, not `Required`; a task-mode call
  (`tools/call` with `task:{}`) behaves exactly as it does today, unchanged.
- A plain (non-task) `tools/call` for `chat`/`store_idea` no longer errors with "call it with
  task:{}" — it does real work.
- No model call is ever awaited unboundedly on the request thread: a plain call waits a short,
  bounded window for the job to finish, then, if it hasn't, returns without erroring and without
  cancelling the job — the job keeps running in the background exactly like every other AI turn
  (ADR-0010), and the caller can call the tool again to pick up the result once it's ready.
- The bounded-wait branch reuses the same validate/claim/spawn logic `enqueue_task` already uses —
  no second copy of the business rules — and shares its terminal-state cache, so a task-mode
  `tasks/get`/`tasks/result` poll and a plain-call retry racing on the same idea can never both
  read `web::jobs::peek`'s one-shot terminal slot and roll the other over to a false `Idle`.
- A plain retry for the same idea while its job is still running reattaches to that same in-flight
  job instead of erroring "already busy" or spawning a second one; once that job's result has been
  served once, a later plain call for the same idea starts a fresh job rather than replaying the
  stale cached reply.
- The code keeps a clear marker of the fact that these two tools were `TaskSupport::Required`
  before this change, and why, so the constraint can be reasoned about — and reinstated — later;
  this is not a silent relaxation.
- ADR-0028 (ADR-0027 is already reserved above for the cross-idea-retrieval embeddings verdict)
  records this as a scoped, deliberate exception to ADR-0010's "never block the request thread on
  a model call," and explains how it differs from the "blocking call_tool inline" alternative
  ADR-0024 already considered and rejected.
- Every commit ships through this gate: `bash scripts/gate.sh` green.

# Module-boundary cleanup from the 2026-09-29 doc-sync (owner: "queue L1–L6 and move AppState into web")

The owner wants the module graph in `docs/02-module-reference.md` (D4) to be true again: nothing
depends on `web`, dependencies point one way, and the docs the doc-sync sweep found false are corrected.

## Acceptance criteria

- `AppState` lives in `web` (`web::state`). No file under `src/web/` imports `crate::app`, so the only
  edge between the two is `app → web`. `app` keeps building the router. Behaviour is unchanged: the
  existing tests stay green without edits other than import paths.
- `LlmBackendKind` lives in `ai`, so `ai` never imports `config`. `config` reads it from `ai`, and
  behaviour and the settings page are unchanged.
- D4 lists the real edges, including `concepts → memory` (skill hydration uses
  `memory::compact::effective_window`) and `config` as a bin-level leaf.
- `docs/10-testing-strategy.md` describes the test suite that exists. The keystone rebuild test is
  described as the fixture-based test it is (it covers `fact_links` and `edges`), not a randomized
  property test.
- `docs/13-mcp-server-inbound.md` says a plain MCP caller can cancel with the task id from its
  "still running" note.
- `docs/09-web-ui.md` says the related panel's tag-drift notes cover this idea's own tags only.
- The doc checker no longer flags external-crate paths such as `http::request::Parts`, and still
  flags a genuinely unknown path.
- Every commit ships through `bash scripts/gate.sh` green.
- Lexical edges get less noisy (owner, 2026-09-29: "Lexical edge noise - fix too"). Before any change,
  each option ADR-0027 left open is measured on the frozen eval corpus against the labels:
  - top-2 instead of top-3;
  - 3 shared words instead of 2;
  - a higher display floor for pairs related only lexically.

  The option shipped is the one with the best precision among those that keep at least the current
  recall (3 of 5 related pairs), with fewer false neighbours shown per idea as the tie-break. The
  measurement and the choice are recorded. With 9 ideas this is tuning on the evaluation data, so the
  choice is provisional.

# Intent — gated build plans (the librarian's rules, in the app)

The build-prompt buttons hand a coding agent whatever the discussion said, including its mistakes:
open questions come out as settled, gates get dropped, anchors point at the wrong lines, and invented
syntax and mixed-unit arithmetic slip through. I want both buttons to produce a build plan that a
builder can trust, or at least one that says exactly what it didn't verify.

## Acceptance criteria

- Both chips produce a build-plan artifact with Goal, Settled (each with a verbatim quote), Verify
  first (each premise with its check), Open questions, Plan (tasks with runnable accepts) and Kill
  criteria. The transcript only gets a pointer turn.
- Deterministic gates, with no model call, move ungrounded, colliding or refuted claims out of
  Settled and say why, in a Quarantined section that is never copied as buildable.
- REFUTED audit findings never reach Settled in the audited depth.
- What I said is pinned; what only the foil concluded is labelled for confirmation.
- The plan page gives me copy-ready `PROMPT.md` and `plan.md` blocks.
- A backslash-escaped quote grounds like the plain quote.
- ADR-0030 records the decision; ADR-0022 and ADR-0023 are amended, not rewritten.
