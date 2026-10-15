//! Typed daemon settings, produced by the command-line front end.

use std::ffi::CString;
use std::fs::File;
use std::path::PathBuf;

/// Socket used when no `-l` or `-L` is given.
pub const DEFAULT_SOCKET: &str = "/tmp/rrdcached.sock";

/// Port used by network listeners that do not name one.
pub const DEFAULT_PORT: &str = "42217";

/// syslog priorities, least verbose first, as `-V` names them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Emerg,
    Alert,
    Crit,
    #[default]
    Err,
    Warning,
    Notice,
    Info,
    Debug,
}

impl LogLevel {
    const NAMES: [(&'static str, LogLevel); 8] = [
        ("LOG_EMERG", LogLevel::Emerg),
        ("LOG_ALERT", LogLevel::Alert),
        ("LOG_CRIT", LogLevel::Crit),
        ("LOG_ERR", LogLevel::Err),
        ("LOG_WARNING", LogLevel::Warning),
        ("LOG_NOTICE", LogLevel::Notice),
        ("LOG_INFO", LogLevel::Info),
        ("LOG_DEBUG", LogLevel::Debug),
    ];

    /// The level for a `-V` argument. Names are matched exactly.
    // Compatibility: `log_debug`, `debug` and `7` are all rejected.
    // Provenance: black-box compatibility test.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::NAMES
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, level)| *level)
    }
}

/// A `-w`, `-f` or `-z` value in seconds.
///
/// The reference keeps these in a C `int`, so values past `i32::MAX` wrap
/// before its startup warnings compare them. [`Interval::as_c_int`] gives
/// that wrapped value; timers use [`Interval::timer_seconds`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval(i64);

impl Interval {
    pub const fn from_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    pub const fn seconds(self) -> i64 {
        self.0
    }

    // Compatibility: `-w 2147483648` is accepted and then trips the `-z`
    // warning, because the stored int is negative.
    // Provenance: black-box compatibility test.
    pub const fn as_c_int(self) -> i32 {
        self.0 as i32
    }

    /// The period a timer uses: at least one second and at most what a C
    /// `int` holds, so a wrapped or huge value never yields a zero or
    /// unbounded timer.
    pub fn timer_seconds(self) -> u64 {
        self.0.clamp(1, i64::from(i32::MAX)) as u64
    }
}

/// Where a listener binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListenAddress {
    /// A Unix socket path, possibly relative to the starting directory.
    Unix(PathBuf),
    /// `host`, `host:port`, `[v6]:port`, or empty for every interface.
    Network(String),
}

impl ListenAddress {
    /// Reads an `-l` argument: `unix:<path>` or a path starting with `/`
    /// is a Unix socket, anything else a network address.
    pub fn parse(text: &str) -> Self {
        if let Some(path) = text.strip_prefix("unix:") {
            Self::Unix(PathBuf::from(path))
        } else if text.starts_with('/') {
            Self::Unix(PathBuf::from(text))
        } else {
            Self::Network(text.to_owned())
        }
    }
}

/// `-m` and `-s` as they stand when a listener is declared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SocketAccess {
    pub mode: Option<u32>,
    pub group: Option<u32>,
}

impl SocketAccess {
    /// The mode a Unix socket gets.
    // Compatibility: `-s` alone gives 0760 and `-m` overrides it in either
    // order. Provenance: rrdcached(1) and black-box compatibility test.
    // Rondi policy: without either, the socket is 0600 rather than what the
    // umask leaves (RCD-5).
    pub fn mode(&self) -> u32 {
        match (self.mode, self.group) {
            (Some(mode), _) => mode,
            (None, Some(_)) => 0o760,
            (None, None) => 0o600,
        }
    }
}

/// One `-l` or `-L`, with the `-m`, `-s` and `-P` settings before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerSpec {
    pub address: ListenAddress,
    pub access: SocketAccess,
    /// Commands the listener accepts; `None` accepts every command.
    pub permissions: Option<Vec<String>>,
}

/// A `-U` account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub name: CString,
    pub uid: u32,
    pub gid: u32,
}

/// The `-o` file, opened while the options were read.
#[derive(Debug)]
pub struct LogFile {
    pub path: PathBuf,
    pub file: File,
}

/// Everything the daemon needs to start.
#[derive(Debug)]
pub struct DaemonConfig {
    /// `-b`, already created and resolved to its real path.
    pub base: Option<PathBuf>,
    /// `-B`.
    pub confine_to_base: bool,
    /// `-R`.
    pub recursive_create: bool,
    /// `-O`.
    pub no_overwrite: bool,
    /// `-F`.
    pub flush_at_shutdown: bool,
    pub write_interval: Interval,
    pub flush_interval: Interval,
    pub write_jitter: Interval,
    pub write_threads: u32,
    pub allocation_chunk: usize,
    /// `-j`, resolved to its real path.
    pub journal: Option<PathBuf>,
    pub pid_file: Option<PathBuf>,
    pub log_file: Option<LogFile>,
    pub log_level: LogLevel,
    pub listeners: Vec<ListenerSpec>,
    pub user: Option<Account>,
    pub group: Option<u32>,
    /// Upper bound on the memory held by pending values.
    pub max_pending_bytes: usize,
}

impl Default for DaemonConfig {
    // Compatibility: `-w 300`, `-f 3600`, `-t 4`, `-a 1` (RCD-2).
    // Provenance: rrdcached(1).
    fn default() -> Self {
        Self {
            base: None,
            confine_to_base: false,
            recursive_create: false,
            no_overwrite: false,
            flush_at_shutdown: false,
            write_interval: Interval::from_seconds(300),
            flush_interval: Interval::from_seconds(3600),
            write_jitter: Interval::from_seconds(0),
            write_threads: 4,
            allocation_chunk: 1,
            journal: None,
            pid_file: None,
            log_file: None,
            log_level: LogLevel::default(),
            listeners: Vec::new(),
            user: None,
            group: None,
            max_pending_bytes: super::super::DEFAULT_RRDCACHED_QUEUE_BYTES,
        }
    }
}

impl DaemonConfig {
    /// Startup warnings, one entry per message, in the order printed.
    // Compatibility: the warnings and their order (RCD-2); `-B` without
    // `-b` replaces the `-R` warning.
    // Provenance: black-box compatibility test.
    pub fn warnings(&self) -> Vec<&'static str> {
        let write = self.write_interval.as_c_int();
        let mut warnings = Vec::new();
        if self.flush_interval.as_c_int() < write.wrapping_mul(2) {
            warnings
                .push("WARNING: flush interval (-f) should be at least 2x write interval (-w) !");
        }
        if self.write_jitter.as_c_int() > write {
            warnings
                .push("WARNING: write delay (-z) should NOT be larger than write interval (-w) !");
        }
        if self.confine_to_base && self.base.is_none() {
            warnings.push(
                "WARNING: -B does not make sense without -b!\n  Consult the rrdcached documentation",
            );
        } else if self.recursive_create && !self.confine_to_base {
            warnings.push(
                "WARNING: -R does not make sense without -B!\n  Consult the rrdcached documentation",
            );
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_match_names_exactly() {
        assert_eq!(LogLevel::from_name("LOG_DEBUG"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::from_name("LOG_ERR"), Some(LogLevel::Err));
        for name in ["log_debug", "DEBUG", "7", "", "LOG_DEBUG "] {
            assert_eq!(LogLevel::from_name(name), None, "{name:?}");
        }
        assert!(LogLevel::Notice < LogLevel::Info);
    }

    #[test]
    fn listen_addresses_split_unix_from_network() {
        assert_eq!(
            ListenAddress::parse("unix:rel.sock"),
            ListenAddress::Unix("rel.sock".into())
        );
        assert_eq!(
            ListenAddress::parse("/run/s.sock"),
            ListenAddress::Unix("/run/s.sock".into())
        );
        assert_eq!(
            ListenAddress::parse("rel.sock"),
            ListenAddress::Network("rel.sock".into())
        );
        assert_eq!(
            ListenAddress::parse(""),
            ListenAddress::Network(String::new())
        );
    }

    #[test]
    fn socket_mode_prefers_dash_m_then_dash_s() {
        let mut access = SocketAccess::default();
        assert_eq!(access.mode(), 0o600);
        access.group = Some(20);
        assert_eq!(access.mode(), 0o760);
        access.mode = Some(0o640);
        assert_eq!(access.mode(), 0o640);
    }

    #[test]
    fn warnings_compare_the_wrapped_c_values() {
        let mut config = DaemonConfig::default();
        assert!(config.warnings().is_empty());
        config.write_interval = Interval::from_seconds(10);
        config.flush_interval = Interval::from_seconds(19);
        config.write_jitter = Interval::from_seconds(11);
        config.recursive_create = true;
        assert_eq!(config.warnings().len(), 3);
        config.flush_interval = Interval::from_seconds(20);
        config.write_jitter = Interval::from_seconds(10);
        config.confine_to_base = true;
        assert_eq!(
            config.warnings(),
            ["WARNING: -B does not make sense without -b!\n  Consult the rrdcached documentation"]
        );
        config.base = Some(PathBuf::from("/data"));
        assert!(config.warnings().is_empty());
        // 2^31 is negative as a C int, so the zero jitter exceeds it.
        config.write_interval = Interval::from_seconds(1 << 31);
        config.write_jitter = Interval::from_seconds(0);
        assert_eq!(
            config.warnings(),
            ["WARNING: write delay (-z) should NOT be larger than write interval (-w) !"]
        );
    }

    #[test]
    fn timer_periods_stay_positive_and_bounded() {
        assert_eq!(Interval::from_seconds(0).timer_seconds(), 1);
        assert_eq!(Interval::from_seconds(-7).timer_seconds(), 1);
        assert_eq!(Interval::from_seconds(90).timer_seconds(), 90);
        assert_eq!(
            Interval::from_seconds(i64::MAX).timer_seconds(),
            i32::MAX as u64
        );
    }
}
