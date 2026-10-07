#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    temp: tempfile::TempDir,
    alias: PathBuf,
}

fn rrdtool(args: &[String]) {
    let output = Command::new("rrdtool").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `fine.rrd` holds x and y at a 10-second step and `coarse.rrd` holds z at
/// a 30-second step, so CDEFs combining them run finer than z.
fn fixture() -> Option<Fixture> {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        eprintln!("skipping RPN differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let fine = temp.path().join("fine.rrd").display().to_string();
    let coarse = temp.path().join("coarse.rrd").display().to_string();
    let create = |file: &str, step: &str, sources: &[&str]| {
        let mut args = vec!["create", file, "--start", "1000000000", "--step", step];
        args.extend(sources);
        args.push("RRA:AVERAGE:0.5:1:100");
        rrdtool(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>());
    };
    create(&fine, "10", &["DS:x:GAUGE:30:U:U", "DS:y:GAUGE:30:U:U"]);
    create(&coarse, "30", &["DS:z:GAUGE:90:U:U"]);
    let mut updates = vec![String::from("update"), fine.clone()];
    for index in 1..=60_i64 {
        let x = if index % 7 == 0 {
            String::from("U")
        } else {
            format!("{}.5", (index * 37) % 23 - 7)
        };
        updates.push(format!("{}:{x}:{}", 1_000_000_000 + index * 10, index % 5));
    }
    rrdtool(&updates);
    let mut updates = vec![String::from("update"), coarse.clone()];
    for index in 1..=20_i64 {
        let z = if index % 6 == 0 {
            String::from("U")
        } else {
            format!("{}.25", (index * 13) % 11 - 3)
        };
        updates.push(format!("{}:{z}", 1_000_000_000 + index * 30));
    }
    rrdtool(&updates);
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture { temp, alias })
}

impl Fixture {
    fn file(&self, name: &str) -> String {
        self.temp.path().join(name).display().to_string()
    }

    fn xport_args(&self, cdef: &str) -> Vec<String> {
        vec![
            String::from("xport"),
            String::from("--start"),
            String::from("1000000000"),
            String::from("--end"),
            String::from("1000000600"),
            format!("DEF:x={}:x:AVERAGE", self.file("fine.rrd")),
            format!("DEF:y={}:y:AVERAGE", self.file("fine.rrd")),
            format!("DEF:z={}:z:AVERAGE", self.file("coarse.rrd")),
            format!("CDEF:c={cdef}"),
            String::from("XPORT:c:c"),
        ]
    }

    fn graphv_args(&self, elements: &[&str]) -> Vec<String> {
        let mut args = vec![
            String::from("graphv"),
            String::from("-"),
            String::from("--start"),
            String::from("1000000000"),
            String::from("--end"),
            String::from("1000000600"),
            format!("DEF:x={}:x:AVERAGE", self.file("fine.rrd")),
            String::from("LINE1:x#ff0000"),
        ];
        args.extend(elements.iter().map(|element| element.to_string()));
        args
    }

    fn run_both(&self, args: &[String]) -> (Output, Output) {
        let run = |program: &Path| Command::new(program).args(args).output().unwrap();
        (run(Path::new("rrdtool")), run(&self.alias))
    }

    fn assert_matches(&self, args: &[String]) {
        let (upstream, rondi) = self.run_both(args);
        assert_eq!(rondi.status.code(), upstream.status.code(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stdout),
            String::from_utf8_lossy(&upstream.stdout),
            "{args:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            String::from_utf8_lossy(&upstream.stderr),
            "{args:?}"
        );
    }

    fn assert_print_lines_match(&self, elements: &[&str]) {
        let (upstream, rondi) = self.run_both(&self.graphv_args(elements));
        let print_lines = |output: &Output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| line.starts_with("print["))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(rondi.status.code(), upstream.status.code(), "{elements:?}");
        assert_eq!(print_lines(&rondi), print_lines(&upstream), "{elements:?}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            String::from_utf8_lossy(&upstream.stderr),
            "{elements:?}"
        );
    }
}

// rpn_parse scans numbers with "%40[0-9.e+-]", requires a following comma,
// and converts them with rrd_strtodbl.
#[test]
fn rpn_number_lexing_matches_rpn_parse() {
    let Some(fixture) = fixture() else { return };
    for cdef in [
        "x,1E3,+",
        "x,1.5E1,+",
        "x,1e1100,+",
        "x,1e-1100,+",
        "x,1e,+",
        "x,POP,5",
        "x,1111111111111111111111111111111111111111,+",
        "x,11111111111111111111111111111111111111111,+",
    ] {
        fixture.assert_matches(&fixture.xport_args(cdef));
    }
}

#[test]
fn rpn_errors_use_rrdtool_wording() {
    let Some(fixture) = fixture() else { return };
    for cdef in [
        "x,+",
        "x,PREV(nope),+",
        "x,1,2",
        "+,x5",
        "x,TREND",
        "1,x,TREND",
        "x,1,200,1,PERCENT",
        "x,1,+,PREDICT",
        "x,1,1,1,1,1,INDEX",
    ] {
        fixture.assert_matches(&fixture.xport_args(cdef));
    }
}

// rrd_graph.c data_calc rejects a CDEF whose expression names no DEF or CDEF.
#[test]
fn constant_cdef_is_rejected_like_rrdtool() {
    let Some(fixture) = fixture() else { return };
    for cdef in ["NEWWEEK", "1,2,+", "TIME,UN"] {
        fixture.assert_matches(&fixture.xport_args(cdef));
    }
}

// rrd_rpncalc.c OP_PERCENT reads s[start - 1 + round(percent * n / 100)].
#[test]
fn rpn_percent_rank_and_count_edges_match_rrdtool() {
    let Some(fixture) = fixture() else { return };
    for cdef in [
        "100,x,y,1,10,3,PERCENT,+",
        "x,1,2,3,4,0,3,PERCENT,+,+",
        "x,50,0,PERCENT,+",
        "x,y,1,2,100,3,PERCENT,+,+",
        "x,y,2,1,50,3,PERCENT,+",
        "50,0,PERCENT,x,+",
    ] {
        fixture.assert_matches(&fixture.xport_args(cdef));
    }
}

// RRDtool's AVG and PREDICT sums start at +0.0, so averaging negative zeros
// yields +0.0; dividing by it shows the sign without printing a zero.
#[test]
fn rpn_negative_zero_sums_match_rrdtool() {
    let Some(fixture) = fixture() else { return };
    fixture.assert_matches(&fixture.xport_args("x,POP,0,-1,*,1,AVG,1,EXC,/,0,GT"));
    let mut args = fixture.xport_args("1,0,1,30,n,PREDICT,/,0,GT");
    args.insert(8, String::from("CDEF:n=x,0,*,-1,*"));
    fixture.assert_matches(&args);
}

// vdef_parse scans "%40[0-9.e+-],%29[A-Z]" and converts with rrd_strtodbl.
#[test]
fn vdef_parameter_lexing_matches_vdef_parse() {
    let Some(fixture) = fixture() else { return };
    for vdef in [
        "VDEF:v=x,1E2,PERCENT",
        "VDEF:v=x,95e,PERCENT",
        "VDEF:v=x,95,PERCENT,",
        "VDEF:v=x,MAXIMUM,",
        "VDEF:v=x,150,PERCENT",
        "VDEF:v=x,PERCENT",
        "VDEF:v=x,95,MAXIMUM",
        "VDEF:v=x,95",
        "VDEF:v=x,1-2,PERCENT",
        "VDEF:v=x,1e1100,PERCENT",
        "VDEF:v=x,FOO",
        "VDEF:v=x",
    ] {
        fixture.assert_print_lines_match(&[vdef, "PRINT:v:%lf"]);
    }
}

// auto_scale has no symbol for an infinite magnitude and prints '?'.
#[test]
fn print_si_scale_of_infinity_matches_rrdtool() {
    let Some(fixture) = fixture() else { return };
    fixture.assert_print_lines_match(&[
        "CDEF:c=x,INF,+",
        "VDEF:v=c,MAXIMUM",
        "PRINT:v:%6.2lf %s",
        "CDEF:d=x,NEGINF,+",
        "VDEF:w=d,MINIMUM",
        "PRINT:w:%6.2lf %s",
    ]);
}
