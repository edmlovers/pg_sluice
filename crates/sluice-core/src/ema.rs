//! Learning how much CPU a query really costs.
//!
//! The planner gives a cost in its own units. To turn that into core-seconds we
//! keep, per query pattern (`queryId`), a moving average of the CPU it used and
//! of its planner cost. Their ratio converts the next planner cost into an
//! estimate: `est = planner_cost × ratio`.
//!
//! Until a pattern has been seen [`MIN_SAMPLES`] times, the global ratio (the
//! same average over every query) is used instead.

/// Samples a pattern needs before its own ratio is trusted.
pub const MIN_SAMPLES: u32 = 5;

/// Exponential moving averages for one query pattern (or for all queries).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ema {
    /// Average CPU used, in core-seconds.
    pub cpu: f64,
    /// Average planner cost.
    pub cost: f64,
    /// Samples seen so far.
    pub n: u32,
}

impl Ema {
    /// Adds one finished query: `ema ← ema + alpha × (sample − ema)`.
    ///
    /// The first sample is taken as is, so the average doesn't start out pulled
    /// towards zero. Samples that aren't finite, non-negative numbers are ignored.
    pub fn update(&mut self, cpu: f64, cost: f64, alpha: f64) {
        if !is_valid(cpu) || !is_valid(cost) {
            return;
        }
        if self.n == 0 {
            self.cpu = cpu;
            self.cost = cost;
        } else {
            self.cpu += alpha * (cpu - self.cpu);
            self.cost += alpha * (cost - self.cost);
        }
        self.n = self.n.saturating_add(1);
    }

    /// Core-seconds per planner cost unit, once there are enough samples.
    pub fn ratio(&self) -> Option<f64> {
        if self.n < MIN_SAMPLES || self.cost <= 0.0 {
            return None;
        }
        let ratio = self.cpu / self.cost;
        is_valid(ratio).then_some(ratio)
    }
}

/// The ratio to use for a query: its own pattern's if that is trustworthy,
/// otherwise `global`.
pub fn ratio(entry: Option<&Ema>, global: f64) -> f64 {
    entry.and_then(Ema::ratio).unwrap_or(global)
}

fn is_valid(x: f64) -> bool {
    x.is_finite() && x >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHA: f64 = 0.1;
    const GLOBAL: f64 = 1e-5;

    fn ema_with(samples: &[(f64, f64)]) -> Ema {
        let mut ema = Ema::default();
        for &(cpu, cost) in samples {
            ema.update(cpu, cost, ALPHA);
        }
        ema
    }

    #[test]
    fn first_sample_is_taken_as_is() {
        let ema = ema_with(&[(2.0, 1000.0)]);
        assert_eq!(
            ema,
            Ema {
                cpu: 2.0,
                cost: 1000.0,
                n: 1
            }
        );
    }

    #[test]
    fn later_samples_move_the_average_by_alpha() {
        let ema = ema_with(&[(2.0, 1000.0), (4.0, 2000.0)]);
        // 2 + 0.1 × (4 − 2) = 2.2, 1000 + 0.1 × (2000 − 1000) = 1100
        assert!((ema.cpu - 2.2).abs() < 1e-12);
        assert!((ema.cost - 1100.0).abs() < 1e-9);
        assert_eq!(ema.n, 2);
    }

    #[test]
    fn needs_min_samples_before_using_its_own_ratio() {
        let mut ema = Ema::default();
        for i in 1..MIN_SAMPLES {
            ema.update(1.0, 1000.0, ALPHA);
            assert_eq!(ratio(Some(&ema), GLOBAL), GLOBAL, "after {i} samples");
        }
        ema.update(1.0, 1000.0, ALPHA);
        assert_eq!(ratio(Some(&ema), GLOBAL), 1e-3);
    }

    #[test]
    fn unknown_pattern_uses_global() {
        assert_eq!(ratio(None, GLOBAL), GLOBAL);
    }

    #[test]
    fn zero_cost_uses_global() {
        let ema = ema_with(&[(1.0, 0.0); 5]);
        assert_eq!(ratio(Some(&ema), GLOBAL), GLOBAL);
    }

    #[test]
    fn bad_samples_are_ignored() {
        let mut ema = ema_with(&[(1.0, 1000.0)]);
        let before = ema;
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            ema.update(bad, 1000.0, ALPHA);
            ema.update(1.0, bad, ALPHA);
        }
        assert_eq!(ema, before);
    }

    #[test]
    fn huge_ratio_that_overflows_uses_global() {
        let ema = Ema {
            cpu: f64::MAX,
            cost: f64::MIN_POSITIVE,
            n: 10,
        };
        assert_eq!(ratio(Some(&ema), GLOBAL), GLOBAL);
    }

    #[test]
    fn sample_count_does_not_overflow() {
        let mut ema = Ema {
            cpu: 1.0,
            cost: 1.0,
            n: u32::MAX,
        };
        ema.update(1.0, 1.0, ALPHA);
        assert_eq!(ema.n, u32::MAX);
    }
}
