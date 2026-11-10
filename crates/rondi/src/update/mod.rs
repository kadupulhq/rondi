//! `update` and `updatev`: readings become amounts, PDPs, archive rows and
//! file bytes (docs/compatibility/update.md, compute.md CP-10 to CP-16).

mod amount;
mod arguments;
mod consolidate;
mod holt_winters;
mod pdp;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use amount::{DsKind, amount, stored_text};
use arguments::{Argument, ArgumentError, parse_argument, template_order};
use consolidate::{ArchiveKind, RowRange, consolidate_all};
use holt_winters::{Group, Seasons, smooth};
use pdp::{Completed, PdpState, StepPlan, Time, advance, plan, runs};

use crate::holt_winters::{MAX_WINDOW_LEN, Role, param};
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
///
/// Raw and numeric updates take one value per data source in definition
/// order, computed sources included, unlike text arguments (CP-10). The
/// value given for a computed source is ignored: the source is evaluated
/// as usual and its last value is stored as `U`, as for a text argument
/// that leaves it out.
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
///
/// Raw and numeric updates take one value per data source in definition
/// order, computed sources included, unlike text arguments (CP-10). The
/// value given for a computed source is ignored: the source is evaluated
/// as usual and its last value is stored as `U`, as for a text argument
/// that leaves it out.
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
///
/// Raw and numeric updates take one value per data source in definition
/// order, computed sources included, unlike text arguments (CP-10). The
/// value given for a computed source is ignored: the source is evaluated
/// as usual and its last value is stored as `U`, as for a text argument
/// that leaves it out.
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
///
/// Raw and numeric updates take one value per data source in definition
/// order, computed sources included, unlike text arguments (CP-10). The
/// value given for a computed source is ignored: the source is evaluated
/// as usual and its last value is stored as `U`, as for a text argument
/// that leaves it out.
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
            // keeps (UP-13) is given in exponent form, which keeps every
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

/// One raw update as an argument. `computed` marks each data source that
/// is computed; the value given for one is replaced by `U`.
fn raw_argument(update: &RrdRawUpdate<'_>, computed: &[bool]) -> Result<Argument, StoreError> {
    let sources = computed.len();
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
            .zip(computed)
            .map(|(value, &computed)| match value {
                Some(text) if !computed => text.to_string(),
                _ => "U".to_string(),
            })
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

/// Where a command's file writes go. Without Holt-Winters archives each
/// write happens when the argument makes it. With them, writes are held
/// until the command ends, and the smoothing pass's rows (HW-18) are queued
/// after every other row of the command.
struct Writes {
    hold: bool,
    /// The span of the header that changed while held.
    header: Option<(usize, usize)>,
    rows: Vec<RowRange>,
}

impl Writes {
    /// Writes `header[from..to]`, now or at [`Writes::commit`]. The header
    /// buffer only accumulates changes, so writing its final state once
    /// stores what writing each span in turn would.
    fn header(
        &mut self,
        file: &mut File,
        header: &[u8],
        from: usize,
        to: usize,
    ) -> Result<(), StoreError> {
        if !self.hold {
            return write_at(file, from as u64, &header[from..to]);
        }
        self.header = Some(match self.header {
            Some((start, end)) => (start.min(from), end.max(to)),
            None => (from, to),
        });
        Ok(())
    }

    /// Stores row ranges, now or at [`Writes::commit`], in their order.
    fn rows(
        &mut self,
        file: &mut File,
        archives: &[RrdArchiveInfo],
        row_len: usize,
        rows: Vec<RowRange>,
    ) -> Result<(), StoreError> {
        if self.hold {
            self.rows.extend(rows);
            return Ok(());
        }
        for range in &rows {
            write_range(file, &archives[range.archive], row_len, range)?;
        }
        Ok(())
    }

    /// Stores the held writes: the header first, then the rows in order.
    fn commit(
        self,
        file: &mut File,
        header: &[u8],
        archives: &[RrdArchiveInfo],
        row_len: usize,
    ) -> Result<(), StoreError> {
        if let Some((from, to)) = self.header {
            write_at(file, from as u64, &header[from..to])?;
        }
        for range in &self.rows {
            write_range(file, &archives[range.archive], row_len, range)?;
        }
        Ok(())
    }
}

/// The group of a file's Holt-Winters archives, after the checks that let
/// Rondi update it; `None` for a file without them.
fn holt_winters_group(info: &RrdInfo, kinds: &[ArchiveKind]) -> Result<Option<Group>, StoreError> {
    if !kinds
        .iter()
        .any(|kind| matches!(kind, ArchiveKind::HoltWinters(_)))
    {
        return Ok(None);
    }
    let group = Group::find(kinds).ok_or_else(|| {
        StoreError::RrdUnsupported(
            "Holt-Winters archives without exactly one prediction archive and at most one \
             archive of each other type are not supported by Rondi"
                .into(),
        )
    })?;
    // Compatibility: a FAILURES window above 28 (only in a crafted file)
    // fails the update before anything is written, where RRDtool would
    // overwrite the CDP state beyond the history.
    // Provenance: specification (docs/compatibility/holt-winters.md HW-5).
    if let Some(index) = group.failures {
        let window_len = info.archives[index].param_u64(param::WINDOW_LEN);
        if window_len > MAX_WINDOW_LEN {
            return Err(StoreError::Rrd(format!(
                "FAILURES window length {window_len} exceeds {MAX_WINDOW_LEN}"
            )));
        }
    }
    // Compatibility: a prediction, SEASONAL or DEVSEASONAL link that names
    // an archive of another type changes what RRDtool computes in ways no
    // vector pins; Rondi refuses such a file before writing anything.
    // `rrdtool create` stores a misnumbered rra-num as given (create.md
    // CR-35), so an ordinary create can produce one, not only a crafted
    // file. DEVPREDICT and FAILURES links are followed.
    // Provenance: black-box compatibility test (hwimp with each link
    // patched); docs/compatibility/holt-winters.md Open questions.
    if !group.season_links_match_types(&info.archives) {
        return Err(StoreError::RrdUnsupported(
            "Holt-Winters prediction or season archives whose dependent_rra_idx names an \
             archive of another type are not supported by Rondi"
                .into(),
        ));
    }
    debug_assert!(matches!(
        kinds[group.prediction],
        ArchiveKind::HoltWinters(Role::Prediction { .. })
    ));
    Ok(Some(group))
}

/// The most bytes [`write_range`] writes at once.
const WRITE_CHUNK: usize = 1 << 20;

/// Stores a range of rows, wrapping to row 0 at the archive's row count
/// (UP-30), in writes of at most [`WRITE_CHUNK`] bytes.
/// `row_len` is the archive's row width in bytes, one value per data
/// source.
fn write_range(
    file: &mut File,
    archive: &RrdArchiveInfo,
    row_len: usize,
    range: &RowRange,
) -> Result<(), StoreError> {
    if range.count == 0 || row_len == 0 {
        return Ok(());
    }
    let row: Vec<u8> = range
        .values
        .iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect();
    debug_assert_eq!(row.len(), row_len);
    let chunk_rows = (WRITE_CHUNK / row_len).max(1) as u64;
    let row_len = row_len as u64;
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
    summaries: Option<&mut Vec<RrdUpdateSummary>>,
) -> Result<(), StoreError> {
    let mut file = RrdFileLock::exclusive(file)?;
    file.seek(SeekFrom::Start(0))?;
    let info = read_info(&mut file, path)?;
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
    let group = holt_winters_group(&info, &archive_kinds)?;

    let layout = Layout::new(&info);
    let mut header = vec![0; layout.end];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;

    let source_count = info.data_sources.len();
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
    let computed: Vec<bool> = info.data_sources.iter().map(|s| s.is_computed()).collect();
    let programs: Vec<Option<Option<ComputeProgram>>> = info
        .data_sources
        .iter()
        .map(|source| source.is_computed().then(|| source.program.clone()))
        .collect();
    let seasons = match &group {
        Some(group) => Seasons::load(&mut file, &info, group)?,
        None => Seasons::default(),
    };
    let row_len = source_count * VALUE_LEN;
    let mut command = Command {
        file: &mut file,
        writes: Writes {
            hold: group.is_some(),
            header: None,
            rows: Vec::new(),
        },
        info,
        header,
        layout,
        order,
        kinds,
        archive_kinds,
        group,
        rows_in_file,
        computed,
        programs,
        seasons,
        triggered: false,
    };
    let result = apply_arguments(&mut command, input, skip_past_updates, summaries);
    // Compatibility: one smoothing pass follows the last argument of a
    // command whose season counters triggered; its rows are stored after
    // every other write of the command and are not printed. A command that
    // fails runs no pass (UP-34 keeps what it applied).
    // Provenance: specification (docs/compatibility/holt-winters.md HW-18;
    // the failure rule is the Holt-Winters plan's ruling on HW-18's open
    // point).
    if let Some(group) = &command.group
        && result.is_ok()
        && command.triggered
    {
        let mut rows = Vec::new();
        smooth(
            group,
            &mut command.info.archives,
            &mut command.seasons,
            &command.info.version,
            &mut rows,
        );
        encode_archives(&mut command.header, &command.layout, &command.info);
        let Command {
            file,
            writes,
            header,
            info,
            layout,
            ..
        } = &mut command;
        writes.header(file, header, layout.cdp, layout.end)?;
        writes.rows(file, &info.archives, row_len, rows)?;
    }
    let Command {
        file,
        writes,
        header,
        info,
        ..
    } = command;
    writes.commit(file, &header, &info.archives, row_len)?;
    result
}

/// Puts every archive's CDP entries and current row into `header`.
fn encode_archives(header: &mut [u8], layout: &Layout, info: &RrdInfo) {
    let source_count = info.data_sources.len();
    let archive_count = info.archives.len();
    for (rra, archive) in info.archives.iter().enumerate() {
        for (ds, cdp) in archive.cdp_prep.iter().enumerate() {
            // Classic consolidation changes the named fields only;
            // Holt-Winters state lives in words 2 to 7 (RS-53), which
            // classic archives carry through unchanged.
            let offset = layout.cdp_entry(source_count, rra, ds);
            put_f64(header, offset, cdp.value);
            put_u64(header, offset + 8, cdp.unknown_datapoints);
            for slot in 2..8 {
                put_u64(header, offset + 8 * slot, cdp.words[slot]);
            }
            put_f64(header, offset + 64, cdp.primary_value);
            put_f64(header, offset + 72, cdp.secondary_value);
        }
        put_u64(
            header,
            layout.pointer(source_count, archive_count, rra),
            archive.current_row,
        );
    }
}

/// The state of one `update` command, read once before its arguments.
struct Command<'a> {
    file: &'a mut File,
    info: RrdInfo,
    header: Vec<u8>,
    layout: Layout,
    order: Option<Vec<usize>>,
    kinds: Vec<DsKind>,
    archive_kinds: Vec<ArchiveKind>,
    group: Option<Group>,
    rows_in_file: Vec<u64>,
    computed: Vec<bool>,
    programs: Vec<Option<Option<ComputeProgram>>>,
    seasons: Seasons,
    /// Set when a season counter reaches a smoothing point (HW-17).
    triggered: bool,
    writes: Writes,
}

/// Applies every argument in turn, each completely before the next is
/// examined (UP-3, UP-34), appending the rows of applied arguments to
/// `summaries`.
fn apply_arguments(
    command: &mut Command<'_>,
    input: &Input<'_, '_>,
    skip_past_updates: bool,
    mut summaries: Option<&mut Vec<RrdUpdateSummary>>,
) -> Result<(), StoreError> {
    let Command {
        file,
        info,
        header,
        layout,
        order,
        kinds,
        archive_kinds,
        group,
        rows_in_file,
        computed,
        programs,
        seasons,
        triggered,
        writes,
    } = command;
    let step = info.step;
    let source_count = info.data_sources.len();
    let row_len = source_count * VALUE_LEN;
    for index in 0..input.len() {
        // Provenance: specification (docs/compatibility/update.md UP-6 to UP-8).
        let argument = match input {
            Input::Text { arguments, .. } => {
                parse_argument(arguments[index], source_count, order.as_deref(), now())
                    .map_err(argument_error)?
            }
            Input::Raw(updates) => raw_argument(&updates[index], computed)?,
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
                    writes.header(file, header, layout.pdp, layout.cdp)?;
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
        if let Err(error) = evaluate_computed(programs, &mut completed) {
            for (ds, state) in states.iter().enumerate() {
                if kinds[ds] != DsKind::Compute {
                    encode_source(header, ds, state);
                }
            }
            writes.header(file, header, layout.pdp, layout.cdp)?;
            return Err(StoreError::RrdExpression(error));
        }

        // Provenance: specification (docs/compatibility/update.md UP-23,
        // UP-28 to UP-32).
        let mut rows: Vec<RowRange> = Vec::new();
        let mut written = Vec::new();
        let mut past_end = false;
        let runs = runs(&plan, step, &completed);
        for (position, run) in runs.iter().enumerate() {
            let early_first_row = position == 0 && plan.early_first_print;
            let consolidated = consolidate_all(
                &mut info.archives,
                archive_kinds,
                run,
                step,
                early_first_row,
                rows_in_file,
                &mut rows,
                summaries.is_some().then_some(&mut written),
                group
                    .as_ref()
                    .map(|group| (group, &mut *seasons, &mut *triggered)),
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
            put_u64(header, layout.live, argument.time.secs as u64);
            if info.live_head_len >= 16 {
                put_u64(header, layout.live + 8, argument.time.usec);
            }
        }
        for (ds, state) in states.iter().enumerate() {
            encode_source(header, ds, state);
            let source = &mut info.data_sources[ds];
            let text = stored_text(&argument.readings[ds]);
            let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
            source.raw_last_value = text[..end].to_vec();
            source.unknown_seconds = state.unknown_seconds;
            source.pdp_value = state.value;
        }
        encode_archives(header, layout, info);
        writes.header(file, header, layout.live, layout.end)?;
        writes.rows(file, &info.archives, row_len, rows)?;
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

    fn hw_input(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../rondi-cli/tests/data/hw/inputs")
            .join(format!("{name}.rrd"))
    }

    /// Copies Holt-Winters input `name` into `dir`, with its version text
    /// replaced by `version` when given.
    fn hw_copy(dir: &Path, name: &str, version: Option<&[u8; 4]>) -> std::path::PathBuf {
        let mut bytes = std::fs::read(hw_input(name)).unwrap();
        if let Some(version) = version {
            bytes[4..8].copy_from_slice(version);
        }
        let path = dir.join(format!(
            "{name}-{}.rrd",
            String::from_utf8_lossy(version.map_or(b"same", |v| v))
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// One-PDP arguments for hwsm10 from 1000000010 on.
    fn hwsm10_arguments(count: i64) -> Vec<String> {
        (1..=count)
            .map(|i| format!("{}:{}", 1_000_000_000 + 10 * i, 50 + (i * 7) % 13))
            .collect()
    }

    /// The centred circular mean of each DS-0 row with half-width 1.
    fn centred_1(rows: &[Vec<f64>]) -> Vec<f64> {
        let p = rows.len();
        (0..p)
            .map(|i| (rows[(i + p - 1) % p][0] + rows[i][0] + rows[(i + 1) % p][0]) / 3.0)
            .collect()
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| (x - y).abs() <= 1e-12 * y.abs().max(1.0))
    }

    #[test]
    fn smoothing_pass_runs_once_after_command() {
        // hwsm10: version 0004, P = 10, stored window 0.3, so h = 1 and the
        // model is additive. The same file at 0003 uses 0.05 (h = 0) and
        // never smooths, so it shows the command without its pass.
        let temp = tempfile::tempdir().unwrap();
        let smoothed = hw_copy(temp.path(), "hwsm10", None);
        let plain = hw_copy(temp.path(), "hwsm10", Some(b"0003"));
        let flag = |path: &Path| {
            crate::inspect_rrd_file(path).unwrap().archives[1].cdp_prep[0]
                .word_u64(crate::holt_winters::cdp::INIT_FLAG)
        };
        let before = flag(&smoothed);
        // 30 PDPs pass SEASONAL's row 0 three times; six PDPs follow the
        // last trigger.
        let arguments = hwsm10_arguments(30);
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let (verbose, result) = update_rrd_text(&smoothed, None, &arguments, false);
        result.unwrap();
        let (plain_rows, result) = update_rrd_text(&plain, None, &arguments, false);
        result.unwrap();
        assert!(flag(&smoothed) >= before + 2, "two or more triggers");
        // The pass prints nothing: updatev shows what the plain file shows.
        assert_eq!(format!("{verbose:?}"), format!("{plain_rows:?}"));

        // One pass over the rows the last argument left.
        let seasonal = centred_1(&stored_rows(&plain, 1));
        let mean = seasonal.iter().sum::<f64>() / seasonal.len() as f64;
        let expected: Vec<f64> = seasonal.iter().map(|v| v - mean).collect();
        let got = |path: &Path, archive| -> Vec<f64> {
            stored_rows(path, archive)
                .iter()
                .map(|row| row[0])
                .collect()
        };
        assert!(
            close(&got(&smoothed, 1), &expected),
            "{:?}",
            got(&smoothed, 1)
        );
        assert!(close(
            &got(&smoothed, 2),
            &centred_1(&stored_rows(&plain, 2))
        ));
        let intercept = |path: &Path| {
            crate::inspect_rrd_file(path).unwrap().archives[0].cdp_prep[0]
                .word_f64(crate::holt_winters::cdp::INTERCEPT)
        };
        assert!(close(&[intercept(&smoothed)], &[intercept(&plain) + mean]));

        // Nothing else differs: later PDPs saw the rows and intercept as
        // they were before the pass.
        let info = crate::inspect_rrd_file(&plain).unwrap();
        let layout = Layout::new(&info);
        let (a, b) = (
            std::fs::read(&smoothed).unwrap(),
            std::fs::read(&plain).unwrap(),
        );
        let intercept_at = layout.cdp_entry(1, 0, 0) + 8 * crate::holt_winters::cdp::INTERCEPT;
        let season_rows =
            info.archives[1].data_offset as usize..info.archives[3].data_offset as usize;
        for (offset, (x, y)) in a.iter().zip(&b).enumerate() {
            let expected_change = (4..8).contains(&offset)
                || (intercept_at..intercept_at + 8).contains(&offset)
                || season_rows.contains(&offset);
            assert!(x == y || expected_change, "byte {offset} differs");
        }
    }

    #[test]
    fn failed_command_skips_smoothing_pass() {
        let temp = tempfile::tempdir().unwrap();
        let smoothed = hw_copy(temp.path(), "hwsm10", None);
        let plain = hw_copy(temp.path(), "hwsm10", Some(b"0003"));
        let mut arguments = hwsm10_arguments(30);
        arguments.push("1000000310:x".into());
        arguments.push("1000000320:5".into());
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let (rows, result) = update_rrd_text(&smoothed, None, &arguments, false);
        assert!(
            matches!(result, Err(StoreError::RrdExpression(_))),
            "{result:?}"
        );
        assert_eq!(rows.len(), 30 * 5);
        let (_, plain_result) = update_rrd_text(&plain, None, &arguments, false);
        assert!(plain_result.is_err());
        // UP-34 keeps the 30 applied arguments; no pass runs, so the files
        // differ only in the version text.
        let (mut a, b) = (
            std::fs::read(&smoothed).unwrap(),
            std::fs::read(&plain).unwrap(),
        );
        a[4..8].copy_from_slice(b"0003");
        assert!(a == b);
        assert_eq!(
            crate::inspect_rrd_file(&smoothed).unwrap().last_update,
            1_000_000_300
        );
    }

    #[test]
    fn quiet_and_verbose_write_same_hw_bytes() {
        let temp = tempfile::tempdir().unwrap();
        // hwsm10 smooths; hwruns gets runs of two and of many PDPs, gaps and
        // split updates.
        let mut runs = hwsm10_arguments(12);
        runs.extend([
            "1000000140:7".to_string(),
            "1000000185:U".to_string(),
            "1000000700:9.5".to_string(),
            "1000000712:3".to_string(),
        ]);
        for (name, arguments) in [("hwsm10", hwsm10_arguments(40)), ("hwruns", runs)] {
            let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
            let verbose = hw_copy(temp.path(), name, None);
            let quiet = temp.path().join(format!("{name}-quiet.rrd"));
            let raw = temp.path().join(format!("{name}-raw.rrd"));
            std::fs::copy(&verbose, &quiet).unwrap();
            std::fs::copy(&verbose, &raw).unwrap();
            let before = std::fs::read(&verbose).unwrap();
            let (rows, result) = update_rrd_text(&verbose, None, &arguments, false);
            result.unwrap();
            assert!(!rows.is_empty());
            update_rrd_text_quiet(&quiet, None, &arguments, false).unwrap();
            let updates: Vec<RrdRawUpdate<'_>> = arguments
                .iter()
                .map(|argument| {
                    let (time, value) = argument.split_once(':').unwrap();
                    RrdRawUpdate {
                        timestamp: time.parse().unwrap(),
                        timestamp_usec: 0,
                        values: vec![(value != "U").then_some(value)],
                    }
                })
                .collect();
            update_rrd_raw_batch(&raw, &updates, false).unwrap();
            let written = std::fs::read(&verbose).unwrap();
            assert_ne!(written, before, "{name}");
            assert!(std::fs::read(&quiet).unwrap() == written, "{name}: quiet");
            assert!(std::fs::read(&raw).unwrap() == written, "{name}: raw");
        }
    }

    #[test]
    fn classic_cdp_words_2_to_7_are_written_back() {
        // Compatibility: update changes only words 0, 1, 8 and 9 of a
        // classic archive's CDP entries; any other bytes stay as they were.
        // Provenance: specification (docs/compatibility/update.md UP-33).
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("words.rrd");
        create(&path);
        let info = crate::inspect_rrd_file(&path).unwrap();
        let layout = Layout::new(&info);
        let mut bytes = std::fs::read(&path).unwrap();
        let patterns: Vec<u64> = (2..8u64).map(|slot| 0x1111_1111_1111_1111 * slot).collect();
        for ds in 0..2 {
            let entry = layout.cdp_entry(2, 0, ds);
            for (slot, word) in (2..8).zip(&patterns) {
                put_u64(&mut bytes, entry + 8 * slot, *word);
            }
        }
        std::fs::write(&path, &bytes).unwrap();

        let (rows, result) =
            update_rrd_text(&path, None, &["1000000010:5:6", "1000000020:9:9"], false);
        result.unwrap();
        assert_eq!(rows.len(), 2);
        let after = std::fs::read(&path).unwrap();
        for ds in 0..2 {
            let entry = layout.cdp_entry(2, 0, ds);
            assert_eq!(
                after[entry + 16..entry + 64],
                bytes[entry + 16..entry + 64],
                "DS {ds}"
            );
            // Words 8 and 9 did change.
            assert_ne!(after[entry + 64..entry + 80], bytes[entry + 64..entry + 80]);
        }
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
        let summaries = update_rrd_raw_batch(&path, &[update], false).unwrap();
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

    #[test]
    fn microsecond_difference_is_an_integer_subtraction() {
        // Compatibility: the microsecond difference is subtracted as
        // integers before it becomes a double; RRDtool 1.11.0 stores a PDP
        // value of 2806.775807 here, where subtracting doubles gives
        // 2806.775808.
        // Provenance: black-box compatibility test (rrdtool 1.11.0, macOS
        // arm64); specification (docs/compatibility/update.md UP-14).
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usec.rrd");
        crate::create_rrd_file(
            &path,
            1_000_000_000,
            10,
            &["DS:a:GAUGE:9223372036854775807:U:U".to_string()],
            &["RRA:LAST:0.5:1:5".to_string()],
            false,
        )
        .unwrap();
        let (_, first) = update_rrd_text(&path, None, &["9223381260226809856:U"], false);
        let (_, second) = update_rrd_text(&path, None, &["inf:1"], false);
        if cfg!(target_arch = "aarch64") {
            first.unwrap();
            second.unwrap();
            let info = crate::inspect_rrd_file(&path).unwrap();
            let value = info.data_sources[0].pdp_value;
            assert_eq!(format!("{value:.6}"), "2806.775807", "{value}");
        } else {
            // Elsewhere the first time is refused (UP-9).
            assert!(first.is_err() || second.is_err(), "{first:?} {second:?}");
        }
    }

    #[test]
    fn raw_value_for_a_computed_source_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let raw = temp.path().join("raw.rrd");
        let text = temp.path().join("text.rrd");
        crate::create_rrd_file(
            &raw,
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
        std::fs::copy(&raw, &text).unwrap();

        let rows =
            update_rrd_raw_values_verbose(&raw, 1_000_000_010, &[Some("3"), Some("123")]).unwrap();
        let (expected, result) = update_rrd_text(&text, None, &["1000000010:3"], false);
        result.unwrap();
        assert_eq!(rows, expected);
        assert_eq!(rows[0].values, vec![3.0, 6.0]);
        let info = crate::inspect_rrd_file(&raw).unwrap();
        assert_eq!(info.data_sources[1].raw_last_value, b"U");
        assert_eq!(std::fs::read(&raw).unwrap(), std::fs::read(&text).unwrap());

        // A numeric value for it is ignored too.
        update_rrd_values(&raw, 1_000_000_020, &[Some(4.0), Some(99.0)]).unwrap();
        let info = crate::inspect_rrd_file(&raw).unwrap();
        assert_eq!(info.data_sources[1].raw_last_value, b"U");
    }
}
