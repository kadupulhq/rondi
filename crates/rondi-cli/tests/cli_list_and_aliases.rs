#![cfg(unix)]
//! Differential tests for `rrd_list.c` (`rrd_list_r`, `rrd_list_rec`), the
//! `--imginfo` format check, and `rrdupdate.c`'s rrdupdate, rrdcreate and
//! rrdinfo executables. Read-only cases run both programs in one directory
//! so readdir order is the same for both.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

fn pinned_rrdtool_available() -> bool {
    Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("1.11.0"))
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
    std::fs::create_dir_all(dir.join("d2/inner")).unwrap();
    std::fs::copy(dir.join("a.rrd"), dir.join("d2/x.rrd.rrd")).unwrap();
    std::fs::copy(dir.join("a.rrd"), dir.join("d2/y.rrd")).unwrap();
    std::fs::write(dir.join("d2/notes.txt"), b"x").unwrap();
    std::fs::copy(dir.join("a.rrd"), dir.join("d2/inner/z.rrd")).unwrap();
    symlink("missing.rrd", dir.join("d2/dangling.rrd")).unwrap();
    std::fs::create_dir_all(dir.join("empty")).unwrap();
}

type Outcome = (Option<i32>, String, String);

fn run(program: &Path, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Outcome {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .args(args)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS");
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs every case in one shared directory and reports all that diverge.
/// Arguments and extra environment for one case.
type Case<'a> = (&'a [&'a str], &'a [(&'a str, &'a str)]);

fn each(cases: &[Case<'_>]) {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let alias_dir = tempfile::tempdir().unwrap();
    let alias = alias_dir.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let failures = cases
        .iter()
        .filter(|(args, env)| {
            let up = run(Path::new("rrdtool"), dir.path(), args, env);
            let ro = run(&alias, dir.path(), args, env);
            up != ro
        })
        .map(|(args, env)| format!("{args:?} {env:?}"))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "diverging cases:\n{}",
        failures.join("\n")
    );
}

// rrd_list.c:150-245 and rrd_list.c:350, which prints strerror(errno)
// without a newline and still exits 0.
#[test]
fn list_matches_rrd_list_r() {
    each(&[
        (&["list", "a..b"], &[]),
        (&["list", "a.rrd"], &[]),
        (&["list", "./a.rrd"], &[]),
        (&["list", "sub"], &[]),
        (&["list", "sub/"], &[]),
        (&["list", "nonexist"], &[]),
        (&["list", "nonexist.rrd"], &[]),
        (&["list", "a.rrd/"], &[]),
        (&["list", "empty"], &[]),
        (&["list", "d2"], &[]),
        (&["list", "--recursive", "d2"], &[]),
        (&["list", "--recursive", "."], &[]),
        (&["list", "*.rrd"], &[]),
        (&["list", "./*.rrd"], &[]),
        (&["list", "d2/*.rrd"], &[]),
        (&["list", "--recursive", "d2/*.rrd"], &[]),
        (&["list", "d2/*.none"], &[]),
        (&["list", "d2"], &[("RRDCACHED_ADDRESS", "")]),
        (&["list", "--daemon", "", "d2"], &[]),
    ]);
}

// bad_format_imginfo (rrd_graph.c:5880): the image is written, then the
// format is checked and nothing is printed.
#[test]
fn graph_imginfo_format_errors() {
    let base = [
        "graph",
        "o.png",
        "--start",
        "1700000000",
        "--end",
        "1700000600",
        "DEF:x=a.rrd:x:AVERAGE",
        "LINE1:x#ff0000",
    ];
    let cases = [
        "--imginfo=<IMG%s>",
        "--imginfo=%s %lu %d",
        "--imginfo=%s %lu %lu %",
    ]
    .map(|imginfo| {
        let mut args = base.to_vec();
        args.push(imginfo);
        args
    });
    let cases = cases
        .iter()
        .map(|args| (args.as_slice(), &[][..]))
        .collect::<Vec<_>>();
    each(&cases);
}

// rrdupdate.c: rrdupdate, rrdcreate and rrdinfo, found through PATH so
// argv[0] is the same bare name for both programs.
#[test]
fn rrdupdate_rrdcreate_and_rrdinfo_executables() {
    if !pinned_rrdtool_available() {
        oracle_skip!("skipping: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let upstream_bin = Path::new(
        &String::from_utf8(
            Command::new("sh")
                .args(["-c", "command -v rrdtool"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned(),
    )
    .parent()
    .unwrap()
    .to_path_buf();
    let rondi_bin = tempfile::tempdir().unwrap();
    for name in ["rrdupdate", "rrdcreate", "rrdinfo"] {
        symlink(env!("CARGO_BIN_EXE_rondi"), rondi_bin.path().join(name)).unwrap();
    }
    let cases: &[&[&str]] = &[
        &["rrdcreate"],
        &[
            "rrdcreate",
            "n.rrd",
            "--start",
            "1700000000",
            "DS:x:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:10",
        ],
        &["rrdcreate", "n.rrd", "-Z"],
        &["rrdinfo"],
        &["rrdinfo", "nonexist.rrd"],
        &["rrdinfo", "a.rrd", "b.rrd"],
        &["rrdupdate"],
        &["rrdupdate", "a.rrd"],
        &["rrdupdate", "a.rrd", "1700000300:1"],
        &["rrdupdate", "-Q", "a.rrd", "1:1"],
        &["rrdupdate", "a.rrd", "1700000900:3"],
    ];
    let mut failures = Vec::new();
    for case in cases {
        let mut outcomes = Vec::new();
        for bin in [upstream_bin.as_path(), rondi_bin.path()] {
            let dir = tempfile::tempdir().unwrap();
            fixture(dir.path());
            let out = Command::new(case[0])
                .args(&case[1..])
                .current_dir(dir.path())
                .env("PATH", bin)
                .env("LC_ALL", "C")
                .env_remove("RRDCACHED_ADDRESS")
                .output()
                .unwrap();
            outcomes.push((
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                std::fs::read(dir.path().join("a.rrd"))
                    .unwrap_or_default()
                    .len(),
            ));
        }
        if outcomes[0] != outcomes[1] {
            failures.push(format!("{case:?}: {:?} vs {:?}", outcomes[0], outcomes[1]));
        }
    }
    assert!(
        failures.is_empty(),
        "diverging cases:\n{}",
        failures.join("\n")
    );
}
