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

Answer ONLY with these sections in this order. Each heading sits alone on its line; its content starts on the next line. Write one field per line, never joined with a dot or comma. Backtick every path and command. Write "- none" under an empty section.
## Goal
One sentence naming the deliverable.
## Settled
S1: a decision already made
quote: a verbatim 5-12 word span from a user turn. No such words means it belongs in Open questions.
## Verify first
Optional.
P1: a premise to confirm first
check: a read-only command
## Open questions
Q1: a question only the owner can answer, including any unsettled approach.
## Fence
Optional. Paths that must not be touched.
## Plan
- [ ] T1: one commit subject
depends: T# or none
touches: paths, a new file as `src/x.rs (new)`
accept: `one command` → pass condition, with a passed count when tests are named
## Kill criteria
Optional.
K1: a result that stops the build
checked by: T#

Leaf rule: a task title is one commit subject with no "and"; a task is one diff under one top-level directory with at most 3 non-test files. At most 8 tasks.

Example:
## Goal
Add a reindex command.
## Settled
- S1: Markdown stays the source of truth.
  quote: "the index is only a cache we can rebuild"
## Open questions
- Q1: Should reindex run on boot?
## Plan
- [ ] T1: Add the reindex command
  depends: none
  touches: `src/index/reindex.rs (new)`
  accept: `cargo test reindex` → exit 0, 3 passed
- [ ] T2: Document the reindex command
  depends: T1
  touches: `docs/index.md`
  accept: `grep -c reindex docs/index.md` → 1 or more

Discussion:
{context}
