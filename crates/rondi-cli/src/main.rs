use clap::{Parser, Subcommand};
use rondi::{
    DatabaseConfig, RrdDataSourceTune, RrdDumpHeader, RrdResizeAction, RrdTuneBound, Store, Update,
    create_rrd_file, dump_rrd_file_with_header, fetch_rrd_file, first_rrd_time,
    parse_rrd_scaled_duration, resize_rrd_file, restore_rrd_file, tune_rrd_data_sources,
    update_rrd_raw_values_precise, update_rrd_raw_values_precise_verbose,
};
use std::fmt::Write as FmtWrite;
use std::io::{BufRead, Read, Seek, Write};
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
    if std::env::args().nth(1).as_deref() == Some("--rrdproxy-launcher") {
        return rrdproxy_mode(&std::env::args().skip(2).collect::<Vec<_>>());
    }
    if invoked_as == "rrdcached" {
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        return rrdcached_mode(&args).await;
    }
    if invoked_as == "rrdtool" {
        initialize_rrdtool_locale();
        let args = std::env::args().collect::<Vec<_>>();
        if args.get(1).is_some_and(|arg| arg == "-") {
            return rrdtool_batch();
        }
        if args.len() == 1 {
            print!("{}", rrdtool_usage(false));
            return Ok(());
        }
        // `rrdtool help <command>` is the three-argument spelling of
        // `rrdtool <command>`; both print that command's usage.
        let command_args = if args.len() == 3 && args[1] == "help" {
            &args[2..]
        } else {
            &args[1..]
        };
        if command_args.len() == 1 && !RRDTOOL_COMMANDS.contains(&command_args[0].as_str()) {
            print!("{}", rrdtool_command_usage(&command_args[0]));
            return Ok(());
        }
        if let Some(text) = rrdtool_builtin_reply(command_args, false) {
            print!("{text}");
            return Ok(());
        }
        let result = match command_args[0].as_str() {
            "create" => rrdtool_create(command_args),
            "fetch" => rrdtool_fetch(command_args),
            "update" => rrdtool_update(command_args),
            "updatev" => rrdtool_updatev(command_args),
            "last" => rrdtool_last(command_args),
            "lastupdate" => rrdtool_lastupdate(command_args),
            "first" => rrdtool_first(command_args),
            "info" => rrdtool_info(command_args),
            "dump" => rrdtool_dump(command_args),
            "restore" => rrdtool_restore(command_args),
            "tune" => rrdtool_tune(command_args),
            "list" => rrdtool_list(command_args),
            "resize" => rrdtool_resize(command_args),
            "xport" => rrdtool_xport(command_args),
            "graph" => rrdtool_graph(command_args, false),
            "graphv" => rrdtool_graph(command_args, true),
            "flushcached" => rrdtool_flushcached(command_args),
            command => Err(format!("unknown function '{command}'").into()),
        };
        if let Err(error) = result {
            eprintln!("ERROR: {error}");
            std::process::exit(1);
        }
        return Ok(());
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
    let store = Store::open(&args.root)?;
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

/// Usage for a lone argument that is not a data command. Kadupul reads the
/// version from this banner, so `-v` and unknown words print it and succeed.
fn rrdtool_command_usage(command: &str) -> String {
    let body = match command {
        "quit" => " * quit - closing a session in remote mode\n\n\trrdtool quit\n",
        "ls" => " * ls - lists all *.rrd files in current directory\n\n\trrdtool ls\n",
        "cd" => " * cd - changes the current directory\n\n\trrdtool cd new directory\n",
        "mkdir" => " * mkdir - creates a new directory\n\n\trrdtool mkdir newdirectoryname\n",
        "pwd" => " * pwd - returns the current working directory\n\n\trrdtool pwd\n",
        _ => return rrdtool_usage(false),
    };
    format!("{RRDTOOL_USAGE_HEADER}{body}\n{RRDTOOL_USAGE_FOOTER}")
}

/// Replies RRDtool produces before command dispatch once at least a command
/// and one argument are present.
fn rrdtool_builtin_reply(args: &[String], remote: bool) -> Option<String> {
    match args.first()?.as_str() {
        "help" | "--help" | "-help" | "-?" | "-h" => Some(rrdtool_usage(remote)),
        "--version" | "version" | "v" | "-v" | "-version" => {
            Some("RRDtool 1.11.0  Copyright by Tobi Oetiker (1.011000)\n".to_owned())
        }
        _ => None,
    }
}

/// RRDtool's `-` mode accepts one command per input line and flushes a result
/// marker after every successful command. Cacti keeps this process open while
/// polling, so commands must be handled without terminating the process.
fn rrdtool_batch() -> Result<(), Box<dyn std::error::Error>> {
    let started = std::time::Instant::now();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        // RRDtool counts the newline itself as an argument, so only an
        // unterminated blank final line is "not enough arguments"; a blank
        // terminated line falls through to the usage text below.
        let terminated = line.ends_with('\n');
        let arguments = match shell_words::split(line.trim_end_matches(['\n', '\r'])) {
            Ok(arguments) if !arguments.is_empty() || terminated => arguments,
            Ok(_) => {
                writeln!(stdout, "ERROR: not enough arguments")?;
                stdout.flush()?;
                continue;
            }
            Err(_) => {
                writeln!(stdout, "ERROR: creating arguments")?;
                stdout.flush()?;
                continue;
            }
        };
        if arguments.first().is_some_and(|command| command == "quit") {
            if arguments.len() == 1 {
                break;
            }
            writeln!(stdout, "ERROR: invalid parameter count for quit")?;
            stdout.flush()?;
            continue;
        }
        // The remote directory commands (ls, cd, mkdir, pwd) are not
        // implemented, so they report an unknown function instead of usage.
        let remote_directory_command = arguments
            .first()
            .is_some_and(|command| matches!(command.as_str(), "ls" | "cd" | "mkdir" | "pwd"));
        let builtin = if arguments.len() < 2 && !remote_directory_command {
            Some(rrdtool_usage(true))
        } else {
            rrdtool_builtin_reply(&arguments, true)
        };
        if let Some(text) = builtin {
            write!(stdout, "{text}")?;
            writeln!(stdout, "{}", rrdtool_batch_ack(started))?;
            stdout.flush()?;
            continue;
        }
        let result = match arguments[0].as_str() {
            "create" => rrdtool_create(&arguments),
            "fetch" => rrdtool_fetch(&arguments),
            "update" => rrdtool_update(&arguments),
            "updatev" => rrdtool_updatev(&arguments),
            "last" => rrdtool_last(&arguments),
            "lastupdate" => rrdtool_lastupdate(&arguments),
            "first" => rrdtool_first(&arguments),
            "info" => rrdtool_info(&arguments),
            "dump" => rrdtool_dump(&arguments),
            "restore" => rrdtool_restore(&arguments),
            "tune" => rrdtool_tune(&arguments),
            "list" => rrdtool_list(&arguments),
            "resize" => rrdtool_resize(&arguments),
            command => Err(format!("unknown function '{command}'").into()),
        };
        match result {
            Ok(()) => writeln!(stdout, "{}", rrdtool_batch_ack(started))?,
            Err(error) => writeln!(stdout, "ERROR: {error}")?,
        }
        stdout.flush()?;
    }
    Ok(())
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
    let mut positional = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--start" | "-b" => {
                index += 1;
                let value = args.get(index).ok_or("create --start requires a time")?;
                start = parse_rrd_time(value, now).map_err(|error| {
                    if error
                        .to_string()
                        .starts_with("unsupported RRDtool time specification:")
                    {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("start time: unparsable time: {value}"),
                        )
                        .into()
                    } else {
                        error
                    }
                })?;
                start_was_set = true;
            }
            "--step" | "-s" => {
                index += 1;
                step = parse_rrd_scaled_duration(
                    args.get(index).ok_or("create --step requires a duration")?,
                    1,
                )?;
                step_was_set = true;
            }
            "--template" | "-t" => {
                index += 1;
                template_file = Some(
                    args.get(index)
                        .ok_or("create --template requires a template file")?
                        .clone(),
                );
            }
            "--source" | "-r" => {
                index += 1;
                source_files.push(
                    args.get(index)
                        .ok_or("create --source requires a source file")?
                        .clone(),
                );
            }
            "--no-overwrite" | "-O" => no_overwrite = true,
            "--daemon" | "-d" => {
                index += 1;
                let address = args
                    .get(index)
                    .ok_or("create --daemon requires an address")?;
                daemon_address = Some(address.clone());
            }
            value if value.starts_with("--daemon=") => {
                daemon_address = Some(value["--daemon=".len()..].to_owned());
            }
            value if value.starts_with('-') => {
                return Err(format!("unsupported rrdtool create option: {value}").into());
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if positional.is_empty() {
        return Err(
            "Usage: rrdtool create file.rrd [--start epoch] [--step seconds] DS:... RRA:...".into(),
        );
    }
    let filename = positional.remove(0);
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
    if start < 315_360_000 {
        return Err("the first entry to the RRD should be after 1980".into());
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
        if daemon_address
            .as_ref()
            .is_some_and(|address| !address.is_empty())
            || std::env::var("RRDCACHED_ADDRESS").is_ok_and(|address| !address.is_empty())
        {
            return Err("create --source with --daemon is unsupported".into());
        }
        let source_path = std::path::Path::new(&source_files[0]);
        match std::fs::metadata(source_path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!("Not a regular file: {}", source_files[0]).into());
            }
            Ok(_) => {}
            Err(error) => {
                let detail = match error.kind() {
                    std::io::ErrorKind::NotFound => "No such file or directory".to_owned(),
                    _ => error
                        .to_string()
                        .split(" (os error ")
                        .next()
                        .unwrap_or("I/O error")
                        .to_owned(),
                };
                return Err(format!(
                    "error checking for source RRD {}: {}",
                    source_files[0], detail
                )
                .into());
            }
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
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
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
    } else {
        create_rrd_file(
            filename,
            start,
            step,
            &data_sources,
            &archives,
            no_overwrite,
        )?;
    }
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
    if args.len() < 3 {
        return Err(
            "Usage: rrdtool fetch <file> <CF> [--resolution seconds] [--start epoch] [--end epoch]"
                .into(),
        );
    }
    let filename = PathBuf::from(&args[1]);
    let cf = &args[2];
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut start_spec = None::<String>;
    let mut end_spec = None::<String>;
    let mut start = now - 24 * 60 * 60;
    let mut end = now;
    let mut resolution = 1_u64;
    let mut align_start = false;
    let mut daemon_address = None::<String>;
    let mut index = 3;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "--start" | "-s" | "--end" | "-e" | "--resolution" | "-r" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("option {argument} requires a value"))?;
                match argument.as_str() {
                    "--start" | "-s" => start_spec = Some(value.clone()),
                    "--end" | "-e" => end_spec = Some(value.clone()),
                    _ => resolution = parse_rrd_resolution(value)?,
                }
                index += 2;
            }
            "--align-start" | "-a" => {
                align_start = true;
                index += 1;
            }
            "--daemon" | "-d" => {
                index += 1;
                daemon_address = Some(
                    args.get(index)
                        .ok_or("fetch --daemon requires an address")?
                        .clone(),
                );
                index += 1;
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
                index += 1;
            }
            unsupported if unsupported.starts_with('-') => {
                return Err(format!("unsupported fetch option: {unsupported}").into());
            }
            positional => return Err(format!("unexpected fetch argument: {positional}").into()),
        }
    }
    (start, end) =
        resolve_rrd_range_times(start_spec.as_deref(), end_spec.as_deref(), start, end, now)?;
    if start < 315_360_000 {
        return Err("the first entry to fetch should be after 1980".into());
    }
    if align_start {
        if resolution == 0 {
            return Err("resolution must be positive".into());
        }
        let delta = start.rem_euclid(i64::try_from(resolution)?);
        start = start.checked_sub(delta).ok_or("start time overflows")?;
        end = end.checked_sub(delta).ok_or("end time overflows")?;
    }

    let daemon_address = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty());
    let from_daemon = daemon_address.is_some();
    if !from_daemon {
        ensure_rrd_file_exists(&filename)?;
        let metadata = std::fs::metadata(&filename)?;
        if !metadata.is_file() || metadata.len() == 0 {
            let error = if metadata.len() == 0 {
                std::io::Error::from_raw_os_error(libc::EINVAL)
            } else {
                let file = std::fs::File::open(&filename)?;
                let length = usize::try_from(metadata.len()).unwrap_or(1).max(1);
                let mapped = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        length,
                        libc::PROT_READ,
                        libc::MAP_PRIVATE,
                        std::os::fd::AsRawFd::as_raw_fd(&file),
                        0,
                    )
                };
                if mapped == libc::MAP_FAILED {
                    std::io::Error::last_os_error()
                } else {
                    unsafe { libc::munmap(mapped, length) };
                    std::io::Error::from_raw_os_error(libc::EINVAL)
                }
            };
            let errno = error.raw_os_error().unwrap_or(libc::EINVAL);
            let message =
                unsafe { std::ffi::CStr::from_ptr(libc::strerror(errno)).to_string_lossy() };
            return Err(format!("mmaping file '{}': {message}", filename.display()).into());
        }
        if metadata.len() < 128 {
            return Err("reached EOF while loading header rrd->stat_head".into());
        }
        let mut file = std::fs::File::open(&filename)?;
        let mut cookie = [0_u8; 4];
        file.read_exact(&mut cookie)?;
        if cookie != *b"RRD\0" {
            return Err(format!("'{}' is not an RRD file", filename.display()).into());
        }
        let mut version_bytes = [0_u8; 4];
        file.seek(std::io::SeekFrom::Start(4))?;
        file.read_exact(&mut version_bytes)?;
        if version_bytes.iter().all(u8::is_ascii_digit) {
            let version = std::str::from_utf8(&version_bytes)?.parse::<u32>()?;
            if version > 5 {
                return Err(format!("can't handle RRD file version {version:04}").into());
            }
        }
    }
    let result = if let Some(address) = daemon_address {
        let escaped = escape_rrdcached_field(&filename.to_string_lossy());
        let command = format!("FETCH {escaped} {cf} {start} {end}");
        parse_rrdcached_fetch(&send_rrdcached_multiline_command(&address, &command)?)?
    } else {
        fetch_rrd_file(&filename, cf, start, end, resolution)?
    };
    print!("           ");
    for data_source in &result.data_sources {
        print!("{data_source:>20}");
    }
    println!("\n");
    for row in result.rows {
        print!("{:>10}:", row.timestamp);
        for value in row.values {
            match value {
                Some(value) => print!(" {}", format_fetch_value(value)),
                None if from_daemon => print!(" {}", rrd_daemon_unknown_text()),
                None => print!(" {}", rrd_unknown_text()),
            }
        }
        println!();
    }
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
    let filename = args.get(1).ok_or("graph requires an output filename")?;
    let mut format = String::from("PNG");
    let mut xport_args = vec![String::from("xport")];
    let mut definitions = Vec::new();
    let mut graph_gprints = Vec::<(String, String)>::new();
    let mut graph_series = Vec::<GraphSeries>::new();
    let mut graph_prints = Vec::<GraphPrint>::new();
    let mut image_width = 400_u32;
    let mut image_height = 100_u32;
    let mut graph_title = None::<String>;
    let mut vertical_label = None::<String>;
    let mut vertical_label_angle = 90.0_f64;
    let mut imginfo = None::<String>;
    let mut lower_limit = None::<f64>;
    let mut upper_limit = None::<f64>;
    let mut no_legend = false;
    let mut rigid_scale = false;
    let mut allow_shrink = false;
    let mut alt_autoscale = false;
    let mut alt_autoscale_min = false;
    let mut alt_autoscale_max = false;
    let mut only_graph = false;
    let mut full_size_mode = false;
    let mut force_rules_legend = false;
    let mut legend_bottomup = false;
    let mut graph_colors = GraphColors::default();
    let mut grid_dash = Vec::<f64>::new();
    let mut border_width = 2_u32;
    let mut si_base = 1000_u32;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut graph_start_spec = None::<String>;
    let mut graph_end_spec = None::<String>;
    let mut index = 2;
    while index < args.len() {
        let argument = &args[index];
        if let Some(value) = argument.strip_prefix("--imgformat=") {
            format = value.to_ascii_uppercase();
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--color=") {
            graph_colors.parse_override(value)?;
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--imginfo=") {
            imginfo = Some(value.to_owned());
            index += 1;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--base=") {
            si_base = parse_graph_base(value)?;
            index += 1;
            continue;
        }
        if let Some(address) = argument.strip_prefix("--daemon=") {
            xport_args.push(String::from("--daemon"));
            xport_args.push(address.to_owned());
            index += 1;
            continue;
        }
        match argument.as_str() {
            "--imgformat" | "-a" => {
                format = args
                    .get(index + 1)
                    .ok_or("--imgformat requires a value")?
                    .to_ascii_uppercase();
                index += 2;
            }
            "--start" | "-s" | "--end" | "-e" | "--step" | "-S" | "--maxrows" | "-m" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?;
                match argument.as_str() {
                    "--start" | "-s" => graph_start_spec = Some(value.clone()),
                    "--end" | "-e" => graph_end_spec = Some(value.clone()),
                    _ => {}
                }
                xport_args.push(argument.clone());
                xport_args.push(value.clone());
                index += 2;
            }
            "--base" | "-b" => {
                si_base = parse_graph_base(args.get(index + 1).ok_or("--base requires a value")?)?;
                index += 2;
            }
            "--width" | "-w" | "--height" | "-h" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .parse::<u32>()?;
                match argument.as_str() {
                    "--width" | "-w" => image_width = value,
                    _ => image_height = value,
                }
                index += 2;
            }
            "--title" | "-t" | "--vertical-label" | "-v" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .clone();
                if matches!(argument.as_str(), "--title" | "-t") {
                    graph_title = Some(value);
                } else {
                    vertical_label = Some(value);
                }
                index += 2;
            }
            "--vertical-label-angle" => {
                vertical_label_angle = args
                    .get(index + 1)
                    .ok_or("--vertical-label-angle requires a value")?
                    .parse()?;
                if !vertical_label_angle.is_finite() {
                    return Err("--vertical-label-angle must be finite".into());
                }
                index += 2;
            }
            "--imginfo" | "-f" => {
                imginfo = Some(
                    args.get(index + 1)
                        .ok_or("--imginfo requires a value")?
                        .clone(),
                );
                index += 2;
            }
            "--lower-limit" | "-l" | "--upper-limit" | "-u" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .parse::<f64>()?;
                if !value.is_finite() {
                    return Err(format!("{argument} must be finite").into());
                }
                if matches!(argument.as_str(), "--lower-limit" | "-l") {
                    lower_limit = Some(value);
                } else {
                    upper_limit = Some(value);
                }
                index += 2;
            }
            "--no-legend" | "-g" => {
                no_legend = true;
                index += 1;
            }
            "--only-graph" | "-j" => {
                only_graph = true;
                index += 1;
            }
            "--full-size-mode" | "-D" => {
                full_size_mode = true;
                index += 1;
            }
            "--force-rules-legend" | "-F" => {
                force_rules_legend = true;
                index += 1;
            }
            "--color" | "-c" => {
                graph_colors
                    .parse_override(args.get(index + 1).ok_or("--color requires a value")?)?;
                index += 2;
            }
            "--grid-dash" => {
                let value = args.get(index + 1).ok_or("--grid-dash requires a value")?;
                let (on, off) = value.split_once(':').ok_or("--grid-dash expects on:off")?;
                let on = on.parse::<f64>()?;
                let off = off.parse::<f64>()?;
                if !on.is_finite() || !off.is_finite() || on <= 0.0 || off < 0.0 {
                    return Err(
                        "--grid-dash requires a positive on length and nonnegative off length"
                            .into(),
                    );
                }
                grid_dash = vec![on, off];
                index += 2;
            }
            "--border" => {
                border_width = args
                    .get(index + 1)
                    .ok_or("--border requires a value")?
                    .parse()?;
                if border_width > 64 {
                    return Err("--border width exceeds 64 pixels".into());
                }
                index += 2;
            }
            value if value.starts_with("--legend-direction=") => {
                match &value["--legend-direction=".len()..] {
                    "topdown" => legend_bottomup = false,
                    "bottomup" | "bottomup2" => legend_bottomup = true,
                    direction => {
                        return Err(format!("invalid legend direction: {direction}").into());
                    }
                }
                index += 1;
            }
            "--rigid" | "-r" => {
                rigid_scale = true;
                index += 1;
            }
            "--allow-shrink" => {
                allow_shrink = true;
                index += 1;
            }
            "--alt-autoscale" | "-A" => {
                alt_autoscale = true;
                index += 1;
            }
            "--alt-autoscale-min" | "-J" => {
                alt_autoscale_min = true;
                index += 1;
            }
            "--alt-autoscale-max" | "-M" => {
                alt_autoscale_max = true;
                index += 1;
            }
            "--daemon" | "-d" => {
                let address = args
                    .get(index + 1)
                    .ok_or("graph --daemon requires an address")?;
                xport_args.push(String::from("--daemon"));
                xport_args.push(address.clone());
                index += 2;
            }
            option if option.starts_with("--daemon=") => {
                xport_args.push(String::from("--daemon"));
                xport_args.push(option["--daemon=".len()..].to_owned());
                index += 1;
            }
            value
                if value.starts_with("DEF:")
                    || value.starts_with("CDEF:")
                    || value.starts_with("XPORT:")
                    || value.starts_with("VDEF:") =>
            {
                definitions.push(value.to_owned());
                index += 1;
            }
            value if value.starts_with("PRINT:") || value.starts_with("GPRINT:") => {
                graph_prints.push(parse_graph_print(value)?);
                index += 1;
            }
            value if value.starts_with("TICK:") => {
                let (series, legend) = parse_graph_tick(value)?;
                definitions.push(format!("XPORT:{}:{legend}", series.variable));
                graph_series.push(series);
                index += 1;
            }
            value if value.starts_with("HRULE:") => {
                graph_series.push(parse_graph_hrule(value)?);
                index += 1;
            }
            value if value.starts_with("VRULE:") => {
                graph_series.push(parse_graph_vrule(value, now)?);
                index += 1;
            }
            value if value.starts_with("LINE") || value.starts_with("AREA:") => {
                let (series, legend) = parse_graph_series(value)?;
                if series.stack
                    && !graph_series
                        .iter()
                        .any(|previous| matches!(previous.style, "line" | "area"))
                {
                    return Err(format!("STACK has no preceding LINE or AREA in {value}").into());
                }
                definitions.push(format!("XPORT:{}:{legend}", series.variable));
                let label = if series.style == "area" {
                    format!("  {legend}")
                } else {
                    legend
                };
                graph_gprints.push((series.style.to_owned(), label));
                graph_series.push(series);
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(format!("unsupported graph option: {value}").into());
            }
            value => return Err(format!("unsupported graph element: {value}").into()),
        }
    }
    if !matches!(
        format.as_str(),
        "XML" | "JSON" | "XMLENUM" | "JSONTIME" | "PNG"
    ) {
        return Err(format!("RRDtool graph format {format} is unsupported").into());
    }
    match format.as_str() {
        "JSON" => xport_args.push(String::from("--json")),
        "JSONTIME" => {
            xport_args.push(String::from("--json"));
            xport_args.push(String::from("--showtime"));
        }
        "XMLENUM" => xport_args.push(String::from("--enumds")),
        _ => {}
    }
    if format == "XML" || format == "XMLENUM" {
        xport_args.push(String::from("--showtime"));
    }
    // RRDtool permits a graph made only from rules. Its graph engine still
    // builds a time axis; the XPORT-backed Rondi renderer needs a private
    // constant series to provide that timeline in PNG mode.
    if format == "PNG"
        && !definitions
            .iter()
            .any(|definition| definition.starts_with("XPORT:"))
    {
        definitions.push(String::from("CDEF:__rondi_rule_anchor=0,0,+"));
        definitions.push(String::from("XPORT:__rondi_rule_anchor:"));
    }
    xport_args.extend(definitions);
    let rendered =
        render_xport_with_graph_prints(&xport_args, Some(&graph_gprints), &graph_prints, si_base)?;
    let graph_image = if format == "PNG" {
        Some(render_graph_png(
            &rendered,
            &graph_series,
            GraphPngOptions {
                width: image_width,
                height: image_height,
                title: graph_title.as_deref(),
                vertical_label: vertical_label.as_deref(),
                vertical_label_angle,
                lower_limit,
                upper_limit,
                show_legend: !no_legend,
                rigid_scale,
                allow_shrink,
                alt_autoscale,
                alt_autoscale_min,
                alt_autoscale_max,
                only_graph,
                full_size_mode,
                force_rules_legend,
                legend_bottomup,
                colors: graph_colors,
                grid_dash,
                border_width,
            },
        )?)
    } else {
        None
    };
    let (output, xport_start, xport_end, step, prints) = (
        rendered.output,
        rendered.start,
        rendered.end,
        rendered.step,
        rendered.prints,
    );
    let (start, end) = if graph_start_spec.is_none() && graph_end_spec.is_none() {
        (xport_start, xport_end)
    } else {
        resolve_rrd_range_times(
            graph_start_spec.as_deref(),
            graph_end_spec.as_deref(),
            now - 24 * 60 * 60,
            now,
            now,
        )?
    };
    if filename == "-" {
        if verbose {
            print!("graph_start = {start}\ngraph_end = {end}\ngraph_step = {step}\n");
            for (index, value) in prints.iter().enumerate() {
                println!("print[{index}] = {}", serde_json::to_string(value)?);
            }
            println!(
                "image = BLOB_SIZE:{}",
                graph_image.as_ref().map_or(output.len(), Vec::len)
            );
        }
        if let Some(image) = graph_image.as_deref() {
            if let Some(format) = imginfo.as_deref() {
                let (width, height) = png_dimensions(image)?;
                println!("{}", format_imginfo(format, "memory", width, height)?);
            }
            std::io::stdout().write_all(image)?;
        } else {
            print!("{output}");
        }
    } else {
        // rrd_tool.c only recognizes the separate `--imginfo`/`-f` spelling
        // when deciding whether to print the canvas size.
        let print_dimensions = !verbose
            && !args[1..]
                .iter()
                .any(|argument| argument == "--imginfo" || argument == "-f");
        if print_dimensions {
            let (width, height) = match graph_image.as_deref() {
                Some(image) => png_dimensions(image)?,
                None => (0, 0),
            };
            println!("{width}x{height}");
        }
        if let Some(image) = graph_image.as_deref() {
            std::fs::write(filename, image)?;
            if let Some(format) = imginfo.as_deref() {
                let basename = Path::new(filename)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(filename);
                let (width, height) = png_dimensions(image)?;
                println!("{}", format_imginfo(format, basename, width, height)?);
            }
        } else {
            std::fs::write(filename, output.as_bytes())?;
        }
        if verbose {
            print!("graph_start = {start}\ngraph_end = {end}\ngraph_step = {step}\n");
            for (index, value) in prints.iter().enumerate() {
                println!("print[{index}] = {}", serde_json::to_string(value)?);
            }
        } else {
            for value in &prints {
                println!("{value}");
            }
        }
    }
    Ok(())
}

// rrd_graph.c reads --base with atol, so trailing text after the digits is
// ignored before the 1000/1024 check.
fn parse_graph_base(value: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let trimmed = value.trim_start();
    let negative = trimmed.starts_with('-');
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let base = unsigned
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0_u64, |total, digit| {
            total
                .saturating_mul(10)
                .saturating_add(u64::from(digit - b'0'))
        });
    match (negative, base) {
        (false, 1000) => Ok(1000),
        (false, 1024) => Ok(1024),
        _ => Err("the only sensible value for base apart from 1000 is 1024".into()),
    }
}

fn parse_graph_series(
    definition: &str,
) -> Result<(GraphSeries, String), Box<dyn std::error::Error>> {
    let (directive, source_and_legend) = definition
        .split_once(':')
        .ok_or_else(|| format!("invalid graph element: {definition}"))?;
    let (style, line_width) = if directive == "AREA" {
        ("area", 0.0)
    } else if let Some(width) = directive.strip_prefix("LINE") {
        let parsed = if width.is_empty() {
            1.0
        } else {
            width.parse::<f64>()?
        };
        if !parsed.is_finite() || parsed < 0.0 {
            return Err(format!("invalid line width in {definition}").into());
        }
        ("line", parsed)
    } else {
        return Err(format!("unsupported graph element: {definition}").into());
    };
    let (source, legend_and_flags) = source_and_legend
        .split_once(':')
        .unwrap_or((source_and_legend, ""));
    let mut legend_parts = legend_and_flags.split(':').collect::<Vec<_>>();
    let mut stack = false;
    let mut skip_scale = false;
    let mut grad_height = if style == "area" { 50.0 } else { 0.0 };
    let mut dash_pattern = Vec::new();
    let mut dash_offset = 0.0;
    loop {
        match legend_parts.last().copied() {
            Some("STACK") => {
                stack = true;
                legend_parts.pop();
            }
            Some("skipscale") => {
                skip_scale = true;
                legend_parts.pop();
            }
            Some(flag) if flag.starts_with("gradheight=") => {
                grad_height = flag["gradheight=".len()..].parse::<f64>()?;
                if !grad_height.is_finite() {
                    return Err(format!("invalid gradheight in {definition}").into());
                }
                legend_parts.pop();
            }
            Some("dashes") if style == "line" => {
                dash_pattern = vec![5.0, 5.0];
                legend_parts.pop();
            }
            Some(flag) if style == "line" && flag.starts_with("dashes=") => {
                dash_pattern = parse_dash_pattern(&flag[7..], definition)?;
                legend_parts.pop();
            }
            Some(flag) if style == "line" && flag.starts_with("dash-offset=") => {
                dash_offset = flag[12..].parse::<f64>()?;
                if !dash_offset.is_finite() {
                    return Err(format!("invalid dash offset in {definition}").into());
                }
                legend_parts.pop();
            }
            _ => break,
        }
    }
    let legend = legend_parts.join(":");
    let mut source_parts = source.split('#');
    let variable = source_parts.next().unwrap_or_default();
    let color = source_parts
        .next()
        .map(|value| parse_graph_color(value, definition))
        .transpose()?;
    let color2 = source_parts
        .next()
        .map(|value| parse_graph_color(value, definition))
        .transpose()?;
    if source_parts.next().is_some() || (color2.is_some() && style != "area") {
        return Err(format!("invalid graph color in {definition}").into());
    }
    if variable.is_empty() || variable.contains(':') {
        return Err(format!("invalid data variable in graph element: {definition}").into());
    }
    if color.is_none() && !legend.is_empty() {
        return Err("cannot specify a legend without a color".into());
    }
    Ok((
        GraphSeries {
            variable: variable.to_owned(),
            style,
            line_width,
            stack,
            skip_scale,
            color,
            color2,
            grad_height,
            tick_fraction: 0.0,
            rule_value: None,
            rule_time: None,
            legend: legend.clone(),
            dash_pattern,
            dash_offset,
        },
        legend.to_owned(),
    ))
}

fn parse_graph_tick(definition: &str) -> Result<(GraphSeries, String), Box<dyn std::error::Error>> {
    let value = definition
        .strip_prefix("TICK:")
        .ok_or_else(|| format!("invalid TICK element: {definition}"))?;
    let (source, options) = value.split_once(':').unwrap_or((value, ""));
    let (variable, color) = source
        .split_once('#')
        .ok_or_else(|| format!("TICK color is required in {definition}"))?;
    let color = parse_graph_color(color, definition)?;
    let mut options = options.split(':');
    let first = options.next().unwrap_or_default();
    let (fraction, legend) = match first.parse::<f64>() {
        Ok(fraction) => (fraction, options.collect::<Vec<_>>().join(":")),
        Err(_) if first.is_empty() => (0.1, options.collect::<Vec<_>>().join(":")),
        Err(_) => (
            0.1,
            std::iter::once(first)
                .chain(options)
                .collect::<Vec<_>>()
                .join(":"),
        ),
    };
    if variable.is_empty() || !fraction.is_finite() {
        return Err(format!("invalid TICK element: {definition}").into());
    }
    Ok((
        GraphSeries {
            variable: variable.to_owned(),
            style: "tick",
            line_width: 1.0,
            stack: false,
            skip_scale: true,
            color: Some(color),
            color2: None,
            grad_height: 0.0,
            tick_fraction: fraction,
            rule_value: None,
            rule_time: None,
            legend: legend.clone(),
            dash_pattern: Vec::new(),
            dash_offset: 0.0,
        },
        legend,
    ))
}

fn parse_graph_hrule(definition: &str) -> Result<GraphSeries, Box<dyn std::error::Error>> {
    let value = definition
        .strip_prefix("HRULE:")
        .ok_or_else(|| format!("invalid HRULE element: {definition}"))?;
    let (value_and_color, legend_and_flags) = value.split_once(':').unwrap_or((value, ""));
    let (legend, dash_pattern, dash_offset) = parse_rule_legend(legend_and_flags, definition)?;
    let (value, color) = value_and_color
        .split_once('#')
        .ok_or_else(|| format!("HRULE color is required in {definition}"))?;
    let value = value.parse::<f64>()?;
    if !value.is_finite() {
        return Err(format!("invalid HRULE value in {definition}").into());
    }
    Ok(GraphSeries {
        variable: String::new(),
        style: "hrule",
        line_width: 1.0,
        stack: false,
        skip_scale: false,
        color: Some(parse_graph_color(color, definition)?),
        color2: None,
        grad_height: 0.0,
        tick_fraction: 0.0,
        rule_value: Some(value),
        rule_time: None,
        legend,
        dash_pattern,
        dash_offset,
    })
}

fn parse_graph_vrule(
    definition: &str,
    now: i64,
) -> Result<GraphSeries, Box<dyn std::error::Error>> {
    let value = definition
        .strip_prefix("VRULE:")
        .ok_or_else(|| format!("invalid VRULE element: {definition}"))?;
    let (time_and_color, legend_and_flags) = value.split_once(':').unwrap_or((value, ""));
    let (legend, dash_pattern, dash_offset) = parse_rule_legend(legend_and_flags, definition)?;
    let (time, color) = time_and_color
        .split_once('#')
        .ok_or_else(|| format!("VRULE color is required in {definition}"))?;
    let time = parse_rrd_time(time, now)?;
    Ok(GraphSeries {
        variable: String::new(),
        style: "vrule",
        line_width: 1.0,
        stack: false,
        skip_scale: true,
        color: Some(parse_graph_color(color, definition)?),
        color2: None,
        grad_height: 0.0,
        tick_fraction: 0.0,
        rule_value: None,
        rule_time: Some(time),
        legend,
        dash_pattern,
        dash_offset,
    })
}

fn parse_dash_pattern(
    value: &str,
    definition: &str,
) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let values = value
        .split(',')
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty()
        || (values.len() != 1 && values.len() % 2 != 0)
        || values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
    {
        return Err(format!("invalid dash pattern in {definition}").into());
    }
    Ok(values)
}

fn parse_rule_legend(
    value: &str,
    definition: &str,
) -> Result<(String, Vec<f64>, f64), Box<dyn std::error::Error>> {
    let mut parts = value.split(':').collect::<Vec<_>>();
    let mut pattern = Vec::new();
    let mut offset = 0.0;
    loop {
        match parts.last().copied() {
            Some("dashes") => {
                pattern = vec![5.0, 5.0];
                parts.pop();
            }
            Some(flag) if flag.starts_with("dashes=") => {
                pattern = parse_dash_pattern(&flag[7..], definition)?;
                parts.pop();
            }
            Some(flag) if flag.starts_with("dash-offset=") => {
                offset = flag[12..].parse::<f64>()?;
                if !offset.is_finite() {
                    return Err(format!("invalid dash offset in {definition}").into());
                }
                parts.pop();
            }
            _ => break,
        }
    }
    Ok((parts.join(":"), pattern, offset))
}

fn parse_graph_color(value: &str, definition: &str) -> Result<[u8; 4], Box<dyn std::error::Error>> {
    if !matches!(value.len(), 6 | 8) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("invalid graph color in {definition}").into());
    }
    Ok([
        u8::from_str_radix(&value[0..2], 16)?,
        u8::from_str_radix(&value[2..4], 16)?,
        u8::from_str_radix(&value[4..6], 16)?,
        if value.len() == 8 {
            u8::from_str_radix(&value[6..8], 16)?
        } else {
            255
        },
    ])
}

#[derive(Clone)]
struct GraphSeries {
    variable: String,
    style: &'static str,
    line_width: f64,
    stack: bool,
    skip_scale: bool,
    color: Option<[u8; 4]>,
    color2: Option<[u8; 4]>,
    grad_height: f64,
    tick_fraction: f64,
    rule_value: Option<f64>,
    rule_time: Option<i64>,
    legend: String,
    dash_pattern: Vec<f64>,
    dash_offset: f64,
}

struct GraphPngOptions<'a> {
    width: u32,
    height: u32,
    title: Option<&'a str>,
    vertical_label: Option<&'a str>,
    vertical_label_angle: f64,
    lower_limit: Option<f64>,
    upper_limit: Option<f64>,
    show_legend: bool,
    rigid_scale: bool,
    allow_shrink: bool,
    alt_autoscale: bool,
    alt_autoscale_min: bool,
    alt_autoscale_max: bool,
    only_graph: bool,
    full_size_mode: bool,
    force_rules_legend: bool,
    legend_bottomup: bool,
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

impl GraphColors {
    fn parse_override(&mut self, value: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (tag, color) = value
            .split_once('#')
            .ok_or("--color expects TAG#rrggbb[aa]")?;
        let color = parse_graph_color(color, value)?;
        let target = match tag {
            "BACK" => &mut self.back,
            "CANVAS" => &mut self.canvas,
            "SHADEA" => &mut self.shade_a,
            "SHADEB" => &mut self.shade_b,
            "GRID" => &mut self.grid,
            "MGRID" => &mut self.mgrid,
            "FONT" => &mut self.font,
            "AXIS" => &mut self.axis,
            "FRAME" => &mut self.frame,
            "ARROW" => &mut self.arrow,
            _ => return Err(format!("unknown graph color tag: {tag}").into()),
        };
        *target = color;
        Ok(())
    }
}

#[derive(Debug)]
struct GraphPrint {
    kind: &'static str,
    variable: String,
    consolidation: Option<String>,
    format: String,
    formatter: GraphPrintFormatter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphPrintFormatter {
    Numeric,
    Strftime,
}

struct GraphVdef {
    name: String,
    variable: String,
    function: rondi::VdefFunction,
    percentile: Option<f64>,
}

#[derive(Default)]
struct GraphDefOptions {
    start: Option<String>,
    end: Option<String>,
    daemon: Option<String>,
}

/// Parse `DEF:vname=rrd:ds:cf[:key=value...]` the way rrd_graph_helper.c
/// splits graph arguments: `\:` escapes a colon, `key=value` fields may
/// appear anywhere after the first, the last repeated key wins, and fields
/// that nothing consumes are an error.
fn parse_graph_def(
    value: &str,
) -> Result<(rondi::RrdXportDefinition, GraphDefOptions), Box<dyn std::error::Error>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' if chars.peek() == Some(&':') => field.push(chars.next().unwrap_or(':')),
            ':' => fields.push(std::mem::take(&mut field)),
            _ => field.push(ch),
        }
    }
    fields.push(field);
    // (key, value, consumed) in argument order; positional fields have no key.
    let mut entries = Vec::<(Option<String>, String, bool)>::new();
    let mut vname_rrd = None;
    for field in fields.iter().skip(1) {
        match field.split_once('=') {
            Some((key, rest)) if vname_rrd.is_none() => {
                vname_rrd = Some((key.to_owned(), rest.to_owned()));
            }
            Some((key, rest)) => entries.push((Some(key.to_owned()), rest.to_owned(), false)),
            None => entries.push((None, field.clone(), false)),
        }
    }
    let mut take = |key: &str| {
        entries
            .iter_mut()
            .rev()
            .find(|(name, _, _)| name.as_deref() == Some(key))
            .map(|(_, value, used)| {
                *used = true;
                value.clone()
            })
    };
    let reduce = take("reduce");
    let daemon = take("daemon");
    let step = take("step");
    let start = take("start");
    let end = take("end");
    if let Some(reduce) = &reduce
        && !matches!(
            reduce.as_str(),
            "AVERAGE"
                | "MIN"
                | "MAX"
                | "LAST"
                | "HWPREDICT"
                | "MHWPREDICT"
                | "DEVPREDICT"
                | "SEASONAL"
                | "DEVSEASONAL"
                | "FAILURES"
        )
    {
        return Err(format!("bad reduce CF: {reduce}").into());
    }
    let step = match step {
        Some(text) => match text.trim_start().parse::<i64>() {
            Ok(step) if step >= 1 => Some(step.unsigned_abs()),
            _ => return Err(format!("Bad step value: {text}").into()),
        },
        None => None,
    };
    let Some((name, file)) = vname_rrd else {
        return Err(format!("No argument for definition of vdef/rrd in {value}").into());
    };
    let mut next_positional = |what: &str| {
        entries
            .iter_mut()
            .find(|(key, _, used)| key.is_none() && !used)
            .map(|(_, field, used)| {
                *used = true;
                field.clone()
            })
            .ok_or_else(|| format!("No argument for definition of {what} in {value}"))
    };
    let data_source = next_positional("DS")?;
    let consolidation = next_positional("CF")?;
    let unused = entries
        .iter()
        .filter(|(_, _, used)| !used)
        .map(|(key, field, _)| match key {
            Some(key) => format!("{key}={field}"),
            None => field.clone(),
        })
        .collect::<Vec<_>>();
    if !unused.is_empty() {
        return Err(format!(
            "Unused Arguments \"{}\" in command : {value}",
            unused.join(":")
        )
        .into());
    }
    Ok((
        rondi::RrdXportDefinition {
            name,
            file: PathBuf::from(file),
            data_source,
            consolidation: consolidation.to_ascii_uppercase(),
            step,
            start: None,
            end: None,
            reduce,
        },
        GraphDefOptions { start, end, daemon },
    ))
}

fn parse_graph_vdef(definition: &str) -> Result<GraphVdef, Box<dyn std::error::Error>> {
    let (name, expression) = definition
        .strip_prefix("VDEF:")
        .and_then(|value| value.split_once('='))
        .ok_or_else(|| format!("invalid VDEF: {definition}"))?;
    if name.is_empty() {
        return Err(format!("invalid VDEF: {definition}").into());
    }
    let (variable, specification) = expression
        .split_once(',')
        .ok_or_else(|| format!("Comma expected in VDEF definition {expression}"))?;
    if variable.is_empty() {
        return Err(format!("invalid VDEF: {definition}").into());
    }
    let (function_name, percentile) = parse_vdef_specification(name, specification)?;
    let function = rondi::VdefFunction::parse(function_name)
        .ok_or_else(|| format!("Unknown function '{function_name}' in VDEF '{name}'\n"))?;
    let needs_percentile = matches!(
        function,
        rondi::VdefFunction::Percent | rondi::VdefFunction::PercentNan
    );
    match percentile {
        None if needs_percentile => {
            return Err(
                format!("Function '{function_name}' needs parameter in VDEF '{name}'\n").into(),
            );
        }
        Some(percentile) if needs_percentile && !(0.0..=100.0).contains(&percentile) => {
            return Err(
                format!("Parameter '{percentile:.6}' out of range in VDEF '{name}'\n").into(),
            );
        }
        Some(_) if !needs_percentile => {
            return Err(format!(
                "Function '{function_name}' needs no parameter in VDEF '{name}'\n"
            )
            .into());
        }
        _ => {}
    }
    Ok(GraphVdef {
        name: name.to_owned(),
        variable: variable.to_owned(),
        function,
        percentile,
    })
}

/// Mirrors `vdef_parse`: it scans `%40[0-9.e+-],%29[A-Z]` and converts the
/// number with `rrd_strtodbl`, falling back to a bare function name. Text
/// after a parsed `number,FUNCTION` pair is not checked upstream.
fn parse_vdef_specification<'a>(
    name: &str,
    specification: &'a str,
) -> Result<(&'a str, Option<f64>), String> {
    let function_length = |text: &str| {
        text.bytes()
            .take(29)
            .take_while(u8::is_ascii_uppercase)
            .count()
    };
    let number_length = specification
        .bytes()
        .take(40)
        .take_while(|byte| matches!(byte, b'0'..=b'9' | b'.' | b'e' | b'+' | b'-'))
        .count();
    let mut function = "";
    let mut parameter = None;
    if number_length > 0 {
        if let Some(rest) = specification[number_length..].strip_prefix(',') {
            function = &rest[..function_length(rest)];
        }
        parameter = rondi::parse_rrd_number(&specification[..number_length]);
    }
    if parameter.is_none() {
        if function_length(specification) != specification.len() {
            return Err(format!(
                "Unknown function string '{specification}' in VDEF '{name}'"
            ));
        }
        function = specification;
    }
    Ok((function, parameter))
}

fn parse_graph_print(definition: &str) -> Result<GraphPrint, Box<dyn std::error::Error>> {
    let (directive, body) = definition
        .split_once(':')
        .ok_or("invalid graph print element")?;
    let mut fields = body.splitn(4, ':');
    let variable = fields.next().unwrap_or_default();
    let second = fields.next().unwrap_or_default();
    let third = fields.next();
    let fourth = fields.next();
    if variable.is_empty() || second.is_empty() {
        return Err(format!("invalid graph print element: {definition}").into());
    }
    let (consolidation, format, suffix) = if let Some(third) = third {
        if matches!(third, "strftime" | "valstrftime") {
            (None, second, Some(third))
        } else {
            let format = third;
            if !matches!(second, "AVERAGE" | "MIN" | "MAX" | "LAST") {
                return Err(format!("unsupported graph print consolidation: {second}").into());
            }
            (Some(second.to_owned()), format, fourth)
        }
    } else {
        (None, second, None)
    };
    let formatter = match suffix.unwrap_or_default() {
        "" => GraphPrintFormatter::Numeric,
        "strftime" => GraphPrintFormatter::Strftime,
        // RRDtool 1.11.0 does not implement this suffix; it validates the
        // format as a numeric printf format, so retain that observed behavior.
        "valstrftime" => GraphPrintFormatter::Numeric,
        other => return Err(format!("unsupported graph print formatter: {other}").into()),
    };
    let kind = match directive {
        "PRINT" => "print",
        "GPRINT" => "gprint",
        _ => return Err(format!("invalid graph print element: {definition}").into()),
    };
    if formatter == GraphPrintFormatter::Numeric && parse_graph_numeric_format(format).is_err() {
        return Err(format!("bad format for PRINT in \"{format}'").into());
    }
    Ok(GraphPrint {
        kind,
        variable: variable.to_owned(),
        consolidation,
        format: format.to_owned(),
        formatter,
    })
}

fn rrdtool_xport(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/xport.txt"));
        return Ok(());
    }
    let (output, _, _, _) = render_xport(args)?;
    print!("{output}");
    Ok(())
}

fn render_xport(args: &[String]) -> Result<(String, i64, i64, u64), Box<dyn std::error::Error>> {
    render_xport_with_gprints(args, None)
}

fn render_xport_with_gprints(
    args: &[String],
    graph_gprints: Option<&[(String, String)]>,
) -> Result<(String, i64, i64, u64), Box<dyn std::error::Error>> {
    let rendered = render_xport_with_graph_prints(args, graph_gprints, &[], 1000)?;
    Ok((rendered.output, rendered.start, rendered.end, rendered.step))
}

struct RenderedGraphXport {
    output: String,
    start: i64,
    end: i64,
    step: u64,
    prints: Vec<String>,
    variables: Vec<String>,
    rows: Vec<Vec<Option<f64>>>,
}

fn render_xport_with_graph_prints(
    args: &[String],
    graph_gprints: Option<&[(String, String)]>,
    graph_prints: &[GraphPrint],
    si_base: u32,
) -> Result<RenderedGraphXport, Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut start = None::<String>;
    let mut end = None::<String>;
    let mut max_rows = 400_i64;
    let mut requested_step = 0_i64;
    let mut daemon_address = None::<String>;
    let mut json = false;
    let mut show_time = false;
    let mut enum_ds = false;
    let mut definitions = Vec::<rondi::RrdXportDefinition>::new();
    let mut definition_options = Vec::<GraphDefOptions>::new();
    let mut cdefs = Vec::<rondi::RrdXportCdef>::new();
    let mut vdefs = Vec::<GraphVdef>::new();
    let mut exports = Vec::<rondi::RrdXportColumn>::new();
    let mut index = 1;
    while index < args.len() {
        let argument = &args[index];
        match argument.as_str() {
            "--start" | "-s" | "--end" | "-e" | "--maxrows" | "-m" | "--step" | "-S" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("option {argument} requires a value"))?;
                match argument.as_str() {
                    "--start" | "-s" => start = Some(value.clone()),
                    "--end" | "-e" => end = Some(value.clone()),
                    "--maxrows" | "-m" => max_rows = value.parse()?,
                    _ => requested_step = value.parse()?,
                }
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--showtime" | "-t" => {
                show_time = true;
                index += 1;
            }
            "--enumds" => {
                enum_ds = true;
                index += 1;
            }
            "--daemon" | "-d" => {
                let address = args
                    .get(index + 1)
                    .ok_or("xport --daemon requires an address")?;
                if daemon_address.is_some() {
                    return Err("You cannot specify --daemon more than once.".into());
                }
                daemon_address = Some(address.clone());
                index += 2;
            }
            option if option.starts_with("--daemon=") => {
                if daemon_address.is_some() {
                    return Err("You cannot specify --daemon more than once.".into());
                }
                daemon_address = Some(option["--daemon=".len()..].to_owned());
                index += 1;
            }
            value if value.starts_with("DEF:") => {
                let (definition, options) = parse_graph_def(value)?;
                definitions.push(definition);
                definition_options.push(options);
                index += 1;
            }
            value if value.starts_with("XPORT:") => {
                let mut fields = value[6..].splitn(2, ':');
                let name = fields.next().unwrap_or_default();
                let legend = fields.next().unwrap_or_default();
                exports.push(rondi::RrdXportColumn {
                    variable: name.to_owned(),
                    legend: legend.trim_matches('"').to_owned(),
                });
                index += 1;
            }
            value if value.starts_with("CDEF:") => {
                let (name, expression) = value[5..]
                    .split_once('=')
                    .ok_or_else(|| format!("invalid CDEF: {value}"))?;
                if name.is_empty() || expression.is_empty() {
                    return Err(format!("invalid CDEF: {value}").into());
                }
                cdefs.push(rondi::RrdXportCdef {
                    name: name.to_owned(),
                    expression: expression.to_owned(),
                });
                index += 1;
            }
            value if value.starts_with("VDEF:") => {
                vdefs.push(parse_graph_vdef(value)?);
                index += 1;
            }
            value if value.starts_with('-') => {
                return Err(format!("unsupported xport option: {value}").into());
            }
            value => return Err(format!("unexpected xport argument: {value}").into()),
        }
    }
    if exports.is_empty() {
        return Err("no XPORT found, nothing to do".into());
    }
    if definitions.is_empty() {
        return Err("xport requires at least one DEF".into());
    }
    let (start, end) = resolve_rrd_range_times(
        start.as_deref(),
        end.as_deref(),
        now - 24 * 60 * 60,
        now,
        now,
    )?;
    if start < 315_360_000 {
        return Err(format!("the first entry to fetch should be after 1980 ({start})").into());
    }
    let visible_export_count = exports.len();
    for (definition, options) in definitions.iter_mut().zip(&definition_options) {
        if options.start.is_none() && options.end.is_none() {
            continue;
        }
        let start_spec = options.start.clone().unwrap_or_else(|| start.to_string());
        let end_spec = options.end.clone().unwrap_or_else(|| end.to_string());
        let (def_start, def_end) =
            resolve_rrd_range_times(Some(&start_spec), Some(&end_spec), start, end, now)?;
        if def_start < 315_360_000 {
            return Err(
                format!("the first entry to fetch should be after 1980 ({def_start})").into(),
            );
        }
        if def_end < def_start {
            return Err(format!("start ({def_start}) should be less than end ({def_end})").into());
        }
        definition.start = Some(def_start);
        definition.end = Some(def_end);
    }
    let default_address = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty());
    let mut flushed = std::collections::HashSet::new();
    for (definition, options) in definitions.iter().zip(&definition_options) {
        let Some(address) = options.daemon.as_ref().or(default_address.as_ref()) else {
            continue;
        };
        let filename = definition.file.to_string_lossy().into_owned();
        if flushed.insert((address.clone(), filename.clone())) {
            send_rrdcached_flush(address, &filename)?;
        }
    }
    for variable in vdefs.iter().map(|vdef| vdef.variable.as_str()).chain(
        graph_prints
            .iter()
            .filter(|print| !vdefs.iter().any(|vdef| vdef.name == print.variable))
            .map(|print| print.variable.as_str()),
    ) {
        if !exports.iter().any(|export| export.variable == variable) {
            exports.push(rondi::RrdXportColumn {
                variable: variable.to_owned(),
                legend: String::new(),
            });
        }
    }
    let mut result = rondi::fetch_xport_with_cdefs(
        &definitions,
        &cdefs,
        &exports,
        start,
        end,
        requested_step.max(0) as u64,
        max_rows.max(0) as u64,
    )?;
    let mut vdef_values = std::collections::HashMap::new();
    for vdef in &vdefs {
        let values = result
            .raw_variables
            .get(&vdef.variable)
            .cloned()
            .ok_or_else(|| format!("VDEF source variable {} is unavailable", vdef.variable))?;
        let value = rondi::evaluate_vdef(
            vdef.function,
            vdef.percentile,
            &values,
            result.start,
            result.step,
        )?;
        vdef_values.insert(vdef.name.clone(), value);
    }
    let mut graph_gprints_out = graph_gprints.unwrap_or_default().to_vec();
    let mut print_values = Vec::new();
    let mut si_scale = GraphSiScale::new(si_base);
    for graph_print in graph_prints {
        let (value, timestamp) = if let Some(value) = vdef_values.get(&graph_print.variable) {
            (value.value, value.timestamp)
        } else {
            if graph_print.formatter != GraphPrintFormatter::Numeric {
                return Err(
                    "strftime graph printing requires a VDEF in this implementation".into(),
                );
            }
            let consolidation = graph_print.consolidation.as_deref().ok_or_else(|| {
                format!(
                    "PRINT/GPRINT variable {} is not a VDEF",
                    graph_print.variable
                )
            })?;
            let values = result
                .raw_variables
                .get(&graph_print.variable)
                .ok_or_else(|| format!("unknown graph print variable {}", graph_print.variable))?
                .iter()
                .copied()
                .filter(|value| value.is_finite())
                .collect::<Vec<_>>();
            let value = match consolidation {
                "AVERAGE" if values.is_empty() => rrd_nan(),
                "AVERAGE" => values.iter().sum::<f64>() / values.len() as f64,
                "MIN" => values
                    .iter()
                    .copied()
                    .reduce(f64::min)
                    .unwrap_or_else(rrd_nan),
                "MAX" => values
                    .iter()
                    .copied()
                    .reduce(f64::max)
                    .unwrap_or_else(rrd_nan),
                "LAST" => values.last().copied().unwrap_or_else(rrd_nan),
                _ => unreachable!(),
            };
            (value, None)
        };
        let formatted = format_graph_print(
            value,
            timestamp,
            graph_print.formatter,
            &graph_print.format,
            &mut si_scale,
        )?;
        if graph_print.kind == "gprint" {
            graph_gprints_out.push((String::from("gprint"), formatted));
        } else {
            print_values.push(formatted);
        }
    }
    exports.truncate(visible_export_count);
    for row in &mut result.rows {
        row.truncate(visible_export_count);
    }
    let output = if json {
        format_xport_json(
            result.start,
            result.end,
            result.step,
            &exports,
            &result.rows,
            XportFormatOptions {
                show_time,
                enum_ds: false,
                graph_gprints: Some(&graph_gprints_out),
                graph_prints: Some(&print_values),
            },
        )
    } else {
        format_xport_xml(
            result.start,
            result.end,
            result.step,
            &exports,
            &result.rows,
            XportFormatOptions {
                show_time,
                enum_ds,
                graph_gprints: Some(&graph_gprints_out),
                graph_prints: Some(&print_values),
            },
        )
    };
    Ok(RenderedGraphXport {
        output,
        start: result.start,
        end: result.end,
        step: result.step,
        prints: print_values,
        variables: exports
            .iter()
            .map(|export| export.variable.clone())
            .collect(),
        rows: result.rows,
    })
}

fn render_graph_png(
    graph: &RenderedGraphXport,
    series: &[GraphSeries],
    options: GraphPngOptions<'_>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let GraphPngOptions {
        width,
        height,
        title,
        vertical_label,
        vertical_label_angle,
        lower_limit,
        upper_limit,
        show_legend,
        rigid_scale,
        allow_shrink,
        alt_autoscale,
        alt_autoscale_min,
        alt_autoscale_max,
        only_graph,
        full_size_mode,
        force_rules_legend,
        legend_bottomup,
        colors,
        grid_dash,
        border_width,
    } = options;
    if width < 10 {
        return Err("width below 10 pixels".into());
    }
    if height < 10 {
        return Err("height below 10 pixels".into());
    }
    if width > 4096 || height > 4096 {
        return Err("PNG graph dimensions exceed the 4096 pixel safety limit".into());
    }
    let (minimum, maximum) = graph_scale_bounds(
        graph_value_bounds(graph, series),
        GraphScaleOptions {
            lower_limit,
            upper_limit,
            rigid: rigid_scale,
            allow_shrink,
            alternate: alt_autoscale,
            alternate_min: alt_autoscale_min,
            alternate_max: alt_autoscale_max,
        },
    )?;
    // RRDtool treats these as plot dimensions unless full-size mode is set.
    // only-graph suppresses every margin and label around the plot.
    let has_legend = !only_graph
        && show_legend
        && series.iter().any(|item| {
            if item.color.is_none() || item.legend.is_empty() {
                return false;
            }
            if force_rules_legend || !matches!(item.style, "hrule" | "vrule") {
                return true;
            }
            item.rule_value
                .is_some_and(|value| (minimum..=maximum).contains(&value))
                || item
                    .rule_time
                    .is_some_and(|time| (graph.start..=graph.end).contains(&time))
        });
    let horizontal_margin = if only_graph {
        0
    } else {
        81 + if vertical_label.is_some() { 16 } else { 0 }
    };
    let vertical_margin = if only_graph {
        0
    } else {
        (if has_legend { 55 } else { 39 }) + if title.is_some() { 13 } else { 0 }
    };
    let (width, height) = if full_size_mode {
        let plot_width = width
            .checked_sub(horizontal_margin)
            .ok_or("full-size width leaves no graph area")?;
        let plot_height = height
            .checked_sub(vertical_margin)
            .ok_or("full-size height leaves no graph area")?;
        if plot_width < 10 || plot_height < 10 {
            return Err("full-size dimensions leave less than 10 pixels for the graph".into());
        }
        (plot_width, plot_height)
    } else {
        (width, height)
    };
    let plot_left = if only_graph {
        0
    } else {
        51 + if vertical_label.is_some() { 16 } else { 0 }
    };
    let plot_top = if only_graph {
        0
    } else {
        15 + if title.is_some() { 13 } else { 0 }
    };
    let canvas_width = width + horizontal_margin;
    let canvas_height = height + vertical_margin;
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
    if has_legend {
        let mut x = left;
        let y = bottom + 26;
        let mut legend_series = series.iter().collect::<Vec<_>>();
        if legend_bottomup {
            legend_series.reverse();
        }
        for item in legend_series {
            let (Some(color), false) = (item.color, item.legend.is_empty()) else {
                continue;
            };
            if !force_rules_legend
                && matches!(item.style, "hrule" | "vrule")
                && !item
                    .rule_value
                    .is_some_and(|value| (minimum..=maximum).contains(&value))
                && !item
                    .rule_time
                    .is_some_and(|time| (graph.start..=graph.end).contains(&time))
            {
                continue;
            }
            let sample_x = x;
            draw_styled_line(
                &mut pixels,
                canvas_width,
                canvas_height,
                (x, y - 3),
                (x + 10, y - 3),
                DashStroke {
                    color,
                    pattern: &item.dash_pattern,
                    offset: item.dash_offset,
                    width: item.line_width,
                },
            );
            if item.style == "tick" && colors.frame[3] > 0 {
                draw_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (sample_x.saturating_sub(1), y - 5),
                    (sample_x + 11, y - 5),
                    colors.frame,
                );
                draw_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (sample_x.saturating_sub(1), y + 1),
                    (sample_x + 11, y + 1),
                    colors.frame,
                );
                draw_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (sample_x.saturating_sub(1), y - 5),
                    (sample_x.saturating_sub(1), y + 1),
                    colors.frame,
                );
                draw_line(
                    &mut pixels,
                    canvas_width,
                    canvas_height,
                    (sample_x + 11, y - 5),
                    (sample_x + 11, y + 1),
                    colors.frame,
                );
            }
            x += 13;
            draw_text(
                &mut pixels,
                canvas_width,
                canvas_height,
                x,
                y - 7,
                &ascii_text(&item.legend),
                colors.font,
            );
            x = x.saturating_add((item.legend.len() as u32).saturating_mul(8) + 14);
        }
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

fn graph_value_bounds(graph: &RenderedGraphXport, series: &[GraphSeries]) -> (f64, f64) {
    let prepared = prepare_graph_series(graph, series);
    let (mut minimum, mut maximum) = (f64::INFINITY, f64::NEG_INFINITY);
    for (item, prepared_series) in series.iter().zip(&prepared) {
        if item.skip_scale || item.style == "tick" {
            continue;
        }
        if item.rule_value.is_some() || item.rule_time.is_some() {
            continue;
        }
        for value in prepared_series.values.iter().flatten().copied() {
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
    }
    if minimum.is_finite() && maximum.is_finite() {
        (minimum, maximum)
    } else {
        (0.0, 1.0)
    }
}

struct GraphScaleOptions {
    lower_limit: Option<f64>,
    upper_limit: Option<f64>,
    rigid: bool,
    allow_shrink: bool,
    alternate: bool,
    alternate_min: bool,
    alternate_max: bool,
}

fn graph_scale_bounds(
    (data_minimum, data_maximum): (f64, f64),
    options: GraphScaleOptions,
) -> Result<(f64, f64), &'static str> {
    let GraphScaleOptions {
        lower_limit,
        upper_limit,
        rigid,
        allow_shrink,
        alternate,
        alternate_min,
        alternate_max,
    } = options;
    let mut minimum = lower_limit.map_or(data_minimum, |lower| {
        if !rigid && lower > data_minimum {
            data_minimum
        } else {
            lower
        }
    });
    let mut maximum = upper_limit.map_or(data_maximum, |upper| {
        if !rigid && upper < data_maximum {
            data_maximum
        } else {
            upper
        }
    });
    if rigid && allow_shrink {
        if lower_limit.is_some_and(|lower| lower < data_minimum) {
            minimum = data_minimum;
        }
        if upper_limit.is_some_and(|upper| upper > data_maximum) {
            maximum = data_maximum;
        }
    }
    if minimum > maximum || (lower_limit.is_some() && upper_limit.is_some() && minimum == maximum) {
        return Err("graph lower limit must be less than upper limit");
    }
    if minimum == maximum {
        let padding = minimum.abs().max(1.0) * 0.05;
        if lower_limit.is_some() {
            maximum += padding;
        } else if upper_limit.is_some() {
            minimum -= padding;
        } else {
            minimum -= padding;
            maximum += padding;
        }
    }
    let span = maximum - minimum;
    if alternate {
        minimum -= span * 0.1;
        maximum += span * 0.1;
    } else if alternate_min {
        minimum -= span * 0.1;
    } else if alternate_max {
        maximum += span * 0.1;
    }
    Ok((minimum, maximum))
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

struct XportFormatOptions<'a> {
    show_time: bool,
    enum_ds: bool,
    graph_gprints: Option<&'a [(String, String)]>,
    graph_prints: Option<&'a [String]>,
}

fn format_graph_print(
    value: f64,
    timestamp: Option<i64>,
    formatter: GraphPrintFormatter,
    format: &str,
    si_scale: &mut GraphSiScale,
) -> Result<String, Box<dyn std::error::Error>> {
    match formatter {
        GraphPrintFormatter::Numeric => format_graph_numeric(value, format, si_scale),
        GraphPrintFormatter::Strftime => match timestamp {
            Some(timestamp) => format_graph_time(timestamp, format),
            None => Ok(clean_graph_time_format(format)),
        },
    }
}

fn format_graph_time(timestamp: i64, format: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::ffi::CString;
    let timestamp = timestamp as libc::time_t;
    let mut broken_down = unsafe { std::mem::zeroed::<libc::tm>() };
    let converted = unsafe { libc::localtime_r(&timestamp, &mut broken_down) };
    if converted.is_null() {
        return Err("graph print timestamp is outside the platform time range".into());
    }
    let format = CString::new(format)?;
    let mut buffer = [0_u8; 4096];
    let length = unsafe {
        libc::strftime(
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            format.as_ptr(),
            &broken_down,
        )
    };
    if length == 0 {
        return Err("graph print strftime output is empty or exceeds its buffer".into());
    }
    Ok(String::from_utf8_lossy(&buffer[..length]).into_owned())
}

fn clean_graph_time_format(format: &str) -> String {
    let mut result = String::with_capacity(format.len());
    let mut chars = format.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            result.push(ch);
            continue;
        }
        let Some(code) = chars.next() else {
            break;
        };
        match code {
            '%' => result.push('%'),
            'n' => result.push('\n'),
            't' => result.push('\t'),
            'F' => result.push_str("----------"),
            'T' => result.push_str("--:--:--"),
            'R' => result.push_str("--:--"),
            'D' => result.push_str("--/--/--"),
            'Y' | 'G' => result.push_str("----"),
            'j' => result.push_str("---"),
            'C' | 'd' | 'g' | 'H' | 'I' | 'm' | 'M' | 'S' | 'U' | 'V' | 'W' | 'y' => {
                result.push_str("--")
            }
            'E' | 'O' => {
                chars.next();
                result.push('-');
            }
            _ => result.push('-'),
        }
    }
    result
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

fn format_graph_numeric(
    value: f64,
    format: &str,
    si_scale: &mut GraphSiScale,
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

fn format_xport_xml(
    start: i64,
    end: i64,
    step: u64,
    exports: &[rondi::RrdXportColumn],
    rows: &[Vec<Option<f64>>],
    options: XportFormatOptions<'_>,
) -> String {
    let mut output = String::new();
    writeln!(
        output,
        "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n\n<xport>\n  <meta>"
    )
    .unwrap();
    writeln!(output, "    <start>{}</start>", start + step as i64).unwrap();
    writeln!(output, "    <end>{end}</end>").unwrap();
    writeln!(output, "    <step>{step}</step>").unwrap();
    writeln!(output, "    <rows>{}</rows>", rows.len()).unwrap();
    writeln!(output, "    <columns>{}</columns>", exports.len()).unwrap();
    output.push_str("    <legend>\n");
    for export in exports {
        writeln!(output, "      <entry>{}</entry>", export.legend).unwrap();
    }
    output.push_str("    </legend>\n");
    if let Some(prints) = options.graph_prints.filter(|values| !values.is_empty()) {
        output.push_str("    <prints>\n");
        for value in prints {
            writeln!(output, "        <print>{value}</print>").unwrap();
        }
        output.push_str("    </prints>\n");
    }
    if let Some(gprints) = options.graph_gprints.filter(|values| !values.is_empty()) {
        output.push_str("    <gprints>\n");
        for (kind, value) in gprints {
            writeln!(output, "        <{kind}>{value}</{kind}>").unwrap();
        }
        output.push_str("    </gprints>\n");
    }
    output.push_str("  </meta>\n  <data>\n");
    for (row_index, row) in rows.iter().enumerate() {
        let time = start + (row_index as i64 + 1) * step as i64;
        if options.show_time {
            write!(output, "    <row><t>{time}</t>").unwrap();
        } else {
            output.push_str("    <row>");
        }
        for (column, value) in row.iter().enumerate() {
            let tag = if options.enum_ds {
                format!("v{column}")
            } else {
                "v".to_owned()
            };
            match value {
                Some(value) => {
                    write!(output, "<{tag}>{}</{tag}>", format_xport_value(*value)).unwrap();
                }
                None => write!(output, "<{tag}>NaN</{tag}>").unwrap(),
            }
        }
        output.push_str("</row>\n");
    }
    output.push_str("  </data>\n</xport>\n");
    output
}

fn normalize_json_graph_nan(value: &str) -> String {
    // RRDtool's JSON graph serializer emits NaN as `nan`, while graphv's
    // diagnostics and other text formats preserve printf's `-nan` spelling.
    value.replace("-nan", "nan")
}

fn format_xport_json(
    start: i64,
    end: i64,
    step: u64,
    exports: &[rondi::RrdXportColumn],
    rows: &[Vec<Option<f64>>],
    options: XportFormatOptions<'_>,
) -> String {
    let mut output = String::new();
    output.push_str("{ \"about\": \"RRDtool graph JSON output\",\n  \"meta\": {\n");
    writeln!(output, "    \"start\": {},", start + step as i64).unwrap();
    writeln!(output, "    \"end\": {end},").unwrap();
    writeln!(output, "    \"step\": {step},").unwrap();
    output.push_str("    \"legend\": [\n");
    for (index, export) in exports.iter().enumerate() {
        let comma = if index + 1 < exports.len() { "," } else { "" };
        writeln!(
            output,
            "      {}{comma}",
            serde_json::to_string(&export.legend).unwrap()
        )
        .unwrap();
    }
    output.push_str("          ]");
    let prints = options.graph_prints.filter(|values| !values.is_empty());
    if let Some(prints) = prints {
        output.push_str("\n    \"prints\": [\n");
        for (index, value) in prints.iter().enumerate() {
            let comma = if index + 1 < prints.len() { "," } else { "" };
            writeln!(
                output,
                "        {{ \"print\": {} }}{comma}",
                serde_json::to_string(&normalize_json_graph_nan(value)).unwrap()
            )
            .unwrap();
        }
        output.push_str("        ],");
    }
    if let Some(gprints) = options.graph_gprints.filter(|values| !values.is_empty()) {
        output.push_str("\n    ,\"gprints\": [\n");
        for (index, (kind, value)) in gprints.iter().enumerate() {
            let comma = if index + 1 < gprints.len() { "," } else { "" };
            writeln!(
                output,
                "        {{ \"{kind}\": {} }}{comma}",
                serde_json::to_string(&normalize_json_graph_nan(value)).unwrap()
            )
            .unwrap();
        }
        output.push_str("        ]\n     },\n  \"data\": [\n");
    } else {
        output.push_str("\n     },\n  \"data\": [\n");
    }
    for (row_index, row) in rows.iter().enumerate() {
        let time = start + (row_index as i64 + 1) * step as i64;
        output.push_str("    [ ");
        if options.show_time {
            write!(output, "\"{time}\",").unwrap();
        }
        for (column, value) in row.iter().enumerate() {
            if column > 0 {
                output.push_str(", ");
            }
            match value {
                Some(value) if value.is_finite() => {
                    write!(output, "{}", format_xport_value(*value)).unwrap()
                }
                _ => output.push_str("null"),
            }
        }
        let comma = if row_index + 1 < rows.len() { "," } else { "" };
        writeln!(output, " ]{comma}").unwrap();
    }
    output.push_str("  ]\n}\n");
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
    if args.len() < 3 {
        return Err("Usage: rrdtool update <file> <timestamp:value>...".into());
    }
    let filename = PathBuf::from(&args[1]);
    let mut template = None;
    let mut skip_past_updates = false;
    let mut daemon_address = None;
    let mut samples = Vec::new();
    let mut index = 2;
    while index < args.len() {
        if args[index] == "--" {
            samples.extend(args[index + 1..].iter().cloned());
            break;
        }
        match args[index].as_str() {
            "--template" | "-t" => {
                index += 1;
                template = Some(
                    args.get(index)
                        .ok_or("update --template requires a colon-separated DS list")?
                        .split(':')
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
            }
            "--daemon" | "-d" => {
                index += 1;
                let address = args
                    .get(index)
                    .ok_or("update --daemon requires an address")?;
                daemon_address = Some(address.clone());
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
            }
            "--skip-past-updates" | "-s" => skip_past_updates = true,
            option if option.starts_with('-') => {
                return Err(format!("unsupported update option: {option}").into());
            }
            sample => samples.push(sample.to_owned()),
        }
        index += 1;
    }
    let daemon_address = daemon_address.or_else(|| {
        std::env::var("RRDCACHED_ADDRESS")
            .ok()
            .filter(|address| !address.is_empty())
    });
    if verbose && daemon_address.is_some() {
        return Err("rrdtool updatev cannot be used with rrdcached".into());
    }
    if daemon_address.is_none() {
        ensure_rrd_file_exists(&filename)?;
    }
    // Local RRD writes use RRDtool's blocking per-file fcntl lock in the
    // library. A directory-wide Rondi lock incorrectly serializes unrelated
    // files and prevents the poller from updating them in parallel.
    let source_names = if let Some(template) = &template {
        let info = inspect_rrd(&args[1])?;
        let mut indices = Vec::with_capacity(template.len());
        for name in template {
            indices.push(
                info.data_sources
                    .iter()
                    .position(|source| source.name == *name)
                    .ok_or_else(|| format!("unknown DS name '{name}'"))?,
            );
        }
        Some(indices)
    } else {
        None
    };
    let verbose_source_names = if verbose {
        Some(
            inspect_rrd(filename.to_str().ok_or("RRD filename is not valid UTF-8")?)?
                .data_sources
                .into_iter()
                .map(|source| source.name)
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };
    if verbose {
        println!("return_value = 0");
    }
    let mut daemon_samples = Vec::new();
    for sample in &samples {
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
        if template.is_none() && daemon_address.is_none() {
            let source_count = inspect_rrd(&args[1])?.data_sources.len();
            if raw_values.len() > source_count {
                return Err(format!(
                    "{}: found extra data on update argument: {}",
                    filename.display(),
                    source_count + 1
                )
                .into());
            }
            if raw_values.len() < source_count {
                return Err(format!(
                    "{}: expected {source_count} data source readings (got {}) from {timestamp}",
                    filename.display(),
                    raw_values.len()
                )
                .into());
            }
        }
        let numeric_values = values
            .split(':')
            .enumerate()
            .map(|(value_index, value)| {
                parse_rrd_update_value(value).ok_or_else(|| {
                    let data_sources = inspect_rrd(&args[1])
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
                    format!(
                        "{}: Function update_pdp_prep, case DST_{source_index} - Cannot convert '{value}' to float",
                        filename.display()
                    )
                })
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
            let mut expanded = vec![None; inspect_rrd(&args[1])?.data_sources.len()];
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
            UpdateTimestamp {
                seconds: parse_rrd_time(timestamp, update_now.floor() as i64)?,
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
        if skip_past_updates && daemon_address.is_none() {
            let info = inspect_rrd(&args[1])?;
            if (update_time.seconds, update_time.microseconds)
                <= (info.last_update, info.last_update_usec)
            {
                continue;
            }
        }
        if daemon_address.is_some() {
            let encoded_values = raw_values
                .iter()
                .map(|value| value.as_deref().unwrap_or("U"))
                .collect::<Vec<_>>()
                .join(":");
            daemon_samples.push(format!("{}:{encoded_values}", update_time.format_rrd()));
            continue;
        }
        if verbose {
            let raw_values = raw_values.iter().map(Option::as_deref).collect::<Vec<_>>();
            let summaries = update_rrd_raw_values_precise_verbose(
                &filename,
                update_time.seconds,
                update_time.microseconds,
                &raw_values,
            )
            .map_err(|error| rrd_update_error(&filename, error))?;
            for summary in summaries {
                for (source_name, value) in verbose_source_names
                    .as_ref()
                    .expect("verbose source names are initialized")
                    .iter()
                    .zip(summary.values)
                {
                    println!(
                        "[{}]RRA[{}][{}]DS[{}] = {}",
                        summary.timestamp,
                        summary.consolidation,
                        summary.pdp_per_row,
                        source_name,
                        format_rrd_scientific(value)
                    );
                }
            }
        } else {
            let raw_values = raw_values.iter().map(Option::as_deref).collect::<Vec<_>>();
            update_rrd_raw_values_precise(
                &filename,
                update_time.seconds,
                update_time.microseconds,
                &raw_values,
            )
            .map_err(|error| rrd_update_error(&filename, error))?;
        }
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
        rondi::StoreError::RrdTimestamp(message) => {
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
    let mut daemon = None;
    let mut files = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "-d" | "--daemon" => {
                index += 1;
                daemon = Some(
                    args.get(index)
                        .ok_or("flushcached: --daemon requires an address")?
                        .clone(),
                );
            }
            option if option.starts_with("--daemon=") => {
                daemon = Some(option["--daemon=".len()..].to_owned());
            }
            option if option.starts_with('-') => {
                return Err(format!("flushcached: unknown option: {option}").into());
            }
            filename => files.push(filename.to_owned()),
        }
        index += 1;
    }
    if files.is_empty() {
        return Err("Usage: rrdtool flushcached [--daemon|-d <addr>] <file> [<file> ...]".into());
    }
    let daemon = daemon.or_else(|| std::env::var("RRDCACHED_ADDRESS").ok());
    let Some(daemon) = daemon.filter(|address| !address.is_empty()) else {
        return Err("Daemon address \"(null)\" unknown. Please use the \"--daemon\" option to set an address on the command line or set the \"RRDCACHED_ADDRESS\" environment variable.".into());
    };
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

// xport rows go through rrd_snprintf, which only emits '-' for values below
// zero, so -0 prints unsigned. updatev uses libc printf and keeps the sign.
fn format_xport_value(value: f64) -> String {
    format_rrd_scientific(if value == 0.0 { 0.0 } else { value })
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
    let mut filename = None::<String>;
    let mut daemon_address = None::<String>;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--daemon" | "-d" => {
                index += 1;
                daemon_address = Some(
                    args.get(index)
                        .ok_or("last --daemon requires an address")?
                        .clone(),
                );
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
            }
            option if option.starts_with('-') => {
                return Err(format!("unsupported last option: {option}").into());
            }
            path => {
                if filename.replace(path.to_owned()).is_some() {
                    return Err("rrdtool last accepts one filename".into());
                }
            }
        }
        index += 1;
    }
    let Some(filename) = filename else {
        print!("{}", rrdtool_last_help());
        return Ok(());
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
        if let Err(error) = std::fs::metadata(&filename) {
            if error.kind() == std::io::ErrorKind::NotFound {
                println!("-1");
                return Err(format!("opening '{filename}': No such file or directory").into());
            }
        }
        println!("{}", inspect_rrd(&filename)?.last_update);
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
    if args.len() != 2 {
        return Err("Usage: rrdtool lastupdate filename.rrd".into());
    }
    ensure_rrd_file_exists(std::path::Path::new(&args[1]))?;
    let info = inspect_rrd(&args[1])?;
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
    let mut archive_index = 0usize;
    let mut daemon_address = None::<String>;
    let mut filename = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--rraindex" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or("first --rraindex requires a number")?;
                archive_index = raw
                    .parse::<usize>()
                    .map_err(|_| "invalid rraindex number")?;
            }
            "--daemon" | "-d" => {
                index += 1;
                let address = args
                    .get(index)
                    .ok_or("first --daemon requires an address")?;
                daemon_address = Some(address.clone());
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
            }
            option if option.starts_with('-') => {
                return Err(format!("unsupported rrdtool first option: {option}").into());
            }
            value => {
                if filename.replace(value).is_some() {
                    return Err("rrdtool first accepts one filename".into());
                }
            }
        }
        index += 1;
    }
    let filename = filename.ok_or("Usage: rrdtool first [--rraindex number] filename.rrd")?;
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
    if let Err(error) = ensure_rrd_file_exists(std::path::Path::new(filename)) {
        if std::path::Path::new(filename)
            .metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            println!("-1");
        }
        return Err(error);
    }
    println!("{}", first_rrd_time(filename, archive_index)?);
    Ok(())
}

fn rrdtool_info(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/info.txt"));
        return Ok(());
    }
    let mut filename = None::<String>;
    let mut daemon_address = None::<String>;
    let mut noflush = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--daemon" | "-d" => {
                index += 1;
                daemon_address = Some(
                    args.get(index)
                        .ok_or("info --daemon requires an address")?
                        .clone(),
                );
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
            }
            "--noflush" | "-F" => noflush = true,
            option if option.starts_with('-') => {
                return Err(format!("unsupported info option: {option}").into());
            }
            path => {
                if filename.replace(path.to_owned()).is_some() {
                    return Err("rrdtool info accepts one filename".into());
                }
            }
        }
        index += 1;
    }
    let filename =
        filename.ok_or("Usage: rrdtool info [--daemon|-d <addr>] [--noflush|-F] file")?;
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
    let mut header = RrdDumpHeader::Dtd;
    let mut positional = Vec::new();
    let mut daemon_address = None::<String>;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--no-header" | "-n" => header = RrdDumpHeader::None,
            "--header" | "-h" => {
                index += 1;
                header = match args.get(index).map(String::as_str) {
                    Some("none") => RrdDumpHeader::None,
                    Some("dtd") => RrdDumpHeader::Dtd,
                    Some("xsd") => RrdDumpHeader::Xsd,
                    Some(value) => return Err(format!("invalid dump header: {value}").into()),
                    None => return Err("dump --header requires none, dtd, or xsd".into()),
                };
            }
            "--daemon" | "-d" => {
                index += 1;
                daemon_address = Some(
                    args.get(index)
                        .ok_or("dump --daemon requires an address")?
                        .clone(),
                );
            }
            option if option.starts_with("--daemon=") => {
                daemon_address = Some(option["--daemon=".len()..].to_owned());
            }
            value if value.starts_with('-') => {
                return Err(format!("unsupported rrdtool dump option: {value}").into());
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if !(1..=2).contains(&positional.len()) {
        return Err(
            "Usage: rrdtool dump [--header {none,xsd,dtd}] [--no-header] file.rrd [file.xml]"
                .into(),
        );
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
        std::fs::write(output, xml)?;
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
    let mut force_overwrite = false;
    let mut range_check = false;
    let mut positional = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--force-overwrite" => force_overwrite = true,
            "--range-check" => range_check = true,
            value if value.starts_with('-') && value != "-" => {
                return Err(format!("unsupported rrdtool restore option: {value}").into());
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    if positional.len() != 2 {
        return Err(
            "Usage: rrdtool restore [--range-check] [--force-overwrite] file.xml file.rrd".into(),
        );
    }
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
    let mut filename = None;
    let mut daemon_address = None::<String>;
    let mut settings = Vec::<(&'static str, String)>::new();
    let mut raw_settings = Vec::<String>::new();
    let mut index = 1;
    while index < args.len() {
        let argument = args[index].as_str();
        let (setting, inline_value) = if let Some(value) = argument.strip_prefix("--heartbeat=") {
            (Some("heartbeat"), Some(value))
        } else if let Some(value) = argument.strip_prefix("--data-source-type=") {
            (Some("type"), Some(value))
        } else if let Some(value) = argument.strip_prefix("--data-source-rename=") {
            (Some("rename"), Some(value))
        } else if let Some(value) = argument.strip_prefix("--minimum=") {
            (Some("minimum"), Some(value))
        } else if let Some(value) = argument.strip_prefix("--maximum=") {
            (Some("maximum"), Some(value))
        } else {
            match argument {
                "--heartbeat" | "-h" => (Some("heartbeat"), None),
                "--data-source-type" | "-d" => (Some("type"), None),
                "--data-source-rename" | "-r" => (Some("rename"), None),
                "--minimum" | "-i" => (Some("minimum"), None),
                "--maximum" | "-a" => (Some("maximum"), None),
                "--daemon" | "-D" => {
                    index += 1;
                    daemon_address = Some(
                        args.get(index)
                            .ok_or("tune --daemon requires an address")?
                            .clone(),
                    );
                    index += 1;
                    continue;
                }
                option if option.starts_with("--daemon=") => {
                    daemon_address = Some(option["--daemon=".len()..].to_owned());
                    index += 1;
                    continue;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unsupported rrdtool tune option: {value}").into());
                }
                value => {
                    if filename.replace(value.to_owned()).is_some() {
                        return Err("rrdtool tune expects one RRD filename".into());
                    }
                    index += 1;
                    continue;
                }
            }
        };
        if let Some(name) = setting {
            let value = if let Some(value) = inline_value {
                raw_settings.push(argument.to_owned());
                value.to_owned()
            } else {
                raw_settings.push(argument.to_owned());
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("tune --{name} requires a value"))?
                    .clone();
                raw_settings.push(value.clone());
                value
            };
            settings.push((name, value));
        }
        index += 1;
    }

    let filename = filename.ok_or("Usage: rrdtool tune file.rrd [--heartbeat DS:VALUE] [--minimum DS:VALUE] [--maximum DS:VALUE]")?;
    if settings.is_empty() {
        return Ok(());
    }
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
    for (setting, value) in settings {
        if setting == "rename" {
            let (old_name, new_name) = value
                .split_once(':')
                .ok_or("invalid arguments for data source rename")?;
            let ds_index = current_names
                .iter()
                .position(|source| source == old_name)
                .ok_or_else(|| format!("No DS called {old_name}"))?;
            changes[ds_index].new_name = Some(new_name.to_owned());
            current_names[ds_index] = new_name.to_owned();
            continue;
        }
        let (name, value) = value
            .split_once(':')
            .ok_or_else(|| format!("invalid arguments for {setting}"))?;
        let ds_index = current_names
            .iter()
            .position(|source| source == name)
            .ok_or_else(|| format!("No DS called {name}"))?;
        match setting {
            "type" => changes[ds_index].kind = Some(value.to_owned()),
            "heartbeat" => {
                let heartbeat = value
                    .parse::<i64>()
                    .map_err(|_| "invalid arguments for heartbeat")?;
                changes[ds_index].heartbeat = Some(heartbeat as u64);
            }
            "minimum" | "maximum" => {
                let bound = if value == "U" {
                    RrdTuneBound::Unbounded
                } else {
                    RrdTuneBound::Value(
                        value
                            .parse::<f64>()
                            .map_err(|_| format!("invalid arguments for {setting} ds value"))?,
                    )
                };
                if setting == "minimum" {
                    changes[ds_index].minimum = Some(bound);
                } else {
                    changes[ds_index].maximum = Some(bound);
                }
            }
            _ => unreachable!(),
        }
    }
    tune_rrd_data_sources(&filename, &changes)?;
    Ok(())
}

fn rrdtool_resize(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/resize.txt"));
        return Ok(());
    }
    if args.len() != 5 {
        return Err("Usage: rrdtool resize <file> <rraindex> GROW|SHRINK <rows>".into());
    }
    ensure_rrd_file_exists(std::path::Path::new(&args[1]))?;
    let rra_index = parse_c_integer(&args[2])?;
    let row_count = parse_c_integer(&args[4])?;
    if row_count <= 0 {
        return Err("Please grow or shrink with at least 1 row".into());
    }
    let action = match args[3].as_str() {
        "GROW" => RrdResizeAction::Grow,
        "SHRINK" => RrdResizeAction::Shrink,
        _ => return Err("Invalid action: must be GROW or SHRINK".into()),
    };
    resize_rrd_file(
        &args[1],
        std::env::current_dir()?.join("resize.rrd"),
        usize::try_from(rra_index).map_err(|_| "invalid RRA index")?,
        action,
        u64::try_from(row_count).map_err(|_| "invalid row count")?,
    )?;
    Ok(())
}

fn parse_c_integer(value: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |rest| (true, rest));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (digits, radix) = if let Some(hex) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        (hex, 16)
    } else if unsigned.len() > 1 && unsigned.starts_with('0') {
        (&unsigned[1..], 8)
    } else {
        (unsigned, 10)
    };
    let parsed = i64::from_str_radix(digits, radix)?;
    Ok(if negative { -parsed } else { parsed })
}

fn rrdtool_list(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/list.txt"));
        return Ok(());
    }
    let mut recursive = false;
    let mut directory = None;
    let mut daemon = None;
    let mut noflush = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--recursive" | "-r" => recursive = true,
            "--noflush" | "-F" => noflush = true,
            "--daemon" | "-d" => {
                daemon = Some(
                    args.get(index + 1)
                        .ok_or("rrdtool list --daemon requires an address")?
                        .clone(),
                );
            }
            value if value.starts_with("--daemon=") => daemon = Some(value[9..].to_owned()),
            value if value.starts_with('-') => {
                return Err(format!("unsupported rrdtool list option: {value}").into());
            }
            value => {
                if directory.replace(PathBuf::from(value)).is_some() {
                    return Err("rrdtool list expects one directory".into());
                }
            }
        }
        index += if matches!(args[index].as_str(), "--daemon" | "-d") {
            2
        } else {
            1
        };
    }
    let directory = directory.ok_or("Usage: rrdtool list [--recursive] <dirname>")?;
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
    match parse_rrd_scaled_duration(value, 1) {
        Ok(resolution) => Ok(resolution),
        Err(_) if is_rrd_zero_duration(value) => Err("resolution: value must be positive".into()),
        Err(error) => {
            let message = error.to_string();
            let detail = if message.contains("duration must be a positive integer") {
                "value must be (suffixed) positive number"
            } else if message.contains("duration has trailing garbage") {
                "value has trailing garbage"
            } else {
                return Err(error.into());
            };
            Err(format!("resolution: {detail}").into())
        }
    }
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

/// Implements the numeric and `now +/- duration` start forms accepted by the
/// pinned RRDtool create command. The full at-style grammar remains broader.
fn parse_rrd_time(value: &str, now: i64) -> Result<i64, Box<dyn std::error::Error>> {
    let normalized = value.replace(['_', ','], " ").to_ascii_lowercase();
    if let Some(timestamp) = parse_rrd_reference_time(&normalized, now) {
        return Ok(timestamp);
    }
    if let Ok(epoch) = normalized.trim().parse::<i64>() {
        return Ok(epoch);
    }

    // A sign begins the at-style offset. Try each occurrence because absolute
    // references may themselves contain separators (for example dates).
    for (index, character) in normalized.char_indices() {
        if !matches!(character, '+' | '-') {
            continue;
        }
        let reference = normalized[..index].trim();
        let offset = normalized[index..]
            .chars()
            .filter(|character| !character.is_ascii_whitespace() && *character != '_')
            .collect::<String>();
        let Some(reference_time) = parse_rrd_reference_time(reference, now) else {
            continue;
        };
        return apply_rrd_offsets(reference_time, &offset);
    }

    Err(format!("unsupported RRDtool time specification: {value}").into())
}

fn parse_rrd_reference_time(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if matches!(value, "now" | "n") {
        return Some(now);
    }
    if value == "epoch" {
        return Some(0);
    }
    if let Some(timestamp) = parse_rrd_absolute_date(value, now) {
        return Some(timestamp);
    }
    let (special_time, rest) = value.split_once(char::is_whitespace)?;
    let hour = match special_time {
        "midnight" => 0,
        "noon" => 12,
        "teatime" => 16,
        _ => return None,
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return set_local_hour(now, hour);
    }
    if let Some(timestamp) = parse_rrd_absolute_date(&format!("{rest} {hour:02}:00"), now) {
        return Some(timestamp);
    }
    let base = set_local_hour(now, hour)?;
    let day_delta = match rest {
        "today" => Some(0),
        "yesterday" => Some(-1),
        "tomorrow" => Some(1),
        _ => parse_weekday(rest).map(|day| {
            let raw = libc::time_t::try_from(base).unwrap_or_default();
            let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
            if unsafe { libc::localtime_r(&raw, &mut local) }.is_null() {
                return 0;
            }
            day - local.tm_wday
        }),
    }?;
    shift_local_calendar(base, CalendarUnit::Days, i64::from(day_delta)).ok()
}

fn parse_weekday(value: &str) -> Option<i32> {
    Some(match value {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

#[cfg(unix)]
fn set_local_hour(timestamp: i64, hour: i32) -> Option<i64> {
    let mut raw = libc::time_t::try_from(timestamp).ok()?;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&raw, &mut local) }.is_null() {
        return None;
    }
    local.tm_hour = hour;
    local.tm_min = 0;
    local.tm_sec = 0;
    local.tm_isdst = -1;
    raw = unsafe { libc::mktime(&mut local) };
    if raw == -1 {
        return None;
    }
    #[cfg(target_pointer_width = "64")]
    {
        Some(raw)
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        Some(raw as i64)
    }
}

#[cfg(not(unix))]
fn set_local_hour(timestamp: i64, hour: i32) -> Option<i64> {
    Some(timestamp - timestamp.rem_euclid(86_400) + i64::from(hour) * 3_600)
}

#[derive(Clone, Copy)]
enum CalendarUnit {
    Days,
    Months,
    Years,
}

#[cfg(unix)]
fn shift_local_calendar(
    timestamp: i64,
    unit: CalendarUnit,
    amount: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    let mut timestamp = libc::time_t::try_from(timestamp)?;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
        return Err("RRDtool local time is outside the supported range".into());
    }
    let amount = i32::try_from(amount)?;
    match unit {
        CalendarUnit::Days => {
            local.tm_mday = local.tm_mday.checked_add(amount).ok_or("date overflow")?
        }
        CalendarUnit::Months => {
            local.tm_mon = local.tm_mon.checked_add(amount).ok_or("date overflow")?
        }
        CalendarUnit::Years => {
            local.tm_year = local.tm_year.checked_add(amount).ok_or("date overflow")?
        }
    }
    local.tm_isdst = -1;
    timestamp = unsafe { libc::mktime(&mut local) };
    if timestamp == -1 {
        return Err("RRDtool calendar time is outside the supported range".into());
    }
    #[cfg(target_pointer_width = "64")]
    {
        Ok(timestamp)
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        Ok(timestamp as i64)
    }
}

#[cfg(not(unix))]
fn shift_local_calendar(
    timestamp: i64,
    unit: CalendarUnit,
    amount: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    let seconds = match unit {
        CalendarUnit::Days => 86_400,
        CalendarUnit::Months => 31 * 86_400,
        CalendarUnit::Years => 366 * 86_400,
    };
    timestamp
        .checked_add(amount.checked_mul(seconds).ok_or("date overflow")?)
        .ok_or_else(|| "date overflow".into())
}

fn apply_rrd_offsets(reference: i64, offsets: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let bytes = offsets.as_bytes();
    let mut index = 0;
    let mut sign = 1_i64;
    let mut previous_unit = "";
    let mut timestamp = reference;
    let mut saw_offset = false;
    while index < bytes.len() {
        if bytes[index] == b'+' || bytes[index] == b'-' {
            sign = if bytes[index] == b'+' { 1 } else { -1 };
            index += 1;
        } else if !saw_offset {
            return Err(format!("invalid RRDtool time offset: {offsets}").into());
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(format!("invalid RRDtool time offset: {offsets}").into());
        }
        let amount = offsets[start..index].parse::<i64>()?;
        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let mut unit = &offsets[unit_start..index];
        if unit.is_empty() {
            unit = "s";
        }
        if unit == "m" {
            unit = match previous_unit {
                "d" | "day" | "days" | "w" | "wk" | "week" | "weeks" | "mon" | "month"
                | "months" | "y" | "yr" | "year" | "years" => "mon",
                "s" | "sec" | "second" | "seconds" | "min" | "minute" | "minutes" | "h" | "hr"
                | "hour" | "hours" => "min",
                _ if amount < 6 => "mon",
                _ => "min",
            };
        }
        let signed = amount
            .checked_mul(sign)
            .ok_or("RRDtool time offset overflows")?;
        match unit {
            "s" | "sec" | "second" | "seconds" => {
                timestamp = timestamp
                    .checked_add(signed)
                    .ok_or("time offset overflows")?;
            }
            "m" | "min" | "minute" | "minutes" => {
                timestamp = timestamp
                    .checked_add(signed.checked_mul(60).ok_or("time offset overflows")?)
                    .ok_or("time offset overflows")?;
            }
            "h" | "hr" | "hour" | "hours" => {
                timestamp = timestamp
                    .checked_add(signed.checked_mul(3_600).ok_or("time offset overflows")?)
                    .ok_or("time offset overflows")?;
            }
            "d" | "day" | "days" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Days, signed)?;
            }
            "w" | "wk" | "week" | "weeks" => {
                timestamp = shift_local_calendar(
                    timestamp,
                    CalendarUnit::Days,
                    signed.checked_mul(7).ok_or("date overflow")?,
                )?;
            }
            "mon" | "month" | "months" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Months, signed)?;
            }
            "y" | "yr" | "year" | "years" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Years, signed)?;
            }
            _ => return Err(format!("unsupported RRDtool time unit: {unit}").into()),
        }
        previous_unit = unit;
        saw_offset = true;
    }
    if !saw_offset {
        return Err(format!("invalid RRDtool time offset: {offsets}").into());
    }
    Ok(timestamp)
}

/// RRDtool's at-style parser uses local calendar time plus `mktime`'s DST
/// normalization. Cover common fully specified calendar forms while retaining
/// that system behavior. Month/day names and broader at-style references remain
/// outside this bounded implementation.
#[cfg(unix)]
fn parse_rrd_absolute_date(value: &str, now: i64) -> Option<i64> {
    use std::ffi::CString;

    let normalized = value.replace(['_', ','], " ");
    let input = CString::new(normalized).ok()?;
    const FORMATS: [&[u8]; 16] = [
        b"%Y-%m-%d %H:%M:%S\0",
        b"%Y-%m-%d %H:%M\0",
        b"%Y-%m-%dT%H:%M:%S\0",
        b"%m/%d/%Y %H:%M:%S\0",
        b"%m/%d/%Y %H:%M\0",
        b"%d.%m.%Y %H:%M:%S\0",
        b"%d.%m.%Y %H:%M\0",
        b"%H:%M:%S %Y-%m-%d\0",
        b"%H:%M %Y-%m-%d\0",
        b"%b %d %Y %H:%M:%S\0",
        b"%b %d %Y %H:%M\0",
        b"%B %d %Y %H:%M:%S\0",
        b"%B %d %Y %H:%M\0",
        b"%H:%M %b %d %Y\0",
        b"%H:%M:%S %B %d %Y\0",
        b"%I:%M %p %b %d %Y\0",
    ];
    for format in FORMATS {
        // strptime and mktime use the same local-time and DST rules as RRDtool's
        // rrd_parsetime + mktime path for these calendar forms.
        let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
        let end = unsafe { libc::strptime(input.as_ptr(), format.as_ptr().cast(), &mut tm) };
        if end.is_null() || unsafe { *end } != 0 {
            continue;
        }
        tm.tm_isdst = -1;
        let timestamp = unsafe { libc::mktime(&mut tm) };
        if timestamp != -1 {
            return Some(timestamp as i64);
        }
    }

    // RRDtool preserves the current local time-of-day when a calendar date is
    // given without an explicit time component.
    const DATE_ONLY_FORMATS: [(&[u8], bool); 5] = [
        (b"%B %d %Y\0", true),
        (b"%b %d %Y\0", true),
        (b"%m/%d/%Y\0", false),
        (b"%d.%m.%Y\0", false),
        (b"%Y%m%d\0", false),
    ];
    let mut raw_now = libc::time_t::try_from(now).ok()?;
    for (format, preserve_time) in DATE_ONLY_FORMATS {
        let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
        if unsafe { libc::localtime_r(&raw_now, &mut tm) }.is_null() {
            return None;
        }
        let end = unsafe { libc::strptime(input.as_ptr(), format.as_ptr().cast(), &mut tm) };
        if end.is_null() || unsafe { *end } != 0 {
            continue;
        }
        if !preserve_time {
            tm.tm_hour = 0;
            tm.tm_min = 0;
            tm.tm_sec = 0;
        }
        tm.tm_isdst = -1;
        raw_now = unsafe { libc::mktime(&mut tm) };
        if raw_now != -1 {
            #[cfg(target_pointer_width = "64")]
            return Some(raw_now);
            #[cfg(not(target_pointer_width = "64"))]
            return Some(raw_now as i64);
        }
    }
    None
}

#[cfg(not(unix))]
fn parse_rrd_absolute_date(_value: &str, _now: i64) -> Option<i64> {
    None
}

#[derive(Debug, PartialEq, Eq)]
enum RangeTimeSpec {
    Absolute(i64),
    RelativeToStart(String),
    RelativeToEnd(String),
}

fn parse_range_time_spec(
    value: &str,
    now: i64,
) -> Result<RangeTimeSpec, Box<dyn std::error::Error>> {
    let normalized = value.trim().to_ascii_lowercase();
    for (reference, kind) in [("start", 0_u8), ("end", 1_u8), ("s", 0_u8), ("e", 1_u8)] {
        if let Some(offset) = normalized.strip_prefix(reference)
            && offset.starts_with(['+', '-'])
        {
            if kind == 0 {
                return Ok(RangeTimeSpec::RelativeToStart(offset.to_owned()));
            }
            return Ok(RangeTimeSpec::RelativeToEnd(offset.to_owned()));
        }
    }
    if let Ok(timestamp) = normalized.parse::<i64>() {
        return Ok(RangeTimeSpec::Absolute(if timestamp > 0 {
            timestamp
        } else {
            now.checked_add(timestamp)
                .ok_or("relative fetch time overflows")?
        }));
    }
    Ok(RangeTimeSpec::Absolute(parse_rrd_time(&normalized, now)?))
}

/// Resolve RRDtool's pair-dependent `start`/`end` references. In RRDtool,
/// `start-1d` is based on the resolved end and `end+1d` on the resolved start;
/// references to the same endpoint or mutually relative endpoints are invalid.
fn resolve_rrd_range_times(
    start_spec: Option<&str>,
    end_spec: Option<&str>,
    default_start: i64,
    default_end: i64,
    now: i64,
) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    use RangeTimeSpec::{Absolute, RelativeToEnd, RelativeToStart};
    let start_spec = match start_spec {
        Some(value) => parse_range_time_spec(value, now)?,
        None if end_spec.is_none() => Absolute(default_start),
        None => RelativeToEnd(String::from("-24h")),
    };
    let end_spec = end_spec
        .map(|value| parse_range_time_spec(value, now))
        .transpose()?
        .unwrap_or(Absolute(default_end));
    match (start_spec, end_spec) {
        (Absolute(start), Absolute(end)) => Ok((start, end)),
        (RelativeToEnd(offset), Absolute(end)) => Ok((apply_rrd_offsets(end, &offset)?, end)),
        (Absolute(start), RelativeToStart(offset)) => {
            Ok((start, apply_rrd_offsets(start, &offset)?))
        }
        (RelativeToStart(_), _) => {
            Err("the start time cannot be specified relative to itself".into())
        }
        (_, RelativeToEnd(_)) => Err("the end time cannot be specified relative to itself".into()),
        (RelativeToEnd(_), RelativeToStart(_)) => {
            Err("the start and end times cannot be specified relative to each other".into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UpdateTimestamp {
    seconds: i64,
    microseconds: u64,
}

impl UpdateTimestamp {
    fn format_rrd(self) -> String {
        if self.microseconds == 0 {
            self.seconds.to_string()
        } else {
            format!("{}.{:06}", self.seconds, self.microseconds)
        }
    }
}

/// RRDtool accepts `N` for the current time and interprets negative numeric
/// update times as offsets from the current time.
fn parse_rrd_update_timestamp(
    value: &str,
    now: f64,
) -> Result<UpdateTimestamp, Box<dyn std::error::Error>> {
    let timestamp = if value == "N" {
        now
    } else {
        let timestamp = rondi::parse_rrd_number(value)
            .filter(|timestamp| timestamp.is_finite())
            .ok_or("invalid numeric timestamp")?;
        if timestamp < 0.0 {
            now + timestamp
        } else {
            timestamp
        }
    };
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp >= i64::MAX as f64 {
        return Err("update timestamp is outside the supported range".into());
    }
    let mut seconds = timestamp.floor() as i64;
    let mut microseconds = ((timestamp - seconds as f64) * 1_000_000.0) as u64;
    if microseconds >= 1_000_000 {
        seconds = seconds
            .checked_add(1)
            .ok_or("update timestamp is outside the supported range")?;
        microseconds = 0;
    }
    Ok(UpdateTimestamp {
        seconds,
        microseconds,
    })
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
    use super::{XportFormatOptions, format_xport_xml};
    use rondi::RrdXportColumn;

    // rrd_xport.c writes these text nodes unescaped, so the document is not
    // well-formed XML when they contain markup characters.
    #[test]
    fn writes_graph_xport_text_nodes_verbatim() {
        let exports = [RrdXportColumn {
            variable: "rate".to_owned(),
            legend: "load & <peak>".to_owned(),
        }];
        let prints = ["value > 1 & < 2".to_owned()];
        let gprints = [("gprint".to_owned(), "<ok & done>".to_owned())];
        let xml = format_xport_xml(
            100,
            110,
            10,
            &exports,
            &[vec![Some(2.0)]],
            XportFormatOptions {
                show_time: false,
                enum_ds: false,
                graph_gprints: Some(&gprints),
                graph_prints: Some(&prints),
            },
        );

        assert!(xml.contains("<entry>load & <peak></entry>"));
        assert!(xml.contains("<print>value > 1 & < 2</print>"));
        assert!(xml.contains("<gprint><ok & done></gprint>"));
    }
}

#[cfg(test)]
mod time_spec_tests {
    use super::{
        RangeTimeSpec, UpdateTimestamp, parse_range_time_spec, parse_rrd_time,
        parse_rrd_update_timestamp, resolve_rrd_range_times,
    };

    #[test]
    fn parses_rrd_create_numeric_and_common_relative_times() {
        assert_eq!(
            parse_rrd_time("1000000000", 2_000_000_000).unwrap(),
            1_000_000_000
        );
        assert_eq!(parse_rrd_time("now", 2_000_000_000).unwrap(), 2_000_000_000);
        assert_eq!(
            parse_rrd_time("now - 1 hour", 2_000_000_000).unwrap(),
            1_999_996_400
        );
        assert_eq!(
            parse_rrd_time("now+2d", 2_000_000_000).unwrap(),
            2_000_172_800
        );
    }

    #[test]
    fn rejects_unsupported_or_overflowing_rrd_time_forms() {
        assert!(parse_rrd_time("end-1d", 2_000_000_000).is_err());
        assert!(parse_rrd_time("now+999999999999999999999d", 2_000_000_000).is_err());
    }

    #[test]
    fn resolves_range_times_relative_to_the_other_endpoint() {
        assert_eq!(
            resolve_rrd_range_times(
                Some("end-30s"),
                Some("2000000000"),
                1_999_913_600,
                2_000_000_000,
                2_000_000_000,
            )
            .unwrap(),
            (1_999_999_970, 2_000_000_000)
        );
        assert_eq!(
            resolve_rrd_range_times(
                Some("2000000000"),
                Some("start+30s"),
                1_999_913_600,
                2_000_000_000,
                2_000_000_000,
            )
            .unwrap(),
            (2_000_000_000, 2_000_000_030)
        );
    }

    #[test]
    fn rejects_self_and_mutually_relative_range_times() {
        let now = 2_000_000_000;
        assert!(resolve_rrd_range_times(Some("start-1h"), None, now - 86_400, now, now).is_err());
        assert!(resolve_rrd_range_times(None, Some("end+1h"), now - 86_400, now, now).is_err());
        assert!(
            resolve_rrd_range_times(Some("end-1h"), Some("start+1h"), now - 86_400, now, now)
                .is_err()
        );
    }

    #[test]
    fn fetch_negative_and_zero_times_are_relative_to_now() {
        assert_eq!(
            parse_range_time_spec("1000000000", 2_000_000_000).unwrap(),
            RangeTimeSpec::Absolute(1_000_000_000)
        );
        assert_eq!(
            parse_range_time_spec("-3600", 2_000_000_000).unwrap(),
            RangeTimeSpec::Absolute(1_999_996_400)
        );
    }

    #[test]
    fn update_n_and_negative_times_are_relative_to_current_time() {
        assert_eq!(
            parse_rrd_update_timestamp("N", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 2_000_000_000,
                microseconds: 750_000
            }
        );
        assert_eq!(
            parse_rrd_update_timestamp("-60", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 1_999_999_940,
                microseconds: 750_000
            }
        );
        assert_eq!(
            parse_rrd_update_timestamp("10.9", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 10,
                microseconds: 900_000
            }
        );
        assert!(parse_rrd_update_timestamp("NaN", 2_000_000_000.75).is_err());
    }
}

#[cfg(test)]
mod graph_stroke_tests {
    use super::{
        GraphPngOptions, GraphScaleOptions, RenderedGraphXport, draw_line_with_width,
        draw_styled_line, draw_vertical_text, graph_scale_bounds, graph_value_bounds,
        interpolate_graph_color, parse_graph_hrule, parse_graph_series, parse_graph_tick,
        parse_graph_vrule, prepare_graph_series, render_graph_png, set_pixel, tick_mark_range,
    };

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
        for invalid in ["0", "-1,2", "1,2,3", "NaN", "inf"] {
            assert!(
                parse_graph_series(&format!("LINE:x#ffffff:x:dashes={invalid}")).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn dash_offset_is_parsed_and_must_be_finite() {
        let parsed = parse_graph_series("LINE:x#ffffff:x:dashes=2,3:dash-offset=-1.5")
            .unwrap()
            .0;
        assert_eq!(parsed.dash_pattern, [2.0, 3.0]);
        assert_eq!(parsed.dash_offset, -1.5);
        assert!(parse_graph_series("LINE:x#ffffff:x:dash-offset=NaN").is_err());
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
        let rule = parse_graph_hrule("HRULE:4#ff0000:limit:dashes=2,4:dash-offset=1").unwrap();
        assert_eq!(rule.legend, "limit");
        assert_eq!(rule.dash_pattern, [2.0, 4.0]);
        assert_eq!(rule.dash_offset, 1.0);
    }

    #[test]
    fn vertical_rule_accepts_default_dash_options() {
        let rule =
            parse_graph_vrule("VRULE:1000000030#00ff00:deploy:dashes", 1_000_000_000).unwrap();
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
            output: String::new(),
            start: 0,
            end: 1,
            step: 1,
            prints: Vec::new(),
            variables: vec!["base".into(), "upper".into()],
            rows: vec![vec![Some(3.0), Some(2.0)]],
        };
        let prepared = prepare_graph_series(&graph, &[hidden.clone(), visible]);
        assert_eq!(prepared[1].values, [Some(5.0)]);
        assert_eq!(graph_value_bounds(&graph, &[hidden]), (3.0, 3.0));
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
    fn skipscale_series_is_drawn_but_does_not_expand_automatic_bounds() {
        let in_scale = parse_graph_series("LINE:normal#ffffff:normal").unwrap().0;
        let excluded = parse_graph_series("LINE:spike#ffffff:spike:skipscale")
            .unwrap()
            .0;
        assert_eq!(excluded.legend, "spike");
        assert!(excluded.skip_scale);

        let graph = RenderedGraphXport {
            output: String::new(),
            start: 0,
            end: 10,
            step: 1,
            prints: Vec::new(),
            variables: vec![String::from("normal"), String::from("spike")],
            rows: vec![
                vec![Some(2.0), Some(1000.0)],
                vec![Some(4.0), Some(-1000.0)],
            ],
        };
        assert_eq!(
            graph_value_bounds(&graph, &[in_scale, excluded]),
            (2.0, 4.0)
        );
    }

    #[test]
    fn rigid_limits_hold_until_allow_shrink_is_selected() {
        let limits = || GraphScaleOptions {
            lower_limit: Some(0.0),
            upper_limit: Some(10.0),
            rigid: true,
            allow_shrink: false,
            alternate: false,
            alternate_min: false,
            alternate_max: false,
        };
        assert_eq!(
            graph_scale_bounds((2.0, 8.0), limits()).unwrap(),
            (0.0, 10.0)
        );
        let shrunk = GraphScaleOptions {
            allow_shrink: true,
            ..limits()
        };
        assert_eq!(graph_scale_bounds((2.0, 8.0), shrunk).unwrap(), (2.0, 8.0));
    }

    #[test]
    fn flexible_limits_expand_to_include_outlying_data() {
        let options = GraphScaleOptions {
            lower_limit: Some(3.0),
            upper_limit: Some(7.0),
            rigid: false,
            allow_shrink: false,
            alternate: false,
            alternate_min: false,
            alternate_max: false,
        };
        assert_eq!(graph_scale_bounds((2.0, 8.0), options).unwrap(), (2.0, 8.0));
    }

    #[test]
    fn alternate_autoscale_modes_expand_the_requested_side() {
        let opts = |alternate, alternate_min, alternate_max| GraphScaleOptions {
            lower_limit: None,
            upper_limit: None,
            rigid: false,
            allow_shrink: false,
            alternate,
            alternate_min,
            alternate_max,
        };
        assert_eq!(
            graph_scale_bounds((2.0, 8.0), opts(true, false, false)).unwrap(),
            (1.4, 8.6)
        );
        assert_eq!(
            graph_scale_bounds((2.0, 8.0), opts(false, true, false)).unwrap(),
            (1.4, 8.0)
        );
        assert_eq!(
            graph_scale_bounds((2.0, 8.0), opts(false, false, true)).unwrap(),
            (2.0, 8.6)
        );
    }

    #[test]
    fn invalid_and_collapsed_scale_limits_are_rejected() {
        let invalid = GraphScaleOptions {
            lower_limit: Some(9.0),
            upper_limit: Some(4.0),
            rigid: true,
            allow_shrink: false,
            alternate: false,
            alternate_min: false,
            alternate_max: false,
        };
        assert!(graph_scale_bounds((2.0, 8.0), invalid).is_err());
        let collapsed = GraphScaleOptions {
            lower_limit: Some(4.0),
            upper_limit: Some(4.0),
            rigid: true,
            allow_shrink: false,
            alternate: false,
            alternate_min: false,
            alternate_max: false,
        };
        assert!(graph_scale_bounds((2.0, 8.0), collapsed).is_err());
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
        assert!(parse_graph_series("AREA:load#ff0000#00ff00:Load:gradheight=NaN").is_err());
        // RRDtool's AREA parser does not enable PARSE_DASHES, so these tokens
        // remain part of the legend instead of changing the fill style.
        let area = parse_graph_series("AREA:load#ff0000:Load:dashes")
            .unwrap()
            .0;
        assert_eq!(area.legend, "Load:dashes");
        assert!(area.dash_pattern.is_empty());
    }

    #[test]
    fn tick_directive_uses_source_default_fraction_and_vertical_direction() {
        let (default_tick, _) = parse_graph_tick("TICK:events#ff000080").unwrap();
        assert_eq!(default_tick.tick_fraction, 0.1);
        assert_eq!(default_tick.legend, "");
        assert!(default_tick.skip_scale);

        let (labeled_tick, legend) = parse_graph_tick("TICK:events#ff0000:-0.25:Events").unwrap();
        assert_eq!(labeled_tick.tick_fraction, -0.25);
        assert_eq!(legend, "Events");
        assert_eq!(tick_mark_range(10, 110, 25, -0.25), (10, 35));
        assert_eq!(tick_mark_range(10, 110, 25, 0.25), (85, 110));
    }

    #[test]
    fn zero_fraction_tick_is_invisible_like_rrdtool() {
        let graph = RenderedGraphXport {
            output: String::new(),
            start: 0,
            end: 10,
            step: 1,
            prints: Vec::new(),
            variables: vec!["events".to_owned()],
            rows: vec![vec![Some(1.0)]; 11],
        };
        let (zero, _) = parse_graph_tick("TICK:events#ff0000:0").unwrap();
        let (visible, _) = parse_graph_tick("TICK:events#ff0000:0.5").unwrap();
        let render = |tick: super::GraphSeries| {
            let png = render_graph_png(
                &graph,
                &[tick],
                GraphPngOptions {
                    width: 40,
                    height: 30,
                    title: None,
                    vertical_label: None,
                    vertical_label_angle: 90.0,
                    lower_limit: None,
                    upper_limit: None,
                    show_legend: false,
                    rigid_scale: false,
                    allow_shrink: false,
                    alt_autoscale: false,
                    alt_autoscale_min: false,
                    alt_autoscale_max: false,
                    only_graph: false,
                    full_size_mode: false,
                    force_rules_legend: false,
                    legend_bottomup: false,
                    colors: super::GraphColors::default(),
                    grid_dash: Vec::new(),
                    border_width: 0,
                },
            )
            .unwrap();
            let mut reader = png::Decoder::new(std::io::Cursor::new(png))
                .read_info()
                .unwrap();
            let mut bytes = vec![0; reader.output_buffer_size()];
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
        let rule = parse_graph_hrule("HRULE:20#ff000080:Threshold").unwrap();
        assert_eq!(rule.rule_value, Some(20.0));
        assert_eq!(rule.legend, "Threshold");
        assert_eq!(rule.color, Some([255, 0, 0, 128]));
        assert!(parse_graph_hrule("HRULE:NaN#ff0000").is_err());

        let graph = RenderedGraphXport {
            output: String::new(),
            start: 0,
            end: 10,
            step: 1,
            prints: Vec::new(),
            variables: vec![String::from("load")],
            rows: vec![vec![Some(2.0)], vec![Some(4.0)]],
        };
        let load = parse_graph_series("LINE:load#ffffff:Load").unwrap().0;
        assert_eq!(graph_value_bounds(&graph, &[load, rule]), (2.0, 4.0));
    }

    #[test]
    fn vrule_accepts_rrd_timestamps_and_does_not_affect_data_scale() {
        let rule =
            parse_graph_vrule("VRULE:1000000030#00ff0080:Maintenance", 1_000_000_000).unwrap();
        assert_eq!(rule.rule_time, Some(1_000_000_030));
        assert_eq!(rule.legend, "Maintenance");
        assert_eq!(rule.color, Some([0, 255, 0, 128]));
        assert_eq!(
            parse_graph_vrule("VRULE:now#00ff00", 1_000_000_000)
                .unwrap()
                .rule_time,
            Some(1_000_000_000)
        );
    }

    #[test]
    fn stacked_series_adds_previous_values_and_carries_unknown_as_baseline() {
        let first = parse_graph_series("AREA:base#ff0000:Base").unwrap().0;
        let second = parse_graph_series("AREA:extra#0000ff:Extra:STACK")
            .unwrap()
            .0;
        assert!(second.stack);
        let graph = RenderedGraphXport {
            output: String::new(),
            start: 0,
            end: 30,
            step: 10,
            prints: Vec::new(),
            variables: vec![String::from("base"), String::from("extra")],
            rows: vec![
                vec![Some(2.0), Some(3.0)],
                vec![Some(4.0), None],
                vec![None, Some(1.0)],
            ],
        };
        let prepared = prepare_graph_series(&graph, &[first, second]);
        assert_eq!(prepared[0].values, vec![Some(2.0), Some(4.0), None]);
        assert_eq!(prepared[1].baseline, vec![Some(2.0), Some(4.0), Some(0.0)]);
        assert_eq!(prepared[1].values, vec![Some(5.0), Some(4.0), Some(1.0)]);
        assert_eq!(
            graph_value_bounds(
                &graph,
                &[
                    parse_graph_series("AREA:base#ff0000:Base").unwrap().0,
                    parse_graph_series("AREA:extra#0000ff:Extra:STACK")
                        .unwrap()
                        .0,
                ]
            ),
            (1.0, 5.0)
        );
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
}
