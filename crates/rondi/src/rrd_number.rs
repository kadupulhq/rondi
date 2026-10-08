//! Numeric conversion used by RRDtool's update path.

/// Parse a decimal using the conversion algorithm in RRDtool 1.11's
/// `rrd_strtodbl`. The order of the floating-point operations is observable in
/// epoch-sized values with fractional seconds.
pub fn parse_rrd_number(input: &str) -> Option<f64> {
    // rrd_strtodbl only consults the special spellings when rrd_strtod made
    // no conversion, which also covers an out-of-range exponent.
    match parse_rrd_decimal(input) {
        Some((number, end)) => (end == input.len()).then_some(number),
        None => parse_special(input),
    }
}

/// `rrd_strtodbl()` with an error context, returning RRDtool's message for
/// text that does not convert or converts only in part.
pub(crate) fn rrd_strtodbl(input: &str, context: &str) -> Result<f64, String> {
    match parse_rrd_decimal(input) {
        Some((number, end)) if end == input.len() => Ok(number),
        Some((number, end)) => Err(format!(
            "{context} - Converted '{input}' to {number:.6}, but cannot convert '{}'",
            String::from_utf8_lossy(&input.as_bytes()[end..])
        )),
        None => parse_special(input)
            .ok_or_else(|| format!("{context} - Cannot convert '{input}' to float")),
    }
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

/// C `strtol(text, &end, base)` for a 64-bit `long`: leading white space, an
/// optional sign, and for base 0 a `0x` or `0` prefix choosing hex or octal.
/// Overflow saturates as glibc and Apple libc do. Returns the value and the
/// byte offset of `end`, which is 0 when nothing converts.
pub fn c_strtol(text: &str, base: u32) -> (i64, usize) {
    let bytes = text.as_bytes();
    let mut position = 0;
    while position < bytes.len()
        && matches!(bytes[position], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    {
        position += 1;
    }
    let negative = bytes.get(position) == Some(&b'-');
    if matches!(bytes.get(position), Some(b'-' | b'+')) {
        position += 1;
    }
    let hex_prefix = bytes.get(position) == Some(&b'0')
        && matches!(bytes.get(position + 1), Some(b'x' | b'X'))
        && bytes.get(position + 2).is_some_and(u8::is_ascii_hexdigit);
    let base = match base {
        0 if hex_prefix => 16,
        0 if bytes.get(position) == Some(&b'0') => 8,
        0 => 10,
        other => other,
    };
    if base == 16 && hex_prefix {
        position += 2;
    }
    let digits_start = position;
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    while let Some(digit) = bytes
        .get(position)
        .and_then(|byte| char::from(*byte).to_digit(36))
        .filter(|digit| *digit < base)
    {
        match magnitude
            .checked_mul(u64::from(base))
            .and_then(|value| value.checked_add(u64::from(digit)))
        {
            Some(value) => magnitude = value,
            None => overflow = true,
        }
        position += 1;
    }
    if position == digits_start {
        return (0, 0);
    }
    let value = if negative {
        if overflow || magnitude > i64::MIN.unsigned_abs() {
            i64::MIN
        } else {
            0_i64.wrapping_sub_unsigned(magnitude)
        }
    } else if overflow || magnitude > i64::MAX as u64 {
        i64::MAX
    } else {
        magnitude as i64
    };
    (value, position)
}

/// `rrd_diff()` from rrd_diff.c: the decimal difference `a - b` of two
/// integer strings, computed digit by digit and converted with rrd_strtod.
/// Any `-` before the first digit makes a number negative; mixed signs,
/// missing digits, and more than LAST_DS_LEN digits give NaN.
pub(crate) fn rrd_diff(a: &str, b: &str) -> f64 {
    const LAST_DS_LEN: usize = 30;
    fn digits(text: &[u8]) -> (bool, &[u8]) {
        let mut position = 0;
        let mut negative = false;
        while position < text.len() && !text[position].is_ascii_digit() {
            negative |= text[position] == b'-';
            position += 1;
        }
        let start = position;
        while position < text.len() && text[position].is_ascii_digit() {
            position += 1;
        }
        (negative, &text[start..position])
    }
    let (a_negative, a) = digits(a.as_bytes());
    let (b_negative, b) = digits(b.as_bytes());
    if a.is_empty() || b.is_empty() || a_negative != b_negative {
        return crate::rrd_binary::rrd_nan();
    }
    let m = a.len().max(b.len());
    if m > LAST_DS_LEN {
        return crate::rrd_binary::rrd_nan();
    }
    let zero = i32::from(b'0');
    let mut result_text = vec![b' '; m + 2];
    let mut carry = 0;
    for x in 0..m {
        let a_digit = a.len().checked_sub(x + 1).map(|index| i32::from(a[index]));
        let b_digit = b.len().checked_sub(x + 1).map(|index| i32::from(b[index]));
        let mut digit = match (a_digit, b_digit) {
            (Some(a_digit), Some(b_digit)) => a_digit - carry - b_digit + zero,
            (Some(a_digit), None) => a_digit - carry,
            (None, Some(b_digit)) => zero - b_digit - carry + zero,
            (None, None) => unreachable!("x < max(len(a), len(b))"),
        };
        if digit < zero {
            digit += 10;
            carry = 1;
        } else if digit > zero + 9 {
            digit -= 10;
            carry = 1;
        } else {
            carry = 0;
        }
        result_text[m + 1 - x] = digit as u8;
    }
    let negate = carry == 1;
    if negate {
        // Ten's complement of the digits written so far.
        let mut position = m + 1;
        for _ in 0..m {
            if !result_text[position].is_ascii_digit() {
                break;
            }
            let mut digit = i32::from(b'9') - i32::from(result_text[position]) + carry + zero;
            if digit > zero + 9 {
                digit -= 10;
                carry = 1;
            } else {
                carry = 0;
            }
            result_text[position] = digit as u8;
            position -= 1;
        }
    }
    let text = String::from_utf8_lossy(&result_text);
    let mut result = match rrd_strtodbl(&text, "expected a number") {
        Ok(value) if negate => -value,
        Ok(value) => value,
        Err(_) => crate::rrd_binary::rrd_nan(),
    };
    if a_negative && b_negative {
        result = -result;
    }
    result
}

/// `rrd_strtod()`: the value and the end of the converted text, or `None`
/// when it leaves `endptr` at the start (no digits, or an exponent outside
/// the double range).
fn parse_rrd_decimal(input: &str) -> Option<(f64, usize)> {
    let bytes = input.as_bytes();
    let mut position = 0;
    // C isspace(), which unlike is_ascii_whitespace includes \v.
    while bytes
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
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

    // `number * 10. + digit` is contracted like the pinned builds do, which
    // matters once the digits pass 2^53.
    let mut number = 0.0_f64;
    let mut digits = 0_usize;
    while let Some(byte @ b'0'..=b'9') = bytes.get(position).copied() {
        number = crate::rrd_binary::rrd_mul_add(number, 10.0, f64::from(byte - b'0'));
        position += 1;
        digits += 1;
    }

    let mut exponent = 0_i32;
    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let mut decimals = 0_i32;
        while let Some(byte @ b'0'..=b'9') = bytes.get(position).copied() {
            number = crate::rrd_binary::rrd_mul_add(number, 10.0, f64::from(byte - b'0'));
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
            // rrd_strtod accumulates into a C int, which wraps.
            explicit_exponent = explicit_exponent
                .wrapping_mul(10)
                .wrapping_add(i32::from(byte - b'0'));
            position += 1;
        }
        exponent = if exponent_negative {
            exponent.wrapping_sub(explicit_exponent)
        } else {
            exponent.wrapping_add(explicit_exponent)
        };
    }
    // DBL_MIN_EXP and DBL_MAX_EXP bound the decimal exponent in rrd_strtod.
    if !(f64::MIN_EXP..=f64::MAX_EXP).contains(&exponent) {
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
    Some((number, position))
}

#[cfg(test)]
mod tests {
    use super::{c_strtol, parse_rrd_number};

    #[test]
    fn strtol_follows_c_prefixes_and_saturation() {
        assert_eq!(c_strtol("0x1", 0), (1, 3));
        assert_eq!(c_strtol("010", 0), (8, 3));
        assert_eq!(c_strtol("1abc", 0), (1, 1));
        assert_eq!(c_strtol(" -12", 0), (-12, 4));
        assert_eq!(c_strtol("0x", 0), (0, 1));
        assert_eq!(c_strtol("09", 0), (0, 1));
        assert_eq!(c_strtol("abc", 0), (0, 0));
        assert_eq!(c_strtol("-", 10), (0, 0));
        assert_eq!(c_strtol("99999999999999999999", 10).0, i64::MAX);
        assert_eq!(c_strtol("-99999999999999999999", 10).0, i64::MIN);
        assert_eq!(c_strtol("-9223372036854775808", 10).0, i64::MIN);
    }

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
