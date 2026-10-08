#![cfg(unix)]
//! Differential tests for the number formatting of xport output, which
//! goes through `rrd_snprintf` (rrd_snprintf.c fmtflt).

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
        let run = |program: &Path| -> Output {
            Command::new(program)
                .args(args)
                .current_dir(self.temp.path())
                .env("TZ", "UTC")
                .env("LC_ALL", locale)
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
                "{locale} {args:?}\n  rondi:    {rondi:?}\n  upstream: {upstream:?}"
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
