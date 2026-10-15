//! `:valstrfduration` formatting (docs/compatibility/format-strings.md
//! FS-40 to FS-45 and FS-51).

use super::field::Field;
use super::host::host_number;
use super::operation_nan;
use super::spec::{Flags, Notation, NumberSpec, parse_digits};

/// Used when the format is empty (FS-44).
pub const DEFAULT_DURATION_FORMAT: &str = "%H:%02m:%02s";

/// RRDtool writes the result into a 200-byte buffer.
const MOST_BYTES: u64 = 199;

// Compatibility: once the untruncated text would reach INT_MAX bytes the
// C library's printf fails, and RRDtool then ends the whole command with
// no output and status 0.
// Provenance: black-box compatibility test (PRINT:v:|%2147483646H fails,
// PRINT:v:%2147483646H prints).
const MOST_UNCUT_BYTES: u64 = i32::MAX as u64 - 1;

/// Why a duration could not be formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DurationError {
    /// A conversion RRDtool rejects; the item fails with this message.
    #[error("Wrong duration format")]
    WrongFormat,
    /// The C library would fail on the size of the text. RRDtool then
    /// stops the command silently; the message is never shown.
    #[error("duration text too long")]
    TooLong,
}

/// The quantities a conversion letter can print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quantity {
    Weeks,
    Days,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
}

/// Duration conversions: a whole count, or the count within the next
/// larger unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    Whole(Quantity),
    Within(Quantity),
}

impl Count {
    fn from_letter(letter: u8) -> Option<Self> {
        Some(match letter {
            b'W' => Count::Whole(Quantity::Weeks),
            b'd' => Count::Within(Quantity::Days),
            b'D' => Count::Whole(Quantity::Days),
            b'h' => Count::Within(Quantity::Hours),
            b'H' => Count::Whole(Quantity::Hours),
            b'm' => Count::Within(Quantity::Minutes),
            b'M' => Count::Whole(Quantity::Minutes),
            b's' => Count::Within(Quantity::Seconds),
            b'S' => Count::Whole(Quantity::Seconds),
            b'f' => Count::Within(Quantity::Milliseconds),
            _ => return None,
        })
    }
}

/// A duration split into fractional units, each derived from the next
/// smaller one as RRDtool does, so rounding matches for huge values.
struct Units {
    millis: f64,
    seconds: f64,
    minutes: f64,
    hours: f64,
    days: f64,
    weeks: f64,
}

impl Units {
    fn new(millis: f64) -> Self {
        let seconds = millis / 1000.0;
        let minutes = seconds / 60.0;
        let hours = minutes / 60.0;
        let days = hours / 24.0;
        Self {
            millis,
            seconds,
            minutes,
            hours,
            days,
            weeks: days / 7.0,
        }
    }

    fn whole(&self, quantity: Quantity) -> f64 {
        match quantity {
            Quantity::Weeks => self.weeks,
            Quantity::Days => self.days,
            Quantity::Hours => self.hours,
            Quantity::Minutes => self.minutes,
            Quantity::Seconds => self.seconds,
            Quantity::Milliseconds => self.millis,
        }
    }

    /// The quantity minus the whole number of the next larger unit.
    fn within(&self, quantity: Quantity) -> f64 {
        let (larger, per) = match quantity {
            Quantity::Days => (self.weeks, 7.0),
            Quantity::Hours => (self.days, 24.0),
            Quantity::Minutes => (self.hours, 60.0),
            Quantity::Seconds => (self.minutes, 60.0),
            Quantity::Milliseconds => (self.seconds, 1000.0),
            Quantity::Weeks => return self.weeks,
        };
        subtract_product(self.whole(quantity), per, larger.floor())
    }

    fn count(&self, count: Count) -> f64 {
        match count {
            Count::Whole(quantity) => self.whole(quantity),
            Count::Within(quantity) => self.within(quantity),
        }
    }
}

// Compatibility: `small - per * whole` is evaluated with a fused
// multiply-add on aarch64 and with two roundings on x86_64, which changes
// the modulo fields of values near 1e20 ms (format-duration-048).
// Provenance: black-box compatibility test on macOS arm64, Linux aarch64
// and Linux x86_64.
fn subtract_product(small: f64, per: f64, whole: f64) -> f64 {
    if cfg!(target_arch = "aarch64") {
        (-per).mul_add(whole, small)
    } else {
        small - per * whole
    }
}

/// One `%[0][width][.precision]letter` conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Conversion {
    zero: bool,
    width: i32,
    precision: Option<u32>,
    count: Option<Count>,
}

/// A digit run read the way RRDtool reads it: as a C long that saturates,
/// then narrowed to a C int (keeping the low 32 bits).
fn c_int(digits: &[u8]) -> i32 {
    let (value, _) = parse_digits(digits);
    let long = i64::try_from(value).unwrap_or(i64::MAX);
    long as i32
}

/// Parses the conversion after a `%`; returns it and the bytes used.
fn parse_conversion(bytes: &[u8]) -> Result<(Conversion, usize), DurationError> {
    let mut at = 0;
    let zero = bytes.first() == Some(&b'0');
    if zero {
        at += 1;
    }
    let width_digits = bytes[at..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let width = c_int(&bytes[at..at + width_digits]);
    at += width_digits;
    let mut precision = None;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let digits = bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits > 0 {
            let value = c_int(&bytes[at..at + digits]);
            precision = Some(u32::try_from(value).map_err(|_| DurationError::WrongFormat)?);
        }
        at += digits;
    }
    let letter = *bytes.get(at).ok_or(DurationError::WrongFormat)?;
    let count = match letter {
        b'%' => None,
        _ => Some(Count::from_letter(letter).ok_or(DurationError::WrongFormat)?),
    };
    Ok((
        Conversion {
            zero,
            width,
            precision,
            count,
        },
        at + 1,
    ))
}

/// The number of one conversion, as `printf("%0*.*f")` would print it.
fn render_count(conversion: &Conversion, value: f64, decimal_point: &str) -> Field {
    let precision = conversion.precision.unwrap_or(0);
    // Compatibility: the fraction is cut, not rounded: floor(v * 10^p) /
    // 10^p, so a precision past about 308 prints nan or inf.
    // Provenance: black-box compatibility test (FS-42, %.308H prints inf).
    let scale = 10_f64.powf(f64::from(precision));
    let mut shown = (value * scale).floor() / scale;
    if shown.is_nan() {
        shown = operation_nan();
    }
    let mut spec = NumberSpec::new(Notation::Fixed);
    spec.precision = Some(u64::from(precision));
    spec.width = u64::from(conversion.width.unsigned_abs());
    spec.flags = Flags {
        left: conversion.width < 0,
        zero: conversion.zero,
        ..Flags::default()
    };
    host_number(&spec, shown, decimal_point)
}

/// Formats `millis` with a `valstrfduration` format. The result is at
/// most 199 bytes, as RRDtool's buffer allows. NaN and infinities print
/// as the C library prints them, whatever the format. `decimal_point` is
/// the locale's radix character.
pub fn format_duration(
    format: &str,
    millis: f64,
    decimal_point: &str,
) -> Result<Vec<u8>, DurationError> {
    if !millis.is_finite() {
        let spec = NumberSpec {
            precision: Some(0),
            ..NumberSpec::new(Notation::Fixed)
        };
        let mut out = Vec::new();
        host_number(&spec, millis, decimal_point).write_prefix(&mut out, MOST_BYTES);
        return Ok(out);
    }
    let format = if format.is_empty() {
        DEFAULT_DURATION_FORMAT
    } else {
        format
    };
    // Compatibility: a negative duration is formatted as its absolute
    // value with one '-' before the whole text (FS-43). Fractions of a
    // millisecond disappear only because every count is floored; `%.2f`
    // of 1234567.89 ms still prints 567.88.
    // Provenance: black-box compatibility test.
    let units = Units::new(millis.abs());
    let mut text = Field::new();
    if millis < 0.0 {
        text.push_str("-");
    }
    let bytes = format.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let next = format[at..]
            .find('%')
            .map_or(bytes.len(), |offset| at + offset);
        text.push_str(&format[at..next]);
        if next == bytes.len() {
            break;
        }
        let (conversion, used) = parse_conversion(&bytes[next + 1..])?;
        at = next + 1 + used;
        match conversion.count {
            None => text.push_str("%"),
            Some(count) => {
                if conversion.width == i32::MIN {
                    return Err(DurationError::TooLong);
                }
                text.append(render_count(&conversion, units.count(count), decimal_point));
            }
        }
        if text.len() > MOST_UNCUT_BYTES {
            return Err(DurationError::TooLong);
        }
    }
    if text.len() > MOST_UNCUT_BYTES {
        return Err(DurationError::TooLong);
    }
    let mut out = Vec::new();
    text.write_prefix(&mut out, MOST_BYTES);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{DurationError, format_duration};

    fn show(format: &str, millis: f64) -> String {
        String::from_utf8(format_duration(format, millis, ".").unwrap()).unwrap()
    }

    const ALL: &str = "%W/%d/%D/%h/%H/%m/%M/%s/%S/%f";

    // Expectations from the format-duration vectors (RRDtool 1.11.0).
    #[test]
    fn letters_and_moduli() {
        assert_eq!(show(ALL, 90_061_001.0), "0/1/1/1/25/1/1501/1/90061/1");
        assert_eq!(show(ALL, 694_861_001.0), "1/1/8/1/193/1/11581/1/694861/1");
        assert_eq!(show(ALL, 61_001.0), "0/0/0/0/0/1/1/1/61/1");
        assert_eq!(show(ALL, 1.5), "0/0/0/0/0/0/0/0/0/1");
        assert_eq!(show(ALL, 0.0004), "0/0/0/0/0/0/0/0/0/0");
        assert_eq!(
            show(ALL, 1e15),
            "1653439/1/11574074/1/277777777/46/16666666666/40/1000000000000/0"
        );
    }

    #[test]
    fn huge_values_follow_the_target_arithmetic() {
        let text = show(ALL, 1e20);
        let seconds = if cfg!(target_arch = "aarch64") {
            40
        } else {
            32
        };
        assert_eq!(
            text,
            format!(
                "165343915343/6/1157407407407/9/27777777777777/46/1666666666666666/{seconds}/100000000000000000/0"
            )
        );
    }

    #[test]
    fn width_precision_and_zero_flag() {
        assert_eq!(show("%5D", 90_061_001.0), "    1");
        assert_eq!(show("%05d", 90_061_001.0), "00001");
        assert_eq!(show("%00010D", 90_061_001.0), "0000000001");
        assert_eq!(show("%.3S", 90_061_001.0), "90061.001");
        assert_eq!(show("%.2S", 1999.0), "1.99");
        assert_eq!(show("%0.1H", 90_061_001.0), "25.0");
        assert_eq!(show("%.1M", 59_999.0), "0.9");
        assert_eq!(show("%05.2s", 90_061_001.0), "01.00");
        assert_eq!(show("%10.3M", 90_061_001.0), "  1501.016");
        assert_eq!(show("%.10H", 90_061_001.0), "25.0169447222");
        assert_eq!(show("%.H", 90_061_001.0), "25");
        assert_eq!(show("%5.H", 90_061_001.0), "   25");
        assert_eq!(show("%.0S", 1999.0), "1");
        assert_eq!(show("%8.3S %08.2f", 1_234_567.89), "1234.567 00567.88");
    }

    #[test]
    fn literal_text_and_percent() {
        assert_eq!(show("%%%D%%", 90_061_001.0), "%1%");
        assert_eq!(show("%5%", 90_061_001.0), "%");
        assert_eq!(show("x%.%y", 90_061_001.0), "x%y");
        assert_eq!(show("plain", 1.0), "plain");
        assert_eq!(show("", 90_061_001.0), "25:01:01");
    }

    #[test]
    fn negative_values_get_one_sign() {
        assert_eq!(show("x%D y%H", -90_061_001.0), "-x1 y25");
        assert_eq!(show("%f", -0.5), "-0");
        assert_eq!(show("%S %f", -1500.0), "-1 500");
        assert_eq!(show("%H", -0.0), "0");
    }

    #[test]
    fn rejected_conversions() {
        for format in [
            "%x", "%", "%lf", "%0", "%5", "%.", "%-5D", "%+D", "% H", "%..1H", "%1d%",
        ] {
            assert_eq!(
                format_duration(format, 1.0, "."),
                Err(DurationError::WrongFormat),
                "{format:?}"
            );
        }
        assert_eq!(
            format_duration("%.2147483648H", 1.0, "."),
            Err(DurationError::WrongFormat)
        );
    }

    #[test]
    fn widths_wrap_like_a_c_int() {
        assert_eq!(show("|%4294967297H|", 90_061_001.0), "|25|");
        assert_eq!(show("|%9223372036854775807H|", 90_061_001.0), "|25|");
        assert_eq!(show("|%.4294967297H|", 90_061_001.0), "|25.0|");
        let left = show("|%3000000000H|", 90_061_001.0);
        assert!(left.starts_with("|25    "));
        assert_eq!(left.len(), 199);
        assert_eq!(show("%99999999999D", 1.0), " ".repeat(199));
    }

    #[test]
    fn output_is_cut_and_overflow_is_silent() {
        assert_eq!(show("%0400D", 90_061_001.0), "0".repeat(199));
        assert_eq!(
            show(&format!("{}%060D", "x".repeat(150)), -90_061_001.0).len(),
            199
        );
        assert_eq!(show("%2147483646H", 1.0).len(), 199);
        for format in [
            "|%2147483646H",
            "%2147483647H",
            "%2147483648H",
            "%.1H%2147483646H",
        ] {
            assert_eq!(
                format_duration(format, 90_061_001.0, "."),
                Err(DurationError::TooLong),
                "{format:?}"
            );
        }
    }

    #[test]
    fn non_finite_values_print_as_numbers() {
        assert_eq!(show("%Q", f64::INFINITY), "inf");
        assert_eq!(show("x%D", f64::NEG_INFINITY), "-inf");
        let nan = show("%D", f64::NAN.copysign(1.0));
        assert_eq!(nan, "nan");
        assert_eq!(show("%.400H", 90_061_001.0).trim_start_matches('-'), "nan");
        assert_eq!(show("%.308H", 90_061_001.0), "inf");
    }

    #[test]
    fn locale_decimal_point() {
        let text = format_duration("%.3S", 90_061_001.0, ",").unwrap();
        assert_eq!(text, b"90061,001");
    }
}
