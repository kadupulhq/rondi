//! `LIST [RECURSIVE] <path>` (RC-100).

use super::files::{Names, open_at, stat_at};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;

/// Directories descended into below the listed one.
const DEPTH_MAX: usize = 64;

/// Names one listing returns at most.
const ENTRIES_MAX: usize = 1 << 20;

/// What an entry of a directory is, without following a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Directory,
    File,
    Link,
    Other,
}

fn kind_of(status: &libc::stat) -> Kind {
    match status.st_mode & libc::S_IFMT {
        libc::S_IFDIR => Kind::Directory,
        libc::S_IFREG => Kind::File,
        libc::S_IFLNK => Kind::Link,
        _ => Kind::Other,
    }
}

/// The entries of an open directory, other than `.` and `..`, in the
/// order the file system returns them.
pub(crate) fn read_names(directory: &File) -> io::Result<Vec<(Vec<u8>, Kind)>> {
    let reopened = open_at(
        directory,
        &CString::new(".").expect("no NUL"),
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let fd = reopened.into_raw_fd();
    // SAFETY: fd is an open directory descriptor this function owns;
    // fdopendir takes it over and closedir below releases both.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so fd is still ours to close.
        drop(unsafe { File::from_raw_fd(fd) });
        return Err(error);
    }
    let mut names = Vec::new();
    let mut failure = None;
    loop {
        // errno distinguishes the end of the stream from a failure.
        // SAFETY: the stream is open; errno is thread local.
        unsafe { *errno_location() = 0 };
        // SAFETY: the stream is open and owned here.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let code = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if code != 0 {
                failure = Some(io::Error::from_raw_os_error(code));
            }
            break;
        }
        // SAFETY: readdir returned a valid entry whose name is NUL
        // terminated; it is copied before the next call.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes()
            .to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let kind = CString::new(name.clone())
            .ok()
            .and_then(|c_name| stat_at(directory, &c_name).ok())
            .map_or(Kind::Other, |status| kind_of(&status));
        names.push((name, kind));
        if names.len() > ENTRIES_MAX {
            failure = Some(io::Error::from_raw_os_error(libc::E2BIG));
            break;
        }
    }
    // SAFETY: the stream was opened above and is closed exactly once.
    unsafe { libc::closedir(stream) };
    match failure {
        Some(error) => Err(error),
        None => Ok(names),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

/// Answers `LIST`. `fields` are the request's fields after the command.
// Compatibility: the path must start with `/` unless RECURSIVE (upper
// case) precedes it; a path holding `..` is refused, under -B with
// `Cannot read:` when it holds `../`; a path ending in `.rrd` names one
// entry (a file lists itself, a directory gives ENXIO, a symbolic link
// nothing); other paths list a directory: its subdirectories and regular
// `.rrd` files, or with RECURSIVE every regular `.rrd` file below it
// relative to it (RC-100). Provenance: rrdcached(1) and black-box
// compatibility test.
pub(crate) fn list(names: &Names, fields: &[Vec<u8>]) -> Result<Vec<u8>, Vec<u8>> {
    const USAGE: &[u8] = b"Usage: LIST [RECURSIVE] /[<path>]";
    let Some(base) = names.base() else {
        return Err(b"No base directory defined".to_vec());
    };
    let (recursive, path) = match fields {
        [first, second, ..] if first == b"RECURSIVE" => (true, second.as_slice()),
        [first, ..] if first != b"RECURSIVE" && first.starts_with(b"/") => {
            (false, first.as_slice())
        }
        _ => return Err(USAGE.to_vec()),
    };
    let mut joined = base.to_vec();
    if !path.starts_with(b"/") {
        joined.push(b'/');
    }
    joined.extend_from_slice(path);
    let failed = |error: io::Error| {
        let mut text = b"List ".to_vec();
        text.extend_from_slice(&joined);
        text.extend_from_slice(b": ");
        text.extend_from_slice(super::error_text(&error).as_bytes());
        text
    };
    if names.confined() && super::files::contains(path, b"../") {
        let mut text = b"Cannot read: ".to_vec();
        text.extend_from_slice(&joined);
        return Err(text);
    }
    if super::files::contains(path, b"..") {
        return Err(failed(io::Error::from_raw_os_error(libc::EACCES)));
    }
    let resolved = resolve(names, base, path).map_err(failed)?;
    let mut found = Vec::new();
    match resolved {
        Resolved::Nothing => {}
        Resolved::One(name) => found.push(name),
        Resolved::Directory(directory) => {
            let mut visited = Vec::new();
            collect(
                names.confined(),
                &directory,
                recursive,
                &[],
                &mut visited,
                &mut found,
            )
            .map_err(failed)?;
        }
    }
    let mut reply = format!("{} RRDs\n", found.len()).into_bytes();
    for name in found {
        reply.extend_from_slice(&name);
        reply.push(b'\n');
    }
    Ok(reply)
}

enum Resolved {
    /// A path naming a symbolic link to something other than a directory.
    Nothing,
    /// A path naming an `.rrd` file: the file's name.
    One(Vec<u8>),
    Directory(File),
}

/// Finds the listed path. Without -B links are followed anywhere; under
/// -B they are followed only while the result stays below the base.
// Rondi policy (RC-100): under -B a link leading out of the base
// directory lists nothing instead of the files it points to.
fn resolve(names: &Names, base: &[u8], path: &[u8]) -> io::Result<Resolved> {
    let parts = path
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .collect::<Vec<_>>();
    let base_dir =
        super::files::open_directory_path(std::path::Path::new(std::ffi::OsStr::from_bytes(base)))?;
    let Some((last, parents)) = parts.split_last() else {
        return Ok(Resolved::Directory(base_dir));
    };
    let mut directory = base_dir;
    for part in parents {
        directory = enter(names, &directory, part)?;
    }
    let name = CString::new(*last).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let status = stat_at(&directory, &name)?;
    let kind = kind_of(&status);
    if last.ends_with(b".rrd") && !path.ends_with(b"/") {
        return match kind {
            Kind::File => Ok(Resolved::One(last.to_vec())),
            Kind::Directory => Err(io::Error::from_raw_os_error(libc::ENXIO)),
            Kind::Link => Ok(Resolved::Nothing),
            Kind::Other => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        };
    }
    match kind {
        Kind::Directory => Ok(Resolved::Directory(open_at(
            &directory,
            &name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?)),
        Kind::Link => match enter(names, &directory, last) {
            Ok(target) => Ok(Resolved::Directory(target)),
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => Ok(Resolved::Nothing),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(Resolved::Nothing),
            Err(error) => Err(error),
        },
        _ => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
    }
}

/// Opens the directory `name` in `directory`, following a link when
/// unconfined or when its target stays below the base.
fn enter(names: &Names, directory: &File, name: &[u8]) -> io::Result<File> {
    let c_name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    match open_at(directory, &c_name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
        Ok(opened) => Ok(opened),
        Err(error) => {
            let is_link = stat_at(directory, &c_name)
                .is_ok_and(|status| status.st_mode & libc::S_IFMT == libc::S_IFLNK);
            if !is_link {
                return Err(error);
            }
            let target = follow(directory, &c_name)?;
            if names.confined() && !within(names, &target)? {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
            }
            Ok(target)
        }
    }
}

/// Opens a link's target as a directory, through the kernel's lookup.
fn follow(directory: &File, name: &CString) -> io::Result<File> {
    // SAFETY: the descriptor is open, the name NUL terminated, and the
    // returned descriptor is owned by exactly one File.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just opened and is not owned elsewhere.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Whether an opened directory is the base directory or below it: walking
/// `..` from it reaches the base before the file system root.
fn within(names: &Names, directory: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let base = names.base().unwrap_or_default();
    let base = std::fs::metadata(std::path::Path::new(std::ffi::OsStr::from_bytes(base)))?;
    let parent = CString::new("..").expect("no NUL");
    let mut current = directory.try_clone()?;
    for _ in 0..4096 {
        let here = current.metadata()?;
        if here.dev() == base.dev() && here.ino() == base.ino() {
            return Ok(true);
        }
        let up = open_at(&current, &parent, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let above = up.metadata()?;
        if above.dev() == here.dev() && above.ino() == here.ino() {
            return Ok(false);
        }
        current = up;
    }
    Ok(false)
}

fn collect(
    confined: bool,
    directory: &File,
    recursive: bool,
    prefix: &[u8],
    visited: &mut Vec<(u64, u64)>,
    found: &mut Vec<Vec<u8>>,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let here = directory.metadata()?;
    visited.push((here.dev(), here.ino()));
    for (name, kind) in read_names(directory)? {
        if found.len() >= ENTRIES_MAX {
            return Err(io::Error::from_raw_os_error(libc::E2BIG));
        }
        let mut relative = prefix.to_vec();
        relative.extend_from_slice(&name);
        let is_rrd = name.ends_with(b".rrd");
        match kind {
            Kind::File if is_rrd => found.push(relative),
            Kind::Directory if !recursive => found.push(relative),
            Kind::Directory | Kind::Link if recursive => {
                // Compatibility: RECURSIVE descends into links to
                // directories and skips links to files (list-030).
                // Provenance: black-box compatibility test.
                // Rondi policy: under -B no link is followed (list-003).
                if kind == Kind::Link && confined {
                    continue;
                }
                let c_name =
                    CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                let child = match kind {
                    Kind::Link => match follow(directory, &c_name) {
                        Ok(child) => child,
                        Err(_) => continue,
                    },
                    _ => match open_at(directory, &c_name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                        Ok(child) => child,
                        Err(_) => continue,
                    },
                };
                let status = child.metadata()?;
                // A link back to a directory being listed would never end.
                if visited.contains(&(status.dev(), status.ino())) || visited.len() > DEPTH_MAX {
                    continue;
                }
                relative.push(b'/');
                collect(confined, &child, true, &relative, visited, found)?;
            }
            _ => {}
        }
    }
    visited.pop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap().join("b");
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::create_dir_all(base.join("dir.rrd")).unwrap();
        for name in ["a.rrd", "sub/d.rrd", "sub/deep/f.rrd", "notes.txt"] {
            std::fs::write(base.join(name), b"x").unwrap();
        }
        std::fs::create_dir_all(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join("out/o.rrd"), b"x").unwrap();
        std::os::unix::fs::symlink("a.rrd", base.join("link.rrd")).unwrap();
        std::os::unix::fs::symlink("sub", base.join("lnk")).unwrap();
        std::os::unix::fs::symlink("../out", base.join("away")).unwrap();
        std::os::unix::fs::symlink(".", base.join("sub/loop")).unwrap();
        (temp, base)
    }

    fn run(names: &Names, request: &str) -> String {
        let fields = request
            .split(' ')
            .map(|field| field.as_bytes().to_vec())
            .collect::<Vec<_>>();
        let reply = match list(names, &fields) {
            Ok(reply) => reply,
            Err(error) => [b"-1 ".to_vec(), error].concat(),
        };
        let text = String::from_utf8(reply).unwrap();
        let mut lines = text.lines().collect::<Vec<_>>();
        lines[1..].sort_unstable();
        lines.join("\n")
    }

    #[test]
    fn listing_follows_the_spec_with_and_without_confinement() {
        let (_temp, base) = tree();
        let base_text = base.display().to_string();
        for confine in [false, true] {
            let names = Names::new(Some(&base), confine).unwrap();
            assert_eq!(run(&names, "/"), "3 RRDs\na.rrd\ndir.rrd\nsub");
            assert_eq!(run(&names, "/sub"), "2 RRDs\nd.rrd\ndeep");
            assert_eq!(run(&names, "/lnk"), "2 RRDs\nd.rrd\ndeep");
            assert_eq!(run(&names, "RECURSIVE sub"), "2 RRDs\nd.rrd\ndeep/f.rrd");
            assert_eq!(run(&names, "/a.rrd"), "1 RRDs\na.rrd");
            assert_eq!(run(&names, "/link.rrd"), "0 RRDs");
            assert!(run(&names, "/dir.rrd").starts_with(&format!("-1 List {base_text}/dir.rrd: ")));
            assert_eq!(
                run(&names, "/notes.txt"),
                format!("-1 List {base_text}/notes.txt: Not a directory")
            );
            assert_eq!(
                run(&names, "/missing"),
                format!("-1 List {base_text}/missing: No such file or directory")
            );
            assert_eq!(
                run(&names, "/sub/.."),
                format!("-1 List {base_text}/sub/..: Permission denied")
            );
            assert_eq!(run(&names, "sub"), "-1 Usage: LIST [RECURSIVE] /[<path>]");
            assert_eq!(
                run(&names, "recursive /"),
                "-1 Usage: LIST [RECURSIVE] /[<path>]"
            );
        }
        let open = Names::new(Some(&base), false).unwrap();
        assert_eq!(
            run(&open, "RECURSIVE /"),
            "6 RRDs\na.rrd\naway/o.rrd\nlnk/d.rrd\nlnk/deep/f.rrd\nsub/d.rrd\nsub/deep/f.rrd"
        );
        assert_eq!(run(&open, "/away"), "1 RRDs\no.rrd");
        assert_eq!(
            run(&open, "/../"),
            format!("-1 List {base_text}/../: Permission denied")
        );
        let confined = Names::new(Some(&base), true).unwrap();
        assert_eq!(
            run(&confined, "RECURSIVE /"),
            "3 RRDs\na.rrd\nsub/d.rrd\nsub/deep/f.rrd"
        );
        assert_eq!(run(&confined, "/away"), "0 RRDs");
        assert_eq!(
            run(&confined, "/../"),
            format!("-1 Cannot read: {base_text}/../")
        );
        let unbased = Names::new(None, false).unwrap();
        assert_eq!(run(&unbased, "/"), "-1 No base directory defined");
    }

    #[test]
    fn directory_entries_report_their_own_kind() {
        let (_temp, base) = tree();
        let directory = super::super::files::open_directory_path(Path::new(&base)).unwrap();
        let entries = read_names(&directory).unwrap();
        assert!(entries.contains(&(b"lnk".to_vec(), Kind::Link)));
        assert!(entries.contains(&(b"sub".to_vec(), Kind::Directory)));
        assert!(entries.contains(&(b"a.rrd".to_vec(), Kind::File)));
    }
}
