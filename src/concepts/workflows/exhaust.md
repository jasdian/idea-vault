---
name: exhaust
description: "Attack the idea in rounds until a round finds nothing new, audit what was found, rework what the audit rejected, and synthesize"
use_when: "One pass of critics feels thin and you want the idea run into the ground until the objections stop being new."
avoid_when: "The idea is a sketch — a loop of attacks on a sketch only finds the gaps you already know."
stages:
  - kind: loop
    steps:
      - {role: critic, skill: premortem}
      - {role: critic, skill: devils-advocate}
      - {role: researcher, skill: second-order-effects}
    dry_rounds: 1
    max_rounds: 3
    max_calls: 12
  - kind: audit
  - kind: refine
    role: advocate
    skill: steelman
    max_rounds: 1
  - kind: synthesize
---

Run it into the ground, with a stop rule (ADR-0034): the three lenses attack in rounds, each
round told what is already found, until a round adds nothing new or the caps are hit. The audit
labels every distinct finding; the advocate reworks the refuted and uncertain ones by id and the
audit checks them again; the synthesizer converges what holds.
