//! Primary data points: apportioning an amount across step boundaries
//! (UP-14, UP-19 to UP-25).

use crate::rrd_binary::{rrd_mul_add, rrd_nan};

const MICROS_PER_SECOND: f64 = 1_000_000.0;

/// A time as whole seconds and microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Time {
    pub secs: i64,
    pub usec: u64,
}

/// The step boundaries crossed between the last update and a new time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StepPlan {
    /// The interval *i* (UP-14).
    pub interval: f64,
    /// Start of the step holding the last update (*T0*).
    pub t0: i64,
    /// Start of the step holding the new time (*T1*).
    pub t1: i64,
    /// Step boundaries crossed (*n*).
    pub n: u64,
    /// Whole seconds from the last update to the first boundary (UP-20).
    pub lead1: f64,
    /// Time from the last boundary to the new time (UP-20).
    pub tail: f64,
    /// Time from the last update to the last boundary, *W* (UP-20).
    pub w: f64,
    /// Whether the update is split rather than whole (UP-20).
    pub split: bool,
    /// Whether a row completed by the first run of a split update is
    /// printed one slot early: *s0* + *lead1* < *T0* + *S* (UP-32).
    pub early_first_print: bool,
}

/// The per-DS PDP state: unknown seconds and the accumulated value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PdpState {
    pub unknown_seconds: u64,
    pub value: f64,
}

/// The PDP values an update completes for one DS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Completed {
    /// No step boundary was crossed.
    None,
    /// One value for every completed PDP.
    Whole(f64),
    /// A first PDP and a value for each further PDP.
    Split { first: f64, rest: f64 },
}

/// Consecutive completed PDPs sharing one value per DS. PDP *j* of the run
/// starts at `start + j·S`; the run ends at `end`, which is `start + len·S`
/// unless the step count wrapped (see [`plan`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Run {
    pub start: i64,
    pub len: u64,
    pub end: i64,
    pub values: Vec<f64>,
}

/// The step as signed seconds, clamped to `1..=i64::MAX` so that planning
/// stays total; every valid file step is unchanged.
fn step_secs(step: u64) -> i64 {
    i64::try_from(step).unwrap_or(i64::MAX).max(1)
}

fn step_start(secs: i64, step: i64) -> i64 {
    secs.saturating_sub(secs.rem_euclid(step))
}

fn micros(usec: u64) -> f64 {
    usec as f64 / MICROS_PER_SECOND
}

/// Work out the interval and the step boundaries between `last` and `new`.
pub(crate) fn plan(last: Time, new: Time, step: u64) -> StepPlan {
    let step_secs = step_secs(step);

    // Compatibility: the microsecond difference is taken in wrapping
    // 64-bit integers before it becomes a double, which matters once a
    // microsecond count is too large for a double to hold exactly.
    // Provenance: specification (docs/compatibility/update.md UP-14).
    let interval = new.secs.saturating_sub(last.secs) as f64
        + (new.usec as i64).wrapping_sub(last.usec as i64) as f64 / MICROS_PER_SECOND;

    let t0 = step_start(last.secs, step_secs);
    let t1 = step_start(new.secs, step_secs);
    // Compatibility: the count of step boundaries crossed wraps to a signed
    // 32-bit value and is then used as an unsigned 64-bit count, so 2^32
    // steps count as none and 2^31 steps as nearly 2^64.
    // Provenance: specification (docs/compatibility/update.md UP-37).
    let n = (t1.saturating_sub(t0) / step_secs) as i32 as i64 as u64;

    // Provenance: specification (docs/compatibility/update.md UP-20).
    let tail = new.secs.saturating_sub(t1) as f64 + micros(new.usec);
    let w = t1.saturating_sub(last.secs) as f64 - micros(last.usec);
    // The remainder of the rounded W, so it follows W's rounding.
    let lead1 = (w % step as f64).floor();
    let split = n >= 2 && lead1 >= 1.0;
    // Provenance: specification (docs/compatibility/update.md UP-32).
    let early_first_print =
        split && i128::from(last.secs) + (lead1 as i128) < i128::from(t0) + i128::from(step_secs);

    StepPlan {
        interval,
        t0,
        t1,
        n,
        lead1,
        tail,
        w,
        split,
        early_first_print,
    }
}

/// Apply one DS's amount to its PDP state and return the PDPs it completes
/// (UP-19 to UP-25). A NaN amount counts as unknown.
pub(crate) fn advance(
    state: &mut PdpState,
    amount: Option<f64>,
    plan: &StepPlan,
    step: u64,
    heartbeat: u64,
) -> Completed {
    let amount = amount.filter(|a| !a.is_nan());

    if plan.n == 0 {
        // Provenance: specification (docs/compatibility/update.md UP-19).
        match amount {
            None => {
                state.unknown_seconds = state
                    .unknown_seconds
                    .saturating_add(plan.interval.trunc() as u64)
            }
            Some(a) if state.value.is_nan() => state.value = a,
            Some(a) => state.value += a,
        }
        return Completed::None;
    }

    let u = state.unknown_seconds;
    let base = if state.value.is_nan() {
        0.0
    } else {
        state.value
    };
    // Provenance: specification (docs/compatibility/update.md UP-24).
    let too_unknown = u.saturating_mul(2) > step;
    let step_f = step as f64;
    // n·S and (n − 1)·S in the wrapping count arithmetic of `plan`.
    let span = plan.n.wrapping_mul(step);

    let (completed, rate) = if plan.split {
        // Provenance: specification (docs/compatibility/update.md UP-22).
        let (vt, ut, rest, rate) = match amount {
            Some(a) => {
                let q = (a * plan.lead1) / plan.interval;
                let vt = rrd_mul_add(q / plan.lead1, plan.lead1, base);
                let rem = a - q;
                let r2 = rem / (plan.interval - plan.lead1);
                let later_span = plan.n.wrapping_sub(1).wrapping_mul(step) as f64;
                let rest = (r2 * (plan.w - plan.lead1)) / later_span;
                (vt, u as f64, rest, Some(r2))
            }
            None => (state.value, u as f64 + plan.lead1, rrd_nan(), None),
        };
        let first = if too_unknown || plan.lead1 > heartbeat as f64 {
            rrd_nan()
        } else {
            vt / (step_f - ut)
        };
        (Completed::Split { first, rest }, rate)
    } else {
        // Provenance: specification (docs/compatibility/update.md UP-21).
        let (vt, e, rate) = match amount {
            Some(a) => {
                let r = a / plan.interval;
                (rrd_mul_add(r, plan.w, base), 0.0, Some(r))
            }
            None => (state.value, plan.w, None),
        };
        let value = if too_unknown || plan.interval > heartbeat as f64 {
            rrd_nan()
        } else {
            vt / ((span as f64 - u as f64) - e)
        };
        (Completed::Whole(value), rate)
    };

    // Provenance: specification (docs/compatibility/update.md UP-25).
    // A known amount whose carry is NaN (an infinite amount minus its
    // infinite share) leaves the step as an unknown amount would.
    let carried = rate.map(|r| r * plan.tail).filter(|value| !value.is_nan());
    *state = match carried {
        Some(value) => PdpState {
            unknown_seconds: 0,
            value,
        },
        None => PdpState {
            unknown_seconds: plan.tail.trunc() as u64,
            value: rrd_nan(),
        },
    };
    completed
}

/// Group the PDPs completed by one update, one entry per DS, into runs of
/// equal values in time order (UP-23).
pub(crate) fn runs(plan: &StepPlan, step: u64, completed: &[Completed]) -> Vec<Run> {
    if plan.n == 0 {
        return Vec::new();
    }
    let collect = |pick: fn(&Completed) -> f64| completed.iter().map(pick).collect();
    if plan.split {
        let first = |c: &Completed| match *c {
            Completed::Split { first, .. } => first,
            _ => unreachable!("every DS of a split update completes a split"),
        };
        let rest = |c: &Completed| match *c {
            Completed::Split { rest, .. } => rest,
            _ => unreachable!("every DS of a split update completes a split"),
        };
        let second_start = plan.t0.saturating_add(step_secs(step));
        vec![
            Run {
                start: plan.t0,
                len: 1,
                end: second_start,
                values: collect(first),
            },
            Run {
                start: second_start,
                len: plan.n.saturating_sub(1),
                end: plan.t1,
                values: collect(rest),
            },
        ]
    } else {
        let whole = |c: &Completed| match *c {
            Completed::Whole(value) => value,
            _ => unreachable!("every DS of a whole update completes a whole value"),
        };
        vec![Run {
            start: plan.t0,
            len: plan.n,
            end: plan.t1,
            values: collect(whole),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrd_binary::rrd_nan;

    const STEP: u64 = 10;
    const HEARTBEAT: u64 = 600;

    fn at(secs: i64) -> Time {
        Time { secs, usec: 0 }
    }

    fn state(unknown_seconds: u64, value: f64) -> PdpState {
        PdpState {
            unknown_seconds,
            value,
        }
    }

    fn whole_value(completed: Completed) -> f64 {
        match completed {
            Completed::Whole(value) => value,
            other => panic!("expected a whole update, got {other:?}"),
        }
    }

    #[test]
    fn same_step_accumulates() {
        let mut pdp = state(0, rrd_nan());
        let plan_a = plan(at(100), at(105), STEP);
        assert_eq!(
            advance(&mut pdp, Some(10.0), &plan_a, STEP, HEARTBEAT),
            Completed::None
        );
        assert_eq!(pdp, state(0, 10.0));

        let plan_b = plan(at(105), at(107), STEP);
        assert_eq!(
            advance(&mut pdp, None, &plan_b, STEP, HEARTBEAT),
            Completed::None
        );
        assert_eq!(pdp, state(2, 10.0));
        assert!(runs(&plan_b, STEP, &[Completed::None]).is_empty());
    }

    #[test]
    fn whole_two_steps() {
        let mut pdp = state(0, rrd_nan());
        let p = plan(at(100), at(120), STEP);
        assert_eq!(p.n, 2);
        assert!(!p.split);
        assert_eq!(p.w, 20.0);
        let done = advance(&mut pdp, Some(100.0), &p, STEP, HEARTBEAT);
        assert_eq!(done, Completed::Whole(5.0));
        assert_eq!(pdp, state(0, 0.0));
        assert_eq!(
            runs(&p, STEP, &[done]),
            vec![Run {
                start: 100,
                len: 2,
                end: 120,
                values: vec![5.0]
            }]
        );
    }

    #[test]
    fn split_three_steps() {
        let mut pdp = state(0, 10.0);
        let p = plan(at(105), at(130), STEP);
        assert_eq!(p.n, 3);
        assert_eq!(p.lead1, 5.0);
        assert!(p.split);
        let done = advance(&mut pdp, Some(50.0), &p, STEP, HEARTBEAT);
        assert_eq!(
            done,
            Completed::Split {
                first: 2.0,
                rest: 2.0
            }
        );
        assert_eq!(pdp, state(0, 0.0));
        assert_eq!(
            runs(&p, STEP, &[done]),
            vec![
                Run {
                    start: 100,
                    len: 1,
                    end: 110,
                    values: vec![2.0]
                },
                Run {
                    start: 110,
                    len: 2,
                    end: 130,
                    values: vec![2.0]
                },
            ]
        );
    }

    #[test]
    fn unknown_amount_whole() {
        let mut pdp = state(0, rrd_nan());
        let p = plan(at(100), at(125), STEP);
        let done = advance(&mut pdp, None, &p, STEP, HEARTBEAT);
        assert!(whole_value(done).is_nan());
        assert_eq!(pdp.unknown_seconds, 5);
        assert!(pdp.value.is_nan());
    }

    #[test]
    fn half_step_unknown_still_known() {
        let p = plan(at(105), at(110), STEP);

        let mut half = state(5, 25.0);
        assert_eq!(
            advance(&mut half, Some(0.0), &p, STEP, HEARTBEAT),
            Completed::Whole(5.0)
        );

        let mut over = state(6, 20.0);
        let done = advance(&mut over, Some(0.0), &p, STEP, HEARTBEAT);
        assert!(whole_value(done).is_nan());
    }

    #[test]
    fn heartbeat_uses_interval_or_lead1() {
        let mut whole = state(0, rrd_nan());
        let p = plan(at(100), at(900), STEP);
        let done = advance(&mut whole, Some(800.0), &p, STEP, HEARTBEAT);
        assert!(whole_value(done).is_nan());

        let mut split = state(0, 10.0);
        let p = plan(at(105), at(900), STEP);
        match advance(&mut split, None, &p, STEP, HEARTBEAT) {
            Completed::Split { first, rest } => {
                assert_eq!(first, 2.0);
                assert!(rest.is_nan());
            }
            other => panic!("expected a split update, got {other:?}"),
        }
    }

    #[test]
    fn interval_adds_converted_parts() {
        let p = plan(
            Time {
                secs: 1_000_000_003,
                usec: 400_000,
            },
            Time {
                secs: 1_000_000_005,
                usec: 100_000,
            },
            STEP,
        );
        assert_eq!(p.interval, 2.0 + (-300000.0 / 1000000.0));
        assert!(!p.split);
    }

    #[test]
    fn huge_step_and_extreme_times_do_not_panic() {
        let extremes = [
            Time {
                secs: i64::MIN,
                usec: 0,
            },
            Time {
                secs: i64::MIN,
                usec: 999_999,
            },
            Time { secs: 0, usec: 0 },
            Time {
                secs: i64::MAX,
                usec: 0,
            },
            Time {
                secs: i64::MAX,
                usec: 999_999,
            },
        ];
        for step in [0, 1, STEP, u64::MAX] {
            for last in extremes {
                for new in extremes {
                    let p = plan(last, new, step);
                    for amount in [None, Some(1.0)] {
                        for start in [state(0, rrd_nan()), state(u64::MAX, 1.0)] {
                            let mut pdp = start;
                            let done = advance(&mut pdp, amount, &p, step, u64::MAX);
                            let mut pdp = start;
                            advance(&mut pdp, amount, &p, step, 0);
                            let _ = runs(&p, step, &[done]);
                        }
                    }
                }
            }
        }
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-10 * expected.abs(),
            "{actual} is not within 1e-10 of {expected}"
        );
    }

    #[test]
    fn fractional_split_matches_reference() {
        // Provenance: black-box compatibility test (reviewer replay, two GAUGE sources).
        let readings: [(Time, [Option<f64>; 2]); 3] = [
            (
                Time {
                    secs: 1_000_000_003,
                    usec: 500_000,
                },
                [Some(7.3), None],
            ),
            (
                Time {
                    secs: 1_000_000_035,
                    usec: 250_000,
                },
                [Some(3.1), Some(2.9)],
            ),
            (
                Time {
                    secs: 1_000_000_047,
                    usec: 750_000,
                },
                [None, Some(1.7)],
            ),
        ];
        let mut last = at(1_000_000_000);
        let mut pdps = [state(0, rrd_nan()); 2];
        let mut all_runs = Vec::new();
        for (new, xs) in readings {
            let p = plan(last, new, STEP);
            let completed: Vec<Completed> = pdps
                .iter_mut()
                .zip(xs)
                .map(|(pdp, x)| {
                    let amount = x.map(|x| x * p.interval);
                    advance(pdp, amount, &p, STEP, HEARTBEAT)
                })
                .collect();
            all_runs.extend(runs(&p, STEP, &completed));
            last = new;
        }

        let expected = [
            (1_000_000_000, 1, [4.415, 2.4857142857]),
            (1_000_000_010, 2, [3.1775, 2.9725]),
            (1_000_000_030, 1, [3.1, 2.33]),
        ];
        assert_eq!(all_runs.len(), expected.len());
        for (run, (start, len, values)) in all_runs.iter().zip(expected) {
            assert_eq!((run.start, run.len), (start, len));
            assert_eq!(run.values.len(), 2);
            for (&actual, want) in run.values.iter().zip(values) {
                assert_close(actual, want);
            }
        }

        assert_eq!(pdps[0].unknown_seconds, 7);
        assert!(pdps[0].value.is_nan());
        assert_eq!(pdps[1].unknown_seconds, 0);
        assert_close(pdps[1].value, 13.175);
    }
}
