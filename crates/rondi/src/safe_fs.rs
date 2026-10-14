//! Descriptor-relative operations for daemon paths. Operator CLI paths retain
//! their existing symlink behavior unless they explicitly use this module.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

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
        directory = open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
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
