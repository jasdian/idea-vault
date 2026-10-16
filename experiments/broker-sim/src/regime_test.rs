//! Regime experiment: does per-asset regime switching add learnable depth to the solved
//! fill game? Base config is one that was KILLED in the baseline test (tape noise 0.3 ATR,
//! k = 2), so any pass comes from the regimes.
//!
//! Bots (all quote around the same EMA fair estimate, under the same 1-yell-per-tick budget):
//!   A  regime-blind Estimator ±x* (the solved strategy)
//!   H  human-style heuristic (momentum run + gap back-off), its window and threshold tuned
//!      on calibration days
//!   C  Bayesian regime filter on the tape (knows the catalog, not the state)
//!   R  regime oracle (told the true regime, not fair)
//!   Rp placebo: R reading another asset's regime labels
//!
//! Reading, fixed before running; every d is a mean daily-P&L gap / sd of A's daily P&L:
//!   d(R−A) < 1             → regimes are cosmetic (no decision attached)
//!   d(H−A) ≥ 1             → learnable depth; if also d(C−H) ≥ 1, bots still lead
//!   d(H−A) < 1 ≤ d(C−A)    → the depth belongs to statistical bots
//!   d(C−A) < 1 ≤ d(R−A)    → the value exists but cannot be detected (slot machine)
//!   |d(Rp−A)| ≥ 0.3        → placebo failed; the harness, not regime knowledge, makes the gap

use crate::bots::Estimator;
use crate::engine::{DayResult, Rules, optimal_offset};
use crate::market::{Observation, Tick};
use crate::regime::{Dynamics, Nature, RegimeParams, day};
use crate::regime_bots::{Heuristic, RegimeFilter, RegimeOracle};
use crate::rng::Rng;
use crate::stats::{Summary, paired};
use crate::{Args, CAL_TAG, EVAL_TAG, calibrate, play};

const DURATIONS: [f64; 4] = [15.0, 45.0, 180.0, 720.0];
const DAY_TICKS: usize = 180;
const TAPE_NOISE_ATR: f64 = 0.3;
const K: f64 = 2.0;

fn natures() -> Vec<Nature> {
    vec![
        // GOLD: mostly calm.
        Nature {
            start: 1800.0,
            sigma: 0.40,
            home: [0.5, 0.15, 0.15, 0.1, 0.1],
        },
        // OIL: trendy and volatile.
        Nature {
            start: 80.0,
            sigma: 0.05,
            home: [0.15, 0.2, 0.2, 0.3, 0.15],
        },
        // TECH: news-driven.
        Nature {
            start: 150.0,
            sigma: 0.12,
            home: [0.1, 0.2, 0.2, 0.15, 0.35],
        },
    ]
}

fn dynamics() -> Dynamics {
    Dynamics {
        calm_vol: 0.6,
        calm_reversion: 0.05,
        trend_drift: 0.25,
        volatile_vol: 2.5,
        jump_prob: 0.04,
        jump_size: 8.0,
    }
}

fn params(mean_duration: f64, shuffle_natures: bool) -> RegimeParams {
    RegimeParams {
        natures: natures(),
        dynamics: dynamics(),
        mean_duration,
        shuffle_natures,
        obs: Observation {
            warmup_ticks: 400,
            ticks: DAY_TICKS,
            bar_len: 10,
            atr_bars: 14,
            tape_noise_atr: TAPE_NOISE_ATR,
            informed_window: 5,
        },
    }
}

/// What the filter believes each asset's regime mix is: its own nature, or the average nature
/// when natures are reshuffled daily.
fn believed_homes(p: &RegimeParams) -> Vec<[f64; 5]> {
    let n = p.natures.len() as f64;
    let mean: [f64; 5] =
        std::array::from_fn(|j| p.natures.iter().map(|x| x.home[j]).sum::<f64>() / n);
    p.natures
        .iter()
        .map(|x| if p.shuffle_natures { mean } else { x.home })
        .collect()
}

fn heuristic(alpha: f64, window: usize, run_atr: f64) -> Heuristic {
    Heuristic {
        alpha,
        window,
        run_atr,
        gap_ticks: 5,
        gap_atr: 1.5,
        cool: 10,
    }
}

fn mean_pnl(days: impl Iterator<Item = DayResult>) -> f64 {
    let v: Vec<f64> = days.map(|r| r.pnl).collect();
    Summary::of(&v).mean
}

/// The heuristic's (window, run threshold) a practised player would settle on.
fn tune_heuristic(cal: &[Vec<Tick>], rules: &Rules, alpha: f64, seed: u64) -> (usize, f64) {
    [10, 20, 40]
        .into_iter()
        .flat_map(|w| [0.5, 1.0, 2.0].map(|r| (w, r)))
        .map(|(w, r)| {
            let h = heuristic(alpha, w, r);
            let m = mean_pnl(
                cal.iter()
                    .enumerate()
                    .map(|(d, m)| play(&h, m, rules, seed, CAL_TAG, d)),
            );
            ((w, r), m)
        })
        .fold(((20, 1.0), f64::MIN), |best, cur| {
            if cur.1 > best.1 { cur } else { best }
        })
        .0
}

struct Row {
    duration: f64,
    shuffle: bool,
    alpha: f64,
    h_params: (usize, f64),
    a: Summary,
    h: Summary,
    c: Summary,
    r: Summary,
    d_ra: f64,
    d_ha: f64,
    d_ca: f64,
    d_ch: f64,
    d_rpa: f64,
    accuracy: f64,
    /// Paired effect size (mean daily gap / sd of daily gaps) and share of days won, vs A.
    pair_h: (f64, f64),
    pair_c: (f64, f64),
}

impl Row {
    fn reading(&self) -> String {
        let core = if self.d_ra < 1.0 {
            "regimes cosmetic"
        } else if self.d_ha >= 1.0 && self.d_ch >= 1.0 {
            "learnable depth, bots lead"
        } else if self.d_ha >= 1.0 {
            "learnable depth"
        } else if self.d_ca >= 1.0 {
            "depth belongs to bots"
        } else {
            "value undetectable"
        };
        if self.d_rpa.abs() >= 0.3 {
            format!("{core} (placebo FAILED)")
        } else {
            core.to_string()
        }
    }
}

fn evaluate(duration: f64, shuffle: bool, a: &Args) -> Row {
    let p = params(duration, shuffle);
    let rules = Rules { k: K, limit: 10 };
    let x = optimal_offset(K);
    let market = |tag: u64, d: usize| day(&p, Rng::derive(a.seed, &[tag, d as u64]));
    let cal: Vec<Vec<Tick>> = (0..a.cal).map(|d| market(CAL_TAG, d)).collect();
    let alpha = calibrate(&cal, &rules, x, a.seed, true);
    let h_params = tune_heuristic(&cal, &rules, alpha, a.seed);
    let sigmas: Vec<f64> = p.natures.iter().map(|n| n.sigma).collect();
    let bot_a = Estimator {
        alpha,
        offset_atr: x,
        shown_atr: true,
    };
    let bot_h = heuristic(alpha, h_params.0, h_params.1);
    let bot_c = RegimeFilter {
        alpha,
        sigmas: sigmas.clone(),
        homes: believed_homes(&p),
        dynamics: dynamics(),
        mean_duration: duration,
        tape_noise_atr: TAPE_NOISE_ATR,
        block: 5,
    };
    let oracle = |label_shift| RegimeOracle {
        alpha,
        sigmas: sigmas.clone(),
        dynamics: dynamics(),
        mean_duration: duration,
        label_shift,
    };
    let (bot_r, bot_rp) = (oracle(0), oracle(1));
    let days: Vec<[f64; 6]> = (0..a.days)
        .map(|d| {
            let m = market(EVAL_TAG, d);
            let pnl = |r: DayResult| r.pnl;
            [
                pnl(play(&bot_a, &m, &rules, a.seed, EVAL_TAG, d)),
                pnl(play(&bot_h, &m, &rules, a.seed, EVAL_TAG, d)),
                pnl(play(&bot_c, &m, &rules, a.seed, EVAL_TAG, d)),
                pnl(play(&bot_r, &m, &rules, a.seed, EVAL_TAG, d)),
                pnl(play(&bot_rp, &m, &rules, a.seed, EVAL_TAG, d)),
                bot_c.map_accuracy(&m),
            ]
        })
        .collect();
    let col = |i: usize| Summary::of(&days.iter().map(|r| r[i]).collect::<Vec<_>>());
    let (sa, sh, sc, sr, srp) = (col(0), col(1), col(2), col(3), col(4));
    let d = |hi: &Summary, lo: &Summary| (hi.mean - lo.mean) / sa.sd;
    let pnl_col = |i: usize| days.iter().map(|r| r[i]).collect::<Vec<_>>();
    let pair = |i: usize| {
        let (x, y) = (pnl_col(i), pnl_col(0));
        let gaps: Vec<f64> = x.iter().zip(&y).map(|(p, q)| p - q).collect();
        let g = Summary::of(&gaps);
        (g.mean / g.sd, paired(&x, &y).win_rate)
    };
    Row {
        duration,
        shuffle,
        alpha,
        h_params,
        d_ra: d(&sr, &sa),
        d_ha: d(&sh, &sa),
        d_ca: d(&sc, &sa),
        d_ch: d(&sc, &sh),
        d_rpa: d(&srp, &sa),
        accuracy: col(5).mean,
        pair_h: pair(1),
        pair_c: pair(2),
        a: sa,
        h: sh,
        c: sc,
        r: sr,
    }
}

pub fn run(a: &Args) {
    println!(
        "# Regime test: per-asset natures and regime switching\n\n\
         seed {} · {} eval days · {} calibration days · 3 assets · {DAY_TICKS} ticks/day · \
         1 yell/tick · limit ±10 · tape noise {TAPE_NOISE_ATR} ATR · k = {K} · ATR shown\n\n\
         Regimes: Calm (0.6σ, reverts), TrendUp/Down (±0.25σ/tick), Volatile (2.5σ), Jumpy \
         (4%/tick 8σ jumps; informed fills priced at fair 5 ticks ahead). Natures: GOLD calm-heavy, \
         OIL trend/vol-heavy, TECH jumpy-heavy; \"shuffled\" reassigns them between assets daily.\n\n\
         d = gap in mean daily P&L / sd of A's daily P&L. Reading rules in src/regime_test.rs.\n",
        a.seed, a.days, a.cal
    );
    println!(
        "| mean regime (ticks) | (days) | natures | α | H (window, run ATR) | A mean±sd | H mean±sd | C mean±sd | R mean±sd | d(R−A) | d(H−A) | d(C−A) | d(C−H) | d(Rp−A) placebo | C regime accuracy | reading | paired d (H−A) | H>A days | paired d (C−A) | C>A days |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    DURATIONS
        .iter()
        .flat_map(|&dur| [false, true].map(|s| (dur, s)))
        .map(|(dur, s)| evaluate(dur, s, a))
        .for_each(|r| {
            println!(
                "| {:.0} | {:.2} | {} | {} | ({}, {}) | {:.1}±{:.1} | {:.1}±{:.1} | {:.1}±{:.1} | {:.1}±{:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0}% | {} | {:.2} | {:.0}% | {:.2} | {:.0}% |",
                r.duration,
                r.duration / DAY_TICKS as f64,
                if r.shuffle { "shuffled" } else { "fixed" },
                r.alpha,
                r.h_params.0,
                r.h_params.1,
                r.a.mean, r.a.sd, r.h.mean, r.h.sd, r.c.mean, r.c.sd, r.r.mean, r.r.sd,
                r.d_ra, r.d_ha, r.d_ca, r.d_ch, r.d_rpa,
                100.0 * r.accuracy,
                r.reading(),
                r.pair_h.0,
                100.0 * r.pair_h.1,
                r.pair_c.0,
                100.0 * r.pair_c.1
            )
        });
}
