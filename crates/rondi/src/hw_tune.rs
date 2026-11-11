//! The Holt-Winters options of `rrdtool tune` (HW-30 to HW-35).
//!
//! Provenance: specification (docs/compatibility/holt-winters.md section 4).
//! Each option is checked and written in argument order, and the first
//! failure stops the rest with the earlier writes kept (TM-4). Both the CLI
//! and rrdcached's `TUNE` call [`tune_holt_winters_file`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::holt_winters::{MAX_WINDOW_LEN, Role, cdp, param};
use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RrdFileLock, RrdInfo, STAT_HEAD_LEN,
    VALUE_LEN, open_rrd_write, read_info, rrd_nan,
};
use crate::storage::StoreError;

/// One Holt-Winters `tune` option. Values are the option's raw text, read
/// by HW-31 to HW-33 when the option runs. `AberrantReset` names its data
/// source by index: the caller resolves the name, which earlier `--data-
/// source-rename` options may have changed, and reports HW-35's name errors.
#[derive(Debug, Clone, PartialEq)]
pub enum HwTuneOption {
    Alpha(String),
    Beta(String),
    Gamma(String),
    GammaDeviation(String),
    SmoothingWindow(String),
    SmoothingWindowDeviation(String),
    DeltaPos(String),
    DeltaNeg(String),
    WindowLength(String),
    FailureThreshold(String),
    AberrantReset(usize),
}

/// The archive an option tunes and the parameter slot it writes (HW-30).
#[derive(Clone, Copy)]
enum Target {
    Prediction,
    Seasonal,
    DevSeasonal,
    Failures,
}

impl Target {
    fn matches(self, role: Role) -> bool {
        matches!(
            (self, role),
            (Self::Prediction, Role::Prediction { .. })
                | (Self::Seasonal, Role::Seasonal)
                | (Self::DevSeasonal, Role::DevSeasonal)
                | (Self::Failures, Role::Failures)
        )
    }

    fn missing(self) -> &'static str {
        match self {
            Self::Failures => "Failures RRA does not exist in this RRD",
            _ => "Holt-Winters RRA does not exist in this RRD",
        }
    }
}

fn failed(message: &str) -> StoreError {
    StoreError::Rrd(message.to_owned())
}

/// HW-31: the longest leading number, with `nan` and `inf` spellings.
/// `closed` admits 0 and 1 (the smoothing windows). NaN passes the check.
fn unit_value(text: &str, closed: bool) -> Result<f64, StoreError> {
    let value = crate::rrd_number::leading_number(text)
        .ok_or_else(|| failed("Unable to parse Holt-Winters parameter"))?;
    let outside = if closed {
        value < 0.0 || value > 1.0
    } else {
        value <= 0.0 || value >= 1.0
    };
    if outside {
        return Err(failed("Holt-Winters parameter must be between 0 and 1"));
    }
    Ok(value)
}

/// HW-32: a deviation multiplier.
fn delta_value(text: &str) -> Result<f64, StoreError> {
    // Compatibility: text with no number at all stores 0.0, while a number
    // followed by other text fails after the lower bound (HW-32, defect).
    // Provenance: black-box compatibility test.
    let Some(value) = crate::rrd_number::leading_number(text) else {
        return Ok(0.0);
    };
    if value < 0.1 {
        return Err(failed("Parameter specified is too small"));
    }
    if crate::rrd_number::parse_rrd_number(text).is_none() {
        return Err(failed("Unable to parse parameter in set_deltaarg"));
    }
    Ok(value)
}

/// HW-33: the leading integer, as C `atoi` reads it, in 1 to 28.
fn window_value(text: &str) -> Result<u64, StoreError> {
    let value = crate::rrd_number::c_strtol(text, 10).0 as i32;
    if !(1..=MAX_WINDOW_LEN as i32).contains(&value) {
        return Err(failed("Parameter must be between 1 and 28"));
    }
    Ok(value as u64)
}

/// The file being tuned: its layout, and writes of single words.
struct Tuned<'a> {
    file: &'a mut File,
    info: RrdInfo,
}

impl Tuned<'_> {
    fn sources(&self) -> usize {
        self.info.data_sources.len()
    }

    fn role(&self, archive: usize) -> Option<Role> {
        Role::from_cf(&self.info.archives[archive].raw_consolidation)
    }

    /// The first archive `target` tunes (HW-30; later ones are left alone).
    fn first(&self, target: Target) -> Result<usize, StoreError> {
        (0..self.info.archives.len())
            .find(|&index| self.role(index).is_some_and(|role| target.matches(role)))
            .ok_or_else(|| failed(target.missing()))
    }

    fn param_offset(&self, archive: usize, slot: usize) -> u64 {
        let record = STAT_HEAD_LEN + self.sources() * DS_DEF_LEN + archive * RRA_DEF_LEN;
        // The ten parameter slots follow the CF name, padding, rows and
        // PDPs per row (CR-40).
        (record + 40 + VALUE_LEN * slot) as u64
    }

    fn cdp_offset(&self, archive: usize, source: usize) -> u64 {
        let start = STAT_HEAD_LEN
            + self.sources() * (DS_DEF_LEN + PDP_PREP_LEN)
            + self.info.archives.len() * RRA_DEF_LEN
            + self.info.live_head_len;
        (start + (archive * self.sources() + source) * CDP_PREP_LEN) as u64
    }

    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StoreError> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(bytes)?;
        Ok(())
    }

    fn set_param(&mut self, archive: usize, slot: usize, word: u64) -> Result<(), StoreError> {
        self.info.archives[archive].param_words[slot] = word;
        self.write(self.param_offset(archive, slot), &word.to_le_bytes())
    }

    fn set_cdp(&mut self, archive: usize, source: usize, slot: usize) -> Result<(), StoreError> {
        let offset = self.cdp_offset(archive, source) + (slot * VALUE_LEN) as u64;
        self.write(offset, &rrd_nan().to_le_bytes())
    }

    /// Zeroes the first `length` history bytes of `source` (HW-14).
    fn clear_history(
        &mut self,
        archive: usize,
        source: usize,
        length: u64,
    ) -> Result<(), StoreError> {
        let length = length.min(MAX_WINDOW_LEN) as usize;
        self.write(self.cdp_offset(archive, source), &vec![0; length])
    }

    fn clear_all_history(&mut self, archive: usize, length: u64) -> Result<(), StoreError> {
        for source in 0..self.sources() {
            self.clear_history(archive, source, length)?;
        }
        Ok(())
    }

    /// HW-34: `-s` and `-S` raise a version below 4 to `0004`.
    fn raise_version(&mut self) -> Result<(), StoreError> {
        let level = crate::rrd_number::c_strtol(&self.info.version, 10).0;
        if level < 4 {
            self.write(4, b"0004\0")?;
            self.info.version = "0004".into();
        }
        Ok(())
    }

    /// Sets every row of `source` in `archive` to unknown.
    fn clear_rows(&mut self, archive: usize, source: usize) -> Result<(), StoreError> {
        let sources = self.sources();
        // The header's row count is untrusted: bound the archive by the
        // file's length, with checked arithmetic, before sizing the read.
        let file_len = self.file.metadata()?.len();
        let info = &self.info.archives[archive];
        crate::rrd_binary::require_rows_in_file(&self.info, info, file_len)?;
        let start = info.data_offset;
        let length = usize::try_from(info.rows * (sources * VALUE_LEN) as u64)
            .map_err(|_| StoreError::RrdFormat("archive larger than memory".into()))?;
        let mut block = vec![0_u8; length];
        self.file.seek(SeekFrom::Start(start))?;
        self.file.read_exact(&mut block)?;
        for row in block.chunks_exact_mut(sources * VALUE_LEN) {
            row[source * VALUE_LEN..(source + 1) * VALUE_LEN]
                .copy_from_slice(&rrd_nan().to_le_bytes());
        }
        self.write(start, &block)
    }

    /// HW-35: forgets what every Holt-Winters archive learned for `source`.
    fn aberrant_reset(&mut self, source: usize) -> Result<(), StoreError> {
        for archive in 0..self.info.archives.len() {
            match self.role(archive) {
                Some(Role::Prediction { .. }) => {
                    for slot in [
                        cdp::INTERCEPT,
                        cdp::LAST_INTERCEPT,
                        cdp::SLOPE,
                        cdp::LAST_SLOPE,
                    ] {
                        self.set_cdp(archive, source, slot)?;
                    }
                }
                Some(Role::Seasonal | Role::DevSeasonal) => {
                    self.set_cdp(archive, source, cdp::SEASONAL)?;
                    self.set_cdp(archive, source, cdp::LAST_SEASONAL)?;
                    self.clear_rows(archive, source)?;
                }
                Some(Role::Failures) => {
                    let window = self.info.archives[archive].param_u64(param::WINDOW_LEN);
                    self.clear_history(archive, source, window)?;
                }
                Some(Role::DevPredict) | None => {}
            }
        }
        Ok(())
    }

    fn apply(&mut self, option: &HwTuneOption) -> Result<(), StoreError> {
        use HwTuneOption as O;
        let (target, slot, word) = match option {
            O::Alpha(text) => (Target::Prediction, param::ALPHA, unit_value(text, false)?),
            O::Beta(text) => (Target::Prediction, param::BETA, unit_value(text, false)?),
            O::Gamma(text) => (Target::Seasonal, param::GAMMA, unit_value(text, false)?),
            O::GammaDeviation(text) => {
                (Target::DevSeasonal, param::GAMMA, unit_value(text, false)?)
            }
            O::SmoothingWindow(text) | O::SmoothingWindowDeviation(text) => {
                let value = unit_value(text, true)?;
                self.raise_version()?;
                let target = if matches!(option, O::SmoothingWindow(_)) {
                    Target::Seasonal
                } else {
                    Target::DevSeasonal
                };
                (target, param::SMOOTHING_WINDOW, value)
            }
            O::DeltaPos(text) => (Target::Failures, param::DELTA_POS, delta_value(text)?),
            O::DeltaNeg(text) => (Target::Failures, param::DELTA_NEG, delta_value(text)?),
            O::WindowLength(text) | O::FailureThreshold(text) => {
                let value = window_value(text)?;
                let archive = self.first(Target::Failures)?;
                let (slot, cleared) = if matches!(option, O::WindowLength(_)) {
                    (param::WINDOW_LEN, value)
                } else {
                    let window = self.info.archives[archive].param_u64(param::WINDOW_LEN);
                    (param::THRESHOLD, window)
                };
                self.set_param(archive, slot, value)?;
                // HW-30: the history of the tuned archive is cleared, even
                // when the value is unchanged.
                return self.clear_all_history(archive, cleared);
            }
            O::AberrantReset(source) => {
                if *source >= self.sources() {
                    return Err(failed("aberrant-reset data source index out of range"));
                }
                return self.aberrant_reset(*source);
            }
        };
        let archive = self.first(target)?;
        self.set_param(archive, slot, word.to_bits())
    }
}

/// Applies `options` to the RRD at `path`, in order.
pub fn tune_holt_winters(path: &Path, options: &[HwTuneOption]) -> Result<(), StoreError> {
    tune_holt_winters_file(open_rrd_write(path)?, path, options)
}

/// [`tune_holt_winters`] on a file the caller opened read-write. `path`
/// names it in errors.
pub fn tune_holt_winters_file(
    file: File,
    path: &Path,
    options: &[HwTuneOption],
) -> Result<(), StoreError> {
    let mut file = RrdFileLock::exclusive(file)?;
    file.seek(SeekFrom::Start(0))?;
    let info = read_info(&mut file, path)?;
    let mut tuned = Tuned {
        file: &mut file,
        info,
    };
    for option in options {
        tuned.apply(option)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use HwTuneOption as O;

    fn create(dir: &Path, name: &str, words: &[&str]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut all = vec![
            "DS:x:GAUGE:20:U:U".to_owned(),
            "DS:y:GAUGE:20:U:U".to_owned(),
        ];
        all.extend(words.iter().map(|word| (*word).to_owned()));
        let options = crate::CreateOptions {
            step: Some(10),
            start: Some(1_000_000_000),
            ..Default::default()
        };
        crate::create_rrd(path.to_str().unwrap(), &options, &all).unwrap();
        path
    }

    fn learned(dir: &Path, name: &str, words: &[&str]) -> std::path::PathBuf {
        let path = create(dir, name, words);
        let updates: Vec<String> = (1..=30)
            .map(|i| format!("{}:{}:{}", 1_000_000_000 + 10 * i, i % 5 * 10 + i, 3 * i))
            .collect();
        let updates: Vec<&str> = updates.iter().map(String::as_str).collect();
        crate::update_rrd_text(&path, None, &updates, false)
            .1
            .unwrap();
        path
    }

    fn info(path: &Path) -> RrdInfo {
        crate::inspect_rrd_file(path).unwrap()
    }

    fn tune(path: &Path, options: &[HwTuneOption]) -> Result<(), String> {
        tune_holt_winters(path, options).map_err(|error| error.to_string())
    }

    const GROUP: &str = "RRA:HWPREDICT:20:0.1:0.01:5";

    /// Review Focus 3 (TM-4): options run in order and stop at the first
    /// failure, keeping the earlier writes.
    #[test]
    fn hw_options_stop_at_first_failure() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &[GROUP]);
        let result = tune(
            &path,
            &[
                O::Alpha("0.5".into()),
                O::Beta("2".into()),
                O::Gamma("0.3".into()),
            ],
        );
        assert_eq!(
            result,
            Err("Holt-Winters parameter must be between 0 and 1".into())
        );
        let info = info(&path);
        assert_eq!(info.archives[0].param_f64(param::ALPHA), 0.5);
        assert_eq!(info.archives[0].param_f64(param::BETA), 0.01);
        assert_eq!(info.archives[1].param_f64(param::GAMMA), 0.1);
    }

    /// HW-30: each option writes the first archive of its type only.
    #[test]
    fn options_write_the_first_archive_of_their_type() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(
            temp.path(),
            "a.rrd",
            &[GROUP, "RRA:MHWPREDICT:20:0.2:0.02:5"],
        );
        tune(
            &path,
            &[
                O::Alpha("0.5x".into()),
                O::Beta(" 0.25".into()),
                O::Gamma("0.3".into()),
                O::GammaDeviation("0.4".into()),
                O::SmoothingWindow("1".into()),
                O::SmoothingWindowDeviation("0".into()),
                O::DeltaPos("3".into()),
                O::DeltaNeg("4.5".into()),
                O::WindowLength("12".into()),
                O::FailureThreshold("3.7".into()),
            ],
        )
        .unwrap();
        let info = info(&path);
        let words = |i: usize| info.archives[i].param_words;
        let f = |v: f64| v.to_bits();
        assert_eq!(words(0)[1..3], [f(0.5), f(0.25)]);
        assert_eq!(words(1)[1..3], [f(0.3), f(1.0)]);
        assert_eq!(words(2)[1..3], [f(0.4), 0]);
        assert_eq!(words(4)[1..6], [f(3.0), f(4.5), 2, 12, 3]);
        // The second group keeps its values.
        assert_eq!(words(5)[1..3], [f(0.2), f(0.02)]);
        assert_eq!(words(6)[1..3], [f(0.2), 0]);
        assert_eq!(words(9)[1..6], [f(2.0), f(2.0), 7, 9, 7]);
        assert_eq!(info.version, "0004");
    }

    /// Review Focus 4 (HW-34): `-s` and `-S` raise the version before the
    /// archive lookup, so it stays raised when they fail; a later version
    /// is kept.
    #[test]
    fn smoothing_window_option_raises_version_on_failure() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &["RRA:AVERAGE:0.5:1:5"]);
        for option in [
            O::SmoothingWindow("0.5".into()),
            O::SmoothingWindowDeviation("0.5".into()),
        ] {
            assert_eq!(
                tune(&path, &[option]),
                Err("Holt-Winters RRA does not exist in this RRD".into())
            );
        }
        assert_eq!(info(&path).version, "0004");
        // A value check failure comes first and leaves the version alone.
        let other = create(temp.path(), "b.rrd", &["RRA:AVERAGE:0.5:1:5"]);
        assert_eq!(
            tune(&other, &[O::SmoothingWindow("1.5".into())]),
            Err("Holt-Winters parameter must be between 0 and 1".into())
        );
        assert_eq!(info(&other).version, "0003");
        let path = temp.path().join("v5.rrd");
        let options = crate::CreateOptions {
            step: Some(10),
            start: Some(1_000_000_000),
            ..Default::default()
        };
        crate::create_rrd(
            path.to_str().unwrap(),
            &options,
            &["DS:d:DCOUNTER:20:U:U".into(), GROUP.into()],
        )
        .unwrap();
        tune(&path, &[O::SmoothingWindow("0.5".into())]).unwrap();
        assert_eq!(info(&path).version, "0005");
        for (option, message) in [
            (
                O::Alpha("0.5".into()),
                "Holt-Winters RRA does not exist in this RRD",
            ),
            (
                O::DeltaPos("3".into()),
                "Failures RRA does not exist in this RRD",
            ),
            (
                O::WindowLength("5".into()),
                "Failures RRA does not exist in this RRD",
            ),
        ] {
            assert_eq!(tune(&other, &[option]), Err(message.into()));
        }
    }

    /// HW-31: `nan` passes and is stored with the sign bit flipped from the
    /// platform's NaN; no number at all is a parse error.
    #[test]
    fn nan_alpha_stores_opposite_sign_nan() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &[GROUP]);
        tune(&path, &[O::Alpha("nan".into())]).unwrap();
        let word = info(&path).archives[0].param_words[param::ALPHA];
        assert_eq!(word, (-rrd_nan()).to_bits());
        #[cfg(target_arch = "x86_64")]
        assert_eq!(word, 0x7ff8_0000_0000_0000);
        #[cfg(not(target_arch = "x86_64"))]
        assert_eq!(word, 0xfff8_0000_0000_0000);
        for (text, message) in [
            ("x", "Unable to parse Holt-Winters parameter"),
            ("", "Unable to parse Holt-Winters parameter"),
            ("inf", "Holt-Winters parameter must be between 0 and 1"),
            ("0", "Holt-Winters parameter must be between 0 and 1"),
            ("1", "Holt-Winters parameter must be between 0 and 1"),
        ] {
            assert_eq!(tune(&path, &[O::Alpha(text.into())]), Err(message.into()));
        }
    }

    /// HW-32 and HW-33 value forms.
    #[test]
    fn delta_without_number_stores_zero() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &[GROUP]);
        tune(&path, &[O::DeltaPos("x".into())]).unwrap();
        assert_eq!(info(&path).archives[4].param_words[param::DELTA_POS], 0);
        for (option, message) in [
            (
                O::DeltaNeg("0.05x".into()),
                "Parameter specified is too small",
            ),
            (O::DeltaNeg("-1".into()), "Parameter specified is too small"),
            (
                O::DeltaNeg("1x".into()),
                "Unable to parse parameter in set_deltaarg",
            ),
            (
                O::WindowLength("29".into()),
                "Parameter must be between 1 and 28",
            ),
            (
                O::FailureThreshold("x".into()),
                "Parameter must be between 1 and 28",
            ),
            (
                O::FailureThreshold("-3".into()),
                "Parameter must be between 1 and 28",
            ),
        ] {
            assert_eq!(tune(&path, &[option]), Err(message.into()));
        }
        // The threshold may exceed the window (HW-33).
        tune(&path, &[O::FailureThreshold("20".into())]).unwrap();
        assert_eq!(info(&path).archives[4].param_words[param::THRESHOLD], 20);
    }

    fn history(path: &Path, archive: usize, source: usize) -> Vec<u8> {
        let info = info(path);
        crate::holt_winters::failure_history(&info.archives[archive].cdp_prep[source], 28)
    }

    fn set_history(path: &Path, archive: usize, source: usize, bytes: &[u8]) {
        let mut file = open_rrd_write(path).unwrap();
        let info = read_info(&mut file, path).unwrap();
        let tuned = Tuned {
            file: &mut file,
            info,
        };
        let offset = tuned.cdp_offset(archive, source);
        let mut data = std::fs::read(path).unwrap();
        data[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        std::fs::write(path, data).unwrap();
    }

    /// HW-30: `-w` clears the new window's bytes of the tuned archive and
    /// `-f` the current window's, for every DS.
    #[test]
    fn window_options_clear_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &[GROUP, GROUP]);
        for archive in [4, 9] {
            for source in [0, 1] {
                set_history(&path, archive, source, &[1; 28]);
            }
        }
        tune(&path, &[O::WindowLength("5".into())]).unwrap();
        let mut want = vec![0; 5];
        want.extend([1; 23]);
        assert_eq!(history(&path, 4, 0), want);
        assert_eq!(history(&path, 4, 1), want);
        assert_eq!(history(&path, 9, 0), vec![1; 28]);
        tune(&path, &[O::FailureThreshold("2".into())]).unwrap();
        assert_eq!(history(&path, 4, 1), want);
        tune(
            &path,
            &[
                O::WindowLength("20".into()),
                O::FailureThreshold("2".into()),
            ],
        )
        .unwrap();
        let mut want = vec![0; 20];
        want.extend([1; 8]);
        assert_eq!(history(&path, 4, 0), want);
    }

    /// Every archive's stored rows, in storage order, one value per DS.
    fn stored_rows(path: &Path) -> Vec<Vec<Vec<f64>>> {
        let info = info(path);
        let bytes = std::fs::read(path).unwrap();
        let sources = info.data_sources.len();
        info.archives
            .iter()
            .map(|archive| {
                (0..archive.rows as usize)
                    .map(|row| {
                        (0..sources)
                            .map(|source| {
                                let at = archive.data_offset as usize
                                    + (row * sources + source) * VALUE_LEN;
                                f64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
                            })
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    /// A row count whose byte size wraps past `u64` is refused before the
    /// rows are read, not used to size the read.
    #[test]
    fn aberrant_reset_refuses_rows_past_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = create(temp.path(), "a.rrd", &[GROUP]);
        let mut bytes = std::fs::read(&path).unwrap();
        // SEASONAL is archive 1; its row count is at offset 24 of its
        // definition. Adding 2^61 rows leaves rows * 2 DS * 8 bytes unchanged
        // modulo 2^64.
        let at = 128 + 2 * 120 + 120 + 24;
        let rows = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        bytes[at..at + 8].copy_from_slice(&(rows + (1 << 61)).to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert!(tune(&path, &[O::AberrantReset(0)]).is_err());
    }

    /// Review Focus 5 (HW-35): a reset of one DS changes only that DS's
    /// learned state, in every Holt-Winters archive.
    #[test]
    fn aberrant_reset_touches_one_ds() {
        let temp = tempfile::tempdir().unwrap();
        // Rondi's update models one group per file.
        let path = learned(temp.path(), "a.rrd", &["RRA:AVERAGE:0.5:1:5", GROUP]);
        set_history(&path, 5, 0, &[1; 28]);
        let before = info(&path);
        let rows_before = stored_rows(&path);
        tune(&path, &[O::AberrantReset(0)]).unwrap();
        let after = info(&path);
        let rows_after = stored_rows(&path);
        let nan = rrd_nan().to_bits();
        for (index, (was, now)) in before.archives.iter().zip(&after.archives).enumerate() {
            assert_eq!(was.param_words, now.param_words, "archive {index}");
            assert_eq!(
                was.cdp_prep[1].words, now.cdp_prep[1].words,
                "archive {index}: DS y changed"
            );
            let (was0, now0) = (was.cdp_prep[0].words, now.cdp_prep[0].words);
            let reset: &[usize] = match Role::from_cf(&now.raw_consolidation) {
                Some(Role::Prediction { .. }) => &[2, 3, 4, 5],
                Some(Role::Seasonal | Role::DevSeasonal) => &[2, 3],
                _ => &[],
            };
            for slot in 0..10 {
                if reset.contains(&slot) {
                    assert_eq!(now0[slot], nan, "archive {index} slot {slot}");
                } else if index != 5 || slot >= 4 {
                    assert_eq!(now0[slot], was0[slot], "archive {index} slot {slot}");
                }
            }
            let season = matches!(
                Role::from_cf(&now.raw_consolidation),
                Some(Role::Seasonal | Role::DevSeasonal)
            );
            for (row_was, row_now) in rows_before[index].iter().zip(&rows_after[index]) {
                assert_eq!(row_was[1].to_bits(), row_now[1].to_bits());
                if season {
                    assert_eq!(row_now[0].to_bits(), nan);
                } else {
                    assert_eq!(row_was[0].to_bits(), row_now[0].to_bits());
                }
            }
        }
        let mut want = vec![0; 9];
        want.extend([1; 19]);
        assert_eq!(history(&path, 5, 0), want);
        // Every group is reset, not only the first.
        let two = create(temp.path(), "two.rrd", &[GROUP, GROUP]);
        for archive in [4, 9] {
            set_history(&two, archive, 1, &[1; 28]);
        }
        tune(&two, &[O::AberrantReset(1)]).unwrap();
        assert_eq!(history(&two, 4, 1), want);
        assert_eq!(history(&two, 9, 1), want);
        // A file without Holt-Winters archives is left as it was.
        let plain = create(temp.path(), "p.rrd", &["RRA:AVERAGE:0.5:1:5"]);
        let bytes = std::fs::read(&plain).unwrap();
        tune(&plain, &[O::AberrantReset(1)]).unwrap();
        assert_eq!(std::fs::read(&plain).unwrap(), bytes);
    }
}
