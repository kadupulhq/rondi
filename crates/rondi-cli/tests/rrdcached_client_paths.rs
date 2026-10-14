#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Output};

fn available() -> bool {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("rrdtool is unavailable for client-path differentials".into());
        return false;
    }
    common::require_oracle();
    true
}

fn run_client(
    program: &Path,
    directory: &Path,
    arguments: &[&str],
    address: &str,
    strip: Option<&str>,
) -> Output {
    let mut command = Command::new(program);
    command
        .current_dir(directory)
        .args(arguments)
        .args(["--daemon", address])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .env_remove("RRDCACHED_STRIPPATH");
    if let Some(strip) = strip {
        command.env("RRDCACHED_STRIPPATH", strip);
    }
    command.output().unwrap()
}

fn request_line(line: &str) -> String {
    let (command, remainder) = line.split_once(' ').unwrap_or((line, ""));
    format!("{} {remainder}", command.to_ascii_uppercase())
}

fn tcp_case(
    program: &Path,
    directory: &Path,
    arguments: &[&str],
    strip: Option<&str>,
) -> (String, Option<i32>, String, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(common::io_timeout())).unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        if !line.is_empty() {
            let reply: &[u8] = if line.to_ascii_uppercase().starts_with("FETCH ") {
                b"7 Success\nFlushVersion: 1\nStart: 1000000000\nEnd: 1000000010\nStep: 10\nDSCount: 1\nDSName: v\n1000000010: 1.0\n"
            } else {
                b"0 1000000000\n"
            };
            stream.write_all(reply).unwrap();
        }
        request_line(&line)
    });
    let output = run_client(program, directory, arguments, &address, strip);
    (
        server.join().unwrap(),
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr)
            .unwrap()
            .replace(&address, "<daemon>"),
    )
}

#[test]
fn remote_client_strips_paths_like_rrd_client_c() {
    if !available() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let alias = directory.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    // get_path uses a string prefix, not a filesystem-component boundary,
    // and removes every leading slash left after stripping that prefix.
    for (filename, strip) in [
        ("/client/data/a.rrd", Some("/client/data")),
        ("/client/data///a b.rrd", Some("/client/data/")),
        ("/client/database/a.rrd", Some("/client/data")),
        ("/client/data/a.rrd", Some("")),
        ("/client/data/a.rrd", Some("/client/data/a.rrd")),
        ("relative/a.rrd", Some("relative/")),
        ("relative/a.rrd", None),
        ("/client/data/a.rrd", None),
        ("/client/data/a.rrd", Some("/other")),
    ] {
        let upstream = tcp_case(Path::new("rrdtool"), &directory, &["last", filename], strip);
        let actual = tcp_case(&alias, &directory, &["last", filename], strip);
        assert_eq!(actual, upstream, "filename={filename:?}, strip={strip:?}");
    }
}

fn unix_case(
    program: &Path,
    directory: &Path,
    filename: &str,
) -> (String, Option<i32>, String, String) {
    let socket = directory.join("daemon.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(common::io_timeout())).unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream).read_line(&mut line).unwrap();
        if !line.is_empty() {
            stream.write_all(b"0 1000000000\n").unwrap();
        }
        request_line(&line)
    });
    let output = run_client(
        program,
        directory,
        &["last", filename],
        &format!("unix:{}", socket.display()),
        Some("/ignored/for/local/daemons"),
    );
    (
        server.join().unwrap(),
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn local_client_resolves_paths_like_rrd_client_c() {
    if !available() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    std::fs::create_dir(directory.join("data")).unwrap();
    std::fs::write(directory.join("data/a b.rrd"), b"").unwrap();
    std::os::unix::fs::symlink("data/a b.rrd", directory.join("link.rrd")).unwrap();
    let alias = directory.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for filename in [
        "data/a b.rrd",
        "link.rrd",
        "data/new.rrd",
        "new.rrd",
        "missing/new.rrd",
    ] {
        let upstream = unix_case(Path::new("rrdtool"), &directory, filename);
        let actual = unix_case(&alias, &directory, filename);
        assert_eq!(actual, upstream, "filename={filename:?}");
    }
}

#[test]
fn daemon_file_commands_share_upstream_path_translation() {
    if !available() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let alias = directory.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let filename = "/client/data/a b.rrd";
    for arguments in [
        vec![
            "create",
            filename,
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:v:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ],
        vec!["last", filename],
        vec!["first", filename, "--rraindex", "0"],
        vec!["update", filename, "1000000010:1"],
        vec!["flushcached", filename],
        vec![
            "fetch",
            filename,
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000010",
        ],
    ] {
        for strip in ["/client/data", "/other"] {
            let upstream = tcp_case(Path::new("rrdtool"), &directory, &arguments, Some(strip));
            let actual = tcp_case(&alias, &directory, &arguments, Some(strip));
            assert_eq!(actual, upstream, "arguments={arguments:?}, strip={strip:?}");
        }
    }
}
