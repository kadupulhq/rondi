//! Port of `rrd_snprintf` (RRDtool 1.11.0 `src/rrd_snprintf.c`, Holger
//! Weiss' portable snprintf) as RRDtool builds it: without `localeconv`, so
//! the decimal point is always `.` and the grouping separator `,`, and with
//! `LDOUBLE` as `double`. Its `%e`/`%f` conversion scales by repeated powers
//! of ten and rounds the scaled fraction, so it is not correctly rounded and
//! differs from the C library for some values.

/// A `rrd_snprintf` argument.
#[derive(Debug, Clone, Copy)]
pub enum Arg<'a> {
    Double(f64),
    Str(&'a str),
    Int(i64),
}

const F_MINUS: u32 = 1 << 0;
const F_PLUS: u32 = 1 << 1;
const F_SPACE: u32 = 1 << 2;
const F_NUM: u32 = 1 << 3;
const F_ZERO: u32 = 1 << 4;
const F_QUOTE: u32 = 1 << 5;
const F_UP: u32 = 1 << 6;
const F_UNSIGNED: u32 = 1 << 7;
const F_TYPE_G: u32 = 1 << 8;
const F_TYPE_E: u32 = 1 << 9;

/// `DBL_MIN_10_EXP` and `DBL_MAX_10_EXP`.
const MIN_10_EXP: i32 = -307;
const MAX_10_EXP: i32 = 308;

/// Formats `format` with `args` like `rrd_snprintf` into an unbounded
/// buffer. On a conversion overflow RRDtool stops and keeps what it wrote.
pub fn rrd_snprintf(format: &str, args: &[Arg<'_>]) -> String {
    let mut out = Vec::new();
    let mut args = args.iter().copied();
    let bytes = format.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        i += 1;
        if ch != b'%' {
            out.push(ch);
            continue;
        }
        let mut flags = 0;
        while let Some(&flag) = bytes.get(i) {
            flags |= match flag {
                b'-' => F_MINUS,
                b'+' => F_PLUS,
                b' ' => F_SPACE,
                b'#' => F_NUM,
                b'0' => F_ZERO,
                b'\'' => F_QUOTE,
                _ => break,
            };
            i += 1;
        }
        let mut width: i32 = 0;
        let mut overflow = false;
        while let Some(digit @ b'0'..=b'9') = bytes.get(i).copied() {
            let digit = i32::from(digit - b'0');
            if width > (i32::MAX - digit) / 10 {
                overflow = true;
                break;
            }
            width = 10 * width + digit;
            i += 1;
        }
        if overflow {
            break;
        }
        if bytes.get(i) == Some(&b'*') {
            i += 1;
            width = match args.next() {
                Some(Arg::Int(value)) => value as i32,
                _ => 0,
            };
            if width < 0 {
                flags |= F_MINUS;
                width = -width;
            }
        }
        let mut precision: i32 = -1;
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            precision = 0;
            while let Some(digit @ b'0'..=b'9') = bytes.get(i).copied() {
                let digit = i32::from(digit - b'0');
                if precision > (i32::MAX - digit) / 10 {
                    overflow = true;
                    break;
                }
                precision = 10 * precision + digit;
                i += 1;
            }
            if overflow {
                break;
            }
            if bytes.get(i) == Some(&b'*') {
                i += 1;
                precision = match args.next() {
                    Some(Arg::Int(value)) => value as i32,
                    _ => 0,
                };
                if precision < 0 {
                    precision = -1;
                }
            }
        }
        // Length modifiers only select the C argument type.
        match bytes.get(i) {
            Some(b'h') | Some(b'l') => {
                let modifier = bytes[i];
                i += 1;
                if bytes.get(i) == Some(&modifier) {
                    i += 1;
                }
            }
            Some(b'L' | b'j' | b't' | b'z') => i += 1,
            _ => {}
        }
        let Some(&conversion) = bytes.get(i) else {
            break;
        };
        i += 1;
        match conversion {
            b'd' | b'i' => {
                let value = int_arg(args.next());
                fmtint(&mut out, value, 10, width, precision, flags);
            }
            b'X' | b'x' | b'o' | b'u' => {
                if conversion == b'X' {
                    flags |= F_UP;
                }
                let base = match conversion {
                    b'X' | b'x' => 16,
                    b'o' => 8,
                    _ => 10,
                };
                flags |= F_UNSIGNED;
                let value = int_arg(args.next());
                fmtint(&mut out, value, base, width, precision, flags);
            }
            b'A' | b'E' | b'G' | b'F' | b'a' | b'e' | b'g' | b'f' => {
                match conversion {
                    b'E' | b'e' => flags |= F_TYPE_E,
                    b'G' | b'g' => flags |= F_TYPE_G,
                    _ => {}
                }
                if conversion.is_ascii_uppercase() {
                    flags |= F_UP;
                }
                let value = match args.next() {
                    Some(Arg::Double(value)) => value,
                    Some(Arg::Int(value)) => value as f64,
                    _ => 0.0,
                };
                if fmtflt(&mut out, value, width, precision, flags) {
                    break;
                }
            }
            b'c' => out.push(int_arg(args.next()) as u8),
            b's' => {
                let value = match args.next() {
                    Some(Arg::Str(value)) => value,
                    _ => "(null)",
                };
                fmtstr(&mut out, value.as_bytes(), width, precision, flags);
            }
            b'%' => out.push(b'%'),
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn int_arg(arg: Option<Arg<'_>>) -> i64 {
    match arg {
        Some(Arg::Int(value)) => value,
        Some(Arg::Double(value)) => value as i64,
        _ => 0,
    }
}

fn fmtstr(out: &mut Vec<u8>, value: &[u8], width: i32, precision: i32, flags: u32) {
    let noprecision = precision == -1;
    let strln = value
        .iter()
        .take(if noprecision {
            usize::MAX
        } else {
            precision as usize
        })
        .count() as i32;
    let mut padlen = (width - strln).max(0);
    if flags & F_MINUS != 0 {
        padlen = -padlen;
    }
    while padlen > 0 {
        out.push(b' ');
        padlen -= 1;
    }
    out.extend_from_slice(&value[..strln as usize]);
    while padlen < 0 {
        out.push(b' ');
        padlen += 1;
    }
}

fn fmtint(out: &mut Vec<u8>, value: i64, base: u64, width: i32, mut precision: i32, flags: u32) {
    let noprecision = precision == -1;
    let mut sign = 0_u8;
    let uvalue = if flags & F_UNSIGNED != 0 {
        value as u64
    } else {
        if value < 0 {
            sign = b'-';
        } else if flags & F_PLUS != 0 {
            sign = b'+';
        } else if flags & F_SPACE != 0 {
            sign = b' ';
        }
        value.unsigned_abs()
    };
    let iconvert = convert(uvalue, base, flags & F_UP != 0);
    let pos = iconvert.len() as i32;
    let mut hexprefix = 0_u8;
    if flags & F_NUM != 0 && uvalue != 0 {
        match base {
            8 if precision <= pos => precision = pos + 1,
            16 => hexprefix = if flags & F_UP != 0 { b'X' } else { b'x' },
            _ => {}
        }
    }
    let separators = if flags & F_QUOTE != 0 {
        getnumsep(pos)
    } else {
        0
    };
    let mut zpadlen = (precision - pos - separators).max(0);
    let mut spadlen = (width
        - separators
        - precision.max(pos)
        - i32::from(sign != 0)
        - if hexprefix != 0 { 2 } else { 0 })
    .max(0);
    if flags & F_MINUS != 0 {
        spadlen = -spadlen;
    } else if flags & F_ZERO != 0 && noprecision {
        zpadlen += spadlen;
        spadlen = 0;
    }
    while spadlen > 0 {
        out.push(b' ');
        spadlen -= 1;
    }
    if sign != 0 {
        out.push(sign);
    }
    if hexprefix != 0 {
        out.push(b'0');
        out.push(hexprefix);
    }
    while zpadlen > 0 {
        out.push(b'0');
        zpadlen -= 1;
    }
    emit_digits(out, &iconvert, separators > 0);
    while spadlen < 0 {
        out.push(b' ');
        spadlen += 1;
    }
}

/// Writes reversed `digits` most significant first, with `,` grouping.
fn emit_digits(out: &mut Vec<u8>, digits: &[u8], separators: bool) {
    let mut pos = digits.len();
    while pos > 0 {
        pos -= 1;
        out.push(digits[pos]);
        if separators && pos > 0 && pos % 3 == 0 {
            out.push(b',');
        }
    }
}

fn getnumsep(digits: i32) -> i32 {
    (digits - i32::from(digits % 3 == 0)) / 3
}

/// The digits of `value` in reverse order.
fn convert(mut value: u64, base: u64, caps: bool) -> Vec<u8> {
    let digits: &[u8] = if caps {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut buf = Vec::new();
    loop {
        buf.push(digits[(value % base) as usize]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    buf
}

fn getexponent(value: f64) -> i32 {
    let mut tmp = value.abs();
    let mut exponent = 0;
    while tmp < 1.0 && tmp > 0.0 {
        exponent -= 1;
        if exponent < MIN_10_EXP {
            break;
        }
        tmp *= 10.0;
    }
    while tmp >= 10.0 {
        exponent += 1;
        if exponent > MAX_10_EXP {
            break;
        }
        tmp /= 10.0;
    }
    exponent
}

/// `cast`: the integer part, or `u64::MAX` when it does not fit.
fn cast(value: f64) -> u64 {
    if value >= u64::MAX as f64 {
        return u64::MAX;
    }
    let result = value as u64;
    if result as f64 <= value {
        result
    } else {
        result - 1
    }
}

fn myround(value: f64) -> u64 {
    let intpart = cast(value);
    if value - (intpart as f64) < 0.5 {
        intpart
    } else {
        intpart + 1
    }
}

fn mypow10(mut exponent: i32) -> f64 {
    let mut result = 1.0;
    while exponent > 0 {
        result *= 10.0;
        exponent -= 1;
    }
    while exponent < 0 {
        result /= 10.0;
        exponent += 1;
    }
    result
}

/// Port of `fmtflt`. Returns true on the integer-part overflow that makes
/// `rrd_vsnprintf` stop.
fn fmtflt(out: &mut Vec<u8>, fvalue: f64, width: i32, mut precision: i32, flags: u32) -> bool {
    let mut estyle = flags & F_TYPE_E != 0;
    let mut omitzeros = false;
    let mut exponent = 0;
    if precision == -1 {
        precision = 6;
    }
    let mut sign = 0_u8;
    if fvalue < 0.0 {
        sign = b'-';
    } else if flags & F_PLUS != 0 {
        sign = b'+';
    } else if flags & F_SPACE != 0 {
        sign = b' ';
    }
    let infnan: Option<&[u8]> = if fvalue.is_nan() {
        Some(if flags & F_UP != 0 { b"NAN" } else { b"nan" })
    } else if fvalue.is_infinite() {
        Some(if flags & F_UP != 0 { b"INF" } else { b"inf" })
    } else {
        None
    };
    if let Some(text) = infnan {
        let mut iconvert = Vec::new();
        if sign != 0 {
            iconvert.push(sign);
        }
        iconvert.extend_from_slice(text);
        let len = iconvert.len() as i32;
        fmtstr(out, &iconvert, width, len, flags);
        return false;
    }
    if flags & (F_TYPE_E | F_TYPE_G) != 0 {
        if flags & F_TYPE_G != 0 {
            if precision == 0 {
                precision = 1;
            }
            precision -= 1;
            if flags & F_NUM == 0 {
                omitzeros = true;
            }
        }
        exponent = getexponent(fvalue);
        estyle = true;
    }
    let (intpart, fracpart) = loop {
        precision = precision.min(19);
        let mut ufvalue = fvalue.abs();
        if estyle {
            ufvalue /= mypow10(exponent);
        }
        let mut intpart = cast(ufvalue);
        if intpart == u64::MAX {
            return true;
        }
        let mask = mypow10(precision) as u64;
        let mut fracpart = myround(mask as f64 * (ufvalue - intpart as f64));
        if fracpart >= mask {
            intpart += 1;
            fracpart = 0;
            if estyle && intpart == 10 {
                intpart = 1;
                exponent += 1;
            }
        }
        if flags & F_TYPE_G != 0 && estyle && precision + 1 > exponent && exponent >= -4 {
            precision -= exponent;
            estyle = false;
            continue;
        }
        break (intpart, fracpart);
    };
    let mut econvert = Vec::new();
    if estyle {
        let esign = if exponent < 0 {
            exponent = -exponent;
            b'-'
        } else {
            b'+'
        };
        econvert = convert(exponent as u64, 10, false);
        econvert.truncate(3);
        if econvert.len() == 1 {
            econvert.push(b'0');
        }
        econvert.push(esign);
        econvert.push(if flags & F_UP != 0 { b'E' } else { b'e' });
    }
    let iconvert = convert(intpart, 10, false);
    let fconvert = if fracpart != 0 {
        convert(fracpart, 10, false)
    } else {
        Vec::new()
    };
    let fpos = fconvert.len() as i32;
    let mut leadfraczeros = precision - fpos;
    let mut omitcount = 0;
    if omitzeros {
        if fpos > 0 {
            while omitcount < fpos && fconvert[omitcount as usize] == b'0' {
                omitcount += 1;
            }
        } else {
            omitcount = precision;
            leadfraczeros = 0;
        }
        precision -= omitcount;
    }
    let emitpoint = precision > 0 || flags & F_NUM != 0;
    let separators = if flags & F_QUOTE != 0 {
        getnumsep(iconvert.len() as i32)
    } else {
        0
    };
    let mut padlen = (width
        - iconvert.len() as i32
        - econvert.len() as i32
        - precision
        - separators
        - i32::from(emitpoint)
        - i32::from(sign != 0))
    .max(0);
    if flags & F_MINUS != 0 {
        padlen = -padlen;
    } else if flags & F_ZERO != 0 && padlen > 0 {
        if sign != 0 {
            out.push(sign);
            sign = 0;
        }
        while padlen > 0 {
            out.push(b'0');
            padlen -= 1;
        }
    }
    while padlen > 0 {
        out.push(b' ');
        padlen -= 1;
    }
    if sign != 0 {
        out.push(sign);
    }
    emit_digits(out, &iconvert, separators > 0);
    if emitpoint {
        out.push(b'.');
    }
    while leadfraczeros > 0 {
        out.push(b'0');
        leadfraczeros -= 1;
    }
    let mut fpos = fpos;
    while fpos > omitcount {
        fpos -= 1;
        out.push(fconvert[fpos as usize]);
    }
    for byte in econvert.iter().rev() {
        out.push(*byte);
    }
    while padlen < 0 {
        out.push(b' ');
        padlen += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{Arg, rrd_snprintf};

    #[test]
    fn scientific_values_round_after_power_of_ten_scaling() {
        let format = |value| rrd_snprintf("%0.10e", &[Arg::Double(value)]);
        assert_eq!(format(7.5), "7.5000000000e+00");
        assert_eq!(format(-0.0), "0.0000000000e+00");
        assert_eq!(format(25_186.497_579_5), "2.5186497580e+04");
        assert_eq!(format(7.5e-310), "0.0750000000e-308");
        assert_eq!(format(f64::INFINITY), "inf");
        assert_eq!(format(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn print_formats_take_a_value_and_a_unit() {
        let format = |text, value| rrd_snprintf(text, &[Arg::Double(value), Arg::Str("k")]);
        assert_eq!(format("%6.2lf %s", 4.5), "  4.50 k");
        assert_eq!(format("%-8.1lf|", 4.25), "4.3     |");
        assert_eq!(format("%+.3lg%%", 0.000_123_45), "+0.000123%");
        assert_eq!(format("%'.0lf", 1_234_567.0), "1,234,567");
    }

    #[test]
    fn integer_conversions_follow_fmtint() {
        let int = |text, value| rrd_snprintf(text, &[Arg::Int(value)]);
        assert_eq!(int("%d", -42), "-42");
        assert_eq!(int("%+5i|", 7), "   +7|");
        assert_eq!(int("% d", 7), " 7");
        assert_eq!(int("%05d", -42), "-0042");
        assert_eq!(int("%-5d|", 42), "42   |");
        assert_eq!(int("%.4d", 42), "0042");
        assert_eq!(int("%#x %#X", 255), "0xff 0");
        assert_eq!(
            rrd_snprintf("%#x %#X", &[Arg::Int(255), Arg::Int(255)]),
            "0xff 0XFF"
        );
        assert_eq!(int("%#o", 8), "010");
        assert_eq!(int("%u", 3), "3");
        assert_eq!(int("%'d", 1_234_567), "1,234,567");
        assert_eq!(int("%c", 65), "A");
        assert_eq!(int("%lld %hd %hhd %jd %zd %td", 1), "1 0 0 0 0 0");
    }

    #[test]
    fn string_and_star_arguments() {
        assert_eq!(
            rrd_snprintf(
                "[%5s][%-5s][%.2s]",
                &[Arg::Str("ab"), Arg::Str("ab"), Arg::Str("abc")]
            ),
            "[   ab][ab   ][ab]"
        );
        assert_eq!(rrd_snprintf("%s", &[]), "(null)");
        assert_eq!(rrd_snprintf("%*d|", &[Arg::Int(-4), Arg::Int(1)]), "1   |");
        assert_eq!(
            rrd_snprintf("%.*f", &[Arg::Int(2), Arg::Double(1.0)]),
            "1.00"
        );
        assert_eq!(
            rrd_snprintf("%.*f", &[Arg::Int(-1), Arg::Double(1.0)]),
            "1.000000"
        );
        assert_eq!(rrd_snprintf("100%% %q", &[]), "100% ");
        assert_eq!(rrd_snprintf("%", &[]), "");
    }

    #[test]
    fn float_special_cases() {
        let double = |text, value| rrd_snprintf(text, &[Arg::Double(value)]);
        assert_eq!(double("%f", f64::NAN), "nan");
        assert_eq!(double("%5F|", f64::NAN), "  NAN|");
        assert_eq!(double("%+e", f64::INFINITY), "+inf");
        assert_eq!(double("%E", 1234.5), "1.234500E+03");
        assert_eq!(double("%G", 1e-10), "1E-10");
        assert_eq!(double("%g", 100_000.0), "100000");
        assert_eq!(double("%g", 1e6), "1e+06");
        assert_eq!(double("%#g", 1.0), "1.00000");
        assert_eq!(double("%.0g", 2.5), "3");
        assert_eq!(double("%+08.2f", 3.125), "+0003.13");
        assert_eq!(double("% .1f", 2.0), " 2.0");
        assert_eq!(double("%#.0f", 2.0), "2.");
        assert_eq!(double("%.25f", 0.5), "0.5000000000000000000");
        assert_eq!(double("%.1e", 9.96), "1.0e+01");
        assert_eq!(double("%a", 1.5), "1.500000");
        assert_eq!(double("%f", 0.0), "0.000000");
        assert_eq!(rrd_snprintf("%f", &[Arg::Int(2)]), "2.000000");
        assert_eq!(rrd_snprintf("%d", &[Arg::Double(2.9)]), "2");
    }

    #[test]
    fn overflowing_conversions_stop_the_output() {
        assert_eq!(rrd_snprintf("a%fb", &[Arg::Double(1e30)]), "a");
        assert_eq!(rrd_snprintf("a%99999999999db", &[Arg::Int(1)]), "a");
        assert_eq!(rrd_snprintf("a%.99999999999fb", &[Arg::Double(1.0)]), "a");
    }
}
