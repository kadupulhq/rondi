//! The amount a reading contributes to a data source (UP-10 to UP-18).

use std::cmp::Ordering;

use crate::rrd_number::{parse_rrd_number, rrd_strtodbl};

/// Width of the stored-text field of a data source.
const STORED_LEN: usize = 30;

/// Data source types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DsKind {
    Gauge,
    Counter,
    Derive,
    Absolute,
    DCounter,
    DDerive,
    Compute,
}

impl DsKind {
    /// Parse a raw, possibly NUL-padded, type field.
    pub(crate) fn from_raw(raw_kind: &[u8]) -> Option<DsKind> {
        let end = raw_kind
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(raw_kind.len());
        match &raw_kind[..end] {
            b"GAUGE" => Some(DsKind::Gauge),
            b"COUNTER" => Some(DsKind::Counter),
            b"DERIVE" => Some(DsKind::Derive),
            b"ABSOLUTE" => Some(DsKind::Absolute),
            b"DCOUNTER" => Some(DsKind::DCounter),
            b"DDERIVE" => Some(DsKind::DDerive),
            b"COMPUTE" => Some(DsKind::Compute),
            _ => None,
        }
    }

    /// The name used in messages and files.
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            DsKind::Gauge => "GAUGE",
            DsKind::Counter => "COUNTER",
            DsKind::Derive => "DERIVE",
            DsKind::Absolute => "ABSOLUTE",
            DsKind::DCounter => "DCOUNTER",
            DsKind::DDerive => "DDERIVE",
            DsKind::Compute => "COMPUTE",
        }
    }
}

/// The stored text kept after a reading is processed: the first 29 bytes,
/// NUL-padded to 30.
// Provenance: specification (docs/compatibility/update.md UP-13).
pub(crate) fn stored_text(reading: &str) -> [u8; STORED_LEN] {
    let mut field = [0u8; STORED_LEN];
    let bytes = reading.as_bytes();
    let len = bytes.len().min(STORED_LEN - 1);
    field[..len].copy_from_slice(&bytes[..len]);
    field
}

/// The amount of one reading, or `None` when it is unknown.
///
/// `stored` is the data source's stored-text field; `interval` is in seconds.
/// Errors carry the message without the file-name prefix.
pub(crate) fn amount(
    kind: DsKind,
    reading: &str,
    stored: &[u8],
    interval: f64,
    heartbeat: u64,
    minimum: Option<f64>,
    maximum: Option<f64>,
) -> Result<Option<f64>, String> {
    // COMPUTE sources are never read from arguments.
    if kind == DsKind::Compute {
        return Ok(None);
    }
    // Provenance: specification (docs/compatibility/update.md UP-10).
    if interval > heartbeat as f64 || reading.starts_with('U') {
        return Ok(None);
    }

    let stored = stored_str(stored);
    let (value, rate_basis) = match kind {
        DsKind::Gauge => {
            let x = convert(kind, reading)?;
            // Compatibility: a gauge range check compares the reading itself.
            // Provenance: black-box compatibility test (range-gauge-reading).
            (x * interval, Some(x))
        }
        DsKind::Absolute => (convert(kind, reading)?, None),
        DsKind::Counter => {
            if !reading.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("not a simple unsigned integer: '{reading}'"));
            }
            let mut d = decimal_text_difference(reading, &stored);
            // Compatibility: the wrap constants are not the counter's range.
            // Provenance: specification (docs/compatibility/update.md UP-16).
            if d < 0.0 {
                d += 4294967295.0;
                if d < 0.0 {
                    d += 18446744069414584320.0;
                }
            }
            (d, None)
        }
        DsKind::Derive => {
            let digits = reading.strip_prefix('-').unwrap_or(reading);
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("not a simple signed integer: '{reading}'"));
            }
            (decimal_text_difference(reading, &stored), None)
        }
        DsKind::DCounter | DsKind::DDerive => {
            if stored.starts_with('U') {
                return Ok(None);
            }
            let new = convert(kind, reading)?;
            let old = convert(kind, &stored)?;
            let d = new - old;
            // Compatibility: a DCOUNTER change is unknown when it points away
            // from zero relative to the new reading's sign: a decrease to a
            // positive reading, or an increase to a negative one. A reading
            // of zero, or an unchanged reading, is accepted either way.
            // Provenance: specification (docs/compatibility/update.md UP-17).
            if kind == DsKind::DCounter && ((d < 0.0 && new > 0.0) || (d > 0.0 && new < 0.0)) {
                return Ok(None);
            }
            (d, None)
        }
        DsKind::Compute => unreachable!("handled above"),
    };

    if value.is_nan() {
        return Ok(None);
    }
    // Provenance: specification (docs/compatibility/update.md UP-18).
    let rate = rate_basis.unwrap_or(value / interval);
    let below = minimum.is_some_and(|m| !m.is_nan() && rate < m);
    let above = maximum.is_some_and(|m| !m.is_nan() && rate > m);
    Ok((!below && !above).then_some(value))
}

/// The stored text up to its first NUL.
fn stored_str(stored: &[u8]) -> String {
    let end = stored.iter().position(|&b| b == 0).unwrap_or(stored.len());
    String::from_utf8_lossy(&stored[..end]).into_owned()
}

/// `reading − stored` for two integer texts, formed exactly in decimal and
/// then converted by the decimal conversion (NM-5); NaN when unknown.
// Provenance: specification (docs/compatibility/update.md UP-16,
// docs/compatibility/numbers.md NM-5 and NM-7).
fn decimal_text_difference(reading: &str, stored: &str) -> f64 {
    if reading.len() > STORED_LEN {
        return f64::NAN;
    }
    let (Some((reading_negative, reading_digits)), Some((stored_negative, stored_digits))) =
        (split_integer_text(reading), split_integer_text(stored))
    else {
        return f64::NAN;
    };
    if reading_negative != stored_negative {
        return f64::NAN;
    }
    let reading_digits = reading_digits.trim_start_matches('0');
    let stored_digits = stored_digits.trim_start_matches('0');
    // With equal signs, reading − stored has the sign of the magnitude
    // difference, flipped when both texts are negative. Two equal negative
    // texts give `-0`, which converts to negative zero.
    let (magnitude, magnitude_negative) = match compare_magnitudes(reading_digits, stored_digits) {
        Ordering::Less => (subtract_magnitudes(stored_digits, reading_digits), true),
        Ordering::Equal | Ordering::Greater => {
            (subtract_magnitudes(reading_digits, stored_digits), false)
        }
    };
    let negative = magnitude_negative != reading_negative;
    let text = if negative {
        format!("-{magnitude}")
    } else {
        magnitude
    };
    parse_rrd_number(&text).unwrap_or(f64::NAN)
}

/// An optional `-` followed by at least one decimal digit: the sign and the
/// digits.
fn split_integer_text(text: &str) -> Option<(bool, &str)> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some((negative, digits))
}

/// Order of two digit strings without leading zeros.
fn compare_magnitudes(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `larger − smaller` for digit strings without leading zeros, where
/// `larger` is not less than `smaller`; the result has no leading zeros
/// (`0` when they are equal).
fn subtract_magnitudes(larger: &str, smaller: &str) -> String {
    let mut borrow = 0;
    let mut smaller_digits = smaller.bytes().rev();
    let mut result: Vec<u8> = larger
        .bytes()
        .rev()
        .map(|digit| {
            let subtrahend = smaller_digits.next().map_or(0, |d| d - b'0') + borrow;
            let minuend = digit - b'0';
            borrow = u8::from(minuend < subtrahend);
            b'0' + minuend + 10 * borrow - subtrahend
        })
        .collect();
    while result.len() > 1 && result.last() == Some(&b'0') {
        result.pop();
    }
    if result.is_empty() {
        result.push(b'0');
    }
    result.reverse();
    String::from_utf8(result).expect("ASCII digits")
}

/// Floating conversion of UP-11. Prefix checks look at the reading as given;
/// only then are C-locale blanks skipped for the numeric conversion.
fn convert(kind: DsKind, text: &str) -> Result<f64, String> {
    let bytes = text.as_bytes();
    let starts = |prefix: &str| {
        bytes
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    };
    if starts("nan") || starts("-nan") {
        return Ok(f64::NAN);
    }
    if starts("inf") {
        return Ok(f64::INFINITY);
    }
    if starts("-inf") {
        return Ok(f64::NEG_INFINITY);
    }
    let context = format!("Function update_pdp_prep, case DST_{}", kind.type_name());
    let blanks = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r');
    // Messages quote the reading as given, so retarget them after converting
    // the blank-trimmed text.
    let trimmed = text.trim_start_matches(blanks);
    // After blanks, a nan/inf spelling is not special: the numeric conversion
    // finds no number there. Provenance: specification
    // (docs/compatibility/update.md UP-11).
    let lower = trimmed.to_ascii_lowercase();
    if text.len() != trimmed.len()
        && ["nan", "-nan", "inf", "-inf"]
            .iter()
            .any(|special| lower.starts_with(special))
    {
        return Err(format!("{context} - Cannot convert '{text}' to float"));
    }
    rrd_strtodbl(trimmed, &context).map_err(|message| {
        let lead = &text[..text.len() - trimmed.len()];
        message.replacen(&format!("'{trimmed}'"), &format!("'{lead}{trimmed}'"), 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(kind: DsKind, stored: &str, reading: &str) -> Result<Option<f64>, String> {
        amount(kind, reading, &stored_text(stored), 1.0, 600, None, None)
    }

    #[test]
    fn counter_wraps_at_32_bits_then_64() {
        let cases = [
            ("4294967296", "5", 4.0),
            ("4294967295", "0", 0.0),
            ("18446744073709551615", "5", 0.0),
        ];
        for (stored, reading, expected) in cases {
            assert_eq!(
                run(DsKind::Counter, stored, reading),
                Ok(Some(expected)),
                "{stored} -> {reading}"
            );
        }
    }

    #[test]
    fn difference_of_equal_readings_is_zero() {
        for text in ["0", "7", "000123", "99999999999999999999999"] {
            let x = decimal_text_difference(text, text);
            assert!(x == 0.0 && x.is_sign_positive(), "{text}");
        }
        let x = decimal_text_difference("-0012", "-12");
        assert!(x == 0.0 && x.is_sign_negative());
    }

    #[test]
    fn difference_handles_readings_of_different_lengths() {
        assert_eq!(decimal_text_difference("1000", "999"), 1.0);
        assert_eq!(decimal_text_difference("999", "1000"), -1.0);
        assert_eq!(decimal_text_difference("12345", "7"), 12338.0);
        assert_eq!(decimal_text_difference("7", "12345"), -12338.0);
        assert_eq!(decimal_text_difference("0005", "3"), 2.0);
        assert_eq!(run(DsKind::Counter, "9", "10"), Ok(Some(1.0)));
    }

    #[test]
    fn difference_of_long_readings_is_exact_before_conversion() {
        // 2^70 + 1 − 2^70 is 1 only when subtracted exactly.
        assert_eq!(
            decimal_text_difference("1180591620717411303425", "1180591620717411303424"),
            1.0
        );
        assert_eq!(
            decimal_text_difference(
                "100000000000000000000000000000",
                "99999999999999999999999999999"
            ),
            1.0
        );
        assert_eq!(
            decimal_text_difference("123456789012345678901234567", "0"),
            parse_rrd_number("123456789012345678901234567").unwrap()
        );
        // UP-16: a reading longer than 30 characters is unknown.
        assert!(decimal_text_difference(&"1".repeat(31), "0").is_nan());
        assert!(!decimal_text_difference(&"1".repeat(30), "0").is_nan());
        assert!(decimal_text_difference(&format!("-{}", "1".repeat(30)), "-1").is_nan());
    }

    #[test]
    fn derive_negative_readings_take_the_true_sign() {
        // Provenance: black-box compatibility test (numbers.md NM-7).
        let cases = [
            ("-5", "-3", 2.0),
            ("-3", "-5", -2.0),
            ("-0012", "-007", 5.0),
            ("-7", "-0", 7.0),
            ("-0", "-7", -7.0),
            ("5", "3", -2.0),
        ];
        for (stored, reading, expected) in cases {
            assert_eq!(
                run(DsKind::Derive, stored, reading),
                Ok(Some(expected)),
                "{stored} -> {reading}"
            );
        }
        assert_eq!(
            run(DsKind::Derive, "-123456789012345678901234567", "-1"),
            Ok(Some(
                parse_rrd_number("123456789012345678901234566").unwrap()
            ))
        );
    }

    #[test]
    fn difference_is_unknown_for_mixed_signs_or_non_integers() {
        for (reading, stored) in [
            ("5", "-5"),
            ("-5", "5"),
            ("0", "-0"),
            ("5", "U"),
            ("5", ""),
            ("", "5"),
            ("-", "-5"),
            ("5", "1.5"),
        ] {
            assert!(
                decimal_text_difference(reading, stored).is_nan(),
                "{stored} -> {reading}"
            );
        }
    }

    #[test]
    fn counter_wrap_edges() {
        let cases = [
            // One below the first constant still wraps to a positive value.
            ("4294967294", "0", 1.0),
            ("4294967296", "0", -1.0 + 18446744069414584320.0),
            ("1", "0", 4294967294.0),
            ("0", "0", 0.0),
            // Past 2^64 − 1 the second step still lands short of zero.
            ("18446744073709551616", "0", 0.0),
        ];
        for (stored, reading, expected) in cases {
            assert_eq!(
                run(DsKind::Counter, stored, reading),
                Ok(Some(expected)),
                "{stored} -> {reading}"
            );
        }
    }

    #[test]
    fn derive_negative_minus_zero() {
        let x = run(DsKind::Derive, "-5", "-5").unwrap().unwrap();
        assert!(x == 0.0 && x.is_sign_negative());
    }

    #[test]
    fn counter_long_difference_rounds_by_rs38() {
        // Compatibility: the conversion rounds with the platform's
        // multiply-add (restore.md RS-39), fused on aarch64 only.
        // Provenance: black-box compatibility test (RRDtool 1.11.0 on macOS
        // arm64, Linux aarch64 and Linux x86_64, step-1 COUNTER 0 then this
        // reading, stored value read from the file).
        let expected = if cfg!(target_arch = "aarch64") {
            1.234567890123457e22
        } else {
            1.2345678901234568e22
        };
        assert_eq!(
            run(DsKind::Counter, "0", "12345678901234567890123"),
            Ok(Some(expected))
        );
    }

    #[test]
    fn unknown_prefix_and_heartbeat_skip_parsing() {
        let stored = stored_text("0");
        let go = |reading, interval| {
            amount(DsKind::Counter, reading, &stored, interval, 600, None, None)
        };
        assert_eq!(go("Uxyz", 1.0), Ok(None));
        assert_eq!(go("zz", 601.0), Ok(None));
        assert!(go("zz", 600.0).is_err());
    }

    #[test]
    fn counter_rejects_non_digits() {
        assert_eq!(
            run(DsKind::Counter, "0", "1.5"),
            Err("not a simple unsigned integer: '1.5'".to_string())
        );
    }

    #[test]
    fn derive_rejects_plus() {
        assert_eq!(
            run(DsKind::Derive, "0", "+5"),
            Err("not a simple signed integer: '+5'".to_string())
        );
    }

    #[test]
    fn gauge_conversion_messages() {
        assert_eq!(
            run(DsKind::Gauge, "U", "12abc"),
            Err("Function update_pdp_prep, case DST_GAUGE - Converted '12abc' to 12.000000, but cannot convert 'abc'".to_string())
        );
        let cannot = |t: &str| {
            format!("Function update_pdp_prep, case DST_GAUGE - Cannot convert '{t}' to float")
        };
        assert_eq!(run(DsKind::Gauge, "U", "e5"), Err(cannot("e5")));
        assert_eq!(run(DsKind::Gauge, "U", "1e2000"), Err(cannot("1e2000")));
    }

    #[test]
    fn gauge_specials() {
        assert_eq!(run(DsKind::Gauge, "U", "nanx"), Ok(None));
        let stored = stored_text("U");
        assert_eq!(
            amount(DsKind::Gauge, "Inf5", &stored, 2.0, 600, None, None),
            Ok(Some(f64::INFINITY))
        );
        assert_eq!(run(DsKind::Gauge, "U", "1e-400"), Ok(Some(0.0)));
        assert_eq!(run(DsKind::Gauge, "U", "0e600"), Ok(None));
    }

    #[test]
    fn gauge_range_uses_reading() {
        let stored = stored_text("U");
        assert_eq!(
            amount(DsKind::Gauge, "0.1", &stored, 3.0, 600, None, Some(0.1)),
            Ok(Some(0.1 * 3.0))
        );
        assert!(
            amount(DsKind::Gauge, "0.7", &stored, 3.0, 600, Some(0.7), None)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn absolute_range_uses_rate() {
        let stored = stored_text("U");
        let go = |r| amount(DsKind::Absolute, r, &stored, 10.0, 600, None, Some(1.0));
        assert_eq!(go("10"), Ok(Some(10.0)));
        assert_eq!(go("15"), Ok(None));
    }

    #[test]
    fn counter_range_uses_rate() {
        let stored = stored_text("0");
        let go = |min, max| amount(DsKind::Counter, "5", &stored, 10.0, 600, min, max);
        assert_eq!(go(None, Some(1.0)), Ok(Some(5.0)));
        assert_eq!(go(Some(0.6), None), Ok(None));
    }

    #[test]
    fn dcounter_decrease_is_unknown_and_bad_stored_text_fails() {
        assert_eq!(run(DsKind::DCounter, "10", "5"), Ok(None));
        assert_eq!(run(DsKind::DCounter, "-5", "-3"), Ok(None));
        let err = run(DsKind::DCounter, "12abc", "20").unwrap_err();
        assert!(err.contains("'12abc'"), "{err}");
        assert_eq!(run(DsKind::DCounter, "U", "12abc"), Ok(None));
    }

    #[test]
    fn dcounter_counts_down_with_a_negative_reading() {
        assert_eq!(run(DsKind::DCounter, "3", "-5"), Ok(Some(-8.0)));
        assert_eq!(
            run(DsKind::DCounter, "3", "-inf"),
            Ok(Some(f64::NEG_INFINITY))
        );
        assert_eq!(run(DsKind::DCounter, "-3", "1"), Ok(Some(4.0)));
        assert_eq!(run(DsKind::DCounter, "-3", "-3"), Ok(Some(0.0)));
        assert_eq!(run(DsKind::DCounter, "3", "0"), Ok(Some(-3.0)));
        assert_eq!(run(DsKind::DCounter, "-3", "0"), Ok(Some(3.0)));
        assert_eq!(run(DsKind::DCounter, "-inf", "-5"), Ok(None));
        assert_eq!(run(DsKind::DCounter, "inf", "5"), Ok(None));
    }

    #[test]
    fn leading_blank_nan_is_rejected() {
        assert_eq!(
            run(DsKind::Gauge, "U", " nan"),
            Err(
                "Function update_pdp_prep, case DST_GAUGE - Cannot convert ' nan' to float"
                    .to_string()
            )
        );
    }

    #[test]
    fn leading_blank_message_keeps_reading() {
        assert_eq!(
            run(DsKind::Gauge, "U", "  5x"),
            Err("Function update_pdp_prep, case DST_GAUGE - Converted '  5x' to 5.000000, but cannot convert 'x'"
                .to_string())
        );
        assert_eq!(run(DsKind::Gauge, "U", "\t\n 5"), Ok(Some(5.0)));
    }

    #[test]
    fn non_ascii_blank_is_not_skipped() {
        assert_eq!(
            run(DsKind::Gauge, "U", "\u{a0}5"),
            Err(
                "Function update_pdp_prep, case DST_GAUGE - Cannot convert '\u{a0}5' to float"
                    .to_string()
            )
        );
    }

    #[test]
    fn stored_text_truncates_to_29_bytes() {
        let reading = "0123456789".repeat(4);
        let field = stored_text(&reading);
        assert_eq!(&field[..29], &reading.as_bytes()[..29]);
        assert_eq!(field[29], 0);
        let short = stored_text("5");
        assert_eq!(short[0], b'5');
        assert!(short[1..].iter().all(|&b| b == 0));
    }
}
