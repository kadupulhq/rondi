//! Port of RRDtool 1.11.0 `rrd_modify.c`, the `rrdtool tune` DS/RRA
//! operations: `DEL:`, `DS:`, `DELRRA:`, `RRA#index:[+-=]rows`, `RRA:` and
//! the `--step` stretch. The result is built in memory, archives reordered
//! oldest row first, and written through `write_rrd`'s temporary file and
//! rename, as upstream. Files with COMPUTE data sources or Holt-Winters
//! archives are refused, as `create` refuses them.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN,
    RrdFileLock, STAT_HEAD_LEN, VALUE_LEN, open_rrd_write, read_info, rrd_mul_add, rrd_nan,
};
use crate::rrd_create::{ErrorSlot, parse_ds, parse_rra, write_rrd};
use crate::storage::StoreError;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// Indices into cdp_prep_t.scratch and pdp_prep_t.scratch (rrd_format.h).
const CDP_VAL: usize = 0;
const CDP_UNKN_PDP_CNT: usize = 1;
const CDP_PRIMARY_VAL: usize = 8;
const CDP_SECONDARY_VAL: usize = 9;
const PDP_SCRATCH: usize = 32;

/// glibc's `realloc(ptr, 0)` frees a non-NULL `ptr` and returns NULL, which
/// rrd_modify.c reports as an allocation failure; macOS returns a new
/// minimum-size block. Only data-source-less results reach that case.
const REALLOC_ZERO_FAILS: bool = cfg!(all(target_os = "linux", target_env = "gnu"));

/// One `rra_mod_op_t`: `op` is `=`, `-` or `+` for an existing archive
/// (`DELRRA:n` is `=` with zero rows) and `a` for an added `RRA:` whose
/// definition is `def`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RrdRraOp {
    pub index: i32,
    pub op: u8,
    pub row_count: u32,
    pub def: Option<String>,
}

#[derive(Clone)]
struct Rrd {
    version: [u8; 5],
    pdp_step: u64,
    ds_defs: Vec<[u8; DS_DEF_LEN]>,
    rra_defs: Vec<[u8; RRA_DEF_LEN]>,
    live_head: [u8; LIVE_HEAD_LEN],
    pdp_preps: Vec<[u8; PDP_PREP_LEN]>,
    /// RRA-major: `rra * ds_cnt + ds`.
    cdp_preps: Vec<[u8; CDP_PREP_LEN]>,
    cur_rows: Vec<u64>,
    /// Raw value bits, so copied NaN payloads survive unchanged.
    values: Vec<u64>,
}

impl Rrd {
    fn last_up(&self) -> i64 {
        i64::from_le_bytes(self.live_head[..8].try_into().unwrap())
    }

    fn ds_cnt(&self) -> usize {
        self.ds_defs.len()
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; STAT_HEAD_LEN];
        bytes[..4].copy_from_slice(b"RRD\0");
        bytes[4..9].copy_from_slice(&self.version);
        bytes[16..24].copy_from_slice(&FLOAT_COOKIE.to_le_bytes());
        bytes[24..32].copy_from_slice(&(self.ds_defs.len() as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&(self.rra_defs.len() as u64).to_le_bytes());
        bytes[40..48].copy_from_slice(&self.pdp_step.to_le_bytes());
        self.ds_defs
            .iter()
            .for_each(|def| bytes.extend_from_slice(def));
        self.rra_defs
            .iter()
            .for_each(|def| bytes.extend_from_slice(def));
        bytes.extend_from_slice(&self.live_head);
        self.pdp_preps
            .iter()
            .for_each(|prep| bytes.extend_from_slice(prep));
        self.cdp_preps
            .iter()
            .for_each(|prep| bytes.extend_from_slice(prep));
        self.cur_rows
            .iter()
            .for_each(|row| bytes.extend_from_slice(&row.to_le_bytes()));
        bytes.reserve(self.values.len() * VALUE_LEN);
        self.values
            .iter()
            .for_each(|value| bytes.extend_from_slice(&value.to_le_bytes()));
        bytes
    }
}

fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn put_word(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn scratch(prep: &[u8; CDP_PREP_LEN], index: usize) -> u64 {
    word(prep, index * 8)
}

fn set_scratch(prep: &mut [u8; CDP_PREP_LEN], index: usize, value: u64) {
    put_word(prep, index * 8, value);
}

fn rows(def: &[u8; RRA_DEF_LEN]) -> u64 {
    word(def, 24)
}

fn pdp_cnt(def: &[u8; RRA_DEF_LEN]) -> u64 {
    word(def, 32)
}

fn cf_name(def: &[u8; RRA_DEF_LEN]) -> &[u8] {
    let field = &def[..20];
    &field[..field.iter().position(|byte| *byte == 0).unwrap_or(20)]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cf {
    Average,
    Minimum,
    Maximum,
    Last,
    Other,
}

/// `rrd_cf_conv` for the consolidation functions Rondi writes.
fn cf(def: &[u8; RRA_DEF_LEN]) -> Cf {
    match cf_name(def) {
        b"AVERAGE" => Cf::Average,
        b"MIN" => Cf::Minimum,
        b"MAX" => Cf::Maximum,
        b"LAST" => Cf::Last,
        _ => Cf::Other,
    }
}

/// rrd_create.c:2772, including C's behaviour for a negative dividend.
fn positive_mod(a: i32, b: i32) -> i32 {
    if b == 0 {
        return 0;
    }
    let x = a.wrapping_rem(b);
    if x < 0 { x + b } else { x }
}

/// rrd_create.c:2794 `end_time_for_row`, with its `int` arithmetic.
fn end_time_for_row(
    step: u64,
    last_up: i64,
    rra: &[u8; RRA_DEF_LEN],
    cur_row: i32,
    row: i32,
) -> i64 {
    let timeslot = pdp_cnt(rra).wrapping_mul(step) as i32;
    if timeslot == 0 {
        return last_up;
    }
    let past_cnt = positive_mod(cur_row.wrapping_sub(row), rows(rra) as i32);
    (last_up - last_up % i64::from(timeslot)) - i64::from(past_cnt.wrapping_mul(timeslot))
}

/// rrd_create.c:2825 `row_for_time`: -1 when `req_time` is out of range.
fn row_for_time(
    step: u64,
    last_up: i64,
    rra: &[u8; RRA_DEF_LEN],
    cur_row: i32,
    req_time: i64,
) -> i32 {
    let mut last_up = last_up;
    let mut req_time = req_time;
    let timeslot = pdp_cnt(rra).wrapping_mul(step) as i32;
    if timeslot == 0 {
        return -1;
    }
    let slot = i64::from(timeslot);
    let delta = req_time % slot;
    if delta > 0 {
        req_time += slot - delta;
    }
    let delta = req_time % slot;
    if delta > 0 {
        last_up += slot - delta;
    }
    let row_cnt = rows(rra) as i32;
    if req_time > last_up {
        return -1;
    }
    if req_time <= i64::from((last_up as i32).wrapping_sub(row_cnt.wrapping_mul(timeslot))) {
        return -1;
    }
    let past_cnt = ((last_up - req_time) / slot) as i32;
    if past_cnt >= row_cnt {
        return -1;
    }
    let row = positive_mod(cur_row.wrapping_sub(past_cnt), row_cnt);
    if row < 0 { row + row_cnt } else { row }
}

/// One `candidate_t`, laid out for `qsort` with `sort_candidates`.
#[repr(C)]
struct Candidate {
    rra_index: i32,
    pdp_cnt: u64,
    row_cnt: u64,
    /// Offset of the archive's first value in the input's value array.
    values: usize,
}

extern "C" fn sort_candidates(a: *const libc::c_void, b: *const libc::c_void) -> libc::c_int {
    if a == b {
        return 0;
    }
    // SAFETY: qsort passes pointers to elements of the Candidate slice.
    let (a, b) = unsafe { (&*a.cast::<Candidate>(), &*b.cast::<Candidate>()) };
    if a.rra_index == b.rra_index {
        return 0;
    }
    // rrd_modify.c:72-77: unsigned long differences converted to int.
    if a.pdp_cnt == b.pdp_cnt {
        return b.row_cnt.wrapping_sub(a.row_cnt) as i32;
    }
    a.pdp_cnt.wrapping_sub(b.pdp_cnt) as i32
}

/// rrd_modify.c:87 `find_candidate_rras` with `select_for_modify`, sorted
/// by the C library's own qsort so ties fall where upstream's do.
fn find_candidate_rras(input: &Rrd, tofill: &[u8; RRA_DEF_LEN]) -> Vec<Candidate> {
    let wanted = cf(tofill);
    let mut candidates = Vec::new();
    let mut total_rows = 0_usize;
    for (index, def) in input.rra_defs.iter().enumerate() {
        let other = cf(def);
        if other == wanted || other == Cf::Average {
            candidates.push(Candidate {
                rra_index: index as i32,
                pdp_cnt: pdp_cnt(def),
                row_cnt: rows(def),
                values: input.ds_cnt() * total_rows,
            });
        }
        total_rows += rows(def) as usize;
    }
    if candidates.len() > 1 {
        // SAFETY: the pointer and length describe the live Vec, and the
        // comparator only reads two of its elements.
        unsafe {
            libc::qsort(
                candidates.as_mut_ptr().cast(),
                candidates.len(),
                std::mem::size_of::<Candidate>(),
                Some(sort_candidates),
            );
        }
    }
    candidates
}

/// `tmp + v * cand_timeslot` in populate_row (rrd_modify.c:530). Its
/// product also appears in the other arm of the conditional; GCC computes it
/// once and adds separately, while Clang fuses the add into it.
fn populate_mul_add(v: f64, slot: f64, tmp: f64) -> f64 {
    if cfg!(all(target_arch = "aarch64", target_vendor = "apple")) {
        v.mul_add(slot, tmp)
    } else {
        v * slot + tmp
    }
}

/// Where populate_row writes: the output step, last update and DS count.
#[derive(Clone, Copy)]
struct OutShape {
    step: u64,
    last_up: i64,
    ds_cnt: usize,
}

/// rrd_modify.c:407 `populate_row`: fills unknown cells of rows
/// `start..start + count` of `values` (the archive's first row) from the
/// input's archives.
#[allow(clippy::too_many_arguments)]
fn populate_row(
    input: &Rrd,
    out: OutShape,
    ds_map: &[i32],
    new_rra: &[u8; RRA_DEF_LEN],
    cur_row: i32,
    values: &mut [u64],
    start: i32,
    count: i32,
) {
    if input.rra_defs.is_empty() {
        return;
    }
    let wanted = cf(new_rra);
    if wanted == Cf::Other {
        return;
    }
    let in_ds = input.ds_cnt();
    let candidates = find_candidate_rras(input, new_rra);
    let in_last_up = input.last_up();
    for ri in 0..count.max(0) {
        let row = start + ri;
        let new_timeslot = pdp_cnt(new_rra).wrapping_mul(out.step) as i64;
        let row_end_time = end_time_for_row(out.step, out.last_up, new_rra, cur_row, row);
        let row_start_time = row_end_time - new_timeslot + 1;
        for candidate in &candidates {
            let r = &input.rra_defs[candidate.rra_index as usize];
            let r_rows = rows(r);
            let cand_cur_row = input.cur_rows[candidate.rra_index as usize] as i32;
            let mut cand_row_start =
                row_for_time(input.pdp_step, in_last_up, r, cand_cur_row, row_start_time);
            let mut cand_row_end =
                row_for_time(input.pdp_step, in_last_up, r, cand_cur_row, row_end_time);
            if cand_row_start == -1 && cand_row_end != -1 {
                cand_row_start = cand_cur_row;
            } else if cand_row_start != -1 && cand_row_end == -1 {
                // (cand_cur_row - 1) % r->row_cnt is computed unsigned.
                cand_row_end = ((cand_cur_row.wrapping_sub(1) as i64 as u64) % r_rows) as i32;
            } else if cand_row_start == -1 && cand_row_end == -1 {
                continue;
            }
            let mut cand_rows = cand_row_end.wrapping_sub(cand_row_start).wrapping_add(1);
            if cand_rows < 0 {
                cand_rows = (cand_rows as i64 as u64).wrapping_add(r_rows) as i32;
            }
            let cand_timeslot = pdp_cnt(r).wrapping_mul(input.pdp_step) as i32;
            for (k, &in_k) in ds_map.iter().enumerate().take(out.ds_cnt) {
                let cell = row as usize * out.ds_cnt + k;
                if !f64::from_bits(values[cell]).is_nan() {
                    continue;
                }
                if in_k < 0 {
                    continue;
                }
                let mut tmp = rrd_nan();
                let mut last = rrd_nan();
                let mut covered = 0_i32;
                let mut cand_row = cand_row_start;
                for _ in 0..cand_rows.max(0) {
                    let v = f64::from_bits(
                        input.values[candidate.values + cand_row as usize * in_ds + in_k as usize],
                    );
                    cand_row = ((cand_row.wrapping_add(1) as i64 as u64) % r_rows) as i32;
                    if v.is_nan() {
                        continue;
                    }
                    match wanted {
                        Cf::Average => {
                            let slot = f64::from(cand_timeslot);
                            tmp = if tmp.is_nan() {
                                v * slot
                            } else {
                                populate_mul_add(v, slot, tmp)
                            };
                            covered = covered.wrapping_add(cand_timeslot);
                            last = tmp / f64::from(covered);
                        }
                        Cf::Minimum => {
                            tmp = if tmp.is_nan() || tmp >= v { v } else { tmp };
                            last = tmp;
                        }
                        Cf::Maximum => {
                            tmp = if tmp.is_nan() || tmp <= v { v } else { tmp };
                            last = tmp;
                        }
                        Cf::Last => {
                            tmp = v;
                            last = v;
                        }
                        Cf::Other => return,
                    }
                }
                values[cell] = last.to_bits();
            }
        }
    }
}

/// rrd_create.c:1250 `init_cdp` for AVERAGE, MIN, MAX and LAST.
fn init_cdp(
    step: u64,
    last_up: i64,
    rra: &[u8; RRA_DEF_LEN],
    pdp_prep: &[u8; PDP_PREP_LEN],
    cdp: &mut [u8; CDP_PREP_LEN],
) {
    set_scratch(cdp, CDP_VAL, rrd_nan().to_bits());
    let period = step.wrapping_mul(pdp_cnt(rra));
    let unknown_seconds = word(pdp_prep, PDP_SCRATCH);
    let unknown = if period == 0 || step == 0 {
        0
    } else {
        ((last_up as u64).wrapping_sub(unknown_seconds) % period) / step
    };
    set_scratch(cdp, CDP_UNKN_PDP_CNT, unknown);
}

/// rrd_modify.c:749 `rrd_modify_structure`. `ops` receives each matched
/// operation's final row count, as upstream writes it back.
fn modify_structure(
    input: &Rrd,
    remove: &[String],
    add: &[String],
    ops: &mut [(RrdRraOp, i32)],
    error: &mut ErrorSlot,
) -> Result<Rrd, StoreError> {
    let version = crate::rrd_number::c_strtol(
        &String::from_utf8_lossy(
            &input.version[..input.version.iter().position(|b| *b == 0).unwrap_or(5)],
        ),
        10,
    )
    .0;
    if !(3..=5).contains(&version) {
        return Err(StoreError::Rrd(
            "direct modification is only supported for version 3, 4 or 5 of RRD files. Consider to dump/restore before retrying a modification".into(),
        ));
    }
    let step = input.pdp_step;
    let last_up = input.last_up();
    let mut version_field = [0_u8; 5];
    let len = input
        .version
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(4)
        .min(4);
    version_field[..len].copy_from_slice(&input.version[..len]);
    let mut out = Rrd {
        version: version_field,
        pdp_step: step,
        ds_defs: Vec::new(),
        rra_defs: Vec::new(),
        live_head: input.live_head,
        pdp_preps: Vec::new(),
        cdp_preps: Vec::new(),
        cur_rows: Vec::new(),
        values: Vec::new(),
    };

    // 'c' copy, 'd' delete, 'a' add, one per input DS then one per added DS.
    let mut ds_ops = input
        .ds_defs
        .iter()
        .map(|def| {
            let name = &def[..def[..20].iter().position(|b| *b == 0).unwrap_or(20)];
            if remove.iter().any(|removed| removed.as_bytes() == name) {
                b'd'
            } else {
                b'c'
            }
        })
        .collect::<Vec<_>>();
    for (index, op) in ds_ops.iter().enumerate() {
        if *op == b'c' {
            out.ds_defs.push(input.ds_defs[index]);
            out.pdp_preps.push(input.pdp_preps[index]);
        }
    }

    // add_dss (rrd_modify.c:328); its parse errors do not stop it.
    for def in add {
        let mut added = [0_u8; DS_DEF_LEN];
        let mut ignored_version = "0003";
        parse_ds(&def[3..], &mut added, &mut ignored_version, error);
        let name = &added[..added[..20].iter().position(|b| *b == 0).unwrap_or(20)];
        if out.ds_defs.iter().any(|existing| {
            &existing[..existing[..20].iter().position(|b| *b == 0).unwrap_or(20)] == name
        }) {
            return Err(StoreError::Rrd(format!(
                "Duplicate DS name: {}",
                String::from_utf8_lossy(name)
            )));
        }
        out.ds_defs.push(added);
        let mut prep = [0_u8; PDP_PREP_LEN];
        prep[0] = b'U';
        put_word(
            &mut prep,
            PDP_SCRATCH,
            (last_up as u64).checked_rem(step).unwrap_or(0),
        );
        put_word(&mut prep, PDP_SCRATCH + 8, 0.0_f64.to_bits());
        out.pdp_preps.push(prep);
        ds_ops.push(b'a');
    }

    let mut ds_map = Vec::with_capacity(out.ds_cnt());
    for (index, op) in ds_ops.iter().enumerate() {
        match op {
            b'c' => ds_map.push(index as i32),
            b'a' => ds_map.push(-1),
            _ => {}
        }
    }
    let out_ds = out.ds_cnt();
    let in_ds = input.ds_cnt();

    // handle_rra_defs (rrd_modify.c:202).
    for (j, def) in input.rra_defs.iter().enumerate() {
        let mut final_row_count = rows(def) as i32;
        if let Some((op, recorded)) = ops.iter_mut().find(|(op, _)| op.index == j as i32) {
            match op.op {
                b'=' => final_row_count = op.row_count as i32,
                b'-' => final_row_count = final_row_count.wrapping_sub(op.row_count as i32),
                b'+' => final_row_count = final_row_count.wrapping_add(op.row_count as i32),
                _ => {}
            }
            if final_row_count < 0 {
                final_row_count = 0;
            }
            *recorded = final_row_count;
        }
        if final_row_count == 0 {
            continue;
        }
        // With no data sources the cdp_prep realloc asks for zero bytes;
        // the second such call frees the block and fails under glibc.
        if out_ds == 0 && !out.rra_defs.is_empty() && REALLOC_ZERO_FAILS {
            return Err(StoreError::Rrd("Cannot allocate memory".into()));
        }
        let mut new_def = *def;
        put_word(&mut new_def, 24, final_row_count as i64 as u64);
        out.rra_defs.push(new_def);
        out.cur_rows.push((final_row_count as i64 - 1) as u64);
        let mut ii = 0;
        for (i, op) in ds_ops.iter().enumerate() {
            match op {
                b'c' => {
                    out.cdp_preps.push(input.cdp_preps[j * in_ds + i]);
                    ii += 1;
                }
                b'a' => {
                    let mut cdp = [0_u8; CDP_PREP_LEN];
                    init_cdp(step, last_up, &new_def, &out.pdp_preps[ii], &mut cdp);
                    out.cdp_preps.push(cdp);
                    ii += 1;
                }
                _ => {}
            }
        }
    }

    // mod_rras (rrd_modify.c:563).
    let shape = OutShape {
        step,
        last_up,
        ds_cnt: out_ds,
    };
    let unknown = rrd_nan().to_bits();
    let mut in_offset = 0_usize;
    let mut out_rra = 0_usize;
    for (rra_index, def) in input.rra_defs.iter().enumerate() {
        let in_rows = rows(def) as usize;
        let in_values = in_ds * in_rows;
        let op = ops.iter().find(|(op, _)| op.index == rra_index as i32);
        if matches!(op, Some((_, 0))) {
            // Upstream skips before advancing its input offset, so every
            // later archive is read starting at the deleted one's data.
            continue;
        }
        let out_def = out.rra_defs[out_rra];
        let out_rows = rows(&out_def) as usize;
        let mut block = unknown_rows(out_ds, out_rows)?;
        let mut ii = 0_usize;
        let mut oi = 0_usize;
        if let Some((op, _)) = op {
            let mut kind = op.op;
            let mut row_count = op.row_count as u64;
            if kind == b'=' {
                if row_count < in_rows as u64 {
                    row_count = in_rows as u64 - row_count;
                    kind = b'-';
                } else if row_count > in_rows as u64 {
                    row_count -= in_rows as u64;
                    kind = b'+';
                }
            }
            match kind {
                b'-' => ii = row_count as usize,
                b'+' => {
                    // A count that wrapped the int row total would make
                    // upstream write past its allocation.
                    if row_count > out_rows as u64 {
                        return Err(StoreError::Rrd("out of memory".into()));
                    }
                    oi = row_count as usize;
                    // Upstream passes out->rra_ptr[rra_index], indexed by the
                    // input archive; it only differs after a deletion.
                    let cur_row = out
                        .cur_rows
                        .get(rra_index)
                        .copied()
                        .unwrap_or(out.cur_rows[out_rra]) as i32;
                    populate_row(
                        input,
                        shape,
                        &ds_map,
                        &out_def,
                        cur_row,
                        &mut block,
                        0,
                        row_count as i32,
                    );
                }
                _ => {}
            }
        }
        let cur_row = input.cur_rows[rra_index] as usize;
        while ii < in_rows && oi < out_rows {
            let real_ii = (ii + cur_row + 1) % in_rows;
            let mut jj = 0;
            for (j, ds_op) in ds_ops.iter().enumerate() {
                match ds_op {
                    b'c' => {
                        block[oi * out_ds + jj] = input.values[in_offset + real_ii * in_ds + j];
                        jj += 1;
                    }
                    b'a' => {
                        block[oi * out_ds + jj] = unknown;
                        jj += 1;
                    }
                    _ => {}
                }
            }
            ii += 1;
            oi += 1;
        }
        out.values.extend_from_slice(&block);
        in_offset += in_values;
        out_rra += 1;
    }

    // add_rras (rrd_modify.c:1171).
    let last_rra_cnt = out.rra_defs.len();
    // Without data sources, either the cdp_prep or the rrd_value realloc
    // of an added archive is a zero-byte realloc of a live block.
    if out_ds == 0 && REALLOC_ZERO_FAILS && ops.iter().any(|(op, _)| op.op == b'a') {
        for (op, _) in ops.iter().filter(|(op, _)| op.op == b'a') {
            let mut def = [0_u8; RRA_DEF_LEN];
            parse_rra(op.def.as_deref().unwrap_or_default(), &mut def, step, error);
            if let Some(message) = error.take() {
                return Err(StoreError::Rrd(message));
            }
        }
        return Err(StoreError::Rrd("out of memory".into()));
    }
    for (op, _) in ops.iter().filter(|(op, _)| op.op == b'a') {
        let mut def = [0_u8; RRA_DEF_LEN];
        parse_rra(op.def.as_deref().unwrap_or_default(), &mut def, step, error);
        if let Some(message) = error.take() {
            return Err(StoreError::Rrd(message));
        }
        out.rra_defs.push(def);
    }
    for curr_rra in last_rra_cnt..out.rra_defs.len() {
        let def = out.rra_defs[curr_rra];
        let row_cnt = rows(&def) as usize;
        let cur_row = row_cnt as i64 - 1;
        out.cur_rows.push(cur_row as u64);
        let start_rows = out.rra_defs[..curr_rra]
            .iter()
            .map(|d| rows(d) as usize)
            .sum();
        let mut block = unknown_rows(out_ds, row_cnt)?;
        populate_row(
            input,
            shape,
            &ds_map,
            &def,
            cur_row as i32,
            &mut block,
            0,
            row_cnt as i32,
        );
        out.values.extend_from_slice(&block);
        prepare_cdps(input, &mut out, curr_rra, start_rows, &ds_map);
    }
    Ok(out)
}

/// `rows` rows of `ds_cnt` unknown values, or upstream's "out of memory"
/// when they cannot be allocated.
fn unknown_rows(ds_cnt: usize, rows: usize) -> Result<Vec<u64>, StoreError> {
    let out_of_memory = || StoreError::Rrd("out of memory".into());
    let len = ds_cnt.checked_mul(rows).ok_or_else(out_of_memory)?;
    let mut block = Vec::new();
    block.try_reserve_exact(len).map_err(|_| out_of_memory())?;
    block.resize(len, rrd_nan().to_bits());
    Ok(block)
}

/// rrd_modify.c:1012 `prepare_CDPs` for the archive just appended to `out`.
/// `start_values_index` is a row count used as a value index, as upstream.
fn prepare_cdps(
    input: &Rrd,
    out: &mut Rrd,
    curr_rra: usize,
    start_values_index: usize,
    ds_map: &[i32],
) {
    let def = out.rra_defs[curr_rra];
    let wanted = cf(&def);
    let candidates = find_candidate_rras(input, &def);
    let chosen = candidates.iter().find(|candidate| {
        candidate.pdp_cnt == 1 || cf(&input.rra_defs[candidate.rra_index as usize]) == Cf::Average
    });
    let out_ds = out.ds_cnt();
    let in_ds = input.ds_cnt();
    let in_last_up = input.last_up();
    for (i, mapped) in ds_map.iter().enumerate().take(out_ds) {
        let mut cdp = [0_u8; CDP_PREP_LEN];
        init_cdp(
            out.pdp_step,
            out.last_up(),
            &def,
            &out.pdp_preps[i],
            &mut cdp,
        );
        if let (Some(chosen), true) = (chosen, *mapped != -1) {
            let mapped = *mapped as usize;
            let cand_def = &input.rra_defs[chosen.rra_index as usize];
            let cand_rows = rows(cand_def);
            let cand_cur_row = input.cur_rows[chosen.rra_index as usize] as i32;
            let timeslot = pdp_cnt(&def).wrapping_mul(input.pdp_step) as i32;
            let delta = if timeslot == 0 {
                0
            } else {
                (in_last_up % i64::from(timeslot)) as i32
            };
            let mut end_time = in_last_up;
            if delta != 0 {
                end_time = in_last_up - i64::from(delta) + i64::from(timeslot);
            }
            let start_time = end_time - i64::from(timeslot) + 1;
            let start_row = row_for_time(
                input.pdp_step,
                in_last_up,
                cand_def,
                cand_cur_row,
                start_time,
            );
            let mut end_row =
                row_for_time(input.pdp_step, in_last_up, cand_def, cand_cur_row, end_time);
            if end_time >= in_last_up {
                end_row = cand_cur_row;
            }
            let mut cnt = end_row.wrapping_sub(start_row).wrapping_add(1);
            if end_row < start_row {
                cnt = (cnt as i64 as u64).wrapping_add(cand_rows) as i32;
            }
            let row_cnt = cand_rows as i32;
            let ccdp = &input.cdp_preps[chosen.rra_index as usize * in_ds + mapped];
            cdp = [0_u8; CDP_PREP_LEN];
            let curr =
                out.values[start_values_index + out_ds * out.cur_rows[curr_rra] as usize + i];
            set_scratch(&mut cdp, CDP_PRIMARY_VAL, curr);
            set_scratch(&mut cdp, CDP_VAL, 0.0_f64.to_bits());
            if start_row == -1 {
                set_scratch(&mut cdp, CDP_SECONDARY_VAL, scratch(ccdp, CDP_PRIMARY_VAL));
            } else {
                let mut pre_start = start_row - 1;
                if pre_start < 0 {
                    pre_start = row_cnt - 1;
                }
                let value_at =
                    |row: i32| input.values[chosen.values + in_ds * row as usize + mapped];
                set_scratch(&mut cdp, CDP_SECONDARY_VAL, value_at(pre_start));
                for j in 0..cnt.max(0) {
                    let row = positive_rem(start_row.wrapping_add(j), row_cnt);
                    let v = f64::from_bits(value_at(row));
                    // update_cdp with rra_step_cnt 0 and elapsed_pdp_st 1.
                    if v.is_nan() {
                        let unknown = scratch(&cdp, CDP_UNKN_PDP_CNT).wrapping_add(1);
                        set_scratch(&mut cdp, CDP_UNKN_PDP_CNT, unknown);
                    } else {
                        let value = f64::from_bits(scratch(&cdp, CDP_VAL));
                        let value = calculate_cdp_val(value, v, wanted);
                        set_scratch(&mut cdp, CDP_VAL, value.to_bits());
                    }
                }
            }
        }
        out.cdp_preps.push(cdp);
    }
}

/// `(start_row + j) % row_cnt` in `int`; the operands are never negative.
fn positive_rem(a: i32, b: i32) -> i32 {
    if b == 0 { 0 } else { a.wrapping_rem(b) }
}

/// rrd_update.c:2236 `calculate_cdp_val` with one elapsed PDP.
fn calculate_cdp_val(cdp_val: f64, pdp_temp_val: f64, cf: Cf) -> f64 {
    if cdp_val.is_nan() {
        return pdp_temp_val;
    }
    match cf {
        Cf::Average => rrd_mul_add(pdp_temp_val, 1.0, cdp_val),
        Cf::Minimum => {
            if pdp_temp_val < cdp_val {
                pdp_temp_val
            } else {
                cdp_val
            }
        }
        Cf::Maximum => {
            if pdp_temp_val > cdp_val {
                pdp_temp_val
            } else {
                cdp_val
            }
        }
        _ => pdp_temp_val,
    }
}

/// rrd_modify.c:699 `stretch_rras`.
fn stretch_rras(rrd: &mut Rrd, stretch: u64) {
    let ds_cnt = rrd.ds_cnt();
    for (index, def) in rrd.rra_defs.iter_mut().enumerate() {
        if cf(def) != Cf::Other {
            for prep in &mut rrd.cdp_preps[index * ds_cnt..(index + 1) * ds_cnt] {
                // The count is scaled through the union's double member.
                let count = f64::from_bits(scratch(prep, CDP_UNKN_PDP_CNT)) * stretch as f64;
                set_scratch(prep, CDP_UNKN_PDP_CNT, count.to_bits());
            }
        }
        let scaled = pdp_cnt(def).wrapping_mul(stretch);
        put_word(def, 32, scaled);
    }
    rrd.pdp_step /= stretch;
}

fn read_rrd(bytes: &[u8], info: &crate::RrdInfo) -> Result<Rrd, StoreError> {
    let ds_cnt = info.data_sources.len();
    let rra_cnt = info.archives.len();
    if info.live_head_len != LIVE_HEAD_LEN {
        return Err(StoreError::Rrd(
            "direct modification is only supported for version 3, 4 or 5 of RRD files. Consider to dump/restore before retrying a modification".into(),
        ));
    }
    let mut offset = STAT_HEAD_LEN;
    let mut take = |len: usize| {
        let slice = &bytes[offset..offset + len];
        offset += len;
        slice
    };
    let ds_defs = (0..ds_cnt)
        .map(|_| take(DS_DEF_LEN).try_into().unwrap())
        .collect::<Vec<[u8; DS_DEF_LEN]>>();
    let rra_defs = (0..rra_cnt)
        .map(|_| take(RRA_DEF_LEN).try_into().unwrap())
        .collect::<Vec<[u8; RRA_DEF_LEN]>>();
    let live_head = take(LIVE_HEAD_LEN).try_into().unwrap();
    let pdp_preps = (0..ds_cnt)
        .map(|_| take(PDP_PREP_LEN).try_into().unwrap())
        .collect();
    let cdp_preps = (0..ds_cnt * rra_cnt)
        .map(|_| take(CDP_PREP_LEN).try_into().unwrap())
        .collect();
    let cur_rows = (0..rra_cnt)
        .map(|_| word(take(RRA_PTR_LEN), 0))
        .collect::<Vec<_>>();
    let value_count = rra_defs.iter().map(|def| rows(def) as usize).sum::<usize>() * ds_cnt;
    let values = (0..value_count).map(|_| word(take(VALUE_LEN), 0)).collect();
    if ds_defs.iter().any(|def| &def[20..27] == b"COMPUTE")
        || rra_defs.iter().any(|def| cf(def) == Cf::Other)
    {
        return Err(StoreError::RrdUnsupported(
            "tune DS/RRA modification supports basic data sources and AVERAGE/MIN/MAX/LAST archives only".into(),
        ));
    }
    if rra_defs
        .iter()
        .zip(&cur_rows)
        .any(|(def, cur_row)| *cur_row >= rows(def))
    {
        return Err(StoreError::Rrd("invalid RRA pointer in RRD file".into()));
    }
    let mut version = [0_u8; 5];
    version.copy_from_slice(&bytes[4..9]);
    Ok(Rrd {
        version,
        pdp_step: word(bytes, 40),
        ds_defs,
        rra_defs,
        live_head,
        pdp_preps,
        cdp_preps,
        cur_rows,
        values,
    })
}

/// `handle_modify` after its argument parsing (rrd_modify.c:1435): rewrite
/// `path` with data sources `remove`d and `add`ed (full `DS:` definitions)
/// and the archive operations applied. `newstep` is `tune --step`, or a
/// value below 1 for none.
pub fn modify_rrd_file(
    path: impl AsRef<Path>,
    remove: &[String],
    add: &[String],
    rra_ops: &[RrdRraOp],
    newstep: i64,
) -> Result<(), StoreError> {
    let path = path.as_ref();
    let mut file = RrdFileLock::exclusive(open_rrd_write(path)?)?;
    let info = read_info(&mut file, path)?;
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_end(&mut bytes)?;
    let input = read_rrd(&bytes, &info)?;

    let mut error = ErrorSlot::default();
    let mut ops = rra_ops
        .iter()
        .cloned()
        .map(|op| (op, -1))
        .collect::<Vec<_>>();
    let out = if newstep > 0 {
        let step = input.pdp_step;
        let newstep = newstep as u64;
        if !(step % newstep == 0 && step / newstep > 1) {
            return Err(StoreError::Rrd(
                "invalid 'newstep' parameter. The newsize must divide the old step parameter without a remainder.".into(),
            ));
        }
        let mut stretched = modify_structure(&input, &[], &[], &mut [], &mut error)?;
        stretch_rras(&mut stretched, step / newstep);
        modify_structure(&stretched, remove, add, &mut ops, &mut error)?
    } else {
        modify_structure(&input, remove, add, &mut ops, &mut error)?
    };
    // A failed DS definition does not stop upstream, which writes the
    // definition's uninitialized bytes and then reports the error. Report
    // it without replacing the file.
    if let Some(message) = error.take() {
        return Err(StoreError::Rrd(message));
    }
    let name = path
        .to_str()
        .ok_or_else(|| StoreError::Rrd("file name is not UTF-8".into()))?;
    write_rrd(name, &out.to_bytes(), false)?;
    drop(file);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{positive_mod, row_for_time};

    #[test]
    fn positive_mod_keeps_c_remainder_sign_rules() {
        assert_eq!(positive_mod(-1, 5), 4);
        assert_eq!(positive_mod(7, 5), 2);
        assert_eq!(positive_mod(3, 0), 0);
    }

    #[test]
    fn row_for_time_matches_slot_alignment() {
        let mut rra = [0_u8; super::RRA_DEF_LEN];
        rra[24..32].copy_from_slice(&10_u64.to_le_bytes());
        rra[32..40].copy_from_slice(&1_u64.to_le_bytes());
        // last_up 1000, step 100: row 9 holds the slot ending at 1000.
        assert_eq!(row_for_time(100, 1000, &rra, 9, 1000), 9);
        assert_eq!(row_for_time(100, 1000, &rra, 9, 901), 9);
        assert_eq!(row_for_time(100, 1000, &rra, 9, 900), 8);
        assert_eq!(row_for_time(100, 1000, &rra, 9, 1001), -1);
        assert_eq!(row_for_time(100, 1000, &rra, 9, 0), -1);
    }
}
