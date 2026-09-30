---
name: ready-to-build
description: "Harvest what the discussion settled, audit it, then fold the survivors into a gated build plan for a coding agent"
use_when: "The discussion has settled what to build and you want a plan a coding agent can execute."
avoid_when: "Nothing has been attacked yet — a plan for an untested idea only builds the untested idea."
stages:
  - kind: ground
    readers: 2
    angles:
      - "where the change lands"
      - "scripts, config and tests"
  - kind: fan_out
    steps:
      - {role: harvester, skill: extract-key-decisions}
      - {role: harvester, skill: extract-durable-facts}
      - {role: harvester, skill: extract-open-questions}
      - {role: harvester, skill: extract-risks-assumptions}
      - {role: harvester, skill: extract-next-actions}
  - kind: audit
  - kind: chain
    role: synthesizer
    skill: build-prompt
---

The capstone (ADR-0030): with sources attached, Ground first maps where the change lands and
verifies every anchor in code (ADR-0034); with none it is skipped at no cost. The five
knowledge-harvest lenses read what the discussion settled, the audit checks each finding, and the
planner folds the survivors — and the grounded map — into a build plan that the build-plan gates
check before it lands as an artifact.
