//! `update` and `updatev`: readings become amounts, PDPs, archive rows and
//! file bytes (docs/compatibility/update.md, compute.md CP-10 to CP-16).

mod amount;
mod arguments;
mod consolidate;
mod pdp;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use amount::{DsKind, amount, stored_text};
use arguments::{Argument, ArgumentError, parse_argument, template_order};
use consolidate::{ArchiveKind, RowRange, consolidate_all};
use pdp::{Completed, PdpState, StepPlan, Time, advance, plan, runs};

use crate::rpn::ComputeProgram;
use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN, RrdArchiveInfo, RrdFileLock,
    RrdInfo, STAT_HEAD_LEN, VALUE_LEN, divides_by_zero, open_rrd_write, read_info, rrd_nan,
    rrd_strerror,
};
use crate::storage::StoreError;

/// One row written by `updatev`, as printed (UP-31, UP-32).
#[derive(Clone, Debug, PartialEq)]
pub struct RrdUpdateSummary {
    pub timestamp: i64,
    pub consolidation: String,
    pub pdp_per_row: u64,
    pub values: Vec<f64>,
}

/// One update given as a time and one reading per data source; `None` is
/// the reading `U`.
#[derive(Clone, Debug, PartialEq)]
pub struct RrdRawUpdate<'a> {
    pub timestamp: i64,
    pub timestamp_usec: u64,
    pub values: Vec<Option<&'a str>>,
}

/// Applies `rrdtool update` arguments to the file at `path`. The summaries
/// cover every argument applied, also when a later one fails (UP-31).
pub fn update_rrd_text(
    path: impl AsRef<Path>,
    template: Option<&str>,
    arguments: &[&str],
    skip_past_updates: bool,
) -> (Vec<RrdUpdateSummary>, Result<(), StoreError>) {
    let path = path.as_ref();
    match open_rrd_write(path) {
        Ok(file) => update_rrd_text_file(file, path, template, arguments, skip_past_updates),
        Err(error) => (Vec::new(), Err(error)),
    }
}

/// [`update_rrd_text`] on a file the caller opened for reading and
/// writing. `path` names it in errors.
pub fn update_rrd_text_file(
    file: File,
    path: &Path,
    template: Option<&str>,
    arguments: &[&str],
    skip_past_updates: bool,
) -> (Vec<RrdUpdateSummary>, Result<(), StoreError>) {
    let input = Input::Text {
        template,
        arguments,
    };
    let mut summaries = Vec::new();
    let result = apply(file, path, &input, skip_past_updates, Some(&mut summaries));
    (summaries, result)
}

/// [`update_rrd_text`] without the summaries, which a long gap would
/// otherwise build one per row.
pub fn update_rrd_text_quiet(
    path: impl AsRef<Path>,
    template: Option<&str>,
    arguments: &[&str],
    skip_past_updates: bool,
) -> Result<(), StoreError> {
    let path = path.as_ref();
    update_rrd_text_file_quiet(
        open_rrd_write(path)?,
        path,
        template,
        arguments,
        skip_past_updates,
    )
}

/// [`update_rrd_text_quiet`] on a file the caller opened for reading and
/// writing. `path` names it in errors.
pub fn update_rrd_text_file_quiet(
    file: File,
    path: &Path,
    template: Option<&str>,
    arguments: &[&str],
    skip_past_updates: bool,
) -> Result<(), StoreError> {
    let input = Input::Text {
        template,
        arguments,
    };
    apply(file, path, &input, skip_past_updates, None)
}

/// Applies updates whose readings are already split per data source.
pub fn update_rrd_raw_batch(
    path: impl AsRef<Path>,
    updates: &[RrdRawUpdate<'_>],
    skip_past_updates: bool,
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    let path = path.as_ref();
    update_rrd_raw_batch_file(open_rrd_write(path)?, path, updates, skip_past_updates)
}

/// [`update_rrd_raw_batch`] on a file the caller opened for reading and
/// writing. `path` names it in errors.
pub fn update_rrd_raw_batch_file(
    file: File,
    path: &Path,
    updates: &[RrdRawUpdate<'_>],
    skip_past_updates: bool,
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    let mut summaries = Vec::new();
    apply(
        file,
        path,
        &Input::Raw(updates),
        skip_past_updates,
        Some(&mut summaries),
    )?;
    Ok(summaries)
}

/// Updates a one-DS file with one value.
pub fn update_rrd_file(
    path: impl AsRef<Path>,
    timestamp: i64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    update_rrd_file_precise(path, timestamp, 0, value)
}

/// Updates a one-DS file with one value at a time with microseconds.
pub fn update_rrd_file_precise(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    value: Option<f64>,
) -> Result<(), StoreError> {
    // A file with more than one data source fails the reading count.
    let reading = numeric_reading(value);
    run_raw(
        path.as_ref(),
        timestamp,
        timestamp_usec,
        &[Some(reading.as_str())],
        false,
    )
    .map(drop)
}

/// Updates every data source with one value each.
pub fn update_rrd_values(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<f64>],
) -> Result<(), StoreError> {
    let readings: Vec<String> = values.iter().copied().map(numeric_reading).collect();
    let readings: Vec<Option<&str>> = readings.iter().map(|text| Some(text.as_str())).collect();
    run_raw(path.as_ref(), timestamp, 0, &readings, false).map(drop)
}

/// Updates every data source with one reading each.
pub fn update_rrd_raw_values(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<&str>],
) -> Result<(), StoreError> {
    run_raw(path.as_ref(), timestamp, 0, values, false).map(drop)
}

/// [`update_rrd_raw_values`] at a time with microseconds.
pub fn update_rrd_raw_values_precise(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<&str>],
) -> Result<(), StoreError> {
    run_raw(path.as_ref(), timestamp, timestamp_usec, values, false).map(drop)
}

/// [`update_rrd_values`], returning the rows written.
pub fn update_rrd_values_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<f64>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    let readings: Vec<String> = values.iter().copied().map(numeric_reading).collect();
    let readings: Vec<Option<&str>> = readings.iter().map(|text| Some(text.as_str())).collect();
    run_raw(path.as_ref(), timestamp, 0, &readings, true)
}

/// [`update_rrd_raw_values`], returning the rows written.
pub fn update_rrd_raw_values_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    values: &[Option<&str>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    run_raw(path.as_ref(), timestamp, 0, values, true)
}

/// [`update_rrd_raw_values_precise`], returning the rows written.
pub fn update_rrd_raw_values_precise_verbose(
    path: impl AsRef<Path>,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<&str>],
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    run_raw(path.as_ref(), timestamp, timestamp_usec, values, true)
}

/// Applies one raw update. Only a `verbose` call builds the summaries; the
/// others return an empty list.
fn run_raw(
    path: &Path,
    timestamp: i64,
    timestamp_usec: u64,
    values: &[Option<&str>],
    verbose: bool,
) -> Result<Vec<RrdUpdateSummary>, StoreError> {
    let update = RrdRawUpdate {
        timestamp,
        timestamp_usec,
        values: values.to_vec(),
    };
    let mut summaries = Vec::new();
    apply(
        open_rrd_write(path)?,
        path,
        &Input::Raw(std::slice::from_ref(&update)),
        false,
        verbose.then_some(&mut summaries),
    )?;
    Ok(summaries)
}

/// The reading text for a numeric value.
fn numeric_reading(value: Option<f64>) -> String {
    match value {
        None => "U".to_string(),
        Some(x) if x.is_nan() => "U".to_string(),
        Some(x) if x == f64::INFINITY => "inf".to_string(),
        Some(x) if x == f64::NEG_INFINITY => "-inf".to_string(),
        Some(x) => {
            // A reading longer than the 29 characters a stored reading
            // keeps (UP-14) is given in exponent form, which keeps every
            // digit of the shortest round-trip text.
            let plain = format!("{x}");
            if plain.len() <= 29 {
                plain
            } else {
                format!("{x:e}")
            }
        }
    }
}

/// Where the arguments of one call come from.
enum Input<'a, 'b> {
    Text {
        template: Option<&'a str>,
        arguments: &'a [&'b str],
    },
    Raw(&'a [RrdRawUpdate<'b>]),
}

impl Input<'_, '_> {
    fn len(&self) -> usize {
        match self {
            Input::Text { arguments, .. } => arguments.len(),
            Input::Raw(updates) => updates.len(),
        }
    }
}

/// Byte offsets of the live parts of the header (`create.md` CR-38 to
/// CR-44).
struct Layout {
    live: usize,
    pdp: usize,
    cdp: usize,
    end: usize,
}

impl Layout {
    fn new(info: &RrdInfo) -> Layout {
        // read_info has already checked that these sums fit in the file.
        let sources = info.data_sources.len();
        let archives = info.archives.len();
        let live = STAT_HEAD_LEN + sources * DS_DEF_LEN + archives * RRA_DEF_LEN;
        let pdp = live + info.live_head_len;
        let cdp = pdp + sources * PDP_PREP_LEN;
        Layout {
            live,
            pdp,
            cdp,
            end: info.header_size,
        }
    }

    fn pdp_entry(&self, source: usize) -> usize {
        self.pdp + source * PDP_PREP_LEN
    }

    fn cdp_entry(&self, sources: usize, archive: usize, source: usize) -> usize {
        self.cdp + (archive * sources + source) * CDP_PREP_LEN
    }

    fn pointer(&self, sources: usize, archives: usize, archive: usize) -> usize {
        self.cdp + sources * archives * CDP_PREP_LEN + archive * RRA_PTR_LEN
    }
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn put_f64(bytes: &mut [u8], offset: usize, value: f64) {
    put_u64(bytes, offset, value.to_bits());
}

fn write_at(file: &mut File, offset: u64, bytes: &[u8]) -> Result<(), StoreError> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)?;
    Ok(())
}

fn now() -> Time {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Time {
        secs: i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        usec: u64::from(elapsed.subsec_micros()),
    }
}

fn argument_error(error: ArgumentError) -> StoreError {
    match error {
        ArgumentError::Message(message) => StoreError::RrdExpression(message),
        ArgumentError::Time(message) => StoreError::RrdTimestamp(message),
    }
}

fn raw_argument(update: &RrdRawUpdate<'_>, sources: usize) -> Result<Argument, StoreError> {
    if update.values.len() != sources {
        return Err(StoreError::RrdExpression(format!(
            "expected {sources} data source readings (got {}) from {}",
            update.values.len(),
            update.timestamp
        )));
    }
    Ok(Argument {
        time: Time {
            secs: update.timestamp,
            usec: update.timestamp_usec,
        },
        readings: update
            .values
            .iter()
            .map(|value| value.unwrap_or("U").to_string())
            .collect(),
    })
}

/// The PDP values of one completed step shape, one per DS, taken from or
/// put back into a list of completed PDPs.
fn pick(completed: &[Completed], rest: bool) -> Vec<f64> {
    completed
        .iter()
        .map(|c| match *c {
            Completed::None => rrd_nan(),
            Completed::Whole(value) => value,
            Completed::Split { first, .. } if !rest => first,
            Completed::Split { rest, .. } => rest,
        })
        .collect()
}

/// Replaces each computed source's PDPs with its expression evaluated over
/// the PDPs of the same step (CP-12, CP-13). Nothing is evaluated when no
/// step completes (CP-16).
fn evaluate_computed(
    programs: &[Option<Option<ComputeProgram>>],
    completed: &mut [Completed],
) -> Result<(), String> {
    if !programs.iter().any(Option::is_some) {
        return Ok(());
    }
    let shapes: &[bool] = match completed.first() {
        None | Some(Completed::None) => return Ok(()),
        Some(Completed::Whole(_)) => &[false],
        Some(Completed::Split { .. }) => &[false, true],
    };
    for &rest in shapes {
        let mut values = pick(completed, rest);
        for (index, program) in programs.iter().enumerate() {
            let Some(program) = program else { continue };
            // An expression whose stored nodes do not decode yields unknown.
            values[index] = match program {
                Some(program) => program
                    .evaluate(&values)
                    .map_err(|error| error.to_string())?,
                None => rrd_nan(),
            };
            completed[index] = match completed[index] {
                Completed::Split { first, .. } if rest => Completed::Split {
                    first,
                    rest: values[index],
                },
                Completed::Split { rest: later, .. } => Completed::Split {
                    first: values[index],
                    rest: later,
                },
                _ => Completed::Whole(values[index]),
            };
        }
    }
    Ok(())
}

/// The most bytes [`write_range`] writes at once.
const WRITE_CHUNK: usize = 1 << 20;

/// Stores a range of rows, wrapping to row 0 at the archive's row count
/// (UP-30), in writes of at most [`WRITE_CHUNK`] bytes.
fn write_range(
    file: &mut File,
    archive: &RrdArchiveInfo,
    range: &RowRange,
) -> Result<(), StoreError> {
    let row: Vec<u8> = range
        .values
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect();
    let row_len = row.len() as u64;
    let chunk_rows = (WRITE_CHUNK / row.len()).max(1) as u64;
    let chunk = row.repeat(chunk_rows.min(range.count) as usize);
    let mut next = range.first_row;
    let mut remaining = range.count;
    while remaining > 0 {
        let rows = remaining.min(archive.rows - next).min(chunk_rows);
        // Every queued row lies inside the file (`rows_in_file`), so this
        // offset cannot overflow.
        let offset = archive.data_offset + next * row_len;
        write_at(file, offset, &chunk[..(rows * row_len) as usize])?;
        next += rows;
        if next == archive.rows {
            next = 0;
        }
        remaining -= rows;
    }
    Ok(())
}

/// Opens the file's state and applies every argument in turn, each
/// completely before the next is examined (UP-3, UP-34). Rows of applied
/// arguments are appended to `summaries`, when the caller asked for them.
fn apply(
    file: File,
    path: &Path,
    input: &Input<'_, '_>,
    skip_past_updates: bool,
    mut summaries: Option<&mut Vec<RrdUpdateSummary>>,
) -> Result<(), StoreError> {
    let mut file = RrdFileLock::exclusive(file)?;
    file.seek(SeekFrom::Start(0))?;
    let mut info = read_info(&mut file, path)?;
    let file_len = file.metadata()?.len();

    // Provenance: specification (docs/compatibility/update.md UP-4, UP-5;
    // compute.md CP-10).
    let names: Vec<&str> = info.data_sources.iter().map(|s| s.name.as_str()).collect();
    let order = match input {
        Input::Text {
            template: Some(template),
            ..
        } => template_order(Some(template), &names).map_err(StoreError::Rrd)?,
        _ if info.data_sources.iter().any(|s| s.is_computed()) => Some(
            (0..info.data_sources.len())
                .filter(|&index| !info.data_sources[index].is_computed())
                .collect(),
        ),
        _ => None,
    };

    let kinds = info
        .data_sources
        .iter()
        .map(|source| {
            DsKind::from_raw(&source.raw_kind).ok_or_else(|| {
                let message = crate::rrd_binary::dst_conv_error(&source.raw_kind);
                StoreError::Rrd(String::from_utf8_lossy(&message.unwrap_or_default()).into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let archive_kinds = info
        .archives
        .iter()
        .map(|archive| ArchiveKind::from_raw(&archive.raw_consolidation))
        .collect::<Result<Vec<_>, _>>()?;
    if info.step == 0
        || info
            .archives
            .iter()
            .any(|archive| archive.pdp_per_row == 0 || archive.rows == 0)
    {
        return Err(divides_by_zero());
    }

    let layout = Layout::new(&info);
    let mut header = vec![0; layout.end];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;

    let step = info.step;
    let source_count = info.data_sources.len();
    let archive_count = info.archives.len();
    // Rows of each archive that lie inside the file; a damaged row count
    // can claim more.
    let row_bytes = (source_count * VALUE_LEN) as u64;
    let rows_in_file: Vec<u64> = info
        .archives
        .iter()
        .map(|archive| match file_len.checked_sub(archive.data_offset) {
            Some(_) if row_bytes == 0 => u64::MAX,
            Some(room) => room / row_bytes,
            None => 0,
        })
        .collect();
    let programs: Vec<Option<Option<ComputeProgram>>> = info
        .data_sources
        .iter()
        .map(|source| source.is_computed().then(|| source.program.clone()))
        .collect();

    for index in 0..input.len() {
        // Provenance: specification (docs/compatibility/update.md UP-6 to UP-8).
        let argument = match input {
            Input::Text { arguments, .. } => {
                parse_argument(arguments[index], source_count, order.as_deref(), now())
                    .map_err(argument_error)?
            }
            Input::Raw(updates) => raw_argument(&updates[index], source_count)?,
        };

        // Provenance: specification (docs/compatibility/update.md UP-9).
        let last = Time {
            secs: info.last_update,
            usec: info.last_update_usec,
        };
        if argument.time <= last {
            if skip_past_updates {
                continue;
            }
            return Err(StoreError::RrdExpression(format!(
                "illegal attempt to update using time {} when last update time is {} (minimum one second step)",
                argument.time.secs, last.secs
            )));
        }
        let plan = plan(last, argument.time, step);

        // Provenance: specification (docs/compatibility/update.md UP-10 to
        // UP-18, UP-13, UP-34).
        let mut amounts = Vec::with_capacity(source_count);
        for (ds, reading) in argument.readings.iter().enumerate() {
            let source = &info.data_sources[ds];
            let found = amount(
                kinds[ds],
                reading,
                &source.raw_last_value,
                plan.interval,
                source.heartbeat,
                source.minimum,
                source.maximum,
            );
            match found {
                Ok(value) => amounts.push(value),
                Err(message) => {
                    for (earlier, text) in argument.readings[..ds].iter().enumerate() {
                        let offset = layout.pdp_entry(earlier);
                        header[offset..offset + 30].copy_from_slice(&stored_text(text));
                    }
                    write_at(
                        &mut file,
                        layout.pdp as u64,
                        &header[layout.pdp..layout.cdp],
                    )?;
                    return Err(StoreError::RrdExpression(message));
                }
            }
        }

        // Provenance: specification (docs/compatibility/update.md UP-19 to
        // UP-25; compute.md CP-15).
        let mut before = Vec::with_capacity(source_count);
        let mut states = Vec::with_capacity(source_count);
        let mut completed = Vec::with_capacity(source_count);
        for (source, &found) in info.data_sources.iter().zip(&amounts) {
            let mut state = PdpState {
                unknown_seconds: source.unknown_seconds,
                value: source.pdp_value,
            };
            before.push(state);
            completed.push(advance(&mut state, found, &plan, step, source.heartbeat));
            states.push(state);
        }

        let encode_source = |header: &mut [u8], ds: usize, state: &PdpState| {
            let offset = layout.pdp_entry(ds);
            header[offset..offset + 30].copy_from_slice(&stored_text(&argument.readings[ds]));
            put_u64(header, offset + 32, state.unknown_seconds);
            put_f64(header, offset + 40, state.value);
        };

        // Provenance: specification (docs/compatibility/compute.md CP-12,
        // CP-13, CP-16).
        if let Err(error) = evaluate_computed(&programs, &mut completed) {
            for (ds, state) in states.iter().enumerate() {
                if kinds[ds] != DsKind::Compute {
                    encode_source(&mut header, ds, state);
                }
            }
            write_at(
                &mut file,
                layout.pdp as u64,
                &header[layout.pdp..layout.cdp],
            )?;
            return Err(StoreError::RrdExpression(error));
        }

        // Provenance: specification (docs/compatibility/update.md UP-23,
        // UP-28 to UP-32).
        let mut rows: Vec<RowRange> = Vec::new();
        let mut written = Vec::new();
        let mut past_end = false;
        for (position, run) in runs(&plan, step, &completed).iter().enumerate() {
            let early_first_row = position == 0 && plan.early_first_print;
            let consolidated = consolidate_all(
                &mut info.archives,
                &archive_kinds,
                run,
                step,
                early_first_row,
                &rows_in_file,
                &mut rows,
                summaries.is_some().then_some(&mut written),
            );
            if consolidated.is_err() {
                past_end = true;
                // Compatibility: a split update failing in its first run
                // keeps the PDP state UP-25 gives for a tail of 0.
                // Provenance: specification (docs/compatibility/update.md UP-38).
                if position == 0 && plan.split {
                    let at_boundary = StepPlan { tail: 0.0, ..plan };
                    for (ds, source) in info.data_sources.iter().enumerate() {
                        let mut state = before[ds];
                        advance(
                            &mut state,
                            amounts[ds],
                            &at_boundary,
                            step,
                            source.heartbeat,
                        );
                        states[ds] = state;
                    }
                }
                break;
            }
        }

        // Provenance: specification (docs/compatibility/update.md UP-33,
        // UP-38). When a row falls past the end of the file, the PDP and
        // CDP state, current rows and earlier rows are written but the last
        // update time is not, and the update fails.
        if !past_end {
            put_u64(&mut header, layout.live, argument.time.secs as u64);
            if info.live_head_len >= 16 {
                put_u64(&mut header, layout.live + 8, argument.time.usec);
            }
        }
        for (ds, state) in states.iter().enumerate() {
            encode_source(&mut header, ds, state);
            let source = &mut info.data_sources[ds];
            let text = stored_text(&argument.readings[ds]);
            let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            source.raw_last_value = text[..end].to_vec();
            source.unknown_seconds = state.unknown_seconds;
            source.pdp_value = state.value;
        }
        for (rra, archive) in info.archives.iter().enumerate() {
            for (ds, cdp) in archive.cdp_prep.iter().enumerate() {
                let offset = layout.cdp_entry(source_count, rra, ds);
                put_f64(&mut header, offset, cdp.value);
                put_u64(&mut header, offset + 8, cdp.unknown_datapoints);
                put_f64(&mut header, offset + 64, cdp.primary_value);
                put_f64(&mut header, offset + 72, cdp.secondary_value);
            }
            put_u64(
                &mut header,
                layout.pointer(source_count, archive_count, rra),
                archive.current_row,
            );
        }
        write_at(
            &mut file,
            layout.live as u64,
            &header[layout.live..layout.end],
        )?;
        for range in &rows {
            write_range(&mut file, &info.archives[range.archive], range)?;
        }
        if past_end {
            if let Some(summaries) = summaries.as_deref_mut() {
                summaries.append(&mut written);
            }
            let unwritten = std::io::Error::from_raw_os_error(0);
            return Err(StoreError::RrdExpression(format!(
                "writing rrd: {}",
                rrd_strerror(&unwritten)
            )));
        }

        info.last_update = argument.time.secs;
        info.last_update_usec = argument.time.usec;
        if let Some(summaries) = summaries.as_deref_mut() {
            summaries.append(&mut written);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(path: &Path) {
        crate::create_rrd_file(
            path,
            1_000_000_000,
            10,
            &[
                "DS:a:COUNTER:600:U:U".to_string(),
                "DS:b:COUNTER:600:U:U".to_string(),
            ],
            &["RRA:AVERAGE:0.5:1:10".to_string()],
            false,
        )
        .unwrap();
    }

    #[test]
    fn failed_argument_keeps_earlier_stored_texts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("partial.rrd");
        let first_only = temp.path().join("first.rrd");
        create(&path);
        std::fs::copy(&path, &first_only).unwrap();

        let (summaries, result) = update_rrd_text(
            &path,
            None,
            &["1000000010:5:6", "1000000020:7:x", "1000000030:8:9"],
            false,
        );
        let (expected, first) = update_rrd_text(&first_only, None, &["1000000010:5:6"], false);

        assert!(result.is_err(), "{result:?}");
        first.unwrap();
        assert!(!expected.is_empty());
        // Rows are compared by bits, since unknown values are NaN.
        let bits = |rows: &[RrdUpdateSummary]| {
            rows.iter()
                .map(|row| {
                    let values: Vec<u64> = row.values.iter().map(|v| v.to_bits()).collect();
                    (
                        row.timestamp,
                        row.consolidation.clone(),
                        row.pdp_per_row,
                        values,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(&summaries), bits(&expected));
        let info = crate::inspect_rrd_file(&path).unwrap();
        assert_eq!(info.last_update, 1_000_000_010);
        assert_eq!(info.data_sources[0].raw_last_value, b"7");
        assert_eq!(info.data_sources[1].raw_last_value, b"6");
    }

    /// A file with no data sources: one archive of `rows` rows.
    fn write_zero_source_file(path: &Path, rows: u64) {
        let mut bytes = vec![0u8; STAT_HEAD_LEN + RRA_DEF_LEN];
        bytes[..4].copy_from_slice(b"RRD\0");
        bytes[4..9].copy_from_slice(b"0003\0");
        bytes[16..24].copy_from_slice(&crate::rrd_binary::FLOAT_COOKIE.to_le_bytes());
        bytes[32..40].copy_from_slice(&1u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&300u64.to_le_bytes());
        let rra = STAT_HEAD_LEN;
        bytes[rra..rra + 7].copy_from_slice(b"AVERAGE");
        bytes[rra + 24..rra + 32].copy_from_slice(&rows.to_le_bytes());
        bytes[rra + 32..rra + 40].copy_from_slice(&1u64.to_le_bytes());
        bytes[rra + 40..rra + 48].copy_from_slice(&0.5f64.to_le_bytes());
        bytes.extend_from_slice(&1_000_000_000i64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn zero_source_file_with_huge_rows_finishes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("empty.rrd");
        write_zero_source_file(&path, u64::MAX);
        let steps: u64 = 2_000_000_000;
        let timestamp = 1_000_000_000 + (steps * 300) as i64;
        let update = RrdRawUpdate {
            timestamp,
            timestamp_usec: 0,
            values: Vec::new(),
        };
        let started = std::time::Instant::now();
        let summaries = update_rrd_raw_batch(&path, &[update], false).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(summaries.is_empty());
        let info = crate::inspect_rrd_file(&path).unwrap();
        // The starting row 0 advances once per PDP (UP-30).
        assert_eq!(info.archives[0].current_row, steps);
        assert_eq!(info.last_update, timestamp);
    }

    /// The stored values of an archive's rows, one `Vec` per row.
    fn stored_rows(path: &Path, archive: usize) -> Vec<Vec<f64>> {
        let info = crate::inspect_rrd_file(path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        let sources = info.data_sources.len();
        let archive = &info.archives[archive];
        let start = archive.data_offset as usize;
        bytes[start..start + archive.rows as usize * sources * VALUE_LEN]
            .chunks(sources * VALUE_LEN)
            .map(|row| {
                row.chunks(VALUE_LEN)
                    .map(|value| f64::from_le_bytes(value.try_into().unwrap()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn long_gap_fills_every_row_in_bounded_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("long.rrd");
        let rows = 2_000_000u64;
        crate::create_rrd_file(
            &path,
            1_000_000_000,
            1,
            &["DS:a:GAUGE:10000000:U:U".to_string()],
            &[format!("RRA:AVERAGE:0.5:1:{rows}")],
            false,
        )
        .unwrap();
        update_rrd_values(&path, 1_000_000_001, &[Some(1.0)]).unwrap();
        let before = crate::inspect_rrd_file(&path).unwrap().archives[0].current_row;

        update_rrd_values(&path, 1_003_000_001, &[Some(5.0)]).unwrap();

        let info = crate::inspect_rrd_file(&path).unwrap();
        // Three million rows into two million: UP-29 writes each row once,
        // so the current row comes back to where it was.
        assert_eq!(info.archives[0].current_row, before);
        let stored = stored_rows(&path, 0);
        for row in [
            0,
            1,
            524_287,
            524_288,
            1_000_000,
            before as usize,
            1_999_999,
        ] {
            assert_eq!(stored[row], vec![5.0], "row {row}");
        }
        assert!(stored.iter().all(|row| row == &[5.0]));
    }

    #[test]
    fn non_verbose_updates_store_what_verbose_ones_do() {
        let temp = tempfile::tempdir().unwrap();
        let quiet = temp.path().join("quiet.rrd");
        let loud = temp.path().join("loud.rrd");
        create(&quiet);
        std::fs::copy(&quiet, &loud).unwrap();
        let updates = [
            RrdRawUpdate {
                timestamp: 1_000_000_010,
                timestamp_usec: 0,
                values: vec![Some("1"), Some("2")],
            },
            RrdRawUpdate {
                timestamp: 1_000_000_095,
                timestamp_usec: 250_000,
                values: vec![Some("300"), None],
            },
        ];
        let rows = update_rrd_raw_batch(&loud, &updates, false).unwrap();
        assert_eq!(rows.len(), 9);
        apply(
            open_rrd_write(&quiet).unwrap(),
            &quiet,
            &Input::Raw(&updates),
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(&quiet).unwrap(),
            std::fs::read(&loud).unwrap()
        );
    }

    #[test]
    fn long_numeric_readings_keep_their_digits() {
        for x in [
            1e30,
            1e30 + 1e16,
            1.234_567_890_123_456_7e-15,
            -1.5e300,
            0.1,
            42.0,
        ] {
            let text = numeric_reading(Some(x));
            assert!(text.len() <= 29, "{text}");
            assert_eq!(
                text.parse::<f64>().unwrap().to_bits(),
                x.to_bits(),
                "{text}"
            );
        }
        assert_eq!(numeric_reading(Some(42.0)), "42");
        assert_eq!(numeric_reading(None), "U");
        assert_eq!(numeric_reading(Some(f64::NAN)), "U");
        assert_eq!(numeric_reading(Some(f64::INFINITY)), "inf");
        assert_eq!(numeric_reading(Some(f64::NEG_INFINITY)), "-inf");

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dderive.rrd");
        crate::create_rrd_file(
            &path,
            1_000_000_000,
            10,
            &["DS:a:DDERIVE:600:U:U".to_string()],
            &["RRA:LAST:0.5:1:5".to_string()],
            false,
        )
        .unwrap();
        update_rrd_values(&path, 1_000_000_010, &[Some(1e30)]).unwrap();
        let rows = update_rrd_values_verbose(&path, 1_000_000_020, &[Some(1e30 + 1e16)]).unwrap();
        assert_eq!(rows.len(), 1);
        // 1e16 over 10 s; the difference of the two readings is exact to
        // within a unit in the last place of 1e30.
        let rate = rows[0].values[0];
        assert!((rate - 1e15).abs() < 2e13, "{rate}");
    }

    #[test]
    fn undecodable_compute_program_is_unknown() {
        // Rondi's own fallback for a damaged program. RRDtool 1.11.0 does
        // not decode such files this way; flagged for provenance review.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("compute.rrd");
        crate::create_rrd_file(
            &path,
            1_000_000_000,
            10,
            &[
                "DS:a:GAUGE:600:U:U".to_string(),
                "DS:y:COMPUTE:a,2,*".to_string(),
            ],
            &["RRA:LAST:0.5:1:5".to_string()],
            false,
        )
        .unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        // The first node of the second definition's program gets a code
        // no node has.
        bytes[STAT_HEAD_LEN + DS_DEF_LEN + 40] = 250;
        std::fs::write(&path, bytes).unwrap();

        let (rows, result) = update_rrd_text(&path, None, &["1000000010:3"], false);
        result.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values[0], 3.0);
        assert!(rows[0].values[1].is_nan());
    }
}
