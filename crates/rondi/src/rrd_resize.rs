//! `rrdtool resize`: adding unknown rows to, or removing the oldest rows
//! from, one archive of an RRD, writing the result to a new file.
//!
//! The behavior is specified in docs/compatibility/resize.md, whose rule
//! numbers (RZ-n) are cited below. The input's layout is read into a
//! [`Layout`]; [`Plan::new`] turns it and the request into the output header
//! and a short list of row operations, without touching any file; a
//! [`Transfer`] then carries the operations out between the two files, so
//! the archives never have to fit in memory.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::rrd_binary::{
    DS_DEF_LEN, RRA_DEF_LEN, RRA_PTR_LEN, RrdResizeAction, STAT_HEAD_LEN, VALUE_LEN, read_info,
    rrd_nan, rrd_strerror,
};
use crate::storage::StoreError;

const ROWS_OFFSET: usize = 24;
/// Microseconds that a version 3 live header adds after the last update.
const LIVE_USEC_LEN: usize = 8;

fn rrd_error(message: impl Into<String>) -> StoreError {
    StoreError::Rrd(message.into())
}

/// What the input's header says about where everything is.
#[derive(Debug, Clone)]
struct Layout {
    version: String,
    /// Length of everything before the first row.
    head_len: u64,
    /// Offset of the archive current-row words.
    pointers_offset: u64,
    /// Offset of the last update.
    live_offset: u64,
    /// Whether the live header lacks the microseconds (before version 3).
    legacy_live_head: bool,
    sources: u64,
    /// Rows and current row of each archive.
    archives: Vec<(u64, u64)>,
}

impl Layout {
    fn row_len(&self) -> u64 {
        self.sources * VALUE_LEN as u64
    }

    /// Length of all rows, or `None` when it overflows.
    fn data_len(&self) -> Option<u64> {
        self.archives.iter().try_fold(0_u64, |total, (rows, _)| {
            total.checked_add(rows.checked_mul(self.row_len())?)
        })
    }
}

/// One step of the row copy. Counts are in values, not bytes, and are wide
/// enough for any current row a file can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Copy values from the input to the output.
    Copy(u128),
    /// Move the input position forward over values.
    Skip(u128),
    /// Write unknown values.
    Unknown(u128),
}

/// The complete description of the output.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    head: Vec<u8>,
    /// Length the output is created with.
    output_len: u64,
    steps: Vec<Step>,
}

impl Plan {
    /// Builds the output header from the input's `head` bytes and the steps
    /// that move the rows (RZ-5 to RZ-11, RZ-17).
    fn new(
        layout: &Layout,
        input_head: &[u8],
        archive: usize,
        action: RrdResizeAction,
        rows: u64,
    ) -> Result<Self, StoreError> {
        let too_large = || rrd_error("the resized file would be too large");
        let (old_rows, current) = layout.archives[archive];
        let new_rows = match action {
            RrdResizeAction::Grow => old_rows.checked_add(rows).ok_or_else(too_large)?,
            RrdResizeAction::Shrink => old_rows - rows,
        };
        let row_len = layout.row_len();
        let resized_data = layout
            .data_len()
            .and_then(|length| length.checked_sub(old_rows * row_len))
            .and_then(|length| length.checked_add(new_rows.checked_mul(row_len)?))
            .ok_or_else(too_large)?;
        // Compatibility: the output is sized from the input's own layout,
        // so a version 0001 file (8-byte live header) gets a file 8 bytes
        // too short for the version 0003 header written into it (RZ-17).
        // Provenance: black-box compatibility test.
        let output_len = layout
            .head_len
            .checked_add(resized_data)
            .filter(|length| *length <= i64::MAX as u64)
            .ok_or_else(too_large)?;

        let sources = u128::from(layout.sources);
        let n = i128::from(old_rows);
        let c = i128::from(current);
        let k = i128::from(rows);
        let values = |count: i128| (count.max(0) as u128) * sources;
        let before_target: u128 = layout.archives[..archive]
            .iter()
            .map(|(rows, _)| u128::from(*rows) * sources)
            .sum();
        let mut steps = vec![Step::Copy(before_target)];
        let mut new_current = c;
        match action {
            // RZ-5: the new rows follow the current row, as the oldest.
            RrdResizeAction::Grow => {
                steps.extend([Step::Copy(values(c + 1)), Step::Unknown(values(k))]);
            }
            // RZ-6: the oldest rows go, first those after the current row.
            RrdResizeAction::Shrink => {
                let after = n - 1 - c;
                if k <= after {
                    steps.extend([Step::Copy(values(c + 1)), Step::Skip(values(k))]);
                } else {
                    let before = k - after;
                    new_current = c - before;
                    steps.extend([
                        Step::Skip(values(before)),
                        Step::Copy(values(c + 1 - before)),
                        Step::Skip(values(after)),
                    ]);
                }
            }
        }

        let mut head = input_head.to_vec();
        if layout.legacy_live_head {
            head[4..8].copy_from_slice(b"0003");
            let usec = (layout.live_offset + 8) as usize;
            head.splice(usec..usec, [0_u8; LIVE_USEC_LEN]);
        }
        let definition = STAT_HEAD_LEN
            + layout.sources as usize * DS_DEF_LEN
            + archive * RRA_DEF_LEN
            + ROWS_OFFSET;
        head[definition..definition + 8].copy_from_slice(&new_rows.to_le_bytes());
        let pointer = layout.pointers_offset as usize
            + if layout.legacy_live_head {
                LIVE_USEC_LEN
            } else {
                0
            }
            + archive * RRA_PTR_LEN;
        // RZ-9: a current row out of range is carried, not corrected.
        head[pointer..pointer + 8].copy_from_slice(&(new_current as u64).to_le_bytes());
        Ok(Self {
            head,
            output_len,
            steps,
        })
    }
}

/// Copies between the input and the output the way the reference does:
/// every value write past the output's length fails without moving the
/// write position, and every value read past the input's end repeats the
/// previous value, with a partial value overlaid on it.
struct Transfer<'a> {
    input: &'a mut File,
    input_len: u64,
    read_at: u64,
    output: BufWriter<&'a mut File>,
    output_len: u64,
    write_at: u64,
    previous: [u8; VALUE_LEN],
    /// Where the first write that did not fit was attempted.
    overflow_at: Option<u64>,
}

impl<'a> Transfer<'a> {
    fn room(&self) -> u64 {
        self.output_len - self.write_at
    }

    /// Writes whole values from `bytes`, as many as fit.
    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        let fits = (bytes.len() as u64).min(self.room()) as usize;
        self.output.write_all(&bytes[..fits])?;
        self.write_at += fits as u64;
        if fits < bytes.len() {
            self.overflow_at.get_or_insert(self.write_at);
        }
        Ok(())
    }

    /// Writes `count` copies of one value.
    fn emit_repeated(&mut self, value: [u8; VALUE_LEN], count: u128) -> io::Result<()> {
        const BATCH: usize = 4096;
        let fitting = (self.room() / VALUE_LEN as u64) as u128;
        let mut remaining = count.min(fitting) as u64;
        let batch: Vec<u8> = std::iter::repeat_n(value, BATCH).flatten().collect();
        while remaining > 0 {
            let values = remaining.min(BATCH as u64) as usize;
            self.emit(&batch[..values * VALUE_LEN])?;
            remaining -= values as u64;
        }
        if count > fitting {
            self.overflow_at.get_or_insert(self.write_at);
        }
        Ok(())
    }

    /// Copies `length` bytes from the current read position.
    fn copy_bytes(&mut self, length: u64) -> io::Result<()> {
        if length == 0 {
            return Ok(());
        }
        self.input.seek(SeekFrom::Start(self.read_at))?;
        let mut chunk = vec![0_u8; 1 << 16];
        let mut remaining = length;
        while remaining > 0 {
            let part = remaining.min(chunk.len() as u64) as usize;
            self.input.read_exact(&mut chunk[..part])?;
            self.emit(&chunk[..part])?;
            remaining -= part as u64;
        }
        self.read_at += length;
        Ok(())
    }

    fn copy_values(&mut self, count: u128) -> io::Result<()> {
        let value_len = VALUE_LEN as u64;
        let available = self.input_len.saturating_sub(self.read_at);
        let whole = count.min(u128::from(available / value_len)) as u64;
        if whole > 0 {
            // Only what fits is written; the rest of the reads still happen.
            let written = (whole * value_len).min(self.room());
            self.copy_bytes(written)?;
            self.read_at += whole * value_len - written;
            self.input.seek(SeekFrom::Start(self.read_at - value_len))?;
            self.input.read_exact(&mut self.previous)?;
            if written < whole * value_len {
                self.overflow_at.get_or_insert(self.write_at);
            }
        }
        let mut remaining = count - u128::from(whole);
        let partial = self.input_len.saturating_sub(self.read_at);
        if remaining > 0 && partial > 0 {
            self.input.seek(SeekFrom::Start(self.read_at))?;
            self.input
                .read_exact(&mut self.previous[..partial as usize])?;
            let value = self.previous;
            self.emit(&value)?;
            self.read_at += value_len;
            remaining -= 1;
        }
        if remaining > 0 {
            self.emit_repeated(self.previous, remaining)?;
            let skipped = u64::try_from(remaining.saturating_mul(u128::from(value_len)));
            self.read_at = self.read_at.saturating_add(skipped.unwrap_or(u64::MAX));
        }
        Ok(())
    }

    fn run(&mut self, steps: &[Step]) -> io::Result<()> {
        for step in steps {
            match *step {
                Step::Copy(values) => self.copy_values(values)?,
                Step::Skip(values) => {
                    let bytes = values.saturating_mul(VALUE_LEN as u128);
                    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
                    self.read_at = self.read_at.saturating_add(bytes);
                }
                Step::Unknown(values) => self.emit_repeated(rrd_nan().to_le_bytes(), values)?,
            }
        }
        // RZ-15: whatever follows is copied while the output has room.
        let rest = self.room().min(self.input_len.saturating_sub(self.read_at));
        self.copy_bytes(rest)
    }
}

/// Resizes archive `archive` of `input` by `rows` and writes the result to
/// `output`, which must not exist yet.
///
/// When the input holds bytes the result leaves out (trailing bytes, or the
/// last 8 bytes of a version 0001 file), `on_ignored_bytes` receives their
/// number before the function returns, also when it then fails.
pub fn resize_rrd_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    archive: usize,
    action: RrdResizeAction,
    rows: u64,
    on_ignored_bytes: &mut dyn FnMut(u64),
) -> Result<(), StoreError> {
    let (input, output) = (input.as_ref(), output.as_ref());
    // Compatibility: the input is opened for reading and writing, so a
    // read-only file or a directory fails here, though nothing is written
    // to it (RZ-10).
    // Provenance: black-box compatibility test.
    let mut source = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(input)
        .map_err(|error| {
            StoreError::RrdFile(format!(
                "opening '{}': {}",
                input.display(),
                rrd_strerror(&error)
            ))
        })?;
    let info = read_info(&mut source, input)?;
    let input_len = source.metadata()?.len();
    let live_offset = (STAT_HEAD_LEN
        + info.data_sources.len() * DS_DEF_LEN
        + info.archives.len() * RRA_DEF_LEN) as u64;
    let layout = Layout {
        version: info.version.clone(),
        head_len: info.header_size as u64,
        pointers_offset: info.header_size as u64 - (info.archives.len() * RRA_PTR_LEN) as u64,
        live_offset,
        legacy_live_head: info.live_head_len < 16,
        sources: info.data_sources.len() as u64,
        archives: info
            .archives
            .iter()
            .map(|archive| (archive.rows, archive.current_row))
            .collect(),
    };
    // RZ-20: rows whose unwrapped size passes the end of the file.
    if layout
        .data_len()
        .and_then(|length| length.checked_add(layout.head_len))
        .is_none_or(|end| end > input_len)
    {
        return Err(StoreError::RrdFormat(
            "RRA rows extend past the end of the file".into(),
        ));
    }
    if archive >= layout.archives.len() {
        return Err(rrd_error("no such RRA in this RRD"));
    }
    if action == RrdResizeAction::Shrink && rows >= layout.archives[archive].0 {
        return Err(rrd_error("This RRA is not that big"));
    }
    let mut input_head = vec![0_u8; info.header_size];
    source.seek(SeekFrom::Start(0))?;
    source.read_exact(&mut input_head)?;
    let plan = Plan::new(&layout, &input_head, archive, action, rows)?;

    let cannot_create = |error: &io::Error| {
        rrd_error(format!(
            "Can't create '{}': {}",
            output.display(),
            rrd_strerror(error)
        ))
    };
    let directory = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // RZ-19: refuse what cannot be written before creating anything.
    if crate::rrd_create::available_space(directory)
        .is_some_and(|available| plan.output_len > available)
    {
        return Err(cannot_create(&io::Error::from_raw_os_error(enospc())));
    }
    // Compatibility: RRDtool truncates and rewrites an existing output, and
    // follows a symbolic link there; Rondi creates it exclusively, private
    // to the user, and never replaces anything, its input included
    // (RZ-div-existing-output, RZ-13).
    // Provenance: black-box compatibility test.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut target = options
        .open(output)
        .map_err(|error| cannot_create(&error))?;
    let result = write_output(&mut source, input_len, &mut target, &layout, &plan);
    match result {
        Ok(ignored) => {
            if let Some(ignored) = ignored.ignored_bytes {
                on_ignored_bytes(ignored);
            }
            match ignored.overflow_at {
                // RZ-17: a version 0001 file whose last write does not fit.
                Some(position) => Err(rrd_error(format!(
                    "attempting to write beyond end of file ({position} + {VALUE_LEN} > {})",
                    plan.output_len
                ))),
                None => Ok(()),
            }
        }
        Err(WriteFailure::Version) => {
            // RZ-14: the output was sized and is left zero-filled.
            Err(rrd_error(format!(
                "Do not know how to handle RRD version {}",
                layout.version
            )))
        }
        Err(WriteFailure::Io(error)) => {
            drop(target);
            let _ = std::fs::remove_file(output);
            Err(error.into())
        }
    }
}

#[cfg(unix)]
fn enospc() -> i32 {
    libc::ENOSPC
}

#[cfg(not(unix))]
fn enospc() -> i32 {
    28
}

struct Written {
    ignored_bytes: Option<u64>,
    overflow_at: Option<u64>,
}

enum WriteFailure {
    Version,
    Io(io::Error),
}

impl From<io::Error> for WriteFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn write_output(
    source: &mut File,
    input_len: u64,
    target: &mut File,
    layout: &Layout,
    plan: &Plan,
) -> Result<Written, WriteFailure> {
    target.set_len(plan.output_len)?;
    // Compatibility: only these version strings are resized; others fail
    // after the output was created (RZ-14).
    // Provenance: black-box compatibility test.
    if !matches!(layout.version.as_str(), "0001" | "0003" | "0004") {
        return Err(WriteFailure::Version);
    }
    let mut transfer = Transfer {
        input: source,
        input_len,
        read_at: layout.head_len,
        output: BufWriter::with_capacity(1 << 16, target),
        output_len: plan.output_len,
        write_at: 0,
        previous: [0; VALUE_LEN],
        overflow_at: None,
    };
    transfer.emit(&plan.head)?;
    transfer.run(&plan.steps)?;
    transfer.output.flush()?;
    let ignored = input_len.saturating_sub(transfer.read_at);
    Ok(Written {
        ignored_bytes: (ignored > 0).then_some(ignored),
        overflow_at: transfer.overflow_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file with `sources` data sources and archives of the given rows
    /// and current rows; every stored value is its own byte offset.
    fn file(sources: u64, archives: &[(u64, u64)], legacy: bool) -> Vec<u8> {
        let live = if legacy { 8 } else { 16 };
        let mut bytes = b"RRD\0".to_vec();
        bytes.extend_from_slice(if legacy { b"0001" } else { b"0003" });
        bytes.resize(16, 0);
        bytes.extend_from_slice(&crate::rrd_binary::FLOAT_COOKIE.to_le_bytes());
        bytes.extend_from_slice(&sources.to_le_bytes());
        bytes.extend_from_slice(&(archives.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&60_u64.to_le_bytes());
        bytes.resize(STAT_HEAD_LEN, 0);
        for index in 0..sources {
            let mut record = [0_u8; DS_DEF_LEN];
            record[0] = b'a' + index as u8;
            record[20..25].copy_from_slice(b"GAUGE");
            bytes.extend_from_slice(&record);
        }
        for (rows, _) in archives {
            let mut record = [0_u8; RRA_DEF_LEN];
            record[..7].copy_from_slice(b"AVERAGE");
            record[24..32].copy_from_slice(&rows.to_le_bytes());
            record[32..40].copy_from_slice(&1_u64.to_le_bytes());
            bytes.extend_from_slice(&record);
        }
        bytes.extend_from_slice(&1_000_000_000_u64.to_le_bytes());
        bytes.resize(bytes.len() + live - 8, 0);
        let prep = 112 * sources as usize + 80 * sources as usize * archives.len();
        bytes.resize(bytes.len() + prep, 0);
        for (_, current) in archives {
            bytes.extend_from_slice(&current.to_le_bytes());
        }
        let total: u64 = archives.iter().map(|(rows, _)| rows * sources).sum();
        for _ in 0..total {
            let offset = bytes.len() as f64;
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        bytes
    }

    fn resize(
        input: &[u8],
        archive: usize,
        action: RrdResizeAction,
        rows: u64,
    ) -> (Result<(), String>, Option<Vec<u8>>, u64) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("in.rrd");
        let out = temp.path().join("resize.rrd");
        std::fs::write(&path, input).unwrap();
        let mut ignored = 0;
        let result = resize_rrd_file(&path, &out, archive, action, rows, &mut |bytes| {
            ignored += bytes;
        })
        .map_err(|error| error.to_string());
        (result, std::fs::read(&out).ok(), ignored)
    }

    fn values(bytes: &[u8], from: usize) -> Vec<f64> {
        bytes[from..]
            .chunks(8)
            .map(|chunk| f64::from_le_bytes(chunk.try_into().unwrap()))
            .collect()
    }

    fn word(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    /// Offset of the first row of a version 3 file built by `file`.
    fn data_start(sources: usize, archives: usize) -> usize {
        128 + 120 * (sources + archives)
            + 16
            + 112 * sources
            + 80 * sources * archives
            + 8 * archives
    }

    #[test]
    fn grow_inserts_unknown_rows_after_the_current_row() {
        let input = file(1, &[(5, 2)], false);
        let start = data_start(1, 1);
        let (result, output, ignored) = resize(&input, 0, RrdResizeAction::Grow, 2);
        result.unwrap();
        let output = output.unwrap();
        assert_eq!(ignored, 0);
        assert_eq!(word(&output, 128 + 120 + 24), 7);
        assert_eq!(word(&output, start - 8), 2);
        let old = values(&input, start);
        let new = values(&output, start);
        assert_eq!(&new[..3], &old[..3]);
        assert!(new[3].is_nan() && new[4].is_nan());
        assert_eq!(&new[5..], &old[3..]);
    }

    #[test]
    fn shrink_removes_the_oldest_rows() {
        // RZ-6's examples with n = 5.
        let start = data_start(1, 1);
        for (current, rows, kept, new_current) in [
            (0, 4, vec![0], 0),
            (2, 4, vec![2], 0),
            (4, 1, vec![1, 2, 3, 4], 3),
        ] {
            let input = file(1, &[(5, current)], false);
            let (result, output, _) = resize(&input, 0, RrdResizeAction::Shrink, rows);
            result.unwrap();
            let output = output.unwrap();
            let old = values(&input, start);
            let expected: Vec<f64> = kept.iter().map(|row: &usize| old[*row]).collect();
            assert_eq!(values(&output, start), expected, "current {current}");
            assert_eq!(word(&output, start - 8), new_current);
        }
    }

    #[test]
    fn other_archives_and_trailing_bytes() {
        let mut input = file(2, &[(3, 1), (4, 0)], false);
        let start = data_start(2, 2);
        input.extend_from_slice(&[7; 13]);
        let (result, output, ignored) = resize(&input, 1, RrdResizeAction::Shrink, 1);
        result.unwrap();
        let output = output.unwrap();
        assert_eq!(ignored, 13);
        assert_eq!(output.len(), input.len() - 13 - 16);
        // The first archive is untouched; the second loses its row 1.
        assert_eq!(output[start..start + 48], input[start..start + 48]);
        assert_eq!(
            output[start + 48..start + 64],
            input[start + 48..start + 64]
        );
        assert_eq!(output[start + 64..], input[start + 80..input.len() - 13]);
    }

    #[test]
    fn legacy_files_are_written_as_version_3() {
        let input = file(2, &[(5, 2), (3, 1)], true);
        let (result, output, ignored) = resize(&input, 0, RrdResizeAction::Grow, 1);
        result.unwrap();
        let output = output.unwrap();
        assert_eq!(&output[4..8], b"0003");
        assert_eq!(output.len(), input.len() + 16);
        assert_eq!(ignored, 8);
        // The microseconds are inserted; the last value does not fit.
        let live = 128 + 120 * 4;
        assert_eq!(output[live..live + 8], input[live..live + 8]);
        assert_eq!(word(&output, live + 8), 0);
        let prep = 112 * 2 + 80 * 4;
        assert_eq!(
            output[live + 16..live + 16 + prep],
            input[live + 8..live + 8 + prep]
        );
        let input_rows = live + 8 + prep + 16;
        let output_rows = live + 16 + prep + 16;
        // Rows 0 to 2, one unknown row, rows 3 and 4 less their last value.
        assert_eq!(
            output[output_rows..output_rows + 48],
            input[input_rows..input_rows + 48]
        );
        assert!(
            values(&output, output_rows + 48)[..2]
                .iter()
                .all(|v| v.is_nan())
        );
        assert_eq!(
            output[output_rows + 64..],
            input[input_rows + 48..input.len() - 8]
        );
        let (result, output, ignored) = resize(&input, 1, RrdResizeAction::Shrink, 1);
        let length = input.len() - 16;
        assert_eq!(
            result.unwrap_err(),
            format!("attempting to write beyond end of file ({length} + 8 > {length})")
        );
        assert_eq!(output.unwrap().len(), length);
        assert_eq!(ignored, 0);
    }

    #[test]
    fn checks_and_refusals() {
        let input = file(1, &[(5, 2)], false);
        assert_eq!(
            resize(&input, 1, RrdResizeAction::Grow, 1).0.unwrap_err(),
            "no such RRA in this RRD"
        );
        assert_eq!(
            resize(&input, 0, RrdResizeAction::Shrink, 5).0.unwrap_err(),
            "This RRA is not that big"
        );
        let mut v5 = input.clone();
        v5[4..8].copy_from_slice(b"0005");
        let (result, output, _) = resize(&v5, 0, RrdResizeAction::Grow, 2);
        assert_eq!(
            result.unwrap_err(),
            "Do not know how to handle RRD version 0005"
        );
        let output = output.unwrap();
        assert_eq!(output.len(), input.len() + 16);
        assert!(output.iter().all(|byte| *byte == 0));
        // The row count wraps to the stored length, so only the unwrapped
        // size shows that the rows do not fit.
        let mut huge = input.clone();
        huge[128 + 120 + 24..128 + 120 + 32].copy_from_slice(&((1_u64 << 61) + 5).to_le_bytes());
        let (result, output, _) = resize(&huge, 0, RrdResizeAction::Grow, 1);
        assert_eq!(
            result.unwrap_err(),
            "invalid RRD file: RRA rows extend past the end of the file"
        );
        assert!(output.is_none());
        let (result, output, _) = resize(&input, 0, RrdResizeAction::Grow, u64::MAX);
        assert!(result.is_err());
        assert!(output.is_none());
    }

    #[test]
    fn existing_output_is_never_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resize.rrd");
        let input = file(1, &[(5, 2)], false);
        std::fs::write(&path, &input).unwrap();
        let error = resize_rrd_file(&path, &path, 0, RrdResizeAction::Grow, 1, &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(error.ends_with("File exists"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), input);
    }

    /// Current rows out of range read past their archive and, at the end of
    /// the input, repeat the last value read; nothing panics or loops.
    #[test]
    fn out_of_range_current_rows_stay_bounded() {
        for current in [5, 9, u64::MAX / 2, u64::MAX] {
            for action in [RrdResizeAction::Grow, RrdResizeAction::Shrink] {
                let input = file(2, &[(5, current), (3, 1)], false);
                let (_, output, _) = resize(&input, 0, action, 2);
                let output = output.unwrap();
                let grown: i64 = if action == RrdResizeAction::Grow {
                    32
                } else {
                    -32
                };
                assert_eq!(output.len() as i64, input.len() as i64 + grown);
            }
        }
    }
}
