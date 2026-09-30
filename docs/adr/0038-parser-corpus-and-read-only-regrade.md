# ADR-0038 — Parser corpus and read-only regrade

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions recorded 2026-09-30)

## Context

Most fix commits in this codebase are about code that judges a model answer: the audit parser, the
store-time fact parser and its evidence gate, the skill output contracts, and the build-plan parser
and gates G1–G14. Each was tuned against a handful of hand-written strings, and each change risked
flipping a verdict on a real answer nobody kept. The run journal
([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)) now keeps the verbatim answer beside
the verdict a parser gave it, which makes a re-check possible with no model call.

What can and cannot be re-checked matters. A recorded answer is what the model said to an *old*
prompt. Re-running today's parser over it shows what a **parse, detector or gate change** flips. It
shows nothing about a **prompt change**, which alters what the model would say.

## Decision

We will make every deterministic judge of a model answer write one canonical **verdict line**, journal
it, and provide two read-only ways to replay it: the `regrade` subcommand over the owner's journals,
and a committed **parser corpus** test.

- **`summarize` is the single definition.** `regrade::summarize(kind, raw, haystack)` returns a
  space-separated `key=value` line whose first token is `pass=<n>` (units accepted), and dispatches to
  each parser's own function: `concepts::audit::summarize_audit`, `memory::extract::summarize_facts`,
  `ai::contract::summarize_contract` and `concepts::build_plan::finish::summarize_plan_gates`. The parse
  sites journal `Verdict { summary: <that line> }` and the corpus test calls the same function, so a
  journaled line and a replayed one cannot drift. The parser kinds are `audit` (over `n` findings),
  `facts`, `contract` (by its frontmatter name) and `plan-gates`.
- **`idea-vault regrade [--idea <slug>] [--parser audit|facts|contract|plan-gates] [--strict]`** walks
  `vault/*/.runs/*.jsonl`. For each `verdict` entry it re-runs the *current* parser over the paired
  `llm_call.response_text` and compares lines. It prints one line per flip
  (`<slug>/<run_id> #<seq> <parser> <key before->after>`) and then totals: flips to pass, flips to
  fail, changed with the pass count held, unchanged and skipped with each reason. It **never writes to
  the vault**. Exit is 0, or 1 only under `--strict` when there is a flip.
- **A haystack is recovered or the verdict is skipped.** A grounding verdict (facts, plan gates) needs
  the text its quotes were checked against. The journal records a `HaystackRef`: the conversation's
  length and SHA-256, the idea body's SHA-256, and the body text itself only when the same run
  rewrote it (a store consolidates the body). `conversation.md` is append-only, so regrade re-hashes
  the recorded-length prefix; a prefix or body hash that no longer matches gives **skipped (haystack
  changed)**, never a regrade against drifted text.
- **The corpus is curated by hand.** `idea-vault regrade --export <slug>/<run_id>#<seq> <case-name>`
  copies one journaled answer, its parser and its haystack slice into
  `tests/fixtures/raw-outputs/<parser>/<case>.md` (`create_new`; it never overwrites a case). This is
  the only write regrade makes, it is never into the vault, and **nothing is exported automatically**:
  journals hold fetched web text and private notes, so what enters the repo is the owner's explicit
  choice.
- **`tests/parser_corpus.rs`** runs every fixture through today's parsers and compares the verdict
  lines with the committed `tests/fixtures/parser-corpus.snap`. A mismatch prints the flip report and
  fails. `PARSER_CORPUS_BLESS=1 cargo test --test parser_corpus` rewrites the snapshot, so a parser
  change carries a snapshot diff in the same commit. Blessing is an ask-user change
  ([ADR-0041](./0041-no-mistakes-gate.md)): the gate refuses to run while the variable is set, and the
  snapshot change must be declared under `## Expectation changes`. The corpus runs under
  `cargo test`, and gate step 7 replays it once more by name.
- **Seeded by hand.** The initial ten cases come from known-bad shapes: an escaped-quote store
  extraction, an invented quote, a garbled audit, a partial audit, unclosed fences and a preamble
  before a ranked list, and a gated and an unusable build plan.
- **Scope.** Replay covers parse, detector and gate code only. `regrade` needs a vault, so
  `regrade --strict` is not a gate step; the corpus test is its offline stand-in.

The flow is [D40](../10-testing-strategy.md).

## Consequences

- **A parser edit shows what it flips before it ships**, against real answers and against the curated
  corpus, with no model call and deterministically.
- **A parser change is a visible snapshot diff.** Editing a fixture or the snapshot is an
  expectation change and is declared, not slipped in.
- **The dependency direction holds.** `regrade` is a bin-level driver like `import`
  ([D4](../02-module-reference.md)); the parsers never import it, because they own their summary
  functions and `regrade::summarize` only dispatches. `ParserKind` and `HaystackRef` live in
  `ai::verdict` because the journal carries them and `ai` sits below every parser.
- **Older journals degrade, not fail.** Lines are read tolerantly; a verdict whose call is missing is
  skipped with a reason.
- **Prompt changes need something else.** They are pinned by the golden tests for parse-coupled
  prompts ([ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md)), not by regrade.

## Alternatives considered

- **Key a fail-closed replay backend on a request digest.** Rejected: every prompt edit changes the
  digest, so every edit would invalidate the recording. Keying on the *parser*, not the request,
  survives prompt edits. (This is also unrelated to the MCP replay of
  [ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md).)
- **Export corpus cases automatically from every run.** Rejected: privacy, and an unreviewed corpus is
  not a set of expectations.
- **A model judging whether a regraded verdict is right.** Rejected for the same reason as
  [ADR-0023](./0023-verification-layer.md) rejects a model judging grounding.
- **Compare journal to journal (a structural comparator).** Rejected for now: the reindex invariant
  is already covered by the golden vault, and there is no journal-to-journal comparison need yet.
- **Make `regrade --strict` a gate step.** Rejected: it needs the owner's vault, and the gate must run
  offline on a fresh clone.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
