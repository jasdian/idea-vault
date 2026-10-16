//! The three bots of the kill test. Oracle and Estimator share one scheduler (which asset to
//! look at, which side to yell) so the only difference between them is the price they yell.

use crate::engine::{Account, Obs, Side, Strategy, Yell};
use crate::rng::Rng;

/// Round-robin attention over assets; lean against inventory, alternate when flat, and never
/// yell into the limit.
fn schedule(obs: &Obs, acct: &Account) -> (usize, Side) {
    let n = acct.inv.len();
    let asset = obs.t % n;
    let q = acct.inv[asset];
    let side = match q {
        q if q >= obs.rules.limit => Side::Sell,
        q if q <= -obs.rules.limit => Side::Buy,
        q if q > 0 => Side::Sell,
        q if q < 0 => Side::Buy,
        _ if (obs.t / n).is_multiple_of(2) => Side::Buy,
        _ => Side::Sell,
    };
    (asset, side)
}

fn quote(side: Side, mid: f64, offset_atr: f64, atr: f64) -> f64 {
    mid - side.sign() * offset_atr * atr
}

/// Knows the hidden fair value and system ATR; quotes fair ± x*. The skill ceiling.
pub struct Oracle {
    pub offset_atr: f64,
}

impl Strategy for Oracle {
    type State = ();
    fn init(&self, _: usize, _: Rng) {}
    fn act(&self, _: &(), obs: &Obs, acct: &Account) -> ((), Option<Yell>) {
        let (asset, side) = schedule(obs, acct);
        let price = quote(
            side,
            obs.tick.fair[asset],
            self.offset_atr,
            obs.tick.atr_sys[asset],
        );
        ((), Some(Yell { asset, side, price }))
    }
}

/// Reads only the tape: EMA(α) of prints as its fair estimate, tape ATR as its scale, and
/// quotes estimate ± x*. The "player who has solved the curve".
pub struct Estimator {
    pub alpha: f64,
    pub offset_atr: f64,
    /// The game displays the system ATR, so the player need not estimate scale from the tape.
    pub shown_atr: bool,
}

impl Strategy for Estimator {
    type State = Option<Vec<f64>>;
    fn init(&self, _: usize, _: Rng) -> Self::State {
        None
    }
    fn act(&self, s: &Self::State, obs: &Obs, acct: &Account) -> (Self::State, Option<Yell>) {
        let tape = &obs.tick.tape;
        let est: Vec<f64> = match s {
            None => tape.clone(),
            Some(prev) => prev
                .iter()
                .zip(tape)
                .map(|(&e, &p)| e + self.alpha * (p - e))
                .collect(),
        };
        let (asset, side) = schedule(obs, acct);
        let atr = if self.shown_atr {
            obs.tick.atr_sys[asset]
        } else {
            obs.tick.atr_tape[asset]
        };
        let price = quote(side, est[asset], self.offset_atr, atr);
        (Some(est), Some(Yell { asset, side, price }))
    }
}

/// A bad player: random asset, random side, random price within [-1, 2] ATR of edge around the
/// last print.
pub struct Random;

impl Strategy for Random {
    type State = Rng;
    fn init(&self, _: usize, rng: Rng) -> Rng {
        rng
    }
    fn act(&self, r: &Rng, obs: &Obs, acct: &Account) -> (Rng, Option<Yell>) {
        let (asset, r) = r.below(acct.inv.len());
        let (u, r) = r.uniform();
        let (off, r) = r.range(-1.0, 2.0);
        let side = if u < 0.5 { Side::Buy } else { Side::Sell };
        let price = quote(side, obs.tick.tape[asset], off, obs.tick.atr_tape[asset]);
        (r, Some(Yell { asset, side, price }))
    }
}
