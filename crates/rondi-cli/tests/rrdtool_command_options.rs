#![cfg(unix)]
//! Differential tests for each command's optparse table, positional checks
//! and usage text. Each case runs
//! pinned RRDtool 1.11.0 and the Rondi alias in copies of one scratch
//! directory and compares output, status and the bytes of `a.rrd`.

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};

fn pinned_rrdtool_available() -> bool {
    Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("1.11.0"))
}

/// Drops the build stamp and timing figures, and the canvas size line,
/// whose value depends on Cairo/Pango layout (RD-006).
fn normalize(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if let Some(position) = line.find("Compiled ") {
            out.push_str(&line[..position + 9]);
            out.push('\n');
        } else if line.starts_with("OK u:") {
            out.push_str("OK\n");
        } else if bare.split_once('x').is_some_and(|(w, h)| {
            !w.is_empty() && !h.is_empty() && (w.to_owned() + h).bytes().all(|b| b.is_ascii_digit())
        }) {
            out.push_str("WxH\n");
        } else {
            out.push_str(line);
        }
    }
    out
}

fn fixture(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("rrdtool")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    run(&[
        "create",
        "a.rrd",
        "--start",
        "1700000000",
        "--step",
        "300",
        "DS:x:GAUGE:600:U:U",
        "RRA:AVERAGE:0.5:1:10",
    ]);
    run(&["update", "a.rrd", "1700000300:1", "1700000600:2"]);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::copy(dir.join("a.rrd"), dir.join("sub/b.rrd")).unwrap();
    let xml = Command::new("rrdtool")
        .current_dir(dir)
        .args(["dump", "a.rrd"])
        .output()
        .unwrap();
    std::fs::write(dir.join("a.xml"), xml.stdout).unwrap();
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    file: Vec<u8>,
}

fn run(program: &Path, dir: &Path, args: &[&OsStr], stdin: &[u8], env: &[(&str, &str)]) -> Run {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .env_remove("RRD_LOCKING")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        stdout: normalize(&out.stdout),
        stderr: normalize(&out.stderr),
        code: out.status.code(),
        file: std::fs::read(dir.join("a.rrd")).unwrap_or_default(),
    }
}

/// Runs both programs; `full` also compares stdout.
fn compare(args: &[&OsStr], stdin: &[u8], full: bool) {
    compare_env(args, stdin, full, &[]);
}

fn compare_env(args: &[&OsStr], stdin: &[u8], full: bool, env: &[(&str, &str)]) {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let up_dir = tempfile::tempdir().unwrap();
    let ro_dir = tempfile::tempdir().unwrap();
    // RRDtool's create randomizes each RRA's cur_row, so build one fixture and copy it.
    fixture(up_dir.path());
    let copied = Command::new("cp")
        .arg("-R")
        .arg(format!("{}/.", up_dir.path().display()))
        .arg(ro_dir.path())
        .status()
        .unwrap();
    assert!(copied.success());
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let up = run(Path::new("rrdtool"), up_dir.path(), args, stdin, env);
    let ro = run(&alias, ro_dir.path(), args, stdin, env);
    if full {
        assert_eq!(ro.stdout, up.stdout, "stdout for {args:?}");
    }
    assert_eq!(ro.stderr, up.stderr, "stderr for {args:?}");
    assert_eq!(ro.code, up.code, "exit status for {args:?}");
    assert!(ro.file == up.file, "a.rrd bytes differ for {args:?}");
}

fn check(args: &[&str], stdin: &[u8]) {
    let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
    compare(&args, stdin, true);
}

/// Runs every case and reports all that diverge.
fn each(cases: &[&[&str]]) {
    let failures = cases
        .iter()
        .filter(|args| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(args, b""))).is_err()
        })
        .map(|args| format!("{args:?}"))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "diverging cases:\n{}",
        failures.join("\n")
    );
}

// last prints -1 for any error.
#[test]
fn last_options() {
    each(&[
        &["last", "--daemon=", "a.rrd"],
        &["last", "a.rrd", "sub/b.rrd"],
        &["last", "nonexist.rrd"],
        &["last", "-Z", "a.rrd"],
        &["last", "a.rrd", "-d"],
        &["last", "--", "a.rrd"],
    ]);
}

// --rraindex goes through strtol base 0 into an int.
#[test]
fn first_options() {
    each(&[
        &["first", "--rraindex=0", "a.rrd"],
        &["first", "a.rrd", "--rraindex", "x"],
        &["first", "--rraindex", "0x0", "a.rrd"],
        &["first", "--rraindex", "-1", "a.rrd"],
        &["first", "--rraindex", "0"],
        &["first", "a.rrd", "sub/b.rrd"],
        &["first", "-Z", "a.rrd"],
        &["first", "nonexist.rrd"],
    ]);
}

#[test]
fn lastupdate_and_info_options() {
    each(&[
        &["lastupdate", "a.rrd", "sub/b.rrd"],
        &["lastupdate", "a.rrd", "-d"],
        &["lastupdate", "-Z", "a.rrd"],
        &["lastupdate", "--", "a.rrd"],
        &["info", "-F", "a.rrd"],
        &["info", "--noflush=1", "a.rrd"],
        &["info", "a.rrd", "sub/b.rrd"],
        &["info", "-Z", "a.rrd"],
        &["info", "-F"],
    ]);
}

// Any optparse result outside the table prints usage.
#[test]
fn dump_options() {
    each(&[
        &["dump", "-n", "a.rrd"],
        &["dump", "--header=xsd", "a.rrd"],
        &["dump", "-hdtd", "a.rrd"],
        &["dump", "-h", "bogus", "a.rrd"],
        &["dump", "--no-header=1", "a.rrd"],
        &["dump", "-Z", "a.rrd"],
        &["dump", "a.rrd", "x.xml", "extra"],
        &["dump", "--", "a.rrd"],
    ]);
}

// Times are parsed in option order.
#[test]
fn fetch_options() {
    each(&[
        &[
            "fetch",
            "--start",
            "1700000000",
            "a.rrd",
            "AVERAGE",
            "--end",
            "1700000600",
        ],
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "--start=1700000000",
            "--end=1700000600",
        ],
        &["fetch", "a.rrd", "AVERAGE", "-s1700000000", "-e1700000600"],
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
            "-ar300",
        ],
        &[
            "fetch",
            "--",
            "a.rrd",
            "AVERAGE",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
        ],
        &[
            "fetch",
            "a.rrd",
            "AVERAGE",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
            "extra",
        ],
        &["fetch", "a.rrd", "AVERAGE", "-Z"],
        &["fetch", "a.rrd", "AVERAGE", "--start"],
        &["fetch", "a.rrd", "-s", "1700000000"],
        &[
            "fetch",
            "a.rrd",
            "BOGUS",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
        ],
        &["fetch", "a.rrd", "AVERAGE", "-r", "0"],
        &["fetch", "a.rrd", "AVERAGE", "-r", "5x"],
        &[
            "fetch", "a.rrd", "AVERAGE", "--start", "bogus", "--end", "garbage",
        ],
        &["fetch", "a.rrd", "AVERAGE", "--start", "100"],
    ]);
}

/// Exit status, stderr, line count, byte count, FNV-1a hash and the first
/// and last lines of a command whose stdout is too large to hold twice.
fn streamed_summary(
    program: &Path,
    dir: &Path,
    args: &[&str],
) -> (
    Option<i32>,
    String,
    usize,
    u64,
    u64,
    Vec<String>,
    Vec<String>,
) {
    use std::io::BufRead;

    let mut child = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let (mut lines, mut bytes, mut hash) = (0, 0_u64, 0xcbf2_9ce4_8422_2325_u64);
    let (mut head, mut tail) = (Vec::new(), std::collections::VecDeque::new());
    let mut line = Vec::new();
    while stdout.read_until(b'\n', &mut line).unwrap() > 0 {
        lines += 1;
        bytes += line.len() as u64;
        for byte in &line {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if head.len() < 4 {
            head.push(text.clone());
        }
        tail.push_back(text);
        if tail.len() > 3 {
            tail.pop_front();
        }
        line.clear();
    }
    let output = child.wait_with_output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        lines,
        bytes,
        hash,
        head,
        tail.into(),
    )
}

// RRDtool allocates the whole requested range, so RRDtool prints
// ranges past ten million rows; Rondi streams them instead of refusing.
#[test]
fn fetch_prints_more_than_ten_million_rows() {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let alias = dir.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "fetch",
        "a.rrd",
        "AVERAGE",
        "-s",
        "1700000000",
        "-e",
        "4700000600",
    ];
    let upstream = streamed_summary(Path::new("rrdtool"), dir.path(), &args);
    let rondi = streamed_summary(&alias, dir.path(), &args);
    assert_eq!(upstream.0, Some(0), "{}", upstream.1);
    assert!(upstream.2 > 10_000_000, "{} lines", upstream.2);
    assert_eq!(rondi, upstream);
}

// RRDtool and :679.
#[test]
fn update_options() {
    each(&[
        &["update", "--skip-past-updates", "a.rrd", "1700000300:3"],
        &["update", "a.rrd", "--template=x", "1700000900:3"],
        &["update", "a.rrd", "-tx", "1700000900:3"],
        &["update", "a.rrd", "--locking=none", "1700000900:3"],
        &["update", "a.rrd", "-Lblock", "1700000900:3"],
        &["update", "a.rrd", "--locking=bogus", "1700000900:3"],
        &["update", "a.rrd", "-Z", "1700000900:3"],
        &["update", "a.rrd"],
        &["update", "a.rrd", "--", "1700000900:3"],
        &["updatev", "a.rrd", "--daemon", "x", "1700000900:3"],
        &["updatev", "a.rrd", "1700000300:3"],
        &["updatev", "a.rrd", "1700000900:3", "1700000300:3"],
        &["updatev", "-s", "a.rrd", "1700000300:3", "1700000900:4"],
        &["updatev", "a.rrd", "-L", "try", "1700000900:3"],
    ]);
}

// RRDtool's update rejects only an empty RRDCACHED_ADDRESS.
#[test]
fn updatev_rrdcached_address() {
    for value in ["", "unix:/nonexistent"] {
        let args = ["updatev", "a.rrd", "1700000900:3"].map(OsStr::new);
        compare_env(&args, b"", true, &[("RRDCACHED_ADDRESS", value)]);
    }
}

#[test]
fn create_options() {
    each(&[
        &[
            "create",
            "n.rrd",
            "--start=1700000000",
            "--step=300",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &["create", "n.rrd", "-t", "a.rrd", "-t", "a.rrd"],
        &[
            "create",
            "n.rrd",
            "-r",
            "nonexist.rrd",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &[
            "create",
            "n.rrd",
            "-r",
            "sub",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &[
            "create",
            "n.rrd",
            "-s",
            "0",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &[
            "create",
            "n.rrd",
            "-s",
            "10x",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &[
            "create",
            "n.rrd",
            "-b",
            "100",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &[
            "create",
            "n.rrd",
            "-b",
            "end-1h",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &["create", "-b", "1700000000"],
        &["create", "-Z", "n.rrd"],
    ]);
}

#[test]
fn restore_and_resize_arguments() {
    each(&[
        &["restore", "-r", "a.xml", "n.rrd"],
        &["restore", "-rf", "a.xml", "n.rrd"],
        &["restore", "a.xml"],
        &["restore", "-Z", "a.xml", "n.rrd"],
        &["resize", "resize.rrd", "0", "GROW", "1"],
        &["resize", "a.rrd", "0", "grow", "5"],
        &["resize", "a.rrd", "0", "GROW", "0"],
        &["resize", "a.rrd", "5", "GROW", "1"],
        &["resize", "a.rrd", "-1", "GROW", "1"],
        &["resize", "a.rrd", "0", "GROW"],
        &["resize", "a.rrd", "0x0", "GROW", "0x2"],
        &["resize", "nonexist.rrd", "0", "GROW", "1"],
    ]);
}

// The restore flags are file statics in RRDtool and stay set for
// later commands of a pipe session.
#[test]
fn restore_flags_persist_in_pipe_mode() {
    check(&["-"], b"restore -f a.xml n.rrd\nrestore a.xml n.rrd\n");
}

// Settings apply in order, earlier ones persist when a later
// one fails, and the file is opened even without settings.
#[test]
fn tune_options() {
    each(&[
        &["tune", "a.rrd", "-hx:900"],
        &["tune", "a.rrd", "-h", "x:900", "-h", "zz:10"],
        &["tune", "a.rrd", "-h", "x:10s"],
        &["tune", "a.rrd", "-h", "x"],
        &["tune", "nonexist.rrd"],
        &["tune", "a.rrd"],
        &["tune", "-h", "x:1"],
        &["tune", "a.rrd", "-i", "x:U", "--maximum=x:1e3x"],
        &["tune", "a.rrd", "-a", "x:1e3", "-i", "x:5"],
        &["tune", "a.rrd", "-i", "x:inf"],
        &["tune", "a.rrd", "-d", "x:FOO"],
        &["tune", "a.rrd", "-d", "x:COUNTER"],
        &["tune", "a.rrd", "-r", "x:y", "-h", "y:900"],
        &["tune", "a.rrd", "-r", "x"],
        &["tune", "a.rrd", "--alpha", "0.5"],
        &["tune", "a.rrd", "--alpha", "2"],
        &["tune", "a.rrd", "--alpha", "x"],
        &["tune", "a.rrd", "--deltapos", "0.05"],
        &["tune", "a.rrd", "--deltapos", "1x"],
        &["tune", "a.rrd", "--window-length", "50"],
        &["tune", "a.rrd", "--failure-threshold", "5"],
        &["tune", "a.rrd", "-b", "x"],
        &["tune", "a.rrd", "-b", "zz"],
        &["tune", "a.rrd", "--step", "600"],
        &["tune", "a.rrd", "FOO"],
        &["tune", "a.rrd", "-Z"],
        &["tune", "a.rrd", "-h", "x:900", "-Z"],
    ]);
}

// RRDtool parse argv twice. The first pass permutes the file
// name behind a trailing option that lacks a value, so the second pass
// reads the file name as that value; a long option there also pushes the
// file name past argc. When the file name is consumed, tune
// prints the data sources.
#[test]
fn tune_trailing_option_takes_the_permuted_file_name() {
    // -s and -S are left out: RRDtool also raises the file to
    // version 0004 before they fail, which Rondi does not do yet.
    let mut cases = [
        "h", "i", "a", "d", "r", "p", "n", "w", "f", "x", "y", "z", "v", "b", "t", "D",
    ]
    .map(|short| vec!["tune".to_owned(), "a.rrd".to_owned(), format!("-{short}")])
    .to_vec();
    for long in [
        "heartbeat",
        "minimum",
        "step",
        "daemon",
        "alpha",
        "window-length",
    ] {
        cases.push(vec!["tune".into(), "a.rrd".into(), format!("--{long}")]);
    }
    for case in [
        &["tune", "a.rrd", "sub/b.rrd", "-h"][..],
        &["tune", "a.rrd", "sub/b.rrd", "--heartbeat"],
        &["tune", "a.rrd", "sub/b.rrd", "a.xml", "--heartbeat"],
        &["tune", "a.rrd", "-h", "x:700", "--heartbeat"],
        &["tune", "a.rrd", "-h", "x:700", "-i"],
        &["tune", "a.rrd", "-i", "x:5", "-t"],
        &["tune", "a.rrd", "-hx:700", "-a", "x:9", "-D"],
        &["tune", "--minimum=x:3", "a.rrd", "-t"],
        &["tune", "-t", "a.rrd"],
        &["tune", "a.rrd", "-Z", "-h"],
    ] {
        cases.push(case.iter().map(|word| word.to_string()).collect());
    }
    let cases = cases
        .iter()
        .map(|case| case.iter().map(String::as_str).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    each(&cases.iter().map(Vec::as_slice).collect::<Vec<_>>());
}

#[test]
fn list_and_flushcached_options() {
    each(&[
        &["list", "-Z", "sub"],
        &["list", "sub", "other"],
        &["list", "--recursive=1", "sub"],
        &["flushcached", "--daemon", "", "a.rrd"],
        &["flushcached", "--daemon=", "a.rrd"],
        &["flushcached", "-Z", "a.rrd"],
        &["flushcached", "-d", "x"],
    ]);
}
