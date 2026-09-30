# ADR-0040 — Recipe provenance on artifacts, and a targeted audit re-ask

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions recorded 2026-09-30)
- **Amends:** [ADR-0023](./0023-verification-layer.md) (the Auditor may be re-asked once, for the findings it left unanswered)

## Context

An AI-written artifact says nothing about what produced it. Skills and workflows are owner-editable
markdown ([ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md),
[ADR-0035](./0035-workflows-as-markdown-and-the-workflow-book.md)), and several prompts are coupled to
code that parses their answers, so an artifact written last month cannot be told from one written
today, and "why did this lens come back off-contract" has no answer on the page.

Separately, the factored audit ([ADR-0023](./0023-verification-layer.md)) is one call over the whole
numbered finding list. Its raw text was parsed and dropped, and a malformed or partial answer left
every unanswered finding `UNCERTAIN` for a formatting reason, not a substantive one. Only a fully
unparseable audit was marked failed; a partial one was silent.

## Decision

We will stamp every AI-written artifact with an optional **`recipe:`** frontmatter block, freeze the
prompts whose answers code parses behind golden tests, and give the Auditor **at most one targeted
re-ask**.

**Recipe** (`domain::Recipe`, on `ArtifactFrontmatter`, all fields optional except `build`):

| Field | Meaning |
|---|---|
| `skill`, `skill_digest`, `skill_source` | the skill's name; 12 hex digits of the skill file's **raw bytes** (before `{context}` is filled); `built-in`, `vault override` or `vault` |
| `workflow`, `workflow_digest` | the workflow's name; 12 hex digits of the resolved workflow file's raw bytes |
| `templates` | `id@vN:digest12` for each parse-coupled prompt the run used |
| `build` | `CARGO_PKG_VERSION`, plus `+<sha>` when the image was built with `IDEA_VAULT_BUILD_SHA` (a Dockerfile `ARG`; unset under `cargo run`, which stamps the version alone) |
| `contract` | one `<lens>: off-contract: <violation>` line per model call (fan-out lens, chained step, Ground reader) whose kept answer the call recorded as `OffContract`, truncations included; read from the `ContractOutcome`, never re-derived by re-validating the kept text (a truncated answer is often shape-valid); empty when every call was on contract |

- **Where it is written.** Every AI-written artifact writer: `knowledge` (extraction lenses and
  synthesis), `memory::extract` (the store-time quarantine artifact), the build-plan writers
  (`build_plan::finish` for a quick or audited plan; the workbench copies the base plan's recipe
  onto an answered version, restamped with the build that folded the answers in) and the workflow
  engine (stage artifacts and the run record). A skill run that only appends a turn writes no
  artifact and so no recipe. The field is truth in the markdown, so reindex round-trips it unchanged.
- **Old artifacts show "provenance unknown", never "stale".** An artifact without `recipe` parses as
  before.
- **R19 shows it.** The artifact view renders `skill <name> @ <digest12> (<source>) ·
  build <version>[+<sha>]`, lists off-contract lenses, and adds a **"recipe changed since"** badge when the
  stored skill or workflow digest differs from the live registry's (or the skill is no longer
  registered). `/skills` shows each skill's and workflow's digest and marks overrides.
- **Registered templates.** `ai::provenance::PromptTemplate { id, version, text }` covers only the
  parse-coupled prompts: `audit`, `audit-reask`, `retry-note`, `extract` and `consolidate`. Each has an
  explicit version bumped by hand on purpose and a golden file in `tests/fixtures/prompt-goldens/`
  (also the audit prompt for a fixed input, the re-ask suffix, the retry note, the extraction
  instruction, and `agents::build_prompt` over a fixed test skill). The ref carries the text's digest,
  so an edit that forgot to bump the version still shows as a different ref. Personas and skills are
  **digested, never frozen**: they are owner-editable prompt data.
- **Audit re-ask.** When the first audit answer leaves any finding without a verdict (a malformed or a
  partial answer), and the caller can fund it, `concepts::audit` makes **one** extra Auditor call whose
  suffix lists only the missing ids (`reask_suffix`). The results are merged **first verdict wins**. If
  the re-ask errors or stays garbled, the existing `UNCERTAIN` fallback is kept, a call that failed
  outright is not re-asked (it was an outage, not a malformed audit), and a partial answer now logs a
  warning. Each audit call journals its own verdict ([ADR-0038](./0038-parser-corpus-and-read-only-regrade.md)).
  A swarm has no call budget, so it always may re-ask; a workflow's audit and Refine passes may re-ask
  only when the `CallBudget` can fund two calls, and the re-ask is charged like any other request
  ([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)).

## Consequences

- **An artifact says what made it.** An owner can tell an old artifact from a new one, see that the
  skill behind it has since been edited, and see which lens fell off its contract.
- **The frontmatter grows an optional field.** Artifacts stay readable by older builds only if those
  ignore unknown keys; within this codebase the field is optional and absent on legacy files.
- **Fewer `UNCERTAIN` verdicts for formatting reasons.** The price is one extra call on a malformed or
  partial audit only, never on a complete one. This updates ADR-0023's consequence "one more call per
  swarm" to "one more call per swarm, and at most one more when the audit answer is malformed or
  partial".
- **A prompt wording change is a deliberate act.** It fails its golden until the golden is updated and
  declared under `## Expectation changes` ([ADR-0041](./0041-no-mistakes-gate.md)), and the template's
  version is bumped so old and new artifacts differ.
- **The build id needs a build argument.** Without `IDEA_VAULT_BUILD_SHA` (a plain `cargo run`) it is
  the crate version alone.

## Alternatives considered

- **Digest only the parse-coupled template constants, no build id.** Rejected by the owner in favour of
  a build id through a Dockerfile `ARG`.
- **A frozen-prompt change-log table enforced by the gate.** Rejected: its "measured effect" rows
  cannot be filled honestly and it is ceremony for a solo owner. The goldens are enough.
- **Freeze every persona and skill.** Rejected: they are the owner's own editable data, and a freeze
  would fight the skill book ([ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md)).
- **One auditor call per finding.** Rejected in ADR-0023 for cost; a single targeted re-ask keeps the
  cost bounded.
- **Majority-of-N audit votes.** Deferred: it triples the heaviest call and low-temperature votes are
  correlated.
- **Store provenance in the journal only.** Rejected: the journal is diagnostics and pruned
  ([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)); provenance must travel with the
  artifact, which is truth.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
