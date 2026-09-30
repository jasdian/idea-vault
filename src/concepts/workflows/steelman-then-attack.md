---
name: steelman-then-attack
description: "Build the strongest case for the idea first, then send three critics at that steelman, audit what they find, and synthesize"
use_when: "The idea is young and an early attack would only hit its weakest phrasing."
avoid_when: "The idea has already been steelmanned and nothing about it has changed since."
stages:
  - kind: chain
    role: advocate
    skill: steelman
  - kind: fan_out
    steps:
      - {role: critic, skill: premortem}
      - {role: critic, skill: cheapest-disproof}
      - {role: critic, skill: devils-advocate}
  - kind: audit
  - kind: synthesize
---

The advocate's steelman is carried forward as a prior stage, so the three critics attack the
strongest version of the idea rather than its first draft.
