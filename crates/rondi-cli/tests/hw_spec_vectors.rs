#![cfg(unix)]
//! Frozen vectors for files with Holt-Winters archives: info, dump, fetch,
//! update and updatev. The behavior is specified in
//! docs/compatibility/holt-winters.md; the vectors under tests/data/hw were
//! recorded from RRDtool 1.11.0 by scripts/gen-create-resize-vectors.py.
//! Rondi implements info, dump, update, updatev and tune of these files,
//! and fetch reads their rows as it does a classic archive's, so those
//! cases are expected to match RRDtool.
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
