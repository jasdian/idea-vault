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
- plan::tests (plan.md header, two tests): the `Rules:` header gains `Findings` (owner decision D-c).
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
