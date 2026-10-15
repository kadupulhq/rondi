use std::io::{self, BufRead, Write};
use std::time::Instant;

/// One input line as the session sees it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) bytes: Vec<u8>,
    /// False when input ended before a newline closed the line.
    pub(crate) terminated: bool,
}

/// Reads the next line, or `None` at the end of input.
///
/// A NUL byte hides the rest of its physical line, and the line then
/// carries on with the next physical line.
// Compatibility: `last a.rrd<NUL>\nlast a.rrd\n` is the single line
// `last a.rrdlast a.rrd`.
// Provenance: black-box compatibility test (PM-8, pipe-session-118, -119,
// -147 to -151).
pub(crate) fn read_line(input: &mut impl BufRead) -> io::Result<Option<Line>> {
    let mut bytes = Vec::new();
    let mut started = false;
    loop {
        let mut physical = Vec::new();
        if input.read_until(b'\n', &mut physical)? == 0 {
            return Ok(started.then_some(Line {
                bytes,
                terminated: false,
            }));
        }
        started = true;
        let terminated = physical.last() == Some(&b'\n');
        if terminated {
            physical.pop();
        }
        match physical.iter().position(|&byte| byte == 0) {
            Some(nul) => {
                bytes.extend_from_slice(&physical[..nul]);
                if !terminated {
                    return Ok(Some(Line {
                        bytes,
                        terminated: false,
                    }));
                }
            }
            None => {
                bytes.extend_from_slice(&physical);
                return Ok(Some(Line { bytes, terminated }));
            }
        }
    }
}

// Compatibility: bytes 128 to 255 at the ends of a line are trimmed where
// C `char` is signed (macOS, x86) and kept where it is unsigned (Linux
// aarch64), so this follows Rust's `c_char` for the target.
// Provenance: black-box compatibility test (PM-10, pipe-session-143, -144,
// -146).
const TRIM_HIGH_BYTES: bool = std::ffi::c_char::MIN != 0;

fn is_edge_blank(byte: u8) -> bool {
    (1..=32).contains(&byte) || (TRIM_HIGH_BYTES && byte >= 128)
}

/// Removes blank and control bytes from both ends.
pub(crate) fn trim(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| !is_edge_blank(byte))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&byte| !is_edge_blank(byte))
        .map_or(start, |last| last + 1);
    &bytes[start..end]
}

/// Splits a trimmed line into arguments, or `None` when a quote is left
/// open. Spaces separate arguments; single and double quotes group text
/// and are removed; nothing else is special.
pub(crate) fn split(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut words = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut quote = None;
    for &byte in line {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => current.get_or_insert_with(Vec::new).push(byte),
            None if byte == b' ' => words.extend(current.take()),
            None if byte == b'\'' || byte == b'"' => {
                quote = Some(byte);
                current.get_or_insert_with(Vec::new);
            }
            None => current.get_or_insert_with(Vec::new).push(byte),
        }
    }
    if quote.is_some() {
        return None;
    }
    words.extend(current);
    Some(words)
}

/// A session command that exists only in pipe mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builtin {
    Quit,
    Pwd,
    Ls,
    Cd,
    Mkdir,
}

impl Builtin {
    pub(crate) fn named(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"quit" => Self::Quit,
            b"pwd" => Self::Pwd,
            b"ls" => Self::Ls,
            b"cd" => Self::Cd,
            b"mkdir" => Self::Mkdir,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Pwd => "pwd",
            Self::Ls => "ls",
            Self::Cd => "cd",
            Self::Mkdir => "mkdir",
        }
    }

    fn operands(self) -> usize {
        match self {
            Self::Quit | Self::Pwd | Self::Ls => 0,
            Self::Cd | Self::Mkdir => 1,
        }
    }
}

/// What a line asks the session to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Usage,
    Version,
    Builtin(Builtin, Vec<u8>),
    WrongCount(Builtin),
    Run(Vec<Vec<u8>>),
    Unknown(Vec<u8>),
}

/// Decides what a split line does. `is_command` tells whether a word
/// names a command that runs from the command line too.
pub(crate) fn classify(words: Vec<Vec<u8>>, is_command: impl Fn(&[u8]) -> bool) -> Action {
    let Some(first) = words.first() else {
        return Action::Usage;
    };
    if let Some(builtin) = Builtin::named(first) {
        return if words.len() - 1 == builtin.operands() {
            Action::Builtin(builtin, words.get(1).cloned().unwrap_or_default())
        } else {
            Action::WrongCount(builtin)
        };
    }
    if words.len() == 1 {
        return Action::Usage;
    }
    match first.as_slice() {
        b"help" | b"--help" | b"-h" | b"-?" => Action::Usage,
        b"version" | b"--version" | b"-v" => Action::Version,
        name if is_command(name) => Action::Run(words),
        _ => Action::Unknown(words.into_iter().next().unwrap_or_default()),
    }
}

/// Changes the working directory, reporting failures with the system
/// error number.
pub(crate) fn change_dir(path: &[u8]) -> Result<(), i32> {
    std::env::set_current_dir(super::os_path(path)).map_err(|error| errno_of(&error))
}

fn errno_of(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn join(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Runs a session until end of input or `quit`.
pub(crate) fn run_session(
    input: &mut impl BufRead,
    is_command: impl Fn(&[u8]) -> bool,
    mut run: impl FnMut(&[String]) -> Result<(), Vec<u8>>,
) {
    let started = Instant::now();
    loop {
        let line = match read_line(input) {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => return,
        };
        let reply = respond(&line, &is_command, &mut run, started);
        let mut out = io::stdout().lock();
        let _ = out.write_all(&reply.text);
        let _ = out.flush();
        if reply.quit {
            return;
        }
    }
}

struct Reply {
    text: Vec<u8>,
    quit: bool,
}

impl Reply {
    fn text(text: Vec<u8>) -> Self {
        Self { text, quit: false }
    }

    fn error(message: &[u8]) -> Self {
        Self::text(join(&[b"ERROR: ", message, b"\n"]))
    }

    fn ok(mut text: Vec<u8>, started: Instant) -> Self {
        text.extend_from_slice(ok_line(started).as_bytes());
        Self::text(text)
    }
}

fn respond(
    line: &Line,
    is_command: &impl Fn(&[u8]) -> bool,
    run: &mut impl FnMut(&[String]) -> Result<(), Vec<u8>>,
    started: Instant,
) -> Reply {
    if !line.terminated {
        if line.bytes.is_empty() {
            return Reply::text(Vec::new());
        }
        // Compatibility: a last line made only of spaces and not ended by
        // a newline is rejected, unlike a blank line that is.
        // Provenance: black-box compatibility test.
        if line.bytes.iter().all(|&byte| byte == b' ') {
            return Reply::error(b"not enough arguments");
        }
    }
    let Some(words) = split(trim(&line.bytes)) else {
        return Reply::error(b"creating arguments");
    };
    match classify(words, is_command) {
        Action::Usage => Reply::ok(super::PIPE_USAGE.as_bytes().to_vec(), started),
        Action::Version => Reply::ok(super::VERSION_LINE.as_bytes().to_vec(), started),
        Action::WrongCount(builtin) => {
            Reply::error(format!("invalid parameter count for {}", builtin.name()).as_bytes())
        }
        Action::Builtin(Builtin::Quit, _) => Reply {
            text: Vec::new(),
            quit: true,
        },
        Action::Builtin(Builtin::Pwd, _) => match std::env::current_dir() {
            Ok(path) => {
                let mut text = super::path_bytes(&path);
                text.push(b'\n');
                Reply::ok(text, started)
            }
            Err(error) => Reply::error(
                join(&[
                    b"getcwd ",
                    super::system_error_text(errno_of(&error)).as_bytes(),
                ])
                .as_slice(),
            ),
        },
        Action::Builtin(Builtin::Ls, _) => match list_directory() {
            Ok(text) => Reply::ok(text, started),
            Err(errno) => Reply::error(
                join(&[b"opendir .: ", super::system_error_text(errno).as_bytes()]).as_slice(),
            ),
        },
        Action::Builtin(Builtin::Cd, path) => match change_dir(&path) {
            Ok(()) => Reply::ok(Vec::new(), started),
            Err(errno) => Reply::error(&join(&[
                b"chdir ",
                &path,
                b" ",
                super::system_error_text(errno).as_bytes(),
            ])),
        },
        Action::Builtin(Builtin::Mkdir, path) => match std::fs::create_dir(super::os_path(&path)) {
            Ok(()) => Reply::ok(Vec::new(), started),
            Err(error) => Reply::error(&join(&[
                b"mkdir ",
                &path,
                b": ",
                super::system_error_text(errno_of(&error)).as_bytes(),
            ])),
        },
        Action::Run(words) => {
            let args = words
                .iter()
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect::<Vec<_>>();
            match run(&args) {
                Ok(()) => Reply::ok(Vec::new(), started),
                Err(message) => Reply::error(&message),
            }
        }
        Action::Unknown(name) => Reply::error(&join(&[b"unknown function '", &name, b"'"])),
    }
}

/// `d <name>` for each directory, `- <name>` for each `.rrd` file.
fn list_directory() -> Result<Vec<u8>, i32> {
    let mut text = Vec::new();
    for name in directory_names()? {
        // Symbolic links count as what they point to.
        let Ok(meta) = std::fs::metadata(super::os_path(&name)) else {
            continue;
        };
        let marker: &[u8] = if meta.is_dir() {
            b"d "
        } else if meta.is_file() && is_rrd_name(&name) {
            b"- "
        } else {
            continue;
        };
        text.extend_from_slice(marker);
        text.extend_from_slice(&name);
        text.push(b'\n');
    }
    Ok(text)
}

/// The entry names of the working directory in the order the system
/// returns them, `.` and `..` included wherever they fall.
// Compatibility: `ls` does not sort, and `.` and `..` keep their place in
// the enumeration (last on some Linux file systems).
// Provenance: black-box compatibility test (PM-22, rrdtool_optparse.rs).
#[cfg(unix)]
fn directory_names() -> Result<Vec<Vec<u8>>, i32> {
    // SAFETY: the stream is checked for NULL, read until readdir returns
    // NULL, and closed exactly once; each entry name is copied out before
    // the next readdir call can reuse its storage.
    unsafe {
        let stream = libc::opendir(c".".as_ptr());
        if stream.is_null() {
            return Err(super::last_os_errno());
        }
        let mut names = Vec::new();
        loop {
            let entry = libc::readdir(stream);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
            names.push(name.to_bytes().to_vec());
        }
        libc::closedir(stream);
        Ok(names)
    }
}

#[cfg(not(unix))]
fn directory_names() -> Result<Vec<Vec<u8>>, i32> {
    let entries = std::fs::read_dir(".").map_err(|error| errno_of(&error))?;
    let mut names = vec![b".".to_vec(), b"..".to_vec()];
    names.extend(
        entries
            .flatten()
            .map(|entry| super::path_bytes(std::path::Path::new(&entry.file_name()))),
    );
    Ok(names)
}

// Compatibility: `ls` lists a file when its name is longer than the
// suffix and ends in `.rrd` or `.RRD` (not `.Rrd`, and not a file named
// just `.rrd`).
// Provenance: black-box compatibility test.
fn is_rrd_name(name: &[u8]) -> bool {
    name.len() > 4 && (name.ends_with(b".rrd") || name.ends_with(b".RRD"))
}

fn ok_line(started: Instant) -> String {
    let (user, system) = cpu_seconds();
    format!(
        "OK u:{user:.2} s:{system:.2} r:{:.2}\n",
        started.elapsed().as_secs_f64()
    )
}

#[cfg(unix)]
fn cpu_seconds() -> (f64, f64) {
    // SAFETY: getrusage fills the zeroed struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return (0.0, 0.0);
        }
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    (seconds(usage.ru_utime), seconds(usage.ru_stime))
}

#[cfg(not(unix))]
fn cpu_seconds() -> (f64, f64) {
    (0.0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8]) -> Vec<(Vec<u8>, bool)> {
        let mut reader = input;
        let mut out = Vec::new();
        while let Some(line) = read_line(&mut reader).unwrap() {
            out.push((line.bytes, line.terminated));
        }
        out
    }

    fn words(list: &[&str]) -> Vec<Vec<u8>> {
        list.iter().map(|word| word.as_bytes().to_vec()).collect()
    }

    #[test]
    fn reader_splits_at_newlines() {
        assert_eq!(
            lines(b"a\nb"),
            vec![(b"a".to_vec(), true), (b"b".to_vec(), false)]
        );
        assert!(lines(b"").is_empty());
        assert_eq!(lines(b"\n"), vec![(Vec::new(), true)]);
    }

    #[test]
    fn nul_hides_the_rest_and_joins_the_next_line() {
        assert_eq!(
            lines(b"last a.rrd\0junk\nlast a.rrd\n"),
            vec![(b"last a.rrdlast a.rrd".to_vec(), true)]
        );
        assert_eq!(
            lines(b"\0\nlast a.rrd\n"),
            vec![(b"last a.rrd".to_vec(), true)]
        );
        assert_eq!(lines(b"x\0"), vec![(b"x".to_vec(), false)]);
        assert_eq!(lines(b"\0\n"), vec![(Vec::new(), false)]);
        assert_eq!(
            lines(b"last a.rrd\0j\n\0k\nl\0\n"),
            vec![(b"last a.rrdl".to_vec(), false)]
        );
    }

    #[test]
    fn trim_removes_blank_and_control_bytes_at_the_ends() {
        assert_eq!(trim(b" \t\rlast a.rrd\r\n "), b"last a.rrd");
        assert_eq!(trim(b"last\ta.rrd"), b"last\ta.rrd");
        assert_eq!(trim(b"\x7flast\x7f"), b"\x7flast\x7f");
        assert_eq!(trim(b"   "), b"");
        let high = trim(b"\xfflast a.rrd\xe9");
        if TRIM_HIGH_BYTES {
            assert_eq!(high, b"last a.rrd");
        } else {
            assert_eq!(high, b"\xfflast a.rrd\xe9");
        }
    }

    #[test]
    fn split_handles_spaces_and_quotes() {
        assert_eq!(split(b"last  a.rrd").unwrap(), words(&["last", "a.rrd"]));
        assert_eq!(split(b"last\ta.rrd").unwrap(), words(&["last\ta.rrd"]));
        assert_eq!(split(b"'a'.rrd").unwrap(), words(&["a.rrd"]));
        assert_eq!(split(b"\"a.r\"'rd'").unwrap(), words(&["a.rrd"]));
        assert_eq!(split(b"'a\".rrd'").unwrap(), words(&["a\".rrd"]));
        assert_eq!(
            split(b"last \"\" a.rrd").unwrap(),
            words(&["last", "", "a.rrd"])
        );
        assert_eq!(split(b"\"a b\" c").unwrap(), words(&["a b", "c"]));
        assert_eq!(split(b"a\\b #c").unwrap(), words(&["a\\b", "#c"]));
        assert_eq!(split(b"").unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(split(b"\"a\\\"b\""), None);
        assert_eq!(split(b"'it'\\''s'"), None);
    }

    #[test]
    fn classify_follows_the_dispatch_table() {
        let is_command = |name: &[u8]| name == b"last";
        let action = |list: &[&str]| classify(words(list), is_command);
        assert_eq!(action(&[]), Action::Usage);
        assert_eq!(action(&["last"]), Action::Usage);
        assert_eq!(
            action(&["quit"]),
            Action::Builtin(Builtin::Quit, Vec::new())
        );
        assert_eq!(action(&["quit", "x"]), Action::WrongCount(Builtin::Quit));
        assert_eq!(action(&["cd"]), Action::WrongCount(Builtin::Cd));
        assert_eq!(
            action(&["cd", ".."]),
            Action::Builtin(Builtin::Cd, b"..".to_vec())
        );
        assert_eq!(action(&["help", "last"]), Action::Usage);
        assert_eq!(action(&["-v", "x"]), Action::Version);
        assert_eq!(
            action(&["last", "a.rrd"]),
            Action::Run(words(&["last", "a.rrd"]))
        );
        assert_eq!(
            action(&["Last", "a.rrd"]),
            Action::Unknown(b"Last".to_vec())
        );
        assert_eq!(action(&["QUIT"]), Action::Usage);
    }

    #[test]
    fn rrd_names() {
        assert!(is_rrd_name(b"a.rrd"));
        assert!(is_rrd_name(b"..rrd"));
        assert!(is_rrd_name(b"x.RRD"));
        assert!(!is_rrd_name(b".rrd"));
        assert!(!is_rrd_name(b"a.Rrd"));
        assert!(!is_rrd_name(b"a.rrdx"));
    }

    #[test]
    fn ok_line_has_two_decimals() {
        let line = ok_line(Instant::now());
        let fields = line.trim_end().split(' ').collect::<Vec<_>>();
        assert_eq!(fields[0], "OK");
        for (field, prefix) in fields[1..].iter().zip(["u:", "s:", "r:"]) {
            let number = field.strip_prefix(prefix).unwrap();
            let (whole, fraction) = number.split_once('.').unwrap();
            assert!(whole.bytes().all(|byte| byte.is_ascii_digit()));
            assert_eq!(fraction.len(), 2);
        }
    }
}
