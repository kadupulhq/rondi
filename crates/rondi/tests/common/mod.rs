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
