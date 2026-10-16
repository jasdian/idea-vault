//! Regime-switching market. Each asset has a *nature* (its own mix of regimes) and switches
//! regime at random (mean duration D ticks), drawing the next one from that mix. Optionally
//! natures are reshuffled between assets every day, so "GOLD is the calm one" cannot be
//! memorised.

use crate::market::{Observation, Path, Regime, Tick, assemble};
use crate::rng::Rng;

/// The switching regimes, in the index order used by every `[f64; 5]` below.
pub const CATALOG: [Regime; 5] = [
    Regime::Calm,
    Regime::TrendUp,
    Regime::TrendDown,
    Regime::Volatile,
    Regime::Jumpy,
];

pub const JUMPY: usize = 4;

#[derive(Clone, Debug)]
pub struct Nature {
    pub start: f64,
    /// Base per-tick σ in price units.
    pub sigma: f64,
    /// Long-run weight of each `CATALOG` regime for this asset.
    pub home: [f64; 5],
}

/// Per-regime fair-value dynamics, in units of the asset's σ.
#[derive(Clone, Debug)]
pub struct Dynamics {
    pub calm_vol: f64,
    /// Calm pulls fair back toward its level at regime entry by this fraction per tick.
    pub calm_reversion: f64,
    pub trend_drift: f64,
    pub volatile_vol: f64,
    pub jump_prob: f64,
    pub jump_size: f64,
}

impl Dynamics {
    pub fn vol(&self, r: Regime) -> f64 {
        match r {
            Regime::Calm => self.calm_vol,
            Regime::Volatile => self.volatile_vol,
            _ => 1.0,
        }
    }

    pub fn drift(&self, r: Regime) -> f64 {
        match r {
            Regime::TrendUp => self.trend_drift,
            Regime::TrendDown => -self.trend_drift,
            _ => 0.0,
        }
    }

    pub fn jump_prob(&self, r: Regime) -> f64 {
        match r {
            Regime::Jumpy => self.jump_prob,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RegimeParams {
    pub natures: Vec<Nature>,
    pub dynamics: Dynamics,
    pub mean_duration: f64,
    pub shuffle_natures: bool,
    pub obs: Observation,
}

/// P(next = j | now = i) for an asset whose regime mix is `home`.
pub fn transition(home: &[f64; 5], mean_duration: f64, i: usize, j: usize) -> f64 {
    let switch = 1.0 / mean_duration;
    if i == j {
        1.0 - switch
    } else {
        switch * home[j] / (1.0 - home[i])
    }
}

/// Index drawn from unnormalised `weights` by a uniform `u`.
fn pick(weights: &[f64], u: f64) -> usize {
    let target = u * weights.iter().sum::<f64>();
    weights
        .iter()
        .scan(0.0, |acc, &w| {
            *acc += w;
            Some(*acc)
        })
        .position(|c| target < c)
        .unwrap_or(weights.len() - 1)
}

fn path(n: &Nature, d: &Dynamics, mean_duration: f64, len: usize, rng: Rng) -> Path {
    let (u0, rng) = rng.uniform();
    let first = pick(&n.home, u0);
    let (fair, regime): (Vec<f64>, Vec<Regime>) = (0..len)
        .scan(
            (n.start, n.start, first, rng),
            |(fair, anchor, ri, r), _| {
                let (u_switch, r1) = r.uniform();
                let (u_pick, r2) = r1.uniform();
                let (z, r3) = r2.normal();
                let (u_jump, r4) = r3.uniform();
                let (j, r5) = r4.normal();
                *r = r5;
                if u_switch < 1.0 / mean_duration {
                    let others: Vec<f64> = (0..5)
                        .map(|k| if k == *ri { 0.0 } else { n.home[k] })
                        .collect();
                    *ri = pick(&others, u_pick);
                    *anchor = *fair;
                }
                let reg = CATALOG[*ri];
                let reversion = match reg {
                    Regime::Calm => d.calm_reversion * (*anchor - *fair),
                    _ => 0.0,
                };
                let jump = if u_jump < d.jump_prob(reg) {
                    j * d.jump_size * n.sigma
                } else {
                    0.0
                };
                *fair += d.drift(reg) * n.sigma + d.vol(reg) * n.sigma * z + reversion + jump;
                Some((*fair, reg))
            },
        )
        .unzip();
    Path { fair, regime }
}

/// Today's natures: as configured, or with the regime mixes permuted between assets
/// (each asset keeps its price level and σ).
fn natures_for_day(p: &RegimeParams, rng: Rng) -> Vec<Nature> {
    if !p.shuffle_natures {
        return p.natures.clone();
    }
    let n = p.natures.len();
    let (order, _) = (1..n)
        .rev()
        .fold(((0..n).collect::<Vec<_>>(), rng), |(o, r), i| {
            let (j, r) = r.below(i + 1);
            let swapped = o
                .iter()
                .enumerate()
                .map(|(k, &v)| match k {
                    k if k == i => o[j],
                    k if k == j => o[i],
                    _ => v,
                })
                .collect();
            (swapped, r)
        });
    p.natures
        .iter()
        .zip(&order)
        .map(|(own, &src)| Nature {
            home: p.natures[src].home,
            ..own.clone()
        })
        .collect()
}

/// One day of the regime market, deterministic in `(params, rng)`.
pub fn day(p: &RegimeParams, rng: Rng) -> Vec<Tick> {
    let len = p.obs.warmup_ticks + p.obs.ticks + p.obs.informed_window;
    let natures = natures_for_day(p, Rng::derive(rng.next_u64().0, &[0x5A]));
    let paths: Vec<Path> = natures
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let r = Rng::derive(rng.next_u64().0, &[i as u64]);
            path(n, &p.dynamics, p.mean_duration, len, r)
        })
        .collect();
    assemble(&paths, &p.obs, |i| {
        Rng::derive(rng.next_u64().0, &[i as u64, 1])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const HOME: [f64; 5] = [0.5, 0.15, 0.15, 0.1, 0.1];

    #[test]
    fn transition_rows_sum_to_one() {
        for i in 0..5 {
            let row: f64 = (0..5).map(|j| transition(&HOME, 45.0, i, j)).sum();
            assert!((row - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn regimes_switch_at_roughly_the_mean_duration() {
        let n = Nature {
            start: 100.0,
            sigma: 0.1,
            home: HOME,
        };
        let p = path(&n, &dynamics(), 50.0, 200_000, Rng::derive(9, &[]));
        let switches = p.regime.windows(2).filter(|w| w[0] != w[1]).count();
        let mean = 200_000.0 / switches as f64;
        assert!((mean - 50.0).abs() < 3.0, "mean duration {mean}");
    }

    #[test]
    fn trend_regimes_drift_their_way() {
        let only_up = Nature {
            start: 0.0,
            sigma: 1.0,
            home: [0.0, 1.0, 0.0, 0.0, 0.0],
        };
        let p = path(&only_up, &dynamics(), 1e12, 4000, Rng::derive(2, &[]));
        // 4000 ticks × 0.25σ drift = 1000σ against ~63σ of diffusion.
        assert!(p.fair[3999] > 800.0);
    }

    #[test]
    fn shuffle_permutes_homes_and_keeps_levels() {
        let natures = vec![
            Nature {
                start: 1.0,
                sigma: 1.0,
                home: [1.0, 0.0, 0.0, 0.0, 0.0],
            },
            Nature {
                start: 2.0,
                sigma: 2.0,
                home: [0.0, 1.0, 0.0, 0.0, 0.0],
            },
            Nature {
                start: 3.0,
                sigma: 3.0,
                home: [0.0, 0.0, 1.0, 0.0, 0.0],
            },
        ];
        let p = RegimeParams {
            natures,
            dynamics: dynamics(),
            mean_duration: 45.0,
            shuffle_natures: true,
            obs: Observation {
                warmup_ticks: 0,
                ticks: 1,
                bar_len: 1,
                atr_bars: 1,
                tape_noise_atr: 0.0,
                informed_window: 0,
            },
        };
        let moved = (0..50).any(|s| {
            let day = natures_for_day(&p, Rng::derive(s, &[]));
            let mut homes: Vec<usize> = day.iter().map(|n| pick(&n.home, 0.5)).collect();
            assert_eq!(
                day.iter().map(|n| n.start).collect::<Vec<_>>(),
                [1.0, 2.0, 3.0]
            );
            let changed = homes != [0, 1, 2];
            homes.sort();
            assert_eq!(homes, [0, 1, 2]);
            changed
        });
        assert!(moved);
    }
}
