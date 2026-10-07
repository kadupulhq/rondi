#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    _temp: tempfile::TempDir,
    alias: PathBuf,
    database: PathBuf,
}

fn pinned_rrdtool() -> bool {
    Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
}

// Sixty 10-second samples with an unknown every seventh row, so CDEFs that
// replace unknowns and VDEFs over the graph buffer see a mix of both.
fn fixture() -> Option<Fixture> {
    if !pinned_rrdtool() {
        eprintln!("skipping graph/xport differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("a.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:100",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let mut update = Command::new("rrdtool");
    update.arg("update").arg(&database);
    for row in 1..=60_i64 {
        let value = if row % 7 == 0 {
            String::from("U")
        } else {
            format!("{}.5", (row * 37) % 23 - 7)
        };
        update.arg(format!("{}:{value}", 1_000_000_000 + row * 10));
    }
    let updated = update.output().unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture {
        _temp: temp,
        alias,
        database,
    })
}

fn run(program: &Path, args: &[String]) -> Output {
    Command::new(program).args(args).output().unwrap()
}

fn assert_same_stdout(fixture: &Fixture, args: &[String]) {
    let expected = run(Path::new("rrdtool"), args);
    let actual = run(&fixture.alias, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{args:?}\nupstream stderr: {}\nrondi stderr: {}",
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout),
        "{args:?}"
    );
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn xport_args(fixture: &Fixture, extra: &[&str], elements: &[String]) -> Vec<String> {
    let mut args = strings(&["xport", "--start", "1000000000", "--end", "1000000600"]);
    args.extend(strings(extra));
    args.push(format!("DEF:x={}:x:AVERAGE", fixture.database.display()));
    args.extend(elements.iter().cloned());
    args
}

#[test]
fn xport_prints_negative_zero_without_a_sign() {
    let Some(fixture) = fixture() else { return };
    let elements = strings(&["CDEF:c=x,0,*,-1,*", "XPORT:c:c"]);
    for extra in [&[][..], &["--json"]] {
        assert_same_stdout(&fixture, &xport_args(&fixture, extra, &elements));
    }
}

#[test]
fn xport_xml_prints_infinities_and_json_prints_null() {
    let Some(fixture) = fixture() else { return };
    let elements = strings(&[
        "CDEF:p=x,INF,+",
        "CDEF:n=x,NEGINF,+",
        "XPORT:p:p",
        "XPORT:n:n",
    ]);
    for extra in [&[][..], &["--json"], &["--enumds"]] {
        assert_same_stdout(&fixture, &xport_args(&fixture, extra, &elements));
    }
}

// rrd_xport.c copies legend and PRINT text into the XML document without
// escaping it.
#[test]
fn xml_output_writes_legend_and_print_text_verbatim() {
    let Some(fixture) = fixture() else { return };
    let def = format!("DEF:x={}:x:AVERAGE", fixture.database.display());
    assert_same_stdout(
        &fixture,
        &xport_args(&fixture, &[], &strings(&["XPORT:x:a & <b>"])),
    );
    let mut graph = strings(&[
        "graph",
        "-",
        "--imgformat",
        "XML",
        "--start",
        "1000000000",
        "--end",
        "1000000600",
    ]);
    graph.extend([
        def,
        String::from("LINE1:x#ff0000:x & <y>"),
        String::from("VDEF:v=x,MAXIMUM"),
        String::from("PRINT:v:%6.2lf & <z>"),
        String::from("GPRINT:v:%6.2lf <&>"),
    ]);
    assert_same_stdout(&fixture, &graph);
}
