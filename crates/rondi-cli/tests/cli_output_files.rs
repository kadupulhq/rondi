#![cfg(unix)]
//! Output files the CLI writes itself (graph and graphv images, dump XML)
//! refuse a symbolic link or a hard-linked regular file at the output path.
//! RRDtool follows both; Rondi deviates on purpose so a planted link cannot
//! redirect the write.

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
        assert_eq!(
            String::from_utf8_lossy(&run.stderr),
            "ERROR: refusing to write 'out.png': it is a symbolic link\n"
        );
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
    assert_eq!(
        String::from_utf8_lossy(&run.stderr),
        "ERROR: refusing to write 'out.png': it has more than one hard link\n"
    );
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
    let run = graph(dir.path(), "graph", "/dev/null");
    assert!(run.status.success(), "{run:?}");
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
    assert_eq!(
        String::from_utf8_lossy(&run.stderr),
        "ERROR: refusing to write 'out': it is a symbolic link\n"
    );
    assert_eq!(
        std::fs::read(dir.path().join("victim")).unwrap(),
        b"keep me"
    );
}
