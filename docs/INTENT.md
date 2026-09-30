# Intent — plan owner answers: never demoted by G6, G10 settled by its answer (ADR-0030, ADR-0032)

Two defects seen live on a plan lineage. An owner's workbench answer holding a figure ("2.1.285",
"24h") was moved from Settled to Verify first by G6, whose figure and recount checks exist to catch
model-invented numbers; the answer then fell out of `answered_in_lineage`, which read only Settled.
And G10 re-opened its "gate language without a kill row" question with the identical text the
moment the owner answered it, because the answer removed the only thing that suppressed it. Design:
ADR-0030 amendment (G6 owner-answer exemption, G10 settled-through and quoted-mention rules) and
ADR-0032 amendment (lineage reads every section; the workbench restores a demoted answer).

## Acceptance criteria

- A Settled item that is an owner answer (`answers` or `unblocks`, Owner provenance, text equal to
  its quote or a `…` clip of it) is never moved by G6; a foil-grounded item, an owner quote without
  an answer key, and a model paraphrase carrying the answer keys keep today's G6 behaviour.
- `answered_in_lineage` reads `answers` items from Settled, Verify first, Open and Quarantined, so
  an answer a gate moved is still carried and never re-asked.
- A workbench answer moves every owner answer an older version left in Verify first back to
  Settled, unmarked, before the gates run.
- G10 scans only owner turns after the newest owner answer to a G10 question; gate language the
  owner writes after that answer still opens a question.
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
