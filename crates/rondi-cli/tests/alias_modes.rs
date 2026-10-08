#![cfg(unix)]

#[macro_use]
mod common;

use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Stdio};

fn normalize_rrdtool_compiled_stamp(output: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(output)
        .split_inclusive('\n')
        .map(|line| {
            line.split_once("Compiled ")
                .map(|(prefix, _)| format!("{prefix}Compiled \n"))
                .unwrap_or_else(|| line.to_owned())
        })
        .collect::<String>()
        .into_bytes()
}

#[test]
fn rpn_roll_small_stack_matches_rrdtool_1110_for_shift_range() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping ROLL differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("roll.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:v:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );

    for (prefix, suffix) in [("9", ""), ("1,4", ",POP"), ("1,2,4", ",POP,+")] {
        let count = prefix.split(',').count();
        for shift in ["-2", "-1", "-0.9", "0", "0.9", "1", "1.9", "2"] {
            let expression = format!("CDEF:r=v,POP,{prefix},{count},{shift},ROLL{suffix}");
            let args = [
                "xport",
                "--json",
                "--start",
                "1000000010",
                "--end",
                "1000000020",
            ];
            let run = |program: &std::path::Path| {
                Command::new(program)
                    .args(args)
                    .arg(format!("DEF:v={}:v:AVERAGE", file.display()))
                    .arg(&expression)
                    .arg("XPORT:r:R")
                    .output()
                    .unwrap()
            };
            let expected = run(std::path::Path::new("rrdtool"));
            let actual = run(&alias);
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "count {count}, shift {shift}"
            );
            assert_eq!(
                actual.stdout, expected.stdout,
                "count {count}, shift {shift} stdout"
            );
            assert_eq!(
                actual.stderr, expected.stderr,
                "count {count}, shift {shift} stderr"
            );
        }
    }
}

#[test]
fn rrdcached_help_matches_pinned_stdout_and_exit_status() {
    if !Command::new("rrdcached")
        .arg("--help")
        .output()
        .is_ok_and(|output| output.status.code() == Some(1))
    {
        oracle_skip!("skipping rrdcached help differential: pinned rrdcached is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdcached");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for option in ["-h", "--help"] {
        let upstream = Command::new("rrdcached").arg(option).output().unwrap();
        let rondi = Command::new(&alias).arg(option).output().unwrap();
        assert_eq!(upstream.status.code(), Some(1));
        assert_eq!(rondi.status.code(), upstream.status.code());
        assert_eq!(rondi.stdout, upstream.stdout);
        assert_eq!(rondi.stderr, upstream.stderr);
    }
}

#[test]
fn rrdcached_option_parsing_matches_pinned_daemon() {
    if !Command::new("rrdcached")
        .arg("--help")
        .output()
        .is_ok_and(|output| output.status.code() == Some(1))
    {
        oracle_skip!("skipping rrdcached option differential: pinned rrdcached is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdcached");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    // Every case stops during option parsing, so neither daemon starts.
    for args in [
        &["--version"][..],
        &["--help=x"],
        &["--listen", "/x"],
        &["-q"],
        &["-xh"],
        &["-hq"],
        &["-gF", "-h"],
        &["foo", "-h"],
        &["-V"],
        &["-V", "LOG_FOO", "-h"],
        &["-w", "abc"],
        &["-w", "0"],
        &["-w", "5x"],
        &["-f", "abc"],
        &["-z", "abc"],
        &["-w1800", "-z100", "-f3600", "-h"],
        &["-z", "5000", "-h"],
        &["-h", "-f", "10", "-w", "20"],
        &["-U", "rondi-no-such-user"],
        &["-G", "rondi-no-such-group"],
        &["-t", ""],
        &["-a", ""],
        &["-B", "-h"],
        &["-R", "-h"],
        &["-P", "FOO,PING", "-h"],
        &["-P", "FOO"],
    ] {
        let upstream = Command::new("rrdcached").args(args).output().unwrap();
        let rondi = Command::new(&alias).args(args).output().unwrap();
        assert_eq!(rondi.status.code(), upstream.status.code(), "{args:?}");
        assert_eq!(rondi.stdout, upstream.stdout, "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            String::from_utf8_lossy(&upstream.stderr),
            "{args:?}"
        );
    }
}

#[test]
fn rrdproxy_alias_preserves_pinned_version_and_help_invocations() {
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool-proxy.php");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    let version = Command::new(&alias).arg("--version").output().unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    assert!(version.starts_with("RRDtool Proxy Server v1.2.17, Copyright (C) 2004-"));
    assert!(version.ends_with(" The Cacti Group\r\n"));

    let help = Command::new(&alias).arg("--help").output().unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.starts_with(&version));
    assert!(help.contains("Usage: rrdtool-proxy.php [-w|--wizard] [-v|--version]"));
    assert!(help.contains("-s --systemd   - Adjust output messages for systemd\r\n"));

    let invalid = Command::new(&alias).arg("--invalid").output().unwrap();
    assert!(invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stdout)
            .starts_with("ERROR: Invalid Parameter --invalid\r\n")
    );
}

#[test]
fn php_rrdproxy_launcher_forwards_arguments_and_output() {
    let php = Command::new("php").arg("-v").output();
    if !php.is_ok_and(|output| output.status.success()) {
        oracle_skip!("skipping PHP launcher test: PHP CLI is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool-proxy");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let script = format!(
        "{}/compat/rrdtool-proxy.php",
        env!("CARGO_MANIFEST_DIR").trim_end_matches("/crates/rondi-cli")
    );

    for argument in ["--version", "--help", "--invalid"] {
        let direct = Command::new(&alias).arg(argument).output().unwrap();
        let through_php = Command::new("php")
            .arg(&script)
            .arg(argument)
            .env("RONDI_BIN", &alias)
            .output()
            .unwrap();
        assert_eq!(through_php.status, direct.status);
        assert_eq!(through_php.stdout, direct.stdout);
        assert_eq!(through_php.stderr, direct.stderr);
    }
}

#[test]
fn rrdtool_usage_and_version_invocations_match_pinned_tool() {
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    // Kadupul detects the installed version with /^RRDtool ([0-9.]+) / on
    // the output of `rrdtool -v`.
    let version = Command::new(&alias).arg("-v").output().unwrap();
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("RRDtool 1.11.0 "));

    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping usage differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    for args in [
        &[][..],
        &["-v"],
        &["--version"],
        &["-h"],
        &["bogus"],
        &["fetch"],
        &["ls"],
        &["help", "fetch"],
        &["help", "bogus"],
        &["help", "a", "b"],
        &["-v", "x"],
        &["version", "x"],
        &["bogus", "a", "b"],
    ] {
        let upstream = Command::new("rrdtool").args(args).output().unwrap();
        let rondi = Command::new(&alias).args(args).output().unwrap();
        assert_eq!(rondi.status.code(), upstream.status.code(), "{args:?}");
        assert_eq!(
            normalize_rrdtool_compiled_stamp(&rondi.stdout),
            normalize_rrdtool_compiled_stamp(&upstream.stdout),
            "{args:?}"
        );
        assert_eq!(rondi.stderr, upstream.stderr, "{args:?}");
    }
}

#[test]
fn rrdtool_fetch_alias_matches_pinned_tool_output() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        oracle_skip!("skipping RRDtool CLI differential: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let updated = Command::new("rrdtool")
        .args([
            "update",
            file.to_str().unwrap(),
            "1000000010:1",
            "1000000020:2",
            "1000000030:3",
        ])
        .output()
        .unwrap();
    assert!(updated.status.success());

    let oracle = Command::new("rrdtool")
        .args([
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let replacement = Command::new(alias)
        .args([
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert!(oracle.status.success());
    assert!(
        replacement.status.success(),
        "{}",
        String::from_utf8_lossy(&replacement.stderr)
    );
    assert_eq!(replacement.stdout, oracle.stdout);
}

#[test]
fn rrdtool_fetch_start_and_end_references_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool range-reference differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("range.rrd");
    assert!(
        Command::new("rrdtool")
            .args([
                "create",
                file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:a:GAUGE:20:U:U",
                "RRA:AVERAGE:0.5:1:8",
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("rrdtool")
            .args([
                "update",
                file.to_str().unwrap(),
                "1000000010:1",
                "1000000020:2",
                "1000000030:3",
                "1000000040:4",
                "1000000050:5",
            ])
            .status()
            .unwrap()
            .success()
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (start, end) in [("end-30s", "1000000050"), ("1000000010", "start+30s")] {
        let args = [
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            start,
            "--end",
            end,
            "--resolution",
            "10",
        ];
        let expected = Command::new("rrdtool").args(args).output().unwrap();
        let actual = Command::new(&alias).args(args).output().unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout, "range {start}..{end}");
    }
    let definition = format!("DEF:a={}:a:AVERAGE", file.display());
    for (command, format_args) in [
        ("xport", vec!["--step", "10", "--json", "XPORT:a:Series"]),
        ("graphv", vec!["--imgformat=JSON", "XPORT:a:Series"]),
    ] {
        let mut args = if command == "graphv" {
            vec![command, "-", "--start", "end-40s", "--end", "1000000050"]
        } else {
            vec![command, "--start", "end-40s", "--end", "1000000050"]
        };
        if command == "xport" {
            args.extend(format_args[..3].iter().copied());
        } else {
            args.extend(format_args[..1].iter().copied());
        }
        args.push(&definition);
        args.extend(format_args.last().copied());
        let expected = Command::new("rrdtool").args(&args).output().unwrap();
        let actual = Command::new(&alias).args(&args).output().unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout, "{command} relative range");
    }
}

#[test]
fn rrdtool_fetch_negative_times_are_relative_to_now_like_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool relative-fetch differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("relative.rrd");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let start = now - 1_000;
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            &start.to_string(),
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:120",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let updates = (start + 10..=now - 100)
        .step_by(10)
        .map(|timestamp| format!("{timestamp}:1"))
        .collect::<Vec<_>>();
    let updated = Command::new("rrdtool")
        .arg("update")
        .arg(&file)
        .args(&updates)
        .output()
        .unwrap();
    assert!(updated.status.success());
    let arguments = [
        "fetch",
        file.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "-800",
        "--end",
        "-400",
        "--resolution",
        "10s",
    ];
    let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let rondi = Command::new(alias).args(arguments).output().unwrap();
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    let normalize = |stdout: &[u8]| {
        String::from_utf8_lossy(stdout)
            .lines()
            .map(|line| {
                line.split_once(':')
                    .filter(|(timestamp, _)| timestamp.trim().parse::<i64>().is_ok())
                    .map_or_else(|| line.to_owned(), |(_, values)| values.to_owned())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(normalize(&rondi.stdout), normalize(&upstream.stdout));
}

#[test]
fn rrdtool_batch_mode_runs_poller_commands_and_update_templates() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool batch differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:b:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    std::fs::copy(&ours, &oracle).unwrap();
    let commands = format!(
        "update {} --template b:a 1000000010:20:10 1000000020:40:30\nlast {}\nquit\n",
        ours.display(),
        ours.display()
    );
    let mut rondi = Command::new(&alias)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    rondi
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let rondi = rondi.wait_with_output().unwrap();
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );

    let commands = commands.replace(ours.to_str().unwrap(), oracle.to_str().unwrap());
    let mut upstream = Command::new("rrdtool")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    upstream
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let upstream = upstream.wait_with_output().unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    let normalize = |output: &[u8]| {
        String::from_utf8_lossy(output)
            .lines()
            .filter(|line| !line.starts_with("OK u:"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(normalize(&rondi.stdout), normalize(&upstream.stdout));
    let ours_bytes = std::fs::read(&ours).unwrap();
    let oracle_bytes = std::fs::read(&oracle).unwrap();
    let differences = ours_bytes
        .iter()
        .zip(&oracle_bytes)
        .enumerate()
        .filter_map(|(offset, (ours, oracle))| (ours != oracle).then_some((offset, *ours, *oracle)))
        .take(20)
        .collect::<Vec<_>>();
    assert!(differences.is_empty(), "byte diffs: {differences:?}");
}

#[test]
fn rrdtool_batch_mode_usage_and_errors_match_pinned_tool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping RRDtool batch usage differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let commands = "bogus a\n\nfetch\n   \nhelp x\n-v x\nfetch \"a\nquit x\n   ";
    let run = |program: &std::path::Path| {
        let mut child = Command::new(program)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(commands.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let upstream = run(std::path::Path::new("rrdtool"));
    let rondi = run(&alias);
    let normalize = |output: &[u8]| {
        String::from_utf8(normalize_rrdtool_compiled_stamp(output))
            .unwrap()
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("OK u:") {
                    "OK\n"
                } else {
                    line
                }
            })
            .collect::<String>()
    };
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(normalize(&rondi.stdout), normalize(&upstream.stdout));
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn aligned_multi_step_update_matches_upstream_bytes_for_multi_pdp_archive() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping aligned multi-PDP differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:60:U:U",
            "RRA:AVERAGE:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (program, path) in [
        (alias.as_os_str(), ours.as_path()),
        (std::ffi::OsStr::new("rrdtool"), oracle.as_path()),
    ] {
        let output = Command::new(program)
            .args(["update", path.to_str().unwrap(), "1000000020:10"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let ours_bytes = std::fs::read(&ours).unwrap();
    let oracle_bytes = std::fs::read(&oracle).unwrap();
    assert_eq!(
        ours_bytes, oracle_bytes,
        "aligned two-step GAUGE archive bytes differ"
    );
}

#[test]
fn update_daemon_equals_down_socket_matches_upstream_failure() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping unavailable-daemon differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:60:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let unavailable = format!("unix:{}", temp.path().join("missing.sock").display());
    let ours_result = Command::new(&alias)
        .args([
            "update",
            ours.to_str().unwrap(),
            &format!("--daemon={unavailable}"),
            "1000000010:7",
        ])
        .output()
        .unwrap();
    let upstream_result = Command::new("rrdtool")
        .args([
            "update",
            oracle.to_str().unwrap(),
            &format!("--daemon={unavailable}"),
            "1000000010:7",
        ])
        .output()
        .unwrap();
    assert_eq!(
        ours_result.status,
        upstream_result.status,
        "Rondi stderr: {}; RRDtool stderr: {}",
        String::from_utf8_lossy(&ours_result.stderr),
        String::from_utf8_lossy(&upstream_result.stderr)
    );
    assert_eq!(ours_result.stdout, upstream_result.stdout);
    assert_eq!(ours_result.stderr, upstream_result.stderr);
    assert!(!ours_result.status.success());
    assert_eq!(std::fs::read(ours).unwrap(), std::fs::read(oracle).unwrap());
}

#[test]
fn read_commands_with_down_daemon_report_upstream_connect_error() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping unavailable-daemon read differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("read.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:60:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let unavailable = format!("unix:{}", temp.path().join("missing.sock").display());
    let file = file.to_str().unwrap();
    for args in [
        &["fetch", file, "AVERAGE", "--daemon", &unavailable][..],
        &["last", file, "--daemon", &unavailable],
        &["first", file, "--daemon", &unavailable],
        &["info", file, "--daemon", &unavailable],
        &["dump", file, "--daemon", &unavailable],
        &["flushcached", file, "--daemon", &unavailable],
    ] {
        let ours = Command::new(&alias).args(args).output().unwrap();
        let upstream = Command::new("rrdtool").args(args).output().unwrap();
        // RRDtool also prints a local read on stdout after the connect
        // failure for fetch, last, first, info, and dump; Rondi does not fall
        // back, so only the diagnostic and status are compared.
        assert_eq!(ours.status.code(), upstream.status.code(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&ours.stderr),
            String::from_utf8_lossy(&upstream.stderr),
            "{args:?}"
        );
    }
}

#[test]
fn local_updates_to_different_files_in_one_directory_can_run_concurrently() {
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let files = [temp.path().join("a.rrd"), temp.path().join("b.rrd")];
    for file in &files {
        let created = Command::new(&alias)
            .args([
                "create",
                file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "1",
                "DS:value:GAUGE:200:U:U",
                "RRA:AVERAGE:0.5:1:200",
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    let mut children = files
        .iter()
        .enumerate()
        .map(|(file_index, file)| {
            let samples = (1..=100)
                .map(|step| format!("{}:{}", 1_000_000_000 + step, file_index + step))
                .collect::<Vec<_>>();
            Command::new(&alias)
                .arg("update")
                .arg(file)
                .args(samples)
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    for file in &files {
        let last = Command::new(&alias)
            .args(["last", file.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(last.status.success());
        assert_eq!(String::from_utf8_lossy(&last.stdout).trim(), "1000000100");
    }
}

#[test]
fn held_rrd_lock_fails_fast_and_honors_rrd_locking_like_rrdtool() {
    use std::os::fd::AsRawFd;
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping lock differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("held.rrd");
    let created = Command::new("rrdtool")
        .args(["create", file.to_str().unwrap(), "--start", "1000000000"])
        .args(["--step", "10", "DS:v:GAUGE:30:U:U", "RRA:AVERAGE:0.5:1:8"])
        .output()
        .unwrap();
    assert!(created.status.success());
    // Each tool gets its own copy so RRD_LOCKING=none updates do not collide.
    let ours_file = temp.path().join("ours.rrd");
    std::fs::copy(&file, &ours_file).unwrap();
    let holders = [&file, &ours_file].map(|path| {
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        // SAFETY: zeroed flock is a valid whole-file request once l_type is set.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        // SAFETY: the descriptor is open and `lock` is a valid flock structure.
        assert_eq!(
            unsafe { libc::fcntl(holder.as_raw_fd(), libc::F_SETLK, &lock) },
            0
        );
        holder
    });
    let commands: [&[&str]; 4] = [
        &["update", "FILE", "1000000010:1"],
        &[
            "fetch",
            "FILE",
            "AVERAGE",
            "-s",
            "1000000000",
            "-e",
            "1000000030",
        ],
        &["info", "FILE"],
        &["dump", "FILE"],
    ];
    for locking in [None, Some("try"), Some("bogus"), Some("none")] {
        for arguments in commands {
            let run = |program: &std::path::Path, path: &std::path::Path| {
                let mut command = Command::new(program);
                for argument in arguments {
                    if *argument == "FILE" {
                        command.arg(path);
                    } else {
                        command.arg(argument);
                    }
                }
                command.env("TZ", "UTC");
                match locking {
                    Some(mode) => command.env("RRD_LOCKING", mode),
                    None => command.env_remove("RRD_LOCKING"),
                };
                let mut child = command
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while child.try_wait().unwrap().is_none() {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        panic!("{program:?} {arguments:?} blocked on a held RRD lock");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                child.wait_with_output().unwrap()
            };
            let ours = run(&alias, &ours_file);
            let theirs = run(std::path::Path::new("rrdtool"), &file);
            assert_eq!(
                ours.status.code(),
                theirs.status.code(),
                "{locking:?} {arguments:?}"
            );
            assert_eq!(
                String::from_utf8_lossy(&ours.stderr),
                String::from_utf8_lossy(&theirs.stderr),
                "{locking:?} {arguments:?}"
            );
        }
    }
    drop(holders);
}

#[test]
fn update_through_rrd_symlink_follows_the_target_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool symlink differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours-target.rrd");
    let oracle = temp.path().join("oracle-target.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:60:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    std::fs::copy(&ours, &oracle).unwrap();
    let ours_link = temp.path().join("ours-link.rrd");
    let oracle_link = temp.path().join("oracle-link.rrd");
    symlink(&ours, &ours_link).unwrap();
    symlink(&oracle, &oracle_link).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let actual = Command::new(&alias)
        .args(["update", ours_link.to_str().unwrap(), "1000000010:3"])
        .output()
        .unwrap();
    let expected = Command::new("rrdtool")
        .args(["update", oracle_link.to_str().unwrap(), "1000000010:3"])
        .output()
        .unwrap();
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    assert_eq!(std::fs::read(ours).unwrap(), std::fs::read(oracle).unwrap());
}

#[test]
fn negative_tuned_heartbeat_matches_pinned_rrdtool_file_and_diagnostic() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping negative heartbeat differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:60:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    std::fs::copy(&ours, &oracle).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let actual = Command::new(&alias)
        .args(["tune", ours.to_str().unwrap(), "--heartbeat", "value:-1"])
        .output()
        .unwrap();
    let expected = Command::new("rrdtool")
        .args(["tune", oracle.to_str().unwrap(), "--heartbeat", "value:-1"])
        .output()
        .unwrap();
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    assert_eq!(std::fs::read(ours).unwrap(), std::fs::read(oracle).unwrap());
}

#[test]
fn rrdtool_update_skip_past_updates_matches_pinned_tool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    std::fs::copy(&ours, &oracle).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let ours_update = Command::new(alias)
        .args([
            "update",
            ours.to_str().unwrap(),
            "--skip-past-updates",
            "1000000010:1",
            "1000000020:2",
            "1000000015:99",
            "1000000030:3",
        ])
        .output()
        .unwrap();
    assert!(
        ours_update.status.success(),
        "{}",
        String::from_utf8_lossy(&ours_update.stderr)
    );
    let oracle_update = Command::new("rrdtool")
        .args([
            "update",
            oracle.to_str().unwrap(),
            "--skip-past-updates",
            "1000000010:1",
            "1000000020:2",
            "1000000015:99",
            "1000000030:3",
        ])
        .output()
        .unwrap();
    assert!(
        oracle_update.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle_update.stderr)
    );
    let ours_bytes = std::fs::read(&ours).unwrap();
    let oracle_bytes = std::fs::read(&oracle).unwrap();
    let differences = ours_bytes
        .iter()
        .zip(&oracle_bytes)
        .enumerate()
        .filter_map(|(offset, (ours, oracle))| (ours != oracle).then_some((offset, *ours, *oracle)))
        .take(20)
        .collect::<Vec<_>>();
    assert!(differences.is_empty(), "byte diffs: {differences:?}");
}

#[test]
fn rrdtool_update_out_of_order_diagnostic_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping out-of-order update differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:v:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    // RRDtool create randomizes the archive start row, so both runs share one file image.
    std::fs::copy(&ours, &oracle).unwrap();
    for samples in [
        &["1000000010:1"][..],
        &["1000000010:2"],
        &["1000000005.5:2"],
        &["1000000020:3", "1000000030:4", "1000000025:5"],
    ] {
        let upstream = Command::new("rrdtool")
            .arg("update")
            .arg(&oracle)
            .args(samples)
            .output()
            .unwrap();
        let rondi = Command::new(&alias)
            .arg("update")
            .arg(&ours)
            .args(samples)
            .output()
            .unwrap();
        let expected_stderr = String::from_utf8_lossy(&upstream.stderr)
            .replace(oracle.to_str().unwrap(), ours.to_str().unwrap());
        assert_eq!(rondi.status.code(), upstream.status.code(), "{samples:?}");
        assert_eq!(rondi.stdout, upstream.stdout, "{samples:?}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            expected_stderr,
            "{samples:?}"
        );
    }
    assert_eq!(
        std::fs::read(&ours).unwrap(),
        std::fs::read(&oracle).unwrap()
    );
}

#[test]
fn rrdtool_update_n_and_negative_timestamps_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update-time differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let baseline = temp.path().join("baseline.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            baseline.to_str().unwrap(),
            "--start",
            &(now - 600).to_string(),
            "--step",
            "10",
            "DS:a:GAUGE:120:U:U",
            "RRA:AVERAGE:0.5:1:120",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (case, timestamp) in [("now", "N"), ("relative", "-60")] {
        let upstream_file = temp.path().join(format!("{case}-upstream.rrd"));
        let rondi_file = temp.path().join(format!("{case}-rondi.rrd"));
        std::fs::copy(&baseline, &upstream_file).unwrap();
        std::fs::copy(&baseline, &rondi_file).unwrap();
        let mut upstream_command = Command::new("rrdtool");
        let mut rondi_command = Command::new(&alias);
        upstream_command.arg("update").arg(&upstream_file);
        rondi_command.arg("update").arg(&rondi_file);
        if timestamp.starts_with('-') {
            upstream_command.arg("--");
            rondi_command.arg("--");
        }
        let upstream = upstream_command
            .arg(format!("{timestamp}:1"))
            .output()
            .unwrap();
        let rondi = rondi_command
            .arg(format!("{timestamp}:1"))
            .output()
            .unwrap();
        assert!(
            upstream.status.success(),
            "{}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        assert!(
            rondi.status.success(),
            "{}",
            String::from_utf8_lossy(&rondi.stderr)
        );
        let last_update = |path: &std::path::Path| {
            let output = Command::new("rrdtool")
                .args(["last", path.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<i64>()
                .unwrap()
        };
        assert!((last_update(&upstream_file) - last_update(&rondi_file)).abs() <= 1);
    }
}

#[test]
fn rrdtool_update_fractional_timestamps_match_upstream_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping fractional update timestamp differential: rrdtool is not installed");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let template = temp.path().join("template.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            template.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    std::fs::copy(&template, &upstream_file).unwrap();
    std::fs::copy(&template, &rondi_file).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let updates = [
        "1000000010.25:1",
        "1000000010.75:3",
        "1000000020.5:5",
        "1000000030:6",
    ];
    let upstream = Command::new("rrdtool")
        .arg("update")
        .arg(&upstream_file)
        .args(updates)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("update")
        .arg(&rondi_file)
        .args(updates)
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );
}

#[test]
fn rrdtool_update_uses_rrd_strtod_rounding_for_epoch_fractions() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping rrd_strtod timestamp differential: rrdtool is not installed");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let template = temp.path().join("template.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            template.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    std::fs::copy(&template, &upstream_file).unwrap();
    std::fs::copy(&template, &rondi_file).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let updates = [
        "1000000010.0000001:1",
        "1000000010.0000019:2",
        "1000000010.9999999:3",
        "1000000020.1234567:4",
    ];
    let upstream = Command::new("rrdtool")
        .arg("update")
        .arg(&upstream_file)
        .args(updates)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("update")
        .arg(&rondi_file)
        .args(updates)
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );
}

#[test]
fn rrdtool_update_special_values_and_exponent_range_follow_rrd_strtod() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping rrd_strtod special-value differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let template = temp.path().join("template.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            template.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream_dir = temp.path().join("upstream");
    let rondi_dir = temp.path().join("rondi");
    std::fs::create_dir(&upstream_dir).unwrap();
    std::fs::create_dir(&rondi_dir).unwrap();

    for value in [
        "nan", "-nan", "NaN", "inf", "-inf", "Infinity", "1e-1100", "1e400", "+inf",
    ] {
        // The template keeps RRDtool's randomized archive pointer identical in
        // both copies; relative paths keep diagnostics comparable.
        std::fs::copy(&template, upstream_dir.join("t.rrd")).unwrap();
        std::fs::copy(&template, rondi_dir.join("t.rrd")).unwrap();
        let update = |program: &std::path::Path, dir: &std::path::Path| {
            Command::new(program)
                .current_dir(dir)
                .args([
                    "update",
                    "t.rrd",
                    &format!("1000000010:{value}"),
                    "1000000020:1",
                ])
                .output()
                .unwrap()
        };
        let upstream = update(std::path::Path::new("rrdtool"), &upstream_dir);
        let rondi = update(&alias, &rondi_dir);
        assert_eq!(rondi.status.code(), upstream.status.code(), "{value}");
        assert_eq!(rondi.stdout, upstream.stdout, "{value}");
        assert_eq!(
            String::from_utf8_lossy(&rondi.stderr),
            String::from_utf8_lossy(&upstream.stderr),
            "{value}"
        );
        assert_eq!(
            std::fs::read(rondi_dir.join("t.rrd")).unwrap(),
            std::fs::read(upstream_dir.join("t.rrd")).unwrap(),
            "{value}"
        );
    }
}

#[test]
fn rrdtool_update_at_style_calendar_timestamp_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool at-style update differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let baseline = temp.path().join("baseline.rrd");
    let created = Command::new("rrdtool")
        .env("TZ", "UTC")
        .args([
            "create",
            baseline.to_str().unwrap(),
            "--start",
            "1500000000",
            "--step",
            "10",
            "DS:a:GAUGE:3600:U:U",
            "RRA:AVERAGE:0.5:1:100",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    std::fs::copy(&baseline, &upstream_file).unwrap();
    std::fs::copy(&baseline, &rondi_file).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let sample = "March 10 2020 12:30@1";
    let upstream = Command::new("rrdtool")
        .env("TZ", "UTC")
        .args(["update", upstream_file.to_str().unwrap(), sample])
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .env("TZ", "UTC")
        .args(["update", rondi_file.to_str().unwrap(), sample])
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    let last_update = |path: &std::path::Path| {
        let output = Command::new("rrdtool")
            .args(["last", path.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<i64>()
            .unwrap()
    };
    assert_eq!(last_update(&upstream_file), last_update(&rondi_file));
}

#[test]
fn rrdtool_fetch_unknown_consolidation_function_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool fetch CF diagnostic differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    for cf in ["BADCF", "average"] {
        let arguments = [
            "fetch",
            file.to_str().unwrap(),
            cf,
            "--start",
            "1000000000",
            "--end",
            "1000000010",
            "--resolution",
            "10",
        ];
        let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
        let rondi = Command::new(&alias).args(arguments).output().unwrap();
        assert_eq!(rondi.status, upstream.status, "{cf}");
        assert_eq!(rondi.stdout, upstream.stdout, "{cf}");
        assert_eq!(rondi.stderr, upstream.stderr, "{cf}");
    }
}

#[test]
fn rrdtool_fetch_zero_resolution_diagnostic_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool fetch resolution differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    for resolution in ["0", "0s", "0m", "0h", "0d", "0w", "0M", "0y"] {
        let arguments = [
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            "--resolution",
            resolution,
        ];
        let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
        let rondi = Command::new(&alias).args(arguments).output().unwrap();
        assert_eq!(rondi.status, upstream.status, "{resolution}");
        assert_eq!(rondi.stdout, upstream.stdout, "{resolution}");
        assert_eq!(rondi.stderr, upstream.stderr, "{resolution}");
    }
    for resolution in ["foo", "1foo", "-1", "1.5"] {
        let arguments = [
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            "--resolution",
            resolution,
        ];
        let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
        let rondi = Command::new(&alias).args(arguments).output().unwrap();
        assert_eq!(rondi.status, upstream.status, "{resolution}");
        assert_eq!(rondi.stdout, upstream.stdout, "{resolution}");
        assert_eq!(rondi.stderr, upstream.stderr, "{resolution}");
    }
}

#[test]
fn rrdtool_create_alias_makes_files_upstream_can_update_and_fetch() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("created-by-rondi.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let created = Command::new(&alias)
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000003",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:b:COUNTER:30:0:100000",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.5:2:4",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let info = Command::new(&alias)
        .args(["info", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        info.status.success(),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );
    let oracle_file = temp.path().join("created-by-rrdtool.rrd");
    let oracle_created = Command::new("rrdtool")
        .args([
            "create",
            oracle_file.to_str().unwrap(),
            "--start",
            "1000000003",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:b:COUNTER:30:0:100000",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.5:2:4",
        ])
        .output()
        .unwrap();
    assert!(oracle_created.status.success());
    let rondi_dump = Command::new("rrdtool")
        .args(["dump", file.to_str().unwrap()])
        .output()
        .unwrap();
    let upstream_dump = Command::new("rrdtool")
        .args(["dump", oracle_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(rondi_dump.status.success());
    assert!(upstream_dump.status.success());
    assert_eq!(rondi_dump.stdout, upstream_dump.stdout);
    let update = Command::new("rrdtool")
        .args([
            "update",
            file.to_str().unwrap(),
            "1000000010:2:100",
            "1000000020:4:150",
            "1000000030:6:220",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    for (cf, start, end, resolution, expected_value) in [
        (
            "AVERAGE",
            "1000000000",
            "1000000050",
            "10",
            "2.0000000000e+00",
        ),
        ("MAX", "1000000000", "1000000050", "20", "4.0000000000e+00"),
    ] {
        let fetched = Command::new("rrdtool")
            .args([
                "fetch",
                file.to_str().unwrap(),
                cf,
                "--start",
                start,
                "--end",
                end,
                "--resolution",
                resolution,
            ])
            .output()
            .unwrap();
        assert!(
            fetched.status.success(),
            "{}",
            String::from_utf8_lossy(&fetched.stderr)
        );
        assert!(
            String::from_utf8_lossy(&fetched.stdout).contains(expected_value),
            "cf={cf}: {}",
            String::from_utf8_lossy(&fetched.stdout)
        );
    }
}

#[test]
fn rrdtool_create_accepts_pinned_now_relative_start_forms() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create-time differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let create_args = [
        "--start",
        "now - 1 hour",
        "--step",
        "10",
        "DS:a:GAUGE:20:U:U",
        "RRA:AVERAGE:0.5:1:8",
    ];
    let upstream = Command::new("rrdtool")
        .arg("create")
        .arg(&upstream_file)
        .args(create_args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let rondi = Command::new(alias)
        .arg("create")
        .arg(&rondi_file)
        .args(create_args)
        .output()
        .unwrap();
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );

    let upstream_last = Command::new("rrdtool")
        .args(["last", upstream_file.to_str().unwrap()])
        .output()
        .unwrap();
    let rondi_last = Command::new("rrdtool")
        .args(["last", rondi_file.to_str().unwrap()])
        .output()
        .unwrap();
    let upstream_last = String::from_utf8(upstream_last.stdout)
        .unwrap()
        .trim()
        .parse::<i64>()
        .unwrap();
    let rondi_last = String::from_utf8(rondi_last.stdout)
        .unwrap()
        .trim()
        .parse::<i64>()
        .unwrap();
    assert!(
        (upstream_last - rondi_last).abs() <= 10,
        "independent `now - 1 hour` evaluations should differ by at most one step; upstream={upstream_last}, Rondi={rondi_last}"
    );
}

#[test]
fn rrdtool_create_common_absolute_date_forms_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool absolute-date differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (case, start) in [
        ("month_full", "March 10 2020 12:30"),
        ("time_first", "12:30 Mar 10 2020"),
        ("month_abbrev", "Mar 10 2020 12:30"),
    ] {
        let upstream_file = temp.path().join(format!("{case}-upstream.rrd"));
        let rondi_file = temp.path().join(format!("{case}-rondi.rrd"));
        let run_create = |executable: &std::ffi::OsStr, file: &std::path::Path| {
            Command::new(executable)
                .env("TZ", "UTC")
                .args(["create"])
                .arg(file)
                .args([
                    "--start",
                    start,
                    "--step",
                    "10",
                    "DS:a:GAUGE:20:U:U",
                    "RRA:AVERAGE:0.5:1:8",
                ])
                .output()
                .unwrap()
        };
        let upstream = run_create(std::ffi::OsStr::new("rrdtool"), &upstream_file);
        let rondi = run_create(alias.as_os_str(), &rondi_file);
        assert!(
            upstream.status.success(),
            "{}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        assert!(
            rondi.status.success(),
            "{}",
            String::from_utf8_lossy(&rondi.stderr)
        );
        let get_last = |file: &std::path::Path| {
            let output = Command::new("rrdtool")
                .args(["last", file.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<i64>()
                .unwrap()
        };
        assert_eq!(get_last(&upstream_file), get_last(&rondi_file));
    }
}

#[test]
fn rrdtool_create_date_only_and_special_time_forms_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool date-only differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (case, start) in [
        ("month_date", "March 10 2020"),
        ("us_date", "03/10/2020"),
        ("eu_date", "10.03.2020"),
        ("compact_date", "20200310"),
        ("noon_date", "noon March 10 2020"),
        ("weekday", "noon Monday"),
        ("relative_day", "noon yesterday"),
    ] {
        let upstream_file = temp.path().join(format!("{case}-upstream.rrd"));
        let rondi_file = temp.path().join(format!("{case}-rondi.rrd"));
        let run_create = |executable: &std::ffi::OsStr, file: &std::path::Path| {
            Command::new(executable)
                .env("TZ", "UTC")
                .args(["create"])
                .arg(file)
                .args([
                    "--start",
                    start,
                    "--step",
                    "10",
                    "DS:a:GAUGE:20:U:U",
                    "RRA:AVERAGE:0.5:1:8",
                ])
                .output()
                .unwrap()
        };
        let upstream = run_create(std::ffi::OsStr::new("rrdtool"), &upstream_file);
        let rondi = run_create(alias.as_os_str(), &rondi_file);
        assert!(
            upstream.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        assert!(
            rondi.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&rondi.stderr)
        );
        let last = |file: &std::path::Path| {
            let output = Command::new("rrdtool")
                .args(["last", file.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<i64>()
                .unwrap()
        };
        assert!(
            (last(&upstream_file) - last(&rondi_file)).abs() <= 1,
            "case={case}"
        );
    }
}

#[test]
fn rrdtool_create_rejects_standalone_day_tokens_like_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping standalone date rejection differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for token in ["today", "yesterday", "tomorrow", "Monday"] {
        let run_create = |executable: &std::ffi::OsStr| {
            Command::new(executable)
                .env("TZ", "UTC")
                .args(["create"])
                .arg(temp.path().join(format!("{token}.rrd")))
                .args([
                    "--start",
                    token,
                    "--step",
                    "10",
                    "DS:a:GAUGE:20:U:U",
                    "RRA:AVERAGE:0.5:1:8",
                ])
                .output()
                .unwrap()
        };
        let upstream = run_create(std::ffi::OsStr::new("rrdtool"));
        let rondi = run_create(alias.as_os_str());
        assert!(
            !upstream.status.success(),
            "RRDtool unexpectedly accepted {token}"
        );
        assert!(
            !rondi.status.success(),
            "Rondi unexpectedly accepted {token}"
        );
        assert_eq!(
            upstream.stderr, rondi.stderr,
            "diagnostic changed for {token}"
        );
    }
}

#[test]
fn rrdtool_calendar_offsets_and_dst_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool calendar-offset differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (case, start, timezone) in [
        ("dst_day", "March 10 2024 12:00 -1day", "America/New_York"),
        ("ambiguous_m", "March 10 2020 12:30 -1m", "UTC"),
        ("compound", "March 10 2020 12:30 -1day+2h", "UTC"),
    ] {
        let upstream_file = temp.path().join(format!("{case}-upstream.rrd"));
        let rondi_file = temp.path().join(format!("{case}-rondi.rrd"));
        let run_create = |executable: &std::ffi::OsStr, file: &std::path::Path| {
            Command::new(executable)
                .env("TZ", timezone)
                .args(["create"])
                .arg(file)
                .args([
                    "--start",
                    start,
                    "--step",
                    "10",
                    "DS:a:GAUGE:20:U:U",
                    "RRA:AVERAGE:0.5:1:8",
                ])
                .output()
                .unwrap()
        };
        let upstream = run_create(std::ffi::OsStr::new("rrdtool"), &upstream_file);
        let rondi = run_create(alias.as_os_str(), &rondi_file);
        assert!(
            upstream.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        assert!(
            rondi.status.success(),
            "case={case}: {}",
            String::from_utf8_lossy(&rondi.stderr)
        );
        let last = |file: &std::path::Path| {
            let output = Command::new("rrdtool")
                .args(["last", file.to_str().unwrap()])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout)
                .unwrap()
                .trim()
                .parse::<i64>()
                .unwrap()
        };
        assert_eq!(last(&upstream_file), last(&rondi_file), "case={case}");
    }
}

#[test]
fn rrdtool_create_duration_suffixes_match_upstream_settings() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create-duration differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let definitions = [
        "--start",
        "1000000000",
        "--step",
        "10s",
        "DS:a:GAUGE:1m:U:U",
        "RRA:AVERAGE:0.5:30s:1h",
    ];
    for (executable, file) in [
        ("rrdtool", upstream_file.as_path()),
        (env!("CARGO_BIN_EXE_rondi"), rondi_file.as_path()),
    ] {
        let mut command = Command::new(executable);
        if executable.ends_with("rondi") {
            let alias = temp.path().join("rrdtool");
            if !alias.exists() {
                symlink(executable, &alias).unwrap();
            }
            command = Command::new(alias);
        }
        let output = command
            .arg("create")
            .arg(file)
            .args(definitions)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let upstream_info = Command::new("rrdtool")
        .args(["info", upstream_file.to_str().unwrap()])
        .output()
        .unwrap();
    let rondi_info = Command::new("rrdtool")
        .args(["info", rondi_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(upstream_info.status.success());
    assert!(rondi_info.status.success());
    let selected_settings = |output: &[u8]| {
        String::from_utf8_lossy(output)
            .lines()
            .filter(|line| {
                line.starts_with("step =")
                    || line.starts_with("ds[a].minimal_heartbeat")
                    || line.starts_with("rra[0].rows")
                    || line.starts_with("rra[0].pdp_per_row")
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        selected_settings(&rondi_info.stdout),
        selected_settings(&upstream_info.stdout)
    );
}

#[test]
fn rrdtool_create_rejects_xff_one_like_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool xff differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let definitions = [
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:a:GAUGE:20:U:U",
        "RRA:AVERAGE:1:1:8",
    ];
    let upstream = Command::new("rrdtool")
        .arg("create")
        .arg(&upstream_file)
        .args(definitions)
        .output()
        .unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let rondi = Command::new(alias)
        .arg("create")
        .arg(&rondi_file)
        .args(definitions)
        .output()
        .unwrap();
    assert_eq!(rondi.status.success(), upstream.status.success());
    assert!(!rondi.status.success());
}

#[test]
fn rrdtool_create_replaces_by_default_and_honors_no_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("overwrite.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let create = |step: &str, no_overwrite: bool| {
        let mut command = Command::new(&alias);
        command.args(["create"]);
        if no_overwrite {
            command.arg("--no-overwrite");
        }
        command.args([
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            step,
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ]);
        command.output().unwrap()
    };
    assert!(create("10", false).status.success());
    assert!(create("20", false).status.success());
    let refused = create("30", true);
    assert!(!refused.status.success());
    let info = Command::new(&alias)
        .args(["info", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(info.status.success());
    assert!(String::from_utf8_lossy(&info.stdout).contains("step = 20"));
}

#[test]
fn rrdtool_create_uses_format_v5_for_double_counter_sources() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool v5 create differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let rondi_file = temp.path().join("rondi-v5.rrd");
    let oracle_file = temp.path().join("upstream-v5.rrd");
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let definitions = [
        "--start",
        "1000000000",
        "--step",
        "10",
        "DS:dc:DCOUNTER:30:U:U",
        "DS:dd:DDERIVE:30:U:U",
        "RRA:AVERAGE:0.5:1:8",
    ];
    let mut ours = Command::new(&alias);
    ours.arg("create").arg(&rondi_file).args(definitions);
    let ours = ours.output().unwrap();
    assert!(
        ours.status.success(),
        "{}",
        String::from_utf8_lossy(&ours.stderr)
    );
    let mut upstream = Command::new("rrdtool");
    upstream.arg("create").arg(&oracle_file).args(definitions);
    let upstream = upstream.output().unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    let info = Command::new("rrdtool")
        .args(["info", rondi_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&info.stdout).contains("rrd_version = \"0005\""));
    let dump = |path: &std::path::Path| {
        Command::new("rrdtool")
            .args(["dump", path.to_str().unwrap()])
            .output()
            .unwrap()
            .stdout
    };
    assert_eq!(dump(&rondi_file), dump(&oracle_file));
}

#[test]
fn rrdtool_fetch_alias_matches_multiple_sources_and_archives() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        oracle_skip!("skipping RRDtool CLI differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("multi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:0:U",
            "DS:b:GAUGE:30:U:100",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.5:2:4",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            file.to_str().unwrap(),
            "1000000010:1:8",
            "1000000020:3:9",
            "1000000030:U:10",
            "1000000040:5:7",
            "1000000050:6:6",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (cf, resolution) in [("AVERAGE", "10"), ("MAX", "20")] {
        let args = [
            "fetch",
            file.to_str().unwrap(),
            cf,
            "--start",
            "1000000000",
            "--end",
            "1000000060",
            "--resolution",
            resolution,
        ];
        let expected = Command::new("rrdtool").args(args).output().unwrap();
        let actual = Command::new(&alias).args(args).output().unwrap();
        assert!(expected.status.success());
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout, "fetch mismatch for {cf}");
    }
}

#[test]
fn rrdtool_update_alias_mutates_the_original_rrd_in_place() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        oracle_skip!("skipping RRDtool CLI differential: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let rondi_file = temp.path().join("rondi.rrd");
    let oracle_file = temp.path().join("oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            rondi_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:c:COUNTER:20:U:U",
            "DS:d:DERIVE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    std::fs::copy(&rondi_file, &oracle_file).unwrap();
    for file in [&rondi_file, &oracle_file] {
        let initial = Command::new("rrdtool")
            .args([
                "update",
                file.to_str().unwrap(),
                "1000000010:1:9007199254740993:-9007199254740993",
            ])
            .output()
            .unwrap();
        assert!(initial.status.success());
    }
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let replacement = Command::new(&alias)
        .args([
            "update",
            rondi_file.to_str().unwrap(),
            "1000000020:2:9007199254740994:-9007199254740994",
        ])
        .output()
        .unwrap();
    assert!(
        replacement.status.success(),
        "{}",
        String::from_utf8_lossy(&replacement.stderr)
    );
    let oracle = Command::new("rrdtool")
        .args([
            "update",
            oracle_file.to_str().unwrap(),
            "1000000020:2:9007199254740994:-9007199254740994",
        ])
        .output()
        .unwrap();
    assert!(oracle.status.success());
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&oracle_file).unwrap()
    );
}

#[test]
fn rrdtool_xport_raw_def_and_export_match_rrdtool_xml_and_json() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool xport differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sample.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:load:GAUGE:30:U:U",
            "DS:other:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:16",
            "RRA:MAX:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    for sample in [
        "1000000010:2:20",
        "1000000020:4:40",
        "1000000030:6:60",
        "1000000040:8:80",
    ] {
        let update = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), sample])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:load={}:load:AVERAGE", file.display());
    let other_def = format!("DEF:other={}:other:MAX", file.display());
    for extra in [Vec::<&str>::new(), vec!["--json", "--showtime"]] {
        let mut args = vec![
            "xport",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--step",
            "10",
        ];
        args.extend(extra);
        let expected = Command::new("rrdtool")
            .args(&args)
            .arg(&def)
            .arg(&other_def)
            .arg("XPORT:load:Load")
            .arg("XPORT:other:Other")
            .output()
            .unwrap();
        let actual = Command::new(&alias)
            .args(&args)
            .arg(&def)
            .arg(&other_def)
            .arg("XPORT:load:Load")
            .arg("XPORT:other:Other")
            .output()
            .unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout);
    }
    let args = [
        "xport",
        "--start",
        "1000000000",
        "--end",
        "1000000040",
        "--step",
        "10",
    ];
    let expected = Command::new("rrdtool")
        .args(args)
        .arg(&def)
        .arg("XPORT:load")
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(args)
        .arg(&def)
        .arg("XPORT:load")
        .output()
        .unwrap();
    assert!(expected.status.success());
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);

    // Exercise CDEF dependency ordering, arithmetic, comparison, IF and UNKN
    // handling against the pinned upstream executable.
    let cdef_args = [
        "xport",
        "--start",
        "1000000000",
        "--end",
        "1000000040",
        "--step",
        "10",
        "--json",
    ];
    let expected = Command::new("rrdtool")
        .args(cdef_args)
        .arg(&def)
        .arg(&other_def)
        .arg("CDEF:sum=load,other,+")
        .arg("CDEF:masked=sum,50,GT,sum,UNKN,IF")
        .arg("CDEF:add_known=load,UNKN,ADDNAN")
        .arg("CDEF:min_known=UNKN,load,MINNAN")
        .arg("CDEF:max_known=load,UNKN,MAXNAN")
        .arg("CDEF:rounded=load,2,/,ROUND")
        .arg("CDEF:degrees=load,0,*,1,+,RAD2DEG")
        .arg("CDEF:mean=load,other,2,AVG")
        .arg("CDEF:median=load,other,2,MEDIAN")
        .arg("CDEF:deviation=load,other,2,STDEV")
        .arg("CDEF:depth=load,other,+,DEPTH,EXC,/")
        .arg("CDEF:copied=load,other,2,COPY,4,AVG")
        .arg("CDEF:sorted_min=load,other,2,SORT,POP")
        .arg("CDEF:sorted_max=load,other,2,REV,POP")
        .arg("CDEF:stack_index=load,other,1,INDEX,POP,EXC,POP")
        .arg("CDEF:stack_min=load,other,2,SMIN")
        .arg("CDEF:stack_max=load,other,2,SMAX")
        .arg("CDEF:time=load,POP,0,TIME,+")
        .arg("CDEF:count=load,POP,0,COUNT,+")
        .arg("CDEF:width=load,POP,0,STEPWIDTH,+")
        .arg("CDEF:running=load,PREV,ADDNAN")
        .arg("CDEF:previous_source=PREV(load)")
        .arg("CDEF:rotated=load,other,100,200,3,1,ROLL,POP,POP,+")
        .arg("CDEF:percent50=load,other,50,2,PERCENT")
        .arg("CDEF:percent95=load,other,95,2,PERCENT")
        .arg("CDEF:trend=load,20,TREND")
        .arg("CDEF:trend_nan=load,20,TRENDNAN")
        .arg("CDEF:gappy=load,4,GT,load,UNKN,IF")
        .arg("CDEF:trend_gappy=gappy,20,TREND")
        .arg("CDEF:trend_gappy_nan=gappy,20,TRENDNAN")
        .arg("CDEF:predict=0,10,2,20,load,PREDICT")
        .arg("CDEF:sigma=0,10,2,20,load,PREDICTSIGMA")
        .arg("CDEF:predict_percent=0,10,2,20,50,load,PREDICTPERC")
        .arg("CDEF:predict_percent_nearest=0,10,2,20,-50,load,PREDICTPERC")
        .arg("CDEF:predict_repeat=10,-2,20,load,PREDICT")
        .arg("XPORT:sum:Sum")
        .arg("XPORT:masked:Masked")
        .arg("XPORT:add_known:AddKnown")
        .arg("XPORT:min_known:MinKnown")
        .arg("XPORT:max_known:MaxKnown")
        .arg("XPORT:rounded:Rounded")
        .arg("XPORT:degrees:Degrees")
        .arg("XPORT:mean:Mean")
        .arg("XPORT:median:Median")
        .arg("XPORT:deviation:Deviation")
        .arg("XPORT:depth:Depth")
        .arg("XPORT:copied:Copied")
        .arg("XPORT:sorted_min:SortedMin")
        .arg("XPORT:sorted_max:SortedMax")
        .arg("XPORT:stack_index:StackIndex")
        .arg("XPORT:stack_min:StackMin")
        .arg("XPORT:stack_max:StackMax")
        .arg("XPORT:time:Time")
        .arg("XPORT:count:Count")
        .arg("XPORT:width:Width")
        .arg("XPORT:running:Running")
        .arg("XPORT:previous_source:PreviousSource")
        .arg("XPORT:rotated:Rotated")
        .arg("XPORT:percent50:Percent50")
        .arg("XPORT:percent95:Percent95")
        .arg("XPORT:trend:Trend")
        .arg("XPORT:trend_nan:TrendNan")
        .arg("XPORT:trend_gappy:TrendGappy")
        .arg("XPORT:trend_gappy_nan:TrendGappyNan")
        .arg("XPORT:predict:Predict")
        .arg("XPORT:sigma:Sigma")
        .arg("XPORT:predict_percent:PredictPercent")
        .arg("XPORT:predict_percent_nearest:PredictPercentNearest")
        .arg("XPORT:predict_repeat:PredictRepeat")
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(cdef_args)
        .arg(&def)
        .arg(&other_def)
        .arg("CDEF:sum=load,other,+")
        .arg("CDEF:masked=sum,50,GT,sum,UNKN,IF")
        .arg("CDEF:add_known=load,UNKN,ADDNAN")
        .arg("CDEF:min_known=UNKN,load,MINNAN")
        .arg("CDEF:max_known=load,UNKN,MAXNAN")
        .arg("CDEF:rounded=load,2,/,ROUND")
        .arg("CDEF:degrees=load,0,*,1,+,RAD2DEG")
        .arg("CDEF:mean=load,other,2,AVG")
        .arg("CDEF:median=load,other,2,MEDIAN")
        .arg("CDEF:deviation=load,other,2,STDEV")
        .arg("CDEF:depth=load,other,+,DEPTH,EXC,/")
        .arg("CDEF:copied=load,other,2,COPY,4,AVG")
        .arg("CDEF:sorted_min=load,other,2,SORT,POP")
        .arg("CDEF:sorted_max=load,other,2,REV,POP")
        .arg("CDEF:stack_index=load,other,1,INDEX,POP,EXC,POP")
        .arg("CDEF:stack_min=load,other,2,SMIN")
        .arg("CDEF:stack_max=load,other,2,SMAX")
        .arg("CDEF:time=load,POP,0,TIME,+")
        .arg("CDEF:count=load,POP,0,COUNT,+")
        .arg("CDEF:width=load,POP,0,STEPWIDTH,+")
        .arg("CDEF:running=load,PREV,ADDNAN")
        .arg("CDEF:previous_source=PREV(load)")
        .arg("CDEF:rotated=load,other,100,200,3,1,ROLL,POP,POP,+")
        .arg("CDEF:percent50=load,other,50,2,PERCENT")
        .arg("CDEF:percent95=load,other,95,2,PERCENT")
        .arg("CDEF:trend=load,20,TREND")
        .arg("CDEF:trend_nan=load,20,TRENDNAN")
        .arg("CDEF:gappy=load,4,GT,load,UNKN,IF")
        .arg("CDEF:trend_gappy=gappy,20,TREND")
        .arg("CDEF:trend_gappy_nan=gappy,20,TRENDNAN")
        .arg("CDEF:predict=0,10,2,20,load,PREDICT")
        .arg("CDEF:sigma=0,10,2,20,load,PREDICTSIGMA")
        .arg("CDEF:predict_percent=0,10,2,20,50,load,PREDICTPERC")
        .arg("CDEF:predict_percent_nearest=0,10,2,20,-50,load,PREDICTPERC")
        .arg("CDEF:predict_repeat=10,-2,20,load,PREDICT")
        .arg("XPORT:sum:Sum")
        .arg("XPORT:masked:Masked")
        .arg("XPORT:add_known:AddKnown")
        .arg("XPORT:min_known:MinKnown")
        .arg("XPORT:max_known:MaxKnown")
        .arg("XPORT:rounded:Rounded")
        .arg("XPORT:degrees:Degrees")
        .arg("XPORT:mean:Mean")
        .arg("XPORT:median:Median")
        .arg("XPORT:deviation:Deviation")
        .arg("XPORT:depth:Depth")
        .arg("XPORT:copied:Copied")
        .arg("XPORT:sorted_min:SortedMin")
        .arg("XPORT:sorted_max:SortedMax")
        .arg("XPORT:stack_index:StackIndex")
        .arg("XPORT:stack_min:StackMin")
        .arg("XPORT:stack_max:StackMax")
        .arg("XPORT:time:Time")
        .arg("XPORT:count:Count")
        .arg("XPORT:width:Width")
        .arg("XPORT:running:Running")
        .arg("XPORT:previous_source:PreviousSource")
        .arg("XPORT:rotated:Rotated")
        .arg("XPORT:percent50:Percent50")
        .arg("XPORT:percent95:Percent95")
        .arg("XPORT:trend:Trend")
        .arg("XPORT:trend_nan:TrendNan")
        .arg("XPORT:trend_gappy:TrendGappy")
        .arg("XPORT:trend_gappy_nan:TrendGappyNan")
        .arg("XPORT:predict:Predict")
        .arg("XPORT:sigma:Sigma")
        .arg("XPORT:predict_percent:PredictPercent")
        .arg("XPORT:predict_percent_nearest:PredictPercentNearest")
        .arg("XPORT:predict_repeat:PredictRepeat")
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&actual.stdout),
        String::from_utf8_lossy(&expected.stdout)
    );
}

#[test]
fn rrdtool_xport_prediction_matches_mixed_resolution_history() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping mixed-resolution prediction differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let fine_file = temp.path().join("fine.rrd");
    let coarse_file = temp.path().join("coarse.rrd");
    for (file, step, archive) in [
        (&fine_file, "10", "RRA:AVERAGE:0.5:1:16"),
        (&coarse_file, "20", "RRA:MAX:0.5:1:8"),
    ] {
        let created = Command::new("rrdtool")
            .args([
                "create",
                file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                step,
                "DS:value:GAUGE:60:U:U",
                archive,
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    for index in 1..=10 {
        let fine_update = Command::new("rrdtool")
            .args([
                "update",
                fine_file.to_str().unwrap(),
                &format!("{}:{}", 1_000_000_000 + index * 10, index * 3),
            ])
            .output()
            .unwrap();
        assert!(fine_update.status.success());
        if index % 2 == 0 {
            let coarse_update = Command::new("rrdtool")
                .args([
                    "update",
                    coarse_file.to_str().unwrap(),
                    &format!("{}:{}", 1_000_000_000 + index * 10, index * 11),
                ])
                .output()
                .unwrap();
            assert!(coarse_update.status.success());
        }
    }

    let fine = format!("DEF:fine={}:value:AVERAGE", fine_file.display());
    let coarse = format!("DEF:coarse={}:value:MAX", coarse_file.display());
    let args = [
        "xport",
        "--start",
        "1000000000",
        "--end",
        "1000000100",
        "--step",
        "10",
    ];
    let run = |program: &std::path::Path| {
        Command::new(program)
            .args(args)
            .arg(&fine)
            .arg(&coarse)
            .arg("CDEF:forecast=0,10,2,20,coarse,PREDICT")
            .arg("XPORT:forecast:Forecast")
            .output()
            .unwrap()
    };
    let upstream = run(std::path::Path::new("rrdtool"));
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let actual = run(&alias);
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.status.code(), upstream.status.code());
    assert_eq!(actual.stdout, upstream.stdout);
    assert_eq!(actual.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_rpn_numeric_literals_follow_rrd_strtod() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!(
            "skipping RRDtool RPN numeric conversion differential: rrdtool is not installed"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("numeric.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            "1000000010:1",
            "1000000020:1",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:value={}:value:AVERAGE", database.display());
    let args = [
        "graphv",
        "-",
        "--imgformat=XML",
        "--start",
        "1000000010",
        "--end",
        "1000000030",
    ];
    let elements = [
        "CDEF:literal=value,0,*,1000000010.9999999,+",
        "XPORT:literal:Literal",
        "PRINT:literal:LAST:%0.7lf",
    ];
    let expected = Command::new("rrdtool")
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    assert!(String::from_utf8_lossy(&expected.stdout).contains("1000000011.0000000"));
}

#[test]
fn rrdtool_xport_trend_duration_rounding_matches_rrd_strtod() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool TREND duration differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("trend-duration.rrd");
    let step = 1_000_000_010_i64;
    let start = 2 * step;
    let first_update = start + step;
    let second_update = first_update + step;
    let create = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            &start.to_string(),
            "--step",
            &step.to_string(),
            "DS:value:GAUGE:3000000000:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            &format!("{first_update}:1"),
            &format!("{second_update}:3"),
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:value={}:value:AVERAGE", database.display());
    let args = [
        "xport",
        "--start",
        &start.to_string(),
        "--end",
        &second_update.to_string(),
        "--step",
        &step.to_string(),
        "--json",
    ];
    let elements = [
        "CDEF:trend=value,1000000010.9999999,TREND",
        "XPORT:trend:Trend",
    ];
    let expected = Command::new("rrdtool")
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    assert!(String::from_utf8_lossy(&expected.stdout).contains("5.0000000000e-01"));
}

#[test]
fn rrdtool_xport_cdef_limit_matches_upstream_bounds_and_unknowns() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping RRDtool LIMIT differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("limit.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            "1000000010:2",
            "1000000020:4",
            "1000000030:6",
            "1000000040:U",
            "1000000050:8",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:value={}:value:AVERAGE", database.display());
    let args = [
        "xport",
        "--start",
        "1000000000",
        "--end",
        "1000000050",
        "--step",
        "10",
        "--json",
    ];
    let elements = [
        "CDEF:limited=value,3,7,LIMIT",
        "CDEF:unknown_min=value,UNKN,7,LIMIT",
        "CDEF:unknown_max=value,3,UNKN,LIMIT",
        "XPORT:limited:Limited",
        "XPORT:unknown_min:Unknown minimum",
        "XPORT:unknown_max:Unknown maximum",
    ];
    let expected = Command::new("rrdtool")
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(args)
        .arg(&def)
        .args(elements)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
}

#[test]
fn rrdtool_xport_rpn_aggregates_follow_upstream_stack_order() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool RPN aggregate differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("aggregate-order.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args: Vec<String> = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:avg=value,POP,10000000000000000,-10000000000000000,1,3,AVG".to_owned(),
        "CDEF:stdev=value,POP,10000000000000000,10000000000000002,-1,3,STDEV".to_owned(),
        "XPORT:avg:avg".to_owned(),
        "XPORT:stdev:stdev".to_owned(),
    ]
    .into();
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "upstream: {}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "rondi: {}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_rpn_percent_sorts_unknown_values_first() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool RPN PERCENT differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("percent-unknown-order.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:percent=value,POP,UNKN,5,10,50,3,PERCENT".to_owned(),
        "XPORT:percent:percent".to_owned(),
    ];
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_now_uses_whole_seconds() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool NOW differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("now.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:now=value,POP,NOW,1000,%".to_owned(),
        "XPORT:now:now".to_owned(),
    ];
    let mut matched_same_second = false;
    let mut last_outputs = None;
    for _ in 0..8 {
        let upstream = Command::new("rrdtool")
            .arg("xport")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let rondi = Command::new(&alias)
            .arg("xport")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let upstream = upstream.wait_with_output().unwrap();
        let rondi = rondi.wait_with_output().unwrap();
        assert!(upstream.status.success());
        assert!(rondi.status.success());
        if upstream.stdout == rondi.stdout {
            matched_same_second = true;
            break;
        }
        last_outputs = Some((upstream.stdout, rondi.stdout));
    }
    assert!(
        matched_same_second,
        "NOW differential did not overlap the same second; latest outputs: {:?}",
        last_outputs.map(|(upstream, rondi)| (
            String::from_utf8_lossy(&upstream).into_owned(),
            String::from_utf8_lossy(&rondi).into_owned()
        ))
    );
}

#[test]
fn rrdtool_xport_rpn_sort_orders_unknown_values_first() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool RPN SORT differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sort-unknown-order.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:middle=value,POP,UNKN,5,10,3,SORT,1,INDEX,EXC,POP,EXC,POP,EXC,POP".to_owned(),
        "XPORT:middle:middle".to_owned(),
    ];
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_rpn_index_truncates_fractional_argument() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool RPN INDEX differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("index-truncation.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:selected=value,POP,10,20,1.5,INDEX,POP,EXC,POP".to_owned(),
        "XPORT:selected:selected".to_owned(),
    ];
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_rpn_count_operators_truncate_fractional_values() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool RPN count differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("count-truncation.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "DS:other:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:10:20"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        format!("DEF:other={}:other:AVERAGE", file.display()),
        "CDEF:mean=value,other,2.5,AVG".to_owned(),
        "CDEF:copied=value,other,2.5,COPY,4,AVG".to_owned(),
        "XPORT:mean:mean".to_owned(),
        "XPORT:copied:copied".to_owned(),
    ];
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_xport_rpn_zero_copy_and_roll_counts_are_noops() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool zero-count RPN differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("zero-stack-count.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(update.status.success());

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = [
        "--json".to_owned(),
        "--start".to_owned(),
        "1000000010".to_owned(),
        "--end".to_owned(),
        "1000000020".to_owned(),
        format!("DEF:value={}:value:AVERAGE", file.display()),
        "CDEF:copied=value,POP,42,0,COPY".to_owned(),
        "CDEF:rolled=value,POP,42,0,1,ROLL".to_owned(),
        "XPORT:copied:copied".to_owned(),
        "XPORT:rolled:rolled".to_owned(),
    ];
    let upstream = Command::new("rrdtool")
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .arg("xport")
        .args(&args)
        .output()
        .unwrap();
    assert!(
        upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream.stderr)
    );
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn graph_print_and_gprint_printf_grammar_matches_pinned_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping graph printf differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("format.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            "1000000010:1234.5",
            "1000000020:2567.89",
            "1000000030:2050.0",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let definition = format!("DEF:x={}:x:AVERAGE", database.display());
    let formats = [
        "%lf",
        "%.2lf",
        "%+lf",
        "%-lf",
        "% lf",
        "%0lf",
        "%#lf",
        "%10lf",
        "%010.2lf",
        "%-10.3lg",
        "% .2le",
        "%#.0lf",
        "%0.3lG",
        "%1lF",
        "value=%.2lf",
        "%% %.2lf %%",
        "%.2lf %s",
        "%.2lf %S",
        "-%10.4g +%%",
        "%g",
        "%010.2LE",
        "%#10.4g",
        "% 08.3e",
        "%-#12.5G",
        "%+12.0F",
        "%010.0E",
        "%0.1f",
        "%9.3e",
        "%.lf",
        "%lf %lf",
        "%lf %n",
        "%s",
    ];

    for (index, format) in formats.iter().enumerate() {
        for directive in ["PRINT", "GPRINT"] {
            let arguments = |output: &std::path::Path| {
                vec![
                    "graphv".to_owned(),
                    output.to_str().unwrap().to_owned(),
                    "--imgformat".to_owned(),
                    "JSON".to_owned(),
                    "--start".to_owned(),
                    "1000000010".to_owned(),
                    "--end".to_owned(),
                    "1000000030".to_owned(),
                    definition.clone(),
                    "XPORT:x:x".to_owned(),
                    format!("{directive}:x:AVERAGE:{format}"),
                ]
            };
            let upstream_output = temp
                .path()
                .join(format!("upstream-{index}-{directive}.json"));
            let rondi_output = temp.path().join(format!("rondi-{index}-{directive}.json"));
            let expected = Command::new("rrdtool")
                .args(arguments(&upstream_output))
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .args(arguments(&rondi_output))
                .output()
                .unwrap();
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "{directive} {format}"
            );
            assert_eq!(
                actual.stdout, expected.stdout,
                "{directive} {format} stdout"
            );
            assert_eq!(
                actual.stderr, expected.stderr,
                "{directive} {format} stderr"
            );
            if expected.status.success() {
                let expected_file = std::fs::read(&upstream_output).unwrap_or_default();
                let actual_file = std::fs::read(&rondi_output).unwrap_or_default();
                assert_eq!(
                    actual_file, expected_file,
                    "{directive} {format} output file"
                );
            }
        }
    }

    for (index, exponent) in (-6..=7).enumerate() {
        let scale_file = temp.path().join(format!("scale-{exponent}.rrd"));
        let created = Command::new("rrdtool")
            .args([
                "create",
                scale_file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:x:GAUGE:30:U:U",
                "RRA:AVERAGE:0.5:1:4",
            ])
            .output()
            .unwrap();
        assert!(created.status.success());
        let value = 1000_f64.powi(exponent);
        let updated = Command::new("rrdtool")
            .args([
                "update",
                scale_file.to_str().unwrap(),
                &format!("1000000010:{value}"),
            ])
            .output()
            .unwrap();
        assert!(updated.status.success());
        let scale_definition = format!("DEF:x={}:x:AVERAGE", scale_file.display());
        let arguments = |output: &std::path::Path| {
            vec![
                "graphv".to_owned(),
                output.to_str().unwrap().to_owned(),
                "--imgformat".to_owned(),
                "JSON".to_owned(),
                "--start".to_owned(),
                "1000000000".to_owned(),
                "--end".to_owned(),
                "1000000020".to_owned(),
                scale_definition.clone(),
                "XPORT:x:x".to_owned(),
                "PRINT:x:AVERAGE:%0.2lf %s".to_owned(),
            ]
        };
        let upstream_output = temp.path().join(format!("scale-upstream-{index}.json"));
        let rondi_output = temp.path().join(format!("scale-rondi-{index}.json"));
        let expected = Command::new("rrdtool")
            .args(arguments(&upstream_output))
            .output()
            .unwrap();
        let actual = Command::new(&alias)
            .args(arguments(&rondi_output))
            .output()
            .unwrap();
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "SI exponent {exponent}"
        );
        assert_eq!(
            actual.stdout, expected.stdout,
            "SI exponent {exponent} stdout"
        );
        assert_eq!(
            actual.stderr, expected.stderr,
            "SI exponent {exponent} stderr"
        );
        assert_eq!(
            std::fs::read(&rondi_output).unwrap_or_default(),
            std::fs::read(&upstream_output).unwrap_or_default(),
            "SI exponent {exponent} output file"
        );
    }

    for (index, sample) in ["0", "-1234.5", "U"].iter().enumerate() {
        let scale_file = temp.path().join(format!("scale-special-{index}.rrd"));
        let created = Command::new("rrdtool")
            .args([
                "create",
                scale_file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:x:GAUGE:30:U:U",
                "RRA:AVERAGE:0.5:1:4",
            ])
            .output()
            .unwrap();
        assert!(created.status.success());
        let updated = Command::new("rrdtool")
            .args([
                "update",
                scale_file.to_str().unwrap(),
                &format!("1000000010:{sample}"),
            ])
            .output()
            .unwrap();
        assert!(updated.status.success());
        let scale_definition = format!("DEF:x={}:x:AVERAGE", scale_file.display());
        let arguments = |output: &std::path::Path| {
            vec![
                "graphv".to_owned(),
                output.to_str().unwrap().to_owned(),
                "--imgformat".to_owned(),
                "JSON".to_owned(),
                "--start".to_owned(),
                "1000000000".to_owned(),
                "--end".to_owned(),
                "1000000020".to_owned(),
                scale_definition.clone(),
                "XPORT:x:x".to_owned(),
                "PRINT:x:AVERAGE:%0.2lf %s".to_owned(),
            ]
        };
        let upstream_output = temp.path().join(format!("special-upstream-{index}.json"));
        let rondi_output = temp.path().join(format!("special-rondi-{index}.json"));
        let expected = Command::new("rrdtool")
            .args(arguments(&upstream_output))
            .output()
            .unwrap();
        let actual = Command::new(&alias)
            .args(arguments(&rondi_output))
            .output()
            .unwrap();
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "SI special {sample}"
        );
        assert_eq!(actual.stdout, expected.stdout, "SI special {sample} stdout");
        assert_eq!(actual.stderr, expected.stderr, "SI special {sample} stderr");
        assert_eq!(
            std::fs::read(&rondi_output).unwrap_or_default(),
            std::fs::read(&upstream_output).unwrap_or_default(),
            "SI special {sample} output file"
        );
    }
}

#[test]
fn graph_valstrftime_rejection_matches_pinned_rrdtool_1110() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping graph valstrftime differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("valstrftime.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let executable = env!("CARGO_BIN_EXE_rondi");
    let alias = temp.path().join("rrdtool");
    symlink(executable, &alias).unwrap();
    let definition = format!("DEF:x={}:x:AVERAGE", database.display());
    let args = [
        "graphv",
        "-",
        "--imgformat=JSON",
        "--start",
        "1000000010",
        "--end",
        "1000000050",
        definition.as_str(),
        "XPORT:x:Series",
        "VDEF:v=x,AVERAGE",
        "PRINT:v:%F %T:valstrftime",
    ];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    let actual = Command::new(alias).args(args).output().unwrap();
    assert_eq!(actual.status.code(), expected.status.code());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
}

#[test]
fn deterministic_irregular_gauge_sequences_match_pinned_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping randomized sequence differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_rondi");
    let alias = temp.path().join("rrdtool");
    symlink(executable, &alias).unwrap();

    for initial_seed in 1_u64..=8 {
        let oracle = temp.path().join(format!("oracle-{initial_seed}.rrd"));
        let rondi = temp.path().join(format!("rondi-{initial_seed}.rrd"));
        let created = Command::new("rrdtool")
            .args([
                "create",
                oracle.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:x:GAUGE:25:U:U",
                "RRA:AVERAGE:0.5:2:24",
                "RRA:MAX:0.5:2:24",
            ])
            .output()
            .unwrap();
        assert!(created.status.success());
        std::fs::copy(&oracle, &rondi).unwrap();

        let mut state = initial_seed;
        let mut timestamp = 1_000_000_000_i64;
        let mut samples = Vec::new();
        for _ in 0..32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            timestamp += 10 * (1 + (state % 4) as i64);
            let value = if state % 7 == 0 {
                String::from("U")
            } else {
                format!("{}.{:02}", (state % 401) as i64 - 200, (state >> 8) % 100)
            };
            samples.push(format!("{timestamp}:{value}"));
        }

        for batch in samples.chunks(4) {
            let expected = Command::new("rrdtool")
                .arg("update")
                .arg(&oracle)
                .args(batch)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .arg("update")
                .arg(&rondi)
                .args(batch)
                .output()
                .unwrap();
            assert_eq!(actual.status.code(), expected.status.code());
            assert_eq!(actual.stdout, expected.stdout);
            assert_eq!(actual.stderr, expected.stderr);
            assert_eq!(
                std::fs::read(&rondi).unwrap(),
                std::fs::read(&oracle).unwrap(),
                "seed={initial_seed}, batch={batch:?}"
            );
        }

        let end = (timestamp + 30).to_string();
        for consolidation in ["AVERAGE", "MAX"] {
            let args = [
                "--resolution",
                "20",
                "--start",
                "1000000000",
                "--end",
                end.as_str(),
            ];
            let expected = Command::new("rrdtool")
                .arg("fetch")
                .arg(&oracle)
                .arg(consolidation)
                .args(args)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .arg("fetch")
                .arg(&rondi)
                .arg(consolidation)
                .args(args)
                .output()
                .unwrap();
            assert_eq!(actual.status.code(), expected.status.code());
            assert_eq!(actual.stdout, expected.stdout, "seed={initial_seed}");
            assert_eq!(actual.stderr, expected.stderr, "seed={initial_seed}");
        }
    }
}

#[test]
fn deterministic_mixed_data_source_sequences_match_pinned_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping mixed data-source differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let executable = env!("CARGO_BIN_EXE_rondi");
    let alias = temp.path().join("rrdtool");
    symlink(executable, &alias).unwrap();

    for initial_seed in 11_u64..=13 {
        let oracle = temp.path().join(format!("oracle-{initial_seed}.rrd"));
        let rondi = temp.path().join(format!("rondi-{initial_seed}.rrd"));
        let created = Command::new("rrdtool")
            .args([
                "create",
                oracle.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:c:COUNTER:40:U:4294967295",
                "DS:d:DERIVE:40:U:U",
                "DS:a:ABSOLUTE:40:U:U",
                "DS:dc:DCOUNTER:40:U:U",
                "DS:dd:DDERIVE:40:U:U",
                "RRA:AVERAGE:0.5:2:24",
                "RRA:MAX:0.5:2:24",
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        std::fs::copy(&oracle, &rondi).unwrap();

        let mut state = initial_seed;
        let mut timestamp = 1_000_000_000_i64;
        let mut counter = 4_u64;
        let mut samples = Vec::new();
        for sample_index in 0..24 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            timestamp += 10 * (1 + (state % 3) as i64);
            if sample_index > 1 {
                counter = (counter + 1 + state % 97) % (1_u64 << 32);
            }
            let derive = (state % 20_001) as i64 - 10_000;
            let absolute = (state >> 11) % 500_000;
            let dcounter = ((state >> 19) % 100_000) as f64 / 16.0;
            let dderive = ((state >> 27) % 20_001) as i64 - 10_000;
            let counter_text = match sample_index {
                0 => "4294967290".to_owned(),
                1 => "4".to_owned(),
                _ => counter.to_string(),
            };
            samples.push(format!(
                "{timestamp}:{counter_text}:{derive}:{absolute}:{dcounter:.4}:{dderive}"
            ));
        }

        for batch in samples.chunks(4) {
            let expected = Command::new("rrdtool")
                .arg("update")
                .arg(&oracle)
                .args(batch)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .arg("update")
                .arg(&rondi)
                .args(batch)
                .output()
                .unwrap();
            assert_eq!(actual.status.code(), expected.status.code());
            assert_eq!(actual.stdout, expected.stdout);
            assert_eq!(actual.stderr, expected.stderr);
            assert_eq!(
                std::fs::read(&rondi).unwrap(),
                std::fs::read(&oracle).unwrap(),
                "seed={initial_seed}, batch={batch:?}"
            );
        }

        let end = (timestamp + 30).to_string();
        for consolidation in ["AVERAGE", "MAX"] {
            let options = ["--resolution", "20", "--start", "1000000000", "--end", &end];
            let expected = Command::new("rrdtool")
                .arg("fetch")
                .arg(&oracle)
                .arg(consolidation)
                .args(options)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .arg("fetch")
                .arg(&rondi)
                .arg(consolidation)
                .args(options)
                .output()
                .unwrap();
            assert_eq!(actual.status.code(), expected.status.code());
            assert_eq!(actual.stdout, expected.stdout, "seed={initial_seed}");
            assert_eq!(actual.stderr, expected.stderr, "seed={initial_seed}");
        }
    }
}

#[test]
fn rrdtool_graphv_xml_json_and_graph_file_match_xport_subset() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool graph differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("graph.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            database.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            database.to_str().unwrap(),
            "1000000010:1",
            "1000000020:3",
            "1000000030:U",
            "1000000040:5",
            "1000000050:9",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let executable = env!("CARGO_BIN_EXE_rondi");
    let alias = temp.path().join("rrdtool");
    symlink(executable, &alias).unwrap();
    let def = format!("DEF:x={}:x:AVERAGE", database.display());
    let image_dimensions = |path: &std::path::Path| {
        let bytes = std::fs::read(path).unwrap();
        let reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        (reader.info().width, reader.info().height)
    };
    let image_pixel = |path: &std::path::Path, x: u32, y: u32| {
        let bytes = std::fs::read(path).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut pixels).unwrap();
        assert_eq!(frame.color_type, png::ColorType::Rgb);
        let offset = ((y * frame.width + x) * 3) as usize;
        [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
    };
    let render_options =
        |program: &std::path::Path, path: &std::path::Path, options: &[&str], elements: &[&str]| {
            Command::new(program)
                .args([
                    "graph",
                    path.to_str().unwrap(),
                    "--imgformat=PNG",
                    "--width",
                    "200",
                    "--height",
                    "140",
                    "--start",
                    "1000000010",
                    "--end",
                    "1000000050",
                ])
                .args(options)
                .arg(&def)
                .args(elements)
                .output()
                .unwrap()
        };
    for (option, expected_size) in [
        ("--only-graph", (200, 140)),
        ("-j", (200, 140)),
        ("--full-size-mode", (200, 140)),
        ("-D", (200, 140)),
    ] {
        let our_path = temp.path().join(format!("layout-ours-{option}.png"));
        let upstream_path = temp.path().join(format!("layout-upstream-{option}.png"));
        let options = [option, "--title", "Load", "--vertical-label", "bits/s"];
        for (program, path) in [
            (alias.as_path(), &our_path),
            (std::path::Path::new("rrdtool"), &upstream_path),
        ] {
            let result = render_options(program, path, &options, &["LINE1:x#ff0000:load"]);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                image_dimensions(path),
                expected_size,
                "option={option}, program={program:?}"
            );
        }
    }
    for (options, expect_legend) in [
        (&["-g"][..], false),
        (&["--force-rules-legend"][..], true),
        (&["-F"][..], true),
    ] {
        let path = temp
            .path()
            .join(format!("rule-legend-{}.png", expect_legend));
        let result = render_options(
            alias.as_path(),
            &path,
            options,
            &["HRULE:100#ff0000:outside"],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let (_width, height) = image_dimensions(&path);
        assert_eq!(height > 140 + 39, expect_legend, "options={options:?}");
    }
    let direction_paths = [
        temp.path().join("legend-topdown.png"),
        temp.path().join("legend-bottomup.png"),
    ];
    for (path, direction) in direction_paths.iter().zip(["topdown", "bottomup"]) {
        let result = render_options(
            alias.as_path(),
            path,
            &[&format!("--legend-direction={direction}")],
            &["LINE1:x#ff0000:first", "LINE1:x#0000ff:second"],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_ne!(
        std::fs::read(&direction_paths[0]).unwrap(),
        std::fs::read(&direction_paths[1]).unwrap()
    );
    let baseline_path = temp.path().join("color-baseline.png");
    let baseline = render_options(
        alias.as_path(),
        &baseline_path,
        &[],
        &["LINE1:x#ff0000:load"],
    );
    assert!(baseline.status.success());
    let baseline_bytes = std::fs::read(&baseline_path).unwrap();
    for (index, tag) in [
        "BACK", "CANVAS", "SHADEA", "SHADEB", "GRID", "MGRID", "FONT", "AXIS", "FRAME", "ARROW",
    ]
    .iter()
    .enumerate()
    {
        let path = temp.path().join(format!("color-{tag}.png"));
        let color = format!("{tag}#12ab34");
        let color_arg = match index % 3 {
            0 => vec!["--color".to_owned(), color.clone()],
            1 => vec!["-c".to_owned(), color.clone()],
            _ => vec![format!("--color={color}")],
        };
        let color_args = color_arg.iter().map(String::as_str).collect::<Vec<_>>();
        let mut options = color_args;
        if matches!(*tag, "SHADEA" | "SHADEB") {
            options.extend(["--border", "2"]);
        }
        let elements = if *tag == "FRAME" {
            &["TICK:x#ff0000:0.5:events"][..]
        } else {
            &["LINE1:x#ff0000:load"][..]
        };
        let result = render_options(alias.as_path(), &path, &options, elements);
        assert!(
            result.status.success(),
            "tag={tag}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_ne!(
            std::fs::read(path).unwrap(),
            baseline_bytes,
            "color tag {tag} had no visible effect"
        );
    }
    let back_path = temp.path().join("background-color.png");
    let back_result = render_options(
        alias.as_path(),
        &back_path,
        &["--border", "0", "--color=BACK#12ab34"],
        &["LINE1:x"],
    );
    assert!(back_result.status.success());
    assert_eq!(image_pixel(&back_path, 0, 0), [0x12, 0xab, 0x34]);
    let canvas_path = temp.path().join("canvas-color.png");
    let canvas_result = render_options(
        alias.as_path(),
        &canvas_path,
        &["--only-graph", "-c", "CANVAS#12ab34"],
        &["LINE1:x"],
    );
    assert!(canvas_result.status.success());
    assert_eq!(image_pixel(&canvas_path, 10, 10), [0x12, 0xab, 0x34]);
    let alpha_path = temp.path().join("background-alpha.png");
    let alpha_result = render_options(
        alias.as_path(),
        &alpha_path,
        &["--border", "0", "--color", "BACK#ff000080"],
        &["LINE1:x"],
    );
    assert!(alpha_result.status.success());
    assert_eq!(image_pixel(&alpha_path, 0, 0), [255, 127, 127]);
    for (option, value) in [("--grid-dash", "1:3"), ("--border", "0")] {
        let path = temp.path().join(format!("option-{option}.png"));
        let result = render_options(
            alias.as_path(),
            &path,
            &[option, value],
            &["LINE1:x#ff0000:load"],
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_ne!(
            std::fs::read(path).unwrap(),
            baseline_bytes,
            "{option} should affect rendered pixels"
        );
    }
    for (option, value) in [
        ("--grid-dash", "bad"),
        ("--border", "65"),
        ("--color", "UNKNOWN#ffffff"),
        ("--color", "BACK#xyz"),
    ] {
        let path = temp.path().join("invalid-graph-option.png");
        let result = render_options(
            alias.as_path(),
            &path,
            &[option, value],
            &["LINE1:x#ff0000:load"],
        );
        assert!(
            !result.status.success(),
            "{option} {value} should be rejected"
        );
    }
    for (index, options) in [
        vec!["-A"],
        vec!["--alt-autoscale"],
        vec!["-J"],
        vec!["-M"],
        vec![
            "--rigid",
            "--allow-shrink",
            "--lower-limit",
            "0",
            "--upper-limit",
            "100",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let options = options.into_iter().collect::<Vec<_>>();
        let path_ours = temp.path().join(format!("scale-ours-{index}.png"));
        let path_upstream = temp.path().join(format!("scale-upstream-{index}.png"));
        for program in [alias.as_path(), std::path::Path::new("rrdtool")] {
            let path = if program == alias.as_path() {
                &path_ours
            } else {
                &path_upstream
            };
            let result = render_options(program, path, &options, &["LINE1:x#ff0000:load"]);
            assert!(
                result.status.success(),
                "program={program:?}, options={options:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
    for angle in ["90", "270", "45"] {
        let path_ours = temp.path().join(format!("label-angle-ours-{angle}.png"));
        let path_upstream = temp
            .path()
            .join(format!("label-angle-upstream-{angle}.png"));
        let options = [
            "--vertical-label",
            "Throughput",
            "--vertical-label-angle",
            angle,
        ];
        for (program, path) in [
            (alias.as_path(), &path_ours),
            (std::path::Path::new("rrdtool"), &path_upstream),
        ] {
            let result = render_options(program, path, &options, &["LINE1:x#ff0000:load"]);
            assert!(
                result.status.success(),
                "angle={angle}, program={program:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        assert_eq!(
            image_dimensions(&path_ours),
            image_dimensions(&path_upstream)
        );
    }
    let bad_angle = render_options(
        alias.as_path(),
        &temp.path().join("bad-angle.png"),
        &["--vertical-label-angle", "NaN"],
        &["LINE1:x#ff0000:load"],
    );
    assert!(!bad_angle.status.success());
    for (fraction, should_draw) in [("0", false), ("0.5", true)] {
        let ours_path = temp.path().join(format!("tick-{fraction}-rondi.png"));
        let upstream_path = temp.path().join(format!("tick-{fraction}-rrdtool.png"));
        let render = |program: &std::path::Path, path: &std::path::Path| {
            Command::new(program)
                .args([
                    "graph",
                    path.to_str().unwrap(),
                    "--imgformat=PNG",
                    "--width",
                    "40",
                    "--height",
                    "30",
                    "--start",
                    "1000000010",
                    "--end",
                    "1000000050",
                ])
                .arg(&def)
                .arg(format!("TICK:x#ff0000:{fraction}"))
                .output()
                .unwrap()
        };
        for (program, path) in [
            (alias.as_path(), &ours_path),
            (std::path::Path::new("rrdtool"), &upstream_path),
        ] {
            let output = render(program, path);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let image = std::fs::read(path).unwrap();
            let mut reader = png::Decoder::new(std::io::Cursor::new(image))
                .read_info()
                .unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            let frame = reader.next_frame(&mut pixels).unwrap();
            pixels.truncate(frame.buffer_size());
            let draws_red = pixels.chunks_exact(3).any(|pixel| pixel == [255, 0, 0]);
            assert_eq!(
                draws_red, should_draw,
                "program={program:?}, fraction={fraction}"
            );
        }
    }
    let graph_elements = [
        vec!["XPORT:x:Series"],
        vec!["AREA:x#ff0000:Area", "LINE1:x#00ff00:Line"],
        vec!["AREA:x#ff0000:Base", "AREA:x#0000ff:Stack:STACK"],
        vec![
            "LINE1:x#ff0000:Load",
            "GPRINT:x:AVERAGE:%0.2lf",
            "PRINT:x:AVERAGE:%0.1lf",
        ],
        vec![
            "XPORT:x:Series",
            "GPRINT:x:MIN:%0.0lf",
            "GPRINT:x:MAX:%0.0lf",
            "GPRINT:x:LAST:%0.0lf",
        ],
        vec![
            "VDEF:avg=x,AVERAGE",
            "VDEF:max=x,MAXIMUM",
            "VDEF:last=x,LAST",
            "VDEF:first=x,FIRST",
            "VDEF:pct=x,50,PERCENT",
            "VDEF:pctnan=x,50,PERCENTNAN",
            "LINE1:x#ff0000:Load",
            "GPRINT:avg:%0.2lf",
            "GPRINT:max:%0.2lf",
            "GPRINT:last:%0.2lf",
            "GPRINT:first:%F %T:strftime",
            "GPRINT:pct:%0.2lf",
            "GPRINT:pctnan:%0.2lf",
            "PRINT:avg:%0.2lf",
            "PRINT:first:%F %T:strftime",
        ],
    ];
    for format in ["XML", "JSON"] {
        let imgformat = format!("--imgformat={format}");
        let args = [
            "graphv",
            "-",
            imgformat.as_str(),
            "--start",
            "1000000010",
            "--end",
            "1000000050",
        ];
        for elements in &graph_elements {
            let expected = Command::new("rrdtool")
                .env("TZ", "UTC")
                .args(args)
                .arg(&def)
                .args(elements)
                .output()
                .unwrap();
            let actual = Command::new(&alias)
                .env("TZ", "UTC")
                .args(args)
                .arg(&def)
                .args(elements)
                .output()
                .unwrap();
            assert!(
                expected.status.success(),
                "{}",
                String::from_utf8_lossy(&expected.stderr)
            );
            assert!(
                actual.status.success(),
                "{}",
                String::from_utf8_lossy(&actual.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "format={format}, {elements:?}"
            );
        }
    }

    let expected_path = temp.path().join("expected.xml");
    let actual_path = temp.path().join("actual.xml");
    let args = [
        "graph",
        "--imgformat=XML",
        "--start",
        "1000000010",
        "--end",
        "1000000050",
    ];
    let expected = Command::new("rrdtool")
        .arg("graph")
        .arg(&expected_path)
        .args(&args[1..])
        .arg(&def)
        .args(&graph_elements[1])
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .arg("graph")
        .arg(&actual_path)
        .args(&args[1..])
        .arg(&def)
        .args(&graph_elements[1])
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        std::fs::read(actual_path).unwrap(),
        std::fs::read(expected_path).unwrap()
    );

    let png_path = temp.path().join("native-graph.png");
    let png = Command::new(&alias)
        .args([
            "graph",
            png_path.to_str().unwrap(),
            "--imgformat=PNG",
            "--width",
            "160",
            "--height",
            "64",
            "--title",
            "Traffic",
            "--vertical-label",
            "bits/s",
            "--start",
            "1000000010",
            "--end",
            "1000000050",
        ])
        .arg(&def)
        .args(&graph_elements[1])
        .output()
        .unwrap();
    assert!(
        png.status.success(),
        "{}",
        String::from_utf8_lossy(&png.stderr)
    );
    let expected_png_path = temp.path().join("upstream-graph.png");
    let expected_png = Command::new("rrdtool")
        .args([
            "graph",
            expected_png_path.to_str().unwrap(),
            "--imgformat=PNG",
            "--width",
            "160",
            "--height",
            "64",
            "--title",
            "Traffic",
            "--vertical-label",
            "bits/s",
            "--start",
            "1000000010",
            "--end",
            "1000000050",
        ])
        .arg(&def)
        .args(&graph_elements[1])
        .output()
        .unwrap();
    assert!(
        expected_png.status.success(),
        "{}",
        String::from_utf8_lossy(&expected_png.stderr)
    );
    let image = std::fs::read(png_path).unwrap();
    let expected_image = std::fs::read(expected_png_path).unwrap();
    let decoder = png::Decoder::new(std::io::Cursor::new(&image));
    let mut reader = decoder.read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    let expected_decoder = png::Decoder::new(std::io::Cursor::new(&expected_image));
    let mut expected_reader = expected_decoder.read_info().unwrap();
    let mut expected_pixels = vec![0; expected_reader.output_buffer_size().unwrap()];
    let expected_info = expected_reader.next_frame(&mut expected_pixels).unwrap();
    assert_eq!(
        (info.width, info.height),
        (expected_info.width, expected_info.height)
    );
    let colors = pixels[..info.buffer_size()]
        .chunks_exact(3)
        .collect::<Vec<_>>();
    assert!(colors.iter().any(|pixel| *pixel == [255, 0, 0]));
    assert!(colors.iter().any(|pixel| *pixel == [0, 255, 0]));

    let stacked_images = [
        temp.path().join("native-stacked.png"),
        temp.path().join("rrdtool-stacked.png"),
    ];
    for (program, path) in [
        (alias.as_path(), stacked_images[0].as_path()),
        (std::path::Path::new("rrdtool"), stacked_images[1].as_path()),
    ] {
        let rendered = Command::new(program)
            .args([
                "graph",
                path.to_str().unwrap(),
                "--imgformat=PNG",
                "--width",
                "160",
                "--height",
                "64",
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ])
            .arg(&def)
            .args(&graph_elements[2])
            .output()
            .unwrap();
        assert!(
            rendered.status.success(),
            "{}: {}",
            program.display(),
            String::from_utf8_lossy(&rendered.stderr)
        );
    }
    for path in stacked_images {
        let data = std::fs::read(path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        let plot_colors = pixels[..info.buffer_size()]
            .chunks_exact(3)
            .enumerate()
            .filter(|(pixel_index, _)| {
                let x = *pixel_index as u32 % info.width;
                let y = *pixel_index as u32 / info.width;
                (51..211).contains(&x) && (15..79).contains(&y)
            })
            .map(|(_, pixel)| pixel)
            .collect::<Vec<_>>();
        assert!(plot_colors.iter().any(|pixel| **pixel == [255, 0, 0]));
        assert!(plot_colors.iter().any(|pixel| **pixel == [0, 0, 255]));
    }

    let tick_images = [
        temp.path().join("native-ticks.png"),
        temp.path().join("rrdtool-ticks.png"),
    ];
    for (program, path) in [
        (alias.as_path(), tick_images[0].as_path()),
        (std::path::Path::new("rrdtool"), tick_images[1].as_path()),
    ] {
        let tick = Command::new(program)
            .args([
                "graph",
                path.to_str().unwrap(),
                "--imgformat=PNG",
                "--width",
                "160",
                "--height",
                "64",
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ])
            .arg(&def)
            .arg("TICK:x#ff0000:0.25:Events")
            .output()
            .unwrap();
        assert!(
            tick.status.success(),
            "{}: {}",
            program.display(),
            String::from_utf8_lossy(&tick.stderr)
        );
    }
    for (image_index, path) in tick_images.into_iter().enumerate() {
        let data = std::fs::read(path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        let red_ticks_in_plot = pixels[..info.buffer_size()]
            .chunks_exact(3)
            .enumerate()
            .filter(|(pixel_index, pixel)| {
                let x = *pixel_index as u32 % info.width;
                let y = *pixel_index as u32 / info.width;
                *pixel == [255, 0, 0] && (51..211).contains(&x) && (15..79).contains(&y)
            })
            .count();
        assert!(
            red_ticks_in_plot > 0,
            "image {image_index} has no plot TICK marks"
        );
    }

    let rule_images = [
        temp.path().join("native-hrule.png"),
        temp.path().join("rrdtool-hrule.png"),
    ];
    for (program, path) in [
        (alias.as_path(), rule_images[0].as_path()),
        (std::path::Path::new("rrdtool"), rule_images[1].as_path()),
    ] {
        let rule = Command::new(program)
            .args([
                "graph",
                path.to_str().unwrap(),
                "--imgformat=PNG",
                "--width",
                "160",
                "--height",
                "64",
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ])
            .arg(&def)
            .arg("LINE1:x#00ff00:Load")
            .arg("HRULE:4#ff0000:Threshold")
            .output()
            .unwrap();
        assert!(
            rule.status.success(),
            "{}: {}",
            program.display(),
            String::from_utf8_lossy(&rule.stderr)
        );
    }
    for path in rule_images {
        let data = std::fs::read(path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert!(
            pixels[..info.buffer_size()]
                .chunks_exact(3)
                .enumerate()
                .any(|(pixel_index, pixel)| {
                    let x = pixel_index as u32 % info.width;
                    let y = pixel_index as u32 / info.width;
                    pixel == [255, 0, 0] && (51..211).contains(&x) && (15..79).contains(&y)
                })
        );
    }

    let vrule_images = [
        temp.path().join("native-vrule.png"),
        temp.path().join("rrdtool-vrule.png"),
    ];
    for (program, path) in [
        (alias.as_path(), vrule_images[0].as_path()),
        (std::path::Path::new("rrdtool"), vrule_images[1].as_path()),
    ] {
        let rule = Command::new(program)
            .args([
                "graph",
                path.to_str().unwrap(),
                "--imgformat=PNG",
                "--width",
                "160",
                "--height",
                "64",
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ])
            .arg(&def)
            .arg("LINE1:x#00ff00:Load")
            .arg("VRULE:1000000030#ff0000:Maintenance")
            .output()
            .unwrap();
        assert!(
            rule.status.success(),
            "{}: {}",
            program.display(),
            String::from_utf8_lossy(&rule.stderr)
        );
    }
    for path in vrule_images {
        let data = std::fs::read(path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert!(
            pixels[..info.buffer_size()]
                .chunks_exact(3)
                .enumerate()
                .any(|(pixel_index, pixel)| {
                    let x = pixel_index as u32 % info.width;
                    let y = pixel_index as u32 / info.width;
                    pixel == [255, 0, 0] && (51..211).contains(&x) && (15..79).contains(&y)
                })
        );
    }

    let no_legend = ["--width", "160", "--height", "64", "--no-legend"];
    let actual_no_legend_path = temp.path().join("native-no-legend.png");
    let expected_no_legend_path = temp.path().join("upstream-no-legend.png");
    for (program, path) in [
        (alias.as_path(), actual_no_legend_path.as_path()),
        (
            std::path::Path::new("rrdtool"),
            expected_no_legend_path.as_path(),
        ),
    ] {
        let rendered = Command::new(program)
            .arg("graph")
            .arg(path)
            .arg("--imgformat=PNG")
            .args(no_legend)
            .args(["--start", "1000000010", "--end", "1000000050"])
            .arg(&def)
            .args(&graph_elements[1])
            .output()
            .unwrap();
        assert!(
            rendered.status.success(),
            "{}",
            String::from_utf8_lossy(&rendered.stderr)
        );
    }
    let png_dimensions = |path: &std::path::Path| {
        let data = std::fs::read(path).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let reader = decoder.read_info().unwrap();
        (reader.info().width, reader.info().height)
    };
    assert_eq!(
        png_dimensions(&actual_no_legend_path),
        png_dimensions(&expected_no_legend_path)
    );

    let ours_dir = temp.path().join("imginfo-ours");
    let upstream_dir = temp.path().join("imginfo-upstream");
    std::fs::create_dir_all(&ours_dir).unwrap();
    std::fs::create_dir_all(&upstream_dir).unwrap();
    let imginfo_format = "%%image:%s:%lu:%lu";
    let render_imginfo = |program: &std::path::Path, path: &std::path::Path| {
        Command::new(program)
            .args(["graph", path.to_str().unwrap(), "--imgformat=PNG"])
            .args([
                "--width",
                "200",
                "--height",
                "140",
                "--start",
                "1000000010",
                "--end",
                "1000000050",
            ])
            .args(["--imginfo", imginfo_format])
            .arg(&def)
            .arg("LINE1:x#ff0000:load")
            .output()
            .unwrap()
    };
    let ours_imginfo = render_imginfo(alias.as_path(), &ours_dir.join("same.png"));
    let upstream_imginfo = render_imginfo(
        std::path::Path::new("rrdtool"),
        &upstream_dir.join("same.png"),
    );
    assert!(
        ours_imginfo.status.success(),
        "{}",
        String::from_utf8_lossy(&ours_imginfo.stderr)
    );
    assert!(
        upstream_imginfo.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream_imginfo.stderr)
    );
    assert_eq!(ours_imginfo.stdout, upstream_imginfo.stdout);
    let invalid_imginfo = Command::new(alias.as_path())
        .args([
            "graph",
            temp.path().join("invalid-imginfo.png").to_str().unwrap(),
            "--imgformat=PNG",
            "--imginfo",
            "%s %u %u",
        ])
        .arg(&def)
        .arg("LINE1:x#ff0000:load")
        .output()
        .unwrap();
    assert!(!invalid_imginfo.status.success());
}

#[test]
fn rrdtool_xport_local_calendar_rpn_operators_match_at_new_year_boundary() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool xport differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("calendar.rrd");
    let create = Command::new("rrdtool")
        .env("TZ", "UTC")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1704066300",
            "--step",
            "300",
            "DS:value:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:32",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    for timestamp in (1704066600..=1704067800).step_by(300) {
        let sample = format!("{timestamp}:1");
        let update = Command::new("rrdtool")
            .env("TZ", "UTC")
            .args(["update", file.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:value={}:value:AVERAGE", file.display());
    let args = vec![
        "xport",
        "--start",
        "1704066300",
        "--end",
        "1704067800",
        "--step",
        "300",
        "--json",
    ];
    let mut oracle = Command::new("rrdtool");
    oracle
        .env("TZ", "UTC")
        .args(&args)
        .arg(&def)
        .arg("CDEF:time=value,POP,0,TIME,+")
        .arg("CDEF:ltime=value,POP,0,LTIME,+")
        .arg("CDEF:newday=value,POP,0,NEWDAY,+")
        .arg("CDEF:newweek=value,POP,0,NEWWEEK,+")
        .arg("CDEF:newmonth=value,POP,0,NEWMONTH,+")
        .arg("CDEF:newyear=value,POP,0,NEWYEAR,+")
        .arg("XPORT:time:Time")
        .arg("XPORT:ltime:LocalTime")
        .arg("XPORT:newday:NewDay")
        .arg("XPORT:newweek:NewWeek")
        .arg("XPORT:newmonth:NewMonth")
        .arg("XPORT:newyear:NewYear");
    let expected = oracle.output().unwrap();
    let mut implementation = Command::new(&alias);
    implementation
        .env("TZ", "UTC")
        .args(&args)
        .arg(&def)
        .arg("CDEF:time=value,POP,0,TIME,+")
        .arg("CDEF:ltime=value,POP,0,LTIME,+")
        .arg("CDEF:newday=value,POP,0,NEWDAY,+")
        .arg("CDEF:newweek=value,POP,0,NEWWEEK,+")
        .arg("CDEF:newmonth=value,POP,0,NEWMONTH,+")
        .arg("CDEF:newyear=value,POP,0,NEWYEAR,+")
        .arg("XPORT:time:Time")
        .arg("XPORT:ltime:LocalTime")
        .arg("XPORT:newday:NewDay")
        .arg("XPORT:newweek:NewWeek")
        .arg("XPORT:newmonth:NewMonth")
        .arg("XPORT:newyear:NewYear");
    let actual = implementation.output().unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rpn_newweek_matches_locale_first_weekday_from_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping NEWWEEK locale differential: rrdtool is not installed");
        return;
    }
    // German weeks start on Monday, unlike the C locale. Without a locale whose
    // first weekday differs, this comparison cannot detect a fixed weekday.
    let locale = "de_DE.UTF-8";
    let installed = Command::new("locale")
        .arg("-a")
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|name| name.to_ascii_lowercase().replace('-', "") == "de_de.utf8")
        });
    if !installed {
        oracle_skip!("skipping NEWWEEK locale differential: locale {locale} is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("newweek.rrd");
    let start = 1_704_067_200_i64; // 2024-01-01 00:00:00 UTC
    let step = 86_400_i64;
    let end = start + step * 10;
    let start_text = start.to_string();
    let step_text = step.to_string();
    let end_text = end.to_string();
    let created = Command::new("rrdtool")
        .env("TZ", "UTC")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            &start_text,
            "--step",
            &step_text,
            "DS:value:GAUGE:172800:U:U",
            "RRA:AVERAGE:0.5:1:12",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let samples = (1..=10)
        .map(|day| format!("{}:{day}", start + step * day))
        .collect::<Vec<_>>();
    let updated = Command::new("rrdtool")
        .env("TZ", "UTC")
        .arg("update")
        .arg(&file)
        .args(&samples)
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let definition = format!("DEF:value={}:value:AVERAGE", file.display());
    let args = [
        "xport",
        "--start",
        &start_text,
        "--end",
        &end_text,
        "--step",
        &step_text,
        "--json",
    ];
    let expected = Command::new("rrdtool")
        .env("TZ", "UTC")
        .env_remove("LC_ALL")
        .env("LC_TIME", locale)
        .args(args)
        .arg(&definition)
        .arg("CDEF:newweek=value,POP,0,NEWWEEK,+")
        .arg("XPORT:newweek:NewWeek")
        .output()
        .unwrap();
    let actual = Command::new(alias)
        .env("TZ", "UTC")
        .env_remove("LC_ALL")
        .env("LC_TIME", locale)
        .args(args)
        .arg(&definition)
        .arg("CDEF:newweek=value,POP,0,NEWWEEK,+")
        .arg("XPORT:newweek:NewWeek")
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rrdtool_xport_ltime_matches_across_daylight_saving_transition() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool xport differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("dst.rrd");
    let create = Command::new("rrdtool")
        .env("TZ", "America/Los_Angeles")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1710064200",
            "--step",
            "300",
            "DS:value:GAUGE:600:U:U",
            "RRA:AVERAGE:0.5:1:16",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    for timestamp in (1710064500..=1710066000).step_by(300) {
        let sample = format!("{timestamp}:1");
        let update = Command::new("rrdtool")
            .env("TZ", "America/Los_Angeles")
            .args(["update", file.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let def = format!("DEF:value={}:value:AVERAGE", file.display());
    let args = [
        "xport",
        "--start",
        "1710064200",
        "--end",
        "1710066000",
        "--step",
        "300",
        "--json",
    ];
    let expressions = [
        "CDEF:local=value,POP,0,LTIME,+",
        "CDEF:day=value,POP,0,NEWDAY,+",
    ];
    let exports = ["XPORT:local:LocalTime", "XPORT:day:NewDay"];
    let expected = Command::new("rrdtool")
        .env("TZ", "America/Los_Angeles")
        .args(args)
        .arg(&def)
        .args(expressions)
        .args(exports)
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .env("TZ", "America/Los_Angeles")
        .args(args)
        .arg(&def)
        .args(expressions)
        .args(exports)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rrdtool_last_lastupdate_and_first_aliases_match_pinned_output() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        oracle_skip!("skipping RRDtool CLI differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:b:COUNTER:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
            "RRA:MAX:0.5:2:3",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            file.to_str().unwrap(),
            "1000000010:3:100",
            "1000000020:4:150",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for args in [
        vec!["last"],
        vec!["last", file.to_str().unwrap()],
        vec!["lastupdate", file.to_str().unwrap()],
        vec!["first", file.to_str().unwrap()],
        vec!["first", file.to_str().unwrap(), "--rraindex", "1"],
        vec!["first", "--rraindex", "1", file.to_str().unwrap()],
        vec!["info", file.to_str().unwrap()],
        vec!["dump", file.to_str().unwrap()],
    ] {
        let expected = Command::new("rrdtool").args(&args).output().unwrap();
        let actual = Command::new(&alias).args(&args).output().unwrap();
        assert!(expected.status.success());
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            normalize_rrdtool_compiled_stamp(&actual.stdout),
            normalize_rrdtool_compiled_stamp(&expected.stdout),
            "output mismatch for {args:?}"
        );
    }
    let missing_filename = temp.path().join("missing.rrd");
    let missing_last_args = ["last", missing_filename.to_str().unwrap()];
    let expected = Command::new("rrdtool")
        .args(missing_last_args)
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(missing_last_args)
        .output()
        .unwrap();
    assert_eq!(actual.status, expected.status);
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);

    let short_time_file = temp.path().join("short-time.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            short_time_file.to_str().unwrap(),
            "--start",
            "01/01/1981",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let args = ["lastupdate", short_time_file.to_str().unwrap()];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    let actual = Command::new(&alias).args(args).output().unwrap();
    assert!(expected.status.success());
    assert!(actual.status.success());
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rrdtool_missing_file_diagnostics_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool missing-file differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let missing_file = temp.path().join("missing.rrd");
    let path = missing_file.to_str().unwrap();
    for arguments in [
        vec![
            "fetch",
            path,
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000100",
            "--resolution",
            "10",
        ],
        vec!["update", path, "1000000010:1"],
        vec!["first", path],
        vec!["lastupdate", path],
        vec!["info", path],
        vec!["dump", path],
        vec!["tune", path, "--heartbeat", "a:20"],
        vec!["resize", path, "0", "GROW", "1"],
    ] {
        let upstream = Command::new("rrdtool").args(&arguments).output().unwrap();
        let rondi = Command::new(&alias).args(&arguments).output().unwrap();
        assert_eq!(rondi.status, upstream.status, "{arguments:?}");
        assert_eq!(
            normalize_rrdtool_compiled_stamp(&rondi.stdout),
            normalize_rrdtool_compiled_stamp(&upstream.stdout),
            "{arguments:?}"
        );
        assert_eq!(rondi.stderr, upstream.stderr, "{arguments:?}");
    }

    let directory = temp.path().join("directory.rrd");
    std::fs::create_dir(&directory).unwrap();
    let empty_file = temp.path().join("empty.rrd");
    std::fs::write(&empty_file, []).unwrap();
    let short_file = temp.path().join("short.rrd");
    std::fs::write(&short_file, [0_u8; 127]).unwrap();
    let bad_cookie_file = temp.path().join("bad-cookie.rrd");
    std::fs::write(&bad_cookie_file, [0_u8; 128]).unwrap();
    for path in [&directory, &empty_file, &short_file, &bad_cookie_file] {
        let arguments = [
            "fetch",
            path.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000100",
            "--resolution",
            "10",
        ];
        let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
        let rondi = Command::new(&alias).args(arguments).output().unwrap();
        assert_eq!(rondi.status, upstream.status);
        assert_eq!(rondi.stdout, upstream.stdout);
        assert_eq!(rondi.stderr, upstream.stderr);
    }

    let unsupported_version = temp.path().join("unsupported-version.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            unsupported_version.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let mut bytes = std::fs::read(&unsupported_version).unwrap();
    bytes[4..8].copy_from_slice(b"9999");
    std::fs::write(&unsupported_version, bytes).unwrap();
    let arguments = [
        "fetch",
        unsupported_version.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000000",
        "--end",
        "1000000010",
        "--resolution",
        "10",
    ];
    let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
    let rondi = Command::new(&alias).args(arguments).output().unwrap();
    assert_eq!(rondi.status, upstream.status);
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_update_malformed_row_diagnostics_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update diagnostic differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("invalid-update.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    for sample in [
        "1000000010",
        "1000000010:1:2",
        "nonsense:1",
        "1000000010:abc",
        "1000000010:",
    ] {
        let upstream = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), sample])
            .output()
            .unwrap();
        let rondi = Command::new(&alias)
            .args(["update", file.to_str().unwrap(), sample])
            .output()
            .unwrap();
        assert_eq!(rondi.status.code(), upstream.status.code(), "{sample}");
        assert_eq!(rondi.stdout, upstream.stdout, "{sample}");
        assert_eq!(rondi.stderr, upstream.stderr, "{sample}");
    }
}

#[test]
fn rrdtool_update_missing_data_source_diagnostic_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update diagnostic differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("short-update.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:first:GAUGE:20:U:U",
            "DS:second:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let upstream = Command::new("rrdtool")
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args(["update", file.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_update_template_arity_diagnostics_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update-template differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("template-arity.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:first:GAUGE:20:U:U",
            "DS:second:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    for (template, sample) in [
        ("first:second", "1000000010:1"),
        ("first:second", "1000000010:1:2:3"),
        ("missing", "1000000010:1"),
    ] {
        let upstream = Command::new("rrdtool")
            .args([
                "update",
                file.to_str().unwrap(),
                "--template",
                template,
                sample,
            ])
            .output()
            .unwrap();
        let rondi = Command::new(&alias)
            .args([
                "update",
                file.to_str().unwrap(),
                "--template",
                template,
                sample,
            ])
            .output()
            .unwrap();
        assert_eq!(rondi.status.code(), upstream.status.code(), "{sample}");
        assert_eq!(rondi.stdout, upstream.stdout, "{sample}");
        assert_eq!(rondi.stderr, upstream.stderr, "{sample}");
    }
}

#[test]
fn rrdtool_update_duplicate_template_names_match_upstream_file_bytes() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool update-template differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream_file = temp.path().join("upstream-template.rrd");
    let rondi_file = temp.path().join("rondi-template.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:first:GAUGE:20:U:U",
            "DS:second:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    std::fs::copy(&upstream_file, &rondi_file).unwrap();
    let upstream = Command::new("rrdtool")
        .args([
            "update",
            upstream_file.to_str().unwrap(),
            "--template",
            "first:first",
            "1000000010:1:2",
        ])
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args([
            "update",
            rondi_file.to_str().unwrap(),
            "--template",
            "first:first",
            "1000000010:1:2",
        ])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    assert_eq!(
        std::fs::read(rondi_file).unwrap(),
        std::fs::read(upstream_file).unwrap()
    );
}

#[test]
fn rrdtool_create_template_copies_supported_structure_and_start_time() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create-template differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let template = temp.path().join("template.rrd");
    let upstream_file = temp.path().join("upstream-created.rrd");
    let rondi_file = temp.path().join("rondi-created.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            template.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
            "RRA:MAX:0.25:2:3",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let upstream = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--template",
            template.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args([
            "create",
            rondi_file.to_str().unwrap(),
            "--template",
            template.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());

    let expected = rondi::inspect_rrd_file(upstream_file).unwrap();
    let actual = rondi::inspect_rrd_file(&rondi_file).unwrap();
    assert_eq!(actual.version, expected.version);
    assert_eq!(actual.step, 10);
    assert_eq!(actual.last_update, 1_000_000_000);
    assert_eq!(actual.last_update, expected.last_update);
    assert_eq!(actual.data_sources.len(), 1);
    assert_eq!(actual.data_sources[0].name, expected.data_sources[0].name);
    assert_eq!(actual.data_sources[0].kind, expected.data_sources[0].kind);
    assert_eq!(
        actual.data_sources[0].heartbeat,
        expected.data_sources[0].heartbeat
    );
    assert_eq!(actual.archives.len(), 2);
    for (actual_archive, expected_archive) in actual.archives.iter().zip(&expected.archives) {
        assert_eq!(actual_archive.consolidation, expected_archive.consolidation);
        assert_eq!(actual_archive.xff, expected_archive.xff);
        assert_eq!(actual_archive.pdp_per_row, expected_archive.pdp_per_row);
        assert_eq!(actual_archive.rows, expected_archive.rows);
    }
    let readable_by_upstream = Command::new("rrdtool")
        .args(["last", rondi_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(readable_by_upstream.status.success());
    assert_eq!(
        String::from_utf8_lossy(&readable_by_upstream.stdout).trim(),
        "1000000000"
    );

    let upstream_extended_file = temp.path().join("upstream-extended.rrd");
    let rondi_extended_file = temp.path().join("rondi-extended.rrd");
    let extended_arguments = |path: &std::path::Path| {
        vec![
            "create".to_owned(),
            path.to_string_lossy().into_owned(),
            "--template".to_owned(),
            template.to_string_lossy().into_owned(),
            "--start".to_owned(),
            "1000000100".to_owned(),
            "--step".to_owned(),
            "20".to_owned(),
            "DS:extra:GAUGE:30:U:U".to_owned(),
            "RRA:MIN:0.5:1:6".to_owned(),
        ]
    };
    let upstream = Command::new("rrdtool")
        .args(extended_arguments(&upstream_extended_file))
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args(extended_arguments(&rondi_extended_file))
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    let extended_expected = rondi::inspect_rrd_file(upstream_extended_file).unwrap();
    let extended_actual = rondi::inspect_rrd_file(&rondi_extended_file).unwrap();
    assert_eq!(extended_actual.step, 20);
    assert_eq!(extended_actual.last_update, 1_000_000_100);
    assert_eq!(extended_actual.last_update, extended_expected.last_update);
    assert_eq!(
        extended_actual
            .data_sources
            .iter()
            .map(|source| source.name.as_str())
            .collect::<Vec<_>>(),
        ["value", "extra"]
    );
    assert_eq!(
        extended_actual.data_sources.len(),
        extended_expected.data_sources.len()
    );
    assert_eq!(extended_actual.archives.len(), 3);
    assert_eq!(
        extended_actual.archives.len(),
        extended_expected.archives.len()
    );
}

#[test]
fn rrdtool_create_template_failure_diagnostics_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!(
            "skipping RRDtool create-template diagnostic differential: rrdtool is not installed"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let output = temp.path().join("output.rrd");
    let missing_template = temp.path().join("missing-template.rrd");
    let upstream = Command::new("rrdtool")
        .args([
            "create",
            output.to_str().unwrap(),
            "--template",
            missing_template.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args([
            "create",
            output.to_str().unwrap(),
            "--template",
            missing_template.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);

    let template = temp.path().join("template-for-duplicate.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            template.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let arguments = [
        "create",
        output.to_str().unwrap(),
        "--template",
        template.to_str().unwrap(),
        "DS:value:GAUGE:20:U:U",
    ];
    let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
    let rondi = Command::new(&alias).args(arguments).output().unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdtool_create_source_prefills_matching_schema_and_remains_interoperable() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create-source differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let source = temp.path().join("source.rrd");
    let upstream_file = temp.path().join("upstream-prefilled.rrd");
    let rondi_file = temp.path().join("rondi-prefilled.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            source.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.5:2:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let updated = Command::new("rrdtool")
        .args([
            "update",
            source.to_str().unwrap(),
            "1000000010:1",
            "1000000020:2",
        ])
        .output()
        .unwrap();
    assert!(updated.status.success());
    let source_bytes = std::fs::read(&source).unwrap();
    let create_args = |target: &std::path::Path| {
        vec![
            "create".to_owned(),
            target.to_string_lossy().into_owned(),
            "--source".to_owned(),
            source.to_string_lossy().into_owned(),
            "--step".to_owned(),
            "10".to_owned(),
            "DS:value:GAUGE:20:U:U".to_owned(),
            "RRA:AVERAGE:0.5:1:8".to_owned(),
            "RRA:MAX:0.5:2:4".to_owned(),
        ]
    };
    let upstream = Command::new("rrdtool")
        .args(create_args(&upstream_file))
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args(create_args(&rondi_file))
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    assert_eq!(std::fs::read(source).unwrap(), source_bytes);

    let source_info = rondi::inspect_rrd_file(&upstream_file).unwrap();
    let actual_info = rondi::inspect_rrd_file(&rondi_file).unwrap();
    assert_eq!(actual_info.step, source_info.step);
    assert_eq!(actual_info.last_update, source_info.last_update);
    assert_eq!(
        actual_info.data_sources[0].name,
        source_info.data_sources[0].name
    );
    assert_eq!(actual_info.archives.len(), source_info.archives.len());
    let upstream_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            upstream_file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    let rondi_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            rondi_file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert!(upstream_fetch.status.success());
    assert!(rondi_fetch.status.success());
    assert_eq!(rondi_fetch.stdout, upstream_fetch.stdout);

    let empty_source = temp.path().join("empty-source.rrd");
    let empty_upstream = temp.path().join("empty-upstream.rrd");
    let empty_rondi = temp.path().join("empty-rondi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            empty_source.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let create_args = |target: &std::path::Path| {
        vec![
            "create".to_owned(),
            target.to_string_lossy().into_owned(),
            "--source".to_owned(),
            empty_source.to_string_lossy().into_owned(),
            "--step".to_owned(),
            "20".to_owned(),
            "DS:value:GAUGE:20:U:U".to_owned(),
            "RRA:AVERAGE:0.5:1:8".to_owned(),
        ]
    };
    let upstream = Command::new("rrdtool")
        .args(create_args(&empty_upstream))
        .output()
        .unwrap();
    let rondi = Command::new(&alias)
        .args(create_args(&empty_rondi))
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert!(upstream.status.success());
    assert_eq!(rondi::inspect_rrd_file(&empty_rondi).unwrap().step, 20);
    let fetch_args = |file: &std::path::Path| {
        vec![
            "fetch".to_owned(),
            file.to_string_lossy().into_owned(),
            "AVERAGE".to_owned(),
            "--start".to_owned(),
            "1000000000".to_owned(),
            "--end".to_owned(),
            "1000000160".to_owned(),
            "--resolution".to_owned(),
            "20".to_owned(),
        ]
    };
    let upstream_fetch = Command::new("rrdtool")
        .args(fetch_args(&empty_upstream))
        .output()
        .unwrap();
    let rondi_fetch = Command::new("rrdtool")
        .args(fetch_args(&empty_rondi))
        .output()
        .unwrap();
    assert!(upstream_fetch.status.success());
    assert!(rondi_fetch.status.success());
    assert_eq!(rondi_fetch.stdout, upstream_fetch.stdout);
    for file in [&empty_upstream, &empty_rondi] {
        let updated = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), "1000000020:5"])
            .output()
            .unwrap();
        assert!(updated.status.success());
    }
    let upstream_fetch = Command::new("rrdtool")
        .args(fetch_args(&empty_upstream))
        .output()
        .unwrap();
    let rondi_fetch = Command::new("rrdtool")
        .args(fetch_args(&empty_rondi))
        .output()
        .unwrap();
    assert!(upstream_fetch.status.success());
    assert!(rondi_fetch.status.success());
    assert_eq!(rondi_fetch.stdout, upstream_fetch.stdout);
}

#[test]
fn rrdtool_create_source_failure_diagnostics_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create-source diagnostics: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let source = temp.path().join("source.rrd");
    let target = temp.path().join("target.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            source.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    std::fs::write(&target, b"existing target").unwrap();

    let missing = temp.path().join("missing.rrd");
    let corrupt_source = temp.path().join("corrupt.rrd");
    std::fs::write(&corrupt_source, b"not an RRD file").unwrap();
    let source_directory = temp.path().join("source-directory");
    std::fs::create_dir(&source_directory).unwrap();
    for source_name in [
        missing.to_str().unwrap(),
        corrupt_source.to_str().unwrap(),
        source_directory.to_str().unwrap(),
        source.to_str().unwrap(),
    ] {
        let arguments = [
            "create",
            target.to_str().unwrap(),
            "--source",
            source_name,
            "--no-overwrite",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ];
        let upstream = Command::new("rrdtool").args(arguments).output().unwrap();
        let rondi = Command::new(&alias).args(arguments).output().unwrap();
        assert_eq!(rondi.status.code(), upstream.status.code());
        assert_eq!(rondi.stdout, upstream.stdout);
        assert_eq!(rondi.stderr, upstream.stderr);
    }
}

#[test]
fn rrdtool_create_missing_archive_and_data_source_errors_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool create diagnostics differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let cases = [
        vec!["create"],
        vec!["create", "missing-rra.rrd", "DS:value:GAUGE:20:U:U"],
        vec!["create", "missing-ds.rrd", "RRA:AVERAGE:0.5:1:8"],
    ];
    for arguments in cases {
        let upstream = Command::new("rrdtool")
            .current_dir(temp.path())
            .args(&arguments)
            .output()
            .unwrap();
        let rondi = Command::new(&alias)
            .current_dir(temp.path())
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(rondi.status, upstream.status, "{arguments:?}");
        assert_eq!(
            normalize_rrdtool_compiled_stamp(&rondi.stdout),
            normalize_rrdtool_compiled_stamp(&upstream.stdout),
            "{arguments:?}"
        );
        assert_eq!(rondi.stderr, upstream.stderr, "{arguments:?}");
    }
}

#[test]
fn rrdtool_supported_command_no_argument_help_matches_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool help differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for command in [
        "create",
        "dump",
        "fetch",
        "first",
        "graph",
        "graphv",
        "info",
        "last",
        "lastupdate",
        "list",
        "resize",
        "restore",
        "tune",
        "update",
        "updatev",
        "xport",
    ] {
        let upstream = Command::new("rrdtool").arg(command).output().unwrap();
        let rondi = Command::new(&alias).arg(command).output().unwrap();
        assert_eq!(rondi.status, upstream.status, "{command}");
        assert_eq!(
            normalize_rrdtool_compiled_stamp(&rondi.stdout),
            normalize_rrdtool_compiled_stamp(&upstream.stdout),
            "{command}"
        );
        assert_eq!(rondi.stderr, upstream.stderr, "{command}");
    }
}

#[test]
fn rrdtool_list_alias_matches_directory_and_recursive_output() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        oracle_skip!("skipping RRDtool CLI differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rrds");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    for file in [root.join("a.rrd"), root.join("nested/b.rrd")] {
        let created = Command::new("rrdtool")
            .args([
                "create",
                file.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:x:GAUGE:20:U:U",
                "RRA:AVERAGE:0.5:1:4",
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
    }
    std::fs::write(root.join("notes.txt"), "skip").unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let linked_rrd = outside.join("through-link.rrd");
    let linked_created = Command::new("rrdtool")
        .args([
            "create",
            linked_rrd.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:4",
        ])
        .output()
        .unwrap();
    assert!(linked_created.status.success());
    symlink(&outside, root.join("linked")).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let rrd_pattern = root.join("*.rrd").to_string_lossy().into_owned();
    let all_pattern = root.join("*").to_string_lossy().into_owned();
    for args in [
        vec!["list", root.to_str().unwrap()],
        vec!["list", "--recursive", root.to_str().unwrap()],
        vec!["list", rrd_pattern.as_str()],
        vec!["list", all_pattern.as_str()],
    ] {
        let expected = Command::new("rrdtool").args(&args).output().unwrap();
        let actual = Command::new(&alias).args(&args).output().unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.stdout, expected.stdout, "list mismatch for {args:?}");
        if args.get(1) == Some(&"--recursive") {
            assert!(
                String::from_utf8_lossy(&expected.stdout)
                    .lines()
                    .any(|entry| entry == "linked/through-link.rrd")
            );
        }
    }

    let args = ["list", "--recursive", rrd_pattern.as_str()];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    let actual = Command::new(&alias).args(args).output().unwrap();
    assert_eq!(actual.status.success(), expected.status.success());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);

    let missing_pattern = root.join("missing-*.rrd").to_string_lossy().into_owned();
    let args = ["list", missing_pattern.as_str()];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    let actual = Command::new(&alias).args(args).output().unwrap();
    assert_eq!(actual.status.success(), expected.status.success());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
}

#[test]
fn rrdtool_dump_header_options_and_output_file_match_upstream() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        oracle_skip!("skipping RRDtool dump differential: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for header in ["none", "dtd", "xsd"] {
        let upstream = Command::new("rrdtool")
            .args(["dump", "--header", header, file.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(upstream.status.success());
        let replacement = Command::new(&alias)
            .args(["dump", "--header", header, file.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(
            replacement.status.success(),
            "{}",
            String::from_utf8_lossy(&replacement.stderr)
        );
        assert_eq!(replacement.stdout, upstream.stdout, "header={header}");
    }
    let output_path = temp.path().join("dump.xml");
    let upstream = Command::new("rrdtool")
        .args([
            "dump",
            file.to_str().unwrap(),
            upstream_path(temp.path()).to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(upstream.status.success());
    let replacement = Command::new(&alias)
        .args([
            "dump",
            file.to_str().unwrap(),
            output_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(replacement.status.success());
    assert_eq!(
        std::fs::read(output_path).unwrap(),
        std::fs::read(upstream_path(temp.path())).unwrap()
    );
}

#[test]
fn rrdtool_restore_dump_round_trip_preserves_rows_and_update_state() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool restore differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original.rrd");
    let restored = temp.path().join("restored.rrd");
    let xml = temp.path().join("snapshot.xml");
    let rondi_xml = temp.path().join("rondi-snapshot.xml");
    let upstream_restored = temp.path().join("upstream-restored.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            original.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:in:GAUGE:30:U:U",
            "DS:out:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:AVERAGE:0.5:2:8",
            "RRA:MAX:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            original.to_str().unwrap(),
            "1000000010:1:2",
            "1000000020:3:4",
            "1000000030:5:6",
            "1000000040:7:8",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let dumped = Command::new("rrdtool")
        .args(["dump", original.to_str().unwrap(), xml.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        dumped.status.success(),
        "{}",
        String::from_utf8_lossy(&dumped.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let restored_output = Command::new(&alias)
        .args(["restore", xml.to_str().unwrap(), restored.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        restored_output.status.success(),
        "{}",
        String::from_utf8_lossy(&restored_output.stderr)
    );
    let restored_bytes_before = std::fs::read(&restored).unwrap();
    let refused_overwrite = Command::new(&alias)
        .args(["restore", xml.to_str().unwrap(), restored.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!refused_overwrite.status.success());
    assert_eq!(std::fs::read(&restored).unwrap(), restored_bytes_before);
    let forced_overwrite = Command::new(&alias)
        .args([
            "restore",
            "--force-overwrite",
            xml.to_str().unwrap(),
            restored.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        forced_overwrite.status.success(),
        "{}",
        String::from_utf8_lossy(&forced_overwrite.stderr)
    );

    let dumped_by_rondi = Command::new(&alias)
        .args([
            "dump",
            original.to_str().unwrap(),
            rondi_xml.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        dumped_by_rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&dumped_by_rondi.stderr)
    );
    let restored_by_upstream = Command::new("rrdtool")
        .args([
            "restore",
            rondi_xml.to_str().unwrap(),
            upstream_restored.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        restored_by_upstream.status.success(),
        "{}",
        String::from_utf8_lossy(&restored_by_upstream.stderr)
    );
    let fetch_args = [
        "fetch",
        upstream_restored.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000000",
        "--end",
        "1000000040",
        "--resolution",
        "10",
    ];
    let expected = Command::new("rrdtool")
        .args([
            "fetch",
            original.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    let actual = Command::new("rrdtool").args(fetch_args).output().unwrap();
    assert_eq!(actual.stdout, expected.stdout);

    let ranged_xml = temp.path().join("range-checked.xml");
    let rondi_ranged = temp.path().join("rondi-range-checked.rrd");
    let upstream_ranged = temp.path().join("upstream-range-checked.rrd");
    let xml_text = std::fs::read_to_string(&xml)
        .unwrap()
        .replace("<max>NaN</max>", "<max>5</max>");
    std::fs::write(&ranged_xml, xml_text).unwrap();
    let args = [
        "restore",
        "--range-check",
        ranged_xml.to_str().unwrap(),
        rondi_ranged.to_str().unwrap(),
    ];
    let actual = Command::new(&alias).args(args).output().unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    let args = [
        "restore",
        "--range-check",
        ranged_xml.to_str().unwrap(),
        upstream_ranged.to_str().unwrap(),
    ];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let args = [
        "fetch",
        rondi_ranged.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000000",
        "--end",
        "1000000040",
        "--resolution",
        "10",
    ];
    let actual = Command::new("rrdtool").args(args).output().unwrap();
    let args = [
        "fetch",
        upstream_ranged.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000000",
        "--end",
        "1000000040",
        "--resolution",
        "10",
    ];
    let expected = Command::new("rrdtool").args(args).output().unwrap();
    assert_eq!(actual.stdout, expected.stdout);

    for (cf, start, end, resolution) in [
        ("AVERAGE", "1000000000", "1000000040", "10"),
        ("AVERAGE", "1000000000", "1000000040", "20"),
        ("MAX", "1000000000", "1000000040", "10"),
    ] {
        let args = [
            "fetch",
            original.to_str().unwrap(),
            cf,
            "--start",
            start,
            "--end",
            end,
            "--resolution",
            resolution,
        ];
        let expected = Command::new("rrdtool").args(args).output().unwrap();
        let args = [
            "fetch",
            restored.to_str().unwrap(),
            cf,
            "--start",
            start,
            "--end",
            end,
            "--resolution",
            resolution,
        ];
        let actual = Command::new("rrdtool").args(args).output().unwrap();
        assert!(expected.status.success());
        assert!(actual.status.success());
        assert_eq!(
            actual.stdout, expected.stdout,
            "restored fetch mismatch for {cf}/{resolution}"
        );
    }

    for file in [&original, &restored] {
        let output = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), "1000000050:9:10"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let fetch_args = [
        "fetch",
        original.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000010",
        "--end",
        "1000000050",
        "--resolution",
        "10",
    ];
    let expected = Command::new("rrdtool").args(fetch_args).output().unwrap();
    let fetch_args = [
        "fetch",
        restored.to_str().unwrap(),
        "AVERAGE",
        "--start",
        "1000000010",
        "--end",
        "1000000050",
        "--resolution",
        "10",
    ];
    let actual = Command::new("rrdtool").args(fetch_args).output().unwrap();
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rrdtool_restore_preserves_version_five_double_counter_state() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool v5 restore differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("double.rrd");
    let restored = temp.path().join("double-restored.rrd");
    let xml = temp.path().join("double.xml");
    let created = Command::new("rrdtool")
        .args([
            "create",
            original.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:counter:DCOUNTER:30:U:U",
            "DS:derive:DDERIVE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let updated = Command::new("rrdtool")
        .args([
            "update",
            original.to_str().unwrap(),
            "1000000010:100.25:-5.5",
            "1000000020:101.75:-2.25",
            "1000000030:105.125:1.125",
        ])
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let dumped = Command::new("rrdtool")
        .args(["dump", original.to_str().unwrap(), xml.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        dumped.status.success(),
        "{}",
        String::from_utf8_lossy(&dumped.stderr)
    );

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let actual = Command::new(&alias)
        .args(["restore", xml.to_str().unwrap(), restored.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    let actual = Command::new("rrdtool")
        .args([
            "fetch",
            restored.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    let expected = Command::new("rrdtool")
        .args([
            "fetch",
            original.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert_eq!(actual.stdout, expected.stdout);
    for file in [&original, &restored] {
        let update = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), "1000000040:108.5:3.75"])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    let actual = Command::new("rrdtool")
        .args([
            "fetch",
            restored.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000010",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    let expected = Command::new("rrdtool")
        .args([
            "fetch",
            original.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000010",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert_eq!(actual.stdout, expected.stdout);
}

#[test]
fn rrdtool_tune_heartbeat_and_bounds_match_upstream_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool tune differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:in:GAUGE:30:U:U",
            "DS:out:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&upstream_file, &rondi_file).unwrap();

    let options = [
        "--heartbeat",
        "in:55",
        "--minimum",
        "in:0.5",
        "--maximum",
        "out:100",
    ];
    let expected = Command::new("rrdtool")
        .arg("tune")
        .arg(&upstream_file)
        .args(options)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let actual = Command::new(&alias)
        .arg("tune")
        .arg(&rondi_file)
        .args(options)
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );

    let expected = Command::new("rrdtool")
        .args(["tune", upstream_file.to_str().unwrap()])
        .output()
        .unwrap();
    let actual = Command::new(&alias)
        .args(["tune", rondi_file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(expected.status.success());
    assert!(actual.status.success());
    assert_eq!(actual.stdout, expected.stdout);

    for file in [&upstream_file, &rondi_file] {
        let update = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), "1000000010:1:25"])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );

    let options = [
        "--minimum",
        "in:U",
        "--maximum",
        "out:U",
        "--heartbeat",
        "out:60",
    ];
    let expected = Command::new("rrdtool")
        .arg("tune")
        .arg(&upstream_file)
        .args(options)
        .output()
        .unwrap();
    assert!(expected.status.success());
    let actual = Command::new(&alias)
        .arg("tune")
        .arg(&rondi_file)
        .args(options)
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );
}

#[test]
fn rrdtool_tune_data_source_type_resets_last_value_like_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool tune type differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let updated = Command::new("rrdtool")
        .args(["update", upstream_file.to_str().unwrap(), "1000000010:4"])
        .output()
        .unwrap();
    assert!(updated.status.success());
    std::fs::copy(&upstream_file, &rondi_file).unwrap();

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for kind in ["DERIVE", "DCOUNTER"] {
        let args = ["--data-source-type", &format!("x:{kind}")];
        let expected = Command::new("rrdtool")
            .arg("tune")
            .arg(&upstream_file)
            .args(args)
            .output()
            .unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        let actual = Command::new(&alias)
            .arg("tune")
            .arg(&rondi_file)
            .args(args)
            .output()
            .unwrap();
        assert!(
            actual.status.success(),
            "{}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(
            std::fs::read(&rondi_file).unwrap(),
            std::fs::read(&upstream_file).unwrap()
        );
        let info = Command::new("rrdtool")
            .args(["info", upstream_file.to_str().unwrap()])
            .output()
            .unwrap();
        let info = String::from_utf8(info.stdout).unwrap();
        assert!(info.contains(&format!("ds[x].type = \"{kind}\"")));
        assert!(info.contains("ds[x].last_ds = \"UNKN\""));

        let sample = match kind {
            "DERIVE" => "1000000020:8",
            _ => "1000000030:8.25",
        };
        for file in [&upstream_file, &rondi_file] {
            let update = Command::new("rrdtool")
                .args(["update", file.to_str().unwrap(), sample])
                .output()
                .unwrap();
            assert!(
                update.status.success(),
                "{}",
                String::from_utf8_lossy(&update.stderr)
            );
        }
        assert_eq!(
            std::fs::read(&rondi_file).unwrap(),
            std::fs::read(&upstream_file).unwrap()
        );
    }
}

#[test]
fn rrdtool_tune_data_source_rename_matches_upstream_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool tune rename differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_file = temp.path().join("upstream.rrd");
    let rondi_file = temp.path().join("rondi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:in:GAUGE:30:U:U",
            "DS:out:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    let updated = Command::new("rrdtool")
        .args(["update", upstream_file.to_str().unwrap(), "1000000010:4:8"])
        .output()
        .unwrap();
    assert!(updated.status.success());
    std::fs::copy(&upstream_file, &rondi_file).unwrap();

    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let args = ["--data-source-rename", "in:input"];
    let expected = Command::new("rrdtool")
        .arg("tune")
        .arg(&upstream_file)
        .args(args)
        .output()
        .unwrap();
    assert!(
        expected.status.success(),
        "{}",
        String::from_utf8_lossy(&expected.stderr)
    );
    let actual = Command::new(&alias)
        .arg("tune")
        .arg(&rondi_file)
        .args(args)
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );

    for file in [&upstream_file, &rondi_file] {
        let update = Command::new("rrdtool")
            .args(["update", file.to_str().unwrap(), "1000000020:5:9"])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );

    let options = [
        "--data-source-rename",
        "input:traffic",
        "--heartbeat",
        "traffic:45",
    ];
    let expected = Command::new("rrdtool")
        .arg("tune")
        .arg(&upstream_file)
        .args(options)
        .output()
        .unwrap();
    assert!(expected.status.success());
    let actual = Command::new(&alias)
        .arg("tune")
        .arg(&rondi_file)
        .args(options)
        .output()
        .unwrap();
    assert!(
        actual.status.success(),
        "{}",
        String::from_utf8_lossy(&actual.stderr)
    );
    assert_eq!(
        std::fs::read(&rondi_file).unwrap(),
        std::fs::read(&upstream_file).unwrap()
    );
}

#[test]
fn rrdtool_resize_grow_and_shrink_match_upstream_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool resize differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream_dir = temp.path().join("upstream");
    let rondi_dir = temp.path().join("rondi");
    std::fs::create_dir(&upstream_dir).unwrap();
    std::fs::create_dir(&rondi_dir).unwrap();
    let upstream_file = upstream_dir.join("sample.rrd");
    let rondi_file = rondi_dir.join("sample.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream_file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    for timestamp in (1_000_000_010..=1_000_000_130).step_by(10) {
        let update = format!("{timestamp}:{}", timestamp % 97);
        let result = Command::new("rrdtool")
            .args(["update", upstream_file.to_str().unwrap(), &update])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    std::fs::copy(&upstream_file, &rondi_file).unwrap();
    let original_bytes = std::fs::read(&upstream_file).unwrap();
    let alias = rondi_dir.join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    for action in ["GROW", "SHRINK"] {
        let upstream = Command::new("rrdtool")
            .current_dir(&upstream_dir)
            .args(["resize", "sample.rrd", "0", action, "2"])
            .output()
            .unwrap();
        assert!(
            upstream.status.success(),
            "{}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        let replacement = Command::new(&alias)
            .current_dir(&rondi_dir)
            .args(["resize", "sample.rrd", "0", action, "2"])
            .output()
            .unwrap();
        assert!(
            replacement.status.success(),
            "{}",
            String::from_utf8_lossy(&replacement.stderr)
        );
        assert_eq!(
            std::fs::read(upstream_dir.join("resize.rrd")).unwrap(),
            std::fs::read(rondi_dir.join("resize.rrd")).unwrap(),
            "{action} output differs byte-for-byte"
        );
        let second_resize = Command::new(&alias)
            .current_dir(&rondi_dir)
            .args(["resize", "sample.rrd", "0", action, "2"])
            .output()
            .unwrap();
        assert!(
            !second_resize.status.success(),
            "resize replaced an existing output"
        );
        assert_eq!(std::fs::read(&upstream_file).unwrap(), original_bytes);
        assert_eq!(std::fs::read(&rondi_file).unwrap(), original_bytes);
        let upstream_fetch = Command::new("rrdtool")
            .args([
                "fetch",
                upstream_dir.join("resize.rrd").to_str().unwrap(),
                "AVERAGE",
                "--start",
                "1000000000",
                "--end",
                "1000000150",
                "--resolution",
                "10",
            ])
            .output()
            .unwrap();
        let rondi_fetch = Command::new("rrdtool")
            .args([
                "fetch",
                rondi_dir.join("resize.rrd").to_str().unwrap(),
                "AVERAGE",
                "--start",
                "1000000000",
                "--end",
                "1000000150",
                "--resolution",
                "10",
            ])
            .output()
            .unwrap();
        assert!(upstream_fetch.status.success());
        assert!(rondi_fetch.status.success());
        assert_eq!(upstream_fetch.stdout, rondi_fetch.stdout);
        std::fs::remove_file(upstream_dir.join("resize.rrd")).unwrap();
        std::fs::remove_file(rondi_dir.join("resize.rrd")).unwrap();
    }
}

#[test]
fn rrdtool_updatev_matches_upstream_output_and_file_changes() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        oracle_skip!("skipping RRDtool updatev differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let upstream = temp.path().join("upstream.rrd");
    let replacement = temp.path().join("rondi.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            upstream.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:AVERAGE:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&upstream, &replacement).unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    for (timestamp, value) in [
        (1_000_000_010, "5"),
        (1_000_000_025, "8"),
        (1_000_000_040, "U"),
    ] {
        let sample = format!("{timestamp}:{value}");
        let expected = Command::new("rrdtool")
            .args(["updatev", upstream.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        let actual = Command::new(&alias)
            .args(["updatev", replacement.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert_eq!(actual.status.success(), expected.status.success());
        let actual_file = std::fs::read(&replacement).unwrap();
        let expected_file = std::fs::read(&upstream).unwrap();
        let differences = actual_file
            .iter()
            .zip(&expected_file)
            .enumerate()
            .filter_map(|(offset, (left, right))| (left != right).then_some(offset))
            .collect::<Vec<_>>();
        assert!(
            differences.is_empty(),
            "file mismatch after {sample}; first differing offsets: {:?}",
            &differences[..differences.len().min(24)]
        );
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
    }
}

fn upstream_path(directory: &std::path::Path) -> std::path::PathBuf {
    directory.join("upstream-dump.xml")
}

#[test]
fn rrdtool_update_converts_dcounter_text_only_after_a_known_sample() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!("skipping DCOUNTER text differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for kind in ["DCOUNTER", "DDERIVE"] {
        for value in ["abc", "+inf"] {
            let mut transcripts = Vec::new();
            for (name, program) in [("oracle", Path::new("rrdtool")), ("rondi", alias.as_path())] {
                let file = temp.path().join(format!("{name}-{kind}-{value}.rrd"));
                let created = Command::new("rrdtool")
                    .arg("create")
                    .arg(&file)
                    .args([
                        "--start",
                        "1000000000",
                        "--step",
                        "10",
                        &format!("DS:d:{kind}:100:U:U"),
                        "RRA:LAST:0:1:5",
                    ])
                    .output()
                    .unwrap();
                assert!(created.status.success());
                let mut transcript = Vec::new();
                for sample in [format!("1000000005:{value}"), "1000000015:3".to_owned()] {
                    let output = Command::new(program)
                        .arg("update")
                        .arg(&file)
                        .arg(&sample)
                        .output()
                        .unwrap();
                    let stderr = String::from_utf8_lossy(&output.stderr)
                        .replace(file.to_str().unwrap(), "FILE");
                    transcript.push((output.status.code(), stderr));
                }
                transcripts.push(transcript);
            }
            assert_eq!(transcripts[0], transcripts[1], "{kind} {value}");
        }
    }
}

/// rrd_fetch.c copies stored values unchanged, so a stored infinity is printed
/// as one; only NaN is unknown.
#[test]
fn stored_infinity_survives_fetch_and_xport() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping stored infinity differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let alias = temp.path().join("rrdtool");
    symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let file = temp.path().join("infinity.rrd");
    let file = file.to_str().unwrap();
    let run = |program: &Path, args: &[&str]| {
        let output = Command::new(program).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{program:?} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let upstream = Path::new("rrdtool");
    run(
        upstream,
        &[
            "create",
            file,
            "--start",
            "1700000000",
            "--step",
            "10",
            "DS:x:GAUGE:20:U:U",
            "DS:y:GAUGE:20:U:U",
            "RRA:MAX:0.5:1:10",
            "RRA:MIN:0.5:1:10",
            "RRA:AVERAGE:0.5:1:10",
        ],
    );
    for (offset, x, y) in [
        (10, "inf", "-inf"),
        (20, "-inf", "1"),
        (30, "inf", "U"),
        (40, "2", "inf"),
    ] {
        run(
            upstream,
            &[
                "update",
                file,
                &format!("{}:{x}:{y}", 1_700_000_000 + offset),
            ],
        );
    }
    for cf in ["MAX", "MIN", "AVERAGE"] {
        let fetch = [
            "fetch",
            file,
            cf,
            "--start",
            "1700000000",
            "--end",
            "1700000040",
        ];
        assert_eq!(run(&alias, &fetch), run(upstream, &fetch), "fetch {cf}");
        let def_x = format!("DEF:x={file}:x:{cf}");
        let def_y = format!("DEF:y={file}:y:{cf}");
        let xport = [
            "xport",
            "--start",
            "1700000000",
            "--end",
            "1700000040",
            &def_x,
            &def_y,
            "XPORT:x",
            "XPORT:y",
        ];
        assert_eq!(run(&alias, &xport), run(upstream, &xport), "xport {cf}");
    }
}
