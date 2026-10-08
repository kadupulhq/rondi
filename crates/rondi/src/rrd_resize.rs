//! Port of RRDtool 1.11.0 `rrd_resize.c` after its argument checks. Both
//! files are handled the way rrd_open maps them: the input is read whole,
//! the output is created O_RDWR|O_CREAT|O_TRUNC at the size computed from
//! the copied header, and every read, write and seek keeps rrd_file's
//! position rules, so short and overlong layouts behave as upstream.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN, RrdFileLock,
    RrdResizeAction, STAT_HEAD_LEN, VALUE_LEN, open_output_file, open_rrd_write, read_info,
    rrd_nan, rrd_strerror,
};
use crate::storage::StoreError;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// An rrd_file_t over a mapped buffer: `pos` may run past `len` and
/// rrd_read/rrd_write clamp or refuse exactly as rrd_open.c:1004-1094.
struct Mapped {
    bytes: Vec<u8>,
    pos: u64,
}

impl Mapped {
    /// Reads up to one value into `buffer`, keeping its old bytes past a
    /// short read; returns the count, 0 at the end.
    fn read(&mut self, buffer: &mut [u8; VALUE_LEN]) -> usize {
        let len = self.bytes.len() as u64;
        if self.pos > len {
            return 0;
        }
        let available = (len - self.pos).min(VALUE_LEN as u64) as usize;
        let start = self.pos as usize;
        buffer[..available].copy_from_slice(&self.bytes[start..start + available]);
        self.pos += available as u64;
        available
    }

    fn write(&mut self, data: &[u8], error: &mut Option<String>) {
        if data.is_empty() {
            return;
        }
        let len = self.bytes.len() as u64;
        if self.pos + data.len() as u64 > len {
            *error = Some(format!(
                "attempting to write beyond end of file ({} + {} > {})",
                self.pos as i64,
                data.len(),
                len
            ));
            return;
        }
        let start = self.pos as usize;
        self.bytes[start..start + data.len()].copy_from_slice(data);
        self.pos += data.len() as u64;
    }
}

/// `rrd_read` one value into the shared buffer and `rrd_write` it out.
fn copy_value(
    input: &mut Mapped,
    out: &mut Mapped,
    buffer: &mut [u8; VALUE_LEN],
    error: &mut Option<String>,
) {
    input.read(buffer);
    out.write(&buffer[..], error);
}

struct Copy<'a> {
    input: &'a mut Mapped,
    out: &'a mut Mapped,
    buffer: &'a mut [u8; VALUE_LEN],
    error: &'a mut Option<String>,
}

impl Copy<'_> {
    /// rrd_resize.c:212-248: drop rows after the cursor, wrapping to the
    /// start of the archive when they run past its end.
    fn shrink(&mut self, ds: u64, cur_row: &mut u64, row_cnt: &mut u64, modify: &mut i64) {
        let row_bytes = (VALUE_LEN as u64).wrapping_mul(ds);
        // (cur_row - modify) % row_cnt in unsigned long, stored in a long.
        let mut remove_end = (cur_row.wrapping_sub(*modify as u64) % *row_cnt) as i64;
        if remove_end <= *cur_row as i64 {
            while remove_end >= 0 {
                self.input.pos = self.input.pos.wrapping_add(row_bytes);
                *cur_row = cur_row.wrapping_sub(1);
                *row_cnt = row_cnt.wrapping_sub(1);
                remove_end -= 1;
                *modify += 1;
            }
        }
        let mut row = 0_u64;
        while row <= *cur_row {
            for _ in 0..ds {
                copy_value(self.input, self.out, self.buffer, self.error);
            }
            row += 1;
        }
        while *modify < 0 {
            self.input.pos = self.input.pos.wrapping_add(row_bytes);
            *row_cnt = row_cnt.wrapping_sub(1);
            *modify += 1;
        }
    }
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Resize archive `rra_index` of `input_path` into `output_path` by
/// `row_count` rows (rrd_resize.c:59-301).
pub fn resize_rrd_file(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    rra_index: usize,
    action: RrdResizeAction,
    row_count: u64,
) -> Result<(), StoreError> {
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();
    if input_path == Path::new("resize.rrd") {
        return Err(StoreError::Rrd("resize.rrd is a reserved name".into()));
    }
    if row_count == 0 {
        return Err(StoreError::Rrd(
            "Please grow or shrink with at least 1 row".into(),
        ));
    }
    let mut modify = row_count as i64;
    if action == RrdResizeAction::Shrink {
        modify = -modify;
    }

    let mut input_file = RrdFileLock::exclusive(open_rrd_write(input_path)?)?;
    let info = read_info(&mut input_file, input_path)?;
    let mut old = Vec::new();
    input_file.seek(SeekFrom::Start(0))?;
    input_file.read_to_end(&mut old)?;
    let ds_cnt = info.data_sources.len();
    let rra_cnt = info.archives.len();
    let rra_start = STAT_HEAD_LEN + ds_cnt * DS_DEF_LEN;
    let live_start = rra_start + rra_cnt * RRA_DEF_LEN;
    let pdp_start = live_start + info.live_head_len;
    let pointer_start = info.header_size - rra_cnt * RRA_PTR_LEN;

    if rra_index >= rra_cnt {
        return Err(StoreError::Rrd("no such RRA in this RRD".into()));
    }
    let row_cnt_offset = |rra: usize| RRA_DEF_LEN * rra + 24;
    let mut rra_defs = old[rra_start..live_start].to_vec();
    if modify < 0 && (info.archives[rra_index].rows as i64) <= -modify {
        return Err(StoreError::Rrd("This RRA is not that big".into()));
    }

    // rrd_open(RRD_CREAT) sizes the file from the copied header, whose
    // version is still the input's.
    let target_rows = u64_at(&rra_defs, row_cnt_offset(rra_index)).wrapping_add(modify as u64);
    rra_defs[row_cnt_offset(rra_index)..row_cnt_offset(rra_index) + 8]
        .copy_from_slice(&target_rows.to_le_bytes());
    let values = (0..rra_cnt).fold(0_u64, |total, rra| {
        total.wrapping_add(u64_at(&rra_defs, row_cnt_offset(rra)))
    });
    let newfile_size = (info.header_size as u64).wrapping_add(
        (VALUE_LEN as u64)
            .wrapping_mul(values)
            .wrapping_mul(ds_cnt as u64),
    );
    let create_error = |error: std::io::Error| {
        StoreError::Rrd(format!(
            "Can't create '{}': {}",
            output_path.display(),
            rrd_strerror(&error)
        ))
    };
    // O_CREAT|O_TRUNC as rrd_open.c:284-293, truncating only once the
    // path is known to be a regular file.
    let output_file = open_output_file(output_path, false).map_err(create_error)?;
    let mut output_file = RrdFileLock::exclusive(output_file)?;
    output_file.set_len(0).map_err(create_error)?;
    output_file.set_len(newfile_size).map_err(create_error)?;
    let new_len = usize::try_from(newfile_size)
        .map_err(|_| create_error(std::io::Error::from_raw_os_error(libc::EFBIG)))?;
    let mut out = Mapped {
        bytes: vec![0; new_len],
        pos: 0,
    };
    let target_rows = u64_at(&rra_defs, row_cnt_offset(rra_index)).wrapping_sub(modify as u64);
    rra_defs[row_cnt_offset(rra_index)..row_cnt_offset(rra_index) + 8]
        .copy_from_slice(&target_rows.to_le_bytes());
    let mut rra_ptrs = (0..rra_cnt)
        .map(|rra| u64_at(&old, pointer_start + rra * RRA_PTR_LEN))
        .collect::<Vec<_>>();

    let mut stat_head = old[..STAT_HEAD_LEN].to_vec();
    match crate::rrd_number::c_strtol(&info.version, 10).0 as i32 {
        3 | 4 => {}
        1 => stat_head[7] = b'3',
        _ => {
            return Err(StoreError::Rrd(format!(
                "Do not know how to handle RRD version {}",
                info.version
            )));
        }
    }

    let mut error = None;
    out.write(&stat_head, &mut error);
    out.write(&old[STAT_HEAD_LEN..rra_start], &mut error);
    out.write(&rra_defs, &mut error);
    let mut live_head = [0_u8; LIVE_HEAD_LEN];
    live_head[..8].copy_from_slice(&info.last_update.to_le_bytes());
    live_head[8..].copy_from_slice(&info.last_update_usec.to_le_bytes());
    out.write(&live_head, &mut error);
    let cdp_start = pdp_start + ds_cnt * PDP_PREP_LEN;
    out.write(&old[pdp_start..cdp_start], &mut error);
    out.write(&old[cdp_start..pointer_start], &mut error);
    out.write(&old[pointer_start..info.header_size], &mut error);

    let mut input = Mapped {
        bytes: old,
        pos: info.header_size as u64,
    };
    let ds = ds_cnt as u64;
    // rrd_resize.c keeps one `buffer` for every copy.
    let mut buffer = [0_u8; VALUE_LEN];
    let before = (0..rra_index).fold(0_u64, |total, rra| {
        total.wrapping_add(ds.wrapping_mul(u64_at(&rra_defs, row_cnt_offset(rra))))
    });
    for _ in 0..before {
        copy_value(&mut input, &mut out, &mut buffer, &mut error);
    }
    if modify > 0 {
        for _ in 0..ds.wrapping_mul(rra_ptrs[rra_index].wrapping_add(1)) {
            copy_value(&mut input, &mut out, &mut buffer, &mut error);
        }
        buffer = rrd_nan().to_le_bytes();
        for _ in 0..ds.wrapping_mul(modify as u64) {
            out.write(&buffer, &mut error);
        }
    } else {
        let mut row_cnt = u64_at(&rra_defs, row_cnt_offset(rra_index));
        let mut copy = Copy {
            input: &mut input,
            out: &mut out,
            buffer: &mut buffer,
            error: &mut error,
        };
        copy.shrink(ds, &mut rra_ptrs[rra_index], &mut row_cnt, &mut modify);
        rra_defs[row_cnt_offset(rra_index)..row_cnt_offset(rra_index) + 8]
            .copy_from_slice(&row_cnt.to_le_bytes());
    }
    loop {
        let read = input.read(&mut buffer);
        if read == 0 {
            break;
        }
        if out.pos + read as u64 > out.bytes.len() as u64 {
            eprintln!(
                "WARNING: ignoring last {read} bytes\nWARNING: if you see this message multiple times for a single file you're in trouble"
            );
            continue;
        }
        out.write(&buffer[..read], &mut error);
    }
    let row_cnt = u64_at(&rra_defs, row_cnt_offset(rra_index)).wrapping_add(modify as u64);
    rra_defs[row_cnt_offset(rra_index)..row_cnt_offset(rra_index) + 8]
        .copy_from_slice(&row_cnt.to_le_bytes());
    out.pos = (STAT_HEAD_LEN + DS_DEF_LEN * ds_cnt) as u64;
    out.write(&rra_defs, &mut error);
    out.pos += (LIVE_HEAD_LEN + PDP_PREP_LEN * ds_cnt + CDP_PREP_LEN * ds_cnt * rra_cnt) as u64;
    let pointers = rra_ptrs
        .iter()
        .flat_map(|pointer| pointer.to_le_bytes())
        .collect::<Vec<_>>();
    out.write(&pointers, &mut error);

    output_file.seek(SeekFrom::Start(0))?;
    output_file.write_all(&out.bytes)?;
    match error {
        Some(message) => Err(StoreError::Rrd(message)),
        None => Ok(()),
    }
}
