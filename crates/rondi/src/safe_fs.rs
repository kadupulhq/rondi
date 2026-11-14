//! Descriptor-relative operations for daemon paths. Operator CLI paths retain
//! their existing symlink behavior unless they explicitly use this module.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;

// A search-only descriptor needs execute permission alone, as a pathname
// lookup does, so execute-only ancestors stay traversable.
#[cfg(any(target_os = "linux", target_os = "android"))]
const DIRECTORY_SEARCH: OFlags = OFlags::PATH;
// rustix has no name for O_SEARCH, so take the value from the C library.
#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
const DIRECTORY_SEARCH: OFlags = OFlags::from_bits_retain(libc::O_SEARCH as _);
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
const DIRECTORY_SEARCH: OFlags = OFlags::RDONLY;

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
            match rustix::fs::mkdirat(parent, name.as_c_str(), Mode::from_raw_mode(0o700)) {
                Ok(()) => {}
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            }
            let directory = open_at(
                parent,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )?;
            let metadata = directory.metadata()?;
            // A replacement directory owned by another account must never
            // become the staging area.
            if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                return Err(Errno::PERM.into());
            }
            return Ok(Self {
                parent,
                name,
                directory,
            });
        }
        Err(Errno::EXIST.into())
    }
}

impl Drop for PrivateDirectory<'_> {
    fn drop(&mut self) {
        // Remove only our staging file; a rename of the directory cannot
        // redirect this cleanup elsewhere.
        let _ = rustix::fs::unlinkat(&self.directory, c"rrd", AtFlags::empty());
        let Ok(ours) = rustix::fs::fstat(&self.directory) else {
            return;
        };
        if let Ok(current) =
            rustix::fs::statat(self.parent, self.name.as_c_str(), AtFlags::SYMLINK_NOFOLLOW)
            && current.st_dev == ours.st_dev
            && current.st_ino == ours.st_ino
        {
            // This name still identifies our empty staging directory.
            let _ = rustix::fs::unlinkat(self.parent, self.name.as_c_str(), AtFlags::REMOVEDIR);
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
            _ => return Err(Errno::PERM.into()),
        };
        if create {
            match rustix::fs::mkdirat(&directory, name.as_c_str(), Mode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        directory = open_at(
            &directory,
            &name,
            DIRECTORY_SEARCH | OFlags::DIRECTORY,
            Mode::empty(),
        )?;
    }
    Ok(directory)
}

/// `flags` as `std::os::unix::fs::OpenOptionsExt::custom_flags` takes them.
pub fn custom_flags(flags: OFlags) -> i32 {
    flags.bits() as i32
}

/// Open `name` in `directory` without following a final symbolic link.
/// `name` must be a single path component.
pub fn open_at(directory: &File, name: &CStr, flags: OFlags, mode: Mode) -> io::Result<File> {
    if name.to_bytes().is_empty()
        || name.to_bytes().contains(&b'/')
        || name.to_bytes() == b"."
        || name.to_bytes() == b".."
    {
        return Err(Errno::PERM.into());
    }
    let fd = rustix::fs::openat(
        directory,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY,
        mode,
    )?;
    Ok(File::from(fd))
}

pub fn open_file(path: &Path, flags: OFlags, mode: Mode) -> io::Result<File> {
    let parent = path.parent().ok_or(Errno::INVAL)?;
    let name = CString::new(path.file_name().ok_or(Errno::INVAL)?.as_bytes())?;
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
        let opened = super::open_file(
            &search_only.join("data/metric.rrd"),
            OFlags::RDONLY,
            Mode::empty(),
        );
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
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
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
        rustix::fs::renameat(&staging.directory, c"rrd", &parent, c"result").unwrap();
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
