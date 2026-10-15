// The `rrdcached` personality: reads the daemon's command line into a
// `DaemonConfig` and hands it to `rondi_server::rrdcached::run`.
//
// Behavior follows docs/compatibility/rrdcached-daemon.md (RCD-1, RCD-2),
// written from the rrdcached(1) manual and black-box runs of rrdcached
// 1.11.0.

use rondi_server::rrdcached::{
    self as daemon, Account, DEFAULT_SOCKET, DaemonConfig, Interval, ListenAddress, ListenerSpec,
    LogFile, LogLevel, OptionFailure, SocketAccess,
};
use std::io::Write;
use std::path::PathBuf;

// Compatibility: the usage text, byte for byte (opt-001).
// Provenance: black-box compatibility test (output of `rrdcached -h`).
const USAGE: &str = "RRDCacheD 1.11.0\n\
Copyright (C) 2008,2009 Florian octo Forster and Kevin Brintnall\n\
\n\
Usage: rrdcached [options]\n\
\n\
Valid options are:\n\
\x20 -a <size>     Memory allocation chunk size. Default is 1.\n\
\x20 -B            Restrict file access to paths within -b <dir>\n\
\x20 -b <dir>      Base directory to change to.\n\
\x20 -F            Always flush all updates at shutdown\n\
\x20 -f <seconds>  Interval in which to flush dead data.\n\
\x20 -G <group>    Unprivileged group used when running.\n\
\x20 -g            Do not fork and run in the foreground.\n\
\x20 -j <dir>      Directory in which to create the journal files.\n\
\x20 -L            Open sockets on all INET interfaces using default port.\n\
\x20 -l <address>  Socket address to listen to.\n\
\x20               Default: unix:/tmp/rrdcached.sock\n\
\x20 -m <mode>     File permissions (octal) of all following UNIX sockets\n\
\x20 -O            Do not allow CREATE commands to overwrite existing\n\
\x20               files, even if asked to.\n\
\x20 -o <file>     Log to given file instead of syslog.\n\
\x20 -P <perms>    Sets the permissions to assign to all following sockets\n\
\x20 -p <file>     Location of the PID-file.\n\
\x20 -R            Allow recursive directory creation within -b <dir>\n\
\x20 -s <id|name>  Group owner of all following UNIX sockets\n\
\x20               (the socket will also have read/write permissions for that group)\n\
\x20 -t <threads>  Number of write threads.\n\
\x20 -U <user>     Unprivileged user account used when running.\n\
\x20 -V <LOGLEVEL> Max syslog level to log with, with LOG_DEBUG being\n\
\x20               the maximum and LOG_EMERG minimum; see syslog.h\n\
\x20 -w <seconds>  Interval in which to write data.\n\
\x20 -z <delay>    Delay writes up to <delay> seconds to spread load\n\
\n\
For more information and a detailed description of all options please refer\n\
to the rrdcached(1) manual page.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opt {
    AllocationChunk,
    Confine,
    Base,
    FlushAtShutdown,
    FlushInterval,
    Group,
    Foreground,
    Help,
    Journal,
    ListenEverywhere,
    Listen,
    SocketMode,
    NoOverwrite,
    LogFile,
    Permissions,
    PidFile,
    RecursiveCreate,
    SocketGroup,
    Threads,
    User,
    LogLevel,
    WriteInterval,
    WriteJitter,
}

// Compatibility: the option letters of `rrdcached -h`, plus `--help`; no
// other long option exists (RCD-1).
// Provenance: rrdcached(1) and black-box compatibility test.
const OPTIONS: &[(u8, Opt, bool)] = &[
    (b'a', Opt::AllocationChunk, true),
    (b'B', Opt::Confine, false),
    (b'b', Opt::Base, true),
    (b'F', Opt::FlushAtShutdown, false),
    (b'f', Opt::FlushInterval, true),
    (b'G', Opt::Group, true),
    (b'g', Opt::Foreground, false),
    (b'h', Opt::Help, false),
    (b'j', Opt::Journal, true),
    (b'L', Opt::ListenEverywhere, false),
    (b'l', Opt::Listen, true),
    (b'm', Opt::SocketMode, true),
    (b'O', Opt::NoOverwrite, false),
    (b'o', Opt::LogFile, true),
    (b'P', Opt::Permissions, true),
    (b'p', Opt::PidFile, true),
    (b'R', Opt::RecursiveCreate, false),
    (b's', Opt::SocketGroup, true),
    (b't', Opt::Threads, true),
    (b'U', Opt::User, true),
    (b'V', Opt::LogLevel, true),
    (b'w', Opt::WriteInterval, true),
    (b'z', Opt::WriteJitter, true),
];

/// One item of the command line.
#[derive(Debug, PartialEq, Eq)]
enum Scanned {
    Option(Opt, String),
    /// The message for a word the table rejects, without its newline.
    Rejected(Vec<u8>),
}

/// Walks the arguments as the daemon's option reader does. Words that are
/// not options are skipped and the first `--` ends the options.
// Compatibility: after an unknown letter the rest of its word is skipped,
// and the next option word is read from the unknown letter's position
// rather than from its first letter, so `-gx -abc` reads `-b c` and
// `-gx -h` reports an empty option name; error texts are cut to 47 bytes
// and at a NUL byte, as C strings are.
// Provenance: black-box compatibility test.
struct Scanner<'a> {
    words: &'a [String],
    next: usize,
    /// Where the next short-option word is read from.
    resume_at: usize,
    pending: Option<(usize, usize)>,
    ended: bool,
}

impl<'a> Scanner<'a> {
    fn new(words: &'a [String]) -> Self {
        Self {
            words,
            next: 0,
            resume_at: 1,
            pending: None,
            ended: false,
        }
    }

    fn rejected(text: &[u8], name: &[u8]) -> Scanned {
        let mut message = text.to_vec();
        message.extend_from_slice(name);
        message.push(b'\'');
        if let Some(nul) = message.iter().position(|byte| *byte == 0) {
            message.truncate(nul);
        }
        message.truncate(47);
        Scanned::Rejected(message)
    }

    fn long(&self, body: &str) -> Scanned {
        match body.split_once('=') {
            None if body == "help" => Scanned::Option(Opt::Help, String::new()),
            Some(("help", _)) => Self::rejected(b"option takes no arguments -- '", b"help"),
            _ => Self::rejected(b"invalid option -- '", body.as_bytes()),
        }
    }

    /// Reads the letter at `offset` of word `index`.
    fn letter(&mut self, index: usize, offset: usize) -> Scanned {
        let word = &self.words[index];
        let bytes = word.as_bytes();
        let byte = bytes.get(offset).copied().unwrap_or(0);
        let Some(&(_, option, takes_value)) = OPTIONS.iter().find(|(letter, ..)| *letter == byte)
        else {
            self.resume_at = offset;
            return Self::rejected(b"invalid option -- '", &[byte]);
        };
        self.resume_at = 1;
        let rest = offset + 1;
        if !takes_value {
            if rest < bytes.len() {
                self.pending = Some((index, rest));
            }
            return Scanned::Option(option, String::new());
        }
        if rest < bytes.len() {
            // The letter is ASCII, so `rest` is a character boundary.
            return Scanned::Option(option, word[rest..].to_owned());
        }
        match self.words.get(self.next) {
            Some(value) => {
                self.next += 1;
                Scanned::Option(option, value.clone())
            }
            None => Self::rejected(b"option requires an argument -- '", &[byte]),
        }
    }
}

impl Iterator for Scanner<'_> {
    type Item = Scanned;

    fn next(&mut self) -> Option<Scanned> {
        if let Some((index, offset)) = self.pending.take() {
            return Some(self.letter(index, offset));
        }
        while !self.ended {
            let index = self.next;
            let word = self.words.get(index)?;
            self.next += 1;
            if word == "--" {
                self.ended = true;
            } else if let Some(body) = word.strip_prefix("--") {
                return Some(self.long(body));
            } else if word.len() > 1 && word.starts_with('-') {
                let offset = self.resume_at;
                return Some(self.letter(index, offset));
            }
        }
        None
    }
}

/// The option values that touch the system: directories that `-b`
/// creates, paths that `-j` and `-o` open, accounts that `-U`, `-G` and
/// `-s` name.
trait Environment {
    fn base_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure>;
    fn journal_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure>;
    fn log_file(&mut self, value: &str) -> Result<LogFile, OptionFailure>;
    fn user(&mut self, value: &str) -> Option<Account>;
    fn group(&mut self, value: &str) -> Option<u32>;
    fn socket_group(&mut self, value: &str) -> Option<u32>;
}

struct System;

impl Environment for System {
    fn base_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure> {
        daemon::prepare_base_directory(value)
    }

    fn journal_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure> {
        daemon::resolve_journal_directory(value)
    }

    fn log_file(&mut self, value: &str) -> Result<LogFile, OptionFailure> {
        daemon::open_log_file(value)
    }

    fn user(&mut self, value: &str) -> Option<Account> {
        daemon::lookup_user(value)
    }

    fn group(&mut self, value: &str) -> Option<u32> {
        daemon::lookup_group(value)
    }

    fn socket_group(&mut self, value: &str) -> Option<u32> {
        daemon::lookup_socket_group(value)
    }
}

/// What the command line asks for.
#[derive(Debug)]
enum Verdict {
    /// Exit with this status, after printing the usage text `usage` times.
    Exit {
        status: i32,
        usage: usize,
    },
    Run(Box<DaemonConfig>),
}

/// The listener settings in force at a point of the command line.
#[derive(Clone, Debug, Default)]
struct ListenerDefaults {
    access: SocketAccess,
    permissions: Option<Vec<String>>,
}

/// Reads the options in order, as the reference does: most errors only set
/// the exit status and reading goes on, so a later option can still change
/// it, while some stop at once.
// Compatibility: `-w`, `-f`, `-z` and `-P` errors set the status and go
// on; a successful `-P` name resets it to 0; `-h` sets 1 and an unknown
// option 0, each printing the usage text once more; `-a`, `-b`, `-G`, `-j`, `-m`,
// `-o`, `-s`, `-t` and `-U` errors stop reading. Warnings are printed
// unless reading stopped.
// Provenance: black-box compatibility test.
struct Reader<'a, E> {
    environment: &'a mut E,
    errors: &'a mut dyn Write,
    config: DaemonConfig,
    defaults: ListenerDefaults,
    status: i32,
    usage: usize,
}

impl<E: Environment> Reader<'_, E> {
    fn note(&mut self, text: &str) {
        let _ = writeln!(self.errors, "{text}");
    }

    /// Handles one option; `Err` stops reading with that status.
    fn apply(&mut self, option: Opt, value: &str) -> Result<(), i32> {
        match option {
            Opt::AllocationChunk => {
                // Compatibility: a whole C int above zero (opt-013, opt-014);
                // an empty value is a missing argument.
                // Provenance: black-box compatibility test.
                let chunk =
                    c_long(value, 10).filter(|size| (1..=i64::from(i32::MAX)).contains(size));
                if value.is_empty() {
                    self.note("Missing argument for -a");
                    return Err(10);
                }
                let Some(chunk) = chunk else {
                    self.note(&format!("Invalid allocation size: {value}"));
                    return Err(10);
                };
                self.config.allocation_chunk = chunk as usize;
            }
            Opt::Confine => self.config.confine_to_base = true,
            Opt::Base => {
                let base = self
                    .environment
                    .base_directory(value)
                    .map_err(|failure| self.fail(failure))?;
                self.config.base = Some(base);
            }
            Opt::FlushAtShutdown => self.config.flush_at_shutdown = true,
            Opt::FlushInterval => {
                if let Some(interval) = self.interval(value, "flush interval", 3) {
                    self.config.flush_interval = interval;
                }
            }
            Opt::Group => {
                let Some(gid) = self.environment.group(value) else {
                    self.note(&format!(
                        "read_options: couldn't map \"{value}\" to a group, Sorry"
                    ));
                    return Err(5);
                };
                self.config.group = Some(gid);
            }
            Opt::Foreground => {
                // Rondi never forks: it always runs in the foreground (RCD-12).
            }
            Opt::Help => {
                self.status = 1;
                self.usage += 1;
            }
            Opt::Journal => {
                let journal = self
                    .environment
                    .journal_directory(value)
                    .map_err(|failure| self.fail(failure))?;
                self.config.journal = Some(journal);
            }
            Opt::ListenEverywhere => self.listen(ListenAddress::Network(String::new())),
            Opt::Listen => self.listen(ListenAddress::parse(value)),
            Opt::SocketMode => {
                // Compatibility: an octal C long from 0 to 07777 (opt-017).
                // Provenance: black-box compatibility test.
                let Some(mode) = c_long(value, 8).filter(|mode| (0..=0o7777).contains(mode)) else {
                    self.note(&format!("read_options: Invalid file mode \"{value}\"."));
                    return Err(5);
                };
                self.defaults.access.mode = Some(mode as u32);
            }
            Opt::NoOverwrite => self.config.no_overwrite = true,
            Opt::LogFile => {
                let file = self
                    .environment
                    .log_file(value)
                    .map_err(|failure| self.fail(failure))?;
                self.config.log_file = Some(file);
            }
            Opt::Permissions => self.permissions(value),
            Opt::PidFile => self.config.pid_file = Some(PathBuf::from(value)),
            Opt::RecursiveCreate => self.config.recursive_create = true,
            Opt::SocketGroup => {
                let Some(gid) = self.environment.socket_group(value) else {
                    self.note(&format!(
                        "read_options: couldn't map \"{value}\" to a group, Sorry"
                    ));
                    return Err(5);
                };
                self.defaults.access.group = Some(gid);
            }
            Opt::Threads => {
                let threads =
                    c_long(value, 10).filter(|count| (1..=i64::from(i32::MAX)).contains(count));
                if value.is_empty() {
                    self.note("Missing argument for -t");
                    return Err(1);
                }
                let Some(threads) = threads else {
                    self.note(&format!("Invalid thread count: -t {value}"));
                    return Err(1);
                };
                self.config.write_threads = threads as u32;
            }
            Opt::User => {
                let Some(account) = self.environment.user(value) else {
                    self.note(&format!(
                        "read_options: couldn't map \"{value}\" to a user, Sorry"
                    ));
                    return Err(5);
                };
                self.config.user = Some(account);
            }
            Opt::LogLevel => match LogLevel::from_name(value) {
                Some(level) => self.config.log_level = level,
                None => {
                    self.note(&format!(
                        "Unrecognized log level '{value}'; falling back to default LOG_ERR."
                    ));
                    self.config.log_level = LogLevel::Err;
                }
            },
            Opt::WriteInterval => {
                if let Some(interval) = self.interval(value, "write interval", 2) {
                    self.config.write_interval = interval;
                }
            }
            Opt::WriteJitter => {
                if let Some(interval) = self.interval(value, "write jitter", 2) {
                    self.config.write_jitter = interval;
                }
            }
        }
        Ok(())
    }

    fn fail(&mut self, failure: OptionFailure) -> i32 {
        self.note(&failure.message);
        failure.status
    }

    fn interval(&mut self, value: &str, what: &str, status: i32) -> Option<Interval> {
        match parse_interval(value) {
            Ok(interval) => Some(interval),
            Err(problem) => {
                self.note(&format!("Invalid {what} {value}: {}", problem.text()));
                self.status = status;
                None
            }
        }
    }

    fn listen(&mut self, address: ListenAddress) {
        self.config.listeners.push(ListenerSpec {
            address,
            access: self.defaults.access,
            permissions: self.defaults.permissions.clone(),
        });
    }

    /// `-P`: replaces the command list for the listeners that follow.
    // Compatibility: names are split on commas and spaces, empty ones are
    // skipped, each name sets the status (0 if known, 4 if not), and an
    // empty list leaves the listeners unrestricted (RC-130, opt-042,
    // opt-047). Provenance: black-box compatibility test.
    fn permissions(&mut self, value: &str) {
        let mut allowed = Vec::new();
        for name in value.split([',', ' ']).filter(|name| !name.is_empty()) {
            if daemon::is_command(name) {
                allowed.push(name.to_owned());
                self.status = 0;
            } else {
                self.note(&format!(
                    "read_options: Adding permission \"{name}\" to socket failed. Most likely, this permission doesn't exist. Check your command line."
                ));
                self.status = 4;
            }
        }
        self.defaults.permissions = (!allowed.is_empty()).then_some(allowed);
    }

    fn rejected(&mut self, mut line: Vec<u8>) {
        line.push(b'\n');
        let _ = self.errors.write_all(&line);
        self.status = 0;
        self.usage += 1;
    }
}

/// Reads `args` (without the program name).
fn read_options(
    args: &[String],
    environment: &mut impl Environment,
    errors: &mut dyn Write,
) -> Verdict {
    let mut reader = Reader {
        environment,
        errors,
        config: DaemonConfig::default(),
        defaults: ListenerDefaults::default(),
        status: 0,
        usage: 0,
    };
    let mut stopped = false;
    for item in Scanner::new(args) {
        match item {
            Scanned::Option(option, value) => {
                if let Err(status) = reader.apply(option, &value) {
                    reader.status = status;
                    stopped = true;
                    break;
                }
            }
            Scanned::Rejected(message) => reader.rejected(message),
        }
    }
    if !stopped {
        for warning in reader.config.warnings() {
            reader.note(warning);
        }
    }
    if reader.usage > 0 || reader.status != 0 {
        return Verdict::Exit {
            status: reader.status,
            usage: reader.usage,
        };
    }
    let mut config = reader.config;
    if config.listeners.is_empty() {
        config.listeners.push(ListenerSpec {
            address: ListenAddress::Unix(PathBuf::from(DEFAULT_SOCKET)),
            access: reader.defaults.access,
            permissions: reader.defaults.permissions,
        });
    }
    Verdict::Run(Box::new(config))
}

/// Why a `-w`, `-f` or `-z` value was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntervalProblem {
    NotNumber,
    TrailingGarbage,
    NotPositive,
}

impl IntervalProblem {
    fn text(self) -> &'static str {
        match self {
            Self::NotNumber => "value must be (suffixed) positive number",
            Self::TrailingGarbage => "value has trailing garbage",
            Self::NotPositive => "value must be positive",
        }
    }
}

/// Reads a duration: decimal digits, then optionally one unit letter.
// Compatibility: units s, m, h, d, w, M (31 days) and y (366 days); text
// after the unit letter is ignored (`1m30x` is 60 seconds); any other
// character after the digits is trailing garbage; a leading sign or space
// is not a number; an overlong number saturates (RCD-1).
// Provenance: black-box compatibility test.
fn parse_interval(text: &str) -> Result<Interval, IntervalProblem> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(IntervalProblem::NotNumber);
    }
    let number = text[..digits].parse::<i64>().unwrap_or(i64::MAX);
    let unit = match text.as_bytes().get(digits) {
        None | Some(b's') => 1,
        Some(b'm') => 60,
        Some(b'h') => 3600,
        Some(b'd') => 86_400,
        Some(b'w') => 604_800,
        Some(b'M') => 2_678_400,
        Some(b'y') => 31_622_400,
        Some(_) => return Err(IntervalProblem::TrailingGarbage),
    };
    let seconds = number.wrapping_mul(unit);
    if seconds <= 0 {
        return Err(IntervalProblem::NotPositive);
    }
    Ok(Interval::from_seconds(seconds))
}

/// A whole-string C `strtol`: optional white space and sign, then digits
/// of `radix` up to the end. `None` for anything else or an overflow.
fn c_long(text: &str, radix: u32) -> Option<i64> {
    let text = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let value = i64::from_str_radix(digits, radix).ok()?;
    Some(if negative { -value } else { value })
}

/// Runs the `rrdcached` personality and exits with its status.
pub(crate) fn rrdcached_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let verdict = read_options(args, &mut System, &mut std::io::stderr());
    let status = match verdict {
        Verdict::Exit { status, usage } => {
            let mut stdout = std::io::stdout();
            for _ in 0..usage {
                let _ = stdout.write_all(USAGE.as_bytes());
            }
            let _ = stdout.flush();
            status
        }
        Verdict::Run(config) => daemon::run(*config),
    };
    std::process::exit(status);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment that touches nothing: `bad` values fail, the rest
    /// succeed.
    struct Fake;

    impl Environment for Fake {
        fn base_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure> {
            if value.contains("bad") {
                return Err(OptionFailure {
                    status: 5,
                    message: format!("bad base {value}"),
                });
            }
            Ok(PathBuf::from(value))
        }

        fn journal_directory(&mut self, value: &str) -> Result<PathBuf, OptionFailure> {
            if value.contains("bad") {
                return Err(OptionFailure {
                    status: 6,
                    message: format!("bad journal {value}"),
                });
            }
            Ok(PathBuf::from(value))
        }

        fn log_file(&mut self, value: &str) -> Result<LogFile, OptionFailure> {
            if value.contains("bad") {
                return Err(OptionFailure {
                    status: 6,
                    message: format!("bad log {value}"),
                });
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap();
            Ok(LogFile {
                path: PathBuf::from(value),
                file,
            })
        }

        fn user(&mut self, value: &str) -> Option<Account> {
            (value == "alice").then(|| Account {
                name: c"alice".to_owned(),
                uid: 1000,
                gid: 1000,
            })
        }

        fn group(&mut self, value: &str) -> Option<u32> {
            (value == "staff").then_some(20)
        }

        fn socket_group(&mut self, value: &str) -> Option<u32> {
            self.group(value)
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    fn read(list: &[&str]) -> (Verdict, String) {
        let mut errors = Vec::new();
        let verdict = read_options(&words(list), &mut Fake, &mut errors);
        (verdict, String::from_utf8(errors).unwrap())
    }

    fn exit(list: &[&str]) -> (i32, usize, String) {
        match read(list) {
            (Verdict::Exit { status, usage }, errors) => (status, usage, errors),
            (Verdict::Run(_), errors) => panic!("{list:?} would run: {errors}"),
        }
    }

    fn parsed(list: &[&str]) -> (DaemonConfig, String) {
        match read(list) {
            (Verdict::Run(config), errors) => (*config, errors),
            (verdict, errors) => panic!("{list:?}: {verdict:?} {errors}"),
        }
    }

    #[test]
    fn usage_is_printed_for_help_and_unknown_options() {
        assert_eq!(exit(&["-h"]), (1, 1, String::new()));
        assert_eq!(exit(&["--help"]), (1, 1, String::new()));
        assert_eq!(exit(&["-x"]), (0, 1, "invalid option -- 'x'\n".to_owned()));
        assert_eq!(
            exit(&["--bogus"]),
            (0, 1, "invalid option -- 'bogus'\n".to_owned())
        );
        assert_eq!(
            exit(&["--help=1"]),
            (0, 1, "option takes no arguments -- 'help'\n".to_owned())
        );
        assert_eq!(
            exit(&["-w"]),
            (0, 1, "option requires an argument -- 'w'\n".to_owned())
        );
        assert_eq!(exit(&["--h"]), (0, 1, "invalid option -- 'h'\n".to_owned()));
    }

    #[test]
    fn the_last_status_setter_wins() {
        let interval = "Invalid write interval 0: value must be positive\n";
        assert_eq!(exit(&["-w", "0", "-x"]).0, 0);
        assert_eq!(
            exit(&["-x", "-w", "0"]),
            (2, 1, format!("invalid option -- 'x'\n{interval}"))
        );
        assert_eq!(exit(&["-h", "-w", "0"]).0, 2);
        assert_eq!(exit(&["-w", "0", "-h"]).0, 1);
        assert_eq!(exit(&["-w", "0", "-w", "5"]).0, 2);
        assert_eq!(exit(&["-h", "-x"]).0, 0);
        // A valid -P name clears an earlier error; a later bad one sets 4.
        let (config, errors) = parsed(&["-w", "bad", "-P", "FOO,PING"]);
        assert!(
            errors.starts_with(
                "Invalid write interval bad: value must be (suffixed) positive number\n"
            )
        );
        assert_eq!(
            config.listeners[0].permissions,
            Some(vec!["PING".to_owned()])
        );
        assert_eq!(exit(&["-P", "PING,nosuch"]).0, 4);
        assert_eq!(exit(&["-P", "BOGUS", "-P", ""]).0, 4);
    }

    #[test]
    fn stopping_errors_end_reading_and_skip_warnings() {
        assert_eq!(
            exit(&["-t", "0", "-w", "0"]),
            (1, 0, "Invalid thread count: -t 0\n".to_owned())
        );
        assert_eq!(exit(&["-t", "0", "-P", "PING"]).0, 1);
        assert_eq!(exit(&["-a", "0", "-P", "PING"]).0, 10);
        assert_eq!(
            exit(&["-m", "999"]),
            (
                5,
                0,
                "read_options: Invalid file mode \"999\".\n".to_owned()
            )
        );
        assert_eq!(
            exit(&["-s", "nogroup"]),
            (
                5,
                0,
                "read_options: couldn't map \"nogroup\" to a group, Sorry\n".to_owned()
            )
        );
        assert_eq!(
            exit(&["-U", "nouser"]),
            (
                5,
                0,
                "read_options: couldn't map \"nouser\" to a user, Sorry\n".to_owned()
            )
        );
        assert_eq!(exit(&["-G", "nogroup"]).0, 5);
        assert_eq!(
            exit(&["-b", "bad", "-P", "PING"]),
            (5, 0, "bad base bad\n".to_owned())
        );
        assert_eq!(exit(&["-j", "bad", "-P", "PING"]).0, 6);
        assert_eq!(exit(&["-o", "bad", "-P", "PING"]).0, 6);
        assert_eq!(exit(&["-R", "-t", "0"]).2, "Invalid thread count: -t 0\n");
        // Usage asked for earlier is still printed after a stop.
        assert_eq!(
            exit(&["-h", "-t", "0"]),
            (1, 1, "Invalid thread count: -t 0\n".to_owned())
        );
    }

    #[test]
    fn warnings_follow_errors_unless_reading_stopped() {
        let (status, usage, errors) =
            exit(&["-R", "-w", "10", "-f", "19", "-z", "11", "-P", "BOGUS"]);
        assert_eq!((status, usage), (4, 0));
        assert_eq!(
            errors,
            "read_options: Adding permission \"BOGUS\" to socket failed. Most likely, this permission doesn't exist. Check your command line.\n\
             WARNING: flush interval (-f) should be at least 2x write interval (-w) !\n\
             WARNING: write delay (-z) should NOT be larger than write interval (-w) !\n\
             WARNING: -R does not make sense without -B!\n  Consult the rrdcached documentation\n"
        );
        let (_, errors) = parsed(&["-B", "-bdata", "-R"]);
        assert_eq!(errors, "");
        let (_, errors) = parsed(&["-B", "-R"]);
        assert_eq!(
            errors,
            "WARNING: -B does not make sense without -b!\n  Consult the rrdcached documentation\n"
        );
    }

    #[test]
    fn listeners_take_the_settings_before_them() {
        let (config, _) = parsed(&[
            "-gB",
            "-bdata",
            "-m",
            "0600",
            "-P",
            "PING",
            "-l",
            "unix:first.sock",
            "-s",
            "staff",
            "-P",
            "PING,STATS",
            "-l",
            "/second.sock",
            "-l",
            "127.0.0.1:5",
            "-L",
            "-m",
            "0666",
            "-P",
            "UPDATE",
        ]);
        assert!(config.confine_to_base);
        assert_eq!(config.base, Some(PathBuf::from("data")));
        let summary = config
            .listeners
            .iter()
            .map(|spec| {
                (
                    spec.address.clone(),
                    spec.access.mode(),
                    spec.permissions.clone(),
                )
            })
            .collect::<Vec<_>>();
        let ping = Some(vec!["PING".to_owned()]);
        let ping_stats = Some(vec!["PING".to_owned(), "STATS".to_owned()]);
        assert_eq!(
            summary,
            vec![
                (ListenAddress::Unix("first.sock".into()), 0o600, ping),
                (
                    ListenAddress::Unix("/second.sock".into()),
                    0o600,
                    ping_stats.clone()
                ),
                (
                    ListenAddress::Network("127.0.0.1:5".into()),
                    0o600,
                    ping_stats.clone()
                ),
                (ListenAddress::Network(String::new()), 0o600, ping_stats),
            ]
        );
        assert_eq!(config.listeners[1].access.group, Some(20));
    }

    #[test]
    fn default_listener_uses_the_final_settings() {
        let (config, _) = parsed(&["-P", "PING", "-s", "staff", "extra-word"]);
        assert_eq!(config.listeners.len(), 1);
        let spec = &config.listeners[0];
        assert_eq!(spec.address, ListenAddress::Unix(DEFAULT_SOCKET.into()));
        assert_eq!(spec.access.mode(), 0o760);
        assert_eq!(spec.permissions, Some(vec!["PING".to_owned()]));
        // An empty list removes the restriction.
        let (config, _) = parsed(&["-P", "PING", "-P", ",,"]);
        assert_eq!(config.listeners[0].permissions, None);
    }

    #[test]
    fn option_values_are_read_like_the_reference() {
        let (config, errors) = parsed(&[
            "-w1800", "-f", "1h", "-z", "5m", "-t", "+2", "-a", " 3", "-V", "LOG_INFO", "-U",
            "alice", "-G", "staff", "-F", "-O", "-p", "pid", "-j", "j", "-o", "log",
        ]);
        assert_eq!(errors, "");
        assert_eq!(config.write_interval, Interval::from_seconds(1800));
        assert_eq!(config.flush_interval, Interval::from_seconds(3600));
        assert_eq!(config.write_jitter, Interval::from_seconds(300));
        assert_eq!((config.write_threads, config.allocation_chunk), (2, 3));
        assert_eq!(config.log_level, LogLevel::Info);
        assert_eq!(config.user.as_ref().map(|user| user.uid), Some(1000));
        assert_eq!(config.group, Some(20));
        assert!(config.flush_at_shutdown && config.no_overwrite);
        assert_eq!(config.pid_file, Some(PathBuf::from("pid")));
        assert_eq!(config.journal, Some(PathBuf::from("j")));
        assert!(config.log_file.is_some());
        let (config, errors) = parsed(&["-V", "LOG_BOGUS"]);
        assert_eq!(
            errors,
            "Unrecognized log level 'LOG_BOGUS'; falling back to default LOG_ERR.\n"
        );
        assert_eq!(config.log_level, LogLevel::Err);
    }

    #[test]
    fn an_unknown_letter_moves_where_the_next_word_is_read() {
        let usage_and_errors = |list: &[&str]| {
            let (status, usage, errors) = exit(list);
            (status, usage, errors)
        };
        // At the start of a word the rest of it is skipped.
        assert_eq!(
            usage_and_errors(&["-xh"]),
            (0, 1, "invalid option -- 'x'\n".to_owned())
        );
        assert_eq!(
            usage_and_errors(&["-xh", "-w", "0"]),
            (
                2,
                1,
                "invalid option -- 'x'\nInvalid write interval 0: value must be positive\n"
                    .to_owned()
            )
        );
        // Each -h and each rejected word prints the usage once more.
        assert_eq!(
            usage_and_errors(&["-hx"]),
            (0, 2, "invalid option -- 'x'\n".to_owned())
        );
        // Further in, the next option word is read from that position.
        let empty = "invalid option -- '\n";
        assert_eq!(
            usage_and_errors(&["-gx", "-h"]),
            (0, 2, format!("invalid option -- 'x'\n{empty}"))
        );
        assert_eq!(
            usage_and_errors(&["-gx", "-w", "5", "-h"]),
            (0, 3, format!("invalid option -- 'x'\n{empty}{empty}"))
        );
        assert_eq!(
            usage_and_errors(&["-gx", "--bogus", "-h"]),
            (
                0,
                3,
                format!("invalid option -- 'x'\ninvalid option -- 'bogus'\n{empty}")
            )
        );
        assert_eq!(usage_and_errors(&["-gx", "-gB", "-h"]).0, 1);
        assert_eq!(usage_and_errors(&["-gx", "-BBh"]).0, 1);
        assert_eq!(
            usage_and_errors(&["-gx", "--", "-h"]),
            (0, 1, "invalid option -- 'x'\n".to_owned())
        );
        assert_eq!(
            exit(&["-gx", "-abbad"]),
            (5, 1, "invalid option -- 'x'\nbad base bad\n".to_owned())
        );
        assert_eq!(
            usage_and_errors(&["-gBx", "-abcd"]).2,
            "invalid option -- 'x'\ninvalid option -- 'c'\nWARNING: -B does not make sense without -b!\n  Consult the rrdcached documentation\n"
        );
    }

    #[test]
    fn intervals_follow_the_reference_grammar() {
        for (text, seconds) in [
            ("5", 5),
            ("010", 10),
            ("5s", 5),
            ("5m", 300),
            ("1m30x", 60),
            ("5mm", 300),
            ("1h", 3600),
            ("1d1", 86_400),
            ("1w", 604_800),
            ("1M", 2_678_400),
            ("1y", 31_622_400),
            ("99999999999999999999", i64::MAX),
        ] {
            assert_eq!(
                parse_interval(text),
                Ok(Interval::from_seconds(seconds)),
                "{text}"
            );
        }
        for (text, problem) in [
            ("", IntervalProblem::NotNumber),
            ("x", IntervalProblem::NotNumber),
            ("-5", IntervalProblem::NotNumber),
            ("+5", IntervalProblem::NotNumber),
            (" 5", IntervalProblem::NotNumber),
            ("5x", IntervalProblem::TrailingGarbage),
            ("5S", IntervalProblem::TrailingGarbage),
            ("5.5", IntervalProblem::TrailingGarbage),
            ("30 ", IntervalProblem::TrailingGarbage),
            ("0", IntervalProblem::NotPositive),
            ("0m5", IntervalProblem::NotPositive),
        ] {
            assert_eq!(parse_interval(text), Err(problem), "{text}");
        }
    }

    #[test]
    fn numbers_follow_c_strtol() {
        assert_eq!(c_long(" +12", 10), Some(12));
        assert_eq!(c_long("02", 10), Some(2));
        assert_eq!(c_long("-1", 10), Some(-1));
        assert_eq!(c_long("0640", 8), Some(0o640));
        for text in ["", "1x", "08", "+", " ", "99999999999999999999"] {
            let radix = if text == "08" { 8 } else { 10 };
            assert_eq!(c_long(text, radix), None, "{text}");
        }
        assert_eq!(exit(&["-t", "2147483648"]).0, 1);
        assert_eq!(
            exit(&["-t", "", "-w", "0"]),
            (1, 0, "Missing argument for -t\n".to_owned())
        );
        assert_eq!(
            exit(&["-a", ""]),
            (10, 0, "Missing argument for -a\n".to_owned())
        );
        assert_eq!(exit(&["-a", "2147483648"]).0, 10);
        assert_eq!(exit(&["-m", "10000"]).0, 5);
        assert_eq!(exit(&["-m", "-1"]).0, 5);
        let (config, _) = parsed(&["-m", "07777", "-l", "/s"]);
        assert_eq!(config.listeners[0].access.mode(), 0o7777);
    }

    #[test]
    fn usage_text_matches_the_recorded_bytes() {
        assert!(USAGE.starts_with("RRDCacheD 1.11.0\nCopyright (C) 2008,2009"));
        assert!(
            USAGE.contains(
                "\n  -z <delay>    Delay writes up to <delay> seconds to spread load\n\n"
            )
        );
        assert!(USAGE.ends_with("refer\nto the rrdcached(1) manual page.\n"));
    }

    /// Deterministic random argument lists never panic and always end in
    /// a verdict.
    #[test]
    fn random_command_lines_never_panic() {
        const WORDS: &[&str] = &[
            "-a",
            "-B",
            "-b",
            "-F",
            "-f",
            "-G",
            "-g",
            "-h",
            "-j",
            "-L",
            "-l",
            "-m",
            "-O",
            "-o",
            "-P",
            "-p",
            "-R",
            "-s",
            "-t",
            "-U",
            "-V",
            "-w",
            "-z",
            "--help",
            "--",
            "-",
            "---",
            "--x=1",
            "-gB",
            "-w5",
            "0",
            "1",
            "-1",
            "5m",
            "1y",
            "99999999999999999999",
            "x",
            "",
            " ",
            "bad",
            "PING",
            "nosuch,PING",
            "PING,",
            "LOG_DEBUG",
            "staff",
            "alice",
            "unix:",
            "/s",
            "[::1",
            "é",
            "\u{0}",
            "0777",
            "07777",
            "1m30x",
            ",,",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let length = (next() % 9) as usize;
            let args = (0..length)
                .map(|_| {
                    let word = WORDS[(next() % WORDS.len() as u64) as usize];
                    if next() % 5 == 0 {
                        format!("{word}{}", WORDS[(next() % WORDS.len() as u64) as usize])
                    } else {
                        word.to_owned()
                    }
                })
                .collect::<Vec<_>>();
            let mut errors = Vec::new();
            match read_options(&args, &mut Fake, &mut errors) {
                Verdict::Exit { status, .. } => assert!((0..=10).contains(&status), "{args:?}"),
                Verdict::Run(config) => assert!(!config.listeners.is_empty(), "{args:?}"),
            }
        }
    }
}
