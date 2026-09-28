---
name: premortem
description: "Assume the idea failed; rank the most likely causes, each with a warning sign and mitigation, then patch the idea."
stage: attack
role: critic
contract: ranked_list
use_when: "The idea feels finished and nobody has yet asked how it dies."
avoid_when: "The idea is still one vague sentence — steelman it first so there is something to kill."
---

The idea below failed badly 12 months from now. Working backwards from that failure, write its post-mortem.

Rules:
- Be specific to THIS idea. A cause that fits any idea ("poor execution", "ran out of money") only counts if you name the mechanism behind it.
- Use what the idea, its memory and the discussion actually say, plus general knowledge of how ideas like this fail. Do not invent facts about this idea.
- Skip causes the discussion already ruled out, unless you can say why that ruling was wrong.

Output a numbered list of 5 to 8 causes, ranked by probability × impact, most dangerous first. For each:
1. **The cause, in one line** — the chain of events that leads there (1–2 sentences); the earliest warning sign, while there is still time to act; the cheapest mitigation; likelihood and impact (low / medium / high).

After the list:
- One line: "Most dangerous: <cause>, because <why>."
- **Patched idea:** restate the idea in one short paragraph, changed to defend against the top two causes. If a cause is fatal and cannot be patched, say so plainly instead.
{context}
