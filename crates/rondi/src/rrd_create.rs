//! Port of RRDtool 1.11.0 `rrd_create.c`: `rrd_create_r2`, `parseDS`,
//! `parseGENERIC_DS`, `parseRRA`, `rrd_init_data` and `write_rrd`, for the
//! DS types and consolidation functions Rondi updates. COMPUTE data sources
//! and the Holt-Winters CFs are refused.

use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN,
    STAT_HEAD_LEN, VALUE_LEN, read_template_definitions, rrd_nan, rrd_scaled_duration,
};
use crate::rrd_number::rrd_strtodbl;
use crate::storage::StoreError;
use std::io::Write;
use std::path::Path;

const DS_NAM_SIZE: usize = 20;
const DST_SIZE: usize = 20;

/// The C library's single error slot: `rrd_set_error` replaces the text and
/// `rrd_test_error` asks whether any is set.
#[derive(Default)]
struct ErrorSlot(Option<String>);

impl ErrorSlot {
    fn set(&mut self, message: impl Into<String>) {
        self.0 = Some(message.into());
    }

    fn is_set(&self) -> bool {
        self.0.is_some()
    }
}

/// `rrd_create_r2` without `--source`. `last_up` is -1 when no start time was
/// given and `pdp_step` 0 when no step was given, as rrd_create.c passes them.
pub fn rrd_create_r2(
    filename: &str,
    pdp_step: u64,
    last_up: i64,
    no_overwrite: bool,
    template: Option<&str>,
    argv: &[String],
) -> Result<(), StoreError> {
    if !cfg!(target_pointer_width = "64") || cfg!(target_endian = "big") {
        return Err(StoreError::RrdUnsupported(
            "RRD creation currently requires a 64-bit little-endian target".into(),
        ));
    }
    let mut error = ErrorSlot::default();
    if no_overwrite && std::fs::metadata(filename).is_ok() {
        return Err(StoreError::Rrd(format!(
            "creating '{filename}': File exists"
        )));
    }
    let mut version = "0003";
    let mut step = pdp_step;
    let last_up_set = last_up > 0;
    let mut live_last_up = if last_up_set {
        last_up
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64)
            - 10
    };
    let mut ds_defs: Vec<[u8; DS_DEF_LEN]> = Vec::new();
    let mut rra_defs: Vec<[u8; RRA_DEF_LEN]> = Vec::new();
    let mut template_latest_last_up = 0;
    if let Some(template) = template {
        let definitions = read_template_definitions(Path::new(template))
            .map_err(|_| StoreError::Rrd(format!("Cannot open template RRD {template}")))?;
        if step == 0 {
            step = definitions.pdp_step;
        }
        ds_defs = definitions.ds_defs;
        rra_defs = definitions.rra_defs;
        template_latest_last_up = definitions.last_up;
    }
    if step == 0 {
        step = 300;
    }

    for arg in argv {
        if let Some(def) = arg.strip_prefix("DS:") {
            let mut ds_def = [0_u8; DS_DEF_LEN];
            parse_ds(def, &mut ds_def, &mut version, &mut error);
            let name = c_string(&ds_def[..DS_NAM_SIZE]);
            if ds_defs
                .iter()
                .any(|existing| c_string(&existing[..DS_NAM_SIZE]) == name)
            {
                error.set(format!(
                    "Duplicate DS name: {}",
                    String::from_utf8_lossy(name)
                ));
            }
            ds_defs.push(ds_def);
            if let Some(message) = error.0.take() {
                return Err(StoreError::Rrd(message));
            }
        } else if arg.starts_with("RRA:") {
            let mut rra_def = [0_u8; RRA_DEF_LEN];
            parse_rra(arg, &mut rra_def, step, &mut error);
            if let Some(message) = error.0.take() {
                return Err(StoreError::Rrd(message));
            }
            rra_defs.push(rra_def);
        } else {
            return Err(StoreError::Rrd(format!("can't parse argument '{arg}'")));
        }
    }
    if rra_defs.is_empty() {
        return Err(StoreError::Rrd(
            "you must define at least one Round Robin Archive".into(),
        ));
    }
    if ds_defs.is_empty() {
        return Err(StoreError::Rrd(
            "you must define at least one Data Source".into(),
        ));
    }
    if !last_up_set && template_latest_last_up > 0 {
        live_last_up = template_latest_last_up;
    }
    let bytes = rrd_init_data(version, step, live_last_up, &ds_defs, &rra_defs)?;
    write_rrd(filename, &bytes, no_overwrite)
}

/// `DS_RE` from rrd_create.c:310, matched the way GRegex does:
/// `^([-a-zA-Z0-9_]{1,19})(?:=([-a-zA-Z0-9_]{1,19})(?:\[([0-9]+)\])?)?:([A-Z]{1,19}):(.+)$`.
/// Returns the name, the type and the type arguments.
fn match_ds_re(def: &str) -> Option<(&str, &str, &str)> {
    let is_name = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    let bytes = def.as_bytes();
    let run = |from: usize, accept: &dyn Fn(u8) -> bool| {
        bytes[from..]
            .iter()
            .take_while(|byte| accept(**byte))
            .count()
    };
    let name_len = run(0, &is_name);
    if !(1..=19).contains(&name_len) {
        return None;
    }
    let mut position = name_len;
    if bytes.get(position) == Some(&b'=') {
        let mapped = run(position + 1, &is_name);
        if !(1..=19).contains(&mapped) {
            return None;
        }
        position += 1 + mapped;
        if bytes.get(position) == Some(&b'[') {
            let digits = run(position + 1, &|byte| byte.is_ascii_digit());
            if digits == 0 || bytes.get(position + 1 + digits) != Some(&b']') {
                return None;
            }
            position += digits + 2;
        }
    }
    if bytes.get(position) != Some(&b':') {
        return None;
    }
    let dst_start = position + 1;
    let dst_len = run(dst_start, &|byte| byte.is_ascii_uppercase());
    if !(1..=19).contains(&dst_len) || bytes.get(dst_start + dst_len) != Some(&b':') {
        return None;
    }
    let args = &def[dst_start + dst_len + 1..];
    // `.` stops at a newline and `$` also matches before a final newline.
    let args = args.strip_suffix('\n').unwrap_or(args);
    if args.is_empty() || args.contains('\n') {
        return None;
    }
    Some((&def[..name_len], &def[dst_start..dst_start + dst_len], args))
}

/// rrd_create.c:328 `parseDS`.
fn parse_ds(def: &str, ds_def: &mut [u8; DS_DEF_LEN], version: &mut &str, error: &mut ErrorSlot) {
    let Some((name, dst, dst_args)) = match_ds_re(def) else {
        error.set("invalid DS format");
        return;
    };
    copy_c_string(&mut ds_def[..DS_NAM_SIZE], name);
    let known = matches!(
        dst,
        "COUNTER" | "ABSOLUTE" | "GAUGE" | "DERIVE" | "COMPUTE" | "DCOUNTER" | "DDERIVE"
    );
    if !known {
        // dst_conv reports the unknown name before parseDS replaces it.
        error.set(format!("unknown data acquisition function '{dst}'"));
    }
    if matches!(dst, "DCOUNTER" | "DDERIVE") && *version < "0005" {
        *version = "0005";
    }
    match dst {
        "COUNTER" | "ABSOLUTE" | "GAUGE" | "DERIVE" | "DCOUNTER" | "DDERIVE" => {
            copy_c_string(&mut ds_def[DS_NAM_SIZE..DS_NAM_SIZE + DST_SIZE], dst);
            parse_generic_ds(dst_args, ds_def, error);
        }
        "COMPUTE" => error.set("COMPUTE data sources are not supported by Rondi"),
        _ => error.set(format!("invalid DS type specified ({dst})")),
    }
}

/// rrd_create.c:1098 `parseGENERIC_DS`.
fn parse_generic_ds(def: &str, ds_def: &mut [u8; DS_DEF_LEN], error: &mut ErrorSlot) {
    let failure = (|| -> Result<(), &'static str> {
        let colon = def.find(':').ok_or("missing separator")?;
        if colon >= 32 {
            return Err("heartbeat too long");
        }
        let heartbeat = rrd_scaled_duration(&def[..colon], 1)?;
        put_u64(ds_def, 40, heartbeat);
        let Some((min, max)) = scan_min_max(&def[colon + 1..]) else {
            return Err("failed to extract min:max");
        };
        let mut parse_bound = |text: &str, offset: usize, context: &str| {
            let value = if text == "U" {
                rrd_nan()
            } else {
                match rrd_strtodbl(text, context) {
                    Ok(value) => value,
                    Err(message) => {
                        error.set(message);
                        return None;
                    }
                }
            };
            put_u64(ds_def, offset, value.to_bits());
            Some(value)
        };
        let Some(min) = parse_bound(min, 48, "parsing min val") else {
            return Ok(());
        };
        let Some(max) = parse_bound(max, 56, "parsing max val") else {
            return Ok(());
        };
        if min >= max {
            return Err("min must be less than max in DS definition");
        }
        Ok(())
    })();
    if let Err(reason) = failure {
        error.set(format!("failed to parse data source {def}: {reason}"));
    }
}

/// `sscanf(text, "%18[^:]:%18[^:]", min, max) == 2`.
fn scan_min_max(text: &str) -> Option<(&str, &str)> {
    let field = |from: usize| {
        let length = text.as_bytes()[from..]
            .iter()
            .take(18)
            .take_while(|byte| **byte != b':')
            .count();
        (length > 0).then(|| (&text[from..from + length], from + length))
    };
    let (min, end) = field(0)?;
    if text.as_bytes().get(end) != Some(&b':') {
        return None;
    }
    let (max, _) = field(end + 1)?;
    Some((min, max))
}

/// rrd_create.c:480 `parseRRA` for AVERAGE, MIN, MAX and LAST.
fn parse_rra(def: &str, rra_def: &mut [u8; RRA_DEF_LEN], pdp_step: u64, error: &mut ErrorSlot) {
    let mut cf_nam = String::new();
    let mut pdp_cnt = 0_u64;
    let mut token_idx = 0;
    let token_min = 4;
    // strtok_r skips empty fields, so "::" and a trailing ":" vanish.
    for token in def[4..].split(':').filter(|token| !token.is_empty()) {
        match token_idx {
            0 => {
                // sscanf(token, "%19[A-Z]", cf_nam)
                cf_nam = token
                    .bytes()
                    .take(19)
                    .take_while(u8::is_ascii_uppercase)
                    .map(char::from)
                    .collect();
                if cf_nam.is_empty() {
                    error.set("Failed to parse CF name");
                }
                copy_c_string(&mut rra_def[..20], &cf_nam);
                match cf_nam.as_str() {
                    "AVERAGE" | "MIN" | "MAX" | "LAST" => {}
                    "HWPREDICT" | "MHWPREDICT" | "DEVPREDICT" | "SEASONAL" | "DEVSEASONAL"
                    | "FAILURES" => error.set(format!(
                        "Holt-Winters consolidation function {cf_nam} is not supported by Rondi"
                    )),
                    _ => error.set(format!("Unrecognized consolidation function {cf_nam}")),
                }
                pdp_cnt = 1;
                put_u64(rra_def, 32, pdp_cnt);
            }
            1 => match crate::parse_rrd_number(token) {
                Some(xff) if !(xff < 0.0 || xff >= 1.0) => put_u64(rra_def, 40, xff.to_bits()),
                parsed => {
                    error.set("Invalid xff: must be between 0 and 1");
                    put_u64(rra_def, 40, parsed.unwrap_or(0.0).to_bits());
                }
            },
            2 => match rrd_scaled_duration(token, pdp_step) {
                Ok(value) => {
                    pdp_cnt = value;
                    put_u64(rra_def, 32, pdp_cnt);
                }
                Err(reason) => error.set(format!("Invalid step {token}: {reason}")),
            },
            3 => match rrd_scaled_duration(token, pdp_step.wrapping_mul(pdp_cnt)) {
                Ok(rows) => put_u64(rra_def, 24, rows),
                Err(reason) => error.set(format!("Invalid row count {token}: {reason}")),
            },
            4 => error.set(format!(
                "Unexpected extra argument for consolidation function {cf_nam}"
            )),
            _ => error.set("Unknown error"),
        }
        if error.is_set() {
            return;
        }
        token_idx += 1;
    }
    if token_idx < token_min {
        error.set(format!(
            "Expected at least {token_min} arguments for RRA but got {token_idx}"
        ));
    }
}

/// rrd_create.c:1310 `rrd_init_data` followed by `write_fh`'s layout.
/// Rondi starts every archive at row 0; RRDtool picks a random row, and
/// every row of a new archive is unknown either way.
fn rrd_init_data(
    version: &str,
    pdp_step: u64,
    last_up: i64,
    ds_defs: &[[u8; DS_DEF_LEN]],
    rra_defs: &[[u8; RRA_DEF_LEN]],
) -> Result<Vec<u8>, StoreError> {
    let ds_cnt = ds_defs.len();
    let rra_cnt = rra_defs.len();
    let row_counts = rra_defs
        .iter()
        .map(|def| u64_at(def, 24))
        .collect::<Vec<_>>();
    let values = row_counts
        .iter()
        .try_fold(0_u64, |total, rows| total.checked_add(*rows))
        .and_then(|rows| rows.checked_mul(ds_cnt as u64))
        .and_then(|values| usize::try_from(values).ok())
        .and_then(|values| values.checked_mul(VALUE_LEN))
        .ok_or_else(|| StoreError::Rrd("cannot allocate memory".into()))?;
    let header_len = STAT_HEAD_LEN
        + ds_cnt * DS_DEF_LEN
        + rra_cnt * RRA_DEF_LEN
        + LIVE_HEAD_LEN
        + ds_cnt * PDP_PREP_LEN
        + ds_cnt * rra_cnt * CDP_PREP_LEN
        + rra_cnt * RRA_PTR_LEN;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(header_len + values)
        .map_err(|_| StoreError::Rrd("cannot allocate memory".into()))?;
    bytes.resize(STAT_HEAD_LEN, 0);
    bytes[0..4].copy_from_slice(b"RRD\0");
    bytes[4..8].copy_from_slice(version.as_bytes());
    put_u64(&mut bytes, 16, FLOAT_COOKIE.to_bits());
    put_u64(&mut bytes, 24, ds_cnt as u64);
    put_u64(&mut bytes, 32, rra_cnt as u64);
    put_u64(&mut bytes, 40, pdp_step);
    for def in ds_defs {
        bytes.extend_from_slice(def);
    }
    for def in rra_defs {
        bytes.extend_from_slice(def);
    }
    bytes.extend_from_slice(&last_up.to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    // reset_pdp_prep (rrd_create.c:1290); time_t % unsigned long is unsigned.
    let unknown_seconds = (last_up as u64) % pdp_step;
    for _ in 0..ds_cnt {
        let offset = bytes.len();
        bytes.resize(offset + PDP_PREP_LEN, 0);
        bytes[offset] = b'U';
        put_u64(&mut bytes, offset + 32, unknown_seconds);
        put_u64(&mut bytes, offset + 40, rrd_nan().to_bits());
    }
    // init_cdp (rrd_create.c:1250).
    for def in rra_defs {
        let period = pdp_step.wrapping_mul(u64_at(def, 32));
        if period == 0 {
            return Err(StoreError::Rrd("RRA period is zero".into()));
        }
        let unknown_pdps = ((last_up as u64).wrapping_sub(unknown_seconds) % period) / pdp_step;
        for _ in 0..ds_cnt {
            let offset = bytes.len();
            bytes.resize(offset + CDP_PREP_LEN, 0);
            put_u64(&mut bytes, offset, rrd_nan().to_bits());
            put_u64(&mut bytes, offset + 8, unknown_pdps);
        }
    }
    bytes.resize(bytes.len() + rra_cnt * RRA_PTR_LEN, 0);
    let unknown = rrd_nan().to_le_bytes();
    for _ in 0..values / VALUE_LEN {
        bytes.extend_from_slice(&unknown);
    }
    Ok(bytes)
}

/// rrd_create.c:1406 `write_rrd`: "-" goes to stdout; anything else is
/// written to `<name>XXXXXX` beside the target, given the target's mode or
/// 0644 regardless of umask, and renamed over it.
fn write_rrd(filename: &str, bytes: &[u8], no_overwrite: bool) -> Result<(), StoreError> {
    if filename == "-" {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()?;
        return Ok(());
    }
    let (temp_path, mut file) =
        mkstemp(filename).map_err(|_| StoreError::Rrd("Cannot create temporary file".into()))?;
    let result = (|| -> Result<(), StoreError> {
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        let mode = std::fs::metadata(filename)
            .map(|metadata| metadata.permissions())
            .unwrap_or_else(|_| default_permissions());
        std::fs::set_permissions(&temp_path, mode)
            .map_err(|_| StoreError::Rrd("Cannot chmod temporary file!".into()))?;
        // A hard link keeps --no-overwrite from replacing a file created
        // after the stat above; RRDtool's rename would replace it.
        let renamed = if no_overwrite {
            std::fs::hard_link(&temp_path, filename)
        } else {
            std::fs::rename(&temp_path, filename)
        };
        renamed.map_err(|_| StoreError::Rrd("Cannot rename temporary file to final file!".into()))
    })();
    let _ = std::fs::remove_file(&temp_path);
    result
}

#[cfg(unix)]
fn default_permissions() -> std::fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    std::fs::Permissions::from_mode(0o644)
}

#[cfg(not(unix))]
fn default_permissions() -> std::fs::Permissions {
    std::fs::File::open(".")
        .and_then(|file| file.metadata())
        .map(|metadata| metadata.permissions())
        .unwrap_or_else(|_| unreachable!("current directory metadata"))
}

/// `mkstemp("<filename>XXXXXX")`: mode 0600, created exclusively.
fn mkstemp(filename: &str) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
        ^ u64::from(std::process::id()).rotate_left(32);
    let mut last_error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    for _ in 0..100 {
        let mut value = seed
            .wrapping_add(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let suffix = (0..6)
            .map(|_| {
                let letter = LETTERS[(value % LETTERS.len() as u64) as usize];
                value /= LETTERS.len() as u64;
                char::from(letter)
            })
            .collect::<String>();
        let path = std::path::PathBuf::from(format!("{filename}{suffix}"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = error;
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

fn c_string(field: &[u8]) -> &[u8] {
    &field[..field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len())]
}

/// `strncpy` into a zeroed field of which the last byte stays NUL.
fn copy_c_string(field: &mut [u8], value: &str) {
    let count = value.len().min(field.len() - 1);
    field[..count].copy_from_slice(&value.as_bytes()[..count]);
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::{match_ds_re, scan_min_max};

    #[test]
    fn ds_regex_matches_like_gregex() {
        assert_eq!(
            match_ds_re("x:GAUGE:20:U:U"),
            Some(("x", "GAUGE", "20:U:U"))
        );
        assert_eq!(match_ds_re("x=y[2]:GAUGE:1"), Some(("x", "GAUGE", "1")));
        assert_eq!(match_ds_re("x=y:GAUGE:1\n"), Some(("x", "GAUGE", "1")));
        assert_eq!(match_ds_re("x.y:GAUGE:1"), None);
        assert_eq!(match_ds_re("x:GAUGE:"), None);
        assert_eq!(match_ds_re("x:Gauge:1"), None);
        assert_eq!(match_ds_re("abcdefghijklmnopqrst:GAUGE:1"), None);
        assert_eq!(match_ds_re("x=y[]:GAUGE:1"), None);
    }

    #[test]
    fn min_max_scan_stops_at_eighteen_bytes() {
        assert_eq!(scan_min_max("U:U:extra"), Some(("U", "U")));
        assert_eq!(scan_min_max("U"), None);
        assert_eq!(scan_min_max(":U"), None);
        assert_eq!(scan_min_max("1234567890123456789:U"), None);
        assert_eq!(
            scan_min_max("1:1234567890123456789"),
            Some(("1", "123456789012345678"))
        );
    }
}
