/// C library conversions RRDtool's command parsers call, either through
/// libc itself or as ports of RRDtool's own helpers in rrd_strtod.c.
mod cparse;
/// Option parsing for `graph`, `graphv` and `xport`, ported from
/// `rrd_graph_options` (rrd_graph.c:5042) and `rrd_xport` (rrd_xport.c:76).
/// Options that only affect Cairo/Pango rendering are validated exactly as
/// upstream validates them and then left unused.
mod graph_options;
/// Port of RRDtool 1.11.0 `src/optparse.c`, the option parser every
/// `rrdtool` command uses. Non-option words are permuted to the end of
/// `argv`, so after the loop `argv[optind..]` holds the positionals in their
/// original order.
mod optparse;

use optparse::{ArgType, LongOpt, OptParse, opt};

use clap::{Parser, Subcommand};
use rondi::time::{
    UpdateTimestamp, parse_rrd_update_timestamp, resolve_rrd_range_times, rrd_parsetime,
    rrd_proc_start_end,
};
use rondi::{
    DEFAULT_IDEMPOTENCY_WINDOW, DEFAULT_MAX_ROWS, DatabaseConfig, RrdDataSourceTune, RrdDumpHeader,
    RrdResizeAction, RrdTuneBound, Store, StoreOptions, Update, dump_rrd_file_with_header,
    fetch_rrd_file, first_rrd_time, parse_rrd_scaled_duration, resize_rrd_file, restore_rrd_file,
    tune_rrd_data_sources, update_rrd_text,
};
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "rondi", version, about = "Round-robin time-series storage")]
struct Args {
    /// Use the server's Unix socket instead of local storage.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Local storage root.
    #[arg(long, global = true, default_value = "./data")]
    root: PathBuf,
    /// Largest archive row count accepted by create and import.
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_ROWS)]
    max_rows: usize,
    /// Seconds a server update's request ID stays available for retries.
    #[arg(long, global = true, default_value_t = DEFAULT_IDEMPOTENCY_WINDOW.as_secs())]
    idempotency_window: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the local server using this same executable.
    Server {
        #[arg(long, default_value = "./run/rondi.sock")]
        listen: PathBuf,
        #[arg(long, default_value_t = 256)]
        queue_capacity: usize,
    },
    Create {
        name: String,
        #[arg(long)]
        step: u64,
        #[arg(long)]
        heartbeat: u64,
        #[arg(long)]
        rows: usize,
        #[arg(long)]
        start: i64,
    },
    Update {
        name: String,
        timestamp: i64,
        value: String,
    },
    Fetch {
        name: String,
    },
    Health,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let invoked_as = std::env::args_os()
        .next()
        .and_then(|value| {
            PathBuf::from(value)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--rrdproxy-launcher")) {
        return rrdproxy_mode(&std::env::args().skip(2).collect::<Vec<_>>());
    }
    if invoked_as == "rrdcached" {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        return rrdcached_mode(&args).await;
    }
    if invoked_as == "rrdtool" {
        initialize_rrdtool_locale();
        // RRDtool passes argv bytes through; Rust strings cannot hold
        // invalid UTF-8, so such bytes become U+FFFD instead of panicking.
        let args = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let status = rrdtool_main(&args);
        let _ = std::io::stdout().flush();
        std::process::exit(status);
    }
    if invoked_as == "rrdtool-proxy"
        || invoked_as == "rrdtool-proxy.php"
        || invoked_as == "rrdproxy"
    {
        return rrdproxy_mode(&std::env::args().skip(1).collect::<Vec<_>>());
    }
    let args = Args::parse();
    if let Some(socket) = args.socket {
        return server_mode(socket, args.command).await;
    }
    let options = StoreOptions {
        max_rows: args.max_rows,
        idempotency_window: std::time::Duration::from_secs(args.idempotency_window),
    };
    let store = Store::open_with(&args.root, options)?;
    match args.command {
        Command::Server {
            listen,
            queue_capacity,
        } => {
            drop(store);
            return rondi_server::run(rondi_server::ServerConfig {
                root: args.root,
                socket: listen,
                queue_capacity,
                store: options,
            })
            .await;
        }
        Command::Create {
            name,
            step,
            heartbeat,
            rows,
            start,
        } => store.create(
            &name,
            DatabaseConfig {
                step,
                heartbeat,
                rows,
                start,
            },
        )?,
        Command::Update {
            name,
            timestamp,
            value,
        } => store.update(
            &name,
            Update {
                timestamp,
                value: parse_value(&value)?,
            },
        )?,
        Command::Fetch { name } => {
            println!("{}", serde_json::to_string_pretty(&store.fetch(&name)?)?)
        }
        Command::Health => println!("{{\"status\":\"ok\",\"format_version\":1}}"),
    }
    Ok(())
}

#[cfg(unix)]
fn initialize_rrdtool_locale() {
    // The upstream rrdtool executable calls setlocale(LC_ALL, "") before
    // command dispatch so graph time and calendar operators use the active
    // environment locale.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
    }
}

#[cfg(not(unix))]
fn initialize_rrdtool_locale() {}

fn rrdproxy_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let version = format!(
        "RRDtool Proxy Server v1.2.17, Copyright (C) 2004-{} The Cacti Group\r\n",
        current_local_year()
    );
    let help = format!(
        "{version}\r\nUsage: rrdtool-proxy.php [-w|--wizard] [-v|--version] [-h|--help] [-f|--force] [-s|--systemd]\r\n\r\nOptional:\r\n    -v --version   - Display the version of RRDtool Proxy Server\r\n    -h --help      - Display this help\r\n    -w --wizard    - Start Configuration Wizard\r\n    -f --force     - Allow multiple proxy instances running on a single server\r\n    -s --systemd   - Adjust output messages for systemd\r\n\r\n"
    );
    if args.is_empty() {
        return Err(
            "Cacti RRDProxy daemon and encrypted client protocol are not implemented".into(),
        );
    }
    for arg in args {
        match arg.as_str() {
            "-v" | "--version" => {
                print!("{version}");
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{help}");
                return Ok(());
            }
            "-w" | "--wizard" | "-f" | "--force" | "-s" | "--systemd" => {}
            parameter => {
                print!("ERROR: Invalid Parameter {parameter}\r\n\r\n{help}");
                return Ok(());
            }
        }
    }
    Err("Cacti RRDProxy daemon and encrypted client protocol are not implemented".into())
}

fn current_local_year() -> i32 {
    // The compatibility banner follows PHP's date('Y') at invocation time.
    // SAFETY: `time` is called with a null pointer to request the current time;
    // `localtime_r` writes into the initialized `tm` value and returns null only
    // on conversion failure.
    unsafe {
        let timestamp = libc::time(std::ptr::null_mut());
        let mut local = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&timestamp, &mut local).is_null() {
            2026
        } else {
            local.tm_year + 1900
        }
    }
}

async fn rrdcached_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from("/tmp");
    let mut base_seen = false;
    let mut journal_directory = None;
    let mut pid_file = None;
    let mut log_file = None;
    let mut no_overwrite = false;
    let mut base_only = false;
    let mut allow_recursive_mkdir = false;
    let mut active_socket_mode = None;
    let mut active_socket_commands = None;
    let mut active_socket_group = None;
    // Each listener takes the -m/-P/-s values in effect when it was named.
    let mut listeners = Vec::<(String, Option<u32>, Option<Vec<String>>, Option<u32>)>::new();
    let mut daemon_user = None;
    let mut daemon_group = None;
    let mut write_timeout_seconds = 300;
    let mut write_jitter_seconds = 0;
    let mut flush_interval_seconds = 3600;
    let mut queue_threads = 4_usize;
    let mut allocation_chunk = 1_usize;
    // read_options keeps parsing after most errors; the last status wins and a
    // negative status (an unknown option) still exits successfully.
    let mut status = 0_i32;
    let mut index = 0;
    let mut offset = 0;
    while let Some(parsed) = next_rrdcached_option(args, &mut index, &mut offset) {
        let (option, value) = match parsed {
            Ok(parsed) => parsed,
            Err(message) => {
                eprintln!("{message}");
                print!("{RRDCACHED_HELP}");
                status = -1;
                continue;
            }
        };
        let value = value.unwrap_or_default();
        match option {
            'h' => {
                print!("{RRDCACHED_HELP}");
                status = 1;
            }
            'l' | 'L' => {
                let address = if option == 'L' { String::new() } else { value };
                listeners.push((
                    address,
                    active_socket_mode,
                    active_socket_commands.clone(),
                    active_socket_group,
                ));
            }
            'b' => {
                base_seen = true;
                root = PathBuf::from(value);
            }
            'j' => journal_directory = Some(PathBuf::from(value)),
            'p' => pid_file = Some(PathBuf::from(value)),
            'o' => log_file = Some(PathBuf::from(value)),
            'O' => no_overwrite = true,
            'R' => allow_recursive_mkdir = true,
            'B' => base_only = true,
            // Rondi always stays in the foreground and flushes accepted
            // entries during graceful shutdown, so these request behavior
            // already enabled.
            'F' | 'g' => {}
            'm' => {
                let mode = u32::from_str_radix(&value, 8)
                    .ok()
                    .filter(|mode| *mode <= 0o7777);
                let Some(mode) = mode else {
                    eprintln!("read_options: Invalid file mode \"{value}\".");
                    std::process::exit(5);
                };
                active_socket_mode = Some(mode);
            }
            'P' => {
                let commands = value
                    .split([',', ' '])
                    .filter(|command| !command.is_empty())
                    .map(str::to_ascii_uppercase)
                    .collect::<Vec<_>>();
                let mut valid = Vec::new();
                for command in commands {
                    if RRDCACHED_COMMANDS.contains(&command.as_str()) {
                        valid.push(command);
                    } else {
                        eprintln!(
                            "read_options: Adding permission \"{command}\" to socket failed. Most likely, this permission doesn't exist. Check your command line."
                        );
                        status = 4;
                    }
                }
                active_socket_commands = if valid.is_empty() { None } else { Some(valid) };
            }
            's' => {
                let Some(group) = resolve_rrdcached_group(&value) else {
                    eprintln!("read_options: couldn't map \"{value}\" to a group, Sorry");
                    std::process::exit(5);
                };
                active_socket_group = Some(group);
            }
            'G' => {
                let Some(group) = resolve_rrdcached_daemon_group(&value) else {
                    eprintln!("read_options: couldn't map \"{value}\" to a group, Sorry");
                    std::process::exit(5);
                };
                daemon_group = Some(group);
            }
            'U' => {
                let Some(user) = resolve_rrdcached_daemon_user(&value) else {
                    eprintln!("read_options: couldn't map \"{value}\" to a user, Sorry");
                    std::process::exit(5);
                };
                daemon_user = Some(user);
            }
            'V' => {
                if !matches!(
                    value.as_str(),
                    "LOG_EMERG"
                        | "LOG_ALERT"
                        | "LOG_CRIT"
                        | "LOG_ERR"
                        | "LOG_WARNING"
                        | "LOG_NOTICE"
                        | "LOG_INFO"
                        | "LOG_DEBUG"
                ) {
                    eprintln!("Unrecognized log level '{value}'; falling back to default LOG_ERR.");
                }
            }
            'w' => match rrdcached_duration(&value) {
                Ok(seconds) => write_timeout_seconds = seconds,
                Err(detail) => {
                    eprintln!("Invalid write interval {value}: {detail}");
                    status = 2;
                }
            },
            'f' => match rrdcached_duration(&value) {
                Ok(seconds) => flush_interval_seconds = seconds,
                Err(detail) => {
                    eprintln!("Invalid flush interval {value}: {detail}");
                    status = 3;
                }
            },
            'z' => match rrdcached_duration(&value) {
                Ok(seconds) => write_jitter_seconds = seconds,
                Err(detail) => {
                    eprintln!("Invalid write jitter {value}: {detail}");
                    status = 2;
                }
            },
            't' => {
                if value.is_empty() {
                    eprintln!("Missing argument for -t");
                    std::process::exit(1);
                }
                let parsed = value.parse::<i32>().ok().filter(|threads| *threads > 0);
                let Some(parsed) = parsed else {
                    eprintln!("Invalid thread count: -t {value}");
                    std::process::exit(1);
                };
                queue_threads = parsed as usize;
            }
            'a' => {
                if value.is_empty() {
                    eprintln!("Missing argument for -a");
                    std::process::exit(10);
                }
                let parsed = value.parse::<i32>().ok().filter(|size| *size > 0);
                let Some(parsed) = parsed else {
                    eprintln!("Invalid allocation size: {value}");
                    std::process::exit(10);
                };
                allocation_chunk = parsed as usize;
            }
            _ => unreachable!("next_rrdcached_option returns only known options"),
        }
    }
    if flush_interval_seconds < write_timeout_seconds.saturating_mul(2) {
        eprintln!("WARNING: flush interval (-f) should be at least 2x write interval (-w) !");
    }
    if write_jitter_seconds > write_timeout_seconds {
        eprintln!("WARNING: write delay (-z) should NOT be larger than write interval (-w) !");
    }
    if base_only && !base_seen {
        eprintln!(
            "WARNING: -B does not make sense without -b!\n  Consult the rrdcached documentation"
        );
    }
    if allow_recursive_mkdir && !base_only {
        eprintln!(
            "WARNING: -R does not make sense without -B!\n  Consult the rrdcached documentation"
        );
    }
    if status != 0 {
        std::process::exit(status.max(0));
    }
    // Rondi does not change identity after startup. Refusing a different
    // account keeps the daemon from silently running with more privilege
    // than the operator asked for.
    // SAFETY: geteuid and getegid have no preconditions.
    let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if daemon_user.is_some_and(|user| user != euid)
        || daemon_group.is_some_and(|group| group != egid)
    {
        return Err(
            "rrdcached -U/-G cannot switch accounts in Rondi; start the service as that user and group"
                .into(),
        );
    }
    if listeners.len() > 1 {
        return Err("rrdcached mode supports one listener; give -l only once".into());
    }
    let (socket, socket_mode, socket_commands, socket_group) = match listeners.pop() {
        Some((address, mode, commands, group)) => {
            let socket = if let Some(path) = address.strip_prefix("unix:") {
                PathBuf::from(path)
            } else if address.starts_with('/') {
                PathBuf::from(address)
            } else {
                return Err(
                    "rrdcached network listeners are not enabled; use a Unix socket".into(),
                );
            };
            (socket, mode, commands, group)
        }
        None => (
            PathBuf::from("/tmp/rrdcached.sock"),
            active_socket_mode,
            active_socket_commands,
            active_socket_group,
        ),
    };
    rondi_server::run_rrdcached(rondi_server::RrdcachedConfig {
        root,
        socket,
        journal_directory,
        pid_file,
        log_file,
        no_overwrite,
        allow_recursive_mkdir,
        socket_mode,
        socket_commands,
        socket_group,
        allocation_chunk,
        write_timeout_seconds,
        flush_interval_seconds,
        queue_threads,
        max_pending_bytes: rondi_server::DEFAULT_RRDCACHED_QUEUE_BYTES,
    })
    .await
}

const RRDCACHED_COMMANDS: &[&str] = &[
    "UPDATE",
    "WROTE",
    "TUNE",
    "DUMP",
    "FLUSH",
    "FLUSHALL",
    "PENDING",
    "FORGET",
    "QUEUE",
    "STATS",
    "HELP",
    "PING",
    "BATCH",
    ".",
    "FETCH",
    "FETCHBIN",
    "INFO",
    "FIRST",
    "LAST",
    "CREATE",
    "LIST",
    "SUSPEND",
    "RESUME",
    "SUSPENDALL",
    "RESUMEALL",
    "QUIT",
];

/// One step of RRDtool's bundled optparse for rrdcached: short options may
/// be clustered (`-gF`) or carry an attached argument (`-w1800`), `--help` is
/// the only long option, non-option words are skipped, and `--` ends parsing.
fn next_rrdcached_option(
    args: &[String],
    index: &mut usize,
    offset: &mut usize,
) -> Option<Result<(char, Option<String>), String>> {
    const WITH_ARGUMENT: &str = "abfGjlmoPpstUVwz";
    const WITHOUT_ARGUMENT: &str = "BFghLOR";
    loop {
        let arg = args.get(*index)?;
        if *offset == 0 {
            if arg == "--" {
                return None;
            }
            if let Some(name) = arg.strip_prefix("--") {
                *index += 1;
                return Some(match name.split_once('=') {
                    None if name == "help" => Ok(('h', None)),
                    Some(("help", _)) => Err("option takes no arguments -- 'help'".to_owned()),
                    _ => Err(format!("invalid option -- '{name}'")),
                });
            }
            if !arg.starts_with('-') || arg.len() == 1 {
                *index += 1;
                continue;
            }
            *offset = 1;
        }
        let option = arg[*offset..].chars().next()?;
        let rest = &arg[*offset + option.len_utf8()..];
        if WITH_ARGUMENT.contains(option) {
            *offset = 0;
            *index += 1;
            if !rest.is_empty() {
                return Some(Ok((option, Some(rest.to_owned()))));
            }
            let Some(value) = args.get(*index) else {
                return Some(Err(format!("option requires an argument -- '{option}'")));
            };
            *index += 1;
            return Some(Ok((option, Some(value.clone()))));
        }
        if !WITHOUT_ARGUMENT.contains(option) {
            // optparse abandons the rest of a cluster after an unknown option.
            *offset = 0;
            *index += 1;
            return Some(Err(format!("invalid option -- '{option}'")));
        }
        if rest.is_empty() {
            *offset = 0;
            *index += 1;
        } else {
            *offset += option.len_utf8();
        }
        return Some(Ok((option, None)));
    }
}

/// `rrd_scaled_duration` with a divisor of one, reporting RRDtool's text.
fn rrdcached_duration(value: &str) -> Result<u64, &'static str> {
    if !value.starts_with(|first: char| first.is_ascii_digit()) {
        return Err("value must be (suffixed) positive number");
    }
    if is_rrd_zero_duration(value) {
        return Err("value must be positive");
    }
    parse_rrd_scaled_duration(value, 1).map_err(|_| "value has trailing garbage")
}

fn resolve_rrdcached_daemon_user(value: &str) -> Option<libc::uid_t> {
    use std::ffi::CString;
    // RRDtool treats any all-digit argument (including an empty one) as a
    // numeric id and everything else as a name.
    let user = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let uid = value.parse::<libc::uid_t>().unwrap_or(0);
        // SAFETY: getpwuid returns a pointer to libc-owned static data.
        unsafe { libc::getpwuid(uid) }
    } else {
        let name = CString::new(value).ok()?;
        // SAFETY: name is NUL terminated and remains alive for the call.
        unsafe { libc::getpwnam(name.as_ptr()) }
    };
    if user.is_null() {
        None
    } else {
        // SAFETY: non-null user points to libc-owned passwd data.
        Some(unsafe { (*user).pw_uid })
    }
}

fn resolve_rrdcached_daemon_group(value: &str) -> Option<libc::gid_t> {
    use std::ffi::CString;
    let group = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let gid = value.parse::<libc::gid_t>().unwrap_or(0);
        // SAFETY: getgrgid returns a pointer to libc-owned static data.
        unsafe { libc::getgrgid(gid) }
    } else {
        let name = CString::new(value).ok()?;
        // SAFETY: name is NUL terminated and remains alive for the call.
        unsafe { libc::getgrnam(name.as_ptr()) }
    };
    if group.is_null() {
        None
    } else {
        // SAFETY: non-null group points to libc-owned group data.
        Some(unsafe { (*group).gr_gid })
    }
}

fn resolve_rrdcached_group(value: &str) -> Option<u32> {
    use std::ffi::CString;
    let group = if let Ok(gid) = value.parse::<libc::gid_t>() {
        if gid > 0 {
            // SAFETY: getgrgid returns a pointer to libc-owned static data.
            unsafe { libc::getgrgid(gid) }
        } else {
            let name = CString::new(value).ok()?;
            // SAFETY: name is NUL terminated and remains alive for the call.
            unsafe { libc::getgrnam(name.as_ptr()) }
        }
    } else {
        let name = CString::new(value).ok()?;
        // SAFETY: name is NUL terminated and remains alive for the call.
        unsafe { libc::getgrnam(name.as_ptr()) }
    };
    if group.is_null() {
        None
    } else {
        // SAFETY: non-null group points to libc-owned group data.
        let group = unsafe { &*group };
        Some(group.gr_gid as u32)
    }
}

const RRDCACHED_HELP: &str = concat!(
    "RRDCacheD 1.11.0\n",
    "Copyright (C) 2008,2009 Florian octo Forster and Kevin Brintnall\n\n",
    "Usage: rrdcached [options]\n\n",
    "Valid options are:\n",
    "  -a <size>     Memory allocation chunk size. Default is 1.\n",
    "  -B            Restrict file access to paths within -b <dir>\n",
    "  -b <dir>      Base directory to change to.\n",
    "  -F            Always flush all updates at shutdown\n",
    "  -f <seconds>  Interval in which to flush dead data.\n",
    "  -G <group>    Unprivileged group used when running.\n",
    "  -g            Do not fork and run in the foreground.\n",
    "  -j <dir>      Directory in which to create the journal files.\n",
    "  -L            Open sockets on all INET interfaces using default port.\n",
    "  -l <address>  Socket address to listen to.\n",
    "                Default: unix:/tmp/rrdcached.sock\n",
    "  -m <mode>     File permissions (octal) of all following UNIX sockets\n",
    "  -O            Do not allow CREATE commands to overwrite existing\n",
    "                files, even if asked to.\n",
    "  -o <file>     Log to given file instead of syslog.\n",
    "  -P <perms>    Sets the permissions to assign to all following sockets\n",
    "  -p <file>     Location of the PID-file.\n",
    "  -R            Allow recursive directory creation within -b <dir>\n",
    "  -s <id|name>  Group owner of all following UNIX sockets\n",
    "                (the socket will also have read/write permissions for that group)\n",
    "  -t <threads>  Number of write threads.\n",
    "  -U <user>     Unprivileged user account used when running.\n",
    "  -V <LOGLEVEL> Max syslog level to log with, with LOG_DEBUG being\n",
    "                the maximum and LOG_EMERG minimum; see syslog.h\n",
    "  -w <seconds>  Interval in which to write data.\n",
    "  -z <delay>    Delay writes up to <delay> seconds to spread load\n\n",
    "For more information and a detailed description of all options please refer\n",
    "to the rrdcached(1) manual page.\n",
);

const RRDTOOL_COMMANDS: &[&str] = &[
    "create",
    "fetch",
    "update",
    "updatev",
    "last",
    "lastupdate",
    "first",
    "info",
    "dump",
    "restore",
    "tune",
    "list",
    "resize",
    "xport",
    "graph",
    "graphv",
    "flushcached",
];

const RRDTOOL_USAGE_HEADER: &str = concat!(
    "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n",
    "               Compiled \n\n",
    "Usage: rrdtool [options] command command_options\n",
);

const RRDTOOL_USAGE_FOOTER: &str = concat!(
    "RRDtool is distributed under the Terms of the GNU General\n",
    "Public License Version 2. (www.gnu.org/copyleft/gpl.html)\n\n",
    "For more information read the RRD manpages\n\n",
);

fn rrdtool_usage(remote: bool) -> String {
    let mut text = String::from(RRDTOOL_USAGE_HEADER);
    text.push_str(concat!(
        "Valid commands: create, update, updatev, graph, graphv,  dump, restore,\n",
        "\t\tlast, lastupdate, first, info, list, fetch, tune,\n",
        "\t\tresize, xport, flushcached\n\n",
    ));
    if remote {
        text.push_str("Valid remote commands: quit, ls, cd, mkdir, pwd\n\n");
    }
    text.push_str(RRDTOOL_USAGE_FOOTER);
    text
}

/// `PrintUsage` (rrd_tool.c:43). Kadupul reads the version from this
/// banner, so `-v` and unknown words print it and succeed.
fn print_usage(command: &str, remote: bool) {
    if RRDTOOL_COMMANDS.contains(&command) {
        // Each command prints its own usage when given no arguments.
        let _ = rrdtool_dispatch(&[command.to_owned()]);
        return;
    }
    let body = match command {
        "quit" => " * quit - closing a session in remote mode\n\n\trrdtool quit\n",
        "ls" => " * ls - lists all *.rrd files in current directory\n\n\trrdtool ls\n",
        "cd" => " * cd - changes the current directory\n\n\trrdtool cd new directory\n",
        "mkdir" => " * mkdir - creates a new directory\n\n\trrdtool mkdir newdirectoryname\n",
        "pwd" => " * pwd - returns the current working directory\n\n\trrdtool pwd\n",
        _ => {
            print!("{}", rrdtool_usage(remote));
            return;
        }
    };
    print!("{RRDTOOL_USAGE_HEADER}{body}\n{RRDTOOL_USAGE_FOOTER}");
}

/// rrd_tool.c:447 `main` after `setlocale`. Returns the exit status.
fn rrdtool_main(argv: &[String]) -> i32 {
    match argv.len() {
        1 => {
            print_usage("", false);
            0
        }
        2 | 3 if argv[1] == "-" => rrdtool_batch(argv),
        2 => {
            print_usage(&argv[1], false);
            0
        }
        3 if argv[1] == "help" => {
            print_usage(&argv[2], false);
            0
        }
        _ => handle_input_line(argv, false),
    }
}

/// The command switch in `HandleInputLine` (rrd_tool.c:713), shared by
/// argv and pipe mode. `args[0]` is the command name.
fn rrdtool_dispatch(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args[0].as_str() {
        "create" => rrdtool_create(args),
        "dump" => rrdtool_dump(args),
        "info" => rrdtool_info(args),
        "updatev" => rrdtool_updatev(args),
        "list" => rrdtool_list(args),
        "restore" => rrdtool_restore(args),
        "resize" => rrdtool_resize(args),
        "last" => print_minus_one_on_error(rrdtool_last(args)),
        "lastupdate" => rrdtool_lastupdate(args),
        "first" => print_minus_one_on_error(rrdtool_first(args)),
        "update" => rrdtool_update(args),
        "fetch" => rrdtool_fetch(args),
        "xport" => rrdtool_xport(args),
        "graph" => rrdtool_graph(args, false),
        "graphv" => rrdtool_graph(args, true),
        "tune" => rrdtool_tune(args),
        "flushcached" => rrdtool_flushcached(args),
        command => Err(format!("unknown function '{command}'").into()),
    }
}

fn rrd_strerror(errno: libc::c_int) -> String {
    // SAFETY: strerror returns a pointer to a NUL-terminated message.
    unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)) }
        .to_string_lossy()
        .into_owned()
}

fn last_errno() -> libc::c_int {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// `HandleInputLine` (rrd_tool.c:579). `argv[0]` is the program name.
fn handle_input_line(argv: &[String], remote: bool) -> i32 {
    let argc = argv.len();
    let word = |index: usize| argv.get(index).map(String::as_str);
    if remote {
        match word(1) {
            Some("quit") => {
                if argc != 2 {
                    println!("ERROR: invalid parameter count for quit");
                    return 1;
                }
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            Some("cd") => {
                if argc != 3 {
                    println!("ERROR: invalid parameter count for cd");
                    return 1;
                }
                if let Err(errno) = c_path_call(&argv[2], |path| unsafe { libc::chdir(path) }) {
                    println!("ERROR: chdir {} {}", argv[2], rrd_strerror(errno));
                    return 1;
                }
                return 0;
            }
            Some("pwd") => {
                if argc != 2 {
                    println!("ERROR: invalid parameter count for pwd");
                    return 1;
                }
                match std::env::current_dir() {
                    Ok(cwd) => {
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(cwd.as_os_str().as_encoded_bytes());
                        let _ = stdout.write_all(b"\n");
                    }
                    Err(error) => {
                        let errno = error.raw_os_error().unwrap_or(libc::EIO);
                        println!("ERROR: getcwd {}", rrd_strerror(errno));
                        return 1;
                    }
                }
                return 0;
            }
            Some("mkdir") => {
                if argc != 3 {
                    println!("ERROR: invalid parameter count for mkdir");
                    return 1;
                }
                if let Err(errno) =
                    c_path_call(&argv[2], |path| unsafe { libc::mkdir(path, 0o777) })
                {
                    println!("ERROR: mkdir {}: {}", argv[2], rrd_strerror(errno));
                    return 1;
                }
                return 0;
            }
            Some("ls") => {
                if argc != 2 {
                    println!("ERROR: invalid parameter count for ls");
                    return 1;
                }
                return remote_ls();
            }
            _ => {}
        }
    }
    if argc < 3 || matches!(word(1), Some("help" | "--help" | "-help" | "-?" | "-h")) {
        print_usage("", remote);
        return 0;
    }
    let result = if matches!(
        word(1),
        Some("--version" | "version" | "v" | "-v" | "-version")
    ) {
        println!("RRDtool 1.11.0  Copyright by Tobi Oetiker (1.011000)");
        Ok(())
    } else {
        rrdtool_dispatch(&argv[1..])
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            if remote {
                println!("ERROR: {error}");
            } else {
                eprintln!("ERROR: {error}");
            }
            1
        }
    }
}

/// Runs a libc call that takes one path and reports `errno` on failure.
fn c_path_call(
    path: &str,
    call: impl FnOnce(*const libc::c_char) -> libc::c_int,
) -> Result<(), libc::c_int> {
    let Ok(path) = std::ffi::CString::new(path) else {
        return Err(libc::ENOENT);
    };
    if call(path.as_ptr()) == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// The remote `ls` command (rrd_tool.c:669), in readdir order with `.`
/// and `..`.
fn remote_ls() -> i32 {
    // SAFETY: opendir/readdir/stat/closedir are used on a directory handle
    // owned by this function; each entry name is NUL-terminated.
    unsafe {
        let directory = libc::opendir(c".".as_ptr());
        if directory.is_null() {
            let errno = last_errno();
            println!("ERROR: opendir .: {}", rrd_strerror(errno));
            return errno;
        }
        let mut stdout = std::io::stdout().lock();
        loop {
            let entry = libc::readdir(directory);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
            let mut status = std::mem::zeroed::<libc::stat>();
            if libc::stat(name.as_ptr(), &mut status) != 0 {
                continue;
            }
            let bytes = name.to_bytes();
            let kind = status.st_mode & libc::S_IFMT;
            if kind == libc::S_IFDIR {
                let _ = stdout.write_all(b"d ");
                let _ = stdout.write_all(bytes);
                let _ = stdout.write_all(b"\n");
            }
            if bytes.len() > 4
                && kind == libc::S_IFREG
                && matches!(&bytes[bytes.len() - 4..], b".rrd" | b".RRD")
            {
                let _ = stdout.write_all(b"- ");
                let _ = stdout.write_all(bytes);
                let _ = stdout.write_all(b"\n");
            }
        }
        libc::closedir(directory);
    }
    0
}

const MAX_LENGTH: u64 = 10000;

/// `fgetslong` (rrd_tool.c:414): one line of raw bytes. Text after an
/// embedded NUL is lost and the next `fgets` chunk is appended, as in C.
fn fgetslong(input: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let mut chunk = Vec::new();
        match input.take(MAX_LENGTH - 1).read_until(b'\n', &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = chunk.split(|byte| *byte == 0).next().unwrap_or_default();
        line.extend_from_slice(text);
        if line.last() == Some(&b'\n') {
            return Some(line);
        }
    }
    (!line.is_empty()).then_some(line)
}

/// `CountArgs` (rrd_tool.c:880): words separated by spaces only.
fn count_args(line: &[u8]) -> usize {
    let mut count = 0;
    let mut in_arg = false;
    let start = line
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(line.len());
    for byte in &line[start..] {
        if *byte == b' ' && in_arg {
            in_arg = false;
        }
        if *byte != b' ' && !in_arg {
            in_arg = true;
            count += 1;
        }
    }
    count
}

/// `CreateArgs` (rrd_tool.c:905): split on spaces, single and double quotes
/// group (the other kind is literal inside), backslash is literal. `None`
/// for an unterminated quote.
fn create_args(line: &[u8]) -> Option<Vec<String>> {
    // The comparisons use the platform's `char`, so bytes from 0x80 count
    // as blanks where `char` is signed.
    let blank = |byte: u8| byte as libc::c_char <= b' ' as libc::c_char;
    let mut end = line.len();
    // The trailing-blank loop stops before index 0.
    while end > 1 && blank(line[end - 1]) {
        end -= 1;
    }
    let mut start = 0;
    while start < end && blank(line[start]) {
        start += 1;
    }
    let mut args = Vec::<Vec<u8>>::new();
    let mut quote = 0_u8;
    let mut in_arg = false;
    for &byte in &line[start..end] {
        match byte {
            b' ' => {
                if quote != 0 {
                    args.last_mut()?.push(byte);
                } else {
                    in_arg = false;
                }
            }
            b'"' | b'\'' => {
                if quote != 0 {
                    if quote == byte {
                        quote = 0;
                    } else {
                        args.last_mut()?.push(byte);
                    }
                } else {
                    if !in_arg {
                        args.push(Vec::new());
                        in_arg = true;
                    }
                    quote = byte;
                }
            }
            _ => {
                if !in_arg {
                    args.push(Vec::new());
                    in_arg = true;
                }
                args.last_mut()?.push(byte);
            }
        }
    }
    if quote != 0 {
        return None;
    }
    Some(
        args.iter()
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

/// The `rrdtool -` loop in rrd_tool.c `main` (rrd_tool.c:473). Cacti and
/// Kadupul keep this process open while polling.
fn rrdtool_batch(argv: &[String]) -> i32 {
    let started = std::time::Instant::now();
    // rrd_tool.c:491 chroots only under HAVE_GETEUID, which configure.ac
    // never checks for (it tests getuid), so every build takes the chdir
    // path, root included.
    let firstdir = argv.get(2).map_or("", String::as_str);
    if !firstdir.is_empty()
        && let Err(errno) = c_path_call(firstdir, |path| unsafe { libc::chdir(path) })
    {
        eprintln!("ERROR: chdir {firstdir} {}", rrd_strerror(errno));
        std::process::exit(errno);
    }
    let mut stdin = std::io::stdin().lock();
    while let Some(line) = fgetslong(&mut stdin) {
        if count_args(&line) == 0 {
            println!("ERROR: not enough arguments");
            continue;
        }
        match create_args(&line) {
            None => println!("ERROR: creating arguments"),
            Some(words) => {
                let mut args = Vec::with_capacity(words.len() + 1);
                args.push(argv[0].clone());
                args.extend(words);
                if handle_input_line(&args, true) == 0 {
                    println!("{}", rrdtool_batch_ack(started));
                }
            }
        }
        let _ = std::io::stdout().flush();
    }
    0
}

#[cfg(unix)]
fn rrdtool_batch_ack(started: std::time::Instant) -> String {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes one `rusage` struct into the valid buffer.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if status != 0 {
        return "OK".to_owned();
    }
    // SAFETY: successful getrusage initialized the struct.
    let usage = unsafe { usage.assume_init() };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
    format!(
        "OK u:{:.2} s:{:.2} r:{:.2}",
        seconds(usage.ru_utime),
        seconds(usage.ru_stime),
        started.elapsed().as_secs_f64()
    )
}

#[cfg(not(unix))]
fn rrdtool_batch_ack(_started: std::time::Instant) -> String {
    "OK".to_owned()
}

fn rrdtool_create(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", rrdtool_create_help());
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut start = now - 10;
    let mut step = 300_u64;
    let mut start_was_set = false;
    let mut step_was_set = false;
    let mut no_overwrite = false;
    let mut template_file = None::<String>;
    let mut source_files = Vec::<String>::new();
    let mut daemon_address = None::<String>;
    // rrd_create.c:81.
    const LONGOPTS: &[LongOpt] = &[
        opt("start", b'b' as i32, ArgType::Required),
        opt("step", b's' as i32, ArgType::Required),
        opt("daemon", b'd' as i32, ArgType::Required),
        opt("source", b'r' as i32, ArgType::Required),
        opt("template", b't' as i32, ArgType::Required),
        opt("no-overwrite", b'O' as i32, ArgType::None),
    ];
    let mut options = OptParse::new(args.to_vec());
    loop {
        let option = options.long(LONGOPTS);
        let value = options.value().to_owned();
        match u8::try_from(option).map(char::from) {
            _ if option == optparse::DONE => break,
            Ok('d') => daemon_address = Some(value),
            Ok('b') => {
                start = rrd_parsetime(&value, now)
                    .map_err(|error| format!("start time: {error}"))?
                    .absolute()
                    .ok_or(
                        "specifying time relative to the 'start' or 'end' makes no sense here",
                    )?;
                if start < 315_360_000 {
                    return Err("the first entry to the RRD should be after 1980".into());
                }
                start_was_set = true;
            }
            Ok('s') => {
                step = parse_scaled_duration_option(&value, "step size")?;
                step_was_set = true;
            }
            Ok('O') => no_overwrite = true,
            Ok('r') => {
                match std::fs::metadata(&value) {
                    Err(error) => {
                        let errno = error.raw_os_error().unwrap_or(libc::EIO);
                        return Err(format!(
                            "error checking for source RRD {value}: {}",
                            rrd_strerror(errno)
                        )
                        .into());
                    }
                    Ok(metadata) if !metadata.is_file() => {
                        return Err(format!("Not a regular file: {value}").into());
                    }
                    Ok(_) => {}
                }
                source_files.push(value);
            }
            Ok('t') => {
                if template_file.is_some() {
                    return Err("template already set".into());
                }
                template_file = Some(value);
            }
            Ok('?') => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let mut positional = options.positionals().to_vec();
    if positional.is_empty() {
        return Err("need name of an rrd file to create".into());
    }
    let filename = positional.remove(0);
    let daemon_address = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty());
    if source_files.is_empty() && daemon_address.is_none() {
        rondi::rrd_create_r2(
            &filename,
            if step_was_set { step } else { 0 },
            if start_was_set { start } else { -1 },
            no_overwrite,
            template_file.as_deref(),
            &positional,
        )?;
        return Ok(());
    }
    let extra_data_sources = positional
        .iter()
        .filter(|value| value.starts_with("DS:"))
        .cloned()
        .collect::<Vec<_>>();
    let extra_archives = positional
        .iter()
        .filter(|value| value.starts_with("RRA:"))
        .cloned()
        .collect::<Vec<_>>();
    if extra_data_sources.len() + extra_archives.len() != positional.len() {
        return Err(
            "RRDtool create options outside the supported DS/RRA subset are unsupported".into(),
        );
    }
    let mut data_sources = Vec::new();
    let mut archives = Vec::new();
    if let Some(template_file) = &template_file {
        let template = inspect_rrd(template_file)
            .map_err(|_| format!("Cannot open template RRD {template_file}"))?;
        if !step_was_set {
            step = template.step;
        }
        if !start_was_set {
            start = template.last_update;
        }
        for source in template.data_sources {
            if !matches!(
                source.kind.as_str(),
                "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
            ) {
                return Err(format!(
                    "create --template does not support template data source type {}",
                    source.kind
                )
                .into());
            }
            let minimum = source
                .minimum
                .map_or_else(|| "U".to_owned(), |value| value.to_string());
            let maximum = source
                .maximum
                .map_or_else(|| "U".to_owned(), |value| value.to_string());
            data_sources.push(format!(
                "DS:{}:{}:{}:{}:{}",
                source.name, source.kind, source.heartbeat, minimum, maximum
            ));
        }
        for archive in template.archives {
            if !matches!(
                archive.consolidation.as_str(),
                "AVERAGE" | "MIN" | "MAX" | "LAST"
            ) {
                return Err(format!(
                    "create --template does not support template consolidation function {}",
                    archive.consolidation
                )
                .into());
            }
            archives.push(format!(
                "RRA:{}:{}:{}:{}",
                archive.consolidation, archive.xff, archive.pdp_per_row, archive.rows
            ));
        }
    }
    data_sources.extend(extra_data_sources);
    archives.extend(extra_archives);
    let mut seen_data_source_names = std::collections::HashSet::new();
    for definition in &data_sources {
        if let Some(name) = definition
            .strip_prefix("DS:")
            .and_then(|value| value.split([':', '=']).next())
        {
            if !seen_data_source_names.insert(name) {
                return Err(format!("Duplicate DS name: {name}").into());
            }
        }
    }
    if archives.is_empty() {
        return Err("you must define at least one Round Robin Archive".into());
    }
    if data_sources.is_empty() {
        return Err("you must define at least one Data Source".into());
    }
    if !source_files.is_empty() {
        if source_files.len() != 1 || template_file.is_some() {
            return Err(
                "create --source currently supports one source file and cannot be combined with --template".into(),
            );
        }
        if daemon_address.is_some() {
            return Err("create --source with --daemon is unsupported".into());
        }
        if no_overwrite && std::fs::metadata(&filename).is_ok() {
            return Err(format!("creating '{}': File exists", filename).into());
        }
        let source_info = inspect_rrd(&source_files[0])
            .map_err(|_| format!("Cannot open source RRD {}", source_files[0]))?;
        let rebase_empty_source = step != source_info.step;
        if !start_was_set {
            start = source_info.last_update;
        }
        if start != source_info.last_update {
            return Err(
                "create --source currently requires the start time to match the source last update"
                    .into(),
            );
        }
        if data_sources.len() != source_info.data_sources.len()
            || !data_sources
                .iter()
                .zip(&source_info.data_sources)
                .all(|(definition, source)| rrd_ds_definition_matches(definition, source))
        {
            return Err(
                "create --source currently requires matching data source definitions".into(),
            );
        }
        if archives.len() != source_info.archives.len()
            || !archives
                .iter()
                .zip(&source_info.archives)
                .all(|(definition, archive)| {
                    rrd_archive_definition_matches(definition, archive, step)
                })
        {
            return Err("create --source currently requires matching archive definitions".into());
        }
        let mut xml = dump_rrd_file_with_header(&source_files[0], RrdDumpHeader::None)?;
        if rebase_empty_source {
            if !source_info
                .data_sources
                .iter()
                .all(|source| source.last_value == "U" && source.pdp_value.is_nan())
                || !rrd_dump_archive_values_are_unknown(&xml)
            {
                return Err(
                    "create --source with a different step currently requires an empty source RRD"
                        .into(),
                );
            }
            let source_step = format!("<step>{}</step>", source_info.step);
            if !xml.contains(&source_step) {
                return Err("cannot rebase source RRD step in its dump".into());
            }
            xml = xml.replacen(&source_step, &format!("<step>{step}</step>"), 1);
        }
        restore_rrd_file(&xml, &filename, !no_overwrite, false)?;
        return Ok(());
    }
    let Some(address) = daemon_address else {
        unreachable!("a local create without --source returned above");
    };
    let mut fields = vec![
        "CREATE".to_owned(),
        filename.clone(),
        "-b".to_owned(),
        start.to_string(),
        "-s".to_owned(),
        step.to_string(),
    ];
    if no_overwrite {
        fields.push("-O".to_owned());
    }
    fields.extend(data_sources);
    fields.extend(archives);
    let command = fields
        .iter()
        .map(|field| field.replace('\\', "\\\\").replace(' ', "\\ "))
        .collect::<Vec<_>>()
        .join(" ");
    send_rrdcached_command(&address, &command)?;
    Ok(())
}

fn rrdtool_create_help() -> &'static str {
    "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n* create - create a new RRD\n\n\trrdtool create filename [--start|-b start time]\n\t\t[--step|-s step]\n\t\t[--template|-t template-file]\n\t\t[--source|-r source-file]\n\t\t[--no-overwrite|-O]\n\t\t[--daemon|-d address]\n\t\t[DS:ds-name:DST:dst arguments]\n\t\t[RRA:CF:cf arguments]\n\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
}

fn rrdtool_fetch(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/fetch.txt"));
        return Ok(());
    }
    // rrd_fetch.c:83.
    const LONGOPTS: &[LongOpt] = &[
        opt("resolution", b'r' as i32, ArgType::Required),
        opt("start", b's' as i32, ArgType::Required),
        opt("end", b'e' as i32, ArgType::Required),
        opt("align-start", b'a' as i32, ArgType::None),
        opt("daemon", b'd' as i32, ArgType::Required),
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut start_tv = rrd_parsetime("end-24h", now)?;
    let mut end_tv = rrd_parsetime("now", now)?;
    let mut resolution = 1_u64;
    let mut align_start = false;
    let mut daemon_address = None::<String>;
    let mut options = OptParse::new(args.to_vec());
    loop {
        let option = options.long(LONGOPTS);
        match u8::try_from(option).map(char::from) {
            _ if option == optparse::DONE => break,
            Ok('s') => {
                start_tv = rrd_parsetime(options.value(), now)
                    .map_err(|error| format!("start time: {error}"))?;
            }
            Ok('e') => {
                end_tv = rrd_parsetime(options.value(), now)
                    .map_err(|error| format!("end time: {error}"))?;
            }
            Ok('a') => align_start = true,
            Ok('r') => resolution = parse_rrd_resolution(options.value())?,
            Ok('d') => daemon_address = Some(options.value().to_owned()),
            Ok('?') => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let (mut start, mut end) = rrd_proc_start_end(&mut start_tv, &mut end_tv)?;
    if start < 315_360_000 {
        return Err("the first entry to fetch should be after 1980".into());
    }
    if align_start {
        let delta = start.rem_euclid(i64::try_from(resolution)?);
        start = start.checked_sub(delta).ok_or("start time overflows")?;
        end = end.checked_sub(delta).ok_or("end time overflows")?;
    }
    if end < start {
        return Err(format!("start ({start}) should be less than end ({end})").into());
    }
    let [filename, cf, ..] = options.positionals() else {
        return Err("Usage: rrdtool fetch <file> <CF> [options]".into());
    };
    let filename = PathBuf::from(filename);
    if !matches!(
        cf.as_str(),
        "AVERAGE"
            | "MIN"
            | "MAX"
            | "LAST"
            | "HWPREDICT"
            | "MHWPREDICT"
            | "SEASONAL"
            | "DEVSEASONAL"
            | "DEVPREDICT"
            | "FAILURES"
    ) {
        return Err(format!("unknown consolidation function '{cf}'").into());
    }

    let daemon_address = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty());
    let from_daemon = daemon_address.is_some();
    let result = if let Some(address) = daemon_address {
        let escaped = escape_rrdcached_field(&filename.to_string_lossy());
        let command = format!("FETCH {escaped} {cf} {start} {end}");
        parse_rrdcached_fetch(&send_rrdcached_multiline_command(&address, &command)?)?
    } else {
        fetch_rrd_file(&filename, cf, start, end, resolution)?
    };
    // stdout is line buffered; one write per row dominates large fetches.
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    write!(out, "           ")?;
    for data_source in &result.data_sources {
        write!(out, "{data_source:>20}")?;
    }
    writeln!(out, "\n")?;
    for row in result.rows {
        write!(out, "{:>10}:", row.timestamp)?;
        for value in row.values {
            match value {
                Some(value) => write!(out, " {}", format_fetch_value(value))?,
                None if from_daemon => write!(out, " {}", rrd_daemon_unknown_text())?,
                None => write!(out, " {}", rrd_unknown_text())?,
            }
        }
        writeln!(out)?;
    }
    out.flush()?;
    Ok(())
}

#[inline]
fn rrd_nan() -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        f64::from_bits(0xfff8_0000_0000_0000)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        f64::NAN
    }
}

// RRDtool prints unknowns with printf. glibc spells a NaN with its sign bit
// set as `-nan`; the Apple and BSD libcs print `nan` for every NaN. Which
// path carries the sign bit depends on the CPU's default NaN.
fn rrd_unknown_text() -> &'static str {
    if cfg!(all(target_env = "gnu", target_arch = "x86_64")) {
        "-nan"
    } else {
        "nan"
    }
}

fn rrd_daemon_unknown_text() -> &'static str {
    if cfg!(all(target_env = "gnu", not(target_arch = "x86_64"))) {
        "-nan"
    } else {
        "nan"
    }
}

fn ensure_rrd_file_exists(filename: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(error) = std::fs::metadata(filename) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Err(format!(
                "opening '{}': No such file or directory",
                filename.display()
            )
            .into());
        }
        return Err(error.into());
    }
    Ok(())
}

fn parse_rrdcached_fetch(
    response: &str,
) -> Result<rondi::RrdFetchResult, Box<dyn std::error::Error>> {
    let mut lines = response.lines();
    let version = lines
        .next()
        .ok_or("FETCH response is missing FlushVersion")?;
    if version != "FlushVersion: 1" {
        return Err(format!("unsupported rrdcached FETCH response: {version}").into());
    }
    let parse_field = |line: Option<&str>, key: &str| -> Result<i64, Box<dyn std::error::Error>> {
        let (actual, value) = line
            .ok_or_else(|| format!("FETCH response is missing {key}"))?
            .split_once(": ")
            .ok_or_else(|| format!("invalid FETCH {key} field"))?;
        if actual != key {
            return Err(format!("expected FETCH {key}, got {actual}").into());
        }
        Ok(value.parse()?)
    };
    let start = parse_field(lines.next(), "Start")?;
    let end = parse_field(lines.next(), "End")?;
    let step = u64::try_from(parse_field(lines.next(), "Step")?)?;
    let ds_count = usize::try_from(parse_field(lines.next(), "DSCount")?)?;
    let names = lines
        .next()
        .and_then(|line| line.strip_prefix("DSName: "))
        .ok_or("FETCH response is missing DSName")?
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if names.len() != ds_count || step == 0 || start >= end {
        return Err("invalid rrdcached FETCH metadata".into());
    }
    let mut rows = Vec::new();
    for line in lines {
        let (timestamp, values) = line
            .split_once(':')
            .ok_or("invalid rrdcached FETCH data row")?;
        let values = values
            .split_whitespace()
            .map(|value| {
                let parsed: f64 = value.parse()?;
                Ok(if parsed.is_finite() {
                    Some(parsed)
                } else {
                    None
                })
            })
            .collect::<Result<Vec<_>, std::num::ParseFloatError>>()?;
        if values.len() != ds_count {
            return Err("rrdcached FETCH row has an invalid data-source count".into());
        }
        rows.push(rondi::RrdFetchRow {
            timestamp: timestamp.trim().parse()?,
            values,
        });
    }
    Ok(rondi::RrdFetchResult {
        start,
        end,
        step,
        data_sources: names,
        rows,
    })
}

fn rrdtool_graph(args: &[String], verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        if verbose {
            print!("{}", include_str!("help/graphv.txt"));
        } else {
            print!("{}", include_str!("help/graph.txt"));
        }
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let options = graph_options::rrd_graph_options(args, now)?;
    let Some((filename, elements)) = options.positionals.split_first() else {
        return Err("missing filename".into());
    };
    let format = options.imgformat;
    let image_width = options.width;
    let image_height = options.height;
    let graph_title = options.title;
    let vertical_label = options.vertical_label;
    let vertical_label_angle = options.vertical_label_angle;
    let imginfo = options.imginfo;
    let lower_limit = options.lower_limit;
    let upper_limit = options.upper_limit;
    let no_legend = options.no_legend;
    let rigid_scale = options.rigid;
    let allow_shrink = options.allow_shrink;
    let alt_autoscale = options.alt_autoscale;
    let alt_autoscale_min = options.alt_autoscale_min;
    let alt_autoscale_max = options.alt_autoscale_max;
    let only_graph = options.only_graph;
    let full_size_mode = options.full_size_mode;
    let force_rules_legend = options.force_rules_legend;
    let legend_direction = options.legend_direction;
    let graph_colors = options.colors;
    let grid_dash = options.grid_dash;
    let border_width = options.border;
    let si_base = options.base;
    let daemon_address = options.daemon;
    let requested_step = i64::from(options.step.unwrap_or(0));
    if !matches!(
        format.as_str(),
        "XML" | "JSON" | "XMLENUM" | "JSONTIME" | "CSV" | "TSV" | "SSV" | "PNG"
    ) {
        return Err(format!("RRDtool graph format {format} is unsupported").into());
    }
    let (mut im, _) = prepare_graph_image(GraphImageRequest {
        start: options.start,
        end: options.end,
        step: requested_step,
        xsize: i64::from(image_width),
        daemon: daemon_address,
        elements,
    })?;
    options.layout.apply(&mut im);
    // rrd_graph_v: graph_paint, then image_info and the image itself.
    let mut info = GraphInfo::default();
    let to_memory = filename == "-";
    let mut image = None::<Vec<u8>>;
    if format != "PNG" {
        let xport_format = match format.as_str() {
            "XML" => XportFormat::Xml { flags: 2 },
            "XMLENUM" => XportFormat::Xml { flags: 6 },
            "JSON" => XportFormat::Xml { flags: 1 },
            "JSONTIME" => XportFormat::Xml { flags: 3 },
            "CSV" => XportFormat::Separated(','),
            "TSV" => XportFormat::Separated('\t'),
            _ => XportFormat::Separated(';'),
        };
        flush_and_prepare_data(&mut im)?;
        let data = im.xport(true)?;
        info.push("graph_start", InfoValue::Count(data.start));
        info.push("graph_end", InfoValue::Count(data.end));
        info.push("graph_step", InfoValue::Count(data.step as i64));
        let (output, error) = match xport_format {
            XportFormat::Xml { flags } => {
                format_xport_xmljson(flags, &mut im, &data, si_base, to_memory)
            }
            XportFormat::Separated(separator) => (format_xport_sv(separator, &data), None),
        };
        if !to_memory {
            write_output_file(filename, output.as_bytes())?;
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        if to_memory {
            image = Some(output.into_bytes());
        }
        match print_calc(&mut im, si_base, &mut info)? {
            PrintCalc::Done(_) => {}
            PrintCalc::Silent => return Ok(()),
        }
    } else {
        flush_and_prepare_data(&mut im)?;
        let graph_elements = match print_calc(&mut im, si_base, &mut info)? {
            PrintCalc::Done(count) => count,
            PrintCalc::Silent => return Ok(()),
        };
        // graph_paint stops after print_calc when nothing is drawn.
        if graph_elements {
            use rondi::graph_layout::{
                ALTAUTOSCALE, ALTAUTOSCALE_MAX, ALTAUTOSCALE_MIN, FORCE_RULES_LEGEND,
                FULL_SIZE_MODE, NOLEGEND, ONLY_GRAPH, TEXT_PROP_LEGEND, text_height,
            };
            if image_width < 10 {
                return Err("width below 10 pixels".into());
            }
            if image_height < 10 {
                return Err("height below 10 pixels".into());
            }
            im.xsize = i64::from(image_width);
            im.ysize = i64::from(image_height);
            im.title.clone_from(&graph_title);
            im.ylegend.clone_from(&vertical_label);
            im.minval = lower_limit.unwrap_or_else(rrd_nan);
            im.maxval = upper_limit.unwrap_or_else(rrd_nan);
            im.rigid = rigid_scale;
            im.allow_shrink = allow_shrink;
            im.base = i64::from(si_base);
            im.legenddirection = legend_direction;
            for (set, flag) in [
                (alt_autoscale, ALTAUTOSCALE),
                (alt_autoscale_min, ALTAUTOSCALE_MIN),
                (alt_autoscale_max, ALTAUTOSCALE_MAX),
                (no_legend, NOLEGEND),
                (only_graph, ONLY_GRAPH),
                (force_rules_legend, FORCE_RULES_LEGEND),
                (full_size_mode, FULL_SIZE_MODE),
            ] {
                if set {
                    im.extra_flags |= flag;
                }
            }
            // graph_paint_timestring.
            im.graph_size_location(true)?;
            for (key, value) in [
                ("graph_left", im.xorigin),
                ("graph_top", im.yorigin - im.ysize),
                ("graph_width", im.xsize),
                ("graph_height", im.ysize),
                ("image_width", im.ximg),
                ("image_height", im.yimg),
                ("graph_start", im.start),
                ("graph_end", im.end),
            ] {
                info.push(key, InfoValue::Count(value));
            }
            im.data_proc()?;
            if !im.logarithmic {
                im.si_unit();
            }
            if (!im.rigid || im.allow_shrink) && !im.logarithmic {
                im.expand_range();
            }
            info.push("value_min", InfoValue::Val(im.minval));
            info.push("value_max", InfoValue::Val(im.maxval));
            let legend_size = im.text_prop[TEXT_PROP_LEGEND];
            let mut legends = Vec::new();
            for (index, x0, y0) in im.legend_origins() {
                let legend = im.gdes[index].legend.clone();
                let width = im.legend_text_width(&legend);
                let height = text_height(legend_size);
                let count = legends.len();
                info.push(format!("legend[{count}]"), InfoValue::Str(legend.clone()));
                info.push(
                    format!("coords[{count}]"),
                    InfoValue::Str(format!(
                        "{},{},{},{}",
                        c_printf_double("%.0f", x0),
                        c_printf_double("%.0f", y0 - height),
                        c_printf_double("%.0f", x0 + width),
                        c_printf_double("%.0f", y0)
                    )),
                );
                legends.push(GraphLegend {
                    x: x0,
                    y: y0,
                    text: legend,
                    gdes_index: index,
                });
            }
            let (rendered, series) = graph_render_input(&im);
            let png = render_graph_png(
                &rendered,
                &series,
                GraphPngOptions {
                    canvas: GraphCanvas {
                        width: u32::try_from(im.ximg)?,
                        height: u32::try_from(im.yimg)?,
                        left: u32::try_from(im.xorigin)?,
                        top: u32::try_from(im.yorigin - im.ysize)?,
                        plot_width: u32::try_from(im.xsize)?,
                        plot_height: u32::try_from(im.ysize)?,
                        minimum: im.minval,
                        maximum: im.maxval,
                    },
                    legends: &legends,
                    title: graph_title.as_deref(),
                    vertical_label: vertical_label.as_deref(),
                    vertical_label_angle,
                    only_graph,
                    colors: graph_colors,
                    grid_dash,
                    border_width,
                },
            )?;
            let (width, height) = png_dimensions(&png)?;
            if !to_memory {
                write_output_file(filename, &png)?;
            }
            if let Some(format) = imginfo.as_deref().filter(|format| !format.is_empty()) {
                let basename = if to_memory {
                    "memory"
                } else {
                    Path::new(filename)
                        .file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or(filename)
                };
                info.push(
                    "image_info",
                    InfoValue::Str(format_imginfo(format, basename, width, height)?),
                );
            }
            if to_memory {
                image = Some(png);
            }
        }
    }
    if let Some(image) = image {
        info.push("image", InfoValue::Blob(image));
    }
    let mut stdout = std::io::stdout().lock();
    if verbose {
        info.print(&mut stdout)?;
    } else {
        // rrd_tool.c only recognizes the separate `--imginfo`/`-f` spelling
        // when deciding whether to print the canvas size.
        let imginfo_flag = args[2..]
            .iter()
            .any(|argument| argument == "--imginfo" || argument == "-f");
        info.print_graph(&mut stdout, to_memory, imginfo_flag)?;
    }
    Ok(())
}

enum XportFormat {
    Xml { flags: u8 },
    Separated(char),
}

#[derive(Debug)]
enum InfoValue {
    Val(f64),
    Count(i64),
    Str(String),
    Blob(Vec<u8>),
}

/// The ordered `rrd_info_t` list `grinfo_push` builds.
#[derive(Default)]
struct GraphInfo {
    entries: Vec<(String, InfoValue)>,
}

impl GraphInfo {
    fn push(&mut self, key: impl Into<String>, value: InfoValue) {
        self.entries.push((key.into(), value));
    }

    /// `rrd_info_print`.
    fn print(&self, out: &mut impl Write) -> std::io::Result<()> {
        for (key, value) in &self.entries {
            write!(out, "{key} = ")?;
            match value {
                InfoValue::Val(value) if value.is_nan() => writeln!(out, "NaN")?,
                InfoValue::Val(value) => writeln!(out, "{}", c_printf_double("%0.10e", *value))?,
                InfoValue::Count(value) => writeln!(out, "{value}")?,
                InfoValue::Str(value) => writeln!(out, "\"{value}\"")?,
                InfoValue::Blob(value) => {
                    writeln!(out, "BLOB_SIZE:{}", value.len())?;
                    out.write_all(value)?;
                }
            }
        }
        Ok(())
    }

    /// `rrd_graph` plus the `graph` branch of rrd_tool.c: the WxH line,
    /// image_info and PRINT lines, or the image itself for `-`.
    fn print_graph(
        &self,
        out: &mut impl Write,
        to_memory: bool,
        imginfo_flag: bool,
    ) -> std::io::Result<()> {
        let count = |key: &str| {
            self.entries.iter().find_map(|(name, value)| match value {
                InfoValue::Count(value) if name == key => Some(*value),
                _ => None,
            })
        };
        let mut lines = Vec::new();
        for (key, value) in &self.entries {
            if let (true, InfoValue::Str(text)) = (key == "image_info", value) {
                lines.push(text.as_str());
            }
        }
        for (key, value) in &self.entries {
            match value {
                InfoValue::Str(text) if key.starts_with("print") => lines.push(text.as_str()),
                InfoValue::Blob(bytes) if key == "image" => out.write_all(bytes)?,
                _ => {}
            }
        }
        if !to_memory && !imginfo_flag {
            writeln!(
                out,
                "{}x{}",
                count("image_width").unwrap_or(0),
                count("image_height").unwrap_or(0)
            )?;
        }
        if !to_memory {
            for line in lines {
                writeln!(out, "{line}")?;
            }
        }
        Ok(())
    }
}

struct GraphImageRequest<'a> {
    start: i64,
    end: i64,
    step: i64,
    xsize: i64,
    daemon: Option<String>,
    elements: &'a [String],
}

/// The image step and `rrd_graph_script`, after the option parser checked
/// the time range. The flag reports a parser that stopped
/// without an error, after which RRDtool keeps the elements read so far.
fn prepare_graph_image(
    request: GraphImageRequest<'_>,
) -> Result<(rondi::graph::GraphImage, bool), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let (start, end) = (request.start, request.end);
    let step = request.step.max((end - start) / request.xsize.max(1));
    let mut im = rondi::graph::GraphImage::new(start, end, step.max(0) as u64);
    im.daemon_addr = request
        .daemon
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok());
    // newGraphDescription starts each DEF window as absolute times at the
    // image window and parses only the given `start=`/`end=`.
    let resolve = |start_spec: Option<&str>, end_spec: Option<&str>, start: i64, end: i64| {
        let start_text = start.to_string();
        let end_text = end.to_string();
        resolve_rrd_range_times(
            Some(start_spec.unwrap_or(&start_text)),
            Some(end_spec.unwrap_or(&end_text)),
            now,
        )
        .map_err(|error| error.to_string())
    };
    match im.graph_script(request.elements, &resolve) {
        Ok(()) => Ok((im, false)),
        Err(rondi::graph::ScriptError::Silent) => Ok((im, true)),
        Err(rondi::graph::ScriptError::Error(message)) => Err(message.into()),
    }
}

/// `data_fetch` (flushing each DEF's file through its rrdcached first) and
/// `data_calc`.
fn flush_and_prepare_data(
    im: &mut rondi::graph::GraphImage,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut flushed = std::collections::HashSet::new();
    let mut flush_error = None::<Box<dyn std::error::Error>>;
    let mut hook = |address: &str, filename: &str| {
        if flushed.insert((address.to_owned(), filename.to_owned()))
            && let Err(error) = send_rrdcached_flush(address, filename)
        {
            let message = error.to_string();
            flush_error = Some(error);
            return Err(rondi::StoreError::RrdExpression(message));
        }
        Ok(())
    };
    let fetched = im.data_fetch(&mut hook);
    if let Some(error) = flush_error {
        return Err(error);
    }
    fetched?;
    im.data_calc()?;
    Ok(())
}

/// Writes a command's output file like `fopen(path, "w")`, except that a
/// symbolic link as the last component, or a regular file with other hard
/// links, is refused instead of followed. This is a deliberate safety
/// deviation from RRDtool: a link planted at an output path cannot redirect
/// the write to another file.
#[cfg(unix)]
fn write_output_file(path: &str, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // OpenOptions creates with 0666 less the umask, as fopen does.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| -> Box<dyn std::error::Error> {
            if error.raw_os_error() == Some(libc::ELOOP) {
                format!("refusing to write '{path}': it is a symbolic link").into()
            } else {
                error.into()
            }
        })?;
    let metadata = file.metadata()?;
    // Devices such as /dev/null keep their contents; only a regular file is
    // truncated, and only once it is known to have no other names.
    if metadata.is_file() {
        if metadata.nlink() > 1 {
            return Err(
                format!("refusing to write '{path}': it has more than one hard link").into(),
            );
        }
        file.set_len(0)?;
    }
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_output_file(path: &str, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, bytes)?;
    Ok(())
}

fn rrdtool_xport(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/xport.txt"));
        return Ok(());
    }
    let (output, error) = render_xport(args)?;
    print!("{output}");
    match error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// The xport document and, when formatting a PRINT failed part way, the
/// error RRDtool reports after writing the head of the document.
fn render_xport(args: &[String]) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let xport = graph_options::rrd_xport_options(args, now)?;
    let (json, show_time, enum_ds) = (xport.json, xport.showtime, xport.enumds);
    let (mut im, _) = prepare_graph_image(GraphImageRequest {
        start: xport.start,
        end: xport.end,
        step: i64::from(xport.step),
        xsize: xport.maxrows,
        daemon: xport.daemon,
        elements: &xport.positionals,
    })?;
    if im.gdes.is_empty() {
        return Err("can't make an xport without contents".into());
    }
    flush_and_prepare_data(&mut im)?;
    let data = im.xport(false)?;
    let flags = u8::from(json) | (u8::from(show_time) << 1) | (u8::from(enum_ds) << 2);
    Ok(format_xport_xmljson(flags, &mut im, &data, 1000, false))
}

/// Turns graph elements into the native renderer's series, with one column
/// per drawn element on the common xport grid.
fn graph_render_input(im: &rondi::graph::GraphImage) -> (RenderedGraphXport, Vec<GraphSeries>) {
    use rondi::graph::Gf;
    let mut series = Vec::new();
    let mut sources = Vec::new();
    for (index, element) in im.gdes.iter().enumerate() {
        let style = match element.gf {
            Gf::Line => "line",
            Gf::Area => "area",
            Gf::Tick => "tick",
            Gf::Hrule => "hrule",
            Gf::Vrule => "vrule",
            _ => continue,
        };
        let (rule_value, rule_time) = match element.gf {
            Gf::Hrule => (Some(element.yrule), None),
            Gf::Vrule => (None, Some(element.xrule)),
            _ => (None, None),
        };
        let variable = format!("#{index}");
        if matches!(element.gf, Gf::Line | Gf::Area | Gf::Tick) {
            sources.push((variable.clone(), element.vidx, element.yrule));
        }
        series.push(GraphSeries {
            variable,
            style,
            line_width: match element.gf {
                Gf::Line => element.linewidth,
                Gf::Area => 0.0,
                _ => 1.0,
            },
            stack: element.stack,
            color: element.color,
            color2: element.color2,
            grad_height: if element.gf == Gf::Area {
                element.gradheight
            } else {
                0.0
            },
            tick_fraction: if element.gf == Gf::Tick {
                element.yrule
            } else {
                0.0
            },
            rule_value,
            rule_time,
            dash_pattern: element.dashes.clone(),
            dash_offset: element.dash_offset,
        });
    }
    let step = sources
        .iter()
        .filter_map(|(_, vidx, _)| vidx.map(|vidx| im.gdes[vidx].step))
        .fold(
            0,
            |step, next| if step == 0 { next } else { gcd_u64(step, next) },
        );
    let step = if step == 0 { im.step.max(1) } else { step };
    let step_i64 = step as i64;
    let start = im.start - im.start.rem_euclid(step_i64);
    let mut end = im.end - im.end.rem_euclid(step_i64);
    if im.end > end {
        end += step_i64;
    }
    let row_count = usize::try_from((end - start) / step_i64).unwrap_or(0);
    let rows = (0..row_count)
        .map(|row| {
            let now = start + row as i64 * step_i64;
            sources
                .iter()
                .map(|(_, vidx, yrule)| {
                    let value = match vidx {
                        Some(vidx) => im.value_at(*vidx, now),
                        None => *yrule,
                    };
                    (!value.is_nan()).then_some(value)
                })
                .collect()
        })
        .collect();
    (
        RenderedGraphXport {
            start,
            end,
            variables: sources.into_iter().map(|(name, _, _)| name).collect(),
            rows,
        },
        series,
    )
}

fn gcd_u64(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

struct RenderedGraphXport {
    start: i64,
    end: i64,
    variables: Vec<String>,
    rows: Vec<Vec<Option<f64>>>,
}

enum PrintCalc {
    /// Whether any element draws on the canvas.
    Done(bool),
    /// A formatter failed without setting an error; RRDtool returns from
    /// the command with no output and status 0.
    Silent,
}

/// `LOCALTIME_R(..., FORCE_UTC_TIME)` without `--utc`.
fn local_tm(timestamp: i64) -> libc::tm {
    let timestamp = timestamp as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&timestamp, &mut tm) };
    tm
}

fn c_strftime(format: &str, tm: &libc::tm, max: usize) -> Option<String> {
    let format = std::ffi::CString::new(format).ok()?;
    let mut buffer = vec![0_u8; max];
    let length = unsafe { libc::strftime(buffer.as_mut_ptr().cast(), max, format.as_ptr(), tm) };
    Some(String::from_utf8_lossy(&buffer[..length]).into_owned())
}

/// Port of `print_calc`: pushes `print[n]` info entries, writes GPRINT
/// legends back into the elements, and resolves rule values.
fn print_calc(
    im: &mut rondi::graph::GraphImage,
    si_base: u32,
    info: &mut GraphInfo,
) -> Result<PrintCalc, Box<dyn std::error::Error>> {
    use rondi::graph::{FMT_LEG_LEN, Gf, ValueFormatter};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut tmvdef = local_tm(now);
    let mut graphelement = false;
    let mut si_scale = GraphSiScale::new(si_base);
    let mut prline_cnt = 0;
    for i in 0..im.gdes.len() {
        let vidx = im.gdes[i].vidx;
        match im.gdes[i].gf {
            Gf::Print | Gf::Gprint => {
                let source = vidx.map(|vidx| &im.gdes[vidx]);
                if let Some(source) = source.filter(|source| source.gf == Gf::Vdef) {
                    tmvdef = local_tm(source.vf.when);
                }
                let printval = im.print_value(i);
                let never = source.is_some_and(|source| source.vf.never);
                let element = &im.gdes[i];
                // GPRINT writes into the FMT_LEG_LEN legend buffer: numeric
                // formats through snprintf(FMT_LEG_LEN - 2), the `%.0f`
                // timestamp fallback through snprintf(FMT_LEG_LEN).
                let legend_limit = match (element.strftm, element.vformatter) {
                    (false, ValueFormatter::Numeric) => FMT_LEG_LEN - 2,
                    _ => FMT_LEG_LEN,
                };
                let text = if element.strftm {
                    if never {
                        Some(time_clean(&element.format))
                    } else {
                        // A result longer than the buffer leaves its
                        // contents undefined; treat it as empty.
                        Some(c_strftime(&element.format, &tmvdef, FMT_LEG_LEN).unwrap_or_default())
                    }
                } else {
                    match element.vformatter {
                        ValueFormatter::Numeric => {
                            match format_graph_numeric(
                                printval,
                                &element.format,
                                &mut si_scale,
                                GraphPrintf::Libc,
                            ) {
                                Ok(text) => Some(text),
                                Err(_) => {
                                    return Err(format!(
                                        "invalid format string '{}' (should match '{}')",
                                        element.format, BAD_FORMAT_PRINT_PATTERN
                                    )
                                    .into());
                                }
                            }
                        }
                        ValueFormatter::Timestamp => {
                            format_value_timestamp(printval, &element.format)
                        }
                        ValueFormatter::Duration => {
                            format_value_duration(printval, &element.format)?
                        }
                    }
                };
                let Some(text) = text else {
                    return Ok(PrintCalc::Silent);
                };
                if element.gf == Gf::Print {
                    info.push(format!("print[{prline_cnt}]"), InfoValue::Str(text));
                    prline_cnt += 1;
                } else {
                    im.gdes[i].legend = truncate_c_buffer(text, legend_limit);
                    graphelement = true;
                }
            }
            Gf::Line | Gf::Area | Gf::Tick => graphelement = true,
            Gf::Hrule => {
                if im.gdes[i].yrule.is_nan() {
                    im.gdes[i].yrule = vidx.map_or_else(rrd_nan, |vidx| im.gdes[vidx].vf.val);
                }
                graphelement = true;
            }
            Gf::Vrule => {
                if im.gdes[i].xrule == 0 {
                    im.gdes[i].xrule = vidx.map_or(0, |vidx| im.gdes[vidx].vf.when);
                }
                graphelement = true;
            }
            Gf::Stack => {
                return Err("STACK should already be turned into LINE or AREA here".into());
            }
            _ => {}
        }
    }
    Ok(PrintCalc::Done(graphelement))
}

const BAD_FORMAT_PRINT_PATTERN: &str =
    "^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*(?:%[sS])?(?:[^%]+|%%)*$";

/// `VALUE_FORMATTER_TIMESTAMP` in print_calc: gmtime of the value truncated
/// to whole seconds, or `%.0f` outside the `long long` range. `None` is a
/// strftime failure, which RRDtool reports without an error message.
fn format_value_timestamp(value: f64, format: &str) -> Option<String> {
    // timestamp_to_tm (rrd_graph.c:1810) compares the truncated value with
    // itself, so only the range check can reject a finite value.
    if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
        return Some(c_printf_double("%.0f", value));
    }
    let timestamp = value as i64 as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::gmtime_r(&timestamp, &mut tm) };
    let format = if format.is_empty() {
        "%Y-%m-%d %H:%M:%S"
    } else {
        format
    };
    let text = c_strftime(format, &tm, rondi::graph::FMT_LEG_LEN)?;
    (!text.is_empty()).then_some(text)
}

/// `VALUE_FORMATTER_DURATION` in print_calc.
fn format_value_duration(
    value: f64,
    format: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !value.is_finite() {
        return Ok(Some(c_printf_double("%f", value)));
    }
    let format = if format.is_empty() {
        "%H:%02m:%02s"
    } else {
        format
    };
    strfduration(format, value)
        .map(|text| Some(truncate_c_buffer(text, rondi::graph::FMT_LEG_LEN)))
        .map_err(Into::into)
}

fn truncate_c_buffer(mut text: String, size: usize) -> String {
    if text.len() >= size {
        let mut cut = size - 1;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

fn c_printf_double(format: &str, value: f64) -> String {
    let format = std::ffi::CString::new(format).unwrap_or_default();
    let mut buffer = [0 as libc::c_char; 512];
    let length =
        unsafe { libc::snprintf(buffer.as_mut_ptr(), buffer.len(), format.as_ptr(), value) };
    let length = usize::try_from(length).unwrap_or(0).min(buffer.len() - 1);
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Port of `strfduration` (rrd_graph.c): `duration` is in milliseconds.
fn strfduration(format: &str, duration: f64) -> Result<String, String> {
    let seconds = duration.abs() / 1000.0;
    let minutes = seconds / 60.0;
    let hours = minutes / 60.0;
    let days = hours / 24.0;
    let weeks = days / 7.0;
    let mut output = String::new();
    if duration < 0.0 {
        output.push('-');
    }
    let bytes = format.as_bytes();
    let mut f = 0;
    while f < bytes.len() {
        if bytes[f] != b'%' {
            let ch = format[f..].chars().next().unwrap_or('?');
            output.push(ch);
            f += ch.len_utf8();
            continue;
        }
        f += 1;
        let zpad = bytes.get(f) == Some(&b'0');
        if zpad {
            f += 1;
        }
        let mut width = 0_i32;
        if bytes.get(f).is_some_and(u8::is_ascii_digit) {
            let digits = bytes[f..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            width = format[f..f + digits].parse().unwrap_or(i32::MAX);
            f += digits;
        }
        let mut precision = 0_i32;
        if bytes.get(f) == Some(&b'.') {
            f += 1;
            let negative = bytes.get(f) == Some(&b'-');
            let start = f + usize::from(negative || bytes.get(f) == Some(&b'+'));
            let digits = bytes[start.min(bytes.len())..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 0 {
                let value: i32 = format[start..start + digits].parse().unwrap_or(i32::MAX);
                precision = if negative { -value } else { value };
                if precision < 0 {
                    return Err("Wrong duration format".into());
                }
                f = start + digits;
            }
        }
        let value = match bytes.get(f) {
            Some(b'%') => {
                output.push('%');
                f += 1;
                continue;
            }
            Some(b'W') => weeks,
            Some(b'd') => days - weeks.trunc() * 7.0,
            Some(b'D') => days,
            Some(b'h') => hours - days.trunc() * 24.0,
            Some(b'H') => hours,
            Some(b'm') => minutes - hours.trunc() * 60.0,
            Some(b'M') => minutes,
            Some(b's') => seconds - minutes.trunc() * 60.0,
            Some(b'S') => seconds,
            Some(b'f') => duration.abs() - seconds.trunc() * 1000.0,
            _ => return Err("Wrong duration format".into()),
        };
        f += 1;
        let scale = 10_f64.powi(precision);
        let pval = (value * scale).trunc() / scale;
        let spec = format!("%{}{width}.{precision}f", if zpad { "0" } else { "" });
        output.push_str(&c_printf_double(&spec, pval));
    }
    Ok(output)
}

/// Port of `time_clean`: the strftime format with every conversion
/// replaced by dashes of the expected width.
fn time_clean(format: &str) -> String {
    let bytes = format.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut j = 0;
    while j < bytes.len() && j < rondi::graph::FMT_LEG_LEN - 1 {
        if bytes[j] != b'%' {
            result.push(bytes[j]);
            j += 1;
            continue;
        }
        let next = bytes.get(j + 1).copied().unwrap_or(0);
        match next {
            b'E' | b'O' => {
                result.push(b'-');
                j += 2;
            }
            b'C' | b'd' | b'g' | b'H' | b'I' | b'm' | b'M' | b'S' | b'U' | b'V' | b'W' | b'y' => {
                result.extend_from_slice(b"--");
                j += 1;
            }
            b'j' => {
                result.extend_from_slice(b"---");
                j += 1;
            }
            b'G' | b'Y' => {
                result.extend_from_slice(b"----");
                j += 1;
            }
            b'R' => {
                result.extend_from_slice(b"--:--");
                j += 1;
            }
            b'T' => {
                result.extend_from_slice(b"--:--:--");
                j += 1;
            }
            b'F' => {
                result.extend_from_slice(b"----------");
                j += 1;
            }
            b'D' => {
                result.extend_from_slice(b"--/--/--");
                j += 1;
            }
            b'n' => {
                result.extend_from_slice(b"\r\n");
                j += 1;
            }
            b't' => {
                result.push(b'\t');
                j += 1;
            }
            b'%' => {
                result.push(b'%');
                j += 1;
            }
            b' ' | b'.' | b'@' => {
                result.push(b'%');
                result.push(next);
                j += 1;
            }
            _ => {
                result.push(b'-');
                j += 1;
            }
        }
        j += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

#[derive(Clone)]
struct GraphSeries {
    variable: String,
    style: &'static str,
    line_width: f64,
    stack: bool,
    color: Option<[u8; 4]>,
    color2: Option<[u8; 4]>,
    grad_height: f64,
    tick_fraction: f64,
    rule_value: Option<f64>,
    rule_time: Option<i64>,
    dash_pattern: Vec<f64>,
    dash_offset: f64,
}

/// The canvas, graph area and value range `graph_size_location`,
/// `data_proc` and `expand_range` computed.
struct GraphCanvas {
    width: u32,
    height: u32,
    left: u32,
    top: u32,
    plot_width: u32,
    plot_height: u32,
    minimum: f64,
    maximum: f64,
}

/// A legend placed by `leg_place`: the bottom-left pen position of its text.
struct GraphLegend {
    x: f64,
    y: f64,
    text: String,
    gdes_index: usize,
}

struct GraphPngOptions<'a> {
    canvas: GraphCanvas,
    legends: &'a [GraphLegend],
    title: Option<&'a str>,
    vertical_label: Option<&'a str>,
    vertical_label_angle: f64,
    only_graph: bool,
    colors: GraphColors,
    grid_dash: Vec<f64>,
    border_width: u32,
}

#[derive(Clone)]
struct GraphColors {
    back: [u8; 4],
    canvas: [u8; 4],
    shade_a: [u8; 4],
    shade_b: [u8; 4],
    grid: [u8; 4],
    mgrid: [u8; 4],
    font: [u8; 4],
    axis: [u8; 4],
    frame: [u8; 4],
    arrow: [u8; 4],
}

impl Default for GraphColors {
    fn default() -> Self {
        Self {
            back: [255, 255, 255, 255],
            canvas: [255, 255, 255, 255],
            shade_a: [210, 210, 210, 255],
            shade_b: [120, 120, 120, 255],
            grid: [220, 220, 220, 255],
            mgrid: [220, 220, 220, 255],
            font: [80, 80, 80, 255],
            axis: [80, 80, 80, 255],
            frame: [80, 80, 80, 255],
            arrow: [80, 80, 80, 255],
        }
    }
}

fn render_graph_png(
    graph: &RenderedGraphXport,
    series: &[GraphSeries],
    options: GraphPngOptions<'_>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let GraphPngOptions {
        canvas,
        legends,
        title,
        vertical_label,
        vertical_label_angle,
        only_graph,
        colors,
        grid_dash,
        border_width,
    } = options;
    let GraphCanvas {
        width: canvas_width,
        height: canvas_height,
        left: plot_left,
        top: plot_top,
        plot_width: width,
        plot_height: height,
        minimum,
        maximum,
    } = canvas;
    if canvas_width > 4096 || canvas_height > 4096 {
        return Err("PNG graph dimensions exceed the 4096 pixel safety limit".into());
    }
    let pixel_count = usize::try_from(canvas_width)
        .ok()
        .and_then(|width| {
            usize::try_from(canvas_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or("PNG graph dimensions overflow")?;
    let mut pixels = vec![255_u8; pixel_count.checked_mul(3).ok_or("PNG size overflows")?];
    for pixel in pixels.chunks_exact_mut(3) {
        pixel.copy_from_slice(&composite_rgb(colors.back, [255, 255, 255]));
    }
    let canvas_rgb = composite_rgb(colors.canvas, composite_rgb(colors.back, [255, 255, 255]));
    for y in plot_top..=plot_top + height {
        for x in plot_left..=plot_left + width {
            set_pixel(
                &mut pixels,
                canvas_width,
                canvas_height,
                x,
                y,
                [canvas_rgb[0], canvas_rgb[1], canvas_rgb[2], 255],
            );
        }
    }
    let left = plot_left;
    let top = plot_top;
    let right = plot_left + width;
    let bottom = plot_top + height;
    let y_for = |value: f64| -> u32 {
        let ratio = ((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0);
        bottom - (ratio * f64::from(bottom - top)).round() as u32
    };
    let axis = colors.axis;
    if !only_graph {
        for tick in 0..=4 {
            let y = top + (u64::from(bottom - top) * tick / 4) as u32;
            draw_styled_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (left, y),
                (right, y),
                DashStroke {
                    color: colors.mgrid,
                    pattern: &grid_dash,
                    offset: 0.0,
                    width: 1.0,
                },
            );
            let value = maximum - (maximum - minimum) * tick as f64 / 4.0;
            draw_text(
                &mut pixels,
                canvas_width,
                canvas_height,
                left.saturating_sub(38),
                y.saturating_sub(4),
                &format_tick(value),
                colors.font,
            );
        }
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (left, top),
            (left, bottom),
            axis,
        );
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (left, bottom),
            (right, bottom),
            axis,
        );
        for tick in 0..=4 {
            let x = left + (u64::from(right - left) * tick / 4) as u32;
            draw_styled_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (x, top),
                (x, bottom),
                DashStroke {
                    color: colors.grid,
                    pattern: &grid_dash,
                    offset: 0.0,
                    width: 1.0,
                },
            );
            draw_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (x, bottom),
                (x, bottom + 2),
                axis,
            );
            let timestamp = graph.start.saturating_add(
                ((graph.end.saturating_sub(graph.start) as i128 * tick as i128) / 4) as i64,
            );
            draw_text(
                &mut pixels,
                canvas_width,
                canvas_height,
                x.saturating_sub(12),
                bottom + 4,
                &timestamp.to_string(),
                colors.font,
            );
        }
        if let Some(title) = title {
            let title = ascii_text(title);
            let x = canvas_width.saturating_sub((title.len() as u32).saturating_mul(8)) / 2;
            draw_text(
                &mut pixels,
                canvas_width,
                canvas_height,
                x,
                2,
                &title,
                colors.font,
            );
        }
        if let Some(label) = vertical_label {
            draw_vertical_text(
                &mut pixels,
                canvas_width,
                canvas_height,
                8,
                top + height / 2 - (label.len() as u32 * 8 / 2),
                &ascii_text(label),
                colors.font,
                vertical_label_angle,
            );
        }
    }
    let prepared_series = prepare_graph_series(graph, series);
    for (series_index, item) in series.iter().enumerate() {
        let prepared = &prepared_series[series_index];
        let color = item.color.unwrap_or([0, 0, 0, 0]);
        if item.style == "hrule" {
            let value = item.rule_value.expect("HRULE carries a numeric value");
            if value < minimum || value > maximum {
                continue;
            }
            let y = y_for(value);
            draw_styled_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (left, y),
                (right, y),
                DashStroke {
                    color,
                    pattern: &item.dash_pattern,
                    offset: item.dash_offset,
                    width: item.line_width,
                },
            );
            continue;
        }
        if item.style == "vrule" {
            let time = item.rule_time.expect("VRULE carries a timestamp");
            if time < graph.start || time > graph.end {
                continue;
            }
            let duration = graph.end.saturating_sub(graph.start);
            let x = if duration == 0 {
                left
            } else {
                left + ((i128::from(time.saturating_sub(graph.start)) * i128::from(width))
                    / i128::from(duration)) as u32
            };
            draw_styled_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (x, top),
                (x, bottom),
                DashStroke {
                    color,
                    pattern: &item.dash_pattern,
                    offset: item.dash_offset,
                    width: item.line_width,
                },
            );
            continue;
        }
        if item.style == "tick" {
            // RRDtool's GF_TICK drawing loop only paints for positive or
            // negative fractions; zero disables the tick stroke entirely.
            if item.tick_fraction == 0.0 {
                continue;
            }
            let tick_height = (f64::from(height) * item.tick_fraction.abs())
                .round()
                .max(1.0) as u32;
            let (tick_start, tick_end) =
                tick_mark_range(top, bottom, tick_height, item.tick_fraction);
            for (row_index, value) in prepared.values.iter().enumerate() {
                let Some(value) = *value else {
                    continue;
                };
                if !value.is_finite() || value == 0.0 {
                    continue;
                }
                let x = if graph.rows.len() <= 1 {
                    left
                } else {
                    left + ((row_index as u64 * u64::from(width))
                        / u64::try_from(graph.rows.len() - 1).unwrap_or(1))
                        as u32
                };
                draw_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (x, tick_start),
                    (x, tick_end),
                    color,
                );
            }
            continue;
        }
        let Some(color) = item.color else { continue };
        let points = prepared
            .values
            .iter()
            .enumerate()
            .map(|(row_index, value)| {
                let x = if graph.rows.len() <= 1 {
                    left
                } else {
                    left + ((row_index as u64 * u64::from(width))
                        / u64::try_from(graph.rows.len() - 1).unwrap_or(1))
                        as u32
                };
                value
                    .to_owned()
                    .filter(|value| value.is_finite())
                    .map(|value| (x, y_for(value)))
            })
            .collect::<Vec<_>>();
        let base_points = prepared
            .baseline
            .iter()
            .enumerate()
            .map(|(row_index, value)| {
                let x = if graph.rows.len() <= 1 {
                    left
                } else {
                    left + ((row_index as u64 * u64::from(width))
                        / u64::try_from(graph.rows.len() - 1).unwrap_or(1))
                        as u32
                };
                value.map(|value| (x, y_for(value)))
            })
            .collect::<Vec<_>>();
        if item.style == "area" {
            for index in 0..points.len().saturating_sub(1) {
                let pair = [&points[index], &points[index + 1]];
                let base_pair = [&base_points[index], &base_points[index + 1]];
                if let (
                    [Some((x1, y1)), Some((x2, y2))],
                    [Some((_, base_y1)), Some((_, base_y2))],
                ) = (pair, base_pair)
                {
                    let span = x2.saturating_sub(*x1).max(1);
                    for x in *x1..=*x2 {
                        let ratio = f64::from(x - *x1) / f64::from(span);
                        let y = (*y1 as f64 + (*y2 as f64 - *y1 as f64) * ratio).round() as u32;
                        let baseline = (*base_y1 as f64
                            + (*base_y2 as f64 - *base_y1 as f64) * ratio)
                            .round() as u32;
                        let low = y.min(baseline);
                        let high = y.max(baseline);
                        for fill_y in low..=high {
                            let fill_color = if let Some(color2) = item.color2 {
                                let distance = fill_y.abs_diff(y);
                                let span = if item.grad_height == 0.0 {
                                    high.saturating_sub(low).max(1)
                                } else {
                                    item.grad_height.abs().round().max(1.0) as u32
                                };
                                interpolate_graph_color(
                                    color,
                                    color2,
                                    f64::from(distance) / f64::from(span),
                                )
                            } else {
                                color
                            };
                            set_pixel(
                                &mut pixels,
                                canvas_width,
                                canvas_height,
                                x,
                                fill_y,
                                fill_color,
                            );
                        }
                    }
                }
            }
        }
        let mut stroke_offset = item.dash_offset;
        for pair in points.windows(2) {
            if let [Some(start), Some(end)] = pair {
                stroke_offset += draw_styled_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    *start,
                    *end,
                    DashStroke {
                        color,
                        pattern: &item.dash_pattern,
                        offset: stroke_offset,
                        width: item.line_width,
                    },
                );
            }
        }
    }
    for legend in legends {
        let x = legend.x.max(0.0) as u32;
        let y = legend.y.max(0.0) as u32;
        let item = series
            .iter()
            .find(|item| item.variable == format!("#{}", legend.gdes_index));
        let text_x = match item.and_then(|item| item.color.map(|color| (item, color))) {
            Some((item, color)) => {
                draw_styled_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (x, y.saturating_sub(4)),
                    (x + 10, y.saturating_sub(4)),
                    DashStroke {
                        color,
                        pattern: &item.dash_pattern,
                        offset: item.dash_offset,
                        width: item.line_width.max(1.0),
                    },
                );
                x + 13
            }
            None => x,
        };
        draw_text(
            &mut pixels,
            canvas_width,
            canvas_height,
            text_x,
            y.saturating_sub(9),
            &ascii_text(legend.text.trim_start_matches(' ')),
            colors.font,
        );
    }
    if border_width > 0 && !only_graph {
        for border in 0..border_width.min(canvas_width.min(canvas_height) / 2) {
            let right_edge = canvas_width.saturating_sub(1 + border);
            let bottom_edge = canvas_height.saturating_sub(1 + border);
            draw_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (border, border),
                (right_edge, border),
                colors.shade_a,
            );
            draw_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (border, border),
                (border, bottom_edge),
                colors.shade_a,
            );
            draw_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (border, bottom_edge),
                (right_edge, bottom_edge),
                colors.shade_b,
            );
            draw_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (right_edge, border),
                (right_edge, bottom_edge),
                colors.shade_b,
            );
        }
    }
    if !only_graph {
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (left, top),
            (left.saturating_sub(3), top + 5),
            colors.arrow,
        );
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (left, top),
            (left + 3, top + 5),
            colors.arrow,
        );
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (right, bottom),
            (right.saturating_sub(5), bottom.saturating_sub(3)),
            colors.arrow,
        );
        draw_line(
            &mut pixels,
            canvas_width,
            canvas_height,
            (right, bottom),
            (right.saturating_sub(5), bottom + 3),
            colors.arrow,
        );
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, canvas_width, canvas_height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
    }
    Ok(output)
}

fn interpolate_graph_color(first: [u8; 4], second: [u8; 4], ratio: f64) -> [u8; 4] {
    let ratio = ratio.clamp(0.0, 1.0);
    std::array::from_fn(|index| {
        (f64::from(first[index]) * (1.0 - ratio) + f64::from(second[index]) * ratio).round() as u8
    })
}

fn composite_rgb(foreground: [u8; 4], background: [u8; 3]) -> [u8; 3] {
    let alpha = f64::from(foreground[3]) / 255.0;
    std::array::from_fn(|channel| {
        (f64::from(foreground[channel]) * alpha + f64::from(background[channel]) * (1.0 - alpha))
            .round() as u8
    })
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info()?;
    Ok((reader.info().width, reader.info().height))
}

fn format_imginfo(
    format: &str,
    filename: &str,
    width: u32,
    height: u32,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut output = String::new();
    let mut chars = format.chars().peekable();
    let mut argument = 0;
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let conversion = chars.next().ok_or("invalid --imginfo format")?;
        if conversion == '%' {
            output.push('%');
            continue;
        }
        let value = match (argument, conversion) {
            (0, 's') => filename.to_owned(),
            (1, 'l') if chars.next() == Some('u') => width.to_string(),
            (2, 'l') if chars.next() == Some('u') => height.to_string(),
            _ => return Err("--imginfo format must use %s, %lu, and %lu in that order".into()),
        };
        output.push_str(&value);
        argument += 1;
    }
    if argument != 3 {
        return Err("--imginfo format must use %s, %lu, and %lu in that order".into());
    }
    Ok(output)
}

fn tick_mark_range(top: u32, bottom: u32, height: u32, fraction: f64) -> (u32, u32) {
    let height = height.min(bottom.saturating_sub(top));
    if fraction < 0.0 {
        (top, top + height)
    } else {
        (bottom - height, bottom)
    }
}

struct PreparedGraphSeries {
    values: Vec<Option<f64>>,
    baseline: Vec<Option<f64>>,
}

fn prepare_graph_series(
    graph: &RenderedGraphXport,
    series: &[GraphSeries],
) -> Vec<PreparedGraphSeries> {
    let mut prepared = Vec::with_capacity(series.len());
    let mut previous_values = vec![Some(0.0); graph.rows.len()];
    let mut has_previous_graph = false;
    for item in series {
        let mut values = vec![None; graph.rows.len()];
        let mut baseline = vec![None; graph.rows.len()];
        let column = graph
            .variables
            .iter()
            .position(|variable| variable == &item.variable);
        if let Some(column) = column {
            for (row_index, row) in graph.rows.iter().enumerate() {
                let base = if item.stack && has_previous_graph {
                    previous_values[row_index].unwrap_or(0.0)
                } else {
                    0.0
                };
                baseline[row_index] = Some(base);
                let raw_value = row
                    .get(column)
                    .copied()
                    .flatten()
                    .filter(|value| value.is_finite());
                values[row_index] = match raw_value {
                    Some(value) => Some(base + value),
                    None if item.stack && has_previous_graph => Some(base),
                    None => None,
                };
            }
            if matches!(item.style, "line" | "area") {
                previous_values.clone_from(&values);
                has_previous_graph = true;
            }
        }
        prepared.push(PreparedGraphSeries { values, baseline });
    }
    prepared
}

fn format_tick(value: f64) -> String {
    if value == 0.0 {
        return String::from("0");
    }
    if value.abs() >= 10_000.0 || value.abs() < 0.01 {
        format!("{value:.1e}")
    } else {
        format!("{value:.2}")
    }
}

fn ascii_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '?' })
        .collect()
}

fn draw_text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    text: &str,
    color: [u8; 4],
) {
    use font8x8::{BASIC_FONTS, UnicodeFonts};
    for (character_index, character) in text.chars().enumerate() {
        let glyph = BASIC_FONTS
            .get(character)
            .unwrap_or_else(|| BASIC_FONTS.get('?').expect("ASCII fallback glyph exists"));
        let origin_x = x.saturating_add(character_index as u32 * 8);
        for (row, bits) in glyph.iter().copied().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) != 0 {
                    set_pixel(
                        pixels,
                        width,
                        height,
                        origin_x + column,
                        y + row as u32,
                        color,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // Mirrors the pixel buffer and geometry inputs used by draw_text.
fn draw_vertical_text(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    text: &str,
    color: [u8; 4],
    angle_degrees: f64,
) {
    use font8x8::{BASIC_FONTS, UnicodeFonts};
    let source_width = text.chars().count() as f64 * 8.0;
    let radians = -angle_degrees.to_radians();
    let (sin, cos) = radians.sin_cos();
    let corners = [
        (0.0, 0.0),
        (source_width, 0.0),
        (0.0, 8.0),
        (source_width, 8.0),
    ];
    let min_x = corners
        .iter()
        .map(|(cx, cy)| cx * cos - cy * sin)
        .fold(f64::INFINITY, f64::min);
    let min_y = corners
        .iter()
        .map(|(cx, cy)| cx * sin + cy * cos)
        .fold(f64::INFINITY, f64::min);
    for (character_index, character) in text.chars().enumerate() {
        let glyph = BASIC_FONTS
            .get(character)
            .unwrap_or_else(|| BASIC_FONTS.get('?').expect("ASCII fallback glyph exists"));
        for (row, bits) in glyph.iter().copied().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) != 0 {
                    let source_x = character_index as f64 * 8.0 + column as f64;
                    let source_y = row as f64;
                    let target_x =
                        (source_x * cos - source_y * sin - min_x).round().max(0.0) as u32;
                    let target_y =
                        (source_x * sin + source_y * cos - min_y).round().max(0.0) as u32;
                    set_pixel(
                        pixels,
                        width,
                        height,
                        x.saturating_add(target_x),
                        y.saturating_add(target_y),
                        color,
                    );
                }
            }
        }
    }
}

fn set_pixel(pixels: &mut [u8], width: u32, height: u32, x: u32, y: u32, color: [u8; 4]) {
    if x >= width || y >= height {
        return;
    }
    let offset = ((y as usize * width as usize) + x as usize) * 3;
    let alpha = u16::from(color[3]);
    let inverse_alpha = 255 - alpha;
    for (channel, source) in color[..3].iter().enumerate() {
        let destination = u16::from(pixels[offset + channel]);
        pixels[offset + channel] =
            ((u16::from(*source) * alpha + destination * inverse_alpha + 127) / 255) as u8;
    }
}

fn draw_line(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    (x0, y0): (u32, u32),
    (x1, y1): (u32, u32),
    color: [u8; 4],
) {
    let (mut x, mut y) = (x0 as i32, y0 as i32);
    let (x1, y1) = (x1 as i32, y1 as i32);
    let dx = (x1 - x).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let dy = -(y1 - y).abs();
    let sy = if y < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        set_pixel(pixels, width, height, x as u32, y as u32, color);
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
    }
}

fn draw_line_with_width(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    start: (u32, u32),
    end: (u32, u32),
    color: [u8; 4],
    stroke_width: f64,
) {
    if stroke_width <= 0.0 {
        return;
    }
    if stroke_width <= 1.0 {
        draw_line(pixels, width, height, start, end, color);
        return;
    }
    // rrd_graph_helper.c only rejects negative widths and leaves the rest to
    // cairo_set_line_width. A stroke whose half width reaches past every
    // corner covers the whole surface, which is what Cairo paints.
    if stroke_width / 2.0 >= f64::from(width).hypot(f64::from(height)) {
        for py in 0..height {
            for px in 0..width {
                set_pixel(pixels, width, height, px, py, color);
            }
        }
        return;
    }
    let radius = (stroke_width / 2.0).ceil() as i32;
    let mut x = start.0 as i32;
    let mut y = start.1 as i32;
    let (x1, y1) = (end.0 as i32, end.1 as i32);
    let dx = (x1 - x).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let dy = -(y1 - y).abs();
    let sy = if y < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        for offset_y in -radius..=radius {
            for offset_x in -radius..=radius {
                if f64::from(offset_x * offset_x + offset_y * offset_y).sqrt()
                    <= stroke_width / 2.0 + 0.5
                {
                    let px = x + offset_x;
                    let py = y + offset_y;
                    if px >= 0 && py >= 0 {
                        set_pixel(pixels, width, height, px as u32, py as u32, color);
                    }
                }
            }
        }
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
    }
}

struct DashStroke<'a> {
    color: [u8; 4],
    pattern: &'a [f64],
    offset: f64,
    width: f64,
}

fn draw_styled_line(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    start: (u32, u32),
    end: (u32, u32),
    stroke: DashStroke<'_>,
) -> f64 {
    let DashStroke {
        color,
        pattern,
        offset,
        width: stroke_width,
    } = stroke;
    if pattern.is_empty() {
        draw_line_with_width(pixels, width, height, start, end, color, stroke_width);
        return f64::from(start.0.abs_diff(end.0).max(start.1.abs_diff(end.1)));
    }
    if stroke_width <= 0.0 {
        return 0.0;
    }
    let mut x = start.0 as i32;
    let mut y = start.1 as i32;
    let (x1, y1) = (end.0 as i32, end.1 as i32);
    let dx = (x1 - x).abs();
    let sx = if x < x1 { 1 } else { -1 };
    let dy = -(y1 - y).abs();
    let sy = if y < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    let total: f64 = if pattern.len() == 1 {
        pattern[0] * 2.0
    } else {
        pattern.iter().sum()
    };
    let phase = offset.rem_euclid(total);
    let mut distance = 0_u32;
    loop {
        let mut position = (f64::from(distance) + phase).rem_euclid(total);
        let mut index = 0;
        while index < pattern.len() && position >= pattern[index] {
            position -= pattern[index];
            index += 1;
        }
        if index % 2 == 0 && x >= 0 && y >= 0 {
            draw_line_with_width(
                pixels,
                width,
                height,
                (x as u32, y as u32),
                (x as u32, y as u32),
                color,
                stroke_width,
            );
        }
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += sx;
        }
        if doubled <= dx {
            error += dx;
            y += sy;
        }
        distance = distance.saturating_add(1);
    }
    f64::from(distance)
}

/// SI scaling state that RRDtool's print_calc shares across every PRINT and
/// GPRINT of one graph.
struct GraphSiScale {
    base: u32,
    magnitude: Option<f64>,
    symbol: &'static str,
}

impl GraphSiScale {
    fn new(base: u32) -> Self {
        Self {
            base,
            magnitude: None,
            symbol: "",
        }
    }
}

/// Which printf formats a PRINT value: print_calc uses the C library
/// (`sprintf_alloc`, locale aware) and rrd_xport.c:1235 uses `rrd_snprintf`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GraphPrintf {
    Libc,
    Rrd,
}

fn format_graph_numeric(
    value: f64,
    format: &str,
    si_scale: &mut GraphSiScale,
    printf: GraphPrintf,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::ffi::CString;

    let parsed = parse_graph_numeric_format(format)
        .map_err(|_| format!("bad format for PRINT in \"{format}'"))?;
    let scaled_value = match parsed.si_symbol {
        Some(index) => {
            // %S reuses the magnitude chosen by the first scaled value, unless
            // that value scaled to zero; %s always picks a new one.
            let reuse = format.as_bytes()[parsed.substitutions[index].end - 1] == b'S';
            match si_scale.magnitude {
                Some(magnitude) if reuse => value / magnitude,
                _ => {
                    let (scaled, magnitude, symbol) = graph_si_scale(value, si_scale.base);
                    si_scale.magnitude = (!reuse || scaled != 0.0).then_some(magnitude);
                    si_scale.symbol = symbol;
                    scaled
                }
            }
        }
        None => value,
    };
    let si_symbol = si_scale.symbol;
    if printf == GraphPrintf::Rrd {
        // RRDtool rewrites %S to %s before formatting.
        let mut format = format.to_owned();
        if let Some(index) = parsed.si_symbol {
            let end = parsed.substitutions[index].end;
            format.replace_range(end - 1..end, "s");
        }
        return Ok(rondi::rrd_snprintf::rrd_snprintf(
            &format,
            &[
                rondi::rrd_snprintf::Arg::Double(scaled_value),
                rondi::rrd_snprintf::Arg::Str(si_symbol),
            ],
        ));
    }
    let mut output = String::with_capacity(format.len() + 32);
    let mut cursor = 0;
    for substitution in parsed.substitutions {
        append_graph_format_literal(&mut output, &format[cursor..substitution.start]);
        match substitution.kind {
            GraphFormatSubstitutionKind::Floating => {
                let conversion = CString::new(&format[substitution.start..substitution.end])?;
                let mut buffer = [0 as libc::c_char; 4096];
                let length = unsafe {
                    libc::snprintf(
                        buffer.as_mut_ptr(),
                        buffer.len(),
                        conversion.as_ptr(),
                        scaled_value,
                    )
                };
                if length < 0 || length as usize >= buffer.len() {
                    return Err("graph print output exceeds its buffer".into());
                }
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length as usize)
                };
                output.push_str(&String::from_utf8_lossy(bytes));
            }
            GraphFormatSubstitutionKind::SiSymbol => output.push_str(si_symbol),
        }
        cursor = substitution.end;
    }
    append_graph_format_literal(&mut output, &format[cursor..]);
    Ok(output)
}

#[derive(Clone, Copy)]
struct GraphFormatSubstitution {
    start: usize,
    end: usize,
    kind: GraphFormatSubstitutionKind,
}

#[derive(Clone, Copy)]
enum GraphFormatSubstitutionKind {
    Floating,
    SiSymbol,
}

struct ParsedGraphNumericFormat {
    substitutions: Vec<GraphFormatSubstitution>,
    si_symbol: Option<usize>,
}

fn parse_graph_numeric_format(format: &str) -> Result<ParsedGraphNumericFormat, ()> {
    let bytes = format.as_bytes();
    let mut substitutions = Vec::with_capacity(2);
    let mut si_symbol = None;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        let Some(&next) = bytes.get(index) else {
            return Err(());
        };
        if next == b'%' {
            index += 1;
            continue;
        }
        if matches!(next, b's' | b'S') {
            if substitutions.is_empty() || si_symbol.is_some() {
                return Err(());
            }
            si_symbol = Some(substitutions.len());
            substitutions.push(GraphFormatSubstitution {
                start,
                end: index + 1,
                kind: GraphFormatSubstitutionKind::SiSymbol,
            });
            index += 1;
            continue;
        }
        if substitutions
            .iter()
            .any(|item| matches!(item.kind, GraphFormatSubstitutionKind::Floating))
        {
            return Err(());
        }
        if matches!(next, b'-' | b'+' | b' ' | b'0' | b'#') {
            index += 1;
        }
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let precision_start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if precision_start == index {
                return Err(());
            }
        }
        if bytes.get(index) != Some(&b'l') {
            return Err(());
        }
        index += 1;
        if !bytes
            .get(index)
            .is_some_and(|code| matches!(code, b'e' | b'E' | b'f' | b'F' | b'g' | b'G'))
        {
            return Err(());
        }
        index += 1;
        substitutions.push(GraphFormatSubstitution {
            start,
            end: index,
            kind: GraphFormatSubstitutionKind::Floating,
        });
    }
    if !substitutions
        .iter()
        .any(|item| matches!(item.kind, GraphFormatSubstitutionKind::Floating))
    {
        return Err(());
    }
    if let Some(symbol_index) = si_symbol {
        let numeric_index = substitutions
            .iter()
            .position(|item| matches!(item.kind, GraphFormatSubstitutionKind::Floating))
            .ok_or(())?;
        if symbol_index < numeric_index {
            return Err(());
        }
    }
    Ok(ParsedGraphNumericFormat {
        substitutions,
        si_symbol,
    })
}

fn append_graph_format_literal(output: &mut String, literal: &str) {
    let mut chars = literal.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' && chars.clone().next() == Some('%') {
            output.push('%');
            chars.next();
        } else {
            output.push(ch);
        }
    }
}

fn graph_si_scale(value: f64, base: u32) -> (f64, f64, &'static str) {
    const SYMBOLS: [&str; 13] = [
        "a", "f", "p", "n", "u", "m", " ", "k", "M", "G", "T", "P", "E",
    ];
    if value == 0.0 || value.is_nan() {
        return (value, 1.0, SYMBOLS[6]);
    }
    if value.is_infinite() {
        // auto_scale converts floor(log(inf)) to int: aarch64 saturates to
        // INT_MAX and divides by pow(base, INT_MAX) = inf, while x86_64 yields
        // INT_MIN and divides by zero. Either index is outside the table.
        #[cfg(target_arch = "x86_64")]
        let factor = 0.0;
        #[cfg(not(target_arch = "x86_64"))]
        let factor = f64::INFINITY;
        return (value / factor, factor, "?");
    }
    let base = f64::from(base);
    let exponent = (value.abs().ln() / base.ln()).floor() as i32;
    let factor = base.powf(f64::from(exponent));
    let symbol = usize::try_from(exponent + 6)
        .ok()
        .and_then(|index| SYMBOLS.get(index))
        .copied()
        .unwrap_or("?");
    (value / factor, factor, symbol)
}

/// `escapeJSON` from rrd_xport.c.
fn escape_json(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' | '\\' => {
                output.push('\\');
                output.push(ch);
            }
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if (ch as u32) < 0x20 => write!(output, "\\u{:04x}", ch as u32).unwrap(),
            ch => output.push(ch),
        }
    }
    output
}

/// Port of `rrd_xport_format_addprints`. Elements are visited in definition
/// order and written to separate prints, gprints and rules lists. A numeric
/// format error is reported after the head of the document was written.
fn format_xport_addprints(
    json: bool,
    output: &mut String,
    im: &mut rondi::graph::GraphImage,
    si_base: u32,
) -> Result<(), String> {
    use rondi::graph::Gf;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let mut tmvdef = local_tm(now);
    let (mut prints, mut gprints, mut rules) = (String::new(), String::new(), String::new());
    let mut si_scale = GraphSiScale::new(si_base);
    // time_clean writes to a different buffer than the one printed, so a
    // never-set VDEF time repeats the previous formatted text.
    let mut dbuf = String::new();
    let entry = |tag: &str, text: &str| {
        if json {
            format!(",\n        {{ \"{tag}\": \"{text}\" }}")
        } else {
            format!("        <{tag}>{text}</{tag}>\n")
        }
    };
    for i in 0..im.gdes.len() {
        let element = &im.gdes[i];
        match element.gf {
            Gf::Print | Gf::Gprint => {
                let source = element.vidx.map(|vidx| &im.gdes[vidx]);
                if let Some(source) = source.filter(|source| source.gf == Gf::Vdef) {
                    tmvdef = local_tm(source.vf.when);
                }
                let never = source.is_some_and(|source| source.vf.never);
                let printval = im.print_value(i);
                let element = &im.gdes[i];
                if element.strftm {
                    if !never {
                        dbuf = c_strftime(&element.format, &tmvdef, 1024).unwrap_or_default();
                    }
                } else {
                    match format_graph_numeric(
                        printval,
                        &element.format,
                        &mut si_scale,
                        GraphPrintf::Rrd,
                    ) {
                        Ok(text) => dbuf = text,
                        Err(_) => {
                            return Err(format!("bad format for PRINT in \"{}'", element.format));
                        }
                    }
                }
                let text = if json {
                    escape_json(&dbuf)
                } else {
                    dbuf.clone()
                };
                let (tag, buffer) = if element.gf == Gf::Print {
                    ("print", &mut prints)
                } else {
                    ("gprint", &mut gprints)
                };
                buffer.push_str(&entry(tag, &text));
            }
            Gf::Comment => {
                let text = if json {
                    escape_json(&element.legend)
                } else {
                    element.legend.clone()
                };
                gprints.push_str(&entry("comment", &text));
            }
            Gf::Line => {
                let legend = element
                    .legend
                    .trim_start_matches(|ch: char| ch.is_ascii_whitespace());
                let text = if json {
                    escape_json(legend)
                } else {
                    legend.to_owned()
                };
                gprints.push_str(&entry("line", &text));
            }
            Gf::Area => gprints.push_str(&entry("area", &element.legend)),
            Gf::Stack => gprints.push_str(&entry("stack", &element.legend)),
            Gf::TextAlign => {
                let align = match element.txtalign {
                    rondi::graph::TextAlign::Left => "left",
                    rondi::graph::TextAlign::Right => "right",
                    rondi::graph::TextAlign::Center => "center",
                    rondi::graph::TextAlign::Justified => "justified",
                };
                gprints.push_str(&entry("align", align));
            }
            Gf::Hrule => rules.push_str(&entry("hrule", &format_xport_value(element.vf.val))),
            Gf::Vrule => rules.push_str(&entry("vrule", &element.vf.when.to_string())),
            _ => {}
        }
    }
    let sections = [
        ("prints", prints, true),
        ("gprints", gprints, false),
        ("rules", rules, false),
    ];
    for (name, data, first) in sections {
        if data.is_empty() {
            continue;
        }
        if json {
            if first {
                writeln!(output, "    \"{name}\": [").unwrap();
                output.push_str(&data[2..]);
                output.push_str("\n        ],\n");
            } else {
                writeln!(output, "    ,\"{name}\": [").unwrap();
                output.push_str(&data[2..]);
                output.push_str("\n        ]\n");
            }
        } else {
            writeln!(output, "    <{name}>").unwrap();
            output.push_str(&data);
            writeln!(output, "    </{name}>").unwrap();
        }
    }
    Ok(())
}

/// Port of `rrd_xport_format_xmljson`. Flags: 1 JSON, 2 show time, 4
/// enumerate value tags. RRDtool only removes a trailing comma after the
/// meta lists when the document is built in memory (`graph -`); xport and
/// graph files stream it out. On a format error the partial document and
/// the message are both returned.
fn format_xport_xmljson(
    flags: u8,
    im: &mut rondi::graph::GraphImage,
    data: &rondi::graph::XportData,
    si_base: u32,
    in_memory: bool,
) -> (String, Option<String>) {
    let json = flags & 1 != 0;
    let show_time = flags & 2 != 0;
    let enum_ds = flags & 4 != 0;
    let step = data.step as i64;
    let mut output = String::new();
    if json {
        output.push_str("{ \"about\": \"RRDtool graph JSON output\",\n  \"meta\": {\n");
        writeln!(output, "    \"start\": {},", data.start + step).unwrap();
        writeln!(output, "    \"end\": {},", data.end).unwrap();
        writeln!(output, "    \"step\": {step},").unwrap();
        output.push_str("    \"legend\": [\n");
    } else {
        output.push_str("<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n\n<xport>\n  <meta>\n");
        writeln!(output, "    <start>{}</start>", data.start + step).unwrap();
        writeln!(output, "    <end>{}</end>", data.end).unwrap();
        writeln!(output, "    <step>{step}</step>").unwrap();
        writeln!(output, "    <rows>{}</rows>", data.rows.len()).unwrap();
        writeln!(output, "    <columns>{}</columns>", data.legends.len()).unwrap();
        output.push_str("    <legend>\n");
    }
    for (index, legend) in data.legends.iter().enumerate() {
        let entry = legend.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
        if json {
            let comma = if index + 1 < data.legends.len() {
                ","
            } else {
                ""
            };
            writeln!(output, "      \"{}\"{comma}", escape_json(entry)).unwrap();
        } else {
            writeln!(output, "      <entry>{entry}</entry>").unwrap();
        }
    }
    output.push_str(if json {
        "          ]\n"
    } else {
        "    </legend>\n"
    });
    if let Err(error) = format_xport_addprints(json, &mut output, im, si_base) {
        return (output, Some(error));
    }
    if in_memory && output.as_bytes().get(output.len().wrapping_sub(2)) == Some(&b',') {
        let last = output.pop().unwrap_or('\n');
        output.pop();
        output.push(last);
    }
    output.push_str(if json {
        "     },\n  \"data\": [\n"
    } else {
        "  </meta>\n  <data>\n"
    });
    for (row_index, row) in data.rows.iter().enumerate() {
        let time = data.start + (row_index as i64 + 1) * step;
        if json {
            output.push_str("    [ ");
            if show_time {
                write!(output, "\"{time}\",").unwrap();
            }
        } else if show_time {
            write!(output, "    <row><t>{time}</t>").unwrap();
        } else {
            output.push_str("    <row>");
        }
        for (column, value) in row.iter().enumerate() {
            if json {
                if value.is_nan() || value.is_infinite() {
                    output.push_str("null");
                } else {
                    output.push_str(&format_xport_value(*value));
                }
                if column + 1 < row.len() {
                    output.push_str(", ");
                }
            } else {
                let tag = if enum_ds {
                    format!("v{column}")
                } else {
                    "v".to_owned()
                };
                if value.is_nan() {
                    write!(output, "<{tag}>NaN</{tag}>").unwrap();
                } else {
                    write!(output, "<{tag}>{}</{tag}>", format_xport_value(*value)).unwrap();
                }
            }
        }
        if json {
            output.push_str(if time <= data.end - step {
                " ],\n"
            } else {
                " ]\n"
            });
        } else {
            output.push_str("</row>\n");
        }
    }
    output.push_str(if json {
        "  ]\n}\n"
    } else {
        "  </data>\n</xport>\n"
    });
    (output, None)
}

/// Port of `rrd_xport_format_sv` for CSV, TSV and SSV.
fn format_xport_sv(separator: char, data: &rondi::graph::XportData) -> String {
    let mut output = String::from("\"time\"");
    for legend in &data.legends {
        let entry = legend.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
        write!(output, "{separator}\"{entry}\"").unwrap();
    }
    output.push_str("\r\n");
    let step = data.step as i64;
    for (row_index, row) in data.rows.iter().enumerate() {
        write!(output, "{}", data.start + (row_index as i64 + 1) * step).unwrap();
        for value in row {
            if value.is_nan() {
                write!(output, "{separator}\"NaN\"").unwrap();
            } else {
                write!(output, "{separator}\"{}\"", format_xport_value(*value)).unwrap();
            }
        }
        output.push_str("\r\n");
    }
    output
}

fn rrdtool_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    rrdtool_update_impl(args, false)
}

fn rrdtool_updatev(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    rrdtool_update_impl(args, true)
}

fn rrdtool_update_impl(args: &[String], verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        if verbose {
            print!("{}", include_str!("help/updatev.txt"));
        } else {
            print!("{}", include_str!("help/update.txt"));
        }
        return Ok(());
    }
    // rrd_update.c:304 (updatev) and :679 (update).
    const UPDATE_LONGOPTS: &[LongOpt] = &[
        opt("template", b't' as i32, ArgType::Required),
        opt("daemon", b'd' as i32, ArgType::Required),
        opt("skip-past-updates", b's' as i32, ArgType::None),
        opt("locking", b'L' as i32, ArgType::Required),
    ];
    const UPDATEV_LONGOPTS: &[LongOpt] = &[
        opt("template", b't' as i32, ArgType::Required),
        opt("skip-past-updates", b's' as i32, ArgType::None),
        opt("locking", b'L' as i32, ArgType::Required),
    ];
    let mut template = None;
    let mut skip_past_updates = false;
    let mut daemon_address = None;
    let mut locking = String::new();
    let mut options = OptParse::new(args.to_vec());
    loop {
        let option = options.long(if verbose {
            UPDATEV_LONGOPTS
        } else {
            UPDATE_LONGOPTS
        });
        match u8::try_from(option).map(char::from) {
            _ if option == optparse::DONE => break,
            Ok('t') => {
                template = Some(
                    options
                        .value()
                        .split(':')
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
            }
            Ok('s') => skip_past_updates = true,
            Ok('d') => daemon_address = Some(options.value().to_owned()),
            Ok('L') => {
                if !rondi::is_rrd_locking_mode(options.value()) {
                    return Err(format!("unsupported locking mode '{}'\n", options.value()).into());
                }
                locking = options.value().to_owned();
            }
            Ok('?') => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let [filename_text, samples @ ..] = options.positionals() else {
        return Err("Not enough arguments".into());
    };
    if samples.is_empty() {
        return Err("Not enough arguments".into());
    }
    let filename = PathBuf::from(filename_text);
    let daemon_address = daemon_address.or_else(|| {
        std::env::var("RRDCACHED_ADDRESS")
            .ok()
            .filter(|address| !address.is_empty())
    });
    if verbose {
        // rrd_update_v refuses only an empty RRDCACHED_ADDRESS and otherwise
        // updates the file directly.
        if std::env::var_os("RRDCACHED_ADDRESS").is_some_and(|address| address.is_empty()) {
            return Err("The \"RRDCACHED_ADDRESS\" environment variable is defined, but \"updatev\" cannot work with rrdcached. Either unset the environment variable or use \"update\" instead.".into());
        }
    }
    let daemon_address = if verbose { None } else { daemon_address };
    // Local RRD writes use RRDtool's per-file fcntl lock in the library, which
    // tries once unless $RRD_LOCKING says otherwise. A directory-wide Rondi
    // lock would serialize unrelated files and stop parallel poller updates.
    if daemon_address.is_none() {
        ensure_rrd_file_exists(&filename)?;
        let template = template.as_ref().map(|names| names.join(":"));
        let arguments = samples.iter().map(String::as_str).collect::<Vec<_>>();
        let (summaries, result) = rondi::with_rrd_locking(&locking, || {
            update_rrd_text(
                &filename,
                template.as_deref(),
                &arguments,
                skip_past_updates,
            )
        });
        if verbose {
            // rrd_update_v reports the rows written before a failure too.
            println!("return_value = {}", if result.is_ok() { 0 } else { -1 });
            let names = inspect_rrd(filename_text)
                .map(|info| info.data_sources)
                .unwrap_or_default();
            for summary in summaries {
                for (source, value) in names.iter().zip(summary.values) {
                    println!(
                        "[{}]RRA[{}][{}]DS[{}] = {}",
                        summary.timestamp,
                        summary.consolidation,
                        summary.pdp_per_row,
                        source.name,
                        format_rrd_scientific(value)
                    );
                }
            }
        }
        return result.map_err(|error| rrd_update_error(&filename, error));
    }
    let mut source_count = None;
    let source_names = if let Some(template) = &template {
        let info = inspect_rrd(filename_text)?;
        let mut indices = Vec::with_capacity(template.len());
        for name in template {
            indices.push(
                info.data_sources
                    .iter()
                    .position(|source| source.name == *name)
                    .ok_or_else(|| format!("unknown DS name '{name}'"))?,
            );
        }
        source_count = Some(info.data_sources.len());
        Some(indices)
    } else {
        None
    };
    let mut daemon_samples = Vec::new();
    for sample in samples {
        let (timestamp, values, at_style) =
            if let Some((timestamp, values)) = sample.split_once('@') {
                (timestamp, values, true)
            } else if let Some((timestamp, values)) = sample.split_once(':') {
                (timestamp, values, false)
            } else {
                return Err(format!(
                    "{}: expected timestamp not found in data source from {sample}",
                    filename.display()
                )
                .into());
            };
        let raw_values = values
            .split(':')
            .map(|value| {
                if value.eq_ignore_ascii_case("U") || value.eq_ignore_ascii_case("UNKNOWN") {
                    None
                } else {
                    Some(value.to_owned())
                }
            })
            .collect::<Vec<_>>();
        let numeric_values = values
            .split(':')
            .enumerate()
            .map(|(value_index, value)| {
                parse_rrd_update_value(value).map_or_else(|| {
                    let data_sources = inspect_rrd(filename_text)
                        .map(|info| info.data_sources)
                        .unwrap_or_default();
                    let source_index = template
                        .as_ref()
                        .and_then(|names| names.get(value_index))
                        .and_then(|name| {
                            data_sources
                                .iter()
                                .find(|source| source.name == *name)
                                .map(|source| source.kind.as_str())
                        })
                        .or_else(|| data_sources.get(value_index).map(|source| source.kind.as_str()))
                        .unwrap_or("GAUGE");
                    // update_pdp_prep converts DCOUNTER/DDERIVE text only when
                    // the previous sample is known; the library decides.
                    if matches!(source_index, "DCOUNTER" | "DDERIVE") {
                        return Ok(None);
                    }
                    Err(format!(
                        "{}: Function update_pdp_prep, case DST_{source_index} - Cannot convert '{value}' to float",
                        filename.display()
                    ))
                }, Ok)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let raw_values = if let Some(indices) = &source_names {
            if numeric_values.len() > indices.len() {
                return Err(format!(
                    "{}: found extra data on update argument: {}",
                    filename.display(),
                    indices.len() + 1
                )
                .into());
            }
            if numeric_values.len() < indices.len() {
                return Err(format!(
                    "{}: expected {} data source readings (got {}) from {timestamp}",
                    filename.display(),
                    indices.len(),
                    numeric_values.len()
                )
                .into());
            }
            let source_count = match source_count {
                Some(count) => count,
                None => *source_count.insert(inspect_rrd(filename_text)?.data_sources.len()),
            };
            let mut expanded = vec![None; source_count];
            for (source_index, raw_value) in indices.iter().zip(raw_values) {
                expanded[*source_index] = raw_value;
            }
            expanded
        } else {
            raw_values
        };
        let update_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        let update_time = if at_style {
            let seconds = rrd_parsetime(timestamp, update_now.floor() as i64)
                .map_err(|error| format!("{}: ds time: {timestamp}: {error}", filename.display()))?
                .absolute()
                .ok_or_else(|| {
                    format!(
                        "{}: specifying time relative to the 'start' or 'end' makes no sense here: {timestamp}",
                        filename.display()
                    )
                })?;
            UpdateTimestamp {
                seconds,
                microseconds: 0,
            }
        } else {
            parse_rrd_update_timestamp(timestamp, update_now).map_err(|_| {
                format!(
                    "{}: error while parsing time in get_time_from_reading - Cannot convert '{timestamp}' to float",
                    filename.display()
                )
            })?
        };
        let encoded_values = raw_values
            .iter()
            .map(|value| value.as_deref().unwrap_or("U"))
            .collect::<Vec<_>>()
            .join(":");
        daemon_samples.push(format!("{}:{encoded_values}", update_time.format_rrd()));
    }
    if let Some(address) = daemon_address {
        if !daemon_samples.is_empty() {
            #[cfg(unix)]
            send_rrdcached_update_on_stream(
                connect_rrdcached(&address)?,
                PathBuf::from(&args[1]).as_path(),
                &daemon_samples,
            )?;
            #[cfg(not(unix))]
            return Err("rrdcached updates are unavailable on this platform".into());
        }
    }
    Ok(())
}

/// RRDtool prefixes per-sample update failures with the file name.
fn rrd_update_error(filename: &Path, error: rondi::StoreError) -> Box<dyn std::error::Error> {
    match error {
        rondi::StoreError::RrdTimestamp(message) | rondi::StoreError::RrdExpression(message) => {
            format!("{}: {message}", filename.display()).into()
        }
        error => error.into(),
    }
}

fn rrdtool_flushcached(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!(
            "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n* flushcached - flush cached data out to an RRD file\n\n\trrdtool flushcached filename.rrd\n\t\t[-d|--daemon <address>]\n\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
        );
        return Ok(());
    }
    // rrd_flushcached.c:27.
    const LONGOPTS: &[LongOpt] = &[opt("daemon", b'd' as i32, ArgType::Required)];
    let mut daemon = None;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon = Some(options.value().to_owned()),
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let files = options.positionals().to_vec();
    if files.is_empty() {
        return Err(format!(
            "Usage: rrdtool {} [--daemon|-d <addr>] <file> [<file> ...]",
            args[0]
        )
        .into());
    }
    // rrd_client_connect reads RRDCACHED_ADDRESS only when no --daemon was
    // given, and the message prints opt_daemon with %s.
    let address = match &daemon {
        Some(address) => address.clone(),
        None => std::env::var("RRDCACHED_ADDRESS").unwrap_or_default(),
    };
    if address.is_empty() {
        return Err(format!(
            "Daemon address \"{}\" unknown. Please use the \"--daemon\" option to set an address on the command line or set the \"RRDCACHED_ADDRESS\" environment variable.",
            daemon.as_deref().unwrap_or("(null)")
        )
        .into());
    }
    let daemon = address;
    for filename in &files {
        if let Err(error) = send_rrdcached_flush(&daemon, filename) {
            if error.is::<RrdcachedConnectError>() {
                return Err(error);
            }
            // RRDtool 1.11.0 reports the number of filenames after the
            // command name, independent of the point at which flushing failed.
            let remaining = files.len().saturating_sub(1);
            return Err(format!(
                "Flushing of file \"{filename}\" failed: rrdcached@{daemon}: {error}. Skipping remaining {remaining} file{}.",
                if remaining == 1 { "" } else { "s" }
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn send_rrdcached_flush(address: &str, filename: &str) -> Result<(), Box<dyn std::error::Error>> {
    let escaped = filename.replace('\\', "\\\\").replace(' ', "\\ ");
    send_rrdcached_command(address, &format!("FLUSH {escaped}"))?;
    Ok(())
}

#[cfg(unix)]
fn send_rrdcached_command(
    address: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if command.contains('\n') || command.contains('\r') {
        return Err("invalid rrdcached socket or filename".into());
    }
    let mut stream = connect_rrdcached(address)?;
    writeln!(stream, "{command}")?;
    let mut response = String::new();
    read_bounded_line(
        &mut std::io::BufReader::new(stream),
        &mut response,
        MAX_RRDCACHED_LINE_BYTES,
    )?;
    let response = response.trim_end();
    if response.starts_with("0 ") {
        return Ok(response.to_owned());
    }
    Err(response
        .split_once(' ')
        .map(|(_, message)| message)
        .unwrap_or(response)
        .to_owned()
        .into())
}

#[cfg(not(unix))]
fn send_rrdcached_command(
    _address: &str,
    _command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    Err("rrdcached Unix socket commands are unavailable on this platform".into())
}

#[cfg(not(unix))]
fn send_rrdcached_flush(_address: &str, _filename: &str) -> Result<(), Box<dyn std::error::Error>> {
    Err("rrdcached Unix socket flushes are unavailable on this platform".into())
}

#[cfg(unix)]
fn send_rrdcached_update_on_stream(
    mut stream: impl std::io::Write + std::io::Read,
    filename: &std::path::Path,
    samples: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    if filename.to_string_lossy().chars().any(char::is_whitespace) {
        return Err("rrdtool update: invalid rrdcached socket or filename".into());
    }
    writeln!(
        stream,
        "UPDATE {} {}",
        filename.display(),
        samples.join(" ")
    )?;
    let mut response = String::new();
    read_bounded_line(
        &mut std::io::BufReader::new(stream),
        &mut response,
        MAX_RRDCACHED_LINE_BYTES,
    )?;
    let response = response.trim_end();
    if response.starts_with("0 ") {
        return Ok(());
    }
    Err(response
        .split_once(' ')
        .map(|(_, message)| message)
        .unwrap_or(response)
        .to_owned()
        .into())
}

/// xport and graph data values go through `rrd_snprintf("%0.10e")`
/// (rrd_xport.c:714, 968, 985), which is not correctly rounded and never
/// signs -0. updatev uses libc printf and keeps the sign.
fn format_xport_value(value: f64) -> String {
    rondi::rrd_snprintf::rrd_snprintf("%0.10e", &[rondi::rrd_snprintf::Arg::Double(value)])
}

fn format_rrd_scientific(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let scientific = format!("{value:.10e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("scientific notation has e");
    format!(
        "{mantissa}e{:+03}",
        exponent.parse::<i32>().expect("valid scientific exponent")
    )
}

fn rrdtool_last(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", rrdtool_last_help());
        return Ok(());
    }
    // rrd_last.c:18.
    const LONGOPTS: &[LongOpt] = &[opt("daemon", b'd' as i32, ArgType::Required)];
    let mut daemon_address = None::<String>;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon_address = Some(options.value().to_owned()),
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let [filename] = options.positionals() else {
        return Err(format!("Usage: rrdtool {} [--daemon|-d <addr>] <file>", args[0]).into());
    };
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        let escaped_filename = filename.replace('\\', "\\\\").replace(' ', "\\ ");
        let response = send_rrdcached_command(&address, &format!("LAST {escaped_filename}"))?;
        let timestamp = response
            .split_whitespace()
            .nth(1)
            .ok_or("invalid rrdcached LAST response")?;
        println!("{timestamp}");
    } else {
        println!("{}", inspect_rrd(filename)?.last_update);
    }
    Ok(())
}

fn rrdtool_last_help() -> &'static str {
    "RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>\n               Compiled \n\nUsage: rrdtool [options] command command_options\n* last - show last update time for RRD\n\n\trrdtool last filename.rrd\n\t\t[--daemon|-d address]\n\nRRDtool is distributed under the Terms of the GNU General\nPublic License Version 2. (www.gnu.org/copyleft/gpl.html)\n\nFor more information read the RRD manpages\n\n"
}

fn rrdtool_lastupdate(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/lastupdate.txt"));
        return Ok(());
    }
    // rrd_lastupdate.c:19.
    const LONGOPTS: &[LongOpt] = &[opt("daemon", b'd' as i32, ArgType::Required)];
    let mut daemon_address = None::<String>;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon_address = Some(options.value().to_owned()),
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let [filename] = options.positionals() else {
        return Err(format!("Usage: rrdtool {} [--daemon|-d <addr>] <file>", args[0]).into());
    };
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        send_rrdcached_flush(&address, filename)?;
    }
    ensure_rrd_file_exists(std::path::Path::new(filename))?;
    let info = inspect_rrd(filename)?;
    println!(
        " {}",
        info.data_sources
            .iter()
            .map(|ds| ds.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!();
    print!("{:>10}:", info.last_update);
    for source in info.data_sources {
        print!(" {}", source.last_value);
    }
    println!();
    Ok(())
}

fn rrdtool_first(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/first.txt"));
        return Ok(());
    }
    // rrd_first.c:18.
    const LONGOPTS: &[LongOpt] = &[
        opt("rraindex", 129, ArgType::Required),
        opt("daemon", b'd' as i32, ArgType::Required),
    ];
    let mut archive_index = 0_i32;
    let mut daemon_address = None::<String>;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            129 => {
                // rrd_first.c:33 stores strtol(optarg, &endptr, 0) in an int.
                archive_index = rondi::c_strtol(options.value(), 0).0 as i32;
                if archive_index < 0 {
                    return Err("invalid rraindex number".into());
                }
            }
            option if option == b'd' as i32 => daemon_address = Some(options.value().to_owned()),
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let Some(filename) = options.positionals().first() else {
        return Err(format!(
            "usage rrdtool {} [--rraindex number] [--daemon|-d <addr>] file.rrd",
            args[0]
        )
        .into());
    };
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        let escaped_filename = filename.replace('\\', "\\\\").replace(' ', "\\ ");
        let response = send_rrdcached_command(
            &address,
            &format!("FIRST {escaped_filename} {archive_index}"),
        )?;
        let timestamp = response
            .split_whitespace()
            .nth(1)
            .ok_or("invalid rrdcached FIRST response")?;
        println!("{timestamp}");
        return Ok(());
    }
    println!("{}", first_rrd_time(filename, archive_index as usize)?);
    Ok(())
}

/// rrd_tool.c:736-750 prints the time_t that rrd_first and rrd_last return,
/// which is -1 on every error, before the ERROR line.
fn print_minus_one_on_error(
    result: Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if result.is_err() {
        println!("-1");
    }
    result
}

fn rrdtool_info(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/info.txt"));
        return Ok(());
    }
    // rrd_info.c:72.
    const LONGOPTS: &[LongOpt] = &[
        opt("daemon", b'd' as i32, ArgType::Required),
        opt("noflush", b'F' as i32, ArgType::None),
    ];
    let mut daemon_address = None::<String>;
    let mut noflush = false;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon_address = Some(options.value().to_owned()),
            option if option == b'F' as i32 => noflush = true,
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let [filename] = options.positionals() else {
        return Err(format!(
            "Usage: rrdtool {} [--daemon |-d <addr> [--noflush|-F]] <file>",
            args[0]
        )
        .into());
    };
    let filename = filename.clone();
    if !noflush {
        if let Some(address) = daemon_address
            .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
            .filter(|address| !address.is_empty())
        {
            send_rrdcached_flush(&address, &filename)?;
        }
    }
    ensure_rrd_file_exists(std::path::Path::new(&filename))?;
    let info = inspect_rrd(&filename)?;
    if info.data_sources.iter().any(|ds| {
        !matches!(
            ds.kind.as_str(),
            "GAUGE" | "COUNTER" | "DERIVE" | "ABSOLUTE" | "DCOUNTER" | "DDERIVE"
        )
    }) || info.archives.iter().any(|archive| {
        !matches!(
            archive.consolidation.as_str(),
            "AVERAGE" | "MIN" | "MAX" | "LAST"
        )
    }) {
        return Err(
            "rrdtool info output for this data-source or consolidation type is not implemented"
                .into(),
        );
    }
    println!("filename = \"{}\"", filename);
    println!("rrd_version = \"{}\"", info.version);
    println!("step = {}", info.step);
    println!("last_update = {}", info.last_update);
    println!("header_size = {}", info.header_size);
    for (index, source) in info.data_sources.iter().enumerate() {
        println!("ds[{}].index = {index}", source.name);
        println!("ds[{}].type = \"{}\"", source.name, source.kind);
        println!(
            "ds[{}].minimal_heartbeat = {}",
            source.name, source.heartbeat
        );
        println!(
            "ds[{}].min = {}",
            source.name,
            format_info_value(source.minimum)
        );
        println!(
            "ds[{}].max = {}",
            source.name,
            format_info_value(source.maximum)
        );
        println!("ds[{}].last_ds = \"{}\"", source.name, source.last_value);
        println!(
            "ds[{}].value = {}",
            source.name,
            format_info_float(source.pdp_value)
        );
        println!(
            "ds[{}].unknown_sec = {}",
            source.name, source.unknown_seconds
        );
    }
    for (index, archive) in info.archives.iter().enumerate() {
        println!("rra[{index}].cf = \"{}\"", archive.consolidation);
        println!("rra[{index}].rows = {}", archive.rows);
        println!("rra[{index}].cur_row = {}", archive.current_row);
        println!("rra[{index}].pdp_per_row = {}", archive.pdp_per_row);
        println!("rra[{index}].xff = {}", format_rrd_float(archive.xff));
        for (ds_index, prep) in archive.cdp_prep.iter().enumerate() {
            println!(
                "rra[{index}].cdp_prep[{ds_index}].value = {}",
                format_info_float(prep.value)
            );
            println!(
                "rra[{index}].cdp_prep[{ds_index}].unknown_datapoints = {}",
                prep.unknown_datapoints
            );
        }
    }
    Ok(())
}

fn rrdtool_dump(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/dump.txt"));
        return Ok(());
    }
    // rrd_dump.c:556.
    const LONGOPTS: &[LongOpt] = &[
        opt("daemon", b'd' as i32, ArgType::Required),
        opt("header", b'h' as i32, ArgType::Required),
        opt("no-header", b'n' as i32, ArgType::None),
    ];
    let usage = format!(
        "usage rrdtool {} [--header|-h {{none,xsd,dtd}}]\n[--no-header|-n]\n[--daemon|-d address]\nfile.rrd [file.xml]",
        args[0]
    );
    let mut header = RrdDumpHeader::Dtd;
    let mut daemon_address = None::<String>;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon_address = Some(options.value().to_owned()),
            option if option == b'n' as i32 => header = RrdDumpHeader::None,
            // parse_opt_xmlheader returns -1 for other words, which
            // rrd_dump_opt_r writes like "none".
            option if option == b'h' as i32 => {
                header = match options.value() {
                    "dtd" => RrdDumpHeader::Dtd,
                    "xsd" => RrdDumpHeader::Xsd,
                    _ => RrdDumpHeader::None,
                };
            }
            _ => return Err(usage.into()),
        }
    }
    let positional = options.positionals().to_vec();
    if !(1..=2).contains(&positional.len()) {
        return Err(usage.into());
    }
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        send_rrdcached_flush(&address, &positional[0])?;
    }
    ensure_rrd_file_exists(std::path::Path::new(&positional[0]))?;
    let xml = dump_rrd_file_with_header(&positional[0], header)?;
    if let Some(output) = positional.get(1) {
        write_output_file(output, xml.as_bytes())?;
    } else {
        print!("{xml}");
    }
    Ok(())
}

fn rrdtool_restore(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/restore.txt"));
        return Ok(());
    }
    // rrd_restore.c:1383. Upstream keeps both flags in file statics that
    // nothing resets, so in pipe mode they stay set for later restores.
    static RANGE_CHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static FORCE_OVERWRITE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    const LONGOPTS: &[LongOpt] = &[
        opt("range-check", b'r' as i32, ArgType::None),
        opt("force-overwrite", b'f' as i32, ArgType::None),
    ];
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'r' as i32 => {
                RANGE_CHECK.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            option if option == b'f' as i32 => {
                FORCE_OVERWRITE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {}
        }
    }
    let positional = options.positionals();
    if positional.len() != 2 {
        return Err(format!(
            "usage rrdtool {} [--range-check|-r] [--force-overwrite|-f] file.xml file.rrd",
            args[0]
        )
        .into());
    }
    let range_check = RANGE_CHECK.load(std::sync::atomic::Ordering::Relaxed);
    let force_overwrite = FORCE_OVERWRITE.load(std::sync::atomic::Ordering::Relaxed);
    let xml = if positional[0] == "-" {
        let mut xml = String::new();
        std::io::stdin().read_to_string(&mut xml)?;
        xml
    } else {
        std::fs::read_to_string(&positional[0])?
    };
    restore_rrd_file(&xml, &positional[1], force_overwrite, range_check)?;
    Ok(())
}

fn rrdtool_tune(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/tune.txt"));
        return Ok(());
    }
    // rrd_tune.c:78. rrd_tune reads only --daemon in a first pass that
    // ignores errors; rrd_tune_r then applies every option in order.
    const LONGOPTS: &[LongOpt] = &[
        opt("heartbeat", b'h' as i32, ArgType::Required),
        opt("minimum", b'i' as i32, ArgType::Required),
        opt("maximum", b'a' as i32, ArgType::Required),
        opt("data-source-type", b'd' as i32, ArgType::Required),
        opt("data-source-rename", b'r' as i32, ArgType::Required),
        opt("deltapos", b'p' as i32, ArgType::Required),
        opt("deltaneg", b'n' as i32, ArgType::Required),
        opt("window-length", b'w' as i32, ArgType::Required),
        opt("failure-threshold", b'f' as i32, ArgType::Required),
        opt("alpha", b'x' as i32, ArgType::Required),
        opt("beta", b'y' as i32, ArgType::Required),
        opt("gamma", b'z' as i32, ArgType::Required),
        opt("gamma-deviation", b'v' as i32, ArgType::Required),
        opt("smoothing-window", b's' as i32, ArgType::Required),
        opt("smoothing-window-deviation", b'S' as i32, ArgType::Required),
        opt("aberrant-reset", b'b' as i32, ArgType::Required),
        opt("step", b't' as i32, ArgType::Required),
        opt("daemon", b'D' as i32, ArgType::Required),
    ];
    let mut daemon_address = None::<String>;
    // The settings in long form, for the daemon's TUNE command.
    let mut raw_settings = Vec::new();
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'D' as i32 => daemon_address = Some(options.value().to_owned()),
            option => {
                if let Some(longopt) = LONGOPTS.iter().find(|longopt| longopt.short == option) {
                    raw_settings.push(format!("--{}", longopt.name));
                    raw_settings.push(options.value().to_owned());
                }
            }
        }
    }
    let Some(filename) = options.positionals().first().cloned() else {
        return Err("missing file name".into());
    };
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        let escaped_filename = filename.replace('\\', "\\\\").replace(' ', "\\ ");
        send_rrdcached_command(&address, &format!("FLUSH {escaped_filename}"))?;
        let mut fields = vec![
            "TUNE".to_owned(),
            filename.clone(),
            (raw_settings.len() + 1).to_string(),
            "tune".to_owned(),
        ];
        fields.extend(raw_settings);
        let command = fields
            .iter()
            .map(|field| field.replace('\\', "\\\\").replace(' ', "\\ "))
            .collect::<Vec<_>>()
            .join(" ");
        send_rrdcached_command(&address, &command)?;
        let _ = send_rrdcached_command(&address, &format!("FORGET {escaped_filename}"));
        return Ok(());
    }
    ensure_rrd_file_exists(std::path::Path::new(&filename))?;
    let info = rondi::inspect_rrd_file(&filename)?;
    let mut changes = info
        .data_sources
        .iter()
        .map(|source| RrdDataSourceTune {
            name: source.name.clone(),
            kind: None,
            new_name: None,
            heartbeat: None,
            minimum: None,
            maximum: None,
        })
        .collect::<Vec<_>>();
    let mut current_names = info
        .data_sources
        .iter()
        .map(|source| source.name.clone())
        .collect::<Vec<_>>();
    let ds_match = |names: &[String], name: &str| {
        names
            .iter()
            .position(|source| source == name)
            .ok_or_else(|| format!("unknown data source name '{name}'"))
    };
    let mut options = OptParse::new(args.to_vec());
    // rrd_tune_r edits the header through the file mapping, so settings
    // applied before a failing option stay written.
    let applied = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let option = options.long(LONGOPTS);
            if option == optparse::DONE {
                break;
            }
            let value = options.value();
            match u8::try_from(option).map(char::from) {
                Ok('h') => {
                    let Some((name, heartbeat)) = tune_scan_heartbeat(value) else {
                        return Err("invalid arguments for heartbeat".into());
                    };
                    let ds = ds_match(&current_names, &name)?;
                    changes[ds].heartbeat = Some(heartbeat as u64);
                }
                Ok(kind @ ('i' | 'a')) => {
                    let which = if kind == 'i' { "minimum" } else { "maximum" };
                    let bound = tune_scan_pair(value, c"%19[a-zA-Z0-9_-]:%40[U0-9.e+-]").and_then(
                        |(name, text)| {
                            let bound = if text == "U" {
                                RrdTuneBound::Unbounded
                            } else {
                                let value = cparse::rrd_strtodbl(&text, None).ok()?;
                                if value.is_nan() {
                                    RrdTuneBound::Unbounded
                                } else {
                                    RrdTuneBound::Value(value)
                                }
                            };
                            Some((name, bound))
                        },
                    );
                    let Some((name, bound)) = bound else {
                        return Err(format!("invalid arguments for {which} ds value").into());
                    };
                    let ds = ds_match(&current_names, &name)?;
                    if kind == 'i' {
                        changes[ds].minimum = Some(bound);
                    } else {
                        changes[ds].maximum = Some(bound);
                    }
                }
                Ok('d') => {
                    let Some((name, kind)) = tune_scan_pair(value, c"%19[a-zA-Z0-9_-]:%19[A-Z]")
                    else {
                        return Err("invalid arguments for data source type".into());
                    };
                    let ds = ds_match(&current_names, &name)?;
                    if !matches!(
                        kind.as_str(),
                        "COUNTER"
                            | "ABSOLUTE"
                            | "GAUGE"
                            | "DERIVE"
                            | "COMPUTE"
                            | "DCOUNTER"
                            | "DDERIVE"
                    ) {
                        return Err(format!("unknown data acquisition function '{kind}'").into());
                    }
                    changes[ds].kind = Some(kind);
                }
                Ok('r') => {
                    let Some((name, new_name)) =
                        tune_scan_pair(value, c"%19[a-zA-Z0-9_-]:%19[a-zA-Z0-9_-]")
                    else {
                        return Err("invalid arguments for data source type".into());
                    };
                    let ds = ds_match(&current_names, &name)?;
                    changes[ds].new_name = Some(new_name.clone());
                    current_names[ds] = new_name;
                }
                // Rondi opens no Holt-Winters file, so after the value checks
                // these always find their RRA missing (rrd_tune.c:475-640).
                Ok('p' | 'n') => {
                    let (status, parsed) = cparse::rrd_strtodbl_status(value, None);
                    let param = parsed.unwrap_or_else(|_| cparse::rrd_strtod_prefix(value));
                    if matches!(status, 1 | 2) && param < 0.1 {
                        return Err("Parameter specified is too small".into());
                    }
                    if status == 1 {
                        return Err("Unable to parse parameter in set_deltaarg".into());
                    }
                    return Err("Failures RRA does not exist in this RRD".into());
                }
                Ok('f' | 'w') => {
                    let param = cparse::atoi(value) as libc::c_ulong;
                    if !(1..=28).contains(&param) {
                        return Err("Parameter must be between 1 and 28".into());
                    }
                    return Err("Failures RRA does not exist in this RRD".into());
                }
                Ok(kind @ ('x' | 'y' | 'z' | 'v' | 's' | 'S')) => {
                    let (status, parsed) = cparse::rrd_strtodbl_status(value, None);
                    let param = parsed.unwrap_or_else(|_| cparse::rrd_strtod_prefix(value));
                    let out_of_range = if matches!(kind, 's' | 'S') {
                        !(0.0..=1.0).contains(&param)
                    } else {
                        param <= 0.0 || param >= 1.0
                    };
                    if matches!(status, 1 | 2) && out_of_range {
                        return Err("Holt-Winters parameter must be between 0 and 1".into());
                    }
                    if status == 0 {
                        return Err("Unable to parse Holt-Winters parameter".into());
                    }
                    return Err("Holt-Winters RRA does not exist in this RRD".into());
                }
                Ok('b') => {
                    let Some(name) = tune_scan_name(value) else {
                        return Err("invalid argument for aberrant-reset".into());
                    };
                    // reset_aberrant_coefficients does nothing without
                    // Holt-Winters archives.
                    ds_match(&current_names, &name)?;
                }
                Ok('?') => return Err(options.errmsg.clone().into()),
                _ => {}
            }
        }
        Ok(())
    })();
    let changed = changes.iter().any(|change| {
        change.kind.is_some()
            || change.new_name.is_some()
            || change.heartbeat.is_some()
            || change.minimum.is_some()
            || change.maximum.is_some()
    });
    if changed {
        tune_rrd_data_sources(&filename, &changes)?;
    }
    applied?;

    // handle_modify (rrd_modify.c:1299) takes the words after the file name.
    for argument in &options.positionals()[1..] {
        let known = ["DEL:", "DS:", "RRA#", "RRA:"]
            .iter()
            .any(|prefix| argument.starts_with(prefix) && argument.len() > prefix.len())
            || (argument.starts_with("DELRRA:") && argument.len() > 7);
        if !known {
            return Err(format!("unparsable argument: {argument}").into());
        }
    }
    if options.positionals().len() > 1 {
        return Err("rrdtool tune DS/RRA modification (rrd_modify) is not implemented".into());
    }
    Ok(())
}

/// `sscanf(value, DS_NAM_FMT ":%ld", ...) == 2` (rrd_tune.c:258).
fn tune_scan_heartbeat(value: &str) -> Option<(String, libc::c_long)> {
    let input = cparse::c_string(value);
    let mut name = [0_u8; 20];
    let mut heartbeat: libc::c_long = 0;
    // SAFETY: %19[ writes at most 20 bytes and %ld a long.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%19[a-zA-Z0-9_-]:%ld".as_ptr(),
            name.as_mut_ptr(),
            &mut heartbeat as *mut libc::c_long,
        )
    };
    (matched == 2).then(|| (cparse::buffer_text(&name), heartbeat))
}

/// Two string conversions of at most 40 bytes each.
fn tune_scan_pair(value: &str, format: &std::ffi::CStr) -> Option<(String, String)> {
    let input = cparse::c_string(value);
    let mut first = [0_u8; 41];
    let mut second = [0_u8; 41];
    // SAFETY: every format passed has two %N[ conversions with N <= 40.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            format.as_ptr(),
            first.as_mut_ptr(),
            second.as_mut_ptr(),
        )
    };
    (matched == 2).then(|| (cparse::buffer_text(&first), cparse::buffer_text(&second)))
}

/// `sscanf(value, DS_NAM_FMT, ds_nam) == 1`.
fn tune_scan_name(value: &str) -> Option<String> {
    let input = cparse::c_string(value);
    let mut name = [0_u8; 20];
    // SAFETY: %19[ writes at most 20 bytes.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%19[a-zA-Z0-9_-]".as_ptr(),
            name.as_mut_ptr(),
        )
    };
    (matched == 1).then(|| cparse::buffer_text(&name))
}

fn rrdtool_resize(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/resize.txt"));
        return Ok(());
    }
    // rrd_resize.c:27-64, in order.
    if args[1] == "resize.rrd" {
        return Err("resize.rrd is a reserved name".into());
    }
    if args.len() != 5 {
        return Err("wrong number of parameters".into());
    }
    let target_rra = rondi::c_strtol(&args[2], 0).0 as u64;
    let action = match args[3].as_str() {
        "GROW" => RrdResizeAction::Grow,
        "SHRINK" => RrdResizeAction::Shrink,
        _ => return Err("I can only GROW or SHRINK".into()),
    };
    let modify = rondi::c_strtol(&args[4], 0).0;
    if modify < 1 {
        return Err("Please grow or shrink with at least 1 row".into());
    }
    resize_rrd_file(
        &args[1],
        std::env::current_dir()?.join("resize.rrd"),
        usize::try_from(target_rra).unwrap_or(usize::MAX),
        action,
        modify as u64,
    )?;
    Ok(())
}

fn rrdtool_list(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/list.txt"));
        return Ok(());
    }
    // rrd_list.c:256.
    const LONGOPTS: &[LongOpt] = &[
        opt("daemon", b'd' as i32, ArgType::Required),
        opt("noflush", b'F' as i32, ArgType::None),
        opt("recursive", b'r' as i32, ArgType::None),
    ];
    let mut recursive = false;
    let mut daemon = None;
    let mut noflush = false;
    let mut options = OptParse::new(args.to_vec());
    loop {
        match options.long(LONGOPTS) {
            optparse::DONE => break,
            option if option == b'd' as i32 => daemon = Some(options.value().to_owned()),
            option if option == b'F' as i32 => noflush = true,
            option if option == b'r' as i32 => recursive = true,
            optparse::ERROR => return Err(options.errmsg.into()),
            _ => {
                return Err(format!(
                    "Usage: rrdtool {} [--daemon <addr> [--noflush]] <file>",
                    args[0]
                )
                .into());
            }
        }
    }
    let [directory] = options.positionals() else {
        return Err(format!(
            "Usage: rrdtool {} [--daemon <addr> [--noflush]] [--recursive] <directory>",
            args[0]
        )
        .into());
    };
    let directory = PathBuf::from(directory);
    if let Some(address) = daemon.or_else(|| std::env::var("RRDCACHED_ADDRESS").ok()) {
        let path = directory.to_string_lossy();
        let protocol_path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        let command = if recursive {
            format!("LIST RECURSIVE {}", escape_rrdcached_field(&protocol_path))
        } else {
            format!("LIST {}", escape_rrdcached_field(&protocol_path))
        };
        if !noflush {
            send_rrdcached_command(&address, "FLUSHALL")?;
        }
        let response = send_rrdcached_multiline_command(&address, &command)?;
        print!("{response}");
        return Ok(());
    }
    let text = list_rrd_entries(&directory, recursive)?;
    print!("{text}");
    Ok(())
}

fn escape_rrdcached_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\ ")
}

#[cfg(unix)]
fn send_rrdcached_multiline_command(
    address: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = connect_rrdcached(address)?;
    writeln!(stream, "{command}")?;
    let mut reader = std::io::BufReader::new(stream);
    let mut header = String::new();
    read_bounded_line(&mut reader, &mut header, 1024 * 1024)?;
    if header.starts_with('-') {
        return Err(header
            .split_once(' ')
            .map(|(_, message)| message.trim_end())
            .unwrap_or(header.trim_end())
            .to_owned()
            .into());
    }
    let count: usize = header
        .split_whitespace()
        .next()
        .ok_or("empty rrdcached LIST response")?
        .parse()?;
    const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
    if count > MAX_RESPONSE_BYTES {
        return Err("rrdcached response exceeds 64 MiB limit".into());
    }
    let mut body = String::new();
    for _ in 0..count {
        let mut line = String::new();
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(body.len());
        if remaining == 0 {
            return Err("rrdcached response exceeds 64 MiB limit".into());
        }
        if read_bounded_line(&mut reader, &mut line, remaining.min(1024 * 1024))? == 0 {
            return Err("incomplete rrdcached LIST response".into());
        }
        body.push_str(&line);
    }
    Ok(body)
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize> {
    line.clear();
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rrdcached response line exceeds 1 MiB",
            ));
        }
        let complete = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            break;
        }
    }
    let length = bytes.len();
    *line = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(length)
}

const MAX_RRDCACHED_LINE_BYTES: usize = 1024 * 1024;

#[cfg(unix)]
trait RrdcachedStream: Read + Write {}

#[cfg(unix)]
impl<T: Read + Write> RrdcachedStream for T {}

#[cfg(unix)]
fn connect_rrdcached(
    address: &str,
) -> Result<Box<dyn RrdcachedStream>, Box<dyn std::error::Error>> {
    open_rrdcached_stream(address).map_err(|error| {
        let error_text = error.to_string();
        let detail = error_text
            .split(" (os error ")
            .next()
            .unwrap_or("Internal error");
        RrdcachedConnectError(format!("Unable to connect to rrdcached: {detail}")).into()
    })
}

/// A failed connection, kept distinct so `flushcached` can report it without
/// the per-file wrapper, as RRDtool connects once before flushing any file.
#[derive(Debug)]
struct RrdcachedConnectError(String);

impl std::fmt::Display for RrdcachedConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RrdcachedConnectError {}

#[cfg(unix)]
fn open_rrdcached_stream(
    address: &str,
) -> Result<Box<dyn RrdcachedStream>, Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixStream;

    if let Some(path) = address.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("empty rrdcached Unix socket path".into());
        }
        return Ok(Box::new(UnixStream::connect(path)?));
    }
    if address.starts_with('/') {
        return Ok(Box::new(UnixStream::connect(address)?));
    }
    if let Some(endpoint) = address
        .strip_prefix("tcp:")
        .or_else(|| address.contains(':').then_some(address))
    {
        if endpoint.is_empty() {
            return Err("empty rrdcached TCP endpoint".into());
        }
        return Ok(Box::new(std::net::TcpStream::connect(endpoint)?));
    }
    Err(format!("unsupported rrdcached address: {address}").into())
}

#[cfg(not(unix))]
fn send_rrdcached_multiline_command(
    _address: &str,
    _command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    Err("rrdcached Unix socket commands are unavailable on this platform".into())
}

fn list_rrd_entries(
    directory: &std::path::Path,
    recursive: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::fmt::Write as _;
    let input = directory.to_string_lossy();
    if input.contains("..") {
        return Err("path traversal is not allowed".into());
    }
    if input.contains('*') || input.contains('?') {
        if recursive {
            print_list_error(libc::EINVAL);
            return Ok(String::new());
        }
        let matches = match glob::glob(input.as_ref()) {
            Ok(paths) => match paths.collect::<Result<Vec<_>, _>>() {
                Ok(matches) => matches,
                Err(_) => {
                    print_list_error(libc::ENOENT);
                    return Ok(String::new());
                }
            },
            Err(_) => {
                print_list_error(libc::ENOENT);
                return Ok(String::new());
            }
        };
        if matches.is_empty() {
            print_list_error(libc::ENOENT);
            return Ok(String::new());
        }
        let mut output = String::new();
        for matched in matches {
            if let Some(name) = matched.file_name() {
                writeln!(output, "{}", name.to_string_lossy()).unwrap();
            }
        }
        return Ok(output);
    }
    if input.ends_with(".rrd") {
        let metadata = std::fs::metadata(directory)?;
        if !metadata.is_file() {
            return Err("RRD path is not a regular file".into());
        }
        let name = directory.file_name().ok_or("RRD path has no filename")?;
        return Ok(format!("{}\n", name.to_string_lossy()));
    }
    if !std::fs::metadata(directory)?.is_dir() {
        return Err("list path is not a directory".into());
    }
    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        recursive: bool,
        out: &mut String,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let current = entry.path();
            let metadata = std::fs::metadata(&current)?;
            if metadata.is_dir() && recursive {
                walk(root, &current, recursive, out)?;
                continue;
            }
            if metadata.is_file() && !name.to_string_lossy().ends_with(".rrd") {
                continue;
            }
            let relative = current.strip_prefix(root).unwrap_or(&current);
            writeln!(out, "{}", relative.to_string_lossy().replace('\\', "/")).unwrap();
        }
        Ok(())
    }
    let mut out = String::new();
    walk(directory, directory, recursive, &mut out)?;
    Ok(out)
}

fn print_list_error(errno: libc::c_int) {
    // RRDtool writes strerror(errno) without a trailing newline for list errors.
    let message = unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)).to_string_lossy() };
    eprint!("{message}");
}

fn format_info_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else {
        format_rrd_float(value)
    }
}

fn format_info_value(value: Option<f64>) -> String {
    value.map_or_else(|| "NaN".to_owned(), format_rrd_float)
}

fn inspect_rrd(path: &str) -> Result<rondi::RrdInfo, Box<dyn std::error::Error>> {
    // Metadata inspection takes RRDtool's shared file lock and bounds reads to its header.
    Ok(rondi::inspect_rrd_file(path)?)
}

// rrd_tool.c prints fetch rows with printf, so LC_NUMERIC picks the decimal
// separator.
fn format_fetch_value(value: f64) -> String {
    let mut buffer = [0 as libc::c_char; 64];
    let length =
        unsafe { libc::snprintf(buffer.as_mut_ptr(), buffer.len(), c"%0.10e".as_ptr(), value) };
    match usize::try_from(length) {
        Ok(length) if length < buffer.len() => {
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
            String::from_utf8_lossy(bytes).into_owned()
        }
        _ => format_rrd_float(value),
    }
}

fn format_rrd_float(value: f64) -> String {
    let formatted = format!("{value:0.10e}");
    let Some((mantissa, exponent)) = formatted.split_once('e') else {
        return formatted;
    };
    let exponent = exponent.parse::<i32>().unwrap_or_default();
    format!("{mantissa}e{exponent:+03}")
}

fn parse_rrd_update_value(value: &str) -> Option<Option<f64>> {
    if value.eq_ignore_ascii_case("U") || value.eq_ignore_ascii_case("UNKNOWN") {
        return Some(None);
    }
    rondi::parse_rrd_number(value).map(Some)
}

fn parse_value(value: &str) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    if value.eq_ignore_ascii_case("U") || value.eq_ignore_ascii_case("UNKNOWN") {
        return Ok(None);
    }
    Ok(Some(value.parse()?))
}

fn parse_rrd_resolution(value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    parse_scaled_duration_option(value, "resolution")
}

/// `rrd_scaled_duration(value, 1, ...)` with its message after `label: `.
fn parse_scaled_duration_option(
    value: &str,
    label: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    Ok(parse_rrd_scaled_duration(value, 1).map_err(|error| format!("{label}: {error}"))?)
}

fn is_rrd_zero_duration(value: &str) -> bool {
    let digits = value
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 || value[..digits].parse::<u64>().ok() != Some(0) {
        return false;
    }
    match &value[digits..] {
        "" => true,
        suffix if suffix.len() == 1 => matches!(
            suffix.as_bytes()[0],
            b's' | b'm' | b'h' | b'd' | b'w' | b'M' | b'y'
        ),
        _ => false,
    }
}

fn rrd_dump_archive_values_are_unknown(xml: &str) -> bool {
    let mut values = xml.split("<v>").skip(1);
    let mut count = 0_usize;
    for value in &mut values {
        let Some((value, _)) = value.split_once("</v>") else {
            return false;
        };
        if !matches!(value.trim(), "NaN" | "nan" | "-nan") {
            return false;
        }
        count += 1;
    }
    count > 0
}

fn rrd_ds_definition_matches(definition: &str, source: &rondi::RrdDataSourceInfo) -> bool {
    let fields = definition.split(':').collect::<Vec<_>>();
    if fields.len() != 6 || fields[0] != "DS" {
        return false;
    }
    let bound_matches = |text: &str, expected: Option<f64>| {
        if text.eq_ignore_ascii_case("U") {
            expected.is_none()
        } else {
            text.parse::<f64>()
                .ok()
                .zip(expected)
                .is_some_and(|(actual, expected)| actual == expected)
        }
    };
    fields[1] == source.name
        && fields[2] == source.kind
        && parse_rrd_scaled_duration(fields[3], 1).is_ok_and(|value| value == source.heartbeat)
        && bound_matches(fields[4], source.minimum)
        && bound_matches(fields[5], source.maximum)
}

fn rrd_archive_definition_matches(
    definition: &str,
    archive: &rondi::RrdArchiveInfo,
    base_step: u64,
) -> bool {
    let fields = definition.split(':').collect::<Vec<_>>();
    if fields.len() != 5 || fields[0] != "RRA" || fields[1] != archive.consolidation {
        return false;
    }
    let Some(xff) = fields[2].parse::<f64>().ok() else {
        return false;
    };
    let Some(pdp_per_row) = parse_rrd_scaled_duration(fields[3], base_step).ok() else {
        return false;
    };
    let Some(rows) =
        parse_rrd_scaled_duration(fields[4], base_step.saturating_mul(pdp_per_row)).ok()
    else {
        return false;
    };
    xff == archive.xff && pdp_per_row == archive.pdp_per_row && rows == archive.rows
}

async fn server_mode(socket: PathBuf, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use tokio::net::UnixStream;
    let (method, path, body) = match command {
        Command::Create {
            name,
            step,
            heartbeat,
            rows,
            start,
        } => (
            "POST",
            "/v1/databases".to_string(),
            serde_json::json!({"name":name,"config":{"step":step,"heartbeat":heartbeat,"rows":rows,"start":start}}),
        ),
        Command::Update {
            name,
            timestamp,
            value,
        } => (
            "POST",
            format!("/v1/databases/{name}/updates"),
            serde_json::json!({"id":format!("cli-{timestamp}-{name}"),"timestamp":timestamp,"value":parse_value(&value)?}),
        ),
        Command::Fetch { name } => (
            "GET",
            format!("/v1/databases/{name}/points"),
            serde_json::Value::Null,
        ),
        Command::Health => ("GET", "/v1/health".to_string(), serde_json::Value::Null),
        Command::Server { .. } => return Err("server cannot be invoked as a client request".into()),
    };
    let stream = UnixStream::connect(socket).await?;
    let encoded = if body.is_null() {
        Vec::new()
    } else {
        serde_json::to_vec(&body)?
    };
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, "localhost")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(encoded)))?;
    let response = sender.send_request(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    let body = std::str::from_utf8(&body)?;
    if !status.is_success() {
        return Err(format!("server error (HTTP {status}): {}", body.trim()).into());
    }
    if !body.trim().is_empty() {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(body)?)?
        );
    }
    Ok(())
}

#[cfg(test)]
mod xml_output_tests {
    use super::format_xport_xmljson;

    // rrd_xport.c writes these text nodes unescaped, so the document is not
    // well-formed XML when they contain markup characters.
    #[test]
    fn writes_graph_xport_text_nodes_verbatim() {
        let mut im = rondi::graph::GraphImage::new(1_000_000_000, 1_000_000_010, 10);
        let script = ["DEF:rate=unused.rrd:x:AVERAGE", "COMMENT:<ok & done>"].map(String::from);
        im.graph_script(&script, &|_, _, start, end| Ok((start, end)))
            .unwrap();
        let data = rondi::graph::XportData {
            start: 100,
            end: 110,
            step: 10,
            columns: vec![0],
            legends: vec!["load & <peak>".to_owned()],
            rows: vec![vec![2.0]],
        };
        let (xml, error) = format_xport_xmljson(0, &mut im, &data, 1000, false);

        assert_eq!(error, None);
        assert!(xml.contains("<entry>load & <peak></entry>"));
        assert!(xml.contains("<comment><ok & done></comment>"));
    }
}

#[cfg(test)]
mod graph_stroke_tests {
    use super::{
        GraphCanvas, GraphPngOptions, GraphSeries, RenderedGraphXport, draw_line_with_width,
        draw_styled_line, draw_vertical_text, graph_render_input, interpolate_graph_color,
        prepare_graph_series, render_graph_png, set_pixel, tick_mark_range,
    };

    /// Parses one drawing element through rrd_graph_script after DEFs for
    /// every source name used here, and returns its renderer series with
    /// the source name as its column.
    /// A renderer series plus the legend and scale exclusion of its element.
    #[derive(Clone)]
    struct TestElement {
        series: GraphSeries,
        legend: String,
        skip_scale: bool,
    }

    impl std::ops::Deref for TestElement {
        type Target = GraphSeries;

        fn deref(&self) -> &GraphSeries {
            &self.series
        }
    }

    fn element(definition: &str) -> Result<TestElement, rondi::graph::ScriptError> {
        let names = [
            "rate", "base", "x", "upper", "normal", "spike", "load", "events", "extra",
        ];
        let mut script: Vec<String> = names
            .iter()
            .map(|name| format!("DEF:{name}=unused.rrd:x:AVERAGE"))
            .collect();
        script.push(definition.to_owned());
        let mut im = rondi::graph::GraphImage::new(1_000_000_000, 1_000_000_100, 10);
        im.graph_script(&script, &|_, _, start, end| Ok((start, end)))?;
        let last = im.gdes.last().expect("a drawing element");
        let source = last.vname.clone();
        // data_proc leaves TICKs and skipscale elements out of the range.
        let skip_scale = last.skipscale || last.gf == rondi::graph::Gf::Tick;
        let legend = last
            .legend
            .strip_prefix("  ")
            .unwrap_or(&last.legend)
            .to_owned();
        let (_, series) = graph_render_input(&im);
        let mut series = series.into_iter().last().expect("a drawing element");
        series.variable = source;
        Ok(TestElement {
            series,
            legend,
            skip_scale,
        })
    }

    fn parse_graph_series(
        definition: &str,
    ) -> Result<(TestElement, ()), rondi::graph::ScriptError> {
        element(definition).map(|series| (series, ()))
    }

    #[test]
    fn line_directive_width_is_retained_and_defaults_to_one() {
        assert_eq!(
            parse_graph_series("LINE:rate#ffffff:rate")
                .unwrap()
                .0
                .line_width,
            1.0
        );
        assert_eq!(
            parse_graph_series("LINE2.5:rate#ffffff:rate")
                .unwrap()
                .0
                .line_width,
            2.5
        );
        assert_eq!(
            parse_graph_series("LINE0:rate#ffffff:rate")
                .unwrap()
                .0
                .line_width,
            0.0
        );
        assert_eq!(
            parse_graph_series("AREA:rate#ffffff:rate")
                .unwrap()
                .0
                .line_width,
            0.0
        );
    }

    #[test]
    fn wider_line_directive_paints_a_wider_stroke() {
        let mut thin = vec![0; 20 * 20 * 3];
        let mut thick = vec![0; 20 * 20 * 3];
        draw_line_with_width(&mut thin, 20, 20, (3, 10), (16, 10), [255, 0, 0, 255], 1.0);
        draw_line_with_width(&mut thick, 20, 20, (3, 10), (16, 10), [255, 0, 0, 255], 3.0);
        let colored_pixels = |pixels: &[u8]| {
            pixels
                .chunks_exact(3)
                .filter(|pixel| *pixel == [255, 0, 0])
                .count()
        };
        assert!(colored_pixels(&thick) > colored_pixels(&thin));
    }

    #[test]
    fn absent_line_color_is_invisible_and_cannot_have_a_legend() {
        let series = parse_graph_series("LINE:base::STACK").unwrap().0;
        assert_eq!(series.color, None);
        assert!(series.stack);
        // The empty field is the legend; the next one is left unused.
        assert!(parse_graph_series("LINE:base::Hidden base").is_err());
    }

    #[test]
    fn absent_area_color_is_invisible_but_remains_a_valid_series() {
        let series = parse_graph_series("AREA:base").unwrap().0;
        assert_eq!(series.color, None);
        assert_eq!(series.style, "area");
        assert!(series.legend.is_empty());
    }

    #[test]
    fn dashes_default_to_five_pixel_on_and_off_segments() {
        let series = parse_graph_series("LINE:rate#ffffff:rate:dashes")
            .unwrap()
            .0;
        assert_eq!(series.dash_pattern, [5.0, 5.0]);
        assert_eq!(series.dash_offset, 0.0);
    }

    #[test]
    fn custom_dash_patterns_accept_one_or_even_positive_lengths() {
        assert_eq!(
            parse_graph_series("LINE:x#ffffff:x:dashes=3")
                .unwrap()
                .0
                .dash_pattern,
            [3.0]
        );
        assert_eq!(
            parse_graph_series("LINE:x#ffffff:x:dashes=2,4,6,8")
                .unwrap()
                .0
                .dash_pattern,
            [2.0, 4.0, 6.0, 8.0]
        );
        // getDouble accepts any rrd_strtodbl number, so these parse.
        for accepted in ["0", "-1,2", "1,2,3", "NaN", "inf"] {
            assert!(
                parse_graph_series(&format!("LINE:x#ffffff:x:dashes={accepted}")).is_ok(),
                "{accepted}"
            );
        }
        assert!(parse_graph_series("LINE:x#ffffff:x:dashes=a").is_err());
    }

    #[test]
    fn dash_offset_is_parsed() {
        let parsed = parse_graph_series("LINE:x#ffffff:x:dashes=2,3:dash-offset=-1.5")
            .unwrap()
            .0;
        assert_eq!(parsed.dash_pattern, [2.0, 3.0]);
        assert_eq!(parsed.dash_offset, -1.5);
        assert!(parse_graph_series("LINE:x#ffffff:x:dash-offset=abc").is_err());
    }

    #[test]
    fn line_dash_renderer_draws_gaps_and_honors_offset() {
        let mut pixels = vec![255; 30 * 3];
        draw_styled_line(
            &mut pixels,
            30,
            1,
            (0, 0),
            (29, 0),
            super::DashStroke {
                color: [0, 0, 0, 255],
                pattern: &[3.0, 3.0],
                offset: 0.0,
                width: 1.0,
            },
        );
        let black: Vec<_> = pixels
            .chunks_exact(3)
            .enumerate()
            .filter_map(|(x, p)| (p == [0, 0, 0]).then_some(x))
            .collect();
        assert_eq!(black, (0..30).filter(|x| x % 6 < 3).collect::<Vec<_>>());
        let mut offset = vec![255; 30 * 3];
        draw_styled_line(
            &mut offset,
            30,
            1,
            (0, 0),
            (29, 0),
            super::DashStroke {
                color: [0, 0, 0, 255],
                pattern: &[3.0, 3.0],
                offset: 3.0,
                width: 1.0,
            },
        );
        assert_ne!(offset, pixels);
    }

    #[test]
    fn horizontal_rule_accepts_dash_options() {
        let rule = element("HRULE:4#ff0000:limit:dashes=2,4:dash-offset=1").unwrap();
        assert_eq!(rule.legend, "limit");
        assert_eq!(rule.dash_pattern, [2.0, 4.0]);
        assert_eq!(rule.dash_offset, 1.0);
    }

    #[test]
    fn vertical_rule_accepts_default_dash_options() {
        let rule = element("VRULE:1000000030#00ff00:deploy:dashes").unwrap();
        assert_eq!(rule.dash_pattern, [5.0, 5.0]);
        assert_eq!(rule.legend, "deploy");
    }

    #[test]
    fn legend_samples_use_dash_style() {
        let series = parse_graph_series("LINE:x#ffffff:label:dashes=2,2")
            .unwrap()
            .0;
        assert_eq!(series.dash_pattern, [2.0, 2.0]);
        assert!(!series.legend.is_empty());
    }

    #[test]
    fn invisible_stack_series_keeps_values_for_following_stack_and_scale() {
        let hidden = parse_graph_series("AREA:base::STACK").unwrap().0;
        let visible = parse_graph_series("AREA:upper#ff0000:Upper:STACK")
            .unwrap()
            .0;
        let graph = RenderedGraphXport {
            start: 0,
            end: 1,
            variables: vec!["base".into(), "upper".into()],
            rows: vec![vec![Some(3.0), Some(2.0)]],
        };
        let prepared = prepare_graph_series(&graph, &[hidden.series, visible.series]);
        assert_eq!(prepared[1].values, [Some(5.0)]);
    }

    #[test]
    fn graph_color_alpha_is_parsed_and_blended_over_the_existing_pixel() {
        let series = parse_graph_series("LINE:rate#ff000080:rate").unwrap().0;
        assert_eq!(series.color, Some([255, 0, 0, 128]));

        let mut pixel = [255, 255, 255];
        set_pixel(&mut pixel, 1, 1, 0, 0, [255, 0, 0, 128]);
        assert_eq!(pixel, [255, 127, 127]);
        set_pixel(&mut pixel, 1, 1, 0, 0, [0, 0, 255, 0]);
        assert_eq!(pixel, [255, 127, 127]);
    }

    #[test]
    fn skipscale_series_is_parsed_as_excluded_from_automatic_bounds() {
        let excluded = parse_graph_series("LINE:spike#ffffff:spike:skipscale")
            .unwrap()
            .0;
        assert_eq!(excluded.legend, "spike");
        assert!(excluded.skip_scale);
    }

    #[test]
    fn area_gradient_colors_and_height_are_parsed_and_interpolated() {
        let area = parse_graph_series("AREA:load#ff0000ff#0000ff80:Load:gradheight=24:skipscale")
            .unwrap()
            .0;
        assert_eq!(area.legend, "Load");
        assert_eq!(area.color, Some([255, 0, 0, 255]));
        assert_eq!(area.color2, Some([0, 0, 255, 128]));
        assert_eq!(area.grad_height, 24.0);
        assert_eq!(
            interpolate_graph_color([255, 0, 0, 255], [0, 0, 255, 127], 0.5),
            [128, 0, 128, 191]
        );
        assert!(parse_graph_series("AREA:load#ff0000#00ff00:Load:gradheight=abc").is_err());
        // RRDtool's AREA parser does not enable PARSE_DASHES, so the keyword
        // is an unused argument.
        assert!(parse_graph_series("AREA:load#ff0000:Load:dashes").is_err());
    }

    #[test]
    fn tick_directive_uses_source_default_fraction_and_vertical_direction() {
        // rrd_graph_helper.c requires the positional fraction.
        assert!(element("TICK:events#ff000080").is_err());

        let labeled_tick = element("TICK:events#ff0000:-0.25:Events").unwrap();
        assert!(labeled_tick.skip_scale);
        assert_eq!(labeled_tick.tick_fraction, -0.25);
        assert_eq!(labeled_tick.legend, "Events");
        assert_eq!(tick_mark_range(10, 110, 25, -0.25), (10, 35));
        assert_eq!(tick_mark_range(10, 110, 25, 0.25), (85, 110));
    }

    #[test]
    fn zero_fraction_tick_is_invisible_like_rrdtool() {
        let graph = RenderedGraphXport {
            start: 0,
            end: 10,
            variables: vec!["events".to_owned()],
            rows: vec![vec![Some(1.0)]; 11],
        };
        let zero = element("TICK:events#ff0000:0").unwrap();
        let visible = element("TICK:events#ff0000:0.5").unwrap();
        let render = |tick: TestElement| {
            let png = render_graph_png(
                &graph,
                &[tick.series],
                GraphPngOptions {
                    canvas: GraphCanvas {
                        width: 121,
                        height: 71,
                        left: 51,
                        top: 15,
                        plot_width: 40,
                        plot_height: 30,
                        minimum: 0.0,
                        maximum: 1.0,
                    },
                    legends: &[],
                    title: None,
                    vertical_label: None,
                    vertical_label_angle: 90.0,
                    only_graph: false,
                    colors: super::GraphColors::default(),
                    grid_dash: Vec::new(),
                    border_width: 0,
                },
            )
            .unwrap();
            let mut reader = png::Decoder::new(std::io::Cursor::new(png))
                .read_info()
                .unwrap();
            let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut bytes).unwrap();
            bytes.truncate(info.buffer_size());
            bytes
        };
        let contains_red = |pixels: &[u8]| pixels.chunks_exact(3).any(|pixel| pixel == [255, 0, 0]);
        assert!(!contains_red(&render(zero)));
        assert!(contains_red(&render(visible)));
    }

    #[test]
    fn vertical_label_rotation_handles_both_directions_and_arbitrary_angles() {
        let mut normal = vec![255; 48 * 40 * 3];
        let mut reverse = normal.clone();
        let mut diagonal = normal.clone();
        draw_vertical_text(&mut normal, 48, 40, 2, 2, "RRD", [0, 0, 0, 255], 90.0);
        draw_vertical_text(&mut reverse, 48, 40, 2, 2, "RRD", [0, 0, 0, 255], 270.0);
        draw_vertical_text(&mut diagonal, 48, 40, 2, 2, "RRD", [0, 0, 0, 255], 45.0);
        assert_ne!(normal, reverse);
        assert_ne!(normal, diagonal);
        assert!(diagonal.chunks_exact(3).any(|pixel| pixel == [0, 0, 0]));
    }

    #[test]
    fn numeric_hrule_is_parsed_without_expanding_data_scale_bounds() {
        let rule = element("HRULE:20#ff000080:Threshold").unwrap();
        assert_eq!(rule.rule_value, Some(20.0));
        assert_eq!(rule.legend, "Threshold");
        assert_eq!(rule.color, Some([255, 0, 0, 128]));
        assert!(element("HRULE:abc#ff0000").is_err());
    }

    #[test]
    fn vrule_accepts_numeric_timestamps_only() {
        let rule = element("VRULE:1000000030#00ff0080:Maintenance").unwrap();
        assert_eq!(rule.rule_time, Some(1_000_000_030));
        assert_eq!(rule.legend, "Maintenance");
        assert_eq!(rule.color, Some([0, 255, 0, 128]));
        // A VRULE value is a VDEF or a number, not an at-style time.
        assert!(element("VRULE:now#00ff00").is_err());
    }

    #[test]
    fn stacked_series_adds_previous_values_and_carries_unknown_as_baseline() {
        let first = parse_graph_series("AREA:base#ff0000:Base").unwrap().0;
        let second = parse_graph_series("AREA:extra#0000ff:Extra:STACK")
            .unwrap()
            .0;
        assert!(second.stack);
        let graph = RenderedGraphXport {
            start: 0,
            end: 30,
            variables: vec![String::from("base"), String::from("extra")],
            rows: vec![
                vec![Some(2.0), Some(3.0)],
                vec![Some(4.0), None],
                vec![None, Some(1.0)],
            ],
        };
        let prepared = prepare_graph_series(&graph, &[first.series, second.series]);
        assert_eq!(prepared[0].values, vec![Some(2.0), Some(4.0), None]);
        assert_eq!(prepared[1].baseline, vec![Some(2.0), Some(4.0), Some(0.0)]);
        assert_eq!(prepared[1].values, vec![Some(5.0), Some(4.0), Some(1.0)]);
    }
}

#[cfg(test)]
mod rrdcached_response_tests {
    use super::{MAX_RRDCACHED_LINE_BYTES, read_bounded_line};
    use std::io::Cursor;

    #[test]
    fn bounded_rrdcached_line_accepts_the_configured_limit() {
        let response = format!("{}\n", "x".repeat(MAX_RRDCACHED_LINE_BYTES - 1));
        let mut reader = Cursor::new(response.as_bytes());
        let mut line = String::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, MAX_RRDCACHED_LINE_BYTES).unwrap(),
            MAX_RRDCACHED_LINE_BYTES
        );
        assert_eq!(line.as_bytes().last(), Some(&b'\n'));
    }

    #[test]
    fn bounded_rrdcached_line_rejects_an_oversized_unterminated_response() {
        let response = "x".repeat(MAX_RRDCACHED_LINE_BYTES + 1);
        let mut reader = Cursor::new(response.as_bytes());
        let mut line = String::new();
        let error =
            read_bounded_line(&mut reader, &mut line, MAX_RRDCACHED_LINE_BYTES).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    // The helper tests above pass even if a call site reads with an unbounded
    // read_line, so drive each client path against a daemon that never sends a
    // newline. The daemon stops writing once the client hangs up.
    #[cfg(unix)]
    fn serve_oversized_response(prefix: &'static str) -> (tempfile::TempDir, String) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rrdcached.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut command = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut command)
                .unwrap();
            let _ = stream.write_all(prefix.as_bytes());
            let chunk = [b'x'; 64 * 1024];
            for _ in 0..(2 * MAX_RRDCACHED_LINE_BYTES / chunk.len()) {
                if stream.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        (dir, format!("unix:{}", socket.display()))
    }

    #[cfg(unix)]
    fn assert_oversized_line_error(error: &dyn std::error::Error) {
        let message = error.to_string();
        // An unbounded read surfaces the payload itself as the error text.
        assert!(
            message == "rrdcached response line exceeds 1 MiB",
            "{message:.80}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn single_line_commands_reject_an_oversized_daemon_response() {
        let (_dir, address) = serve_oversized_response("");
        let error = super::send_rrdcached_command(&address, "FLUSH x.rrd").unwrap_err();
        assert_oversized_line_error(&*error);
    }

    #[cfg(unix)]
    #[test]
    fn updates_reject_an_oversized_daemon_response() {
        let (_dir, address) = serve_oversized_response("");
        let stream = super::connect_rrdcached(&address).unwrap();
        let error = super::send_rrdcached_update_on_stream(
            stream,
            std::path::Path::new("x.rrd"),
            &["1000000010:1".to_owned()],
        )
        .unwrap_err();
        assert_oversized_line_error(&*error);
    }

    #[cfg(unix)]
    #[test]
    fn multiline_commands_reject_an_oversized_header_or_body_line() {
        for prefix in ["", "1 entries follow\n"] {
            let (_dir, address) = serve_oversized_response(prefix);
            let error = super::send_rrdcached_multiline_command(&address, "LIST /").unwrap_err();
            assert_oversized_line_error(&*error);
        }
    }
}
