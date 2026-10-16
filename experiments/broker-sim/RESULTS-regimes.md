# Regime test: per-asset natures and regime switching

seed 42 · 2000 eval days · 300 calibration days · 3 assets · 180 ticks/day · 1 yell/tick · limit ±10 · tape noise 0.3 ATR · k = 2 · ATR shown

Regimes: Calm (0.6σ, reverts), TrendUp/Down (±0.25σ/tick), Volatile (2.5σ), Jumpy (4%/tick 8σ jumps; informed fills priced at fair 5 ticks ahead). Natures: GOLD calm-heavy, OIL trend/vol-heavy, TECH jumpy-heavy; "shuffled" reassigns them between assets daily.

d = gap in mean daily P&L / sd of A's daily P&L. Reading rules in src/regime_test.rs.

| mean regime (ticks) | (days) | natures | α | H (window, run ATR) | A mean±sd | H mean±sd | C mean±sd | R mean±sd | d(R−A) | d(H−A) | d(C−A) | d(C−H) | d(Rp−A) placebo | C regime accuracy | reading | paired d (H−A) | H>A days | paired d (C−A) | C>A days |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 15 | 0.08 | fixed | 0.6 | (10, 2) | 18.5±7.5 | 18.4±8.0 | 20.9±31.7 | 35.6±32.4 | 2.27 | -0.02 | 0.32 | 0.34 | -1.22 | 36% | value undetectable (placebo FAILED) | -0.02 | 51% | 0.08 | 45% |
| 15 | 0.08 | shuffled | 0.6 | (10, 2) | 19.8±8.5 | 19.7±8.8 | 20.8±33.1 | 34.2±34.1 | 1.70 | -0.01 | 0.12 | 0.13 | -1.11 | 32% | value undetectable (placebo FAILED) | -0.01 | 50% | 0.03 | 45% |
| 45 | 0.25 | fixed | 0.6 | (20, 0.5) | 18.1±8.2 | 14.0±40.1 | 28.8±50.7 | 53.0±50.5 | 4.24 | -0.50 | 1.30 | 1.80 | -2.66 | 47% | depth belongs to bots (placebo FAILED) | -0.10 | 36% | 0.21 | 51% |
| 45 | 0.25 | shuffled | 0.6 | (10, 2) | 19.4±8.8 | 19.4±9.4 | 30.1±53.5 | 52.8±54.7 | 3.77 | -0.01 | 1.21 | 1.21 | -2.43 | 44% | depth belongs to bots (placebo FAILED) | -0.01 | 51% | 0.20 | 52% |
| 180 | 1.00 | fixed | 0.6 | (20, 0.5) | 17.9±9.3 | 31.9±55.9 | 51.0±74.0 | 78.5±66.5 | 6.54 | 1.52 | 3.57 | 2.06 | -2.38 | 61% | learnable depth, bots lead (placebo FAILED) | 0.25 | 50% | 0.45 | 63% |
| 180 | 1.00 | shuffled | 0.6 | (20, 0.5) | 19.7±10.7 | 32.7±57.1 | 49.0±76.8 | 76.7±65.6 | 5.34 | 1.22 | 2.75 | 1.53 | -2.49 | 59% | learnable depth, bots lead (placebo FAILED) | 0.23 | 50% | 0.38 | 61% |
| 720 | 4.00 | fixed | 0.6 | (40, 1) | 17.5±10.1 | 38.9±61.6 | 62.5±80.3 | 85.2±71.4 | 6.68 | 2.11 | 4.44 | 2.33 | -2.09 | 70% | learnable depth, bots lead (placebo FAILED) | 0.35 | 58% | 0.57 | 68% |
| 720 | 4.00 | shuffled | 0.6 | (20, 0.5) | 19.4±12.0 | 41.8±65.1 | 65.9±85.8 | 87.9±73.4 | 5.69 | 1.86 | 3.87 | 2.00 | -1.93 | 68% | learnable depth, bots lead (placebo FAILED) | 0.34 | 55% | 0.55 | 67% |
