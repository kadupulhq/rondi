//! `FETCH` and `FETCHBIN` (RC-70 to RC-74).

use super::files::Access;
use super::service::{Answer, Shared};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Rows one reply may hold, as for a collected `rrdtool fetch`.
const ROWS_MAX: usize = 10_000_000;

/// The rows of a fetch, formatted as they are sent.
pub(crate) struct FetchReply {
    rows: rondi::RrdFetchRows,
    /// Indexes of the data sources sent, in order.
    columns: Vec<usize>,
    binary: bool,
}

/// `printf("%.<digits>e")` text: at least two exponent digits with a sign.
pub(crate) fn c_exponent(value: f64, digits: usize) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.to_owned();
    }
    let text = format!("{value:.digits$e}");
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let exponent = exponent.parse::<i32>().unwrap_or(0);
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
}

/// The NaN written for an unknown value.
// Compatibility: unknown values carry the sign of the NaN the reference
// computes: negative on x86_64, positive elsewhere (platform notes).
// Provenance: black-box compatibility test.
// Rondi difference (known defect): a NaN read from the file is reported
// with this sign too, not its stored one.
const UNKNOWN_BITS: u64 = if cfg!(target_arch = "x86_64") {
    0xfff8_0000_0000_0000
} else {
    0x7ff8_0000_0000_0000
};

fn unknown() -> f64 {
    f64::from_bits(UNKNOWN_BITS)
}

/// A value as `FETCH` prints it: `%0.17e`, or `nan`/`-nan` by sign.
fn text_value(value: Option<f64>) -> String {
    let value = value.unwrap_or_else(unknown);
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_owned();
    }
    c_exponent(value, 17)
}

/// Reads an integer time the way C `strtol` with base 0 does: optional
/// white space and sign, a `0x` or `0` prefix choosing hexadecimal or
/// octal, trailing text ignored.
// Compatibility: a field without leading digits, or out of range, is
// refused; zero and below count back from now (RC-71). Provenance:
// black-box compatibility test.
fn parse_time(text: &[u8], now: i64) -> Option<i64> {
    let text = std::str::from_utf8(text).ok()?;
    let (value, end) = rondi::c_strtol(text, 0);
    if end == 0 || rondi::c_strtoll_errno(text, 0).1 == libc::ERANGE {
        return None;
    }
    Some(if value <= 0 {
        now.saturating_add(value)
    } else {
        value
    })
}

/// Answers `FETCH`/`FETCHBIN <file> <CF> [<start> [<end>] [<column>...]]`.
pub(crate) fn fetch(shared: &Shared, fields: &[Vec<u8>], binary: bool) -> Answer {
    let failed = |text: String| Answer::Failed(text.into_bytes());
    let joined = shared.names.join(&fields[1]);
    let target = match shared.locate(&joined) {
        Ok(target) => target,
        Err(answer) => return answer,
    };
    let now = super::service::now();
    let mut start = now - 86_400;
    let mut end = now;
    if let Some(text) = fields.get(3) {
        match parse_time(text, now) {
            Some(time) => start = time,
            None => {
                return failed(format!(
                    "Cannot parse start time `{}': Only simple integers are allowed.",
                    String::from_utf8_lossy(text)
                ));
            }
        }
    }
    if let Some(text) = fields.get(4) {
        match parse_time(text, now) {
            Some(time) => end = time,
            None => {
                return failed(format!(
                    "Cannot parse end time `{}': Only simple integers are allowed.",
                    String::from_utf8_lossy(text)
                ));
            }
        }
    }
    // Compatibility: pending values are written first (RC-73).
    // Provenance: black-box compatibility test.
    shared.write_entry(&joined);
    let failed_fetch = |text: String| failed(format!("rrd_fetch_r failed: {text}"));
    let cf = &fields[2];
    if let Some(error) = rondi::cf_conv_error(cf) {
        return failed_fetch(String::from_utf8_lossy(&error).into_owned());
    }
    let file = match target.open(Access::Read) {
        Ok(file) => file,
        Err(error) => return failed_fetch(super::service::opening(&joined, &error)),
    };
    let cf = String::from_utf8_lossy(cf);
    let path = Path::new(OsStr::from_bytes(&joined));
    let rows = match rondi::fetch_rrd_open_rows(file, path, &cf, start, end, 1) {
        Ok(rows) => rows,
        Err(error) => return failed_fetch(error.to_string()),
    };
    // Rondi policy (RCD-60): a reply is bounded like a collected fetch.
    if rows.len() > ROWS_MAX {
        return failed_fetch(format!(
            "RRD fetch output exceeds the {ROWS_MAX} row safety limit"
        ));
    }
    let columns = match select_columns(&rows.data_sources, &fields[5.min(fields.len())..], &joined)
    {
        Ok(columns) => columns,
        Err(text) => return failed(text),
    };
    Answer::Fetch(Box::new(FetchReply {
        rows,
        columns,
        binary,
    }))
}

/// The data sources a request selects (RC-72).
// Compatibility: more names than data sources is refused before anything
// else; an empty name is skipped; no name selects every data source.
// Provenance: black-box compatibility test.
fn select_columns(
    sources: &[String],
    names: &[Vec<u8>],
    joined: &[u8],
) -> Result<Vec<usize>, String> {
    if names.len() > sources.len() {
        return Err("too many fields given - duplicates!".to_owned());
    }
    let mut columns = Vec::new();
    for name in names.iter().filter(|name| !name.is_empty()) {
        let shown = String::from_utf8_lossy(name);
        let Some(index) = sources
            .iter()
            .position(|source| source.as_bytes() == name.as_slice())
        else {
            return Err(format!(
                "field {shown} not found in {}",
                String::from_utf8_lossy(joined)
            ));
        };
        if columns.contains(&index) {
            return Err(format!("field {shown} already used"));
        }
        columns.push(index);
    }
    if columns.is_empty() {
        columns = (0..sources.len()).collect();
    }
    Ok(columns)
}

impl FetchReply {
    /// The reply in pieces of about `chunk` bytes, so a long one never
    /// sits whole in memory.
    pub(crate) fn chunks(&self, chunk: usize) -> impl Iterator<Item = Vec<u8>> + Send + '_ {
        let header = self.header();
        let mut parts: Box<dyn Iterator<Item = Vec<u8>> + Send + '_> = if self.binary {
            Box::new(self.columns.iter().flat_map(move |&column| {
                let name = &self.rows.data_sources[column];
                let head = format!(
                    "DSName-{name}: BinaryData {} 8 {}\n",
                    self.rows.len(),
                    if cfg!(target_endian = "little") {
                        "LITTLE"
                    } else {
                        "BIG"
                    }
                )
                .into_bytes();
                let values = self.rows.rows().map(move |row| {
                    row.values[column]
                        .unwrap_or_else(unknown)
                        .to_ne_bytes()
                        .to_vec()
                });
                std::iter::once(head)
                    .chain(values)
                    .chain(std::iter::once(b"\n".to_vec()))
            }))
        } else {
            Box::new(self.rows.rows().map(move |row| {
                let mut line = format!("{}:", row.timestamp);
                for &column in &self.columns {
                    line.push(' ');
                    line.push_str(&text_value(row.values[column]));
                }
                line.push('\n');
                line.into_bytes()
            }))
        };
        let mut pending = Some(header);
        std::iter::from_fn(move || {
            let mut out = pending.take().unwrap_or_default();
            for part in parts.by_ref() {
                out.extend_from_slice(&part);
                if out.len() >= chunk {
                    break;
                }
            }
            (!out.is_empty()).then_some(out)
        })
    }

    fn header(&self) -> Vec<u8> {
        let rows = &self.rows;
        let lines = if self.binary {
            5 + self.columns.len()
        } else {
            6 + rows.len()
        };
        let mut header = format!(
            "{lines} Success\nFlushVersion: 1\nStart: {}\nEnd: {}\nStep: {}\nDSCount: {}\n",
            rows.start,
            rows.end,
            rows.step,
            self.columns.len()
        );
        if !self.binary {
            let names = self
                .columns
                .iter()
                .map(|&column| rows.data_sources[column].as_str())
                .collect::<Vec<_>>();
            header.push_str(&format!("DSName: {}\n", names.join(" ")));
        }
        header.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponents_print_like_c() {
        assert_eq!(c_exponent(1.0, 17), "1.00000000000000000e+00");
        assert_eq!(c_exponent(0.1, 17), "1.00000000000000006e-01");
        assert_eq!(
            c_exponent(6.666_666_666_666_671e299, 17),
            "6.66666666666667073e+299"
        );
        assert_eq!(c_exponent(-5.0, 10), "-5.0000000000e+00");
        assert_eq!(c_exponent(f64::NEG_INFINITY, 10), "-inf");
        assert_eq!(c_exponent(f64::INFINITY, 10), "inf");
        assert_eq!(c_exponent(f64::NAN, 10), "nan");
        assert_eq!(
            text_value(Some(f64::from_bits(0xfff8_0000_0000_0000))),
            "-nan"
        );
        assert_eq!(text_value(Some(f64::NAN)), "nan");
    }

    #[test]
    fn times_parse_like_strtol() {
        assert_eq!(parse_time(b"1000000000", 5), Some(1_000_000_000));
        assert_eq!(parse_time(b"0x3B9ACA00", 5), Some(1_000_000_000));
        assert_eq!(parse_time(b"07346545000", 5), Some(1_000_000_000));
        assert_eq!(parse_time(b"1000000000abc", 5), Some(1_000_000_000));
        assert_eq!(parse_time(b"-3600", 10_000), Some(6_400));
        assert_eq!(parse_time(b"0", 10_000), Some(10_000));
        for refused in [
            &b"x"[..],
            b"now-1h",
            b"99999999999999999999",
            b"-99999999999999999999",
            b"",
        ] {
            assert_eq!(parse_time(refused, 5), None);
        }
    }

    #[test]
    fn columns_select_and_refuse() {
        let sources = ["x".to_owned(), "y".to_owned()];
        let names = |list: &[&str]| {
            list.iter()
                .map(|name| name.as_bytes().to_vec())
                .collect::<Vec<_>>()
        };
        assert_eq!(select_columns(&sources, &names(&[]), b"f").unwrap(), [0, 1]);
        assert_eq!(
            select_columns(&sources, &names(&["y", "x"]), b"f").unwrap(),
            [1, 0]
        );
        assert_eq!(
            select_columns(&sources, &names(&["", "y"]), b"f").unwrap(),
            [1]
        );
        assert_eq!(
            select_columns(&sources, &names(&[""]), b"f").unwrap(),
            [0, 1]
        );
        assert_eq!(
            select_columns(&sources, &names(&["x", "x"]), b"f").unwrap_err(),
            "field x already used"
        );
        assert_eq!(
            select_columns(&sources, &names(&["x", "y", ""]), b"f").unwrap_err(),
            "too many fields given - duplicates!"
        );
        assert_eq!(
            select_columns(&sources, &names(&["z"]), b"/b/a.rrd").unwrap_err(),
            "field z not found in /b/a.rrd"
        );
    }
}
