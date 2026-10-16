//! The y-axis value range (GR-70 to GR-73).

/// Bound values a range is rounded to, as multiples of a power of the base
/// (GR-70).
const SENSIBLE: [f64; 46] = [
    0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 1.0, 1.2, 1.5, 1.8, 2.0, 2.5, 3.0, 3.5, 4.0, 5.0, 6.0,
    7.0, 8.0, 9.0, 10.0, 20.0, 25.0, 30.0, 40.0, 50.0, 60.0, 70.0, 75.0, 80.0, 90.0, 100.0, 125.0,
    200.0, 250.0, 300.0, 400.0, 500.0, 600.0, 700.0, 750.0, 800.0, 900.0, 1000.0,
];

/// The extremes a logarithmic axis assumes without a positive value; they
/// are widened like data to 2.55e-9 and 10.2 (GR-72).
const LOG_EMPTY: (f64, f64) = (5.1e-9, 5.1);

/// How the value range is chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeOptions {
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    pub rigid: bool,
    pub allow_shrink: bool,
    pub alt_autoscale: bool,
    pub alt_autoscale_min: bool,
    pub alt_autoscale_max: bool,
    pub logarithmic: bool,
    pub base: f64,
    /// `--y-grid` step and label factor.
    pub y_grid: Option<(f64, i32)>,
}

impl RangeOptions {
    /// Whether the graph is abandoned without output (GR-21): an upper
    /// limit of +infinity or a lower limit of -infinity. Other infinite or
    /// overflowing ranges are drawn.
    //
    // Compatibility: `-u 1.7e308` reports `value_max = inf` and
    // `-u 1e308 -l -1e308` a range wider than the largest double; neither
    // abandons the graph.
    // Provenance: black-box compatibility test (num-024, -027, -036 and
    // probes with -u 1.7e308 and -u 1e308 -l -1e308).
    pub fn abandons(&self) -> bool {
        self.upper == Some(f64::INFINITY) || self.lower == Some(f64::NEG_INFINITY)
    }
}

impl Default for RangeOptions {
    fn default() -> Self {
        Self {
            lower: None,
            upper: None,
            rigid: false,
            allow_shrink: false,
            alt_autoscale: false,
            alt_autoscale_min: false,
            alt_autoscale_max: false,
            logarithmic: false,
            base: 1000.0,
            y_grid: None,
        }
    }
}

/// The chosen range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueRange {
    pub min: f64,
    pub max: f64,
}

/// The values that take part in scaling, already stacked.
#[derive(Clone, Debug, Default)]
pub struct Scale {
    min: f64,
    max: f64,
    min_positive: f64,
    any: bool,
}

impl Scale {
    pub fn new() -> Self {
        Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            min_positive: f64::INFINITY,
            any: false,
        }
    }

    /// Adds one value; unknown and infinite values are ignored.
    pub fn add(&mut self, value: f64) {
        // Compatibility: infinite data does not stretch the range.
        // Provenance: black-box compatibility test (scale-144 to -154).
        if !value.is_finite() {
            return;
        }
        self.any = true;
        // Strict comparisons keep the first of equal values, so a negative
        // zero after a positive one is not the minimum.
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
        if value > 0.0 {
            self.min_positive = self.min_positive.min(value);
        }
    }
}

fn known(value: Option<f64>) -> Option<f64> {
    value.filter(|value| !value.is_nan())
}

/// The largest power of `base` not above `magnitude`.
fn power_below(magnitude: f64, base: f64) -> f64 {
    if magnitude <= 0.0 || !magnitude.is_finite() {
        return 1.0;
    }
    let mut exponent = (magnitude.ln() / base.ln()).floor();
    // Correct for logarithms that land just beside an exact power.
    while base.powf(exponent) > magnitude {
        exponent -= 1.0;
    }
    while base.powf(exponent + 1.0) <= magnitude {
        exponent += 1.0;
    }
    base.powf(exponent)
}

/// Moves `value` (already divided by the power) away from zero to a
/// sensible value, keeping it when it is already one.
fn away(magnitude: f64) -> Option<f64> {
    SENSIBLE
        .iter()
        .copied()
        .find(|&candidate| candidate >= magnitude)
}

/// Moves towards zero to the next smaller sensible value.
fn toward(magnitude: f64) -> Option<f64> {
    SENSIBLE
        .iter()
        .rev()
        .copied()
        .find(|&candidate| candidate < magnitude)
}

fn round_bound(value: f64, power: f64, up: bool) -> f64 {
    let scaled = value / power;
    let magnitude = scaled.abs();
    // Compatibility: a bound below the smallest sensible value is kept.
    // Provenance: black-box compatibility test (scale-019, -096).
    if magnitude < SENSIBLE[0] {
        return value;
    }
    let outward = (scaled >= 0.0) == up;
    let chosen = if outward {
        away(magnitude)
    } else {
        toward(magnitude)
    };
    match chosen {
        Some(candidate) => candidate.copysign(scaled) * power,
        None => value,
    }
}

/// Chooses the value range (GR-70 to GR-73).
pub fn value_range(scale: &Scale, options: &RangeOptions) -> ValueRange {
    if options.logarithmic {
        return logarithmic_range(scale, options);
    }
    let (min, max) = settle(limited(scale, options));
    if options.rigid && !options.allow_shrink {
        return ValueRange { min, max };
    }
    let alt_min = options.alt_autoscale || options.alt_autoscale_min;
    let alt_max = options.alt_autoscale || options.alt_autoscale_max;
    if alt_min || alt_max {
        let spread = (max - min) * 0.1;
        return ValueRange {
            min: if alt_min { min - spread } else { min },
            max: if alt_max { max + spread } else { max },
        };
    }
    if let Some((step, factor)) = options.y_grid {
        // Compatibility: with --y-grid the bounds move out to whole label
        // intervals instead of sensible values.
        // Provenance: black-box compatibility test (scale-189 to -191).
        let interval = step * f64::from(factor);
        if interval > 0.0 && interval.is_finite() {
            return ValueRange {
                min: (min / interval).floor() * interval,
                max: (max / interval).ceil() * interval,
            };
        }
    }
    let power = power_below(min.abs().max(max.abs()), options.base);
    ValueRange {
        min: round_bound(min, power, false),
        max: round_bound(max, power, true),
    }
}

/// The range of a logarithmic axis (GR-72).
fn logarithmic_range(scale: &Scale, options: &RangeOptions) -> ValueRange {
    let (low, high) = if scale.min_positive.is_finite() {
        (scale.min_positive, scale.max)
    } else {
        LOG_EMPTY
    };
    // Compatibility: a limit that reaches past the extreme replaces it;
    // otherwise the extreme is halved or doubled.
    // Provenance: black-box compatibility test (layout fuzzing against the
    // oracle; -o with -u between 5 and 10.2 and no positive value, and
    // -u 100 with a largest value of 56.67).
    let min = match known(options.lower) {
        // Compatibility: a lower limit equal to the extreme is still halved,
        // while an equal upper limit is kept (-l 5.1e-9 and -u 5.1 without
        // a positive value).
        Some(lower) if options.rigid || lower < low => lower,
        _ => low / 2.0,
    };
    let max = match known(options.upper) {
        Some(upper) if options.rigid || upper >= high => upper,
        _ => high * 2.0,
    };
    ValueRange { min, max }
}

/// The data extremes with the limits applied (GR-70).
fn limited(scale: &Scale, options: &RangeOptions) -> (f64, f64) {
    let (min, max) = if scale.any {
        (scale.min, scale.max)
    } else {
        (0.0, 1.0)
    };
    let (lower, upper) = (known(options.lower), known(options.upper));
    if !options.rigid {
        return (
            lower.map_or(min, |lower| min.min(lower)),
            upper.map_or(max, |upper| max.max(upper)),
        );
    }
    if options.allow_shrink {
        // Compatibility: shrinking keeps the part of the limits the data
        // covers, which is then rounded like data.
        // Provenance: black-box compatibility test (scale-170, -171).
        return (
            lower.map_or(min, |lower| min.max(lower)),
            upper.map_or(max, |upper| max.min(upper)),
        );
    }
    (lower.unwrap_or(min), upper.unwrap_or(max))
}

/// Step 2 of GR-70: an empty or inverted range.
fn settle((min, max): (f64, f64)) -> (f64, f64) {
    if min == max {
        if max == 0.0 {
            // Compatibility: a range of zeros is 0..1 even when the zeros
            // are negative.
            // Provenance: black-box compatibility test (range-021).
            (0.0, 1.0)
        } else {
            (min, max + max.abs() * 0.01)
        }
    } else if min > max {
        // Compatibility: inverted limits keep the upper one and put the
        // lower one just beside it, below for a positive lower limit and
        // otherwise past it, so the range can stay inverted.
        // Provenance: black-box compatibility test (graph_data_differential
        // and the inverted-limit probes recorded under GR-70).
        (max * if min > 0.0 { 0.99 } else { 1.01 }, max)
    } else {
        (min, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(values: &[f64], change: impl FnOnce(&mut RangeOptions)) -> (f64, f64) {
        let mut scale = Scale::new();
        for value in values {
            scale.add(*value);
        }
        let mut options = RangeOptions::default();
        change(&mut options);
        let range = value_range(&scale, &options);
        (range.min, range.max)
    }

    #[test]
    fn sensible_rounding() {
        assert_eq!(range(&[1.333, 19.333], |_| {}), (1.2, 20.0));
        assert_eq!(range(&[0.0, 1.0], |_| {}), (0.0, 1.0));
        assert_eq!(range(&[3.0], |_| {}), (2.5, 3.5));
        assert_eq!(range(&[-3.0], |_| {}), (-3.0, -2.5));
        assert_eq!(range(&[1.5, 2000.0], |_| {}), (1.5, 2000.0));
        assert_eq!(range(&[900.0, 2000.0], |_| {}), (800.0, 2000.0));
        assert_eq!(range(&[], |_| {}), (0.0, 1.0));
        assert_eq!(range(&[0.0], |_| {}), (0.0, 1.0));
        let negative_zeros = range(&[-0.0, -0.0], |_| {});
        assert!(negative_zeros.0.is_sign_positive() && negative_zeros.1 == 1.0);
        assert_eq!(range(&[f64::NAN, 5.0], |_| {}), (4.0, 6.0));
    }

    #[test]
    fn limits_rigid_and_shrink() {
        let data = [-70.0, 56.67];
        assert_eq!(range(&data, |o| o.upper = Some(10.0)), (-70.0, 60.0));
        assert_eq!(
            range(&data, |o| {
                o.upper = Some(2.0);
                o.lower = Some(1.0);
                o.rigid = true;
            }),
            (1.0, 2.0)
        );
        assert_eq!(
            range(&data, |o| {
                o.upper = Some(2.0);
                o.lower = Some(1.0);
                o.rigid = true;
                o.allow_shrink = true;
            }),
            (0.8, 2.0)
        );
        assert_eq!(
            range(&data, |o| {
                o.upper = Some(100.0);
                o.lower = Some(0.0);
                o.rigid = true;
                o.allow_shrink = true;
            }),
            (0.0, 60.0)
        );
        assert_eq!(
            range(&[5.0], |o| {
                o.upper = Some(5.0);
                o.lower = Some(5.0);
                o.rigid = true;
            }),
            (5.0, 5.05)
        );
        let wide = range(&[1.2e6, 2.0e7], |o| {
            o.upper = Some(1000.0);
            o.lower = Some(-1000.0);
        });
        assert_eq!(wide, (-1000.0, 2.0e7));
        let abandons = |upper: f64, lower: f64| {
            RangeOptions {
                upper: Some(upper),
                lower: Some(lower),
                ..RangeOptions::default()
            }
            .abandons()
        };
        assert!(abandons(f64::INFINITY, 0.0));
        assert!(abandons(1.0, f64::NEG_INFINITY));
        assert!(!abandons(f64::NEG_INFINITY, f64::INFINITY));
        assert!(!abandons(1e308, -1e308));
    }

    #[test]
    fn alternative_autoscale_and_logarithmic() {
        let data = [1.3333333333333333, 19.333333333333332];
        let (min, max) = range(&data, |o| o.alt_autoscale = true);
        assert!(
            (min + 0.4666666666666666).abs() < 1e-12 && (max - 21.133333333333333).abs() < 1e-12
        );
        let (min, max) = range(&data, |o| o.alt_autoscale_min = true);
        assert!((min + 0.4666666666666666).abs() < 1e-12 && max == data[1]);
        assert_eq!(range(&[0.0], |o| o.alt_autoscale = true), (-0.1, 1.1));
        assert_eq!(
            range(&[-70.0, 3.33, 56.0], |o| o.logarithmic = true),
            (1.665, 112.0)
        );
        assert_eq!(range(&[-1.0], |o| o.logarithmic = true), (2.55e-9, 10.2));
        // A limit past the extreme replaces it; otherwise it is halved or
        // doubled.
        let log = |upper: Option<f64>, lower: Option<f64>| {
            range(&[-70.0, 3.33, 56.0], |o| {
                o.logarithmic = true;
                o.upper = upper;
                o.lower = lower;
            })
        };
        assert_eq!(log(Some(100.0), None), (1.665, 100.0));
        assert_eq!(log(Some(50.0), Some(1.0)), (1.0, 112.0));
        assert_eq!(log(None, Some(5.0)), (1.665, 112.0));
        assert_eq!(
            range(&[-1.0], |o| {
                o.logarithmic = true;
                o.upper = Some(5.1);
                o.lower = Some(5.1e-9);
            }),
            (2.55e-9, 5.1)
        );
        assert_eq!(
            range(&[-70.0, 56.67], |o| o.y_grid = Some((2.0, 1))),
            (-70.0, 58.0)
        );
        assert_eq!(
            range(&[-70.0, 56.67], |o| o.y_grid = Some((0.5, 4))),
            (-70.0, 58.0)
        );
    }

    #[test]
    fn base_1024_powers() {
        assert_eq!(
            range(&[1.3333e6, 2.0e7], |o| o.base = 1024.0),
            (1.2 * 1_048_576.0, 20.0 * 1_048_576.0)
        );
    }
}
