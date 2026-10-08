#![cfg(unix)]
//! Differential tests for `rrd_graph_options` (rrd_graph.c:5042): optparse
//! forms and the full long-option table with each option's validation.
//! Each test compares the pinned RRDtool 1.11.0 executable with the Rondi
//! alias on identical arguments in TZ=UTC. PNG bytes are never compared;
//! `graph` writes to /dev/null so stdout carries only the WxH line and PRINT
//! output, and `graphv` comparisons stop at the `image = BLOB_SIZE` key.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RANGE: [&str; 4] = ["--start", "1000000000", "--end", "1000001500"];

struct Fixture {
    temp: tempfile::TempDir,
    alias: PathBuf,
}

fn upstream_ok(args: &[String]) {
    let output = Command::new("rrdtool").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Option<Fixture> {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        });
    if !pinned {
        oracle_skip!("skipping graph option differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a.rrd").display().to_string();
    let b = temp.path().join("b.rrd").display().to_string();
    let owned = |items: &[&str]| {
        items
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>()
    };
    upstream_ok(&owned(&[
        "create",
        &a,
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:x:GAUGE:30:U:U",
        "DS:y:GAUGE:30:U:U",
        "RRA:AVERAGE:0.5:1:200",
    ]));
    upstream_ok(&owned(&[
        "create",
        &b,
        "--start",
        "1000000000",
        "--step",
        "30",
        "DS:z:GAUGE:90:U:U",
        "RRA:AVERAGE:0.5:1:100",
    ]));
    let mut update = vec![String::from("update"), a.clone()];
    for i in 1..=150_i64 {
        let x = if i % 7 == 0 {
            String::from("U")
        } else {
            format!("{}.5", (i * 37) % 23 - 7)
        };
        update.push(format!("{}:{x}:{}", 1_000_000_000 + i * 10, i % 5));
    }
    upstream_ok(&update);
    let mut update = vec![String::from("update"), b.clone()];
    for i in 1..=50_i64 {
        let z = if i % 6 == 0 {
            String::from("U")
        } else {
            format!("{}.25", (i * 13) % 11 - 3)
        };
        update.push(format!("{}:{z}", 1_000_000_000 + i * 30));
    }
    upstream_ok(&update);
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture { temp, alias })
}

impl Fixture {
    fn run(&self, program: &Path, args: &[String]) -> Output {
        Command::new(program)
            .args(args)
            .current_dir(self.temp.path())
            .env("TZ", "UTC")
            .output()
            .unwrap()
    }

    /// `graph /dev/null <range> DEF:x DEF:z <elements>`.
    fn graph(&self, elements: &[&str]) -> Vec<String> {
        let mut args = vec![String::from("graph"), String::from("/dev/null")];
        args.extend(RANGE.iter().map(|s| s.to_string()));
        args.push(String::from("DEF:x=a.rrd:x:AVERAGE"));
        args.push(String::from("DEF:z=b.rrd:z:AVERAGE"));
        args.extend(elements.iter().map(|s| s.to_string()));
        args
    }

    fn assert_same(&self, args: &[String]) {
        let upstream = self.run(Path::new("rrdtool"), args);
        let rondi = self.run(&self.alias, args);
        let text = |output: &Output| {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let head: Vec<_> = stdout
                .lines()
                .take_while(|l| !l.starts_with("image = "))
                .collect();
            (
                output.status.code(),
                head.join("\n"),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
        assert_eq!(text(&rondi), text(&upstream), "{args:?}");
    }

    fn assert_all_same(&self, cases: &[Vec<String>]) {
        let mut failures = Vec::new();
        for args in cases {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.assert_same(args)))
                .is_err()
            {
                failures.push(format!("{args:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "diverging cases:\n{}",
            failures.join("\n")
        );
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

// rrd_graph.c:5042 uses optparse_long: `--opt=value`, attached short values,
// bundled flags, `--`, and options before the filename are all accepted.
#[test]
fn graph_options_use_optparse_semantics() {
    let Some(f) = fixture() else { return };
    let p = "PRINT:x:AVERAGE:%6.2lf";
    f.assert_all_same(&[
        args(&[
            "graph",
            "/dev/null",
            "--start=1000000000",
            "--end=1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "/dev/null",
            "-s1000000000",
            "-e1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "--start",
            "1000000000",
            "--end",
            "1000001500",
            "/dev/null",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "/dev/null",
            "-gj",
            "--start",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "/dev/null",
            "--start",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
            "--",
        ]),
        args(&[
            "graph",
            "/dev/null",
            "--sta",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "/dev/null",
            "--start",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
            "--upper-limit",
        ]),
    ]);
}

// Options emitted by Kadupul lib/rrd.php (lines 1117-1331, 2642-2714) plus the
// rest of the rrd_graph.c:5063 long-option table.
#[test]
fn graph_accepts_the_full_rrd_graph_option_table() {
    let Some(f) = fixture() else { return };
    let p = "PRINT:x:AVERAGE:%6.2lf";
    let cases: Vec<Vec<String>> = [
        vec!["--pango-markup"],
        vec!["--font", "TITLE:10:"],
        vec!["--font=LEGEND:8:"],
        vec!["--units=si"],
        vec!["--slope-mode"],
        vec!["--units-exponent=3"],
        vec!["--units-length", "5"],
        vec!["--tabwidth", "10"],
        vec!["--watermark", "W"],
        vec!["--y-grid=5:2"],
        vec!["--y-grid", "none"],
        vec!["--x-grid", "MINUTE:1:HOUR:1:HOUR:1:0:%H"],
        vec!["--alt-y-grid"],
        vec!["--no-gridfit"],
        vec!["--dynamic-labels"],
        vec!["--right-axis", "1:0"],
        vec!["--right-axis-label", "r"],
        vec!["--right-axis-format", "%1.0lf"],
        vec!["--legend-position", "east"],
        vec!["--zoom", "2"],
        vec!["-m", "2"],
        vec!["--no-minor"],
        vec!["--logarithmic"],
        vec!["--interlaced"],
        vec!["--disable-rrdtool-tag"],
        vec!["--use-nan-for-all-missing-data"],
        vec!["--utc"],
        vec!["--week-fmt", "%V"],
        vec!["--graph-render-mode", "mono"],
        vec!["--font-render-mode", "normal"],
        vec!["--font-smoothing-threshold", "3"],
        vec!["--alt-y-mrtg"],
        vec!["--border", "100"],
        vec!["--units=xx"],
        vec!["--legend-position=foo"],
        vec!["--legend-direction=foo"],
        vec!["--zoom", "0"],
        vec!["-y", "0:2"],
        vec!["-x", "bad"],
        vec!["--right-axis", "bad"],
        vec!["-l", "abc"],
        vec!["-w", "10x"],
        vec!["--maxrows", "5"],
        vec!["-c", "BACK#zz"],
        vec!["-c", "FOO#ffffff"],
        vec!["-a", "png"],
        vec!["-a", "GIF"],
    ]
    .iter()
    .map(|extra| {
        let mut elements = extra.clone();
        elements.push(p);
        f.graph(&elements)
    })
    .collect();
    f.assert_all_same(&cases);
}
