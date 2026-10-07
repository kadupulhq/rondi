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

// rrd_tool.c prints "WxH" unless an exact --imginfo/-f argument is present,
// then the image_info line and every PRINT result. Rondi's PNG canvas size
// is not pixel-compatible yet, so its dimension line is checked against the
// written file instead of the upstream text.
#[test]
fn graph_to_file_prints_dimensions_and_print_lines() {
    let Some(fixture) = fixture() else { return };
    let def = format!("DEF:x={}:x:AVERAGE", fixture.database.display());
    let image = fixture._temp.path().join("graph.png");
    let elements = [
        def.as_str(),
        "LINE1:x#ff0000",
        "VDEF:v=x,MAXIMUM",
        "PRINT:v:%6.2lf",
        "GPRINT:v:%6.2lf",
        "PRINT:x:AVERAGE:%6.2lf",
    ];
    let graph = |output: &Path, options: &[&str]| {
        let mut args = vec![String::from("graph"), output.display().to_string()];
        args.extend(strings(&["--start", "1000000000", "--end", "1000000600"]));
        args.extend(strings(options));
        args.extend(strings(&elements));
        args
    };
    let lines = |output: Output| -> Vec<String> {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    };
    // Canvas sizes are replaced by placeholders before comparing, so only
    // which lines appear and their order are checked against upstream.
    let normalize = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                let is_size = line.split_once('x').is_some_and(|(width, height)| {
                    [width, height]
                        .iter()
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                });
                if is_size {
                    String::from("<size>")
                } else if line.starts_with("<IMG ") {
                    String::from("<imginfo>")
                } else {
                    line.clone()
                }
            })
            .collect()
    };
    let imginfo = "<IMG %s %lu %lu>";
    let imginfo_equals = format!("--imginfo={imginfo}");
    for options in [
        vec![],
        vec!["--imginfo", imginfo],
        vec!["-f", imginfo],
        vec![imginfo_equals.as_str()],
    ] {
        let args = graph(&image, &options);
        let expected = lines(run(Path::new("rrdtool"), &args));
        let actual = lines(run(&fixture.alias, &args));
        assert_eq!(normalize(&actual), normalize(&expected), "{options:?}");
        if normalize(&actual)[0] == "<size>" {
            let bytes = std::fs::read(&image).unwrap();
            let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!(actual[0], format!("{width}x{height}"));
        }
    }
    let xml = fixture._temp.path().join("graph.xml");
    assert_same_stdout(&fixture, &graph(&xml, &["--imgformat", "XML"]));
    assert_same_stdout(&fixture, &graph(Path::new("-"), &["--imgformat", "XML"]));
}

// graphv layout keys and image bytes are not compatible yet, so graphv
// comparisons are limited to the exit status and print[] lines.
fn assert_same_prints(fixture: &Fixture, args: &[String]) {
    let prints = |output: &Output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("print["))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let expected = run(Path::new("rrdtool"), args);
    let actual = run(&fixture.alias, args);
    assert_eq!(
        actual.status.code(),
        expected.status.code(),
        "{args:?}\nupstream stderr: {}\nrondi stderr: {}",
        String::from_utf8_lossy(&expected.stderr),
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(prints(&actual), prints(&expected), "{args:?}");
}

fn graph_args(
    fixture: &Fixture,
    command: &str,
    options: &[&str],
    elements: &[&str],
) -> Vec<String> {
    let mut args = strings(&[command, "-", "--start", "1000000000", "--end", "1000000600"]);
    args.extend(strings(options));
    args.push(format!("DEF:x={}:x:AVERAGE", fixture.database.display()));
    args.push(String::from("LINE1:x#ff0000"));
    args.extend(strings(elements));
    args
}

// rrd_graph.c print_calc keeps one magnitude across PRINT and GPRINT: the
// first %S (or any %s) sets it, later %S values reuse it, and a zero %S
// result leaves it unset. auto_scale divides by powers of --base.
#[test]
fn print_si_magnitude_is_shared_and_honors_base() {
    let Some(fixture) = fixture() else { return };
    let elements = [
        "CDEF:k=x,1000,*",
        "CDEF:m=x,1000000,*",
        "CDEF:z=x,0,*",
        "VDEF:kmax=k,MAXIMUM",
        "VDEF:mmin=m,MINIMUM",
        "VDEF:zmax=z,MAXIMUM",
        "VDEF:xmax=x,MAXIMUM",
        "PRINT:zmax:%6.2lf %S",
        "PRINT:kmax:%6.2lf %S",
        "GPRINT:mmin:%6.2lf %S",
        "PRINT:mmin:%6.2lf %S",
        "PRINT:xmax:%6.2lf %s",
        "PRINT:kmax:%6.2lf %S",
        "PRINT:mmin:%6.2lf",
        "PRINT:kmax:%6.2lf %S",
    ];
    for options in [
        &[][..],
        &["--base", "1024"],
        &["-b", "1000"],
        &["--base=1024"],
        &["--base", "1001"],
    ] {
        assert_same_prints(
            &fixture,
            &graph_args(&fixture, "graphv", options, &elements),
        );
        let mut xml = options.to_vec();
        xml.extend(["--imgformat", "XML"]);
        assert_same_stdout(&fixture, &graph_args(&fixture, "graph", &xml, &elements));
    }
}

// RRDtool's graph buffer runs one step past the last full row before --end.
// CDEFs are evaluated over that slot too, so `x,UN,0,x,IF` turns its
// unknown into zero, and with data past --end the slot holds a real sample.
#[test]
fn graph_cdefs_and_vdefs_cover_the_right_edge_slot() {
    let Some(fixture) = fixture() else { return };
    let mut elements = vec!["CDEF:c=x,UN,0,x,IF"];
    let vdefs = ["AVERAGE", "STDEV", "LAST", "TOTAL", "LSLSLOPE", "MAXIMUM"];
    let definitions = vdefs
        .iter()
        .map(|function| format!("VDEF:v{function}=c,{function}"))
        .collect::<Vec<_>>();
    let prints = vdefs
        .iter()
        .map(|function| format!("PRINT:v{function}:%.12le"))
        .collect::<Vec<_>>();
    elements.extend(definitions.iter().map(String::as_str));
    elements.extend(prints.iter().map(String::as_str));
    elements.extend([
        "PRINT:c:AVERAGE:%.12le",
        "PRINT:c:LAST:%.12le",
        "PRINT:x:LAST:%.12le",
    ]);
    for end in ["1000000600", "1000000300", "1000000305"] {
        let mut args = strings(&["graphv", "-", "--start", "1000000000", "--end", end]);
        args.push(format!("DEF:x={}:x:AVERAGE", fixture.database.display()));
        args.push(String::from("LINE1:x#ff0000"));
        args.extend(strings(&elements));
        assert_same_prints(&fixture, &args);
    }
}

// rrd_graph_helper.c accepts DEF options anywhere after vname=rrd, with `\:`
// escaping a colon. A step coarser than the fetched archive goes through
// rrd_reduce_data, which also applies to the export-wide --step.
#[test]
fn def_options_match_rrdtool_graph_helper() {
    let Some(fixture) = fixture() else { return };
    let colon_path = fixture._temp.path().join("with:colon.rrd");
    std::fs::copy(&fixture.database, &colon_path).unwrap();
    let database = fixture.database.display().to_string();
    let escaped = colon_path.display().to_string().replace(':', "\\:");
    let cases = [
        (vec![], format!("{database}:x:AVERAGE:step=30")),
        (vec![], format!("{database}:step=30:x:AVERAGE")),
        (vec![], format!("{database}:x:AVERAGE:step=25")),
        (vec![], format!("{database}:x:AVERAGE:step=40:reduce=MAX")),
        (vec![], format!("{database}:x:AVERAGE:reduce=MIN:step=60")),
        (vec![], format!("{database}:x:AVERAGE:step=30:reduce=LAST")),
        (vec![], format!("{database}:x:AVERAGE:start=1000000050")),
        (vec![], format!("{database}:x:AVERAGE:end=1000000300")),
        (
            vec![],
            format!("{database}:x:AVERAGE:start=end-90:end=1000000400"),
        ),
        (vec![], format!("{escaped}:x:AVERAGE")),
        (vec!["--step", "30"], format!("{database}:x:AVERAGE")),
        (vec!["--step", "20"], format!("{database}:x:MAX")),
        (vec![], format!("{database}:x:AVERAGE:foo=1")),
        (vec![], format!("{database}:x:AVERAGE:extra")),
        (vec![], format!("{database}:x:AVERAGE:step=0")),
        (vec![], format!("{database}:x:AVERAGE:step=3x")),
        (vec![], format!("{database}:x:AVERAGE:step=30:step=20")),
        (vec![], format!("{database}:x:AVERAGE:reduce=BOGUS")),
        (vec![], format!("{database}:x")),
        (
            vec![],
            format!("{database}:x:AVERAGE:start=1000000090:end=1000000050"),
        ),
    ];
    for (options, definition) in cases {
        let mut elements = vec![format!("DEF:x={definition}"), String::from("XPORT:x:x")];
        // RRDtool limits a CDEF to the overlap of its inputs' fetch windows;
        // Rondi does not model per-DEF windows inside CDEFs yet.
        if !definition.contains("start=") && !definition.contains("end=") {
            elements.extend(strings(&["CDEF:c=x,UN,0,x,IF", "XPORT:c:c"]));
        }
        let mut args = strings(&["xport", "--start", "1000000000", "--end", "1000000600"]);
        args.extend(strings(&options));
        args.extend(elements);
        let expected = run(Path::new("rrdtool"), &args);
        let actual = run(&fixture.alias, &args);
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
        assert_eq!(
            String::from_utf8_lossy(&actual.stderr),
            String::from_utf8_lossy(&expected.stderr),
            "{args:?}"
        );
    }
}

// rrd_tool.c prints fetch rows with printf("%0.10e"), so a decimal-comma
// LC_NUMERIC changes the separator.
#[test]
fn fetch_values_follow_lc_numeric() {
    let Some(fixture) = fixture() else { return };
    let args = [
        "fetch",
        fixture.database.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000000",
        "--end",
        "1000000100",
    ];
    let fetch = |program: &Path| {
        Command::new(program)
            .args(args)
            .env("LC_ALL", "de_DE.UTF-8")
            .output()
            .unwrap()
    };
    let expected = fetch(Path::new("rrdtool"));
    if !String::from_utf8_lossy(&expected.stdout).contains("5000000000e+00")
        || !String::from_utf8_lossy(&expected.stdout).contains(',')
    {
        eprintln!("skipping LC_NUMERIC fetch differential: de_DE.UTF-8 is not installed");
        return;
    }
    let actual = fetch(&fixture.alias);
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
}
