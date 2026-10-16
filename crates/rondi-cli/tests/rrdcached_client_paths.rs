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
        // A client that fails realpath connects and hangs up at once, and
        // macOS then rejects setsockopt with EINVAL; the read still sees EOF.
        let _ = stream.set_read_timeout(Some(common::io_timeout()));
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

/// RRDtool `client_connect` treats anything without a `unix:` prefix
/// or leading slash as a network address, and `connect_network` reports
/// parse and resolver failures in its own words.
#[test]
fn daemon_address_errors_match_rrd_client_c() {
    if !available() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let alias = directory.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let long_socket = format!("unix:/{}", "a".repeat(200));
    let addresses = [
        "unix:".to_owned(),
        "unix:/nonexistent/rrdcached.sock".to_owned(),
        long_socket,
        "[::1".to_owned(),
        "[::1]x".to_owned(),
        format!("[::1]:{closed_port}"),
        format!("127.0.0.1:{closed_port}"),
        "127.0.0.1:".to_owned(),
        "127.0.0.1:not-a-port".to_owned(),
        format!("tcp:127.0.0.1:{closed_port}"),
    ];
    for address in &addresses {
        let upstream = run_client(
            Path::new("rrdtool"),
            &directory,
            &["update", "a.rrd", "1000000300:1"],
            address,
            None,
        );
        let actual = run_client(
            &alias,
            &directory,
            &["update", "a.rrd", "1000000300:1"],
            address,
            None,
        );
        assert_eq!(
            (
                actual.status.code(),
                &actual.stdout,
                String::from_utf8_lossy(&actual.stderr)
            ),
            (
                upstream.status.code(),
                &upstream.stdout,
                String::from_utf8_lossy(&upstream.stderr)
            ),
            "address={address:?}"
        );
    }
}

/// Runs `update --daemon address` while answering on `listener`. The
/// default port is shared machine-wide, so connections that do not carry
/// the expected request are dropped, and a client that never connects
/// ends the wait instead of blocking in accept.
fn default_port_case(
    listener: &std::net::TcpListener,
    program: &Path,
    directory: &Path,
    address: &str,
) -> (Option<String>, Option<i32>, String, String) {
    let mut child = Command::new(program)
        .current_dir(directory)
        .args(["update", "a.rrd", "1000000300:1", "--daemon", address])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env_remove("RRDCACHED_ADDRESS")
        .env_remove("RRDCACHED_STRIPPATH")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + common::io_timeout();
    let mut request = None;
    while request.is_none() && std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(common::io_timeout())).unwrap();
                let mut line = String::new();
                let _ = BufReader::new(&mut stream).read_line(&mut line);
                if line.starts_with("update ") || line.starts_with("UPDATE ") {
                    stream
                        .write_all(b"0 errors, enqueued 1 value(s).\n")
                        .unwrap();
                    request = Some(request_line(&line));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("accept on the default port failed: {error}"),
        }
    }
    let output = child.wait_with_output().unwrap();
    (
        request,
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// A host without a port connects to RRDCACHED_DEFAULT_PORT (42217). The
/// case is skipped when another process already listens there.
#[test]
fn bare_host_daemon_address_uses_the_default_port() {
    if !available() {
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let directory = std::fs::canonicalize(temporary.path()).unwrap();
    let alias = directory.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for (bind, address) in [("127.0.0.1:42217", "127.0.0.1"), ("[::1]:42217", "[::1]")] {
        let Ok(listener) = std::net::TcpListener::bind(bind) else {
            eprintln!("skipping --daemon {address}: cannot listen on {bind}");
            continue;
        };
        let upstream = default_port_case(&listener, Path::new("rrdtool"), &directory, address);
        let actual = default_port_case(&listener, &alias, &directory, address);
        // AI_ADDRCONFIG drops ::1 on hosts without a non-loopback IPv6
        // address, such as the CI containers, so only IPv4 must connect.
        if address == "127.0.0.1" {
            assert_eq!(
                upstream.0.as_deref(),
                Some("UPDATE a.rrd 1000000300:1\n"),
                "address={address:?}, upstream={upstream:?}"
            );
        }
        assert_eq!(actual, upstream, "address={address:?}");
    }
}
