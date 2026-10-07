//! Numeric conversion used by RRDtool's update path.

/// Parse a decimal using the conversion algorithm in RRDtool 1.11's
/// `rrd_strtodbl`. The order of the floating-point operations is observable in
/// epoch-sized values with fractional seconds.
pub fn parse_rrd_number(input: &str) -> Option<f64> {
    // rrd_strtodbl only consults the special spellings when rrd_strtod made
    // no conversion, which also covers an out-of-range exponent.
    parse_rrd_decimal(input).or_else(|| parse_special(input))
}

fn parse_special(input: &str) -> Option<f64> {
    let starts_with = |prefix: &str| {
        input
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    // RRDtool assigns the opposite NaN sign to each spelling and matches
    // prefixes only, so trailing text such as "infinity" is accepted.
    if starts_with("-nan") {
        Some(crate::rrd_binary::rrd_nan())
    } else if starts_with("nan") {
        Some(-crate::rrd_binary::rrd_nan())
    } else if starts_with("inf") {
        Some(f64::INFINITY)
    } else if starts_with("-inf") {
        Some(f64::NEG_INFINITY)
    } else {
        None
    }
}

fn parse_rrd_decimal(input: &str) -> Option<f64> {
    let bytes = input.as_bytes();
    let mut position = 0;
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }

    let negative = match bytes.get(position) {
        Some(b'-') => {
            position += 1;
            true
        }
        Some(b'+') => {
            position += 1;
            false
        }
        _ => false,
    };

    let mut number = 0.0_f64;
    let mut digits = 0_usize;
    while let Some(byte @ b'0'..=b'9') = bytes.get(position).copied() {
        number = number * 10.0 + f64::from(byte - b'0');
        position += 1;
        digits += 1;
    }

    let mut exponent = 0_i32;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let mut decimals = 0_i32;
        while let Some(byte @ b'0'..=b'9') = bytes.get(position).copied() {
            number = number * 10.0 + f64::from(byte - b'0');
            position += 1;
            digits += 1;
            decimals = decimals.checked_add(1)?;
        }
        exponent = exponent.checked_sub(decimals)?;
    }
    if digits == 0 {
        return None;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        let exponent_negative = match bytes.get(position) {
            Some(b'-') => {
                position += 1;
                true
            }
            Some(b'+') => {
                position += 1;
                false
            }
            _ => false,
        };
        let mut explicit_exponent = 0_i32;
        while let Some(byte @ b'0'..=b'9') = bytes.get(position).copied() {
            explicit_exponent = explicit_exponent
                .checked_mul(10)?
                .checked_add(i32::from(byte - b'0'))?;
            position += 1;
        }
        exponent = if exponent_negative {
            exponent.checked_sub(explicit_exponent)?
        } else {
            exponent.checked_add(explicit_exponent)?
        };
    }
    // DBL_MIN_EXP and DBL_MAX_EXP bound the decimal exponent in rrd_strtod.
    if !(f64::MIN_EXP..=f64::MAX_EXP).contains(&exponent) {
        return None;
    }
    if position != bytes.len() {
        return None;
    }

    if negative {
        number = -number;
    }
    let mut power_of_ten = 10.0_f64;
    let mut remaining_power = exponent.unsigned_abs();
    while remaining_power > 0 {
        if remaining_power & 1 == 1 {
            number = if exponent < 0 {
                number / power_of_ten
            } else {
                number * power_of_ten
            };
        }
        remaining_power >>= 1;
        power_of_ten *= power_of_ten;
    }
    Some(number)
}

#[cfg(test)]
mod tests {
    use super::parse_rrd_number;

    #[test]
    fn epoch_fraction_rounding_matches_rrdtool_strtod() {
        assert_eq!(
            parse_rrd_number("1000000010.9999999"),
            Some(1_000_000_011.0)
        );
        assert_eq!(
            parse_rrd_number("1000000010.0000001"),
            Some(1_000_000_010.0)
        );
        assert_eq!(
            parse_rrd_number("1000000010.1234567"),
            Some(1_000_000_010.1234568)
        );
    }

    #[test]
    fn parses_rrd_numeric_exponents_and_rejects_trailing_text() {
        assert_eq!(parse_rrd_number("1.25e2"), Some(125.0));
        assert_eq!(parse_rrd_number("  -1.25E-2"), Some(-0.0125));
        assert_eq!(parse_rrd_number("1.2tail"), None);
        // RRDtool's converter consumes an exponent marker with no digits as
        // an exponent of zero; preserve that parser quirk.
        assert_eq!(parse_rrd_number("1e"), Some(1.0));
    }

    #[test]
    fn rejects_exponents_outside_the_double_exponent_range() {
        assert_eq!(parse_rrd_number("1e1024"), Some(f64::INFINITY));
        assert_eq!(parse_rrd_number("1e1025"), None);
        assert_eq!(parse_rrd_number("1e-1021"), Some(0.0));
        assert_eq!(parse_rrd_number("1e-1022"), None);
        assert_eq!(parse_rrd_number("1e-1100"), None);
        assert_eq!(parse_rrd_number("0.1e-1021"), None);
    }

    #[test]
    fn accepts_special_spellings_by_prefix() {
        assert_eq!(parse_rrd_number("inf"), Some(f64::INFINITY));
        assert_eq!(parse_rrd_number("Infinity"), Some(f64::INFINITY));
        assert_eq!(parse_rrd_number("-INF"), Some(f64::NEG_INFINITY));
        assert!(parse_rrd_number("NaN").is_some_and(f64::is_nan));
        assert!(parse_rrd_number("-nanx").is_some_and(f64::is_nan));
        assert!(
            parse_rrd_number("nan").unwrap().is_sign_negative()
                != parse_rrd_number("-nan").unwrap().is_sign_negative()
        );
        assert_eq!(parse_rrd_number("+inf"), None);
        assert_eq!(parse_rrd_number(" inf"), None);
    }
}
