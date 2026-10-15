//! The `rrdcached` daemon: startup, listeners, privileges, the pid file,
//! signals, timers and shutdown.
//!
//! The behavior follows docs/compatibility/rrdcached-daemon.md (`RCD-n`
//! rules), written from the rrdcached(1) manual and black-box runs of
//! rrdcached 1.11.0. The command line is read by the `rrdcached`
//! personality of the `rondi` executable, which builds a [`DaemonConfig`]
//! and calls [`run`]. Requests, the cache and the journal are reached only
//! through the engine adapter in `engine.rs`.

mod accounts;
mod config;
mod daemon;
mod engine;
mod listener;
mod log;
mod paths;
mod pidfile;
mod signals;

pub use accounts::{lookup_group, lookup_socket_group, lookup_user};
pub use config::{
    Account, DEFAULT_PORT, DEFAULT_SOCKET, DaemonConfig, Interval, ListenAddress, ListenerSpec,
    LogFile, LogLevel, SocketAccess,
};
pub use paths::{open_log_file, prepare_base_directory, resolve_journal_directory};

pub(crate) use listener::open_json_listener;

use config::LogLevel as Level;
use daemon::{Daemon, Engine, Timing};
use engine::{EngineSettings, ProtocolEngine};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// An option value that stops startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionFailure {
    /// The process exit status.
    pub status: i32,
    /// The text for standard error, without a final newline.
    pub message: String,
}

impl OptionFailure {
    pub(crate) fn new(status: i32, message: String) -> Self {
        Self { status, message }
    }
}

/// Whether `-P` accepts `word` as a command name.
pub fn is_command(word: &str) -> bool {
    ProtocolEngine::knows_command(word)
}

/// The C library's text for `errno`, as the reference prints it.
pub(crate) fn errno_text(errno: i32) -> String {
    error_text(&std::io::Error::from_raw_os_error(errno))
}

/// An I/O error's text without Rust's ` (os error N)` suffix.
pub(crate) fn error_text(error: &std::io::Error) -> String {
    let text = error.to_string();
    match (error.raw_os_error(), text.rfind(" (os error ")) {
        (Some(_), Some(at)) => text[..at].to_owned(),
        _ => text,
    }
}

/// Write permission for the group and others: never left on a pid file,
/// log file or directory the daemon creates.
// Rondi policy (RCD-13): the reference creates these with the mode the
// umask leaves, so a umask looser than 022 makes them group or world
// writable; Rondi removes those bits, which changes nothing under the
// usual umask 022.
const SHARED_WRITE: u32 = 0o022;

/// Removes group and other write permission from an open file.
pub(crate) fn drop_shared_write(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let mode = file.metadata()?.mode() & 0o7777;
    if mode & SHARED_WRITE == 0 {
        return Ok(());
    }
    // SAFETY: fchmod on a descriptor this process holds.
    if unsafe { libc::fchmod(file.as_raw_fd(), (mode & !SHARED_WRITE) as libc::mode_t) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Creates `path` and its missing parents with the mode the umask leaves,
/// less group and other write permission.
pub(crate) fn create_directories(path: &std::path::Path) -> std::io::Result<()> {
    let missing = path
        .ancestors()
        .take_while(|ancestor| {
            !ancestor.as_os_str().is_empty() && std::fs::symlink_metadata(ancestor).is_err()
        })
        .collect::<Vec<_>>();
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => drop_shared_write(&listener::open_directory(directory)?)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    if !path.is_dir() {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    Ok(())
}

const DAEMONIZE_FAILED: &str = "rrdcached: daemonize failed, exiting.";

/// Runs the daemon until a signal stops it and returns the exit status.
///
/// Call before the process starts any other thread: sockets are bound and
/// the process identity changes before the runtime exists.
// Compatibility: the startup order (pid file, listeners, identity change,
// journal replay before connections are accepted) and the
// `daemonize failed` endings (RCD-4, RCD-10, RCD-11, RCD-31).
// Provenance: rrdcached(1) and black-box compatibility test.
pub fn run(config: DaemonConfig) -> i32 {
    let mut config = config;
    let log = Arc::new(log::Log::start(config.log_level, config.log_file.take()));
    let blocked = signals::BlockedSignals::block();
    log.event(Level::Info, "rrdcached_started", "starting up");
    let mut stderr = std::io::stderr();

    // Rondi policy: a daemon that would serve as root must be confined to
    // its base directory (RCD-5).
    if accounts::serves_as_root(config.user.as_ref())
        && !(config.confine_to_base && config.base.is_some())
    {
        let _ = writeln!(
            stderr,
            "rrdcached: refuses to run as root without -B and -b; use -U and -G or confine it"
        );
        return 1;
    }

    // The process never changes directory, so a relative pid file stays
    // where it was created and its guard can remove it.
    let pid_file = match config.pid_file.as_deref() {
        Some(path) => match pidfile::PidFile::create(path, &mut stderr) {
            Ok(guard) => Some(guard),
            Err(fatal) => {
                let _ = writeln!(stderr, "{}\n{DAEMONIZE_FAILED}", fatal.0);
                return 1;
            }
        },
        None => None,
    };
    let Ok(listeners) = listener::open_all(&config.listeners, &mut stderr, true) else {
        let _ = writeln!(stderr, "{DAEMONIZE_FAILED}");
        return 1;
    };
    if let Err(message) = accounts::change_identity(config.user.as_ref(), config.group) {
        let _ = writeln!(stderr, "{message}\n{DAEMONIZE_FAILED}");
        return 1;
    }
    // Rondi policy: one daemon owns a base directory at a time; the lock is
    // taken as the serving account (RCD-3).
    let base_lock = match &config.base {
        Some(base) => match rondi::Store::open(base) {
            Ok(store) => Some(store),
            Err(error) => {
                let _ = writeln!(
                    stderr,
                    "rrdcached: cannot lock base directory '{}': {error}",
                    base.display()
                );
                return 1;
            }
        },
        None => None,
    };
    if config.journal.is_some() {
        log.event(
            Level::Info,
            "rrdcached_journal_replay",
            "checking for journal files",
        );
    }
    let opened = ProtocolEngine::open(&EngineSettings {
        base: config.base.as_deref(),
        confine_to_base: config.confine_to_base,
        journal: config.journal.as_deref(),
        max_pending_bytes: config.max_pending_bytes,
        write_interval: config.write_interval.timer_seconds(),
        flush_at_shutdown: config.flush_at_shutdown,
        allocation_chunk: config.allocation_chunk,
        no_overwrite: config.no_overwrite,
        recursive_create: config.recursive_create,
    });
    let (engine, replayed) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = writeln!(stderr, "rrdcached: {error}");
            return 1;
        }
    };
    if config.journal.is_some() {
        log.event(
            Level::Info,
            "rrdcached_journal_replayed",
            "journal processing complete",
        );
    }
    // Compatibility: with `-F`, values replayed from the journal are
    // written before the first client is served (journal-008).
    // Provenance: black-box compatibility test.
    if replayed && config.flush_at_shutdown {
        for path in engine.pending() {
            engine.write(&path);
        }
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(stderr, "rrdcached: cannot start runtime: {error}");
            return 1;
        }
    };
    let timing = Timing {
        write_threads: config.write_threads as usize,
        flush_interval: Duration::from_secs(config.flush_interval.timer_seconds()),
        write_jitter: Duration::from_secs(
            u64::try_from(config.write_jitter.seconds()).unwrap_or(0),
        ),
        journaled: config.journal.is_some(),
    };
    let served = runtime.block_on(async {
        let signals = signals::listen()?;
        drop(blocked);
        log.event(
            Level::Info,
            "rrdcached_listening",
            "listening for connections",
        );
        let daemon = Daemon {
            engine: Arc::new(engine),
            listeners,
            timing,
            log: Arc::clone(&log),
        };
        Ok::<_, std::io::Error>(daemon.run(signals).await)
    });
    runtime.shutdown_timeout(Duration::from_secs(5));
    drop(base_lock);
    drop(pid_file);
    match served {
        Ok(_) => {
            log.event(Level::Info, "rrdcached_stopped", "goodbye");
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "rrdcached: cannot handle signals: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_drops_the_os_error_suffix() {
        assert_eq!(errno_text(libc::ENOENT), "No such file or directory");
        assert_eq!(error_text(&std::io::Error::other("plain")), "plain");
    }

    #[test]
    fn command_names_come_from_the_protocol() {
        for word in [
            "PING", "ping", "BATCH", "FlushAll", "QUIT", "HELP", "WROTE", ".",
        ] {
            assert!(is_command(word), "{word}");
        }
        for word in ["", "BOGUS", "PING STATS", "ping;stats"] {
            assert!(!is_command(word), "{word}");
        }
    }
}
