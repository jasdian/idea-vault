# ADR-0015 — Knowledge extraction with persisted per-idea artifacts

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

## Context

The classic swarm ([ADR-0006](./0006-bounded-concurrency-swarm.md), [D14](../06-concepts/swarm.md))
fans agents out over an idea and converges them into one synthesized turn; intermediate per-agent
outputs are deliberately discarded — only the converged result is kept. That is the right default
for "run it into the ground" chat, but it is the wrong default for a distinct owner need: pulling
durable, categorized knowledge out of a long discussion — decisions made, facts established, open
questions, risks/assumptions, next actions — as something the owner can keep, reread, and share,
not just a single merged paragraph buried in the transcript. The owner wants each lens's raw
findings to survive as its own file, not just the digest.

## Decision

We will add `concepts::knowledge::extract_knowledge`, which reuses the swarm's shared machinery —
one `AgentRole::Researcher` per extraction lens, the same bounded `fan_out` (ADR-0006 semaphore, K
in flight), the same `judge` and `synthesize` steps — but **persists every non-empty per-lens
finding as a truth file** in a new `vault/<slug>/artifacts/` directory
(`<run-stamp>-<lens-short>.md`), plus the synthesis as `<run-stamp>-synthesis.md`, and appends the
synthesis as one `## assistant (knowledge)` conversation turn. This is a deliberate, scoped
divergence from the classic swarm's "intermediates are never persisted" rule — it does not
supersede that rule for swarm proper, it adds a second orchestration with a different persistence
contract.

Five built-in lens skills ship with a reserved `extract-` prefix in `SkillRegistry::builtin()`
(`extract-key-decisions`, `extract-durable-facts`, `extract-open-questions`,
`extract-risks-assumptions`, `extract-next-actions`). `SkillRegistry::move_names()` hides
`extract-*` from the interactive moves chip row — they are orchestrator-only lenses — but they stay
registered and resolvable, so they remain usable as ordinary swarm angles too.

An artifact file's extension carries its role: `.md` is truth (frontmatter + body) and is indexed
into `search_fts` as kind `'artifact'`, exactly like an idea body or a conversation turn. `.html` is
a derived, unindexed export — an optional, self-contained report the owner can open standalone or
hand to someone else — written by the web layer after the orchestrator returns, never treated as a
knowledge source.

File stems use a run timestamp (`%Y%m%d-%H%M%S`) plus the lens's short name (the `extract-` prefix
stripped), disambiguated via `domain::slug::disambiguate` with a predicate that probes **both**
`.md` and `.html` for the candidate stem — so an exported `.html` report can never silently shadow
(or be shadowed by) a `.md` truth file with the same stem, and two runs in the same second still get
a `-2` suffix instead of colliding.

Policy for degraded runs: an unknown lens name fails fast before any model call (a misconfigured
request, not a runtime condition). If every lens fails or every result is empty, the run errors with
`ConceptError::NothingToSynthesize` and **nothing is written** — no partial artifact set. If at least
one finding is non-empty but the synthesizer itself returns an Ok-but-empty result, the findings are
still persisted, the synthesis artifact and conversation turn are skipped with a `tracing::warn!`,
and the run still succeeds — the findings are the primary deliverable here, unlike the classic
swarm, where an empty synthesis is the terminal (non-)result of the whole run.

`POST /idea/{slug}/extract` (R18) runs as an ADR-0010 background job — claim → spawn → poll — the
same shape as chat/skill/swarm. All vault writes (every finding `.md`, the synthesis `.md`, and the
conversation turn) happen in one await-free block after the last model call, so a cancel can no
longer land between them (tokio only aborts at await points): the whole `.md` set + turn is
all-or-nothing. The optional `.html` report is written *after* that block returns, so a failure or a
racing cancel while rendering it can cost only the derived report, never the findings.

## Consequences

- A new truth directory, `vault/<slug>/artifacts/`, sits beside `memory/` — the reindex invariant
  ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)) extends to it: artifact `.md` files
  are walked and re-indexed like every other markdown source, and a rebuild reproduces the same
  `search_fts` rows.
- The owner gets a durable, per-lens paper trail instead of only a converged paragraph — useful for
  ideas revisited long after the discussion that produced them.
- The classic swarm's "never persist intermediates" invariant is now scoped, not universal: readers
  of `concepts::swarm` must not assume it holds for `concepts::knowledge`, and vice versa.
- One more vault subdirectory to keep the mental model straight: `memory/` is distilled facts
  reloaded on Reopen; `artifacts/` is on-demand extraction output, not part of the Reopen context
  load.
- `extract-*` joins the reserved-prefix vocabulary a future skill author must avoid, alongside the
  existing move-name conventions.

## Alternatives considered

- **Persist findings only as conversation turns (one per lens).** Rejected: floods the transcript
  with five turns per run and makes the discussion harder to read; a single synthesis turn plus
  separate artifact files keeps the transcript legible while still keeping the raw findings.
- **A new SQLite table for artifacts.** Rejected: nothing beyond full-text search needs to query
  artifacts today: the existing `search_fts` kind discriminator (`'artifact'`, alongside
  `idea_body`/`conversation`) is sufficient, and a bespoke table would be one more thing the reindex
  invariant has to keep honest for no present benefit.
- **Render the HTML report on demand from a route, not write it to disk.** Rejected: the owner
  explicitly wants a durable, shareable file living in the vault they can open outside the app or
  send to someone else, not a page that only exists while the server is running.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
