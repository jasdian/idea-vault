//! Regime-aware bots for the regime experiment. All use the regime-blind Estimator's EMA as
//! their fair estimate and one shared policy (`regime_yell`); they differ only in how they form
//! a `View` of each asset:
//!
//! - `RegimeOracle` is told the true regime (or, as a placebo, another asset's).
//! - `RegimeFilter` runs a Bayesian HMM filter over tape returns. It knows the regime catalog,
//!   each asset's σ and regime mix, and the switch rate: the best a learnable player could do.
//! - `Heuristic` uses a rule a person could follow: "a big recent run continues for a while;
//!   after a gap, back off".

use crate::bots::{ema, lean, quote};
use crate::engine::{Account, Obs, Side, Strategy, Yell, best_offset, fill_prob};
use crate::market::Tick;
use crate::regime::{CATALOG, Dynamics, JUMPY, transition};
use crate::rng::Rng;

/// A player's belief about one asset right now.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct View {
    /// Expected fair-value move (price units) between now and the close.
    pub expected_move: f64,
    /// Don't quote this asset (informed traders are likely about).
    pub avoid: bool,
}

/// Expected cumulative move over `h` ticks of a per-tick drift that survives each tick with
/// probability `persist`.
pub fn persisted(drift: f64, persist: f64, h: usize) -> f64 {
    if persist >= 1.0 {
        drift * h as f64
    } else {
        drift * (1.0 - persist.powi(h as i32)) / (1.0 - persist)
    }
}

/// Round-robin attention, skipping avoided assets. On the chosen asset, yell the side and
/// offset with the highest expected value given the view; with no expected move this is
/// exactly the Estimator's ±x* lean quote.
pub fn regime_yell(obs: &Obs, acct: &Account, est: &[f64], views: &[View]) -> Option<Yell> {
    let n = acct.inv.len();
    let (k, limit) = (obs.rules.k, obs.rules.limit);
    (0..n)
        .map(|i| (obs.t + i) % n)
        .filter(|&a| !views[a].avoid)
        .find_map(|a| {
            let atr = obs.tick.atr_sys[a];
            let m = views[a].expected_move / atr;
            let q = acct.inv[a];
            let allowed = |s: &Side| (q + s.sign() as i64).abs() <= limit;
            let ev = |s: Side| {
                let ms = s.sign() * m;
                let e = best_offset(ms, k);
                (s, e, (ms + e) * fill_prob(e, k))
            };
            let pick = if m == 0.0 {
                Some(ev(lean(obs.t, n, q, limit)))
            } else {
                [Side::Buy, Side::Sell]
                    .into_iter()
                    .filter(allowed)
                    .map(ev)
                    .max_by(|x, y| x.2.total_cmp(&y.2))
            };
            pick.map(|(side, e, _)| Yell {
                asset: a,
                side,
                price: quote(side, est[a], e, atr),
            })
        })
}

/// Knows each asset's true regime (but not fair). `label_shift = 1` is the placebo: it reads
/// the regime of the *next* asset, which is independent of this one's.
pub struct RegimeOracle {
    pub alpha: f64,
    pub sigmas: Vec<f64>,
    pub dynamics: Dynamics,
    pub mean_duration: f64,
    pub label_shift: usize,
}

impl Strategy for RegimeOracle {
    type State = Option<Vec<f64>>;
    fn init(&self, _: usize, _: Rng) -> Self::State {
        None
    }
    fn act(&self, s: &Self::State, obs: &Obs, acct: &Account) -> (Self::State, Option<Yell>) {
        let est = ema(s, &obs.tick.tape, self.alpha);
        let n = est.len();
        let persist = 1.0 - 1.0 / self.mean_duration;
        let views: Vec<View> = (0..n)
            .map(|a| {
                let r = obs.tick.regime[(a + self.label_shift) % n];
                View {
                    expected_move: persisted(
                        self.dynamics.drift(r) * self.sigmas[a],
                        persist,
                        obs.ticks_left,
                    ),
                    avoid: r == crate::market::Regime::Jumpy,
                }
            })
            .collect();
        let yell = regime_yell(obs, acct, &est, &views);
        (Some(est), yell)
    }
}

/// Bayesian forward filter over regimes, updated on `block`-tick tape returns.
pub struct RegimeFilter {
    pub alpha: f64,
    pub sigmas: Vec<f64>,
    /// Regime mix per asset as the player believes it (the average mix when natures are
    /// shuffled daily, since the player cannot know today's assignment).
    pub homes: Vec<[f64; 5]>,
    pub dynamics: Dynamics,
    pub mean_duration: f64,
    pub tape_noise_atr: f64,
    pub block: usize,
}

#[derive(Clone, Debug)]
pub struct FilterState {
    est: Option<Vec<f64>>,
    pub post: Vec<[f64; 5]>,
    anchor: Option<Vec<f64>>,
}

fn normalise(w: [f64; 5]) -> [f64; 5] {
    let s: f64 = w.iter().sum();
    w.map(|x| x / s)
}

fn log_normal(x: f64, mean: f64, var: f64) -> f64 {
    -0.5 * ((x - mean).powi(2) / var + var.ln())
}

impl RegimeFilter {
    fn predict(&self, a: usize, post: &[f64; 5]) -> [f64; 5] {
        (0..self.block).fold(*post, |p, _| {
            normalise(std::array::from_fn(|j| {
                (0..5)
                    .map(|i| p[i] * transition(&self.homes[a], self.mean_duration, i, j))
                    .sum()
            }))
        })
    }

    fn update(&self, a: usize, post: &[f64; 5], ret: f64, atr: f64) -> [f64; 5] {
        let (d, sigma, b) = (&self.dynamics, self.sigmas[a], self.block as f64);
        let noise_var = 2.0 * (self.tape_noise_atr * atr).powi(2);
        let loglik: [f64; 5] = std::array::from_fn(|j| {
            let r = CATALOG[j];
            let var = b * (d.vol(r) * sigma).powi(2) + noise_var;
            let mean = b * d.drift(r) * sigma;
            let pj = 1.0 - (1.0 - d.jump_prob(r)).powf(b);
            let base = log_normal(ret, mean, var);
            if pj > 0.0 {
                let jumped = log_normal(ret, mean, var + (d.jump_size * sigma).powi(2));
                let hi = base.max(jumped);
                hi + ((1.0 - pj) * (base - hi).exp() + pj * (jumped - hi).exp()).ln()
            } else {
                base
            }
        });
        let prior = self.predict(a, post);
        let top = loglik.iter().copied().fold(f64::MIN, f64::max);
        normalise(std::array::from_fn(|j| prior[j] * (loglik[j] - top).exp()))
    }

    /// The filter's state after observing `tick` at time `t`.
    pub fn observe(&self, s: &FilterState, t: usize, tick: &Tick) -> FilterState {
        let est = Some(ema(&s.est, &tick.tape, self.alpha));
        match &s.anchor {
            None => FilterState {
                est,
                post: s.post.clone(),
                anchor: Some(tick.tape.clone()),
            },
            Some(anchor) if t.is_multiple_of(self.block) => FilterState {
                est,
                post: (0..anchor.len())
                    .map(|a| self.update(a, &s.post[a], tick.tape[a] - anchor[a], tick.atr_sys[a]))
                    .collect(),
                anchor: Some(tick.tape.clone()),
            },
            Some(_) => FilterState { est, ..s.clone() },
        }
    }

    pub fn fresh(&self) -> FilterState {
        FilterState {
            est: None,
            post: self.homes.clone(),
            anchor: None,
        }
    }

    /// Share of ticks where the filter's most likely regime is the true one.
    pub fn map_accuracy(&self, market: &[Tick]) -> f64 {
        let (_, hits) =
            market
                .iter()
                .enumerate()
                .fold((self.fresh(), 0usize), |(s, hits), (t, tick)| {
                    let s = self.observe(&s, t, tick);
                    let correct = s
                        .post
                        .iter()
                        .zip(&tick.regime)
                        .filter(|(p, r)| {
                            let map = (0..5).max_by(|&i, &j| p[i].total_cmp(&p[j])).unwrap_or(0);
                            CATALOG[map] == **r
                        })
                        .count();
                    (s, hits + correct)
                });
        hits as f64 / (market.len() * market.first().map_or(1, |t| t.regime.len())) as f64
    }
}

impl Strategy for RegimeFilter {
    type State = FilterState;
    fn init(&self, _: usize, _: Rng) -> FilterState {
        self.fresh()
    }
    fn act(&self, s: &FilterState, obs: &Obs, acct: &Account) -> (FilterState, Option<Yell>) {
        let s = self.observe(s, obs.t, obs.tick);
        let persist = 1.0 - 1.0 / self.mean_duration;
        let views: Vec<View> = s
            .post
            .iter()
            .zip(&self.sigmas)
            .map(|(p, &sigma)| View {
                expected_move: persisted(
                    (0..5)
                        .map(|j| p[j] * self.dynamics.drift(CATALOG[j]))
                        .sum::<f64>()
                        * sigma,
                    persist,
                    obs.ticks_left,
                ),
                avoid: p[JUMPY] > 0.5,
            })
            .collect();
        let yell = s
            .est
            .as_ref()
            .and_then(|est| regime_yell(obs, acct, est, &views));
        (s, yell)
    }
}

/// A rule a person could follow: if the last `window` ticks ran more than `run_atr` ATR one
/// way, expect the run to continue at that pace for another `window` ticks; after a
/// `gap_ticks` move of more than `gap_atr` ATR, stay off that asset for `cool` ticks.
pub struct Heuristic {
    pub alpha: f64,
    pub window: usize,
    pub run_atr: f64,
    pub gap_ticks: usize,
    pub gap_atr: f64,
    pub cool: usize,
}

#[derive(Clone, Debug)]
pub struct HeuristicState {
    est: Option<Vec<f64>>,
    /// The last `window + 1` prints per asset, oldest first.
    hist: Vec<Vec<f64>>,
    avoid_until: Vec<usize>,
}

impl Strategy for Heuristic {
    type State = HeuristicState;
    fn init(&self, n: usize, _: Rng) -> HeuristicState {
        HeuristicState {
            est: None,
            hist: vec![Vec::new(); n],
            avoid_until: vec![0; n],
        }
    }
    fn act(&self, s: &HeuristicState, obs: &Obs, acct: &Account) -> (HeuristicState, Option<Yell>) {
        let tick = obs.tick;
        let est = ema(&s.est, &tick.tape, self.alpha);
        let hist: Vec<Vec<f64>> = s
            .hist
            .iter()
            .zip(&tick.tape)
            .map(|(h, &p)| {
                let skip = (h.len() + 1).saturating_sub(self.window + 1);
                h.iter()
                    .copied()
                    .skip(skip)
                    .chain(std::iter::once(p))
                    .collect()
            })
            .collect();
        let back =
            |h: &[f64], lag: usize| h.len().checked_sub(lag + 1).map(|i| h[h.len() - 1] - h[i]);
        let avoid_until: Vec<usize> = hist
            .iter()
            .zip(&s.avoid_until)
            .zip(&tick.atr_sys)
            .map(|((h, &until), &atr)| match back(h, self.gap_ticks) {
                Some(gap) if gap.abs() > self.gap_atr * atr => obs.t + self.cool,
                _ => until,
            })
            .collect();
        let views: Vec<View> = hist
            .iter()
            .zip(&avoid_until)
            .zip(&tick.atr_sys)
            .map(|((h, &until), &atr)| View {
                expected_move: match back(h, self.window) {
                    Some(run) if run.abs() > self.run_atr * atr => {
                        run * self.window.min(obs.ticks_left) as f64 / self.window as f64
                    }
                    _ => 0.0,
                },
                avoid: obs.t < until,
            })
            .collect();
        let yell = regime_yell(obs, acct, &est, &views);
        (
            HeuristicState {
                est: Some(est),
                hist,
                avoid_until,
            },
            yell,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bots::Estimator;
    use crate::engine::{Rules, optimal_offset, run_day};
    use crate::market::{AssetSpec, MarketParams, day};

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

    #[test]
    fn regime_oracle_on_a_plain_walk_is_the_estimator() {
        let p = MarketParams {
            assets: vec![
                AssetSpec {
                    start: 100.0,
                    sigma: 0.1,
                },
                AssetSpec {
                    start: 50.0,
                    sigma: 0.05,
                },
            ],
            ticks: 180,
            warmup_ticks: 300,
            bar_len: 10,
            atr_bars: 14,
            news_prob: 0.005,
            news_sigma_mult: 10.0,
            tape_noise_atr: 0.3,
        };
        let rules = Rules { k: 2.0, limit: 10 };
        let m = day(&p, Rng::derive(5, &[]));
        let est = Estimator {
            alpha: 0.3,
            offset_atr: optimal_offset(2.0),
            shown_atr: true,
        };
        let ro = RegimeOracle {
            alpha: 0.3,
            sigmas: vec![0.1, 0.05],
            dynamics: dynamics(),
            mean_duration: 45.0,
            label_shift: 0,
        };
        let r = |s: u64| Rng::derive(s, &[]);
        let (a, b) = (
            run_day(&est, &m, &rules, r(1), r(2)),
            run_day(&ro, &m, &rules, r(1), r(2)),
        );
        assert_eq!((a.pnl, a.fills), (b.pnl, b.fills));
    }

    #[test]
    fn persisted_drift_saturates_at_expected_duration() {
        assert!((persisted(1.0, 0.0, 10) - 1.0).abs() < 1e-12);
        assert!((persisted(1.0, 1.0 - 1.0 / 50.0, 100_000) - 50.0).abs() < 1e-6);
        assert!((persisted(2.0, 1.0, 7) - 14.0).abs() < 1e-12);
    }

    #[test]
    fn filter_learns_a_persistent_trend() {
        use crate::market::Observation;
        use crate::regime::{Nature, RegimeParams, day};
        let p = RegimeParams {
            natures: vec![Nature {
                start: 100.0,
                sigma: 0.1,
                home: [0.0, 1.0, 0.0, 0.0, 0.0],
            }],
            dynamics: dynamics(),
            mean_duration: 1e12,
            shuffle_natures: false,
            obs: Observation {
                warmup_ticks: 400,
                ticks: 180,
                bar_len: 10,
                atr_bars: 14,
                tape_noise_atr: 0.3,
                informed_window: 5,
            },
        };
        let f = RegimeFilter {
            alpha: 0.3,
            sigmas: vec![0.1],
            homes: vec![[0.2; 5]],
            dynamics: dynamics(),
            mean_duration: 1e12,
            tape_noise_atr: 0.3,
            block: 5,
        };
        // Average belief in TrendUp over the second half of 20 always-trending days.
        let belief: f64 = (0..20)
            .map(|d| {
                let m = day(&p, Rng::derive(11, &[d]));
                let (_, sum) =
                    m.iter()
                        .enumerate()
                        .fold((f.fresh(), 0.0), |(s, sum), (t, tick)| {
                            let s = f.observe(&s, t, tick);
                            let add = if t >= 90 { s.post[0][1] } else { 0.0 };
                            (s, sum + add)
                        });
                sum / 90.0
            })
            .sum::<f64>()
            / 20.0;
        assert!(belief > 0.7, "mean TrendUp belief {belief}");
    }
}
