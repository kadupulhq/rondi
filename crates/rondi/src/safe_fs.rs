//! Descriptor-relative operations for daemon paths. Operator CLI paths retain
//! their existing symlink behavior unless they explicitly use this module.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

// A search-only descriptor needs execute permission alone, as a pathname
// lookup does, so execute-only ancestors stay traversable.
#[cfg(any(target_os = "linux", target_os = "android"))]
const DIRECTORY_SEARCH: i32 = libc::O_PATH;
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
const DIRECTORY_SEARCH: i32 = libc::O_SEARCH;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
const DIRECTORY_SEARCH: i32 = libc::O_RDONLY;

/// Keep CREATE's unpublished file inaccessible to other accounts, even when
/// the destination directory is writable by those accounts.
pub(crate) struct PrivateDirectory<'a> {
    parent: &'a File,
    name: CString,
    pub(crate) directory: File,
}

impl<'a> PrivateDirectory<'a> {
    pub(crate) fn create(parent: &'a File) -> io::Result<Self> {
        use std::hash::{BuildHasher, Hasher};
        use std::os::unix::fs::MetadataExt;
        for _ in 0..100 {
            let random = std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish();
            let name = CString::new(format!(".rondi-create-{random:016x}")).unwrap();
            // SAFETY: parent is open and name is NUL terminated.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            let directory = open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let metadata = directory.metadata()?;
            // SAFETY: geteuid has no preconditions. A replacement directory
            // owned by another account must never become the staging area.
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            return Ok(Self {
                parent,
                name,
                directory,
            });
        }
        Err(io::Error::from_raw_os_error(libc::EEXIST))
    }
}

impl Drop for PrivateDirectory<'_> {
    fn drop(&mut self) {
        // SAFETY: both descriptors remain open. Remove only our staging file;
        // a rename of the directory cannot redirect this cleanup elsewhere.
        unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"rrd".as_ptr(), 0) };
        let mut ours: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: directory is open and the output buffer remains valid.
        if unsafe { libc::fstat(self.directory.as_raw_fd(), &mut ours) } != 0 {
            return;
        }
        let mut current: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor, name, and output buffer remain valid.
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                &mut current,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && current.st_dev == ours.st_dev
            && current.st_ino == ours.st_ino
        {
            // SAFETY: this name still identifies our empty staging directory.
            unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            };
        }
    }
}

pub fn open_directory(path: &Path, create: bool) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = File::open("/")?;
    for component in absolute.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => CString::new(name.as_bytes())?,
            _ => return Err(io::Error::from_raw_os_error(libc::EPERM)),
        };
        if create {
            // SAFETY: the descriptor and NUL-terminated name remain valid.
            let status = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
            if status != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(io::Error::last_os_error());
            }
        }
        directory = open_at(&directory, &name, DIRECTORY_SEARCH | libc::O_DIRECTORY, 0)?;
    }
    Ok(directory)
}

pub fn open_at(directory: &File, name: &CStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    if name.to_bytes().is_empty()
        || name.to_bytes().contains(&b'/')
        || name.to_bytes() == b"."
        || name.to_bytes() == b".."
    {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    // SAFETY: directory is open, name is NUL terminated, and the returned
    // descriptor is transferred exactly once into an owning File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn open_file(path: &Path, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?
            .as_bytes(),
    )?;
    let directory = open_directory(parent, false)?;
    open_at(&directory, &name, flags, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn traversal_passes_execute_only_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let search_only = root.join("search-only");
        std::fs::create_dir_all(search_only.join("data")).unwrap();
        std::fs::write(search_only.join("data/metric.rrd"), b"rrd").unwrap();
        std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o300)).unwrap();
        let opened = super::open_file(&search_only.join("data/metric.rrd"), libc::O_RDONLY, 0);
        let created = super::open_directory(&search_only.join("data/new"), true);
        std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o700)).unwrap();
        opened.unwrap();
        created.unwrap();
        assert!(search_only.join("data/new").is_dir());
    }

    #[test]
    fn staging_rename_cannot_replace_payload_or_redirect_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let parent = File::open(temp.path()).unwrap();
        let staging = PrivateDirectory::create(&parent).unwrap();
        assert_eq!(
            staging.directory.metadata().unwrap().permissions().mode() & 0o077,
            0
        );
        let mut file = open_at(
            &staging.directory,
            c"rrd",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .unwrap();
        file.write_all(b"original RRD").unwrap();
        let original = temp
            .path()
            .join(std::ffi::OsStr::from_bytes(staging.name.to_bytes()));
        let moved = temp.path().join("moved");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("rrd"), b"replacement").unwrap();
        // Publishing resolves the payload through the pinned private directory.
        assert_eq!(
            unsafe {
                libc::renameat(
                    staging.directory.as_raw_fd(),
                    c"rrd".as_ptr(),
                    parent.as_raw_fd(),
                    c"result".as_ptr(),
                )
            },
            0
        );
        drop(staging);
        let mut bytes = String::new();
        File::open(temp.path().join("result"))
            .unwrap()
            .read_to_string(&mut bytes)
            .unwrap();
        assert_eq!(bytes, "original RRD");
        assert_eq!(std::fs::read(original.join("rrd")).unwrap(), b"replacement");
    }
}
