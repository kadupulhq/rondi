#![cfg(unix)]
//! Frozen vectors for files with Holt-Winters archives: info, dump, fetch,
//! update and updatev. The behavior is specified in
//! docs/compatibility/holt-winters.md; the vectors under tests/data/hw were
//! recorded from RRDtool 1.11.0 by scripts/gen-create-resize-vectors.py.
//! Rondi implements info, dump, fetch, update and updatev of these files,
//! so those cases are expected to match RRDtool; tune of a file with
//! Holt-Winters archives is still refused (HW-36), so its cases are
//! expected to fail cleanly and leave the file unchanged.
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the cases also run live against it.

mod common;
#[path = "spec_vectors/mod.rs"]
mod spec_vectors;

use std::process::Command;

use spec_vectors::{Expect, Suite};

fn suite() -> Suite {
    Suite::new("hw", false)
}

/// Every archive type and command of the first two stages has a case.
#[test]
fn hw_vectors_cover_the_specification() {
    let suite = suite();
    let cases = suite.cases();
    let vectors = suite.vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        assert!(
            matches!(
                case.expect,
                Expect::Same | Expect::Unsupported | Expect::CleanError
            ),
            "{}: unexpected expectation",
            case.id
        );
    }
    for command in ["info", "dump", "fetch", "update", "updatev"] {
        assert!(
            cases.iter().any(|case| case.args[0] == command),
            "no case runs {command}"
        );
    }
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        "HWPREDICT",
        "SEASONAL",
        "DEVSEASONAL",
        "DEVPREDICT",
        "FAILURES",
        "mhwimp.rrd",
        "hwsw0.rrd",
        "hwpdp3.rrd",
        "hw2ds.rrd",
    ] {
        assert!(
            joined.iter().any(|args| args.contains(needle)),
            "no case uses {needle}"
        );
    }
}

#[test]
fn rondi_matches_frozen_hw_vectors() {
    spec_vectors::replay_rondi(&suite());
}

#[test]
fn rrdtool_reproduces_hw_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live Holt-Winters vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    spec_vectors::replay_oracle(&suite());
}

/// Each Holt-Winters `tune` option on a file with the archive it tunes is
/// refused by its long name, as rrdcached's TUNE does, and the file is left
/// unchanged (HW-36).
#[test]
fn tune_refuses_holt_winters_options_by_long_name() {
    let temp = tempfile::tempdir().unwrap();
    let rondi = temp.path().join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
    let input =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hw/inputs/hwimp.rrd");
    let original = std::fs::read(&input).unwrap();
    let path = temp.path().join("hwimp.rrd");
    for (option, value, long) in [
        ("-x", "0.5", "alpha"),
        ("-y", "0.5", "beta"),
        ("-z", "0.5", "gamma"),
        ("-v", "0.5", "gamma-deviation"),
        ("-s", "0.5", "smoothing-window"),
        ("-S", "0.5", "smoothing-window-deviation"),
        ("-p", "2", "deltapos"),
        ("-n", "2", "deltaneg"),
        ("-w", "5", "window-length"),
        ("-f", "3", "failure-threshold"),
        ("-b", "x", "aberrant-reset"),
    ] {
        std::fs::write(&path, &original).unwrap();
        let output = Command::new(&rondi)
            .arg("tune")
            .arg(&path)
            .args([option, value])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{option}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("ERROR: --{long} is not supported by Rondi\n"),
            "{option}"
        );
        assert!(
            std::fs::read(&path).unwrap() == original,
            "{option} changed the file"
        );
    }
}
