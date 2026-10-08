//! Shared integration-test helpers. Each test binary compiles its own copy,
//! so unused items are expected.
#![allow(dead_code)]

use std::process::Command;
use std::time::Duration;

/// Report a differential test that cannot reach its oracle. CI sets
/// `RONDI_REQUIRE_ORACLE=1` so a missing or wrong oracle fails the run
/// instead of letting the test pass without comparing anything.
pub fn skipped(reason: String) {
    if oracle_required() {
        panic!("{reason}; RONDI_REQUIRE_ORACLE=1 forbids skipping");
    }
    eprintln!("{reason}");
}

pub fn oracle_required() -> bool {
    std::env::var_os("RONDI_REQUIRE_ORACLE").is_some_and(|value| value == "1")
}

/// Panics unless `rrdtool` on PATH is the pinned 1.11.0 release.
pub fn require_oracle() {
    let output = Command::new("rrdtool")
        .arg("--version")
        .output()
        .expect("rrdtool is not on PATH");
    let banner = String::from_utf8_lossy(&output.stdout);
    assert!(
        banner.starts_with("RRDtool 1.11.0 "),
        "expected the pinned RRDtool 1.11.0 oracle, found: {}",
        banner.lines().next().unwrap_or_default()
    );
}

/// Upper bound for one blocking read or startup wait in tests that talk to a
/// daemon. Loaded CI hosts can stall a reply for seconds, so the default is
/// generous; a correct reply never waits for it. Override with
/// `RONDI_TEST_TIMEOUT_SECS`.
pub fn io_timeout() -> Duration {
    let seconds = std::env::var("RONDI_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(seconds)
}

#[allow(unused_macros)]
macro_rules! oracle_skip {
    ($($arg:tt)*) => {
        $crate::common::skipped(format!($($arg)*))
    };
}

/// One command's observable result.
#[cfg(unix)]
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Two directories holding byte-identical fixtures: `up` for the pinned
/// RRDtool and `ro` for the Rondi alias. Commands run with TZ=UTC, LC_ALL=C
/// and no RRDCACHED_ADDRESS unless the caller overrides them.
#[cfg(unix)]
pub struct Sides {
    _temp: tempfile::TempDir,
    pub up: std::path::PathBuf,
    pub ro: std::path::PathBuf,
    rondi: std::path::PathBuf,
}

#[cfg(unix)]
impl Sides {
    /// Runs each `setup` command with upstream rrdtool in a seed directory
    /// next to the given files, then copies the result to both sides.
    /// Returns None when the oracle is missing and skipping is allowed.
    pub fn new(setup: &[&[&str]], files: &[(&str, &[u8])]) -> Option<Self> {
        if Command::new("rrdtool").arg("--version").output().is_err() {
            skipped("skipping RRDtool differential test: rrdtool is not installed".into());
            return None;
        }
        require_oracle();
        let temp = tempfile::tempdir().unwrap();
        let seed = temp.path().join("seed");
        let up = temp.path().join("up");
        let ro = temp.path().join("ro");
        let bin = temp.path().join("bin");
        for dir in [&seed, &up, &ro, &bin] {
            std::fs::create_dir(dir).unwrap();
        }
        for (name, contents) in files {
            std::fs::write(seed.join(name), contents).unwrap();
        }
        for args in setup {
            let output = Command::new("rrdtool")
                .args(*args)
                .current_dir(&seed)
                .env("TZ", "UTC")
                .output()
                .unwrap();
            assert!(output.status.success(), "setup {args:?}: {output:?}");
        }
        for entry in std::fs::read_dir(&seed).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), up.join(entry.file_name())).unwrap();
                std::fs::copy(entry.path(), ro.join(entry.file_name())).unwrap();
            }
        }
        let rondi = bin.join("rrdtool");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
        Some(Self {
            _temp: temp,
            up,
            ro,
            rondi,
        })
    }

    /// The Rondi binary under the `rrdtool` name.
    pub fn rondi(&self) -> &std::path::Path {
        &self.rondi
    }

    /// Writes the same file into both sides.
    pub fn write(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.up.join(name), contents).unwrap();
        std::fs::write(self.ro.join(name), contents).unwrap();
    }

    fn run(
        program: &std::path::Path,
        dir: &std::path::Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Outcome {
        let output = Command::new(program)
            .args(args)
            .current_dir(dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        Outcome {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Runs `args` on both sides and returns (upstream, rondi).
    pub fn both(&self, args: &[&str], env: &[(&str, &str)]) -> (Outcome, Outcome) {
        (
            Self::run(std::path::Path::new("rrdtool"), &self.up, args, env),
            Self::run(&self.rondi, &self.ro, args, env),
        )
    }

    pub fn assert_same(&self, args: &[&str]) {
        let (up, ro) = self.both(args, &[]);
        assert_eq!(ro, up, "args: {args:?}");
    }

    /// Asserts that `name` has the same bytes, or is missing, on both sides.
    pub fn assert_same_file(&self, name: &str) {
        let up = std::fs::read(self.up.join(name)).ok();
        let ro = std::fs::read(self.ro.join(name)).ok();
        assert!(
            ro == up,
            "{name} differs (upstream {:?} bytes, rondi {:?} bytes)",
            up.as_ref().map(Vec::len),
            ro.as_ref().map(Vec::len)
        );
    }
}
