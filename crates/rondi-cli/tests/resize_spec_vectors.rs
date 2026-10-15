#![cfg(unix)]
//! Frozen vectors for `rrdtool resize`: argument checks and their order,
//! row insertion and removal relative to the current row, malformed inputs
//! and the fixed `resize.rrd` output. The behavior is specified in
//! docs/compatibility/resize.md; the vectors under tests/data/resize were
//! recorded from RRDtool 1.11.0 by scripts/gen-create-resize-vectors.py.
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the cases also run live against it.

mod common;
#[path = "spec_vectors/mod.rs"]
mod spec_vectors;

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

use spec_vectors::{Expect, Suite};

fn suite() -> Suite {
    Suite::new("resize", false)
}

#[test]
fn resize_vectors_cover_the_specification() {
    let suite = suite();
    let cases = suite.cases();
    assert!(cases.len() >= 60, "only {} cases", cases.len());
    let vectors = suite.vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        if case.expect == Expect::Divergent {
            assert!(
                case.note.starts_with("RZ-div-"),
                "{}: no divergence id",
                case.id
            );
        }
    }
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        "GROW",
        "SHRINK",
        "resize.rrd",
        "ptr0.rrd",
        "ptrlast.rrd",
        "hw.rrd",
    ] {
        assert!(
            joined.iter().any(|args| args.contains(needle)),
            "no case uses {needle}"
        );
    }
    let successes = vectors
        .values()
        .filter(|sections| sections[0].1 == b"0")
        .count();
    assert!(successes >= 30, "only {successes} successful resizes");
}

#[test]
fn rondi_matches_frozen_resize_vectors() {
    spec_vectors::replay_rondi(&suite());
}

#[test]
fn rrdtool_reproduces_resize_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live resize vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    spec_vectors::replay_oracle(&suite());
}

/// RZ output rules that both implementations follow: the output is written
/// to `resize.rrd` in the working directory, never next to the input, and
/// the input keeps its inode and bytes.
#[test]
fn resize_writes_resize_rrd_in_the_working_directory() {
    let bench = spec_vectors::Bench::new();
    let input = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/resize/inputs/base.rrd"),
    )
    .unwrap();
    let mut programs = vec![("rondi", bench.rondi.clone())];
    if Command::new("rrdtool").arg("--version").output().is_ok() {
        common::require_oracle();
        programs.push(("rrdtool", PathBuf::from("rrdtool")));
    } else {
        common::skipped("resize file checks: checking Rondi only".into());
    }
    for (who, program) in programs {
        let dir = bench.work.join(who);
        let elsewhere = dir.join("data");
        let run_in = dir.join("run");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(&run_in).unwrap();
        std::fs::write(elsewhere.join("in.rrd"), &input).unwrap();
        let inode = std::fs::metadata(elsewhere.join("in.rrd")).unwrap().ino();
        let output = Command::new(&program)
            .args(["resize", "../data/in.rrd", "0", "GROW", "2"])
            .current_dir(&run_in)
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{who}: {output:?}");
        assert!(run_in.join("resize.rrd").is_file(), "{who}");
        assert!(!elsewhere.join("resize.rrd").exists(), "{who}");
        assert_eq!(
            std::fs::read(elsewhere.join("in.rrd")).unwrap(),
            input,
            "{who}"
        );
        assert_eq!(
            std::fs::metadata(elsewhere.join("in.rrd")).unwrap().ino(),
            inode,
            "{who}"
        );
        assert_eq!(
            std::fs::metadata(run_in.join("resize.rrd")).unwrap().len(),
            input.len() as u64 + 2 * 2 * 8,
            "{who}"
        );
    }
}
