---
name: build-prompt
description: "Fold the whole discussion into a ready-to-run build prompt for a coding agent."
stage: capstone
role: synthesizer
contract: fenced_markdown
use_when: "The idea has been attacked, grounded and settled, and you want a coding agent to build it."
avoid_when: "No attack move has run yet — you would be handing an untested idea to a builder."
---

Synthesize the ENTIRE discussion below into a single, self-contained BUILD PROMPT that a coding agent (such as Claude Code) can execute to actually build this idea.

Return ONLY the prompt itself, wrapped in one fenced ```markdown code block, ready to copy and paste. The prompt must:
- Open with the goal and the concrete deliverable in the first sentence.
- Fold in what the discussion SETTLED — the decisions, constraints, and disproofs — rather than restating the chat; extract, don't transcribe.
- Lay out an ordered plan: understand → design → implement → verify.
- Say explicitly where the agent should fan out parallel subagents or a workflow (independent modules, multi-angle review) versus work sequentially, and why.
- State the acceptance criteria and how to verify them.
Write it as direct instructions to the agent, specific and imperative — not prose about the idea.
{context}
