//! `DS:` and `RRA:` definition words, shared by `create` and the structure
//! changes of `tune`.
//!
//! The grammar, the order of the checks and the error texts are specified in
//! docs/compatibility/create.md, whose rule numbers (CR-n) are cited below.
//! A word is parsed into a typed definition, which then serializes itself
//! into the 120-byte record the file format stores.

use std::fmt;

use crate::rrd_binary::{DS_DEF_LEN, RRA_DEF_LEN, rrd_nan, rrd_scaled_duration};
use crate::rrd_number::rrd_strtodbl;
use crate::storage::StoreError;

/// Longest data source name, in bytes (CR-17).
const MAX_NAME_LEN: usize = 19;
/// Longest minimum or maximum field, in bytes (CR-20).
const MAX_BOUND_LEN: usize = 18;
/// A heartbeat field this long, in bytes, is refused.
const MAX_HEARTBEAT_FIELD: usize = 32;
/// Longest consolidation function run that is examined (CR-27).
const MAX_CF_LEN: usize = 19;

const NAME_FIELD: usize = 20;
const TYPE_OFFSET: usize = 20;
const HEARTBEAT_OFFSET: usize = 40;
const MIN_OFFSET: usize = 48;
const MAX_OFFSET: usize = 56;
const ROWS_OFFSET: usize = 24;
const PDP_OFFSET: usize = 32;
const XFF_OFFSET: usize = 40;

/// Why a definition word was rejected. `Display` gives the exact text
/// RRDtool prints after `ERROR: `.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DefinitionError {
    DsFormat,
    DsType(String),
    /// A problem with the argument text after the type (CR-20).
    DsArguments {
        arguments: String,
        reason: String,
    },
    /// A minimum or maximum that does not convert; the text already
    /// carries its `parsing min val - ` or `parsing max val - ` context.
    DsBound(String),
    ComputeRefused,
    UnknownCf(String),
    HoltWintersRefused(String),
    TooFewFields {
        needed: usize,
        got: usize,
    },
    ExtraField(&'static str),
    Xff,
    Steps {
        field: String,
        reason: &'static str,
    },
    Rows {
        field: String,
        reason: &'static str,
    },
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DsFormat => f.write_str("invalid DS format"),
            Self::DsType(kind) => write!(f, "invalid DS type specified ({kind})"),
            Self::DsArguments { arguments, reason } => {
                write!(f, "failed to parse data source {arguments}: {reason}")
            }
            Self::DsBound(message) => f.write_str(message),
            // Compatibility: RRDtool accepts COMPUTE and Holt-Winters
            // definitions; Rondi refuses them (CR-80, TM-60).
            // Provenance: black-box compatibility test.
            Self::ComputeRefused => f.write_str("COMPUTE data sources are not supported by Rondi"),
            Self::UnknownCf(run) => write!(f, "Unrecognized consolidation function {run}"),
            Self::HoltWintersRefused(cf) => write!(
                f,
                "Holt-Winters consolidation function {cf} is not supported by Rondi"
            ),
            Self::TooFewFields { needed, got } => {
                write!(
                    f,
                    "Expected at least {needed} arguments for RRA but got {got}"
                )
            }
            Self::ExtraField(cf) => {
                write!(
                    f,
                    "Unexpected extra argument for consolidation function {cf}"
                )
            }
            Self::Xff => f.write_str("Invalid xff: must be between 0 and 1"),
            Self::Steps { field, reason } => write!(f, "Invalid step {field}: {reason}"),
            Self::Rows { field, reason } => write!(f, "Invalid row count {field}: {reason}"),
        }
    }
}

impl std::error::Error for DefinitionError {}

impl From<DefinitionError> for StoreError {
    fn from(error: DefinitionError) -> Self {
        StoreError::Rrd(error.to_string())
    }
}

/// The data source types Rondi stores (CR-19, without `COMPUTE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DsKind {
    Gauge,
    Counter,
    Derive,
    Absolute,
    DCounter,
    DDerive,
}

impl DsKind {
    fn from_word(word: &str) -> Option<Self> {
        Some(match word {
            "GAUGE" => Self::Gauge,
            "COUNTER" => Self::Counter,
            "DERIVE" => Self::Derive,
            "ABSOLUTE" => Self::Absolute,
            "DCOUNTER" => Self::DCounter,
            "DDERIVE" => Self::DDerive,
            _ => return None,
        })
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Gauge => "GAUGE",
            Self::Counter => "COUNTER",
            Self::Derive => "DERIVE",
            Self::Absolute => "ABSOLUTE",
            Self::DCounter => "DCOUNTER",
            Self::DDerive => "DDERIVE",
        }
    }

    /// The double-precision counter types need file version 0005 (CR-37).
    pub(crate) fn needs_version_5(self) -> bool {
        matches!(self, Self::DCounter | Self::DDerive)
    }
}

/// A parsed `DS:` word.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DataSourceDef {
    pub(crate) name: String,
    pub(crate) kind: DsKind,
    pub(crate) heartbeat: u64,
    /// NaN when unbounded.
    pub(crate) minimum: f64,
    pub(crate) maximum: f64,
}

/// The pieces of a `DS:` word that pass the format check of CR-17.
struct DsWord<'a> {
    name: &'a str,
    kind: &'a str,
    arguments: &'a str,
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// Splits `name[=source[[index]]]:TYPE:arguments`, or `None` when the word
/// does not have that shape. The source mapping is only meaningful to
/// `--source`, which this module does not serve, so it is checked and
/// dropped (CR-18, CR-22).
fn split_ds_word(spec: &str) -> Option<DsWord<'_>> {
    let bytes = spec.as_bytes();
    let run = |from: usize, accept: fn(u8) -> bool| {
        from + bytes[from..].iter().take_while(|b| accept(**b)).count()
    };
    let name_end = run(0, is_name_byte);
    if !(1..=MAX_NAME_LEN).contains(&name_end) {
        return None;
    }
    let mut at = name_end;
    if bytes.get(at) == Some(&b'=') {
        let source_end = run(at + 1, is_name_byte);
        if !(1..=MAX_NAME_LEN).contains(&(source_end - at - 1)) {
            return None;
        }
        at = source_end;
        if bytes.get(at) == Some(&b'[') {
            let digits_end = run(at + 1, |b| b.is_ascii_digit());
            if digits_end == at + 1 || bytes.get(digits_end) != Some(&b']') {
                return None;
            }
            at = digits_end + 1;
        }
    }
    if bytes.get(at) != Some(&b':') {
        return None;
    }
    let kind_start = at + 1;
    let kind_end = run(kind_start, |b| b.is_ascii_uppercase());
    if kind_end == kind_start || bytes.get(kind_end) != Some(&b':') || kind_end + 1 == bytes.len() {
        return None;
    }
    // Every byte consumed so far is ASCII, so these are char boundaries.
    Some(DsWord {
        name: &spec[..name_end],
        kind: &spec[kind_start..kind_end],
        arguments: &spec[kind_end + 1..],
    })
}

/// `U` or a number for a DS bound (CR-20 steps 4 and 5, CR-23, CR-25).
fn parse_bound(text: &str, context: &str) -> Result<f64, DefinitionError> {
    if text == "U" {
        return Ok(rrd_nan());
    }
    rrd_strtodbl(text, context).map_err(DefinitionError::DsBound)
}

impl DataSourceDef {
    /// Parses the text after `DS:`.
    pub(crate) fn parse(spec: &str) -> Result<Self, DefinitionError> {
        let word = split_ds_word(spec).ok_or(DefinitionError::DsFormat)?;
        let kind = match DsKind::from_word(word.kind) {
            Some(kind) => kind,
            None if word.kind == "COMPUTE" => return Err(DefinitionError::ComputeRefused),
            None => return Err(DefinitionError::DsType(word.kind.to_owned())),
        };
        let arguments = word.arguments;
        let failed = |reason: &str| DefinitionError::DsArguments {
            arguments: arguments.to_owned(),
            reason: reason.to_owned(),
        };
        // Compatibility: the separator is looked for first, and a heartbeat
        // field of 32 bytes or more is refused before it is read, so
        // `DS:x:GAUGE:x` reports the separator, not the duration.
        // Provenance: black-box compatibility test.
        let (heartbeat_text, bounds) = arguments
            .split_once(':')
            .ok_or_else(|| failed("missing separator"))?;
        if heartbeat_text.len() >= MAX_HEARTBEAT_FIELD {
            return Err(failed("heartbeat too long"));
        }
        let heartbeat = rrd_scaled_duration(heartbeat_text, 1).map_err(failed)?;
        let (minimum_text, maximum_text) =
            split_bounds(bounds.as_bytes()).ok_or_else(|| failed("failed to extract min:max"))?;
        let minimum = parse_bound(&minimum_text, "parsing min val")?;
        let maximum = parse_bound(&maximum_text, "parsing max val")?;
        // NaN compares false, so an unknown bound is never checked.
        if minimum >= maximum {
            return Err(failed("min must be less than max in DS definition"));
        }
        Ok(Self {
            name: word.name.to_owned(),
            kind,
            heartbeat,
            minimum,
            maximum,
        })
    }

    /// The 120-byte definition record (CR-39).
    pub(crate) fn to_bytes(&self) -> [u8; DS_DEF_LEN] {
        let mut record = [0_u8; DS_DEF_LEN];
        put_text(&mut record, 0, self.name.as_bytes());
        put_text(&mut record, TYPE_OFFSET, self.kind.as_str().as_bytes());
        record[HEARTBEAT_OFFSET..HEARTBEAT_OFFSET + 8]
            .copy_from_slice(&self.heartbeat.to_le_bytes());
        record[MIN_OFFSET..MIN_OFFSET + 8].copy_from_slice(&self.minimum.to_le_bytes());
        record[MAX_OFFSET..MAX_OFFSET + 8].copy_from_slice(&self.maximum.to_le_bytes());
        record
    }
}

/// The `min:max` part of the DS arguments: a minimum of 1 to 18 bytes up to
/// a `:`, then a maximum of the next 1 to 18 bytes that are not `:`. Bytes,
/// not characters, are counted, so a cut can split a UTF-8 sequence; the
/// result then fails to convert like any other junk.
fn split_bounds(bytes: &[u8]) -> Option<(String, String)> {
    let colon = bytes.iter().position(|byte| *byte == b':')?;
    if !(1..=MAX_BOUND_LEN).contains(&colon) {
        return None;
    }
    let rest = &bytes[colon + 1..];
    let max_len = rest
        .iter()
        .take(MAX_BOUND_LEN)
        .take_while(|byte| **byte != b':')
        .count();
    if max_len == 0 {
        return None;
    }
    Some((
        String::from_utf8_lossy(&bytes[..colon]).into_owned(),
        String::from_utf8_lossy(&rest[..max_len]).into_owned(),
    ))
}

/// The classic consolidation functions Rondi stores (CR-27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Consolidation {
    Average,
    Minimum,
    Maximum,
    Last,
}

impl Consolidation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Average => "AVERAGE",
            Self::Minimum => "MIN",
            Self::Maximum => "MAX",
            Self::Last => "LAST",
        }
    }
}

const HOLT_WINTERS: [&str; 6] = [
    "HWPREDICT",
    "MHWPREDICT",
    "SEASONAL",
    "DEVSEASONAL",
    "DEVPREDICT",
    "FAILURES",
];

/// A parsed `RRA:` word.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArchiveDef {
    pub(crate) consolidation: Consolidation,
    pub(crate) xff: f64,
    pub(crate) pdp_per_row: u64,
    pub(crate) rows: u64,
}

impl ArchiveDef {
    /// Parses the text after `RRA:`, reading durations against `step`.
    pub(crate) fn parse(spec: &str, step: u64) -> Result<Self, DefinitionError> {
        // CR-26: empty fields are dropped before anything is counted.
        let fields: Vec<&str> = spec.split(':').filter(|field| !field.is_empty()).collect();
        let Some(cf_field) = fields.first() else {
            return Err(DefinitionError::TooFewFields { needed: 4, got: 0 });
        };
        let run_len = cf_field
            .bytes()
            .take(MAX_CF_LEN)
            .take_while(u8::is_ascii_uppercase)
            .count();
        let run = &cf_field[..run_len];
        let consolidation = match run {
            "AVERAGE" => Consolidation::Average,
            "MIN" => Consolidation::Minimum,
            "MAX" => Consolidation::Maximum,
            "LAST" => Consolidation::Last,
            _ if HOLT_WINTERS.contains(&run) => {
                return Err(DefinitionError::HoltWintersRefused(run.to_owned()));
            }
            _ => return Err(DefinitionError::UnknownCf(run.to_owned())),
        };
        // CR-28: the count is checked before the xff.
        let [_, xff_text, steps_text, rows_text] = fields[..] else {
            return Err(if fields.len() < 4 {
                DefinitionError::TooFewFields {
                    needed: 4,
                    got: fields.len(),
                }
            } else {
                DefinitionError::ExtraField(consolidation.as_str())
            });
        };
        let xff = parse_xff(xff_text)?;
        let pdp_per_row =
            rrd_scaled_duration(steps_text, step).map_err(|reason| DefinitionError::Steps {
                field: steps_text.to_owned(),
                reason,
            })?;
        // Compatibility: the row divisor is the slot length in wrapping
        // 64-bit arithmetic; a product of zero reports the duration
        // parser's zero-divisor text.
        // Provenance: black-box compatibility test.
        let rows =
            rrd_scaled_duration(rows_text, step.wrapping_mul(pdp_per_row)).map_err(|reason| {
                DefinitionError::Rows {
                    field: rows_text.to_owned(),
                    reason,
                }
            })?;
        Ok(Self {
            consolidation,
            xff,
            pdp_per_row,
            rows,
        })
    }

    /// The 120-byte definition record (CR-40).
    pub(crate) fn to_bytes(&self) -> [u8; RRA_DEF_LEN] {
        let mut record = [0_u8; RRA_DEF_LEN];
        put_text(&mut record, 0, self.consolidation.as_str().as_bytes());
        record[ROWS_OFFSET..ROWS_OFFSET + 8].copy_from_slice(&self.rows.to_le_bytes());
        record[PDP_OFFSET..PDP_OFFSET + 8].copy_from_slice(&self.pdp_per_row.to_le_bytes());
        record[XFF_OFFSET..XFF_OFFSET + 8].copy_from_slice(&self.xff.to_le_bytes());
        record
    }
}

/// CR-24: any conversion failure and any value outside [0, 1) give the same
/// text; NaN is kept.
fn parse_xff(text: &str) -> Result<f64, DefinitionError> {
    let value = rrd_strtodbl(text, "xff").map_err(|_| DefinitionError::Xff)?;
    if value < 0.0 || value >= 1.0 {
        return Err(DefinitionError::Xff);
    }
    Ok(value)
}

fn put_text(record: &mut [u8], offset: usize, text: &[u8]) {
    let length = text.len().min(NAME_FIELD - 1);
    record[offset..offset + length].copy_from_slice(&text[..length]);
}

/// The NUL-terminated text of a fixed-size name field.
fn field_text(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    &field[..end]
}

/// The name stored in a data source record.
pub(crate) fn stored_ds_name(record: &[u8; DS_DEF_LEN]) -> &[u8] {
    field_text(&record[..NAME_FIELD])
}

/// The consolidation function stored in an archive record.
pub(crate) fn stored_cf(record: &[u8; RRA_DEF_LEN]) -> &[u8] {
    field_text(&record[..NAME_FIELD])
}

/// Whether a stored consolidation function is one Rondi refuses.
pub(crate) fn is_holt_winters(cf: &[u8]) -> bool {
    HOLT_WINTERS.iter().any(|name| name.as_bytes() == cf)
}

/// Row count and PDPs per row of an archive record.
pub(crate) fn stored_archive_shape(record: &[u8; RRA_DEF_LEN]) -> (u64, u64) {
    let word = |offset: usize| {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&record[offset..offset + 8]);
        u64::from_le_bytes(bytes)
    };
    (word(ROWS_OFFSET), word(PDP_OFFSET))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(spec: &str) -> Result<DataSourceDef, String> {
        DataSourceDef::parse(spec).map_err(|error| error.to_string())
    }

    fn rra(spec: &str, step: u64) -> Result<ArchiveDef, String> {
        ArchiveDef::parse(spec, step).map_err(|error| error.to_string())
    }

    #[test]
    fn ds_format_accepts_the_documented_shapes() {
        for spec in [
            "x:GAUGE:20:U:U",
            "a-b:GAUGE:20:U:U",
            "-:GAUGE:20:U:U",
            "x=y:GAUGE:20:U:U",
            "x=y[99]:GAUGE:20:U:U",
            "abcdefghijklmnopqrs:GAUGE:20:U:U",
        ] {
            assert!(ds(spec).is_ok(), "{spec}: {:?}", ds(spec));
        }
    }

    #[test]
    fn ds_format_rejects_malformed_words() {
        for spec in [
            "",
            ":GAUGE:20:U:U",
            "abcdefghijklmnopqrst:GAUGE:20:U:U",
            "x.y:GAUGE:20:U:U",
            "x y:GAUGE:20:U:U",
            "\u{e9}:GAUGE:20:U:U",
            "x=:GAUGE:20:U:U",
            "x=y[:GAUGE:20:U:U",
            "x=y[a]:GAUGE:20:U:U",
            "x=y[-1]:GAUGE:20:U:U",
            "x[1]:GAUGE:20:U:U",
            "x=y=z:GAUGE:20:U:U",
            "x:gauge:20:U:U",
            "x::20:U:U",
            "x",
            "x:GAUGE",
            "x:GAUGE:",
            "y:COMPUTE",
            "y:COMPUTE:",
        ] {
            assert_eq!(ds(spec), Err("invalid DS format".into()), "{spec}");
        }
    }

    #[test]
    fn ds_type_and_refusals_follow_the_format_check() {
        assert_eq!(
            ds("x:GAUGEX:20:U:U"),
            Err("invalid DS type specified (GAUGEX)".into())
        );
        assert_eq!(
            ds("x:COMPUTEX:20:U:U"),
            Err("invalid DS type specified (COMPUTEX)".into())
        );
        assert_eq!(
            ds("y:COMPUTE:x,2,*"),
            Err("COMPUTE data sources are not supported by Rondi".into())
        );
    }

    #[test]
    fn ds_argument_errors_come_in_rule_order() {
        let cases = [
            (
                "x:GAUGE:0:U:U",
                "failed to parse data source 0:U:U: value must be positive",
            ),
            (
                "x:GAUGE:20x:U:U",
                "failed to parse data source 20x:U:U: value has trailing garbage",
            ),
            (
                "x:GAUGE:20",
                "failed to parse data source 20: missing separator",
            ),
            (
                "x:GAUGE:x",
                "failed to parse data source x: missing separator",
            ),
            (
                "x:GAUGE:12345678901234567890123456789012:U:U",
                "failed to parse data source 12345678901234567890123456789012:U:U: heartbeat too long",
            ),
            (
                "x:GAUGE:20:U",
                "failed to parse data source 20:U: failed to extract min:max",
            ),
            (
                "x:GAUGE:20::U",
                "failed to parse data source 20::U: failed to extract min:max",
            ),
            (
                "x:GAUGE:20:U:",
                "failed to parse data source 20:U:: failed to extract min:max",
            ),
            (
                "x:GAUGE:20:1234567890123456789:U",
                "failed to parse data source 20:1234567890123456789:U: failed to extract min:max",
            ),
            (
                "x:GAUGE:20:abc:U",
                "parsing min val - Cannot convert 'abc' to float",
            ),
            (
                "x:GAUGE:20:Ux:U",
                "parsing min val - Cannot convert 'Ux' to float",
            ),
            (
                "x:GAUGE:20:0x10:U",
                "parsing min val - Converted '0x10' to 0.000000, but cannot convert 'x10'",
            ),
            (
                "x:GAUGE:20:5:u",
                "parsing max val - Cannot convert 'u' to float",
            ),
            (
                "x:GAUGE:20:5:5",
                "failed to parse data source 20:5:5: min must be less than max in DS definition",
            ),
            (
                "x:GAUGE:20:-0:0",
                "failed to parse data source 20:-0:0: min must be less than max in DS definition",
            ),
            (
                "x:GAUGE:20:inf:-inf",
                "failed to parse data source 20:inf:-inf: min must be less than max in DS definition",
            ),
        ];
        for (spec, message) in cases {
            assert_eq!(ds(spec), Err(message.into()), "{spec}");
        }
    }

    #[test]
    fn ds_values_are_stored_as_read() {
        let def = ds("abc=q[3]:DCOUNTER:5m:-inf:1234567890123456789:junk").unwrap();
        assert_eq!(def.name, "abc");
        assert_eq!(def.kind, DsKind::DCounter);
        assert_eq!(def.heartbeat, 300);
        assert_eq!(def.minimum, f64::NEG_INFINITY);
        // A maximum longer than 18 bytes is cut (CR-20).
        assert_eq!(
            Some(def.maximum),
            crate::parse_rrd_number("123456789012345678")
        );
        assert!(ds("x:GAUGE:20:nan:5").is_ok());
        assert!(ds("x:GAUGE:20:U:U").unwrap().minimum.is_nan());
        let record = def.to_bytes();
        assert_eq!(&record[..4], b"abc\0");
        assert_eq!(&record[20..29], b"DCOUNTER\0");
        assert_eq!(stored_ds_name(&record), b"abc");
        assert_eq!(u64::from_le_bytes(record[40..48].try_into().unwrap()), 300);
        assert!(record[64..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rra_fields_and_counts() {
        let def = rra("AVERAGE:0.5:1m:1h", 10).unwrap();
        assert_eq!((def.pdp_per_row, def.rows), (6, 60));
        assert!(rra(":AVERAGE::0.5:1:5::", 10).is_ok());
        assert_eq!(
            rra("AVERAGE1:0.5:1:5", 10).unwrap().consolidation,
            Consolidation::Average
        );
        let cases = [
            ("", "Expected at least 4 arguments for RRA but got 0"),
            (":::", "Expected at least 4 arguments for RRA but got 0"),
            ("AVERAGE", "Expected at least 4 arguments for RRA but got 1"),
            (
                "LAST:0.5",
                "Expected at least 4 arguments for RRA but got 2",
            ),
            (
                "AVERAGE:0.5:1",
                "Expected at least 4 arguments for RRA but got 3",
            ),
            (
                "AVERAGE:0.5:1:5:9",
                "Unexpected extra argument for consolidation function AVERAGE",
            ),
            ("FOO", "Unrecognized consolidation function FOO"),
            ("Average:0.5:1:5", "Unrecognized consolidation function A"),
            ("average:0.5:1:5", "Unrecognized consolidation function "),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZ:0.5:1:5",
                "Unrecognized consolidation function ABCDEFGHIJKLMNOPQRS",
            ),
            ("AVERAGE:1:1:5", "Invalid xff: must be between 0 and 1"),
            ("AVERAGE:U:1:5", "Invalid xff: must be between 0 and 1"),
            ("AVERAGE:-0.1:x:5", "Invalid xff: must be between 0 and 1"),
            (
                "AVERAGE:0.5:7s:5",
                "Invalid step 7s: value would truncate when scaled",
            ),
            ("AVERAGE:0.5:0:5", "Invalid step 0: value must be positive"),
            (
                "AVERAGE:0.5:1m:65s",
                "Invalid row count 65s: value would truncate when scaled",
            ),
            (
                "HWPREDICT:20:0:0.01:5",
                "Holt-Winters consolidation function HWPREDICT is not supported by Rondi",
            ),
        ];
        for (spec, message) in cases {
            assert_eq!(rra(spec, 10), Err(message.into()), "{spec}");
        }
        assert_eq!(
            rra("AVERAGE:0.5:2:5", 1 << 63),
            Err("Invalid row count 5: INTERNAL ERROR: Zero divisor".into())
        );
    }

    #[test]
    fn rra_xff_keeps_nan_and_negative_zero() {
        assert!(rra("AVERAGE:nanx:1:5", 10).unwrap().xff.is_nan());
        let zero = rra("AVERAGE:-0:1:5", 10).unwrap().xff;
        assert!(zero == 0.0 && zero.is_sign_negative());
        let record = rra("MAX:0.25:3:7", 10).unwrap().to_bytes();
        assert_eq!(stored_cf(&record), b"MAX");
        assert_eq!(stored_archive_shape(&record), (7, 3));
        assert_eq!(f64::from_le_bytes(record[40..48].try_into().unwrap()), 0.25);
    }

    /// Random words never panic, and every accepted definition serializes.
    #[test]
    fn random_definition_words_never_panic() {
        const ALPHABET: &[u8] = b"DSRA:=[]0123456789UuxmshdwMy.-+eEnaifGAUGEVRMINLTC\xc3\xa9 ";
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let length = (next() % 40) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize])
                .collect();
            let text = String::from_utf8_lossy(&bytes);
            let step = next() % 4 + 1;
            if let Ok(def) = DataSourceDef::parse(&text) {
                let _ = def.to_bytes();
            }
            if let Ok(def) = ArchiveDef::parse(&text, step << (next() % 64)) {
                let _ = def.to_bytes();
            }
            for prefix in ["x:GAUGE:", "AVERAGE:", "x:COUNTER:20:"] {
                let _ = DataSourceDef::parse(&format!("{prefix}{text}"));
                let _ = ArchiveDef::parse(&format!("{prefix}{text}"), step);
            }
        }
    }
}
