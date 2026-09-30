---
name: design-panel
description: "Ground the idea in the attached code, then have three proposers compete on a weighted rubric; audit the proposals and graft the runners-up's best parts into the winner"
use_when: "There are several credible ways to build this and you want them compared on cost, risk, fit and evidence rather than argued."
avoid_when: "The idea is still unstated or untested — attack it first; a panel only ranks ways to do something already worth doing."
stages:
  - kind: ground
    readers: 2
  - kind: panel
    proposers:
      - {role: advocate, skill: steelman}
      - {role: critic, skill: cheapest-disproof}
      - {role: researcher, angle: "the smallest version that ships this week"}
    criteria:
      - {name: cost, weight: 2, zero: "weeks of work or new infrastructure", two: "an afternoon on what already exists"}
      - {name: risk, weight: 2, zero: "a wrong guess is expensive to undo", two: "easy to undo or already de-risked"}
      - {name: fit, weight: 1, zero: "fights the code or the owner's stated constraints", two: "fits both as they stand"}
      - {name: evidence, weight: 1, zero: "rests on assumptions nobody checked", two: "rests on verified anchors or the owner's own words"}
    judges: 1
  - kind: audit
  - kind: synthesize
---

The design panel (ADR-0034): Ground maps the attached sources and verifies every cited anchor in
code (skipped with no call when the idea has no sources). Three proposers then answer from their
own angle — the strongest version, the cheapest disproof-first path, and the smallest thing that
ships this week — and each proposal is scored cold, alone, against the rubric. Code picks the
winner and the grafts; the audit checks the proposals; the synthesizer takes the winner as the
spine and adds only the listed grafts.
