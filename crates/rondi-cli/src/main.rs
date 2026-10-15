/// C library conversions RRDtool's command parsers call, either through
/// libc itself or as ports of RRDtool's own helpers in rrd_strtod.c.
mod cparse;
/// The `rrdtool` front end: command selection, option scanning, error
/// reporting, pipe mode and the single-command programs.
mod frontend;
/// Option parsing for `graph`, `graphv` and `xport`, ported from
/// `rrd_graph_options` (rrd_graph.c:5042) and `rrd_xport` (rrd_xport.c:76).
/// Options that only affect Cairo/Pango rendering are validated exactly as
/// upstream validates them and then left unused.
mod graph_options;
/// Text of graph PRINT and GPRINT items, written from
/// docs/compatibility/format-strings.md.
mod print_format;
/// The `rrdcached` personality: reads the daemon's options and starts it.
mod rrdcached;

use frontend::{OptionScanner, OptionSpec, RawMessage, last_os_errno, system_error_text};
use print_format::{PrintOutcome, local_tm, printf_double as c_printf_double};

use clap::{Parser, Subcommand};
use rondi::time::{
    UpdateTimestamp, parse_time_spec, parse_update_timestamp, resolve_start_end, resolve_window,
};
use rondi::{
    DEFAULT_IDEMPOTENCY_WINDOW, DEFAULT_MAX_ROWS, DatabaseConfig, RrdDataSourceTune, RrdDumpHeader,
    RrdResizeAction, RrdTuneBound, Store, StoreOptions, Update, dump_rrd_file_with_header,
    fetch_rrd_rows, first_rrd_time, parse_rrd_scaled_duration, resize_rrd_file, restore_rrd_file,
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

fn invoked_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|value| {
            PathBuf::from(value)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// argv after `skip` entries, with invalid UTF-8 replaced by U+FFFD like the
/// rrdtool path does, so a path option cannot panic `std::env::args`.
fn lossy_args(skip: usize) -> Vec<String> {
    std::env::args_os()
        .skip(skip)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--rrdproxy-launcher"))
        && invoked_name() == "rrdcached"
    {
        // rrdcached binds its sockets and drops privileges before any runtime
        // thread exists, so it starts its own runtime afterwards.
        return rrdcached::rrdcached_mode(&lossy_args(1));
    }
    let native = !matches!(
        invoked_name().as_str(),
        "rrdtool"
            | "rrdupdate"
            | "rrdcreate"
            | "rrdinfo"
            | "rrdtool-proxy"
            | "rrdtool-proxy.php"
            | "rrdproxy"
    ) && std::env::args_os().nth(1).as_deref()
        != Some(std::ffi::OsStr::new("--rrdproxy-launcher"));
    let args = native.then(Args::parse);
    let prepared = match args.as_ref() {
        Some(args) if args.socket.is_none() => match &args.command {
            Command::Server {
                listen,
                queue_capacity,
            } => Some(rondi_server::prepare_server(rondi_server::ServerConfig {
                root: args.root.clone(),
                socket: listen.clone(),
                queue_capacity: *queue_capacity,
                store: StoreOptions {
                    max_rows: args.max_rows,
                    idempotency_window: std::time::Duration::from_secs(args.idempotency_window),
                },
            })?),
            _ => None,
        },
        _ => None,
    };
    async_main(args, prepared)
}

#[tokio::main]
async fn async_main(
    parsed: Option<Args>,
    prepared: Option<rondi_server::PreparedServer>,
) -> Result<(), Box<dyn std::error::Error>> {
    let invoked_as = invoked_name();
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--rrdproxy-launcher")) {
        return rrdproxy_mode(&lossy_args(2));
    }
    if invoked_as == "rrdtool" {
        initialize_rrdtool_locale();
        // RRDtool passes argv bytes through; Rust strings cannot hold
        // invalid UTF-8, so such bytes become U+FFFD instead of panicking.
        let args = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let status = frontend::run_rrdtool(&args);
        let _ = std::io::stdout().flush();
        std::process::exit(status);
    }
    if let Some(alias) = frontend::Alias::from_program(&invoked_as) {
        let args = std::env::args_os()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let status = frontend::run_alias(alias, &args);
        let _ = std::io::stdout().flush();
        std::process::exit(status);
    }
    if invoked_as == "rrdtool-proxy"
        || invoked_as == "rrdtool-proxy.php"
        || invoked_as == "rrdproxy"
    {
        return rrdproxy_mode(&lossy_args(1));
    }
    if let Some(prepared) = prepared {
        return rondi_server::run(prepared).await;
    }
    let args = parsed.unwrap_or_else(Args::parse);
    if let Some(socket) = args.socket {
        return server_mode(socket, args.command).await;
    }
    let options = StoreOptions {
        max_rows: args.max_rows,
        idempotency_window: std::time::Duration::from_secs(args.idempotency_window),
    };
    let store = Store::open_with(&args.root, options)?;
    match args.command {
        Command::Server { .. } => {
            return Err("server must be prepared before starting runtime threads".into());
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
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("start", Some('b'), 'b'),
        OptionSpec::value("step", Some('s'), 's'),
        OptionSpec::value("daemon", Some('d'), 'd'),
        OptionSpec::value("source", Some('r'), 'r'),
        OptionSpec::value("template", Some('t'), 't'),
        OptionSpec::flag("no-overwrite", Some('O'), 'O'),
    ];
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        let value = found.text().to_owned();
        match found.key {
            'd' => daemon_address = Some(value),
            'b' => {
                start = parse_time_spec(&value, now)
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
            's' => {
                step = parse_scaled_duration_option(&value, "step size")?;
                step_was_set = true;
            }
            'O' => no_overwrite = true,
            'r' => {
                match std::fs::metadata(&value) {
                    Err(error) => {
                        let errno = error.raw_os_error().unwrap_or(libc::EIO);
                        return Err(format!(
                            "error checking for source RRD {value}: {}",
                            system_error_text(errno)
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
            't' => {
                if template_file.is_some() {
                    return Err("template already set".into());
                }
                template_file = Some(value);
            }
            _ => {}
        }
    }
    let mut positional = options.into_operands();
    if positional.is_empty() {
        return Err("need name of an rrd file to create".into());
    }
    let filename = positional.remove(0);
    let daemon_address = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty());
    if source_files.is_empty() && daemon_address.is_none() {
        let options = rondi::CreateOptions {
            step: step_was_set.then_some(step),
            start: start_was_set.then_some(start),
            no_overwrite,
            template: template_file.as_deref(),
        };
        rondi::create_rrd(&filename, &options, &positional)?;
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
            && !seen_data_source_names.insert(name)
        {
            return Err(format!("Duplicate DS name: {name}").into());
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
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("resolution", Some('r'), 'r'),
        OptionSpec::value("start", Some('s'), 's'),
        OptionSpec::value("end", Some('e'), 'e'),
        OptionSpec::flag("align-start", Some('a'), 'a'),
        OptionSpec::value("daemon", Some('d'), 'd'),
    ];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut start_tv = parse_time_spec("end-24h", now)?;
    let mut end_tv = parse_time_spec("now", now)?;
    let mut resolution = 1_u64;
    let mut align_start = false;
    let mut daemon_address = None::<String>;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            's' => {
                start_tv = parse_time_spec(found.text(), now)
                    .map_err(|error| format!("start time: {error}"))?;
            }
            'e' => {
                end_tv = parse_time_spec(found.text(), now)
                    .map_err(|error| format!("end time: {error}"))?;
            }
            'a' => align_start = true,
            'r' => resolution = parse_rrd_resolution(found.text())?,
            'd' => daemon_address = Some(found.text().to_owned()),
            _ => {}
        }
    }
    let (mut start, mut end) = resolve_start_end(&start_tv, &end_tv)?;
    if start < 315_360_000 {
        return Err("the first entry to fetch should be after 1980".into());
    }
    if align_start {
        // rrd_fetch.c takes start % step in unsigned long, so a step above
        // i64::MAX is valid and leaves delta equal to start.
        let delta = (start as u64) % resolution;
        start = start.wrapping_sub(delta as i64);
        end = end.wrapping_sub(delta as i64);
    }
    if end < start {
        return Err(format!("start ({start}) should be less than end ({end})").into());
    }
    let [filename, cf, ..] = options.operands() else {
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
    if let Some(address) = daemon_address {
        let escaped = escape_rrdcached_field(&filename.to_string_lossy());
        let command = format!("FETCH {escaped} {cf} {start} {end}");
        let result = parse_rrdcached_fetch(&send_rrdcached_multiline_command(&address, &command)?)?;
        return write_fetch_rows(&result.data_sources, result.rows);
    }
    // rrd_fetch.c allocates the whole range; streaming keeps memory bounded
    // by the archive while printing the same rows.
    let fetch = fetch_rrd_rows(&filename, cf, start, end, resolution)?;
    // rrd_fetch_fn runs rrd_cf_conv over every RRA while it looks for a
    // match and keeps going past a name it rejects, so the data prints
    // and rrdtool still exits with that error.
    let name_error = inspect_rrd(&filename.to_string_lossy())?
        .archives
        .iter()
        .rev()
        .find_map(|archive| rondi::cf_conv_error(&archive.raw_consolidation));
    write_fetch_rows(&fetch.data_sources, fetch.rows())?;
    match name_error {
        Some(message) => Err(RawMessage(message).into()),
        None => Ok(()),
    }
}

fn write_fetch_rows(
    data_sources: &[String],
    rows: impl IntoIterator<Item = rondi::RrdFetchRow>,
) -> Result<(), Box<dyn std::error::Error>> {
    // stdout is line buffered; one write per row dominates large fetches.
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    write!(out, "           ")?;
    for data_source in data_sources {
        write!(out, "{data_source:>20}")?;
    }
    writeln!(out, "\n")?;
    for row in rows {
        write!(out, "{:>10}:", row.timestamp)?;
        for value in row.values {
            match value {
                Some(value) => write!(out, " {}", format_fetch_value(value))?,
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
                // rrd_client.c:345 uses rrd_strtodbl, whose NaN conversion
                // differs from Rust's parser. Reuse the existing C port and
                // retain the converted value for printf-compatible output.
                rondi::parse_rrd_number(value)
                    .map(Some)
                    .ok_or("invalid rrdcached FETCH value")
            })
            .collect::<Result<Vec<_>, _>>()?;
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

#[test]
fn daemon_fetch_uses_rrd_numeric_conversion() {
    let result = parse_rrdcached_fetch(
        "FlushVersion: 1\nStart: 1000000000\nEnd: 1000000010\nStep: 10\nDSCount: 4\nDSName: a b c d\n1000000010: nan -nan inf -inf\n",
    )
    .unwrap();
    let values = &result.rows[0].values;
    assert!(values[0].unwrap().is_nan());
    assert_eq!(
        values[0].unwrap().is_sign_negative(),
        !cfg!(target_arch = "x86_64")
    );
    assert!(values[1].unwrap().is_nan());
    assert_eq!(
        values[1].unwrap().is_sign_negative(),
        cfg!(target_arch = "x86_64")
    );
    assert_eq!(values[2], Some(f64::INFINITY));
    assert_eq!(values[3], Some(f64::NEG_INFINITY));
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
    let zoom = options.zoom;
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
    let force_utc = options.utc;
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
    // The renderer enforces the same limit on the final canvas, but only after
    // data_proc has allocated per-pixel buffers sized by --width. Print-only
    // graphs draw nothing and keep the unbounded behavior of rrdtool.
    if format == "PNG"
        && (10..).contains(&image_width)
        && (10..).contains(&image_height)
        && !png_within_limit(image_width, image_height)
        && im.gdes.iter().any(|element| {
            use rondi::graph::Gf;
            matches!(
                element.gf,
                Gf::Gprint | Gf::Line | Gf::Area | Gf::Tick | Gf::Hrule | Gf::Vrule | Gf::Stack
            )
        })
    {
        return Err(PNG_DIMENSION_LIMIT_MESSAGE.into());
    }
    // rrd_graph_v: graph_paint, then image_info and the image itself.
    let mut info = GraphInfo::default();
    let to_memory = filename == "-";
    let mut image = None::<Vec<u8>>;
    let mut save_error = None::<String>;
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
        // rrd_graph_xport only streams to the file when rrd_fopen worked;
        // otherwise the buffered data is returned like the "-" image.
        let mut buffered = to_memory;
        if !to_memory {
            match write_output_file(filename, output.as_bytes()) {
                Err(OutputError::Refused(message)) => return Err(message.into()),
                Err(OutputError::Open(_)) => buffered = true,
                Ok(()) | Err(OutputError::Write(_)) => {}
            }
        }
        if let Some(error) = error {
            return Err(error.into());
        }
        if buffered {
            image = Some(output.into_bytes());
        }
        if let PrintOutcome::Abandoned =
            print_format::format_print_items(&mut im, si_base, force_utc, &mut info)?
        {
            return pending_error(&im);
        }
    } else {
        flush_and_prepare_data(&mut im)?;
        let graph_elements =
            match print_format::format_print_items(&mut im, si_base, force_utc, &mut info)? {
                PrintOutcome::Finished { draws } => draws,
                PrintOutcome::Abandoned => return pending_error(&im),
            };
        // Nothing more is done when no element draws.
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
            // calc_horizontal_grid gives up on a NaN scaled range (an
            // infinite limit) without setting an error, so rrd_graph_v
            // returns nothing: no file, no output and a zero exit status.
            if ((im.maxval - im.minval) / im.magfact).is_nan() {
                return Ok(());
            }
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
            // graph_cairo_setup sizes the surface as int(ximg * zoom). cairo
            // refuses a negative or oversized surface and a zero-sized one
            // cannot be written; graph_cairo_finish then fails the save
            // while rrd_graph_v still reports image_info.
            let save_failed = format!(
                "Could not save png to '{}'",
                if to_memory { "memory" } else { filename }
            );
            let surface = |size: i64| c_double_to_int(size as f64 * zoom);
            let (surface_width, surface_height) = (surface(im.ximg), surface(im.yimg));
            let surface_valid = |size: i32| (0..=CAIRO_MAX_IMAGE_SIZE).contains(&size);
            if !surface_valid(surface_width) || !surface_valid(surface_height) {
                eprintln!(
                    "OOPS: Cairo has issues it can't even die: invalid value (typically too big) for the size of the input (surface, pattern, etc.)"
                );
                save_error = Some(save_failed);
            } else if surface_width == 0 || surface_height == 0 {
                // rrd_strtodbl accepts "nan", which cairo_scale rejects.
                if zoom.is_nan() {
                    eprintln!(
                        "OOPS: Cairo has issues it can't even die: invalid matrix (not invertible)"
                    );
                }
                if !to_memory
                    && let Err(OutputError::Refused(message)) = write_output_file(filename, &[])
                {
                    return Err(message.into());
                }
                save_error = Some(save_failed);
            } else {
                // A negative --units-length moves the graph origin left of
                // the image; cairo clips whatever falls outside the surface.
                let clamp = |value: i64| u32::try_from(value.max(0));
                let (rendered, series) = graph_render_input(&im);
                let png = render_graph_png(
                    &rendered,
                    &series,
                    GraphPngOptions {
                        canvas: GraphCanvas {
                            width: clamp(im.ximg)?,
                            height: clamp(im.yimg)?,
                            surface_width: surface_width.unsigned_abs(),
                            surface_height: surface_height.unsigned_abs(),
                            left: clamp(im.xorigin)?,
                            top: clamp(im.yorigin - im.ysize)?,
                            plot_width: clamp(im.xsize)?,
                            plot_height: clamp(im.ysize)?,
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
                if to_memory {
                    image = Some(png);
                } else {
                    // rrd_graph_v keeps building the result after a failed
                    // save and rrdtool reports the error once it has printed it.
                    match write_output_file(filename, &png) {
                        Ok(()) => {}
                        Err(OutputError::Refused(message)) => return Err(message.into()),
                        Err(_) => save_error = Some(save_failed),
                    }
                }
            }
        }
    }
    // rrd_graph_v adds image_info for every format once graph_paint
    // succeeds; ximg and yimg stay 0 when nothing was drawn.
    if let Some(format) = imginfo.as_deref().filter(|format| !format.is_empty()) {
        let basename = if to_memory {
            "memory"
        } else {
            Path::new(filename)
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or(filename)
        };
        let scaled = |size: i64| c_double_to_long(zoom * size as f64);
        info.push(
            "image_info",
            InfoValue::Str(format_imginfo(
                format,
                basename,
                scaled(im.ximg),
                scaled(im.yimg),
            )?),
        );
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
    match save_error {
        Some(message) => Err(message.into()),
        None => pending_error(&im),
    }
}

/// The error `rrd_tool.c` still finds set when a graph or xport otherwise
/// succeeded, such as a daemon that `data_fetch` could not reach.
fn pending_error(im: &rondi::graph::GraphImage) -> Result<(), Box<dyn std::error::Error>> {
    match &im.pending_error {
        Some(message) => Err(message.clone().into()),
        None => Ok(()),
    }
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
                // u_cnt is an unsigned long printed with %lu.
                InfoValue::Count(value) => writeln!(out, "{}", *value as u64)?,
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
        resolve_window(start_spec, end_spec, (start, end), now)
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
        let key = (address.to_owned(), filename.to_owned());
        if flushed.contains(&key) {
            return Ok(None);
        }
        match send_rrdcached_flush(address, filename) {
            Ok(()) => {
                flushed.insert(key);
                Ok(None)
            }
            // data_fetch treats a failed rrdc_connect as "no daemon" and
            // reads the local file, leaving the error set.
            Err(error) if error.is::<RrdcachedConnectError>() => Ok(Some(error.to_string())),
            Err(error) => {
                let message = error.to_string();
                flush_error = Some(error);
                Err(rondi::StoreError::RrdExpression(message))
            }
        }
    };
    let fetched = im.data_fetch(&mut hook);
    if let Some(error) = flush_error {
        return Err(error);
    }
    fetched?;
    im.data_calc()?;
    Ok(())
}

/// Why an output file could not be written. `Refused` is Rondi's own
/// safety check; the others carry the errno RRDtool would report through
/// `rrd_strerror`.
#[derive(Debug)]
enum OutputError {
    Refused(String),
    Open(libc::c_int),
    Write(libc::c_int),
}

impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(message) => f.write_str(message),
            Self::Open(errno) | Self::Write(errno) => f.write_str(&system_error_text(*errno)),
        }
    }
}

impl std::error::Error for OutputError {}

fn errno_of(error: &std::io::Error) -> libc::c_int {
    error.raw_os_error().unwrap_or(libc::EIO)
}

/// Write only an owned, singly linked regular file.
fn write_output_file(path: &str, bytes: &[u8]) -> Result<(), OutputError> {
    // Errors without an errno are the refusals open_output_file raises.
    let mut file = rondi::open_output_file(std::path::Path::new(path), false).map_err(|error| {
        error.raw_os_error().map_or_else(
            || OutputError::Refused(error.to_string()),
            OutputError::Open,
        )
    })?;
    // ftruncate fails on a device such as /dev/null.
    if file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        file.set_len(0)
            .map_err(|error| OutputError::Write(errno_of(&error)))?;
    }
    file.write_all(bytes)
        .map_err(|error| OutputError::Write(errno_of(&error)))
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
    // Unlike graph, rrd_xport connects to the image-wide daemon up front
    // and gives up when that fails.
    #[cfg(unix)]
    if let Some(address) = im
        .daemon_addr
        .as_deref()
        .filter(|address| !address.is_empty())
    {
        connect_rrdcached(address)?;
    }
    flush_and_prepare_data(&mut im)?;
    let data = im.xport(false)?;
    let flags = u8::from(json) | (u8::from(show_time) << 1) | (u8::from(enum_ds) << 2);
    let (output, error) = format_xport_xmljson(flags, &mut im, &data, 1000, false);
    Ok((output, error.or_else(|| im.pending_error.clone())))
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
    /// The written image: the canvas scaled by `--zoom`.
    surface_width: u32,
    surface_height: u32,
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

/// cairo_image_surface_create rejects larger surfaces (MAX_IMAGE_SIZE).
const CAIRO_MAX_IMAGE_SIZE: i32 = 32767;
/// rrdtool has no limit of its own beyond cairo's 32767-pixel side. Rondi
/// also caps the pixel count so one request cannot allocate an unbounded
/// canvas; 2^24 pixels is about 48 MiB of RGB and admits every canvas the
/// earlier 4096x4096 limit did, plus tall or wide graphs up to cairo's side.
const PNG_MAX_PIXELS: u64 = 1 << 24;
const PNG_DIMENSION_LIMIT_MESSAGE: &str = "PNG graph exceeds the 16777216 pixel safety limit";

fn png_within_limit(width: u32, height: u32) -> bool {
    let side = CAIRO_MAX_IMAGE_SIZE.unsigned_abs();
    width <= side && height <= side && u64::from(width) * u64::from(height) <= PNG_MAX_PIXELS
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
        surface_width,
        surface_height,
        left: plot_left,
        top: plot_top,
        plot_width: width,
        plot_height: height,
        minimum,
        maximum,
    } = canvas;
    if !png_within_limit(canvas_width, canvas_height)
        || !png_within_limit(surface_width, surface_height)
    {
        return Err(PNG_DIMENSION_LIMIT_MESSAGE.into());
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
    for pixel in pixels.as_chunks_mut::<3>().0 {
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
    // graph_paint_timestring paints every HRULE and VRULE after the data, so
    // a rule defined before a LINE still covers it.
    let is_rule = |item: &GraphSeries| matches!(item.style, "hrule" | "vrule");
    let paint_order = (0..series.len())
        .filter(|&index| !is_rule(&series[index]))
        .chain((0..series.len()).filter(|&index| is_rule(&series[index])));
    for series_index in paint_order {
        let item = &series[series_index];
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
    // graph_cairo_setup draws through cairo_scale(zoom); the renderer draws
    // at 1:1 and samples the canvas onto the zoomed surface instead.
    if (surface_width, surface_height) != (canvas_width, canvas_height) {
        pixels = zoom_pixels(
            &pixels,
            (canvas_width, canvas_height),
            (surface_width, surface_height),
        );
    }
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, surface_width, surface_height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
    }
    Ok(output)
}

/// C's `(int)` conversion of a double as the pinned build compiles it. An
/// out-of-range or NaN value is undefined behaviour in C: aarch64 FCVTZS
/// saturates and maps NaN to 0 like Rust's `as`, while x86-64 CVTTSD2SI
/// returns the minimum ("integer indefinite").
fn c_double_to_int(value: f64) -> i32 {
    if cfg!(target_arch = "x86_64") && !(-2_147_483_648.0..2_147_483_648.0).contains(&value) {
        return i32::MIN;
    }
    value as i32
}

/// C's `(long)` conversion of a double; see [`c_double_to_int`].
fn c_double_to_long(value: f64) -> i64 {
    if cfg!(target_arch = "x86_64")
        && !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value)
    {
        return i64::MIN;
    }
    value as i64
}

/// Nearest-neighbour resampling of an RGB canvas onto `to`.
fn zoom_pixels(pixels: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let source = |target: u32, from: u32, to: u32| {
        ((u64::from(target) * u64::from(from)) / u64::from(to)) as usize
    };
    let mut output = Vec::with_capacity(to.0 as usize * to.1 as usize * 3);
    for y in 0..to.1 {
        let row = source(y, from.1, to.1) * from.0 as usize;
        for x in 0..to.0 {
            let offset = (row + source(x, from.0, to.0)) * 3;
            output.extend_from_slice(&pixels[offset..offset + 3]);
        }
    }
    output
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

fn format_imginfo(
    format: &str,
    filename: &str,
    width: i64,
    height: i64,
) -> Result<String, Box<dyn std::error::Error>> {
    // bad_format_imginfo (rrd_graph.c:5880).
    let bad_format = || -> Box<dyn std::error::Error> {
        format!(
            "invalid format string '{format}' (should match '^(?:[^%]+|%%)*%s(?:[^%]+|%%)*%lu(?:[^%]+|%%)*%lu(?:[^%]+|%%)*$')"
        )
        .into()
    };
    let mut output = String::new();
    let mut chars = format.chars().peekable();
    let mut argument = 0;
    while let Some(character) = chars.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        let conversion = chars.next().ok_or_else(bad_format)?;
        if conversion == '%' {
            output.push('%');
            continue;
        }
        let value = match (argument, conversion) {
            (0, 's') => filename.to_owned(),
            // The (long) sizes print through %lu.
            (1, 'l') if chars.next() == Some('u') => (width as u64).to_string(),
            (2, 'l') if chars.next() == Some('u') => (height as u64).to_string(),
            _ => return Err(bad_format()),
        };
        output.push_str(&value);
        argument += 1;
    }
    if argument != 3 {
        return Err(bad_format());
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
    let mut si_scale = rondi::format_value::UnitScale::new(si_base);
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
                        dbuf = print_format::strftime_text(&element.format, &tmvdef, 1024);
                    }
                } else {
                    // bad_format_print runs before the "%S" rewrite here.
                    let original = element.format.clone();
                    let mut format = original.clone();
                    match print_format::xport_print_text(printval, &mut format, &mut si_scale) {
                        Ok(text) => dbuf = text,
                        Err(_) => {
                            return Err(format!("bad format for PRINT in \"{original}'"));
                        }
                    }
                    im.gdes[i].format = format;
                }
                let text = if json {
                    escape_json(&dbuf)
                } else {
                    dbuf.clone()
                };
                let (tag, buffer) = if im.gdes[i].gf == Gf::Print {
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
    const UPDATE_LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("template", Some('t'), 't'),
        OptionSpec::value("daemon", Some('d'), 'd'),
        OptionSpec::flag("skip-past-updates", Some('s'), 's'),
        OptionSpec::value("locking", Some('L'), 'L'),
    ];
    const UPDATEV_LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("template", Some('t'), 't'),
        OptionSpec::flag("skip-past-updates", Some('s'), 's'),
        OptionSpec::value("locking", Some('L'), 'L'),
    ];
    let mut template = None;
    let mut skip_past_updates = false;
    let mut daemon_address = None;
    let mut locking = String::new();
    let table = if verbose {
        UPDATEV_LONGOPTS
    } else {
        UPDATE_LONGOPTS
    };
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), table);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            't' => {
                template = Some(
                    found
                        .text()
                        .split(':')
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                );
            }
            's' => skip_past_updates = true,
            'd' => daemon_address = Some(found.text().to_owned()),
            'L' => {
                if !rondi::is_rrd_locking_mode(found.text()) {
                    return Err(format!("unsupported locking mode '{}'\n", found.text()).into());
                }
                locking = found.text().to_owned();
            }
            _ => {}
        }
    }
    let [filename_text, samples @ ..] = options.operands() else {
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
        if let Err(error) = ensure_rrd_file_exists(&filename) {
            // rrd_update_v pushes return_value before _rrd_updatex opens the
            // file, so the open failure still prints it.
            if verbose {
                println!("return_value = -1");
            }
            return Err(error);
        }
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
            let seconds = parse_time_spec(timestamp, update_now.floor() as i64)
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
            parse_update_timestamp(timestamp, update_now).map_err(|_| {
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
        daemon_samples.push(format!("{}:{encoded_values}", update_time));
    }
    if let Some(address) = daemon_address
        && !daemon_samples.is_empty()
    {
        #[cfg(unix)]
        {
            let stream = connect_rrdcached(&address)?;
            let path = rrdcached_client_path(&address, &filename.to_string_lossy())?;
            send_rrdcached_update_on_stream(stream, Path::new(&path), &daemon_samples)?;
        }
        #[cfg(not(unix))]
        return Err("rrdcached updates are unavailable on this platform".into());
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
    const LONGOPTS: &[OptionSpec<char>] = &[OptionSpec::value("daemon", Some('d'), 'd')];
    let mut daemon = None;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        if found.key == 'd' {
            daemon = Some(found.text().to_owned());
        }
    }
    let files = options.operands().to_vec();
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
            let detail = if error.downcast_ref::<RrdcachedPathUnavailable>().is_some() {
                "unknown error".to_owned()
            } else {
                format!("rrdcached@{daemon}: {error}")
            };
            return Err(format!(
                "Flushing of file \"{filename}\" failed: {detail}. Skipping remaining {remaining} file{}.",
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
    let command = translate_rrdcached_file_command(address, command)?;
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
    let filename = filename.to_string_lossy();
    if filename.contains(['\n', '\r', '\0']) {
        return Err("rrdtool update: invalid rrdcached socket or filename".into());
    }
    writeln!(
        stream,
        "UPDATE {} {}",
        escape_rrdcached_field(&filename),
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

/// xport and graph data values use RRDtool's own printf, which is not
/// correctly rounded and never signs -0. updatev uses libc printf and
/// keeps the sign.
fn format_xport_value(value: f64) -> String {
    rondi::format_value::data_value(value)
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
    const LONGOPTS: &[OptionSpec<char>] = &[OptionSpec::value("daemon", Some('d'), 'd')];
    let mut daemon_address = None::<String>;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        if found.key == 'd' {
            daemon_address = Some(found.text().to_owned());
        }
    }
    let [filename] = options.operands() else {
        return Err(format!("Usage: rrdtool {} [--daemon|-d <addr>] <file>", args[0]).into());
    };
    if let Some(address) = daemon_address
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
        let escaped_filename = filename.replace('\\', "\\\\").replace(' ', "\\ ");
        let timestamp = rrdcached_time_value(&address, &format!("LAST {escaped_filename}"))?;
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
    const LONGOPTS: &[OptionSpec<char>] = &[OptionSpec::value("daemon", Some('d'), 'd')];
    let mut daemon_address = None::<String>;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        if found.key == 'd' {
            daemon_address = Some(found.text().to_owned());
        }
    }
    let [filename] = options.operands() else {
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
    // rrd_tool.c prints the names and last_ds text with %s, byte for byte.
    let mut out = Vec::new();
    for source in &info.data_sources {
        out.push(b' ');
        out.extend_from_slice(&source.raw_name);
    }
    write!(out, "\n\n{:>10}:", info.last_update)?;
    for source in &info.data_sources {
        out.push(b' ');
        out.extend_from_slice(&source.raw_last_value);
    }
    out.push(b'\n');
    std::io::stdout().write_all(&out)?;
    Ok(())
}

fn rrdtool_first(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/first.txt"));
        return Ok(());
    }
    // rrd_first.c:18.
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("rraindex", None, 'i'),
        OptionSpec::value("daemon", Some('d'), 'd'),
    ];
    let mut archive_index = 0_i32;
    let mut daemon_address = None::<String>;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            'i' => {
                // rrd_first.c:33 stores strtol(optarg, &endptr, 0) in an int.
                archive_index = rondi::c_strtol(found.text(), 0).0 as i32;
                if archive_index < 0 {
                    return Err("invalid rraindex number".into());
                }
            }
            'd' => daemon_address = Some(found.text().to_owned()),
            _ => {}
        }
    }
    let Some(filename) = options.operands().first() else {
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
        let timestamp = rrdcached_time_value(
            &address,
            &format!("FIRST {escaped_filename} {archive_index}"),
        )?;
        println!("{timestamp}");
        return Ok(());
    }
    println!("{}", first_rrd_time(filename, archive_index as usize)?);
    Ok(())
}

fn rrdtool_info(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/info.txt"));
        return Ok(());
    }
    // rrd_info.c:72.
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("daemon", Some('d'), 'd'),
        OptionSpec::flag("noflush", Some('F'), 'F'),
    ];
    let mut daemon_address = None::<String>;
    let mut noflush = false;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            'd' => daemon_address = Some(found.text().to_owned()),
            'F' => noflush = true,
            _ => {}
        }
    }
    let [filename] = options.operands() else {
        return Err(format!(
            "Usage: rrdtool {} [--daemon |-d <addr> [--noflush|-F]] <file>",
            args[0]
        )
        .into());
    };
    let filename = filename.clone();
    if !noflush
        && let Some(address) = daemon_address
            .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
            .filter(|address| !address.is_empty())
    {
        send_rrdcached_flush(&address, &filename)?;
    }
    ensure_rrd_file_exists(std::path::Path::new(&filename))?;
    let info = inspect_rrd(&filename)?;
    // rrd_info prints a DS or RRA with a name dst_conv or rrd_cf_conv
    // rejects through its default branch and reports the error afterwards.
    if info.data_sources.iter().any(|ds| ds.kind == "COMPUTE")
        || info.archives.iter().any(|archive| {
            !matches!(
                archive.consolidation.as_str(),
                "AVERAGE" | "MIN" | "MAX" | "LAST"
            ) && rondi::cf_conv_error(&archive.raw_consolidation).is_none()
        })
    {
        return Err(
            "rrdtool info output for this data-source or consolidation type is not implemented"
                .into(),
        );
    }
    // rrd_info_print writes names and strings with %s, byte for byte.
    let mut out = Vec::new();
    writeln!(out, "filename = \"{filename}\"")?;
    writeln!(out, "rrd_version = \"{}\"", info.version)?;
    writeln!(out, "step = {}", info.step)?;
    writeln!(out, "last_update = {}", info.last_update)?;
    writeln!(out, "header_size = {}", info.header_size)?;
    for (index, source) in info.data_sources.iter().enumerate() {
        let key = |out: &mut Vec<u8>, field: &str| {
            out.extend_from_slice(b"ds[");
            out.extend_from_slice(&source.raw_name);
            out.extend_from_slice(b"].");
            out.extend_from_slice(field.as_bytes());
            out.extend_from_slice(b" = ");
        };
        let quoted = |out: &mut Vec<u8>, field: &str, value: &[u8]| {
            key(out, field);
            out.push(b'"');
            out.extend_from_slice(value);
            out.extend_from_slice(b"\"\n");
        };
        key(&mut out, "index");
        writeln!(out, "{index}")?;
        quoted(&mut out, "type", &source.raw_kind);
        key(&mut out, "minimal_heartbeat");
        writeln!(out, "{}", source.heartbeat)?;
        key(&mut out, "min");
        writeln!(out, "{}", format_info_value(source.minimum))?;
        key(&mut out, "max");
        writeln!(out, "{}", format_info_value(source.maximum))?;
        quoted(&mut out, "last_ds", &source.raw_last_value);
        key(&mut out, "value");
        writeln!(out, "{}", format_info_float(source.pdp_value))?;
        key(&mut out, "unknown_sec");
        writeln!(out, "{}", source.unknown_seconds)?;
    }
    for (index, archive) in info.archives.iter().enumerate() {
        write!(out, "rra[{index}].cf = \"")?;
        out.extend_from_slice(&archive.raw_consolidation);
        out.extend_from_slice(b"\"\n");
        writeln!(out, "rra[{index}].rows = {}", archive.rows)?;
        writeln!(out, "rra[{index}].cur_row = {}", archive.current_row)?;
        writeln!(out, "rra[{index}].pdp_per_row = {}", archive.pdp_per_row)?;
        writeln!(out, "rra[{index}].xff = {}", format_rrd_float(archive.xff))?;
        for (ds_index, prep) in archive.cdp_prep.iter().enumerate() {
            writeln!(
                out,
                "rra[{index}].cdp_prep[{ds_index}].value = {}",
                format_info_float(prep.value)
            )?;
            writeln!(
                out,
                "rra[{index}].cdp_prep[{ds_index}].unknown_datapoints = {}",
                prep.unknown_datapoints
            )?;
        }
    }
    std::io::stdout().write_all(&out)?;
    match rondi::rrd_name_error(&info) {
        Some(message) => Err(RawMessage(message).into()),
        None => Ok(()),
    }
}

fn rrdtool_dump(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/dump.txt"));
        return Ok(());
    }
    // rrd_dump.c:556.
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("daemon", Some('d'), 'd'),
        OptionSpec::value("header", Some('h'), 'h'),
        OptionSpec::flag("no-header", Some('n'), 'n'),
    ];
    let usage = format!(
        "usage rrdtool {} [--header|-h {{none,xsd,dtd}}]\n[--no-header|-n]\n[--daemon|-d address]\nfile.rrd [file.xml]",
        args[0]
    );
    let mut header = RrdDumpHeader::Dtd;
    let mut daemon_address = None::<String>;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let Ok(found) = found else {
            return Err(usage.into());
        };
        match found.key {
            'd' => daemon_address = Some(found.text().to_owned()),
            'n' => header = RrdDumpHeader::None,
            // parse_opt_xmlheader returns -1 for other words, which
            // rrd_dump_opt_r writes like "none".
            'h' => {
                header = match found.text() {
                    "dtd" => RrdDumpHeader::Dtd,
                    "xsd" => RrdDumpHeader::Xsd,
                    _ => RrdDumpHeader::None,
                };
            }
            _ => return Err(usage.into()),
        }
    }
    let positional = options.operands().to_vec();
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
    let xml = rondi::dump_rrd_file_with_header_bytes(&positional[0], header)?;
    if let Some(output) = positional.get(1) {
        match write_output_file(output, &xml) {
            Ok(()) => {}
            // rrd_dump_opt_r returns -1 from a failed fopen without setting
            // an error, so rrdtool reports nothing and exits 0.
            Err(OutputError::Open(_)) => return Ok(()),
            Err(OutputError::Write(errno)) => {
                return Err(
                    format!("error writing output file: {}", system_error_text(errno)).into(),
                );
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        std::io::stdout().write_all(&xml)?;
    }
    // rrd_dump_opt_r finishes the dump before rrd_tool.c sees the error
    // a rejected DS type or CF name left behind.
    match rondi::rrd_name_error(&inspect_rrd(&positional[0])?) {
        Some(message) => Err(RawMessage(message).into()),
        None => Ok(()),
    }
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
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::flag("range-check", Some('r'), 'r'),
        OptionSpec::flag("force-overwrite", Some('f'), 'f'),
    ];
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            'r' => {
                RANGE_CHECK.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            'f' => {
                FORCE_OVERWRITE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }
    let positional = options.operands();
    if positional.len() != 2 {
        return Err(format!(
            "usage rrdtool {} [--range-check|-r] [--force-overwrite|-f] file.xml file.rrd",
            args[0]
        )
        .into());
    }
    let range_check = RANGE_CHECK.load(std::sync::atomic::Ordering::Relaxed);
    let force_overwrite = FORCE_OVERWRITE.load(std::sync::atomic::Ordering::Relaxed);
    // rrd_restore.c:1242-1263: stdin ends at the first ctrl-Z and silences
    // libxml2's error printing for the rest of the process, as
    // xmlSetGenericErrorFunc does upstream.
    static XML_ERRORS_SILENCED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let xml = if positional[0] == "-" {
        XML_ERRORS_SILENCED.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut xml = Vec::new();
        std::io::stdin().read_to_end(&mut xml)?;
        if let Some(end) = xml.iter().position(|byte| *byte == 0x1a) {
            xml.truncate(end);
        }
        xml
    } else {
        std::fs::read(&positional[0])
            .map_err(|_| format!("Could not create xml reader for: {}", positional[0]))?
    };
    let input = if positional[0] == "-" {
        rondi::RestoreInput::Stdin(&xml)
    } else {
        rondi::RestoreInput::File {
            name: &positional[0],
            xml: &xml,
        }
    };
    let mut stderr = std::io::stderr();
    let mut sink = std::io::sink();
    let diagnostics: &mut dyn std::io::Write =
        if XML_ERRORS_SILENCED.load(std::sync::atomic::Ordering::Relaxed) {
            &mut sink
        } else {
            &mut stderr
        };
    rondi::restore_rrd_input(
        input,
        &positional[1],
        force_overwrite,
        range_check,
        diagnostics,
    )?;
    Ok(())
}

fn rrdtool_tune(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/tune.txt"));
        return Ok(());
    }
    // rrd_tune.c:78. rrd_tune reads only --daemon in a first pass that
    // ignores errors; rrd_tune_r then applies every option in order.
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("heartbeat", Some('h'), 'h'),
        OptionSpec::value("minimum", Some('i'), 'i'),
        OptionSpec::value("maximum", Some('a'), 'a'),
        OptionSpec::value("data-source-type", Some('d'), 'd'),
        OptionSpec::value("data-source-rename", Some('r'), 'r'),
        OptionSpec::value("deltapos", Some('p'), 'p'),
        OptionSpec::value("deltaneg", Some('n'), 'n'),
        OptionSpec::value("window-length", Some('w'), 'w'),
        OptionSpec::value("failure-threshold", Some('f'), 'f'),
        OptionSpec::value("alpha", Some('x'), 'x'),
        OptionSpec::value("beta", Some('y'), 'y'),
        OptionSpec::value("gamma", Some('z'), 'z'),
        OptionSpec::value("gamma-deviation", Some('v'), 'v'),
        OptionSpec::value("smoothing-window", Some('s'), 's'),
        OptionSpec::value("smoothing-window-deviation", Some('S'), 'S'),
        OptionSpec::value("aberrant-reset", Some('b'), 'b'),
        OptionSpec::value("step", Some('t'), 't'),
        OptionSpec::value("daemon", Some('D'), 'D'),
    ];
    let mut daemon_address = None::<String>;
    // The settings in long form, for the daemon's TUNE command.
    let mut raw_settings = Vec::new();
    let mut first_pass = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in first_pass.by_ref() {
        // The second pass reports option errors.
        let Ok(found) = found else {
            continue;
        };
        if found.key == 'D' {
            daemon_address = Some(found.text().to_owned());
        } else if let Some(spec) = LONGOPTS.iter().find(|spec| spec.key == found.key) {
            raw_settings.push(format!("--{}", spec.long));
            raw_settings.push(found.text().to_owned());
        }
    }
    let mut operands = first_pass.operands().to_vec();
    // Compatibility: a long option left without its value at the end also
    // drops the last operand before it, so `tune a.rrd --heartbeat`
    // reports a missing file name while `tune a.rrd b.rrd --heartbeat`
    // reports the missing value.
    // Provenance: black-box compatibility test (CLI-31, cli-truncate-006
    // to -011).
    if first_pass.has_dangling_long() {
        operands.pop();
    }
    let Some(filename) = operands.first().cloned() else {
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
    // The second pass sees the options before the operands, so a trailing
    // short option without a value takes the file name as its value there.
    let second_words = first_pass.rearranged();
    let mut options = OptionScanner::new(&second_words, LONGOPTS);
    // rrd_tune.c:423: --step feeds handle_modify's stretch.
    let mut newstep: libc::c_int = -1;
    // rrd_tune_r edits the header through the file mapping, so settings
    // applied before a failing option stay written.
    let applied = (|| -> Result<(), Box<dyn std::error::Error>> {
        for found in options.by_ref() {
            let found = found?;
            let value = found.text();
            match found.key {
                'h' => {
                    let Some((name, heartbeat)) = tune_scan_heartbeat(value) else {
                        return Err("invalid arguments for heartbeat".into());
                    };
                    let ds = ds_match(&current_names, &name)?;
                    changes[ds].heartbeat = Some(heartbeat as u64);
                }
                kind @ ('i' | 'a') => {
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
                'd' => {
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
                'r' => {
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
                'p' | 'n' => {
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
                'f' | 'w' => {
                    let param = cparse::atoi(value) as libc::c_ulong;
                    if !(1..=28).contains(&param) {
                        return Err("Parameter must be between 1 and 28".into());
                    }
                    return Err("Failures RRA does not exist in this RRD".into());
                }
                kind @ ('x' | 'y' | 'z' | 'v' | 's' | 'S') => {
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
                'b' => {
                    let Some(name) = tune_scan_name(value) else {
                        return Err("invalid argument for aberrant-reset".into());
                    };
                    // reset_aberrant_coefficients does nothing without
                    // Holt-Winters archives.
                    ds_match(&current_names, &name)?;
                }
                't' => newstep = cparse::atoi(value),
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

    // rrd_tune.c:427. Only reached when an option consumed the file name.
    if options.operands().is_empty() {
        for source in rondi::inspect_rrd_file(&filename)?.data_sources {
            if source.kind == "COMPUTE" {
                return Err(
                    "rrdtool tune output for COMPUTE data sources is not implemented".into(),
                );
            }
            // An unknown bound is DNAN, whose sign bit is set on x86_64,
            // where glibc prints it as -nan.
            let bound =
                |value: Option<f64>| c_printf_double("%1.4f", value.unwrap_or_else(rrd_nan));
            println!(
                "DS[{}] typ: {}\thbt: {}\tmin: {}\tmax: {}",
                source.name,
                source.kind,
                source.heartbeat,
                bound(source.minimum),
                bound(source.maximum)
            );
        }
    }

    // The words after the file name are all classified and parsed before
    // anything changes (docs/compatibility/tune-modify.md, TM-8 to TM-13).
    let mut structure_ops = Vec::new();
    for argument in options.operands().get(1..).unwrap_or_default() {
        if let Some(name) = argument
            .strip_prefix("DEL:")
            .filter(|name| !name.is_empty())
        {
            structure_ops.push(rondi::RrdStructureOp::DeleteSource(name.to_owned()));
        } else if let Some(spec) = argument.strip_prefix("DS:").filter(|spec| !spec.is_empty()) {
            structure_ops.push(rondi::RrdStructureOp::AddSource(spec.to_owned()));
        } else if let Some(spec) = argument
            .strip_prefix("RRA#")
            .filter(|spec| !spec.is_empty())
        {
            let Some((index, sign, row_count)) = tune_scan_rra_op(spec) else {
                return Err("Failed to parse RRA# command".into());
            };
            if !matches!(sign, b'=' | b'-' | b'+') {
                let sign = String::from_utf8_lossy(&[sign]).into_owned();
                return Err(
                    format!("Failed to parse RRA# command: invalid operation: {sign}").into(),
                );
            }
            let change = match sign {
                b'=' => rondi::RrdRowChange::Set(row_count),
                b'+' => rondi::RrdRowChange::Grow(row_count),
                _ => rondi::RrdRowChange::Shrink(row_count),
            };
            structure_ops.push(rondi::RrdStructureOp::ResizeArchive { index, change });
        } else if let Some(spec) = argument
            .strip_prefix("RRA:")
            .filter(|spec| !spec.is_empty())
        {
            structure_ops.push(rondi::RrdStructureOp::AddArchive(spec.to_owned()));
        } else if let Some(index) = argument
            .strip_prefix("DELRRA:")
            .filter(|index| !index.is_empty())
        {
            let index = cparse::atoi(index);
            if index < 0 {
                return Err("DELRRA requires a non-negative, integer argument".into());
            }
            structure_ops.push(rondi::RrdStructureOp::DeleteArchive(index as u32));
        } else {
            return Err(format!("unparsable argument: {argument}").into());
        }
    }
    if !structure_ops.is_empty() {
        rondi::modify_rrd_file(&filename, &structure_ops, newstep)?;
    }
    Ok(())
}

/// `sscanf(spec, "%u:%c%u", &idx, &sign, &number) == 3` (rrd_modify.c:1352).
fn tune_scan_rra_op(spec: &str) -> Option<(libc::c_uint, u8, libc::c_uint)> {
    let input = cparse::c_string(spec);
    let mut index: libc::c_uint = 0;
    let mut sign: libc::c_char = 0;
    let mut number: libc::c_uint = 0;
    // SAFETY: each conversion writes the object its argument points to.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%u:%c%u".as_ptr(),
            &mut index as *mut libc::c_uint,
            &mut sign as *mut libc::c_char,
            &mut number as *mut libc::c_uint,
        )
    };
    (matched == 3).then_some((index, sign as u8, number))
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
        &mut |mut ignored| {
            // One warning pair per 8 ignored bytes (docs/compatibility/resize.md RZ-15).
            while ignored > 0 {
                let part = ignored.min(8);
                eprintln!("WARNING: ignoring last {part} bytes");
                eprintln!(
                    "WARNING: if you see this message multiple times for a single file you're in trouble"
                );
                ignored -= part;
            }
        },
    )?;
    Ok(())
}

fn rrdtool_list(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() == 1 {
        print!("{}", include_str!("help/list.txt"));
        return Ok(());
    }
    // rrd_list.c:256.
    const LONGOPTS: &[OptionSpec<char>] = &[
        OptionSpec::value("daemon", Some('d'), 'd'),
        OptionSpec::flag("noflush", Some('F'), 'F'),
        OptionSpec::flag("recursive", Some('r'), 'r'),
    ];
    let mut recursive = false;
    let mut daemon = None;
    let mut noflush = false;
    let mut options = OptionScanner::new(args.get(1..).unwrap_or_default(), LONGOPTS);
    for found in options.by_ref() {
        let found = found?;
        match found.key {
            'd' => daemon = Some(found.text().to_owned()),
            'F' => noflush = true,
            'r' => recursive = true,
            _ => {
                return Err(format!(
                    "Usage: rrdtool {} [--daemon <addr> [--noflush]] <file>",
                    args[0]
                )
                .into());
            }
        }
    }
    let [directory] = options.operands() else {
        return Err(format!(
            "Usage: rrdtool {} [--daemon <addr> [--noflush]] [--recursive] <directory>",
            args[0]
        )
        .into());
    };
    let directory = PathBuf::from(directory);
    // rrdc_connect treats an empty address as none (rrd_client.c:956).
    if let Some(address) = daemon
        .clone()
        .or_else(|| std::env::var("RRDCACHED_ADDRESS").ok())
        .filter(|address| !address.is_empty())
    {
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
    if daemon.is_some() {
        // rrd_list.c:340: an explicit address that did not connect.
        eprintln!("Error connecting to rrdcached");
        return Ok(());
    }
    // rrd_list.c:350: a failed listing prints strerror(errno) without a
    // newline and the command still succeeds.
    match rrd_list_r(recursive, &directory.to_string_lossy()) {
        Ok(Some(text)) => print!("{text}"),
        Ok(None) => eprint!("{}", system_error_text(0)),
        Err(errno) => eprint!("{}", system_error_text(errno)),
    }
    Ok(())
}

fn escape_rrdcached_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\ ")
}

/// get_path can return NULL without setting rrd_error (empty local paths
/// and a remote strip-prefix mismatch). FIRST and LAST then print -1
/// without making rrd_tool.c's final rrd_test_error check fail.
#[derive(Debug)]
struct RrdcachedPathUnavailable;

impl std::fmt::Display for RrdcachedPathUnavailable {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for RrdcachedPathUnavailable {}

fn rrdcached_time_value(
    address: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = send_rrdcached_command(address, command)?;
    let verb = command.split_whitespace().next().unwrap_or_default();
    response
        .split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .ok_or_else(|| format!("invalid rrdcached {verb} response").into())
}

/// Port of RRDtool 1.11.0 rrd_client.c `get_path` (105-193). A local
/// daemon receives a canonical absolute path; a remote daemon receives its
/// own relative path after the configured literal prefix is stripped.
fn rrdcached_client_path(
    address: &str,
    filename: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if address.starts_with('/') || address.starts_with("unix:") {
        if filename.is_empty() {
            return Err(RrdcachedPathUnavailable.into());
        }
        if let Ok(path) = std::fs::canonicalize(filename) {
            return Ok(path.to_string_lossy().into_owned());
        }
        // get_path also handles files that CREATE has not made yet. Keep
        // its last-slash rule, including its "." directory for /new.rrd.
        let slash = filename.rfind('/');
        let directory = match slash {
            Some(index) if index > 0 => &filename[..index],
            _ => ".",
        };
        let directory = std::fs::canonicalize(directory).map_err(|error| {
            let text = error.to_string();
            let detail = text.split(" (os error ").next().unwrap_or(&text);
            format!("realpath({filename}): {detail}")
        })?;
        let suffix = slash.map_or_else(
            || format!("/{filename}"),
            |index| filename[index..].to_owned(),
        );
        return Ok(format!("{}{suffix}", directory.display()));
    }
    if filename.starts_with('/') {
        return match std::env::var("RRDCACHED_STRIPPATH") {
            Ok(prefix) => filename
                .strip_prefix(&prefix)
                .map(|path| path.trim_start_matches('/').to_owned())
                .ok_or_else(|| RrdcachedPathUnavailable.into()),
            Err(_) => Err("absolute path names not allowed when talking to a remote daemon".into()),
        };
    }
    Ok(filename.to_owned())
}

/// Translate the file field after connecting, preserving rrd_client.c's
/// connection-error precedence. LIST uses daemon-relative directory names
/// directly upstream and deliberately does not pass through get_path.
fn translate_rrdcached_file_command(
    address: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let Some((verb, fields)) = command.split_once(' ') else {
        return Ok(command.to_owned());
    };
    if !matches!(
        verb,
        "CREATE" | "FETCH" | "FIRST" | "FLUSH" | "FORGET" | "INFO" | "LAST" | "TUNE"
    ) {
        return Ok(command.to_owned());
    }
    let mut filename = String::new();
    let mut escaped = false;
    let mut end = fields.len();
    for (index, character) in fields.char_indices() {
        if escaped {
            filename.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == ' ' {
            end = index;
            break;
        } else {
            filename.push(character);
        }
    }
    if escaped {
        return Err("invalid rrdcached filename escape".into());
    }
    let path = rrdcached_client_path(address, &filename)?;
    if path.contains(['\n', '\r', '\0']) {
        return Err("invalid rrdcached socket or filename".into());
    }
    Ok(format!(
        "{verb} {}{}",
        escape_rrdcached_field(&path),
        &fields[end..]
    ))
}

#[cfg(unix)]
fn send_rrdcached_multiline_command(
    address: &str,
    command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = connect_rrdcached(address)?;
    let command = translate_rrdcached_file_command(address, command)?;
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

/// Port of RRDtool 1.11.0 rrd_client.c `client_connect` (868-878). Only a
/// `unix:` prefix or a leading slash names a socket path; everything else,
/// including a bare host, is a network address.
#[cfg(unix)]
fn open_rrdcached_stream(
    address: &str,
) -> Result<Box<dyn RrdcachedStream>, Box<dyn std::error::Error>> {
    if let Some(path) = address.strip_prefix("unix:") {
        return Ok(Box::new(connect_rrdcached_unix(path)?));
    }
    if address.starts_with('/') {
        return Ok(Box::new(connect_rrdcached_unix(address)?));
    }
    let (host, port) = split_rrdcached_network_address(address)?;
    Ok(Box::new(connect_rrdcached_network(&host, &port)?))
}

/// `connect_unix` (782-814). The path is truncated to `sun_path` and the
/// whole structure goes to connect(2), so an empty or overlong path fails
/// the way the kernel reports it rather than being rejected here.
#[cfg(unix)]
fn connect_rrdcached_unix(path: &str) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::FromRawFd;

    // SAFETY: the descriptor is owned by `stream` as soon as it exists, so
    // every return closes it, and `address` is a fully initialised
    // sockaddr_un whose size is passed to connect.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = std::os::unix::net::UnixStream::from_raw_fd(fd);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let mut address = std::mem::zeroed::<libc::sockaddr_un>();
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let capacity = address.sun_path.len() - 1;
        for (slot, byte) in address.sun_path.iter_mut().zip(path.bytes().take(capacity)) {
            *slot = byte as libc::c_char;
        }
        let status = libc::connect(
            fd,
            std::ptr::addr_of!(address).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        );
        if status != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(stream)
    }
}

/// The host and service `connect_network` (816-856) hands to getaddrinfo:
/// `[v6]` or `[v6]:port`, otherwise a split at the last colon, with port
/// 42217 when none is given.
#[cfg(unix)]
fn split_rrdcached_network_address(
    address: &str,
) -> Result<(Vec<u8>, Vec<u8>), Box<dyn std::error::Error>> {
    // addr_copy is NI_MAXHOST bytes including its terminator.
    const NI_MAXHOST: usize = 1025;
    const RRDCACHED_DEFAULT_PORT: &[u8] = b"42217";

    let bytes = address.as_bytes();
    let bytes = &bytes[..bytes.len().min(NI_MAXHOST - 1)];
    let (host, port) = if let Some(rest) = bytes.strip_prefix(b"[") {
        let Some(close) = rest.iter().position(|byte| *byte == b']') else {
            return Err(format!("malformed address: {address}").into());
        };
        let after = &rest[close + 1..];
        let port = match after.first() {
            None => None,
            Some(b':') => Some(&after[1..]),
            Some(_) => {
                return Err(
                    format!("garbage after address: {}", String::from_utf8_lossy(after)).into(),
                );
            }
        };
        (&rest[..close], port)
    } else if let Some(colon) = bytes.iter().rposition(|byte| *byte == b':') {
        (&bytes[..colon], Some(&bytes[colon + 1..]))
    } else {
        (bytes, None)
    };
    Ok((
        host.to_vec(),
        port.unwrap_or(RRDCACHED_DEFAULT_PORT).to_vec(),
    ))
}

/// `connect_network` (857-896): try each resolved address in turn and
/// report the errno of the last failed attempt.
#[cfg(unix)]
fn connect_rrdcached_network(
    host: &[u8],
    port: &[u8],
) -> Result<std::net::TcpStream, Box<dyn std::error::Error>> {
    use std::ffi::{CStr, CString};
    use std::os::fd::FromRawFd;

    let c_host = CString::new(host)?;
    let c_port = CString::new(port)?;
    // SAFETY: getaddrinfo receives NUL-terminated strings and zeroed hints,
    // its list is only walked while alive and is freed after a successful
    // call, and each descriptor is owned by a TcpStream as soon as it
    // exists.
    unsafe {
        let mut hints = std::mem::zeroed::<libc::addrinfo>();
        hints.ai_flags = libc::AI_ADDRCONFIG;
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut list: *mut libc::addrinfo = std::ptr::null_mut();
        let status = libc::getaddrinfo(c_host.as_ptr(), c_port.as_ptr(), &hints, &mut list);
        if status != 0 {
            let reason = CStr::from_ptr(libc::gai_strerror(status)).to_string_lossy();
            return Err(format!(
                "failed to resolve address '{}' (port {}): {reason} ({status})",
                String::from_utf8_lossy(host),
                String::from_utf8_lossy(port)
            )
            .into());
        }
        let mut last_error = std::io::Error::from_raw_os_error(0);
        let mut entry = list;
        let mut connected = None;
        while let Some(info) = entry.as_ref() {
            entry = info.ai_next;
            let fd = libc::socket(info.ai_family, info.ai_socktype, info.ai_protocol);
            if fd < 0 {
                last_error = std::io::Error::last_os_error();
                continue;
            }
            let stream = std::net::TcpStream::from_raw_fd(fd);
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            if libc::connect(fd, info.ai_addr, info.ai_addrlen) != 0 {
                last_error = std::io::Error::last_os_error();
                continue;
            }
            connected = Some(stream);
            break;
        }
        libc::freeaddrinfo(list);
        connected.ok_or_else(|| last_error.into())
    }
}

#[cfg(not(unix))]
fn send_rrdcached_multiline_command(
    _address: &str,
    _command: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    Err("rrdcached Unix socket commands are unavailable on this platform".into())
}

/// `rrd_list_r` (rrd_list.c:150). `Ok(None)` is an empty listing, which
/// upstream reports through errno 0; `Err` carries errno.
#[cfg(unix)]
fn rrd_list_r(recursive: bool, dirname: &str) -> Result<Option<String>, libc::c_int> {
    // Prevent moving up the directory tree.
    if dirname.contains("..") {
        return Err(libc::EACCES);
    }
    let c_dirname = std::ffi::CString::new(dirname).map_err(|_| libc::ENOENT)?;
    if dirname.contains(['*', '?']) {
        if recursive {
            return Err(libc::EINVAL);
        }
        let mut out = String::new();
        // SAFETY: glob fills `buffer`, its paths are read while it is alive,
        // and globfree releases it on every path.
        unsafe {
            let mut buffer = std::mem::zeroed::<libc::glob_t>();
            if libc::glob(c_dirname.as_ptr(), 0, None, &mut buffer) != 0 {
                libc::globfree(&mut buffer);
                return Err(libc::ENOENT);
            }
            for index in 0..buffer.gl_pathc as usize {
                let path = std::ffi::CStr::from_ptr(*buffer.gl_pathv.add(index)).to_bytes();
                if let Some(slash) = path.iter().rposition(|byte| *byte == b'/') {
                    out.push_str(&String::from_utf8_lossy(&path[slash + 1..]));
                    out.push('\n');
                }
            }
            libc::globfree(&mut buffer);
        }
        return if out.is_empty() {
            Err(libc::ENOENT)
        } else {
            Ok(Some(out))
        };
    }
    // A name whose first ".rrd" ends it (strstr) is one file.
    if dirname
        .find(".rrd")
        .is_some_and(|index| index + 4 == dirname.len())
    {
        let metadata = c_stat(&c_dirname)?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(libc::ENXIO);
        }
        return match dirname.rfind('/') {
            Some(slash) => Ok(Some(format!("{}\n", &dirname[slash + 1..]))),
            None => Err(libc::EINVAL),
        };
    }
    let metadata = c_stat(&c_dirname)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(libc::ENOTDIR);
    }
    rrd_list_rec(recursive, dirname.as_bytes(), dirname.as_bytes())
}

#[cfg(unix)]
fn c_stat(path: &std::ffi::CStr) -> Result<libc::stat, libc::c_int> {
    // SAFETY: stat writes one struct into the zeroed buffer.
    unsafe {
        let mut status = std::mem::zeroed::<libc::stat>();
        if libc::stat(path.as_ptr(), &mut status) != 0 {
            return Err(last_os_errno());
        }
        Ok(status)
    }
}

/// `rrd_list_rec` (rrd_list.c:39): readdir order, directories listed unless
/// recursing, regular files only with an `.rrd` suffix.
#[cfg(unix)]
fn rrd_list_rec(
    recursive: bool,
    root: &[u8],
    dirname: &[u8],
) -> Result<Option<String>, libc::c_int> {
    let c_dirname = std::ffi::CString::new(dirname).map_err(|_| libc::ENOENT)?;
    let mut out = None::<String>;
    // SAFETY: the directory handle is owned here and closed before return;
    // each entry name is NUL-terminated.
    unsafe {
        let dir = libc::opendir(c_dirname.as_ptr());
        if dir.is_null() {
            return Err(last_os_errno());
        }
        loop {
            let entry = libc::readdir(dir);
            if entry.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if dirname.len() + name.len() + 1 >= libc::PATH_MAX as usize {
                continue;
            }
            let current = [dirname, b"/", name].concat();
            let Ok(c_current) = std::ffi::CString::new(current.clone()) else {
                continue;
            };
            let Ok(status) = c_stat(&c_current) else {
                continue;
            };
            let kind = status.st_mode & libc::S_IFMT;
            if kind == libc::S_IFDIR && recursive {
                if let Ok(Some(nested)) = rrd_list_rec(recursive, root, &current) {
                    out.get_or_insert_with(String::new).push_str(&nested);
                }
                continue;
            }
            if kind == libc::S_IFREG {
                let suffix = name.windows(4).position(|window| window == b".rrd");
                if suffix.is_none_or(|index| index + 4 != name.len()) {
                    continue;
                }
            }
            // move_past_prefix: skip the characters shared with the root.
            let mut short = &current[..];
            if root.len() <= current.len() {
                let shared = root
                    .iter()
                    .zip(&current)
                    .take_while(|(left, right)| left == right)
                    .count();
                short = &current[shared..];
            }
            if short.first() == Some(&b'/') {
                short = &short[1..];
            }
            let out = out.get_or_insert_with(String::new);
            out.push_str(&String::from_utf8_lossy(short));
            out.push('\n');
        }
        libc::closedir(dir);
    }
    Ok(out)
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
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|pixel| *pixel == &[255, 0, 0])
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
            .as_chunks::<3>()
            .0
            .iter()
            .enumerate()
            .filter_map(|(x, p)| (p == &[0, 0, 0]).then_some(x))
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
                        surface_width: 121,
                        surface_height: 71,
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
        let contains_red = |pixels: &[u8]| {
            pixels
                .as_chunks::<3>()
                .0
                .iter()
                .any(|pixel| pixel == &[255, 0, 0])
        };
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
        assert!(
            diagonal
                .as_chunks::<3>()
                .0
                .iter()
                .any(|pixel| pixel == &[0, 0, 0])
        );
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
    fn network_daemon_addresses_split_like_connect_network() {
        let split = |address: &str| {
            super::split_rrdcached_network_address(address)
                .map(|(host, port)| {
                    (
                        String::from_utf8(host).unwrap(),
                        String::from_utf8(port).unwrap(),
                    )
                })
                .map_err(|error| error.to_string())
        };
        let pair = |host: &str, port: &str| Ok((host.to_owned(), port.to_owned()));
        assert_eq!(split("127.0.0.1"), pair("127.0.0.1", "42217"));
        assert_eq!(split("localhost"), pair("localhost", "42217"));
        assert_eq!(split("[::1]"), pair("::1", "42217"));
        assert_eq!(split("[::1]:9"), pair("::1", "9"));
        assert_eq!(split("host:"), pair("host", ""));
        assert_eq!(split("tcp:host:9"), pair("tcp:host", "9"));
        assert_eq!(split("::1"), pair(":", "1"));
        assert_eq!(split("[::1"), Err("malformed address: [::1".to_owned()));
        assert_eq!(split("[::1]x"), Err("garbage after address: x".to_owned()));
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
