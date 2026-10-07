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
