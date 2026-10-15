//! Text of graph PRINT and GPRINT items, and the C library time and locale
//! calls it needs.
//!
//! The formatting rules live in `rondi::format_value`; this module walks
//! the graph elements in order, picks the value and time each item shows,
//! and stores the result as a `print[n]` entry or a GPRINT legend. The
//! behavior is specified in docs/compatibility/format-strings.md.

use std::ffi::{CStr, CString};

use rondi::format_value::{
    DurationError, NumberSpec, PrintFormat, Printer, UnitScale, format_duration, host_number,
    missing_time_text, printf_number,
};
use rondi::graph::{Gf, GraphImage, ValueFormatter};

// `super`, not `crate`: the fuzz crate includes the CLI as a submodule.
use super::{GraphInfo, InfoValue};

/// How the print pass ended.
pub(crate) enum PrintOutcome {
    /// Every item was formatted; `draws` tells whether any element puts
    /// something on the canvas.
    Finished { draws: bool },
    /// A duration needed more text than the C library can produce.
    /// RRDtool then ends the command with no output and status 0.
    Abandoned,
}

/// PRINT text longer than this fails instead of being built.
// Compatibility: RRDtool prints any width or precision the format check
// accepts (FS-10), up to what printf can return; Rondi fails cleanly above
// this bound (FS-50).
// Provenance: black-box compatibility test; docs/compatibility/format-strings.md.
const MOST_PRINT_BYTES: u64 = 1 << 24;

// Compatibility: a numeric GPRINT keeps the first 197 bytes of its text,
// and a text the C library cannot produce (more than INT_MAX bytes) leaves
// the legend empty.
// Provenance: black-box compatibility test (graphv legend[n] for
// GPRINT:v:%200lf and GPRINT:v:%2147483648lf).
const MOST_GPRINT_BYTES: u64 = 197;
const MOST_PRINTF_BYTES: u64 = i32::MAX as u64;

// Compatibility: a GPRINT `:valstrftime` value printed as digits keeps its
// first 199 bytes; PRINT keeps them all.
// Provenance: black-box compatibility test (GPRINT:v:%s:valstrftime of
// 1.55e302 shows a 199-byte legend, PRINT 301 bytes).
const MOST_GPRINT_DIGITS: u64 = 199;

/// RRDtool formats times into a 200-byte buffer.
const STRFTIME_CAPACITY: usize = 200;

/// The value format of `:valstrftime` when none is given (FS-31).
const DEFAULT_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// Formats every PRINT and GPRINT item of `im` in element order, records
/// PRINT text as `print[n]` info entries, stores GPRINT text as the item's
/// legend and gives HRULE/VRULE items their VDEF value or time.
pub(crate) fn format_print_items(
    im: &mut GraphImage,
    si_base: u32,
    force_utc: bool,
    info: &mut GraphInfo,
) -> Result<PrintOutcome, Box<dyn std::error::Error>> {
    let decimal_point = locale_decimal_point();
    let mut scale = UnitScale::new(si_base);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    // Compatibility: a strftime item shows the time of the most recent
    // PRINT or GPRINT item that read a VDEF, or the current time before
    // any did; a VDEF without a time contributes time 0 (FS-30).
    // Provenance: black-box compatibility test (PRINT:x:AVERAGE:%F:strftime
    // after PRINT:v:%lf prints the date of v, after an AVERAGE VDEF 1970).
    let mut shown_time = broken_down_time(now, force_utc);
    let mut print_count = 0_usize;
    let mut draws = false;
    for index in 0..im.gdes.len() {
        let gf = im.gdes[index].gf;
        let vdef = im.gdes[index]
            .vidx
            .map(|vidx| &im.gdes[vidx])
            .filter(|source| source.gf == Gf::Vdef)
            .map(|source| source.vf);
        match gf {
            Gf::Print | Gf::Gprint => {
                if let Some(vdef) = vdef {
                    shown_time = broken_down_time(vdef.when, force_utc);
                }
                let value = im.print_value(index);
                let element = &im.gdes[index];
                let text = if element.strftm {
                    if vdef.is_some_and(|vdef| vdef.never) {
                        missing_time_text(&element.format)
                    } else {
                        strftime_bytes(&element.format, &shown_time, STRFTIME_CAPACITY)
                    }
                } else {
                    match element.vformatter {
                        ValueFormatter::Timestamp => {
                            let limit = if gf == Gf::Print {
                                MOST_PRINT_BYTES
                            } else {
                                MOST_GPRINT_DIGITS
                            };
                            timestamp_text(&element.format, value, &decimal_point, limit)
                        }
                        ValueFormatter::Duration => {
                            match format_duration(&element.format, value, &decimal_point) {
                                Ok(text) => text,
                                Err(DurationError::TooLong) => return Ok(PrintOutcome::Abandoned),
                                Err(error) => return Err(error.to_string().into()),
                            }
                        }
                        ValueFormatter::Numeric => {
                            let format = PrintFormat::parse(&element.format)?;
                            let printer = Printer::Host {
                                decimal_point: &decimal_point,
                            };
                            let field = format.render(value, &mut scale, printer).field;
                            im.gdes[index].format = format.field().to_owned();
                            numeric_text(gf, &field, format.field())?
                        }
                    }
                };
                let text = String::from_utf8_lossy(&text).into_owned();
                if gf == Gf::Print {
                    info.push(format!("print[{print_count}]"), InfoValue::Str(text));
                    print_count += 1;
                } else {
                    im.gdes[index].legend = text;
                }
            }
            Gf::Hrule => {
                if let Some(vdef) = vdef {
                    im.gdes[index].yrule = vdef.val;
                }
            }
            Gf::Vrule => {
                if let Some(vdef) = vdef {
                    im.gdes[index].xrule = vdef.when;
                }
            }
            _ => {}
        }
        draws |= matches!(
            gf,
            Gf::Gprint | Gf::Line | Gf::Area | Gf::Tick | Gf::Hrule | Gf::Vrule | Gf::Stack
        );
    }
    Ok(PrintOutcome::Finished { draws })
}

/// The bytes a numeric PRINT or GPRINT item shows.
fn numeric_text(
    gf: Gf,
    field: &rondi::format_value::Field,
    format: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut text = Vec::new();
    if gf == Gf::Print {
        if field.len() > MOST_PRINT_BYTES {
            return Err(format!(
                "PRINT format '{format}' produces more than {MOST_PRINT_BYTES} bytes"
            )
            .into());
        }
        field.write_prefix(&mut text, MOST_PRINT_BYTES);
    } else if field.len() <= MOST_PRINTF_BYTES {
        field.write_prefix(&mut text, MOST_GPRINT_BYTES);
    }
    Ok(text)
}

/// The text of an xport PRINT or GPRINT entry, formatted with RRDtool's
/// own printf. The first `%S` of `format` is rewritten to `%s` in place,
/// so a later graph pass over the same item scales afresh.
pub(crate) fn xport_print_text(
    value: f64,
    format: &mut String,
    scale: &mut UnitScale,
) -> Result<String, Box<dyn std::error::Error>> {
    let parsed = PrintFormat::parse(format)?;
    *format = parsed.field().to_owned();
    let text = parsed.render(value, scale, Printer::Scaled);
    if text.field.len() > MOST_PRINT_BYTES {
        return Err(
            format!("PRINT format '{format}' produces more than {MOST_PRINT_BYTES} bytes").into(),
        );
    }
    let mut bytes = Vec::new();
    text.field.write_prefix(&mut bytes, MOST_PRINT_BYTES);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `:valstrftime`: the value is a UTC time in seconds, truncated towards
/// zero (FS-31, FS-32). A value printed as digits keeps at most `limit`
/// bytes.
fn timestamp_text(format: &str, value: f64, decimal_point: &str, limit: u64) -> Vec<u8> {
    let format = if format.is_empty() {
        DEFAULT_TIMESTAMP_FORMAT
    } else {
        format
    };
    // Compatibility: a value that is not finite, or whose integer part
    // does not fit the 64-bit time type, prints as printf("%.0f").
    // Provenance: black-box compatibility test (1e20, -1e20, 1e300, inf).
    const TIME_RANGE: f64 = 9_223_372_036_854_775_808.0;
    let seconds = value.trunc();
    if !value.is_finite() || !(-TIME_RANGE..TIME_RANGE).contains(&seconds) {
        let spec = NumberSpec {
            precision: Some(0),
            ..NumberSpec::new(rondi::format_value::Notation::Fixed)
        };
        let field = host_number(&spec, value, decimal_point);
        let mut text = Vec::new();
        field.write_prefix(&mut text, limit);
        return text;
    }
    // Compatibility: when the C library cannot convert the time it leaves
    // the zeroed broken-down time partly filled in, and that is printed
    // (FS-32: large convertible values follow the platform).
    // Provenance: black-box compatibility test (9.2e18 on macOS and glibc).
    let mut tm = zeroed_tm();
    let time = seconds as libc::time_t;
    // SAFETY: both pointers refer to live, properly aligned values.
    unsafe { libc::gmtime_r(&time, &mut tm) };
    strftime_bytes(format, &tm, STRFTIME_CAPACITY)
}

fn zeroed_tm() -> libc::tm {
    // SAFETY: libc::tm is plain data; all-zero is a valid value.
    unsafe { std::mem::zeroed() }
}

/// The local broken-down time of `time`, or UTC with `--utc`.
fn broken_down_time(time: i64, utc: bool) -> libc::tm {
    if utc { utc_tm(time) } else { local_tm(time) }
}

/// `time` as local broken-down time (the `TZ` zone).
pub(crate) fn local_tm(time: i64) -> libc::tm {
    let time = time as libc::time_t;
    let mut tm = zeroed_tm();
    // SAFETY: both pointers refer to live, properly aligned values.
    unsafe { libc::localtime_r(&time, &mut tm) };
    tm
}

/// `time` as UTC broken-down time.
pub(crate) fn utc_tm(time: i64) -> libc::tm {
    let time = time as libc::time_t;
    let mut tm = zeroed_tm();
    // SAFETY: both pointers refer to live, properly aligned values.
    unsafe { libc::gmtime_r(&time, &mut tm) };
    tm
}

/// Formats `tm` with the C library's `strftime` into a buffer of
/// `capacity` bytes.
///
/// When the result does not fit, `strftime` reports failure but leaves
/// what it wrote in the buffer. RRDtool prints that partial text, so this
/// returns it too: the first `capacity` bytes on macOS, the complete
/// conversions that fit in `capacity - 1` bytes on glibc (FS-34).
pub(crate) fn strftime_bytes(format: &str, tm: &libc::tm, capacity: usize) -> Vec<u8> {
    let Ok(format) = CString::new(format) else {
        return Vec::new();
    };
    if capacity == 0 {
        return Vec::new();
    }
    // One spare zero byte ends the text when strftime fills the buffer.
    let mut buffer = vec![0_u8; capacity + 1];
    // SAFETY: the buffer holds `capacity + 1` bytes and strftime writes at
    // most `capacity`; `format` is NUL-terminated and `tm` is valid.
    let length =
        unsafe { libc::strftime(buffer.as_mut_ptr().cast(), capacity, format.as_ptr(), tm) };
    let end = if length > 0 {
        length
    } else {
        buffer
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(capacity)
    };
    buffer.truncate(end);
    buffer
}

/// [`strftime_bytes`] as text, with invalid UTF-8 replaced.
pub(crate) fn strftime_text(format: &str, tm: &libc::tm, capacity: usize) -> String {
    String::from_utf8_lossy(&strftime_bytes(format, tm, capacity)).into_owned()
}

/// The decimal point of the current numeric locale.
pub(crate) fn locale_decimal_point() -> String {
    // SAFETY: localeconv returns a pointer to static locale data that
    // stays valid until the locale changes; it is copied at once.
    unsafe {
        let conventions = libc::localeconv();
        if conventions.is_null() || (*conventions).decimal_point.is_null() {
            return ".".to_owned();
        }
        let point = CStr::from_ptr((*conventions).decimal_point);
        match point.to_str() {
            Ok("") | Err(_) => ".".to_owned(),
            Ok(point) => point.to_owned(),
        }
    }
}

/// Formats `value` with a C format holding one floating-point conversion,
/// as the C library's printf would in the current locale.
pub(crate) fn printf_double(format: &str, value: f64) -> String {
    printf_number(format, value, &locale_decimal_point())
}

#[cfg(test)]
mod tests {
    use super::{strftime_bytes, timestamp_text, utc_tm};

    #[test]
    fn strftime_keeps_partial_text_when_full() {
        let tm = utc_tm(1_000_000_000);
        assert_eq!(strftime_bytes("%Y-%m-%d", &tm, 200), b"2001-09-09");
        assert_eq!(strftime_bytes("", &tm, 200), b"");
        let long = "%Y".repeat(60);
        let text = strftime_bytes(&long, &tm, 200);
        if cfg!(target_os = "macos") {
            assert_eq!(text.len(), 200);
        } else {
            assert_eq!(text.len(), 196);
        }
        assert!(text.iter().all(|byte| b"2001".contains(byte)));
        assert_eq!(strftime_bytes("a\0b", &tm, 200), b"");
    }

    #[test]
    fn timestamps_truncate_towards_zero() {
        assert_eq!(timestamp_text("%H:%M:%S", 1.9, ".", 64), b"00:00:01");
        assert_eq!(timestamp_text("", 0.0, ".", 64), b"1970-01-01 00:00:00");
        assert_eq!(timestamp_text("%Y", -1.0, ".", 64), b"1969");
        assert_eq!(
            timestamp_text("x%Y", 1e20, ".", 64),
            b"100000000000000000000"
        );
        assert_eq!(
            timestamp_text("%Y", -1e20, ".", 64),
            b"-100000000000000000000"
        );
        assert_eq!(timestamp_text("%Y", f64::INFINITY, ".", 64), b"inf");
        assert_eq!(timestamp_text("%Y", 1e300, ".", 199).len(), 199);
    }
}
