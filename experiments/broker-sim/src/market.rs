//! The market for one trading day, generated up front as an immutable tick series.
//!
//! Fills never move the market (the player is a price-taker against a hidden fair value), so
//! the same `Vec<Tick>` can be replayed against every strategy: common random numbers.

use crate::rng::Rng;

#[derive(Clone, Debug)]
pub struct AssetSpec {
    pub start: f64,
    /// Per-tick standard deviation of the fair-value random walk, in price units.
    pub sigma: f64,
}

#[derive(Clone, Debug)]
pub struct MarketParams {
    pub assets: Vec<AssetSpec>,
    /// Tradable ticks in one day (one tick ≈ one yell of attention).
    pub ticks: usize,
    /// Untraded ticks before the open so ATR has history.
    pub warmup_ticks: usize,
    /// Ticks per ATR bar.
    pub bar_len: usize,
    /// Wilder smoothing length, in bars.
    pub atr_bars: usize,
    /// Per-tick, per-asset probability of a news jump.
    pub news_prob: f64,
    /// News jump standard deviation, as a multiple of `sigma`.
    pub news_sigma_mult: f64,
    /// Tape print noise, in units of system ATR.
    pub tape_noise_atr: f64,
}

/// The hidden dynamics an asset is in at one tick. The baseline market is a single `Walk`;
/// the regime market (`crate::regime`) switches between the other five.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    Walk,
    Calm,
    TrendUp,
    TrendDown,
    Volatile,
    Jumpy,
}

/// What exists at one tick. `fair`, `atr_sys`, `fill_fair` and `regime` are the system's hidden
/// truth; `tape` and `atr_tape` are what an honest player can see. `fill_fair` is the value the
/// fill curve is evaluated against: `fair`, except where informed counterparties are present,
/// when it is the fair value a few ticks ahead (they pick off stale quotes).
#[derive(Clone, Debug)]
pub struct Tick {
    pub fair: Vec<f64>,
    pub atr_sys: Vec<f64>,
    pub tape: Vec<f64>,
    pub atr_tape: Vec<f64>,
    pub fill_fair: Vec<f64>,
    pub regime: Vec<Regime>,
}

fn fair_path(spec: &AssetSpec, p: &MarketParams, n: usize, rng: Rng) -> Vec<f64> {
    (0..n)
        .scan((spec.start, rng), |(price, r), _| {
            let (z, r1) = r.normal();
            let (u, r2) = r1.uniform();
            let (j, r3) = r2.normal();
            let jump = if u < p.news_prob {
                j * spec.sigma * p.news_sigma_mult
            } else {
                0.0
            };
            *price += z * spec.sigma + jump;
            *r = r3;
            Some(*price)
        })
        .collect()
}

/// Wilder ATR over `bar_len`-tick bars, as known at each tick: the value after the last
/// *completed* bar (seeded with the first bar's true range).
pub fn atr_series(path: &[f64], bar_len: usize, atr_bars: usize) -> Vec<f64> {
    let bars: Vec<(f64, f64, f64)> = path
        .chunks(bar_len)
        .map(|c| {
            let hi = c.iter().copied().fold(f64::MIN, f64::max);
            let lo = c.iter().copied().fold(f64::MAX, f64::min);
            (hi, lo, c[c.len() - 1])
        })
        .collect();
    let n = atr_bars as f64;
    let atr_after_bar: Vec<f64> = bars
        .iter()
        .enumerate()
        .scan(None::<f64>, |atr, (i, &(hi, lo, _))| {
            let tr = match i.checked_sub(1).map(|j| bars[j].2) {
                Some(pc) => (hi - lo).max((hi - pc).abs()).max((lo - pc).abs()),
                None => hi - lo,
            };
            let next = atr.map_or(tr, |a| (a * (n - 1.0) + tr) / n);
            *atr = Some(next);
            Some(next)
        })
        .collect();
    (0..path.len())
        .map(|t| atr_after_bar[(t / bar_len).saturating_sub(1)].max(f64::EPSILON))
        .collect()
}

/// One asset's generated truth over warmup + day + look-ahead ticks.
pub struct Path {
    pub fair: Vec<f64>,
    pub regime: Vec<Regime>,
}

/// How generated paths become observable ticks.
#[derive(Clone, Debug)]
pub struct Observation {
    pub warmup_ticks: usize,
    pub ticks: usize,
    pub bar_len: usize,
    pub atr_bars: usize,
    pub tape_noise_atr: f64,
    /// Ticks of look-ahead informed counterparties have while an asset is `Jumpy`.
    pub informed_window: usize,
}

/// One asset's whole-day columns, before transposing into ticks.
struct Series {
    fair: Vec<f64>,
    atr_sys: Vec<f64>,
    tape: Vec<f64>,
    atr_tape: Vec<f64>,
    fill_fair: Vec<f64>,
    regime: Vec<Regime>,
}

fn series(path: &Path, o: &Observation, tape_rng: Rng) -> Series {
    let atr_sys = atr_series(&path.fair, o.bar_len, o.atr_bars);
    let tape: Vec<f64> = path
        .fair
        .iter()
        .zip(&atr_sys)
        .scan(tape_rng, |r, (&f, &a)| {
            let (z, r1) = r.normal();
            *r = r1;
            Some(f + z * o.tape_noise_atr * a)
        })
        .collect();
    let atr_tape = atr_series(&tape, o.bar_len, o.atr_bars);
    let last = path.fair.len() - 1;
    let fill_fair = path
        .regime
        .iter()
        .enumerate()
        .map(|(t, r)| match r {
            Regime::Jumpy => path.fair[(t + o.informed_window).min(last)],
            _ => path.fair[t],
        })
        .collect();
    Series {
        fair: path.fair.clone(),
        atr_sys,
        tape,
        atr_tape,
        fill_fair,
        regime: path.regime.clone(),
    }
}

/// Observe generated paths (one per asset, each at least warmup + ticks long) as the day's
/// tradable ticks; `tape_rng(i)` seeds asset i's print noise.
pub fn assemble(paths: &[Path], o: &Observation, tape_rng: impl Fn(usize) -> Rng) -> Vec<Tick> {
    let cols: Vec<Series> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| series(p, o, tape_rng(i)))
        .collect();
    let col = |t: usize, f: fn(&Series) -> &Vec<f64>| cols.iter().map(|c| f(c)[t]).collect();
    (o.warmup_ticks..o.warmup_ticks + o.ticks)
        .map(|t| Tick {
            fair: col(t, |c| &c.fair),
            atr_sys: col(t, |c| &c.atr_sys),
            tape: col(t, |c| &c.tape),
            atr_tape: col(t, |c| &c.atr_tape),
            fill_fair: col(t, |c| &c.fill_fair),
            regime: cols.iter().map(|c| c.regime[t]).collect(),
        })
        .collect()
}

/// One day of the baseline market (a single random-walk-with-news regime), deterministic in
/// `(params, rng)`.
pub fn day(p: &MarketParams, rng: Rng) -> Vec<Tick> {
    let total = p.warmup_ticks + p.ticks;
    let paths: Vec<Path> = p
        .assets
        .iter()
        .enumerate()
        .map(|(i, spec)| Path {
            fair: fair_path(spec, p, total, Rng::derive(rng.next_u64().0, &[i as u64])),
            regime: vec![Regime::Walk; total],
        })
        .collect();
    let o = Observation {
        warmup_ticks: p.warmup_ticks,
        ticks: p.ticks,
        bar_len: p.bar_len,
        atr_bars: p.atr_bars,
        tape_noise_atr: p.tape_noise_atr,
        informed_window: 0,
    };
    assemble(&paths, &o, |i| {
        Rng::derive(rng.next_u64().0, &[i as u64, 1])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn params() -> MarketParams {
        MarketParams {
            assets: vec![AssetSpec {
                start: 100.0,
                sigma: 0.1,
            }],
            ticks: 100,
            warmup_ticks: 200,
            bar_len: 10,
            atr_bars: 14,
            news_prob: 0.01,
            news_sigma_mult: 10.0,
            tape_noise_atr: 0.3,
        }
    }

    #[test]
    fn day_is_deterministic_and_sized() {
        let a = day(&params(), Rng::derive(3, &[]));
        let b = day(&params(), Rng::derive(3, &[]));
        assert_eq!(a.len(), 100);
        assert_eq!(a[50].fair, b[50].fair);
        assert_eq!(a[50].tape, b[50].tape);
    }

    #[test]
    fn atr_of_flat_series_is_zero_ish_and_of_steps_is_step() {
        let flat = vec![5.0; 100];
        assert!(atr_series(&flat, 10, 14)[99] <= f64::EPSILON);
        // Alternating bars of 0 and 1: every bar after the first has true range 1, so Wilder
        // ATR (seeded at the first bar's 0) converges to 1 as 1 − (13/14)^bars.
        let steps: Vec<f64> = (0..2000).map(|t| ((t / 10) % 2) as f64).collect();
        assert!((atr_series(&steps, 10, 14)[1999] - 1.0).abs() < 1e-5);
    }
}
