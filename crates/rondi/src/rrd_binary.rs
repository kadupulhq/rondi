//! Read and fetch support for RRDtool versions 0003-0005 on the common 64-bit
//! little-endian layout. Operations reject source/archive features they do not implement.

use crate::format::{RrdFetchResult, RrdFetchRow};
use crate::storage::StoreError;
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const STAT_HEAD_LEN: usize = 128;
const DS_DEF_LEN: usize = 120;
const RRA_DEF_LEN: usize = 120;
const LIVE_HEAD_LEN: usize = 16;
const PDP_PREP_LEN: usize = 112;
const CDP_PREP_LEN: usize = 80;
const RRA_PTR_LEN: usize = 8;
const VALUE_LEN: usize = 8;
const FLOAT_COOKIE: f64 = 8.642135E130;
const MAX_HEADER_LEN: usize = 64 * 1024 * 1024;

/// Match the NaN produced by RRDtool's `rrd_set_to_DNAN`: the pinned x86_64
/// build uses a negative quiet NaN, while aarch64 and the other supported
/// targets use the positive quiet NaN representation.
#[inline]
pub(crate) fn rrd_nan() -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        f64::from_bits(0xfff8_0000_0000_0000)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        f64::NAN
    }
}

/// Evaluate `a * b + c` the way the pinned RRDtool builds do. GCC and Clang
/// contract this pattern into a fused multiply-add on aarch64, which always
/// has the instruction, while baseline x86_64 builds round the product first.
#[inline]
fn rrd_mul_add(a: f64, b: f64, c: f64) -> f64 {
    #[cfg(target_arch = "aarch64")]
    {
        a.mul_add(b, c)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        a * b + c
    }
}

/// Create an interoperable RRDtool file for the basic DS/RRA grammar.
/// The archive row pointer is initialized deterministically; RRDtool itself may
/// choose any row because every row in a newly created archive is unknown.
pub fn create_rrd_file(
    path: impl AsRef<Path>,
    start: i64,
    step: u64,
    data_sources: &[String],
    archives: &[String],
    no_overwrite: bool,
) -> Result<(), StoreError> {
    if !cfg!(target_pointer_width = "64") || cfg!(target_endian = "big") {
        return Err(StoreError::RrdUnsupported(
            "RRD v3 creation currently requires a 64-bit little-endian target".into(),
        ));
    }
    if step == 0 || data_sources.is_empty() || archives.is_empty() {
        return Err(StoreError::RrdUnsupported(
            "RRD create requires a positive step, at least one DS, and at least one RRA".into(),
        ));
    }
    if start < 315_360_000 {
        return Err(StoreError::RrdUnsupported(
            "the first entry to the RRD should be after 1980".into(),
        ));
    }
    let mut sources = Vec::new();
    let mut source_names = std::collections::HashSet::new();
    for definition in data_sources {
        let fields = definition.split(':').collect::<Vec<_>>();
        if fields.len() != 6
            || fields[0] != "DS"
            || !matches!(
                fields[2],
                "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
            )
        {
            return Err(StoreError::RrdUnsupported(format!(
                "unsupported data source definition: {definition}"
            )));
        }
        if fields[1].is_empty()
            || fields[1].len() > 19
            || !fields[1]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(StoreError::RrdUnsupported(format!(
                "invalid data source name: {}",
                fields[1]
            )));
        }
        if !source_names.insert(fields[1]) {
            return Err(StoreError::RrdUnsupported(format!(
                "duplicate data source name: {}",
                fields[1]
            )));
        }
        let heartbeat = parse_rrd_scaled_duration(fields[3], 1)?;
        if heartbeat == 0 {
            return Err(StoreError::RrdUnsupported(
                "heartbeat must be positive".into(),
            ));
        }
        let minimum = parse_rrd_bound(fields[4])?;
        let maximum = parse_rrd_bound(fields[5])?;
        if minimum.zip(maximum).is_some_and(|(min, max)| min >= max) {
            return Err(StoreError::RrdUnsupported(format!(
                "minimum must be less than maximum for data source {}",
                fields[1]
            )));
        }
        sources.push((fields[1], fields[2], heartbeat, minimum, maximum));
    }
    let mut rras = Vec::new();
    for definition in archives {
        let fields = definition.split(':').collect::<Vec<_>>();
        if fields.len() != 5
            || fields[0] != "RRA"
            || !matches!(fields[1], "AVERAGE" | "MIN" | "MAX" | "LAST")
        {
            return Err(StoreError::RrdUnsupported(format!(
                "unsupported archive definition: {definition}"
            )));
        }
        let xff = fields[2]
            .parse::<f64>()
            .map_err(|_| StoreError::RrdUnsupported("invalid RRA xff".into()))?;
        let pdps = parse_rrd_scaled_duration(fields[3], step)?;
        let row_divisor = step
            .checked_mul(pdps)
            .ok_or_else(|| StoreError::RrdUnsupported("RRA duration divisor overflows".into()))?;
        let rows = parse_rrd_scaled_duration(fields[4], row_divisor)?;
        if !xff.is_finite() || !(0.0..1.0).contains(&xff) || pdps == 0 || rows == 0 {
            return Err(StoreError::RrdUnsupported(format!(
                "invalid archive parameters: {definition}"
            )));
        }
        rras.push((fields[1], xff, pdps, rows));
    }
    let mut bytes = vec![0_u8; STAT_HEAD_LEN];
    bytes[0..4].copy_from_slice(b"RRD\0");
    let file_version = if sources
        .iter()
        .any(|(_, kind, _, _, _)| matches!(*kind, "DCOUNTER" | "DDERIVE"))
    {
        "0005"
    } else {
        "0003"
    };
    copy_fixed(&mut bytes[4..9], file_version);
    put_f64(&mut bytes, 16, FLOAT_COOKIE);
    put_u64(&mut bytes, 24, sources.len() as u64);
    put_u64(&mut bytes, 32, rras.len() as u64);
    put_u64(&mut bytes, 40, step);
    for (name, kind, heartbeat, minimum, maximum) in &sources {
        let offset = bytes.len();
        bytes.resize(offset + DS_DEF_LEN, 0);
        copy_fixed(&mut bytes[offset..offset + 20], name);
        copy_fixed(&mut bytes[offset + 20..offset + 40], kind);
        put_u64(&mut bytes, offset + 40, *heartbeat);
        put_f64(&mut bytes, offset + 48, minimum.unwrap_or(rrd_nan()));
        put_f64(&mut bytes, offset + 56, maximum.unwrap_or(rrd_nan()));
    }
    for (cf, xff, pdps, rows) in &rras {
        let offset = bytes.len();
        bytes.resize(offset + RRA_DEF_LEN, 0);
        copy_fixed(&mut bytes[offset..offset + 20], cf);
        put_u64(&mut bytes, offset + 24, *rows);
        put_u64(&mut bytes, offset + 32, *pdps);
        put_f64(&mut bytes, offset + 40, *xff);
    }
    let live_offset = bytes.len();
    bytes.resize(live_offset + LIVE_HEAD_LEN, 0);
    put_i64(&mut bytes, live_offset, start);
    let unknown = start.rem_euclid(
        i64::try_from(step).map_err(|_| StoreError::RrdUnsupported("step too large".into()))?,
    ) as u64;
    for _ in &sources {
        let offset = bytes.len();
        bytes.resize(offset + PDP_PREP_LEN, 0);
        copy_fixed(&mut bytes[offset..offset + 30], "U");
        put_u64(&mut bytes, offset + 32, unknown);
        put_f64(&mut bytes, offset + 40, rrd_nan());
    }
    let step_i64 =
        i64::try_from(step).map_err(|_| StoreError::RrdUnsupported("step too large".into()))?;
    for (_, _, pdps, _) in &rras {
        let period = step_i64
            .checked_mul(
                i64::try_from(*pdps)
                    .map_err(|_| StoreError::RrdUnsupported("RRA period too large".into()))?,
            )
            .ok_or_else(|| StoreError::RrdUnsupported("RRA period too large".into()))?;
        let unknown_pdps = (start - unknown as i64).rem_euclid(period) as u64 / step;
        for _ in &sources {
            let offset = bytes.len();
            bytes.resize(offset + CDP_PREP_LEN, 0);
            put_f64(&mut bytes, offset, rrd_nan());
            put_u64(&mut bytes, offset + 8, unknown_pdps);
        }
    }
    for _ in &rras {
        bytes.extend_from_slice(&0_u64.to_le_bytes());
    }
    for (_, _, _, rows) in &rras {
        let cells = rows
            .checked_mul(sources.len() as u64)
            .ok_or_else(|| StoreError::RrdUnsupported("RRA size overflow".into()))?;
        let byte_count = cells
            .checked_mul(VALUE_LEN as u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| StoreError::RrdUnsupported("RRA size overflow".into()))?;
        bytes.reserve(byte_count);
        for _ in 0..cells {
            bytes.extend_from_slice(&rrd_nan().to_le_bytes());
        }
    }
    let path = path.as_ref();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .ok_or_else(|| StoreError::RrdUnsupported("invalid output filename".into()))?
        .to_string_lossy();
    let existing_metadata = std::fs::metadata(path).ok();
    if no_overwrite && existing_metadata.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("creating '{}': File exists", path.display()),
        )
        .into());
    }
    static TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (temp_path, mut file) = (0..100)
        .find_map(|_| {
            let id = TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate =
                parent.join(format!(".{filename}.rondi-{}-{id}.tmp", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "unable to allocate temporary RRD filename",
            )
        })??;
    let result = (|| -> Result<(), StoreError> {
        file.write_all(&bytes)?;
        if let Some(metadata) = &existing_metadata {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        if no_overwrite {
            std::fs::hard_link(&temp_path, path)?;
            std::fs::remove_file(&temp_path)?;
        } else {
            std::fs::rename(&temp_path, path)?;
        }
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

/// Parse RRDtool's integer or suffixed duration syntax. The source implementation
/// (`rrd_scaled_duration`) treats bare integers as counts and scales suffixed
/// values, rejecting values that would be truncated by the supplied divisor.
pub fn parse_rrd_scaled_duration(text: &str, divisor: u64) -> Result<u64, StoreError> {
    let bytes = text.as_bytes();
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return Err(StoreError::RrdUnsupported(
            "duration must be a positive integer".into(),
        ));
    }
    let mut value = text[..digits]
        .parse::<u64>()
        .map_err(|_| StoreError::RrdUnsupported("duration overflows".into()))?;
    let suffix = bytes.get(digits).copied();
    let Some(suffix) = suffix else {
        if value == 0 {
            return Err(StoreError::RrdUnsupported(
                "duration must be positive".into(),
            ));
        }
        return Ok(value);
    };
    let multiplier = match suffix {
        b's' => 1,
        b'm' => 60,
        b'h' => 60 * 60,
        b'd' => 24 * 60 * 60,
        b'w' => 7 * 24 * 60 * 60,
        b'M' => 31 * 24 * 60 * 60,
        b'y' => 366 * 24 * 60 * 60,
        _ => {
            return Err(StoreError::RrdUnsupported(
                "duration has trailing garbage".into(),
            ));
        }
    };
    value = value
        .checked_mul(multiplier)
        .ok_or_else(|| StoreError::RrdUnsupported("duration overflows".into()))?;
    if value == 0 {
        return Err(StoreError::RrdUnsupported(
            "duration must be positive".into(),
        ));
    }
    if divisor == 0 || value % divisor != 0 {
        return Err(StoreError::RrdUnsupported(
            "duration would truncate when scaled".into(),
        ));
    }
    Ok(value / divisor)
}

/// rrd_create.c parseGENERIC_DS reads bounds with rrd_strtodbl. A NaN bound
/// is unbounded like `U`, but its bits are stored as parsed.
fn parse_rrd_bound(text: &str) -> Result<Option<f64>, StoreError> {
    if text == "U" {
        return Ok(None);
    }
    crate::parse_rrd_number(text)
        .or_else(|| parse_rrd_special(text))
        .map(Some)
        .ok_or_else(|| StoreError::RrdUnsupported(format!("invalid DS bound: {text}")))
}

/// rrd_strtod.c parse_special: case-insensitive prefixes, with the sign of
/// the NaN inverted as upstream does.
fn parse_rrd_special(text: &str) -> Option<f64> {
    let prefix = |special: &str| {
        text.get(..special.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(special))
    };
    if prefix("-nan") {
        Some(rrd_nan())
    } else if prefix("nan") {
        Some(-rrd_nan())
    } else if prefix("inf") {
        Some(f64::INFINITY)
    } else if prefix("-inf") {
        Some(f64::NEG_INFINITY)
    } else {
        None
    }
}

fn copy_fixed(destination: &mut [u8], value: &str) {
    let count = value.len().min(destination.len().saturating_sub(1));
    destination[..count].copy_from_slice(&value.as_bytes()[..count]);
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_i64(bytes: &mut [u8], offset: usize, value: i64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
    put_u64(bytes, offset, value.to_bits());
}

#[derive(Debug, Clone, PartialEq)]
pub struct RrdInfo {
    pub version: String,
    pub step: u64,
    pub last_update: i64,
    /// Microsecond component of the RRDtool live header's last update time.
    pub last_update_usec: u64,
    pub header_size: usize,
    pub data_sources: Vec<RrdDataSourceInfo>,
    pub archives: Vec<RrdArchiveInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RrdDataSourceInfo {
    pub name: String,
    pub kind: String,
    pub heartbeat: u64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub last_value: String,
    pub pdp_value: f64,
    pub unknown_seconds: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RrdArchiveInfo {
    pub consolidation: String,
    pub rows: u64,
    pub pdp_per_row: u64,
    pub xff: f64,
    pub current_row: u64,
    pub cdp_prep: Vec<RrdCdpPrepInfo>,
    pub(crate) data_offset: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RrdCdpPrepInfo {
    pub value: f64,
    pub unknown_datapoints: u64,
    pub primary_value: f64,
    pub secondary_value: f64,
}

pub(crate) fn inspect_path(path: &Path) -> Result<RrdInfo, StoreError> {
    let mut file = RrdFileLock::shared(open_rrd_read(path)?)?;
    read_info(&mut file)
}

pub(crate) fn fetch_path(
    path: &Path,
    consolidation: &str,
    requested_start: i64,
    requested_end: i64,
    requested_step: u64,
) -> Result<RrdFetchResult, StoreError> {
    if requested_step == 0 || requested_end < requested_start {
        return Err(StoreError::RrdUnsupported(
            "RRD fetch requires a positive resolution and end >= start".into(),
        ));
    }
    let mut file = RrdFileLock::shared(open_rrd_read(path)?)?;
    let info = read_info(&mut file)?;
    let archive_index = choose_archive(
        &info,
        consolidation,
        requested_start,
        requested_end,
        requested_step,
    )?;
    let archive = &info.archives[archive_index];
    let step = info
        .step
        .checked_mul(archive.pdp_per_row)
        .ok_or_else(|| rrd_error("RRD archive step overflows"))?;
    let step_i64 = i64::try_from(step).map_err(|_| rrd_error("RRD archive step overflows"))?;
    let start = requested_start
        .checked_sub(requested_start.rem_euclid(step_i64))
        .ok_or_else(|| rrd_error("RRD fetch start overflows"))?;
    let mut end = requested_end
        .checked_add(step_i64 - requested_end.rem_euclid(step_i64))
        .ok_or_else(|| rrd_error("RRD fetch end overflows"))?;
    let range_rows = usize::try_from((end - start) / step_i64 + 1)
        .map_err(|_| rrd_error("RRD fetch range is too large"))?;
    if range_rows > 10_000_000 {
        return Err(StoreError::RrdUnsupported(
            "RRD fetch range exceeds the 10 million row safety limit".into(),
        ));
    }

    let archive_end = info
        .last_update
        .checked_sub(info.last_update.rem_euclid(step_i64))
        .ok_or_else(|| rrd_error("RRD archive end overflows"))?;
    let archive_start = archive_end
        .checked_sub(
            step_i64
                .checked_mul(
                    i64::try_from(archive.rows - 1)
                        .map_err(|_| rrd_error("RRD row count overflows"))?,
                )
                .ok_or_else(|| rrd_error("RRD archive time range overflows"))?,
        )
        .ok_or_else(|| rrd_error("RRD archive time range overflows"))?;
    let start_offset =
        (i128::from(start) + i128::from(step) - i128::from(archive_start)) / i128::from(step);
    let end_offset = (i128::from(archive_end) - i128::from(end)) / i128::from(step);
    let row_count = i128::from(archive.rows);
    let mut pointer = 0_i128;
    let in_archive_range = start <= archive_end && end >= archive_start - step_i64;
    if in_archive_range {
        pointer = if start_offset <= 0 {
            i128::from(archive.current_row) + 1
        } else {
            i128::from(archive.current_row) + 1 + start_offset
        }
        .rem_euclid(row_count);
    }

    let mut rows = Vec::new();
    let mut timestamp = start
        .checked_add(step_i64)
        .ok_or_else(|| rrd_error("RRD fetch timestamp overflows"))?;
    let mut index = start_offset;
    while index < row_count - end_offset {
        if rows.len() >= 10_000_000 {
            return Err(StoreError::RrdUnsupported(
                "RRD fetch output exceeds the 10 million row safety limit".into(),
            ));
        }
        let values = if index < 0 || index >= row_count || !in_archive_range {
            vec![None; info.data_sources.len()]
        } else {
            let offset = archive
                .data_offset
                .checked_add(
                    u64::try_from(pointer)
                        .map_err(|_| rrd_error("RRD row offset overflows"))?
                        .checked_mul(info.data_sources.len() as u64 * VALUE_LEN as u64)
                        .ok_or_else(|| rrd_error("RRD row offset overflows"))?,
                )
                .ok_or_else(|| rrd_error("RRD row offset overflows"))?;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; info.data_sources.len() * VALUE_LEN];
            file.read_exact(&mut bytes)?;
            pointer += 1;
            bytes
                .chunks_exact(VALUE_LEN)
                .map(|chunk| {
                    let value = f64::from_le_bytes(chunk.try_into().expect("eight byte value"));
                    value.is_finite().then_some(value)
                })
                .collect()
        };
        rows.push(RrdFetchRow { timestamp, values });
        timestamp = timestamp
            .checked_add(step_i64)
            .ok_or_else(|| rrd_error("RRD fetch timestamp overflows"))?;
        index += 1;
        if pointer >= row_count {
            pointer -= row_count;
        }
    }
    // RRDtool reports the aligned request bounds, not the first/last emitted
    // row timestamps.
    end = start
        .checked_add(
            step_i64
                .checked_mul(
                    i64::try_from(rows.len()).map_err(|_| rrd_error("RRD result is too large"))?,
                )
                .ok_or_else(|| rrd_error("RRD fetch range overflows"))?,
        )
        .ok_or_else(|| rrd_error("RRD fetch range overflows"))?;
    Ok(RrdFetchResult {
        start,
        end,
        step,
        data_sources: info.data_sources.into_iter().map(|ds| ds.name).collect(),
        rows,
    })
}

fn update_path(
    path: &Path,
    timestamp: i64,
    timestamp_usec: u64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    update_path_values(path, timestamp, timestamp_usec, &[value]).map(|_| ())
}

fn update_path_values(
    path: &Path,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<f64>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    update_path_values_with_raw(path, timestamp, timestamp_usec, values, None)
}

fn update_path_values_with_raw(
    path: &Path,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<f64>],
    raw_values: Option<&[Option<&str>]>,
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    if timestamp_usec >= 1_000_000 {
        return Err(StoreError::InvalidValue);
    }
    // RRDtool's text update path accepts nan and inf spellings; callers that
    // pass numbers directly keep the stricter finite-only contract.
    if raw_values.is_none() && values.iter().flatten().any(|value| !value.is_finite()) {
        return Err(StoreError::InvalidValue);
    }
    if raw_values.is_some_and(|raw| raw.len() != values.len()) {
        return Err(StoreError::InvalidValue);
    }
    let mut file = RrdFileLock::exclusive(open_rrd_write(path)?)?;
    let info = read_info(&mut file)?;
    if info.data_sources.len() != values.len()
        || info.data_sources.iter().any(|ds| {
            !matches!(
                ds.kind.as_str(),
                "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
            )
        })
        || info.archives.is_empty()
        || info.archives.iter().any(|archive| {
            !matches!(
                archive.consolidation.as_str(),
                "AVERAGE" | "MIN" | "MAX" | "LAST"
            )
        })
    {
        return Err(StoreError::RrdUnsupported(
            "in-place update requires one value per supported data source and basic archives (AVERAGE, MIN, MAX, LAST)".into(),
        ));
    }
    for (index, (source, value)) in info.data_sources.iter().zip(values).enumerate() {
        if let Some(value) = value {
            if matches!(source.kind.as_str(), "COUNTER" | "DERIVE") {
                let valid = if let Some(raw) = raw_values.and_then(|raw| raw[index]) {
                    valid_integer_sample(raw, source.kind == "COUNTER")
                } else {
                    value.fract() == 0.0
                        && (source.kind != "COUNTER" || *value >= 0.0)
                        && value.abs() <= 9_007_199_254_740_992.0
                };
                if !valid {
                    return Err(StoreError::InvalidValue);
                }
            }
        }
    }
    let last_update = info.last_update;
    let last_update_usec = info.last_update_usec;
    if (timestamp, timestamp_usec) <= (last_update, last_update_usec) {
        return Err(StoreError::RrdTimestamp(format!(
            "illegal attempt to update using time {timestamp} when last update time is {last_update} (minimum one second step)"
        )));
    }
    let step = i64::try_from(info.step).map_err(|_| rrd_error("RRD PDP step overflows"))?;
    let interval = (timestamp
        .checked_sub(last_update)
        .ok_or_else(|| rrd_error("RRD update interval overflows"))? as f64)
        + (timestamp_usec as f64 - last_update_usec as f64) / 1_000_000.0;
    let previous_boundary = last_update
        .checked_sub(last_update.rem_euclid(step))
        .ok_or_else(|| rrd_error("RRD timestamp alignment overflows"))?;
    let current_boundary = timestamp
        .checked_sub(timestamp.rem_euclid(step))
        .ok_or_else(|| rrd_error("RRD timestamp alignment overflows"))?;
    let elapsed_steps = current_boundary
        .checked_sub(previous_boundary)
        .ok_or_else(|| rrd_error("RRD elapsed steps overflow"))?
        / step;
    let ds_count = info.data_sources.len();
    let rra_start = STAT_HEAD_LEN + ds_count * DS_DEF_LEN;
    let live_start = rra_start + info.archives.len() * RRA_DEF_LEN;
    let pdp_start = live_start + LIVE_HEAD_LEN;
    let cdp_start = pdp_start + ds_count * PDP_PREP_LEN;
    let pointer_start = cdp_start + ds_count * info.archives.len() * CDP_PREP_LEN;

    // The steps below follow rrd_update.c process_arg so that every archive
    // value is produced by the same sequence of floating-point operations.
    let mut pdp_new = Vec::with_capacity(ds_count);
    let mut last_ds_bytes = Vec::with_capacity(ds_count);
    for (index, (source, value)) in info.data_sources.iter().zip(values).enumerate() {
        pdp_new.push(update_pdp_new(
            source,
            *value,
            raw_values.and_then(|raw| raw[index]),
            interval,
        ));
        let mut last_ds = value.map_or_else(
            || "U".to_owned(),
            |v| {
                raw_values
                    .and_then(|raw| raw[index])
                    .map_or_else(|| v.to_string(), str::to_owned)
            },
        );
        last_ds.truncate(29);
        let mut encoded = [0_u8; 30];
        encoded[..last_ds.len()].copy_from_slice(last_ds.as_bytes());
        last_ds_bytes.push(encoded);
    }
    let mut pdp_prep = info
        .data_sources
        .iter()
        .map(|source| (source.unknown_seconds, source.pdp_value))
        .collect::<Vec<_>>();
    let mut cdp_prep = info
        .archives
        .iter()
        .flat_map(|archive| archive.cdp_prep.iter().cloned())
        .collect::<Vec<_>>();
    let mut current_rows = info
        .archives
        .iter()
        .map(|archive| archive.current_row)
        .collect::<Vec<_>>();
    let mut row_writes = Vec::new();
    let mut summaries = Vec::new();

    if elapsed_steps == 0 {
        for ((unknown, value), amount) in pdp_prep.iter_mut().zip(&pdp_new) {
            if amount.is_nan() {
                *unknown = (*unknown as f64 + interval.floor()) as u64;
            } else if value.is_nan() {
                *value = *amount;
            } else {
                *value += *amount;
            }
        }
    } else {
        let mut interval = interval;
        let mut elapsed = elapsed_steps as u64;
        let mut proc_pdp_count = previous_boundary.div_euclid(step) as u64;
        let mut pre_interval =
            (current_boundary - last_update) as f64 - last_update_usec as f64 / 1_000_000.0;
        let post_interval =
            (timestamp - current_boundary) as f64 + timestamp_usec as f64 / 1_000_000.0;
        if elapsed > 1 {
            // RRDtool closes the previously open PDP on its own before the
            // remaining ones. The cast truncates fractional seconds, so a
            // sub-second last update moves time between the two phases.
            let open_seconds = (pre_interval as u64) % info.step;
            if open_seconds > 0 {
                let open_new = pdp_new
                    .iter_mut()
                    .map(|amount| {
                        if amount.is_nan() || interval <= 0.0 {
                            rrd_nan()
                        } else {
                            let open = *amount * open_seconds as f64 / interval;
                            *amount -= open;
                            open
                        }
                    })
                    .collect::<Vec<_>>();
                let pdp_temp = process_pdp_steps(
                    &info,
                    &mut pdp_prep,
                    open_seconds as f64,
                    open_seconds as f64,
                    0.0,
                    1,
                    &open_new,
                );
                let row_counts =
                    update_cdp_preps(&info, &mut cdp_prep, 1, proc_pdp_count, &pdp_temp);
                let open_close_time = last_update
                    .checked_add(open_seconds as i64)
                    .ok_or_else(|| rrd_error("RRD timestamp alignment overflows"))?;
                write_rra_rows(
                    &info,
                    &cdp_prep,
                    &mut current_rows,
                    &row_counts,
                    open_close_time,
                    &mut row_writes,
                    &mut summaries,
                )?;
                interval -= open_seconds as f64;
                pre_interval -= open_seconds as f64;
                elapsed -= 1;
                proc_pdp_count += 1;
            }
        }
        let pdp_temp = process_pdp_steps(
            &info,
            &mut pdp_prep,
            interval,
            pre_interval,
            post_interval,
            elapsed,
            &pdp_new,
        );
        let row_counts = update_cdp_preps(&info, &mut cdp_prep, elapsed, proc_pdp_count, &pdp_temp);
        write_rra_rows(
            &info,
            &cdp_prep,
            &mut current_rows,
            &row_counts,
            timestamp,
            &mut row_writes,
            &mut summaries,
        )?;
    }

    // All format and semantic checks precede the first write.
    for (offset, row) in &row_writes {
        file.seek(SeekFrom::Start(*offset))?;
        let mut bytes = Vec::with_capacity(row.len() * VALUE_LEN);
        for value in row {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        file.write_all(&bytes)?;
    }
    let mut state = vec![0_u8; pointer_start + info.archives.len() * RRA_PTR_LEN - pdp_start];
    file.seek(SeekFrom::Start(pdp_start as u64))?;
    file.read_exact(&mut state)?;
    for (index, ((unknown, value), last_ds)) in pdp_prep.iter().zip(&last_ds_bytes).enumerate() {
        let offset = index * PDP_PREP_LEN;
        state[offset..offset + 30].copy_from_slice(last_ds);
        put_u64(&mut state, offset + 32, *unknown);
        put_f64(&mut state, offset + 40, *value);
    }
    for (index, scratch) in cdp_prep.iter().enumerate() {
        let offset = cdp_start - pdp_start + index * CDP_PREP_LEN;
        put_f64(&mut state, offset, scratch.value);
        put_u64(&mut state, offset + 8, scratch.unknown_datapoints);
        put_f64(&mut state, offset + 64, scratch.primary_value);
        put_f64(&mut state, offset + 72, scratch.secondary_value);
    }
    for (index, current_row) in current_rows.iter().enumerate() {
        put_u64(
            &mut state,
            pointer_start - pdp_start + index * RRA_PTR_LEN,
            *current_row,
        );
    }
    file.seek(SeekFrom::Start(pdp_start as u64))?;
    file.write_all(&state)?;
    file.seek(SeekFrom::Start(live_start as u64))?;
    file.write_all(&timestamp.to_le_bytes())?;
    file.write_all(&(timestamp_usec as i64).to_le_bytes())?;
    file.sync_data()?;
    Ok(summaries)
}

/// rrd_update.c update_pdp_prep: the rate times seconds contributed by one
/// sample, or NaN when it is unknown, out of range, or past the heartbeat.
fn update_pdp_new(
    source: &RrdDataSourceInfo,
    value: Option<f64>,
    raw_value: Option<&str>,
    interval: f64,
) -> f64 {
    let Some(sample) = value else {
        return rrd_nan();
    };
    if (source.heartbeat as f64) < interval {
        return rrd_nan();
    }
    let previous = source.last_value.as_str();
    let (amount, rate) = match source.kind.as_str() {
        "GAUGE" => (sample * interval, sample),
        "ABSOLUTE" => (sample, sample / interval),
        "COUNTER" | "DERIVE" if previous != "U" => {
            let delta = if let Some(current) = raw_value {
                exact_integer_delta(current, previous)
            } else {
                previous
                    .parse::<f64>()
                    .ok()
                    .map(|previous| sample - previous)
            };
            let Some(mut delta) = delta else {
                return rrd_nan();
            };
            if source.kind == "COUNTER" {
                if delta < 0.0 {
                    delta += 4_294_967_295.0;
                }
                if delta < 0.0 {
                    delta += 18_446_744_069_414_584_320.0;
                }
            }
            (delta, delta / interval)
        }
        "DCOUNTER" | "DDERIVE" if previous != "U" => {
            let Ok(previous) = previous.parse::<f64>() else {
                return rrd_nan();
            };
            if source.kind == "DCOUNTER"
                && ((sample > 0.0 && previous > sample) || (sample < 0.0 && sample > previous))
            {
                return rrd_nan();
            }
            let delta = sample - previous;
            (delta, delta / interval)
        }
        _ => return rrd_nan(),
    };
    if !rate.is_nan()
        && (source.maximum.is_some_and(|maximum| rate > maximum)
            || source.minimum.is_some_and(|minimum| rate < minimum))
    {
        return rrd_nan();
    }
    amount
}

/// rrd_update.c process_pdp_st for every data source. Returns the rate for
/// each completed PDP and leaves the PDP prep area ready for the next update.
fn process_pdp_steps(
    info: &RrdInfo,
    pdp_prep: &mut [(u64, f64)],
    interval: f64,
    pre_interval: f64,
    post_interval: f64,
    elapsed: u64,
    pdp_new: &[f64],
) -> Vec<f64> {
    let elapsed_seconds = elapsed.wrapping_mul(info.step);
    info.data_sources
        .iter()
        .zip(pdp_prep.iter_mut())
        .zip(pdp_new)
        .map(|((source, (unknown, value)), &amount)| {
            let mut pre_unknown = 0.0;
            if amount.is_nan() {
                pre_unknown = pre_interval;
            } else {
                if value.is_nan() {
                    *value = 0.0;
                }
                *value = rrd_mul_add(amount / interval, pre_interval, *value);
            }
            // RRDtool compares the unknown seconds through a signed int cast.
            let rate = if interval > source.heartbeat as f64
                || (info.step as f64 / 2.0) < f64::from(*unknown as i32)
            {
                rrd_nan()
            } else {
                *value / (elapsed_seconds.wrapping_sub(*unknown) as f64 - pre_unknown)
            };
            if amount.is_nan() {
                *unknown = post_interval.floor() as u64;
                *value = rrd_nan();
            } else {
                *unknown = 0;
                *value = amount / interval * post_interval;
            }
            rate
        })
        .collect()
}

/// rrd_update.c update_all_cdp_prep for the basic consolidation functions.
/// Returns how many rows each archive must write.
fn update_cdp_preps(
    info: &RrdInfo,
    cdp_prep: &mut [RrdCdpPrepInfo],
    elapsed: u64,
    proc_pdp_count: u64,
    pdp_temp: &[f64],
) -> Vec<u64> {
    let ds_count = pdp_temp.len();
    info.archives
        .iter()
        .enumerate()
        .map(|(archive_index, archive)| {
            let pdp_count = archive.pdp_per_row;
            let start_offset = pdp_count - proc_pdp_count % pdp_count;
            let row_count = if start_offset <= elapsed {
                ((elapsed - start_offset) / pdp_count + 1).min(archive.rows)
            } else {
                0
            };
            for (ds_index, &rate) in pdp_temp.iter().enumerate() {
                let scratch = &mut cdp_prep[archive_index * ds_count + ds_index];
                if pdp_count > 1 {
                    update_cdp(scratch, archive, rate, row_count, elapsed, start_offset);
                } else {
                    // update_aberrant_CF sets the primary value and, when two
                    // or more PDPs completed, reset_cdp or a second pass sets
                    // the secondary value too.
                    scratch.primary_value = rate;
                    if elapsed > 1 {
                        scratch.secondary_value = rate;
                    }
                }
            }
            row_count
        })
        .collect()
}

/// rrd_update.c update_cdp with initialize_cdp_val, initialize_carry_over
/// and calculate_cdp_val.
fn update_cdp(
    scratch: &mut RrdCdpPrepInfo,
    archive: &RrdArchiveInfo,
    rate: f64,
    row_count: u64,
    elapsed: u64,
    start_offset: u64,
) {
    let pdp_count = archive.pdp_per_row;
    let consolidation = archive.consolidation.as_str();
    if row_count > 0 {
        if rate.is_nan() {
            scratch.unknown_datapoints = scratch.unknown_datapoints.wrapping_add(start_offset);
            scratch.secondary_value = rrd_nan();
        } else {
            scratch.secondary_value = rate;
        }
        if scratch.unknown_datapoints as f64 > pdp_count as f64 * archive.xff {
            scratch.primary_value = rrd_nan();
        } else {
            scratch.primary_value = match consolidation {
                "AVERAGE" => {
                    let cumulative = if scratch.value.is_nan() {
                        0.0
                    } else {
                        scratch.value
                    };
                    let current = if rate.is_nan() { 0.0 } else { rate };
                    rrd_mul_add(current, start_offset as f64, cumulative)
                        / pdp_count.wrapping_sub(scratch.unknown_datapoints) as f64
                }
                "MAX" => {
                    let cumulative = if scratch.value.is_nan() {
                        f64::NEG_INFINITY
                    } else {
                        scratch.value
                    };
                    let current = if rate.is_nan() {
                        f64::NEG_INFINITY
                    } else {
                        rate
                    };
                    if current > cumulative {
                        current
                    } else {
                        cumulative
                    }
                }
                "MIN" => {
                    let cumulative = if scratch.value.is_nan() {
                        f64::INFINITY
                    } else {
                        scratch.value
                    };
                    let current = if rate.is_nan() { f64::INFINITY } else { rate };
                    if current < cumulative {
                        current
                    } else {
                        cumulative
                    }
                }
                _ => rate,
            };
        }
        let carried = (elapsed - start_offset) % pdp_count;
        scratch.value = if carried == 0 || rate.is_nan() {
            match consolidation {
                "MAX" => f64::NEG_INFINITY,
                "MIN" => f64::INFINITY,
                "AVERAGE" => 0.0,
                _ => rrd_nan(),
            }
        } else if consolidation == "AVERAGE" {
            rate * carried as f64
        } else {
            rate
        };
        scratch.unknown_datapoints = if rate.is_nan() { carried } else { 0 };
    } else if rate.is_nan() {
        scratch.unknown_datapoints = scratch.unknown_datapoints.wrapping_add(elapsed);
    } else if scratch.value.is_nan() {
        scratch.value = if consolidation == "AVERAGE" {
            rate * elapsed as f64
        } else {
            rate
        };
    } else {
        scratch.value = match consolidation {
            "AVERAGE" => rrd_mul_add(rate, elapsed as f64, scratch.value),
            "MIN" if rate < scratch.value => rate,
            "MAX" if rate > scratch.value => rate,
            "MIN" | "MAX" => scratch.value,
            _ => rate,
        };
    }
}

/// rrd_update.c write_to_rras: advance each archive pointer and queue the
/// primary value for the first row and the secondary value for the rest.
fn write_rra_rows(
    info: &RrdInfo,
    cdp_prep: &[RrdCdpPrepInfo],
    current_rows: &mut [u64],
    row_counts: &[u64],
    current_time: i64,
    row_writes: &mut Vec<(u64, Vec<f64>)>,
    summaries: &mut Vec<RrdUpdateSummary>,
) -> Result<(), StoreError> {
    let ds_count = info.data_sources.len();
    for (archive_index, archive) in info.archives.iter().enumerate() {
        let mut remaining = row_counts[archive_index];
        if remaining == 0 {
            continue;
        }
        let step_time = archive
            .pdp_per_row
            .checked_mul(info.step)
            .ok_or_else(|| rrd_error("RRD archive step overflows"))?;
        let time = current_time as u64;
        let base_time = time.wrapping_sub(time % step_time);
        let mut step_subtract = 1_u64;
        while remaining > 0 {
            let current_row = &mut current_rows[archive_index];
            *current_row += 1;
            if *current_row >= archive.rows {
                *current_row = 0;
            }
            let row = cdp_prep[archive_index * ds_count..(archive_index + 1) * ds_count]
                .iter()
                .map(|scratch| {
                    if step_subtract == 1 {
                        scratch.primary_value
                    } else {
                        scratch.secondary_value
                    }
                })
                .collect::<Vec<_>>();
            let offset = current_row
                .checked_mul(ds_count as u64 * VALUE_LEN as u64)
                .and_then(|offset| archive.data_offset.checked_add(offset))
                .ok_or_else(|| rrd_error("RRD archive row offset overflows"))?;
            // RRDtool derives the reported time from the count it is
            // decrementing, so rows after the second report later times.
            let row_time = base_time.wrapping_sub(
                remaining
                    .wrapping_sub(step_subtract)
                    .wrapping_mul(step_time),
            );
            summaries.push(RrdUpdateSummary {
                timestamp: row_time as i64,
                consolidation: archive.consolidation.clone(),
                pdp_per_row: archive.pdp_per_row,
                values: row.clone(),
            });
            row_writes.push((offset, row));
            remaining -= 1;
            step_subtract = 2;
        }
    }
    Ok(())
}

fn valid_integer_sample(value: &str, unsigned: bool) -> bool {
    if value.is_empty() || value.len() > 29 {
        return false;
    }
    let digits = if !unsigned && value.starts_with('-') {
        &value[1..]
    } else {
        value
    };
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<i128>().is_ok()
}

fn exact_integer_delta(current: &str, previous: &str) -> Option<f64> {
    let current = current.parse::<i128>().ok()?;
    let previous = previous.parse::<i128>().ok()?;
    if (current < 0) != (previous < 0) {
        return None;
    }
    Some((current - previous) as f64)
}

/// One archive row written by a verbose RRDtool-compatible update.
#[derive(Clone, Debug, PartialEq)]
pub struct RrdUpdateSummary {
    pub timestamp: i64,
    pub consolidation: String,
    pub pdp_per_row: u64,
    pub values: Vec<f64>,
}

/// Update an existing RRDtool v3-v5 file in place for the currently
/// supported GAUGE/basic archive layouts.
pub fn update_rrd_file(
    path: impl AsRef<Path>,
    timestamp: i64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    update_path(path.as_ref(), timestamp, 0, value)
}

/// Update an existing RRDtool file at a normalized timestamp with microsecond
/// precision, preserving the timestamp representation used by RRDtool 1.11.0.
pub fn update_rrd_file_precise(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    update_path(path.as_ref(), timestamp, timestamp_usec, value)
}

/// Update one timestamp in an existing v3 file using one value per data source.
pub fn update_rrd_values(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<f64>],
) -> Result<(), StoreError> {
    update_path_values(path.as_ref(), timestamp, 0, values).map(|_| ())
}

/// Update an RRD while preserving the caller's exact decimal text for each
/// data source. Integer COUNTER and DERIVE differences are calculated before
/// conversion to `f64`, matching RRDtool's string-based `rrd_diff` behavior.
pub fn update_rrd_raw_values(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<&str>],
) -> Result<(), StoreError> {
    update_rrd_raw_values_precise(path, timestamp, 0, values)
}

/// Raw-text counterpart to [`update_rrd_file_precise`].
pub fn update_rrd_raw_values_precise(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<&str>],
) -> Result<(), StoreError> {
    update_path_values_with_raw(
        path.as_ref(),
        timestamp,
        timestamp_usec,
        &parse_raw_values(values)?,
        Some(values),
    )
    .map(|_| ())
}

/// RRDtool's update and updatev both convert samples with rrd_strtodbl.
fn parse_raw_values(values: &[Option<&str>]) -> Result<Vec<Option<f64>>, StoreError> {
    values
        .iter()
        .map(|value| {
            value
                .map(|value| crate::parse_rrd_number(value).ok_or(StoreError::InvalidValue))
                .transpose()
        })
        .collect()
}

/// Update an RRD and return the archive rows written, in RRA and row order.
pub fn update_rrd_values_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<f64>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    update_path_values(path.as_ref(), timestamp, 0, values)
}

/// Raw-text counterpart to [`update_rrd_values_verbose`].
pub fn update_rrd_raw_values_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<&str>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    update_rrd_raw_values_precise_verbose(path, timestamp, 0, values)
}

/// Raw-text verbose counterpart to [`update_rrd_file_precise`].
pub fn update_rrd_raw_values_precise_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<&str>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    update_path_values_with_raw(
        path.as_ref(),
        timestamp,
        timestamp_usec,
        &parse_raw_values(values)?,
        Some(values),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RrdTuneBound {
    Unbounded,
    Value(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RrdDataSourceTune {
    pub name: String,
    pub kind: Option<String>,
    pub new_name: Option<String>,
    pub heartbeat: Option<u64>,
    pub minimum: Option<RrdTuneBound>,
    pub maximum: Option<RrdTuneBound>,
}

/// Change heartbeat and min/max metadata in-place without rewriting archive
/// data. The update holds RRDtool's exclusive whole-file POSIX lock.
pub fn tune_rrd_data_sources(
    path: impl AsRef<Path>,
    changes: &[RrdDataSourceTune],
) -> Result<(), StoreError> {
    let mut file = RrdFileLock::exclusive(open_rrd_write(path.as_ref())?)?;
    let info = read_info(&mut file)?;
    if info.data_sources.iter().any(|source| {
        !matches!(
            source.kind.as_str(),
            "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
        )
    }) {
        return Err(StoreError::RrdUnsupported(
            "tune metadata currently supports basic data-source types only".into(),
        ));
    }
    let mut resolved = Vec::with_capacity(changes.len());
    for change in changes {
        let index = info
            .data_sources
            .iter()
            .position(|source| source.name == change.name)
            .ok_or_else(|| StoreError::RrdUnsupported(format!("No DS called {}", change.name)))?;
        for bound in [change.minimum, change.maximum].into_iter().flatten() {
            if matches!(bound, RrdTuneBound::Value(value) if value.is_nan()) {
                return Err(StoreError::RrdUnsupported(
                    "NaN DS bounds must be expressed as U".into(),
                ));
            }
        }
        if let Some(kind) = &change.kind {
            if !matches!(
                kind.as_str(),
                "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
            ) {
                return Err(StoreError::RrdUnsupported(format!(
                    "unsupported data source type: {kind}"
                )));
            }
        }
        if let Some(name) = &change.new_name {
            if name.is_empty()
                || name.len() > 19
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err(StoreError::RrdUnsupported(format!(
                    "invalid data source name: {name}"
                )));
            }
        }
        resolved.push((index, change));
    }

    for (index, change) in resolved {
        let offset = STAT_HEAD_LEN + index * DS_DEF_LEN;
        if let Some(heartbeat) = change.heartbeat {
            file.seek(SeekFrom::Start((offset + 40) as u64))?;
            file.write_all(&heartbeat.to_le_bytes())?;
        }
        if let Some(kind) = &change.kind {
            if kind != &info.data_sources[index].kind {
                if kind.len() >= 20 {
                    return Err(StoreError::RrdUnsupported(
                        "data source type name is too long".into(),
                    ));
                }
                let mut type_bytes = [0_u8; 20];
                type_bytes[..kind.len()].copy_from_slice(kind.as_bytes());
                file.seek(SeekFrom::Start((offset + 20) as u64))?;
                file.write_all(&type_bytes)?;
                let pdp_start = STAT_HEAD_LEN
                    + info.data_sources.len() * DS_DEF_LEN
                    + info.archives.len() * RRA_DEF_LEN
                    + LIVE_HEAD_LEN;
                let pdp_offset = pdp_start + index * PDP_PREP_LEN;
                file.seek(SeekFrom::Start(pdp_offset as u64))?;
                file.write_all(b"UNKN\0")?;
            }
        }
        if let Some(name) = &change.new_name {
            if name != &info.data_sources[index].name {
                let mut name_bytes = [0_u8; 20];
                name_bytes[..name.len()].copy_from_slice(name.as_bytes());
                file.seek(SeekFrom::Start(offset as u64))?;
                file.write_all(&name_bytes)?;
            }
        }
        for (bound, relative_offset) in [(change.minimum, 48_u64), (change.maximum, 56_u64)] {
            if let Some(bound) = bound {
                let value = match bound {
                    RrdTuneBound::Unbounded => rrd_nan(),
                    RrdTuneBound::Value(value) => value,
                };
                file.seek(SeekFrom::Start((offset as u64) + relative_offset))?;
                file.write_all(&value.to_le_bytes())?;
            }
        }
    }
    if !changes.is_empty() {
        file.sync_data()?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RrdResizeAction {
    Grow,
    Shrink,
}

/// Write an RRDtool-compatible resized copy to `output`, preserving row order
/// and the archive cursor. The input is held under the upstream exclusive lock.
pub fn resize_rrd_file(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    rra_index: usize,
    action: RrdResizeAction,
    row_count: u64,
) -> Result<(), StoreError> {
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();
    if input_path
        .file_name()
        .is_some_and(|name| name == "resize.rrd")
    {
        return Err(StoreError::RrdUnsupported(
            "resize.rrd is a reserved name".into(),
        ));
    }
    if row_count == 0 {
        return Err(StoreError::RrdUnsupported(
            "Please grow or shrink with at least 1 row".into(),
        ));
    }
    if input_path == output_path {
        return Err(StoreError::RrdUnsupported(
            "resize output must be different from the input file".into(),
        ));
    }

    let mut input = RrdFileLock::exclusive(open_rrd_write(input_path)?)?;
    let info = read_info(&mut input)?;
    if !matches!(info.version.as_str(), "0003" | "0004") {
        return Err(StoreError::RrdUnsupported(format!(
            "Do not know how to handle RRD version {}",
            info.version
        )));
    }
    let archive = info
        .archives
        .get(rra_index)
        .ok_or_else(|| StoreError::RrdUnsupported("no such RRA in this RRD".into()))?;
    let new_row_count = match action {
        RrdResizeAction::Grow => archive
            .rows
            .checked_add(row_count)
            .ok_or_else(|| StoreError::RrdUnsupported("RRA row count overflows".into()))?,
        RrdResizeAction::Shrink if archive.rows <= row_count => {
            return Err(StoreError::RrdUnsupported(
                "This RRA is not that big".into(),
            ));
        }
        RrdResizeAction::Shrink => archive.rows - row_count,
    };
    let data_sources = info.data_sources.len();
    let row_bytes = data_sources
        .checked_mul(VALUE_LEN)
        .ok_or_else(|| StoreError::RrdUnsupported("RRA row size overflows".into()))?;
    let header_size = info.header_size;
    let mut header = vec![0_u8; header_size];
    input.seek(SeekFrom::Start(0))?;
    input.read_exact(&mut header)?;
    let rra_start = STAT_HEAD_LEN + data_sources * DS_DEF_LEN;
    put_u64(
        &mut header,
        rra_start + rra_index * RRA_DEF_LEN + 24,
        new_row_count,
    );

    let output_parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output_name = output_path
        .file_name()
        .ok_or_else(|| StoreError::RrdUnsupported("invalid resize output filename".into()))?
        .to_string_lossy();
    static RESIZE_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut temporary = None;
    for _ in 0..100 {
        let id = RESIZE_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = output_parent.join(format!(
            ".{output_name}.rondi-{}-{id}.resize.tmp",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let (temporary_path, mut output) = temporary.ok_or_else(|| {
        StoreError::RrdUnsupported("unable to allocate temporary resize filename".into())
    })?;

    let result = (|| -> Result<(), StoreError> {
        output.write_all(&header)?;
        let unknown_row = rrd_nan().to_le_bytes();
        let mut row = vec![0_u8; row_bytes];
        for (index, current_archive) in info.archives.iter().enumerate() {
            input.seek(SeekFrom::Start(current_archive.data_offset))?;
            if index != rra_index {
                let byte_count = current_archive
                    .rows
                    .checked_mul(row_bytes as u64)
                    .ok_or_else(|| StoreError::RrdUnsupported("RRA byte size overflows".into()))?;
                copy_exact(&mut input, &mut output, byte_count)?;
                continue;
            }

            match action {
                RrdResizeAction::Grow => {
                    let pointer = current_archive.current_row;
                    for output_row in 0..new_row_count {
                        if output_row > pointer && output_row <= pointer + row_count {
                            for value in row.chunks_exact_mut(VALUE_LEN) {
                                value.copy_from_slice(&unknown_row);
                            }
                        } else {
                            let old_row = if output_row <= pointer {
                                output_row
                            } else {
                                output_row - row_count
                            };
                            input.seek(SeekFrom::Start(
                                current_archive.data_offset + old_row * row_bytes as u64,
                            ))?;
                            input.read_exact(&mut row)?;
                        }
                        output.write_all(&row)?;
                    }
                }
                RrdResizeAction::Shrink => {
                    let old_rows = current_archive.rows;
                    let pointer = current_archive.current_row;
                    let remove_start = (pointer + 1) % old_rows;
                    let mut new_pointer = 0_u64;
                    let mut output_row = 0_u64;
                    for old_row in 0..old_rows {
                        let distance = (old_row + old_rows - remove_start) % old_rows;
                        if distance < row_count {
                            continue;
                        }
                        input.seek(SeekFrom::Start(
                            current_archive.data_offset + old_row * row_bytes as u64,
                        ))?;
                        input.read_exact(&mut row)?;
                        output.write_all(&row)?;
                        if old_row == pointer {
                            new_pointer = output_row;
                        }
                        output_row += 1;
                    }
                    if output_row != new_row_count {
                        return Err(StoreError::RrdFormat(
                            "resize row mapping produced an invalid row count".into(),
                        ));
                    }
                    let pointer_start = header_size - info.archives.len() * RRA_PTR_LEN;
                    put_u64(
                        &mut header,
                        pointer_start + rra_index * RRA_PTR_LEN,
                        new_pointer,
                    );
                }
            }
        }

        if action == RrdResizeAction::Shrink {
            let pointer_start = header_size - info.archives.len() * RRA_PTR_LEN;
            output.seek(SeekFrom::Start(pointer_start as u64))?;
            output.write_all(&header[pointer_start..])?;
            output.seek(SeekFrom::End(0))?;
        }
        output.sync_all()?;
        drop(output);
        std::fs::hard_link(&temporary_path, output_path)?;
        std::fs::remove_file(&temporary_path)?;
        std::fs::File::open(output_parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn copy_exact(input: &mut File, output: &mut File, mut byte_count: u64) -> Result<(), StoreError> {
    let mut buffer = [0_u8; 64 * 1024];
    while byte_count > 0 {
        let count = buffer.len().min(byte_count as usize);
        input.read_exact(&mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        byte_count -= count as u64;
    }
    Ok(())
}

/// Fetch data from an existing RRDtool file by path. The file is opened
/// without following a symlink and held under RRDtool-compatible shared
/// advisory locking for the duration of the read.
pub fn fetch_rrd_file(
    path: impl AsRef<Path>,
    consolidation: &str,
    start: i64,
    end: i64,
    resolution: u64,
) -> Result<RrdFetchResult, StoreError> {
    fetch_path(path.as_ref(), consolidation, start, end, resolution)
}

/// Inspect the bounded metadata header of a supported RRDtool file.
pub fn inspect_rrd_file(path: impl AsRef<Path>) -> Result<RrdInfo, StoreError> {
    inspect_path(path.as_ref())
}

/// Return RRDtool's first-row timestamp for an archive index.
pub fn first_rrd_time(path: impl AsRef<Path>, archive_index: usize) -> Result<i64, StoreError> {
    let info = inspect_path(path.as_ref())?;
    let archive = info.archives.get(archive_index).ok_or_else(|| {
        StoreError::RrdUnsupported(format!("invalid rraindex number: {archive_index}"))
    })?;
    let resolution = info
        .step
        .checked_mul(archive.pdp_per_row)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| rrd_error("RRD archive resolution overflows"))?;
    let aligned_last = info
        .last_update
        .checked_sub(info.last_update.rem_euclid(resolution))
        .ok_or_else(|| rrd_error("RRD first timestamp alignment overflows"))?;
    let retained = i64::try_from(archive.rows - 1)
        .ok()
        .and_then(|rows| rows.checked_mul(resolution))
        .ok_or_else(|| rrd_error("RRD archive range overflows"))?;
    aligned_last
        .checked_sub(retained)
        .ok_or_else(|| rrd_error("RRD first timestamp overflows"))
}

/// Dump a supported v3 RRDtool file in the upstream XML format.
pub fn dump_rrd_file(path: impl AsRef<Path>) -> Result<String, StoreError> {
    dump_rrd_file_with_header(path, RrdDumpHeader::Dtd)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RrdDumpHeader {
    None,
    Dtd,
    Xsd,
}

pub fn dump_rrd_file_with_header(
    path: impl AsRef<Path>,
    header: RrdDumpHeader,
) -> Result<String, StoreError> {
    let mut file = RrdFileLock::shared(open_rrd_read(path.as_ref())?)?;
    let info = read_info(&mut file)?;
    if info.data_sources.iter().any(|ds| {
        !matches!(
            ds.kind.as_str(),
            "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
        )
    }) || info.archives.iter().any(|archive| {
        !matches!(
            archive.consolidation.as_str(),
            "AVERAGE" | "MIN" | "MAX" | "LAST"
        )
    }) {
        return Err(StoreError::RrdUnsupported(
            "rrdtool dump currently supports GAUGE/COUNTER/DERIVE/ABSOLUTE/DCOUNTER/DDERIVE and AVERAGE/MIN/MAX/LAST only".into(),
        ));
    }
    let mut out = String::new();
    match header {
        RrdDumpHeader::None => out.push_str("<!-- Round Robin Database Dump -->\n<rrd>\n"),
        RrdDumpHeader::Dtd => out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE rrd SYSTEM \"https://oss.oetiker.ch/rrdtool/rrdtool.dtd\">\n<!-- Round Robin Database Dump -->\n<rrd>\n"),
        RrdDumpHeader::Xsd => out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!-- Round Robin Database Dump -->\n<rrd xmlns=\"https://oss.oetiker.ch/rrdtool/rrdtool-dump.xml\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n\txsi:schemaLocation=\"https://oss.oetiker.ch/rrdtool/rrdtool-dump.xml https://oss.oetiker.ch/rrdtool/rrdtool-dump.xsd\">\n"),
    }
    writeln!(out, "\t<version>{}</version>", info.version).unwrap();
    writeln!(out, "\t<step>{}</step> <!-- Seconds -->", info.step).unwrap();
    writeln!(
        out,
        "\t<lastupdate>{}</lastupdate> <!-- {} -->\n",
        info.last_update,
        rrd_local_timestamp(info.last_update)?
    )
    .unwrap();
    for source in &info.data_sources {
        let name = xml_escape_text(&source.name);
        let kind = xml_escape_text(&source.kind);
        let last_value = xml_escape_text(&source.last_value);
        out.push_str("\t<ds>\n");
        writeln!(out, "\t\t<name> {} </name>", name).unwrap();
        writeln!(out, "\t\t<type> {} </type>", kind).unwrap();
        writeln!(
            out,
            "\t\t<minimal_heartbeat>{}</minimal_heartbeat>",
            source.heartbeat
        )
        .unwrap();
        writeln!(
            out,
            "\t\t<min>{}</min>",
            format_rrd_optional(source.minimum)
        )
        .unwrap();
        writeln!(
            out,
            "\t\t<max>{}</max>",
            format_rrd_optional(source.maximum)
        )
        .unwrap();
        out.push_str("\n\t\t<!-- PDP Status -->\n");
        writeln!(out, "\t\t<last_ds>{}</last_ds>", last_value).unwrap();
        writeln!(
            out,
            "\t\t<value>{}</value>",
            format_rrd_value(source.pdp_value)
        )
        .unwrap();
        writeln!(
            out,
            "\t\t<unknown_sec> {} </unknown_sec>",
            source.unknown_seconds
        )
        .unwrap();
        out.push_str("\t</ds>\n\n");
    }
    out.push_str("\t<!-- Round Robin Archives -->\n");
    let step_i64 = i64::try_from(info.step).map_err(|_| rrd_error("RRD step overflows"))?;
    for archive in &info.archives {
        let resolution = step_i64
            .checked_mul(
                i64::try_from(archive.pdp_per_row)
                    .map_err(|_| rrd_error("RRA resolution overflows"))?,
            )
            .ok_or_else(|| rrd_error("RRA resolution overflows"))?;
        let aligned_last = info
            .last_update
            .checked_sub(info.last_update.rem_euclid(resolution))
            .ok_or_else(|| rrd_error("RRD last timestamp alignment overflows"))?;
        writeln!(out, "\t<rra>\n\t\t<cf>{}</cf>", archive.consolidation).unwrap();
        writeln!(
            out,
            "\t\t<pdp_per_row>{}</pdp_per_row> <!-- {} seconds -->\n",
            archive.pdp_per_row, resolution
        )
        .unwrap();
        out.push_str("\t\t<params>\n");
        writeln!(out, "\t\t<xff>{}</xff>", format_rrd_float(archive.xff)).unwrap();
        out.push_str("\t\t</params>\n\t\t<cdp_prep>\n");
        for prep in &archive.cdp_prep {
            out.push_str("\t\t\t<ds>\n");
            writeln!(
                out,
                "\t\t\t<primary_value>{}</primary_value>",
                format_rrd_value(prep.primary_value)
            )
            .unwrap();
            writeln!(
                out,
                "\t\t\t<secondary_value>{}</secondary_value>",
                format_rrd_value(prep.secondary_value)
            )
            .unwrap();
            writeln!(out, "\t\t\t<value>{}</value>", format_rrd_value(prep.value)).unwrap();
            writeln!(
                out,
                "\t\t\t<unknown_datapoints>{}</unknown_datapoints>",
                prep.unknown_datapoints
            )
            .unwrap();
            out.push_str("\t\t\t</ds>\n");
        }
        out.push_str("\t\t</cdp_prep>\n\t\t<database>\n");
        let rows =
            usize::try_from(archive.rows).map_err(|_| rrd_error("RRA row count overflows"))?;
        for row in 0..rows {
            let ring_row = (archive.current_row + 1 + row as u64) % archive.rows;
            let offset = archive
                .data_offset
                .checked_add(
                    ring_row
                        .checked_mul(info.data_sources.len() as u64 * VALUE_LEN as u64)
                        .ok_or_else(|| rrd_error("RRD row offset overflows"))?,
                )
                .ok_or_else(|| rrd_error("RRD row offset overflows"))?;
            file.seek(SeekFrom::Start(offset))?;
            let timestamp = aligned_last
                .checked_add(
                    (row as i64 - (rows as i64 - 1))
                        .checked_mul(resolution)
                        .ok_or_else(|| rrd_error("RRD dump timestamp overflows"))?,
                )
                .ok_or_else(|| rrd_error("RRD dump timestamp overflows"))?;
            write!(
                out,
                "\t\t\t<!-- {} / {} --> <row>",
                rrd_local_timestamp(timestamp)?,
                timestamp
            )
            .unwrap();
            for _ in &info.data_sources {
                let mut bytes = [0; VALUE_LEN];
                file.read_exact(&mut bytes)?;
                let value = f64::from_le_bytes(bytes);
                write!(out, "<v>{}</v>", format_rrd_value(value)).unwrap();
            }
            out.push_str("</row>\n");
        }
        out.push_str("\t\t</database>\n\t</rra>\n");
    }
    out.push_str("</rrd>\n");
    Ok(out)
}

/// Restore the basic RRDtool XML subset emitted by [`dump_rrd_file_with_header`].
/// The file is assembled at a sibling temporary path and linked/renamed into
/// place only after its headers, prep state, pointers, and archive rows are
/// complete and synced.
pub fn restore_rrd_file(
    xml: &str,
    path: impl AsRef<Path>,
    force_overwrite: bool,
    range_check: bool,
) -> Result<(), StoreError> {
    use roxmltree::Node;

    fn child_text(node: Node<'_, '_>, name: &str) -> Result<String, StoreError> {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
            .and_then(|child| child.text())
            .map(str::trim)
            .map(str::to_owned)
            .ok_or_else(|| StoreError::RrdUnsupported(format!("RRD XML is missing <{name}>")))
    }

    fn single_text_child(node: Node<'_, '_>, name: &str) -> Result<String, StoreError> {
        let element = child(node, name)?;
        let mut text_nodes = element.children().filter(|child| child.is_text());
        let Some(text) = text_nodes.next() else {
            return Err(StoreError::RrdUnsupported(format!(
                "RRD XML <{name}> must contain one text node"
            )));
        };
        if element.children().any(|child| child.is_element()) || text_nodes.next().is_some() {
            return Err(StoreError::RrdUnsupported(format!(
                "RRD XML <{name}> must contain one text node"
            )));
        }
        Ok(text.text().unwrap_or_default().trim().to_owned())
    }

    fn child<'a, 'input>(
        node: Node<'a, 'input>,
        name: &str,
    ) -> Result<Node<'a, 'input>, StoreError> {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
            .ok_or_else(|| StoreError::RrdUnsupported(format!("RRD XML is missing <{name}>")))
    }

    fn parse_f64(text: &str) -> Result<f64, StoreError> {
        let lower = text.to_ascii_lowercase();
        let value = match lower.as_str() {
            "nan" | "+nan" | "-nan" => rrd_nan(),
            "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
            "-inf" | "-infinity" => f64::NEG_INFINITY,
            _ => text.parse::<f64>().map_err(|_| {
                StoreError::RrdUnsupported(format!("invalid RRD XML number: {text}"))
            })?,
        };
        Ok(value)
    }

    // RRDtool's own default dump includes an external DTD declaration. Restore
    // reads the document structure without needing that DTD, and the parser
    // deliberately does not resolve external entities or fetch network data.
    let xml_without_doctype = xml
        .lines()
        .filter(|line| !line.trim_start().starts_with("<!DOCTYPE"))
        .collect::<Vec<_>>()
        .join("\n");
    let document = roxmltree::Document::parse(&xml_without_doctype)
        .map_err(|error| StoreError::RrdUnsupported(format!("invalid RRD XML: {error}")))?;
    let root = document.root_element();
    if root.tag_name().name() != "rrd" {
        return Err(StoreError::RrdUnsupported(
            "RRD XML root element must be <rrd>".into(),
        ));
    }
    let version = child_text(root, "version")?;
    if !matches!(version.as_str(), "0003" | "0005") {
        return Err(StoreError::RrdUnsupported(format!(
            "RRD XML format version {version} is unsupported"
        )));
    }
    let step = child_text(root, "step")?
        .parse::<u64>()
        .map_err(|_| StoreError::RrdUnsupported("invalid RRD XML step".into()))?;
    let last_update = child_text(root, "lastupdate")?
        .parse::<i64>()
        .map_err(|_| StoreError::RrdUnsupported("invalid RRD XML lastupdate".into()))?;
    let ds_nodes = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "ds")
        .collect::<Vec<_>>();
    let rra_nodes = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "rra")
        .collect::<Vec<_>>();
    if ds_nodes.is_empty() || rra_nodes.is_empty() {
        return Err(StoreError::RrdUnsupported(
            "RRD XML requires at least one data source and archive".into(),
        ));
    }

    let mut source_definitions = Vec::with_capacity(ds_nodes.len());
    let mut source_bounds = Vec::with_capacity(ds_nodes.len());
    for ds in &ds_nodes {
        let name = single_text_child(*ds, "name")?;
        let kind = child_text(*ds, "type")?;
        if version == "0003" && matches!(kind.as_str(), "DCOUNTER" | "DDERIVE") {
            return Err(StoreError::RrdUnsupported(
                "RRD format version 0003 cannot store DCOUNTER or DDERIVE".into(),
            ));
        }
        let heartbeat = child_text(*ds, "minimal_heartbeat")?;
        let minimum = child_text(*ds, "min")?;
        let maximum = child_text(*ds, "max")?;
        let min_value = if minimum == "U" {
            None
        } else {
            Some(parse_f64(&minimum)?).filter(|value| !value.is_nan())
        };
        let max_value = if maximum == "U" {
            None
        } else {
            Some(parse_f64(&maximum)?).filter(|value| !value.is_nan())
        };
        let minimum = min_value.map_or_else(|| "U".to_owned(), |value| value.to_string());
        let maximum = max_value.map_or_else(|| "U".to_owned(), |value| value.to_string());
        source_definitions.push(format!("DS:{name}:{kind}:{heartbeat}:{minimum}:{maximum}"));
        source_bounds.push((min_value, max_value));
    }

    let mut archive_definitions = Vec::with_capacity(rra_nodes.len());
    let mut archive_rows = Vec::with_capacity(rra_nodes.len());
    for rra in &rra_nodes {
        let cf = child_text(*rra, "cf")?;
        let pdp_per_row = child_text(*rra, "pdp_per_row")?
            .parse::<u64>()
            .map_err(|_| StoreError::RrdUnsupported("invalid RRD XML pdp_per_row".into()))?;
        let xff = parse_f64(&child_text(child(*rra, "params")?, "xff")?)?;
        let database = child(*rra, "database")?;
        let rows = database
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "row")
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Err(StoreError::RrdUnsupported(
                "RRD XML archive database must contain rows".into(),
            ));
        }
        archive_definitions.push(format!("RRA:{cf}:{xff}:{pdp_per_row}:{}", rows.len()));
        archive_rows.push(rows);
    }

    let destination = path.as_ref();
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = destination
        .file_name()
        .ok_or_else(|| StoreError::RrdUnsupported("invalid restore output filename".into()))?
        .to_string_lossy();
    static RESTORE_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut temporary = None;
    for _ in 0..100 {
        let id = RESTORE_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{filename}.rondi-{}-{id}.restore.tmp",
            std::process::id()
        ));
        if !candidate.exists() {
            temporary = Some(candidate);
            break;
        }
    }
    let temporary = temporary.ok_or_else(|| {
        StoreError::RrdUnsupported("unable to allocate temporary restore filename".into())
    })?;

    let result = (|| -> Result<(), StoreError> {
        create_rrd_file(
            &temporary,
            last_update,
            step,
            &source_definitions,
            &archive_definitions,
            true,
        )?;
        let info = inspect_path(&temporary)?;
        let mut restore_options = std::fs::OpenOptions::new();
        restore_options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            restore_options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = restore_options.open(&temporary)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(StoreError::RrdUnsupported(
                "restore temporary path is not a regular file".into(),
            ));
        }
        let ds_start = STAT_HEAD_LEN;
        let rra_start = ds_start + info.data_sources.len() * DS_DEF_LEN;
        let live_start = rra_start + info.archives.len() * RRA_DEF_LEN;
        let pdp_start = live_start + LIVE_HEAD_LEN;
        let cdp_start = pdp_start + info.data_sources.len() * PDP_PREP_LEN;
        let pointer_start = info.header_size - info.archives.len() * RRA_PTR_LEN;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(version.as_bytes())?;
        file.write_all(&[0])?;
        file.seek(SeekFrom::Start(live_start as u64))?;
        file.write_all(&last_update.to_le_bytes())?;

        for (index, ds) in ds_nodes.iter().enumerate() {
            let offset = pdp_start + index * PDP_PREP_LEN;
            let last_ds = single_text_child(*ds, "last_ds")?;
            if last_ds.len() >= 30 {
                return Err(StoreError::RrdUnsupported(
                    "RRD XML last_ds value is too long".into(),
                ));
            }
            let mut last_ds_bytes = [0_u8; 30];
            last_ds_bytes[..last_ds.len()].copy_from_slice(last_ds.as_bytes());
            file.seek(SeekFrom::Start(offset as u64))?;
            file.write_all(&last_ds_bytes)?;
            let unknown_seconds = child_text(*ds, "unknown_sec")?
                .parse::<u64>()
                .map_err(|_| StoreError::RrdUnsupported("invalid RRD XML unknown_sec".into()))?;
            file.seek(SeekFrom::Start((offset + 32) as u64))?;
            file.write_all(&unknown_seconds.to_le_bytes())?;
            let value = parse_f64(&child_text(*ds, "value")?)?;
            file.seek(SeekFrom::Start((offset + 40) as u64))?;
            file.write_all(&value.to_le_bytes())?;
        }

        for (rra_index, rra) in rra_nodes.iter().enumerate() {
            let cdp = child(*rra, "cdp_prep")?
                .children()
                .filter(|node| node.is_element() && node.tag_name().name() == "ds")
                .collect::<Vec<_>>();
            if cdp.len() != ds_nodes.len() {
                return Err(StoreError::RrdUnsupported(
                    "RRD XML cdp_prep data-source count does not match".into(),
                ));
            }
            for (ds_index, prep) in cdp.iter().enumerate() {
                let offset = cdp_start + (rra_index * ds_nodes.len() + ds_index) * CDP_PREP_LEN;
                for (field, relative_offset) in [
                    ("value", 0_usize),
                    ("primary_value", 64),
                    ("secondary_value", 72),
                ] {
                    let value = parse_f64(&child_text(*prep, field)?)?;
                    file.seek(SeekFrom::Start((offset + relative_offset) as u64))?;
                    file.write_all(&value.to_le_bytes())?;
                }
                let unknown = child_text(*prep, "unknown_datapoints")?
                    .parse::<u64>()
                    .map_err(|_| {
                        StoreError::RrdUnsupported("invalid RRD XML unknown_datapoints".into())
                    })?;
                file.seek(SeekFrom::Start((offset + 8) as u64))?;
                file.write_all(&unknown.to_le_bytes())?;
            }

            let archive = &info.archives[rra_index];
            let pointer = archive.rows - 1;
            file.seek(SeekFrom::Start(
                (pointer_start + rra_index * RRA_PTR_LEN) as u64,
            ))?;
            file.write_all(&pointer.to_le_bytes())?;
            if archive_rows[rra_index].len() as u64 != archive.rows {
                return Err(StoreError::RrdUnsupported(
                    "RRD XML archive row count changed during restore".into(),
                ));
            }
            for (row_index, row) in archive_rows[rra_index].iter().enumerate() {
                let values = row
                    .children()
                    .filter(|node| node.is_element() && node.tag_name().name() == "v")
                    .collect::<Vec<_>>();
                if values.len() != ds_nodes.len() {
                    return Err(StoreError::RrdUnsupported(
                        "RRD XML row data-source count does not match".into(),
                    ));
                }
                for (ds_index, value_node) in values.iter().enumerate() {
                    let text = value_node.text().unwrap_or_default().trim();
                    let mut value = parse_f64(text)?;
                    if range_check
                        && ((source_bounds[ds_index]
                            .0
                            .is_some_and(|minimum| value < minimum))
                            || source_bounds[ds_index]
                                .1
                                .is_some_and(|maximum| value > maximum))
                    {
                        value = rrd_nan();
                    }
                    let offset = archive.data_offset
                        + ((row_index * ds_nodes.len() + ds_index) * VALUE_LEN) as u64;
                    file.seek(SeekFrom::Start(offset))?;
                    file.write_all(&value.to_le_bytes())?;
                }
            }
        }
        file.sync_all()?;
        drop(file);
        if force_overwrite {
            std::fs::rename(&temporary, destination)?;
        } else {
            std::fs::hard_link(&temporary, destination)?;
            std::fs::remove_file(&temporary)?;
        }
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn format_rrd_value(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else {
        format_rrd_float(value)
    }
}

fn format_rrd_optional(value: Option<f64>) -> String {
    value.map_or_else(|| "NaN".to_owned(), format_rrd_float)
}

fn format_rrd_float(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-inf"
        } else {
            "inf"
        }
        .to_owned();
    }
    let formatted = format!("{value:.10e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("scientific format has exponent");
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    format!("{mantissa}e{exponent:+03}")
}

fn rrd_local_timestamp(timestamp: i64) -> Result<String, StoreError> {
    let timestamp =
        libc::time_t::try_from(timestamp).map_err(|_| rrd_error("timestamp out of range"))?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes `local` on success and does not retain either pointer.
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return Err(rrd_error("timestamp cannot be represented in local time"));
    }
    // SAFETY: localtime_r returned a non-null pointer and initialized the structure.
    let local = unsafe { local.assume_init() };
    let format = b"%Y-%m-%d %H:%M:%S %z\0";
    let mut output = [0 as libc::c_char; 64];
    // SAFETY: pointers refer to valid NUL-terminated format and initialized tm; output is writable.
    let len = unsafe {
        libc::strftime(
            output.as_mut_ptr(),
            output.len(),
            format.as_ptr().cast(),
            &local,
        )
    };
    if len == 0 {
        return Err(rrd_error("timestamp formatting failed"));
    }
    // SAFETY: strftime wrote `len` bytes followed by NUL into the fixed buffer.
    let bytes = unsafe { std::slice::from_raw_parts(output.as_ptr().cast::<u8>(), len) };
    String::from_utf8(bytes.to_vec()).map_err(|_| rrd_error("local timestamp is not UTF-8"))
}

fn read_info(file: &mut File) -> Result<RrdInfo, StoreError> {
    let file_len = usize::try_from(file.metadata()?.len())
        .map_err(|_| rrd_error("RRD file length exceeds host size"))?;
    let mut prefix = vec![0; STAT_HEAD_LEN];
    file.read_exact(&mut prefix)
        .map_err(|_| rrd_error("truncated RRD file"))?;
    let header_len = header_length(&prefix)?;
    if header_len > MAX_HEADER_LEN {
        return Err(StoreError::RrdUnsupported(
            "RRD metadata header exceeds the 64 MiB inspection limit".into(),
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut header = vec![0; header_len];
    file.read_exact(&mut header)
        .map_err(|_| rrd_error("truncated RRD metadata header"))?;
    inspect_parts(&header, file_len)
}

fn inspect_parts(bytes: &[u8], file_len: usize) -> Result<RrdInfo, StoreError> {
    if !cfg!(target_pointer_width = "64") || cfg!(target_endian = "big") {
        return Err(StoreError::RrdUnsupported(
            "RRD v3 probe currently requires a 64-bit little-endian target".into(),
        ));
    }
    require(bytes, 0, STAT_HEAD_LEN)?;
    if &bytes[0..4] != b"RRD\0" {
        return Err(StoreError::RrdFormat("invalid RRD cookie".into()));
    }
    let version = fixed_string(bytes, 4, 5)?;
    if !matches!(version.as_str(), "0003" | "0004" | "0005") {
        return Err(StoreError::RrdUnsupported(format!(
            "RRD format version {version} is not supported by the inspector"
        )));
    }
    if f64_at(bytes, 16)? != FLOAT_COOKIE {
        return Err(StoreError::RrdFormat(
            "RRD float cookie does not match the host representation".into(),
        ));
    }

    let ds_count = usize_at(bytes, 24)?;
    let rra_count = usize_at(bytes, 32)?;
    let step = u64_at(bytes, 40)?;
    if ds_count == 0 || rra_count == 0 || step == 0 {
        return Err(StoreError::RrdFormat(
            "RRD requires data sources, archives, and a positive step".into(),
        ));
    }
    let ds_start = STAT_HEAD_LEN;
    let rra_start = checked_add(ds_start, checked_mul(ds_count, DS_DEF_LEN)?)?;
    let live_start = checked_add(rra_start, checked_mul(rra_count, RRA_DEF_LEN)?)?;
    let last_update = i64_at(bytes, live_start)?;
    let last_update_usec = u64_at(bytes, checked_add(live_start, 8)?)?;
    if last_update_usec >= 1_000_000 {
        return Err(StoreError::RrdFormat(
            "RRD last update microseconds are out of range".into(),
        ));
    }
    let pdp_start = checked_add(live_start, LIVE_HEAD_LEN)?;
    let cdp_start = checked_add(pdp_start, checked_mul(ds_count, PDP_PREP_LEN)?)?;
    let pointer_start = checked_add(
        cdp_start,
        checked_mul(checked_mul(ds_count, rra_count)?, CDP_PREP_LEN)?,
    )?;
    let data_start = checked_add(pointer_start, checked_mul(rra_count, RRA_PTR_LEN)?)?;
    require(bytes, data_start, 0)?;

    let mut data_sources = Vec::with_capacity(ds_count);
    for index in 0..ds_count {
        let start = checked_add(ds_start, checked_mul(index, DS_DEF_LEN)?)?;
        let name = fixed_string(bytes, start, 20)?;
        let kind = fixed_string(bytes, checked_add(start, 20)?, 20)?;
        if name.is_empty() || kind.is_empty() {
            return Err(StoreError::RrdFormat(
                "RRD data source definition has an empty name or type".into(),
            ));
        }
        let heartbeat = u64_at(bytes, checked_add(start, 40)?)?;
        // NaN means unbounded; infinite bounds are kept as RRDtool prints them.
        let minimum = Some(f64_at(bytes, checked_add(start, 48)?)?).filter(|v| !v.is_nan());
        let maximum = Some(f64_at(bytes, checked_add(start, 56)?)?).filter(|v| !v.is_nan());
        data_sources.push(RrdDataSourceInfo {
            name,
            kind,
            heartbeat,
            minimum,
            maximum,
            last_value: fixed_string(
                bytes,
                checked_add(pdp_start, checked_mul(index, PDP_PREP_LEN)?)?,
                30,
            )?,
            pdp_value: f64_at(
                bytes,
                checked_add(
                    checked_add(pdp_start, checked_mul(index, PDP_PREP_LEN)?)?,
                    40,
                )?,
            )?,
            unknown_seconds: u64_at(
                bytes,
                checked_add(
                    checked_add(pdp_start, checked_mul(index, PDP_PREP_LEN)?)?,
                    32,
                )?,
            )?,
        });
    }

    let mut archives = Vec::with_capacity(rra_count);
    let mut archive_data_start = data_start;
    for index in 0..rra_count {
        let start = checked_add(rra_start, checked_mul(index, RRA_DEF_LEN)?)?;
        let consolidation = fixed_string(bytes, start, 20)?;
        let rows = u64_at(bytes, checked_add(start, 24)?)?;
        let pdp_per_row = u64_at(bytes, checked_add(start, 32)?)?;
        let xff = f64_at(bytes, checked_add(start, 40)?)?;
        let current_row = u64_at(
            bytes,
            checked_add(pointer_start, checked_mul(index, RRA_PTR_LEN)?)?,
        )?;
        if consolidation.is_empty()
            || rows == 0
            || pdp_per_row == 0
            || !xff.is_finite()
            || !(0.0..=1.0).contains(&xff)
            || current_row >= rows
        {
            return Err(StoreError::RrdFormat(format!(
                "RRD archive {index} has invalid metadata"
            )));
        }
        let archive_bytes = checked_mul(
            checked_mul(
                usize::try_from(rows).map_err(|_| rrd_error("row count overflows"))?,
                ds_count,
            )?,
            VALUE_LEN,
        )?;
        let data_offset = archive_data_start;
        archive_data_start = checked_add(archive_data_start, archive_bytes)?;
        let mut cdp_prep = Vec::with_capacity(ds_count);
        for ds_index in 0..ds_count {
            let prep_offset = checked_add(
                cdp_start,
                checked_mul(
                    checked_add(checked_mul(index, ds_count)?, ds_index)?,
                    CDP_PREP_LEN,
                )?,
            )?;
            cdp_prep.push(RrdCdpPrepInfo {
                value: f64_at(bytes, prep_offset)?,
                unknown_datapoints: u64_at(bytes, checked_add(prep_offset, 8)?)?,
                primary_value: f64_at(bytes, checked_add(prep_offset, 64)?)?,
                secondary_value: f64_at(bytes, checked_add(prep_offset, 72)?)?,
            });
        }
        archives.push(RrdArchiveInfo {
            consolidation,
            rows,
            pdp_per_row,
            xff,
            current_row,
            cdp_prep,
            data_offset: data_offset as u64,
        });
    }
    if archive_data_start > file_len {
        return Err(StoreError::RrdFormat(
            "RRD file is shorter than its declared archive layout".into(),
        ));
    }

    Ok(RrdInfo {
        version,
        step,
        last_update,
        last_update_usec,
        header_size: data_start,
        data_sources,
        archives,
    })
}

fn header_length(stat_head: &[u8]) -> Result<usize, StoreError> {
    require(stat_head, 0, STAT_HEAD_LEN)?;
    if &stat_head[0..4] != b"RRD\0" {
        return Err(StoreError::RrdFormat("invalid RRD cookie".into()));
    }
    let version = fixed_string(stat_head, 4, 5)?;
    if !matches!(version.as_str(), "0003" | "0004" | "0005") {
        return Err(StoreError::RrdUnsupported(format!(
            "RRD format version {version} is not supported by the inspector"
        )));
    }
    let ds_count = usize_at(stat_head, 24)?;
    let rra_count = usize_at(stat_head, 32)?;
    if ds_count == 0 || rra_count == 0 {
        return Err(StoreError::RrdFormat(
            "RRD requires data sources and archives".into(),
        ));
    }
    let mut len = checked_add(STAT_HEAD_LEN, checked_mul(ds_count, DS_DEF_LEN)?)?;
    len = checked_add(len, checked_mul(rra_count, RRA_DEF_LEN)?)?;
    len = checked_add(len, LIVE_HEAD_LEN)?;
    len = checked_add(len, checked_mul(ds_count, PDP_PREP_LEN)?)?;
    len = checked_add(
        len,
        checked_mul(checked_mul(ds_count, rra_count)?, CDP_PREP_LEN)?,
    )?;
    checked_add(len, checked_mul(rra_count, RRA_PTR_LEN)?)
}

fn choose_archive(
    info: &RrdInfo,
    requested_cf: &str,
    start: i64,
    end: i64,
    requested_step: u64,
) -> Result<usize, StoreError> {
    let is_basic_cf = |name: &str| matches!(name, "AVERAGE" | "MIN" | "MAX" | "LAST");
    let mut best_full: Option<(usize, u128)> = None;
    let mut best_partial: Option<(usize, i128, u128)> = None;
    for (index, archive) in info.archives.iter().enumerate() {
        let direct_match = archive.consolidation == requested_cf;
        let interval_one_match = archive.pdp_per_row == 1
            && is_basic_cf(requested_cf)
            && is_basic_cf(&archive.consolidation);
        if !direct_match && !interval_one_match {
            continue;
        }
        let archive_step = info
            .step
            .checked_mul(archive.pdp_per_row)
            .ok_or_else(|| rrd_error("RRD archive step overflows"))?;
        let archive_step_i64 =
            i64::try_from(archive_step).map_err(|_| rrd_error("RRD archive step overflows"))?;
        let archive_end = info
            .last_update
            .checked_sub(info.last_update.rem_euclid(archive_step_i64))
            .ok_or_else(|| rrd_error("RRD archive end overflows"))?;
        let span = archive_step_i64
            .checked_mul(
                i64::try_from(archive.rows).map_err(|_| rrd_error("RRD row count overflows"))?,
            )
            .ok_or_else(|| rrd_error("RRD archive time range overflows"))?;
        let archive_start = archive_end
            .checked_sub(span)
            .ok_or_else(|| rrd_error("RRD archive time range overflows"))?;
        let step_diff = u128::from(archive_step.abs_diff(requested_step));
        if archive_start <= start {
            if best_full.is_none_or(|(_, best_diff)| step_diff < best_diff) {
                best_full = Some((index, step_diff));
            }
        } else {
            let match_span = i128::from(end)
                - i128::from(start)
                - (i128::from(archive_start) - i128::from(start));
            if best_partial.is_none_or(|(_, best_match, best_diff)| {
                match_span > best_match || (match_span == best_match && step_diff < best_diff)
            }) {
                best_partial = Some((index, match_span, step_diff));
            }
        }
    }
    best_full
        .map(|(index, _)| index)
        .or_else(|| best_partial.map(|(index, _, _)| index))
        .ok_or_else(|| {
            StoreError::RrdUnsupported(format!(
                "RRD has no archive matching consolidation function {requested_cf}"
            ))
        })
}

/// A descriptor holding RRDtool's whole-file `fcntl` lock.
///
/// `fcntl` record locks belong to the process, so they never exclude another
/// thread, and closing any descriptor for the file drops every lock the
/// process holds on it. Locked access is therefore also serialized per inode
/// inside the process, and the descriptor is closed before the next thread may
/// open its own lock on that inode.
struct RrdFileLock {
    file: Option<File>,
    #[cfg(unix)]
    key: (u64, u64),
}

#[cfg(unix)]
static LOCKED_RRD_FILES: std::sync::Mutex<std::collections::BTreeSet<(u64, u64)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());
#[cfg(unix)]
static LOCKED_RRD_FILE_RELEASED: std::sync::Condvar = std::sync::Condvar::new();

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RrdLockMode {
    Try,
    Block,
    None,
}

/// RRDtool reads `$RRD_LOCKING` on every open and defaults to a single
/// non-blocking attempt, so a held lock fails with "could not lock RRD".
#[cfg(unix)]
fn rrd_lock_mode() -> RrdLockMode {
    match std::env::var_os("RRD_LOCKING") {
        None => RrdLockMode::Try,
        Some(value) => match value.to_str() {
            Some("" | "try") => RrdLockMode::Try,
            Some("block") => RrdLockMode::Block,
            Some("none") => RrdLockMode::None,
            _ => {
                eprintln!(
                    "unsupported locking mode '{}' in $RRD_LOCKING; assuming 'try'",
                    value.to_string_lossy()
                );
                RrdLockMode::Try
            }
        },
    }
}

impl RrdFileLock {
    #[cfg(unix)]
    fn shared(file: File) -> Result<Self, StoreError> {
        Self::lock(file, false)
    }

    #[cfg(unix)]
    fn exclusive(file: File) -> Result<Self, StoreError> {
        Self::lock(file, true)
    }

    #[cfg(unix)]
    fn lock(file: File, exclusive: bool) -> Result<Self, StoreError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let key = (metadata.dev(), metadata.ino());
        {
            let mut held = LOCKED_RRD_FILES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while held.contains(&key) {
                held = LOCKED_RRD_FILE_RELEASED
                    .wait(held)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            held.insert(key);
        }
        // From here on, Drop closes the descriptor and releases the inode.
        let guard = Self {
            file: Some(file),
            key,
        };
        let operation = match rrd_lock_mode() {
            RrdLockMode::None => return Ok(guard),
            RrdLockMode::Try => libc::F_SETLK,
            RrdLockMode::Block => libc::F_SETLKW,
        };
        let mut lock = libc::flock {
            l_type: if exclusive {
                libc::F_WRLCK
            } else {
                libc::F_RDLCK
            } as _,
            l_whence: libc::SEEK_SET as _,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let fd = guard.file.as_ref().map_or(-1, AsRawFd::as_raw_fd);
        // SAFETY: `lock` is a valid flock structure and the descriptor stays
        // open until the guard is dropped.
        let result = unsafe { libc::fcntl(fd, operation, &mut lock) };
        if result == -1 {
            return Err(StoreError::RrdLocked);
        }
        Ok(guard)
    }

    #[cfg(not(unix))]
    fn shared(_file: File) -> Result<Self, StoreError> {
        Err(StoreError::RrdUnsupported(
            "RRDtool-compatible advisory locking is unavailable on this platform".into(),
        ))
    }

    #[cfg(not(unix))]
    fn exclusive(_file: File) -> Result<Self, StoreError> {
        Err(StoreError::RrdUnsupported(
            "RRDtool-compatible advisory locking is unavailable on this platform".into(),
        ))
    }
}

impl std::ops::Deref for RrdFileLock {
    type Target = File;

    fn deref(&self) -> &File {
        self.file
            .as_ref()
            .expect("locked RRD file is open until drop")
    }
}

impl std::ops::DerefMut for RrdFileLock {
    fn deref_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("locked RRD file is open until drop")
    }
}

impl Drop for RrdFileLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the fcntl lock. It must happen
        // before another thread can lock the inode, or this close would drop
        // that thread's lock as well.
        drop(self.file.take());
        #[cfg(unix)]
        {
            let mut held = LOCKED_RRD_FILES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held.remove(&self.key);
            LOCKED_RRD_FILE_RELEASED.notify_all();
        }
    }
}

fn fixed_string(bytes: &[u8], offset: usize, length: usize) -> Result<String, StoreError> {
    require(bytes, offset, length)?;
    let field = &bytes[offset..offset + length];
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    let value = std::str::from_utf8(&field[..end])
        .map_err(|_| StoreError::RrdFormat("RRD string field is not UTF-8/ASCII".into()))?;
    Ok(value.to_owned())
}

fn xml_escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// RRDtool opens operator-supplied .rrd paths normally, following symlinks.
// Keep that compatibility behavior separate from the stricter native-store
// helpers, which intentionally refuse symlinked database files.
fn open_rrd_read(path: &Path) -> Result<File, StoreError> {
    Ok(File::open(path)?)
}

fn open_rrd_write(path: &Path) -> Result<File, StoreError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    Ok(options.open(path)?)
}

fn usize_at(bytes: &[u8], offset: usize) -> Result<usize, StoreError> {
    usize::try_from(u64_at(bytes, offset)?).map_err(|_| rrd_error("RRD count exceeds host size"))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, StoreError> {
    require(bytes, offset, 8)?;
    Ok(u64::from_le_bytes(
        bytes[offset..offset + 8].try_into().unwrap(),
    ))
}

fn i64_at(bytes: &[u8], offset: usize) -> Result<i64, StoreError> {
    require(bytes, offset, 8)?;
    Ok(i64::from_le_bytes(
        bytes[offset..offset + 8].try_into().unwrap(),
    ))
}

fn f64_at(bytes: &[u8], offset: usize) -> Result<f64, StoreError> {
    require(bytes, offset, 8)?;
    Ok(f64::from_le_bytes(
        bytes[offset..offset + 8].try_into().unwrap(),
    ))
}

fn require(bytes: &[u8], offset: usize, length: usize) -> Result<(), StoreError> {
    let end = checked_add(offset, length)?;
    if end > bytes.len() {
        return Err(StoreError::RrdFormat("truncated RRD file".into()));
    }
    Ok(())
}

fn checked_add(left: usize, right: usize) -> Result<usize, StoreError> {
    left.checked_add(right)
        .ok_or_else(|| rrd_error("RRD offset overflows"))
}

fn checked_mul(left: usize, right: usize) -> Result<usize, StoreError> {
    left.checked_mul(right)
        .ok_or_else(|| rrd_error("RRD length overflows"))
}

fn rrd_error(message: &str) -> StoreError {
    StoreError::RrdFormat(message.into())
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;
    use std::process::Command;

    fn rrdtool_1110_available() -> bool {
        Command::new("rrdtool")
            .arg("--version")
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
            })
    }

    #[test]
    fn another_thread_reading_the_file_does_not_drop_the_process_lock() {
        if !rrdtool_1110_available() {
            eprintln!("skipping lock retention check: pinned RRDtool 1.11.0 is not installed");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("held.rrd");
        create_rrd_file(
            &path,
            1_000_000_000,
            10,
            &["DS:value:GAUGE:30:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            true,
        )
        .unwrap();
        let held = RrdFileLock::exclusive(open_rrd_write(&path).unwrap()).unwrap();
        let reader = {
            let path = path.clone();
            std::thread::spawn(move || inspect_path(&path).map(|info| info.last_update))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        // A second descriptor closed by the reader would release this
        // process's lock and let an external writer in.
        let external = Command::new("rrdtool")
            .args(["update", path.to_str().unwrap(), "1000000010:1"])
            .env_remove("RRD_LOCKING")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&external.stderr),
            "ERROR: could not lock RRD\n"
        );
        assert!(!reader.is_finished());
        drop(held);
        assert_eq!(reader.join().unwrap().unwrap(), 1_000_000_000);
    }
}
