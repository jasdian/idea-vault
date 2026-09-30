# Intent — make-skill button: distil an owner skill from a discussion (ADR-0042, D42)

The owner distils reusable ideation moves by hand today: a move that worked in one discussion
(often one improvised in chat, never a named skill) stays buried in that idea. The make-skill
button runs one background job that reads the discussion and drafts a skill file for the skill
book, which the owner reviews, edits and saves. Design: ADR-0042 (D42, R51, R52; amends ADR-0022
and ADR-0023). Owner decisions of 2026-09-30 are binding: D1 stored ideas can be distilled without
reopening, with a visible thinking indicator; D2 an optional `origin:` field on skill files; D3
evidence grounding is a warning only, never a Save gate; D4 the draft is editable before Save; D5
MCP gets a draft-only `make_skill` tool; D6 built-in and internal names force a rename, an owner
skill is updated with a diff and a stale check; D7 make-workflow is a later phase; D8 the job runs
under the harvester role.

## Acceptance criteria

- An idea page has a "make skill" button; pressing it runs a background job with a visible thinking
  indicator and ends with a skill draft under Artifacts, never a transcript turn (ADR-0042).
- The draft is a skill file the skill book's own loader accepts; every evidence quote is marked
  grounded or not against the discussion (an ungrounded one is warned about, never a Save block,
  D3), and the run journal is never read.
- Nothing reaches vault/.skills/ until the owner presses Save; Save revalidates the (editable) text,
  never overwrites a built-in or internal skill, shows a diff and refuses a stale base when updating
  an owner skill, and the new skill appears on /skills with a "distilled from" link without a
  restart.
- A distil run costs at most 2 model calls; a stored idea can be distilled without reopening it,
  with a visible thinking indicator on the stored view (D1).
- MCP clients can draft with `make_skill` (long-running, idempotent replay) but cannot save (D5).
- Every change is observed failing first, and `bash scripts/gate.sh` is green.

## Expectation changes

- src/domain/skill.rs OutputContract::ALL: 8 → 9 (`skill_draft`); the docs/06-concepts/skills.md
  contract row gains `skill_draft` (tests/doc_examples.rs `skill_field_table_matches_enums`).
- src/concepts/skills.rs INTERNAL_SKILLS: 2 → 3 and BUILTIN +1 (`distill-skill`); tests keyed on
  `BUILTIN.len()` follow without edits.
- src/web/routes/ideas.rs stored_outcome: the Running arm becomes a visible thinking indicator (was
  an aria-hidden poll); the "Only the store job can finish on a Stored idea" statement is withdrawn
  (D1).
