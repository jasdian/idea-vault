# Intent archive

Older intent blocks, newest first. They moved out of `docs/INTENT.md` so that file carries only the
intent of the branch in flight, which gate step 1 checks for structure and freshness (ADR-0041,
owner decision 2026-09-30). Kept verbatim as the record of what each change set out to do.

# Intent — Claude CI hotfix: a failed CI run on main becomes an issue and a hotfix PR (ADR-0041)

A red CI run on main, such as a new stable clippy lint (run 36718191131), stays red until the owner
notices it. The new workflow `.github/workflows/claude-ci-hotfix.yml` runs on a failed `CI` run from
a push to main, or on a dispatch with a run id. The workflow files a `ci-failure` issue. Claude,
holding no write access, diagnoses the failure and, if main still fails, fixes the root cause. A
deterministic job then guards the diff and opens a `Fixes #N` PR on `hotfix/ci-<run_id>`. It works
under the no-mistakes guardrails (ADR-0041, docs/14-no-mistakes-gate.md). `claude-review.yml` also
reviews those hotfix PRs. The owner approved this design, and it is documented in
docs/10-testing-strategy.md.

## Acceptance criteria

- The workflow starts only for a failed `CI` run whose event is `push` on `main` in this repository.
  A dispatch is held to the same check. A pull-request CI run never starts it, so a failing hotfix
  PR cannot loop.
- There is one issue per failing sha, found by a marker the workflow writes, in a serialised triage
  job; one hotfix is in flight at a time; at most 3 hotfix issues are filed per 24 hours; and a
  skipped run names the reason in its summary.
- When main's CI is already green at a later commit, the run records the failure in an issue,
  closes it, and pushes no branch.
- Claude runs with a read-only token, no git or gh tools, unpersisted checkout credentials and a
  restore-only cache, on current main with CI's toolchain and four commands. It gives a diagnosis
  only after 3 failed attempts. A deterministic publish job, which has the write token, refuses
  output that contains a secret and runs the guard before pushing. The guard withholds lint
  suppressions, `#[ignore]`, removed tests, `Cargo.toml` lint changes, symlinks and edits to
  `.github/`, `scripts/`, `CLAUDE.md` or build config. It opens a fix that touches fixtures,
  snapshots, floors or Cargo files as a draft. Only `hotfix/ci-<run_id>` is ever pushed, never
  forced, and nothing merges.
- With `CI_HOTFIX_TOKEN` set, the PR starts CI. Without it, the PR and the issue say CI must be
  started by hand, and a PR that GitHub refuses is reported on the issue with the setting to
  enable.
- `claude-review.yml` reviews `hotfix/ci-*` PRs authored by the one hotfix bot (`CI_HOTFIX_BOT`
  or github-actions), with advisory framing and `allowed_bots` set only for them, as well as the
  owner's PRs; fork PRs stay excluded. Every action in both Claude workflows is pinned to a
  commit SHA.
- `actionlint` (with shellcheck) passes on `.github/workflows/`, and `bash scripts/gate.sh` is
  green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.

# Intent — plan owner answers: never demoted by G6, G10 settled by its answer (ADR-0030, ADR-0032)

Two defects seen live on a plan lineage. An owner's workbench answer holding a figure ("2.1.285",
"24h") was moved from Settled to Verify first by G6, whose figure and recount checks exist to catch
model-invented numbers; the answer then fell out of `answered_in_lineage`, which read only Settled.
And G10 re-opened its "gate language without a kill row" question with the identical text the
moment the owner answered it, because the answer removed the only thing that suppressed it. Design:
ADR-0030 amendment (owner-answer exemption, G6 glued units, G10 settled-by-window and quoted-mention rules) and
ADR-0032 amendment (lineage reads every section; the workbench restores a demoted answer).

## Acceptance criteria

- A Settled item that is an owner answer (`answers` or `unblocks`, Owner provenance, text equal to
  its quote or a prefix of it ending in `…`) is never moved by a claim gate: G6 skips it, and a G4
  anchor fault or G12 freshness cue is a marker on the Settled item, which keeps its `S#` on later
  versions. A foil-grounded item, an owner quote without an answer key, a model paraphrase and a
  bare prefix carrying the answer keys keep every gate.
- G6 finds a figure the discussion states with a glued unit: a claim's `24h` or `24 hours` is not
  `figure not in the discussion` when the discussion says `24h`.
- `answered_in_lineage` reads `answers` items from Settled, Verify first, Open and Quarantined, so
  an answer a gate moved is still carried and never re-asked.
- A workbench answer moves every owner answer an older version left in Verify first back to
  Settled, unmarked, before the gates run.
- An owner answer to a G10 question settles the window that question quoted (wherever it occurs,
  including a later paste) and the answer turn itself; gate language in any other owner turn,
  before or after the answer, still opens a question. The answer text also occurring in an earlier
  turn changes nothing.
- G10 ignores a gate phrase wrapped in a matching pair of quote marks (`"only if"`, `'only if'`,
  curly quotes normalized); a quoted sentence containing gate language still fires.
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

## Expectation changes

None: no fixture, snapshot, floor or existing test expectation changes.

# Intent — drt transplants: run journal, regrade corpus, provenance, foil hardening + lockdown, no-mistakes gate (ADR-0037..0041)

Five practices from the DRT harness, carried into idea-vault. A model call leaves no record beyond
the turn it wrote, so a contract fallback, a truncation or a changed parser cannot be seen or
replayed. Artifacts do not say which skill, workflow or build made them. The claude-code foil
inherits the server's environment and can run for ever. And the shipping gate stops at the first
failing grep, has no seeded tests, and never asks whether an expectation was weakened to go green.
Design: ADR-0037 (run journal, D39, R50), ADR-0038 (parser corpus and regrade, D40), ADR-0039 (foil
hygiene and lockdown), ADR-0040 (recipe provenance, ADR-0023 amendment), ADR-0041 (no-mistakes gate,
D41, ADR-0030 amendment). Owner decisions of 2026-09-30 are binding: journal at
`vault/<slug>/.runs/`, newest 50 runs kept; the foil lockdown is part of ADR-0039; the intent
freshness gate; corpus export only by explicit `regrade --export`; audit re-ask only on a malformed
audit; `depends` is ask-user and `reads` a no-op; the pre-push hook runs `--strict`; undeclared
expectation changes are red.

## Acceptance criteria

- Every AI job writes an append-only `vault/<slug>/.runs/<run_id>.jsonl` (RunStarted, LlmCall,
  ToolCall, Contract, RunFinished; a dropped job ends Cancelled or Panicked); the journal is never
  indexed, never read into a prompt, never forked, keeps the newest 50 runs, and a journal failure
  never fails the turn. R50 shows each call's role, backend, contract outcome, tokens and stop reason.
- A contract fallback, an output truncation and an input truncation are recorded as a
  `ContractOutcome`, not only logged; an output truncation gets the single re-ask, an input
  truncation does not.
- `idea-vault regrade` re-runs today's parsers over journaled raw outputs, prints one line per flip,
  skips a verdict whose haystack changed, never writes to the vault, and exits 1 on a flip only with
  `--strict`; `tests/parser_corpus.rs` checks the committed fixtures against a snapshot.
- Every AI-written artifact carries a `recipe:` (skill or workflow digest, template refs, build); an
  artifact without one shows "provenance unknown", a changed skill shows "recipe changed since"; a
  malformed or partial audit gets at most one targeted re-ask, charged to the call budget.
- The claude child sees only the env pass-list (never `IDEA_VAULT_*`), runs with the Read/Grep/Glob
  tool allowlist, `--strict-mcp-config` and no `--dangerously-skip-permissions`, and ends at the
  1800s turn deadline; every Ollama tool result reaches the model inside the untrusted fence.
- `scripts/check-invariants.sh` collects every finding with an id and a severity, `--strict`
  promotes warnings, `--list` prints the catalog, and every catalog rule has a seeded violation in
  `tests/gate_invariants.rs` that flags exactly that rule.
- `scripts/gate.sh` runs a fixed seven-step pipeline with no skip: intent structure and freshness,
  strict invariants, build, tests (refused while `PARSER_CORPUS_BLESS` is set), fmt, clippy, and an
  honesty step that is red when a fixture, snapshot, rising floor or removed or downgraded rule is
  not declared under `## Expectation changes`; `--install-hook` installs a marked pre-push hook
  idempotently and refuses a foreign one.
- Every `PROMPT.md` tells its executor to act on a failure by what fixing it would change (no-op,
  auto-fix, ask-user), never to weaken a check, and to re-run the full gate after any fix; every
  build-plan field key is classified, and plan.md's `Rules:` header names the Findings rule.
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

## Expectation changes

- plan::tests::prompt_protocol_is_code_owned_and_precedes_the_items: RUN_PROTOCOL rules 9-12 are
  rewritten as the findings protocol (ADR-0041); the "Stop after 3 failed attempts" substring is
  replaced by the new rule phrases, and the byte-identity assertion stays.
- plan::tests (plan.md header, two tests) and web_build_plan::prompt_md_carries_findings_protocol_end_to_end: the `Rules:` header gains `Findings` (owner decision D-c),
  worded `How to run this and its rule 10 Findings` because the findings protocol is rule 10 of
  `## How to run this`, not a PROMPT.md section; a new test holds every header name to PROMPT.md.
- check-invariants catalog: the nine existing rules gain ids (`ollama-url` … `d4-web-app`) and the
  script collects all findings; no rule is removed or downgraded.
- IGNORE_FLOOR: a new ratchet floor at 0, not a rise.
- tests/fixtures/fake-claude.sh: emits an init event listing its `--tools` in every mode and gains
  the `dumpenv`, `leakytools`, `noinit` and `busytools` modes (ADR-0039 init check and env scrub).
- tests/support/mod.rs: gains `ChatScript::ToolCall`; test `ClaudeSettings` literals drop
  `skip_permissions` and gain `turn_timeout` and `env_pass` (ADR-0039).
- check-invariants catalog: two new error rules, `tool-fence` (every `"role": "tool"` message in
  src/ai goes through `fence_untrusted`) and `no-skip-permissions` (no
  `--dangerously-skip-permissions` in src/), each with a seed in `tests/gate_invariants.rs`
  (ADR-0039); nothing removed or downgraded.
- check-invariants catalog: one new error rule, `runs-not-truth` (no run-journal path in
  src/index, src/memory, src/concepts or src/ai/budget.rs), with its seed in
  `tests/gate_invariants.rs` (ADR-0037); nothing removed or downgraded.
- tests/support/mod.rs: gains `ChatScript::Finished`, whose terminal line carries `done_reason`
  and the eval counts (ADR-0037); existing scripts answer byte-for-byte as before.
- concepts::workflows::rounds::tests::refine_skips_with_zero_calls_when_clean: the call budget is
  charged by the backend's request meter instead of by `charge` calls (ADR-0037), so the test's
  dead backend carries the budget's meter; the zero-calls assertion stays.
- tests/fixtures/raw-outputs/: new parser corpus (ADR-0038), ten hand-seeded cases (a clean, a
  garbled and a partial audit; an escaped-quote and an invented-quote store extraction; three
  contract answers; a gated and an unusable build plan). Later cases come only from an explicit
  `idea-vault regrade --export`.
- tests/fixtures/parser-corpus.snap: new snapshot of today's verdict line per corpus case
  (ADR-0038), written once with `PARSER_CORPUS_BLESS=1 cargo test --test parser_corpus`.
- claude_code::tests (two args tests) and tests/claude_backend.rs: `--disallowedTools` now always
  carries `Read(./.runs/**)` ahead of the web pair, and is present with web access on too, so the
  foil never reads the run journal inside its cwd (ADR-0037, ADR-0039 review finding).
- skills::tests::digest_is_of_raw_file_not_filled_prompt: `Skill::recipe` takes the recorded
  `ContractOutcome` instead of re-validating the kept text (ADR-0040 review finding); the same notes
  are asserted, plus a truncated outcome.
- tests/gate_invariants.rs: `SEEDS` rows gain a detection-arm name and seven arm seeds;
  `committed_tree_is_clean` runs against the versioned tree copied out of git, so gitignored
  `.claude/` state never decides it (ADR-0041 review findings).
- checklist-mirror: an absent `.claude/` mirror file is INFO, as an absent `.claude/` already was;
  a present, drifted mirror stays an error, and the catalog severity is unchanged.
- gate.sh step 7: runs on main too, diffing against `HEAD`; only step 1 freshness is skipped there.
- ai::call::CallMeta: gains `peak_prompt_tokens` (skipped when absent, so single-call journal lines
  are unchanged); input truncation reads it for a tool loop instead of the summed prompt tokens.
- journal_flow::skill_job_writes_started_llmcall_contract_finished: the journaled sequence gains the
  `verdict` entry P2 writes beside each contract-checked call (ADR-0038); every other assertion
  stays.
- tests/fixtures/prompt-goldens/: new goldens for the parse-coupled prompts (ADR-0040): the audit
  prompt for a fixed input, its targeted re-ask suffix, the contract retry note, the fact-extraction
  instruction, and `build_prompt` over a fixed test skill; each written by hand from the source.
- swarm call counts (tests/web_concepts.rs `run_swarm_defaults_…` 6→7 and `run_swarm_custom_angles…`
  3→4; tests/swarm_flow.rs `keystone_…` 8→9 and `related_block_reaches_every_angle…` angles+2→+3
  with two auditor bodies): the mock's non-verdict audit answer now earns the one targeted re-ask
  (owner decision 2026-09-30 §7.4, ADR-0023 amendment); a swarm has no call budget to refuse it.
- tests/swarm_flow.rs scripts (`a_garbled_audit_…`, `audit_cap_swarm_turn_…`) and the
  tests/workflow_flow.rs auditor scripts (`auditor_scripts`): a malformed or partial scripted audit
  is followed by the same reply for the re-ask, so every existing assertion keeps its meaning; the
  garbled-audit test reads the synthesizer at body 3 instead of 2.
- test struct literals (tests/build_plan_flow.rs, tests/web_build_plan.rs, tests/plan_workbench.rs
  and the src unit fixtures): `ArtifactFrontmatter` and `PlanInputs` gain `recipe: None`
  (ADR-0040); no assertion changes.
- regrade_flow::swarm_journals_lens_contracts_and_the_audit_verdict: audit verdicts 1→2 and
  regraded-same 5→6 once P2 and P3 meet: the mock's non-verdict audit answer earns P3's one
  targeted re-ask, and every audit call journals its own verdict line (ADR-0038, ADR-0023
  amendment); the four lens contracts are unchanged.

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
