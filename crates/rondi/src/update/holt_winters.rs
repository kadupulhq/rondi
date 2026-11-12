//! Holt-Winters archives in `update`: the six archive types learn together,
//! one PDP or run at a time, and a smoothing pass may follow the command
//! (docs/compatibility/holt-winters.md HW-10 to HW-21).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::consolidate::{ArchiveKind, RowRange, advance_rows, next_row};
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

    /// Whether the prediction archive's `dependent_rra_idx` names SEASONAL
    /// and the season archives' name the prediction archive. Rondi updates
    /// only such files; the links of DEVPREDICT and FAILURES may name any
    /// archive (see [`Group::dependent`]).
    pub fn season_links_match_types(&self, archives: &[RrdArchiveInfo]) -> bool {
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
    }

    /// The archive that the `dependent_rra_idx` of archive `index` names,
    /// when it is in range.
    // Compatibility: DEVPREDICT and FAILURES read the archive their stored
    // dependent_rra_idx names, whatever its type, not the DEVSEASONAL
    // archive found by type.
    // Provenance: black-box compatibility test (vectors 037 to 046, and
    // hwimp with the DEVPREDICT and FAILURES links patched to every index).
    fn dependent(archives: &[RrdArchiveInfo], index: usize) -> Option<usize> {
        usize::try_from(archives[index].param_u64(param::DEPENDENT))
            .ok()
            .filter(|&dependent| dependent < archives.len())
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

/// How the smoothing pass treats one season archive.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Window {
    /// The centred running mean of 2*h* + 1 rows, 1 ≤ *h* < P.
    Half(u64),
    /// The x86_64 treatment of an out-of-range half-width (HW-18a).
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Rotate,
}

/// The smoothing window of season archive `archive`, when the pass smooths
/// it: 1 ≤ *h* < P, so the window is narrower than 2P + 1, or, on x86_64
/// only, the rotation of HW-18a.
// Compatibility: the window fraction is the stored smoothing_window for a
// version that reads as 4 or more and 0.05 otherwise; h = trunc(f·P / 2).
// Provenance: specification (docs/compatibility/holt-winters.md HW-18).
// Compatibility: a fraction of 2 or more (h ≥ P), NaN or negative occurs
// only in a crafted file; such an archive is left unsmoothed, so the pass
// stays linear in P. RRDtool smooths at 2 and above (hwsm10 patched to 2.0
// and 2.5), so Rondi diverges there; fractions in (1, 2) match it.
// Provenance: specification (docs/compatibility/holt-winters.md HW-18 does
// not cover fractions above 1; Rondi's choice).
// Compatibility: on x86_64 only, f·P / 2 that is NaN or at most −2^63
// rotates the rows instead (HW-18a); other platforms leave them unsmoothed.
// Provenance: black-box compatibility test (vector 205 and the HW-18a
// probes in docs/compatibility/holt-winters.md).
fn window(archive: &RrdArchiveInfo, version: &str) -> Option<Window> {
    let fraction = if crate::rrd_binary::version_number(version.as_bytes()) >= 4 {
        archive.param_f64(param::SMOOTHING_WINDOW)
    } else {
        0.05
    };
    let half = (fraction * archive.rows as f64 / 2.0).trunc();
    if half >= 1.0 && half < archive.rows as f64 {
        return Some(Window::Half(half as u64));
    }
    #[cfg(target_arch = "x86_64")]
    if half.is_nan() || half <= -(2f64.powi(63)) {
        return Some(Window::Rotate);
    }
    None
}

/// The rows of HW-18a: row *i* takes the value of row *i* − *k* (mod P),
/// *k* = 2^63 mod P, except row *k*, which becomes 0.
fn rotated(rows: &[f64], sources: usize, period: usize) -> Vec<f64> {
    let k = ((1u128 << 63) % period as u128) as usize;
    let mut out = vec![0.0; rows.len()];
    for row in (0..period).filter(|&row| row != k) {
        let from = (row + period - k) % period;
        out[row * sources..(row + 1) * sources]
            .copy_from_slice(&rows[from * sources..(from + 1) * sources]);
    }
    out
}

/// The smoothing pass of an `update` command (HW-18): smooths the SEASONAL
/// and DEVSEASONAL rows in `seasons`, moves the SEASONAL mean into the
/// prediction archive's intercept, and queues every smoothed row, one range
/// per row, in `rows_out`. Prints nothing. Called once, after the last
/// argument of a command that succeeded and whose season counters
/// triggered.
// Compatibility: each row becomes the centred circular running mean of
// 2h + 1 rows, summed from rows −h … h for row 0 and then by subtracting
// row i − h − 1 and adding row i + h; the sums read the rows as they were
// before the pass. SEASONAL then has its mean m removed (subtracted, or
// divided for MHWPREDICT) and the intercept gains m (added for both). An
// archive holding an unknown for any DS is left unchanged for every DS.
// Provenance: specification (docs/compatibility/holt-winters.md HW-18).
pub(crate) fn smooth(
    group: &Group,
    archives: &mut [RrdArchiveInfo],
    seasons: &mut Seasons,
    version: &str,
    rows_out: &mut Vec<RowRange>,
) {
    let model = Model {
        multiplicative: group.multiplicative,
    };
    for (index, store, seasonal) in [
        (group.seasonal, &mut seasons.seasonal, true),
        (group.dev_seasonal, &mut seasons.deviation, false),
    ] {
        let Some(index) = index else { continue };
        let archive = &archives[index];
        let period = archive.rows;
        let Some(window) = window(archive, version) else {
            continue;
        };
        // Every row must lie in the file and be known.
        let sources = store.sources;
        let complete = sources > 0
            && store.values.len() as u64 == period.saturating_mul(sources as u64)
            && store.values.iter().all(|value| !value.is_nan());
        if !complete {
            continue;
        }
        let period = period as usize;
        let at = |row: usize, ds: usize| store.values[row * sources + ds];
        let mut smoothed = match window {
            Window::Half(_) => vec![0.0; store.values.len()],
            Window::Rotate => rotated(&store.values, sources, period),
        };
        for ds in 0..sources {
            if let Window::Half(half) = window {
                let half = half as usize;
                let width = (2 * half + 1) as f64;
                // Row −h is row P − (h mod P), and so on, circularly.
                let wrap = |offset: isize| offset.rem_euclid(period as isize) as usize;
                let mut sum = 0.0;
                for offset in -(half as isize)..=half as isize {
                    sum += at(wrap(offset), ds);
                }
                smoothed[ds] = sum / width;
                for row in 1..period {
                    sum -= at(wrap(row as isize - half as isize - 1), ds);
                    sum += at(wrap(row as isize + half as isize), ds);
                    smoothed[row * sources + ds] = sum / width;
                }
            }
            if seasonal {
                let mut total = 0.0;
                for row in 0..period {
                    total += smoothed[row * sources + ds];
                }
                let mean = total / period as f64;
                for row in 0..period {
                    let value = &mut smoothed[row * sources + ds];
                    *value = model.remove(*value, mean);
                }
                if let Some(prep) = archives[group.prediction].cdp_prep.get_mut(ds) {
                    prep.set_f64(cdp::INTERCEPT, prep.word_f64(cdp::INTERCEPT) + mean);
                }
            }
        }
        store.values = smoothed;
        rows_out.extend(
            store
                .values
                .chunks(sources)
                .enumerate()
                .map(|(row, values)| RowRange {
                    archive: index,
                    first_row: row as u64,
                    count: 1,
                    values: values.to_vec(),
                }),
        );
    }
}

/// `count` rows of the same values, one per DS.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Span {
    pub values: Vec<f64>,
    pub count: u64,
}

/// What one Holt-Winters archive does with a run: the rows it writes after
/// its current row, in order, or (`skipped`) how many rows its current row
/// advances without writing or printing any.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GroupRows {
    pub archive: usize,
    pub spans: Vec<Span>,
    pub skipped: u64,
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
    // Compatibility: a multiplicative divisor that is not above 0 (negative,
    // 0, −0 or unknown) gives an unknown.
    // Provenance: specification (docs/compatibility/holt-winters.md HW-12a).
    fn remove(self, a: f64, b: f64) -> f64 {
        if !self.multiplicative {
            a - b
        } else if b > 0.0 {
            a / b
        } else {
            rrd_nan()
        }
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

/// What one PDP's learning gives the other archives (HW-12).
struct Learned {
    /// The prediction *p* the prediction archive writes.
    written: f64,
    /// The prediction *p̂* the deviation and failure check use; `None` when
    /// HW-12 gives none.
    used: Option<f64>,
    /// The new coefficient *s'*.
    season: f64,
}

/// Learns from the PDP value `x` with the prefetched coefficient `s`.
// Compatibility: an unknown intercept (never set, or made unknown by
// learning) restarts the level from the next known value and blends a known
// coefficient with the new level; an unknown slope freezes the level; a
// missing coefficient (the first season) predicts nothing and keeps the
// level; otherwise the level, slope and coefficient are smoothed, and an
// unknown value projects the trend one step further.
// Provenance: specification (docs/compatibility/holt-winters.md HW-12, HW-19).
fn learn(
    level: &mut Level,
    x: f64,
    s: f64,
    alpha: f64,
    beta: f64,
    gamma: f64,
    model: Model,
) -> Learned {
    level.last_nan_count = level.nan_count;
    if level.intercept.is_nan() {
        level.last_intercept = level.intercept;
        level.last_slope = level.slope;
        if x.is_nan() {
            return Learned {
                written: rrd_nan(),
                used: None,
                season: s,
            };
        }
        level.intercept = x;
        level.last_intercept = x;
        level.slope = 0.0;
        level.last_slope = 0.0;
        level.nan_count = 1;
        level.last_nan_count = 1;
        let deseasoned = model.remove(x, level.intercept);
        // Compatibility: with the coefficient unknown there is no
        // prediction for the deviation, which is then the unknown value
        // itself rather than one derived from it; on x86_64 the two differ
        // in sign.
        // Provenance: black-box compatibility test (x86_64 probes of
        // hwtune after tune --aberrant-reset and with patched intercept,
        // season and deviation words).
        return Learned {
            written: rrd_nan(),
            used: (!s.is_nan()).then(|| model.apply(x, s)),
            season: if s.is_nan() {
                deseasoned
            } else {
                blend(gamma, deseasoned, s)
            },
        };
    }
    level.last_intercept = level.intercept;
    level.last_slope = level.slope;
    if level.slope.is_nan() {
        return Learned {
            written: rrd_nan(),
            used: None,
            season: if x.is_nan() { s } else { rrd_nan() },
        };
    }
    if s.is_nan() {
        return Learned {
            written: rrd_nan(),
            used: None,
            season: model.remove(x, level.intercept),
        };
    }
    let base = rrd_mul_add(level.nan_count as f64, level.slope, level.intercept);
    let prediction = model.apply(base, s);
    let learned = |season| Learned {
        written: prediction,
        used: Some(prediction),
        season,
    };
    if x.is_nan() {
        level.nan_count = level.nan_count.wrapping_add(1);
        return learned(s);
    }
    level.intercept = blend(alpha, model.remove(x, s), base);
    level.slope = blend(
        beta,
        level.intercept - level.last_intercept,
        level.last_slope,
    );
    level.nan_count = 1;
    learned(blend(gamma, model.remove(x, level.intercept), s))
}

/// The new deviation from the value `x`, the prediction `p` the deviation
/// uses (`None` when there is none) and the prefetched deviation `d`.
// Provenance: specification (docs/compatibility/holt-winters.md HW-13).
fn deviate(x: f64, p: Option<f64>, d: f64, gamma: f64) -> f64 {
    let Some(p) = p else { return rrd_nan() };
    if x.is_nan() {
        d
    } else if d.is_nan() {
        (x - p).abs()
    } else {
        blend(gamma, (x - p).abs(), d)
    }
}

/// Whether `x` falls outside the band around `p` (a bound itself is inside).
// Compatibility: an unknown value is a violation whenever the deviation is
// known, whether or not there is a prediction.
// Provenance: specification (docs/compatibility/holt-winters.md HW-14, HW-19).
fn violates(x: f64, p: Option<f64>, d: f64, delta_pos: f64, delta_neg: f64) -> bool {
    if d.is_nan() {
        return false;
    }
    let p = p.unwrap_or_else(rrd_nan);
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

/// Advances the season counter of a season archive whose current row moves
/// through the `count` rows from `first` on; true when the archive
/// triggers a smoothing pass.
// Compatibility: the counter counts passes through row 0 up to 4; from then
// on the smoothing index triggers. A run follows each row it reaches.
// Provenance: specification (docs/compatibility/holt-winters.md HW-17,
// HW-21).
// Compatibility: the archive has one counter, the first DS's entry; the
// other entries keep their value (vector 058 leaves the second DS at 1).
// Provenance: black-box compatibility test (vector 058, hw2ds).
fn count_season(archive: &mut RrdArchiveInfo, first: u64, count: u64) -> bool {
    let rows = archive.rows;
    let smooth_at = archive.param_u64(param::SMOOTH_IDX);
    // At most `rows` positions, all distinct, so each row is reached at
    // most once and the order of row 0 and the smoothing index does not
    // matter.
    let reached = |row: u64| {
        row < rows
            && (count >= rows
                || (u128::from(row) + u128::from(rows) - u128::from(first)) % u128::from(rows)
                    < u128::from(count))
    };
    let Some(prep) = archive.cdp_prep.first_mut() else {
        return false;
    };
    if count == 0 {
        return false;
    }
    let counter = prep.word_u64(cdp::INIT_FLAG);
    if reached(0) && counter < 4 {
        prep.set_u64(cdp::INIT_FLAG, counter + 1);
        true
    } else {
        counter == 4 && reached(smooth_at)
    }
}

/// Stores the value of PDP `pdp` of a run of `len` into words 8 and 9: a
/// lone PDP sets *primary_value*, a pair sets *primary_value* and then
/// *secondary_value*.
// Provenance: specification (docs/compatibility/holt-winters.md HW-15,
// HW-20).
// Compatibility: DEVPREDICT and FAILURES store their pair the same way.
// Provenance: black-box compatibility test (hwimp, a run of two PDPs).
fn store_result(prep: &mut RrdCdpPrepInfo, pdp: u64, len: u64, value: f64) {
    let slot = if len >= 2 && pdp == 1 {
        cdp::SECONDARY
    } else {
        cdp::PRIMARY
    };
    prep.set_f64(slot, value);
}

/// Every archive of `group`, in archive order, with nothing written yet.
fn empty_rows(group: &Group) -> Vec<GroupRows> {
    let mut rows: Vec<GroupRows> = [
        Some(group.prediction),
        group.seasonal,
        group.dev_seasonal,
        group.dev_predict,
        group.failures,
    ]
    .into_iter()
    .flatten()
    .map(|archive| GroupRows {
        archive,
        spans: Vec::new(),
        skipped: 0,
    })
    .collect();
    rows.sort_by_key(|row| row.archive);
    rows
}

fn rows_of(rows: &mut [GroupRows], archive: Option<usize>) -> Option<&mut GroupRows> {
    rows.iter_mut().find(|row| Some(row.archive) == archive)
}

/// Processes a run for every Holt-Winters archive of `group`: updates their
/// CDP state and the season rows, and returns what each archive writes, in
/// archive order. Current rows do not move; the caller writes each
/// archive's rows at its place. `triggered` is set when a season counter
/// reaches a smoothing point.
pub(crate) fn consolidate_group(
    group: &Group,
    archives: &mut [RrdArchiveInfo],
    seasons: &mut Seasons,
    run: &Run,
    triggered: &mut bool,
) -> Vec<GroupRows> {
    let mut rows = empty_rows(group);
    match run.len {
        0 => {}
        1 | 2 => {
            for pdp in 0..run.len {
                learn_pdp(group, archives, seasons, run, pdp, &mut rows, triggered);
            }
        }
        _ => skip_run(group, archives, seasons, run, &mut rows, triggered),
    }
    rows
}

/// PDP `pdp` (0 or 1) of a run of one or two PDPs of `run.values`.
// Compatibility: a lone PDP writes the season rows and prefetches the row
// after the one written; a pair writes no season row, keeps each new
// coefficient and deviation only in words 8 and 9, and prefetches two rows
// ahead.
// Provenance: specification (docs/compatibility/holt-winters.md HW-11 to
// HW-17, HW-20).
fn learn_pdp(
    group: &Group,
    archives: &mut [RrdArchiveInfo],
    seasons: &mut Seasons,
    run: &Run,
    pdp: u64,
    rows: &mut [GroupRows],
    triggered: &mut bool,
) {
    let model = Model {
        multiplicative: group.multiplicative,
    };
    let lone = run.len == 1;
    // This PDP reaches the row pdp + 1 after the current one (HW-11).
    let reached = |index: Option<usize>| {
        index.map(|index| {
            let archive = &archives[index];
            advance_rows(archive.current_row, archive.rows, pdp + 1)
        })
    };
    let (seasonal_row, deviation_row) = (reached(group.seasonal), reached(group.dev_seasonal));
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
    let dev_predict_source = group
        .dev_predict
        .and_then(|index| Group::dependent(archives, index));
    let failures_source = group
        .failures
        .and_then(|index| Group::dependent(archives, index));
    let mut values: Vec<(Option<usize>, Vec<f64>)> = [
        Some(group.prediction),
        group.seasonal,
        group.dev_seasonal,
        group.dev_predict,
        group.failures,
    ]
    .into_iter()
    .map(|index| (index, Vec::with_capacity(run.values.len())))
    .collect();
    let mut put = |archive: Option<usize>, value: f64| {
        if let Some((_, row)) = values.iter_mut().find(|(index, _)| *index == archive) {
            row.push(value);
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
        let learned = learn(&mut level, x, s, alpha, beta, gamma, model);
        let season = learned.season;
        level.write(prep);
        store_result(prep, pdp, run.len, learned.written);
        put(Some(group.prediction), learned.written);

        let deviation = deviate(x, learned.used, d, gamma_dev);
        // A lone PDP writes its row to the season store before the prefetch
        // reads the row after it (HW-15, HW-16); a pair reads two rows
        // ahead and writes no row (HW-20).
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
            if lone {
                store.set(row, ds, value);
                put(Some(index), value);
            }
            let ahead = store.get(advance_rows(row, archive.rows, run.len), ds);
            if let Some(prep) = archive.cdp_prep.get_mut(ds) {
                prep.set_f64(cdp::LAST_SEASONAL, used);
                prep.set_f64(cdp::SEASONAL, ahead);
                store_result(prep, pdp, run.len, value);
            }
        }
        // Compatibility: DEVPREDICT writes word 3 of the archive its link
        // names, after this PDP's prediction and season updates: the
        // deviation used when that is DEVSEASONAL, the coefficient used for
        // SEASONAL, the last intercept for the prediction archive, and the
        // stored word otherwise (0 in vectors 037 to 046).
        // Provenance: black-box compatibility test (vectors 037 to 046;
        // hwimp with the DEVPREDICT link patched to each archive).
        if let Some(index) = group.dev_predict {
            let used = dev_predict_source
                .and_then(|source| archives[source].cdp_prep.get(ds))
                .map_or_else(rrd_nan, |prep| prep.word_f64(cdp::LAST_SEASONAL));
            if let Some(prep) = archives[index].cdp_prep.get_mut(ds) {
                store_result(prep, pdp, run.len, used);
                put(Some(index), used);
            }
        }
        // Compatibility: FAILURES checks the value against the prediction
        // only when the archive its link names itself links to the
        // prediction archive; the deviation is that archive's word 3 after
        // this PDP's updates (DEVSEASONAL's deviation used, SEASONAL's
        // coefficient used, a DEVPREDICT's stored word). Otherwise it stores
        // the PDP value and keeps its history.
        // Provenance: black-box compatibility test (vectors 037 to 046;
        // hwimp, hwfail and hwsw0 with the DEVPREDICT and FAILURES links
        // patched to each archive).
        if let Some(index) = group.failures {
            let band = failures_source
                .filter(|&source| Group::dependent(archives, source) == Some(group.prediction))
                .map(|source| {
                    archives[source]
                        .cdp_prep
                        .get(ds)
                        .map_or_else(rrd_nan, |prep| prep.word_f64(cdp::LAST_SEASONAL))
                });
            if let Some(prep) = archives[index].cdp_prep.get_mut(ds) {
                let flag = match band {
                    Some(d) => {
                        let violated = violates(x, learned.used, d, delta_pos, delta_neg);
                        record_failure(prep, window, threshold, violated)
                    }
                    None => x,
                };
                store_result(prep, pdp, run.len, flag);
                put(Some(index), flag);
            }
        }
    }

    for (index, row) in [
        (group.seasonal, seasonal_row),
        (group.dev_seasonal, deviation_row),
    ] {
        if let (Some(index), Some(row)) = (index, row) {
            *triggered |= count_season(&mut archives[index], row, 1);
            if let (false, Some(rows)) = (lone, rows_of(rows, Some(index))) {
                rows.skipped += 1;
            }
        }
    }
    for (index, values) in values {
        if values.is_empty() {
            continue;
        }
        if let Some(rows) = rows_of(rows, index) {
            rows.spans.push(Span { values, count: 1 });
        }
    }
}

/// A run of three or more PDPs, whose value is ignored.
// Compatibility: each archive moves min(m, rows) rows: unknown prediction
// and DEVPREDICT rows, 0 in FAILURES, the season archives' rows unchanged;
// the level and slope stay, the NaN counts grow by m and m − 1, the
// failure history clears, and the season archives prefetch rows r0 + m and
// r0 + m + 1.
// Provenance: specification (docs/compatibility/holt-winters.md HW-21).
// Compatibility: the season archives write and print no row for the run;
// DEVPREDICT's words 8 and 9 become unknown and FAILURES' 0, also when
// their links name other archives.
// Provenance: black-box compatibility test (hwimp and hwsw0, runs of three
// and five PDPs).
fn skip_run(
    group: &Group,
    archives: &mut [RrdArchiveInfo],
    seasons: &Seasons,
    run: &Run,
    rows: &mut [GroupRows],
    triggered: &mut bool,
) {
    let len = run.len;
    let sources = run.values.len();
    let written = |archive: &RrdArchiveInfo| len.min(archive.rows);

    let prediction = &mut archives[group.prediction];
    for prep in prediction.cdp_prep.iter_mut().take(sources) {
        prep.set_u64(
            cdp::NAN_COUNT,
            prep.word_u64(cdp::NAN_COUNT).wrapping_add(len),
        );
        prep.set_u64(
            cdp::LAST_NAN_COUNT,
            prep.word_u64(cdp::LAST_NAN_COUNT).wrapping_add(len - 1),
        );
        prep.set_f64(cdp::PRIMARY, rrd_nan());
        prep.set_f64(cdp::SECONDARY, rrd_nan());
    }
    let count = written(prediction);
    if let Some(rows) = rows_of(rows, Some(group.prediction)) {
        rows.spans.push(Span {
            values: vec![rrd_nan(); sources],
            count,
        });
    }

    for (index, store) in [
        (group.seasonal, &seasons.seasonal),
        (group.dev_seasonal, &seasons.deviation),
    ] {
        let Some(index) = index else { continue };
        let archive = &mut archives[index];
        let (current, period) = (archive.current_row, archive.rows);
        let last = advance_rows(current, period, len);
        let after = next_row(last, period);
        for (ds, prep) in archive.cdp_prep.iter_mut().enumerate().take(sources) {
            prep.set_f64(cdp::LAST_SEASONAL, store.get(last, ds));
            prep.set_f64(cdp::SEASONAL, store.get(after, ds));
        }
        let count = written(archive);
        *triggered |= count_season(archive, next_row(current, period), count);
        if let Some(rows) = rows_of(rows, Some(index)) {
            rows.skipped = count;
        }
    }

    for (index, value) in [(group.dev_predict, rrd_nan()), (group.failures, 0.0)] {
        let Some(index) = index else { continue };
        let archive = &mut archives[index];
        let window = archive.param_u64(param::WINDOW_LEN).min(MAX_WINDOW_LEN) as usize;
        let failures = Some(index) == group.failures;
        for prep in archive.cdp_prep.iter_mut().take(sources) {
            if failures {
                clear_history(prep, window);
            }
            prep.set_f64(cdp::PRIMARY, value);
            prep.set_f64(cdp::SECONDARY, value);
        }
        let count = written(archive);
        if let Some(rows) = rows_of(rows, Some(index)) {
            rows.spans.push(Span {
                values: vec![value; sources],
                count,
            });
        }
    }
}

/// Sets the first `window` bytes of the CDP entry, the failure history, to
/// zero.
// Provenance: specification (docs/compatibility/holt-winters.md HW-21).
fn clear_history(prep: &mut RrdCdpPrepInfo, window: usize) {
    let mut bytes: Vec<u8> = prep.words.iter().flat_map(|w| w.to_le_bytes()).collect();
    bytes[..window].fill(0);
    let words = bytes.as_chunks::<8>().0;
    for (slot, chunk) in words.iter().enumerate().take(window.div_ceil(8)) {
        prep.set_u64(slot, u64::from_le_bytes(*chunk));
    }
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

    /// An additive group with P = 4 and every weight 0.5, in archives 0 to 4,
    /// each linked to the archive its type depends on.
    fn additive_group() -> Vec<RrdArchiveInfo> {
        vec![
            archive(
                "HWPREDICT",
                8,
                0,
                &[
                    (param::ALPHA, f(0.5)),
                    (param::BETA, f(0.5)),
                    (param::DEPENDENT, 1),
                ],
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
            archive("DEVPREDICT", 8, 0, &[(param::DEPENDENT, 2)], cdp_entry(&[])),
            archive(
                "FAILURES",
                8,
                0,
                &[
                    (param::DELTA_POS, f(2.0)),
                    (param::DELTA_NEG, f(2.0)),
                    (param::WINDOW_LEN, 9),
                    (param::THRESHOLD, 7),
                    (param::DEPENDENT, 2),
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

    /// The values of the one row an archive writes for a lone PDP.
    fn lone(rows: &GroupRows) -> Vec<f64> {
        assert_eq!(rows.skipped, 0);
        assert_eq!(rows.spans.len(), 1, "{rows:?}");
        assert_eq!(rows.spans[0].count, 1);
        rows.spans[0].values.clone()
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
        let values: Vec<(usize, Vec<f64>)> =
            written.iter().map(|row| (row.archive, lone(row))).collect();
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
        assert!(lone(&written[0])[0].is_nan());
        // s' = x − I = 0 for the additive model.
        assert_eq!(lone(&written[1]), vec![0.0]);
        assert!(lone(&written[2])[0].is_nan());
        assert!(lone(&written[3])[0].is_nan());
        assert_eq!(lone(&written[4]), vec![0.0]);
    }

    /// Runs one PDP of value `x` through `archives` with the given rows.
    fn learn_one(
        archives: &mut [RrdArchiveInfo],
        rows: &mut Seasons,
        x: f64,
    ) -> Vec<(usize, Vec<f64>)> {
        let group = Group::find(&kinds(archives)).unwrap();
        let mut triggered = false;
        consolidate_group(
            &group,
            archives,
            rows,
            &one_pdp(1_000_000_000, x),
            &mut triggered,
        )
        .iter()
        .map(|row| (row.archive, lone(row)))
        .collect()
    }

    fn multiplicative_group() -> Vec<RrdArchiveInfo> {
        let mut archives = additive_group();
        archives[0].consolidation = "MHWPREDICT".into();
        archives[0].raw_consolidation = b"MHWPREDICT".to_vec();
        archives
    }

    #[test]
    fn unknown_intercept_and_season_leave_no_prediction() {
        // HW-12 case 1 with s unknown: there is no p̂, so the deviation is
        // the unknown value itself (its sign shows on x86_64), whether d is
        // known or not.
        for d in [1.0, rrd_nan()] {
            let mut archives = additive_group();
            archives[0].cdp_prep[0].set_f64(cdp::INTERCEPT, rrd_nan());
            archives[1].cdp_prep[0].set_f64(cdp::SEASONAL, rrd_nan());
            archives[2].cdp_prep[0].set_f64(cdp::SEASONAL, d);
            let mut rows = seasons(&[rrd_nan(); 4], &[d; 4]);
            let written = learn_one(&mut archives, &mut rows, 14.0);
            assert_eq!(written[2].1[0].to_bits(), rrd_nan().to_bits(), "d = {d}");
        }
    }

    #[test]
    fn unknown_intercept_restarts_and_blends_the_known_season() {
        // HW-12 case 1 with s = 2 and d = 1 known: I = x, T = 0, p unknown,
        // s' = blend(0.5, 0, 2), and the deviation uses x + s = 16.
        let mut archives = additive_group();
        let prep = &mut archives[0].cdp_prep[0];
        prep.set_f64(cdp::INTERCEPT, rrd_nan());
        prep.set_u64(cdp::NAN_COUNT, 3);
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let written = learn_one(&mut archives, &mut rows, 14.0);
        assert!(written[0].1[0].is_nan());
        assert_eq!(written[1].1, vec![1.0]);
        assert_eq!(written[2].1, vec![1.5]);
        assert_eq!(written[3].1, vec![1.0]);
        assert_eq!(written[4].1, vec![0.0]);
        let level = &archives[0].cdp_prep[0];
        assert_eq!(level.word_f64(cdp::INTERCEPT), 14.0);
        assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 14.0);
        assert_eq!(level.word_f64(cdp::SLOPE), 0.0);
        assert_eq!(level.word_f64(cdp::LAST_SLOPE), 0.0);
        assert_eq!(level.word_u64(cdp::NAN_COUNT), 1);
        assert_eq!(level.word_u64(cdp::LAST_NAN_COUNT), 1);

        // An unknown value keeps the level, leaves no prediction for the
        // deviation (d' unknown though d is known) and is a violation.
        let mut archives = additive_group();
        let prep = &mut archives[0].cdp_prep[0];
        prep.set_f64(cdp::INTERCEPT, rrd_nan());
        prep.set_u64(cdp::NAN_COUNT, 3);
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let written = learn_one(&mut archives, &mut rows, rrd_nan());
        assert!(written[0].1[0].is_nan());
        assert_eq!(written[1].1, vec![2.0]);
        assert!(written[2].1[0].is_nan());
        let level = &archives[0].cdp_prep[0];
        assert!(level.word_f64(cdp::INTERCEPT).is_nan());
        assert!(level.word_f64(cdp::LAST_INTERCEPT).is_nan());
        assert_eq!(level.word_f64(cdp::SLOPE), 1.0);
        assert_eq!(level.word_f64(cdp::LAST_SLOPE), 1.0);
        assert_eq!(level.word_u64(cdp::NAN_COUNT), 3);
        assert_eq!(
            crate::holt_winters::failure_history(&archives[4].cdp_prep[0], 9)[0],
            1
        );
    }

    #[test]
    fn unknown_slope_freezes_the_level() {
        // HW-12 case 2: no prediction, I, T and n unchanged, s' unknown for
        // a known value and s for an unknown one, d' unknown.
        for (x, season) in [(14.0, rrd_nan()), (rrd_nan(), 2.0)] {
            let mut archives = additive_group();
            let prep = &mut archives[0].cdp_prep[0];
            prep.set_f64(cdp::SLOPE, rrd_nan());
            prep.set_u64(cdp::NAN_COUNT, 3);
            let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
            let written = learn_one(&mut archives, &mut rows, x);
            assert!(written[0].1[0].is_nan());
            assert_eq!(written[1].1[0].to_bits(), season.to_bits());
            assert!(written[2].1[0].is_nan());
            let level = &archives[0].cdp_prep[0];
            assert_eq!(level.word_f64(cdp::INTERCEPT), 10.0);
            assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 10.0);
            assert!(level.word_f64(cdp::SLOPE).is_nan());
            assert!(level.word_f64(cdp::LAST_SLOPE).is_nan());
            assert_eq!(level.word_u64(cdp::NAN_COUNT), 3);
            assert_eq!(level.word_u64(cdp::LAST_NAN_COUNT), 3);
            let violated = u8::from(x.is_nan());
            assert_eq!(
                crate::holt_winters::failure_history(&archives[4].cdp_prep[0], 9)[0],
                violated
            );
        }
    }

    #[test]
    fn multiplicative_division_by_non_positive_is_unknown() {
        // HW-12a: s = −1 makes x ⊖ s unknown, so I, T and s' are unknown;
        // p = (10 + 1) × −1 is still written and used by the deviation.
        let mut archives = multiplicative_group();
        archives[1].cdp_prep[0].set_f64(cdp::SEASONAL, -1.0);
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let written = learn_one(&mut archives, &mut rows, 14.0);
        assert_eq!(written[0].1, vec![-11.0]);
        assert!(written[1].1[0].is_nan());
        assert_eq!(written[2].1, vec![13.0]);
        let level = &archives[0].cdp_prep[0];
        assert!(level.word_f64(cdp::INTERCEPT).is_nan());
        assert!(level.word_f64(cdp::SLOPE).is_nan());
        assert_eq!(level.word_u64(cdp::NAN_COUNT), 1);

        // A new intercept at or below 0 keeps I and T but not s'.
        let mut archives = multiplicative_group();
        archives[1].cdp_prep[0].set_f64(cdp::SEASONAL, 1.0);
        let mut rows = seasons(&[7.0, 8.0, 9.0, 10.0], &[5.0, 6.0, 7.0, 8.0]);
        let written = learn_one(&mut archives, &mut rows, -11.0);
        // I = blend(0.5, −11, 11) = 0.
        assert_eq!(archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT), 0.0);
        assert!(written[1].1[0].is_nan());

        // A first value below 0 gives an unknown first coefficient.
        let mut archives = multiplicative_group();
        archives[0].cdp_prep[0].set_f64(cdp::INTERCEPT, rrd_nan());
        archives[1].cdp_prep[0].set_f64(cdp::SEASONAL, rrd_nan());
        let nan = rrd_nan();
        let mut rows = seasons(&[nan; 4], &[nan; 4]);
        let written = learn_one(&mut archives, &mut rows, -5.0);
        assert_eq!(archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT), -5.0);
        assert!(written[1].1[0].is_nan());
    }

    #[test]
    fn multiplicative_smoothing_with_mean_not_above_zero_is_unknown() {
        let mut archives = multiplicative_group();
        for index in [1, 2] {
            archives[index].param_words[param::SMOOTHING_WINDOW] = f(0.5);
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut rows = seasons(&[-1.0, -2.0, -4.0, 1.0], &[3.0, 1.0, 2.0, 6.0]);
        smooth(&group, &mut archives, &mut rows, "0004", &mut Vec::new());
        assert!(rows.seasonal.values.iter().all(|v| v.is_nan()));
        // The mean −1.5 is still added to the intercept.
        let intercept = archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT);
        assert!(close(&[intercept], &[8.5]), "{intercept}");
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
        assert_eq!(lone(&written[4]), vec![1.0]);
        assert_eq!(failures.value.to_bits(), failures.words[0]);
        assert_eq!(failures.unknown_datapoints, failures.words[1]);
    }

    /// The additive group with P = 5 and both season archives at row 1.
    fn period_five_group() -> (Vec<RrdArchiveInfo>, Seasons) {
        let mut archives = additive_group();
        for index in [1, 2] {
            archives[index].rows = 5;
            archives[index].current_row = 1;
        }
        let rows = seasons(&[7.0, 8.0, 9.0, 10.0, 11.0], &[5.0, 6.0, 7.0, 8.0, 9.0]);
        (archives, rows)
    }

    fn run_of(len: u64, value: f64) -> Run {
        Run {
            start: 1_000_000_000,
            len,
            end: 1_000_000_000 + (len * STEP) as i64,
            values: vec![value],
        }
    }

    #[test]
    fn run_of_two_prefetches_two_ahead() {
        let (mut archives, mut rows) = period_five_group();
        // The newest failure result is 1; two shifts move it to byte 2.
        archives[4].cdp_prep[0].set_u64(0, 1);
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut triggered = false;
        let written = consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &run_of(2, 14.0),
            &mut triggered,
        );

        // PDP 1 as in single_pdp_learns_like_hw_12: p = 13, I = 11.5,
        // T = 1.25, s' = 2.25, d' = 1; its prefetch reads row 1 + 1 + 2 = 4.
        // PDP 2: base = 12.75, p = 12.75 + 11, I = blend(0.5, 3, 12.75),
        // T = blend(0.5, I − 11.5, 1.25), s' = blend(0.5, 14 − I, 11).
        let level = &archives[0].cdp_prep[0];
        assert_eq!(level.primary_value, 13.0);
        assert_eq!(level.secondary_value, 23.75);
        assert_eq!(level.word_f64(cdp::INTERCEPT), 7.875);
        assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 11.5);
        assert_eq!(level.word_f64(cdp::SLOPE), -1.1875);
        let predictions: Vec<f64> = written[0].spans.iter().map(|span| span.values[0]).collect();
        assert_eq!(predictions, vec![13.0, 23.75]);

        let seasonal = &archives[1].cdp_prep[0];
        // After the run slot 2 holds row r0 + 4 = 0 and slot 3 the second
        // PDP's coefficient, row 4.
        assert_eq!(seasonal.word_f64(cdp::SEASONAL), 7.0);
        assert_eq!(seasonal.word_f64(cdp::LAST_SEASONAL), 11.0);
        assert_eq!(seasonal.primary_value, 2.25);
        assert_eq!(seasonal.secondary_value, 8.5625);
        let deviation = &archives[2].cdp_prep[0];
        assert_eq!(deviation.word_f64(cdp::SEASONAL), 5.0);
        assert_eq!(deviation.word_f64(cdp::LAST_SEASONAL), 9.0);
        // The season rows are unchanged and nothing is written to them.
        assert_eq!(rows.seasonal.values, vec![7.0, 8.0, 9.0, 10.0, 11.0]);
        assert_eq!(rows.deviation.values, vec![5.0, 6.0, 7.0, 8.0, 9.0]);
        for index in [1, 2] {
            assert!(written[index].spans.is_empty());
            assert_eq!(written[index].skipped, 2);
        }
        // DEVPREDICT writes the deviation each PDP used.
        let dev_rows: Vec<f64> = written[3].spans.iter().map(|span| span.values[0]).collect();
        assert_eq!(dev_rows, vec![1.0, 9.0]);
        assert_eq!(archives[3].cdp_prep[0].primary_value, 1.0);
        assert_eq!(archives[3].cdp_prep[0].secondary_value, 9.0);
        let failures = &archives[4].cdp_prep[0];
        assert_eq!(
            crate::holt_winters::failure_history(failures, 9),
            vec![0, 0, 1, 0, 0, 0, 0, 0, 0]
        );
        assert!(!triggered);
    }

    #[test]
    fn run_of_many_ignores_value() {
        let outcome = |value: f64| {
            let (mut archives, mut rows) = period_five_group();
            for archive in &mut archives {
                archive.rows = 5;
            }
            archives[4].cdp_prep[0].set_u64(0, 0x0101_0001_0100_0101);
            archives[4].cdp_prep[0].set_u64(1, 0x01);
            let group = Group::find(&kinds(&archives)).unwrap();
            let mut triggered = false;
            let written = consolidate_group(
                &group,
                &mut archives,
                &mut rows,
                &run_of(7, value),
                &mut triggered,
            );
            (archives, rows, written, triggered)
        };
        let (archives, rows, written, triggered) = outcome(14.0);
        let (unknown, _, unknown_written, _) = outcome(rrd_nan());
        assert_eq!(format!("{archives:?}"), format!("{unknown:?}"));
        assert_eq!(format!("{written:?}"), format!("{unknown_written:?}"));

        // K = min(7, 5) = 5 rows in every archive.
        let nan_bits = rrd_nan().to_bits();
        for (index, bits) in [(0, nan_bits), (3, nan_bits), (4, 0)] {
            assert_eq!(written[index].spans.len(), 1);
            assert_eq!(written[index].spans[0].count, 5);
            assert_eq!(written[index].spans[0].values[0].to_bits(), bits);
        }
        for index in [1, 2] {
            assert!(written[index].spans.is_empty());
            assert_eq!(written[index].skipped, 5);
        }
        let level = &archives[0].cdp_prep[0];
        assert_eq!(level.word_f64(cdp::INTERCEPT), 10.0);
        assert_eq!(level.word_f64(cdp::LAST_INTERCEPT), 9.0);
        assert_eq!(level.word_f64(cdp::SLOPE), 1.0);
        assert_eq!(level.word_f64(cdp::LAST_SLOPE), 0.5);
        assert_eq!(level.word_u64(cdp::NAN_COUNT), 1 + 7);
        assert_eq!(level.word_u64(cdp::LAST_NAN_COUNT), 1 + 6);
        assert!(level.primary_value.is_nan() && level.secondary_value.is_nan());
        // Slot 3 is row (1 + 7) mod 5 = 3 and slot 2 the row after it.
        let seasonal = &archives[1].cdp_prep[0];
        assert_eq!(seasonal.word_f64(cdp::LAST_SEASONAL), 10.0);
        assert_eq!(seasonal.word_f64(cdp::SEASONAL), 11.0);
        let deviation = &archives[2].cdp_prep[0];
        assert_eq!(deviation.word_f64(cdp::LAST_SEASONAL), 8.0);
        assert_eq!(deviation.word_f64(cdp::SEASONAL), 9.0);
        assert_eq!(rows.seasonal.values, vec![7.0, 8.0, 9.0, 10.0, 11.0]);
        // Words 8 and 9 of the season archives are unchanged.
        assert!(seasonal.primary_value.is_nan());
        let failures = &archives[4].cdp_prep[0];
        assert_eq!(
            crate::holt_winters::failure_history(failures, 9),
            vec![0; 9]
        );
        assert_eq!(
            (failures.primary_value, failures.secondary_value),
            (0.0, 0.0)
        );
        let dev_predict = &archives[3].cdp_prep[0];
        assert!(dev_predict.primary_value.is_nan() && dev_predict.secondary_value.is_nan());
        // Five rows from row 2 pass row 0: the counter goes from 1 to 2.
        assert!(triggered);
        assert_eq!(seasonal.word_u64(cdp::INIT_FLAG), 2);
    }

    #[test]
    fn run_crossing_no_smoothing_point_does_not_trigger() {
        let (mut archives, mut rows) = period_five_group();
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut triggered = false;
        // Rows 2 and 3 only.
        consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &run_of(2, 1.0),
            &mut triggered,
        );
        assert!(!triggered);
        // From row 1, three rows reach 2, 3 and 4; the counter at 4 waits
        // for the smoothing index 2.
        for index in [1, 2] {
            archives[index].current_row = 1;
            archives[index].cdp_prep[0].set_u64(cdp::INIT_FLAG, 4);
        }
        consolidate_group(
            &group,
            &mut archives,
            &mut rows,
            &run_of(3, 1.0),
            &mut triggered,
        );
        assert!(triggered);
        assert_eq!(archives[1].cdp_prep[0].word_u64(cdp::INIT_FLAG), 4);
    }

    /// The centred circular mean of `rows` with half-width `h`, summed
    /// directly.
    fn centred(rows: &[f64], h: usize) -> Vec<f64> {
        let p = rows.len();
        (0..p)
            .map(|i| {
                (0..=2 * h)
                    .map(|k| rows[(i + p * h + k - h) % p])
                    .sum::<f64>()
                    / (2 * h + 1) as f64
            })
            .collect()
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(x, y)| (x - y).abs() <= 1e-12 * y.abs().max(1.0))
    }

    #[test]
    fn half_width_stays_below_the_period() {
        // P = 4: f = 1.99 gives h = 3; f = 2 would give h = P and is left
        // unsmoothed, as are NaN and negative fractions.
        let mut archive = additive_group().remove(1);
        let half = |h| Some(Window::Half(h));
        for (fraction, expected) in [(0.5, half(1)), (1.99, half(3)), (2.0, None), (-1.0, None)] {
            archive.param_words[param::SMOOTHING_WINDOW] = f(fraction);
            assert_eq!(window(&archive, "0004"), expected, "{fraction}");
        }
        // f·P / 2 NaN or at most −2^63 rotates on x86_64 only (HW-18a);
        // +inf and a fraction just above −2^62 (P = 4) stay unsmoothed.
        let rotate = cfg!(target_arch = "x86_64").then_some(Window::Rotate);
        for (fraction, expected) in [
            (f64::NAN, rotate),
            (-f64::NAN, rotate),
            (f64::NEG_INFINITY, rotate),
            (-(2f64.powi(62)), rotate),
            (-1e308, rotate),
            (-(2f64.powi(62)) + 1024.0, None),
            (f64::INFINITY, None),
            (1e308, None),
        ] {
            archive.param_words[param::SMOOTHING_WINDOW] = f(fraction);
            assert_eq!(window(&archive, "0004"), expected, "{fraction}");
        }
    }

    #[test]
    fn rotation_follows_hw_18a() {
        // 2^63 mod P: 0 for P = 8, 1 for P = 7, 3 for P = 5, 8 for P = 12.
        // Row k becomes 0 and row i takes row i − k, as RRDtool on x86_64
        // gives for vector 205 and the P = 5, 7 and 12 probes.
        let rows = [11.0, 0.0, 10.0, 30.0, 10.0, 1.0, 11.0, 31.0];
        assert_eq!(
            rotated(&rows, 1, 8),
            [0.0, 0.0, 10.0, 30.0, 10.0, 1.0, 11.0, 31.0]
        );
        let rows = [27.0, 0.0, -22.0, 9.0, 42.0, -35.0, -32.0];
        assert_eq!(
            rotated(&rows, 1, 7),
            [-32.0, 0.0, 0.0, -22.0, 9.0, 42.0, -35.0]
        );
        let rows = [-8.0, -22.0, 9.0, 42.0, -35.0];
        assert_eq!(rotated(&rows, 1, 5), [9.0, 42.0, -35.0, 0.0, -22.0]);
        let rows: Vec<f64> = (0..12).map(f64::from).collect();
        let expected = [4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 0.0, 1.0, 2.0, 3.0];
        assert_eq!(rotated(&rows, 1, 12), expected);
        // Rows move with all their sources.
        let rows = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(
            rotated(&rows, 2, 5),
            [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 0.0, 0.0, 3.0, 4.0]
        );
    }

    #[test]
    fn smoothing_pass_follows_hw_18() {
        // P = 4, window 0.5 at version 0004: h = 1.
        let mut archives = additive_group();
        for index in [1, 2] {
            archives[index].param_words[param::SMOOTHING_WINDOW] = f(0.5);
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        let seasonal = [1.0, 2.0, 4.0, 9.0];
        let deviation = [3.0, 1.0, 2.0, 6.0];
        let mut rows = seasons(&seasonal, &deviation);
        let mut out = Vec::new();
        smooth(&group, &mut archives, &mut rows, "0004", &mut out);

        let smoothed = centred(&seasonal, 1);
        let mean = smoothed.iter().sum::<f64>() / 4.0;
        let expected: Vec<f64> = smoothed.iter().map(|v| v - mean).collect();
        assert!(
            close(&rows.seasonal.values, &expected),
            "{:?}",
            rows.seasonal.values
        );
        assert!(close(&rows.deviation.values, &centred(&deviation, 1)));
        let intercept = archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT);
        assert!(close(&[intercept], &[10.0 + mean]));
        // Every row of both archives is queued once, in row order; the
        // prefetched coefficients stay.
        let queued: Vec<(usize, u64)> = out.iter().map(|r| (r.archive, r.first_row)).collect();
        assert_eq!(
            queued,
            vec![
                (1, 0),
                (1, 1),
                (1, 2),
                (1, 3),
                (2, 0),
                (2, 1),
                (2, 2),
                (2, 3)
            ]
        );
        assert!(out.iter().all(|r| r.count == 1));
        assert_eq!(archives[1].cdp_prep[0].word_f64(cdp::SEASONAL), 2.0);

        // MHWPREDICT divides by the mean and still adds it to the intercept.
        let mut archives = additive_group();
        archives[0].consolidation = "MHWPREDICT".into();
        archives[0].raw_consolidation = b"MHWPREDICT".to_vec();
        for index in [1, 2] {
            archives[index].param_words[param::SMOOTHING_WINDOW] = f(0.5);
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        let mut rows = seasons(&seasonal, &deviation);
        smooth(&group, &mut archives, &mut rows, "0004", &mut Vec::new());
        let expected: Vec<f64> = smoothed.iter().map(|v| v / mean).collect();
        assert!(close(&rows.seasonal.values, &expected));
        let intercept = archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT);
        assert!(close(&[intercept], &[10.0 + mean]));

        // Version 0003 uses 0.05: h = trunc(0.1) = 0, nothing changes.
        let mut archives = additive_group();
        for index in [1, 2] {
            archives[index].param_words[param::SMOOTHING_WINDOW] = f(0.5);
        }
        let mut rows = seasons(&seasonal, &deviation);
        let mut out = Vec::new();
        smooth(&group, &mut archives, &mut rows, "0003", &mut out);
        assert!(out.is_empty());
        assert_eq!(rows.seasonal.values, seasonal.to_vec());
    }

    #[test]
    fn one_unknown_ds_blocks_smoothing_for_all() {
        let mut archives = additive_group();
        for archive in &mut archives {
            let second = archive.cdp_prep[0].clone();
            archive.cdp_prep.push(second);
        }
        for index in [1, 2] {
            archives[index].param_words[param::SMOOTHING_WINDOW] = f(0.5);
        }
        let group = Group::find(&kinds(&archives)).unwrap();
        // Row-major, two DSs: DS 1 of SEASONAL row 2 is unknown.
        let seasonal = [1.0, 5.0, 2.0, 6.0, 4.0, rrd_nan(), 9.0, 8.0];
        let deviation = [3.0, 1.0, 1.0, 2.0, 2.0, 3.0, 6.0, 4.0];
        let mut rows = seasons(&seasonal, &deviation);
        rows.seasonal.sources = 2;
        rows.deviation.sources = 2;
        let mut out = Vec::new();
        smooth(&group, &mut archives, &mut rows, "0004", &mut out);
        // SEASONAL is left alone for both DSs, and so is the intercept.
        let bits = |values: &[f64]| values.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&rows.seasonal.values), bits(&seasonal));
        assert_eq!(archives[0].cdp_prep[0].word_f64(cdp::INTERCEPT), 10.0);
        assert_eq!(archives[0].cdp_prep[1].word_f64(cdp::INTERCEPT), 10.0);
        // DEVSEASONAL is judged on its own rows and smoothed for both.
        assert!(out.iter().all(|range| range.archive == 2));
        assert_eq!(out.len(), 4);
        let ds0: Vec<f64> = rows.deviation.values.iter().step_by(2).copied().collect();
        let ds1: Vec<f64> = rows
            .deviation
            .values
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect();
        assert!(close(&ds0, &centred(&[3.0, 1.0, 2.0, 6.0], 1)));
        assert!(close(&ds1, &centred(&[1.0, 2.0, 3.0, 4.0], 1)));
    }

    #[test]
    fn dev_predict_and_failures_follow_their_links() {
        let temp = tempfile::tempdir().unwrap();
        // DEVPREDICT names SEASONAL (archive 2), FAILURES the AVERAGE
        // archive (0).
        let path = patched(
            temp.path(),
            "links",
            &[
                (param_offset(4, param::DEPENDENT), 2),
                (param_offset(5, param::DEPENDENT), 0),
            ],
        );
        let arguments: Vec<String> = (0..14)
            .map(|i| {
                format!(
                    "{}:{}",
                    1_000_000_010 + 10 * i,
                    [10, 20, 40, 20][i % 4] + i / 4
                )
            })
            .collect();
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let (rows, result) = update_rrd_text(&path, None, &arguments, false);
        result.unwrap();
        let of = |name: &str| -> Vec<f64> {
            rows.iter()
                .filter(|row| row.consolidation == name)
                .map(|row| row.values[0])
                .collect()
        };
        // FAILURES stores each value; DEVPREDICT the coefficient SEASONAL
        // used, which is its row of one season (four PDPs) before.
        let values: Vec<f64> = arguments
            .iter()
            .map(|a| a.split(':').nth(1).unwrap().parse().unwrap())
            .collect();
        assert_eq!(of("FAILURES"), values);
        let (seasonal, dev_predict) = (of("SEASONAL"), of("DEVPREDICT"));
        assert!(dev_predict[..4].iter().all(|v| v.is_nan()));
        assert_eq!(dev_predict[4..], seasonal[..10]);
        let info = crate::inspect_rrd_file(&path).unwrap();
        let failures = &info.archives[5].cdp_prep[0];
        assert_eq!(
            crate::holt_winters::failure_history(failures, 9),
            vec![0; 9]
        );
        assert_eq!(failures.primary_value, 23.0);

        // FAILURES names DEVPREDICT, which names the prediction archive, so
        // the values are checked, against DEVPREDICT's word 3 (0) as the
        // deviation: every value off the prediction is a violation.
        let path = patched(
            temp.path(),
            "chain",
            &[
                (param_offset(4, param::DEPENDENT), 1),
                (param_offset(5, param::DEPENDENT), 4),
            ],
        );
        let (rows, result) = update_rrd_text(&path, None, &arguments, false);
        result.unwrap();
        let failures: Vec<f64> = rows
            .iter()
            .filter(|row| row.consolidation == "FAILURES")
            .map(|row| row.values[0])
            .collect();
        // From PDP 5 on each is a violation; the seventh (PDP 11) reaches
        // the threshold.
        let mut expected = vec![0.0; 10];
        expected.extend([1.0; 4]);
        assert_eq!(failures, expected);
        let info = crate::inspect_rrd_file(&path).unwrap();
        let history = crate::holt_winters::failure_history(&info.archives[5].cdp_prep[0], 9);
        // The ten PDPs after the first season all have a prediction.
        assert_eq!(history, vec![1; 9]);

        // A prediction link to another type is refused before any write.
        let path = patched(
            temp.path(),
            "pred",
            &[(param_offset(1, param::DEPENDENT), 3)],
        );
        let before = std::fs::read(&path).unwrap();
        let (rows, result) = update_rrd_text(&path, None, &arguments, false);
        assert!(
            matches!(result, Err(StoreError::RrdUnsupported(_))),
            "{result:?}"
        );
        assert!(rows.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn hw_archive_with_several_pdps_per_row_is_refused_untouched() {
        let temp = tempfile::tempdir().unwrap();
        // Each Holt-Winters archive of hwimp (1 to 5) in turn, with two PDPs
        // per row, as a restored, edited dump can give it.
        for archive in 1..6 {
            let path = patched(
                temp.path(),
                &format!("pdp2-{archive}"),
                &[(rows_offset(archive) + 8, 2)],
            );
            let before = std::fs::read(&path).unwrap();
            let (rows, result) =
                update_rrd_text(&path, None, &["1000000010:10", "1000000020:20"], false);
            match &result {
                Err(StoreError::RrdUnsupported(message)) => {
                    assert!(message.contains("PDP per row"), "{message}")
                }
                other => panic!("archive {archive}: {other:?}"),
            }
            assert!(rows.is_empty());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
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

        // SEASONAL claims 2^61 + 4 rows; with one DS the size check wraps
        // and passes (UP-38). Its row 3 is current and the file holds 24 of
        // its rows, so the 21st PDP writes row 24, outside the file.
        let path = patched(temp.path(), "up38", &[(rows_offset(2), 4 + (1 << 61))]);
        let before = std::fs::read(&path).unwrap();
        let arguments: Vec<String> = (1..=30)
            .map(|i| format!("{}:{}", 1_000_000_000 + 10 * i, (i * 7) % 11))
            .collect();
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let (rows, result) = update_rrd_text(&path, None, &arguments, false);
        let error = result.unwrap_err().to_string();
        assert!(error.starts_with("writing rrd: "), "{error}");
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len(), before.len());
        let info = crate::inspect_rrd_file(&path).unwrap();
        // The failing argument's time is not stored.
        assert_eq!(info.last_update, 1_000_000_200);
        let current: Vec<u64> = info.archives.iter().map(|a| a.current_row).collect();
        // Archives before SEASONAL advanced for the failing PDP, SEASONAL
        // advanced to the row outside the file, later archives did not.
        assert_eq!(current, vec![6, 0, 24, 2, 8, 1]);
        // 20 PDPs print six rows each; the failing one prints the AVERAGE,
        // HWPREDICT and the outside row's first value.
        assert_eq!(rows.len(), 20 * 6 + 3);
        assert_eq!(rows.last().unwrap().consolidation, "SEASONAL");
    }
}
