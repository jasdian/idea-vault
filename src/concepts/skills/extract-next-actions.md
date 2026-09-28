---
name: extract-next-actions
description: "Harvest the concrete next actions the discussion pointed to."
stage: extract
role: harvester
contract: bullets_or_empty
hidden: true
---

From the discussion below, harvest ONLY the concrete next actions the discussion pointed to — experiments to run, people to ask, things to build or measure. As markdown bullets, one action per bullet, imperative form. If the discussion pointed to no actions, output nothing.
{context}
