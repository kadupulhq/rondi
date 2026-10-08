#![cfg(unix)]

mod common;

use std::process::Command;

/// Under `RONDI_REQUIRE_ORACLE=1` every external tool the differential tests
/// compare against must be present, so none of them can pass by skipping.
#[test]
fn required_oracles_are_installed() {
    if !common::oracle_required() {
        eprintln!("RONDI_REQUIRE_ORACLE is not set; oracle presence is not enforced");
        return;
    }
    common::require_oracle();
    // `rrdcached -h` exits 1 upstream, so only the spawn is checked.
    Command::new("rrdcached")
        .arg("-h")
        .output()
        .expect("rrdcached is required");
    let php = Command::new("php")
        .arg("-v")
        .output()
        .expect("php is required for the rrdtool-proxy.php launcher test");
    assert!(php.status.success(), "php -v failed: {}", php.status);
}
