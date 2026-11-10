//! Holt-Winters archives in `update`: the six archive types learn together,
//! one PDP at a time (docs/compatibility/holt-winters.md HW-10 to HW-17,
//! HW-19).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::consolidate::{ArchiveKind, next_row};
use super::pdp::Run;
use crate::holt_winters::{MAX_WINDOW_LEN, Role, cdp, param};
use crate::rrd_binary::{RrdArchiveInfo, RrdCdpPrepInfo, RrdInfo, VALUE_LEN, rrd_mul_add, rrd_nan};
use crate::storage::StoreError;

/// The Holt-Winters archives of a file, by archive index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub prediction: usize,
    pub seasonal: Option<usize>,
    pub dev_seasonal: Option<usize>,
    pub dev_predict: Option<usize>,
    pub failures: Option<usize>,
    pub multiplicative: bool,
}

impl Group {
    /// The group of archive kinds; `None` without a prediction archive or
    /// with two archives of one type.
    // Compatibility: the archives are found by type; a file has at most one
    // archive of each type.
    // Provenance: specification (docs/compatibility/holt-winters.md HW-10).
    pub fn find(kinds: &[ArchiveKind]) -> Option<Group> {
        let mut prediction = None;
        let mut seasonal = None;
        let mut dev_seasonal = None;
        let mut dev_predict = None;
        let mut failures = None;
        let mut multiplicative = false;
        for (index, kind) in kinds.iter().enumerate() {
            let ArchiveKind::HoltWinters(role) = *kind else {
                continue;
            };
            let slot = match role {
                Role::Prediction { multiplicative: m } => {
                    multiplicative = m;
                    &mut prediction
                }
                Role::Seasonal => &mut seasonal,
                Role::DevSeasonal => &mut dev_seasonal,
                Role::DevPredict => &mut dev_predict,
                Role::Failures => &mut failures,
            };
            if slot.replace(index).is_some() {
                return None;
            }
        }
        Some(Group {
            prediction: prediction?,
            seasonal,
            dev_seasonal,
            dev_predict,
            failures,
            multiplicative,
        })
    }
}

impl Group {
    /// Whether every archive's `dependent_rra_idx` names the archive its
    /// type depends on: the prediction archive names SEASONAL, the season
    /// archives name the prediction archive, and DEVPREDICT and FAILURES
    /// name DEVSEASONAL.
    pub fn linked_by_type(&self, archives: &[RrdArchiveInfo]) -> bool {
        let names = |index: Option<usize>, target: Option<usize>| match (index, target) {
            (Some(index), Some(target)) => {
                archives[index].param_u64(param::DEPENDENT) == target as u64
            }
            (Some(_), None) => false,
            (None, _) => true,
        };
        names(Some(self.prediction), self.seasonal)
            && names(self.seasonal, Some(self.prediction))
            && names(self.dev_seasonal, Some(self.prediction))
            && names(self.dev_predict, self.dev_seasonal)
            && names(self.failures, self.dev_seasonal)
    }
}

/// The rows of one season archive that lie inside the file, row-major with
/// one value per DS. A row past them reads as unknown.
#[derive(Debug, Default)]
struct SeasonRows {
    values: Vec<f64>,
    sources: usize,
}

impl SeasonRows {
    fn slot(&self, row: u64, ds: usize) -> Option<usize> {
        usize::try_from(row)
            .ok()?
            .checked_mul(self.sources)?
            .checked_add(ds)
            .filter(|&at| at < self.values.len())
    }

    fn get(&self, row: u64, ds: usize) -> f64 {
        self.slot(row, ds)
            .map_or_else(rrd_nan, |at| self.values[at])
    }

    fn set(&mut self, row: u64, ds: usize, value: f64) {
        if let Some(at) = self.slot(row, ds) {
            self.values[at] = value;
        }
    }

    /// Reads the rows of `archive` that fit in a file of `file_len` bytes.
    // Compatibility: a damaged row count can place rows past the end of the
    // file; those read as unknown, and writing one fails the update.
    // Provenance: specification (docs/compatibility/update.md UP-38).
    fn load(
        file: &mut File,
        archive: &RrdArchiveInfo,
        sources: usize,
        file_len: u64,
    ) -> Result<SeasonRows, StoreError> {
        let row_bytes = (sources * VALUE_LEN) as u64;
        if row_bytes == 0 {
            return Ok(SeasonRows::default());
        }
        let room = file_len.saturating_sub(archive.data_offset);
        let fit = archive.rows.min(room / row_bytes);
        // fit * row_bytes <= room <= file_len, so the buffer is bounded by
        // the file.
        let mut bytes = vec![0u8; (fit * row_bytes) as usize];
        if fit > 0 {
            file.seek(SeekFrom::Start(archive.data_offset))?;
            file.read_exact(&mut bytes)?;
        }
        let values = bytes
            .as_chunks::<VALUE_LEN>()
            .0
            .iter()
            .map(|chunk| f64::from_le_bytes(*chunk))
            .collect();
        Ok(SeasonRows { values, sources })
    }
}

/// The SEASONAL and DEVSEASONAL rows of a group, kept in step with the rows
/// the update writes.
#[derive(Debug, Default)]
pub(crate) struct Seasons {
    seasonal: SeasonRows,
    deviation: SeasonRows,
}

impl Seasons {
    /// Reads the SEASONAL and DEVSEASONAL rows, and no others.
    pub fn load(file: &mut File, info: &RrdInfo, group: &Group) -> Result<Seasons, StoreError> {
        let file_len = file.metadata()?.len();
        let sources = info.data_sources.len();
        let mut load = |index: Option<usize>| match index {
            Some(index) => SeasonRows::load(file, &info.archives[index], sources, file_len),
            None => Ok(SeasonRows::default()),
        };
        Ok(Seasons {
            seasonal: load(group.seasonal)?,
            deviation: load(group.dev_seasonal)?,
        })
    }
}

impl Seasons {
    /// Whether a smoothing pass after the command would change any row:
    /// an archive whose half-width is above 0 and whose rows are all known.
    // Compatibility: the window fraction is the stored smoothing_window for
    // a version of 4 or more and 0.05 otherwise; the half-width is
    // trunc(f·P / 2); an archive holding an unknown row is not smoothed.
    // Provenance: specification (docs/compatibility/holt-winters.md HW-18).
    pub fn pass_changes_rows(
        &self,
        group: &Group,
        archives: &[RrdArchiveInfo],
        version: &str,
    ) -> bool {
        let stored = crate::rrd_binary::version_number(version.as_bytes()) >= 4;
        [
            (group.seasonal, &self.seasonal),
            (group.dev_seasonal, &self.deviation),
        ]
        .into_iter()
        .any(|(index, store)| {
            let Some(index) = index else {
                return false;
            };
            let archive = &archives[index];
            let fraction = if stored {
                archive.param_f64(param::SMOOTHING_WINDOW)
            } else {
                0.05
            };
            let half_width = (fraction * archive.rows as f64 / 2.0).trunc();
            let rows_loaded = store.values.len() as u64 / store.sources.max(1) as u64;
            half_width >= 1.0
                && rows_loaded == archive.rows
                && store.values.iter().all(|value| !value.is_nan())
        })
    }
}

/// The row one Holt-Winters archive writes for a PDP, one value per DS.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupRow {
    pub archive: usize,
    pub values: Vec<f64>,
}

/// *blend*(*w*, *a*, *b*) = *w*·*a* + (1 − *w*)·*b*, fused where the
/// reference build fuses.
// Provenance: specification (docs/compatibility/holt-winters.md HW-10, HW-19).
fn blend(weight: f64, a: f64, b: f64) -> f64 {
    rrd_mul_add(weight, a, (1.0 - weight) * b)
}

/// How the season enters the prediction: added or multiplied.
#[derive(Clone, Copy)]
struct Model {
    multiplicative: bool,
}

impl Model {
    /// *a* ⊖ *b*: the value with the season taken out.
    fn remove(self, a: f64, b: f64) -> f64 {
        if self.multiplicative { a / b } else { a - b }
    }

    /// *a* ⊗ *b*: the level with the season put in.
    fn apply(self, a: f64, b: f64) -> f64 {
        if self.multiplicative { a * b } else { a + b }
    }
}

/// The prediction archive's state for one DS (HW-10).
#[derive(Clone, Copy)]
struct Level {
    intercept: f64,
    last_intercept: f64,
    slope: f64,
    last_slope: f64,
    nan_count: u64,
    last_nan_count: u64,
}

impl Level {
    fn read(prep: &RrdCdpPrepInfo) -> Level {
        Level {
            intercept: prep.word_f64(cdp::INTERCEPT),
            last_intercept: prep.word_f64(cdp::LAST_INTERCEPT),
            slope: prep.word_f64(cdp::SLOPE),
            last_slope: prep.word_f64(cdp::LAST_SLOPE),
            nan_count: prep.word_u64(cdp::NAN_COUNT),
            last_nan_count: prep.word_u64(cdp::LAST_NAN_COUNT),
        }
    }

    fn write(&self, prep: &mut RrdCdpPrepInfo) {
        prep.set_f64(cdp::INTERCEPT, self.intercept);
        prep.set_f64(cdp::LAST_INTERCEPT, self.last_intercept);
        prep.set_f64(cdp::SLOPE, self.slope);
        prep.set_f64(cdp::LAST_SLOPE, self.last_slope);
        prep.set_u64(cdp::NAN_COUNT, self.nan_count);
        prep.set_u64(cdp::LAST_NAN_COUNT, self.last_nan_count);
    }
}

/// Learns from the PDP value `x` with the prefetched coefficient `s`;
/// returns the prediction *p* and the new coefficient *s'*.
// Compatibility: no state starts the level from the first known value; a
// missing coefficient (the first season) predicts nothing and keeps the
// level; otherwise the level, slope and coefficient are smoothed, and an
// unknown value projects the trend one step further.
// Provenance: specification (docs/compatibility/holt-winters.md HW-12, HW-19).
#[allow(clippy::too_many_arguments)]
fn learn(
    level: &mut Level,
    x: f64,
    s: f64,
    alpha: f64,
    beta: f64,
    gamma: f64,
    model: Model,
) -> (f64, f64) {
    level.last_nan_count = level.nan_count;
    if level.intercept.is_nan() {
        if !x.is_nan() {
            level.intercept = x;
            level.last_intercept = x;
            level.slope = 0.0;
            level.last_slope = 0.0;
            level.nan_count = 1;
            level.last_nan_count = 1;
        }
        return (rrd_nan(), model.remove(x, level.intercept));
    }
    level.last_intercept = level.intercept;
    level.last_slope = level.slope;
    if s.is_nan() {
        return (rrd_nan(), model.remove(x, level.intercept));
    }
    let base = rrd_mul_add(level.nan_count as f64, level.slope, level.intercept);
    let prediction = model.apply(base, s);
    if x.is_nan() {
        level.nan_count = level.nan_count.wrapping_add(1);
        return (prediction, s);
    }
    level.intercept = blend(alpha, model.remove(x, s), base);
    level.slope = blend(
        beta,
        level.intercept - level.last_intercept,
        level.last_slope,
    );
    level.nan_count = 1;
    (
        prediction,
        blend(gamma, model.remove(x, level.intercept), s),
    )
}

/// The new deviation from the value `x`, the prediction `p` and the
/// prefetched deviation `d`.
// Provenance: specification (docs/compatibility/holt-winters.md HW-13).
fn deviate(x: f64, p: f64, d: f64, gamma: f64) -> f64 {
    if x.is_nan() {
        d
    } else if p.is_nan() {
        rrd_nan()
    } else if d.is_nan() {
        (x - p).abs()
    } else {
        blend(gamma, (x - p).abs(), d)
    }
}

/// Whether `x` falls outside the band around `p` (a bound itself is inside).
// Provenance: specification (docs/compatibility/holt-winters.md HW-14, HW-19).
fn violates(x: f64, p: f64, d: f64, delta_pos: f64, delta_neg: f64) -> bool {
    if p.is_nan() || d.is_nan() {
        return false;
    }
    x.is_nan() || x > rrd_mul_add(delta_pos, d, p) || x < rrd_mul_add(-delta_neg, d, p)
}

/// Shifts `violated` into the failure history of `prep` (newest first, the
/// first `window` bytes of the entry) and returns the FAILURES row value.
// Compatibility: the history is a byte string from byte 0 of the CDP entry,
// so it overlays the accumulator words.
// Provenance: specification (docs/compatibility/holt-winters.md HW-14).
fn record_failure(prep: &mut RrdCdpPrepInfo, window: u64, threshold: u64, violated: bool) -> f64 {
    let window = window.min(MAX_WINDOW_LEN) as usize;
    let mut bytes: Vec<u8> = prep.words.iter().flat_map(|w| w.to_le_bytes()).collect();
    if window > 0 {
        bytes.copy_within(0..window - 1, 1);
        bytes[0] = u8::from(violated);
    }
    let words = bytes.as_chunks::<8>().0;
    for (slot, chunk) in words.iter().enumerate().take(window.div_ceil(8)) {
        prep.set_u64(slot, u64::from_le_bytes(*chunk));
    }
    let ones = bytes[..window].iter().filter(|&&byte| byte == 1).count() as u64;
    if ones >= threshold { 1.0 } else { 0.0 }
}

/// Advances the season counter of a season archive whose next row is
/// `row`; true when the archive triggers a smoothing pass.
// Compatibility: the counter counts passes through row 0 up to 4; from then
// on the smoothing index triggers.
// Provenance: specification (docs/compatibility/holt-winters.md HW-17).
// Compatibility: the archive has one counter, the first DS's entry; the
// other entries keep their value (vector 058 leaves the second DS at 1).
// Provenance: black-box compatibility test (vector 058, hw2ds).
fn count_season(archive: &mut RrdArchiveInfo, row: u64) -> bool {
    let smooth_at = archive.param_u64(param::SMOOTH_IDX);
    let Some(prep) = archive.cdp_prep.first_mut() else {
        return false;
    };
    let counter = prep.word_u64(cdp::INIT_FLAG);
    if row == 0 && counter < 4 {
        prep.set_u64(cdp::INIT_FLAG, counter + 1);
        true
    } else {
        counter == 4 && row == smooth_at
    }
}

/// Processes a run of one PDP for every Holt-Winters archive of `group`:
/// updates their CDP state and the season rows, and returns the row each
/// archive writes, in archive order. Rows are not queued and current rows
/// do not move; the caller writes each row at its archive's place.
/// `triggered` is set when a season counter reaches a smoothing point.
/// Runs of other lengths are refused before consolidation and give no rows.
pub(crate) fn consolidate_group(
    group: &Group,
    archives: &mut [RrdArchiveInfo],
    seasons: &mut Seasons,
    run: &Run,
    triggered: &mut bool,
) -> Vec<GroupRow> {
    if run.len != 1 {
        return Vec::new();
    }
    let model = Model {
        multiplicative: group.multiplicative,
    };
    // Each archive writes the row after its current one (HW-11).
    let next = |index: Option<usize>| {
        index.map(|index| next_row(archives[index].current_row, archives[index].rows))
    };
    let (seasonal_row, deviation_row) = (next(group.seasonal), next(group.dev_seasonal));
    let param = |index: Option<usize>, slot: usize| {
        index.map_or_else(rrd_nan, |index| archives[index].param_f64(slot))
    };
    let alpha = param(Some(group.prediction), param::ALPHA);
    let beta = param(Some(group.prediction), param::BETA);
    let gamma = param(group.seasonal, param::GAMMA);
    let gamma_dev = param(group.dev_seasonal, param::GAMMA);
    let delta_pos = param(group.failures, param::DELTA_POS);
    let delta_neg = param(group.failures, param::DELTA_NEG);
    let (window, threshold) = group.failures.map_or((0, 0), |index| {
        let archive = &archives[index];
        (
            archive.param_u64(param::WINDOW_LEN),
            archive.param_u64(param::THRESHOLD),
        )
    });

    let mut rows: Vec<GroupRow> = [
        Some(group.prediction),
        group.seasonal,
        group.dev_seasonal,
        group.dev_predict,
        group.failures,
    ]
    .into_iter()
    .flatten()
    .map(|archive| GroupRow {
        archive,
        values: Vec::with_capacity(run.values.len()),
    })
    .collect();
    rows.sort_by_key(|row| row.archive);
    let mut put = |archive: Option<usize>, value: f64| {
        if let Some(row) = rows.iter_mut().find(|row| Some(row.archive) == archive) {
            row.values.push(value);
        }
    };

    for (ds, &x) in run.values.iter().enumerate() {
        let coefficient = |index: Option<usize>| {
            index
                .and_then(|index| archives[index].cdp_prep.get(ds))
                .map_or_else(rrd_nan, |prep| prep.word_f64(cdp::SEASONAL))
        };
        let (s, d) = (coefficient(group.seasonal), coefficient(group.dev_seasonal));
        let Some(prep) = archives[group.prediction].cdp_prep.get_mut(ds) else {
            continue;
        };
        let mut level = Level::read(prep);
        let (prediction, season) = learn(&mut level, x, s, alpha, beta, gamma, model);
        level.write(prep);
        prep.set_f64(cdp::PRIMARY, prediction);
        put(Some(group.prediction), prediction);

        let deviation = deviate(x, prediction, d, gamma_dev);
        // Rows go to the season store, then the next row is prefetched
        // (HW-15, HW-16).
        for (index, row, store, value, used) in [
            (
                group.seasonal,
                seasonal_row,
                &mut seasons.seasonal,
                season,
                s,
            ),
            (
                group.dev_seasonal,
                deviation_row,
                &mut seasons.deviation,
                deviation,
                d,
            ),
        ] {
            let (Some(index), Some(row)) = (index, row) else {
                continue;
            };
            let archive = &mut archives[index];
            store.set(row, ds, value);
            let ahead = store.get(next_row(row, archive.rows), ds);
            if let Some(prep) = archive.cdp_prep.get_mut(ds) {
                prep.set_f64(cdp::LAST_SEASONAL, used);
                prep.set_f64(cdp::SEASONAL, ahead);
                prep.set_f64(cdp::PRIMARY, value);
            }
            put(Some(index), value);
        }
        if let Some(prep) = group
            .dev_predict
            .and_then(|index| archives[index].cdp_prep.get_mut(ds))
        {
            prep.set_f64(cdp::PRIMARY, d);
            put(group.dev_predict, d);
        }
        if let Some(prep) = group
            .failures
            .and_then(|index| archives[index].cdp_prep.get_mut(ds))
        {
            let violated = violates(x, prediction, d, delta_pos, delta_neg);
            let flag = record_failure(prep, window, threshold, violated);
            prep.set_f64(cdp::PRIMARY, flag);
            put(group.failures, flag);
        }
    }

    for (index, row) in [
        (group.seasonal, seasonal_row),
        (group.dev_seasonal, deviation_row),
    ] {
        if let (Some(index), Some(row)) = (index, row) {
            *triggered |= count_season(&mut archives[index], row);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::consolidate::{RowRange, consolidate_all};
    use crate::update::{RrdUpdateSummary, update_rrd_text};
    use std::path::{Path, PathBuf};

    const STEP: u64 = 10;

    fn cdp_entry(sets: &[(usize, u64)]) -> RrdCdpPrepInfo {
        let mut prep = RrdCdpPrepInfo {
            value: rrd_nan(),
            unknown_datapoints: 0,
            primary_value: rrd_nan(),
            secondary_value: rrd_nan(),
            words: [0; 10],
        };
        prep.set_f64(0, rrd_nan());
        prep.set_f64(cdp::PRIMARY, rrd_nan());
        prep.set_f64(cdp::SECONDARY, rrd_nan());
        for &(slot, word) in sets {
            prep.set_u64(slot, word);
        }
        prep
    }

    fn archive(
        cf: &str,
        rows: u64,
        current_row: u64,
        params: &[(usize, u64)],
        prep: RrdCdpPrepInfo,
    ) -> RrdArchiveInfo {
        let mut param_words = [0u64; 10];
        for &(slot, word) in params {
            param_words[slot] = word;
        }
        RrdArchiveInfo {
            consolidation: cf.to_owned(),
            raw_consolidation: cf.as_bytes().to_vec(),
            rows,
            pdp_per_row: 1,
            xff: 0.0,
            param_words,
            current_row,
            cdp_prep: vec![prep],
            data_offset: 0,
        }
    }

    fn f(value: f64) -> u64 {
        value.to_bits()
    }

    fn kinds(archives: &[RrdArchiveInfo]) -> Vec<ArchiveKind> {
        archives
            .iter()
            .map(|a| ArchiveKind::from_raw(&a.raw_consolidation).unwrap())
            .collect()
    }

    /// An additive group with P = 4 and every weight 0.5, in archives 0 to 4.
    fn additive_group() -> Vec<RrdArchiveInfo> {
        vec![
            archive(
                "HWPREDICT",
                8,
                0,
                &[(param::ALPHA, f(0.5)), (param::BETA, f(0.5))],
                cdp_entry(&[
                    (cdp::INTERCEPT, f(10.0)),
                    (cdp::LAST_INTERCEPT, f(9.0)),
                    (cdp::SLOPE, f(1.0)),
                    (cdp::LAST_SLOPE, f(0.5)),
                    (cdp::NAN_COUNT, 1),
                    (cdp::LAST_NAN_COUNT, 1),
                ]),
            ),
            archive(
                "SEASONAL",
                4,
                0,
                &[(param::GAMMA, f(0.5)), (param::SMOOTH_IDX, 2)],
                cdp_entry(&[
                    (cdp::SEASONAL, f(2.0)),
                    (cdp::LAST_SEASONAL, f(1.5)),
                    (cdp::INIT_FLAG, 1),
                ]),
            ),
            archive(
                "DEVSEASONAL",
                4,
                0,
                &[(param::GAMMA, f(0.5)), (param::SMOOTH_IDX, 2)],
                cdp_entry(&[
                    (cdp::SEASONAL, f(1.0)),
                    (cdp::LAST_SEASONAL, f(0.25)),
                    (cdp::INIT_FLAG, 1),
                ]),
            ),
            archive("DEVPREDICT", 8, 0, &[], cdp_entry(&[])),
            archive(
                "FAILURES",
                8,
                0,
                &[
                    (param::DELTA_POS, f(2.0)),
                    (param::DELTA_NEG, f(2.0)),
                    (param::WINDOW_LEN, 9),
                    (param::THRESHOLD, 7),
                ],
                cdp_entry(&[(0, 0)]),
            ),
        ]
    }

    fn seasons(seasonal: &[f64], deviation: &[f64]) -> Seasons {
        Seasons {
            seasonal: SeasonRows {
                values: seasonal.to_vec(),
                sources: 1,
            },
            deviation: SeasonRows {
                values: deviation.to_vec(),
                sources: 1,
            },
        }
    }

    fn one_pdp(start: i64, value: f64) -> Run {
        Run {
            start,
            len: 1,
            end: start + STEP as i64,
            values: vec![value],
        }
    }

    #[test]
    fn find_requires_one_prediction_archive_per_file() {
        let group = additive_group();
        let found = Group::find(&kinds(&group)).unwrap();
        assert_eq!(
            found,
            Group {
                prediction: 0,
                seasonal: Some(1),
                dev_seasonal: Some(2),
                dev_predict: Some(3),
                failures: Some(4),
                multiplicative: false,
            }
        );
        assert_eq!(Group::find(&kinds(&group[1..])), None);
        let mut twice = group.clone();
        twice.push(group[1].clone());
        assert_eq!(Group::find(&kinds(&twice)), None);
    }

    #[test]
    fn single_pdp_learns_like_hw_12() {
        let mut archives = additive_group();
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let mut triggered = false;

        let written = consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &one_pdp(1_000_000_000, 14.0),
            &mut triggered,
        );

        // base = 10 + 1·1 = 11, p = 11 + 2 = 13; I = blend(0.5, 12, 11),
        // T = blend(0.5, 1.5, 1), s' = blend(0.5, 2.5, 2),
        // d' = blend(0.5, |14 − 13|, 1).
        let values: Vec<(usize, Vec<f64>)> = written
            .iter()
            .map(|row| (row.archive, row.values.clone()))
            .collect();
        assert_eq!(
            values,
            vec![
                (0, vec![13.0]),
                (1, vec![2.25]),
                (2, vec![1.0]),
                (3, vec![1.0]),
                (4, vec![0.0]),
            ]
        );
        let level = &archives[0].cdp_prep[0];
        assert_eq!(level.word_f64(cdp::INTERCEPT), 11.5);
        assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 10.0);
        assert_eq!(level.word_f64(cdp::SLOPE), 1.25);
        assert_eq!(level.word_f64(cdp::LAST_SLOPE), 1.0);
        assert_eq!(level.word_u64(cdp::NAN_COUNT), 1);
        assert_eq!(level.word_u64(cdp::LAST_NAN_COUNT), 1);
        assert_eq!(level.primary_value, 13.0);
        assert_eq!(level.word_f64(cdp::PRIMARY), 13.0);
        assert!(level.secondary_value.is_nan());

        // The archives write row 1 (HW-11); the prefetch reads row 2 (HW-16).
        let seasonal = &archives[1].cdp_prep[0];
        assert_eq!(rows.seasonal.get(1, 0), 2.25);
        assert_eq!(seasonal.word_f64(cdp::SEASONAL), 9.0);
        assert_eq!(seasonal.word_f64(cdp::LAST_SEASONAL), 2.0);
        assert_eq!(seasonal.primary_value, 2.25);
        let deviation = &archives[2].cdp_prep[0];
        assert_eq!(rows.deviation.get(1, 0), 1.0);
        assert_eq!(deviation.word_f64(cdp::SEASONAL), 7.0);
        assert_eq!(deviation.word_f64(cdp::LAST_SEASONAL), 1.0);
        assert_eq!(archives[3].cdp_prep[0].primary_value, 1.0);
        let failures = &archives[4].cdp_prep[0];
        assert_eq!(
            crate::holt_winters::failure_history(failures, 9),
            vec![0; 9]
        );
        assert_eq!(failures.primary_value, 0.0);
        // Row 1 is neither row 0 nor the smoothing index.
        assert!(!triggered);
        assert_eq!(seasonal.word_u64(cdp::INIT_FLAG), 1);
        // Current rows are moved by the writes, not here.
        assert!(archives.iter().all(|archive| archive.current_row == 0));
    }

    #[test]
    fn first_value_starts_the_level_and_season() {
        let mut archives = additive_group();
        archives[0].cdp_prep[0] = cdp_entry(&[
            (cdp::INTERCEPT, f(rrd_nan())),
            (cdp::LAST_INTERCEPT, f(rrd_nan())),
            (cdp::SLOPE, f(rrd_nan())),
            (cdp::LAST_SLOPE, f(rrd_nan())),
            (cdp::NAN_COUNT, 1),
            (cdp::LAST_NAN_COUNT, 1),
        ]);
        for index in [1, 2] {
            archives[index].cdp_prep[0].set_f64(cdp::SEASONAL, rrd_nan());
            archives[index].cdp_prep[0].set_f64(cdp::LAST_SEASONAL, rrd_nan());
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        let nan = rrd_nan();
        let mut rows = seasons(&[nan; 4], &[nan; 4]);
        let mut triggered = false;
        let written = consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &one_pdp(1_000_000_000, 14.0),
            &mut triggered,
        );
        let level = &archives[0].cdp_prep[0];
        assert_eq!(level.word_f64(cdp::INTERCEPT), 14.0);
        assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 14.0);
        assert_eq!(level.word_f64(cdp::SLOPE), 0.0);
        assert_eq!(level.word_f64(cdp::LAST_SLOPE), 0.0);
        assert!(written[0].values[0].is_nan());
        // s' = x − I = 0 for the additive model.
        assert_eq!(written[1].values, vec![0.0]);
        assert!(written[2].values[0].is_nan());
        assert!(written[3].values[0].is_nan());
        assert_eq!(written[4].values, vec![0.0]);
    }

    #[test]
    fn season_counter_triggers_at_row_zero() {
        let mut archives = additive_group();
        // The next row of each season archive is row 0. A second DS.
        archives[1].current_row = 3;
        archives[2].current_row = 3;
        for archive in &mut archives {
            let second = archive.cdp_prep[0].clone();
            archive.cdp_prep.push(second);
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        for store in [&mut rows.seasonal, &mut rows.deviation] {
            store.values = store.values.iter().flat_map(|&v| [v, v]).collect();
            store.sources = 2;
        }
        let mut triggered = false;
        consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &Run {
                values: vec![14.0, 14.0],
                ..one_pdp(1_000_000_000, 14.0)
            },
            &mut triggered,
        );
        assert!(triggered);
        assert_eq!(archives[1].cdp_prep[0].word_u64(cdp::INIT_FLAG), 2);
        // Only the first DS holds the counter.
        assert_eq!(archives[1].cdp_prep[1].word_u64(cdp::INIT_FLAG), 1);
        assert_eq!(archives[2].cdp_prep[0].word_u64(cdp::INIT_FLAG), 2);
        // The prefetch after row 0 reads row 1.
        assert_eq!(archives[1].cdp_prep[0].word_f64(cdp::SEASONAL), 8.0);
    }

    #[test]
    fn failure_history_shifts_newest_first() {
        let mut archives = additive_group();
        // Far above p + 2·d = 15, a violation.
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut history = [1u8, 1, 1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
        history[9] = 1; // outside the window of 9: never counted or moved
        let failures = &mut archives[4].cdp_prep[0];
        failures.set_u64(0, u64::from_le_bytes(history[..8].try_into().unwrap()));
        failures.set_u64(1, u64::from_le_bytes(history[8..].try_into().unwrap()));
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let mut triggered = false;
        let written = consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &one_pdp(1_000_000_000, 100.0),
            &mut triggered,
        );
        let failures = &archives[4].cdp_prep[0];
        assert_eq!(
            crate::holt_winters::failure_history(failures, 10),
            vec![1, 1, 1, 1, 1, 1, 1, 0, 0, 1]
        );
        // Seven of the nine bytes are 1.
        assert_eq!(written[4].values, vec![1.0]);
        assert_eq!(failures.value.to_bits(), failures.words[0]);
        assert_eq!(failures.unknown_datapoints, failures.words[1]);
    }

    /// Row ranges as (first row, count, value bits) and summaries as
    /// (time, value bits).
    type Writes = (Vec<(u64, u64, Vec<u64>)>, Vec<(i64, Vec<u64>)>);

    /// Every write of `rows` and `summaries` for archive `index`.
    fn of_archive(
        index: usize,
        rows: &[RowRange],
        summaries: &[RrdUpdateSummary],
        name: &str,
    ) -> Writes {
        let bits = |values: &[f64]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        (
            rows.iter()
                .filter(|range| range.archive == index)
                .map(|range| (range.first_row, range.count, bits(&range.values)))
                .collect(),
            summaries
                .iter()
                .filter(|summary| summary.consolidation == name)
                .map(|summary| (summary.timestamp, bits(&summary.values)))
                .collect(),
        )
    }

    #[test]
    fn classic_archive_unchanged_by_hw_group() {
        let average = || {
            let mut average = archive("AVERAGE", 5, 2, &[(0, f(0.5))], cdp_entry(&[(0, 0)]));
            average.pdp_per_row = 3;
            average.xff = 0.5;
            // Words 2 to 7 carry no classic state; they must survive.
            average.cdp_prep[0].words[3] = 0x0123_4567_89ab_cdef;
            average
        };
        let mut alone = vec![average()];
        let mut mixed = vec![average()];
        mixed.extend(additive_group());
        let alone_kinds = kinds(&alone);
        let mixed_kinds = kinds(&mixed);
        let group = Group::find(&mixed_kinds).unwrap();
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let mut triggered = false;
        let (mut alone_rows, mut alone_summaries) = (Vec::new(), Vec::new());
        let (mut mixed_rows, mut mixed_summaries) = (Vec::new(), Vec::new());
        for (pdp, value) in [3.0, rrd_nan(), 5.5, 1e300, -2.0, 0.0, 7.0]
            .iter()
            .enumerate()
        {
            let run = one_pdp(1_000_000_000 + pdp as i64 * STEP as i64, *value);
            consolidate_all(
                &mut alone,
                &alone_kinds,
                &run,
                STEP,
                false,
                &[],
                &mut alone_rows,
                Some(&mut alone_summaries),
                None,
            )
            .unwrap();
            consolidate_all(
                &mut mixed,
                &mixed_kinds,
                &run,
                STEP,
                false,
                &[],
                &mut mixed_rows,
                Some(&mut mixed_summaries),
                Some((&group, &mut rows, &mut triggered)),
            )
            .unwrap();
        }
        // Debug output spells every field, NaN included.
        assert_eq!(format!("{:?}", mixed[0]), format!("{:?}", alone[0]));
        assert_eq!(mixed[0].cdp_prep[0].words[3], 0x0123_4567_89ab_cdef);
        let alone_writes = of_archive(0, &alone_rows, &alone_summaries, "AVERAGE");
        assert!(!alone_writes.0.is_empty());
        assert_eq!(
            of_archive(0, &mixed_rows, &mixed_summaries, "AVERAGE"),
            alone_writes
        );
        // Each Holt-Winters archive wrote one row per PDP, after the
        // classic archive's rows of the same run.
        assert_eq!(mixed_rows.iter().filter(|r| r.archive == 1).count(), 7);
        assert_eq!(mixed_summaries[0].consolidation, "HWPREDICT");
    }

    fn hwimp() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../rondi-cli/tests/data/hw/inputs/hwimp.rrd")
    }

    /// Offsets in hwimp.rrd: one DS, six archives (AVERAGE, HWPREDICT,
    /// SEASONAL, DEVSEASONAL, DEVPREDICT, FAILURES).
    fn param_offset(archive: usize, slot: usize) -> usize {
        128 + 120 + 120 * archive + 40 + 8 * slot
    }

    fn rows_offset(archive: usize) -> usize {
        128 + 120 + 120 * archive + 24
    }

    fn cdp_offset(archive: usize, slot: usize) -> usize {
        128 + 120 + 120 * 6 + 16 + 112 + 80 * archive + 8 * slot
    }

    fn pointer_offset(archive: usize) -> usize {
        cdp_offset(6, 0) + 8 * archive
    }

    fn patched(dir: &Path, name: &str, patches: &[(usize, u64)]) -> PathBuf {
        let mut bytes = std::fs::read(hwimp()).unwrap();
        for &(offset, word) in patches {
            bytes[offset..offset + 8].copy_from_slice(&word.to_le_bytes());
        }
        let path = dir.join(format!("{name}.rrd"));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn crafted_hw_parameters_do_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        let nan = f(f64::NAN);
        let cases: Vec<(&str, Vec<(usize, u64)>)> = vec![
            ("seasonal-0-rows", vec![(rows_offset(2), 0)]),
            ("seasonal-huge-rows", vec![(rows_offset(2), 4 + (1 << 61))]),
            ("seasonal-max-rows", vec![(rows_offset(2), u64::MAX)]),
            (
                "dependent-out-of-range",
                (1..6)
                    .map(|archive| (param_offset(archive, param::DEPENDENT), u64::MAX))
                    .collect(),
            ),
            (
                "window-nan",
                vec![
                    (param_offset(2, param::SMOOTHING_WINDOW), nan),
                    (param_offset(3, param::SMOOTHING_WINDOW), nan),
                ],
            ),
            (
                "window-huge",
                vec![
                    (param_offset(2, param::SMOOTHING_WINDOW), f(1e308)),
                    (param_offset(3, param::SMOOTHING_WINDOW), f(-1e308)),
                ],
            ),
            (
                "window-len-0",
                vec![(param_offset(5, param::WINDOW_LEN), 0)],
            ),
            (
                "window-len-29",
                vec![(param_offset(5, param::WINDOW_LEN), 29)],
            ),
            (
                "window-len-max",
                vec![(param_offset(5, param::WINDOW_LEN), u64::MAX)],
            ),
            (
                "threshold-above-window",
                vec![(param_offset(5, param::THRESHOLD), 100)],
            ),
            (
                "threshold-max-window-0",
                vec![
                    (param_offset(5, param::THRESHOLD), u64::MAX),
                    (param_offset(5, param::WINDOW_LEN), 0),
                ],
            ),
            (
                "smooth-idx-max",
                vec![
                    (param_offset(2, param::SMOOTH_IDX), u64::MAX),
                    (param_offset(3, param::SMOOTH_IDX), u64::MAX),
                ],
            ),
            (
                "current-rows-max",
                (1..6)
                    .map(|archive| (pointer_offset(archive), u64::MAX))
                    .collect(),
            ),
            (
                "nan-count-max",
                vec![
                    (cdp_offset(1, cdp::NAN_COUNT), u64::MAX),
                    (cdp_offset(2, cdp::INIT_FLAG), u64::MAX),
                ],
            ),
            (
                "weights-nan",
                vec![
                    (param_offset(1, param::ALPHA), nan),
                    (param_offset(1, param::BETA), nan),
                    (param_offset(2, param::GAMMA), nan),
                    (param_offset(5, param::DELTA_POS), nan),
                ],
            ),
        ];
        let arguments: Vec<String> = (1..=12)
            .map(|i| format!("{}:{}", 1_000_000_000 + 10 * i, (i * 7) % 11))
            .collect();
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        for (name, patches) in cases {
            let path = patched(temp.path(), name, &patches);
            let before = std::fs::read(&path).unwrap();
            let started = std::time::Instant::now();
            let (_, result) = update_rrd_text(&path, None, &arguments, false);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{name}: took {:?}",
                started.elapsed()
            );
            if name.starts_with("window-len-29") || name.starts_with("window-len-max") {
                // HW-5: refused before anything is written.
                assert!(result.is_err(), "{name}");
                assert_eq!(std::fs::read(&path).unwrap(), before, "{name}");
            }
        }
    }
}
