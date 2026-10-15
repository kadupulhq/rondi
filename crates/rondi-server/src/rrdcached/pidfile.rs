//! The `-p` pid file, removed again when its guard is dropped.

use super::error_text;
use super::listener::{identity_of, stat_at};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The fatal line for a pid file that could not be created or opened.
const OPEN_FAILED: &str = "FATAL: Fail to create/open PID file ";

/// Holds the pid file this process wrote; dropping it removes the entry,
/// through its directory's descriptor, if it is still the same inode.
#[derive(Debug)]
pub(crate) struct PidFile {
    directory: File,
    name: CString,
    identity: (u64, u64),
    // Held open so the inode cannot be freed and its number reused by a
    // file that replaces this one.
    _file: File,
}

/// A pid file that stops startup, with the `FATAL` line to print last.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PidFileError(pub(crate) String);

impl PidFile {
    /// Writes this process's pid to `path`. Diagnostics that do not stop
    /// startup, and those that precede a failure, go to `notes`.
    // Compatibility: the message sequence of RCD-10 and RCD-11 (stale,
    // corrupted, empty and live pid files, missing directories).
    // Provenance: rrdcached(1) and black-box compatibility test.
    pub(crate) fn create(path: &Path, notes: &mut dyn Write) -> Result<Self, PidFileError> {
        if let Some(directory) = path.parent().filter(|dir| !dir.as_os_str().is_empty())
            && !directory.exists()
            && let Err(error) = std::fs::DirBuilder::new().recursive(true).create(directory)
        {
            // Compatibility: the reference reports this twice.
            // Provenance: black-box compatibility test.
            for _ in 0..2 {
                let _ = writeln!(
                    notes,
                    "Failed to create pidfile directory '{}': {}",
                    directory.display(),
                    error_text(&error)
                );
            }
            return Err(PidFileError(OPEN_FAILED.to_owned()));
        }
        let pid = std::process::id();
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path);
        let file = match created {
            Ok(mut file) => {
                write_pid(&mut file, pid).map_err(|_| PidFileError(OPEN_FAILED.to_owned()))?;
                file
            }
            Err(error) => {
                let _ = writeln!(
                    notes,
                    "rrdcached: can't create pid file '{}' ({})",
                    path.display(),
                    error_text(&error)
                );
                // Compatibility: whatever the reason, the existing file is
                // then opened (`-p ''` reports both failures).
                // Provenance: black-box compatibility test.
                replace_stale(path, pid, notes)?
            }
        };
        let metadata = file
            .metadata()
            .map_err(|_| PidFileError(OPEN_FAILED.to_owned()))?;
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .and_then(|name| CString::new(name.as_encoded_bytes()).ok());
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(directory);
        let (Ok(directory), Some(name)) = (directory, name) else {
            return Err(PidFileError(OPEN_FAILED.to_owned()));
        };
        Ok(Self {
            directory,
            name,
            identity: (metadata.dev(), metadata.ino()),
            _file: file,
        })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let ours = stat_at(&self.directory, &self.name)
            .is_ok_and(|found| identity_of(&found) == self.identity);
        if ours {
            // SAFETY: the descriptor is open and the name NUL terminated;
            // unlinkat removes the entry and never follows a link.
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

fn write_pid(file: &mut File, pid: u32) -> std::io::Result<()> {
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()
}

/// Checks an existing pid file and, if no process holds that pid, puts a
/// new file in its place.
// Rondi policy: the existing file is opened without following a symbolic
// link and must be a regular file with one link (owned by root when the
// daemon is root); the new file replaces it by rename, so the old name is
// never truncated in place (RCD-11).
fn replace_stale(path: &Path, pid: u32, notes: &mut dyn Write) -> Result<File, PidFileError> {
    let cannot_open = |notes: &mut dyn Write, reason: String| {
        let _ = writeln!(
            notes,
            "rrdcached: can't open pid file '{}' ({reason})",
            path.display()
        );
        PidFileError(OPEN_FAILED.to_owned())
    };
    let mut existing = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| cannot_open(notes, error_text(&error)))?;
    let metadata = existing
        .metadata()
        .map_err(|error| cannot_open(notes, error_text(&error)))?;
    if !metadata.is_file() {
        return Err(cannot_open(notes, "not a regular file".to_owned()));
    }
    if metadata.nlink() > 1 {
        return Err(cannot_open(notes, format!("{} links", metadata.nlink())));
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 && metadata.uid() != 0 {
        return Err(cannot_open(
            notes,
            format!("owned by uid {}", metadata.uid()),
        ));
    }
    let mut contents = Vec::new();
    (&mut existing)
        .take(4096)
        .read_to_end(&mut contents)
        .map_err(|error| cannot_open(notes, error_text(&error)))?;
    if contents.is_empty() {
        return Err(PidFileError("FATAL: Empty PID file exist".to_owned()));
    }
    let Some(old) = parse_pid(&contents) else {
        return Err(PidFileError("FATAL: PID file is corrupted".to_owned()));
    };
    if process_exists(old) {
        return Err(PidFileError(format!(
            "FATAL: Another rrdcached daemon is running?? (pid {old})"
        )));
    }
    let _ = writeln!(
        notes,
        "rrdcached: removed stale PID file (no rrdcached on pid {old})"
    );
    let replacement =
        write_beside(path, pid).map_err(|error| cannot_open(notes, error_text(&error)))?;
    let _ = writeln!(notes, "rrdcached: starting normally.");
    Ok(replacement)
}

/// Writes a fresh pid file next to `path` and renames it over `path`.
fn write_beside(path: &Path, pid: u32) -> std::io::Result<File> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EISDIR))?;
    let directory = path.parent().unwrap_or(Path::new("."));
    for attempt in 0..100u32 {
        let mut staged_name = std::ffi::OsString::from(".");
        staged_name.push(name);
        staged_name.push(format!(".{pid}.{attempt}"));
        let staged = directory.join(staged_name);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&staged)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let renamed = write_pid(&mut file, pid).and_then(|()| std::fs::rename(&staged, path));
        if let Err(error) = renamed {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(file);
    }
    Err(std::io::Error::from_raw_os_error(libc::EEXIST))
}

/// The pid an existing file names.
// Compatibility: leading white space and a `+` are allowed, the number is
// decimal and positive, fits a pid, and may be followed only by one LF.
// Provenance: black-box compatibility test.
fn parse_pid(contents: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(contents).ok()?;
    let text = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let unsigned = text.strip_prefix('+').unwrap_or(text);
    let digits = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || !matches!(&unsigned[digits..], "" | "\n") {
        return None;
    }
    unsigned[..digits]
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
}

/// Whether `kill(pid, 0)` succeeds.
// Compatibility: a pid this user may not signal counts as stale, as in
// the reference (pid 1 started as an ordinary user).
// Provenance: black-box compatibility test.
fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence and permission.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(path: &Path) -> (Result<PidFile, PidFileError>, String) {
        let mut notes = Vec::new();
        let result = PidFile::create(path, &mut notes);
        (result, String::from_utf8(notes).unwrap())
    }

    #[test]
    fn pid_contents_follow_the_reference_grammar() {
        for (text, pid) in [
            ("42\n", Some(42)),
            ("42", Some(42)),
            ("  42\n", Some(42)),
            ("\n42\n", Some(42)),
            ("+42\n", Some(42)),
            ("0042\n", Some(42)),
            ("0\n", None),
            ("-5\n", None),
            ("12x\n", None),
            ("42 7\n", None),
            ("42\n13\n", None),
            ("0x10\n", None),
            (" \n", None),
            ("2147483648\n", None),
            ("99999999999999999999\n", None),
        ] {
            assert_eq!(parse_pid(text.as_bytes()), pid, "{text:?}");
        }
    }

    #[test]
    fn new_pid_file_is_written_and_removed_by_its_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/sub/pid");
        let (guard, notes) = create(&path);
        let guard = guard.unwrap();
        assert_eq!(notes, "");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn guard_leaves_a_file_that_replaced_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pid");
        let guard = create(&path).0.unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "1\n").unwrap();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n");
    }

    #[test]
    fn existing_pid_files_are_judged_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pid");
        let created = format!(
            "rrdcached: can't create pid file '{}' (File exists)\n",
            path.display()
        );
        for (contents, fatal) in [
            ("", "FATAL: Empty PID file exist".to_owned()),
            ("garbage", "FATAL: PID file is corrupted".to_owned()),
            (
                &*format!("{}\n", std::process::id()),
                format!(
                    "FATAL: Another rrdcached daemon is running?? (pid {})",
                    std::process::id()
                ),
            ),
        ] {
            std::fs::write(&path, contents).unwrap();
            let (result, notes) = create(&path);
            assert_eq!(result.unwrap_err(), PidFileError(fatal));
            assert_eq!(notes, created);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        }
        std::fs::write(&path, "2147483647\n").unwrap();
        let (guard, notes) = create(&path);
        assert!(guard.is_ok());
        assert_eq!(
            notes,
            format!(
                "{created}rrdcached: removed stale PID file (no rrdcached on pid 2147483647)\nrrdcached: starting normally.\n"
            )
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{}\n", std::process::id())
        );
        // Only the pid file is left; the staged copy was renamed into place.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn linked_pid_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "2147483647\n").unwrap();
        let hard = dir.path().join("hard.pid");
        std::fs::hard_link(&victim, &hard).unwrap();
        let (result, notes) = create(&hard);
        assert_eq!(result.unwrap_err(), PidFileError(OPEN_FAILED.to_owned()));
        assert!(notes.ends_with(&format!(
            "rrdcached: can't open pid file '{}' (2 links)\n",
            hard.display()
        )));
        let soft = dir.path().join("soft.pid");
        std::os::unix::fs::symlink(&victim, &soft).unwrap();
        assert!(create(&soft).0.is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "2147483647\n");
    }

    #[test]
    fn an_empty_name_fails_to_create_and_to_open() {
        let (result, notes) = create(Path::new(""));
        assert_eq!(result.unwrap_err(), PidFileError(OPEN_FAILED.to_owned()));
        assert_eq!(
            notes,
            "rrdcached: can't create pid file '' (No such file or directory)\n\
             rrdcached: can't open pid file '' (No such file or directory)\n"
        );
    }

    #[test]
    fn unusable_pid_directories_are_reported_twice() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let (result, notes) = create(&file.join("sub/pid"));
        assert_eq!(result.unwrap_err(), PidFileError(OPEN_FAILED.to_owned()));
        let line = format!(
            "Failed to create pidfile directory '{}': Not a directory\n",
            file.join("sub").display()
        );
        assert_eq!(notes, format!("{line}{line}"));
    }
}
