// `rrdtool list` without a daemon (docs/compatibility/cli.md, CLI-70 to
// CLI-75 and CLI-79).
//
// Paths and names are handled as bytes throughout so that names which are
// not UTF-8 are listed unchanged.

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const SUFFIX: &[u8] = b".rrd";

/// The local `list` (CLI-70, CLI-71, CLI-79). `Ok` holds the whole standard
/// output, every line ending in `\n`, never empty. `Err` holds the error
/// number whose system text the caller prints; 0 means nothing was listed.
pub(crate) fn list_local(recursive: bool, path: &OsStr) -> Result<Vec<u8>, libc::c_int> {
    let path = path.as_bytes();
    // Compatibility: any `..` in the argument, even inside a name, is refused.
    // Provenance: black-box compatibility test (cli_list_and_aliases list_matches_reference, CLI-79).
    if contains(path, b"..") {
        return Err(libc::EACCES);
    }
    if path.iter().any(|&b| b == b'*' || b == b'?') {
        if recursive {
            return Err(libc::EINVAL);
        }
        return list_pattern(path);
    }
    if is_rrd_name(path) {
        return list_rrd_path(path);
    }
    let mut output = Vec::new();
    let entries = std::fs::read_dir(Path::new(OsStr::from_bytes(path))).map_err(errno)?;
    if recursive {
        list_tree(path, &[], entries, &mut output);
    } else {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.as_bytes();
            let Ok(metadata) = std::fs::metadata(child(path, name)) else {
                continue;
            };
            if metadata.is_dir() || is_rrd_name(name) {
                push_line(&mut output, name);
            }
        }
    }
    if output.is_empty() {
        Err(0)
    } else {
        Ok(output)
    }
}

/// Whether `name` ends in `.rrd` and that is the first `.rrd` it contains.
// Compatibility: only the first occurrence of `.rrd` counts, so `a.rrd.rrd`
// is not an RRD name.
// Provenance: black-box compatibility test (cli_list_and_aliases list_matches_reference, CLI-79).
pub(crate) fn is_rrd_name(name: &[u8]) -> bool {
    name.windows(SUFFIX.len())
        .position(|window| window == SUFFIX)
        .is_some_and(|at| at + SUFFIX.len() == name.len())
}

/// Recursive listing (CLI-71): every non-directory entry with an RRD name,
/// relative to the starting directory, with subdirectories expanded in place.
fn list_tree(base: &[u8], relative: &[u8], entries: std::fs::ReadDir, output: &mut Vec<u8>) {
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.as_bytes();
        // Paths are joined as text and never resolved, so a link loop grows
        // the path until the operating system refuses it.
        let full = child(base, name);
        let Ok(metadata) = std::fs::metadata(&full) else {
            continue;
        };
        let shown = if relative.is_empty() {
            name.to_vec()
        } else {
            [relative, b"/", name].concat()
        };
        if metadata.is_dir() {
            if let Ok(inner) = std::fs::read_dir(&full) {
                list_tree(full.as_os_str().as_bytes(), &shown, inner, output);
            }
        } else if is_rrd_name(name) {
            push_line(output, &shown);
        }
    }
}

/// An argument that is itself an RRD path (CLI-79 rule 3).
// Compatibility: an RRD path prints its last component if it is a regular
// file reached through a `/`; otherwise it is an error, never a listing.
// Provenance: black-box compatibility test (cli_list_and_aliases list_matches_reference, CLI-79).
fn list_rrd_path(path: &[u8]) -> Result<Vec<u8>, libc::c_int> {
    let metadata = std::fs::metadata(Path::new(OsStr::from_bytes(path))).map_err(errno)?;
    if !metadata.is_file() {
        return Err(libc::ENXIO);
    }
    let Some(slash) = path.iter().rposition(|&b| b == b'/') else {
        return Err(libc::EINVAL);
    };
    let mut output = Vec::new();
    push_line(&mut output, &path[slash + 1..]);
    Ok(output)
}

/// A shell pattern (CLI-79 rule 2), expanded by the platform's `glob(3)`.
// Compatibility: each match is printed as the text after its last `/`,
// unfiltered; matches without a `/` are not printed.
// Provenance: black-box compatibility test (cli_list_and_aliases list_matches_reference, CLI-79).
fn list_pattern(pattern: &[u8]) -> Result<Vec<u8>, libc::c_int> {
    let Ok(pattern) = CString::new(pattern) else {
        return Err(libc::ENOENT);
    };
    let mut output = Vec::new();
    for found in glob_matches(&pattern) {
        if let Some(slash) = found.iter().rposition(|&b| b == b'/') {
            push_line(&mut output, &found[slash + 1..]);
        }
    }
    if output.is_empty() {
        Err(libc::ENOENT)
    } else {
        Ok(output)
    }
}

/// The sorted matches of `pattern`; empty when `glob` reports no match or
/// fails.
fn glob_matches(pattern: &CStr) -> Vec<Vec<u8>> {
    // SAFETY: an all-zero `glob_t` is the documented initial state.
    let mut results: libc::glob_t = unsafe { std::mem::zeroed() };
    // SAFETY: `pattern` is NUL-terminated and `results` is a valid glob_t.
    let status = unsafe { libc::glob(pattern.as_ptr(), 0, None, &mut results) };
    let mut matches = Vec::new();
    if status == 0 {
        for index in 0..results.gl_pathc as usize {
            // SAFETY: on success `gl_pathv` holds `gl_pathc` valid,
            // NUL-terminated strings (no offset is requested).
            let text = unsafe { CStr::from_ptr(*results.gl_pathv.add(index)) };
            matches.push(text.to_bytes().to_vec());
        }
    }
    // SAFETY: `results` was filled by `glob` and is freed once.
    unsafe { libc::globfree(&mut results) };
    matches
}

fn child(directory: &[u8], name: &[u8]) -> std::path::PathBuf {
    Path::new(OsStr::from_bytes(&[directory, b"/", name].concat())).to_path_buf()
}

fn push_line(output: &mut Vec<u8>, line: &[u8]) {
    output.extend_from_slice(line);
    output.push(b'\n');
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn errno(error: std::io::Error) -> libc::c_int {
    error.raw_os_error().unwrap_or(libc::EINVAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn mkfifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    fn lines(output: &[u8]) -> BTreeSet<Vec<u8>> {
        assert!(output.ends_with(b"\n"));
        output[..output.len() - 1]
            .split(|&b| b == b'\n')
            .map(<[u8]>::to_vec)
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<Vec<u8>> {
        names.iter().map(|n| n.as_bytes().to_vec()).collect()
    }

    #[test]
    fn rrd_names_use_the_first_occurrence() {
        for yes in [
            "a.rrd",
            ".rrd",
            "a..rrd",
            "a.rr.rrd",
            "c.Rrd.rrd",
            "a rrd.rrd",
        ] {
            assert!(is_rrd_name(yes.as_bytes()), "{yes}");
        }
        for no in [
            "a.rrd.rrd",
            "a.rrdb.rrd",
            "b.RRD",
            "a.rrdx",
            "x.rrdrrd",
            "rrd",
            "",
        ] {
            assert!(!is_rrd_name(no.as_bytes()), "{no}");
        }
    }

    #[test]
    fn dot_dot_anywhere_is_permission_denied() {
        assert_eq!(list_local(false, OsStr::new("a..b")), Err(libc::EACCES));
        assert_eq!(
            list_local(true, OsStr::new("x/../y.rrd")),
            Err(libc::EACCES)
        );
    }

    #[test]
    fn patterns_are_invalid_when_recursive() {
        assert_eq!(list_local(true, OsStr::new("d/*.rrd")), Err(libc::EINVAL));
        assert_eq!(list_local(true, OsStr::new("*")), Err(libc::EINVAL));
    }

    #[test]
    fn directory_listing_filters_by_type_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        touch(&dir.join("a.rrd"));
        touch(&dir.join("b.txt"));
        touch(&dir.join("a.rrd.rrd"));
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::create_dir(dir.join("x.rrd")).unwrap();
        mkfifo(&dir.join("f.rrd"));
        symlink("missing.rrd", dir.join("dang.rrd")).unwrap();
        symlink("sub", dir.join("lsub")).unwrap();
        let output = list_local(false, dir.as_os_str()).unwrap();
        assert_eq!(
            lines(&output),
            set(&["a.rrd", "f.rrd", "lsub", "sub", "x.rrd"])
        );
    }

    #[test]
    fn recursive_listing_enters_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir(dir.join("sub")).unwrap();
        touch(&dir.join("sub/s.rrd"));
        std::fs::create_dir(dir.join("x.rrd")).unwrap();
        touch(&dir.join("x.rrd/in.rrd"));
        symlink("sub", dir.join("lsub")).unwrap();
        touch(&dir.join("a.rrd"));
        let output = list_local(true, dir.as_os_str()).unwrap();
        assert_eq!(
            lines(&output),
            set(&["a.rrd", "lsub/s.rrd", "sub/s.rrd", "x.rrd/in.rrd"])
        );
    }

    #[test]
    fn recursive_link_loop_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let looped = tmp.path().join("loop");
        std::fs::create_dir(&looped).unwrap();
        touch(&looped.join("l.rrd"));
        symlink("../loop", looped.join("up")).unwrap();
        let output = list_local(true, looped.as_os_str()).unwrap();
        let all: Vec<&[u8]> = output[..output.len() - 1].split(|&b| b == b'\n').collect();
        assert!(all.iter().all(|line| line.ends_with(b"l.rrd")));
        assert!(all.len() > 1 && all.len() < 100, "{}", all.len());
    }

    #[test]
    fn empty_results_are_error_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert_eq!(list_local(false, empty.as_os_str()), Err(0));
        assert_eq!(list_local(true, empty.as_os_str()), Err(0));
        let text_only = tmp.path().join("text");
        std::fs::create_dir(&text_only).unwrap();
        touch(&text_only.join("b.txt"));
        assert_eq!(list_local(false, text_only.as_os_str()), Err(0));
    }

    #[test]
    fn open_errors_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert_eq!(list_local(false, missing.as_os_str()), Err(libc::ENOENT));
        let notes = tmp.path().join("notes");
        touch(&notes);
        assert_eq!(list_local(false, notes.as_os_str()), Err(libc::ENOTDIR));
    }

    #[test]
    fn rrd_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        touch(&dir.join("a.rrd"));
        std::fs::create_dir(dir.join("x.rrd")).unwrap();
        mkfifo(&dir.join("f.rrd"));
        touch(&dir.join("a.rrd.rrd"));
        for recursive in [false, true] {
            assert_eq!(
                list_local(recursive, dir.join("a.rrd").as_os_str()),
                Ok(b"a.rrd\n".to_vec())
            );
        }
        assert_eq!(
            list_local(false, dir.join("x.rrd").as_os_str()),
            Err(libc::ENXIO)
        );
        assert_eq!(
            list_local(false, dir.join("f.rrd").as_os_str()),
            Err(libc::ENXIO)
        );
        assert_eq!(
            list_local(false, dir.join("missing.rrd").as_os_str()),
            Err(libc::ENOENT)
        );
        assert_eq!(
            list_local(false, dir.join("a.rrd.rrd").as_os_str()),
            Err(libc::ENOTDIR)
        );
    }

    #[test]
    fn patterns_print_sorted_basenames() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        touch(&dir.join("b.rrd"));
        touch(&dir.join("a.rrd"));
        touch(&dir.join("t.txt"));
        std::fs::create_dir(dir.join("x.rrd")).unwrap();
        assert_eq!(
            list_local(false, dir.join("*.rrd").as_os_str()),
            Ok(b"a.rrd\nb.rrd\nx.rrd\n".to_vec())
        );
        assert_eq!(
            list_local(false, dir.join("*.none").as_os_str()),
            Err(libc::ENOENT)
        );
        let mut slash = dir.join("*.rrd").into_os_string();
        slash.push("/");
        assert_eq!(list_local(false, &slash), Ok(b"\n".to_vec()));
    }

    #[test]
    fn non_utf8_names_are_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let name = OsStr::from_bytes(b"a\xff.rrd");
        if std::fs::write(dir.join(name), b"x").is_err() {
            return;
        }
        let output = list_local(false, dir.as_os_str()).unwrap();
        assert!(lines(&output).contains(b"a\xff.rrd".as_slice()));
        assert_eq!(
            list_local(false, OsStr::from_bytes(b"\xff.rrd")),
            Err(libc::ENOENT)
        );
    }
}
