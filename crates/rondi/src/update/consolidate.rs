//! Consolidation of PDP runs into archive rows (UP-26 to UP-30, UP-32).

use super::RrdUpdateSummary;
use super::pdp::Run;
use crate::rrd_binary::{RrdArchiveInfo, RrdCdpPrepInfo, cf_conv_error, rrd_mul_add, rrd_nan};
use crate::storage::StoreError;

const HOLT_WINTERS_CFS: [&[u8]; 6] = [
    b"HWPREDICT",
    b"MHWPREDICT",
    b"SEASONAL",
    b"DEVSEASONAL",
    b"DEVPREDICT",
    b"FAILURES",
];

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
/// Holt-Winters kinds are added as further variants; they receive each
/// `Run` and process its PDPs one at a time, in time order, with their own
/// row pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveKind {
    Classic(Cf),
}

impl ArchiveKind {
    /// The kind behind an archive's on-disk consolidation function name.
    pub(crate) fn from_raw(raw_consolidation: &[u8]) -> Result<ArchiveKind, StoreError> {
        let cf = match raw_consolidation {
            b"AVERAGE" => Cf::Average,
            b"MIN" => Cf::Min,
            b"MAX" => Cf::Max,
            b"LAST" => Cf::Last,
            raw if HOLT_WINTERS_CFS.contains(&raw) => {
                return Err(StoreError::RrdUnsupported(format!(
                    "Holt-Winters consolidation function {} is not supported by Rondi",
                    String::from_utf8_lossy(raw)
                )));
            }
            raw => {
                let message = cf_conv_error(raw).unwrap_or_default();
                return Err(StoreError::Rrd(
                    String::from_utf8_lossy(&message).into_owned(),
                ));
            }
        };
        Ok(ArchiveKind::Classic(cf))
    }
}

/// One archive row to store: one value per DS, in DS order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RowWrite {
    pub archive: usize,
    pub row: u64,
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
    /// End of the last slot the run completes.
    last_slot_end: i128,
    /// Slot length (*D*).
    slot: i128,
    /// Rows written (*K*).
    written: u64,
    /// The time of an opening row completed by the first run of a split
    /// update whose last update has a microsecond part.
    early_opening: Option<i128>,
}

impl PrintedTimes {
    fn new(run: &Run, step: u64, pdp_per_row: u64, written: u64, early: bool) -> Self {
        let step = i128::from(step.max(1));
        let slot = step.saturating_mul(i128::from(pdp_per_row));
        let end = i128::from(run.end);
        let start = i128::from(run.start);
        PrintedTimes {
            last_slot_end: end - end.rem_euclid(slot),
            slot,
            written,
            early_opening: early.then(|| start.wrapping_add(step).wrapping_sub(slot)),
        }
    }

    /// The time of the row `back` rows before the last slot's row.
    fn before_last(&self, back: u64) -> i128 {
        self.last_slot_end
            .wrapping_sub(i128::from(back).wrapping_mul(self.slot))
    }

    fn opening(&self) -> i64 {
        let time = self
            .early_opening
            .unwrap_or_else(|| self.before_last(self.written.saturating_sub(1)));
        wrap_time(time)
    }

    /// The time of the `j`-th row after the opening row.
    fn following(&self, j: u64) -> i64 {
        let back = self.written.saturating_sub(1).saturating_sub(j);
        wrap_time(self.before_last(back).wrapping_add(self.slot))
    }
}

/// Truncates to 64 bits, as the reference's time arithmetic wraps.
fn wrap_time(time: i128) -> i64 {
    time as i64
}

/// A row would be written past the end of the file. The archive's current
/// row has already advanced to it, and the row's first value is in the
/// summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowPastEnd;

// Provenance: specification (docs/compatibility/update.md UP-30, UP-38).
// Compatibility: a row past the end of a damaged file (row count larger
// than the file holds) stops the update at that row.
#[allow(clippy::too_many_arguments)]
fn write_row(
    index: usize,
    archive: &mut RrdArchiveInfo,
    rows_in_file: u64,
    values: Vec<f64>,
    timestamp: i64,
    rows_out: &mut Vec<RowWrite>,
    summaries: &mut Vec<RrdUpdateSummary>,
) -> Result<(), RowPastEnd> {
    // Compatibility: the next row wraps to 0 once it reaches the row count,
    // so a current row stored past the end restarts at row 0, and the
    // largest value wraps to 0 as well.
    let next = archive.current_row.wrapping_add(1);
    archive.current_row = if next >= archive.rows { 0 } else { next };
    if archive.current_row >= rows_in_file {
        // Provenance: specification (docs/compatibility/update.md UP-38).
        summaries.push(RrdUpdateSummary {
            timestamp,
            consolidation: archive.consolidation.clone(),
            pdp_per_row: archive.pdp_per_row,
            values: values.into_iter().take(1).collect(),
        });
        return Err(RowPastEnd);
    }
    summaries.push(RrdUpdateSummary {
        timestamp,
        consolidation: archive.consolidation.clone(),
        pdp_per_row: archive.pdp_per_row,
        values: values.clone(),
    });
    rows_out.push(RowWrite {
        archive: index,
        row: archive.current_row,
        values,
    });
    Ok(())
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
    rows_out: &mut Vec<RowWrite>,
    summaries: &mut Vec<RrdUpdateSummary>,
) -> Result<(), RowPastEnd> {
    let ArchiveKind::Classic(cf) = kind;
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
    write_row(
        index,
        archive,
        rows_in_file,
        opening,
        times.opening(),
        rows_out,
        summaries,
    )?;
    for j in 1..written {
        write_row(
            index,
            archive,
            rows_in_file,
            run.values.clone(),
            times.following(j),
            rows_out,
            summaries,
        )?;
    }
    Ok(())
}

/// Consolidates one run into every archive, in archive order (UP-28). This
/// is the only entry point the update loop uses.
///
/// Holt-Winters archives are coupled PDP by PDP across archives; their later
/// handler takes the whole run for all of its archives here, rather than per
/// archive.
#[allow(clippy::too_many_arguments)]
pub(crate) fn consolidate_all(
    archives: &mut [RrdArchiveInfo],
    kinds: &[ArchiveKind],
    run: &Run,
    step: u64,
    early_first_row: bool,
    rows_in_file: &[u64],
    rows_out: &mut Vec<RowWrite>,
    summaries: &mut Vec<RrdUpdateSummary>,
) -> Result<(), RowPastEnd> {
    // Provenance: specification (docs/compatibility/update.md UP-38). Every
    // archive's CDP state changes for the run, but rows are written archive
    // by archive, so a row past the end of the file keeps the rows and
    // current rows of later archives unwritten.
    let current_rows: Vec<u64> = archives.iter().map(|archive| archive.current_row).collect();
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
                summaries,
            ),
        };
        if result.is_err() && stopped.is_none() {
            stopped = Some((index, rows_out.len(), summaries.len()));
        }
    }
    let Some((failed, rows_len, summaries_len)) = stopped else {
        return Ok(());
    };
    rows_out.truncate(rows_len);
    summaries.truncate(summaries_len);
    for (archive, &current_row) in archives.iter_mut().zip(&current_rows).skip(failed + 1) {
        archive.current_row = current_row;
    }
    Err(RowPastEnd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rrd_binary::{RrdCdpPrepInfo, rrd_nan};
    use std::time::{Duration, Instant};

    const STEP: u64 = 10;

    fn created_cdp() -> RrdCdpPrepInfo {
        RrdCdpPrepInfo {
            value: rrd_nan(),
            unknown_datapoints: 0,
            primary_value: rrd_nan(),
            secondary_value: rrd_nan(),
        }
    }

    fn archive(cf: &str, pdp_per_row: u64, rows: u64, xff: f64) -> RrdArchiveInfo {
        RrdArchiveInfo {
            consolidation: cf.to_owned(),
            raw_consolidation: cf.as_bytes().to_vec(),
            rows,
            pdp_per_row,
            xff,
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
                &mut summaries,
            )
            .unwrap();
        }
        (rows, summaries)
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
        let started = Instant::now();
        let mut rra = archive("AVERAGE", 1, 5, 0.5);
        let (rows, summaries) = feed(&mut rra, &[run(100, 1_000_000_000, 3.0)]);
        assert_eq!(rows.len(), 5);
        assert_eq!(summaries.len(), 5);
        assert_eq!(rra.current_row, 0);

        let mut rra = archive("AVERAGE", 7, 5, 0.5);
        let (rows, _) = feed(&mut rra, &[run(100, 1_000_000_000, 3.0)]);
        assert_eq!(rows.len(), 5);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn huge_pdp_per_row_does_not_overflow() {
        let mut rra = archive("AVERAGE", u64::MAX / 2, 4, 0.5);
        feed(&mut rra, &[run(i64::MAX / 4, u64::MAX / 4, 1.0)]);

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
            &mut summaries,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row, 0);

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
            &mut summaries,
        )
        .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].timestamp, 100);
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
            &mut summaries,
        )
        .unwrap();
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
    fn holt_winters_refused() {
        for cf in [
            "HWPREDICT",
            "MHWPREDICT",
            "SEASONAL",
            "DEVSEASONAL",
            "DEVPREDICT",
            "FAILURES",
        ] {
            match ArchiveKind::from_raw(cf.as_bytes()) {
                Err(StoreError::RrdUnsupported(message)) => assert_eq!(
                    message,
                    format!("Holt-Winters consolidation function {cf} is not supported by Rondi")
                ),
                other => panic!("{cf}: {other:?}"),
            }
        }
        assert_eq!(
            ArchiveKind::from_raw(b"AVERAGE").unwrap(),
            ArchiveKind::Classic(Cf::Average)
        );
        assert!(ArchiveKind::from_raw(b"BOGUS").is_err());
    }
}
