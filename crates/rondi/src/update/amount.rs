//! The amount a reading contributes to a data source (UP-10 to UP-18).

use crate::rrd_number::{rrd_diff, rrd_strtodbl};

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
            let mut d = integer_difference(reading, &stored);
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
            (integer_difference(reading, &stored), None)
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
            // Provenance: black-box compatibility test (rrdtool_differential
            // dcounter_dderive_special_samples_match_rrdtool_byte_for_byte).
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

/// Exact decimal difference of two integer texts (UP-16); NaN when unknown.
fn integer_difference(reading: &str, stored: &str) -> f64 {
    // rrd_diff documents a limit on digits; UP-16 limits characters, so a
    // sign plus 30 digits needs this check.
    if reading.len() > STORED_LEN {
        return f64::NAN;
    }
    rrd_diff(reading, stored)
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
    fn derive_negative_minus_zero() {
        let x = run(DsKind::Derive, "-5", "-5").unwrap().unwrap();
        assert!(x == 0.0 && x.is_sign_negative());
    }

    #[test]
    fn counter_long_difference_rounds_by_rs38() {
        assert_eq!(
            run(DsKind::Counter, "0", "12345678901234567890123"),
            Ok(Some(1.234567890123457e22))
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
