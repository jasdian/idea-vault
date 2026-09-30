# ADR-0042 — Make skill: distil an owner skill from a discussion

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions D1–D8 recorded 2026-09-30)
- **Amends:** [ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md) (an owner skill may be born
  from a draft; the `origin` field; a third engine-only skill), [ADR-0023](./0023-verification-layer.md)
  (the `skill_draft` contract), [ADR-0010](./0010-ai-turns-as-background-jobs.md) (a second job kind
  may run on a Stored idea)
- **Links:** [ADR-0037](./0037-run-journal-diagnostics-only-call-record.md) (the journal is never
  read), [ADR-0040](./0040-recipe-provenance-and-audit-re-ask.md) (the draft's recipe),
  [ADR-0032](./0032-plan-workbench-answers-and-versions.md) (the synchronous-save precedent),
  [ADR-0033](./0033-mcp-idempotent-replay-and-plan-tools.md) (MCP replay), D42

## Context

The owner asked: "idea-vault app will benefit from a make-skill button". Skills are markdown files
([ADR-0022](./0022-skills-as-markdown-and-the-skill-book.md)), and the owner already distils
reusable skills from Claude Code sessions with a `/make-skill` command. In idea-vault the most
valuable move in a discussion is often one the owner improvised in chat ("now assume a regulator
hates it"), which has no heading and stays buried in that one idea.

Forces:

- **Markdown is truth** ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)); `vault/.skills/` is app
  configuration, not idea truth, and reindex never sees it.
- **AI work is a background job** ([ADR-0010](./0010-ai-turns-as-background-jobs.md)) with a visible
  indicator, under the shared permit ([ADR-0006](./0006-bounded-concurrency-swarm.md)).
- **Small local models** ([ADR-0014](./0014-dynamic-context-budget.md)) miss formats; a contract
  with one retry ([ADR-0023](./0023-verification-layer.md)) is the existing answer. A live probe
  (qwen3-8b-local, temperature 0.2, 2026-09-30) produced a valid draft in 2 of 3 tries; the miss was
  a file with an empty prompt, which the contract retries.
- **A skill's body is its prompt.** A literal `{context}` in the distiller's own prompt would be
  filled with the discussion, and an HTML comment in a skill file would be sent to the model.
- **The run journal is diagnostics** ([ADR-0037](./0037-run-journal-diagnostics-only-call-record.md));
  the `runs-not-truth` gate rule forbids reading it from `concepts`.
- **Model text written as a turn becomes evidence** for later quotes (an `Other` turn is not excluded
  from the haystacks).

## Decision

We will add a make-skill button that drafts, and a Save that writes, as two separate steps.

1. **R51 `POST /idea/:slug/make-skill`** is a background job (`RunKind::MakeSkill`) on the
   capstones row and on the stored panel. Its guard admits any state but Draft (a Stored idea is
   distilled without reopening it, **D1**) and requires at least 2 owner turns and 1 named move.
2. The job runs the internal built-in skill **`distill-skill`** (stage `extract`, role `harvester`
   and its [ADR-0026](./0026-per-role-call-profiles.md) profile, **D8**) over the idea's budgeted
   context plus a code-built **move trace** (≤ 1500 bytes, labelled not evidence) and the skill book's
   names (≤ 1024 bytes), in one `ask_on_contract` call with at most one retry.
3. The answer is held to a new output contract, **`skill_draft`**: one `~~~skill` tilde-fenced file
   that parses with the skill loader (no unknown key, slug name, an owner-facing stage and contract,
   not hidden, no `origin`) and a `## Evidence` list of double-quoted passages of at least
   `MIN_QUOTE_WORDS` words. Its violations are `NotASkillFile(why)` and `NoEvidence`. The fence is
   tilde so a drafted prompt's own backtick fences cannot close it.
4. **Code finalizes** the draft: it strips every model-written `{context}`, appends the one slot, sets
   **`origin: <idea-slug>`** (a new optional `SkillFrontmatter` field, **D2**), and checks the file
   with the loader's own rules (`concepts::skills::check_candidate`). Each quote is grounded against
   the non-capstone turns as ✓ owner, ✓ or ✗.
5. The job writes **one `skill_draft` artifact** (`skill-draft-<name>`, recipe of `distill-skill`)
   and **no transcript turn**, and ends with a one-shot notice. The stored view's running state shows
   the visible thinking indicator, and its responses refresh the artifacts panel out of band.
6. **R52 `POST /idea/:slug/artifact/:name/save-skill`** is synchronous with no model call and no job
   slot. The owner may edit the draft first (**D4**). Save revalidates the text against the live
   registry, refuses a built-in or engine-only name (**D6**, `422`), refuses an update whose base
   digest is not the owner file's current digest (`409`), writes `vault/.skills/<name>.md` through
   `vault::store::write_owner_skill` (atomic, slug-checked), and reloads the skills and workflows.
   An ungrounded quote is marked ✗ and warned about but **never blocks Save (D3)**. The artifact is
   never modified.
7. **MCP** gets a draft-only long-running `make_skill` tool (task-optional, idempotent replay); there
   is **no save tool (D5)**, so a client can never write the owner's skill book.
8. **Make workflow** (**D7**) is a later phase: a zero-call draft built in code from the move trace,
   reusing this artifact, review and Save shape.

## Consequences

- `OutputContract` has a ninth variant (`skill_draft`), `ArtifactKind` an eighth (`skill_draft`),
  `INTERNAL_SKILLS` a third name (`distill-skill`), and `RunKind` a tenth (`make-skill`). The skills
  doc's contract row follows `OutputContract::ALL`.
- The skill loader checks `origin` as a slug, so the skill book can link "distilled from <idea>"
  safely; a hand-written file may set it too.
- Two job kinds may now run on a Stored idea: the store job and make-skill. make-skill never changes
  the state, so the stored poll branch serves its polls and notice.
- A distil costs at most 2 model calls under one permit; Save costs none.
- `vault/.skills/` gains a code writer, `vault::store::write_owner_skill`, alongside the owner's own
  edits; it never writes a built-in's name.
- No new env var, no new dependency (the review diff is an in-house LCS over ≤ 32 KiB files), and no
  new invariant rule: `runs-not-truth` already keeps the journal out of `concepts`, and the
  never-overwrite-a-built-in rule is a test.

## Alternatives considered

- **An in-memory draft reviewed on `/skills`** (keyed to the idea) — a second poll path beside
  `/idea/:slug/pending`, no place for the recipe, and a restart loses a paid model call.
- **A deterministic skeleton with a one-paragraph model call** — sees only named moves, so it cannot
  capture an improvised chat move, which is the most valuable candidate. Kept for make-workflow.
- **Reading the run journal for the moves** — forbidden by ADR-0037 and the `runs-not-truth` rule.
- **Provenance as an HTML comment in the skill body** — the body is the prompt, so the comment would
  reach the model.
- **A pointer turn in the transcript** — an `Other` turn is evidence for later quotes.
- **Blocking Save until an owner quote grounds (D3 option a)** — the owner chose a warning only.
- **Letting the button override a built-in (D6 option b)** — overrides stay hand-written.
- **A save tool over MCP (D5 option b)** — Save stays an owner click.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
