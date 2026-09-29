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
