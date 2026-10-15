//! The number printer RRDtool uses for dump, xport data values and xport
//! PRINT entries.
//!
//! Unlike the host C library, this printer does not convert the binary
//! value exactly. Its observable behavior, established by black-box runs
//! of RRDtool 1.11.0 over thousands of values and formats on macOS arm64
//! and Linux x86_64 (see docs/compatibility/format-strings.md), is:
//!
//! - powers of ten come from repeated multiplication by ten (or division
//!   by ten for negative exponents) starting at one, so large ones are
//!   inexact;
//! - the decimal exponent is the largest such power not above the value,
//!   kept within -308..=308, and the mantissa is the value divided by it;
//! - digits after the point are the fraction times a power of ten, rounded
//!   half up, with a carry into the integer part; at most 19 of them are
//!   printed (20 significant digits for `%g`);
//! - the integer part must be below 2^64, otherwise the conversion prints
//!   nothing and ends the output;
//! - `-0` loses its sign, NaN is never signed, and the locale is ignored.

use std::sync::OnceLock;

use super::field::Field;
use super::host::{Digits, NanSign, assemble, exponent_suffix, non_finite, sign_text};
use super::spec::{Notation, NumberSpec};

const MOST_DECIMALS: u64 = 19;
const MOST_EXPONENT: i32 = 308;
const POWERS: usize = (2 * MOST_EXPONENT + 2) as usize;

/// `10^exponent` for exponent in -308..=309, built by repeated
/// multiplication or division from one.
fn power(exponent: i32) -> f64 {
    static TABLE: OnceLock<[f64; POWERS]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [1.0; POWERS];
        let zero = MOST_EXPONENT as usize;
        for step in 1..=MOST_EXPONENT as usize + 1 {
            table[zero + step] = table[zero + step - 1] * 10.0;
        }
        for step in 1..=MOST_EXPONENT as usize {
            table[zero - step] = table[zero - step + 1] / 10.0;
        }
        table
    });
    let index = (exponent.clamp(-MOST_EXPONENT, MOST_EXPONENT + 1) + MOST_EXPONENT) as usize;
    table[index]
}

/// The largest exponent in -308..=308 whose power does not exceed
/// `magnitude`, or -308 when every power does.
fn decimal_exponent(magnitude: f64) -> i32 {
    let (mut low, mut high) = (-MOST_EXPONENT, MOST_EXPONENT);
    if magnitude < power(low) {
        return low;
    }
    while low < high {
        let middle = low + (high - low + 1) / 2;
        if power(middle) <= magnitude {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    low
}

/// Integer part and rounded `decimals`-digit fraction of a magnitude, or
/// `None` when the integer part does not fit in 64 bits.
fn split(magnitude: f64, decimals: u64) -> Option<(u64, u64)> {
    if magnitude >= 18_446_744_073_709_551_616.0 {
        return None;
    }
    let integer = magnitude.trunc();
    let scale = power(decimals as i32);
    let scaled = (magnitude - integer) * scale;
    let mut fraction = scaled.trunc();
    if scaled - fraction >= 0.5 {
        fraction += 1.0;
    }
    // Both values are below 2^64 here: the fraction is under 10^19 + 1.
    let mut integer = integer as u64;
    let mut fraction = fraction as u64;
    let limit = scale as u64;
    if fraction >= limit {
        integer = integer.saturating_add(1);
        fraction -= limit;
    }
    Some((integer, fraction))
}

fn fixed_digits(magnitude: f64, decimals: u64, alternate: bool) -> Option<Digits> {
    let decimals = decimals.min(MOST_DECIMALS);
    let (integer, fraction) = split(magnitude, decimals)?;
    Some(Digits {
        integer: integer.to_string(),
        point: decimals > 0 || alternate,
        fraction: if decimals > 0 {
            format!("{fraction:0width$}", width = decimals as usize)
        } else {
            String::new()
        },
        fraction_zeros: 0,
        suffix: String::new(),
    })
}

fn exponent_digits(magnitude: f64, decimals: u64, alternate: bool, upper: bool) -> Option<Digits> {
    let decimals = decimals.min(MOST_DECIMALS);
    let mut exponent = if magnitude == 0.0 {
        0
    } else {
        decimal_exponent(magnitude)
    };
    let mut parts = split(magnitude / power(exponent), decimals)?;
    if parts.0 >= 10 {
        exponent += 1;
        parts = split(magnitude / power(exponent), decimals)?;
    }
    let mut digits = fixed_from(parts, decimals, alternate);
    digits.suffix = exponent_suffix(i64::from(exponent), upper);
    Some(digits)
}

fn fixed_from((integer, fraction): (u64, u64), decimals: u64, alternate: bool) -> Digits {
    Digits {
        integer: integer.to_string(),
        point: decimals > 0 || alternate,
        fraction: if decimals > 0 {
            format!("{fraction:0width$}", width = decimals as usize)
        } else {
            String::new()
        },
        fraction_zeros: 0,
        suffix: String::new(),
    }
}

fn general_digits(magnitude: f64, precision: u64, alternate: bool, upper: bool) -> Option<Digits> {
    let significant = precision.clamp(1, MOST_DECIMALS + 1);
    let mut exponent = 0;
    if magnitude != 0.0 {
        exponent = decimal_exponent(magnitude);
        if let Some((integer, _)) = split(magnitude / power(exponent), significant - 1)
            && integer >= 10
        {
            exponent += 1;
        }
    }
    let exponent = i64::from(exponent);
    let mut digits = if exponent < -4 || exponent >= significant as i64 {
        exponent_digits(magnitude, significant - 1, alternate, upper)?
    } else {
        let decimals = (significant as i64 - 1 - exponent) as u64;
        fixed_digits(magnitude, decimals, alternate)?
    };
    if !alternate {
        let kept = digits.fraction.trim_end_matches('0').len();
        digits.fraction.truncate(kept);
        digits.point = !digits.fraction.is_empty();
    }
    Some(digits)
}

/// Formats `value` with `spec` as RRDtool's own printf does. `None` means
/// the conversion failed: RRDtool then stops the output before it.
pub fn scaled_number(spec: &NumberSpec, value: f64) -> Option<Field> {
    if !value.is_finite() {
        return Some(non_finite(spec, value, NanSign::Positive));
    }
    let magnitude = value.abs();
    let precision = spec.precision.unwrap_or(6);
    let alternate = spec.flags.alternate;
    let digits = match spec.notation {
        Notation::Fixed => fixed_digits(magnitude, precision, alternate)?,
        Notation::Exponent => exponent_digits(magnitude, precision, alternate, spec.upper)?,
        Notation::General => general_digits(magnitude, precision, alternate, spec.upper)?,
    };
    let mut body = Field::new();
    digits.write(&mut body, ".");
    Some(assemble(
        sign_text(value < 0.0, spec.flags),
        body,
        spec.width,
        spec.flags,
    ))
}

/// A data value as dump and xport write it: `%0.10e` in RRDtool's own
/// printf.
pub fn data_value(value: f64) -> String {
    const SPEC: NumberSpec = {
        let mut spec = NumberSpec::new(Notation::Exponent);
        spec.flags.zero = true;
        spec.precision = Some(10);
        spec
    };
    scaled_number(&SPEC, value)
        .and_then(|field| field.to_string_within(64))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{data_value, decimal_exponent, power, scaled_number};
    use crate::format_value::spec::NumberSpec;

    fn show(format: &str, value: f64) -> Option<String> {
        let spec = NumberSpec::parse(format).unwrap();
        scaled_number(&spec, value).map(|field| field.to_string_within(1 << 16).unwrap())
    }

    // Expected texts below were printed by RRDtool 1.11.0 through xport
    // PRINT and dump.
    #[test]
    fn rounds_half_up_and_caps_precision() {
        assert_eq!(show("%.0lf", 0.5).unwrap(), "1");
        assert_eq!(show("%.0lf", 2.5).unwrap(), "3");
        assert_eq!(show("%010.2lf", 0.125).unwrap(), "0000000.13");
        assert_eq!(show("%.20lf", 0.0).unwrap(), "0.0000000000000000000");
        assert_eq!(
            show("%.20lf", 1234.5678).unwrap(),
            "1234.5678000000000337920"
        );
        assert_eq!(
            show("%.30le", 1234.5678).unwrap(),
            "1.2345678000000000000e+03"
        );
    }

    #[test]
    fn negative_zero_and_specials() {
        assert_eq!(show("%lf", -0.0).unwrap(), "0.000000");
        assert_eq!(show("%+lf", -0.0).unwrap(), "+0.000000");
        assert_eq!(show("%+lf", -f64::NAN).unwrap(), "+nan");
        assert_eq!(show("%010.2lf", f64::NAN).unwrap(), "       nan");
        assert_eq!(show("%010.2lf", f64::NEG_INFINITY).unwrap(), "      -inf");
        assert_eq!(show("%lE", f64::INFINITY).unwrap(), "INF");
    }

    #[test]
    fn exponents_stop_at_minus_308() {
        assert_eq!(show("%le", 1e-310).unwrap(), "0.010000e-308");
        assert_eq!(show("%lg", 5e-324).unwrap(), "0e-308");
        assert_eq!(
            show("%.30le", 5e-324).unwrap(),
            "0.0000000000000004941e-308"
        );
        assert_eq!(show("%le", 1.7e308).unwrap(), "1.700000e+308");
        assert_eq!(decimal_exponent(1e-320), -308);
        assert_eq!(decimal_exponent(1.0), 0);
        assert_eq!(decimal_exponent(9.999), 0);
        assert!(power(309).is_infinite());
    }

    #[test]
    fn fixed_layout_fails_from_two_to_the_64() {
        let below = 18_446_744_073_709_549_568.0;
        assert_eq!(show("%lf", below).unwrap(), "18446744073709549568.000000");
        assert_eq!(show("%lf", 18_446_744_073_709_551_616.0), None);
        assert_eq!(
            show("%le", 18_446_744_073_709_551_616.0).unwrap(),
            "1.844674e+19"
        );
        assert_eq!(show("%lg", 1e21).unwrap(), "1e+21");
        assert_eq!(show("%.20lg", 2.384_705_331_948_161_4e19), None);
    }

    #[test]
    fn general_layout_keeps_twenty_digits() {
        assert_eq!(
            show("%30.20lg", 67508.5245).unwrap().trim(),
            "67508.524499999999534"
        );
        assert_eq!(show("%#lg", 0.0).unwrap(), "0.00000");
        assert_eq!(show("%#.0lg", 0.0).unwrap(), "0.");
        assert_eq!(show("%.0le", 1.5e308).unwrap(), "2e+308");
    }

    #[test]
    fn data_values_use_ten_decimals() {
        assert_eq!(data_value(1.0), "1.0000000000e+00");
        assert_eq!(data_value(-0.0), "0.0000000000e+00");
        assert_eq!(data_value(f64::NAN), "nan");
        assert_eq!(data_value(-5.0), "-5.0000000000e+00");
    }
}
