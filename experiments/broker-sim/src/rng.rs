//! A tiny value-type PRNG (SplitMix64). Every draw consumes `self` and returns the next state,
//! so randomness is threaded explicitly and every run is reproducible from its seed.

#[derive(Clone, Copy, Debug)]
pub struct Rng(u64);

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

fn mix(z: u64) -> u64 {
    let z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// A stream derived from a seed and a path of tags, e.g. `(seed, [day, asset])`.
    pub fn derive(seed: u64, tags: &[u64]) -> Self {
        Self(tags.iter().fold(mix(seed ^ GOLDEN), |acc, &t| {
            mix(acc ^ t.wrapping_mul(GOLDEN))
        }))
    }

    pub fn next_u64(self) -> (u64, Self) {
        let s = self.0.wrapping_add(GOLDEN);
        (mix(s), Self(s))
    }

    /// Uniform in [0, 1).
    pub fn uniform(self) -> (f64, Self) {
        let (x, r) = self.next_u64();
        ((x >> 11) as f64 / (1u64 << 53) as f64, r)
    }

    /// Uniform in [lo, hi).
    pub fn range(self, lo: f64, hi: f64) -> (f64, Self) {
        let (u, r) = self.uniform();
        (lo + (hi - lo) * u, r)
    }

    /// Uniform index in 0..n.
    pub fn below(self, n: usize) -> (usize, Self) {
        let (u, r) = self.uniform();
        (((u * n as f64) as usize).min(n - 1), r)
    }

    /// Standard normal (Box–Muller, one value per two uniforms).
    pub fn normal(self) -> (f64, Self) {
        let (u1, r) = self.uniform();
        let (u2, r) = r.uniform();
        let u1 = u1.max(f64::MIN_POSITIVE);
        (
            (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos(),
            r,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let a = Rng::derive(7, &[1, 2]).next_u64().0;
        let b = Rng::derive(7, &[1, 2]).next_u64().0;
        let c = Rng::derive(7, &[2, 1]).next_u64().0;
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn normal_is_roughly_standard() {
        let (sum, sq, _) = (0..20_000).fold((0.0, 0.0, Rng::derive(1, &[])), |(s, q, r), _| {
            let (x, r) = r.normal();
            (s + x, q + x * x, r)
        });
        let n = 20_000.0;
        assert!((sum / n).abs() < 0.03);
        assert!((sq / n - 1.0).abs() < 0.05);
    }
}
