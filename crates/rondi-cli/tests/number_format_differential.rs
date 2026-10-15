#![cfg(unix)]
//! Differential tests for the number formatting of xport and graph output:
//! `rrd_snprintf` (rrd_snprintf.c fmtflt) for data values and xport PRINT
//! entries, and the C library printf for graph PRINT/GPRINT, under the C
//! locale and de_DE.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    temp: tempfile::TempDir,
    alias: PathBuf,
}

fn fixture() -> Option<Fixture> {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        });
    if !pinned {
        oracle_skip!("skipping number format differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("a.rrd").display().to_string();
    let run = |args: &[&str]| {
        let output = Command::new("rrdtool").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&[
        "create",
        &file,
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:x:GAUGE:30:U:U",
        "RRA:AVERAGE:0.5:1:200",
    ]);
    let updates: Vec<String> = (1..=30_i64)
        .map(|i| {
            format!(
                "{}:{}.{}",
                1_000_000_000 + i * 10,
                (i * 37) % 23 - 7,
                i % 10
            )
        })
        .collect();
    let mut args = vec!["update", file.as_str()];
    args.extend(updates.iter().map(String::as_str));
    run(&args);
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture { temp, alias })
}

impl Fixture {
    fn compare(&self, args: &[String], locale: &str) -> Result<(), String> {
        self.compare_with(args, "LC_ALL", locale)
    }

    fn compare_with(&self, args: &[String], variable: &str, locale: &str) -> Result<(), String> {
        let run = |program: &Path| -> Output {
            Command::new(program)
                .args(args)
                .current_dir(self.temp.path())
                .env("TZ", "UTC")
                .env_remove("LC_ALL")
                .env_remove("LC_NUMERIC")
                .env_remove("LANG")
                .env(variable, locale)
                .output()
                .unwrap()
        };
        let text = |output: &Output| {
            (
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        };
        let (upstream, rondi) = (text(&run(Path::new("rrdtool"))), text(&run(&self.alias)));
        if upstream == rondi {
            Ok(())
        } else {
            Err(format!(
                "{variable}={locale} {args:?}\n  rondi:    {rondi:?}\n  upstream: {upstream:?}"
            ))
        }
    }

    fn xport_cdef(&self, cdef: &str) -> Vec<String> {
        [
            "xport",
            "-s",
            "1000000000",
            "-e",
            "1000000300",
            "DEF:x=a.rrd:x:AVERAGE",
            &format!("CDEF:c={cdef}"),
            "XPORT:c",
        ]
        .map(String::from)
        .to_vec()
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

// fmtflt (rrd_snprintf.c:1111) scales by repeated powers of ten and rounds
// the scaled fraction, so near-ties and subnormals differ from libc printf.
#[test]
fn xport_values_use_rrd_snprintf_rounding() {
    let Some(f) = fixture() else { return };
    let mut cdefs = vec![
        String::from("x,0,*,251864975795e-7,+"),
        String::from("x,1e-310,*"),
        String::from("x,RAD2DEG"),
        String::from("x,0,*,-1,*"),
        String::from("x,0,/"),
        String::from("x,1e300,*,1e10,*"),
    ];
    // Fixed pseudo-random mantissas and exponents.
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    for _ in 0..60 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let mantissa = seed % 10_000_000_000_000;
        let exponent = (seed >> 50) as i64 % 40 - 20;
        cdefs.push(format!("x,0,*,{mantissa}e{exponent},+"));
    }
    check_all(cdefs.iter().map(|cdef| f.compare(&f.xport_cdef(cdef), "C")));
}

fn locale_available(name: &str) -> bool {
    Command::new("locale")
        .arg("-a")
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.eq_ignore_ascii_case(name) || line == name.replace("UTF-8", "utf8")
            })
        })
}

// rrd_tool.c:464 sets LC_ALL from the environment. rrd_snprintf is built
// without localeconv, so xport values and xport PRINT entries keep '.',
// while graph print_calc and graphv values use the C library and follow
// LC_NUMERIC.
#[test]
fn locale_decimal_point_follows_the_printf_used() {
    let Some(f) = fixture() else { return };
    if !locale_available("de_DE.UTF-8") {
        oracle_skip!("skipping locale differential: de_DE.UTF-8 is not installed");
        return;
    }
    let base = |command: &str, target: Option<&str>| {
        let mut args = vec![command.to_owned()];
        args.extend(target.map(str::to_owned));
        args.extend(
            [
                "--start",
                "1000000000",
                "--end",
                "1000000300",
                "DEF:x=a.rrd:x:AVERAGE",
                "VDEF:v=x,AVERAGE",
                "PRINT:v:%6.2lf",
                "GPRINT:v:%8.3lf %s",
            ]
            .map(String::from),
        );
        args
    };
    let mut xport_json = base("xport", None);
    xport_json.insert(1, String::from("--json"));
    xport_json.push(String::from("XPORT:x"));
    let mut xport_xml = base("xport", None);
    xport_xml.push(String::from("XPORT:x"));
    let mut graph_json = base("graphv", Some("-"));
    graph_json.extend(["--imgformat", "JSON", "LINE1:x#ff0000:x"].map(String::from));
    let mut graph_csv = base("graph", Some("-"));
    graph_csv.extend(["--imgformat", "CSV", "LINE1:x#ff0000:x"].map(String::from));
    let mut graph_print = base("graph", Some("graph-output.png"));
    graph_print.extend(["--imgformat", "XML", "LINE1:x#ff0000:x"].map(String::from));
    // PNG graphv: print[n], value_min/value_max (rrd_info_print's printf)
    // and the GPRINT legend.
    let mut graphv_png = base("graphv", Some("graph-output.png"));
    graphv_png.extend(["-l", "-0.5", "LINE1:x#ff0000:x"].map(String::from));
    let cases = [
        xport_json,
        xport_xml,
        graph_json,
        graph_csv,
        graph_print,
        graphv_png,
    ];
    check_all(
        [
            ("LC_ALL", "C"),
            ("LC_ALL", "de_DE.UTF-8"),
            ("LC_NUMERIC", "de_DE.UTF-8"),
            ("LANG", "de_DE.UTF-8"),
        ]
        .iter()
        .flat_map(|setting| cases.iter().map(move |args| (setting, args)))
        .map(|((variable, locale), args)| f.compare_with(args, variable, locale)),
    );
}

// Edge cases of format-strings.md sections 8 (FS-24 to FS-52) against the
// oracle: GPRINT cuts, %s found inside %%s, the text of an unknown time,
// the time a strftime item shows, duration arithmetic and the silent end
// of a duration printf failure.
#[test]
fn print_edge_cases_match_rrdtool() {
    let Some(f) = fixture() else { return };
    let base = |command: &str, items: &[&str]| -> Vec<String> {
        [
            command,
            "/dev/null",
            "--start",
            "1000000000",
            "--end",
            "1000000300",
            "DEF:x=a.rrd:x:AVERAGE",
            "VDEF:v=x,MAXIMUM",
            "VDEF:a=x,AVERAGE",
            "CDEF:big=x,1e300,*",
            "VDEF:b=big,MAXIMUM",
            "CDEF:ms=x,0,*,1234567.89,+",
            "VDEF:d=ms,MAXIMUM",
        ]
        .iter()
        .chain(items)
        .map(|arg| (*arg).to_owned())
        .collect()
    };
    let long_format = format!("GPRINT:v:{}%10lf", "a".repeat(190));
    let cases = [
        base("graphv", &["GPRINT:v:%200lf", "GPRINT:v:%2147483648lf"]),
        base("graphv", &[long_format.as_str(), "GPRINT:b:%s:valstrftime"]),
        base(
            "graph",
            &["PRINT:v:%lf %%S", "PRINT:v:%%s%lf", "PRINT:v:%le %S"],
        ),
        base(
            "graph",
            &[
                "PRINT:a:%Y-%m-%d %H %% %q %j %F %T %EY %5Y <%E>:strftime",
                "PRINT:x:MAX:%Y:strftime",
                "PRINT:v:%lf",
                "PRINT:x:MAX:%Y:strftime",
            ],
        ),
        base(
            "graph",
            &[
                "PRINT:d:%8.3S %08.2f %.2147483647H:valstrfduration",
                "PRINT:d:|%3000000000H|:valstrfduration",
                "PRINT:d:%4294967297H %.4294967297H %.H:valstrfduration",
            ],
        ),
        base("graph", &["PRINT:d:%.2147483648H:valstrfduration"]),
        base(
            "graph",
            &["PRINT:v:%lf", "PRINT:d:|%2147483646H:valstrfduration"],
        ),
        base("graph", &["GPRINT:d:%2147483647H:valstrfduration"]),
    ];
    check_all(cases.iter().map(|args| f.compare(args, "C")));
}

// Random value formats against the oracle: xport PRINT entries use
// RRDtool's own printf, graph PRINT lines the C library's.
#[test]
fn random_print_formats_match_both_printers() {
    let Some(f) = fixture() else { return };
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move |limit: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % limit
    };
    let window = ["--start", "1000000000", "--end", "1000000300"];
    let mut results = Vec::new();
    for _ in 0..6 {
        let mut items = vec![String::from("DEF:x=a.rrd:x:AVERAGE")];
        for i in 0..20 {
            let mantissa = next(10_000_000_000_000_000);
            let exponent = next(80) as i64 - 40;
            let sign = if next(4) == 0 { "-" } else { "" };
            let flag = ["", "", "-", "+", " ", "0", "#"][next(7) as usize];
            let width = ["", "", "12", "30"][next(4) as usize];
            let precision = next(23);
            let conversion = ["le", "lf", "lg", "lE", "lG"][next(5) as usize];
            items.push(format!("CDEF:c{i}=x,0,*,{sign}{mantissa}e{exponent},+"));
            items.push(format!("VDEF:v{i}=c{i},MAXIMUM"));
            items.push(format!(
                "PRINT:v{i}:[%{flag}{width}.{precision}{conversion}]"
            ));
        }
        let command = |head: &[&str], tail: &[&str]| -> Vec<String> {
            head.iter()
                .chain(window.iter())
                .map(|arg| (*arg).to_owned())
                .chain(items.iter().cloned())
                .chain(tail.iter().map(|arg| (*arg).to_owned()))
                .collect()
        };
        results.push(f.compare(&command(&["xport"], &["XPORT:x"]), "C"));
        results.push(f.compare(&command(&["graph", "/dev/null"], &[]), "C"));
    }
    check_all(results);
}
