// The `rrdtool` command-line front end.
//
// Selects a command from the first argument, prints usage, help and
// version text, reports command errors, runs pipe mode (`rrdtool -`) and
// provides the `rrdupdate`, `rrdcreate` and `rrdinfo` programs. The
// commands themselves live in the parent module; they receive their
// arguments with the command name first and report failures as errors.
//
// Behavior follows docs/compatibility/cli.md and
// docs/compatibility/pipe-mode.md, which were written from the manual
// pages and black-box runs of RRDtool 1.11.0.

/// `rrdtool list` without a daemon.
#[cfg(unix)]
pub(crate) mod list;
/// Command option scanning.
///
/// Every `rrdtool` command except `resize` describes its options with a
/// table of [`OptionSpec`] entries and walks the words after the command
/// name with an [`OptionScanner`]. The scanner yields options in the order
/// they appear, collects the remaining words as operands and stops nothing
/// on its own: a command that should stop at the first error returns it.
///
/// The rules are those of docs/compatibility/cli.md, section 3.
pub(crate) mod options;
/// `rrdtool -`: commands read from standard input, one per line.
///
/// The rules are those of docs/compatibility/pipe-mode.md. Input is handled
/// as bytes throughout; arguments become strings only when they are handed
/// to a command.
mod pipe;

pub(crate) use options::{OptionError, OptionScanner, OptionSpec};

use std::error::Error;
use std::io::{self, Write};

type CommandResult = Result<(), Box<dyn Error>>;

/// An error message carried as bytes, for text that need not be UTF-8.
#[derive(Debug)]
pub(crate) struct RawMessage(pub(crate) Vec<u8>);

impl std::fmt::Display for RawMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl Error for RawMessage {}

// The usage pages share their pieces; `concat!` needs literals, so the
// pieces are macros.
#[cfg(test)]
macro_rules! banner {
    () => {
        "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n"
    };
}
#[cfg(test)]
macro_rules! command_list {
    () => {
        "Valid commands: create, update, updatev, graph, graphv,  dump, restore,\n\t\tlast, lastupdate, first, info, list, fetch, tune,\n\t\tresize, xport, flushcached\n"
    };
}
#[cfg(test)]
macro_rules! notice {
    () => {
        "\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
    };
}

#[cfg(test)]
const USAGE: &str = concat!(banner!(), command_list!(), notice!());
#[cfg(test)]
const PIPE_USAGE: &str = concat!(
    banner!(),
    command_list!(),
    "\nValid remote commands: quit, ls, cd, mkdir, pwd\n",
    notice!()
);
const VERSION_LINE: &str = "RRDtool 1.11.0  Copyright by Tobi Oetiker (1.011000)\n";

/// A command that runs from the command line and in pipe mode.
struct Command {
    name: &'static str,
    run: fn(&[String]) -> CommandResult,
    #[cfg(test)]
    help: &'static str,
    /// `first` and `last` print `-1` before their error.
    // Compatibility: the failed time is printed for option and usage
    // errors too, and in pipe mode.
    // Provenance: black-box compatibility test (CLI-36, PM-15).
    minus_one_on_error: bool,
}

const fn command(
    name: &'static str,
    run: fn(&[String]) -> CommandResult,
    #[allow(unused_variables)] help: &'static str,
) -> Command {
    Command {
        name,
        run,
        #[cfg(test)]
        help,
        minus_one_on_error: false,
    }
}

static COMMANDS: &[Command] = &[
    command(
        "create",
        super::rrdtool_create,
        include_str!("../help/create.txt"),
    ),
    command(
        "update",
        super::rrdtool_update,
        include_str!("../help/update.txt"),
    ),
    command(
        "updatev",
        super::rrdtool_updatev,
        include_str!("../help/updatev.txt"),
    ),
    command(
        "graph",
        |args| super::rrdtool_graph(args, false),
        include_str!("../help/graph.txt"),
    ),
    command(
        "graphv",
        |args| super::rrdtool_graph(args, true),
        include_str!("../help/graphv.txt"),
    ),
    command(
        "dump",
        super::rrdtool_dump,
        include_str!("../help/dump.txt"),
    ),
    command(
        "restore",
        super::rrdtool_restore,
        include_str!("../help/restore.txt"),
    ),
    Command {
        minus_one_on_error: true,
        ..command(
            "last",
            super::rrdtool_last,
            include_str!("../help/last.txt"),
        )
    },
    command(
        "lastupdate",
        super::rrdtool_lastupdate,
        include_str!("../help/lastupdate.txt"),
    ),
    Command {
        minus_one_on_error: true,
        ..command(
            "first",
            super::rrdtool_first,
            include_str!("../help/first.txt"),
        )
    },
    command(
        "info",
        super::rrdtool_info,
        include_str!("../help/info.txt"),
    ),
    command(
        "list",
        super::rrdtool_list,
        include_str!("../help/list.txt"),
    ),
    command(
        "fetch",
        super::rrdtool_fetch,
        include_str!("../help/fetch.txt"),
    ),
    command(
        "tune",
        super::rrdtool_tune,
        include_str!("../help/tune.txt"),
    ),
    command(
        "resize",
        super::rrdtool_resize,
        include_str!("../help/resize.txt"),
    ),
    command(
        "xport",
        super::rrdtool_xport,
        include_str!("../help/xport.txt"),
    ),
    command(
        "flushcached",
        super::rrdtool_flushcached,
        include_str!("../help/flushcached.txt"),
    ),
];

/// Help pages for the pipe-mode commands, which `rrdtool help` also knows.
#[cfg(test)]
static SESSION_HELP: &[(&str, &str)] = &[
    ("cd", include_str!("../help/cd.txt")),
    ("mkdir", include_str!("../help/mkdir.txt")),
    ("pwd", include_str!("../help/pwd.txt")),
    ("ls", include_str!("../help/ls.txt")),
    ("quit", include_str!("../help/quit.txt")),
];

fn find_command(name: &[u8]) -> Option<&'static Command> {
    COMMANDS
        .iter()
        .find(|command| command.name.as_bytes() == name)
}

#[cfg(test)]
fn help_page(topic: &str) -> Option<&'static str> {
    find_command(topic.as_bytes())
        .map(|command| command.help)
        .or_else(|| {
            SESSION_HELP
                .iter()
                .find(|(name, _)| *name == topic)
                .map(|(_, page)| *page)
        })
}

// Compatibility: error messages are cut to 4095 bytes, even inside a
// multibyte character.
// Provenance: black-box compatibility test (CLI-35, cli-truncate-014 to
// -017).
const ERROR_MESSAGE_LIMIT: usize = 4095;

fn message_bytes_for(error: &(dyn Error + 'static), locale: rondi::Locale) -> Vec<u8> {
    let mut bytes = if let Some(option) = error.downcast_ref::<OptionError>() {
        option.message_bytes_for(locale)
    } else if let Some(raw) = error.downcast_ref::<RawMessage>() {
        raw.0.clone()
    } else {
        error.to_string().into_bytes()
    };
    bytes.truncate(ERROR_MESSAGE_LIMIT);
    bytes
}

#[cfg(test)]
fn message_bytes(error: &(dyn Error + 'static)) -> Vec<u8> {
    message_bytes_for(error, rondi::Locale::En)
}

fn error_line(message: &[u8]) -> Vec<u8> {
    [b"ERROR: ", message, b"\n"].concat()
}

fn write_stdout(bytes: &[u8]) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

fn write_stderr(bytes: &[u8]) {
    let _ = io::stderr().lock().write_all(bytes);
}

/// Runs a command and returns its error message, if any.
fn execute(command: &Command, args: &[String], locale: rondi::Locale) -> Result<(), Vec<u8>> {
    let result = (command.run)(args);
    let _ = io::stdout().flush();
    let Err(error) = result else {
        return Ok(());
    };
    if command.minus_one_on_error {
        write_stdout(b"-1\n");
    }
    let message = message_bytes_for(error.as_ref(), locale);
    // Compatibility: a failure that leaves no message is reported as a
    // success: no ERROR line, status 0, and `OK` in pipe mode.
    // Provenance: black-box compatibility test (rrdcached_client_paths.rs,
    // a client path that RRDCACHED_STRIPPATH rejects).
    if message.is_empty() {
        return Ok(());
    }
    Err(message)
}

/// The `rrdtool` program. `argv` includes the program name.
pub(crate) fn run_rrdtool(argv: &[String]) -> i32 {
    let locale = super::i18n::messages_locale();
    let args = argv.get(1..).unwrap_or_default();
    let status = match args {
        [] => {
            write_stdout(super::i18n::rrdtool_usage(locale).as_bytes());
            0
        }
        [only] if only == "-" => run_pipe_mode(None, locale),
        [only] => {
            let text = super::i18n::rrdtool_help(only, locale)
                .unwrap_or_else(|| super::i18n::rrdtool_usage(locale));
            write_stdout(text.as_bytes());
            0
        }
        [first, rest @ ..] => match first.as_str() {
            "help" | "--help" | "-h" | "-?" => {
                let page = match rest {
                    [topic] if first == "help" => super::i18n::rrdtool_help(topic, locale),
                    _ => None,
                };
                let text = page.unwrap_or_else(|| super::i18n::rrdtool_usage(locale));
                write_stdout(text.as_bytes());
                0
            }
            "version" | "--version" | "-v" => {
                write_stdout(VERSION_LINE.as_bytes());
                0
            }
            "-" if rest.len() == 1 => run_pipe_mode(Some(&rest[0]), locale),
            name => match find_command(name.as_bytes()) {
                Some(command) => match execute(command, args, locale) {
                    Ok(()) => 0,
                    Err(message) => {
                        write_stderr(&error_line(&message));
                        1
                    }
                },
                None => {
                    let mut message = if locale == rondi::Locale::En {
                        [b"unknown function '", name.as_bytes(), b"'"].concat()
                    } else {
                        let cat = super::i18n::catalog(locale);
                        super::i18n::fmt1(cat.rrdtool_unknown_function, name).into_bytes()
                    };
                    message.truncate(ERROR_MESSAGE_LIMIT);
                    write_stderr(&error_line(&message));
                    1
                }
            },
        },
    };
    let _ = io::stdout().flush();
    status
}

fn run_pipe_mode(directory: Option<&String>, locale: rondi::Locale) -> i32 {
    if let Some(directory) = directory.filter(|directory| !directory.is_empty())
        && let Err(errno) = pipe::change_dir(directory.as_bytes())
    {
        let text = system_error_text(errno);
        write_stderr(format!("ERROR: chdir {directory} {text}\n").as_bytes());
        return errno;
    }
    let mut input = io::stdin().lock();
    pipe::run_session(
        &mut input,
        locale,
        |name| find_command(name).is_some(),
        |args| match find_command(args[0].as_bytes()) {
            Some(command) => execute(command, args, locale),
            None => Err(super::i18n::catalog(locale)
                .rrdtool_unknown_function_bare
                .as_bytes()
                .to_vec()),
        },
    );
    0
}

/// The single-command programs installed next to `rrdtool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Alias {
    Update,
    Create,
    Info,
}

impl Alias {
    pub(crate) fn from_program(name: &str) -> Option<Self> {
        match name {
            "rrdupdate" => Some(Self::Update),
            "rrdcreate" => Some(Self::Create),
            "rrdinfo" => Some(Self::Info),
            _ => None,
        }
    }

    fn command(self) -> Option<&'static Command> {
        let name: &[u8] = match self {
            Self::Update => b"update",
            Self::Create => b"create",
            Self::Info => b"info",
        };
        find_command(name)
    }

    fn usage_for(self, locale: rondi::Locale) -> &'static str {
        let cat = super::i18n::catalog(locale);
        match self {
            Self::Update => cat.alias_usage_update,
            Self::Create => cat.alias_usage_create,
            Self::Info => cat.alias_usage_info,
        }
    }

    /// The error for a run with no arguments at all.
    fn no_arguments_for(self, program: &str, locale: rondi::Locale) -> Vec<u8> {
        let cat = super::i18n::catalog(locale);
        match self {
            Self::Update => cat.alias_not_enough_args.as_bytes().to_vec(),
            Self::Create => cat.alias_need_rrd_name.as_bytes().to_vec(),
            Self::Info => super::i18n::fmt1(cat.alias_info_usage_error, program).into_bytes(),
        }
    }
}

/// `rrdupdate`, `rrdcreate` or `rrdinfo`. `argv` includes the program
/// name, which the command sees in place of its own name.
pub(crate) fn run_alias(alias: Alias, argv: &[String]) -> i32 {
    let locale = super::i18n::messages_locale();
    let program = argv.first().map_or("", String::as_str);
    let outcome = if argv.len() <= 1 {
        Err(alias.no_arguments_for(program, locale))
    } else {
        match alias.command() {
            Some(command) => execute(command, argv, locale),
            None => Err(super::i18n::catalog(locale)
                .rrdtool_unknown_function_bare
                .as_bytes()
                .to_vec()),
        }
    };
    let status = match outcome {
        Ok(()) => 0,
        Err(message) => {
            // Compatibility: these programs report errors on standard
            // output after a short banner and their usage text.
            // Provenance: black-box compatibility test (CLI-54).
            let mut text = b"RRDtool 1.11.0  Copyright by Tobi Oetiker\n\n".to_vec();
            text.extend_from_slice(alias.usage_for(locale).as_bytes());
            text.extend_from_slice(&error_line(&message));
            write_stdout(&text);
            1
        }
    };
    let _ = io::stdout().flush();
    status
}

/// The C library's text for an error number.
pub(crate) fn system_error_text(errno: libc::c_int) -> String {
    // SAFETY: strerror returns a pointer to a NUL-terminated string that
    // stays valid until the next call; it is copied before returning.
    let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) };
    text.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn os_path(bytes: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn os_path(bytes: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(unix)]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &std::path::Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_pages_are_assembled_from_their_parts() {
        assert!(USAGE.starts_with(banner!()));
        assert!(USAGE.ends_with("For more information read the RRD manpages\n\n"));
        assert!(!USAGE.contains("remote"));
        assert!(PIPE_USAGE.contains(
            "flushcached\n\nValid remote commands: quit, ls, cd, mkdir, pwd\n\nRRDtool is"
        ));
    }

    #[test]
    fn every_help_page_has_the_common_frame() {
        let pages = COMMANDS
            .iter()
            .map(|command| command.help)
            .chain(SESSION_HELP.iter().map(|(_, page)| *page));
        for page in pages {
            assert!(page.starts_with(banner!()), "{page}");
            assert!(page.ends_with(notice!()), "{page}");
        }
    }

    #[test]
    fn command_names_match_exactly() {
        assert!(find_command(b"last").is_some());
        for name in ["Last", "las", "lastx", "", "cd", "help"] {
            assert!(find_command(name.as_bytes()).is_none(), "{name}");
        }
        assert!(help_page("cd").is_some());
        assert!(help_page("LAST").is_none());
    }

    #[test]
    fn long_messages_are_cut_to_4095_bytes() {
        let error: Box<dyn Error> = "é".repeat(3000).into();
        let bytes = message_bytes(error.as_ref());
        assert_eq!(bytes.len(), ERROR_MESSAGE_LIMIT);
        assert!(std::str::from_utf8(&bytes).is_err());
        let raw: Box<dyn Error> = Box::new(RawMessage(vec![0xe9, b'x']));
        assert_eq!(message_bytes(raw.as_ref()), vec![0xe9, b'x']);
    }

    #[test]
    fn aliases_come_from_the_program_name() {
        assert_eq!(Alias::from_program("rrdinfo"), Some(Alias::Info));
        assert_eq!(Alias::from_program("rrdtool"), None);
        assert_eq!(
            Alias::Update.command().map(|command| command.name),
            Some("update")
        );
        assert_eq!(
            Alias::Create.command().map(|command| command.name),
            Some("create")
        );
        assert_eq!(
            Alias::Info.command().map(|command| command.name),
            Some("info")
        );
    }

    /// Seeded xorshift64*, so a failing case reproduces.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % bound as u64) as usize
        }
    }

    #[test]
    fn random_words_and_lines_never_panic() {
        const TABLE: &[OptionSpec<char>] = &[
            OptionSpec::value("start", Some('s'), 's'),
            OptionSpec::flag("align-start", Some('a'), 'a'),
            OptionSpec::value("rraindex", None, 'i'),
            OptionSpec::flag("noflush", Some('F'), 'F'),
        ];
        const WORDS: &[&str] = &[
            "-",
            "--",
            "---",
            "-s",
            "-sa",
            "-as",
            "-aF",
            "-Q",
            "-é",
            "--start",
            "--start=",
            "--sta",
            "--noflush=1",
            "--rraindex",
            "--=",
            "x",
            "",
            "é",
            "\u{1f600}",
            "-\u{1f600}",
        ];
        const BYTES: &[u8] = b" \t\r\n\0\x01\x7f\x80\xe9\xff'\"#\\-ab";
        let mut rng = Rng(0x0dd5_eed5);
        for _ in 0..20_000 {
            let words = (0..rng.below(7))
                .map(|_| WORDS[rng.below(WORDS.len())].to_owned())
                .collect::<Vec<_>>();
            let mut scanner = OptionScanner::new(&words, TABLE);
            let mut seen = 0;
            for item in scanner.by_ref() {
                if let Err(error) = item {
                    assert!(error.message_bytes().len() <= 47);
                }
                seen += 1;
                assert!(seen <= words.len() * 4, "{words:?}");
            }
            assert!(scanner.operands().len() <= words.len());
            assert!(scanner.rearranged().len() <= words.len() + 1);

            let input = (0..rng.below(24))
                .map(|_| BYTES[rng.below(BYTES.len())])
                .collect::<Vec<_>>();
            let mut reader = input.as_slice();
            while let Some(line) = pipe::read_line(&mut reader).expect("in-memory read") {
                assert!(line.bytes.len() <= input.len());
                let trimmed = pipe::trim(&line.bytes);
                if let Some(words) = pipe::split(trimmed) {
                    let _ = pipe::classify(words, |name| find_command(name).is_some());
                }
            }
        }
    }
}
