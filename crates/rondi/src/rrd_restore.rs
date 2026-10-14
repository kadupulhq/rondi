//! Port of RRDtool 1.11.0 `rrd_restore.c`: the libxml2 pull-parser walk in
//! `parse_file` and its `parse_tag_*` helpers, and `write_file`.
//!
//! `XmlReader` stands in for libxml2's `xmlTextReader`. It yields the same
//! element, end-element and text nodes for well-formed input and reports
//! `xmlTextReaderGetParserLineNumber` the way the reader advances: it pushes
//! the document to the parser in a 4-byte chunk and then 512-byte chunks, so
//! the line is the one reached at the end of the chunk that completed the
//! current node. Malformed XML fails with Rondi's own description where
//! libxml2 prints its parser diagnostics.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN,
    STAT_HEAD_LEN, rrd_nan,
};
use crate::rrd_number::{c_strtoll_errno, c_strtoul_errno, parse_rrd_decimal};
use crate::storage::StoreError;
use std::io::Write;

const MAX_PAR: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeType {
    Element,
    EndElement,
    Text,
    Other,
}

struct XmlReader<'a> {
    data: &'a [u8],
    position: usize,
    /// End of the furthest token the parser has had to see.
    parsed: usize,
    node: NodeType,
    name: String,
    value: String,
    open: Vec<String>,
    root_closed: bool,
}

impl<'a> XmlReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            position: 0,
            parsed: 0,
            node: NodeType::Other,
            name: String::new(),
            value: String::new(),
            open: Vec::new(),
            root_closed: false,
        }
    }

    /// `xmlTextReaderGetParserLineNumber`.
    fn line(&self) -> usize {
        let chunk_end = if self.parsed <= 4 {
            4
        } else {
            4 + (self.parsed - 4).div_ceil(512) * 512
        };
        let end = chunk_end.min(self.data.len());
        1 + self.data[..end]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
    }

    fn find(&self, needle: &[u8]) -> Option<usize> {
        self.data[self.position..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|offset| self.position + offset)
    }

    fn consume_to(&mut self, end: usize) {
        self.position = end;
        self.parsed = self.parsed.max(end);
    }

    /// `xmlTextReaderRead`: 1 with a node, 0 at the end, Err on bad XML.
    fn read(&mut self) -> Result<bool, String> {
        loop {
            if self.position >= self.data.len() {
                return self.read_end();
            }
            let rest = &self.data[self.position..];
            if rest.starts_with(b"<?") {
                return self.skip_to(b"?>", "unterminated processing instruction");
            }
            if rest.starts_with(b"<!--") {
                return self.skip_to(b"-->", "Comment not terminated");
            }
            if rest.starts_with(b"<![CDATA[") {
                return self.skip_to(b"]]>", "CData section not finished");
            }
            if rest.starts_with(b"<!") {
                return self.read_doctype();
            }
            if rest.starts_with(b"</") {
                return self.read_end_tag();
            }
            if rest.starts_with(b"<") {
                return self.read_start_tag();
            }
            if let Some(read) = self.read_text()? {
                return Ok(read);
            }
        }
    }

    fn read_end(&mut self) -> Result<bool, String> {
        self.parsed = self.data.len();
        if let Some(open) = self.open.last() {
            return Err(format!("Premature end of data in tag {open}"));
        }
        if !self.root_closed {
            return Err("Start tag expected, '<' not found".into());
        }
        Ok(false)
    }

    /// Comments, processing instructions and CDATA are nodes neither
    /// get_xml_element nor get_xml_text looks at.
    fn skip_to(&mut self, terminator: &[u8], unterminated: &str) -> Result<bool, String> {
        let end = self.find(terminator).ok_or(unterminated)?;
        self.consume_to(end + terminator.len());
        self.node = NodeType::Other;
        Ok(true)
    }

    /// DOCTYPE, with an optional internal subset.
    fn read_doctype(&mut self) -> Result<bool, String> {
        let mut depth = 0;
        let end = self.data[self.position + 2..]
            .iter()
            .position(|byte| {
                match byte {
                    b'[' => depth += 1,
                    b']' => depth -= 1,
                    b'>' if depth <= 0 => return true,
                    _ => {}
                }
                false
            })
            .ok_or("DOCTYPE improperly terminated")?;
        self.consume_to(self.position + 2 + end + 1);
        self.node = NodeType::Other;
        Ok(true)
    }

    fn read_end_tag(&mut self) -> Result<bool, String> {
        let end = self.find(b">").ok_or("expected '>'")?;
        let name = String::from_utf8_lossy(&self.data[self.position + 2..end])
            .trim_end()
            .to_owned();
        self.consume_to(end + 1);
        match self.open.pop() {
            Some(open) if open == name => {}
            Some(open) => {
                return Err(format!(
                    "Opening and ending tag mismatch: {open} and {name}"
                ));
            }
            None => return Err("Extra content at the end of the document".into()),
        }
        self.root_closed = self.open.is_empty();
        self.node = NodeType::EndElement;
        self.name = name;
        Ok(true)
    }

    fn read_start_tag(&mut self) -> Result<bool, String> {
        if self.root_closed {
            return Err("Extra content at the end of the document".into());
        }
        let name_start = self.position + 1;
        let name_len = self.data[name_start..]
            .iter()
            .take_while(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>'))
            .count();
        let name =
            String::from_utf8_lossy(&self.data[name_start..name_start + name_len]).into_owned();
        if name.is_empty() {
            return Err("StartTag: invalid element name".into());
        }
        let mut quote = None;
        let end = self.data[name_start + name_len..]
            .iter()
            .position(|byte| match quote {
                Some(open) => {
                    if *byte == open {
                        quote = None;
                    }
                    false
                }
                None if matches!(byte, b'"' | b'\'') => {
                    quote = Some(*byte);
                    false
                }
                None => *byte == b'>',
            })
            .map(|offset| name_start + name_len + offset)
            .ok_or_else(|| format!("Couldn't find end of Start Tag {name}"))?;
        let empty = self.data[end - 1] == b'/';
        self.consume_to(end + 1);
        if empty {
            self.root_closed = self.open.is_empty();
        } else {
            self.open.push(name.clone());
        }
        self.node = NodeType::Element;
        self.name = name;
        Ok(true)
    }

    /// Character data up to the next '<'. Blank text inside an element is a
    /// whitespace node; blank text outside the root is skipped (None).
    fn read_text(&mut self) -> Result<Option<bool>, String> {
        let end = self.find(b"<").unwrap_or(self.data.len());
        let raw = String::from_utf8_lossy(&self.data[self.position..end]).into_owned();
        // Character data is complete once the parser has seen the '<'.
        self.position = end;
        self.parsed = self.parsed.max((end + 1).min(self.data.len()));
        let blank = raw
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        if self.open.is_empty() {
            if blank {
                return Ok(None);
            }
            return Err(if self.root_closed {
                "Extra content at the end of the document".into()
            } else {
                "Start tag expected, '<' not found".into()
            });
        }
        if blank {
            self.node = NodeType::Other;
        } else {
            self.value = decode_entities(&raw)?;
            self.node = NodeType::Text;
        }
        Ok(Some(true))
    }
}

fn decode_entities(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find(';')
            .map(|offset| start + offset)
            .ok_or("EntityRef: expecting ';'")?;
        let entity = &rest[start + 1..end];
        let decoded = match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(decimal) = entity.strip_prefix('#') {
                    decimal.parse().ok()
                } else {
                    return Err(format!("Entity '{entity}' not defined"));
                };
                code.and_then(char::from_u32)
                    .ok_or_else(|| format!("xmlParseCharRef: invalid xmlChar value {entity}"))?
            }
        };
        out.push(decoded);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The rrd_t that parse_file fills, as the byte blocks write_fh writes.
#[derive(Default)]
struct Rrd {
    version: [u8; 5],
    pdp_step: u64,
    last_up: i64,
    ds_defs: Vec<[u8; DS_DEF_LEN]>,
    pdp_preps: Vec<[u8; PDP_PREP_LEN]>,
    rra_defs: Vec<[u8; RRA_DEF_LEN]>,
    cdp_preps: Vec<[u8; CDP_PREP_LEN]>,
    rra_ptrs: Vec<u64>,
    values: Vec<f64>,
}

struct Restore<'a> {
    reader: XmlReader<'a>,
    /// The rrd_set_error slot.
    error: Option<String>,
    /// errno as the conversions leave it.
    errno: i32,
    range_check: bool,
}

type Status = Result<(), ()>;

fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn version_number(version: &[u8; 5]) -> i64 {
    let end = version.iter().position(|byte| *byte == 0).unwrap_or(5);
    crate::rrd_number::c_strtol(&String::from_utf8_lossy(&version[..end]), 10).0 as i32 as i64
}

impl Restore<'_> {
    fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    /// `get_xml_element`.
    fn get_xml_element(&mut self) -> Option<String> {
        loop {
            match self.reader.read() {
                Ok(true) => {}
                Ok(false) => {
                    self.set_error("the xml ended while we were looking for an element".into());
                    return None;
                }
                Err(message) => {
                    self.set_error(format!("error reading/parsing XML: {message}"));
                    return None;
                }
            }
            match self.reader.node {
                NodeType::Text => {
                    let message = format!(
                        "line {}: expected element but found text '{}'",
                        self.reader.line(),
                        self.reader.value
                    );
                    self.set_error(message);
                    return None;
                }
                NodeType::Other => continue,
                NodeType::EndElement => return Some(format!("/{}", self.reader.name)),
                NodeType::Element => return Some(self.reader.name.clone()),
            }
        }
    }

    /// `expect_element`; its message names the two tags the other way round.
    fn expect_element(&mut self, expected: &str) -> Status {
        let name = self.get_xml_element().ok_or(())?;
        if !name.eq_ignore_ascii_case(expected) {
            let message = format!(
                "line {}: expected <{name}> element but found <{expected}>",
                self.reader.line()
            );
            self.set_error(message);
            return Err(());
        }
        Ok(())
    }

    /// `expect_element_end`.
    fn expect_element_end(&mut self, expected: &str) -> Status {
        let name = if self.reader.node == NodeType::EndElement {
            format!("/{}", self.reader.name)
        } else {
            self.get_xml_element().ok_or(())?
        };
        if !name.starts_with('/') || !name[1..].eq_ignore_ascii_case(expected) {
            let message = format!(
                "line {}: expected </{expected}> end element but found <{name}>",
                self.reader.line()
            );
            self.set_error(message);
            return Err(());
        }
        Ok(())
    }

    /// `get_xml_text`: the first white-space-delimited word of the next text
    /// node, or "" when an end element comes first.
    fn get_xml_text(&mut self) -> Option<String> {
        loop {
            match self.reader.read() {
                Ok(true) => {}
                Ok(false) => break,
                Err(message) => {
                    self.set_error(format!("error reading/parsing XML: {message}"));
                    return None;
                }
            }
            match self.reader.node {
                NodeType::Element => {
                    let message = format!(
                        "line {}: expected a value but found a <{}> element",
                        self.reader.line(),
                        self.reader.name
                    );
                    self.set_error(message);
                    return None;
                }
                NodeType::EndElement => return Some(String::new()),
                NodeType::Other => continue,
                NodeType::Text => {
                    let is_space =
                        |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r');
                    let text = self.reader.value.trim_start_matches(is_space);
                    let word = text.split(is_space).next().unwrap_or_default();
                    return Some(word.to_owned());
                }
            }
        }
        self.set_error("file ended while looking for text".into());
        None
    }

    fn get_xml_string(&mut self, field: &mut [u8]) -> Status {
        let text = self.get_xml_text().ok_or(())?;
        if text.len() >= field.len() {
            self.set_error(format!("'{text}' is longer than {}", field.len()));
            return Err(());
        }
        field.fill(0);
        field[..text.len()].copy_from_slice(text.as_bytes());
        Ok(())
    }

    fn get_xml_time_t(&mut self) -> Result<i64, ()> {
        let text = self.get_xml_text().ok_or(())?;
        let (value, erange) = c_strtoll_errno(&text, 0);
        self.errno = if erange { libc::ERANGE } else { 0 };
        if erange {
            let message = format!(
                "ling {}: get_xml_time_t from '{text}' {}",
                self.reader.line(),
                strerror(self.errno)
            );
            self.set_error(message);
            return Err(());
        }
        Ok(value)
    }

    fn get_xml_ulong(&mut self) -> Result<u64, ()> {
        let text = self.get_xml_text().ok_or(())?;
        let (value, erange) = c_strtoul_errno(&text, 0);
        self.errno = if erange { libc::ERANGE } else { 0 };
        if erange {
            let message = format!(
                "ling {}: get_xml_ulong from '{text}' {}",
                self.reader.line(),
                strerror(self.errno)
            );
            self.set_error(message);
            return Err(());
        }
        Ok(value)
    }

    /// `get_xml_double`: any text containing "nan" or "inf" is special;
    /// everything else must convert completely with rrd_strtod.
    fn get_xml_double(&mut self) -> Result<f64, ()> {
        let text = self.get_xml_text().ok_or(())?;
        let lower = text.to_ascii_lowercase();
        if lower.contains("nan") {
            return Ok(rrd_nan());
        }
        if lower.contains("-inf") {
            return Ok(f64::NEG_INFINITY);
        }
        if lower.contains("inf") {
            return Ok(f64::INFINITY);
        }
        match parse_rrd_decimal(&text) {
            Some((value, end)) => {
                if value == f64::INFINITY {
                    self.errno = libc::ERANGE;
                }
                if end == text.len() {
                    return Ok(value);
                }
            }
            // rrd_strtod sets ERANGE without digits or with an exponent
            // out of range.
            None => self.errno = libc::ERANGE,
        }
        let message = format!(
            "ling {}: get_xml_double from '{text}' {}",
            self.reader.line(),
            strerror(self.errno)
        );
        self.set_error(message);
        Err(())
    }

    fn parse_tag_rra_database_row(&mut self, rrd: &mut Rrd, row_start: usize) -> Status {
        for index in 0..rrd.ds_defs.len() {
            self.expect_element("v")?;
            let mut value = self.get_xml_double()?;
            if self.range_check {
                let min = f64::from_le_bytes(rrd.ds_defs[index][48..56].try_into().unwrap());
                let max = f64::from_le_bytes(rrd.ds_defs[index][56..64].try_into().unwrap());
                if (!min.is_nan() && value < min) || (!max.is_nan() && value > max) {
                    value = rrd_nan();
                }
            }
            rrd.values[row_start + index] = value;
            self.expect_element("/v")?;
        }
        Ok(())
    }

    fn parse_tag_rra_database(&mut self, rrd: &mut Rrd) -> Status {
        let ds_cnt = rrd.ds_defs.len();
        let rra = rrd.rra_defs.len() - 1;
        let mut status = Ok(());
        let mut row_cnt = 0_u64;
        while let Some(element) = self.get_xml_element() {
            if element.eq_ignore_ascii_case("row") {
                let row_start = rrd.values.len();
                rrd.values.resize(row_start + ds_cnt, 0.0);
                row_cnt += 1;
                put(&mut rrd.rra_defs[rra], 24, row_cnt);
                status = self.parse_tag_rra_database_row(rrd, row_start);
                if status.is_ok() {
                    status = self.expect_element("/row");
                }
            } else if element.eq_ignore_ascii_case("/database") {
                break;
            } else {
                let message = format!(
                    "line {}: found unexpected tag: {element}",
                    self.reader.line()
                );
                self.set_error(message);
                status = Err(());
            }
            if status.is_err() {
                break;
            }
        }
        // RRDtool picks a random cur_row and rotates the rows to match;
        // the last row keeps them in order. With no rows it divides by
        // zero (rrd_restore.c:450).
        if row_cnt == 0 {
            self.set_error("restore of an RRA without rows is not supported".into());
            return Err(());
        }
        rrd.rra_ptrs[rra] = row_cnt - 1;
        status
    }

    fn parse_tag_rra_cdp_prep_ds(&mut self, rrd: &mut Rrd, prep: usize) -> Status {
        let mut status = Err(());
        while let Some(element) = self.get_xml_element() {
            let lower = element.to_ascii_lowercase();
            let double_slot = match lower.as_str() {
                "primary_value" => Some(8),
                "secondary_value" => Some(9),
                "intercept" | "seasonal" => Some(2),
                "last_intercept" | "last_seasonal" => Some(3),
                "slope" => Some(4),
                "last_slope" => Some(5),
                "value" => Some(0),
                _ => None,
            };
            let count_slot = match lower.as_str() {
                "nan_count" | "init_flag" => Some(6),
                "last_nan_count" => Some(7),
                "unknown_datapoints" => Some(1),
                _ => None,
            };
            status = if let Some(slot) = double_slot {
                self.get_xml_double()
                    .map(|value| put(&mut rrd.cdp_preps[prep], slot * 8, value.to_bits()))
            } else if let Some(slot) = count_slot {
                self.get_xml_ulong()
                    .map(|value| put(&mut rrd.cdp_preps[prep], slot * 8, value))
            } else if lower == "history" {
                self.get_xml_text().ok_or(()).map(|history| {
                    // Only the first MAX_CDP_PAR_EN bytes, as upstream.
                    for (index, byte) in history.bytes().take(MAX_PAR).enumerate() {
                        rrd.cdp_preps[prep][index] = u8::from(byte == b'1');
                    }
                })
            } else if lower == "/ds" {
                break;
            } else {
                self.set_error(format!("parse_tag_rra_cdp_prep: Unknown tag: {element}"));
                status = Err(());
                break;
            };
            if status.is_err() {
                break;
            }
            status = self.expect_element_end(&element);
            if status.is_err() {
                break;
            }
        }
        status
    }

    fn parse_tag_rra_cdp_prep(&mut self, rrd: &mut Rrd) -> Status {
        let ds_cnt = rrd.ds_defs.len();
        let first = rrd.cdp_preps.len() - ds_cnt;
        for index in 0..ds_cnt {
            self.expect_element("ds")?;
            self.parse_tag_rra_cdp_prep_ds(rrd, first + index)?;
        }
        self.expect_element("/cdp_prep")
    }

    fn parse_tag_rra_params(&mut self, rra_def: &mut [u8; RRA_DEF_LEN]) -> Status {
        let mut status = Err(());
        while let Some(element) = self.get_xml_element() {
            let lower = element.to_ascii_lowercase();
            // Upstream assigns each result to `status` and then replaces it
            // with the end-element check, so a bad value only leaves its
            // error text behind.
            if let Some((slot, is_count)) = rra_param_slot(&lower) {
                let _ = self.read_rra_param(rra_def, slot, is_count);
            } else if lower == "value" {
                self.read_rra_param_values(rra_def);
            } else if lower == "/params" {
                return status;
            } else {
                let message = format!(
                    "line {}: parse_tag_rra_params: Unknown tag: {element}",
                    self.reader.line()
                );
                self.set_error(message);
            }
            status = self.expect_element_end(&element);
            if status.is_err() {
                break;
            }
        }
        status
    }

    /// One `<params>` value into `par[slot]`, as a double or an unsigned long.
    fn read_rra_param(
        &mut self,
        rra_def: &mut [u8; RRA_DEF_LEN],
        slot: usize,
        is_count: bool,
    ) -> Status {
        let bits = if is_count {
            self.get_xml_ulong()?
        } else {
            self.get_xml_double()?.to_bits()
        };
        put(rra_def, 40 + slot * 8, bits);
        Ok(())
    }

    /// Compatibility code for 1.0.49 (rrd_restore.c:755-779): every par[]
    /// in `<value>` elements. `i-1 < ARRAY_LENGTH` is false for i == 0.
    fn read_rra_param_values(&mut self, rra_def: &mut [u8; RRA_DEF_LEN]) {
        for index in 0..MAX_PAR {
            if self
                .read_rra_param(rra_def, index, matches!(index, 3..=5))
                .is_err()
            {
                break;
            }
            if (index as u32).wrapping_sub(1) < MAX_PAR as u32
                && (self.expect_element("/value").is_err() || self.expect_element("value").is_err())
            {
                break;
            }
        }
    }

    fn parse_tag_rra(&mut self, rrd: &mut Rrd) -> Status {
        let ds_cnt = rrd.ds_defs.len();
        rrd.rra_defs.push([0; RRA_DEF_LEN]);
        rrd.cdp_preps
            .extend(std::iter::repeat_n([0; CDP_PREP_LEN], ds_cnt));
        rrd.rra_ptrs.push(0);
        let rra = rrd.rra_defs.len() - 1;
        let version = version_number(&rrd.version);
        let mut status = Ok(());
        while let Some(element) = self.get_xml_element() {
            let lower = element.to_ascii_lowercase();
            if lower == "cf" {
                let mut cf = [0_u8; 20];
                status = self.get_xml_string(&mut cf);
                rrd.rra_defs[rra][..20].copy_from_slice(&cf);
                if status.is_ok() {
                    let end = cf.iter().position(|byte| *byte == 0).unwrap_or(20);
                    let name = String::from_utf8_lossy(&cf[..end]).into_owned();
                    if !matches!(
                        name.as_str(),
                        "AVERAGE"
                            | "MIN"
                            | "MAX"
                            | "LAST"
                            | "HWPREDICT"
                            | "MHWPREDICT"
                            | "DEVPREDICT"
                            | "SEASONAL"
                            | "DEVSEASONAL"
                            | "FAILURES"
                    ) {
                        self.set_error(format!(
                            "parse_tag_rra_cf: Unknown consolidation function: {name}"
                        ));
                        status = Err(());
                    }
                }
            } else if lower == "pdp_per_row" {
                status = self
                    .get_xml_ulong()
                    .map(|value| put(&mut rrd.rra_defs[rra], 32, value));
            } else if version == 1 && lower == "xff" {
                status = self
                    .get_xml_double()
                    .map(|value| put(&mut rrd.rra_defs[rra], 40, value.to_bits()));
            } else if version >= 2 && lower == "params" {
                let mut rra_def = rrd.rra_defs[rra];
                let result = self.parse_tag_rra_params(&mut rra_def);
                rrd.rra_defs[rra] = rra_def;
                result?;
                continue;
            } else if lower == "cdp_prep" {
                self.parse_tag_rra_cdp_prep(rrd)?;
                continue;
            } else if lower == "database" {
                self.parse_tag_rra_database(rrd)?;
                continue;
            } else if lower == "/rra" {
                return status;
            } else {
                let message = format!(
                    "line {}: parse_tag_rra: Unknown tag: {element}",
                    self.reader.line()
                );
                self.set_error(message);
                status = Err(());
            }
            status?;
            self.expect_element_end(&element)?;
        }
        status
    }

    fn parse_tag_ds(&mut self, rrd: &mut Rrd) -> Status {
        if !rrd.rra_defs.is_empty() {
            self.set_error(
                "parse_tag_ds: All data source definitions MUST precede the RRA definitions!"
                    .into(),
            );
            return Err(());
        }
        rrd.ds_defs.push([0; DS_DEF_LEN]);
        rrd.pdp_preps.push([0; PDP_PREP_LEN]);
        let ds = rrd.ds_defs.len() - 1;
        let mut status = Ok(());
        while let Some(element) = self.get_xml_element() {
            let lower = element.to_ascii_lowercase();
            status = match lower.as_str() {
                "name" => {
                    let mut name = [0_u8; 20];
                    let result = self.get_xml_string(&mut name);
                    rrd.ds_defs[ds][..20].copy_from_slice(&name);
                    result
                }
                "type" => match self.get_xml_text() {
                    Some(dst) => {
                        if matches!(
                            dst.as_str(),
                            "COUNTER" | "ABSOLUTE" | "GAUGE" | "DERIVE" | "DCOUNTER" | "DDERIVE"
                        ) {
                            rrd.ds_defs[ds][20..40].fill(0);
                            rrd.ds_defs[ds][20..20 + dst.len()].copy_from_slice(dst.as_bytes());
                            Ok(())
                        } else if dst == "COMPUTE" {
                            self.set_error(
                                "COMPUTE data sources are not supported by Rondi".into(),
                            );
                            Err(())
                        } else {
                            self.set_error(format!(
                                "parse_tag_ds_type: Unknown data source type: {dst}"
                            ));
                            Err(())
                        }
                    }
                    None => Err(()),
                },
                "minimal_heartbeat" => self
                    .get_xml_ulong()
                    .map(|value| put(&mut rrd.ds_defs[ds], 40, value)),
                "min" => self
                    .get_xml_double()
                    .map(|value| put(&mut rrd.ds_defs[ds], 48, value.to_bits())),
                "max" => self
                    .get_xml_double()
                    .map(|value| put(&mut rrd.ds_defs[ds], 56, value.to_bits())),
                "cdef" => {
                    self.set_error("COMPUTE data sources are not supported by Rondi".into());
                    Err(())
                }
                "last_ds" => {
                    let mut last_ds = [0_u8; 30];
                    let result = self.get_xml_string(&mut last_ds);
                    rrd.pdp_preps[ds][..30].copy_from_slice(&last_ds);
                    result
                }
                "value" => self
                    .get_xml_double()
                    .map(|value| put(&mut rrd.pdp_preps[ds], 40, value.to_bits())),
                "unknown_sec" => self
                    .get_xml_ulong()
                    .map(|value| put(&mut rrd.pdp_preps[ds], 32, value)),
                "/ds" => break,
                _ => {
                    self.set_error(format!("parse_tag_ds: Unknown tag: {element}"));
                    Err(())
                }
            };
            if status.is_err() {
                break;
            }
            status = self.expect_element_end(&element);
            if status.is_err() {
                break;
            }
        }
        status
    }

    fn parse_tag_rrd(&mut self, rrd: &mut Rrd) -> Status {
        let mut status = Ok(());
        while let Some(element) = self.get_xml_element() {
            let lower = element.to_ascii_lowercase();
            status = match lower.as_str() {
                "version" => {
                    let mut version = [0_u8; 5];
                    let result = self.get_xml_string(&mut version);
                    rrd.version = version;
                    result
                }
                "step" => self.get_xml_ulong().map(|value| rrd.pdp_step = value),
                "lastupdate" => self.get_xml_time_t().map(|value| rrd.last_up = value),
                "ds" => {
                    self.parse_tag_ds(rrd)?;
                    continue;
                }
                "rra" => {
                    self.parse_tag_rra(rrd)?;
                    continue;
                }
                "/rrd" => return status,
                _ => {
                    self.set_error(format!("parse_tag_rrd: Unknown tag: {element}"));
                    Err(())
                }
            };
            if status.is_err() {
                break;
            }
            status = self.expect_element_end(&element);
            if status.is_err() {
                break;
            }
        }
        status
    }
}

/// The par[] slot a `<params>` child names, and whether it is a count.
fn rra_param_slot(tag: &str) -> Option<(usize, bool)> {
    match tag {
        "xff" => Some((0, false)),
        "hw_alpha" | "seasonal_gamma" | "delta_pos" => Some((1, false)),
        "hw_beta" | "smoothing_window" | "delta_neg" => Some((2, false)),
        "dependent_rra_idx" => Some((3, true)),
        "seasonal_smooth_idx" | "window_len" => Some((4, true)),
        "failure_threshold" => Some((5, true)),
        _ => None,
    }
}

fn strerror(errno: i32) -> String {
    crate::rrd_binary::rrd_strerror(&std::io::Error::from_raw_os_error(errno))
}

/// `write_fh` (rrd_create.c:1539): versions below 3 are written as 0003.
fn layout(rrd: &Rrd) -> Vec<u8> {
    let mut bytes = vec![0_u8; STAT_HEAD_LEN];
    bytes[0..3].copy_from_slice(b"RRD");
    if version_number(&rrd.version) < 3 {
        bytes[4..8].copy_from_slice(b"0003");
    } else {
        bytes[4..9].copy_from_slice(&rrd.version);
    }
    put(&mut bytes, 16, FLOAT_COOKIE.to_bits());
    put(&mut bytes, 24, rrd.ds_defs.len() as u64);
    put(&mut bytes, 32, rrd.rra_defs.len() as u64);
    put(&mut bytes, 40, rrd.pdp_step);
    for def in &rrd.ds_defs {
        bytes.extend_from_slice(def);
    }
    for def in &rrd.rra_defs {
        bytes.extend_from_slice(def);
    }
    let mut live = [0_u8; LIVE_HEAD_LEN];
    live[..8].copy_from_slice(&rrd.last_up.to_le_bytes());
    bytes.extend_from_slice(&live);
    for prep in &rrd.pdp_preps {
        bytes.extend_from_slice(prep);
    }
    for prep in &rrd.cdp_preps {
        bytes.extend_from_slice(prep);
    }
    for pointer in &rrd.rra_ptrs {
        bytes.extend_from_slice(&pointer.to_le_bytes());
    }
    for value in &rrd.values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// `rrd_restore` after option parsing: parse `xml` like `parse_file` and
/// write the result like `write_file` (rrd_restore.c:1340-1384). The target
/// is opened O_WRONLY|O_CREAT (|O_EXCL without `force_overwrite`) with mode
/// 0666 and written in place; "-" is stdout. Unlike RRDtool, a longer
/// existing file is truncated to the new length. An error that the parse
/// recorded without failing is returned after the file is written, as
/// rrd_tool.c reports it, and a parse that fails without one writes nothing
/// and returns `Ok`.
pub fn restore_rrd_file(
    xml: &str,
    path: impl AsRef<std::path::Path>,
    force_overwrite: bool,
    range_check: bool,
) -> Result<(), StoreError> {
    if !cfg!(target_pointer_width = "64") || cfg!(target_endian = "big") {
        return Err(StoreError::RrdUnsupported(
            "RRD restore currently requires a 64-bit little-endian target".into(),
        ));
    }
    let mut restore = Restore {
        reader: XmlReader::new(xml.as_bytes()),
        error: None,
        errno: 0,
        range_check,
    };
    let mut rrd = Rrd::default();
    let parsed = restore
        .expect_element("rrd")
        .and_then(|()| restore.parse_tag_rrd(&mut rrd));
    let error = restore.error.take();
    if parsed.is_err() {
        // A parse that fails without setting an error (an empty <params>
        // block) writes nothing and rrd_tool.c reports nothing.
        return error.map_or(Ok(()), |message| Err(StoreError::Rrd(message)));
    }
    let bytes = layout(&rrd);
    let path = path.as_ref();
    if path.as_os_str() == "-" {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.flush()?;
    } else {
        let mut file =
            crate::rrd_binary::open_output_file(path, !force_overwrite).map_err(|error| {
                StoreError::Rrd(format!(
                    "creating '{}': {}",
                    path.display(),
                    crate::rrd_binary::rrd_strerror(&error)
                ))
            })?;
        let written = file
            .write_all(&bytes)
            .and_then(|()| file.set_len(bytes.len() as u64));
        if let Err(error) = written {
            let _ = std::fs::remove_file(path);
            return Err(StoreError::Rrd(format!(
                "a file error occurred while creating '{}': {}",
                path.display(),
                crate::rrd_binary::rrd_strerror(&error)
            )));
        }
    }
    match error {
        Some(message) => Err(StoreError::Rrd(message)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::XmlReader;

    #[test]
    fn line_follows_the_parser_chunks() {
        let mut document = b"<rrd>".to_vec();
        document.extend(std::iter::repeat_n(b'\n', 600));
        document.extend_from_slice(b"<x/></rrd>");
        let mut reader = XmlReader::new(&document);
        assert!(reader.read().unwrap());
        // "<rrd>" ends inside the second chunk, which covers bytes 4..516.
        assert_eq!(reader.line(), 512);
        assert!(reader.read().unwrap());
        assert!(reader.read().unwrap());
        assert_eq!(reader.line(), 601);
    }
}
