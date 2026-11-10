//! Consolidation of PDP runs into archive rows (UP-26 to UP-30, UP-32).

use super::RrdUpdateSummary;
use super::holt_winters::{Group, Seasons, consolidate_group};
use super::pdp::Run;
use crate::holt_winters::Role;
use crate::rrd_binary::{RrdArchiveInfo, RrdCdpPrepInfo, cf_conv_error, rrd_mul_add, rrd_nan};
use crate::storage::StoreError;

/// A classic consolidation function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cf {
    Average,
    Min,
    Max,
    Last,
}

/// How an archive consumes runs of PDPs.
///
/// Holt-Winters archives are coupled PDP by PDP across archives, so they
/// are consolidated together by [`consolidate_group`], each still with its
/// own row pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    Classic(Cf),
    HoltWinters(Role),
}

impl ArchiveKind {
    /// The kind behind an archive's on-disk consolidation function name.
    pub(crate) fn from_raw(raw_consolidation: &[u8]) -> Result<ArchiveKind, StoreError> {
        let cf = match raw_consolidation {
            b"AVERAGE" => Cf::Average,
            b"MIN" => Cf::Min,
            b"MAX" => Cf::Max,
            b"LAST" => Cf::Last,
            raw => {
                if let Some(role) = Role::from_cf(raw) {
                    return Ok(ArchiveKind::HoltWinters(role));
                }
                let message = cf_conv_error(raw).unwrap_or_default();
                return Err(StoreError::Rrd(
                    String::from_utf8_lossy(&message).into_owned(),
                ));
            }
        };
        Ok(ArchiveKind::Classic(cf))
    }
}

/// Consecutive archive rows that store the same values, one per DS in DS
/// order: rows `first_row`, `first_row + 1`, ... wrapping to 0 at the
/// archive's row count (UP-30). `first_row` is below the row count.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RowRange {
    pub archive: usize,
    pub first_row: u64,
    pub count: u64,
    pub values: Vec<f64>,
}

impl Cf {
    /// The accumulator a completed row leaves behind (UP-26).
    fn reset_value(self) -> f64 {
        match self {
            Cf::Average => 0.0,
            Cf::Min => f64::INFINITY,
            Cf::Max => f64::NEG_INFINITY,
            Cf::Last => rrd_nan(),
        }
    }

    fn reset(self, cdp: &mut RrdCdpPrepInfo) {
        cdp.value = self.reset_value();
        cdp.unknown_datapoints = 0;
    }

    /// Folds `copies` PDPs of value `z` into the accumulator in one step,
    /// whatever the number of copies.
    // Provenance: specification (docs/compatibility/update.md UP-26).
    fn fold(self, cdp: &mut RrdCdpPrepInfo, z: f64, copies: u64) {
        if copies == 0 {
            return;
        }
        if z.is_nan() {
            cdp.unknown_datapoints = cdp.unknown_datapoints.saturating_add(copies);
            return;
        }
        let acc = cdp.value;
        cdp.value = match self {
            Cf::Average if acc.is_nan() => z * copies as f64,
            Cf::Average => rrd_mul_add(z, copies as f64, acc),
            Cf::Min if acc.is_nan() || z < acc => z,
            Cf::Max if acc.is_nan() || z > acc => z,
            Cf::Min | Cf::Max => acc,
            Cf::Last => z,
        };
    }

    /// The row value of an accumulator whose last folded PDP was `last`.
    // Provenance: specification (docs/compatibility/update.md UP-27).
    fn finish(self, cdp: &RrdCdpPrepInfo, last: f64, xff: f64, pdp_per_row: u64) -> f64 {
        if cdp.unknown_datapoints as f64 > xff * pdp_per_row as f64 {
            return rrd_nan();
        }
        match self {
            Cf::Last if last.is_nan() => rrd_nan(),
            Cf::Average => cdp.value / pdp_per_row.saturating_sub(cdp.unknown_datapoints) as f64,
            Cf::Min | Cf::Max | Cf::Last => cdp.value,
        }
    }
}

/// Where a run falls in an archive's rows.
struct Placement {
    /// The run's first position in its row (*c*).
    first_position: u64,
    /// Rows the run completes (*N*).
    completed: u128,
    /// PDPs left over after the last completed row.
    remainder: u64,
}

// Provenance: specification (docs/compatibility/update.md UP-28).
fn place(run: &Run, step: u64, pdp_per_row: u64) -> Placement {
    let step = i128::from(step.max(1));
    let pdp_per_row = u128::from(pdp_per_row);
    let first_position = i128::from(run.start)
        .div_euclid(step)
        .rem_euclid(pdp_per_row as i128) as u128;
    // first_position < pdp_per_row ≤ u64::MAX, so the sum fits in u128.
    let end = first_position + u128::from(run.len);
    Placement {
        first_position: first_position as u64,
        completed: end / pdp_per_row,
        remainder: (end % pdp_per_row) as u64,
    }
}

/// The `updatev` times of the rows a run writes (UP-32). They count back
/// from the end of the last slot the run completes, so that the row written
/// last is printed one slot after it.
// Provenance: specification (docs/compatibility/update.md UP-32).
// Compatibility: the times are computed in wrapping 64-bit arithmetic, so a
// run ending near the largest time prints wrapped, negative times.
struct PrintedTimes {
    /// End of the last slot the run completes (*G*).
    last_slot_end: i64,
    /// Slot length (*D*), wrapped modulo 2^64.
    slot: i64,
    /// Rows written (*K*).
    written: u64,
    /// The time of an opening row completed by the first run of a split
    /// update that prints it one slot early.
    early_opening: Option<i64>,
}

impl PrintedTimes {
    fn new(run: &Run, step: u64, pdp_per_row: u64, written: u64, early: bool) -> Self {
        let slot = step.wrapping_mul(pdp_per_row);
        let end = run.end;
        let last_slot_end = if slot == 0 {
            end
        } else {
            end.wrapping_sub(i128::from(end).rem_euclid(i128::from(slot)) as i64)
        };
        let slot = slot as i64;
        PrintedTimes {
            last_slot_end,
            slot,
            written,
            early_opening: early.then(|| {
                run.start
                    .wrapping_add(step.max(1) as i64)
                    .wrapping_sub(slot)
            }),
        }
    }

    /// The time of the row `back` rows before the last slot's row.
    fn before_last(&self, back: u64) -> i64 {
        self.last_slot_end
            .wrapping_sub((back as i64).wrapping_mul(self.slot))
    }

    fn opening(&self) -> i64 {
        self.early_opening
            .unwrap_or_else(|| self.before_last(self.written.saturating_sub(1)))
    }

    /// The time of the `j`-th row after the opening row.
    fn following(&self, j: u64) -> i64 {
        let back = self.written.saturating_sub(1).saturating_sub(j);
        self.before_last(back).wrapping_add(self.slot)
    }
}

/// A row would be written past the end of the file. The archive's current
/// row has already advanced to it, and the row's first value is in the
/// summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowPastEnd;

/// The row UP-30 writes after `current` in an archive of `rows` rows.
pub(super) fn next_row(current: u64, rows: u64) -> u64 {
    let next = current.wrapping_add(1);
    if next >= rows { 0 } else { next }
}

/// Queues `count` rows of `values` after the archive's current row and
/// advances it (UP-30). With `summaries`, the row at position `i` of the
/// range is reported at `time_of(i)`.
///
/// Rows at or past `rows_in_file` lie outside the file: the rows before
/// the first such row are queued, the current row advances to it, and only
/// its first value is reported.
// Compatibility: a row past the end of a damaged file (row count larger
// than the file holds) stops the update at that row.
// Provenance: specification (docs/compatibility/update.md UP-38).
#[allow(clippy::too_many_arguments)]
fn write_rows(
    index: usize,
    archive: &mut RrdArchiveInfo,
    rows_in_file: u64,
    values: &[f64],
    count: u64,
    time_of: impl Fn(u64) -> i64,
    rows_out: &mut Vec<RowRange>,
    summaries: Option<&mut Vec<RrdUpdateSummary>>,
) -> Result<(), RowPastEnd> {
    if count == 0 {
        return Ok(());
    }
    let first_row = next_row(archive.current_row, archive.rows);
    // first_row < rows; when the file holds fewer rows than that, the rows
    // from first_row up to rows_in_file - 1 come before the first row
    // outside the file, with no wrap in between.
    let fitting = if rows_in_file >= archive.rows {
        count
    } else {
        rows_in_file.saturating_sub(first_row).min(count)
    };
    let summary = |archive: &RrdArchiveInfo, timestamp, values: Vec<f64>| RrdUpdateSummary {
        timestamp,
        consolidation: archive.consolidation.clone(),
        pdp_per_row: archive.pdp_per_row,
        values,
    };
    let mut summaries = summaries;
    if fitting > 0 {
        rows_out.push(RowRange {
            archive: index,
            first_row,
            count: fitting,
            values: values.to_vec(),
        });
        if let Some(summaries) = summaries.as_deref_mut() {
            for i in 0..fitting {
                summaries.push(summary(archive, time_of(i), values.to_vec()));
            }
        }
        archive.current_row = advance_rows(archive.current_row, archive.rows, fitting);
    }
    if fitting == count {
        return Ok(());
    }
    archive.current_row = next_row(archive.current_row, archive.rows);
    if let Some(summaries) = summaries {
        summaries.push(summary(
            archive,
            time_of(fitting),
            values.iter().copied().take(1).collect(),
        ));
    }
    Err(RowPastEnd)
}

/// The current row after `count` row writes from `current` in an archive
/// of `rows` rows (UP-30), without stepping through them.
fn advance_rows(current: u64, rows: u64, count: u64) -> u64 {
    if count == 0 || rows == 0 {
        return current;
    }
    // A stored row past the end goes to 0 on the first write (UP-30).
    let (start, remaining) = if current < rows {
        (current, count)
    } else {
        (0, count - 1)
    };
    ((u128::from(start) + u128::from(remaining)) % u128::from(rows)) as u64
}

/// Consolidates one run into one classic archive: folds its PDPs, writes
/// the rows it completes and updates the archive's CDP state and current
/// row. The work is independent of the run's length.
///
/// `early_first_row` marks the first run of a split update whose last
/// update has a microsecond part; it only changes printed times (UP-32).
/// `rows_in_file` is how many of the archive's rows lie inside the file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn consolidate_run(
    index: usize,
    archive: &mut RrdArchiveInfo,
    kind: ArchiveKind,
    run: &Run,
    step: u64,
    early_first_row: bool,
    rows_in_file: u64,
    rows_out: &mut Vec<RowRange>,
    mut summaries: Option<&mut Vec<RrdUpdateSummary>>,
) -> Result<(), RowPastEnd> {
    let ArchiveKind::Classic(cf) = kind else {
        return Ok(());
    };
    // One PDP and one accumulator per data source; the zips below rely on it.
    debug_assert_eq!(run.values.len(), archive.cdp_prep.len());
    if archive.rows == 0 || run.len == 0 {
        return Ok(());
    }
    let pdp_per_row = archive.pdp_per_row.max(1);
    let placement = place(run, step, pdp_per_row);

    if placement.completed == 0 {
        for (cdp, &z) in archive.cdp_prep.iter_mut().zip(&run.values) {
            cf.fold(cdp, z, run.len);
        }
        return Ok(());
    }

    // Provenance: specification (docs/compatibility/update.md UP-29).
    let written = placement.completed.min(u128::from(archive.rows)) as u64;
    if archive.cdp_prep.is_empty() {
        // Rows without data sources hold no bytes and print nothing, so
        // only the current row moves, by `written` steps of UP-30.
        archive.current_row = advance_rows(archive.current_row, archive.rows, written);
        return Ok(());
    }
    let xff = archive.xff;
    let opening: Vec<f64> = archive
        .cdp_prep
        .iter_mut()
        .zip(&run.values)
        .map(|(cdp, &z)| {
            // A one-PDP-per-row archive writes each PDP as it is and never
            // touches its accumulator (UP-28).
            let value = if pdp_per_row == 1 {
                z
            } else {
                cf.fold(cdp, z, pdp_per_row - placement.first_position);
                let value = cf.finish(cdp, z, xff, pdp_per_row);
                cf.reset(cdp);
                cf.fold(cdp, z, placement.remainder);
                value
            };
            cdp.primary_value = value;
            if pdp_per_row > 1 || placement.completed > 1 {
                cdp.secondary_value = z;
            }
            value
        })
        .collect();

    let times = PrintedTimes::new(run, step, pdp_per_row, written, early_first_row);
    write_rows(
        index,
        archive,
        rows_in_file,
        &opening,
        1,
        |_| times.opening(),
        rows_out,
        summaries.as_deref_mut(),
    )?;
    // The K - 1 rows after the opening row all store the run's PDPs.
    write_rows(
        index,
        archive,
        rows_in_file,
        &run.values,
        written - 1,
        |i| times.following(i + 1),
        rows_out,
        summaries,
    )
}

/// Consolidates one run into every archive, in archive order (UP-28). This
/// is the only entry point the update loop uses.
///
/// `hw` carries the file's Holt-Winters archives, their season rows and
/// whether a season counter triggered. The group's state changes for the
/// whole run before any row is written, as every classic archive's does;
/// each Holt-Winters row is then written at its archive's place in archive
/// order. Without `hw`, Holt-Winters archives are left alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn consolidate_all(
    archives: &mut [RrdArchiveInfo],
    kinds: &[ArchiveKind],
    run: &Run,
    step: u64,
    early_first_row: bool,
    rows_in_file: &[u64],
    rows_out: &mut Vec<RowRange>,
    mut summaries: Option<&mut Vec<RrdUpdateSummary>>,
    hw: Option<(&Group, &mut Seasons, &mut bool)>,
) -> Result<(), RowPastEnd> {
    // Provenance: specification (docs/compatibility/update.md UP-38). Every
    // archive's CDP state changes for the run, but rows are written archive
    // by archive, so a row past the end of the file keeps the rows and
    // current rows of later archives unwritten.
    debug_assert_eq!(archives.len(), kinds.len());
    let current_rows: Vec<u64> = archives.iter().map(|archive| archive.current_row).collect();
    let mut group_rows = match hw {
        Some((group, seasons, triggered)) => {
            consolidate_group(group, archives, seasons, run, triggered)
        }
        None => Vec::new(),
    };
    // Compatibility: a Holt-Winters row takes one PDP, so it is printed at
    // the end of that PDP's step, like a one-PDP-per-row classic row.
    // Provenance: specification (docs/compatibility/update.md UP-31, UP-32).
    let hw_times = PrintedTimes::new(run, step, 1, 1, early_first_row);
    let mut stopped = None;
    for (index, (archive, &kind)) in archives.iter_mut().zip(kinds).enumerate() {
        let fit = rows_in_file.get(index).copied().unwrap_or(u64::MAX);
        let result = match kind {
            ArchiveKind::Classic(_) => consolidate_run(
                index,
                archive,
                kind,
                run,
                step,
                early_first_row,
                fit,
                rows_out,
                summaries.as_deref_mut(),
            ),
            // An archive without rows has nothing to write (UP-28).
            ArchiveKind::HoltWinters(_) if archive.rows == 0 => Ok(()),
            ArchiveKind::HoltWinters(_) => {
                match group_rows.iter_mut().find(|row| row.archive == index) {
                    Some(row) => write_rows(
                        index,
                        archive,
                        fit,
                        &std::mem::take(&mut row.values),
                        1,
                        |_| hw_times.opening(),
                        rows_out,
                        summaries.as_deref_mut(),
                    ),
                    None => Ok(()),
                }
            }
        };
        if result.is_err() && stopped.is_none() {
            let reported = summaries.as_deref().map_or(0, Vec::len);
            stopped = Some((index, rows_out.len(), reported));
        }
    }
    let Some((failed, rows_len, summaries_len)) = stopped else {
        return Ok(());
    };
    rows_out.truncate(rows_len);
    if let Some(summaries) = summaries {
        summaries.truncate(summaries_len);
    }
    for (archive, &current_row) in archives.iter_mut().zip(&current_rows).skip(failed + 1) {
        archive.current_row = current_row;
    }
    Err(RowPastEnd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrd_binary::{RrdCdpPrepInfo, rrd_nan};

    /// One stored row, as a range expands to.
    #[derive(Clone, Debug, PartialEq)]
    struct RowWrite {
        archive: usize,
        row: u64,
        values: Vec<f64>,
    }

    /// Expands ranges into rows; `rows_of` gives each archive's row count.
    fn expand(ranges: &[RowRange], rows_of: impl Fn(usize) -> u64) -> Vec<RowWrite> {
        let mut rows = Vec::new();
        for range in ranges {
            let mut row = range.first_row;
            for _ in 0..range.count {
                rows.push(RowWrite {
                    archive: range.archive,
                    row,
                    values: range.values.clone(),
                });
                row = next_row(row, rows_of(range.archive));
            }
        }
        rows
    }

    const STEP: u64 = 10;

    fn created_cdp() -> RrdCdpPrepInfo {
        RrdCdpPrepInfo {
            value: rrd_nan(),
            unknown_datapoints: 0,
            primary_value: rrd_nan(),
            secondary_value: rrd_nan(),
            words: [
                rrd_nan().to_bits(),
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                rrd_nan().to_bits(),
                rrd_nan().to_bits(),
            ],
        }
    }

    fn archive(cf: &str, pdp_per_row: u64, rows: u64, xff: f64) -> RrdArchiveInfo {
        RrdArchiveInfo {
            consolidation: cf.to_owned(),
            raw_consolidation: cf.as_bytes().to_vec(),
            rows,
            pdp_per_row,
            xff,
            param_words: {
                let mut words = [0u64; 10];
                words[0] = xff.to_bits();
                words
            },
            current_row: 0,
            cdp_prep: vec![created_cdp()],
            data_offset: 0,
        }
    }

    fn kind(archive: &RrdArchiveInfo) -> ArchiveKind {
        ArchiveKind::from_raw(&archive.raw_consolidation).expect("classic CF")
    }

    fn run(start: i64, len: u64, value: f64) -> Run {
        let end = i128::from(start) + i128::from(len) * i128::from(STEP);
        Run {
            start,
            len,
            end: end as i64,
            values: vec![value],
        }
    }

    /// Consolidates `runs` into `archive` (archive index 0) and returns the
    /// rows and summaries written.
    fn feed(archive: &mut RrdArchiveInfo, runs: &[Run]) -> (Vec<RowWrite>, Vec<RrdUpdateSummary>) {
        let kind = kind(archive);
        let mut rows = Vec::new();
        let mut summaries = Vec::new();
        for run in runs {
            consolidate_run(
                0,
                archive,
                kind,
                run,
                STEP,
                false,
                u64::MAX,
                &mut rows,
                Some(&mut summaries),
            )
            .unwrap();
        }
        let count = archive.rows;
        (expand(&rows, |_| count), summaries)
    }

    fn row_values(rows: &[RowWrite]) -> Vec<f64> {
        rows.iter().map(|row| row.values[0]).collect()
    }

    #[test]
    fn average_opening_row() {
        let mut rra = archive("AVERAGE", 3, 4, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(100, 2, 2.0)]);
        assert_eq!(
            rows,
            vec![RowWrite {
                archive: 0,
                row: 1,
                values: vec![4.0 / 3.0]
            }]
        );
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].consolidation, "AVERAGE");
        assert_eq!(summaries[0].pdp_per_row, 3);
        assert_eq!(summaries[0].values, vec![4.0 / 3.0]);
        let cdp = &rra.cdp_prep[0];
        assert_eq!(cdp.value.to_bits(), 0.0f64.to_bits());
        assert_eq!(cdp.unknown_datapoints, 0);
        assert_eq!(cdp.primary_value, 4.0 / 3.0);
        assert_eq!(cdp.secondary_value, 2.0);
        assert_eq!(rra.current_row, 1);
    }

    #[test]
    fn fold_without_row() {
        let mut rra = archive("AVERAGE", 3, 4, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(90, 1, 3.0)]);
        assert!(rows.is_empty());
        assert!(summaries.is_empty());
        assert_eq!(rra.cdp_prep[0].value, 3.0);
        assert_eq!(rra.cdp_prep[0].unknown_datapoints, 0);
        assert!(rra.cdp_prep[0].primary_value.is_nan());
        assert_eq!(rra.current_row, 0);

        let (rows, _) = feed(&mut rra, &[run(100, 1, rrd_nan())]);
        assert!(rows.is_empty());
        assert_eq!(rra.cdp_prep[0].value, 3.0);
        assert_eq!(rra.cdp_prep[0].unknown_datapoints, 1);
    }

    #[test]
    fn average_fold_of_many_copies_is_one_fused_step() {
        // Provenance: specification (docs/compatibility/update.md UP-26).
        let mut rra = archive("AVERAGE", 10, 4, 0.5);
        feed(&mut rra, &[run(100, 1, 0.1), run(110, 3, 0.7)]);
        assert_eq!(
            rra.cdp_prep[0].value.to_bits(),
            crate::rrd_binary::rrd_mul_add(0.7, 3.0, 0.1).to_bits()
        );
    }

    #[test]
    fn min_max_last_fold() {
        let mut min = archive("MIN", 3, 4, 0.5);
        feed(&mut min, &[run(90, 1, 3.0), run(100, 1, 2.0)]);
        assert_eq!(min.cdp_prep[0].value, 2.0);

        let mut max = archive("MAX", 3, 4, 0.5);
        feed(&mut max, &[run(90, 1, -0.0), run(100, 1, 0.0)]);
        assert_eq!(max.cdp_prep[0].value.to_bits(), (-0.0f64).to_bits());
        let mut max = archive("MAX", 3, 4, 0.5);
        feed(&mut max, &[run(90, 1, 0.0), run(100, 1, -0.0)]);
        assert_eq!(max.cdp_prep[0].value.to_bits(), 0.0f64.to_bits());

        let mut last = archive("LAST", 3, 4, 0.5);
        feed(&mut last, &[run(90, 1, 3.0), run(100, 1, 2.0)]);
        assert_eq!(last.cdp_prep[0].value, 2.0);

        // A completed row resets the accumulator (UP-26).
        let resets = [
            ("AVERAGE", 0.0),
            ("MIN", f64::INFINITY),
            ("MAX", f64::NEG_INFINITY),
        ];
        for (cf, reset) in resets {
            let mut rra = archive(cf, 3, 4, 0.5);
            feed(&mut rra, &[run(90, 3, 5.0)]);
            assert_eq!(rra.cdp_prep[0].value.to_bits(), reset.to_bits(), "{cf}");
            assert_eq!(rra.cdp_prep[0].unknown_datapoints, 0, "{cf}");
        }
        let mut rra = archive("LAST", 3, 4, 0.5);
        feed(&mut rra, &[run(90, 3, 5.0)]);
        assert!(rra.cdp_prep[0].value.is_nan());
        assert_eq!(rra.cdp_prep[0].unknown_datapoints, 0);
    }

    #[test]
    fn xff_and_last_unknown() {
        // Two unknown PDPs and one known: 2 > 0.5 · 3.
        let mut rra = archive("AVERAGE", 3, 4, 0.5);
        let (rows, _) = feed(&mut rra, &[run(90, 2, rrd_nan()), run(110, 1, 5.0)]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].values[0].is_nan());
        assert!(rra.cdp_prep[0].primary_value.is_nan());
        assert_eq!(rra.cdp_prep[0].unknown_datapoints, 0);

        // One unknown PDP: the known sum is divided by p − cu = 2.
        let mut rra = archive("AVERAGE", 3, 4, 0.5);
        let (rows, _) = feed(
            &mut rra,
            &[run(90, 1, rrd_nan()), run(100, 1, 4.0), run(110, 1, 6.0)],
        );
        assert_eq!(row_values(&rows), vec![5.0]);

        // LAST whose final PDP is unknown.
        let mut rra = archive("LAST", 3, 4, 0.5);
        let (rows, _) = feed(&mut rra, &[run(90, 2, 5.0), run(110, 1, rrd_nan())]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].values[0].is_nan());

        // LAST whose final PDP is known, after an unknown one.
        let mut rra = archive("LAST", 3, 4, 0.5);
        let (rows, _) = feed(&mut rra, &[run(90, 1, rrd_nan()), run(100, 2, 5.0)]);
        assert_eq!(row_values(&rows), vec![5.0]);
    }

    #[test]
    fn one_pdp_per_row_never_folds() {
        let mut rra = archive("AVERAGE", 1, 3, 0.5);
        let (rows, _) = feed(&mut rra, &[run(100, 2, 7.0)]);
        assert_eq!(
            rows,
            vec![
                RowWrite {
                    archive: 0,
                    row: 1,
                    values: vec![7.0]
                },
                RowWrite {
                    archive: 0,
                    row: 2,
                    values: vec![7.0]
                },
            ]
        );
        let cdp = &rra.cdp_prep[0];
        assert!(cdp.value.is_nan());
        assert_eq!(cdp.unknown_datapoints, 0);
        assert_eq!(cdp.primary_value, 7.0);
        assert_eq!(cdp.secondary_value, 7.0);
        assert_eq!(rra.current_row, 2);

        // A single-PDP run leaves secondary_value unchanged (UP-29).
        let (rows, _) = feed(&mut rra, &[run(120, 1, rrd_nan())]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row, 0);
        assert!(rows[0].values[0].is_nan());
        assert!(rra.cdp_prep[0].value.is_nan());
        assert_eq!(rra.cdp_prep[0].unknown_datapoints, 0);
        assert!(rra.cdp_prep[0].primary_value.is_nan());
        assert_eq!(rra.cdp_prep[0].secondary_value, 7.0);
    }

    #[test]
    fn opening_row_then_copies_then_remainder() {
        // c = 1, m = 6, p = 3: N = 2, K = 2, then (1 + 6) mod 3 = 1 copy.
        let mut rra = archive("MAX", 3, 4, 0.5);
        rra.current_row = 3;
        feed(&mut rra, &[run(90, 1, 1.0)]);
        let (rows, _) = feed(&mut rra, &[run(100, 6, 4.0)]);
        assert_eq!(
            rows.iter().map(|row| row.row).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(row_values(&rows), vec![4.0, 4.0]);
        assert_eq!(rra.current_row, 1);
        assert_eq!(rra.cdp_prep[0].value, 4.0);
        assert_eq!(rra.cdp_prep[0].primary_value, 4.0);
        assert_eq!(rra.cdp_prep[0].secondary_value, 4.0);
    }

    #[test]
    fn capped_run_writes_rows_rows_and_does_not_iterate_per_pdp() {
        let mut rra = archive("AVERAGE", 1, 5, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(100, 1_000_000_000, 3.0)]);
        assert_eq!(rows.len(), 5);
        assert_eq!(summaries.len(), 5);
        assert_eq!(rra.current_row, 0);

        let mut rra = archive("AVERAGE", 7, 5, 0.5);
        let (rows, _) = feed(&mut rra, &[run(100, 1_000_000_000, 3.0)]);
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn huge_pdp_per_row_does_not_overflow() {
        let mut rra = archive("AVERAGE", u64::MAX / 2, 4, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(i64::MAX / 4, u64::MAX / 4, 1.0)]);
        // The run ends inside its first row: nothing is written and the
        // accumulator holds every PDP, folded in one step (UP-26).
        assert!(rows.is_empty());
        assert!(summaries.is_empty());
        assert_eq!(rra.current_row, 0);
        let cdp = &rra.cdp_prep[0];
        assert_eq!(cdp.value, (u64::MAX / 4) as f64);
        assert_eq!(cdp.unknown_datapoints, 0);
        assert!(cdp.primary_value.is_nan());
        assert!(cdp.secondary_value.is_nan());

        let mut rra = archive("MIN", u64::MAX, u64::MAX, 0.5);
        rra.current_row = u64::MAX - 1;
        let mut rows = Vec::new();
        let mut summaries = Vec::new();
        let kind = kind(&rra);
        consolidate_run(
            0,
            &mut rra,
            kind,
            &run(i64::MAX, u64::MAX, 1.0),
            u64::MAX,
            true,
            u64::MAX,
            &mut rows,
            Some(&mut summaries),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].first_row, rows[0].count), (0, 1));

        let mut rra = archive("LAST", 1, 3, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(i64::MAX, u64::MAX, 1.0)]);
        assert_eq!(rows.len(), 3);
        assert_eq!(summaries.len(), 3);

        let mut rra = archive("AVERAGE", 3, 0, 0.5);
        let (rows, _) = feed(&mut rra, &[run(100, 10, 1.0)]);
        assert!(rows.is_empty());
        assert!(rra.cdp_prep[0].value.is_nan());
    }

    #[test]
    fn printed_times() {
        let mut rra = archive("AVERAGE", 2, 2, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(100, 7, 1.0)]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            summaries.iter().map(|s| s.timestamp).collect::<Vec<_>>(),
            vec![140, 180]
        );

        // Uncapped: the opening row at E and each further row one slot late.
        let mut rra = archive("AVERAGE", 2, 10, 0.5);
        let (_, summaries) = feed(&mut rra, &[run(100, 7, 1.0)]);
        assert_eq!(
            summaries.iter().map(|s| s.timestamp).collect::<Vec<_>>(),
            vec![120, 160, 180]
        );

        // First run of a split update with µ0 > 0: T0 + S − D.
        let mut rra = archive("AVERAGE", 2, 10, 0.5);
        let mut rows = Vec::new();
        let mut summaries = Vec::new();
        let kind = kind(&rra);
        consolidate_run(
            0,
            &mut rra,
            kind,
            &run(110, 1, 1.0),
            STEP,
            true,
            u64::MAX,
            &mut rows,
            Some(&mut summaries),
        )
        .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].timestamp, 100);
    }

    #[test]
    fn range_stops_at_the_first_row_outside_the_file() {
        let mut rra = archive("LAST", 1, 10, 0.5);
        rra.current_row = 2;
        let kind = kind(&rra);
        let mut rows = Vec::new();
        let mut summaries = Vec::new();
        let result = consolidate_run(
            0,
            &mut rra,
            kind,
            &run(100, 8, 4.0),
            STEP,
            false,
            6,
            &mut rows,
            Some(&mut summaries),
        );
        assert_eq!(result, Err(RowPastEnd));
        assert_eq!(
            expand(&rows, |_| 10)
                .iter()
                .map(|row| row.row)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(rra.current_row, 6);
        assert_eq!(summaries.len(), 4);
        assert_eq!(summaries[3].values, vec![4.0]);

        // Without summaries the same rows are queued.
        let mut quiet = archive("LAST", 1, 10, 0.5);
        quiet.current_row = 2;
        let mut quiet_rows = Vec::new();
        let result = consolidate_run(
            0,
            &mut quiet,
            kind,
            &run(100, 8, 4.0),
            STEP,
            false,
            6,
            &mut quiet_rows,
            None,
        );
        assert_eq!(result, Err(RowPastEnd));
        assert_eq!(quiet_rows, rows);
        assert_eq!(quiet.current_row, 6);
    }

    #[test]
    fn range_wraps_at_the_archive_end() {
        let mut rra = archive("LAST", 1, 5, 0.5);
        rra.current_row = 2;
        let (rows, summaries) = feed(&mut rra, &[run(100, 4, 1.0)]);
        assert_eq!(
            rows.iter().map(|row| row.row).collect::<Vec<_>>(),
            vec![3, 4, 0, 1]
        );
        assert_eq!(summaries.len(), 4);
        assert_eq!(rra.current_row, 1);
    }

    #[test]
    fn consolidate_all_goes_in_archive_order() {
        let mut archives = vec![archive("AVERAGE", 1, 3, 0.5), archive("MAX", 2, 3, 0.5)];
        let kinds: Vec<_> = archives.iter().map(kind).collect();
        let mut rows = Vec::new();
        let mut summaries = Vec::new();
        consolidate_all(
            &mut archives,
            &kinds,
            &run(100, 2, 2.0),
            STEP,
            false,
            &[],
            &mut rows,
            Some(&mut summaries),
            None,
        )
        .unwrap();
        let rows = expand(&rows, |index| archives[index].rows);
        assert_eq!(
            rows.iter()
                .map(|row| (row.archive, row.row))
                .collect::<Vec<_>>(),
            vec![(0, 1), (0, 2), (1, 1)]
        );
        assert_eq!(
            summaries
                .iter()
                .map(|s| s.consolidation.as_str())
                .collect::<Vec<_>>(),
            vec!["AVERAGE", "AVERAGE", "MAX"]
        );
    }

    #[test]
    fn holt_winters_kinds() {
        for cf in [
            "HWPREDICT",
            "MHWPREDICT",
            "SEASONAL",
            "DEVSEASONAL",
            "DEVPREDICT",
            "FAILURES",
        ] {
            let role = Role::from_cf(cf.as_bytes()).unwrap();
            assert_eq!(
                ArchiveKind::from_raw(cf.as_bytes()).unwrap(),
                ArchiveKind::HoltWinters(role),
                "{cf}"
            );
        }
        assert_eq!(
            ArchiveKind::from_raw(b"AVERAGE").unwrap(),
            ArchiveKind::Classic(Cf::Average)
        );
        assert!(ArchiveKind::from_raw(b"BOGUS").is_err());
    }

    #[test]
    fn advance_rows_matches_single_steps() {
        for (current, rows) in [(0, 5), (3, 5), (4, 5), (7, 5), (u64::MAX, 5), (2, 1)] {
            let mut stepped = current;
            for count in 0..12 {
                assert_eq!(
                    advance_rows(current, rows, count),
                    stepped,
                    "{current} {rows} {count}"
                );
                let next = stepped.wrapping_add(1);
                stepped = if next >= rows { 0 } else { next };
            }
        }
        assert_eq!(advance_rows(0, u64::MAX, u64::MAX), 0);
    }
}
