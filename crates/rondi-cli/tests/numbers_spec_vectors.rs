#![cfg(unix)]
//! Frozen vectors for decimal number conversion: `update` readings and
//! times, and `tune --minimum` and `--maximum`. The behavior is specified in
//! docs/compatibility/numbers.md; the vectors under tests/data/numbers were
//! recorded from RRDtool 1.11.0 by scripts/gen-create-resize-vectors.py.
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the cases also run live against it.

mod common;
#[path = "spec_vectors/mod.rs"]
mod spec_vectors;

use std::process::Command;

use spec_vectors::{Expect, Suite};

fn suite() -> Suite {
    Suite::new("numbers", false)
}

/// The table pins every area of the specification.
#[test]
fn numbers_vectors_cover_the_specification() {
    let suite = suite();
    let cases = suite.cases();
    assert!(cases.len() >= 190, "only {} cases", cases.len());
    let vectors = suite.vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        assert_eq!(
            case.expect,
            Expect::Same,
            "{}: number cases are exact",
            case.id
        );
    }
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        "update",
        "tune gauge.rrd -i",
        "tune gauge.rrd -a",
        "e-1022",
        "e1025",
    ] {
        assert!(
            joined.iter().any(|args| args.contains(needle)),
            "no case uses {needle}"
        );
    }
    let failures = vectors
        .values()
        .filter(|sections| sections[0].1 != b"0")
        .count();
    assert!(failures >= 30, "only {failures} failing cases");
}

#[test]
fn rondi_matches_frozen_numbers_vectors() {
    spec_vectors::replay_rondi(&suite());
}

#[test]
fn rrdtool_reproduces_numbers_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live numbers vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    spec_vectors::replay_oracle(&suite());
}
