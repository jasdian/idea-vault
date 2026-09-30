# ADR-0037 — Run journal: a diagnostics-only call record

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions recorded 2026-09-30)
- **Amends:** [ADR-0023](./0023-verification-layer.md) (a contract fallback is recorded as data, not only logged), [ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md) (the call budget charges billed requests)

## Context

A model call left no record beyond the turn or artifact it produced. Four things that matter when a
prompt, parser or model changes were therefore invisible:

- **What the model actually said.** A parse keeps its result and drops the raw text
  (`concepts::audit` parsed the Auditor's answer and threw the answer away), so a parse change could
  never be checked against a real answer.
- **How an answer met its output contract.** `ask_on_contract` repaired, retried or kept an
  off-contract answer and only wrote a `warn!` ([ADR-0023](./0023-verification-layer.md)).
- **Whether a call was cut short.** Ollama reports `done_reason` and the eval counts on its terminal
  chunk, and the claude CLI reports `usage` and a result subtype; `ai::stream` decoded neither, so a
  truncated answer (output hit the limit, or the prompt filled the window and Ollama dropped its
  head, [ADR-0014](./0014-dynamic-context-budget.md)) looked like any other answer.
- **What a call cost in requests.** A workflow's call budget was charged per step
  ([ADR-0034](./0034-grounded-ranked-and-bounded-workflow-stages.md)), but an Ollama tool round is an
  extra request and a retry is another, so the count under-reported.

The vault is the owner's truth and is versioned by the owner separately from the app
([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)). Verbatim responses include fetched web
pages and MCP output, so whatever records them has to stay out of truth, out of the index and out of
every prompt. [ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md) already uses the word
"replay" for something unrelated: replaying a served MCP result to an identical retry. This record
has nothing to do with it.

## Decision

We will write **one append-only journal per AI job** at `vault/<slug>/.runs/<run_id>.jsonl` and
make every backend call fill a small integer **call record**.

- **Kinds.** A run is one of chat, skill, swarm, workflow, extract, compact, store, build-plan or
  replan (`journal::RunKind`). `journal::open_run` mints the id, `YYYYMMDDTHHMMSSmmmZ-<kind>`, and
  `web::jobs::spawn_job` parks the handle on the slug's job slot; `web::routes::idea_llm` scopes the
  backend view to it, so every call the job makes is journaled without `concepts` knowing a journal
  exists. `jobs.rs` stays keyed by slug.
- **Entries** (one JSON object per line, tagged `type`): `run_started` (format version, run id, slug,
  kind, build id), `llm_call` (seq, role, backend, model, temperature in thousandths, SHA-256 of the
  request, the verbatim `response_text`, a `CallMeta`), `tool_call` (Ollama tool rounds: name, args
  hash, result capped at 12,000 chars), `contract` (the `ContractOutcome` of a call), `verdict`
  (a parser's verdict line, [ADR-0038](./0038-parser-corpus-and-read-only-regrade.md)) and
  `run_finished` (`done`, `failed`, `cancelled` or `panicked`, and the call count).
- **`CallMeta`** (`ai::call`): `usage` (prompt tokens, output tokens, `api_calls`), `stop_reason`
  (Ollama `done_reason`, or the claude result subtype), `num_ctx` (Ollama only) and `ms`. A count the
  backend did not report is `None`, never a guess. `output_truncated()` is `stop_reason == "length"`;
  `input_truncated()` is `prompt_tokens >= num_ctx * 98 / 100`, and unknown counts are never a
  truncation. `LlmBackend::chat_meta` returns the meta; `chat` keeps its signature and drops it;
  `TokenStream` carries a `MetaSlot` filled by the terminal chunk or `result` line.
- **Append-only and flushed.** The file is opened `create_new` (two runs can never interleave) and
  every line is written and flushed, so a crash leaves a parseable prefix and `read_run` tolerates a
  torn last line. A `Drop` guard on the writer writes `run_finished` as `cancelled`, or `panicked`
  while unwinding, for a job that never reported an outcome.
- **Integers and strings only.** A float would serialize differently across platforms, so the
  temperature is `temperature_milli`; a unit test walks a serialized entry for JSON floats.
- **Diagnostics, never truth.** The journal is never indexed (reindex skips the dot-dir; the
  `runs-not-truth` invariant rule keeps `src/index`, `src/memory`, `src/concepts` and
  `src/ai/budget.rs` from naming it), never read into a prompt, never readable by the claude-code
  foil whose cwd holds it (`--disallowedTools Read(./.runs/**)`, [ADR-0039](./0039-foil-hygiene-and-lockdown.md)),
  never copied by fork, and pruned to
  the **newest 50 runs per idea** whenever a run opens. The owner may add `.runs/` to the vault's own
  `.gitignore`.
- **A journal never fails a turn.** If the file cannot be opened or a write fails, one warning is
  logged and the job runs unjournaled.
- **Contract outcomes are data.** `ask_on_contract` returns a `ContractOutcome` (`Clean`,
  `Repaired`, `Retried`, `OffContract(violation)`), journaled against the call whose text was kept
  and stamped into an artifact's recipe ([ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md)).
  `Violation::Truncated` joins the existing violations: an **output** truncation goes through the
  existing single re-ask; an **input** truncation gets no re-ask (the same window would truncate
  again), records `OffContract("input truncated")` and warns once.
- **Billed requests, not steps.** A workflow's `CallBudget` is charged by a request meter on the
  backend view (`with_call_meter`): a retry counts, each Ollama tool round counts, one claude process
  counts one. A tool-using call can now cost more than the one call its stage's ceiling assumed, and
  the reserve check then funds fewer elastic rounds.
- **R50, the run inspector.** `GET /idea/{slug}/runs/{run_id}` is a read-only page over one journal:
  per call the role, backend and model, contract outcome, tokens, stop reason and truncation flags,
  with the verbatim response in a `<details>`. The idea page links the newest run as "last run".
  A `run_id` is validated to letters, digits and `-` so it can never name a path outside `.runs/`.
  Lines are read as plain JSON, so a journal from another build still renders what it can.

The lifecycle is [D39](../05-ai-integration.md).

## Consequences

- **The first corpus of real model output.** Parse, detector and gate changes can be checked against
  what a real model said ([ADR-0038](./0038-parser-corpus-and-read-only-regrade.md)).
- **Fallbacks, truncations and token counts are data.** An off-contract lens or a truncated answer
  shows on R50 and in the artifact's recipe instead of one log line.
- **Privacy and size.** The journal holds verbatim responses, including fetched web text and MCP
  output, and the request digests. It lives in the vault beside the idea but is not truth; the
  retention cap bounds it, and it is excluded from fork. An owner who versions the vault with git
  should ignore `.runs/`.
- **`ai` grows a write path.** `ai` still does not depend on `vault`: the journal writes under a path
  the caller hands it, and `open_run` lives in `ai::journal` with `std::fs` only.
- **Unrelated to ADR-0033.** This is a record for inspection. Nothing reads it back into a run, nothing
  serves a stored answer, and no request is keyed on its digest. It must never be called replay.
  (`regrade`, [ADR-0038](./0038-parser-corpus-and-read-only-regrade.md), re-runs *parsers* over the
  recorded text; it never re-runs a model or serves an answer.)
- **`prompt_eval_count` and ADR-0014.** The 98% test uses the count Ollama reports against the
  `num_ctx` actually sent, which [ADR-0014](./0014-dynamic-context-budget.md) already floors at the
  assembled prompt's own window, so a prompt sized against a larger window is never mis-flagged. A
  tool loop sums `usage` over its rounds (each re-sends the whole conversation) for the budget and
  the journal, but judges input truncation by `peak_prompt_tokens`, its largest single round, so a
  healthy multi-round call is never flagged as a truncation it did not have.
- **Cancellation is visible.** A job aborted by the owner leaves `run_finished: cancelled`; a panic
  leaves `panicked`.

## Alternatives considered

- **A replay backend that serves recorded answers (keyed on the request digest).** Rejected: the key
  changes on every prompt edit, the most common edit, and first-in-first-out pairing is flaky under
  bounded swarm concurrency ([ADR-0006](./0006-bounded-concurrency-swarm.md)). `LlmBackend` is also a
  concrete struct, so there is no seam to plug one into. A test-only scripted backend fed from
  journals is deferred until real data exists.
- **Keep the journal outside the vault, next to `index.db`.** Rejected by the owner: it belongs with
  the idea it describes and dies with it; the privacy cost is handled by the dot-dir, the retention
  cap and the fork exclusion.
- **Store it in SQLite.** Rejected: the index is rebuildable and must not hold anything a re-scan
  cannot recreate ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)).
- **Journal Ollama tool rounds as a determinism class.** Rejected: the model picks the tools, so the
  tool sequence is not deterministic, and claude-code's own tools are invisible. Tool rounds are
  recorded as plain data.
- **Record floats.** Rejected: see integers-only above.
- **Seeds for reproducible sampling.** Rejected: Ollama seeds do not reproduce across `num_ctx`,
  batching and hardware, and the claude CLI takes none.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
