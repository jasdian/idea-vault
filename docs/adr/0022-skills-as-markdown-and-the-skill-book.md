# ADR-0022 — Skills as markdown files, owner overrides, and the skill book

- **Status:** Accepted
- **Date:** 2026-09-28
- **Deciders:** owner

## Context

Skills are the product's reusable ideation moves ([D18](../06-concepts/skills.md)). Until now each
one was a hardcoded Rust string: a name, a one-line description, and a one-line prompt, with five
`TODO(skills)` markers where the real prompts should have been. Three things were missing:

- **Routing guidance.** Nothing told the owner (or the foil) *when* to use which move. The chip
  tooltip said "Run the premortem move". The docs promised a `market-size` move and
  "steelman, then stress-test" as the core loop, but there was no steelman skill.
- **Structure.** A skill carried no stage, no persona, and no output shape. Every swarm angle ran
  as a Critic, even `constraints`, which the workflow ran as a Researcher.
- **Owner authorship.** CLAUDE.md calls skills "loadable/composable", but adding a move meant
  recompiling the app.

The owner's agent-harness skill system (`the-unknown`, `backend-mono`) solves the same problems
for coding work:

- Each skill is a `SKILL.md` file with frontmatter.
- A `SKILLBOOK.md` catalogues the skills along a "spine" of stages, with a
  `Use when | Do not use when` table and a "common wrong turns" list.

The owner asked for those mechanisms to be applied to idea-vault's own skills. They chose built-in
markdown files **plus** owner overrides in the vault.

## Decision

We will define every skill as a markdown file: YAML frontmatter plus a prompt body with a
`{context}` slot.

The frontmatter is `SkillFrontmatter`, parsed by `domain::frontmatter::parse_skill`, with
`deny_unknown_fields` so a typo in an owner file is reported rather than ignored:

| Field | Values / meaning |
|---|---|
| `name` | Must equal the file stem and pass `slug::is_valid` |
| `description` | What the move does |
| `stage` | A `SkillStage`: steelman / attack / consequence / converge / capstone / extract |
| `role` | A `SkillRole`: critic / researcher / advocate / harvester / synthesizer — the persona the skill runs under when an orchestrator fans it out |
| `contract` | An `OutputContract`, see [ADR-0023](./0023-verification-layer.md) |
| `use_when` | When to reach for the move |
| `avoid_when` | When not to |
| `hidden` | Registered but not offered as a move chip (the `extract-*` lenses) |

**Where skills live:**

- **Built-ins** are `src/concepts/skills/*.md`, compiled in with `include_str!`, so the app still
  ships as a single binary. They live under `src/` because the Docker build context carries only
  `src`, `templates` and `static`.
- **Owner skills** are `vault/.skills/<name>.md` (`IDEA_VAULT_SKILLS_DIR`). A file that names a
  built-in replaces it in place. A new name is added.

**Loading and reload:** `concepts::skills::LiveSkills` loads built-ins plus owner files at boot.
`POST /skills/reload` swaps in a freshly loaded registry with no restart and no file watcher. A
handler takes one `snapshot()` and moves it into its job, so a reload never changes a run already
in flight.

**When an owner file is invalid** it becomes a `SkillIssue` listed on the skill book, and the
built-in of the same name stays active. That covers:

- a parse error or unknown key;
- a name that doesn't match the file stem, or isn't slug-safe;
- a missing `{context}` slot;
- a file over 32 KB.

Boot never fails on a broken skill file.

**The skill book (`GET /skills`)** lists every skill grouped by stage along the **spine**
(steelman → attack → consequence → converge → capstone, `SkillStage::SPINE`), with its use-when /
not-when guidance, role, output contract and source. Chip tooltips carry the same guidance.

**Coverage (`concepts::coverage`)** reads only `conversation.md`'s turn headings, parsed once by
`vault::store::parse_turn_heading`. It derives:

- which spine stages the discussion has covered;
- a suggested **next move**;
- soft **wrong-turn** warnings: a build prompt before any attack move; the same move three times
  in a row.

Warnings never block. The idea page shows the result as a spine strip. The Store button carries a
"no attack move has run yet" note while that is true.

**The chat foil** carries a ≤1 KB skill book (visible moves, one "name — use when" line each)
inside its budget, so it can recommend a move by name.

**New built-ins and roles:**

- `steelman` — stage steelman, role advocate.
- `market-size` — stage consequence, role researcher.
- The five placeholder prompts are replaced by full prompts.
- `concepts::agents::AgentRole` gains `Advocate` and `Harvester`. The `extract-*` lenses run as
  Harvesters instead of Researchers, whose persona said "from your own knowledge" and contradicted
  "harvest only".

**Swarm angles:** the swarm uses each angle's own role. It rejects a capstone as an angle (400).
Hidden `extract-*` lenses stay usable as angles, as [ADR-0015](./0015-knowledge-extraction-artifacts.md)
decided.

## Consequences

- **Moves are data.** The owner can write, override, and version moves in the vault like any other
  markdown, and see them on the skill book with any load problems.
- **`vault/.skills/` is app configuration, not idea truth.** Like `.mcp-servers.json` and
  `.sources.json`, the idea walker skips it (no `idea.md`) and reindex never sees it. Deleting it
  just restores the built-ins.
- **Skill names are part of the transcript grammar.** `## assistant (skill: <name>)` is why names
  must be canonical slugs. The swarm heading now names its angles (`## assistant (swarm: a, b)`) so
  coverage can see them; the legacy `## assistant (swarm)` still parses (it counts as the default
  angles).
- **Coverage is derived, never stored.** Deleting a turn uncovers its stage. The markdown stays the
  only truth ([ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md)).
- **Some prompt text is load-bearing for tests.** "failed badly 12 months", "cheapest, fastest
  test", "second-order" and "constraints" are phrases tests look for. Changing them in a built-in
  means updating the tests.
- **Prompt budget:** the skill book adds up to 1 KB to every chat prompt, taken out of the
  context budget.

## Alternatives considered

- **Keep skills in Rust, just add fields.** Rejected: no owner authorship, and prompt text buried
  in string literals is hard to review and edit.
- **Owner skills loaded from disk only, no built-ins in the binary.** Rejected: a fresh install
  would have no moves, and the single-binary deployment contract would break.
- **Watch `vault/.skills/` for changes.** Rejected: a watcher is a moving part for a rare action. An
  explicit reload on the page that shows the result is clearer.
- **Persist spine coverage in frontmatter.** Rejected: it duplicates what the transcript already
  says, and could drift from it after a turn deletion.
- **Hard-block wrong turns** (e.g. refuse a build prompt before an attack). Rejected: the owner
  drives the idea. The skill book advises, it doesn't gate.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
