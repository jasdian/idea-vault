//! Summary statistics over daily results.

#[derive(Clone, Copy, Debug)]
pub struct Summary {
    pub mean: f64,
    pub sd: f64,
}

impl Summary {
    /// Mean and sample standard deviation.
    pub fn of(xs: &[f64]) -> Self {
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0);
        Self {
            mean,
            sd: var.sqrt(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Paired {
    pub mean_diff: f64,
    /// Share of days where `a` strictly beat `b` on the same market.
    pub win_rate: f64,
}

pub fn paired(a: &[f64], b: &[f64]) -> Paired {
    let diffs: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    Paired {
        mean_diff: Summary::of(&diffs).mean,
        win_rate: diffs.iter().filter(|&&d| d > 0.0).count() as f64 / diffs.len() as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_paired() {
        let s = Summary::of(&[1.0, 2.0, 3.0]);
        assert!((s.mean - 2.0).abs() < 1e-12 && (s.sd - 1.0).abs() < 1e-12);
        let p = paired(&[3.0, 1.0, 5.0, 2.0], &[1.0, 2.0, 4.0, 2.0]);
        assert!((p.mean_diff - 0.5).abs() < 1e-12 && (p.win_rate - 0.5).abs() < 1e-12);
    }
}
