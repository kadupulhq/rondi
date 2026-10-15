//! How request file names become files: joining with the base directory,
//! the `-B` confinement, and descriptor-relative access below the base.
//!
//! Without `-B` a joined name is used as a path, following symbolic links
//! as the reference does. Under `-B` every component below the base
//! directory is opened relative to its parent's descriptor without
//! following symbolic links, so a link swapped into the tree between a
//! check and a use cannot redirect the operation.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// Directory descriptors used only to reach their entries need search
// permission alone, as a path lookup does.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SEARCH: i32 = libc::O_PATH;
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
const SEARCH: i32 = libc::O_SEARCH;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
const SEARCH: i32 = libc::O_RDONLY;

/// A name refused by the `-B` confinement. Its text is the reply.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Denied(pub(crate) Vec<u8>);

/// The base directory and whether names are confined to it.
pub(crate) struct Names {
    base: Option<Vec<u8>>,
    confine: bool,
    /// The base directory, held open so confined lookups start from it.
    root: Option<File>,
}

/// A file named by a request.
pub(crate) enum Target {
    /// Used as a path.
    Path(PathBuf),
    /// Below the base directory: the open parent and the final name, or
    /// the error that stopped the walk on the way there.
    Within(io::Result<Entry>),
}

/// The parent directory of a confined name and the name in it.
pub(crate) struct Entry {
    pub(crate) parent: File,
    /// `None` for the base directory itself (the empty name).
    pub(crate) name: Option<CString>,
    /// The name ended with `/`, so only a directory matches it.
    pub(crate) directory: bool,
}

impl Names {
    /// `base` must be the base directory's real path.
    pub(crate) fn new(base: Option<&Path>, confine: bool) -> io::Result<Self> {
        let root = match base {
            Some(base) if confine => Some(open_directory_path(base)?),
            _ => None,
        };
        Ok(Self {
            base: base.map(|base| {
                let mut bytes = base.as_os_str().as_bytes().to_vec();
                while bytes.len() > 1 && bytes.ends_with(b"/") {
                    bytes.pop();
                }
                bytes
            }),
            confine: confine && base.is_some(),
            root,
        })
    }

    pub(crate) fn base(&self) -> Option<&[u8]> {
        self.base.as_deref()
    }

    pub(crate) fn confined(&self) -> bool {
        self.confine
    }

    /// The name a request means: an absolute name as sent, anything else
    /// below the base directory with one `/` (RC-30, RC-32).
    // Compatibility: the result is not normalized, so `./a.rrd` and
    // `a.rrd` stay different names. Provenance: black-box compatibility
    // test.
    pub(crate) fn join(&self, name: &[u8]) -> Vec<u8> {
        match &self.base {
            Some(base) if !name.starts_with(b"/") => {
                let mut joined = Vec::with_capacity(base.len() + 1 + name.len());
                joined.extend_from_slice(base);
                if !base.ends_with(b"/") {
                    joined.push(b'/');
                }
                joined.extend_from_slice(name);
                joined
            }
            _ => name.to_vec(),
        }
    }

    /// Resolves a joined name for access, applying `-B`.
    // Compatibility: under -B a name outside `<base>/`, or one containing
    // `../`, is refused with `<name>: Permission denied` (RC-31).
    // Provenance: rrdcached(1) and black-box compatibility test.
    // Rondi policy: below the base no symbolic link is followed and a `..`
    // component is refused, so `link.rrd`, `lnk/d.rrd` and `sub/..` are
    // refused too (RC-31 divergences).
    pub(crate) fn locate(&self, joined: &[u8]) -> Result<Target, Denied> {
        if !self.confine {
            return Ok(Target::Path(PathBuf::from(OsStr::from_bytes(joined))));
        }
        self.walk(joined, false)
    }

    /// Like [`Names::locate`], creating missing directories on the way
    /// (mode 0755 less the umask).
    pub(crate) fn locate_creating(&self, joined: &[u8]) -> Result<Target, Denied> {
        if !self.confine {
            return Ok(Target::Path(PathBuf::from(OsStr::from_bytes(joined))));
        }
        self.walk(joined, true)
    }

    fn walk(&self, joined: &[u8], create: bool) -> Result<Target, Denied> {
        let refuse = || Denied(denied(joined));
        let below = self.below_base(joined)?;
        let parts = components(below).map_err(|()| refuse())?;
        let root = self.root.as_ref().expect("a confined base is open");
        let mut directory = match root.try_clone() {
            Ok(directory) => directory,
            Err(error) => return Ok(Target::Within(Err(error))),
        };
        let (last, parents) = match parts.split_last() {
            Some((last, parents)) => (Some(*last), parents),
            None => (None, &[][..]),
        };
        for component in parents {
            let name = c_name(component);
            if create {
                // SAFETY: the descriptor is open and the name NUL terminated.
                let made = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
                let error = io::Error::last_os_error();
                if made != 0 && error.kind() != io::ErrorKind::AlreadyExists {
                    return Ok(Target::Within(Err(error)));
                }
            }
            directory = match step_into(&directory, &name) {
                Ok(next) => next,
                Err(Step::Link) => return Err(refuse()),
                Err(Step::Failed(error)) => return Ok(Target::Within(Err(error))),
            };
        }
        let name = last.map(c_name);
        if let Some(name) = &name
            && stat_at(&directory, name)
                .is_ok_and(|status| status.st_mode & libc::S_IFMT == libc::S_IFLNK)
        {
            return Err(refuse());
        }
        Ok(Target::Within(Ok(Entry {
            parent: directory,
            name,
            directory: below.ends_with(b"/"),
        })))
    }

    /// The part of a joined name below `<base>/`.
    fn below_base<'a>(&self, joined: &'a [u8]) -> Result<&'a [u8], Denied> {
        let base = self.base.as_deref().unwrap_or_default();
        let below = joined
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix(b"/"))
            .filter(|_| !contains(joined, b"../"));
        below.ok_or_else(|| Denied(denied(joined)))
    }
}

/// `<name>: Permission denied`.
pub(crate) fn denied(joined: &[u8]) -> Vec<u8> {
    let mut text = joined.to_vec();
    text.extend_from_slice(b": Permission denied");
    text
}

pub(crate) fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Why a directory on the way could not be entered. A link is a
/// refusal; any other error is the target's state, so the command answers
/// with its own text for it.
enum Step {
    Link,
    Failed(io::Error),
}

/// The components of a confined name: empty ones (`//`) and `.` skipped,
/// `..` refused.
fn components(below: &[u8]) -> Result<Vec<&[u8]>, ()> {
    let mut parts = Vec::new();
    for part in below.split(|&byte| byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => return Err(()),
            other => parts.push(other),
        }
    }
    Ok(parts)
}

/// Opens the directory `name` in `directory` without following a link.
fn step_into(directory: &File, name: &CString) -> Result<File, Step> {
    match open_at(directory, name, SEARCH | libc::O_DIRECTORY, 0) {
        Ok(next) => Ok(next),
        Err(error) => match stat_at(directory, name) {
            Ok(status) if status.st_mode & libc::S_IFMT == libc::S_IFLNK => Err(Step::Link),
            _ => Err(Step::Failed(error)),
        },
    }
}

fn c_name(bytes: &[u8]) -> CString {
    // Fields never hold NUL: it separates them.
    CString::new(bytes).unwrap_or_default()
}

/// `openat` without following a final link; `.` names the directory.
pub(crate) fn open_at(
    directory: &File,
    name: &CString,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    // SAFETY: the descriptor is open, the name NUL terminated, and the
    // returned descriptor is owned by exactly one File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
            libc::c_uint::from(mode),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned by openat and is not owned elsewhere.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// `fstatat` without following a final link.
pub(crate) fn stat_at(directory: &File, name: &CString) -> io::Result<libc::stat> {
    let mut status = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: the descriptor is open, the name NUL terminated and the
    // buffer valid for one stat structure.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat succeeded and filled the structure.
    Ok(unsafe { status.assume_init() })
}

/// Opens a directory by path, for the base directory and unconfined use.
pub(crate) fn open_directory_path(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

/// Whether a file is opened to read or to read and write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
}

/// Refuses to write through anything but a regular file with one link,
/// owned by root when the daemon runs as root.
// Rondi policy (RD-007): a hard link planted below the base directory, or
// a special file, cannot redirect the daemon's writes to another file.
fn writable(file: File) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    if metadata.nlink() != 1 {
        return Err(io::Error::from_raw_os_error(libc::EMLINK));
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 && metadata.uid() != 0 {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(file)
}

impl Target {
    /// Opens the file. A FIFO or device never blocks the open.
    pub(crate) fn open(&self, access: Access) -> io::Result<File> {
        let opened = self.open_any(access)?;
        match access {
            Access::Read => Ok(opened),
            Access::Write => writable(opened),
        }
    }

    fn open_any(&self, access: Access) -> io::Result<File> {
        let flags = match access {
            Access::Read => libc::O_RDONLY,
            Access::Write => libc::O_RDWR,
        };
        match self {
            Self::Path(path) => {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(access == Access::Write)
                    .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
                    .open(path)
            }
            Self::Within(Err(error)) => Err(copy_error(error)),
            Self::Within(Ok(entry)) => {
                let extra = if entry.directory {
                    libc::O_DIRECTORY
                } else {
                    0
                };
                match &entry.name {
                    Some(name) => open_at(&entry.parent, name, flags | extra, 0),
                    None if access == Access::Write => {
                        Err(io::Error::from_raw_os_error(libc::EISDIR))
                    }
                    None => open_at(&entry.parent, &c_name(b"."), flags | extra, 0),
                }
            }
        }
    }

    /// The file's status, following links only when unconfined.
    pub(crate) fn status(&self) -> io::Result<libc::stat> {
        match self {
            Self::Path(path) => {
                let name = CString::new(path.as_os_str().as_bytes())
                    .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                let mut status = std::mem::MaybeUninit::<libc::stat>::zeroed();
                // SAFETY: the name is NUL terminated and the buffer valid.
                if unsafe { libc::stat(name.as_ptr(), status.as_mut_ptr()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: stat succeeded and filled the structure.
                Ok(unsafe { status.assume_init() })
            }
            Self::Within(Err(error)) => Err(copy_error(error)),
            Self::Within(Ok(entry)) => {
                let status = match &entry.name {
                    Some(name) => stat_at(&entry.parent, name)?,
                    None => stat_at(&entry.parent, &c_name(b"."))?,
                };
                if entry.directory && status.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                Ok(status)
            }
        }
    }
}

/// `io::Error` is not `Clone`; an error from the walk is reported again
/// for each operation on the target.
fn copy_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(error.kind(), error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap().join("b");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.rrd"), b"a").unwrap();
        std::fs::write(base.join("sub/d.rrd"), b"d").unwrap();
        std::fs::write(temp.path().join("outside.rrd"), b"o").unwrap();
        std::os::unix::fs::symlink("a.rrd", base.join("link.rrd")).unwrap();
        std::os::unix::fs::symlink("sub", base.join("lnk")).unwrap();
        (temp, base)
    }

    fn read(target: &Target) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut bytes = Vec::new();
        target.open(Access::Read)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    #[test]
    fn names_join_without_normalizing() {
        let names = Names::new(Some(Path::new("/base")), false).unwrap();
        assert_eq!(names.join(b"a.rrd"), b"/base/a.rrd");
        assert_eq!(names.join(b"./a.rrd"), b"/base/./a.rrd");
        assert_eq!(names.join(b""), b"/base/");
        assert_eq!(names.join(b"/x/y"), b"/x/y");
        let unbased = Names::new(None, true).unwrap();
        assert_eq!(unbased.join(b"a.rrd"), b"a.rrd");
        assert!(!unbased.confined());
    }

    #[test]
    fn confinement_refuses_escapes_and_links() {
        let (_temp, base) = tree();
        let names = Names::new(Some(&base), true).unwrap();
        let joined = |name: &str| names.join(name.as_bytes());
        for name in [
            "../outside.rrd",
            "sub/../a.rrd",
            "link.rrd",
            "lnk/d.rrd",
            "sub/..",
        ] {
            let full = joined(name);
            assert_eq!(
                names.locate(&full).err(),
                Some(Denied(denied(&full))),
                "{name}"
            );
        }
        let outside = base.parent().unwrap().join("outside.rrd");
        assert!(names.locate(outside.as_os_str().as_bytes()).is_err());
        assert_eq!(
            read(&names.locate(&joined("./a.rrd")).unwrap()).unwrap(),
            b"a"
        );
        assert_eq!(
            read(&names.locate(&joined("sub//d.rrd")).unwrap()).unwrap(),
            b"d"
        );
        let missing = names.locate(&joined("missing/x.rrd")).unwrap();
        assert_eq!(
            read(&missing).unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
        let slash = names.locate(&joined("a.rrd/")).unwrap();
        assert_eq!(
            read(&slash).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
        assert_eq!(
            slash.status().unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
        let through_file = names.locate(&joined("a.rrd/x")).unwrap();
        assert_eq!(
            through_file.status().unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
        let empty = names.locate(&joined("")).unwrap();
        assert_eq!(
            empty.status().unwrap().st_mode & libc::S_IFMT,
            libc::S_IFDIR
        );
        assert_eq!(
            empty.open(Access::Write).unwrap_err().raw_os_error(),
            Some(libc::EISDIR)
        );
    }

    #[test]
    fn writes_need_a_single_link_regular_file() {
        let (_temp, base) = tree();
        std::fs::hard_link(base.join("sub/d.rrd"), base.join("sub/hard.rrd")).unwrap();
        let fifo = CString::new(base.join("fifo.rrd").as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL terminated and the mode valid.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for confine in [false, true] {
            let names = Names::new(Some(&base), confine).unwrap();
            let open = |name: &str| {
                names
                    .locate(&names.join(name.as_bytes()))
                    .unwrap()
                    .open(Access::Write)
            };
            assert!(open("a.rrd").is_ok());
            assert_eq!(
                open("sub/hard.rrd").unwrap_err().raw_os_error(),
                Some(libc::EMLINK)
            );
            assert_eq!(
                open("fifo.rrd").unwrap_err().raw_os_error(),
                Some(libc::EINVAL)
            );
            assert_eq!(open("sub").unwrap_err().raw_os_error(), Some(libc::EISDIR));
            assert!(
                names
                    .locate(&names.join(b"sub/hard.rrd"))
                    .unwrap()
                    .open(Access::Read)
                    .is_ok()
            );
        }
    }

    #[test]
    fn unconfined_names_follow_links() {
        let (_temp, base) = tree();
        let names = Names::new(Some(&base), false).unwrap();
        let target = names.locate(&names.join(b"lnk/d.rrd")).unwrap();
        assert_eq!(read(&target).unwrap(), b"d");
        let slash = names.locate(&names.join(b"a.rrd/")).unwrap();
        assert_eq!(
            read(&slash).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }

    #[test]
    fn creating_walk_makes_missing_directories_only_below_the_base() {
        let (_temp, base) = tree();
        let names = Names::new(Some(&base), true).unwrap();
        let joined = names.join(b"x/y/n.rrd");
        let Target::Within(Ok(entry)) = names.locate_creating(&joined).unwrap() else {
            panic!("not created");
        };
        assert_eq!(entry.name.unwrap().as_bytes(), b"n.rrd");
        assert!(base.join("x/y").is_dir());
        assert!(
            names
                .locate_creating(&names.join(b"lnk/new/n.rrd"))
                .is_err()
        );
        assert!(!base.join("sub/new").exists());
    }
}
