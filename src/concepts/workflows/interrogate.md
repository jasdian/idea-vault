---
name: interrogate
description: "Fan out diverse critics + a researcher, audit the findings, synthesize one position (the canonical D19 run-it-into-the-ground pass)"
use_when: "The idea is stated and you want every obvious attack run at once, checked, and folded into one position."
avoid_when: "The idea is still vague — steelman it first, or run steelman-then-attack."
stages:
  - kind: fan_out
    steps:
      - {role: critic, skill: premortem}
      - {role: critic, skill: cheapest-disproof}
      - {role: researcher, skill: constraints}
      - {role: critic, skill: second-order-effects}
  - kind: audit
  - kind: synthesize
---

The canonical D19 pass: four independent lenses attack the idea in parallel, the factored audit
labels what they found CONFIRMED, UNCERTAIN or REFUTED, and the synthesizer converges the
survivors into one position.
