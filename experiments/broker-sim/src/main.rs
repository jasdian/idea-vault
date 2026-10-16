//! Kill test for the broker-pit game's ATR fill model.
//!
//! Pre-registered criterion (set before any run): for each market config,
//!   d = (mean daily P&L of Oracle − mean of Estimator) / sd of Estimator's daily P&L.
//! If d < 1, the edge an expert gains over the "solved curve" heuristic is smaller than one
//! day's noise: skill is invisible within a day and the core loop is KILLED as designed.
//!
//!   cargo run --release -- [--days N] [--cal N] [--seed S] [--shown-atr]
//!
//! `--shown-atr`: the game displays the true ATR, so the Estimator scales its offset by it
//! instead of by an ATR measured off the (noisy) tape.

mod bots;
mod engine;
mod market;
mod rng;
mod stats;

use bots::{Estimator, Oracle, Random};
use engine::{DayResult, Rules, Strategy, optimal_offset, run_day};
use market::{AssetSpec, MarketParams, Tick, day};
use rng::Rng;
use stats::{Summary, paired};

const ALPHAS: [f64; 7] = [0.03, 0.06, 0.1, 0.2, 0.35, 0.6, 1.0];
const CAL_TAG: u64 = 1;
const EVAL_TAG: u64 = 2;

struct Args {
    days: usize,
    cal: usize,
    seed: u64,
    shown_atr: bool,
}

fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let get = |flag: &str, default: u64| {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|_| panic!("{flag} takes an integer"))
            })
            .unwrap_or(default)
    };
    Args {
        days: get("--days", 2000) as usize,
        cal: get("--cal", 300) as usize,
        seed: get("--seed", 42),
        shown_atr: argv.iter().any(|x| x == "--shown-atr"),
    }
}

fn assets() -> Vec<AssetSpec> {
    vec![
        AssetSpec {
            start: 1800.0,
            sigma: 0.40,
        },
        AssetSpec {
            start: 80.0,
            sigma: 0.05,
        },
        AssetSpec {
            start: 150.0,
            sigma: 0.12,
        },
    ]
}

fn params(tape_noise_atr: f64) -> MarketParams {
    MarketParams {
        assets: assets(),
        ticks: 180, // a 3-minute day at one yell per second
        warmup_ticks: 400,
        bar_len: 10,
        atr_bars: 14,
        news_prob: 0.005,
        news_sigma_mult: 10.0,
        tape_noise_atr,
    }
}

fn market_for(p: &MarketParams, seed: u64, tag: u64, d: usize) -> Vec<Tick> {
    day(p, Rng::derive(seed, &[tag, d as u64]))
}

fn play<S: Strategy>(s: &S, m: &[Tick], rules: &Rules, seed: u64, tag: u64, d: usize) -> DayResult {
    let fills = Rng::derive(seed, &[tag, d as u64, 0xF111]);
    let own = Rng::derive(seed, &[tag, d as u64, 0x5747]);
    run_day(s, m, rules, fills, own)
}

/// Pick the Estimator's α on calibration days (disjoint seeds from evaluation), so the
/// Estimator is a competent tape reader, not a strawman.
fn calibrate(p: &MarketParams, rules: &Rules, x: f64, a: &Args) -> f64 {
    let markets: Vec<Vec<Tick>> = (0..a.cal)
        .map(|d| market_for(p, a.seed, CAL_TAG, d))
        .collect();
    ALPHAS
        .iter()
        .map(|&alpha| {
            let e = Estimator {
                alpha,
                offset_atr: x,
                shown_atr: a.shown_atr,
            };
            let mean = markets
                .iter()
                .enumerate()
                .map(|(d, m)| play(&e, m, rules, a.seed, CAL_TAG, d).pnl)
                .sum::<f64>()
                / a.cal as f64;
            (alpha, mean)
        })
        .fold((ALPHAS[0], f64::MIN), |best, cur| {
            if cur.1 > best.1 { cur } else { best }
        })
        .0
}

struct Row {
    noise: f64,
    k: f64,
    x: f64,
    alpha: f64,
    o: Summary,
    e: Summary,
    r: Summary,
    e_edge: Summary,
    e_fills: f64,
    d_oe: f64,
    win_oe: f64,
    d_er: f64,
    win_er: f64,
}

fn evaluate(noise: f64, k: f64, a: &Args) -> Row {
    let p = params(noise);
    let rules = Rules { k, limit: 10 };
    let x = optimal_offset(k);
    let alpha = calibrate(&p, &rules, x, a);
    let (oracle, est) = (
        Oracle { offset_atr: x },
        Estimator {
            alpha,
            offset_atr: x,
            shown_atr: a.shown_atr,
        },
    );
    let results: Vec<(DayResult, DayResult, DayResult)> = (0..a.days)
        .map(|d| {
            let m = market_for(&p, a.seed, EVAL_TAG, d);
            (
                play(&oracle, &m, &rules, a.seed, EVAL_TAG, d),
                play(&est, &m, &rules, a.seed, EVAL_TAG, d),
                play(&Random, &m, &rules, a.seed, EVAL_TAG, d),
            )
        })
        .collect();
    let col = |f: &dyn Fn(&(DayResult, DayResult, DayResult)) -> f64| -> Vec<f64> {
        results.iter().map(f).collect()
    };
    let (o, e, r) = (col(&|t| t.0.pnl), col(&|t| t.1.pnl), col(&|t| t.2.pnl));
    let (oe, er) = (paired(&o, &e), paired(&e, &r));
    let e_sum = Summary::of(&e);
    Row {
        noise,
        k,
        x,
        alpha,
        o: Summary::of(&o),
        r: Summary::of(&r),
        e_edge: Summary::of(&col(&|t| t.1.edge)),
        e_fills: Summary::of(&col(&|t| t.1.fills as f64)).mean,
        d_oe: oe.mean_diff / e_sum.sd,
        win_oe: oe.win_rate,
        d_er: er.mean_diff / e_sum.sd,
        win_er: er.win_rate,
        e: e_sum,
    }
}

fn main() {
    let a = parse_args();
    println!(
        "# Kill test: broker-pit ATR fill model\n\n\
         seed {} · {} eval days · {} calibration days · 3 assets · 180 ticks/day · 1 yell/tick · limit ±10 · ATR shown to player: {}\n\n\
         Criterion (pre-registered): d(O−E) = (mean_O − mean_E) / sd_E. **d < 1 → KILL.**\n",
        a.seed, a.days, a.cal, a.shown_atr
    );
    println!(
        "| tape noise (ATR) | k | x* (ATR) | est α | Oracle mean±sd | Estimator mean±sd | Est. edge-only sd | Random mean±sd | fills/day (E) | d(O−E) | O>E days | d(E−R) | E>R days | verdict |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let rows: Vec<Row> = [0.1, 0.3, 1.0]
        .iter()
        .flat_map(|&n| [1.0, 2.0, 4.0].map(|k| (n, k)))
        .map(|(n, k)| evaluate(n, k, &a))
        .collect();
    rows.iter().for_each(|r| {
        println!(
            "| {:.1} | {:.0} | {:.2} | {} | {:.1}±{:.1} | {:.1}±{:.1} | {:.1} | {:.1}±{:.1} | {:.0} | {:.2} | {:.0}% | {:.2} | {:.0}% | {} |",
            r.noise, r.k, r.x, r.alpha, r.o.mean, r.o.sd, r.e.mean, r.e.sd, r.e_edge.sd,
            r.r.mean, r.r.sd, r.e_fills, r.d_oe, 100.0 * r.win_oe, r.d_er, 100.0 * r.win_er,
            if r.d_oe < 1.0 { "KILL" } else { "pass" }
        )
    });
    let passed = rows.iter().filter(|r| r.d_oe >= 1.0).count();
    println!("\n**{passed}/{} configs pass.**", rows.len());
}
