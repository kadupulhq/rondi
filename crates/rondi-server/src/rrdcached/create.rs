//! `CREATE <file> [options] <DS...> <RRA...>` (RC-80 to RC-82).

use super::files::{Denied, Target, denied};
use super::service::{Answer, Shared};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The earliest start a new file may have: 1980-01-01.
const EARLIEST_START: i64 = 315_360_000;

/// File names in the base directory that belong to Rondi's own store.
const RESERVED: [&[u8]; 2] = [b".rondi.lock", b"rondi.journal"];

struct Options {
    start: Option<i64>,
    step: i64,
    no_overwrite: bool,
    unsupported: Option<&'static str>,
    definitions: Vec<String>,
}

/// C `atol`: leading digits after optional white space and sign; 0 when
/// there are none.
fn atol(text: &[u8]) -> i64 {
    rondi::c_strtol(&String::from_utf8_lossy(text), 10).0
}

// Compatibility: words starting with `-` are options anywhere on the line,
// recognized by their second byte; `-b`, `-s`, `-r` and `-t` take the next
// word, which must not be empty; `DS:` and `RRA:` words are definitions in
// any order; anything else is a usage error (RC-80). Provenance:
// black-box compatibility test.
fn parse(words: &[Vec<u8>]) -> Option<Options> {
    let mut options = Options {
        start: None,
        step: 0,
        no_overwrite: false,
        unsupported: None,
        definitions: Vec::new(),
    };
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if word.starts_with(b"DS:") || word.starts_with(b"RRA:") {
            options
                .definitions
                .push(String::from_utf8_lossy(word).into_owned());
            continue;
        }
        let letter = match word.as_slice() {
            [b'-', letter, ..] => *letter,
            _ => return None,
        };
        match letter {
            b'O' => options.no_overwrite = true,
            b'b' | b's' | b'r' | b't' => {
                let argument = words.next().filter(|argument| !argument.is_empty())?;
                match letter {
                    b'b' => options.start = Some(atol(argument)),
                    b's' => options.step = atol(argument),
                    // Rondi difference (known defect, RC-80): sources and
                    // templates are not supported.
                    b'r' => options.unsupported = options.unsupported.or(Some("-r")),
                    _ => options.unsupported = options.unsupported.or(Some("-t")),
                }
            }
            _ => return None,
        }
    }
    Some(options)
}

pub(crate) fn create(shared: &Shared, name: &[u8], words: &[Vec<u8>]) -> Answer {
    let usage = || Answer::Failed(super::commands::Command::Create.usage().into_bytes());
    let joined = shared.names.join(name);
    if let Err(Denied(text)) = shared.names.locate(&joined) {
        return Answer::Failed(text);
    }
    let (parent, last) = split_name(&joined);
    // Rondi policy (RC-31): an empty name, which would name the base
    // directory, and Rondi's own store files are never replaced.
    if name.is_empty() || RESERVED.contains(&last) {
        return Answer::Failed(denied(&joined));
    }
    let Some(options) = parse(words) else {
        return usage();
    };
    if let Some(option) = options.unsupported {
        return Answer::Failed(format!("unsupported CREATE option: {option}").into_bytes());
    }
    // Compatibility: `-b -1` and a missing `-b` mean ten seconds ago;
    // anything else before 1980 is refused. Provenance: black-box
    // compatibility test.
    let start = match options.start {
        None | Some(-1) => super::service::now() - 10,
        Some(start) if start < EARLIEST_START => {
            return Answer::Failed(b"The first entry must be after 1980.".to_vec());
        }
        Some(start) => start,
    };
    // A step of 0 means the default; the value passes through as C long.
    let step = if options.step == 0 {
        300
    } else {
        options.step as u64
    };
    if joined.ends_with(b"/") {
        return trailing_slash(shared, &joined);
    }
    let directory = match open_parent(shared, &joined, parent) {
        Ok(directory) => directory,
        Err(answer) => return answer,
    };
    let no_overwrite = options.no_overwrite || shared.settings.no_overwrite;
    match rondi::create_rrd_at(
        &directory,
        OsStr::from_bytes(last),
        start,
        step,
        &options.definitions,
        no_overwrite,
    ) {
        Ok(()) => Answer::Done(b"0 RRD created OK\n".to_vec()),
        Err(rondi::StoreError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Answer::Failed(
                format!(
                    "RRD Error: creating '{}': File exists",
                    String::from_utf8_lossy(&joined)
                )
                .into_bytes(),
            )
        }
        Err(error) => Answer::Failed(format!("RRD Error: {error}").into_bytes()),
    }
}

/// The directory part and the last component of a joined name.
fn split_name(joined: &[u8]) -> (&[u8], &[u8]) {
    let trimmed = joined
        .iter()
        .rposition(|&byte| byte != b'/')
        .map_or(&joined[..0], |end| &joined[..=end]);
    match trimmed.iter().rposition(|&byte| byte == b'/') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (&b"."[..], trimmed),
    }
}

/// A name ending in `/` cannot be created.
// Compatibility: for a missing file or a regular file the reference
// reports `Cannot create temporary file`. Provenance: black-box
// compatibility test.
// Rondi policy: a directory is never replaced (as for `dir.rrd`).
fn trailing_slash(shared: &Shared, joined: &[u8]) -> Answer {
    let is_directory = shared.names.locate(joined).is_ok_and(|target| {
        target
            .status()
            .is_ok_and(|status| status.st_mode & libc::S_IFMT == libc::S_IFDIR)
    });
    if is_directory {
        let error = std::io::Error::from_raw_os_error(libc::EISDIR);
        return Answer::Failed(format!("RRD Error: I/O error: {error}").into_bytes());
    }
    Answer::Failed(b"RRD Error: Cannot create temporary file".to_vec())
}

/// Opens the directory a new file goes in, creating it with `-R`.
// Compatibility: a missing directory is refused with a two-line error
// naming it, unless the daemon runs with -R, which creates it with mode
// 0755 under umask 022 (RC-82). Provenance: rrdcached(1) and black-box
// compatibility test.
fn open_parent(shared: &Shared, joined: &[u8], parent: &[u8]) -> Result<std::fs::File, Answer> {
    let missing = || {
        let mut text = b"No permission to recursively create: ".to_vec();
        text.extend_from_slice(parent);
        text.extend_from_slice(b"\nDid you pass -R to the daemon?");
        Answer::Failed(text)
    };
    let failed = |error: std::io::Error| {
        Answer::Failed(
            format!("RRD Error: {}", super::service::opening(parent, &error)).into_bytes(),
        )
    };
    let recursive = shared.settings.recursive_create;
    if !shared.names.confined() {
        let path = Path::new(OsStr::from_bytes(parent));
        return match super::files::open_directory_path(path) {
            Ok(directory) => Ok(directory),
            // The umask sets the mode, less group and other write (RCD-13).
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) && recursive => {
                super::create_directories(path)
                    .and_then(|()| super::files::open_directory_path(path))
                    .map_err(failed)
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Err(missing()),
            Err(error) => Err(failed(error)),
        };
    }
    let locate = |create: bool| {
        if create {
            shared.names.locate_creating(joined)
        } else {
            shared.names.locate(joined)
        }
    };
    let target = locate(false).map_err(|Denied(text)| Answer::Failed(text))?;
    let target = match target {
        Target::Within(Err(error)) if error.raw_os_error() == Some(libc::ENOENT) => {
            if !recursive {
                return Err(missing());
            }
            locate(true).map_err(|Denied(text)| Answer::Failed(text))?
        }
        other => other,
    };
    match target {
        Target::Within(Ok(entry)) => Ok(entry.parent),
        Target::Within(Err(error)) => Err(failed(error)),
        Target::Path(_) => unreachable!("confined names are located below the base"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<Vec<u8>> {
        line.split(' ')
            .map(|word| word.as_bytes().to_vec())
            .collect()
    }

    #[test]
    fn options_read_like_the_reference() {
        let parsed = parse(&words("-bogus 1000000007x -step 60 -Ox DS:a RRA:b")).unwrap();
        assert_eq!(parsed.start, Some(1_000_000_007));
        assert_eq!(parsed.step, 60);
        assert!(parsed.no_overwrite);
        assert_eq!(parsed.definitions, ["DS:a", "RRA:b"]);
        let late = parse(&words("DS:a -b 100 RRA:b")).unwrap();
        assert_eq!(late.start, Some(100));
        assert_eq!(parse(&words("-s x DS:a")).unwrap().step, 0);
        assert_eq!(
            parse(&words("-r a.rrd DS:a")).unwrap().unsupported,
            Some("-r")
        );
        for refused in [
            "--step 60",
            "-z",
            "DS:a junk",
            "-b",
            "-b  1",
            "- DS:a",
            "DS:a ",
        ] {
            assert!(parse(&words(refused)).is_none(), "{refused}");
        }
        assert!(parse(&[]).is_some());
    }

    #[test]
    fn names_split_at_the_last_slash() {
        assert_eq!(split_name(b"/b/x/n.rrd"), (&b"/b/x"[..], &b"n.rrd"[..]));
        assert_eq!(split_name(b"/b/a.rrd/"), (&b"/b"[..], &b"a.rrd"[..]));
        assert_eq!(split_name(b"n.rrd"), (&b"."[..], &b"n.rrd"[..]));
        assert_eq!(split_name(b"/b/"), (&b""[..], &b"b"[..]));
    }
}
