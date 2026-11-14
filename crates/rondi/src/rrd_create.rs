//! `rrdtool create`: building a new RRD from definition words, an optional
//! template, a start time and a step, and publishing it atomically.
//!
//! The behavior is specified in docs/compatibility/create.md, whose rule
//! numbers (CR-n) are cited below. Definitions are parsed by
//! [`crate::rrd_definitions`] into a [`NewRrd`], which knows the complete
//! layout of the file before a byte is written. The bytes are then streamed
//! to a temporary file beside the target, or to standard output for `-`, so
//! the size of the archives never has to fit in memory.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::holt_winters::{Role, cdp};
use crate::rrd_binary::{
    CDP_PREP_LEN, DS_DEF_LEN, FLOAT_COOKIE, LIVE_HEAD_LEN, PDP_PREP_LEN, RRA_DEF_LEN, RRA_PTR_LEN,
    STAT_HEAD_LEN, TemplateDefinitions, VALUE_LEN, read_template_definitions, rrd_nan,
    rrd_strerror,
};
use crate::rrd_definitions::{
    CreateArchive, HwArchiveDef, SourceDef, stored_archive_shape, stored_cf, stored_ds_name,
};
use crate::storage::StoreError;

/// Step used when neither `--step` nor a template gives one (CR-12).
const DEFAULT_STEP: u64 = 300;
/// Seconds before now used as the last update by default (CR-11).
const DEFAULT_START_OFFSET: i64 = 10;
/// Values written per `write` call when filling archive rows.
const FILL_VALUES: usize = 8192;
/// Largest file the cache daemon's `CREATE` writes. A Rondi hardening
/// limit with no RRDtool counterpart: a network client must not be able to
/// make the daemon fill its disk. Local `create` has no such limit (CR-71).
pub const DAEMON_CREATE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The options of `create` other than its definition words.
#[derive(Debug, Clone, Copy, Default)]
pub struct CreateOptions<'a> {
    /// `--step`; `None` takes the template's step or 300.
    pub step: Option<u64>,
    /// `--start`; `None` takes the template's last update or ten seconds
    /// before now.
    pub start: Option<i64>,
    /// `--no-overwrite`.
    pub no_overwrite: bool,
    /// `--template`.
    pub template: Option<&'a str>,
}

/// A fully validated new file: everything needed to produce its bytes.
#[derive(Debug)]
struct NewRrd {
    version: &'static [u8; 4],
    step: u64,
    last_update: i64,
    sources: Vec<[u8; DS_DEF_LEN]>,
    archives: Vec<[u8; RRA_DEF_LEN]>,
}

fn cannot_allocate() -> StoreError {
    // Compatibility: a file too large to build fails with this text and
    // writes nothing (CR-70).
    // Provenance: black-box compatibility test.
    StoreError::Rrd("cannot allocate memory".into())
}

impl NewRrd {
    /// Parses `words` after the template's definitions, applying the order
    /// of CR-15 steps 3 to 5.
    fn build(
        step: u64,
        last_update: i64,
        template: Option<TemplateDefinitions>,
        words: &[String],
    ) -> Result<Self, StoreError> {
        let (mut sources, mut archives) = match template {
            Some(template) => (template.ds_defs, template.rra_defs),
            None => (Vec::new(), Vec::new()),
        };
        let mut double_counters = false;
        let mut holt_winters_v4 = false;
        let mut linked: Vec<HwArchiveDef> = Vec::new();
        for word in words {
            if let Some(spec) = word.strip_prefix("DS:") {
                // CP-1: an expression names sources defined before it,
                // template sources included.
                let def = SourceDef::parse(spec, |name| {
                    sources
                        .iter()
                        .position(|record| stored_ds_name(record) == name.as_bytes())
                })?;
                // CR-16: the stored name, before any `=` mapping, must be new.
                if sources
                    .iter()
                    .any(|record| stored_ds_name(record) == def.name().as_bytes())
                {
                    return Err(StoreError::Rrd(format!(
                        "Duplicate DS name: {}",
                        def.name()
                    )));
                }
                double_counters |= def.needs_version_5();
                sources.push(def.to_bytes());
            } else if let Some(spec) = word.strip_prefix("RRA:") {
                for def in CreateArchive::parse(spec, step, archives.len() as u64)? {
                    archives.push(def.to_bytes());
                    if let CreateArchive::HoltWinters(def) = def {
                        holt_winters_v4 |= def.needs_version_4();
                        linked.push(def);
                    }
                }
            } else {
                return Err(StoreError::Rrd(format!("can't parse argument '{word}'")));
            }
        }
        if archives.is_empty() {
            return Err(StoreError::Rrd(
                "you must define at least one Round Robin Archive".into(),
            ));
        }
        if sources.is_empty() {
            return Err(StoreError::Rrd(
                "you must define at least one Data Source".into(),
            ));
        }
        // CR-35: a link names an archive of the finished file.
        for def in &linked {
            def.check_link(archives.len())?;
        }
        let rrd = Self {
            // CR-37: template definitions never raise the version.
            version: if double_counters {
                b"0005"
            } else if holt_winters_v4 {
                b"0004"
            } else {
                b"0003"
            },
            step,
            last_update,
            sources,
            archives,
        };
        rrd.total_len().ok_or_else(cannot_allocate)?;
        Ok(rrd)
    }

    fn header_len(&self) -> Option<u64> {
        let sources = self.sources.len() as u64;
        let archives = self.archives.len() as u64;
        let per_source = (DS_DEF_LEN + PDP_PREP_LEN) as u64;
        let per_archive = (RRA_DEF_LEN + RRA_PTR_LEN) as u64;
        let cdp = sources
            .checked_mul(archives)?
            .checked_mul(CDP_PREP_LEN as u64)?;
        (STAT_HEAD_LEN as u64 + LIVE_HEAD_LEN as u64)
            .checked_add(sources.checked_mul(per_source)?)?
            .checked_add(archives.checked_mul(per_archive)?)?
            .checked_add(cdp)
    }

    /// Number of stored values in all archives.
    fn row_values(&self) -> Option<u64> {
        let sources = self.sources.len() as u64;
        self.archives.iter().try_fold(0_u64, |total, record| {
            let (rows, _) = stored_archive_shape(record);
            total.checked_add(rows.checked_mul(sources)?)
        })
    }

    /// The file length, or `None` when it does not fit in 64 bits.
    fn total_len(&self) -> Option<u64> {
        let rows = self.row_values()?.checked_mul(VALUE_LEN as u64)?;
        let total = self.header_len()?.checked_add(rows)?;
        // A file offset is signed.
        (total <= i64::MAX as u64).then_some(total)
    }

    /// Everything before the first row: header, definitions, last update,
    /// PDP and CDP state and current-row words (CR-38 to CR-46).
    fn head_bytes(&self) -> Result<Vec<u8>, StoreError> {
        let length = self
            .header_len()
            .and_then(|length| usize::try_from(length).ok())
            .ok_or_else(cannot_allocate)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| cannot_allocate())?;
        let unknown = rrd_nan().to_le_bytes();

        bytes.extend_from_slice(b"RRD\0");
        bytes.extend_from_slice(self.version);
        bytes.resize(16, 0);
        bytes.extend_from_slice(&FLOAT_COOKIE.to_le_bytes());
        bytes.extend_from_slice(&(self.sources.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(self.archives.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&self.step.to_le_bytes());
        bytes.resize(STAT_HEAD_LEN, 0);
        for record in &self.sources {
            bytes.extend_from_slice(record);
        }
        for record in &self.archives {
            bytes.extend_from_slice(record);
        }
        bytes.extend_from_slice(&self.last_update.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());

        // CR-42: the remainder takes the signed last update as unsigned,
        // the way the file's own arithmetic does.
        let last_update = self.last_update as u64;
        let unknown_seconds = last_update % self.step;
        for _ in &self.sources {
            let mut pdp = [0_u8; PDP_PREP_LEN];
            pdp[0] = b'U';
            pdp[32..40].copy_from_slice(&unknown_seconds.to_le_bytes());
            pdp[40..48].copy_from_slice(&unknown);
            bytes.extend_from_slice(&pdp);
        }

        // CR-45: the PDPs already in the current slot are unknown.
        let slot_start = last_update - unknown_seconds;
        for record in &self.archives {
            if let Some(role) = Role::from_cf(stored_cf(record)) {
                let state = holt_winters_state(role);
                for _ in &self.sources {
                    bytes.extend_from_slice(&state);
                }
                continue;
            }
            let (_, pdp_per_row) = stored_archive_shape(record);
            let slot = self.step.wrapping_mul(pdp_per_row);
            if slot == 0 {
                // Only a template archive can get here; parsed archives
                // already refuse a zero slot.
                return Err(StoreError::Rrd(
                    "archive slot length is zero or overflows".into(),
                ));
            }
            let unknown_pdps = (slot_start % slot) / self.step;
            for _ in &self.sources {
                let mut cdp = [0_u8; CDP_PREP_LEN];
                cdp[0..8].copy_from_slice(&unknown);
                cdp[8..16].copy_from_slice(&unknown_pdps.to_le_bytes());
                bytes.extend_from_slice(&cdp);
            }
        }
        // Compatibility: RRDtool picks each current row at random; Rondi
        // always starts at row 0, which no reader can tell apart (CR-46).
        // Provenance: black-box compatibility test.
        bytes.resize(bytes.len() + self.archives.len() * RRA_PTR_LEN, 0);
        Ok(bytes)
    }

    /// Writes the whole file: the head, then every row filled with unknown.
    fn write_to(&self, head: &[u8], out: &mut impl Write) -> io::Result<()> {
        out.write_all(head)?;
        let fill: Vec<u8> = std::iter::repeat_n(rrd_nan().to_le_bytes(), FILL_VALUES)
            .flatten()
            .collect();
        let mut remaining = self.row_values().unwrap_or(0);
        while remaining > 0 {
            let values = remaining.min(FILL_VALUES as u64) as usize;
            out.write_all(&fill[..values * VALUE_LEN])?;
            remaining -= values as u64;
        }
        Ok(())
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// The CDP state of a new Holt-Winters archive, for one data source
/// (HW-1, CR-36): whatever the last update, and for template archives too
/// (CR-52).
fn holt_winters_state(role: Role) -> [u8; CDP_PREP_LEN] {
    let unknown = rrd_nan().to_bits();
    let mut words = [0_u64; CDP_PREP_LEN / VALUE_LEN];
    match role {
        Role::Prediction { .. } => {
            for slot in [
                cdp::INTERCEPT,
                cdp::LAST_INTERCEPT,
                cdp::SLOPE,
                cdp::LAST_SLOPE,
            ] {
                words[slot] = unknown;
            }
            words[cdp::NAN_COUNT] = 1;
            words[cdp::LAST_NAN_COUNT] = 1;
        }
        Role::Seasonal | Role::DevSeasonal => {
            words[cdp::SEASONAL] = unknown;
            words[cdp::LAST_SEASONAL] = unknown;
            words[cdp::INIT_FLAG] = 1;
        }
        Role::DevPredict => words[0] = unknown,
        // An all-zero failure history.
        Role::Failures => {}
    }
    let mut state = [0_u8; CDP_PREP_LEN];
    for (chunk, word) in state.as_chunks_mut::<VALUE_LEN>().0.iter_mut().zip(words) {
        *chunk = word.to_le_bytes();
    }
    state
}

/// Reads `--template` (CR-48). Its definitions are copied as stored,
/// `COMPUTE` data sources and Holt-Winters parameters included (CR-52).
fn open_template(name: &str) -> Result<TemplateDefinitions, StoreError> {
    read_template_definitions(Path::new(name))
        .map_err(|_| StoreError::Rrd(format!("Cannot open template RRD {name}")))
}

/// Creates the RRD `path` from `words` (`DS:` and `RRA:` definitions). The
/// name `-` writes the file to standard output.
pub fn create_rrd(
    path: &str,
    options: &CreateOptions<'_>,
    words: &[String],
) -> Result<(), StoreError> {
    // CR-62: the existence check follows symbolic links and comes before
    // the template and the definitions.
    if options.no_overwrite && std::fs::metadata(path).is_ok() {
        return Err(StoreError::Rrd(format!("creating '{path}': File exists")));
    }
    let template = options.template.map(open_template).transpose()?;
    let step = options
        .step
        .or(template.as_ref().map(|template| template.pdp_step))
        .unwrap_or(DEFAULT_STEP);
    if step == 0 {
        // Only a corrupt template stores a zero step; the option parser
        // never yields one.
        return Err(StoreError::Rrd(match options.template {
            Some(name) if options.step.is_none() => format!("Cannot open template RRD {name}"),
            _ => "step size: value must be positive".into(),
        }));
    }
    let last_update = options
        .start
        .or(template.as_ref().map(|template| template.last_up))
        .unwrap_or_else(|| now_seconds() - DEFAULT_START_OFFSET);
    let rrd = NewRrd::build(step, last_update, template, words)?;
    let head = rrd.head_bytes()?;
    if path == "-" {
        let mut out = BufWriter::new(io::stdout().lock());
        rrd.write_to(&head, &mut out)?;
        out.flush()?;
        return Ok(());
    }
    publish_file(path, &rrd, &head, options.no_overwrite)
}

/// Writes the file beside `path` and moves it into place (CR-61 to CR-65).
fn publish_file(
    path: &str,
    rrd: &NewRrd,
    head: &[u8],
    no_overwrite: bool,
) -> Result<(), StoreError> {
    let length = rrd.total_len().ok_or_else(cannot_allocate)?;
    let directory = Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if available_space(directory).is_some_and(|available| length > available) {
        return Err(cannot_allocate());
    }
    let mode = target_mode(path);
    let (temporary, file) = create_temporary(path)
        .ok_or_else(|| StoreError::Rrd("Cannot create temporary file".into()))?;
    let written = write_and_sync(rrd, head, file, mode);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(StoreError::Rrd(format!(
            "writing temporary file: {}",
            rrd_strerror(&error)
        )));
    }
    let published = if no_overwrite {
        publish_exclusive(&temporary, Path::new(path))
    } else {
        std::fs::rename(&temporary, path)
    };
    published.map_err(|_| {
        let _ = std::fs::remove_file(&temporary);
        StoreError::Rrd("Cannot rename temporary file to final file!".into())
    })
}

fn write_and_sync(rrd: &NewRrd, head: &[u8], file: File, mode: u32) -> io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 16, file);
    rrd.write_to(head, &mut out)?;
    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    // After the writes, which would clear set-user-ID and set-group-ID.
    set_mode(&file, mode)?;
    file.sync_all()
}

/// Moves `temporary` to `target` only if nothing is there, without a gap
/// between the check and the move.
// Compatibility: RRDtool checks and renames in two steps and replaces a
// dangling symbolic link; Rondi refuses any existing entry
// (CR-div-noclobber-dangling).
// Provenance: black-box compatibility test.
fn publish_exclusive(temporary: &Path, target: &Path) -> io::Result<()> {
    match std::fs::hard_link(temporary, target) {
        Ok(()) => {
            // The target now names the file; only the temporary name goes.
            let _ = std::fs::remove_file(temporary);
            Ok(())
        }
        Err(error) if hard_links_unsupported(&error) => {
            if std::fs::symlink_metadata(target).is_ok() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            std::fs::rename(temporary, target)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn hard_links_unsupported(error: &io::Error) -> bool {
    use rustix::io::Errno;
    matches!(
        Errno::from_io_error(error),
        Some(Errno::PERM | Errno::NOTSUP | Errno::MLINK)
    )
}

#[cfg(not(unix))]
fn hard_links_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
}

/// A fresh file named after `path` with a random suffix, in the same
/// directory, created exclusively.
fn create_temporary(path: &str) -> Option<(PathBuf, File)> {
    use std::hash::{BuildHasher, Hasher};
    for _ in 0..100 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        let candidate = PathBuf::from(format!("{path}.{:012x}", hasher.finish() >> 16));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Some((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// CR-64: a new file gets 0644 whatever the umask; a replaced one keeps
/// its permission bits.
#[cfg(unix)]
fn target_mode(path: &str) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map_or(0o644, |metadata| metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn target_mode(_path: &str) -> u32 {
    0o644
}

#[cfg(unix)]
fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Bytes an unprivileged writer may still use in `directory`, when known.
#[cfg(unix)]
pub(crate) fn available_space(directory: &Path) -> Option<u64> {
    usable_bytes(&rustix::fs::statvfs(directory).ok()?)
}

#[cfg(unix)]
fn usable_bytes(stats: &rustix::fs::StatVfs) -> Option<u64> {
    stats.f_bavail.checked_mul(stats.f_frsize)
}

#[cfg(not(unix))]
pub(crate) fn available_space(_directory: &Path) -> Option<u64> {
    None
}

/// Creates `name` in the open `directory` for the cache daemon's `CREATE`,
/// from definition words only (no template or source). The file is staged
/// in a private directory beside it, so other accounts never see it before
/// it is complete, and published with a descriptor-relative rename that a
/// swapped path component cannot redirect.
#[cfg(unix)]
pub fn create_rrd_at(
    directory: &File,
    name: &std::ffi::OsStr,
    start: i64,
    step: u64,
    definitions: &[String],
    no_overwrite: bool,
) -> Result<(), StoreError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::os::unix::ffi::OsStrExt;

    if step == 0 {
        return Err(StoreError::Rrd("step size: value must be positive".into()));
    }
    let name = std::ffi::CString::new(name.as_bytes())
        .ok()
        .filter(|name| {
            !matches!(name.to_bytes(), b"" | b"." | b"..") && !name.to_bytes().contains(&b'/')
        })
        .ok_or(rustix::io::Errno::PERM)
        .map_err(io::Error::from)?;
    // CR-62: an existing target is reported before any definition.
    let existing = entry_status(directory, &name);
    if no_overwrite && existing.is_some() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists).into());
    }
    let rrd = NewRrd::build(step, start, None, definitions)?;
    // The length is computed with checked arithmetic and compared before
    // anything is allocated or created.
    if rrd
        .total_len()
        .is_none_or(|length| length > DAEMON_CREATE_MAX_BYTES)
    {
        return Err(StoreError::Rrd(format!(
            "file would exceed the {} MiB CREATE limit",
            DAEMON_CREATE_MAX_BYTES >> 20
        )));
    }
    let head = rrd.head_bytes()?;
    let mode = existing
        .filter(|status| FileType::from_raw_mode(status.st_mode) == FileType::RegularFile)
        .map_or(0o644, |status| permission_bits(status.st_mode));

    let staging = crate::safe_fs::PrivateDirectory::create(directory)?;
    if let Some(available) = fd_available_space(&staging.directory)
        && rrd.total_len().is_some_and(|length| length > available)
    {
        return Err(cannot_allocate());
    }
    let file = crate::safe_fs::open_at(
        &staging.directory,
        c"rrd",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        Mode::from_raw_mode(0o600),
    )?;
    write_and_sync(&rrd, &head, file, mode)?;
    let source = &staging.directory;
    if no_overwrite {
        rustix::fs::linkat(source, c"rrd", directory, name.as_c_str(), AtFlags::empty())
    } else {
        rustix::fs::renameat(source, c"rrd", directory, name.as_c_str())
    }
    .map_err(io::Error::from)?;
    // Dropping the staging directory removes the linked copy, if any.
    drop(staging);
    Ok(())
}

/// `mode_t` is 16 bits on macOS and 32 on Linux.
#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn permission_bits(mode: rustix::fs::RawMode) -> u32 {
    u32::from(mode) & 0o7777
}

/// `fstatat` without following a final symbolic link.
#[cfg(unix)]
fn entry_status(directory: &File, name: &std::ffi::CStr) -> Option<rustix::fs::Stat> {
    rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW).ok()
}

#[cfg(unix)]
fn fd_available_space(directory: &File) -> Option<u64> {
    usable_bytes(&rustix::fs::fstatvfs(directory).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    fn build(list: &[&str]) -> Result<NewRrd, String> {
        NewRrd::build(10, 1_000_000_000, None, &words(list)).map_err(|error| error.to_string())
    }

    #[test]
    fn words_are_checked_in_argument_order() {
        assert_eq!(
            build(&["DS:bad", "RRA:FOO:0.5:1:5"]).unwrap_err(),
            "invalid DS format"
        );
        assert_eq!(
            build(&["RRA:FOO:0.5:1:5", "DS:bad"]).unwrap_err(),
            "Unrecognized consolidation function FOO"
        );
        assert_eq!(
            build(&["DS:x:GAUGE:20:U:U", "foo", "RRA:AVERAGE:0.5:1:5"]).unwrap_err(),
            "can't parse argument 'foo'"
        );
        assert_eq!(
            build(&["DS:x:GAUGE:20:U:U"]).unwrap_err(),
            "you must define at least one Round Robin Archive"
        );
        assert_eq!(
            build(&["RRA:AVERAGE:0.5:1:5"]).unwrap_err(),
            "you must define at least one Data Source"
        );
        assert_eq!(
            build(&[]).unwrap_err(),
            "you must define at least one Round Robin Archive"
        );
    }

    #[test]
    fn duplicate_names_use_the_target_name() {
        let rra = "RRA:AVERAGE:0.5:1:5";
        assert_eq!(
            build(&["DS:x=z:GAUGE:20:U:U", "DS:x=y:GAUGE:20:U:U", rra]).unwrap_err(),
            "Duplicate DS name: x"
        );
        assert!(build(&["DS:x:GAUGE:20:U:U", "DS:y=x:GAUGE:20:U:U", rra]).is_ok());
        assert!(build(&["DS:x=z:GAUGE:20:U:U", "DS:y=z:GAUGE:20:U:U", rra]).is_ok());
    }

    #[test]
    fn version_rises_for_double_counters() {
        let rra = "RRA:AVERAGE:0.5:1:5";
        assert_eq!(build(&["DS:x:GAUGE:20:U:U", rra]).unwrap().version, b"0003");
        assert_eq!(
            build(&["DS:x:DDERIVE:20:U:U", rra]).unwrap().version,
            b"0005"
        );
    }

    #[test]
    fn impossible_sizes_are_refused_before_writing() {
        for rows in ["2305843009213693952", "18446744073709551615"] {
            assert_eq!(
                build(&["DS:x:GAUGE:20:U:U", &format!("RRA:AVERAGE:0.5:1:{rows}")]).unwrap_err(),
                "cannot allocate memory"
            );
        }
        assert_eq!(
            build(&[
                "DS:a:GAUGE:20:U:U",
                "DS:b:GAUGE:20:U:U",
                "RRA:AVERAGE:0.5:1:1152921504606846976"
            ])
            .unwrap_err(),
            "cannot allocate memory"
        );
    }

    /// The head and rows written for a small file read back through the
    /// repository's own reader with the values CR-38 to CR-46 require.
    #[test]
    fn written_file_reads_back() {
        let rrd = NewRrd::build(
            7,
            1_000_000_123,
            None,
            &words(&[
                "DS:a:GAUGE:20:U:5",
                "DS:b:COUNTER:1m:-1:U",
                "RRA:AVERAGE:0.5:13:4",
                "RRA:MAX:0:5:3",
            ]),
        )
        .unwrap();
        let head = rrd.head_bytes().unwrap();
        let mut bytes = Vec::new();
        rrd.write_to(&head, &mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, rrd.total_len().unwrap());
        assert_eq!(bytes.len(), head.len() + (4 + 3) * 2 * 8);

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("t.rrd");
        std::fs::write(&path, &bytes).unwrap();
        let info = crate::inspect_rrd_file(&path).unwrap();
        assert_eq!(info.version, "0003");
        assert_eq!(
            (info.step, info.last_update, info.last_update_usec),
            (7, 1_000_000_123, 0)
        );
        assert_eq!(info.data_sources[0].name, "a");
        assert_eq!(info.data_sources[0].maximum, Some(5.0));
        assert_eq!(info.data_sources[1].kind, "COUNTER");
        assert_eq!(info.data_sources[1].heartbeat, 60);
        assert_eq!(info.data_sources[1].minimum, Some(-1.0));
        for source in &info.data_sources {
            assert_eq!(source.last_value, "U");
            assert_eq!(source.unknown_seconds, 1_000_000_123 % 7);
            assert!(source.pdp_value.is_nan());
        }
        // CR-45's example: 4 for 13 PDPs per row and 0 for 5.
        let unknown: Vec<u64> = info
            .archives
            .iter()
            .map(|archive| archive.cdp_prep[0].unknown_datapoints)
            .collect();
        assert_eq!(unknown, [4, 0]);
        for archive in &info.archives {
            assert_eq!(archive.current_row, 0);
            assert!(archive.cdp_prep.iter().all(|cdp| cdp.value.is_nan()));
        }
        let rows = &bytes[head.len()..];
        assert!(
            rows.chunks(8)
                .all(|value| value == rrd_nan().to_le_bytes().as_slice())
        );
    }

    /// Writes `rrd` to a file in `dir` and reads it back.
    fn written(rrd: &NewRrd, dir: &Path, name: &str) -> crate::RrdInfo {
        let head = rrd.head_bytes().unwrap();
        let mut bytes = Vec::new();
        rrd.write_to(&head, &mut bytes).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, &bytes).unwrap();
        crate::inspect_rrd_file(&path).unwrap()
    }

    fn nan_bits() -> u64 {
        rrd_nan().to_bits()
    }

    /// CR-34 and CR-36: one implicit word becomes five archives with the
    /// documented rows, parameter slots, links and HW-1 state.
    #[test]
    fn implicit_hwpredict_expands_to_five_archives() {
        let rrd = build(&[
            "DS:x:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:5",
            "RRA:HWPREDICT:12:0.1:0.01:4",
        ])
        .unwrap();
        assert_eq!(rrd.version, b"0003");
        let temp = tempfile::tempdir().unwrap();
        let info = written(&rrd, temp.path(), "hw.rrd");
        let shape: Vec<(&str, u64, u64)> = info
            .archives
            .iter()
            .map(|a| (a.consolidation.as_str(), a.rows, a.pdp_per_row))
            .collect();
        assert_eq!(
            shape,
            [
                ("AVERAGE", 5, 1),
                ("HWPREDICT", 12, 1),
                ("SEASONAL", 4, 1),
                ("DEVSEASONAL", 4, 1),
                ("DEVPREDICT", 12, 1),
                ("FAILURES", 4, 1),
            ]
        );
        let (a, b) = (0.1_f64.to_bits(), 0.01_f64.to_bits());
        let two = 2.0_f64.to_bits();
        let params: Vec<[u64; 6]> = info.archives[1..]
            .iter()
            .map(|a| a.param_words[..6].try_into().unwrap())
            .collect();
        assert_eq!(
            params,
            [
                [0, a, b, 2, 4.0_f64.to_bits(), 0],
                [0, a, 0, 1, 0, 0],
                [0, a, 0, 1, 0, 0],
                [0, 0, 0, 3, 0, 0],
                [0, two, two, 3, 9, 7],
            ]
        );
        let n = nan_bits();
        let cdp: Vec<[u64; 10]> = info.archives[1..]
            .iter()
            .map(|a| a.cdp_prep[0].words)
            .collect();
        assert_eq!(
            cdp,
            [
                [0, 0, n, n, n, n, 1, 1, 0, 0],
                [0, 0, n, n, 0, 0, 1, 0, 0, 0],
                [0, 0, n, n, 0, 0, 1, 0, 0, 0],
                [n, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                [0; 10],
            ]
        );
    }

    /// CR-32, CR-36: explicit archives store their own fields, links
    /// 0-based, and `MHWPREDICT` raises the version (CR-37).
    #[test]
    fn explicit_archives_store_their_fields() {
        let rrd = build(&[
            "DS:x:GAUGE:20:U:U",
            "RRA:MHWPREDICT:5m:0.1:0:2m:2",
            "RRA:SEASONAL:5:0.2:1",
            "RRA:DEVPREDICT:20:5",
            "RRA:FAILURES:20:7x:9.9:5",
            "RRA:DEVSEASONAL:1m:0.3:1:smoothing-window=0.4x",
        ])
        .unwrap();
        assert_eq!(rrd.version, b"0004");
        let temp = tempfile::tempdir().unwrap();
        let info = written(&rrd, temp.path(), "x.rrd");
        let rows: Vec<u64> = info.archives.iter().map(|a| a.rows).collect();
        assert_eq!(rows, [30, 5, 20, 20, 6]);
        let params: Vec<[u64; 6]> = info
            .archives
            .iter()
            .map(|a| a.param_words[..6].try_into().unwrap())
            .collect();
        let f = |v: f64| v.to_bits();
        assert_eq!(
            params,
            [
                [0, f(0.1), 0, 1, f(2.0), 0],
                [0, f(0.2), f(0.05), 0, 0, 0],
                [0, 0, 0, 4, 0, 0],
                [0, f(2.0), f(2.0), 4, 9, 7],
                [0, f(0.3), f(0.4), 0, 0, 0],
            ]
        );
    }

    /// Review Focus 2: an explicit season archive without the option gets
    /// 0.05 and leaves the version at 0003; any form of the option raises it.
    #[test]
    fn smoothing_window_default_and_version() {
        let ds = "DS:x:GAUGE:20:U:U";
        let window = |word: &str| {
            let rrd = build(&[ds, word]).unwrap();
            let temp = tempfile::tempdir().unwrap();
            let info = written(&rrd, temp.path(), "s.rrd");
            (rrd.version, info.archives[0].param_words[2])
        };
        let default = 0.05_f64.to_bits();
        assert_eq!(window("RRA:SEASONAL:5:0.1:1"), (b"0003", default));
        assert_eq!(
            window("RRA:SEASONAL:5:0.1:1:smoothing-window"),
            (b"0004", default)
        );
        assert_eq!(
            window("RRA:SEASONAL:5:0.1:1:smoothing-window="),
            (b"0004", default)
        );
        assert_eq!(
            window("RRA:DEVSEASONAL:5:0.1:1:smoothing-window=0"),
            (b"0004", 0)
        );
        assert_eq!(
            window("RRA:SEASONAL:5:0.1:1:smoothing-window=1"),
            (b"0004", 1.0_f64.to_bits())
        );
        assert_eq!(
            window("RRA:SEASONAL:5:0.1:1:smoothing-window=nan"),
            (b"0004", 0x7ff8_0000_0000_0000)
        );
        // CR-32: alpha's NaN spelling is stored with the opposite sign.
        let rrd = build(&[ds, "RRA:HWPREDICT:20:nan:0.01:5"]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let info = written(&rrd, temp.path(), "n.rrd");
        assert_eq!(info.archives[0].param_words[1], (-rrd_nan()).to_bits());
        assert_eq!(info.archives[1].param_words[1], (-rrd_nan()).to_bits());
    }

    /// CR-33 texts and order.
    #[test]
    fn holt_winters_field_errors() {
        let ds = "DS:x:GAUGE:20:U:U";
        let cases = [
            (
                "RRA:HWPREDICT:20:0.1:0.01",
                "Expected at least 5 arguments for RRA but got 4",
            ),
            (
                "RRA:SEASONAL:5:0.1",
                "Expected at least 4 arguments for RRA but got 3",
            ),
            (
                "RRA:DEVPREDICT:20",
                "Expected at least 3 arguments for RRA but got 2",
            ),
            (
                "RRA:FAILURES:20:7:9",
                "Expected at least 5 arguments for RRA but got 4",
            ),
            (
                "RRA:HWPREDICT:0:2:2:99",
                "Invalid row count 0: value must be positive",
            ),
            (
                "RRA:HWPREDICT:20x:0.1:0.01:5",
                "Invalid row count 20x: value has trailing garbage",
            ),
            (
                "RRA:HWPREDICT:20:0:0.01:5",
                "Invalid alpha: must be between 0 and 1",
            ),
            (
                "RRA:HWPREDICT:20:0.5x:0.01:5",
                "Invalid alpha: must be between 0 and 1",
            ),
            (
                "RRA:HWPREDICT:20:inf:0.01:5",
                "Invalid alpha: must be between 0 and 1",
            ),
            (
                "RRA:HWPREDICT:20:0.1:-0.1:5",
                "Invalid beta: must be between 0 and 1",
            ),
            (
                "RRA:HWPREDICT:20:0.1:0.01:30s",
                "Length of seasonal cycle exceeds length of HW prediction array",
            ),
            (
                "RRA:HWPREDICT:20:0.1:0.01:-3",
                "Length of seasonal cycle exceeds length of HW prediction array",
            ),
            (
                "RRA:HWPREDICT:20:2:0.01:5:1:2",
                "Invalid alpha: must be between 0 and 1",
            ),
            (
                "RRA:SEASONAL:x:0.1:1",
                "Invalid row count x: value must be (suffixed) positive number",
            ),
            (
                "RRA:SEASONAL:5:1:1",
                "Invalid gamma: must be between 0 and 1",
            ),
            (
                "RRA:SEASONAL:5:0.2:1:smoothing-window=2",
                "Invalid smoothing-window 2.000000: must be between 0 and 1",
            ),
            (
                "RRA:SEASONAL:5:0.2:1:smoothing-window=-0.1",
                "Invalid smoothing-window -0.100000: must be between 0 and 1",
            ),
            ("RRA:SEASONAL:5:0.2:1:foo=1", "Invalid option foo=1"),
            (
                "RRA:SEASONAL:5:0.2:1:Smoothing-window=1",
                "Invalid option Smoothing-window=1",
            ),
            (
                "RRA:FAILURES:20:10:9:1",
                "Window length is shorter than the failure threshold",
            ),
            (
                "RRA:FAILURES:20:7:29:1",
                "Window length is out of range 1, 28",
            ),
            (
                "RRA:FAILURES:20:0:9:1",
                "Failure threshold is out of range 1, 28",
            ),
            (
                "RRA:FAILURES:20:x:9:1",
                "Failure threshold is out of range 1, 28",
            ),
            (
                "RRA:FAILURES:20:9:0:1",
                "Window length is shorter than the failure threshold",
            ),
        ];
        for (word, message) in cases {
            assert_eq!(build(&[ds, word]).unwrap_err(), message, "{word}");
        }
        // Beta may be 0 or 1; NaN passes every range check.
        for word in [
            "RRA:HWPREDICT:20:0.1:0:5",
            "RRA:HWPREDICT:20:0.1:1:5",
            "RRA:HWPREDICT:20:nanx:0.01:5",
            "RRA:HWPREDICT:20:0.1:0.01:6x",
            "RRA:HWPREDICT:5:0.1:0.01:5",
        ] {
            assert!(build(&[ds, word]).is_ok(), "{word}");
        }
    }

    /// CR-35: the reference's unchecked inputs are rejected.
    #[test]
    fn cr35_defects_rejected() {
        let ds = "DS:x:GAUGE:20:U:U";
        let avg = "RRA:AVERAGE:0.5:1:5";
        let cases: [(&[&str], &str); 9] = [
            (
                &[ds, avg, "RRA:HWPREDICT:20:0.1:0.01:5:0"],
                "Invalid rra-num 0: no such archive",
            ),
            (
                &[ds, avg, "RRA:HWPREDICT:20:0.1:0.01:5:9"],
                "Invalid rra-num 9: no such archive",
            ),
            (
                &[ds, avg, "RRA:SEASONAL:5:0.1:x"],
                "Invalid rra-num x: no such archive",
            ),
            (
                &[ds, "RRA:DEVPREDICT:20:-1", avg],
                "Invalid rra-num -1: no such archive",
            ),
            (
                &[ds, avg, "RRA:HWPREDICT:20:0.1:0.01:0"],
                "Invalid seasonal period 0: must be positive",
            ),
            (
                &[ds, avg, "RRA:HWPREDICT:20:0.1:0.01:5:2:3"],
                "Unexpected extra argument for consolidation function HWPREDICT",
            ),
            (
                &[ds, "RRA:SEASONAL:5:0.1:1:smoothing-window=0.1:x"],
                "Unexpected extra argument for consolidation function SEASONAL",
            ),
            (
                &[ds, "RRA:FAILURES:20:7:9:1:x"],
                "Unexpected extra argument for consolidation function FAILURES",
            ),
            (
                &[ds, "RRA:DEVPREDICT:20:1:x"],
                "Unexpected extra argument for consolidation function DEVPREDICT",
            ),
        ];
        for (list, message) in cases {
            assert_eq!(build(list).unwrap_err(), message, "{list:?}");
        }
        // The link check counts every archive, including later ones.
        assert!(build(&[ds, "RRA:SEASONAL:5:0.1:2", avg]).is_ok());
    }

    /// Review Focus 1 (CR-52): a template's Holt-Winters archives keep their
    /// definition bytes but get fresh HW-1 state, not the learned state.
    #[test]
    fn template_resets_hw_state() {
        let temp = tempfile::tempdir().unwrap();
        let fresh = build(&["DS:x:GAUGE:20:U:U", "RRA:HWPREDICT:20:0.1:0.01:5"]).unwrap();
        let head = fresh.head_bytes().unwrap();
        let mut bytes = Vec::new();
        fresh.write_to(&head, &mut bytes).unwrap();
        // Patch the SEASONAL smoothing index and every CDP word, as a
        // learned file would hold them.
        let defs = 128 + 120;
        bytes[defs + 120 + 72..defs + 120 + 80].copy_from_slice(&3_u64.to_le_bytes());
        let cdp = defs + 5 * 120 + 16 + 112;
        for word in bytes[cdp..cdp + 5 * 80].chunks_mut(8) {
            word.copy_from_slice(&1.5_f64.to_le_bytes());
        }
        let template = temp.path().join("t.rrd");
        std::fs::write(&template, &bytes).unwrap();
        let out = temp.path().join("o.rrd");
        let options = CreateOptions {
            template: Some(template.to_str().unwrap()),
            ..Default::default()
        };
        create_rrd(out.to_str().unwrap(), &options, &[]).unwrap();
        let info = crate::inspect_rrd_file(&out).unwrap();
        let original = crate::inspect_rrd_file(&template).unwrap();
        assert_eq!(info.version, "0003");
        for (made, from) in info.archives.iter().zip(&original.archives) {
            assert_eq!(made.param_words, from.param_words);
        }
        assert_eq!(info.archives[1].param_words[4], 3);
        let pristine = written(&fresh, temp.path(), "p.rrd");
        for (made, want) in info.archives.iter().zip(&pristine.archives) {
            assert_eq!(made.cdp_prep[0].words, want.cdp_prep[0].words);
        }
    }

    fn options() -> CreateOptions<'static> {
        CreateOptions {
            step: Some(10),
            start: Some(1_000_000_000),
            no_overwrite: false,
            template: None,
        }
    }

    /// A row count far beyond any disk fails at once, before a temporary
    /// file is created, instead of streaming terabytes.
    #[test]
    fn local_create_of_a_huge_file_fails_fast() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("huge.rrd");
        let started = std::time::Instant::now();
        let defs = words(&["DS:x:GAUGE:20:U:U", "RRA:AVERAGE:0.5:1:1099511627776"]);
        let error = create_rrd(path.to_str().unwrap(), &options(), &defs).unwrap_err();
        assert_eq!(error.to_string(), "cannot allocate memory");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// The daemon limit does not apply to local `create`.
    #[test]
    fn local_create_is_not_bound_by_the_daemon_size_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("big.rrd");
        let rows = DAEMON_CREATE_MAX_BYTES / 8 + 1;
        let defs = words(&["DS:x:GAUGE:20:U:U", &format!("RRA:AVERAGE:0.5:1:{rows}")]);
        create_rrd(path.to_str().unwrap(), &options(), &defs).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > DAEMON_CREATE_MAX_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_create_is_capped_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let directory = File::open(temp.path()).unwrap();
        let create = |rows: u64| {
            create_rrd_at(
                &directory,
                std::ffi::OsStr::new("d.rrd"),
                1_000_000_000,
                10,
                &words(&["DS:x:GAUGE:20:U:U", &format!("RRA:AVERAGE:0.5:1:{rows}")]),
                true,
            )
        };
        for rows in [DAEMON_CREATE_MAX_BYTES / 8, 1 << 40] {
            assert_eq!(
                create(rows).unwrap_err().to_string(),
                "file would exceed the 64 MiB CREATE limit",
                "{rows}"
            );
        }
        // A size that overflows 64 bits fails while it is computed.
        assert_eq!(
            create(u64::MAX).unwrap_err().to_string(),
            "cannot allocate memory"
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        create(1000).unwrap();
        assert!(temp.path().join("d.rrd").is_file());
    }

    #[test]
    fn create_publishes_atomically_and_honors_no_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("o.rrd");
        let name = path.to_str().unwrap();
        let options = CreateOptions {
            step: Some(10),
            start: Some(1_000_000_000),
            no_overwrite: true,
            template: None,
        };
        let defs = words(&["DS:x:GAUGE:20:U:U", "RRA:AVERAGE:0.5:1:5"]);
        create_rrd(name, &options, &defs).unwrap();
        assert_eq!(
            create_rrd(name, &options, &defs).unwrap_err().to_string(),
            format!("creating '{name}': File exists")
        );
        #[cfg(unix)]
        {
            let dangling = temp.path().join("d.rrd");
            std::os::unix::fs::symlink("nowhere.rrd", &dangling).unwrap();
            assert_eq!(
                create_rrd(dangling.to_str().unwrap(), &options, &defs)
                    .unwrap_err()
                    .to_string(),
                "Cannot rename temporary file to final file!"
            );
        }
        let entries: Vec<_> = std::fs::read_dir(temp.path()).unwrap().collect();
        assert_eq!(entries.len(), if cfg!(unix) { 2 } else { 1 });
    }
}
