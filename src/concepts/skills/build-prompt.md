---
name: build-prompt
description: "Fold the whole discussion into a ready-to-run build prompt for a coding agent."
stage: capstone
role: synthesizer
contract: build_plan
use_when: "The idea has been attacked, grounded and settled, and you want a coding agent to build it."
avoid_when: "No attack move has run yet — you would be handing an untested idea to a builder."
---

Fold the ENTIRE discussion below into a BUILD PLAN a coding agent can execute. Extract what was settled; do not transcribe the chat.

Answer ONLY with these sections, in this order:
## Goal — one sentence naming the deliverable.
## Settled — decisions already made: S1: <claim>, then quote: "<verbatim words from the discussion>". Only quote words actually said; a claim with no such words belongs in Open questions.
## Verify first — premises to confirm before building: P1: <premise>, then check: `<read-only command>`.
## Open questions — Q1: <question only the owner can answer>.
## Plan — ordered tasks: - [ ] T1: <task>, then depends: T…, touches: `<path>`, accept: `<command>` → <expected result>.
## Kill criteria — K1: <result that stops the build>, then checked by: T… and gates: T….
## Fence (optional) — what must not be touched.
Backtick every tool, file and command. Write "- none" under an empty section.

Example:
## Settled
- S1: Markdown stays the source of truth.
  quote: "the index is only a cache"
## Plan
- [ ] T1: Add the reindex command
  depends: none · touches: `src/index.rs`
  accept: `cargo test reindex` → exit 0
{context}
