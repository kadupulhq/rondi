//! `rrdtool restore`: an XML dump back into a binary RRD file.
//!
//! The work happens in three stages. A walk over the reader's events
//! (`xml_reader`) fills a typed model of the dump, applying each field's
//! grammar and recording the single message RRDtool keeps while it runs.
//! The model is then laid out in the binary format `rrdtool create` writes,
//! and finally the target is opened and written. The behavior is specified
//! in docs/compatibility/restore.md; rule numbers (RS-n) refer to it.
//!
//! Compatibility: the walk, the field grammars, the messages and the file
//! layout reproduce RRDtool 1.11.0.
//! Provenance: black-box compatibility tests and the behavioral
//! specification in docs/compatibility/restore.md; this module was written
//! from that specification and the vectors only.

use std::fmt;
use std::io::Write;
use std::path::Path;

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN,
    STAT_HEAD_LEN, VALUE_LEN, rrd_nan,
};
use crate::xml_reader::{Delivery, Diagnostics, Event, Read, Reader, prepare};

/// Where the dump comes from.
#[derive(Clone, Copy, Debug)]
pub enum RestoreInput<'a> {
    /// Standard input: read a line at a time, with no XML diagnostics.
    Stdin(&'a [u8]),
    /// A named file, whose name prefixes the XML library's diagnostics.
    File { name: &'a str, xml: &'a [u8] },
}

/// Which integer conversion failed, for the message prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreInteger {
    Unsigned,
    Time,
}

/// The container whose unknown-element message is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreScope {
    Rrd,
    Ds,
    Rra,
    Params,
    CdpPrep,
}

/// Every message a restore can end with. `Display` gives the exact text
/// RRDtool prints after `ERROR: `.
#[derive(Clone, Debug, PartialEq)]
pub enum RestoreError {
    /// The input file could not be opened.
    OpenInput(String),
    /// A syntax error reported by the XML parser, with its own wording.
    Xml(String),
    ExpectedElementFoundText {
        line: u64,
        text: String,
    },
    XmlEndedLookingForElement,
    /// RS-13: the two names appear swapped relative to their meaning.
    ExpectedElement {
        line: u64,
        found: String,
        expected: String,
    },
    ExpectedEndElement {
        line: u64,
        expected: String,
        found: String,
    },
    ExpectedValue {
        line: u64,
        element: String,
    },
    FileEndedLookingForText,
    UnknownTag {
        container: RestoreScope,
        line: u64,
        name: String,
    },
    UnexpectedDatabaseTag {
        line: u64,
        name: String,
    },
    DataSourceAfterArchive,
    TooLong {
        token: String,
        limit: usize,
    },
    UnknownType(String),
    UnknownCf(String),
    Integer {
        line: u64,
        kind: RestoreInteger,
        token: String,
        errno: i32,
    },
    Double {
        line: u64,
        token: String,
        errno: i32,
    },
    /// An archive whose `<database>` held no rows (RS-63).
    EmptyDatabase,
    /// A feature Rondi does not implement.
    Unsupported(&'static str),
    /// The restored file would not fit in memory or a file offset.
    TooLarge,
    Create {
        name: String,
        reason: String,
    },
    Write {
        name: String,
        reason: String,
    },
}

fn strerror(errno: i32) -> String {
    crate::rrd_binary::rrd_strerror(&std::io::Error::from_raw_os_error(errno))
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::OpenInput(name) => write!(f, "Could not create xml reader for: {name}"),
            RestoreError::Xml(message) => {
                write!(f, "error reading/parsing XML: {}", library_text(message))
            }
            RestoreError::ExpectedElementFoundText { line, text } => {
                write!(f, "line {line}: expected element but found text '{text}'")
            }
            RestoreError::XmlEndedLookingForElement => {
                f.write_str("the xml ended while we were looking for an element")
            }
            RestoreError::ExpectedElement {
                line,
                found,
                expected,
            } => write!(
                f,
                "line {line}: expected <{found}> element but found <{expected}>"
            ),
            RestoreError::ExpectedEndElement {
                line,
                expected,
                found,
            } => write!(
                f,
                "line {line}: expected </{expected}> end element but found <{found}>"
            ),
            RestoreError::ExpectedValue { line, element } => {
                write!(
                    f,
                    "line {line}: expected a value but found a <{element}> element"
                )
            }
            RestoreError::FileEndedLookingForText => {
                f.write_str("file ended while looking for text")
            }
            RestoreError::UnknownTag {
                container,
                line,
                name,
            } => match container {
                RestoreScope::Rrd => write!(f, "parse_tag_rrd: Unknown tag: {name}"),
                RestoreScope::Ds => write!(f, "parse_tag_ds: Unknown tag: {name}"),
                RestoreScope::Rra => write!(f, "line {line}: parse_tag_rra: Unknown tag: {name}"),
                RestoreScope::Params => {
                    write!(f, "line {line}: parse_tag_rra_params: Unknown tag: {name}")
                }
                RestoreScope::CdpPrep => write!(f, "parse_tag_rra_cdp_prep: Unknown tag: {name}"),
            },
            RestoreError::UnexpectedDatabaseTag { line, name } => {
                write!(f, "line {line}: found unexpected tag: {name}")
            }
            RestoreError::DataSourceAfterArchive => f.write_str(
                "parse_tag_ds: All data source definitions MUST precede the RRA definitions!",
            ),
            RestoreError::TooLong { token, limit } => write!(f, "'{token}' is longer than {limit}"),
            RestoreError::UnknownType(token) => {
                write!(f, "parse_tag_ds_type: Unknown data source type: {token}")
            }
            RestoreError::UnknownCf(token) => {
                write!(
                    f,
                    "parse_tag_rra_cf: Unknown consolidation function: {token}"
                )
            }
            RestoreError::Integer {
                line,
                kind,
                token,
                errno,
            } => {
                let function = match kind {
                    RestoreInteger::Unsigned => "get_xml_ulong",
                    RestoreInteger::Time => "get_xml_time_t",
                };
                // The reference spells this prefix "ling".
                write!(
                    f,
                    "ling {line}: {function} from '{token}' {}",
                    strerror(*errno)
                )
            }
            RestoreError::Double { line, token, errno } => write!(
                f,
                "ling {line}: get_xml_double from '{token}' {}",
                strerror(*errno)
            ),
            // Compatibility: aarch64 builds report this text (RS-63); x86_64
            // builds die from SIGFPE, so there Rondi says what it refused.
            // Provenance: black-box compatibility test
            // (`tiny-self-closing-database` and its Linux overrides).
            RestoreError::EmptyDatabase if cfg!(target_arch = "x86_64") => {
                f.write_str("restore of an RRA without rows is not supported")
            }
            RestoreError::EmptyDatabase => f.write_str("parse_tag_rra: malloc failed."),
            RestoreError::Unsupported(what) => write!(f, "{what} not supported by Rondi"),
            RestoreError::TooLarge => f.write_str("the restored RRD is too large"),
            RestoreError::Create { name, reason } => write!(f, "creating '{name}': {reason}"),
            RestoreError::Write { name, reason } => write!(f, "writing '{name}': {reason}"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// The XML library's message as the reference reports it (RS-40): the
/// message keeps its final newline, every newline becomes a space and the
/// text stops at the first other non-printable byte after the first.
fn library_text(message: &str) -> String {
    let mut text = String::with_capacity(message.len() + 1);
    for (index, ch) in message.chars().chain(std::iter::once('\n')).enumerate() {
        match ch {
            '\n' => text.push(' '),
            ch if index > 0 && (ch < ' ' || ch == '\u{7f}') => break,
            ch => text.push(ch),
        }
    }
    text
}

// ---------------------------------------------------------------------------
// The model of a dump.

/// One 8-byte parameter slot block (ten slots), kept as raw bytes because
/// slots mix doubles and unsigned integers and `<history>` writes bytes.
type Slots = [u8; 80];

#[derive(Clone, Debug, Default)]
struct DataSource {
    name: Vec<u8>,
    kind: Vec<u8>,
    heartbeat: u64,
    min: f64,
    max: f64,
    last_ds: Vec<u8>,
    pdp_value: f64,
    unknown_sec: u64,
}

#[derive(Clone, Debug)]
struct Archive {
    cf: Vec<u8>,
    pdp_per_row: u64,
    params: Slots,
    /// One CDP record per data source.
    cdp: Vec<Slots>,
    /// Row values in dump order, one per data source per row.
    values: Vec<f64>,
    rows: u64,
}

#[derive(Clone, Debug, Default)]
struct Dump {
    version: Vec<u8>,
    step: u64,
    last_update: i64,
    sources: Vec<DataSource>,
    archives: Vec<Archive>,
}

fn set_f64(slots: &mut Slots, slot: usize, value: f64) {
    slots[slot * 8..slot * 8 + 8].copy_from_slice(&value.to_le_bytes());
}

fn set_u64(slots: &mut Slots, slot: usize, value: u64) {
    slots[slot * 8..slot * 8 + 8].copy_from_slice(&value.to_le_bytes());
}

/// The leading decimal number of the version string; none reads as 0.
fn version_number(version: &[u8]) -> u64 {
    version
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .fold(0_u64, |number, digit| {
            number
                .saturating_mul(10)
                .saturating_add(u64::from(digit - b'0'))
        })
}

// ---------------------------------------------------------------------------
// Field grammars (section 4 of the specification).

/// The first run of non-white-space characters (RS-31).
fn token(text: &str) -> &str {
    let is_space = |ch: char| matches!(ch, ' ' | '\t' | '\r' | '\n');
    let trimmed = text.trim_start_matches(is_space);
    trimmed.split(is_space).next().unwrap_or("")
}

/// Why a double did not convert, to pick the error text.
#[derive(Debug, PartialEq)]
struct BadDouble;

/// RS-37 and RS-38: special spellings first, then RRDtool's decimal
/// conversion, which is not correctly rounded. `errno` follows RS-39a.
fn convert_double(token: &str, errno: &mut i32) -> Result<f64, BadDouble> {
    let lower = token.to_ascii_lowercase();
    if lower.contains("nan") {
        return Ok(rrd_nan());
    }
    if lower.contains("-inf") {
        return Ok(f64::NEG_INFINITY);
    }
    if lower.contains("inf") {
        return Ok(f64::INFINITY);
    }
    let bytes = token.as_bytes();
    let mut at = 0;
    while bytes
        .get(at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        at += 1;
    }
    let negative = bytes.get(at) == Some(&b'-');
    if matches!(bytes.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let mut mantissa = 0.0_f64;
    let mut digits = 0_usize;
    let mut fraction = 0_i32;
    let mut after_point = false;
    loop {
        match bytes.get(at) {
            Some(digit @ b'0'..=b'9') => {
                mantissa = crate::rrd_binary::rrd_mul_add(mantissa, 10.0, f64::from(digit - b'0'));
                digits += 1;
                if after_point {
                    fraction = fraction.wrapping_add(1);
                }
            }
            Some(b'.') if !after_point => after_point = true,
            _ => break,
        }
        at += 1;
    }
    if digits == 0 {
        *errno = libc::ERANGE;
        return Err(BadDouble);
    }
    if negative {
        mantissa = -mantissa;
    }
    let mut exponent = 0_i32;
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        let exponent_negative = bytes.get(at) == Some(&b'-');
        if matches!(bytes.get(at), Some(b'-' | b'+')) {
            at += 1;
        }
        let mut written = 0_i32;
        while let Some(digit @ b'0'..=b'9') = bytes.get(at) {
            // The reference accumulates into a C int, which wraps.
            written = written
                .wrapping_mul(10)
                .wrapping_add(i32::from(digit - b'0'));
            at += 1;
        }
        exponent = if exponent_negative {
            written.wrapping_neg()
        } else {
            written
        };
    }
    let exponent = exponent.wrapping_sub(fraction);
    if !(-1021..=1024).contains(&exponent) {
        *errno = libc::ERANGE;
        return Err(BadDouble);
    }
    let mut power = 10.0_f64;
    let mut remaining = exponent.unsigned_abs();
    while remaining != 0 {
        if remaining & 1 == 1 {
            if exponent < 0 {
                mantissa /= power;
            } else {
                mantissa *= power;
            }
        }
        remaining >>= 1;
        power *= power;
    }
    if mantissa.is_infinite() {
        *errno = libc::ERANGE;
    }
    if at != bytes.len() {
        return Err(BadDouble);
    }
    Ok(mantissa)
}

const DS_TYPES: [&str; 7] = [
    "GAUGE", "COUNTER", "DERIVE", "ABSOLUTE", "DCOUNTER", "DDERIVE", "COMPUTE",
];
const CF_NAMES: [&str; 10] = [
    "AVERAGE",
    "MIN",
    "MAX",
    "LAST",
    "HWPREDICT",
    "MHWPREDICT",
    "SEASONAL",
    "DEVSEASONAL",
    "DEVPREDICT",
    "FAILURES",
];

const COMPUTE: &str = "COMPUTE data sources are";

// ---------------------------------------------------------------------------
// The walk (section 3 of the specification).

/// Why the walk stopped early.
#[derive(Debug, PartialEq)]
enum Stop {
    /// Nothing is written; the message decides the exit status.
    Fatal,
    /// RS-27: an empty `<params>` or CDP `<ds>` ends the restore quietly.
    Silent,
}

type Step<T = ()> = Result<T, Stop>;

/// What a read of the next element returned.
#[derive(Clone, Debug)]
enum Tag {
    Open(String),
    Close(String),
}

impl Tag {
    /// The name as the reference prints it: end tags carry a `/`.
    fn shown(&self) -> String {
        match self {
            Tag::Open(name) => name.clone(),
            Tag::Close(name) => format!("/{name}"),
        }
    }
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

struct Walk<'a, 'w> {
    reader: Reader<'a, 'w>,
    message: Option<RestoreError>,
    /// The C library error state that double conversions report (RS-39a).
    errno: i32,
    range_check: bool,
    dump: Dump,
}

impl Walk<'_, '_> {
    fn set(&mut self, error: RestoreError) {
        self.message = Some(error);
    }

    fn fatal<T>(&mut self, error: RestoreError) -> Step<T> {
        self.set(error);
        Err(Stop::Fatal)
    }

    fn line(&mut self) -> u64 {
        self.reader.line()
    }

    fn xml_failure(&self) -> RestoreError {
        RestoreError::Xml(self.reader.failure().unwrap_or_default().to_owned())
    }

    /// RS-12: the next start or end tag, skipping comments, processing
    /// instructions, CDATA and white space. On failure the message is set.
    fn read_element(&mut self) -> Result<Tag, ()> {
        loop {
            match self.reader.read() {
                Read::Event(Event::Start { name, .. }) => return Ok(Tag::Open(name)),
                Read::Event(Event::End { name }) => return Ok(Tag::Close(name)),
                Read::Event(Event::Text { text, blank: false }) => {
                    let line = self.line();
                    self.set(RestoreError::ExpectedElementFoundText { line, text });
                    return Err(());
                }
                Read::Event(_) => {}
                Read::Eof => {
                    self.set(RestoreError::XmlEndedLookingForElement);
                    return Err(());
                }
                Read::Failed => {
                    let error = self.xml_failure();
                    self.set(error);
                    return Err(());
                }
            }
        }
    }

    /// RS-13: the next element must be `expected` (an end tag written with
    /// a leading `/`).
    fn expect_element(&mut self, expected: &str) -> Step {
        let tag = self.read_element().map_err(|()| Stop::Fatal)?;
        let found = tag.shown();
        if same(&found, expected) {
            return Ok(());
        }
        let line = self.line();
        self.fatal(RestoreError::ExpectedElement {
            line,
            found,
            expected: expected.to_owned(),
        })
    }

    /// RS-14: the end of `name`, unless the last event already was it.
    fn expect_end(&mut self, name: &str) -> Result<(), ()> {
        if let Some(Event::End { name: last }) = self.reader.last()
            && same(last, name)
        {
            return Ok(());
        }
        let tag = self.read_element()?;
        if let Tag::Close(found) = &tag
            && same(found, name)
        {
            return Ok(());
        }
        let line = self.line();
        self.set(RestoreError::ExpectedEndElement {
            line,
            expected: name.to_owned(),
            found: tag.shown(),
        });
        Err(())
    }

    /// RS-15: the token of the element's text.
    fn read_value(&mut self) -> Result<String, ()> {
        let mut retrying = false;
        loop {
            let event = if retrying {
                match self.reader.last().cloned() {
                    Some(event) => Read::Event(event),
                    None => Read::Failed,
                }
            } else {
                self.reader.read()
            };
            retrying = false;
            match event {
                Read::Event(Event::Start { name, .. }) => {
                    let line = self.line();
                    self.set(RestoreError::ExpectedValue {
                        line,
                        element: name,
                    });
                    return Err(());
                }
                Read::Event(Event::End { .. }) => return Ok(String::new()),
                Read::Event(Event::Text { text, blank: false }) => {
                    return Ok(token(&text).to_owned());
                }
                Read::Event(_) => {
                    // RS-15a: after a parse error the reference retries
                    // forever on a comment or white space; stop instead.
                    if self.reader.is_dead() && self.reader.failure().is_some() {
                        let error = self.xml_failure();
                        self.set(error);
                        return Err(());
                    }
                }
                Read::Eof => {
                    self.set(RestoreError::FileEndedLookingForText);
                    return Err(());
                }
                Read::Failed => {
                    if self.reader.last().is_none() {
                        let error = self.xml_failure();
                        self.set(error);
                        return Err(());
                    }
                    retrying = true;
                }
            }
        }
    }

    fn read_string(&mut self, limit: usize) -> Result<Vec<u8>, ()> {
        let value = self.read_value()?;
        if value.len() >= limit {
            self.set(RestoreError::TooLong {
                token: value,
                limit,
            });
            return Err(());
        }
        Ok(value.into_bytes())
    }

    /// RS-34 and RS-36: C `strtoul` with base 0.
    fn read_unsigned(&mut self) -> Result<u64, ()> {
        let value = self.read_value()?;
        let (number, errno) = crate::rrd_number::c_strtoul_errno(&value, 0);
        self.errno = errno;
        if errno != 0 {
            let line = self.line();
            self.set(RestoreError::Integer {
                line,
                kind: RestoreInteger::Unsigned,
                token: value,
                errno,
            });
            return Err(());
        }
        Ok(number)
    }

    /// RS-35: C `strtoll` with base 0.
    fn read_time(&mut self) -> Result<i64, ()> {
        let value = self.read_value()?;
        let (number, errno) = crate::rrd_number::c_strtoll_errno(&value, 0);
        self.errno = errno;
        if errno != 0 {
            let line = self.line();
            self.set(RestoreError::Integer {
                line,
                kind: RestoreInteger::Time,
                token: value,
                errno,
            });
            return Err(());
        }
        Ok(number)
    }

    fn read_double(&mut self) -> Result<f64, ()> {
        let value = self.read_value()?;
        match convert_double(&value, &mut self.errno) {
            Ok(number) => Ok(number),
            Err(BadDouble) => {
                let line = self.line();
                let errno = self.errno;
                self.set(RestoreError::Double {
                    line,
                    token: value,
                    errno,
                });
                Err(())
            }
        }
    }

    /// Reads a field with `read`, then its end tag; any failure is fatal.
    fn field<T>(&mut self, name: &str, read: impl FnOnce(&mut Self) -> Result<T, ()>) -> Step<T> {
        let value = read(self).map_err(|()| Stop::Fatal)?;
        self.expect_end(name).map_err(|()| Stop::Fatal)?;
        Ok(value)
    }

    fn unknown<T>(&mut self, container: RestoreScope, tag: &Tag) -> Step<T> {
        let line = self.line();
        self.fatal(RestoreError::UnknownTag {
            container,
            line,
            name: tag.shown(),
        })
    }

    fn root(&mut self) -> Step {
        self.expect_element("rrd")?;
        loop {
            // RS-17: a failed read ends the root as if `</rrd>` was read.
            let Ok(tag) = self.read_element() else {
                return Ok(());
            };
            let Tag::Open(name) = &tag else {
                if let Tag::Close(name) = &tag
                    && same(name, "rrd")
                {
                    return Ok(());
                }
                return self.unknown(RestoreScope::Rrd, &tag);
            };
            match name.to_ascii_lowercase().as_str() {
                "version" => self.dump.version = self.field(name, |walk| walk.read_string(5))?,
                "step" => self.dump.step = self.field(name, Self::read_unsigned)?,
                "lastupdate" => self.dump.last_update = self.field(name, Self::read_time)?,
                "ds" => {
                    if !self.dump.archives.is_empty() {
                        return self.fatal(RestoreError::DataSourceAfterArchive);
                    }
                    self.data_source()?;
                }
                "rra" => self.archive()?,
                _ => return self.unknown(RestoreScope::Rrd, &tag),
            }
        }
    }

    fn data_source(&mut self) -> Step {
        let mut source = DataSource::default();
        while let Ok(tag) = self.read_element() {
            let Tag::Open(name) = &tag else {
                if let Tag::Close(name) = &tag
                    && same(name, "ds")
                {
                    break;
                }
                return self.unknown(RestoreScope::Ds, &tag);
            };
            match name.to_ascii_lowercase().as_str() {
                "name" => source.name = self.field(name, |walk| walk.read_string(20))?,
                "type" => {
                    let kind = self.field(name, |walk| walk.read_string(20))?;
                    let text = String::from_utf8_lossy(&kind).into_owned();
                    if !DS_TYPES.contains(&text.as_str()) {
                        return self.fatal(RestoreError::UnknownType(text));
                    }
                    if text == "COMPUTE" {
                        return self.fatal(RestoreError::Unsupported(COMPUTE));
                    }
                    source.kind = kind;
                }
                "minimal_heartbeat" => source.heartbeat = self.field(name, Self::read_unsigned)?,
                "min" => source.min = self.field(name, Self::read_double)?,
                "max" => source.max = self.field(name, Self::read_double)?,
                "value" => source.pdp_value = self.field(name, Self::read_double)?,
                "last_ds" => source.last_ds = self.field(name, |walk| walk.read_string(30))?,
                "unknown_sec" => source.unknown_sec = self.field(name, Self::read_unsigned)?,
                // RS-70: computed data sources are not implemented.
                "cdef" => return self.fatal(RestoreError::Unsupported(COMPUTE)),
                _ => return self.unknown(RestoreScope::Ds, &tag),
            }
        }
        self.dump.sources.push(source);
        Ok(())
    }

    fn archive(&mut self) -> Step {
        let sources = self.dump.sources.len();
        let mut archive = Archive {
            cf: Vec::new(),
            pdp_per_row: 0,
            params: [0; 80],
            cdp: vec![[0; 80]; sources],
            values: Vec::new(),
            rows: 0,
        };
        let version = version_number(&self.dump.version);
        while let Ok(tag) = self.read_element() {
            let Tag::Open(name) = &tag else {
                if let Tag::Close(name) = &tag
                    && same(name, "rra")
                {
                    break;
                }
                return self.unknown(RestoreScope::Rra, &tag);
            };
            match name.to_ascii_lowercase().as_str() {
                "cf" => {
                    let cf = self.field(name, |walk| walk.read_string(20))?;
                    let text = String::from_utf8_lossy(&cf).into_owned();
                    if !CF_NAMES.contains(&text.as_str()) {
                        return self.fatal(RestoreError::UnknownCf(text));
                    }
                    archive.cf = cf;
                }
                "pdp_per_row" => archive.pdp_per_row = self.field(name, Self::read_unsigned)?,
                // RS-20: version 1 dumps keep xff directly in the archive.
                "xff" if version == 1 => {
                    let xff = self.field(name, Self::read_double)?;
                    set_f64(&mut archive.params, 0, xff);
                }
                "params" if version >= 2 => self.params(&mut archive.params)?,
                "cdp_prep" => self.cdp_prep(&mut archive.cdp)?,
                "database" => self.database(&mut archive)?,
                _ => return self.unknown(RestoreScope::Rra, &tag),
            }
        }
        self.dump.archives.push(archive);
        Ok(())
    }

    /// RS-25, RS-27, RS-28: field failures here are recoverable.
    fn params(&mut self, slots: &mut Slots) -> Step {
        let mut seen = false;
        loop {
            let tag = match self.read_element() {
                Ok(tag) => tag,
                Err(()) if seen => return Ok(()),
                Err(()) => return Err(Stop::Fatal),
            };
            if !seen
                && let Tag::Close(name) = &tag
                && same(name, "params")
            {
                return Err(Stop::Silent);
            }
            if let Tag::Close(name) = &tag
                && same(name, "params")
            {
                return Ok(());
            }
            seen = true;
            let name = tag.shown();
            let slot = match &tag {
                Tag::Close(_) => None,
                Tag::Open(name) => match name.to_ascii_lowercase().as_str() {
                    "xff" => Some((0, false)),
                    "hw_alpha" | "seasonal_gamma" | "delta_pos" => Some((1, false)),
                    "hw_beta" | "smoothing_window" | "delta_neg" => Some((2, false)),
                    "dependent_rra_idx" => Some((3, true)),
                    "seasonal_smooth_idx" | "window_len" => Some((4, true)),
                    "failure_threshold" => Some((5, true)),
                    "value" => {
                        self.legacy_value(slots);
                        self.expect_end(name).map_err(|()| Stop::Fatal)?;
                        continue;
                    }
                    _ => None,
                },
            };
            match slot {
                Some((slot, true)) => {
                    if let Ok(value) = self.read_unsigned() {
                        set_u64(slots, slot, value);
                    }
                }
                Some((slot, false)) => {
                    if let Ok(value) = self.read_double() {
                        set_f64(slots, slot, value);
                    }
                }
                None => {
                    let line = self.line();
                    self.set(RestoreError::UnknownTag {
                        container: RestoreScope::Params,
                        line,
                        name: name.clone(),
                    });
                }
            }
            self.expect_end(&name).map_err(|()| Stop::Fatal)?;
        }
    }

    /// RS-28: the old `<value>` form stores its number in slot 0 and then
    /// fails reading a second value from the element's end.
    fn legacy_value(&mut self, slots: &mut Slots) {
        if let Ok(value) = self.read_double() {
            set_f64(slots, 0, value);
            let _ = self.read_double();
        }
    }

    /// RS-21: one `<ds>` per data source, then `</cdp_prep>`.
    fn cdp_prep(&mut self, records: &mut [Slots]) -> Step {
        for record in records.iter_mut() {
            self.expect_element("ds")?;
            self.cdp_record(record)?;
        }
        self.expect_element("/cdp_prep")
    }

    fn cdp_record(&mut self, record: &mut Slots) -> Step {
        let mut seen = false;
        loop {
            let tag = match self.read_element() {
                Ok(tag) => tag,
                Err(()) if seen => return Ok(()),
                Err(()) => return Err(Stop::Fatal),
            };
            if let Tag::Close(name) = &tag
                && same(name, "ds")
            {
                return if seen { Ok(()) } else { Err(Stop::Silent) };
            }
            seen = true;
            let Tag::Open(name) = &tag else {
                return self.unknown(RestoreScope::CdpPrep, &tag);
            };
            let lower = name.to_ascii_lowercase();
            let double_slot = match lower.as_str() {
                "value" => Some(0),
                "intercept" | "seasonal" => Some(2),
                "last_intercept" | "last_seasonal" => Some(3),
                "slope" => Some(4),
                "last_slope" => Some(5),
                "primary_value" => Some(8),
                "secondary_value" => Some(9),
                _ => None,
            };
            let unsigned_slot = match lower.as_str() {
                "unknown_datapoints" => Some(1),
                "nan_count" | "init_flag" => Some(6),
                "last_nan_count" => Some(7),
                _ => None,
            };
            if let Some(slot) = double_slot {
                let value = self.field(name, Self::read_double)?;
                set_f64(record, slot, value);
            } else if let Some(slot) = unsigned_slot {
                let value = self.field(name, Self::read_unsigned)?;
                set_u64(record, slot, value);
            } else if lower == "history" {
                // RS-26: one byte per character, 1 for `1`, at most ten.
                let history = self.field(name, Self::read_value)?;
                for (byte, ch) in record.iter_mut().zip(history.chars().take(10)) {
                    *byte = u8::from(ch == '1');
                }
            } else {
                return self.unknown(RestoreScope::CdpPrep, &tag);
            }
        }
    }

    /// RS-22 and RS-63: rows until `</database>`.
    fn database(&mut self, archive: &mut Archive) -> Step {
        let mut rows = 0_u64;
        let mut failed = false;
        while let Ok(tag) = self.read_element() {
            match &tag {
                Tag::Close(name) if same(name, "database") => break,
                Tag::Open(name) if same(name, "row") => {
                    self.row(&mut archive.values)?;
                    rows += 1;
                }
                _ => {
                    let line = self.line();
                    self.set(RestoreError::UnexpectedDatabaseTag {
                        line,
                        name: tag.shown(),
                    });
                    failed = true;
                    break;
                }
            }
        }
        // Compatibility: a database without rows is refused even when an
        // unexpected tag already ended it (RS-63). Provenance: black-box
        // compatibility test (`walk-database-other-elem`).
        if rows == 0 {
            return self.fatal(RestoreError::EmptyDatabase);
        }
        if failed {
            return Err(Stop::Fatal);
        }
        archive.rows = archive.rows.saturating_add(rows);
        Ok(())
    }

    fn row(&mut self, values: &mut Vec<f64>) -> Step {
        for index in 0..self.dump.sources.len() {
            self.expect_element("v")?;
            let mut value = self.read_double().map_err(|()| Stop::Fatal)?;
            if self.range_check {
                let source = &self.dump.sources[index];
                // RS-23: inclusive bounds; a NaN bound is not checked.
                let below = !source.min.is_nan() && value < source.min;
                let above = !source.max.is_nan() && value > source.max;
                if below || above {
                    value = rrd_nan();
                }
            }
            values.push(value);
            self.expect_element("/v")?;
        }
        self.expect_element("/row")
    }
}

// ---------------------------------------------------------------------------
// The binary layout (section 6 of the specification).

fn put_string(out: &mut Vec<u8>, value: &[u8], width: usize) {
    let mut field = vec![0_u8; width];
    let len = value.len().min(width);
    field[..len].copy_from_slice(&value[..len]);
    out.extend_from_slice(&field);
}

/// Lays the dump out as `rrdtool create` would on a 64-bit little-endian
/// platform (RS-50 to RS-53, RS-61, RS-62).
fn serialize(dump: &Dump) -> Result<Vec<u8>, RestoreError> {
    let sources = dump.sources.len();
    let archives = dump.archives.len();
    let rows: u64 = dump
        .archives
        .iter()
        .try_fold(0_u64, |total, archive| total.checked_add(archive.rows))
        .ok_or(RestoreError::TooLarge)?;
    let size = (|| {
        let definitions = sources
            .checked_mul(DS_DEF_LEN + PDP_PREP_LEN)?
            .checked_add(archives.checked_mul(RRA_DEF_LEN + RRA_PTR_LEN)?)?;
        let cdp = sources.checked_mul(archives)?.checked_mul(CDP_PREP_LEN)?;
        let values = usize::try_from(rows)
            .ok()?
            .checked_mul(sources)?
            .checked_mul(VALUE_LEN)?;
        STAT_HEAD_LEN
            .checked_add(LIVE_HEAD_LEN)?
            .checked_add(definitions)?
            .checked_add(cdp)?
            .checked_add(values)
    })()
    .ok_or(RestoreError::TooLarge)?;
    let mut out = Vec::new();
    out.try_reserve_exact(size)
        .map_err(|_| RestoreError::TooLarge)?;

    // RS-51: versions below 3 are stored as 0003.
    let version: &[u8] = if version_number(&dump.version) < 3 {
        b"0003"
    } else {
        &dump.version
    };
    out.extend_from_slice(b"RRD\0");
    put_string(&mut out, version, 5);
    out.resize(16, 0);
    out.extend_from_slice(&FLOAT_COOKIE.to_le_bytes());
    out.extend_from_slice(&(sources as u64).to_le_bytes());
    out.extend_from_slice(&(archives as u64).to_le_bytes());
    out.extend_from_slice(&dump.step.to_le_bytes());
    out.resize(STAT_HEAD_LEN, 0);

    for source in &dump.sources {
        put_string(&mut out, &source.name, 20);
        put_string(&mut out, &source.kind, 20);
        let mut slots: Slots = [0; 80];
        set_u64(&mut slots, 0, source.heartbeat);
        set_f64(&mut slots, 1, source.min);
        set_f64(&mut slots, 2, source.max);
        out.extend_from_slice(&slots);
    }
    for archive in &dump.archives {
        put_string(&mut out, &archive.cf, 20);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&archive.rows.to_le_bytes());
        out.extend_from_slice(&archive.pdp_per_row.to_le_bytes());
        out.extend_from_slice(&archive.params);
    }
    out.extend_from_slice(&dump.last_update.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    for source in &dump.sources {
        put_string(&mut out, &source.last_ds, 32);
        let mut slots: Slots = [0; 80];
        set_u64(&mut slots, 0, source.unknown_sec);
        set_f64(&mut slots, 1, source.pdp_value);
        out.extend_from_slice(&slots);
    }
    for archive in &dump.archives {
        for record in &archive.cdp {
            out.extend_from_slice(record);
        }
    }
    // RS-61: any pointer works as long as the rows rotate with it. Rows are
    // stored in dump order, so the newest row sits at the last slot.
    for archive in &dump.archives {
        out.extend_from_slice(&archive.rows.saturating_sub(1).to_le_bytes());
    }
    for archive in &dump.archives {
        for value in &archive.values {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    debug_assert_eq!(out.len(), size);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Writing the target (RS-54 to RS-58).

#[cfg(unix)]
fn write_target(target: &Path, force_overwrite: bool, bytes: &[u8]) -> Result<(), RestoreError> {
    use std::os::unix::fs::FileExt;
    let name = target.display().to_string();
    let file = crate::rrd_binary::open_output_file(target, !force_overwrite).map_err(|error| {
        RestoreError::Create {
            name: name.clone(),
            reason: crate::rrd_binary::rrd_strerror(&error),
        }
    })?;
    let write_error = |error: std::io::Error| RestoreError::Write {
        name: name.clone(),
        reason: crate::rrd_binary::rrd_strerror(&error),
    };
    let metadata = file.metadata().map_err(write_error)?;
    let regular = metadata.is_file();
    let new_len = bytes.len() as u64;
    // Growing first means a file-size limit or a full disk fails before any
    // existing byte changes.
    if regular && metadata.len() < new_len {
        let grown = file.set_len(new_len);
        if let Err(error) = grown {
            let _ = file.set_len(metadata.len());
            if !force_overwrite {
                let _ = std::fs::remove_file(target);
            }
            return Err(write_error(error));
        }
    }
    let written = if regular {
        file.write_all_at(bytes, 0)
    } else {
        (&file).write_all(bytes)
    };
    if let Err(error) = written {
        if !force_overwrite {
            let _ = std::fs::remove_file(target);
        }
        return Err(write_error(error));
    }
    // Compatibility: RRDtool keeps the tail of a longer file; Rondi
    // truncates it (RS-56). Provenance: black-box compatibility test
    // (`opt-force-longer-file`).
    if regular && metadata.len() > new_len {
        file.set_len(new_len).map_err(write_error)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_target(target: &Path, force_overwrite: bool, bytes: &[u8]) -> Result<(), RestoreError> {
    let name = target.display().to_string();
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force_overwrite {
        options.create(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(target).map_err(|error| RestoreError::Create {
        name: name.clone(),
        reason: error.to_string(),
    })?;
    file.write_all(bytes)
        .and_then(|()| file.set_len(bytes.len() as u64))
        .map_err(|error| RestoreError::Write {
            name,
            reason: error.to_string(),
        })
}

// ---------------------------------------------------------------------------
// Entry points.

/// What a restore produced before anything is written.
#[derive(Debug)]
struct Restored {
    /// The file to write; `None` when the walk stopped early.
    bytes: Option<Vec<u8>>,
    /// The message left set; it decides the exit status (RS-6).
    message: Option<RestoreError>,
}

fn restore_bytes(
    input: RestoreInput<'_>,
    range_check: bool,
    diagnostics: &mut dyn Write,
) -> Restored {
    let (xml, delivery, name) = match input {
        RestoreInput::Stdin(xml) => (xml, Delivery::Stdin, None),
        RestoreInput::File { name, xml } => (xml, Delivery::File, Some(name.to_owned())),
    };
    let prepared = prepare(xml, delivery);
    let reader = Reader::new(
        &prepared,
        Diagnostics {
            out: diagnostics,
            name,
        },
    );
    let mut walk = Walk {
        reader,
        message: None,
        errno: 0,
        range_check,
        dump: Dump::default(),
    };
    match walk.root() {
        Ok(()) => match serialize(&walk.dump) {
            Ok(bytes) => Restored {
                bytes: Some(bytes),
                message: walk.message,
            },
            Err(error) => Restored {
                bytes: None,
                message: Some(error),
            },
        },
        // Compatibility: the quiet stop sets no message of its own, so an
        // earlier recoverable one still decides the exit status (RS-6).
        // Provenance: black-box compatibility test (an `<xff>U</xff>` archive
        // followed by one with empty `<params>`).
        Err(Stop::Silent) => Restored {
            bytes: None,
            message: walk.message,
        },
        Err(Stop::Fatal) => Restored {
            bytes: None,
            message: Some(
                walk.message
                    .unwrap_or_else(|| RestoreError::Xml(String::new())),
            ),
        },
    }
}

/// Restores `input` into `target` (`-` for standard output) and writes the
/// XML library's diagnostics for a named file to `diagnostics`.
///
/// Returns `Ok` when RRDtool would exit 0. An `Err` carries the message
/// RRDtool would print; the file may still have been written (RS-6).
pub fn restore_rrd_input(
    input: RestoreInput<'_>,
    target: impl AsRef<Path>,
    force_overwrite: bool,
    range_check: bool,
    diagnostics: &mut dyn Write,
) -> Result<(), RestoreError> {
    let restored = restore_bytes(input, range_check, diagnostics);
    if let Some(bytes) = &restored.bytes {
        let target = target.as_ref();
        if target == Path::new("-") {
            std::io::stdout()
                .lock()
                .write_all(bytes)
                .map_err(|error| RestoreError::Write {
                    name: "-".into(),
                    reason: crate::rrd_binary::rrd_strerror(&error),
                })?;
        } else {
            write_target(target, force_overwrite, bytes)?;
        }
    }
    match restored.message {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Restores an XML dump held in memory into `target`, as `rrdtool restore`
/// does for a named file, without printing XML diagnostics.
pub fn restore_rrd_file(
    xml: impl AsRef<[u8]>,
    target: impl AsRef<Path>,
    force_overwrite: bool,
    range_check: bool,
) -> Result<(), RestoreError> {
    restore_rrd_input(
        RestoreInput::File {
            name: "-",
            xml: xml.as_ref(),
        },
        target,
        force_overwrite,
        range_check,
        &mut std::io::sink(),
    )
}

#[cfg(test)]
mod tests;
