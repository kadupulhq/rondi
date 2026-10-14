#![cfg(unix)]
//! Differential tests for the `rrdtool` front end: rrd_tool.c argv and pipe
//! mode dispatch, CountArgs/CreateArgs, and optparse.c as used by graph,
//! graphv and xport. Each case runs pinned RRDtool 1.11.0 and the Rondi
//! alias in copies of one scratch directory.

#[macro_use]
mod common;

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};

fn pinned_rrdtool_available() -> bool {
    Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("1.11.0"))
}

/// Drops the build stamp and the timing figures.
fn normalize(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(position) = line.find("Compiled ") {
            out.push_str(&line[..position + 9]);
            out.push('\n');
        } else if line.starts_with("OK u:") {
            out.push_str("OK\n");
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
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn run(program: &Path, dir: &Path, args: &[&OsStr], stdin: &[u8]) -> Run {
    let mut child = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        stdout: normalize(&out.stdout),
        stderr: normalize(&out.stderr),
        code: out.status.code(),
    }
}

/// Runs both programs; `full` also compares stdout.
fn compare(args: &[&OsStr], stdin: &[u8], full: bool) {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let up_dir = tempfile::tempdir().unwrap();
    let ro_dir = tempfile::tempdir().unwrap();
    // rrd_create randomizes each RRA's cur_row, so build one fixture and copy it.
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
    let up = run(Path::new("rrdtool"), up_dir.path(), args, stdin);
    let ro = run(&alias, ro_dir.path(), args, stdin);
    if full {
        assert_eq!(ro.stdout, up.stdout, "stdout for {args:?}");
    }
    assert_eq!(ro.stderr, up.stderr, "stderr for {args:?}");
    assert_eq!(ro.code, up.code, "exit status for {args:?}");
}

fn check(args: &[&str], stdin: &[u8]) {
    let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
    compare(&args, stdin, true);
}

fn pipe(stdin: &[u8]) {
    check(&["-"], stdin);
}

const DEF: &str = "DEF:x=a.rrd:x:AVERAGE";

/// Runs every case and reports all that diverge.
fn each<T: std::fmt::Debug>(cases: &[T], test: impl Fn(&T)) {
    let failures = cases
        .iter()
        .filter(|case| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(case))).is_err()
        })
        .map(|case| format!("{case:?}"))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "diverging cases:\n{}",
        failures.join("\n")
    );
}

// rrd_tool.c main(): pipe mode arguments.
#[test]
fn pipe_workdir_argument() {
    check(&["-", "sub"], b"last b.rrd\n");
}

#[test]
fn pipe_bad_workdir_exits_with_errno() {
    check(&["-", "nodir"], b"last a.rrd\n");
}

#[test]
fn dash_with_two_arguments_is_an_unknown_function() {
    check(&["-", "a", "b"], b"last a.rrd\n");
}

// rrd_tool.c:491 would chroot as root, but HAVE_GETEUID is never defined,
// so root also gets a plain chdir. The CI container runs as root.
#[test]
fn pipe_workdir_is_chdir_even_for_root() {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let workdir = dir.path().canonicalize().unwrap();
    let args = [OsStr::new("-"), workdir.as_os_str()];
    let stdin = b"pwd\nls\nlast a.rrd\n";
    let up = run(Path::new("rrdtool"), Path::new("/"), &args, stdin);
    let ro = run(&alias, Path::new("/"), &args, stdin);
    assert!(up.stdout.starts_with(&format!("{}\n", workdir.display())));
    assert_eq!(ro.stdout, up.stdout);
    assert_eq!(ro.stderr, up.stderr);
    assert_eq!(ro.code, up.code);
}

// HandleInputLine(): remote directory commands.
#[test]
fn pipe_directory_commands() {
    pipe(b"mkdir n\ncd n\ncd ..\nls\ncd\ncd nope\nmkdir n\nls x\npwd x\n");
}

// HandleInputLine(): one dispatch table for argv and pipe mode.
#[test]
fn pipe_dispatches_xport() {
    pipe(b"xport --start 1700000000 --end 1700000600 DEF:x=a.rrd:x:AVERAGE XPORT:x\n");
}

#[test]
fn pipe_dispatches_graph() {
    pipe(b"graph o.png --start 1700000000 --end 1700000600 DEF:x=a.rrd:x:AVERAGE LINE1:x#ff0000 PRINT:x:MAX:%6.2lf\nlast a.rrd\n");
}

#[test]
fn pipe_dispatches_flushcached() {
    pipe(b"flushcached a.rrd\n");
}

#[test]
fn pipe_usage_and_version() {
    pipe(b"info\nhelp x\n--version x\nquit x\nfoo bar\n");
}

// CountArgs()/CreateArgs(): only ' ' separates, quotes group, backslash and
// '#' are literal, unterminated quotes reject the line.
#[test]
fn pipe_tab_is_not_a_separator() {
    pipe(b"last\ta.rrd\n");
}

#[test]
fn pipe_backslash_is_literal() {
    pipe(b"last a\\.rrd\n");
}

#[test]
fn pipe_hash_is_literal() {
    pipe(b"last a#.rrd\n");
}

#[test]
fn pipe_quotes_group_and_nest() {
    pipe(b"last 'a'.rrd\nlast \"a.r\"'rd'\nlast 'a\".rrd'\n   last   a.rrd   \n");
}

#[test]
fn pipe_php_escaped_apostrophe_is_rejected() {
    pipe(b"last 'it'\\''s.rrd'\nlast a.rrd\n");
}

#[test]
fn pipe_backslash_quote_in_double_quotes() {
    pipe(b"last \"a\\\"b.rrd\"\n");
}

#[test]
fn pipe_blank_and_space_only_lines() {
    pipe(b"\n   \n\t\r\n");
}

#[test]
fn pipe_unterminated_tab_line() {
    pipe(b"\t");
}

#[test]
fn pipe_unterminated_space_line() {
    pipe(b"last a.rrd\n   ");
}

// fgetslong() reads raw bytes, so a non-UTF-8 byte does not end the session.
#[test]
fn pipe_non_utf8_line_keeps_session() {
    pipe(b"last \xff.rrd\nlast a.rrd\n");
}

// optparse.c through rrd_xport.c's option table, with atoi/atol values.
#[test]
fn xport_option_forms() {
    let cases: &[&[&str]] = &[
        &["--start=1700000000", "--end=1700000600"],
        &["-s1700000000", "-e1700000600", "-m20"],
        &["-s", "1700000000", "-e", "1700000600", "-tm", "20"],
        &[
            "--start",
            "1700000000",
            "--end",
            "1700000600",
            "-m",
            "50abc",
        ],
        &[
            "--start",
            "1700000000",
            "--end",
            "1700000600",
            "--step",
            "abc",
        ],
        &["--start", "1700000000", "--end", "1700000600", "-m", "-5"],
        &[
            "--start",
            "1700000000",
            "--end",
            "1700000600",
            "--maxrows=9",
        ],
        &["--start", "1700000600", "--end", "1700000000"],
        &["--start", "garbage"],
        &["--start", "100"],
        &["--end", "garbage", "--start", "bogus"],
        &["--start", "bogus", "--end", "garbage"],
        &["--start", "end-1h", "--end", "start+1h"],
        &["--start", "-Z", "--end", "garbage"],
        &["-Z"],
        &["--zoom", "2"],
        &["--json=1"],
        &[
            "--start",
            "1700000000",
            "--end",
            "1700000600",
            "-d",
            "x",
            "--daemon",
            "y",
        ],
    ];
    each(cases, |extra| {
        let mut args = vec!["xport"];
        args.extend_from_slice(extra);
        args.extend([DEF, "XPORT:x"]);
        check(&args, b"");
    });
    check(&["xport", DEF, "XPORT:x", "-m"], b"");
    check(
        &[
            "xport",
            "--start",
            "1700000000",
            "--end",
            "1700000600",
            "--",
            DEF,
            "XPORT:x",
        ],
        b"",
    );
    check(
        &[
            "xport",
            DEF,
            "--start",
            "1700000000",
            "XPORT:x",
            "--end",
            "1700000600",
            "--json",
        ],
        b"",
    );
}

/// Graph PNG bytes follow Cairo/Pango (RD-006); graph cases write to
/// /dev/null and compare the size line, PRINT output, stderr and status.
fn graph_status(extra: &[&str]) {
    let mut args = vec![
        "graph",
        "/dev/null",
        "--start",
        "1700000000",
        "--end",
        "1700000600",
        DEF,
        "LINE1:x#ff0000",
    ];
    args.extend_from_slice(extra);
    let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
    compare(&args, b"", true);
}

// rrd_graph.c:5063 option table, each option's validation text, and
// optparse forms.
#[test]
fn graph_option_table() {
    let cases: &[&[&str]] = &[
        &["-A"],
        &["-B", "10"],
        &["-b", "1024"],
        &["-b", "1000x"],
        &["-b", "999"],
        &["-c", "BACK#ffffff"],
        &["-c", "CANVAS#fff"],
        &["-c", "ARROW#ffff"],
        &["-c", "FRAME#ffffff80"],
        &["-c", "BACK#fffff"],
        &["-c", "BACK#zz"],
        &["-c", "FOO#ffffff"],
        &["-c", "back#ffffff"],
        &["-D"],
        &["-E"],
        &["-F"],
        &["-G", "mono"],
        &["-G", "bogus"],
        &["-g"],
        &["-h", "100"],
        &["-h", "9"],
        &["-I"],
        &["-i"],
        &["-J"],
        &["-j"],
        &["-L", "5"],
        &["-l", "0"],
        &["-l", "abc"],
        &["-l", "1x"],
        &["-M"],
        &["-m", "2"],
        &["-m", "0"],
        &["-m", "abc"],
        &["-N"],
        &["-n", "DEFAULT:8:"],
        &["-n", "TITLE:10:Sans"],
        &["-n", "LEGEND:8"],
        &["-n", "FOO:8:"],
        &["-n", "TITLE:x"],
        &["-n", "TITLE:8;Sans"],
        &["-o", "-l", "1"],
        &["-o", "-l", "0"],
        &["-o"],
        &["-P"],
        &["-R", "light"],
        &["-R", "bogus"],
        &["-r"],
        &["-S", "300"],
        &["-S", "abc"],
        &["-T", "20"],
        &["-T", "x"],
        &["-t", "Title"],
        &["-u", "10"],
        &["-u", "nan"],
        &["-v", "lbl"],
        &["-W", "wm"],
        &["-w", "300"],
        &["-w", "10x"],
        &["-w", "5"],
        &["-X", "0"],
        &["-x", "MINUTE:10:HOUR:1:HOUR:1:0:%H"],
        &["-x", "none"],
        &["-x", "bad"],
        &["-x", "MINUTE:10:FOO:1:HOUR:1:0:%H"],
        &["-Y"],
        &["-y", "1:2"],
        &["-y", "none"],
        &["-y", "0:2"],
        &["-y", "1:0"],
        &["-y", "bad"],
        &["-Z"],
        &["--units=si"],
        &["--units", "xx"],
        &["--units=si", "--units=si"],
        &["--add-jsontime", "--add-jsontime"],
        &["--alt-y-mrtg"],
        &["--disable-rrdtool-tag"],
        &["--right-axis", "1:0"],
        &["--right-axis", "0:0"],
        &["--right-axis", "bad"],
        &["--right-axis-label", "r"],
        &["--right-axis-format", "%1.0lf"],
        &["--right-axis-format", "%d"],
        &[
            "--right-axis-formatter",
            "timestamp",
            "--right-axis-format",
            "%d",
        ],
        &["--left-axis-format", "%5.1lf %%"],
        &["--left-axis-format", "%s"],
        &["--left-axis-formatter", "numeric"],
        &["--left-axis-formatter", "bogus"],
        &["--right-axis-formatter", "bogus"],
        &["--legend-position", "south"],
        &["--legend-position=foo"],
        &["--legend-direction", "bottomup2"],
        &["--legend-direction=foo"],
        &["--border", "100"],
        &["--border", "-3"],
        &["--grid-dash", "1:1"],
        &["--grid-dash", "a:1"],
        &["--grid-dash", "1"],
        &["--dynamic-labels"],
        &["--week-fmt", "%V"],
        &["--graph-type", "TIME"],
        &["--graph-type", "foo"],
        &["--allow-shrink"],
        &["--utc"],
        &["--vertical-label-angle", "90"],
        &["--vertical-label-angle", "abc"],
        &["--right-axis-label-angle", "x"],
        &["--right-axis-range", "0:1"],
        &["--right-axis-range", ":1"],
        &["--right-axis-range", "2:1"],
        &["--right-axis-range", "1"],
        &["--right-axis-range", "x:1"],
        &["-a", "PNG"],
        &["-a", "png"],
        &["-a", "GIF"],
        &["--maxrows", "5"],
        &["--sta", "1"],
        &["--rigid=1"],
        &["-Q"],
        &["-d", "x", "-d", "y"],
        &["--start", "1700000600", "--end", "1700000000"],
        &["--upper-limit"],
        &["--start", "garbage"],
        &["--start", "100"],
        &["--end", "garbage", "--start", "bogus"],
        &["--start", "bogus", "-w", "5"],
        &["-w", "5", "--start", "bogus"],
        &["-gjrE"],
        &["-gw300"],
    ];
    each(cases, |extra| graph_status(extra));
}

#[test]
fn graph_option_placement() {
    let cases: &[&[&str]] = &[
        &[
            "graph",
            "--start",
            "1700000000",
            "/dev/null",
            "--end=1700000600",
            DEF,
            "LINE1:x#ff0000",
        ],
        &[
            "graph",
            "/dev/null",
            "-s1700000000",
            "-e1700000600",
            DEF,
            "LINE1:x#ff0000",
            "--",
        ],
        &[
            "graph",
            "-s",
            "1700000000",
            "-e",
            "1700000600",
            "--",
            "/dev/null",
            DEF,
            "LINE1:x#ff0000",
        ],
        &["graph", "-s", "1700000000"],
        &["graphv", "-s", "1700000000", "-Q"],
    ];
    each(cases, |args| {
        let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
        compare(&args, b"", true);
    });
}

// A pipe line with 100k words or one 100k-letter option cluster parses in
// linear time; upstream recurses once per skipped word.
#[test]
fn long_pipe_lines() {
    let mut line =
        b"xport --start 1700000000 --end 1700000600 DEF:x=a.rrd:x:AVERAGE XPORT:x".to_vec();
    line.extend(b" -t".repeat(100_000));
    line.extend(b"\nxport --start 1700000000 --end 1700000600 DEF:x=a.rrd:x:AVERAGE XPORT:x -");
    line.extend(b"t".repeat(100_000));
    line.extend(b"\n");
    pipe(&line);
}

// The argument forms Kadupul's GraphOptionsGenerator writes through
// PipeEncoder, every argument single-quoted, sent through pipe mode.
#[test]
fn kadupul_graph_line_through_pipe_mode() {
    pipe(
        b"graph o.png --imgformat=PNG --start='1700000000' --end='1700000600' \
--pango-markup  --disable-rrdtool-tag  --title='Traffic <b>x</b>' --alt-y-grid \
--height=150 --width=500 --base=1000 --vertical-label='bits per second' \
--slope-mode --units=si --rigid --lower-limit='0' --upper-limit='10' \
--units-exponent='0' --y-grid='1:2' --right-axis '1:0' --right-axis-label 'r' \
--right-axis-format '%5.1lf' --no-gridfit --units-length '5' --tabwidth '40' \
--dynamic-labels --force-rules-legend --font 'TITLE:10:Sans' --font 'DEFAULT:8:' \
--color BACK#FFFFFF --color 'CANVAS#000000AA' --border 2 --watermark 'Kadupul' \
'DEF:x=a.rrd:x:AVERAGE' 'LINE1:x#FF0000:In' 'GPRINT:x:AVERAGE:%8.2lf' \
'PRINT:x:MAX:%8.2lf'\n\
xport --start='1700000000' --end='1700000600' --maxrows=10000 \
'DEF:x=a.rrd:x:AVERAGE' 'XPORT:x:In'\n",
    );
}

// A non-UTF-8 argv byte reaches the command instead of panicking. The
// byte itself prints as U+FFFD, so only the exit status is compared.
#[test]
fn non_utf8_argv_does_not_panic() {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let alias = dir.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for args in [
        &[
            OsStr::new("xport"),
            OsStr::new("--start"),
            OsStr::new("1700000000"),
            OsStr::from_bytes(b"DEF:x=\xff.rrd:x:AVERAGE"),
            OsStr::new("XPORT:x"),
        ][..],
        &[OsStr::new("graph"), OsStr::from_bytes(b"-\xff")],
        &[OsStr::new("info"), OsStr::from_bytes(b"\xff.rrd")],
    ] {
        let up = run(Path::new("rrdtool"), dir.path(), args, b"");
        let ro = run(&alias, dir.path(), args, b"");
        assert_eq!(ro.code, up.code, "exit status for {args:?}: {}", ro.stderr);
    }
}
