#![cfg(unix)]
//! Differential tests for `rrd_xport`'s option table (rrd_xport.c:98) and
//! its atoi/atol values. Each case runs pinned RRDtool 1.11.0 and the Rondi
//! alias with the same arguments and compares stdout, stderr and exit
//! status.

#[macro_use]
mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    temp: tempfile::TempDir,
    alias: PathBuf,
}

fn upstream(args: &[&str]) {
    let output = Command::new("rrdtool").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture() -> Option<Fixture> {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping xport option differential: pinned RRDtool 1.11.0 is not installed");
        return None;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("a.rrd").display().to_string();
    upstream(&[
        "create",
        &file,
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:x:GAUGE:30:U:U",
        "RRA:AVERAGE:0.5:1:200",
    ]);
    let mut updates = vec![String::from("update"), file.clone()];
    for index in 1..=150_i64 {
        let x = if index % 7 == 0 {
            String::from("U")
        } else {
            format!("{}.5", (index * 37) % 23 - 7)
        };
        updates.push(format!("{}:{x}", 1_000_000_000 + index * 10));
    }
    let updates = updates.iter().map(String::as_str).collect::<Vec<_>>();
    upstream(&updates);
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    Some(Fixture { temp, alias })
}

impl Fixture {
    fn def(&self) -> String {
        format!(
            "DEF:x={}:x:AVERAGE",
            self.temp.path().join("a.rrd").display()
        )
    }

    fn assert_matches(&self, extra: &[&str]) {
        let mut args = vec![
            String::from("xport"),
            String::from("-s"),
            String::from("1000000000"),
            String::from("-e"),
            String::from("1000000100"),
        ];
        args.extend(extra.iter().map(|arg| arg.replace("@DEF@", &self.def())));
        let run = |program: &Path| Command::new(program).args(&args).output().unwrap();
        let (up, rondi) = (run(Path::new("rrdtool")), run(&self.alias));
        assert_eq!(rondi.status.code(), up.status.code(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stdout),
            String::from_utf8_lossy(&up.stdout),
            "{args:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            String::from_utf8_lossy(&up.stderr),
            "{args:?}"
        );
    }
}

macro_rules! repro {
    ($name:ident, [$($arg:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            let Some(fixture) = fixture() else { return };
            fixture.assert_matches(&[$($arg),*]);
        }
    };
}

// optparse accepts attached and clustered option arguments.
repro!(
    attached_long_option_value,
    ["--maxrows=20", "@DEF@", "XPORT:x"]
);
repro!(attached_short_option_value, ["-m20", "@DEF@", "XPORT:x"]);
repro!(clustered_short_options, ["-tm", "20", "@DEF@", "XPORT:x"]);
// atol/atoi accept a numeric prefix and turn junk into zero.
repro!(
    maxrows_uses_atol_prefix,
    ["-m", "50abc", "@DEF@", "XPORT:x"]
);
repro!(step_uses_atoi, ["--step", "abc", "@DEF@", "XPORT:x"]);
repro!(
    negative_maxrows_error_text,
    ["-m", "-5", "@DEF@", "XPORT:x"]
);
repro!(missing_option_argument_text, ["@DEF@", "XPORT:x", "-m"]);
repro!(
    start_time_error_prefix,
    ["-s", "garbage", "@DEF@", "XPORT:x"]
);
