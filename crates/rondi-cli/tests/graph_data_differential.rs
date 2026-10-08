#![cfg(unix)]
//! Differential tests for the graph element parser (`rrd_graph_script`),
//! per-element data preparation (`data_fetch`, `data_calc`, `vdef_calc`,
//! `print_calc`) and the RPN calculator against pinned RRDtool 1.11.0.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

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

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// `a.rrd` holds x and y at a 10-second step and `b.rrd` holds z at a
/// 30-second step.
fn fixture() -> Option<Fixture> {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        });
    if !pinned {
        oracle_skip!("skipping graph data differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a.rrd").display().to_string();
    let b = temp.path().join("b.rrd").display().to_string();
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

    fn both(&self, args: &[String]) -> (Output, Output) {
        (
            self.run(Path::new("rrdtool"), args),
            self.run(&self.alias, args),
        )
    }

    fn xport(&self, elements: &[&str]) -> Vec<String> {
        let mut args = owned(&["xport", "-s", "1000000000", "-e", "1000000100"]);
        args.extend(elements.iter().map(|item| item.replace("@A@", "a.rrd")));
        args
    }

    /// `graphv /dev/null` over a 1500-second window with x and z defined.
    fn graphv(&self, elements: &[&str]) -> Vec<String> {
        let mut args = owned(&[
            "graphv",
            "/dev/null",
            "--start",
            "1000000000",
            "--end",
            "1000001500",
            "DEF:x=a.rrd:x:AVERAGE",
            "DEF:z=b.rrd:z:AVERAGE",
        ]);
        args.extend(elements.iter().map(|item| item.to_string()));
        args
    }

    /// Status, stdout and stderr must match exactly.
    fn assert_same(&self, args: &[String]) -> Result<(), String> {
        let (upstream, rondi) = self.both(args);
        let text = |output: &Output| {
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
        if text(&rondi) == text(&upstream) {
            Ok(())
        } else {
            Err(format!(
                "{args:?}\n  rondi:    {:?}\n  upstream: {:?}",
                text(&rondi),
                text(&upstream)
            ))
        }
    }

    /// Status, stderr and the `print[n]` lines must match; image layout keys
    /// are compared elsewhere.
    fn assert_same_prints(&self, args: &[String]) -> Result<(), String> {
        let (upstream, rondi) = self.both(args);
        let text = |output: &Output| {
            let prints: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| line.starts_with("print["))
                .map(str::to_owned)
                .collect();
            (
                output.status.code(),
                prints,
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
        if text(&rondi) == text(&upstream) {
            Ok(())
        } else {
            Err(format!(
                "{args:?}\n  rondi:    {:?}\n  upstream: {:?}",
                text(&rondi),
                text(&upstream)
            ))
        }
    }
}

fn check_all(results: impl IntoIterator<Item = Result<(), String>>) {
    let failures: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(
        failures.is_empty(),
        "diverging cases:\n{}",
        failures.join("\n")
    );
}

// rrd_graph_script (rrd_graph_helper.c:1952) accepts every graph element in
// xport; rrd_xport_fn (rrd_xport.c:280) exports only XPORT columns, and
// rrd_xport_format_addprints still writes PRINT/COMMENT/LINE entries.
#[test]
fn xport_accepts_graph_elements_and_applies_shift() {
    let Some(f) = fixture() else { return };
    let def = "DEF:x=@A@:x:AVERAGE";
    check_all(
        [
            vec![def, "LINE1:x#ff0000", "XPORT:x"],
            vec![def, "PRINT:x:AVERAGE:%lf", "XPORT:x"],
            vec![def, "COMMENT:hi", "TEXTALIGN:left", "XPORT:x"],
            vec![def, "SHIFT:x:3600", "XPORT:x"],
            vec![def, "SHIFT:x:15", "XPORT:x"],
            vec![def, "VDEF:v=x,FIRST", "SHIFT:x:v", "XPORT:x"],
            vec![def, "FOO:x", "XPORT:x"],
            vec![def, "SHIFT:x:abc", "XPORT:x"],
        ]
        .iter()
        .map(|case| f.assert_same(&f.xport(case))),
    );
}

// data_calc (rrd_graph.c:1120) substitutes VDEF values into CDEFs and
// rejects CDEFs without DEF or CDEF inputs.
#[test]
fn vdef_values_substitute_into_cdefs() {
    let Some(f) = fixture() else { return };
    let def = "DEF:x=@A@:x:AVERAGE";
    check_all(
        [
            vec![def, "VDEF:v=x,AVERAGE", "CDEF:c=x,v,-", "XPORT:c"],
            vec![def, "VDEF:v=x,MAXIMUM", "CDEF:c=v", "XPORT:c"],
            vec![def, "VDEF:v=x,AVERAGE", "XPORT:v"],
            vec!["CDEF:c=1,2,+", "XPORT:c"],
        ]
        .iter()
        .map(|case| f.assert_same(&f.xport(case))),
    );
}

// parseArguments and newGraphDescription (rrd_graph_helper.c:264, 505):
// escaped colons, kept quotes, key=value fields, colours on XPORT,
// legend truncation, forward references, vname reuse and error texts.
#[test]
fn element_grammar_matches_rrd_graph_helper() {
    let Some(f) = fixture() else { return };
    let def = "DEF:x=@A@:x:AVERAGE";
    let long_legend = format!("XPORT:x:{}", "L".repeat(300));
    check_all(
        [
            vec![def, "XPORT:x:esc\\:colon"],
            vec![def, "XPORT:x:\"quoted\""],
            vec![def, "XPORT:x:   lead"],
            vec![def, "XPORT:x:legend=foo"],
            vec![def, "XPORT:vname=x:legend=foo"],
            vec![def, "XPORT:x#ff0000:leg"],
            vec![def, "XPORT:x:a:b"],
            vec![def, "XPORT:"],
            vec![def, "XPORT"],
            vec![def, &long_legend],
            vec!["DEF:x", "XPORT:x"],
            vec!["DEF:vname=x:rrd=a.rrd:ds=x:cf=AVERAGE", "XPORT:x"],
            vec!["DEF:x=@A@:x:AVERAGE:debug=1", "XPORT:x"],
            vec!["DEF:x=@A@:x:AVERAGE:daemon=", "XPORT:x"],
            vec![def, "CDEF:c=", "XPORT:c"],
            vec![def, "CDEF:c", "XPORT:c"],
            vec![def, "CDEF:x=x,1,+", "XPORT:x"],
            vec![def, "VDEF:x=x,AVERAGE", "XPORT:x"],
            vec!["XPORT:x", def],
            vec!["CDEF:c=x,1,+", def, "XPORT:c"],
            vec![def, "VDEF:v=x", "XPORT:x"],
            vec![def, "VDEF:v=x,BOGUS", "XPORT:x"],
            vec![def, "VDEF:v=x,95,PERCENTfoo", "XPORT:x"],
            vec![def, "VDEF:v=x,PERCENT", "XPORT:x"],
            vec![def, "VDEF:v=x,5,AVERAGE", "XPORT:x"],
            vec![def, "A:B:C:D:E:F:G:H:I:J:K:L"],
            vec![def, ""],
            vec!["a=b"],
        ]
        .iter()
        .map(|case| f.assert_same(&f.xport(case))),
    );
}

// rpn_parse and rpn_calc (rrd_rpncalc.c:338, 613), including the operand
// order, count conversions and error texts.
#[test]
fn rpn_edge_cases_match_rrd_rpncalc() {
    let Some(f) = fixture() else { return };
    check_all(
        [
            "x,1,+,",
            "",
            "x,POP,-1,AVG",
            "x,-1,REV",
            "x,2000000,AVG",
            "x,INF,-1,ROLL",
            "x,5,INDEX",
            "x,-1,INDEX",
            "20,1,-20,x,PREDICT",
            "-0.5,1,30,x,PREDICT",
            "-5,1,30,x,PREDICT",
            "20,1,30,150,x,PREDICTPERC",
            "10,3,600,x,PREDICT",
            "x,POP,NEGINF,1,2,3,MEDIAN",
            "x,POP,3,NEGINF,1,3,SORT,POP,POP",
            "x,y,+,1,",
            "PREV(x),x,+",
            "x,30,TREND",
            "x,z,+",
        ]
        .iter()
        .map(|cdef| {
            f.assert_same(&f.xport(&[
                "DEF:x=@A@:x:AVERAGE",
                "DEF:y=@A@:y:AVERAGE",
                "DEF:z=b.rrd:z:AVERAGE",
                &format!("CDEF:c={cdef}"),
                "XPORT:c",
            ]))
        }),
    );
}

// OP_PREDICTPERC interpolates `val += deltay * deltax` and the MIN/MAX
// family keep whichever signed zero the C comparisons leave.
#[test]
fn predictperc_and_signed_zero_match() {
    let Some(f) = fixture() else { return };
    let mut cases = Vec::new();
    for percentile in [7, 13, 77, 91] {
        cases.push(format!("300,1,60,{percentile},x,PREDICTPERC"));
    }
    cases.extend(
        [
            "x,POP,0,0,-1,*,MIN",
            "x,POP,0,-1,*,0,MAX",
            "x,POP,0,0,-1,*,MINNAN",
            "x,POP,0,-1,*,0,MAXNAN",
            "x,POP,0,0,-1,*,2,SMAX",
            "x,POP,0,-1,*,0,2,SMIN",
        ]
        .map(String::from),
    );
    check_all(cases.iter().map(|cdef| {
        f.assert_same_prints(&f.graphv(&[
            &format!("CDEF:c={cdef}"),
            "VDEF:a=c,AVERAGE",
            "PRINT:a:%.17le",
            "VDEF:t=c,TOTAL",
            "PRINT:t:%.17le",
            "VDEF:m=c,FIRST",
            "PRINT:m:%.17le",
            "LINE1:x#ff0000",
        ]))
    }));
}

// print_calc (rrd_graph.c:1856-1890) and vdef_calc (6017) read each
// source at its own fetch window and step; the image step is
// max(step, (end-start)/width) (rrd_graph.c:5783).
#[test]
fn prints_use_each_source_window_and_width_step() {
    let Some(f) = fixture() else { return };
    let mut cases = vec![f.graphv(&[
        "LINE1:x#ff0000",
        "VDEF:s=z,LSLSLOPE",
        "VDEF:t=z,TOTAL",
        "VDEF:f=z,FIRST",
        "VDEF:sd=z,STDEV",
        "PRINT:s:%.10le",
        "PRINT:t:%.10le",
        "PRINT:f:%s:strftime",
        "PRINT:sd:%.10le",
        "PRINT:z:AVERAGE:%.10le",
    ])];
    for width in ["400", "100", "30"] {
        cases.push(f.graphv(&[
            "-w",
            width,
            "LINE1:x#ff0000",
            "PRINT:x:MAX:%6.3lf",
            "PRINT:x:AVERAGE:%6.3lf",
            "PRINT:x:MIN:%6.3lf",
        ]));
    }
    cases.push(f.graphv(&[
        "DEF:w=a.rrd:x:AVERAGE:start=1000000500:end=1000000900",
        "CDEF:c=w,2,*",
        "VDEF:v=c,AVERAGE",
        "PRINT:v:%.10le",
        "PRINT:c:MAX:%.10le",
        "PRINT:w:LAST:%.10le",
        "LINE1:x#ff0000",
    ]));
    cases.push(f.graphv(&[
        "DEF:w=a.rrd:x:AVERAGE:step=60:reduce=MAX",
        "VDEF:v=w,AVERAGE",
        "PRINT:v:%.10le",
        "LINE1:x#ff0000",
    ]));
    check_all(cases.iter().map(|args| f.assert_same_prints(args)));
}

// print_calc formatters: strftime on DEF data uses the last VDEF time,
// valstrftime uses gmtime and valstrfduration ports strfduration
// (rrd_graph.c:1684, 1940-1990); HW CFs average like AVERAGE.
#[test]
fn print_formatters_match_print_calc() {
    let Some(f) = fixture() else { return };
    check_all(
        [
            vec![
                "CDEF:t=x,POP,TIME",
                "VDEF:v=t,MAXIMUM",
                "PRINT:v:%Y-%m-%d %H.%M:valstrftime",
            ],
            vec![
                "CDEF:t=x,POP,TIME",
                "VDEF:v=t,MAXIMUM",
                "PRINT:v::valstrftime",
            ],
            vec![
                "CDEF:t=x,1000,*",
                "VDEF:v=t,MAXIMUM",
                "PRINT:v:%H h %m m %s s %f:valstrfduration",
            ],
            vec![
                "CDEF:t=x,1000,*",
                "VDEF:v=t,MINIMUM",
                "PRINT:v:%02H %.2S:valstrfduration",
            ],
            vec![
                "CDEF:t=x,1000,*",
                "VDEF:v=t,MAXIMUM",
                "PRINT:v::valstrfduration",
            ],
            vec![
                "VDEF:v=x,FIRST",
                "PRINT:v:%H%M",
                "PRINT:x:AVERAGE:%H%M:strftime",
            ],
            vec!["VDEF:v=x,AVERAGE", "PRINT:v:%Y %j %n %T %%:strftime"],
            vec!["PRINT:x:HWPREDICT:%6.2lf"],
            vec!["PRINT:x:MINIMUM:%6.2lf"],
            vec!["PRINT:x:AVERAGE:%d"],
            vec!["PRINT:x:AVERAGE"],
            vec!["PRINT:x:AVERAGE:%6.2lf:extra"],
            vec!["PRINT:y:AVERAGE:%6.2lf"],
            vec!["VDEF:v=nope,AVERAGE"],
            vec!["VDEF:v=x,AVERAGE", "VDEF:w=v,MAXIMUM"],
            vec!["DEF:q=a.rrd:nods:AVERAGE", "PRINT:q:AVERAGE:%6.2lf"],
            vec!["DEF:q=nofile.rrd:x:AVERAGE"],
            vec!["DEF:q=a.rrd:y:FOO"],
            vec!["DEF:q=cb//foo:x:AVERAGE"],
        ]
        .iter()
        .map(|extra| {
            let mut elements = vec!["LINE1:x#ff0000"];
            elements.extend(extra.iter().copied());
            f.assert_same_prints(&f.graphv(&elements))
        }),
    );
}

// graph_paint (rrd_graph.c:3975) stops after print_calc when nothing is
// drawn: no image, 0x0 and only print lines.
#[test]
fn print_only_graph_is_not_rendered() {
    let Some(f) = fixture() else { return };
    let graph = |elements: &[&str]| {
        let mut args = f.graphv(elements);
        args[0] = String::from("graph");
        args
    };
    check_all([
        f.assert_same(&graph(&["PRINT:x:MAX:%6.3lf"])),
        f.assert_same(&graph(&["VDEF:v=x,AVERAGE", "PRINT:v:%6.2lf"])),
        f.assert_same(&graph(&["XPORT:x:foo", "PRINT:x:MAX:%6.3lf"])),
        f.assert_same(&f.graphv(&["PRINT:x:MAX:%6.3lf"])),
    ]);
}

// Infinities produced by a CDEF reach xport and graph exports unchanged
// (rrd_xport.c writes them with %0.10e, JSON as null).
#[test]
fn infinities_survive_into_exports() {
    let Some(f) = fixture() else { return };
    let mut json = f.graphv(&["--imgformat", "JSON", "CDEF:c=x,0,/", "LINE1:c#ff0000:c"]);
    json[1] = String::from("-");
    check_all([
        f.assert_same(&f.xport(&["DEF:x=@A@:x:AVERAGE", "CDEF:c=x,0,/", "XPORT:c"])),
        f.assert_same(&json),
    ]);
}

fn start_daemon(dir: &Path, socket: &Path) -> Child {
    let alias = dir.join("rrdcached");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let child = Command::new(&alias)
        .args([
            "-g",
            "-b",
            dir.to_str().unwrap(),
            "-l",
            &format!("unix:{}", socket.display()),
            "-w",
            "3600",
            "-z",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + common::io_timeout();
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "Rondi rrdcached did not open its socket"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child
}

// data_fetch (rrd_graph.c:1003) reads a DEF through its own `daemon=`
// address; cached updates must be visible to both implementations.
#[test]
fn def_daemon_reads_through_rondi_rrdcached() {
    let Some(f) = fixture() else { return };
    let dir = std::fs::canonicalize(f.temp.path()).unwrap();
    let socket = dir.join("d.sock");
    let mut daemon = start_daemon(&dir, &socket);
    let file = dir.join("a.rrd").display().to_string();
    let address = format!("unix\\:{}", socket.display());
    let updated = Command::new("rrdtool")
        .args([
            "update",
            "--daemon",
            &format!("unix:{}", socket.display()),
            &file,
            "1000001510:3:1",
            "1000001520:4:2",
            "1000001530:5:3",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let args = owned(&[
        "xport",
        "-s",
        "1000001400",
        "-e",
        "1000001530",
        &format!("DEF:x={file}:x:AVERAGE:daemon={address}"),
        "XPORT:x",
    ]);
    let result = f.assert_same(&args);
    let _ = daemon.kill();
    let _ = daemon.wait();
    result.unwrap();
}
