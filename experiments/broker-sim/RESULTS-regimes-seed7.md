# Regime test: per-asset natures and regime switching

seed 7 · 2000 eval days · 300 calibration days · 3 assets · 180 ticks/day · 1 yell/tick · limit ±10 · tape noise 0.3 ATR · k = 2 · ATR shown

Regimes: Calm (0.6σ, reverts), TrendUp/Down (±0.25σ/tick), Volatile (2.5σ), Jumpy (4%/tick 8σ jumps; informed fills priced at fair 5 ticks ahead). Natures: GOLD calm-heavy, OIL trend/vol-heavy, TECH jumpy-heavy; "shuffled" reassigns them between assets daily.

d = gap in mean daily P&L / sd of A's daily P&L. Reading rules in src/regime_test.rs.

| mean regime (ticks) | (days) | natures | α | H (window, run ATR) | A mean±sd | H mean±sd | C mean±sd | R mean±sd | d(R−A) | d(H−A) | d(C−A) | d(C−H) | d(Rp−A) placebo | C regime accuracy | reading | paired d (H−A) | H>A days | paired d (C−A) | C>A days |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 15 | 0.08 | fixed | 0.6 | (10, 2) | 18.4±7.4 | 18.5±7.8 | 19.2±31.6 | 37.0±33.3 | 2.51 | 0.02 | 0.12 | 0.10 | -1.36 | 36% | value undetectable (placebo FAILED) | 0.02 | 51% | 0.03 | 45% |
| 15 | 0.08 | shuffled | 0.6 | (10, 2) | 19.6±8.5 | 19.7±9.0 | 21.8±33.2 | 36.5±33.8 | 1.99 | 0.01 | 0.26 | 0.25 | -1.26 | 32% | value undetectable (placebo FAILED) | 0.01 | 51% | 0.07 | 47% |
| 45 | 0.25 | fixed | 0.6 | (10, 2) | 18.0±8.2 | 18.1±8.6 | 28.0±51.8 | 52.3±51.7 | 4.20 | 0.01 | 1.23 | 1.22 | -2.65 | 46% | depth belongs to bots (placebo FAILED) | 0.01 | 52% | 0.20 | 52% |
| 45 | 0.25 | shuffled | 0.6 | (10, 2) | 19.2±9.0 | 19.1±9.2 | 29.0±56.6 | 51.4±55.6 | 3.57 | -0.01 | 1.08 | 1.10 | -2.50 | 43% | depth belongs to bots (placebo FAILED) | -0.01 | 51% | 0.18 | 50% |
| 180 | 1.00 | fixed | 0.6 | (20, 0.5) | 17.5±9.7 | 33.8±53.2 | 49.5±72.7 | 77.3±64.8 | 6.14 | 1.67 | 3.29 | 1.61 | -2.77 | 60% | learnable depth, bots lead (placebo FAILED) | 0.31 | 53% | 0.44 | 62% |
| 180 | 1.00 | shuffled | 0.6 | (20, 0.5) | 19.2±10.6 | 34.8±58.5 | 50.6±77.7 | 76.9±68.8 | 5.43 | 1.47 | 2.96 | 1.49 | -2.58 | 59% | learnable depth, bots lead (placebo FAILED) | 0.27 | 53% | 0.41 | 61% |
| 720 | 4.00 | fixed | 0.6 | (40, 0.5) | 17.1±10.3 | 37.8±63.2 | 60.3±77.5 | 82.5±69.2 | 6.32 | 2.00 | 4.17 | 2.17 | -2.02 | 70% | learnable depth, bots lead (placebo FAILED) | 0.33 | 57% | 0.56 | 67% |
| 720 | 4.00 | shuffled | 0.6 | (20, 0.5) | 19.2±11.8 | 40.4±61.9 | 62.3±83.0 | 85.2±69.5 | 5.62 | 1.81 | 3.67 | 1.86 | -2.06 | 68% | learnable depth, bots lead (placebo FAILED) | 0.34 | 57% | 0.52 | 65% |
