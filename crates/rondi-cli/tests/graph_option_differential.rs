#![cfg(unix)]
//! Differential tests for `rrd_graph_options` (rrd_graph.c:5042): optparse
//! forms and the full long-option table with each option's validation.
//! Each test compares the pinned RRDtool 1.11.0 executable with the Rondi
//! alias on identical arguments in TZ=UTC. PNG bytes are never compared;
//! `graph` writes to graph-output.png so stdout carries only the WxH line and PRINT
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
    let owned = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
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

    /// `graph graph-output.png <range> DEF:x DEF:z <elements>`.
    fn graph(&self, elements: &[&str]) -> Vec<String> {
        let mut args = vec![String::from("graph"), String::from("graph-output.png")];
        args.extend(RANGE.iter().map(ToString::to_string));
        args.push(String::from("DEF:x=a.rrd:x:AVERAGE"));
        args.push(String::from("DEF:z=b.rrd:z:AVERAGE"));
        args.extend(elements.iter().map(ToString::to_string));
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

    /// Compares status, stdout, stderr and the written file of a command
    /// whose output file is graph-output.png. PNG files compare by their
    /// IHDR size, anything else byte for byte.
    fn assert_same_output(&self, args: &[String]) {
        let file = self.temp.path().join("graph-output.png");
        let outcome = |program: &Path| {
            let _ = std::fs::remove_file(&file);
            let output = self.run(program, args);
            let written = std::fs::read(&file).ok().map(|bytes| {
                if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
                    let size =
                        |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
                    format!("png {}x{}", size(16), size(20))
                } else {
                    format!("{bytes:?}")
                }
            });
            let stdout = String::from_utf8_lossy(&output.stdout);
            let head: Vec<_> = stdout
                .lines()
                .take_while(|l| !l.starts_with("image = "))
                .collect();
            (
                output.status.code(),
                head.join("\n"),
                String::from_utf8_lossy(&output.stderr).into_owned(),
                written,
            )
        };
        let upstream = outcome(Path::new("rrdtool"));
        let rondi = outcome(&self.alias);
        assert_eq!(rondi, upstream, "{args:?}");
    }

    fn assert_all_same(&self, cases: &[Vec<String>]) {
        self.assert_all(cases, Self::assert_same);
    }

    fn assert_all_same_output(&self, cases: &[Vec<String>]) {
        self.assert_all(cases, Self::assert_same_output);
    }

    fn assert_all(&self, cases: &[Vec<String>], check: fn(&Self, &[String])) {
        let mut failures = Vec::new();
        for args in cases {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(self, args))).is_err()
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
    items.iter().map(ToString::to_string).collect()
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
            "graph-output.png",
            "--start=1000000000",
            "--end=1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "graph-output.png",
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
            "graph-output.png",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "graph-output.png",
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
            "graph-output.png",
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
            "graph-output.png",
            "--sta",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            p,
        ]),
        args(&[
            "graph",
            "graph-output.png",
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

// Output goes to a file in the fixture directory because Rondi refuses to
// open /dev/null for writing when it is owned by another user.
// rrd_fetch_empty divides by (end - start) / 100, which is zero for a range
// under 100 seconds and traps in the pinned build, so only Rondi's result is
// checked: the graph is drawn from NaN data.
#[test]
fn use_nan_for_all_missing_data_survives_a_short_range() {
    let Some(f) = fixture() else { return };
    let output = f.run(
        &f.alias,
        &args(&[
            "graph",
            "out.png",
            "--use-nan-for-all-missing-data",
            "--start",
            "1000000000",
            "--end",
            "1000000050",
            "DEF:x=missing.rrd:x:AVERAGE",
            "LINE1:x#ff0000",
        ]),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        String::from_utf8_lossy(&output.stdout).ends_with("x141\n"),
        "{output:?}"
    );
}

// rrdtool has no size limit, so only PNG output with something to draw is
// bounded, and the limit applies before --width sizes any per-pixel buffer.
// Print-only graphs and other formats keep working at any width.
#[test]
fn png_dimension_limit_applies_before_data_processing() {
    let Some(f) = fixture() else { return };
    let graph = |extra: &[&str], elements: &[&str]| {
        let mut command = vec!["graph", "out.png"];
        command.extend(RANGE);
        command.extend(extra);
        command.push("DEF:x=a.rrd:x:AVERAGE");
        command.extend(elements);
        f.run(&f.alias, &args(&command))
    };
    for extra in [&["--width", "2000000000"][..], &["--height", "2000000000"]] {
        let output = graph(extra, &["LINE1:x#ff0000"]);
        assert_eq!(output.status.code(), Some(1), "{extra:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "ERROR: PNG graph dimensions exceed the 4096 pixel safety limit\n",
            "{extra:?}"
        );
    }
    let print_only = graph(&["--width", "2000000000"], &["PRINT:x:AVERAGE:%lf"]);
    assert_eq!(print_only.status.code(), Some(0));
    let xml = graph(
        &["--imgformat", "XML", "--width", "5000"],
        &["LINE1:x#ff0000"],
    );
    assert_eq!(xml.status.code(), Some(0));
}

// graph_cairo_setup sizes the surface as int(ximg * zoom) and image_info
// reports (long)(zoom * ximg), while graphv's image_width stays unscaled.
// A surface cairo cannot create or write fails the save after image_info.
#[test]
fn zoom_scales_the_written_image_and_image_info() {
    let Some(f) = fixture() else { return };
    let info = ["--imginfo", "%s %lu %lu"];
    let line = "LINE1:x#ff0000";
    let cases: Vec<Vec<String>> = [
        vec!["--zoom", "2", line],
        vec!["--zoom", "2", info[0], info[1], line],
        vec!["--zoom", "1.5", info[0], info[1], line],
        vec!["--zoom", "0.333", info[0], info[1], line],
        vec!["--zoom", "2", "--only-graph", line],
        vec![
            "--zoom",
            "2",
            "--full-size-mode",
            "-w",
            "300",
            "-h",
            "200",
            line,
        ],
        vec!["--zoom", "0.001", info[0], info[1], line],
        vec!["--zoom", "100", info[0], info[1], line],
        vec!["--zoom", "1e30", info[0], info[1], line],
        vec!["--zoom", "nan", info[0], info[1], line],
        vec!["--zoom", "2", info[0], info[1], "PRINT:x:AVERAGE:%lf"],
        vec![info[0], info[1], "-a", "CSV", line],
    ]
    .iter()
    .flat_map(|extra| {
        let mut graphv = f.graph(extra);
        graphv[0] = String::from("graphv");
        [f.graph(extra), graphv]
    })
    .collect();
    f.assert_all_same_output(&cases);
}

// An infinite --upper-limit or --lower-limit makes si_unit's magfact infinite,
// so calc_horizontal_grid sees a NaN scaled range and graph_paint stops
// without an error: no file, no output, exit 0. --logarithmic is left out
// because the pinned build loops forever there.
#[test]
fn infinite_limits_abandon_the_graph_silently() {
    let Some(f) = fixture() else { return };
    let line = "LINE1:x#ff0000";
    let cases: Vec<Vec<String>> = [
        vec!["-u", "inf", line],
        vec!["-u", "1e400", line],
        vec!["-l", "-inf", line],
        vec!["-l", "-inf", line, "PRINT:x:AVERAGE:%lf"],
        vec!["-u", "inf", "-r", line],
        vec!["-u", "inf", "-l", "-inf", line],
        vec!["-u", "inf", "-A", line],
        vec!["-u", "inf", "-y", "10:2", line],
        vec!["-u", "inf", "--imginfo", "%s %lu %lu", line],
        vec!["-u", "inf", "-j", line],
        vec!["-l", "inf", line],
        vec!["-u", "-inf", line],
        vec!["-u", "nan", line],
        vec!["-u", "1e308", line],
        vec!["-u", "inf", "PRINT:x:AVERAGE:%lf"],
        vec!["-u", "inf", "-a", "CSV", line],
    ]
    .iter()
    .flat_map(|extra| {
        let mut graphv = f.graph(extra);
        graphv[0] = String::from("graphv");
        [f.graph(extra), graphv]
    })
    .collect();
    f.assert_all_same_output(&cases);
}

// --units-length is an atoi value; a negative one shrinks the y-label space
// below zero. graph_left and image_width are unsigned longs in graphv while
// the graph WxH line prints them signed, and a negative image is a cairo
// surface error.
#[test]
fn negative_units_length_moves_the_graph_origin() {
    let Some(f) = fixture() else { return };
    let line = "LINE1:x#ff0000:legend";
    let cases: Vec<Vec<String>> = [
        vec!["--units-length", "-3", line],
        vec!["--units-length", "-60", line],
        vec!["--units-length", "-1000", line],
        vec!["--units-length", "-1000", "--imginfo", "%s %lu %lu", line],
        vec!["--units-length", "-3", "--right-axis", "1:0", line],
        vec!["--units-length", "-3", "--legend-position", "west", line],
        vec!["--units-length", "-3", "--legend-position", "east", line],
        vec!["--units-length", "-3", "--full-size-mode", line],
        vec!["--units-length", "-3", "--zoom", "2", line],
        vec!["--units-length", "-3", "PRINT:x:AVERAGE:%lf"],
    ]
    .iter()
    .flat_map(|extra| {
        let mut graphv = f.graph(extra);
        graphv[0] = String::from("graphv");
        [f.graph(extra), graphv]
    })
    .collect();
    f.assert_all_same_output(&cases);
}
