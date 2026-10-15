//! `rrdtool tune` structure changes: adding and deleting data sources and
//! archives, resizing archives and changing the step.
//!
//! The required behavior is specified in docs/compatibility/tune-modify.md,
//! whose rule numbers (TM-n) are cited below. The file is read into an owned
//! [`Model`], the requested changes are validated into a [`Plan`], the plan is
//! applied to the model and the result is written to a new file that replaces
//! the original.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN,
    RrdFileLock, STAT_HEAD_LEN, VALUE_LEN, open_rrd_write, rrd_nan,
};
use crate::rrd_create::{ErrorSlot, parse_ds, parse_rra};
use crate::storage::StoreError;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const NAME_LEN: usize = 20;
const CF_LEN: usize = 20;
// Field offsets inside the fixed-size records (TM terms section).
const DS_TYPE_OFFSET: usize = 20;
const RRA_ROWS_OFFSET: usize = 24;
const RRA_PDP_OFFSET: usize = 32;
const PDP_UNKNOWN_SEC_OFFSET: usize = 32;
const PDP_VALUE_OFFSET: usize = 40;
const CDP_VALUE_OFFSET: usize = 0;
const CDP_UNKNOWN_OFFSET: usize = 8;
const CDP_PRIMARY_OFFSET: usize = 64;
const CDP_SECONDARY_OFFSET: usize = 72;
/// Largest row count `RRA#i:+N` may produce (TM-36).
const MAX_GROWN_ROWS: u64 = i32::MAX as u64;

/// How `RRA#index:<op><number>` changes an archive's row count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RrdRowChange {
    /// `=N`
    Set(u32),
    /// `+N`
    Grow(u32),
    /// `-N`
    Shrink(u32),
}

/// One `tune` operation word, already classified and, for `RRA#` and
/// `DELRRA:`, parsed (TM-8 to TM-13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RrdStructureOp {
    /// `DEL:name`
    DeleteSource(String),
    /// `DS:spec`, holding the text after `DS:`.
    AddSource(String),
    /// `DELRRA:index`
    DeleteArchive(u32),
    /// `RRA#index:<op><number>`
    ResizeArchive { index: u32, change: RrdRowChange },
    /// `RRA:spec`, holding the text after `RRA:`.
    AddArchive(String),
}

/// Applies `ops` to the RRD file at `path` and replaces the file with the
/// result. `new_step` is the `--step` value read as C `atoi`; zero or less
/// leaves the step alone (TM-49).
pub fn modify_rrd_file(
    path: impl AsRef<Path>,
    ops: &[RrdStructureOp],
    new_step: i32,
) -> Result<(), StoreError> {
    let path = path.as_ref();
    // TM-56: the original stays locked from open until the replacement.
    let mut file = RrdFileLock::exclusive(open_rrd_write(path)?)?;
    file.seek(SeekFrom::Start(0))?;
    let mut input = Vec::new();
    file.read_to_end(&mut input)?;
    let output = restructure(&input, ops, new_step, usize::MAX)?;
    replace_file(path, &file, &output)?;
    drop(file);
    Ok(())
}

/// The whole change on file contents, without touching the file system.
/// `max_bytes` bounds the size of any file this builds.
fn restructure(
    input: &[u8],
    ops: &[RrdStructureOp],
    new_step: i32,
    max_bytes: usize,
) -> Result<Vec<u8>, StoreError> {
    let model = Model::parse(input)?;
    let plan = Plan::new(&model, ops, new_step)?;
    // TM-51: a step change first rewrites the file with no operations and
    // stretches it; the operations then apply to that file.
    let model = match plan.stretch {
        Some(factor) => model.rotated()?.stretched(factor)?,
        None => model,
    };
    model.apply(&plan, max_bytes)?.to_bytes(max_bytes)
}

fn invalid(message: &str) -> StoreError {
    StoreError::RrdFormat(message.into())
}

fn out_of_memory() -> StoreError {
    StoreError::Rrd("out of memory".into())
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut word = [0_u8; 8];
    word.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(word)
}

fn set_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_f64(bytes: &[u8], offset: usize) -> f64 {
    f64::from_bits(get_u64(bytes, offset))
}

fn set_f64(bytes: &mut [u8], offset: usize, value: f64) {
    set_u64(bytes, offset, value.to_bits());
}

/// The bytes of a NUL-padded text field up to its first NUL.
fn field_text(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    &field[..end]
}

fn unknown() -> u64 {
    rrd_nan().to_bits()
}

/// Reserves room for `count` items or reports the change as too large.
fn reserve<T>(vec: &mut Vec<T>, count: usize) -> Result<(), StoreError> {
    vec.try_reserve_exact(count).map_err(|_| out_of_memory())
}

/// Sequential reader over the input file that never reads past its end.
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], StoreError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid("file is shorter than its header says"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn records<const N: usize>(&mut self, count: usize) -> Result<Vec<[u8; N]>, StoreError> {
        let length = count
            .checked_mul(N)
            .ok_or_else(|| invalid("record count overflows"))?;
        Ok(self.take(length)?.as_chunks::<N>().0.to_vec())
    }
}

#[derive(Clone, Debug)]
struct Source {
    def: [u8; DS_DEF_LEN],
    pdp: [u8; PDP_PREP_LEN],
}

impl Source {
    fn name(&self) -> &[u8] {
        field_text(&self.def[..NAME_LEN])
    }

    fn unknown_seconds(&self) -> u64 {
        get_u64(&self.pdp, PDP_UNKNOWN_SEC_OFFSET)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cf {
    Average,
    Min,
    Max,
    Last,
}

impl Cf {
    fn from_field(field: &[u8]) -> Option<Self> {
        match field_text(field) {
            b"AVERAGE" => Some(Self::Average),
            b"MIN" => Some(Self::Min),
            b"MAX" => Some(Self::Max),
            b"LAST" => Some(Self::Last),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct Archive {
    def: [u8; RRA_DEF_LEN],
    cf: Cf,
    /// One consolidation state record per data source.
    cdp: Vec<[u8; CDP_PREP_LEN]>,
    cur_row: u64,
    /// Rows in storage order, one value per data source, as raw bits so
    /// copied NaNs keep their sign and payload.
    values: Vec<u64>,
}

impl Archive {
    fn rows(&self) -> u64 {
        get_u64(&self.def, RRA_ROWS_OFFSET)
    }

    fn pdp_per_row(&self) -> u64 {
        get_u64(&self.def, RRA_PDP_OFFSET)
    }

    fn value(&self, row: u64, source: usize, sources: usize) -> f64 {
        usize::try_from(row)
            .ok()
            .and_then(|row| row.checked_mul(sources))
            .and_then(|start| start.checked_add(source))
            .and_then(|index| self.values.get(index))
            .map_or(f64::NAN, |bits| f64::from_bits(*bits))
    }
}

/// An RRD file held in memory.
#[derive(Clone, Debug)]
struct Model {
    version: [u8; 4],
    step: u64,
    live: [u8; LIVE_HEAD_LEN],
    sources: Vec<Source>,
    archives: Vec<Archive>,
}

impl Model {
    fn parse(bytes: &[u8]) -> Result<Self, StoreError> {
        let mut cursor = Cursor { bytes, position: 0 };
        let head = cursor.take(STAT_HEAD_LEN)?;
        if &head[..4] != b"RRD\0" {
            return Err(invalid("not an RRD file"));
        }
        let mut version = [0_u8; 4];
        version.copy_from_slice(&head[4..8]);
        // Only the layouts with a 16-byte last-update record are handled.
        if !matches!(&version, b"0003" | b"0004" | b"0005") || head[8] != 0 {
            return Err(StoreError::RrdUnsupported(format!(
                "tune cannot change the structure of RRD version {}",
                String::from_utf8_lossy(field_text(&head[4..9]))
            )));
        }
        if get_f64(head, 16) != FLOAT_COOKIE {
            return Err(invalid("float cookie mismatch"));
        }
        let count =
            |offset| usize::try_from(get_u64(head, offset)).map_err(|_| invalid("count too large"));
        let (source_count, archive_count) = (count(24)?, count(32)?);
        let step = get_u64(head, 40);
        if step == 0 || step > i64::MAX as u64 {
            return Err(invalid("step out of range"));
        }
        let source_defs = cursor.records::<DS_DEF_LEN>(source_count)?;
        let archive_defs = cursor.records::<RRA_DEF_LEN>(archive_count)?;
        let mut live = [0_u8; LIVE_HEAD_LEN];
        live.copy_from_slice(cursor.take(LIVE_HEAD_LEN)?);
        let pdp = cursor.records::<PDP_PREP_LEN>(source_count)?;
        let cdp_count = source_count
            .checked_mul(archive_count)
            .ok_or_else(|| invalid("record count overflows"))?;
        let mut cdp = cursor.records::<CDP_PREP_LEN>(cdp_count)?.into_iter();
        let pointers = cursor.records::<RRA_PTR_LEN>(archive_count)?;

        let sources: Vec<Source> = source_defs
            .into_iter()
            .zip(pdp)
            .map(|(def, pdp)| Source { def, pdp })
            .collect();
        if sources.iter().any(|source| {
            field_text(&source.def[DS_TYPE_OFFSET..DS_TYPE_OFFSET + 20]) == b"COMPUTE"
        }) {
            return Err(StoreError::Rrd(
                "COMPUTE data sources are not supported by Rondi".into(),
            ));
        }
        let mut archives = Vec::with_capacity(archive_count);
        for (def, pointer) in archive_defs.into_iter().zip(pointers) {
            let cf = Cf::from_field(&def[..CF_LEN]).ok_or_else(|| {
                StoreError::Rrd(format!(
                    "consolidation function {} is not supported by Rondi",
                    String::from_utf8_lossy(field_text(&def[..CF_LEN]))
                ))
            })?;
            let rows = get_u64(&def, RRA_ROWS_OFFSET);
            let pdp_per_row = get_u64(&def, RRA_PDP_OFFSET);
            let cur_row = u64::from_le_bytes(pointer);
            if rows == 0 || cur_row >= rows {
                return Err(invalid("archive row pointer out of range"));
            }
            if pdp_per_row == 0
                || pdp_per_row
                    .checked_mul(step)
                    .is_none_or(|slot| slot > i64::MAX as u64)
            {
                return Err(invalid("archive slot length out of range"));
            }
            archives.push(Archive {
                def,
                cf,
                cdp: cdp.by_ref().take(source_count).collect(),
                cur_row,
                values: Vec::new(),
            });
        }
        // Row data follows all the pointers.
        for archive in &mut archives {
            let value_count = usize::try_from(archive.rows())
                .ok()
                .and_then(|rows| rows.checked_mul(source_count))
                .and_then(|count| count.checked_mul(VALUE_LEN))
                .ok_or_else(|| invalid("archive too large"))?;
            archive.values = cursor
                .take(value_count)?
                .as_chunks::<VALUE_LEN>()
                .0
                .iter()
                .map(|word| u64::from_le_bytes(*word))
                .collect();
        }
        // TM-30: anything after the last row is dropped.
        Ok(Self {
            version,
            step,
            live,
            sources,
            archives,
        })
    }

    fn last_update(&self) -> i64 {
        i64::from_le_bytes(self.live[..8].try_into().unwrap_or_default())
    }

    fn to_bytes(&self, max_bytes: usize) -> Result<Vec<u8>, StoreError> {
        let sources = self.sources.len();
        let values: usize = self
            .archives
            .iter()
            .map(|archive| archive.values.len())
            .sum();
        let size = [
            Some(STAT_HEAD_LEN),
            sources.checked_mul(DS_DEF_LEN + PDP_PREP_LEN),
            self.archives.len().checked_mul(RRA_DEF_LEN + RRA_PTR_LEN),
            Some(LIVE_HEAD_LEN),
            sources
                .checked_mul(self.archives.len())
                .and_then(|count| count.checked_mul(CDP_PREP_LEN)),
            values.checked_mul(VALUE_LEN),
        ]
        .into_iter()
        .try_fold(0_usize, |total, part| total.checked_add(part?))
        .filter(|size| *size <= max_bytes)
        .ok_or_else(out_of_memory)?;
        let mut bytes = Vec::new();
        reserve(&mut bytes, size)?;
        // TM-20: only the cookies, version, counts and step are set.
        let mut head = [0_u8; STAT_HEAD_LEN];
        head[..4].copy_from_slice(b"RRD\0");
        head[4..8].copy_from_slice(&self.version);
        set_f64(&mut head, 16, FLOAT_COOKIE);
        set_u64(&mut head, 24, sources as u64);
        set_u64(&mut head, 32, self.archives.len() as u64);
        set_u64(&mut head, 40, self.step);
        bytes.extend_from_slice(&head);
        for source in &self.sources {
            bytes.extend_from_slice(&source.def);
        }
        for archive in &self.archives {
            bytes.extend_from_slice(&archive.def);
        }
        bytes.extend_from_slice(&self.live);
        for source in &self.sources {
            bytes.extend_from_slice(&source.pdp);
        }
        for archive in &self.archives {
            for record in &archive.cdp {
                bytes.extend_from_slice(record);
            }
        }
        for archive in &self.archives {
            bytes.extend_from_slice(&archive.cur_row.to_le_bytes());
        }
        for archive in &self.archives {
            for value in &archive.values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        Ok(bytes)
    }

    /// Every archive with its rows oldest first and `cur_row` on the last
    /// row (TM-27), as a rewrite with no operations leaves it.
    fn rotated(mut self) -> Result<Self, StoreError> {
        let sources = self.sources.len();
        for archive in &mut self.archives {
            let oldest = (archive.cur_row + 1) % archive.rows();
            // Bounded by the row data already held.
            let shift = usize::try_from(oldest).map_err(|_| out_of_memory())? * sources;
            archive.values.rotate_left(shift);
            archive.cur_row = archive.rows() - 1;
        }
        Ok(self)
    }

    /// TM-51: divides the step by `factor` and multiplies each archive's
    /// PDPs per row and unknown PDP counts by it, so slot lengths stay the
    /// same.
    fn stretched(mut self, factor: u64) -> Result<Self, StoreError> {
        self.step /= factor;
        for archive in &mut self.archives {
            let pdp_per_row = archive
                .pdp_per_row()
                .checked_mul(factor)
                .ok_or_else(|| invalid("archive slot length out of range"))?;
            set_u64(&mut archive.def, RRA_PDP_OFFSET, pdp_per_row);
            for record in &mut archive.cdp {
                let pdps = get_u64(record, CDP_UNKNOWN_OFFSET).wrapping_mul(factor);
                set_u64(record, CDP_UNKNOWN_OFFSET, pdps);
            }
        }
        Ok(self)
    }

    fn slot(&self, archive: &Archive) -> i128 {
        i128::from(archive.pdp_per_row()) * i128::from(self.step)
    }

    /// TM-39: the archives back-fill may read for a target CF, in the order
    /// they are tried.
    fn candidates(&self, target: Cf) -> Vec<usize> {
        let mut list: Vec<usize> = (0..self.archives.len())
            .filter(|index| {
                matches!(self.archives[*index].cf, Cf::Average)
                    || self.archives[*index].cf == target
            })
            .collect();
        let key = |index: usize| {
            let archive = &self.archives[index];
            (archive.pdp_per_row(), std::cmp::Reverse(archive.rows()))
        };
        order_candidates(&mut list, key);
        list
    }

    /// TM-41: the row of `archive` covering time `t`, if any.
    fn row_at(&self, archive: &Archive, t: i128) -> Option<u64> {
        let slot = self.slot(archive);
        let last = i128::from(self.last_update());
        let rounded = t.div_euclid(slot) * slot + if t.rem_euclid(slot) == 0 { 0 } else { slot };
        if rounded > last {
            return None;
        }
        let newest_end = last - last.rem_euclid(slot);
        let age = (newest_end - rounded) / slot;
        let rows = i128::from(archive.rows());
        if age >= rows {
            return None;
        }
        u64::try_from((i128::from(archive.cur_row) - age).rem_euclid(rows)).ok()
    }

    /// TM-41: the candidate rows read for the target interval ending at
    /// `end` with slot length `slot`.
    fn range_for(&self, archive: &Archive, end: i128, slot: i128) -> Option<RowRange> {
        let start = end - slot + 1;
        let rows = archive.rows();
        let (first, last) = match (self.row_at(archive, start), self.row_at(archive, end)) {
            (None, None) => return None,
            // Compatibility: with only the end covered, reading starts at
            // the newest row, which lies after the interval.
            // Provenance: reference-source-inspection; see
            // docs/compatibility/tune-modify.md (TM-41).
            (None, Some(last)) => (archive.cur_row, last),
            // Compatibility: the row before the newest is taken as an
            // unsigned 64-bit decrement, so from row 0 it is
            // (2^64 - 1) mod rows rather than the last row.
            // Provenance: reference-source-inspection; see
            // docs/compatibility/tune-modify.md (TM-41).
            (Some(first), None) => (first, archive.cur_row.wrapping_sub(1) % rows),
            (Some(first), Some(last)) => (first, last),
        };
        Some(RowRange::new(first, last, rows))
    }

    /// TM-42: one back-filled value for input source `source`, from the
    /// first candidate whose range holds a known value.
    fn fill_value(&self, target: Cf, ranges: &[(usize, RowRange)], source: usize) -> Option<f64> {
        let sources = self.sources.len();
        ranges.iter().find_map(|(index, range)| {
            let archive = &self.archives[*index];
            let slot = self.slot(archive) as f64;
            let mut known = range
                .rows(archive.rows())
                .map(|row| archive.value(row, source, sources))
                .filter(|value| !value.is_nan());
            let first = known.next()?;
            Some(match target {
                Cf::Average => {
                    let mut total = first * slot;
                    let mut count = 1_u64;
                    for value in known {
                        total = accumulate(value, slot, total);
                        count += 1;
                    }
                    total / (slot * count as f64)
                }
                Cf::Min => known.fold(first, |low, value| if value < low { value } else { low }),
                Cf::Max => known.fold(first, |high, value| if value > high { value } else { high }),
                Cf::Last => known.last().unwrap_or(first),
            })
        })
    }

    /// TM-38 to TM-42: `count` new rows, oldest first, for a target archive
    /// with `rows` rows, slot length `slot` and row pointer `pointer`.
    /// `columns` maps each output source to its input source, if any.
    #[allow(clippy::too_many_arguments)]
    fn backfill(
        &self,
        target: Cf,
        slot: i128,
        rows: u64,
        pointer: u64,
        count: u64,
        columns: &[Option<usize>],
        out: &mut Vec<u64>,
    ) {
        let nan = unknown();
        let candidates = if columns.iter().any(Option::is_some) {
            self.candidates(target)
        } else {
            Vec::new()
        };
        let last = i128::from(self.last_update());
        let newest_end = last - last.rem_euclid(slot);
        for new_row in 0..count {
            // TM-40: new row r ends ((c - r) mod rows) slots before the newest.
            let back = pointer.wrapping_sub(new_row) % rows;
            let end = newest_end - i128::from(back) * slot;
            let ranges: Vec<(usize, RowRange)> = candidates
                .iter()
                .filter_map(|index| {
                    self.range_for(&self.archives[*index], end, slot)
                        .map(|range| (*index, range))
                })
                .collect();
            for column in columns {
                let value = column
                    .and_then(|source| self.fill_value(target, &ranges, source))
                    .map_or(nan, f64::to_bits);
                out.push(value);
            }
        }
    }

    /// Applies a validated plan, producing the rewritten file (TM-20 to
    /// TM-48).
    fn apply(&self, plan: &Plan, max_bytes: usize) -> Result<Self, StoreError> {
        let nan = unknown();
        let input_sources = self.sources.len();
        let last = self.last_update();

        // TM-23, TM-25: kept sources in order, then the added ones.
        let mut columns: Vec<Option<usize>> = Vec::new();
        let mut sources = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            if !plan
                .deleted_sources
                .iter()
                .any(|name| name.as_bytes() == source.name())
            {
                columns.push(Some(index));
                sources.push(source.clone());
            }
        }
        for def in &plan.added_sources {
            let mut pdp = [0_u8; PDP_PREP_LEN];
            pdp[0] = b'U';
            set_u64(&mut pdp, PDP_UNKNOWN_SEC_OFFSET, (last as u64) % self.step);
            set_f64(&mut pdp, PDP_VALUE_OFFSET, 0.0);
            columns.push(None);
            sources.push(Source { def: *def, pdp });
        }
        let width = sources.len();

        let kept: Vec<(usize, u64)> = plan
            .archive_rows
            .iter()
            .enumerate()
            .filter_map(|(index, rows)| rows.map(|rows| (index, rows)))
            .collect();
        let output_rows: Vec<u64> = kept
            .iter()
            .map(|(_, rows)| *rows)
            .chain(
                plan.added_archives
                    .iter()
                    .map(|def| get_u64(def, RRA_ROWS_OFFSET)),
            )
            .collect();
        let row_budget = max_bytes / VALUE_LEN;
        let mut budget_used = 0_usize;
        let mut charge = |rows: u64| -> Result<usize, StoreError> {
            let values = usize::try_from(rows)
                .ok()
                .and_then(|rows| rows.checked_mul(width))
                .ok_or_else(out_of_memory)?;
            budget_used = budget_used
                .checked_add(values)
                .filter(|used| *used <= row_budget)
                .ok_or_else(out_of_memory)?;
            Ok(values)
        };

        // TM-35: the input's rows as one sequence, archive after archive.
        let stream: Vec<u64> = self
            .archives
            .iter()
            .flat_map(|archive| archive.values.iter().copied())
            .collect();
        let mut stream_position = 0_usize;
        let mut archives = Vec::with_capacity(output_rows.len());
        for (index, new_rows) in &kept {
            let archive = &self.archives[*index];
            let old_rows = archive.rows();
            let mut values = Vec::new();
            reserve(&mut values, charge(*new_rows)?)?;

            // Compatibility: a kept archive's rows are read at the offset
            // the kept archives before it would have, not its own offset,
            // so a deletion shifts every later archive's rows.
            // Provenance: reference-source-inspection; see
            // docs/compatibility/tune-modify.md (TM-35).
            let length = archive.values.len();
            let mut old = stream
                .get(stream_position..stream_position + length)
                .map(<[u64]>::to_vec)
                .unwrap_or_else(|| archive.values.clone());
            stream_position += length;
            let oldest = usize::try_from((archive.cur_row + 1) % old_rows).unwrap_or(0);
            old.rotate_left(oldest * input_sources);

            let keep_rows = old_rows.min(*new_rows);
            if *new_rows > old_rows && width > 0 {
                // Compatibility: new row times use the last row of whichever
                // archive ends up at this archive's input position in the
                // output, which is another archive after a deletion. With
                // none there, this archive's own last row is used.
                // Provenance: reference-source-inspection; see
                // docs/compatibility/tune-modify.md (TM-40).
                let pointer = output_rows.get(*index).copied().unwrap_or(*new_rows) - 1;
                self.backfill(
                    archive.cf,
                    self.slot(archive),
                    *new_rows,
                    pointer,
                    *new_rows - old_rows,
                    &columns,
                    &mut values,
                );
            }
            if width > 0 {
                let skip = usize::try_from(old_rows - keep_rows).unwrap_or(0);
                for row in skip..usize::try_from(old_rows).unwrap_or(0) {
                    for column in &columns {
                        values.push(match column {
                            Some(source) => old[row * input_sources + source],
                            None => nan,
                        });
                    }
                }
            }

            let mut def = archive.def;
            set_u64(&mut def, RRA_ROWS_OFFSET, *new_rows);
            // TM-28: kept sources keep their state; added ones start empty.
            let cdp = columns
                .iter()
                .zip(&sources)
                .map(|(column, source)| match column {
                    Some(source) => archive.cdp[*source],
                    None => self.empty_cdp(archive, source),
                })
                .collect();
            archives.push(Archive {
                def,
                cf: archive.cf,
                cdp,
                cur_row: new_rows - 1,
                values,
            });
        }

        for def in &plan.added_archives {
            let cf = Cf::from_field(&def[..CF_LEN]).ok_or_else(|| invalid("bad CF"))?;
            let rows = get_u64(def, RRA_ROWS_OFFSET);
            let pdp_per_row = get_u64(def, RRA_PDP_OFFSET);
            let slot = i128::from(pdp_per_row) * i128::from(self.step);
            if rows == 0 || slot == 0 || slot > i128::from(i64::MAX) {
                return Err(invalid("archive slot length out of range"));
            }
            let mut values = Vec::new();
            reserve(&mut values, charge(rows)?)?;
            if width > 0 {
                self.backfill(cf, slot, rows, rows - 1, rows, &columns, &mut values);
            }
            archives.push(Archive {
                def: *def,
                cf,
                cdp: Vec::new(),
                cur_row: rows - 1,
                values,
            });
        }

        // TM-45 to TM-47: added archives' consolidation state reads the new
        // row data, so it is prepared once every row is in place.
        let new_stream: Vec<u64> = archives
            .iter()
            .flat_map(|archive| archive.values.iter().copied())
            .collect();
        let mut rows_before = kept
            .iter()
            .fold(0_u64, |total, (_, rows)| total.wrapping_add(*rows));
        for archive in archives.iter_mut().skip(kept.len()) {
            archive.cdp = self.added_cdp(archive, &columns, &sources, rows_before, &new_stream);
            rows_before = rows_before.wrapping_add(archive.rows());
        }

        Ok(Self {
            version: self.version,
            step: self.step,
            live: self.live,
            sources,
            archives,
        })
    }

    /// TM-28 and TM-45: the state of a source with no consolidated data.
    fn empty_cdp(&self, archive: &Archive, source: &Source) -> [u8; CDP_PREP_LEN] {
        let slot = archive.pdp_per_row().wrapping_mul(self.step).max(1);
        let elapsed = (self.last_update() as u64).wrapping_sub(source.unknown_seconds());
        let mut record = [0_u8; CDP_PREP_LEN];
        set_u64(&mut record, CDP_VALUE_OFFSET, unknown());
        set_u64(
            &mut record,
            CDP_UNKNOWN_OFFSET,
            (elapsed % slot) / self.step,
        );
        record
    }

    /// TM-45 to TM-47: consolidation state for an added archive.
    fn added_cdp(
        &self,
        target: &Archive,
        columns: &[Option<usize>],
        sources: &[Source],
        rows_before: u64,
        new_stream: &[u64],
    ) -> Vec<[u8; CDP_PREP_LEN]> {
        let chosen = self
            .candidates(target.cf)
            .into_iter()
            .find(|index| {
                let archive = &self.archives[*index];
                archive.pdp_per_row() == 1 || archive.cf == Cf::Average
            })
            .map(|index| &self.archives[index]);
        let width = columns.len() as u64;
        let input_sources = self.sources.len();
        let slot = self.slot(target);
        let last = i128::from(self.last_update());
        let end = if last.rem_euclid(slot) == 0 {
            last
        } else {
            last - last.rem_euclid(slot) + slot
        };
        columns
            .iter()
            .zip(sources)
            .enumerate()
            .map(|(output_index, (column, source))| {
                let (Some(candidate), Some(input_index)) = (chosen, column) else {
                    return self.empty_cdp(target, source);
                };
                let mut record = [0_u8; CDP_PREP_LEN];
                // Compatibility: the primary value is read at a position
                // that counts earlier archives in rows rather than values,
                // so with several sources it usually comes from elsewhere.
                // Provenance: reference-source-inspection; see
                // docs/compatibility/tune-modify.md (TM-46).
                let primary = rows_before
                    .checked_add(width.wrapping_mul(target.rows().wrapping_sub(1)))
                    .and_then(|position| position.checked_add(output_index as u64))
                    .and_then(|position| usize::try_from(position).ok())
                    .and_then(|position| new_stream.get(position))
                    .copied()
                    .unwrap_or_else(unknown);
                set_u64(&mut record, CDP_PRIMARY_OFFSET, primary);
                let rows = candidate.rows();
                match self.row_at(candidate, end - slot + 1) {
                    None => {
                        let previous = get_u64(&candidate.cdp[*input_index], CDP_PRIMARY_OFFSET);
                        set_u64(&mut record, CDP_SECONDARY_OFFSET, previous);
                    }
                    Some(first) => {
                        let before = first.checked_sub(1).unwrap_or(rows - 1);
                        let secondary = candidate.value(before, *input_index, input_sources);
                        set_f64(&mut record, CDP_SECONDARY_OFFSET, secondary);
                        // Compatibility: the folded value starts at 0.0, not
                        // unknown, so MIN and MAX compare against zero.
                        // Provenance: reference-source-inspection; see
                        // docs/compatibility/tune-modify.md (TM-47).
                        let mut value = 0.0_f64;
                        let mut unknown_pdps = 0_u64;
                        for row in RowRange::new(first, candidate.cur_row, rows).rows(rows) {
                            let sample = candidate.value(row, *input_index, input_sources);
                            if sample.is_nan() {
                                unknown_pdps += 1;
                                continue;
                            }
                            value = match target.cf {
                                Cf::Average => value + sample,
                                Cf::Min => {
                                    if sample < value {
                                        sample
                                    } else {
                                        value
                                    }
                                }
                                Cf::Max => {
                                    if sample > value {
                                        sample
                                    } else {
                                        value
                                    }
                                }
                                Cf::Last => sample,
                            };
                        }
                        set_f64(&mut record, CDP_VALUE_OFFSET, value);
                        set_u64(&mut record, CDP_UNKNOWN_OFFSET, unknown_pdps);
                    }
                }
                record
            })
            .collect()
    }
}

/// A run of rows read forward from `first`, wrapping at the row count.
#[derive(Clone, Copy, Debug)]
struct RowRange {
    first: u64,
    length: u64,
}

impl RowRange {
    /// TM-41: `last - first + 1` rows, plus the row count when negative.
    fn new(first: u64, last: u64, rows: u64) -> Self {
        let span = i128::from(last) - i128::from(first) + 1;
        let span = if span < 0 {
            span + i128::from(rows)
        } else {
            span
        };
        Self {
            first,
            length: u64::try_from(span).unwrap_or(0),
        }
    }

    fn rows(self, rows: u64) -> impl Iterator<Item = u64> {
        (0..self.length).map(move |step| {
            ((u128::from(self.first) + u128::from(step)) % u128::from(rows)) as u64
        })
    }
}

/// Adds `value * slot` to `total` the way the reference build on this
/// platform rounds it (TM-43).
#[inline]
fn accumulate(value: f64, slot: f64, total: f64) -> f64 {
    // Compatibility: the macOS arm64 build fuses the multiply and add;
    // the Linux builds round the product first.
    // Provenance: black-box compatibility test.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        value.mul_add(slot, total)
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        value * slot + total
    }
}

/// Orders back-fill candidates by fewer PDPs per row, then more rows. Equal
/// keys have no defined order in the reference (TM-44).
fn order_candidates(list: &mut [usize], key: impl Fn(usize) -> (u64, std::cmp::Reverse<u64>)) {
    #[cfg(target_os = "macos")]
    {
        // Compatibility: ties fall where the platform C library's qsort
        // leaves them, so the same sort is asked for.
        // Provenance: black-box compatibility test.
        #[repr(C)]
        struct Entry {
            pdp_per_row: u64,
            rows: u64,
            index: usize,
        }
        extern "C" fn compare(a: *const libc::c_void, b: *const libc::c_void) -> libc::c_int {
            // SAFETY: qsort passes pointers to two elements of the Entry
            // slice below.
            let (a, b) = unsafe { (&*(a as *const Entry), &*(b as *const Entry)) };
            match a.pdp_per_row.cmp(&b.pdp_per_row).then(b.rows.cmp(&a.rows)) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        }
        let mut entries: Vec<Entry> = list
            .iter()
            .map(|index| {
                let (pdp_per_row, std::cmp::Reverse(rows)) = key(*index);
                Entry {
                    pdp_per_row,
                    rows,
                    index: *index,
                }
            })
            .collect();
        // SAFETY: the pointer, count and element size describe `entries`,
        // and the comparator only reads the two elements it is given.
        unsafe {
            libc::qsort(
                entries.as_mut_ptr().cast(),
                entries.len(),
                std::mem::size_of::<Entry>(),
                Some(compare),
            );
        }
        for (slot, entry) in list.iter_mut().zip(entries) {
            *slot = entry.index;
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        list.sort_by_key(|index| key(*index));
    }
}

/// The checked form of the requested change.
#[derive(Debug)]
struct Plan {
    /// TM-51: the factor the step is divided by.
    stretch: Option<u64>,
    deleted_sources: Vec<String>,
    added_sources: Vec<[u8; DS_DEF_LEN]>,
    /// The new row count of each input archive, or None when it is deleted.
    archive_rows: Vec<Option<u64>>,
    added_archives: Vec<[u8; RRA_DEF_LEN]>,
}

impl Plan {
    /// Checks the change in the order of TM-17 and reports the first
    /// failure.
    fn new(model: &Model, ops: &[RrdStructureOp], new_step: i32) -> Result<Self, StoreError> {
        // 1. --step (TM-49, TM-50).
        let stretch = match u64::try_from(new_step) {
            Ok(new_step) if new_step > 0 => {
                if !model.step.is_multiple_of(new_step) || model.step / new_step <= 1 {
                    return Err(StoreError::Rrd(
                        "invalid 'newstep' parameter. The newsize must divide the old step parameter without a remainder.".into(),
                    ));
                }
                Some(model.step / new_step)
            }
            _ => None,
        };
        let step = stretch.map_or(model.step, |factor| model.step / factor);

        // 2. Data sources. A definition that fails to parse is reported only
        // if nothing later fails.
        // Compatibility: a bad DS: word gives way to a later RRA: parse
        // error, and a failed definition takes no part in the duplicate
        // check (TM-19 asks for its own parse error instead).
        // Provenance: black-box compatibility test.
        let deleted_sources: Vec<String> = ops
            .iter()
            .filter_map(|op| match op {
                RrdStructureOp::DeleteSource(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        let mut names: Vec<Vec<u8>> = model
            .sources
            .iter()
            .map(|source| source.name().to_vec())
            .filter(|name| {
                !deleted_sources
                    .iter()
                    .any(|deleted| deleted.as_bytes() == name)
            })
            .collect();
        let mut source_error = None;
        let mut added_sources = Vec::new();
        for op in ops {
            let RrdStructureOp::AddSource(spec) = op else {
                continue;
            };
            let mut def = [0_u8; DS_DEF_LEN];
            let mut error = ErrorSlot::default();
            // tune keeps the file's version whatever the type (TM-21).
            let mut version = "0003";
            parse_ds(spec, &mut def, &mut version, &mut error);
            if let Some(message) = error.take() {
                source_error.get_or_insert(message);
                continue;
            }
            let name = field_text(&def[..NAME_LEN]).to_vec();
            if names.contains(&name) {
                return Err(StoreError::Rrd(format!(
                    "Duplicate DS name: {}",
                    String::from_utf8_lossy(&name)
                )));
            }
            names.push(name);
            added_sources.push(def);
        }

        // 3. Archive resizes and deletions (TM-14, TM-16, TM-31, TM-36, TM-37).
        let mut archive_rows = Vec::with_capacity(model.archives.len());
        for (position, archive) in model.archives.iter().enumerate() {
            let old_rows = archive.rows();
            let first = ops.iter().find_map(|op| match op {
                RrdStructureOp::DeleteArchive(index) if *index as usize == position => Some(None),
                RrdStructureOp::ResizeArchive { index, change }
                    if *index <= i32::MAX as u32 && *index as usize == position =>
                {
                    Some(Some(*change))
                }
                _ => None,
            });
            let rows = match first {
                None => Some(old_rows),
                Some(None) => None,
                Some(Some(change)) => new_row_count(position, old_rows, change)?,
            };
            archive_rows.push(rows);
        }

        // 4. Added archives, with durations read against the new step.
        let mut added_archives = Vec::new();
        for op in ops {
            let RrdStructureOp::AddArchive(spec) = op else {
                continue;
            };
            let mut def = [0_u8; RRA_DEF_LEN];
            let mut error = ErrorSlot::default();
            parse_rra(&format!("RRA:{spec}"), &mut def, step, &mut error);
            if let Some(message) = error.take() {
                return Err(StoreError::Rrd(message));
            }
            added_archives.push(def);
        }

        if let Some(message) = source_error {
            return Err(StoreError::Rrd(message));
        }

        let plan = Self {
            stretch,
            deleted_sources,
            added_sources,
            archive_rows,
            added_archives,
        };
        plan.check_platform(model)?;
        Ok(plan)
    }

    /// TM-57: on Linux the reference cannot drop every data source from a
    /// file in some layouts. The outcomes are reproduced as a rule.
    fn check_platform(&self, model: &Model) -> Result<(), StoreError> {
        if !cfg!(target_os = "linux") || model.sources.is_empty() {
            return Ok(());
        }
        let remaining = model
            .sources
            .iter()
            .filter(|source| {
                !self
                    .deleted_sources
                    .iter()
                    .any(|name| name.as_bytes() == source.name())
            })
            .count()
            + self.added_sources.len();
        if remaining > 0 {
            return Ok(());
        }
        // Compatibility: glibc returns no buffer for a zero-size resize,
        // which the reference reports as one of two allocation errors.
        // Provenance: black-box compatibility test.
        let kept = self.archive_rows.iter().flatten().count();
        let added = self.added_archives.len();
        match (kept, added) {
            (2.., _) => Err(StoreError::Rrd("Cannot allocate memory".into())),
            (_, 1..) => Err(out_of_memory()),
            _ => Ok(()),
        }
    }
}

/// TM-31, TM-36, TM-37: the row count after `RRA#position:<change>`, or
/// None when the archive is deleted.
fn new_row_count(
    position: usize,
    old_rows: u64,
    change: RrdRowChange,
) -> Result<Option<u64>, StoreError> {
    // Row counts are 32-bit signed quantities in the command's arithmetic.
    let old = old_rows as u32;
    let rows = match change {
        RrdRowChange::Set(rows) => rows as i32,
        RrdRowChange::Grow(added) => {
            // Compatibility: RRDtool crashes here; Rondi reports an error.
            // Provenance: black-box compatibility test.
            if old_rows.saturating_add(u64::from(added)) > MAX_GROWN_ROWS {
                return Err(out_of_memory());
            }
            old.wrapping_add(added) as i32
        }
        RrdRowChange::Shrink(removed) => {
            let rows = old.wrapping_sub(removed) as i32;
            if rows > 0 && rows as u64 > old_rows {
                return Err(StoreError::Rrd(format!(
                    "cannot shrink RRA#{position} by {removed} rows: the row count wraps"
                )));
            }
            rows
        }
    };
    Ok((rows > 0).then_some(rows as u64))
}

/// TM-52 to TM-54: writes `bytes` to a new file beside `path`, gives it the
/// original's permission bits and renames it over `path`.
fn replace_file(path: &Path, original: &std::fs::File, bytes: &[u8]) -> Result<(), StoreError> {
    let (temp_path, mut temp) = create_temporary(path)
        .map_err(|_| StoreError::Rrd("Cannot create temporary file".into()))?;
    let written = (|| -> Result<(), StoreError> {
        temp.write_all(bytes)?;
        temp.flush()?;
        let permissions = original.metadata()?.permissions();
        temp.set_permissions(permissions)?;
        drop(temp);
        std::fs::rename(&temp_path, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    written
}

/// Creates `<path>` plus a random six-character suffix, exclusively.
fn create_temporary(path: &Path) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 32);
    let mut error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for _ in 0..100 {
        let mut name = path.as_os_str().to_owned();
        for _ in 0..6 {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            name.push(char::from(LETTERS[(state % LETTERS.len() as u64) as usize]).to_string());
        }
        let candidate = std::path::PathBuf::from(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(failure) if failure.kind() == std::io::ErrorKind::AlreadyExists => error = failure,
            Err(failure) => return Err(failure),
        }
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &[u8] = include_bytes!("../../rondi-cli/tests/data/tune_modify/inputs/base.rrd");
    const TYPES: &[u8] = include_bytes!("../../rondi-cli/tests/data/tune_modify/inputs/types.rrd");
    const TIES: &[u8] = include_bytes!("../../rondi-cli/tests/data/tune_modify/inputs/ties.rrd");
    const V5: &[u8] = include_bytes!("../../rondi-cli/tests/data/tune_modify/inputs/v5.rrd");

    /// A small file: step 60, last update 6000 (a multiple of every slot
    /// below), data sources `a` and `b`.
    fn model(archives: Vec<Archive>) -> Model {
        let mut live = [0_u8; LIVE_HEAD_LEN];
        live[..8].copy_from_slice(&6000_i64.to_le_bytes());
        let source = |name: &str| {
            let mut def = [0_u8; DS_DEF_LEN];
            def[..name.len()].copy_from_slice(name.as_bytes());
            def[DS_TYPE_OFFSET..DS_TYPE_OFFSET + 5].copy_from_slice(b"GAUGE");
            set_u64(&mut def, 40, 120);
            let mut pdp = [0_u8; PDP_PREP_LEN];
            pdp[..2].copy_from_slice(b"7\0");
            set_u64(&mut pdp, PDP_UNKNOWN_SEC_OFFSET, 3);
            Source { def, pdp }
        };
        Model {
            version: *b"0003",
            step: 60,
            live,
            sources: vec![source("a"), source("b")],
            archives,
        }
    }

    /// An archive over two sources whose storage row r holds (r, 100 + r).
    fn archive(cf: &str, rows: u64, pdp_per_row: u64, cur_row: u64) -> Archive {
        let mut def = [0_u8; RRA_DEF_LEN];
        def[..cf.len()].copy_from_slice(cf.as_bytes());
        set_u64(&mut def, RRA_ROWS_OFFSET, rows);
        set_u64(&mut def, RRA_PDP_OFFSET, pdp_per_row);
        let mut record = [0_u8; CDP_PREP_LEN];
        set_u64(&mut record, CDP_UNKNOWN_OFFSET, 1);
        set_f64(&mut record, CDP_PRIMARY_OFFSET, 42.0);
        Archive {
            def,
            cf: Cf::from_field(&def[..CF_LEN]).unwrap(),
            cdp: vec![record; 2],
            cur_row,
            values: (0..rows)
                .flat_map(|row| [(row as f64).to_bits(), (100.0 + row as f64).to_bits()])
                .collect(),
        }
    }

    fn run(model: &Model, ops: &[RrdStructureOp], new_step: i32) -> Result<Model, StoreError> {
        let bytes = model.to_bytes(usize::MAX).unwrap();
        Model::parse(&restructure(&bytes, ops, new_step, 1 << 24)?)
    }

    fn column(archive: &Archive, source: usize, width: usize) -> Vec<f64> {
        archive
            .values
            .chunks(width)
            .map(|row| f64::from_bits(row[source]))
            .collect()
    }

    fn error(result: Result<Model, StoreError>) -> String {
        result.unwrap_err().to_string()
    }

    fn add_source(spec: &str) -> RrdStructureOp {
        RrdStructureOp::AddSource(spec.into())
    }

    fn add_archive(spec: &str) -> RrdStructureOp {
        RrdStructureOp::AddArchive(spec.into())
    }

    fn delete_source(name: &str) -> RrdStructureOp {
        RrdStructureOp::DeleteSource(name.into())
    }

    fn resize(index: u32, change: RrdRowChange) -> RrdStructureOp {
        RrdStructureOp::ResizeArchive { index, change }
    }

    #[test]
    fn frozen_inputs_round_trip_through_the_model() {
        for input in [BASE, TYPES, TIES, V5] {
            let model = Model::parse(input).unwrap();
            assert_eq!(model.to_bytes(usize::MAX).unwrap(), input);
        }
    }

    #[test]
    fn trailing_bytes_are_dropped_and_short_files_rejected() {
        let mut longer = BASE.to_vec();
        longer.extend_from_slice(&[1, 2, 3]);
        let model = Model::parse(&longer).unwrap();
        assert_eq!(model.to_bytes(usize::MAX).unwrap(), BASE);
        for length in [0, 100, STAT_HEAD_LEN, BASE.len() - 1] {
            assert!(Model::parse(&BASE[..length]).is_err(), "length {length}");
        }
    }

    #[test]
    fn rewriting_rotates_rows_oldest_first() {
        let rrd = model(vec![archive("AVERAGE", 4, 1, 1)]);
        let out = run(&rrd, &[delete_source("zz")], -1).unwrap();
        let rotated = &out.archives[0];
        assert_eq!(rotated.cur_row, 3);
        assert_eq!(column(rotated, 0, 2), [2.0, 3.0, 0.0, 1.0]);
        assert_eq!(column(rotated, 1, 2), [102.0, 103.0, 100.0, 101.0]);
    }

    #[test]
    fn deleting_a_source_drops_its_definition_state_and_column() {
        let rrd = model(vec![archive("AVERAGE", 3, 1, 2)]);
        let out = run(&rrd, &[delete_source("a")], -1).unwrap();
        assert_eq!(out.sources.len(), 1);
        assert_eq!(out.sources[0].name(), b"b");
        assert_eq!(out.archives[0].cdp.len(), 1);
        assert_eq!(column(&out.archives[0], 0, 1), [100.0, 101.0, 102.0]);
    }

    #[test]
    fn an_added_source_starts_unknown() {
        let rrd = model(vec![archive("AVERAGE", 3, 5, 2)]);
        let out = run(&rrd, &[add_source("c:GAUGE:600:U:U")], -1).unwrap();
        let added = &out.sources[2];
        assert_eq!(added.name(), b"c");
        assert_eq!(&added.pdp[..2], b"U\0");
        assert_eq!(added.unknown_seconds(), 0);
        assert_eq!(get_f64(&added.pdp, PDP_VALUE_OFFSET), 0.0);
        let record = out.archives[0].cdp[2];
        assert_eq!(get_u64(&record, CDP_VALUE_OFFSET), unknown());
        assert_eq!(get_u64(&record, CDP_UNKNOWN_OFFSET), 0);
        assert!(column(&out.archives[0], 2, 3).iter().all(|v| v.is_nan()));
        assert_eq!(out.archives[0].cdp[0], rrd.archives[0].cdp[0]);
    }

    #[test]
    fn an_added_source_counts_elapsed_steps() {
        let mut rrd = model(vec![archive("AVERAGE", 3, 5, 2)]);
        // 6130: 10 s into a step, two whole steps into the 300 s row.
        rrd.live[..8].copy_from_slice(&6130_i64.to_le_bytes());
        let out = run(&rrd, &[add_source("c:GAUGE:600:U:U")], -1).unwrap();
        assert_eq!(out.sources[2].unknown_seconds(), 10);
        assert_eq!(get_u64(&out.archives[0].cdp[2], CDP_UNKNOWN_OFFSET), 2);
    }

    #[test]
    fn duplicate_names_and_freed_names() {
        let rrd = model(vec![archive("AVERAGE", 3, 1, 2)]);
        assert_eq!(
            error(run(&rrd, &[add_source("a:GAUGE:600:U:U")], -1)),
            "Duplicate DS name: a"
        );
        let twice = [add_source("n:GAUGE:600:U:U"), add_source("n:GAUGE:60:U:U")];
        assert_eq!(error(run(&rrd, &twice, -1)), "Duplicate DS name: n");
        let freed = [delete_source("a"), add_source("a:GAUGE:900:0:10")];
        let out = run(&rrd, &freed, -1).unwrap();
        assert_eq!(out.sources[1].name(), b"a");
        assert!(column(&out.archives[0], 1, 2).iter().all(|v| v.is_nan()));
    }

    #[test]
    fn errors_are_reported_in_stage_order() {
        let rrd = model(vec![archive("AVERAGE", 3, 1, 2)]);
        let bad_archive = add_archive("FOO:0.5:1:1");
        let ops = [bad_archive.clone(), add_source("a:GAUGE:600:U:U")];
        assert_eq!(error(run(&rrd, &ops, -1)), "Duplicate DS name: a");
        let ops = [add_source("bad"), bad_archive];
        assert_eq!(
            error(run(&rrd, &ops, -1)),
            "Unrecognized consolidation function FOO"
        );
        assert_eq!(
            error(run(&rrd, &[add_source("bad")], -1)),
            "invalid DS format"
        );
        let ops = [add_source("x:GAUGE:600:U:U"), add_source("bad")];
        assert_eq!(error(run(&rrd, &ops, -1)), "invalid DS format");
        let ops = [add_source("a:GAUGE:600:U:U")];
        assert!(error(run(&rrd, &ops, 7)).starts_with("invalid 'newstep'"));
    }

    #[test]
    fn row_count_arithmetic_is_32_bit() {
        use RrdRowChange::{Grow, Set, Shrink};
        assert_eq!(new_row_count(0, 10, Set(17)).unwrap(), Some(17));
        assert_eq!(new_row_count(0, 10, Grow(4)).unwrap(), Some(14));
        assert_eq!(new_row_count(0, 10, Shrink(2)).unwrap(), Some(8));
        for change in [Shrink(10), Shrink(11), Set(0), Set(u32::MAX), Set(1 << 31)] {
            assert_eq!(new_row_count(0, 10, change).unwrap(), None, "{change:?}");
        }
        assert!(new_row_count(0, 10, Grow(4_294_967_291)).is_err());
        assert!(new_row_count(0, 10, Grow(u32::MAX)).is_err());
        assert!(new_row_count(0, 10, Shrink(u32::MAX)).is_err());
    }

    #[test]
    fn resizing_keeps_the_newest_rows_and_backfills_older_slots() {
        let rrd = model(vec![
            archive("AVERAGE", 4, 1, 3),
            archive("AVERAGE", 3, 2, 2),
        ]);
        let shrunk = run(&rrd, &[resize(0, RrdRowChange::Shrink(1))], -1).unwrap();
        assert_eq!(column(&shrunk.archives[0], 0, 2), [1.0, 2.0, 3.0]);

        // Archive 1's rows end at 5760, 5880 and 6000. One more row ends at
        // 5640, older than anything either archive holds.
        let grown = run(&rrd, &[resize(1, RrdRowChange::Grow(1))], -1).unwrap();
        let rows = column(&grown.archives[1], 0, 2);
        assert!(rows[0].is_nan());
        assert_eq!(rows[1..], [0.0, 1.0, 2.0]);

        // Archive 0's rows end at 5820..6000. Its two new rows end at 5760
        // and 5700, which lie in archive 1's oldest row (5640, 5760].
        let grown = run(&rrd, &[resize(0, RrdRowChange::Set(6))], -1).unwrap();
        let a = column(&grown.archives[0], 0, 2);
        assert_eq!(a, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        assert_eq!(column(&grown.archives[0], 1, 2)[..2], [100.0, 100.0]);
    }

    #[test]
    fn only_the_first_word_for_an_index_counts() {
        let rrd = model(vec![archive("AVERAGE", 4, 1, 3)]);
        let ops = [
            resize(0, RrdRowChange::Grow(2)),
            RrdStructureOp::DeleteArchive(0),
        ];
        assert_eq!(run(&rrd, &ops, -1).unwrap().archives[0].rows(), 6);
        let ops = [
            RrdStructureOp::DeleteArchive(0),
            resize(0, RrdRowChange::Grow(2)),
        ];
        assert!(run(&rrd, &ops, -1).unwrap().archives.is_empty());
        // Indices of 2^31 and above match nothing.
        let ops = [resize(u32::MAX, RrdRowChange::Grow(2))];
        assert_eq!(run(&rrd, &ops, -1).unwrap().archives[0].rows(), 4);
    }

    #[test]
    fn deleting_an_archive_shifts_later_rows() {
        let mut later = archive("MAX", 2, 1, 1);
        later.values = [7.0, 8.0, 9.0, 10.0].map(f64::to_bits).to_vec();
        let rrd = model(vec![archive("AVERAGE", 4, 1, 3), later]);
        let out = run(&rrd, &[RrdStructureOp::DeleteArchive(0)], -1).unwrap();
        assert_eq!(out.archives.len(), 1);
        // The kept archive reads the first rows of the deleted one.
        assert_eq!(column(&out.archives[0], 0, 2), [0.0, 1.0]);
    }

    #[test]
    fn an_added_archive_is_backfilled_and_prepared() {
        let rrd = model(vec![archive("AVERAGE", 6, 1, 5)]);
        let out = run(&rrd, &[add_archive("AVERAGE:0.5:2:3")], -1).unwrap();
        let added = &out.archives[1];
        assert_eq!(added.cur_row, 2);
        // Rows end at 5760, 5880 and 6000 and average two one-step rows.
        assert_eq!(column(added, 0, 2), [0.5, 2.5, 4.5]);
        let record = added.cdp[0];
        // 6 rows before it and 2 sources: value 6 + 2*2 + 0 of the new row
        // data counts archive 0's rows as single values, landing on its
        // newest row's a (5.0) rather than this archive's newest (4.5).
        assert_eq!(get_f64(&record, CDP_PRIMARY_OFFSET), 5.0);
        // The current row ends at 6000 and spans one-step rows 4 and 5;
        // the secondary value is row 3.
        assert_eq!(get_f64(&record, CDP_SECONDARY_OFFSET), 3.0);
        assert_eq!(get_f64(&record, CDP_VALUE_OFFSET), 9.0);
        assert_eq!(get_u64(&record, CDP_UNKNOWN_OFFSET), 0);

        let max = run(&rrd, &[add_archive("MAX:0.5:2:3")], -1).unwrap();
        assert_eq!(column(&max.archives[1], 1, 2), [101.0, 103.0, 105.0]);
        let min = run(&rrd, &[add_archive("MIN:0.5:2:3")], -1).unwrap();
        // The folded MIN starts at 0.0, below every value here.
        assert_eq!(get_f64(&min.archives[1].cdp[1], CDP_VALUE_OFFSET), 0.0);
    }

    #[test]
    fn step_change_stretches_archives() {
        let rrd = model(vec![archive("AVERAGE", 3, 2, 0)]);
        let ops = [delete_source("zz")];
        let out = run(&rrd, &ops, 20).unwrap();
        assert_eq!(out.step, 20);
        assert_eq!(out.archives[0].pdp_per_row(), 6);
        assert_eq!(get_u64(&out.archives[0].cdp[0], CDP_UNKNOWN_OFFSET), 3);
        assert_eq!(out.sources[0].unknown_seconds(), 3);
        for bad in [7, 60, 120] {
            assert!(run(&rrd, &ops, bad).is_err(), "step {bad}");
        }
        assert_eq!(run(&rrd, &ops, 0).unwrap().step, 60);
        assert_eq!(run(&rrd, &ops, -60).unwrap().step, 60);
    }

    #[test]
    fn row_ranges_wrap() {
        let rows = |range: RowRange, count| range.rows(count).collect::<Vec<_>>();
        assert_eq!(rows(RowRange::new(1, 3, 5), 5), [1, 2, 3]);
        assert_eq!(rows(RowRange::new(3, 1, 5), 5), [3, 4, 0, 1]);
        assert!(rows(RowRange::new(2, 1, 5), 5).is_empty());
    }

    /// xorshift64, so a failing round replays.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    const SOURCE_SPECS: &[&str] = &[
        "n:GAUGE:600:U:U",
        "m:DERIVE:60:0:U",
        "bad",
        "x:GAUGE:1:5:1",
        "c:COMPUTE:a,2,*",
        "q:COUNTER:9999999999999999999999:U:U",
        "g=h[1]:GAUGE:600:U:U",
    ];

    const ARCHIVE_SPECS: &[&str] = &[
        "AVERAGE:0.5:1:20",
        "MIN:0.5:3:7",
        "MAX:0.5:2:9",
        "LAST:0.5:5:4",
        "AVERAGE:0.5:1h:1d",
        "HWPREDICT:10:0.1:0.2:5",
        "AVERAGE:0.5:0:1",
        "LAST:0.5:1:99999999999999",
        "MAX::::",
    ];

    fn random_op(random: &mut Random) -> RrdStructureOp {
        let number = |random: &mut Random| match random.below(4) {
            0 => random.next() as u32,
            1 => u32::MAX - random.below(8) as u32,
            _ => random.below(40) as u32,
        };
        match random.below(8) {
            0 => delete_source(["a", "b", "c", "g", "zz"][random.below(5)]),
            1 => add_source(SOURCE_SPECS[random.below(SOURCE_SPECS.len())]),
            2 => RrdStructureOp::DeleteArchive(number(random)),
            3..=5 => {
                let index = number(random) % 12;
                let rows = number(random) % 64;
                let change = match random.below(3) {
                    0 => RrdRowChange::Set(rows),
                    1 => RrdRowChange::Grow(rows),
                    _ => RrdRowChange::Shrink(rows),
                };
                resize(index, change)
            }
            _ => add_archive(ARCHIVE_SPECS[random.below(ARCHIVE_SPECS.len())]),
        }
    }

    /// Mutated files and random operation lists fail cleanly or produce a
    /// file that reads back; nothing panics.
    #[test]
    fn malformed_input_never_panics() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        for round in 0..3000 {
            let mut bytes = [BASE, TYPES, TIES, V5][random.below(4)].to_vec();
            match random.below(4) {
                // The header and definitions, where counts and sizes live.
                0 => {
                    for _ in 0..1 + random.below(4) {
                        let at = random.below(bytes.len().min(1200));
                        bytes[at] = random.next() as u8;
                    }
                }
                1 => {
                    for _ in 0..1 + random.below(16) {
                        let at = random.below(bytes.len());
                        bytes[at] = random.next() as u8;
                    }
                }
                2 => bytes.truncate(random.below(bytes.len() + 1)),
                _ => {
                    // A plausible but wrong count, step or archive field.
                    let at = [24, 32, 40, 128 + 120 + 24, 128 + 120 + 32][random.below(5)];
                    let value = [0, 1, 2, u64::MAX, 1 << 40, random.next()][random.below(6)];
                    set_u64(&mut bytes, at, value);
                }
            }
            let ops: Vec<RrdStructureOp> = (0..1 + random.below(5))
                .map(|_| random_op(&mut random))
                .collect();
            let new_step = [-1, 0, 1, 7, 10, 20, 30, 60, i32::MAX][random.below(9)];
            if let Ok(output) = restructure(&bytes, &ops, new_step, 1 << 22) {
                assert!(Model::parse(&output).is_ok(), "round {round}: {ops:?}");
            }
        }
    }
}
