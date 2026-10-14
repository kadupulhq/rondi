#![cfg(unix)]
//! Output files the CLI writes itself (graph and graphv images, dump XML)
//! refuse a symbolic link or a hard-linked regular file at the output path.
//! RRDtool follows both; Rondi deviates on purpose so a planted link cannot
//! redirect the write.

mod common;

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

fn rrdtool(dir: &Path, args: &[&str]) -> Output {
    let alias = dir.join("rrdtool");
    if !alias.exists() {
        symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    }
    Command::new(&alias)
        .current_dir(dir)
        .args(args)
        .env_remove("RRDCACHED_ADDRESS")
        .output()
        .unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let created = rrdtool(
        dir.path(),
        &[
            "create",
            "a.rrd",
            "--start",
            "1700000000",
            "--step",
            "300",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
    );
    assert!(created.status.success(), "{created:?}");
    let updated = rrdtool(
        dir.path(),
        &["update", "a.rrd", "1700000300:1", "1700000600:2"],
    );
    assert!(updated.status.success(), "{updated:?}");
    std::fs::write(dir.path().join("victim"), b"keep me").unwrap();
    dir
}

const GRAPH: [&str; 6] = [
    "--start",
    "1700000000",
    "--end",
    "1700000600",
    "DEF:x=a.rrd:x:AVERAGE",
    "LINE1:x#ff0000",
];

fn graph(dir: &Path, command: &str, output: &str) -> Output {
    let mut args = vec![command, output];
    args.extend(GRAPH);
    rrdtool(dir, &args)
}

#[test]
fn graph_refuses_a_symlinked_output_path() {
    let dir = fixture();
    symlink(dir.path().join("victim"), dir.path().join("out.png")).unwrap();
    for command in ["graph", "graphv"] {
        let run = graph(dir.path(), command, "out.png");
        assert_eq!(run.status.code(), Some(1), "{run:?}");
        assert!(String::from_utf8_lossy(&run.stderr).contains("symbolic link"));
        assert_eq!(
            std::fs::read(dir.path().join("victim")).unwrap(),
            b"keep me"
        );
    }
}

#[test]
fn graph_refuses_a_hard_linked_output_path() {
    let dir = fixture();
    std::fs::hard_link(dir.path().join("victim"), dir.path().join("out.png")).unwrap();
    let run = graph(dir.path(), "graph", "out.png");
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    assert!(String::from_utf8_lossy(&run.stderr).contains("more than one hard link"));
    assert_eq!(
        std::fs::read(dir.path().join("victim")).unwrap(),
        b"keep me"
    );
}

#[test]
fn graph_still_overwrites_and_creates_regular_files() {
    let dir = fixture();
    std::fs::write(dir.path().join("out.png"), vec![b'x'; 100_000]).unwrap();
    let run = graph(dir.path(), "graph", "out.png");
    assert!(run.status.success(), "{run:?}");
    let image = std::fs::read(dir.path().join("out.png")).unwrap();
    assert!(image.starts_with(b"\x89PNG"));
    assert!(image.len() < 100_000);
    let run = graph(dir.path(), "graph", "new.png");
    assert!(run.status.success(), "{run:?}");
    assert!(
        std::fs::read(dir.path().join("new.png"))
            .unwrap()
            .starts_with(b"\x89PNG")
    );
}

#[test]
fn csv_graph_and_dump_refuse_a_symlinked_output_path() {
    let dir = fixture();
    symlink(dir.path().join("victim"), dir.path().join("out")).unwrap();
    let mut args = vec!["graph", "out", "--imgformat", "CSV"];
    args.extend(GRAPH);
    let run = rrdtool(dir.path(), &args);
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    let run = rrdtool(dir.path(), &["dump", "a.rrd", "out"]);
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    assert!(String::from_utf8_lossy(&run.stderr).contains("symbolic link"));
    assert_eq!(
        std::fs::read(dir.path().join("victim")).unwrap(),
        b"keep me"
    );
}

fn sides() -> Option<common::Sides> {
    common::Sides::new(
        &[&[
            "create",
            "a.rrd",
            "--start",
            "1700000000",
            "--step",
            "300",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ]],
        &[],
    )
}

#[test]
fn unwritable_output_paths_report_like_rrdtool() {
    let Some(sides) = sides() else { return };
    // rrd_dump_opt_r returns -1 from a failed fopen without setting an
    // error, so dump is silent; the cairo PNG writer reports its own text.
    sides.assert_same(&["dump", "a.rrd", "/nonexistent/x"]);
    for command in ["graph", "graphv"] {
        let mut args = vec![command, "/nonexistent/x.png"];
        args.extend(GRAPH);
        sides.assert_same(&args);
    }
}

#[test]
fn fetch_resolution_beyond_i64_aligns_like_rrdtool() {
    let Some(sides) = sides() else { return };
    // rrd_fetch.c computes start % step in unsigned long.
    for resolution in ["9999999999999999999", "18446744073709551615"] {
        sides.assert_same(&[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "-a",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
            "-r",
            resolution,
        ]);
    }
}

#[test]
fn data_graphs_to_an_unwritable_path_print_the_data() {
    let Some(sides) = sides() else { return };
    // rrd_graph_xport keeps the buffer when rrd_fopen fails, so the data
    // goes to stdout, followed by 0x0 for graph, and the exit status is 0.
    for command in ["graph", "graphv"] {
        for format in ["CSV", "TSV", "SSV", "XML", "XMLENUM", "JSON", "JSONTIME"] {
            let mut args = vec![command, "/nonexistent/x", "--imgformat", format];
            args.extend(GRAPH);
            sides.assert_same(&args);
        }
    }
}

#[test]
fn fetch_align_rejects_a_zero_resolution_like_rrdtool() {
    let Some(sides) = sides() else { return };
    for resolution in ["0", "0s", "0m"] {
        sides.assert_same(&["fetch", "a.rrd", "AVERAGE", "-a", "-r", resolution]);
    }
}

#[test]
fn graph_and_dump_write_to_character_devices() {
    let Some(sides) = sides() else { return };
    for command in ["graph", "graphv"] {
        let mut args = vec![command, "/dev/null"];
        args.extend(GRAPH);
        sides.assert_same(&args);
        let mut args = vec![command, "/dev/null", "--imgformat", "CSV"];
        args.extend(GRAPH);
        sides.assert_same(&args);
    }
    sides.assert_same(&["dump", "a.rrd", "/dev/null"]);
}

#[test]
fn output_through_a_symlink_reaches_a_device_only() {
    let dir = fixture();
    symlink("/dev/null", dir.path().join("null")).unwrap();
    let run = graph(dir.path(), "graph", "null");
    assert!(run.status.success(), "{run:?}");
    let run = rrdtool(dir.path(), &["dump", "a.rrd", "null"]);
    assert!(run.status.success(), "{run:?}");
    // The symlink still resolves to a device; a regular file stays refused.
    symlink(dir.path().join("victim"), dir.path().join("file")).unwrap();
    let run = rrdtool(dir.path(), &["dump", "a.rrd", "file"]);
    assert_eq!(run.status.code(), Some(1), "{run:?}");
    assert_eq!(
        std::fs::read(dir.path().join("victim")).unwrap(),
        b"keep me"
    );
}
