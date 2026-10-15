//! Port of RRDtool 1.11.0 `rrd_restore.c`: the libxml2 pull-parser walk in
//! `parse_file` and its `parse_tag_*` helpers, and `write_file`. The
//! `xmlTextReader` it pulls nodes from is modelled in `xml_reader`.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN,
    STAT_HEAD_LEN, rrd_nan,
};
use crate::rrd_number::{c_strtoll_errno, c_strtoul_errno, parse_rrd_decimal};
use crate::storage::StoreError;
use crate::xml_reader::{NodeType, Source, XmlReader};
use std::io::Write;

const MAX_PAR: usize = 10;

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
    rows: usize,
}

struct Restore<'a> {
    reader: XmlReader<'a>,
    /// The rrd_set_error slot.
    error: Option<String>,
    /// errno as the conversions leave it.
    errno: i32,
    range_check: bool,
}

// Bound XML-driven allocations before growing any vectors.
const MAX_RESTORE_DATA_SOURCES: usize = 4096;
const MAX_RESTORE_ROWS: usize = 1_000_000;
const MAX_RESTORE_VALUES: usize = 1_000_000;
const MAX_RESTORE_ARCHIVES: usize = 4096;
/// Consecutive reader failures get_xml_text tolerates before giving up.
const MAX_READ_FAILURES: usize = 64;

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
        let rc = loop {
            let rc = self.reader.read();
            if rc != 1 {
                break rc;
            }
            match self.reader.node_type() {
                NodeType::Text => {
                    let message = format!(
                        "line {}: expected element but found text '{}'",
                        self.reader.line(),
                        self.reader.value()
                    );
                    self.set_error(message);
                    return None;
                }
                NodeType::EndElement => return Some(format!("/{}", self.reader.name())),
                NodeType::Element => return Some(self.reader.name().to_owned()),
                NodeType::None | NodeType::Other => {}
            }
        };
        if rc == 0 {
            self.set_error("the xml ended while we were looking for an element".into());
        } else {
            let message = format!(
                "error reading/parsing XML: {}",
                self.reader.last_error_text()
            );
            self.set_error(message);
        }
        None
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
        let name = if self.reader.node_type() == NodeType::EndElement {
            format!("/{}", self.reader.name())
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
    /// node, or "" when an end element comes first. Like upstream it keeps
    /// reading after the reader fails (`while (xmlTextReaderRead(reader))`)
    /// and looks at the node the reader stopped on.
    fn get_xml_text(&mut self) -> Option<String> {
        let mut failures = 0;
        loop {
            match self.reader.read() {
                0 => break,
                1 => failures = 0,
                _ => {
                    // Upstream spins here for good when the failed node is
                    // neither text nor an element; stop instead.
                    failures += 1;
                    if failures > MAX_READ_FAILURES {
                        let message = format!(
                            "error reading/parsing XML: {}",
                            self.reader.last_error_text()
                        );
                        self.set_error(message);
                        return None;
                    }
                }
            }
            match self.reader.node_type() {
                NodeType::Element => {
                    let message = format!(
                        "line {}: expected a value but found a <{}> element",
                        self.reader.line(),
                        self.reader.name()
                    );
                    self.set_error(message);
                    return None;
                }
                NodeType::EndElement => return Some(String::new()),
                NodeType::None | NodeType::Other => continue,
                NodeType::Text => {
                    let is_space =
                        |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r');
                    let value = self.reader.value();
                    let text = value.trim_start_matches(is_space);
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
        let (value, errno) = c_strtoll_errno(&text, 0);
        self.errno = errno;
        if errno > 0 {
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
        let (value, errno) = c_strtoul_errno(&text, 0);
        self.errno = errno;
        if errno > 0 {
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
                if rrd.rows >= MAX_RESTORE_ROWS
                    || ds_cnt > MAX_RESTORE_VALUES.saturating_sub(row_start)
                {
                    self.set_error("restore row or value safety limit exceeded".into());
                    return Err(());
                }
                if rrd.values.try_reserve(ds_cnt).is_err() {
                    self.set_error("cannot allocate restore values".into());
                    return Err(());
                }
                rrd.values.resize(row_start + ds_cnt, 0.0);
                rrd.rows += 1;
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
        // zero (rrd_restore.c:473): x86-64 traps with SIGFPE, while
        // aarch64 divides by zero to 0, so cur_row is the random number
        // itself and the rotation buffer size wraps; malloc fails
        // (rrd_restore.c:505-510).
        if row_cnt == 0 {
            let message = if cfg!(target_arch = "aarch64") && ds_cnt > 0 {
                "parse_tag_rra: malloc failed."
            } else {
                "restore of an RRA without rows is not supported"
            };
            self.set_error(message.into());
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
        if rrd.rra_defs.len() >= MAX_RESTORE_ARCHIVES
            || ds_cnt > MAX_RESTORE_VALUES.saturating_sub(rrd.cdp_preps.len())
        {
            self.set_error("restore archive safety limit exceeded".into());
            return Err(());
        }
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
        if rrd.ds_defs.len() >= MAX_RESTORE_DATA_SOURCES {
            self.set_error("restore data source safety limit exceeded".into());
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
/// 0600 and written in place; "-" is stdout. Unlike RRDtool, a longer
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
    restore_rrd_input(
        RestoreInput::Stdin(xml.as_bytes()),
        path,
        force_overwrite,
        range_check,
        &mut std::io::sink(),
    )
}

/// Where `rrdtool restore` reads its XML from; libxml2 reads the two
/// differently (rrd_restore.c:1242-1263).
#[derive(Clone, Copy)]
pub enum RestoreInput<'a> {
    /// A named file, with the name libxml2 puts in its diagnostics.
    File { name: &'a str, xml: &'a [u8] },
    /// Standard input, already cut at the first ctrl-Z.
    Stdin(&'a [u8]),
}

/// `rrd_restore` after option parsing, as [`restore_rrd_file`], for XML
/// read from a file or from stdin. libxml2's parser diagnostics for a named
/// file are written to `diagnostics` before the result is returned;
/// upstream silences them for stdin.
pub fn restore_rrd_input(
    input: RestoreInput<'_>,
    path: impl AsRef<std::path::Path>,
    force_overwrite: bool,
    range_check: bool,
    diagnostics: &mut dyn Write,
) -> Result<(), StoreError> {
    if !cfg!(target_pointer_width = "64") || cfg!(target_endian = "big") {
        return Err(StoreError::RrdUnsupported(
            "RRD restore currently requires a 64-bit little-endian target".into(),
        ));
    }
    let reader = match input {
        RestoreInput::File { name, xml } => XmlReader::new(xml, Source::File, name),
        RestoreInput::Stdin(xml) => XmlReader::new(xml, Source::Stdin, "-"),
    };
    let mut restore = Restore {
        reader,
        error: None,
        errno: 0,
        range_check,
    };
    let mut rrd = Rrd::default();
    let parsed = restore
        .expect_element("rrd")
        .and_then(|()| restore.parse_tag_rrd(&mut rrd));
    let error = restore.error.take();
    if !restore.reader.diagnostics.is_empty() {
        let _ = diagnostics.write_all(&restore.reader.diagnostics);
        let _ = diagnostics.flush();
    }
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
        // Try exclusive creation first even with -f so cleanup never guesses
        // whether the destination belonged to this invocation.
        let (mut file, created) = match crate::rrd_binary::open_output_file(path, true) {
            Ok(file) => (file, true),
            Err(error) if force_overwrite && error.kind() == std::io::ErrorKind::AlreadyExists => {
                (crate::rrd_binary::open_output_file(path, false)?, false)
            }
            Err(error) => {
                return Err(StoreError::Rrd(format!(
                    "creating '{}': {}",
                    path.display(),
                    crate::rrd_binary::rrd_strerror(&error)
                )));
            }
        };
        let written = file
            .write_all(&bytes)
            .and_then(|()| file.set_len(bytes.len() as u64));
        if let Err(error) = written {
            if created {
                remove_created_output(path, &file);
            }
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

fn remove_created_output(path: &std::path::Path, file: &std::fs::File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(opened), Ok(named)) = (file.metadata(), std::fs::symlink_metadata(path)) {
            if opened.dev() == named.dev() && opened.ino() == named.ino() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (path, file); // Preserve the path when inode identity is unavailable.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(xml: &str) -> Restore<'_> {
        Restore {
            reader: XmlReader::new(xml.as_bytes(), Source::Stdin, "-"),
            error: None,
            errno: 0,
            range_check: false,
        }
    }

    #[test]
    fn restore_caps_data_sources_and_archives_before_growing_vectors() {
        let mut rrd = Rrd {
            ds_defs: vec![[0; DS_DEF_LEN]; MAX_RESTORE_DATA_SOURCES],
            ..Rrd::default()
        };
        let mut restore = parser("");
        assert!(restore.parse_tag_ds(&mut rrd).is_err());
        assert_eq!(rrd.ds_defs.len(), MAX_RESTORE_DATA_SOURCES);
        assert!(rrd.pdp_preps.is_empty());
        rrd.rra_defs = vec![[0; RRA_DEF_LEN]; MAX_RESTORE_ARCHIVES];
        assert!(restore.parse_tag_rra(&mut rrd).is_err());
        assert_eq!(rrd.rra_defs.len(), MAX_RESTORE_ARCHIVES);
        assert!(rrd.cdp_preps.is_empty());
    }

    #[test]
    fn restore_caps_values_before_allocating_the_next_xml_row() {
        let mut rrd = Rrd {
            ds_defs: vec![[0; DS_DEF_LEN]],
            rra_defs: vec![[0; RRA_DEF_LEN]],
            rra_ptrs: vec![0],
            values: vec![0.0; MAX_RESTORE_VALUES],
            ..Rrd::default()
        };
        let mut restore = parser("<database><row><v>1</v></row></database>");
        restore.expect_element("database").unwrap();
        assert!(restore.parse_tag_rra_database(&mut rrd).is_err());
        assert_eq!(rrd.values.len(), MAX_RESTORE_VALUES);
        assert_eq!(
            restore.error.as_deref(),
            Some("restore row or value safety limit exceeded")
        );
    }

    #[test]
    fn restore_row_budget_is_shared_by_all_archives() {
        let mut rrd = Rrd {
            ds_defs: vec![[0; DS_DEF_LEN]],
            rra_defs: vec![[0; RRA_DEF_LEN]],
            rra_ptrs: vec![0],
            rows: MAX_RESTORE_ROWS,
            ..Rrd::default()
        };
        let mut restore = parser("<database><row><v>1</v></row></database>");
        restore.expect_element("database").unwrap();
        assert!(restore.parse_tag_rra_database(&mut rrd).is_err());
        assert!(rrd.values.is_empty());
        assert_eq!(rrd.rows, MAX_RESTORE_ROWS);
    }
}
