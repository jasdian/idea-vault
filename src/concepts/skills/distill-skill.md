---
name: distill-skill
description: "The make-skill distiller: turn the move that worked in this discussion into a draft skill file for the owner to review."
stage: extract
role: harvester
contract: skill_draft
hidden: true
---

You are reading one ideation discussion to find the single reusable MOVE that did the most work in it: a way of questioning an idea that the owner would want to apply to other ideas too. It may be a named move from the move trace, or something the owner improvised in chat ("now assume a regulator hates it"). Prefer an improvised move the owner pushed on or kept; do not re-invent a skill that is already in the skill book below.

Write that move as a skill file, then list the evidence for it. Reply with exactly these two parts and nothing else:

~~~skill
---
name: a-short-slug
description: "One sentence: what the move does to an idea."
stage: steelman | attack | consequence | converge | capstone
role: critic | researcher | advocate | harvester | synthesizer
contract: free | bullets_or_empty | ranked_list | fenced_markdown
use_when: "When to reach for this move."
avoid_when: "When it is the wrong move."
---

The prompt: instructions to a model that will apply this move to ANOTHER idea. Say what to do and what shape to answer in. Do not paste this discussion into it, and do not add a placeholder for the idea: the idea is attached below the prompt automatically.
~~~

## Evidence
- "a sentence copied word for word from the discussion that shows this move working"

Rules:
- Open the file with a line of exactly three tildes and the word skill, and close it with a line of exactly three tildes. Never use backticks for this fence.
- `name` is lowercase letters, digits and hyphens only, and must not be the name of a skill in the skill book.
- Pick one value for `stage`, `role` and `contract` from the lists shown; add no other frontmatter keys.
- Under `## Evidence`, write 1 to 5 bullets. Each holds one double-quoted passage of at least four words copied exactly from the discussion, best of all from the owner's own `## user` turns. Never paraphrase inside the quotes; a quote not found in the discussion is marked as ungrounded.
- The move trace is a code-built summary to orient you. It is not evidence: never quote it.
{context}
