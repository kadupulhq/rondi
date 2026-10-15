//! Floating-point conversions as the host C library prints them.
//!
//! Graph PRINT and GPRINT text, `graphv` values and `valstrfduration`
//! numbers come from the C library's `printf`, so they carry its rules:
//! correctly rounded digits (ties to even), the locale's decimal point,
//! `nan`/`inf` padded with spaces, and a platform-specific NaN sign.

use super::field::Field;
use super::spec::{Flags, Notation, NumberSpec};

/// How the host C library shows the sign of a NaN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NanSign {
    /// Never signed; `+` and ` ` are ignored for NaN (macOS).
    Hidden,
    /// Signed like any number, from the NaN's sign bit (glibc).
    FromSignBit,
    /// Treated as positive: `+` and ` ` apply, `-` never shows (RRDtool's
    /// own printf).
    Positive,
}

// Compatibility: macOS prints every NaN as `nan`; glibc prints `-nan` when
// the sign bit is set and honors `+` and ` ` (format-print-261 to -285).
// Provenance: black-box compatibility test; docs/compatibility/format-strings.md FS-14.
pub const HOST_NAN_SIGN: NanSign = if cfg!(target_os = "macos") {
    NanSign::Hidden
} else {
    NanSign::FromSignBit
};

// A double's exact decimal expansion ends within 1074 fractional digits
// and 767 significant digits; past these limits every digit is zero.
const EXACT_FRACTION_DIGITS: u64 = 1100;
const EXACT_SIGNIFICANT_DIGITS: u64 = 800;

/// Formats `value` with `spec` as the host C library would, using
/// `decimal_point` for the radix character.
pub fn host_number(spec: &NumberSpec, value: f64, decimal_point: &str) -> Field {
    if !value.is_finite() {
        return non_finite(spec, value, HOST_NAN_SIGN);
    }
    let sign = sign_text(value.is_sign_negative(), spec.flags);
    let magnitude = value.abs();
    let precision = spec.precision.unwrap_or(6);
    let digits = match spec.notation {
        Notation::Fixed => fixed_digits(magnitude, precision, spec.flags.alternate),
        Notation::Exponent => {
            exponent_digits(magnitude, precision, spec.flags.alternate, spec.upper)
        }
        Notation::General => general_digits(magnitude, precision, spec.flags.alternate, spec.upper),
    };
    let mut body = Field::new();
    digits.write(&mut body, decimal_point);
    assemble(sign, body, spec.width, spec.flags)
}

/// Formats `value` with a C format holding exactly one floating-point
/// conversion, such as `%0.10e` or `x=%.0f`; `%%` prints `%`. A format
/// without a usable conversion is returned with only `%%` collapsed.
pub fn printf_number(format: &str, value: f64, decimal_point: &str) -> String {
    let bytes = format.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    let mut converted = false;
    while at < bytes.len() {
        let next = format[at..]
            .find('%')
            .map_or(bytes.len(), |offset| at + offset);
        out.push_str(&format[at..next]);
        if next == bytes.len() {
            break;
        }
        if bytes.get(next + 1) == Some(&b'%') {
            out.push('%');
            at = next + 2;
            continue;
        }
        match NumberSpec::parse_prefix(&bytes[next + 1..], true) {
            Some((spec, used)) if !converted => {
                let field = host_number(&spec, value, decimal_point);
                out.push_str(&field.to_string_within(u64::MAX).unwrap_or_default());
                converted = true;
                at = next + 1 + used;
            }
            _ => {
                out.push('%');
                at = next + 1;
            }
        }
    }
    out
}

pub(crate) fn sign_text(negative: bool, flags: Flags) -> &'static str {
    if negative {
        "-"
    } else if flags.plus {
        "+"
    } else if flags.space {
        " "
    } else {
        ""
    }
}

/// `nan`/`inf` text, padded with spaces whatever the `0` flag says.
pub(crate) fn non_finite(spec: &NumberSpec, value: f64, nan_sign: NanSign) -> Field {
    let word = match (value.is_nan(), spec.upper) {
        (true, false) => "nan",
        (true, true) => "NAN",
        (false, false) => "inf",
        (false, true) => "INF",
    };
    let sign = match (value.is_nan(), nan_sign) {
        (true, NanSign::Hidden) => "",
        (true, NanSign::Positive) => sign_text(false, spec.flags),
        _ => sign_text(value.is_sign_negative(), spec.flags),
    };
    let mut field = Field::new();
    field.push_str(sign);
    field.push_str(word);
    field.pad(spec.width, spec.flags.left)
}

/// Sign and digits padded to `width`: zeros go between them under the `0`
/// flag (unless `-` is also given), spaces go outside otherwise.
pub(crate) fn assemble(sign: &str, body: Field, width: u64, flags: Flags) -> Field {
    let used = body.len().saturating_add(sign.len() as u64);
    if flags.zero && !flags.left && width > used {
        let mut field = Field::new();
        field.push_str(sign);
        field.push_run(b'0', width - used);
        field.append(body);
        return field;
    }
    let mut field = Field::new();
    field.push_str(sign);
    field.append(body);
    field.pad(width, flags.left)
}

/// The digits of a finite magnitude, split so that long zero tails stay
/// counted rather than spelled out.
#[derive(Debug, Default)]
pub(crate) struct Digits {
    pub integer: String,
    pub point: bool,
    pub fraction: String,
    pub fraction_zeros: u64,
    pub suffix: String,
}

impl Digits {
    pub(crate) fn write(&self, field: &mut Field, decimal_point: &str) {
        field.push_str(&self.integer);
        if self.point {
            field.push_str(decimal_point);
        }
        field.push_str(&self.fraction);
        field.push_run(b'0', self.fraction_zeros);
        field.push_str(&self.suffix);
    }

    /// `%g` without `#`: no trailing fractional zeros, and no point when
    /// nothing follows it.
    fn trim(&mut self) {
        self.fraction_zeros = 0;
        let kept = self.fraction.trim_end_matches('0').len();
        self.fraction.truncate(kept);
        self.point = !self.fraction.is_empty();
    }
}

fn fixed_digits(magnitude: f64, precision: u64, alternate: bool) -> Digits {
    let shown = precision.min(EXACT_FRACTION_DIGITS) as usize;
    let text = format!("{magnitude:.shown$}");
    let (integer, fraction) = text.split_once('.').unwrap_or((&text, ""));
    Digits {
        integer: integer.to_owned(),
        point: precision > 0 || alternate,
        fraction: fraction.to_owned(),
        fraction_zeros: precision - shown as u64,
        suffix: String::new(),
    }
}

/// Rust's `{:e}` text split into mantissa and decimal exponent.
fn scientific(magnitude: f64, precision: u64) -> (String, String, u64, i64) {
    let shown = precision.min(EXACT_SIGNIFICANT_DIGITS) as usize;
    let text = format!("{magnitude:.shown$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let (lead, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    (
        lead.to_owned(),
        fraction.to_owned(),
        precision - shown as u64,
        exponent.parse().unwrap_or(0),
    )
}

pub(crate) fn exponent_suffix(exponent: i64, upper: bool) -> String {
    let letter = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{letter}{sign}{:02}", exponent.unsigned_abs())
}

fn exponent_digits(magnitude: f64, precision: u64, alternate: bool, upper: bool) -> Digits {
    let (lead, fraction, zeros, exponent) = scientific(magnitude, precision);
    Digits {
        integer: lead,
        point: precision > 0 || alternate,
        fraction,
        fraction_zeros: zeros,
        suffix: exponent_suffix(exponent, upper),
    }
}

fn general_digits(magnitude: f64, precision: u64, alternate: bool, upper: bool) -> Digits {
    let significant = precision.max(1);
    let (_, _, _, exponent) = scientific(magnitude, significant - 1);
    let mut digits = if exponent >= -4 && i128::from(exponent) < i128::from(significant) {
        let decimals = i128::from(significant) - 1 - i128::from(exponent);
        fixed_digits(
            magnitude,
            u64::try_from(decimals).unwrap_or(u64::MAX),
            alternate,
        )
    } else {
        exponent_digits(magnitude, significant - 1, alternate, upper)
    };
    if !alternate {
        digits.trim();
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::{NanSign, host_number, non_finite, printf_number};
    use crate::format_value::spec::NumberSpec;

    fn show(format: &str, value: f64) -> String {
        let spec = NumberSpec::parse(format).unwrap();
        host_number(&spec, value, ".")
            .to_string_within(1 << 20)
            .unwrap()
    }

    #[test]
    fn fixed_digits_are_correctly_rounded() {
        assert_eq!(show("%lf", 1.0), "1.000000");
        assert_eq!(show("%.0lf", 0.5), "0");
        assert_eq!(show("%.0lf", 1.5), "2");
        assert_eq!(show("%.3lf", 999.9995), "1000.000");
        assert_eq!(show("%.20lf", 1234.5678), "1234.56780000000003383320");
        assert_eq!(show("%.0lf", -0.0), "-0");
        assert_eq!(show("%#.0lf", -0.0), "-0.");
        assert_eq!(show("%.1200f", 0.5).len(), 1202);
    }

    #[test]
    fn exponent_and_general_layouts() {
        assert_eq!(show("%le", 1234.5), "1.234500e+03");
        assert_eq!(show("%.0E", 5e-324), "5E-324");
        assert_eq!(show("%#.0e", 2.0), "2.e+00");
        assert_eq!(show("%lg", 0.0001), "0.0001");
        assert_eq!(show("%lg", 0.00001), "1e-05");
        assert_eq!(show("%lg", 123456.0), "123456");
        assert_eq!(show("%lg", 1234567.0), "1.23457e+06");
        assert_eq!(show("%#lg", 1.0), "1.00000");
        assert_eq!(show("%.0lg", 0.0), "0");
        assert_eq!(show("%G", 1e-10), "1E-10");
        assert_eq!(show("%.900e", 1.0).len(), 906);
    }

    #[test]
    fn flags_and_width() {
        assert_eq!(show("%+lf", 1.0), "+1.000000");
        assert_eq!(show("% lf", 1.0), " 1.000000");
        assert_eq!(show("%010.2lf", -1.5), "-000001.50");
        assert_eq!(show("%-8.1lf", 1.0), "1.0     ");
        assert_eq!(show("%-08.1lf", 1.0), "1.0     ");
        assert_eq!(show("%8.1lf", 1.0), "     1.0");
    }

    #[test]
    fn non_finite_values_ignore_zero_padding() {
        assert_eq!(show("%05lf", f64::INFINITY), "  inf");
        assert_eq!(show("%-6lF", f64::NEG_INFINITY), "-INF  ");
        assert_eq!(show("%+lf", f64::INFINITY), "+inf");
        let spec = NumberSpec::parse("%+5lf").unwrap();
        let nan = -f64::NAN;
        let text = |sign| non_finite(&spec, nan, sign).to_string_within(64).unwrap();
        assert_eq!(text(NanSign::Hidden), "  nan");
        assert_eq!(text(NanSign::FromSignBit), " -nan");
        let positive = non_finite(&spec, f64::NAN.copysign(1.0), NanSign::FromSignBit);
        assert_eq!(positive.to_string_within(64).unwrap(), " +nan");
    }

    #[test]
    fn decimal_point_comes_from_the_caller() {
        let spec = NumberSpec::parse("%.2f").unwrap();
        let text = host_number(&spec, 2.5, ",").to_string_within(64).unwrap();
        assert_eq!(text, "2,50");
    }

    #[test]
    fn printf_number_handles_surrounding_text() {
        assert_eq!(printf_number("%0.10e", 1.0, "."), "1.0000000000e+00");
        assert_eq!(printf_number("x=%.0f%%", 2.5, "."), "x=2%");
        assert_eq!(printf_number("%d", 2.5, "."), "%d");
        assert_eq!(printf_number("%f %f", 2.0, "."), "2.000000 %f");
    }
}
