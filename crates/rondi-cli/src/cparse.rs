use std::ffi::{CStr, CString};

pub(crate) fn c_string(value: &str) -> CString {
    // Arguments come from argv or a pipe-mode line, neither of which can
    // carry a NUL; C would stop reading there.
    CString::new(value.split('\0').next().unwrap_or_default()).unwrap_or_default()
}

pub(crate) fn atoi(value: &str) -> i32 {
    let value = c_string(value);
    // SAFETY: `value` is a valid NUL-terminated string.
    unsafe { libc::atoi(value.as_ptr()) }
}

// `long` is 32 bits on some targets.
#[allow(clippy::useless_conversion)]
pub(crate) fn atol(value: &str) -> i64 {
    let value = c_string(value);
    // SAFETY: `value` is a valid NUL-terminated string.
    i64::from(unsafe { libc::atol(value.as_ptr()) })
}

pub(crate) fn buffer_text(buffer: &[u8]) -> String {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// `rrd_strtod` (rrd_strtod.c:97). The second value is the end of the
/// conversion; 0 means none, which also covers an out-of-range exponent.
fn rrd_strtod(text: &[u8]) -> (f64, usize) {
    let digit = |position: usize| text.get(position).filter(|byte| byte.is_ascii_digit());
    let mut position = 0;
    while text
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        position += 1;
    }
    let mut negative = false;
    match text.get(position) {
        Some(b'-') => {
            negative = true;
            position += 1;
        }
        Some(b'+') => position += 1,
        _ => {}
    }
    let mut number = 0.0_f64;
    let mut exponent = 0_i32;
    let mut digits = 0;
    while let Some(byte) = digit(position) {
        number = number * 10.0 + f64::from(byte - b'0');
        position += 1;
        digits += 1;
    }
    if text.get(position) == Some(&b'.') {
        position += 1;
        let mut decimals = 0_i32;
        while let Some(byte) = digit(position) {
            number = number * 10.0 + f64::from(byte - b'0');
            position += 1;
            digits += 1;
            decimals = decimals.wrapping_add(1);
        }
        exponent = exponent.wrapping_sub(decimals);
    }
    if digits == 0 {
        return (0.0, 0);
    }
    if negative {
        number = -number;
    }
    if matches!(text.get(position), Some(b'e' | b'E')) {
        position += 1;
        let mut exponent_negative = false;
        match text.get(position) {
            Some(b'-') => {
                exponent_negative = true;
                position += 1;
            }
            Some(b'+') => position += 1,
            _ => {}
        }
        let mut n = 0_i32;
        while let Some(byte) = digit(position) {
            n = n.wrapping_mul(10).wrapping_add(i32::from(byte - b'0'));
            position += 1;
        }
        exponent = if exponent_negative {
            exponent.wrapping_sub(n)
        } else {
            exponent.wrapping_add(n)
        };
    }
    if !(f64::MIN_EXP..=f64::MAX_EXP).contains(&exponent) {
        return (f64::INFINITY, 0);
    }
    let mut p10 = 10.0_f64;
    let mut n = exponent.unsigned_abs();
    while n != 0 {
        if n & 1 == 1 {
            if exponent < 0 {
                number /= p10;
            } else {
                number *= p10;
            }
        }
        n >>= 1;
        p10 *= p10;
    }
    (number, position)
}

/// `rrd_strtodbl` (rrd_strtod.c:59): `Ok` only when the whole string
/// converts. With a label the error carries RRDtool's message.
pub(crate) fn rrd_strtodbl(text: &str, error: Option<&str>) -> Result<f64, String> {
    rrd_strtodbl_status(text, error).1
}

/// `rrd_strtodbl` with its return code: 0 for no conversion, 1 for a
/// partial one (the value is still set), 2 for the whole string.
pub(crate) fn rrd_strtodbl_status(text: &str, error: Option<&str>) -> (u32, Result<f64, String>) {
    let bytes = text.as_bytes();
    let (value, end) = rrd_strtod(bytes);
    if end == 0 {
        let prefix = |head: &str| {
            bytes
                .get(..head.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(head.as_bytes()))
        };
        if prefix("-nan") || prefix("nan") {
            return (2, Ok(f64::NAN));
        }
        if prefix("inf") {
            return (2, Ok(f64::INFINITY));
        }
        if prefix("-inf") {
            return (2, Ok(f64::NEG_INFINITY));
        }
        return (
            0,
            Err(error.map_or_else(String::new, |error| {
                format!("{error} - Cannot convert '{text}' to float")
            })),
        );
    }
    if end < bytes.len() {
        return (
            1,
            Err(error.map_or_else(String::new, |error| {
                format!(
                    "{error} - Converted '{text}' to {}, but cannot convert '{}'",
                    c_format_double(c"%lf", value),
                    String::from_utf8_lossy(&bytes[end..])
                )
            })),
        );
    }
    (2, Ok(value))
}

/// The value a partial conversion (status 1) leaves in `*dbl`.
pub(crate) fn rrd_strtod_prefix(text: &str) -> f64 {
    rrd_strtod(text.as_bytes()).0
}

/// Formats one double with the C library so locale and spelling of
/// non-finite values match RRDtool.
pub(crate) fn c_format_double(format: &CStr, value: f64) -> String {
    let mut buffer = vec![0_u8; 64];
    loop {
        // SAFETY: the buffer length bounds the write and `format` holds one
        // double conversion.
        let written = unsafe {
            libc::snprintf(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                format.as_ptr(),
                value,
            )
        };
        let Ok(written) = usize::try_from(written) else {
            return String::new();
        };
        if written < buffer.len() {
            return String::from_utf8_lossy(&buffer[..written]).into_owned();
        }
        buffer.resize(written + 1, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtodbl_matches_rrd_strtod() {
        assert_eq!(rrd_strtodbl("1.5", None), Ok(1.5));
        assert_eq!(rrd_strtodbl("1e", None), Ok(1.0));
        assert!(rrd_strtodbl("Infinity", None).unwrap().is_infinite());
        assert_eq!(
            rrd_strtodbl("abc", Some("option -l")),
            Err(String::from("option -l - Cannot convert 'abc' to float"))
        );
        assert_eq!(
            rrd_strtodbl("2x", Some("option -u")),
            Err(String::from(
                "option -u - Converted '2x' to 2.000000, but cannot convert 'x'"
            ))
        );
        assert_eq!(rrd_strtodbl("1e400", None), Ok(f64::INFINITY));
        assert!(rrd_strtodbl("1e2000", None).is_err());
    }
}
