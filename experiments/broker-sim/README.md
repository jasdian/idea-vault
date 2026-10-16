# broker-sim: kill test for the "Stock Market Returns" broker-pit game

This is a headless Rust sim for the game idea brainstormed in idea-vault (slug
`stock-market-returns-broker-pit-game`). It answers one question before any graphics get built:
**does the ATR fill model leave visible room for skill beyond the obvious heuristic?**

It's a standalone crate with its own `[workspace]` and no dependencies, and it isn't part of the
idea-vault build.

```bash
cargo test
cargo run --release -- [--days 2000] [--cal 300] [--seed 42] [--shown-atr]
```

## Model

| Piece | Rule |
|---|---|
| Fair value | Per-asset Gaussian random walk (σ per tick) plus news jumps (p = 0.005/tick, 10σ). Hidden. |
| System ATR | Wilder ATR(14) over 10-tick bars of fair value. |
| Tape | fair + N(0, `noise` × ATR). This is what an honest player sees. |
| Yell | One per tick (the attention budget). Asset, side and price. Inventory is limited to ±10 per asset. |
| Fill | Edge = (fair − bid)/ATR for a buy. Fill probability = 1/(1+e^{k·edge}). A fill executes at the yelled price. |
| P&L | Cash + inventory marked at close fair. It splits exactly into fill-time edge + inventory drift. |
| Day | 3 assets, 180 ticks (a 3-minute day at one yell per second). |

The market is generated before the day and fills never move it, so all bots replay the same day
(common random numbers). Every state transition is a pure fold with `Rng` threaded by value.

**Solved quote.** Expected edge per yell x·p(x) peaks at x\* = u/k, where e^u(u−1) = 1
(u ≈ 1.2785). Both skilled bots quote ±x\*.

| Bot | Sees | Quotes |
|---|---|---|
| Oracle | Hidden fair value and system ATR | fair ± x\*: the skill ceiling |
| Estimator | Tape only. Its EMA α is tuned on separate calibration days. | EMA ± x\*: the "solved the curve" player |
| Random | Tape | Random asset, side and edge in [−1, 2] ATR |

## Kill criterion (fixed before running)

d = (mean_Oracle − mean_Estimator) / sd(Estimator daily P&L). **If d < 1, KILL**: the extra edge
an expert can earn over the heuristic is smaller than one day's noise.

## Results (seed 42; seed 7 gives the same picture)

Full tables: [RESULTS.md](RESULTS.md) (Estimator measures ATR from the tape) and
[RESULTS-shown-atr.md](RESULTS-shown-atr.md) (the game shows the true ATR).

**2 of 9 configs pass when ATR is shown, and 3 of 9 when it isn't.** Every config passes only when
the tape is very noisy (1 ATR):

- **Tape noise ≤ 0.3 ATR: KILL at every k** (d = 0.02–0.64). An EMA of the prints recovers fair
  value almost perfectly, so quoting ±x\* earns nearly everything an oracle would. The game is
  solved once a player learns x\*.
- **Tape noise = 1 ATR, k = 2: the only usable pass** (d = 1.25, expert wins 88% of days, and a
  competent player still makes money: +12.9 against the oracle's +20.4).
- **Tape noise = 1 ATR, k = 4: passes, but the competent heuristic player loses money on average**
  (−1.8/day). That fits the swarm's "can't explain the loss" warning: the player does the
  sensible thing and loses anyway.
- The noise = 1 ATR, k = 1 pass in RESULTS.md is an artifact: tape noise inflates the measured ATR
  and the Estimator quotes too wide (4 fills a day). Once the true ATR is shown it fails (d = 0.48).
- **Random vs. competent is separated everywhere** (d = 4–18, 93–100% of days). The game teaches
  "don't be random". Beyond that it has no depth unless the tape is deliberately wrong by about 1
  ATR.

**Verdict:** the fill model as specified is a solved game under any honest tape. Skill depth only
appears when the core skill is "filter a very noisy tape", and only in a narrow k window. That
could be framed as a shouting, rumour-filled pit, but it's a design bet, not a free property of the
model.

## Not tested here

- **Humans.** This tests the bots' ceiling. It doesn't tell you whether people enjoy playing.
- **Attention triage across many assets.** The scheduler here is round-robin, so the one
  candidate source of fun is still unmeasured.
- **Probe spam.** Rejected yells cost nothing here. A per-yell cost or cooldown would change x\*.
- **Adverse selection, NPC counterparties, partial fills.** These are the swarm's candidate fixes
  for the solved optimum. Each is a strategy or engine change that can be compared with the same
  criterion.
