#![cfg(unix)]
//! A LINE width found by fuzzing. rrd_graph_helper.c (newGraphDescription,
//! PARSE_LINEWIDTH) only rejects negative widths; Cairo strokes the rest.

use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn rrdtool_alias(dir: &std::path::Path) -> std::path::PathBuf {
    let alias = dir.join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    alias
}

fn gauge_rrd(dir: &std::path::Path) -> std::path::PathBuf {
    let file = dir.join("g.rrd");
    rondi::create_rrd_file(
        &file,
        1_000_000_000,
        10,
        &["DS:g:GAUGE:30:U:U".to_owned()],
        &["RRA:AVERAGE:0.5:1:8".to_owned()],
        true,
    )
    .unwrap();
    for step in 1..=7_i64 {
        rondi::update_rrd_file(&file, 1_000_000_000 + step * 10, Some(42.0)).unwrap();
    }
    file
}

/// A huge LINE width overflowed the stroke stamp and, without overflow
/// checks, spun for width^2 iterations per point. RRDtool 1.11.0 renders it.
#[test]
fn graph_line_width_is_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let file = gauge_rrd(temp.path());
    let alias = rrdtool_alias(temp.path());
    let mut child = Command::new(alias)
        .args([
            "graph",
            temp.path().join("out.png").to_str().unwrap(),
            "--start",
            "1000000000",
            "--end",
            "1000000080",
            &format!("DEF:a={}:g:AVERAGE", file.display()),
            "LINE5555555555:a#ff0000",
        ])
        .env("TZ", "UTC")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect("graph with LINE5555555555 did not finish within 20 s");
    assert!(status.success(), "graph failed: {status}");
}
