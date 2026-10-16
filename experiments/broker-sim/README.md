# broker-sim: kill test for the "Stock Market Returns" broker-pit game

This is a headless Rust sim for the game idea brainstormed in idea-vault (slug
`stock-market-returns-broker-pit-game`). It answers one question before any graphics get built:
**does the ATR fill model leave visible room for skill beyond the obvious heuristic?**

It's a standalone crate with its own `[workspace]` and no dependencies, and it isn't part of the
idea-vault build.

```bash
cargo test
cargo run --release -- [--days 2000] [--cal 300] [--seed 42] [--shown-atr]
cargo run --release -- regimes [--days 2000] [--cal 300] [--seed 42]   # experiment 2, below
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

## Experiment 2: per-asset natures and regime switching

This tests the owner's follow-up: each asset has a different nature, and its regime changes
from time to time, which makes detection harder. Code is in `src/regime.rs`,
`src/regime_bots.rs` and `src/regime_test.rs`. Tables: [RESULTS-regimes.md](RESULTS-regimes.md)
(seed 42) and [RESULTS-regimes-seed7.md](RESULTS-regimes-seed7.md), which agrees.

**Market.** The base config is one the baseline killed: tape noise 0.3 ATR, k = 2, ATR shown.
Each asset switches between five regimes:

- **Calm:** 0.6σ, reverts to a level.
- **TrendUp / TrendDown:** ±0.25σ drift per tick.
- **Volatile:** 2.5σ.
- **Jumpy:** 4%/tick 8σ jumps. Fills are priced against fair 5 ticks ahead, so informed traders
  pick off stale quotes.

Each tick the regime switches with probability 1/D, and the next regime comes from the asset's
own mix: GOLD calm-heavy, OIL trend/volatile-heavy, TECH jumpy-heavy. "Shuffled" reassigns
those mixes between assets every day.

**Bots.** All quote around the same EMA fair estimate under the same yell budget. N, H, C and R
share one policy: each yell picks the side and offset with the best expected value given the
bot's expected move to the close (with zero expected move this is exactly A's quote, and a test
checks it), and they skip assets they believe are jumpy.

| Bot | Regime view |
|---|---|
| A | None: the solved ±x\* strategy |
| H | Human rule: a recent run continues; back off after a gap. Window and threshold are tuned on calibration days. |
| C | Bayesian HMM filter on the tape. Knows the catalog, σ, each asset's mix (the average mix when shuffled) and D, but not the state. |
| R | Told the true regime, not fair value |
| Rp | Placebo: R reading another asset's labels |

**Results (seed 42).** "d" is the pre-registered rule: gap ÷ sd of A's daily P&L. "Paired" is
the gap ÷ sd of the daily *differences*.

| Mean regime | d(R−A) | d(H−A) | d(C−A) | Paired d (H−A), H>A days | Paired d (C−A), C>A days | C regime accuracy |
|---|---|---|---|---|---|---|
| 15 ticks (1/12 day) | 1.7–2.3 | ≈0 | 0.1–0.3 | ≈0, 50% | ≈0.05, 45% | 32–36% |
| 45 ticks (1/4 day) | 3.8–4.2 | ≈0 | 1.2–1.3 | ≈0, 36–51% | 0.2, 51–52% | 44–47% |
| 180 ticks (1 day) | 5.3–6.5 | 1.2–1.5 | 2.8–3.6 | 0.25, 50% | 0.4–0.45, 61–63% | 59–61% |
| 720 ticks (4 days) | 5.7–6.7 | 1.9–2.1 | 3.9–4.4 | 0.35, 55–58% | 0.55–0.57, 67–68% | 68–70% |

Ranges cover fixed and shuffled natures.

**What it shows:**

- **Regimes carry real decisions.** The oracle beats A by 1.7–6.7 d at every D, so trends and
  informed jumps do change the best quote.
- **Harder detection makes the game shallower at every step, not better.** It never peaks in
  the middle. At 1/12 day nobody can detect the regime and the value is lost (a slot machine).
  At 1/4 day only the Bayesian filter gets anything, so the depth belongs to bots. The human
  rule reaches d ≥ 1 only when regimes last a day or longer, which is the easy-detection end.
  The filter beats the human rule by 1.1–2.3 d at every D where either one gains.
- **Per-asset natures add little.** Shuffling them daily costs only 0.2–0.8 d. Nearly all the
  value comes from reading the tape, not from a learned prior about each asset.
- **The day-scale verdict flips once variance is counted.** Regime play turns edge into
  directional inventory risk: the regime bots' daily sd is 5–8× A's. Judged on the same day's
  difference, the human rule beats A on only 50–58% of days, and even the Bayesian filter on
  61–68% (paired d ≤ 0.57). Paired d grows with √days, so the human rule would need roughly
  8–16 days and the filter 3–6 days to show at d ≥ 1. Skill shows over a week or a season,
  not within one day.
- **The placebo rule was miscalibrated.** The "|d(Rp−A)| ≥ 0.3 → FAILED" flag fires in every
  row, but always negative (−1.1 to −2.8). Acting on uninformative directional beliefs moves the
  quote off x\*, so the placebo can only lose, and the rule should have been one-sided
  (d(Rp−A) ≥ 0.3). No row shows a positive placebo, so nothing suggests the harness creates the
  gaps.

**Verdict.** Regime switching does create learnable depth, but only with regimes that last a
day or more, and with a statistical filter always ahead of any human rule. The result also
comes with a cost the baseline didn't have: single days are dominated by luck on trend calls,
which brings back the "can't explain my loss" warning. If this direction is kept, scoring has
to span many days (a week or season, not one day), and regime evidence should reach players as
discrete cues rather than tape statistics, so humans aren't competing with an HMM.

## Not tested here

- **Humans.** This tests the bots' ceiling. It doesn't tell you whether people enjoy playing.
- **Attention triage across many assets.** Every bot uses round-robin attention (the regime
  bots only skip assets they consider jumpy), so this is still unmeasured.
- **Probe spam.** Rejected yells cost nothing here. A per-yell cost or cooldown would change x\*.
- **Telegraphed regime cues** (headlines, NPC tells), multi-day scoring, partial fills. These
  are the next candidates. Each is a market or engine change that can be compared with the same
  criteria.
