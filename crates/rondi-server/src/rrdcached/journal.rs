//! The `-j` journal: files named `rrd.journal.<seconds>.<microseconds>`
//! holding one request per line (RCD-30 to RCD-32).
//!
//! Lines are kept in a buffer of the file's block size and written in
//! whole blocks, so a daemon that is killed leaves the same prefix on disk
//! as the reference's buffered stream. The buffer is written out when a
//! file is closed: at rotation and at shutdown.

use super::files::{open_at, stat_at};
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const PREFIX: &[u8] = b"rrd.journal";

/// A replayed line longer than this is skipped, like an overlong request.
const REPLAY_LINE_MAX: usize = 2 * 4096;

/// Group and other write permission.
const SHARED_WRITE: u32 = 0o022;

/// The sticky bit.
const STICKY: u32 = 0o1000;

pub(crate) struct Journal {
    directory: File,
    path: PathBuf,
    /// Files by generation: replayed files and the first file are
    /// generation 0, each rotation starts the next.
    files: Vec<(CString, u64)>,
    generation: u64,
    current: Option<File>,
    buffer: Vec<u8>,
    block: usize,
    /// Bytes handed to the journal since startup (`JournalBytes`).
    pub(crate) bytes: u64,
    /// Rotations since startup (`JournalRotate`).
    pub(crate) rotations: u64,
}

impl Journal {
    /// Opens the journal directory and lists the files to replay.
    // Rondi policy (RCD-30): a directory writable by group or others is
    // refused unless it is sticky and owned by the daemon user, so another
    // account cannot plant or swap journal files.
    pub(crate) fn open(path: &Path) -> io::Result<(Self, Vec<CString>)> {
        let directory = super::files::open_directory_path(path)?;
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions.
        let user = unsafe { libc::geteuid() };
        let shared = metadata.mode() & SHARED_WRITE != 0;
        let sticky = metadata.mode() & STICKY != 0;
        if shared && !(sticky && metadata.uid() == user) {
            return Err(io::Error::other(format!(
                "journal directory {} is writable by other users",
                path.display()
            )));
        }
        let mut journal = Self {
            directory,
            path: path.to_path_buf(),
            files: Vec::new(),
            generation: 0,
            current: None,
            buffer: Vec::new(),
            block: 4096,
            bytes: 0,
            rotations: 0,
        };
        let replay = journal.existing()?;
        for name in &replay {
            journal.files.push((name.clone(), 0));
        }
        Ok((journal, replay))
    }

    /// The journal files present, in name order. A file named exactly
    /// `rrd.journal` is renamed to `rrd.journal.0001` first (RCD-31).
    fn existing(&self) -> io::Result<Vec<CString>> {
        let mut names = super::listing::read_names(&self.directory)?
            .into_iter()
            .map(|(name, _)| name)
            .filter(|name| name.starts_with(PREFIX))
            .collect::<Vec<_>>();
        if let Some(at) = names.iter().position(|name| name == PREFIX) {
            let from = CString::new(PREFIX).expect("no NUL");
            let to = CString::new(b"rrd.journal.0001".to_vec()).expect("no NUL");
            // SAFETY: the descriptor is open and both names NUL terminated.
            let renamed = unsafe {
                libc::renameat(
                    self.directory.as_raw_fd(),
                    from.as_ptr(),
                    self.directory.as_raw_fd(),
                    to.as_ptr(),
                )
            };
            if renamed == 0 {
                names[at] = to.into_bytes();
            } else {
                names.remove(at);
            }
        }
        names.sort();
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| CString::new(name).ok())
            .collect())
    }

    /// Opens a journal file for replay. Returns `None` for a name that is
    /// not a single-link regular file owned by the daemon user, including a
    /// symbolic link, which is skipped rather than followed.
    pub(crate) fn open_for_replay(&self, name: &CString) -> io::Result<Option<File>> {
        let file = match open_at(&self.directory, name, libc::O_RDONLY, 0) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no preconditions.
        let owned = metadata.uid() == unsafe { libc::geteuid() };
        if !metadata.is_file() || metadata.nlink() != 1 || !owned {
            return Ok(None);
        }
        Ok(Some(file))
    }

    /// Starts the first journal file.
    pub(crate) fn start(&mut self) -> io::Result<()> {
        self.begin_file()
    }

    fn begin_file(&mut self) -> io::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mut micros = now.subsec_micros();
        for _ in 0..1000 {
            let name = CString::new(format!(
                "rrd.journal.{}.{:06}",
                now.as_secs(),
                micros % 1_000_000
            ))
            .expect("no NUL");
            match open_at(
                &self.directory,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_APPEND,
                0o600,
            ) {
                Ok(file) => {
                    let metadata = file.metadata()?;
                    self.block = usize::try_from(metadata.blksize())
                        .unwrap_or(4096)
                        .clamp(512, 1 << 20);
                    self.files.push((name, self.generation));
                    self.current = Some(file);
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => micros += 1,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::from_raw_os_error(libc::EEXIST))
    }

    /// Adds one line (without its LF).
    pub(crate) fn append(&mut self, line: &[u8]) {
        self.bytes += line.len() as u64 + 1;
        self.buffer.extend_from_slice(line);
        self.buffer.push(b'\n');
        while self.buffer.len() >= self.block {
            let block = self.buffer.drain(..self.block).collect::<Vec<_>>();
            self.write(&block);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if let Some(file) = &mut self.current
            && let Err(error) = file.write_all(bytes)
        {
            tracing::error!(
                journal = %self.path.display(),
                error = %error,
                "rrdcached_journal_write_failed"
            );
        }
    }

    fn close_current(&mut self) {
        let rest = std::mem::take(&mut self.buffer);
        if !rest.is_empty() {
            self.write(&rest);
        }
        self.current = None;
    }

    /// Starts a new file and removes the files older than the previous
    /// one.
    // Compatibility: each -f interval starts a new journal and deletes the
    // set before the previous one, so two generations remain (RCD-31,
    // rrdcached_journal_shutdown_and_rotation_match_upstream).
    // Provenance: rrdcached(1) and black-box compatibility test.
    pub(crate) fn rotate(&mut self) {
        self.close_current();
        self.generation += 1;
        self.rotations += 1;
        if let Err(error) = self.begin_file() {
            tracing::error!(
                journal = %self.path.display(),
                error = %error,
                "rrdcached_journal_rotate_failed"
            );
        }
        let keep = self.generation.saturating_sub(1);
        let (old, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.files)
            .into_iter()
            .partition(|(_, generation)| *generation < keep);
        self.files = kept;
        for (name, _) in old {
            self.remove(&name);
        }
    }

    /// Writes out the buffer and closes the journal, removing every
    /// journal file when the pending values were written.
    pub(crate) fn finish(&mut self, remove: bool) {
        self.close_current();
        if remove {
            for (name, _) in std::mem::take(&mut self.files) {
                self.remove(&name);
            }
        }
    }

    fn remove(&self, name: &CString) {
        // Only a regular file is removed; anything else under the name is
        // not one of ours.
        if stat_at(&self.directory, name)
            .is_ok_and(|status| status.st_mode & libc::S_IFMT == libc::S_IFREG)
        {
            // SAFETY: the descriptor is open and the name NUL terminated.
            unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        }
    }

    pub(crate) fn display(&self, name: &CString) -> PathBuf {
        self.path.join(OsStr::from_bytes(name.as_bytes()))
    }
}

/// Calls `each` with every LF-terminated line of a journal file, reading
/// it in pieces so a large file is never held whole. A line longer than
/// [`REPLAY_LINE_MAX`] is skipped; a last line without LF was never
/// completely written and is not replayed (RCD-32).
pub(crate) fn each_line(
    reader: impl Read,
    mut each: impl FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = std::io::BufReader::new(reader);
    let mut line = Vec::new();
    let mut overlong = false;
    loop {
        let available =
            std::io::BufRead::fill_buf(&mut reader).map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Ok(());
        }
        let (taken, complete) = match available.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at, true),
            None => (available.len(), false),
        };
        if !overlong {
            if line.len() + taken > REPLAY_LINE_MAX {
                overlong = true;
                line.clear();
            } else {
                line.extend_from_slice(&available[..taken]);
            }
        }
        std::io::BufRead::consume(&mut reader, taken + usize::from(complete));
        if complete {
            if !overlong {
                each(&line)?;
            }
            line.clear();
            overlong = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn lines(text: &[u8]) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        each_line(text, |line| {
            found.push(line.to_vec());
            Ok(())
        })
        .unwrap();
        found
    }

    #[test]
    fn only_complete_lines_replay() {
        assert_eq!(lines(b"a\nb\nc"), [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(
            lines(b"a\n\nb\n"),
            [b"a".to_vec(), Vec::new(), b"b".to_vec()]
        );
        assert!(lines(b"partial").is_empty());
        assert!(lines(b"").is_empty());
        let long = [vec![b'x'; REPLAY_LINE_MAX + 1], b"\nok\n".to_vec()].concat();
        assert_eq!(lines(&long), [b"ok".to_vec()]);
        let exact = [vec![b'x'; REPLAY_LINE_MAX], b"\n".to_vec()].concat();
        assert_eq!(lines(&exact)[0].len(), REPLAY_LINE_MAX);
    }

    #[test]
    fn lines_reach_the_file_in_whole_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let (mut journal, replay) = Journal::open(temp.path()).unwrap();
        assert!(replay.is_empty());
        journal.start().unwrap();
        let block = journal.block;
        journal.append(b"update a.rrd 1:1");
        let name = journal.files[0].0.clone();
        let on_disk =
            || std::fs::read(temp.path().join(OsStr::from_bytes(name.as_bytes()))).unwrap();
        assert!(on_disk().is_empty());
        let line = vec![b'x'; block];
        journal.append(&line);
        assert_eq!(on_disk().len(), block);
        journal.finish(false);
        assert_eq!(on_disk().len(), 17 + block + 1);
        assert_eq!(journal.bytes, 17 + block as u64 + 1);
        let mode = std::fs::metadata(temp.path().join(OsStr::from_bytes(name.as_bytes())))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn rotation_keeps_two_generations() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("rrd.journal"), b"update a.rrd 1:1\n").unwrap();
        std::fs::write(temp.path().join("other.file"), b"x").unwrap();
        let (mut journal, replay) = Journal::open(temp.path()).unwrap();
        assert_eq!(replay, [CString::new("rrd.journal.0001").unwrap()]);
        let mut text = Vec::new();
        journal
            .open_for_replay(&replay[0])
            .unwrap()
            .unwrap()
            .read_to_end(&mut text)
            .unwrap();
        assert_eq!(text, b"update a.rrd 1:1\n");
        journal.start().unwrap();
        let count = || {
            std::fs::read_dir(temp.path())
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .as_bytes()
                        .starts_with(PREFIX)
                })
                .count()
        };
        assert_eq!(count(), 2);
        journal.rotate();
        assert_eq!(count(), 3);
        std::thread::sleep(std::time::Duration::from_millis(2));
        journal.rotate();
        assert_eq!(count(), 2);
        assert_eq!(journal.rotations, 2);
        journal.finish(true);
        assert_eq!(count(), 0);
        assert!(temp.path().join("other.file").exists());
    }

    #[test]
    fn shared_directories_must_be_sticky_and_owned() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(Journal::open(temp.path()).is_err());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(Journal::open(temp.path()).is_ok());
    }

    #[test]
    fn linked_or_foreign_files_are_not_replayed() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("rrd.journal.1.000000"), b"x\n").unwrap();
        std::fs::hard_link(
            temp.path().join("rrd.journal.1.000000"),
            temp.path().join("rrd.journal.2.000000"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "rrd.journal.1.000000",
            temp.path().join("rrd.journal.3.000000"),
        )
        .unwrap();
        let (journal, replay) = Journal::open(temp.path()).unwrap();
        assert_eq!(replay.len(), 3);
        for name in &replay {
            assert!(journal.open_for_replay(name).unwrap().is_none());
        }
    }
}
