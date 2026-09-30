# ADR-0023 — A verification layer: output contracts, factored audit, grounded memory

- **Status:** Accepted — amended by [ADR-0037](./0037-run-journal-diagnostics-only-call-record.md) (a contract outcome is recorded, and truncation is a violation) and [ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md) (the Auditor gets at most one targeted re-ask); amended by [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md) (Panel scorers are cold Auditor calls, Refine re-audits, Panel proposals and Loop items are audited as findings); amended by [ADR-0042](./0042-make-skill-distil-owner-skills.md) (a ninth contract, `skill_draft`, with the violations `NotASkillFile` and `NoEvidence`)
- **Date:** 2026-09-28
- **Deciders:** owner

## Context

Local models are unreliable formatters and confident inventors, and nothing in idea-vault checked
their output:

- A skill's answer was persisted however it came back — preamble, sign-off, wrong shape.
- The swarm's "judge" ([D14](../06-concepts/swarm.md)) only dropped byte-identical duplicates. The
  synthesizer received `Finding i (critic)` blocks without knowing which angle produced them, and
  without the idea itself.
- Store-time memory extraction ([D12](../06-concepts/memory.md)):
  - wrote any `FACT:` the model produced into durable memory, including facts nobody said;
  - never showed the model what memory already held, so paraphrased duplicates piled up;
  - ran on the *pre*-consolidation body, contrary to the documented consolidate-then-distil order.

The owner's agent-harness skill system handles the same failure modes in coding work:

- **verified-reporting:** a draft is audited by a separate, *factored* auditor that sees only the
  atomic claims. Claims are labelled CONFIRMED / UNCERTAIN / DISPROVEN. A refuted claim is
  downgraded, never dropped, and a >90% uniform pass is treated as a smell.
- **reflect / librarian:** a lesson needs external evidence before it reaches memory; hypotheses are
  quarantined; a lesson is written as ADD / UPDATE / NOOP against the existing entries.
- **work-hard:** an evaluator-optimizer loop with a capped number of retries.

The owner asked for these to be applied to idea-vault. They chose to run the audit **by default**,
behind a live Settings toggle.

## Decision

We will add three checks, each deterministic where it can be and never fatal to a run.

### 1. Output contracts with one retry

Each skill declares an `OutputContract`: free / bullets-or-empty / ranked-list / fenced-markdown.
The check lives in `ai::contract::validate`, a pure module. It first repairs the answer (strips
preamble and sign-off, normalizes the fence), and only what still fails counts as a violation.

- **Single interactive call** (`skills::invoke`) and a workflow's **chained step** (both go through
  `skills::ask_on_contract`): a violation earns **exactly one** retry under the same permit, with
  the violation read back to the model. The failed answer is not resent. If the retry still misses,
  the best answer is kept and a warning logged.
- **Fan-out agents** (`agents::run_agent`) repair only and never retry, since a retry per agent
  would double the fan-out's cost.
- **Build prompts:** `build-prompt` persists only its fenced block, paired to the **last** bare
  fence because build prompts nest fences.
- **Compaction** pins its four `##` headings, checks them warn-only, and trims by section
  (`contract::trim_sections`) so an over-long summary never loses a heading.

### 2. Factored audit

After a swarm's or workflow's fan-out, `concepts::audit` runs these steps:

1. Split each agent's answer into atomic findings with `contract::items`, interleaved round-robin
   across lenses and capped at 20.
2. Merge near-duplicates across lenses (≥80% word overlap), keeping every lens that raised the
   finding.
3. Run **one** `AgentRole::Auditor` call. The auditor sees only the numbered findings — no critic
   personas or framing — plus the idea, memory and discussion. It is told to prefer UNCERTAIN over
   CONFIRMED when in doubt, and returns `F<n>: CONFIRMED|UNCERTAIN|REFUTED — reason`.

**Parsing and failure:** parsing is deterministic and tolerant. A missing line defaults to
UNCERTAIN. A failed or unparseable audit makes every finding UNCERTAIN and the report is marked
`failed`; the run continues.

**What the synthesizer sees:** the idea statement, then each finding with its lens, role and
verdict (e.g. `Finding 1 (premortem · critic) [REFUTED — …]`), clipped to the budget. It is told to
build on CONFIRMED findings, present UNCERTAIN ones as open questions, and not build on REFUTED ones.

**What code appends to the persisted turn** (the model doesn't write this part):

- the audit tally;
- a warning when more than 90% of at least 4 findings were confirmed;
- a `### Disproven objections` list, with each refuted finding struck through and the auditor's
  reason — downgraded, never dropped;
- or "unverified" when the audit failed.

**Toggle:** the audit is on by default (`LlmSettings.audit_findings`, initial value
`IDEA_VAULT_AUDIT_FINDINGS`) and can be switched off live on the Settings page
([ADR-0011](./0011-live-switchable-llm-backend.md)). Knowledge extraction
([ADR-0015](./0015-knowledge-extraction-artifacts.md)) is not audited: its findings are harvests,
kept per lens.

### 3. Grounded memory

Store now:

1. Consolidates first, then distils facts from the **consolidated** statement plus the transcript.
2. Shows the extractor the facts already in memory as `[[slug]] — title`.
3. Asks for each fact as `FACT:` / `OP: ADD | UPDATE <slug> | NOOP` / `QUOTE: "<verbatim span>"` /
   body.

**The evidence gate** (`grounded`) accepts a fact only if its quote has at least 3 words and occurs,
after normalization, in the raw `conversation.md` or the pre-store idea body. An elided quote
(`a … b`) passes only if its segments occur in order, each within 200 bytes of the previous one, so
two unrelated true fragments can't vouch for a spliced claim. The body the model
just wrote doesn't count — matching against it would be circular.

**Quarantine:** facts that fail the gate are written to `artifacts/<stamp>-quarantined-facts.md`
(`ArtifactKind::Quarantine`) instead of `memory/`. The stored view says so in a one-shot notice. It
also notes when the discussion was too long to read in full.

**UPDATE is append-only:** the new text goes under an `_Updated <date>:_` line, and the owner's
existing text is never replaced. An ADD that slugifies to an existing fact is still skipped (the
old dedupe, kept as a backstop). NOOP is skipped.

## Consequences

- **Every swarm and workflow costs one more model call** by default, the Auditor. On a slow local
  model the owner can switch it off. With it on, every finding reaches the owner already judged,
  and refuted objections stay visible instead of being quietly merged away.
- **A shape violation costs at most one extra call** per interactive skill. Fan-outs never pay it.
- **Memory is stricter.** Small models that paraphrase instead of quoting will see more facts
  quarantined. That is the intended trade: a missing fact can be copied back from the quarantine
  artifact by hand, but an invented fact in memory is silently reloaded on every Reopen.
- **"Memory only grows" (D9) still holds.** UPDATE appends and never rewrites, and quarantined
  facts are truth in `artifacts/`, reindexed and searchable like any artifact
  ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)). They are labelled
  "quarantined facts · unverified".
- **Bounded concurrency is unchanged** ([ADR-0006](./0006-bounded-concurrency-swarm.md)). The
  retry happens under the permit its first call already holds, and the audit is one `run_agent` call
  with its own permit. No nested permits.
- **Workflows gain an explicit `Audit` stage** ([D32](../06-concepts/workflows.md)), which does
  nothing while the toggle is off.

## Alternatives considered

- **One auditor call per finding.** Rejected for a local model: 20 findings would mean 20 calls.
  One call over the numbered list keeps the audit factored (it still sees only the claims) at a
  fixed cost.
- **Drop refuted findings from the synthesis.** Rejected: hiding a disproven objection loses the
  reason it was disproven. Downgrading keeps the trail.
- **Retry fan-out agents too.** Rejected: that doubles the worst-case cost of every swarm, for a
  formatting problem repair already mostly fixes.
- **Let a model judge whether a fact is grounded.** Rejected: an unverifiable model judging another
  model's claim is circular. A substring check against the transcript is deterministic and cheap.
- **Let UPDATE rewrite the existing fact.** Rejected: memory facts are owner-editable truth, and a
  rewrite could silently discard the owner's own edits.
- **No toggle (always audit).** Rejected by the owner in favour of on-by-default with a live toggle.

## Amendments — ADR-0037 and ADR-0040 (2026-09-30)

> **Contract outcomes are data ([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)).**
> The "warn and keep the answer" fallback of the output contract is no longer only a log line:
> `ask_on_contract` returns a `ContractOutcome` (`Clean`, `Repaired`, `Retried`, `OffContract`) that is
> journaled against the call whose text was kept and stamped into an artifact's recipe. A truncated
> answer is a violation (`Violation::Truncated`): an **output** truncation takes the existing single
> retry; an **input** truncation takes none (the same window would truncate again) and is recorded
> `OffContract("input truncated")`.
>
> **The Auditor may be re-asked once ([ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md)).**
> When the audit answer leaves any finding without a verdict and the caller can fund it, one further
> Auditor call is made whose suffix lists only the missing ids, merged first verdict wins; a re-ask
> that errors or stays garbled keeps the `UNCERTAIN` fallback, and a call that failed outright is not
> re-asked. This updates the consequence "one more call per swarm" to "one more call per swarm, and
> at most one more when the audit answer is malformed or partial". Retry and re-ask still run under
> the permit the call already holds, or the audit's own, so
> [ADR-0006](./0006-bounded-concurrency-swarm.md)'s bound is unchanged.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
