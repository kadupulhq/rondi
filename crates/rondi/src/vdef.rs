//! RRDtool-compatible whole-series VDEF aggregation semantics.
//!
//! This evaluates the aggregation functions accepted by RRDtool 1.11.0 VDEF
//! expressions. It does not parse graph scripts or render graphs.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VdefFunction {
    Maximum,
    Minimum,
    Average,
    Stdev,
    Percent,
    PercentNan,
    Total,
    First,
    Last,
    LslSlope,
    LslIntercept,
    LslCorrelation,
}

impl VdefFunction {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "MAXIMUM" => Self::Maximum,
            "MINIMUM" => Self::Minimum,
            "AVERAGE" => Self::Average,
            "STDEV" => Self::Stdev,
            "PERCENT" => Self::Percent,
            "PERCENTNAN" => Self::PercentNan,
            "TOTAL" => Self::Total,
            "FIRST" => Self::First,
            "LAST" => Self::Last,
            "LSLSLOPE" => Self::LslSlope,
            "LSLINT" => Self::LslIntercept,
            "LSLCORREL" => Self::LslCorrelation,
            _ => return None,
        })
    }

    fn needs_percentile(self) -> bool {
        matches!(self, Self::Percent | Self::PercentNan)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VdefResult {
    pub value: f64,
    /// VDEF's time component. MAXIMUM/MINIMUM/FIRST/LAST produce a time;
    /// other operations have no time component.
    pub timestamp: Option<i64>,
}

#[derive(Debug, Error, PartialEq)]
pub enum VdefError {
    #[error("VDEF {function} requires a percentile argument")]
    MissingPercentile { function: &'static str },
    #[error("VDEF {function} does not accept a percentile argument")]
    UnexpectedPercentile { function: &'static str },
    #[error("VDEF percentile must be between 0 and 100")]
    PercentileOutOfRange,
    #[error("VDEF step must be positive")]
    InvalidStep,
    #[error("VDEF series timestamps overflow")]
    TimestampOverflow,
}

/// Evaluate one RRDtool VDEF aggregate over regularly spaced values.
///
/// `start` is the beginning of the first interval, and values are ordered in
/// ascending time. NaN and infinity handling follows rrd_graph.c's vdef_calc.
pub fn evaluate_vdef(
    function: VdefFunction,
    percentile: Option<f64>,
    values: &[f64],
    start: i64,
    step: u64,
) -> Result<VdefResult, VdefError> {
    let name = function.name();
    let percentile = match (function.needs_percentile(), percentile) {
        (true, None) => return Err(VdefError::MissingPercentile { function: name }),
        (false, Some(_)) => return Err(VdefError::UnexpectedPercentile { function: name }),
        (true, Some(value)) if !value.is_finite() || !(0.0..=100.0).contains(&value) => {
            return Err(VdefError::PercentileOutOfRange);
        }
        (_, value) => value,
    };
    if step == 0 {
        return Err(VdefError::InvalidStep);
    }

    let no_time = |value| VdefResult {
        value,
        timestamp: None,
    };
    let result = match function {
        VdefFunction::Percent | VdefFunction::PercentNan => {
            let mut sorted = values.to_vec();
            if function == VdefFunction::PercentNan {
                sorted.retain(|value| !value.is_nan());
            }
            if sorted.is_empty() {
                no_time(f64::NAN)
            } else {
                rrd_qsort_compatible(&mut sorted);
                let index =
                    (percentile.unwrap() * (sorted.len() - 1) as f64 / 100.0).round() as usize;
                no_time(sorted[index])
            }
        }
        VdefFunction::Maximum | VdefFunction::Minimum => {
            let Some(mut index) = values.iter().position(|value| !value.is_nan()) else {
                return Ok(no_time(f64::NAN));
            };
            let mut best = values[index];
            for (next_index, value) in values.iter().copied().enumerate().skip(index + 1) {
                // The initial value is accepted even if infinite; later values
                // are considered only when finite, matching RRDtool's source.
                if value.is_finite()
                    && ((function == VdefFunction::Maximum && value > best)
                        || (function == VdefFunction::Minimum && value < best))
                {
                    best = value;
                    index = next_index;
                }
            }
            VdefResult {
                value: best,
                timestamp: Some(interval_timestamp(start, index, step)?),
            }
        }
        VdefFunction::First => match values.iter().position(|value| !value.is_nan()) {
            Some(index) => VdefResult {
                value: values[index],
                timestamp: Some(interval_start(start, index, step)?),
            },
            None => no_time(f64::NAN),
        },
        VdefFunction::Last => match values.iter().rposition(|value| !value.is_nan()) {
            Some(index) => VdefResult {
                value: values[index],
                timestamp: Some(interval_timestamp(start, index, step)?),
            },
            None => no_time(f64::NAN),
        },
        VdefFunction::Average | VdefFunction::Stdev | VdefFunction::Total => {
            let finite: Vec<_> = values
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .collect();
            if finite.is_empty() {
                no_time(f64::NAN)
            } else {
                let sum = finite.iter().sum::<f64>();
                let average = sum / finite.len() as f64;
                let value = match function {
                    VdefFunction::Average => average,
                    VdefFunction::Total => sum * step as f64,
                    VdefFunction::Stdev => (finite
                        .iter()
                        .map(|value| (value - average).powi(2))
                        .sum::<f64>()
                        / finite.len() as f64)
                        .sqrt(),
                    _ => unreachable!(),
                };
                no_time(value)
            }
        }
        VdefFunction::LslSlope | VdefFunction::LslIntercept | VdefFunction::LslCorrelation => {
            let mut count = 0.0;
            let (mut sum_x, mut sum_y, mut sum_xy, mut sum_xx, mut sum_yy) =
                (0.0, 0.0, 0.0, 0.0, 0.0);
            for (index, value) in values.iter().copied().enumerate() {
                if value.is_finite() {
                    let x = index as f64;
                    count += 1.0;
                    sum_x += x;
                    sum_y += value;
                    sum_xy += x * value;
                    sum_xx += x * x;
                    sum_yy += value * value;
                }
            }
            if count == 0.0 {
                no_time(f64::NAN)
            } else {
                let slope = (sum_x * sum_y - count * sum_xy) / (sum_x * sum_x - count * sum_xx);
                let intercept = (sum_y - slope * sum_x) / count;
                let correlation = (sum_xy - sum_x * sum_y / count)
                    / ((sum_xx - sum_x * sum_x / count) * (sum_yy - sum_y * sum_y / count)).sqrt();
                no_time(match function {
                    VdefFunction::LslSlope => slope,
                    VdefFunction::LslIntercept => intercept,
                    VdefFunction::LslCorrelation => correlation,
                    _ => unreachable!(),
                })
            }
        }
    };
    Ok(result)
}

impl VdefFunction {
    fn name(self) -> &'static str {
        match self {
            Self::Maximum => "MAXIMUM",
            Self::Minimum => "MINIMUM",
            Self::Average => "AVERAGE",
            Self::Stdev => "STDEV",
            Self::Percent => "PERCENT",
            Self::PercentNan => "PERCENTNAN",
            Self::Total => "TOTAL",
            Self::First => "FIRST",
            Self::Last => "LAST",
            Self::LslSlope => "LSLSLOPE",
            Self::LslIntercept => "LSLINT",
            Self::LslCorrelation => "LSLCORREL",
        }
    }
}

fn interval_start(start: i64, index: usize, step: u64) -> Result<i64, VdefError> {
    let offset = i64::try_from(index)
        .ok()
        .and_then(|index| index.checked_mul(i64::try_from(step).ok()?))
        .ok_or(VdefError::TimestampOverflow)?;
    start
        .checked_add(offset)
        .ok_or(VdefError::TimestampOverflow)
}

fn interval_timestamp(start: i64, index: usize, step: u64) -> Result<i64, VdefError> {
    interval_start(
        start,
        index.checked_add(1).ok_or(VdefError::TimestampOverflow)?,
        step,
    )
}

fn rrd_qsort_compatible(values: &mut [f64]) {
    // rrd_graph.c passes vdef_percent_compar to the host C library's qsort.
    // In particular, its comparisons involving infinities are not a strict
    // weak order, so a Rust sort produces different results from RRDtool.
    // Matching the same libc qsort call preserves the pinned implementation's
    // observable behavior on a given platform.
    unsafe extern "C" fn compare(
        left: *const libc::c_void,
        right: *const libc::c_void,
    ) -> libc::c_int {
        let left = unsafe { *left.cast::<f64>() };
        let right = unsafe { *right.cast::<f64>() };
        if left.is_nan() {
            return -1;
        }
        if right.is_nan() {
            return 1;
        }
        if left.is_infinite() {
            return if left.is_sign_negative() { -1 } else { 1 };
        }
        if right.is_infinite() {
            return if right.is_sign_negative() { -1 } else { 1 };
        }
        if left < right { -1 } else { 1 }
    }
    unsafe {
        libc::qsort(
            values.as_mut_ptr().cast(),
            values.len(),
            std::mem::size_of::<f64>(),
            Some(compare),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vdef_function_names_match_rrdtool_vocabulary() {
        for name in [
            "MAXIMUM",
            "MINIMUM",
            "AVERAGE",
            "STDEV",
            "PERCENT",
            "PERCENTNAN",
            "TOTAL",
            "FIRST",
            "LAST",
            "LSLSLOPE",
            "LSLINT",
            "LSLCORREL",
        ] {
            assert!(VdefFunction::parse(name).is_some(), "{name}");
        }
        assert!(VdefFunction::parse("MAX").is_none());
    }

    #[test]
    fn extrema_and_first_last_keep_rrd_interval_timestamp_conventions() {
        let values = [f64::NAN, 2.0, 5.0, 5.0, f64::NAN];
        assert_eq!(
            evaluate_vdef(VdefFunction::Maximum, None, &values, 100, 10).unwrap(),
            VdefResult {
                value: 5.0,
                timestamp: Some(130)
            }
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::First, None, &values, 100, 10)
                .unwrap()
                .timestamp,
            Some(110)
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::Last, None, &values, 100, 10)
                .unwrap()
                .timestamp,
            Some(140)
        );
    }

    #[test]
    fn percentile_nan_and_finite_semantics_are_distinct() {
        let values = [f64::NAN, 1.0, 3.0, f64::INFINITY];
        assert!(
            evaluate_vdef(VdefFunction::Percent, Some(0.0), &values, 0, 1)
                .unwrap()
                .value
                .is_nan()
        );
        // RRDtool's source comparator passes the sign-coded C isinf result
        // through qsort; this is the observed pinned-libc ordering for +INF.
        assert!(
            evaluate_vdef(VdefFunction::PercentNan, Some(0.0), &values, 0, 1)
                .unwrap()
                .value
                .is_infinite()
        );
    }

    #[test]
    fn percentile_comparator_preserves_rrdtool_negative_infinity_sign() {
        let mut values = [f64::NEG_INFINITY, 1.0];
        rrd_qsort_compatible(&mut values);
        assert_eq!(values, [f64::NEG_INFINITY, 1.0]);
    }

    #[test]
    fn averages_total_and_population_stdev_skip_non_finite_values() {
        let values = [1.0, 3.0, f64::NAN, f64::INFINITY];
        assert_eq!(
            evaluate_vdef(VdefFunction::Average, None, &values, 0, 10)
                .unwrap()
                .value,
            2.0
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::Total, None, &values, 0, 10)
                .unwrap()
                .value,
            40.0
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::Stdev, None, &values, 0, 10)
                .unwrap()
                .value,
            1.0
        );
    }

    #[test]
    fn least_squares_uses_series_indices_including_unknown_gaps() {
        let values = [1.0, f64::NAN, 5.0];
        assert_eq!(
            evaluate_vdef(VdefFunction::LslSlope, None, &values, 0, 1)
                .unwrap()
                .value,
            2.0
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::LslIntercept, None, &values, 0, 1)
                .unwrap()
                .value,
            1.0
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::LslCorrelation, None, &values, 0, 1)
                .unwrap()
                .value,
            1.0
        );
    }

    #[test]
    fn rejects_invalid_percentile_and_argument_shapes() {
        assert_eq!(
            evaluate_vdef(VdefFunction::Percent, None, &[], 0, 1),
            Err(VdefError::MissingPercentile {
                function: "PERCENT"
            })
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::Average, Some(50.0), &[], 0, 1),
            Err(VdefError::UnexpectedPercentile {
                function: "AVERAGE"
            })
        );
        assert_eq!(
            evaluate_vdef(VdefFunction::Percent, Some(101.0), &[], 0, 1),
            Err(VdefError::PercentileOutOfRange)
        );
    }
}
