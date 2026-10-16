//! The exchange's rules (fill curve, inventory limit), the player's account, and the day loop.

use crate::market::Tick;
use crate::rng::Rng;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn sign(self) -> f64 {
        match self {
            Side::Buy => 1.0,
            Side::Sell => -1.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Yell {
    pub asset: usize,
    pub side: Side,
    pub price: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// Logistic steepness of the fill curve, per ATR of edge.
    pub k: f64,
    /// Max absolute units held per asset; yells that would breach it are refused.
    pub limit: i64,
}

/// Fill probability for a quote with `edge_atr` ATRs of edge in the player's favour.
/// Edge 0 (quoting at fair) fills half the time; paying through fair fills more often.
pub fn fill_prob(edge_atr: f64, k: f64) -> f64 {
    1.0 / (1.0 + (k * edge_atr).exp())
}

/// The offset x* (in ATR) maximising expected edge per yell, x·p(x). Setting the derivative
/// to zero gives e^u(u−1) = 1 with u = kx; solved by bisection (u ≈ 1.2785).
pub fn optimal_offset(k: f64) -> f64 {
    let g = |u: f64| u.exp() * (u - 1.0) - 1.0;
    let (lo, hi) = (0..100).fold((1.0_f64, 2.0_f64), |(lo, hi), _| {
        let mid = 0.5 * (lo + hi);
        if g(mid) > 0.0 { (lo, mid) } else { (mid, hi) }
    });
    0.5 * (lo + hi) / k
}

/// The offset e (in ATR) maximising p(e)·(m + e): the expected value of one yell when the
/// player expects fair to move `m` ATR in its favour before the position is marked. The fill
/// curve is still evaluated against *current* fair. `best_offset(0, k) == optimal_offset(k)`.
pub fn best_offset(m: f64, k: f64) -> f64 {
    // h(e) = 1 + E − k(m + e)E with E = e^{ke}: positive at e = −m, negative far right.
    let h = |e: f64| {
        let big_e = (k * e).exp();
        1.0 + big_e - k * (m + e) * big_e
    };
    let (lo, hi) = (0..200).fold((-m, m.abs() + 20.0 / k), |(lo, hi), _| {
        let mid = 0.5 * (lo + hi);
        if h(mid) > 0.0 { (mid, hi) } else { (lo, mid) }
    });
    0.5 * (lo + hi)
}

/// The player's book. `edge` accumulates the fill-time edge (fair − price for buys), so that
/// end-of-day P&L splits into edge + inventory mark-to-market.
#[derive(Clone, Debug)]
pub struct Account {
    pub cash: f64,
    pub inv: Vec<i64>,
    pub edge: f64,
    pub fills: u32,
}

impl Account {
    pub fn flat(n_assets: usize) -> Self {
        Self {
            cash: 0.0,
            inv: vec![0; n_assets],
            edge: 0.0,
            fills: 0,
        }
    }

    pub fn can(&self, y: &Yell, rules: &Rules) -> bool {
        let next = self.inv[y.asset] + y.side.sign() as i64;
        next.abs() <= rules.limit
    }

    pub fn filled(&self, y: &Yell, fair: f64) -> Self {
        let s = y.side.sign();
        Self {
            cash: self.cash - s * y.price,
            inv: self
                .inv
                .iter()
                .enumerate()
                .map(|(i, &q)| if i == y.asset { q + s as i64 } else { q })
                .collect(),
            edge: self.edge + s * (fair - y.price),
            fills: self.fills + 1,
        }
    }

    pub fn mark(&self, fair: &[f64]) -> f64 {
        self.cash
            + self
                .inv
                .iter()
                .zip(fair)
                .map(|(&q, &f)| q as f64 * f)
                .sum::<f64>()
    }
}

/// What a strategy sees at one tick. `tick.fair`/`tick.atr_sys` are the system's hidden truth:
/// only the Oracle may read them; honest strategies use `tape`/`atr_tape`.
pub struct Obs<'a> {
    pub t: usize,
    /// Ticks left after this one until the close.
    pub ticks_left: usize,
    pub tick: &'a Tick,
    pub rules: &'a Rules,
}

pub trait Strategy {
    type State: Clone;
    fn init(&self, n_assets: usize, rng: Rng) -> Self::State;
    fn act(&self, s: &Self::State, obs: &Obs, acct: &Account) -> (Self::State, Option<Yell>);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DayResult {
    pub pnl: f64,
    pub edge: f64,
    pub fills: u32,
}

/// Replay one day: each tick the strategy may yell once; the exchange refuses limit breaches
/// and fills by the logistic curve against the hidden fair value. P&L is marked at close fair.
pub fn run_day<S: Strategy>(
    strat: &S,
    market: &[Tick],
    rules: &Rules,
    fill_rng: Rng,
    strat_rng: Rng,
) -> DayResult {
    let n = market.first().map_or(0, |t| t.fair.len());
    let (_, acct, _) = market.iter().enumerate().fold(
        (strat.init(n, strat_rng), Account::flat(n), fill_rng),
        |(state, acct, rng), (t, tick)| {
            let (state, yell) = strat.act(
                &state,
                &Obs {
                    t,
                    ticks_left: market.len() - 1 - t,
                    tick,
                    rules,
                },
                &acct,
            );
            match yell.filter(|y| acct.can(y, rules)) {
                Some(y) => {
                    let fair = tick.fair[y.asset];
                    let edge_atr =
                        y.side.sign() * (tick.fill_fair[y.asset] - y.price) / tick.atr_sys[y.asset];
                    let (u, rng) = rng.uniform();
                    let acct = if u < fill_prob(edge_atr, rules.k) {
                        acct.filled(&y, fair)
                    } else {
                        acct
                    };
                    (state, acct, rng)
                }
                None => (state, acct, rng),
            }
        },
    );
    let close = market.last().map(|t| t.fair.as_slice()).unwrap_or(&[]);
    DayResult {
        pnl: acct.mark(close),
        edge: acct.edge,
        fills: acct.fills,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_curve_shape() {
        assert!((fill_prob(0.0, 2.0) - 0.5).abs() < 1e-12);
        assert!(fill_prob(1.0, 2.0) < 0.5);
        assert!(fill_prob(-1.0, 2.0) > 0.5);
    }

    #[test]
    fn optimal_offset_maximises_expected_edge() {
        for k in [0.5, 1.0, 2.0, 4.0] {
            let x = optimal_offset(k);
            let ev = |x: f64| x * fill_prob(x, k);
            assert!((x * k - 1.278_464_5).abs() < 1e-6);
            assert!(ev(x) > ev(x * 0.95) && ev(x) > ev(x * 1.05));
        }
    }

    #[test]
    fn best_offset_reduces_to_x_star_and_maximises_ev() {
        for k in [1.0, 2.0, 4.0] {
            assert!((best_offset(0.0, k) - optimal_offset(k)).abs() < 1e-9);
            for m in [-1.0, 0.3, 2.0] {
                let ev = |e: f64| (m + e) * fill_prob(e, k);
                let e = best_offset(m, k);
                assert!(
                    ev(e) >= ev(e - 0.01) && ev(e) >= ev(e + 0.01),
                    "k={k} m={m}"
                );
            }
        }
    }

    #[test]
    fn pnl_splits_into_edge_plus_inventory_mark() {
        let a = Account::flat(1)
            .filled(
                &Yell {
                    asset: 0,
                    side: Side::Buy,
                    price: 99.0,
                },
                100.0,
            )
            .filled(
                &Yell {
                    asset: 0,
                    side: Side::Buy,
                    price: 101.0,
                },
                103.0,
            );
        // Edge: (100-99) + (103-101) = 3. Held 2 units bought at fair 100 and 103, close 105.
        assert!((a.edge - 3.0).abs() < 1e-12);
        let inventory = (105.0 - 100.0) + (105.0 - 103.0);
        assert!((a.mark(&[105.0]) - (a.edge + inventory)).abs() < 1e-12);
    }

    #[test]
    fn limit_refuses_breach() {
        let rules = Rules { k: 1.0, limit: 1 };
        let buy = Yell {
            asset: 0,
            side: Side::Buy,
            price: 1.0,
        };
        let a = Account::flat(1).filled(&buy, 1.0);
        assert!(!a.can(&buy, &rules));
        assert!(a.can(
            &Yell {
                side: Side::Sell,
                ..buy
            },
            &rules
        ));
    }
}
