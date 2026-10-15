#![cfg(unix)]
//! Frozen vectors for `rrdtool create`: option and definition grammar,
//! validation order and error text, the initial file layout, templates,
//! sources and output file handling. The behavior is specified in
//! docs/compatibility/create.md; the vectors under tests/data/create were
//! recorded from RRDtool 1.11.0 by scripts/gen-create-resize-vectors.py.
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the cases also run live against it.

mod common;
#[path = "spec_vectors/mod.rs"]
mod spec_vectors;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use spec_vectors::{Expect, Suite};

fn suite() -> Suite {
    Suite::new("create", true)
}

/// The table pins every area of the specification.
#[test]
fn create_vectors_cover_the_specification() {
    let suite = suite();
    let cases = suite.cases();
    assert!(cases.len() >= 300, "only {} cases", cases.len());
    let vectors = suite.vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        if case.expect == Expect::Divergent {
            assert!(
                case.note.starts_with("CR-div-"),
                "{}: no divergence id",
                case.id
            );
        }
    }
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        ":GAUGE:",
        ":COUNTER:",
        ":DERIVE:",
        ":ABSOLUTE:",
        ":DCOUNTER:",
        ":DDERIVE:",
        ":COMPUTE:",
        "RRA:AVERAGE:",
        "RRA:MIN:",
        "RRA:MAX:",
        "RRA:LAST:",
        "HWPREDICT",
        "MHWPREDICT",
        "SEASONAL",
        "DEVSEASONAL",
        "DEVPREDICT",
        "FAILURES",
        "--start",
        "--step",
        "-t ",
        "-r ",
        "-O",
        "--daemon",
        "=y[",
    ] {
        assert!(
            joined.iter().any(|args| args.contains(needle)),
            "no case uses {needle}"
        );
    }
    let errors = vectors
        .values()
        .filter(|sections| sections[0].1 != b"0")
        .count();
    assert!(errors >= 100, "only {errors} error cases");
}

/// Rondi reproduces every frozen vector its expectation requires.
#[test]
fn rondi_matches_frozen_create_vectors() {
    spec_vectors::replay_rondi(&suite());
}

/// The pinned RRDtool still produces the vectors on this platform, and
/// Rondi matches it directly.
#[test]
fn rrdtool_reproduces_create_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live create vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    spec_vectors::replay_oracle(&suite());
}

fn programs(bench: &spec_vectors::Bench) -> Vec<(&'static str, PathBuf)> {
    let mut programs = vec![("rondi", bench.rondi.clone())];
    if Command::new("rrdtool").arg("--version").output().is_ok() {
        common::require_oracle();
        programs.push(("rrdtool", PathBuf::from("rrdtool")));
    } else {
        common::skipped("create file checks: checking Rondi only".into());
    }
    programs
}

fn create(
    program: &Path,
    dir: &Path,
    umask: &str,
    name: &str,
    extra: &[&str],
) -> (Option<i32>, String) {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("umask {umask}; exec \"$0\" \"$@\""))
        .arg(program)
        .args(["create", name])
        .args(extra)
        .current_dir(dir)
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const DEFS: &[&str] = &["DS:x:GAUGE:20:U:U", "RRA:AVERAGE:0.5:1:5"];

/// CR file rules: a new file has mode 0644 whatever the umask; replacing an
/// existing file keeps its permission bits, gives it a new inode and leaves
/// hard links to the old one alone.
#[test]
fn create_file_mode_and_replacement() {
    let bench = spec_vectors::Bench::new();
    for (who, program) in programs(&bench) {
        let dir = bench.work.join(who);
        std::fs::create_dir(&dir).unwrap();
        let mode = |name: &str| std::fs::metadata(dir.join(name)).unwrap().mode() & 0o7777;
        for umask in ["077", "000", "022"] {
            let name = format!("u{umask}.rrd");
            let mut args = vec!["-b", "1000000000"];
            args.extend_from_slice(DEFS);
            assert_eq!(
                create(&program, &dir, umask, &name, &args).0,
                Some(0),
                "{who}"
            );
            assert_eq!(mode(&name), 0o644, "{who} umask {umask}");
        }
        for existing in [0o600, 0o444, 0o4755, 0o000] {
            let name = format!("e{existing:o}.rrd");
            std::fs::write(dir.join(&name), b"old").unwrap();
            std::fs::set_permissions(dir.join(&name), std::fs::Permissions::from_mode(existing))
                .unwrap();
            std::fs::hard_link(dir.join(&name), dir.join(format!("{name}.link"))).unwrap();
            let inode = std::fs::metadata(dir.join(&name)).unwrap().ino();
            let mut args = vec!["-b", "1000000000"];
            args.extend_from_slice(DEFS);
            assert_eq!(
                create(&program, &dir, "022", &name, &args).0,
                Some(0),
                "{who}"
            );
            assert_eq!(mode(&name), existing, "{who} existing {existing:o}");
            assert_ne!(
                std::fs::metadata(dir.join(&name)).unwrap().ino(),
                inode,
                "{who}"
            );
            std::fs::set_permissions(
                dir.join(format!("{name}.link")),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(dir.join(format!("{name}.link"))).unwrap(),
                b"old",
                "{who}: hard link changed"
            );
        }
    }
}

/// CR start and step defaults: without --start the last update is ten
/// seconds before the current time; without --step the step is 300.
#[test]
fn create_defaults_start_and_step() {
    let bench = spec_vectors::Bench::new();
    for (who, program) in programs(&bench) {
        let dir = bench.work.join(who);
        std::fs::create_dir(&dir).unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(
            create(&program, &dir, "022", "d.rrd", DEFS).0,
            Some(0),
            "{who}"
        );
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let bytes = std::fs::read(dir.join("d.rrd")).unwrap();
        let word =
            |offset: usize| u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        assert_eq!(word(40), 300, "{who}: step");
        let last_update = word(128 + 120 + 120);
        assert!(
            (before - 10..=after - 10).contains(&last_update),
            "{who}: last update {last_update} outside {before}-10..{after}-10"
        );
        assert_eq!(word(128 + 120 + 120 + 8), 0, "{who}: microseconds");
    }
}

/// CR initial row: RRDtool chooses every archive's current row at random
/// among its rows, so across many files each row of a small archive is
/// seen. A replacement may choose any row in range; Rondi always chooses
/// row 0 (divergence CR-div-initial-row), which this test pins.
#[test]
fn create_picks_initial_rows_at_random() {
    let bench = spec_vectors::Bench::new();
    for (who, program) in programs(&bench) {
        let dir = bench.work.join(who);
        std::fs::create_dir(&dir).unwrap();
        let mut seen = [false; 3];
        for attempt in 0..60 {
            let name = format!("r{attempt}.rrd");
            let args = [
                "-b",
                "1000000000",
                "DS:x:GAUGE:20:U:U",
                "RRA:AVERAGE:0.5:1:3",
            ];
            assert_eq!(
                create(&program, &dir, "022", &name, &args).0,
                Some(0),
                "{who}"
            );
            let bytes = std::fs::read(dir.join(&name)).unwrap();
            let pointer = 128 + 120 + 120 + 16 + 112 + 80;
            let row = u64::from_le_bytes(bytes[pointer..pointer + 8].try_into().unwrap());
            assert!(row < 3, "{who}: current row {row}");
            seen[row as usize] = true;
            if seen.iter().all(|seen| *seen) {
                break;
            }
        }
        if who == "rondi" {
            assert_eq!(seen, [true, false, false], "rondi: rows seen");
        } else {
            assert!(seen.iter().all(|seen| *seen), "{who}: rows seen {seen:?}");
        }
    }
}
