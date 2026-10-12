//! Token bucket, kept as debt.
//!
//! Every admitted query adds its estimated CPU (in core-seconds) to the
//! budget's debt. The debt drains at a steady rate, `cpu_share × cores`
//! core-seconds per second. A new query is let in while the debt it would
//! leave stays within `burst`.
//!
//! The bucket never reads a clock: callers pass the current time in, so tests
//! can replay any timeline exactly.

use std::time::Duration;

/// Limits of one budget, already converted to core-seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketLimits {
    /// Core-seconds of debt paid off per second.
    pub drain_rate: f64,
    /// Most debt a new query may leave behind, in core-seconds.
    pub burst: f64,
}

impl BucketLimits {
    /// Converts the rule values (`cpu_share` as a fraction, `burst_s` in seconds
    /// of the whole server) for a server with `cores` CPUs.
    pub fn new(cpu_share: f64, burst_s: f64, cores: u32) -> Self {
        let cores = f64::from(cores);
        Self {
            drain_rate: cpu_share * cores,
            burst: burst_s * cores,
        }
    }
}

/// The state of one budget's bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Bucket {
    /// Core-seconds owed. Never negative.
    pub debt: f64,
    /// When `debt` was last brought up to date, in microseconds on a monotonic
    /// clock.
    pub t_last_us: i64,
}

/// Why a query was not admitted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rejected {
    /// Debt at the time of the decision, in core-seconds.
    pub debt: f64,
    /// How long until the same query would fit.
    pub retry_after: Duration,
}

impl Bucket {
    /// Lets a query with estimated cost `est` (core-seconds) in, or refuses it.
    /// On success the estimate is added to the debt.
    ///
    /// A query is always admitted when there is no debt, even if `est` alone is
    /// over `burst`; otherwise a big query could never run. Stopping queries that
    /// are too big is `max_query_cost`'s job.
    pub fn try_admit(
        &mut self,
        limits: &BucketLimits,
        est: f64,
        now_us: i64,
    ) -> Result<(), Rejected> {
        let est = sanitize(est);
        self.drain(limits, now_us);

        if self.debt == 0.0 || self.debt + est <= limits.burst {
            self.debt += est;
            return Ok(());
        }

        let excess = self.debt + est - limits.burst;
        let wait_s = excess / limits.drain_rate;
        Err(Rejected {
            debt: self.debt,
            // Only fails for absurd numbers; this must not panic inside Postgres.
            retry_after: Duration::try_from_secs_f64(wait_s).unwrap_or(Duration::MAX),
        })
    }

    /// Replaces a query's estimate with what it really used, once it is done.
    ///
    /// `settle(est, 0.0)` hands the whole estimate back, which is how an
    /// admission is undone when a later budget rejects the query.
    pub fn settle(&mut self, est: f64, actual: f64) {
        let est = sanitize(est);
        let actual = sanitize(actual);
        self.debt = (self.debt + actual - est).max(0.0);
    }

    /// Pays off the debt for the time since the last update.
    fn drain(&mut self, limits: &BucketLimits, now_us: i64) {
        // A monotonic clock should not go backwards, but if it does, treat it as
        // no time passing rather than adding debt.
        if now_us <= self.t_last_us {
            return;
        }
        let elapsed_s = (now_us - self.t_last_us) as f64 / 1e6;
        self.debt = (self.debt - limits.drain_rate * elapsed_s).max(0.0);
        self.t_last_us = now_us;
    }
}

/// CPU figures come from the planner and from measurements; treat anything that
/// isn't a finite, non-negative number as zero.
fn sanitize(x: f64) -> f64 {
    if x.is_finite() && x > 0.0 { x } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SEC: i64 = 1_000_000;

    /// 8 cores, `cpu_share: 25%`, burst of 20 core-seconds: the debt drains at
    /// 2 core-seconds per second.
    fn limits() -> BucketLimits {
        BucketLimits::new(0.25, 2.5, 8)
    }

    #[test]
    fn converts_rule_values_to_core_seconds() {
        assert_eq!(
            limits(),
            BucketLimits {
                drain_rate: 2.0,
                burst: 20.0
            }
        );
    }

    /// One timeline, step by step. Each row: time (s), what happens, expected
    /// debt afterwards.
    #[test]
    fn example_timeline() {
        let l = limits();
        let mut b = Bucket::default();

        // t=0: three 6 core-second reports go in back to back.
        assert_eq!(b.try_admit(&l, 6.0, 0), Ok(()));
        assert_eq!(b.debt, 6.0);
        assert_eq!(b.try_admit(&l, 6.0, 0), Ok(()));
        assert_eq!(b.try_admit(&l, 6.0, 0), Ok(()));
        assert_eq!(b.debt, 18.0);

        // t=0: a fourth would leave 24 > 20. It fits once 4 core-seconds drain: 2 s.
        assert_eq!(
            b.try_admit(&l, 6.0, 0),
            Err(Rejected {
                debt: 18.0,
                retry_after: Duration::from_secs(2)
            })
        );

        // t=1: debt drained to 16; 22 still too much, 1 s to go.
        assert_eq!(
            b.try_admit(&l, 6.0, SEC),
            Err(Rejected {
                debt: 16.0,
                retry_after: Duration::from_secs(1)
            })
        );

        // t=2: debt 14, 14 + 6 = 20 is exactly the burst: admitted.
        assert_eq!(b.try_admit(&l, 6.0, 2 * SEC), Ok(()));
        assert_eq!(b.debt, 20.0);

        // That query really used 9 core-seconds, not 6. Debt may go over burst.
        b.settle(6.0, 9.0);
        assert_eq!(b.debt, 23.0);

        // t=4: debt 19, 19 + 6 - 20 = 5 over, at 2/s that is 2.5 s.
        assert_eq!(
            b.try_admit(&l, 6.0, 4 * SEC),
            Err(Rejected {
                debt: 19.0,
                retry_after: Duration::from_millis(2500)
            })
        );

        // t=14: 20 s of draining would be 40 core-seconds; debt stops at 0.
        // With no debt even a 30 core-second query (over burst) gets in.
        assert_eq!(b.try_admit(&l, 30.0, 14 * SEC), Ok(()));
        assert_eq!(b.debt, 30.0);

        // t=15: debt 28 is over burst, so even a tiny query waits.
        assert!(b.try_admit(&l, 0.1, 15 * SEC).is_err());
    }

    #[test]
    fn settle_can_lower_the_debt() {
        let l = limits();
        let mut b = Bucket::default();
        b.try_admit(&l, 6.0, 0).unwrap();
        b.settle(6.0, 1.0);
        assert_eq!(b.debt, 1.0);
    }

    #[test]
    fn settle_with_zero_undoes_an_admission() {
        let l = limits();
        let mut b = Bucket::default();
        b.try_admit(&l, 5.0, 0).unwrap();
        b.try_admit(&l, 3.0, 0).unwrap();
        b.settle(3.0, 0.0);
        assert_eq!(b.debt, 5.0);
    }

    #[test]
    fn clock_going_backwards_changes_nothing() {
        let l = limits();
        let mut b = Bucket {
            debt: 10.0,
            t_last_us: 5 * SEC,
        };
        b.try_admit(&l, 1.0, 3 * SEC).unwrap();
        assert_eq!(
            b,
            Bucket {
                debt: 11.0,
                t_last_us: 5 * SEC
            }
        );
    }

    #[test]
    fn huge_wait_does_not_panic() {
        let l = limits();
        let mut b = Bucket {
            debt: 1e300,
            t_last_us: 0,
        };
        let r = b.try_admit(&l, 1.0, 0).unwrap_err();
        assert_eq!(r.retry_after, Duration::MAX);
    }

    #[test]
    fn bad_numbers_count_as_zero() {
        let l = limits();
        let mut b = Bucket::default();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -3.0] {
            b.try_admit(&l, bad, 0).unwrap();
            b.settle(bad, bad);
            b.settle(0.0, bad);
            assert_eq!(b.debt, 0.0, "{bad}");
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Admit { est: f64, dt_us: i64 },
        Settle { est: f64, actual: f64 },
    }

    /// Mostly ordinary costs, plus negative, huge and non-finite ones.
    fn any_number() -> impl Strategy<Value = f64> {
        prop_oneof![
            4 => 0.0..100.0,
            1 => -100.0..0.0,
            1 => 1e6..1e12,
            1 => Just(f64::NAN),
            1 => Just(f64::INFINITY),
            1 => Just(f64::NEG_INFINITY),
        ]
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (any_number(), -SEC..10 * SEC).prop_map(|(est, dt_us)| Op::Admit { est, dt_us }),
            (any_number(), any_number()).prop_map(|(est, actual)| Op::Settle { est, actual }),
        ]
    }

    proptest! {
        #[test]
        fn debt_is_never_negative(
            cpu_share in 0.01..=1.0,
            burst_s in 0.1..60.0,
            cores in 1u32..128,
            ops in prop::collection::vec(op(), 0..200),
        ) {
            let l = BucketLimits::new(cpu_share, burst_s, cores);
            let mut b = Bucket::default();
            let mut now = 0;
            for op in ops {
                match op {
                    Op::Admit { est, dt_us } => {
                        now += dt_us;
                        if let Err(r) = b.try_admit(&l, est, now) {
                            prop_assert!(r.debt > 0.0);
                        }
                    }
                    Op::Settle { est, actual } => b.settle(est, actual),
                }
                prop_assert!(b.debt.is_finite() && b.debt >= 0.0, "{b:?}");
            }
        }

        #[test]
        fn admitted_query_stays_within_burst_unless_debt_was_zero(
            debt in 0.0..100.0,
            est in 0.0..100.0,
        ) {
            let l = limits();
            let mut b = Bucket { debt, t_last_us: 0 };
            if b.try_admit(&l, est, 0).is_ok() && debt > 0.0 {
                prop_assert!(b.debt <= l.burst);
            }
        }
    }
}
