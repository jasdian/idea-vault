---
parser: plan-gates
---
<!-- regrade:raw -->
Sure, here is the plan.

## Goal
Disprove the strategy cheaply before building.

## Settled
- S1: Disproof comes before any code.
  quote: "the cheapest disproof before any Rust exists"
- S2: The backtest uses five years of data.
  quote: "five years of tick data"

## Verify first
- none

## Open questions
- Q1: Which market do we backtest first?

## Plan
- [ ] T1: Write the spec with a dated kill criterion
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0

## Kill criteria
- K1: The backtest prints KILL → stop and report
  checked by: T1
  gates: T1
<!-- regrade:idea -->
A trading strategy to validate.
<!-- regrade:conversation -->
## user
I want the cheapest disproof before any Rust exists.

## assistant
Then write the kill criterion down first.
