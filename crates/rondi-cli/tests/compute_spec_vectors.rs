#![cfg(unix)]
//! Frozen vectors for files with `COMPUTE` data sources: update and
//! updatev, info, dump, lastupdate, last, first, fetch, xport and resize.
//! The behavior is specified in docs/compatibility/compute.md; the vectors
//! under tests/data/compute were recorded from RRDtool 1.11.0 by
//! scripts/gen-create-resize-vectors.py. The definition grammar is pinned
//! by the create vectors and `<cdef>` restore by the restore vectors.
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the cases also run live against it.

mod common;
#[path = "spec_vectors/mod.rs"]
mod spec_vectors;

use std::process::Command;

use spec_vectors::{Expect, Suite};

fn suite() -> Suite {
    Suite::new("compute", false)
}

/// The table pins every area of the specification.
#[test]
fn compute_vectors_cover_the_specification() {
    let suite = suite();
    let cases = suite.cases();
    assert!(cases.len() >= 50, "only {} cases", cases.len());
    let vectors = suite.vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        assert_eq!(
            case.expect,
            Expect::Same,
            "{}: compute cases are exact",
            case.id
        );
    }
    for command in [
        "update",
        "updatev",
        "info",
        "dump",
        "lastupdate",
        "last",
        "first",
        "fetch",
        "xport",
        "resize",
    ] {
        assert!(
            cases.iter().any(|case| case.args[0] == command),
            "no case runs {command}"
        );
    }
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        "-t a:b:c",
        "cpchain.rrd",
        "cpstack.rrd",
        "cpunder.rrd",
        "cpprev.rrd",
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
    assert!(failures >= 9, "only {failures} failing cases");
}

#[test]
fn rondi_matches_frozen_compute_vectors() {
    spec_vectors::replay_rondi(&suite());
}

#[test]
fn rrdtool_reproduces_compute_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live compute vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    spec_vectors::replay_oracle(&suite());
}
