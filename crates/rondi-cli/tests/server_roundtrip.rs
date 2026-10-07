#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn start_server(root: &std::path::Path, socket: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rondi"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "server",
            "--listen",
            socket.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_socket(child: &mut Child, socket: &std::path::Path) {
    for _ in 0..100 {
        if socket.exists() && UnixStream::connect(socket).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut output) = child.stderr.take() {
                let _ = output.read_to_string(&mut stderr);
            }
            panic!("server exited during startup: {status}; stderr: {stderr}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not create its Unix socket");
}

fn wait_for_socket_metadata(
    child: &mut Child,
    socket: &std::path::Path,
    expected_gid: u32,
    expected_mode: u32,
) {
    use std::os::unix::fs::MetadataExt;

    wait_for_socket(child, socket);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(metadata) = std::fs::metadata(socket)
            && metadata.gid() == expected_gid
            && metadata.mode() & 0o7777 == expected_mode
        {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("server exited before socket metadata was ready: {status}");
        }
        if Instant::now() >= deadline {
            panic!(
                "socket {} did not reach group {expected_gid} and mode {expected_mode:o}",
                socket.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn flushcached_client_connects_to_upstream_tcp_daemon() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
        || !Command::new("rrdtool")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping TCP rrdcached differential: upstream tools are not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let file = root.join("tcp.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
        ])
        .args(["DS:value:GAUGE:20:U:U", "RRA:AVERAGE:0.5:1:8"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let address = format!("127.0.0.1:{port}");
    let mut upstream = Command::new("rrdcached")
        .args([
            "-g",
            "-b",
            root.to_str().unwrap(),
            "-l",
            &format!("127.0.0.1:{port}"),
            "-w",
            "3600",
            "-f",
            "7200",
            "-p",
        ])
        .arg(dir.path().join("rrdcached.pid"))
        .args(["-j"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = (0..100).any(|_| {
        if upstream.try_wait().unwrap().is_some() {
            return false;
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
        false
    });
    if !ready {
        let status = upstream.try_wait().unwrap();
        let mut stderr = String::new();
        if let Some(mut pipe) = upstream.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        panic!("upstream rrdcached did not listen on TCP (status {status:?}): {stderr}");
    }

    let update = Command::new("rrdtool")
        .current_dir(&root)
        .args(["update", "tcp.rrd", "--daemon", &address, "1000000010:17"])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );

    let alias = dir.path().join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let rondi_create = Command::new(&alias)
        .current_dir(&root)
        .args([
            "create",
            "created-rondi.rrd",
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "--daemon",
            &address,
        ])
        .output()
        .unwrap();
    assert!(
        rondi_create.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi_create.stderr)
    );
    let upstream_create = Command::new("rrdtool")
        .current_dir(&root)
        .args([
            "create",
            "created-upstream.rrd",
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "--daemon",
            &address,
        ])
        .output()
        .unwrap();
    assert!(
        upstream_create.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream_create.stderr)
    );
    let fetch_created = |filename: &str| {
        let output = Command::new("rrdtool")
            .current_dir(&root)
            .args([
                "fetch",
                filename,
                "AVERAGE",
                "--start",
                "1000000000",
                "--end",
                "1000000020",
                "--resolution",
                "10",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    assert_eq!(
        fetch_created("created-rondi.rrd"),
        fetch_created("created-upstream.rrd")
    );
    let rondi_update = Command::new(&alias)
        .current_dir(&root)
        .args(["update", "tcp.rrd", "--daemon", &address, "1000000020:23"])
        .output()
        .unwrap();
    assert!(
        rondi_update.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi_update.stderr)
    );
    let daemon_fetch = Command::new(&alias)
        .current_dir(&root)
        .args([
            "fetch",
            "tcp.rrd",
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
            "--resolution",
            "10",
            "--daemon",
            &address,
        ])
        .output()
        .unwrap();
    assert!(
        daemon_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon_fetch.stderr)
    );
    let daemon_fetch = String::from_utf8_lossy(&daemon_fetch.stdout);
    assert!(daemon_fetch.contains("1.7000000000e+01"), "{daemon_fetch}");
    assert!(daemon_fetch.contains("2.3000000000e+01"), "{daemon_fetch}");
    for arguments in [
        vec!["last", "tcp.rrd", "--daemon", address.as_str()],
        vec!["first", "tcp.rrd", "--daemon", address.as_str()],
    ] {
        let upstream_query = Command::new("rrdtool")
            .current_dir(&root)
            .args(&arguments)
            .output()
            .unwrap();
        let rondi_query = Command::new(&alias)
            .current_dir(&root)
            .args(&arguments)
            .output()
            .unwrap();
        assert!(
            upstream_query.status.success(),
            "{}",
            String::from_utf8_lossy(&upstream_query.stderr)
        );
        assert!(
            rondi_query.status.success(),
            "{}",
            String::from_utf8_lossy(&rondi_query.stderr)
        );
        assert_eq!(rondi_query.stdout, upstream_query.stdout);
    }
    let flush = Command::new(alias)
        .current_dir(&root)
        .args(["flushcached", "tcp.rrd", "--daemon", &address])
        .output()
        .unwrap();
    assert!(
        flush.status.success(),
        "{}",
        String::from_utf8_lossy(&flush.stderr)
    );
    let fetched = Command::new("rrdtool")
        .args([
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert!(String::from_utf8_lossy(&fetched.stdout).contains("1.7000000000e+01"));
    assert!(String::from_utf8_lossy(&fetched.stdout).contains("2.3000000000e+01"));

    upstream.kill().unwrap();
    upstream.wait().unwrap();
}

#[test]
fn rrdcached_warns_when_flush_interval_is_less_than_twice_write_interval() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached warning differential: rrdcached is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let run = |name: &str, executable: &std::ffi::OsStr| {
        let root = dir.path().join(format!("{name}-root"));
        let socket = dir.path().join(format!("{name}.sock"));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        Command::new(executable)
            .args([
                "-b",
                root.to_str().unwrap(),
                "-l",
                &format!("unix:{}", socket.display()),
                "-w",
                "1",
                "-f",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
    };
    let read_warning = |child: &mut Child| {
        let mut reader = BufReader::new(child.stderr.as_mut().unwrap());
        let warning = loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(
                !line.is_empty(),
                "daemon exited before printing its warning"
            );
            if line.starts_with("WARNING: flush interval (-f)") {
                break line;
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        warning
    };
    let mut upstream = run("upstream", std::ffi::OsStr::new("rrdcached")).unwrap();
    let rondi_alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi_alias).unwrap();
    let mut rondi = run("rondi", rondi_alias.as_os_str()).unwrap();
    let upstream_warning = read_warning(&mut upstream);
    let rondi_warning = read_warning(&mut rondi);
    assert_eq!(rondi_warning, upstream_warning);
    assert_eq!(
        upstream_warning,
        "WARNING: flush interval (-f) should be at least 2x write interval (-w) !\n"
    );
}

#[test]
fn rrdcached_thread_count_validation_matches_upstream() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached thread-count differential: rrdcached is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream = Command::new("rrdcached")
        .args(["-t", "0"])
        .output()
        .unwrap();
    let rondi = Command::new(alias).args(["-t", "0"]).output().unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert_eq!(upstream.status.code(), Some(1));
    assert_eq!(upstream.stderr, b"Invalid thread count: -t 0\n");
}

#[test]
fn rrdcached_allocation_size_validation_matches_upstream() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached allocation-size differential: rrdcached is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream = Command::new("rrdcached")
        .args(["-a", "0"])
        .output()
        .unwrap();
    let rondi = Command::new(alias).args(["-a", "0"]).output().unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    assert_eq!(upstream.status.code(), Some(10));
    assert_eq!(upstream.stderr, b"Invalid allocation size: 0\n");
}

#[test]
fn rrdtool_flushcached_matches_upstream_for_multiple_rrds() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
        || !Command::new("rrdtool")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping flushcached differential: upstream RRDtool is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut results = Vec::new();
    let mut environment_results = Vec::new();
    let mut failure_results = Vec::new();
    let mut file_snapshots = Vec::new();
    let baseline_root = dir.path().join("baseline");
    std::fs::create_dir_all(&baseline_root).unwrap();
    let baseline_files = [
        baseline_root.join("one.rrd"),
        baseline_root.join("two.rrd"),
        baseline_root.join("space name.rrd"),
    ];
    for file in &baseline_files {
        let created = Command::new("rrdtool")
            .args([
                "create",
                file.to_str().unwrap(),
                "-b",
                "1000000000",
                "-s",
                "10",
                "DS:load:GAUGE:20:U:U",
                "RRA:AVERAGE:0.5:1:8",
            ])
            .output()
            .unwrap();
        assert!(created.status.success());
    }
    let shared_root = dir.path().join("shared-data");
    std::fs::create_dir_all(&shared_root).unwrap();
    let shared_root = std::fs::canonicalize(shared_root).unwrap();
    let shared_socket = dir.path().join("shared-run/rrdcached.sock");
    std::fs::create_dir_all(shared_socket.parent().unwrap()).unwrap();
    for name in ["upstream", "rondi"] {
        let run_dir = dir.path().join(name);
        std::fs::create_dir_all(&run_dir).unwrap();
        let root = shared_root.clone();
        let socket = shared_socket.clone();
        std::fs::create_dir_all(run_dir.join("journal")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let files = [
            root.join("one.rrd"),
            root.join("two.rrd"),
            root.join("space name.rrd"),
        ];
        for (baseline, file) in baseline_files.iter().zip(&files) {
            std::fs::copy(baseline, file).unwrap();
        }
        let is_rondi = name == "rondi";
        let cached = run_dir.join("rrdcached");
        if is_rondi {
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &cached).unwrap();
        }
        let cached_exe = if is_rondi {
            cached.as_os_str()
        } else {
            std::ffi::OsStr::new("rrdcached")
        };
        let mut daemon = Command::new(cached_exe)
            .args(["-g", "-B", "-b", root.to_str().unwrap(), "-l"])
            .arg(format!("unix:{}", socket.display()))
            .arg("-p")
            .arg(run_dir.join("rrdcached.pid"))
            .args(["-j"])
            .arg(run_dir.join("journal"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_socket(&mut daemon, &socket);
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        for (file, value) in files.iter().zip(["1", "2", "3"]) {
            let update = format!(
                "UPDATE {} 1000000010:{value}\n",
                file.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace(' ', "\\ ")
            );
            assert_eq!(
                rrdcached_request(&mut reader, &update),
                "0 errors, enqueued 1 value(s).\n"
            );
        }
        drop(reader);
        drop(stream);

        let tool = run_dir.join("rrdtool");
        if is_rondi {
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &tool).unwrap();
        }
        let tool_exe = if is_rondi {
            tool.as_os_str()
        } else {
            std::ffi::OsStr::new("rrdtool")
        };
        let flushed = Command::new(tool_exe)
            .arg("flushcached")
            .arg("--daemon")
            .arg(format!("unix:{}", socket.display()))
            .args(files.iter().map(|file| file.as_os_str()))
            .env("RRDCACHED_ADDRESS", "unix:/rondi-invalid-override.sock")
            .output()
            .unwrap();
        results.push((flushed.status.code(), flushed.stdout, flushed.stderr));
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        assert_eq!(
            rrdcached_request(&mut reader, "UPDATE one.rrd 1000000020:3\n"),
            "0 errors, enqueued 1 value(s).\n"
        );
        drop(reader);
        drop(stream);
        let flushed_from_environment = Command::new(tool_exe)
            .args(["flushcached", files[0].to_str().unwrap()])
            .env("RRDCACHED_ADDRESS", format!("unix:{}", socket.display()))
            .output()
            .unwrap();
        environment_results.push((
            flushed_from_environment.status.code(),
            flushed_from_environment.stdout,
            flushed_from_environment.stderr,
        ));
        let failed = Command::new(tool_exe)
            .arg("flushcached")
            .arg("--daemon")
            .arg(format!("unix:{}", socket.display()))
            .arg(&files[0])
            .arg(root.join("missing.rrd"))
            .arg(&files[1])
            .output()
            .unwrap();
        failure_results.push((failed.status.code(), failed.stdout, failed.stderr));
        file_snapshots.push(
            files
                .iter()
                .map(std::fs::read)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
        );
        unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) };
        daemon.wait().unwrap();
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[0], (Some(0), Vec::new(), Vec::new()));
    assert_eq!(environment_results[0], environment_results[1]);
    assert_eq!(environment_results[0], (Some(0), Vec::new(), Vec::new()));
    assert_eq!(failure_results[0], failure_results[1]);
    assert_eq!(failure_results[0].0, Some(1));
    assert!(String::from_utf8_lossy(&failure_results[0].2).contains("Skipping remaining 2 files."));
    assert_eq!(file_snapshots[0], file_snapshots[1]);
}

#[test]
fn rrdtool_flushcached_usage_and_missing_daemon_errors_match_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping flushcached CLI differential: upstream RRDtool is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    for args in [Vec::<&str>::new(), vec!["missing.rrd"]] {
        let upstream = Command::new("rrdtool")
            .arg("flushcached")
            .args(&args)
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        let rondi = Command::new(&alias)
            .arg("flushcached")
            .args(&args)
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        assert_eq!(
            rondi.status.code(),
            upstream.status.code(),
            "args={args:?}, upstream stderr={}, Rondi stderr={}",
            String::from_utf8_lossy(&upstream.stderr),
            String::from_utf8_lossy(&rondi.stderr)
        );
        let normalize_compiled = |output: &[u8]| {
            String::from_utf8_lossy(output)
                .lines()
                .map(|line| {
                    if line.starts_with("               Compiled ") {
                        "               Compiled ".to_owned()
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            normalize_compiled(&rondi.stdout),
            normalize_compiled(&upstream.stdout)
        );
        assert_eq!(rondi.stderr, upstream.stderr);
    }
}

#[test]
fn rrdtool_xport_daemon_flushes_pending_values_like_upstream() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping xport daemon differential: upstream RRDtool is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("metric.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:load:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());

    let daemon_alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &daemon_alias).unwrap();
    let socket = dir.path().join("rrdcached.sock");
    let journal = dir.path().join("journal");
    std::fs::create_dir_all(&journal).unwrap();
    let mut daemon = Command::new(&daemon_alias)
        .args(["-B", "-b"])
        .arg(&root)
        .args(["-l"])
        .arg(format!("unix:{}", socket.display()))
        .arg("-j")
        .arg(&journal)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut daemon, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE metric.rrd 1000000010:7\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    drop(reader);
    drop(stream);

    let rondi_alias = dir.path().join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi_alias).unwrap();
    for recursive in [false, true] {
        let mut arguments = vec!["list".to_owned(), "--daemon".to_owned()];
        arguments.push(format!("unix:{}", socket.display()));
        if recursive {
            arguments.push("--recursive".to_owned());
        }
        arguments.push("/".to_owned());
        let actual = Command::new(&rondi_alias)
            .args(&arguments)
            .output()
            .unwrap();
        let expected = Command::new("rrdtool").args(&arguments).output().unwrap();
        assert_eq!(actual.status.code(), expected.status.code());
        assert_eq!(actual.stdout, expected.stdout);
        assert_eq!(actual.stderr, expected.stderr);
    }
    let daemon_created = Command::new(&rondi_alias)
        .args([
            "create",
            "--daemon",
            &format!("unix:{}", socket.display()),
            "--start",
            "1000000000",
            "--step",
            "10",
            "daemon-created.rrd",
            "DS:load:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        daemon_created.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon_created.stderr)
    );
    let baseline = dir.path().join("baseline-created.rrd");
    let baseline_create = Command::new("rrdtool")
        .args([
            "create",
            baseline.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:load:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(baseline_create.status.success());
    let daemon_created_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            root.join("daemon-created.rrd").to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
        ])
        .output()
        .unwrap();
    let baseline_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            baseline.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
        ])
        .output()
        .unwrap();
    assert!(daemon_created_fetch.status.success());
    assert!(baseline_fetch.status.success());
    assert_eq!(daemon_created_fetch.stdout, baseline_fetch.stdout);
    let rondi_daemon_fetch = Command::new(&rondi_alias)
        .args([
            "fetch",
            root.join("daemon-created.rrd").to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
            "--daemon",
            &format!("unix:{}", socket.display()),
        ])
        .output()
        .unwrap();
    let upstream_daemon_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            root.join("daemon-created.rrd").to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
            "--daemon",
            &format!("unix:{}", socket.display()),
        ])
        .output()
        .unwrap();
    assert!(
        rondi_daemon_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi_daemon_fetch.stderr)
    );
    assert!(
        upstream_daemon_fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&upstream_daemon_fetch.stderr)
    );
    assert_eq!(
        rondi_daemon_fetch.stdout,
        upstream_daemon_fetch.stdout,
        "Rondi FETCH output: {:?}; upstream FETCH output: {:?}",
        String::from_utf8_lossy(&rondi_daemon_fetch.stdout),
        String::from_utf8_lossy(&upstream_daemon_fetch.stdout),
    );
    let daemon_first = Command::new(&rondi_alias)
        .args([
            "first",
            "--daemon",
            &format!("unix:{}", socket.display()),
            "daemon-created.rrd",
        ])
        .output()
        .unwrap();
    let baseline_first = Command::new("rrdtool")
        .args(["first", baseline.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(daemon_first.status.success());
    assert!(baseline_first.status.success());
    assert_eq!(daemon_first.stdout, baseline_first.stdout);
    let xport_args = [
        "xport",
        "--start",
        "1000000000",
        "--end",
        "1000000020",
        &format!("--daemon=unix:{}", socket.display()),
        &format!("DEF:load={}:load:AVERAGE", file.display()),
        "XPORT:load:load",
    ];
    let rondi = Command::new(&rondi_alias)
        .args(xport_args)
        .env("RRDCACHED_ADDRESS", "unix:/invalid-override.sock")
        .output()
        .unwrap();
    assert!(
        rondi.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi.stderr)
    );
    let upstream = Command::new("rrdtool")
        .args([
            "xport",
            "--start",
            "1000000000",
            "--end",
            "1000000020",
            &format!("DEF:load={}:load:AVERAGE", file.display()),
            "XPORT:load:load",
        ])
        .output()
        .unwrap();
    assert!(upstream.status.success());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
    let daemon_info = Command::new(&rondi_alias)
        .args([
            "info",
            "--daemon",
            &format!("unix:{}", socket.display()),
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let upstream_info = Command::new("rrdtool")
        .args(["info", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(daemon_info.status.success());
    assert!(upstream_info.status.success());
    assert_eq!(daemon_info.stdout, upstream_info.stdout);
    assert_eq!(daemon_info.stderr, upstream_info.stderr);
    let daemon_dump = Command::new(&rondi_alias)
        .args([
            "dump",
            "--daemon",
            &format!("unix:{}", socket.display()),
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let upstream_dump = Command::new("rrdtool")
        .args(["dump", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(daemon_dump.status.success());
    assert!(upstream_dump.status.success());
    assert_eq!(daemon_dump.stdout, upstream_dump.stdout);
    assert_eq!(daemon_dump.stderr, upstream_dump.stderr);
    let daemon_last = Command::new(&rondi_alias)
        .args([
            "last",
            "--daemon",
            &format!("unix:{}", socket.display()),
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let upstream_last = Command::new("rrdtool")
        .args(["last", file.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(daemon_last.status.success());
    assert!(upstream_last.status.success());
    assert_eq!(daemon_last.stdout, upstream_last.stdout);

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE metric.rrd 1000000020:9\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    drop(reader);
    drop(stream);
    let fetch = Command::new(&rondi_alias)
        .args([
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            &format!("--daemon=unix:{}", socket.display()),
        ])
        .env("RRDCACHED_ADDRESS", "unix:/invalid-override.sock")
        .output()
        .unwrap();
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let upstream_fetch = Command::new("rrdtool")
        .args([
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000030",
            "--daemon",
            &format!("unix:{}", socket.display()),
        ])
        .output()
        .unwrap();
    assert!(upstream_fetch.status.success());
    assert_eq!(fetch.stdout, upstream_fetch.stdout);
    assert_eq!(fetch.stderr, upstream_fetch.stderr);

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE metric.rrd 1000000030:11\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    drop(reader);
    drop(stream);
    let graph_args = [
        "graphv",
        "-",
        "--imgformat=JSON",
        "--start",
        "1000000010",
        "--end",
        "1000000030",
        "--daemon",
        &format!("unix:{}", socket.display()),
        &format!("DEF:load={}:load:AVERAGE", file.display()),
        "XPORT:load:load",
    ];
    let rondi_graph = Command::new(&rondi_alias)
        .args(graph_args)
        .output()
        .unwrap();
    let upstream_graph = Command::new("rrdtool")
        .args([
            "graphv",
            "-",
            "--imgformat=JSON",
            "--start",
            "1000000010",
            "--end",
            "1000000030",
            &format!("DEF:load={}:load:AVERAGE", file.display()),
            "XPORT:load:load",
        ])
        .output()
        .unwrap();
    assert!(
        rondi_graph.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi_graph.stderr)
    );
    assert!(upstream_graph.status.success());
    assert_eq!(rondi_graph.stdout, upstream_graph.stdout);
    assert_eq!(rondi_graph.stderr, upstream_graph.stderr);
    let tune_baseline = dir.path().join("tune-baseline.rrd");
    std::fs::copy(&file, &tune_baseline).unwrap();
    let rondi_tune = Command::new(&rondi_alias)
        .args([
            "tune",
            "--daemon",
            &format!("unix:{}", socket.display()),
            file.to_str().unwrap(),
            "--heartbeat",
            "load:40",
        ])
        .output()
        .unwrap();
    let upstream_tune = Command::new("rrdtool")
        .args([
            "tune",
            tune_baseline.to_str().unwrap(),
            "--heartbeat",
            "load:40",
        ])
        .output()
        .unwrap();
    assert!(
        rondi_tune.status.success(),
        "{}",
        String::from_utf8_lossy(&rondi_tune.stderr)
    );
    assert!(upstream_tune.status.success());
    assert_eq!(rondi_tune.stdout, upstream_tune.stdout);
    assert_eq!(rondi_tune.stderr, upstream_tune.stderr);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(tune_baseline).unwrap()
    );
    unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) };
    daemon.wait().unwrap();
}

#[test]
fn rrdcached_recursive_create_requires_and_honors_dash_r() {
    let dir = tempfile::tempdir().unwrap();
    for (name, recursive) in [("default", false), ("recursive", true)] {
        let run_dir = dir.path().join(name);
        std::fs::create_dir_all(&run_dir).unwrap();
        let root = run_dir.join("data");
        let socket = dir.path().join(format!("{name}.sock"));
        std::fs::create_dir_all(&root).unwrap();
        let alias = run_dir.join("rrdcached");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
        let mut command = Command::new(&alias);
        command.args(["-B", "-b", root.to_str().unwrap()]);
        if recursive {
            command.arg("-R");
        }
        let mut child = command
            .arg("-l")
            .arg(format!("unix:{}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_socket(&mut child, &socket);
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let response = rrdcached_request(
            &mut reader,
            "CREATE nested/path.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n",
        );
        if recursive {
            assert_eq!(response, "0 RRD created OK\n");
            assert!(root.join("nested/path.rrd").exists());
        } else {
            assert!(response.starts_with("-1 No permission to recursively create:"));
            assert!(!root.join("nested").exists());
        }
        drop(reader);
        drop(stream);
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn rrdcached_accepts_attached_arguments_and_cacti_options() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let socket = dir.path().join("cacti.sock");
    std::fs::create_dir_all(&root).unwrap();
    let alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    // SAFETY: geteuid and getegid have no preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let mut child = Command::new(&alias)
        .arg("-gB")
        .arg(format!("-b{}", root.display()))
        .args(["-w1800", "-f", "3600", "-z900", "-V", "LOG_INFO"])
        .arg(format!("-U{uid}"))
        .args(["-G", &gid.to_string()])
        .arg(format!("-lunix:{}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(rrdcached_request(&mut reader, "PING\n"), "0 PONG\n");
    drop(reader);
    drop(stream);
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert!(child.wait().unwrap().success());

    // Rondi can neither switch accounts nor serve several sockets, so it
    // refuses these configurations instead of silently ignoring them.
    let mut refused = vec![vec![
        "-l".to_owned(),
        format!("unix:{}", dir.path().join("first.sock").display()),
        "-l".to_owned(),
        format!("unix:{}", dir.path().join("second.sock").display()),
    ]];
    if uid != 0 {
        refused.push(vec!["-U".to_owned(), "0".to_owned()]);
    }
    for args in refused {
        let output = Command::new(&alias)
            .args(["-g", "-b", root.to_str().unwrap()])
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
        assert!(!dir.path().join("first.sock").exists());
    }
}

#[test]
fn rrdcached_socket_mode_matches_upstream() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached socket-mode differential: rrdcached is not installed");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut modes = Vec::new();
    for (name, executable) in [
        ("upstream", std::ffi::OsStr::new("rrdcached")),
        ("rondi", std::ffi::OsStr::new(env!("CARGO_BIN_EXE_rondi"))),
    ] {
        let run_dir = dir.path().join(name);
        let root = run_dir.join("data");
        let socket = run_dir.join("run/rrdcached.sock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let alias = run_dir.join("rrdcached");
        if name == "rondi" {
            std::os::unix::fs::symlink(executable, &alias).unwrap();
        }
        let mut child = Command::new(if name == "rondi" {
            alias.as_os_str()
        } else {
            executable
        })
        .args(["-g", "-B", "-b", root.to_str().unwrap(), "-m", "0660", "-l"])
        .arg(format!("unix:{}", socket.display()))
        .arg("-p")
        .arg(run_dir.join("rrdcached.pid"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        wait_for_socket(&mut child, &socket);
        let mode = (0..100)
            .find_map(|_| {
                let mode = std::fs::metadata(&socket).ok()?.permissions().mode() & 0o7777;
                if mode == 0o660 {
                    Some(mode)
                } else {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .unwrap_or_else(|| std::fs::metadata(&socket).unwrap().permissions().mode() & 0o7777);
        modes.push(mode);
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
    }
    assert_eq!(modes, [0o660, 0o660]);

    let invalid_dir = dir.path().join("invalid");
    std::fs::create_dir_all(&invalid_dir).unwrap();
    let alias = invalid_dir.join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let upstream = Command::new("rrdcached")
        .args(["-m", "0899", "-l", "unix:/tmp/rondi-invalid-mode.sock"])
        .output()
        .unwrap();
    let rondi = Command::new(alias)
        .args(["-m", "0899", "-l", "unix:/tmp/rondi-invalid-mode.sock"])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdcached_default_socket_mode_matches_upstream_umask() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!(
            "skipping rrdcached default socket-mode differential: rrdcached is not installed"
        );
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut modes = Vec::new();
    for (name, executable) in [
        ("upstream", std::ffi::OsStr::new("rrdcached")),
        ("rondi", std::ffi::OsStr::new(env!("CARGO_BIN_EXE_rondi"))),
    ] {
        let run_dir = dir.path().join(name);
        let root = run_dir.join("data");
        let socket = run_dir.join("run/rrdcached.sock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let alias = run_dir.join("rrdcached");
        if name == "rondi" {
            std::os::unix::fs::symlink(executable, &alias).unwrap();
        }
        let mut child = Command::new(if name == "rondi" {
            alias.as_os_str()
        } else {
            executable
        })
        .args(["-g", "-B", "-b", root.to_str().unwrap(), "-l"])
        .arg(format!("unix:{}", socket.display()))
        .arg("-p")
        .arg(run_dir.join("rrdcached.pid"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        wait_for_socket(&mut child, &socket);
        modes.push(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o7777);
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
    }
    assert_eq!(modes[0], modes[1]);
}

#[test]
fn rrdcached_dash_p_restricts_command_permissions() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached permission differential: rrdcached is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut responses = Vec::new();
    for name in ["upstream", "rondi"] {
        let run_dir = dir.path().join(name);
        let root = run_dir.join("data");
        let socket = run_dir.join("run/rrdcached.sock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let alias = run_dir.join("rrdcached");
        let executable = if name == "rondi" {
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
            alias.as_os_str()
        } else {
            std::ffi::OsStr::new("rrdcached")
        };
        let mut child = Command::new(executable)
            .args(["-B", "-b", root.to_str().unwrap(), "-P", "PING", "-l"])
            .arg(format!("unix:{}", socket.display()))
            .arg("-p")
            .arg(run_dir.join("rrdcached.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_socket(&mut child, &socket);
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let ping = rrdcached_request(&mut reader, "PING\n");
        let denied = rrdcached_request(&mut reader, "UPDATE missing.rrd 123:1\n");
        let help = rrdcached_full_request(&mut reader, "HELP\n");
        responses.push((ping, denied, help));
        drop(reader);
        drop(stream);
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
    }
    assert_eq!(responses[0], responses[1]);
    assert_eq!(responses[0].0, "0 PONG\n");
    assert_eq!(responses[0].1, "-1 Permission denied.\n");
}

#[test]
fn rrdcached_socket_group_matches_upstream() {
    if !Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdcached socket-group differential: rrdcached is not installed");
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let gid = unsafe { libc::getgid() };
    // SAFETY: getgrgid returns libc-owned data for a valid process group.
    let group = unsafe { libc::getgrgid(gid) };
    if group.is_null() {
        eprintln!("skipping socket-group test: primary group has no database entry");
        return;
    }
    // SAFETY: non-null group points to libc-owned data with a NUL-terminated name.
    let group_name = unsafe { std::ffi::CStr::from_ptr((*group).gr_name) }
        .to_string_lossy()
        .into_owned();
    let dir = tempfile::tempdir().unwrap();
    let mut socket_metadata = Vec::new();
    for name in ["upstream", "rondi"] {
        let run_dir = dir.path().join(name);
        let root = run_dir.join("data");
        let socket = run_dir.join("run/rrdcached.sock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let alias = run_dir.join("rrdcached");
        let executable = if name == "rondi" {
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
            alias.as_os_str()
        } else {
            std::ffi::OsStr::new("rrdcached")
        };
        let mut child = Command::new(executable)
            .args([
                "-g",
                "-B",
                "-b",
                root.to_str().unwrap(),
                "-s",
                &group_name,
                "-l",
            ])
            .arg(format!("unix:{}", socket.display()))
            .arg("-p")
            .arg(run_dir.join("rrdcached.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_socket_metadata(&mut child, &socket, gid, 0o760);
        let metadata = std::fs::metadata(&socket).unwrap();
        socket_metadata.push((metadata.gid(), metadata.mode() & 0o7777));
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        child.wait().unwrap();
    }
    assert_eq!(socket_metadata[0], (gid, 0o760));
    assert_eq!(socket_metadata[1], socket_metadata[0]);

    let invalid_dir = dir.path().join("invalid-group");
    std::fs::create_dir_all(&invalid_dir).unwrap();
    let alias = invalid_dir.join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let missing_group = "rondi_group_that_does_not_exist_9482";
    let upstream = Command::new("rrdcached")
        .args(["-s", missing_group])
        .output()
        .unwrap();
    let rondi = Command::new(alias)
        .args(["-s", missing_group])
        .output()
        .unwrap();
    assert_eq!(rondi.status.code(), upstream.status.code());
    assert_eq!(rondi.stdout, upstream.stdout);
    assert_eq!(rondi.stderr, upstream.stderr);
}

#[test]
fn rrdcached_pid_file_tracks_owner_and_prevents_a_second_instance() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let socket = dir.path().join("run/rrdcached.sock");
    let second_socket = dir.path().join("run/second.sock");
    let pid_file = dir.path().join("run/rrdcached.pid");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let arguments = |listen: &std::path::Path| {
        vec![
            "-b".to_owned(),
            root.to_string_lossy().into_owned(),
            "-l".to_owned(),
            format!("unix:{}", listen.display()),
            "-p".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        ]
    };
    let mut child = Command::new(&alias)
        .args(arguments(&socket))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);
    assert_eq!(
        std::fs::read_to_string(&pid_file).unwrap().trim(),
        child.id().to_string()
    );

    let conflict = Command::new(&alias)
        .args(arguments(&second_socket))
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(pid_file.exists());
    assert!(!second_socket.exists());

    // SAFETY: the child is the daemon process owned by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rrdcached did not stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while pid_file.exists() && Instant::now() < cleanup_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!pid_file.exists());
    assert!(!socket.exists());

    std::fs::write(&pid_file, "2147483647\n").unwrap();
    let mut restarted = Command::new(&alias)
        .args(arguments(&socket))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut restarted, &socket);
    assert_eq!(
        std::fs::read_to_string(&pid_file).unwrap().trim(),
        restarted.id().to_string()
    );
    // SAFETY: the restarted child belongs to this test.
    assert_eq!(
        unsafe { libc::kill(restarted.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while restarted.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = restarted.kill();
            let _ = restarted.wait();
            panic!("rrdcached did not stop after stale pid-file recovery");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while pid_file.exists() && Instant::now() < cleanup_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!pid_file.exists());
}

#[test]
fn rrdcached_log_option_appends_structured_lifecycle_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let socket = dir.path().join("run/rrdcached.sock");
    let log_file = dir.path().join("run/rrdcached.log");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let mut child = Command::new(&alias)
        .args([
            "-b",
            root.to_str().unwrap(),
            "-l",
            &format!("unix:{}", socket.display()),
            "-o",
            log_file.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(rrdcached_request(&mut reader, "PING\n"), "0 PONG\n");
    drop(reader);
    drop(stream);
    // SAFETY: the child is the daemon process owned by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rrdcached did not stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let logs = std::fs::read_to_string(&log_file).unwrap();
    assert!(logs.contains("rrdcached_started"), "{logs}");
    assert!(logs.contains("rrdcached_stopped"), "{logs}");
    assert!(logs.lines().count() >= 2, "{logs}");
}

fn request(socket: &std::path::Path, method: &str, path: &str, body: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn rrdcached_request(reader: &mut BufReader<&mut UnixStream>, command: &str) -> String {
    reader.get_mut().write_all(command.as_bytes()).unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response
}

fn rrdcached_full_request(reader: &mut BufReader<&mut UnixStream>, command: &str) -> String {
    reader.get_mut().write_all(command.as_bytes()).unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    let body_lines = response
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    for _ in 0..body_lines {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        response.push_str(&line);
    }
    response
}

fn rrdcached_dump(socket: &std::path::Path, command: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.write_all(command.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn rrdcached_fetchbin(socket: &std::path::Path, command: &str) -> Vec<u8> {
    let stream = UnixStream::connect(socket).unwrap();
    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(command.as_bytes()).unwrap();
    let mut response = Vec::new();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let line_count = line
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(line.ends_with(" Success\n"), "{line:?}");
    response.extend_from_slice(line.as_bytes());
    for _ in 0..line_count {
        line.clear();
        reader.read_line(&mut line).unwrap();
        response.extend_from_slice(line.as_bytes());
        if line.contains(": BinaryData ") {
            let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
            let records = fields[2].parse::<usize>().unwrap();
            let element_size = fields[3].parse::<usize>().unwrap();
            let mut payload = vec![0; records * element_size];
            reader.read_exact(&mut payload).unwrap();
            response.extend_from_slice(&payload);
            let mut separator = [0];
            reader.read_exact(&mut separator).unwrap();
            assert_eq!(separator, [b'\n']);
            response.extend_from_slice(&separator);
        }
    }
    response
}

fn rrdcached_stat_value(response: &str, name: &str) -> u64 {
    response
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("missing {name} in rrdcached STATS response: {response}"))
        .parse()
        .unwrap()
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rondi"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn api_create_update_fetch_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("store");
    let socket = dir.path().join("run/rondi.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let mut child = start_server(&root, &socket);
    wait_for_socket(&mut child, &socket);

    let created = request(
        &socket,
        "POST",
        "/v1/databases",
        r#"{"name":"cpu","config":{"step":10,"heartbeat":20,"rows":4,"start":1700000000}}"#,
    );
    assert!(created.starts_with("HTTP/1.1 201"), "{created}");
    let updated = request(
        &socket,
        "POST",
        "/v1/databases/cpu/updates",
        r#"{"id":"sample-1","timestamp":1700000010,"value":3.5}"#,
    );
    assert!(updated.starts_with("HTTP/1.1 200"), "{updated}");
    assert!(updated.contains("durable"), "{updated}");
    let fetched = request(&socket, "GET", "/v1/databases/cpu/points", "");
    assert!(fetched.starts_with("HTTP/1.1 200"), "{fetched}");
    assert!(fetched.contains("3.5"), "{fetched}");

    let create = cli(&[
        "--socket",
        socket.to_str().unwrap(),
        "create",
        "cli-db",
        "--step",
        "10",
        "--heartbeat",
        "20",
        "--rows",
        "4",
        "--start",
        "1700000000",
    ]);
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = cli(&[
        "--socket",
        socket.to_str().unwrap(),
        "update",
        "cli-db",
        "1700000010",
        "8.25",
    ]);
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let fetch = cli(&["--socket", socket.to_str().unwrap(), "fetch", "cli-db"]);
    assert!(
        fetch.status.success(),
        "{}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    assert!(String::from_utf8_lossy(&fetch.stdout).contains("8.25"));

    let local_bypass = cli(&[
        "--root",
        root.to_str().unwrap(),
        "create",
        "bypass",
        "--step",
        "10",
        "--heartbeat",
        "20",
        "--rows",
        "2",
        "--start",
        "1700000000",
    ]);
    assert!(
        !local_bypass.status.success(),
        "local CLI must fail while server owns the root"
    );

    child.kill().unwrap();
    child.wait().unwrap();
    let mut restarted = start_server(&root, &socket);
    wait_for_socket(&mut restarted, &socket);
    let after_restart = request(&socket, "GET", "/v1/databases/cpu/points", "");
    assert!(after_restart.starts_with("HTTP/1.1 200"), "{after_restart}");
    assert!(after_restart.contains("3.5"), "{after_restart}");
    restarted.kill().unwrap();
    restarted.wait().unwrap();
}

#[test]
fn rrdtool_fetch_daemon_resolution_matches_upstream_protocol_behavior() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        eprintln!(
            "skipping daemon FETCH resolution differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("multi-resolution.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:value:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:20",
            "RRA:AVERAGE:0.5:2:20",
            "RRA:AVERAGE:0.5:5:20",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    for index in 1..=10 {
        let timestamp = 1_000_000_000 + index * 10;
        let value = index * index;
        let update = Command::new("rrdtool")
            .args([
                "update",
                file.to_str().unwrap(),
                &format!("{timestamp}:{value}"),
            ])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
    }

    let daemon_alias = dir.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &daemon_alias).unwrap();
    let socket = dir.path().join("rrdcached.sock");
    let journal = dir.path().join("journal");
    std::fs::create_dir_all(&journal).unwrap();
    let mut daemon = Command::new(&daemon_alias)
        .args(["-B", "-b"])
        .arg(&root)
        .args(["-l"])
        .arg(format!("unix:{}", socket.display()))
        .arg("-j")
        .arg(&journal)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut daemon, &socket);

    let rondi_alias = dir.path().join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi_alias).unwrap();
    let mut upstream_by_resolution = Vec::new();
    for resolution in ["10", "20", "50"] {
        let args = [
            "fetch",
            file.to_str().unwrap(),
            "AVERAGE",
            "--resolution",
            resolution,
            "--start",
            "1000000000",
            "--end",
            "1000000100",
            "--daemon",
        ];
        let daemon_address = format!("unix:{}", socket.display());
        let expected = Command::new("rrdtool")
            .args(args)
            .arg(&daemon_address)
            .output()
            .unwrap();
        let actual = Command::new(&rondi_alias)
            .args(args)
            .arg(&daemon_address)
            .output()
            .unwrap();
        assert!(
            expected.status.success(),
            "{}",
            String::from_utf8_lossy(&expected.stderr)
        );
        assert_eq!(
            actual.status.code(),
            expected.status.code(),
            "resolution {resolution}"
        );
        assert_eq!(
            actual.stdout, expected.stdout,
            "resolution {resolution} stdout"
        );
        assert_eq!(
            actual.stderr, expected.stderr,
            "resolution {resolution} stderr"
        );
        upstream_by_resolution.push(expected.stdout);
    }
    assert_eq!(
        upstream_by_resolution[0], upstream_by_resolution[1],
        "pinned rrdcached FETCH does not transmit --resolution"
    );
    assert_eq!(
        upstream_by_resolution[1], upstream_by_resolution[2],
        "pinned rrdcached FETCH does not transmit --resolution"
    );

    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn server_handles_interrupt_and_terminate_and_removes_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    for (label, signal) in [("interrupt", libc::SIGINT), ("terminate", libc::SIGTERM)] {
        let root = dir.path().join(format!("store-{label}"));
        let socket = dir.path().join(format!("run/{label}.sock"));
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let mut child = start_server(&root, &socket);
        wait_for_socket(&mut child, &socket);

        let health = request(&socket, "GET", "/v1/health", "");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        // SAFETY: `child.id()` is a live child process owned by this test.
        let signal_result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
        assert_eq!(signal_result, 0, "failed to send {label} signal");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server did not shut down after {label} within five seconds");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "server exited with {status} on {label}");
        assert!(
            !socket.exists(),
            "server should remove its socket on {label} shutdown"
        );
    }
}

#[test]
fn rrdcached_alias_journals_updates_and_flushes_on_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/rrdcached.sock");
    let alias = dir.path().join("rrdcached");
    std::fs::create_dir_all(dir.path().join("rra")).unwrap();
    // Replies echo -b as given, and upstream rejects a symlinked -b, so both
    // daemons use the canonical base (macOS temporary paths are symlinked).
    let root = std::fs::canonicalize(dir.path().join("rra")).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    let mut child = Command::new(&alias)
        .args([
            "-B",
            "-F",
            "-g",
            "-b",
            root.to_str().unwrap(),
            "-f",
            "2h",
            "-w",
            "5m",
            "-t",
            "2",
            "-a",
            "4",
            "-l",
        ])
        .arg(format!("unix:{}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(rrdcached_request(&mut reader, "PING\n"), "0 PONG\n");
    let rrd = root.join("poller.rrd");
    let canonical_rrd = std::fs::canonicalize(&root).unwrap().join("poller.rrd");
    let canonical_forget_rrd = std::fs::canonicalize(&root).unwrap().join("forget.rrd");
    let relative_rrd = "poller.rrd";
    assert_eq!(
        rrdcached_request(
            &mut reader,
            &format!(
                "CREATE {relative_rrd} -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
            )
        ),
        "0 RRD created OK\n"
    );
    assert!(rrd.exists());
    let info_response = rrdcached_request(&mut reader, "INFO poller.rrd\n");
    assert_eq!(
        info_response,
        format!("20 Info for {} follows\n", canonical_rrd.display())
    );
    let mut info_body = String::new();
    for _ in 0..20 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        info_body.push_str(&line);
    }
    assert!(info_body.contains("rrd_version 2 0003\n"));
    assert!(info_body.contains("ds[load].type 2 GAUGE\n"));
    assert!(info_body.contains("rra[0].cf 2 AVERAGE\n"));
    assert_eq!(
        rrdcached_request(
            &mut reader,
            "CREATE forget.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
        ),
        "0 RRD created OK\n"
    );
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE forget.rrd 1000000010:3\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    std::fs::create_dir_all(root.join("subdir")).unwrap();
    assert_eq!(
        rrdcached_request(
            &mut reader,
            "CREATE subdir/nested.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
        ),
        "0 RRD created OK\n"
    );
    let mut upstream_forget_response = None;
    let mut upstream_last_response = None;
    let mut upstream_duplicate_response = None;
    let mut upstream_suspend_responses = Vec::new();
    let mut upstream_dump_responses = None;
    let mut upstream_fetchbin_response = None;
    if Command::new("rrdcached").arg("--version").output().is_ok() {
        let upstream_socket = dir.path().join("run/upstream-rrdcached.sock");
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let mut upstream = Command::new("rrdcached")
            .args([
                "-g",
                "-B",
                "-F",
                "-b",
                canonical_root.to_str().unwrap(),
                "-l",
            ])
            .arg(format!("unix:{}", upstream_socket.display()))
            .args(["-w", "3600", "-f", "7200", "-t", "2", "-a", "4", "-p"])
            .arg(dir.path().join("run/upstream-rrdcached.pid"))
            .args(["-j"])
            .arg(dir.path().join("run"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_socket(&mut upstream, &upstream_socket);
        let mut upstream_stream = UnixStream::connect(&upstream_socket).unwrap();
        upstream_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut upstream_reader = BufReader::new(&mut upstream_stream);
        let upstream_first_without_index =
            rrdcached_request(&mut upstream_reader, "FIRST poller.rrd\n");
        let rondi_first_without_index = rrdcached_request(&mut reader, "FIRST poller.rrd\n");
        assert_eq!(rondi_first_without_index, upstream_first_without_index);
        let upstream_create_response = rrdcached_request(
            &mut upstream_reader,
            "CREATE upstream-created.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n",
        );
        drop(upstream_reader);
        let rondi_create_response = rrdcached_request(
            &mut reader,
            "CREATE rondi-created.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n",
        );
        assert_eq!(rondi_create_response, upstream_create_response);
        assert_eq!(
            rrdcached_request(
                &mut reader,
                "CREATE rondi-tune.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n",
            ),
            "0 RRD created OK\n"
        );
        let rondi_tune_file = root.join("rondi-tune.rrd");
        let upstream_tune_file = root.join("upstream-tune.rrd");
        std::fs::copy(&rondi_tune_file, &upstream_tune_file).unwrap();
        let upstream_tune_response = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_request(
                &mut upstream_reader,
                "TUNE upstream-tune.rrd 9 tune -r load:metric -h metric:30 -i metric:0 -a metric:100\n",
            )
        };
        let rondi_tune_response = rrdcached_request(
            &mut reader,
            "TUNE rondi-tune.rrd 9 tune -r load:metric -h metric:30 -i metric:0 -a metric:100\n",
        );
        assert_eq!(rondi_tune_response, upstream_tune_response);
        assert_eq!(
            std::fs::read(&rondi_tune_file).unwrap(),
            std::fs::read(&upstream_tune_file).unwrap()
        );
        let tuned_info = rondi::inspect_rrd_file(&rondi_tune_file).unwrap();
        let tuned_ds = &tuned_info.data_sources[0];
        assert_eq!(tuned_ds.name, "metric");
        assert_eq!(tuned_ds.heartbeat, 30);
        assert_eq!(tuned_ds.minimum, Some(0.0));
        assert_eq!(tuned_ds.maximum, Some(100.0));
        let multi_ds_create = "CREATE multi-fetchbin.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U DS:temp:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n";
        let upstream_multi_ds_create = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_request(&mut upstream_reader, multi_ds_create)
        };
        let rondi_multi_ds_create = rrdcached_request(&mut reader, multi_ds_create);
        assert_eq!(rondi_multi_ds_create, upstream_multi_ds_create);
        for update in [
            "UPDATE multi-fetchbin.rrd 1000000010:5:U\n",
            "UPDATE multi-fetchbin.rrd 1000000020:10:4\n",
        ] {
            let upstream_update = {
                let mut upstream_reader = BufReader::new(&mut upstream_stream);
                rrdcached_request(&mut upstream_reader, update)
            };
            assert_eq!(rrdcached_request(&mut reader, update), upstream_update);
        }
        let upstream_multi_fetchbin = rrdcached_fetchbin(
            &upstream_socket,
            "FETCHBIN multi-fetchbin.rrd AVERAGE 1000000000 1000000020\n",
        );
        let rondi_multi_fetchbin = rrdcached_fetchbin(
            &socket,
            "FETCHBIN multi-fetchbin.rrd AVERAGE 1000000000 1000000020\n",
        );
        assert!(rondi_multi_fetchbin.starts_with(b"7 Success\n"));
        assert!(
            rondi_multi_fetchbin
                .windows(b"DSName-load: BinaryData".len())
                .any(|window| window == b"DSName-load: BinaryData")
        );
        assert!(
            rondi_multi_fetchbin
                .windows(b"DSName-temp: BinaryData".len())
                .any(|window| window == b"DSName-temp: BinaryData")
        );
        assert_eq!(rondi_multi_fetchbin, upstream_multi_fetchbin);
        // Only the known `load` column is compared because the printed sign
        // of an unknown value depends on the C library.
        let fetch_text = "FETCH multi-fetchbin.rrd AVERAGE 1000000000 1000000019 load\n";
        let upstream_fetch_text = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, fetch_text)
        };
        assert!(
            upstream_fetch_text.contains(" 5.00000000000000000e+00\n"),
            "{upstream_fetch_text}"
        );
        assert_eq!(
            rrdcached_full_request(&mut reader, fetch_text),
            upstream_fetch_text
        );
        // Replies echo the base directory joined with the requested name,
        // not the resolved path.
        for (command, full) in [
            ("INFO ./poller.rrd\n", true),
            ("FLUSH ./missing.rrd\n", false),
        ] {
            let upstream_echo = {
                let mut upstream_reader = BufReader::new(&mut upstream_stream);
                if full {
                    rrdcached_full_request(&mut upstream_reader, command)
                } else {
                    rrdcached_request(&mut upstream_reader, command)
                }
            };
            assert!(upstream_echo.contains("/./"), "{upstream_echo}");
            let rondi_echo = if full {
                rrdcached_full_request(&mut reader, command)
            } else {
                rrdcached_request(&mut reader, command)
            };
            assert_eq!(rondi_echo, upstream_echo);
        }
        // Upstream closes the connection after some errors, so each
        // command uses its own connection.
        let one_shot = |path: &std::path::Path, command: &str| {
            let mut stream = UnixStream::connect(path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            rrdcached_request(&mut BufReader::new(&mut stream), command)
        };
        for command in [
            "BOGUS\n",
            "bogus x\n",
            "WROTE x\n",
            "UPDATE\n",
            "FLUSH\n",
            "FLUSH ./missing.rrd extra\n",
            "PENDING\n",
            "PENDING poller.rrd extra\n",
            "FORGET\n",
            "INFO\n",
            "LAST\n",
            "SUSPEND\n",
            "RESUME\n",
            "FETCH\n",
            "FETCH poller.rrd\n",
            "TUNE\n",
            "TUNE poller.rrd\n",
            "CREATE\n",
            "PING x\n",
            "QUEUE x\n",
        ] {
            assert_eq!(
                one_shot(&socket, command),
                one_shot(&upstream_socket, command),
                "{command:?}"
            );
        }
        let upstream_forget_multi = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_request(&mut upstream_reader, "FORGET multi-fetchbin.rrd\n")
        };
        assert_eq!(
            rrdcached_request(&mut reader, "FORGET multi-fetchbin.rrd\n"),
            upstream_forget_multi
        );
        let upstream_response = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, "INFO poller.rrd\n")
        };
        let upstream_help_response = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, "HELP\n")
        };
        let rondi_help_response = rrdcached_full_request(&mut reader, "HELP\n");
        assert_eq!(rondi_help_response, upstream_help_response);
        let upstream_fetchbin_help = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, "HELP FETCHBIN\n")
        };
        assert_eq!(
            rrdcached_full_request(&mut reader, "HELP FETCHBIN\n"),
            upstream_fetchbin_help
        );
        for command in [
            "UPDATE",
            "TUNE",
            "DUMP",
            "FLUSH",
            "FLUSHALL",
            "PENDING",
            "FORGET",
            "QUEUE",
            "STATS",
            "HELP",
            "PING",
            "BATCH",
            "FETCH",
            "INFO",
            "FIRST",
            "LAST",
            "CREATE",
            "LIST",
            "SUSPEND",
            "RESUME",
            "SUSPENDALL",
            "RESUMEALL",
            "QUIT",
            "WROTE",
            "UNKNOWN",
        ] {
            let query = format!("HELP {command}\n");
            let upstream_help = {
                let mut upstream_reader = BufReader::new(&mut upstream_stream);
                rrdcached_full_request(&mut upstream_reader, &query)
            };
            assert_eq!(
                rrdcached_full_request(&mut reader, &query),
                upstream_help,
                "help response differed for {command}"
            );
        }
        let upstream_list_response = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, "LIST /\n")
        };
        let upstream_recursive_list_response = {
            let mut upstream_reader = BufReader::new(&mut upstream_stream);
            rrdcached_full_request(&mut upstream_reader, "LIST RECURSIVE /\n")
        };
        upstream_dump_responses = Some(
            [
                "DUMP poller.rrd\n",
                "DUMP poller.rrd -h none\n",
                "DUMP poller.rrd -h xsd\n",
            ]
            .map(|command| rrdcached_dump(&upstream_socket, command)),
        );
        upstream_fetchbin_response = Some(rrdcached_fetchbin(
            &upstream_socket,
            "FETCHBIN poller.rrd AVERAGE 1000000000 1000000020 load\n",
        ));
        let mut upstream_reader = BufReader::new(&mut upstream_stream);
        assert_eq!(
            rrdcached_request(&mut upstream_reader, "UPDATE forget.rrd 1000000010:3\n"),
            "0 errors, enqueued 1 value(s).\n"
        );
        rrdcached_request(&mut reader, "FLUSH forget.rrd\n");
        rrdcached_request(&mut upstream_reader, "FLUSH forget.rrd\n");
        let stats_create =
            "CREATE stats.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n";
        assert_eq!(
            rrdcached_request(&mut reader, stats_create),
            rrdcached_request(&mut upstream_reader, stats_create)
        );
        assert_eq!(
            rrdcached_request(&mut reader, "UPDATE stats.rrd 1000000010:2\n"),
            rrdcached_request(&mut upstream_reader, "UPDATE stats.rrd 1000000010:2\n")
        );
        assert_eq!(
            rrdcached_request(&mut reader, "SUSPEND stats.rrd\n"),
            rrdcached_request(&mut upstream_reader, "SUSPEND stats.rrd\n")
        );
        for update in [
            "UPDATE stats.rrd 1000000020:3\n",
            "UPDATE stats.rrd 1000000030:4\n",
        ] {
            let upstream_update = rrdcached_request(&mut upstream_reader, update);
            assert_eq!(rrdcached_request(&mut reader, update), upstream_update);
        }
        let upstream_stats = rrdcached_full_request(&mut upstream_reader, "STATS\n");
        let rondi_stats = rrdcached_full_request(&mut reader, "STATS\n");
        assert_eq!(
            rrdcached_stat_value(&rondi_stats, "QueueLength"),
            rrdcached_stat_value(&upstream_stats, "QueueLength"),
            "QueueLength must count queued files rather than UPDATE batches"
        );
        assert_eq!(rrdcached_stat_value(&upstream_stats, "QueueLength"), 0);
        assert_eq!(
            rrdcached_stat_value(&rondi_stats, "TreeNodesNumber"),
            rrdcached_stat_value(&upstream_stats, "TreeNodesNumber"),
            "TreeNodesNumber must match the live cache tree"
        );
        assert_eq!(
            rrdcached_stat_value(&rondi_stats, "TreeDepth"),
            rrdcached_stat_value(&upstream_stats, "TreeDepth"),
            "TreeDepth must report the cache AVL tree height"
        );
        let mut tree_test_files = Vec::new();
        for index in 0..8 {
            let rondi_file = format!("rondi-depth-{index:02}.rrd");
            let upstream_file = format!("upstream-depth-{index:02}.rrd");
            let create = |file: &str| {
                format!(
                    "CREATE {file} -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
                )
            };
            assert_eq!(
                rrdcached_request(&mut reader, &create(&rondi_file)),
                "0 RRD created OK\n"
            );
            assert_eq!(
                rrdcached_request(&mut upstream_reader, &create(&upstream_file)),
                "0 RRD created OK\n"
            );
            let rondi_update = format!("UPDATE {rondi_file} 1000000010:1\n");
            assert!(
                rrdcached_request(&mut reader, &rondi_update).starts_with("0 errors, enqueued")
            );
            assert!(
                rrdcached_request(&mut reader, &format!("SUSPEND {rondi_file}\n"))
                    .starts_with("0 ")
            );
            let upstream_update = format!("UPDATE {upstream_file} 1000000010:1\n");
            assert!(
                rrdcached_request(&mut upstream_reader, &upstream_update)
                    .starts_with("0 errors, enqueued")
            );
            assert!(
                rrdcached_request(&mut upstream_reader, &format!("SUSPEND {upstream_file}\n"))
                    .starts_with("0 ")
            );
            tree_test_files.push((rondi_file, upstream_file));
        }
        let upstream_tree_stats = rrdcached_full_request(&mut upstream_reader, "STATS\n");
        let rondi_tree_stats = rrdcached_full_request(&mut reader, "STATS\n");
        assert_eq!(
            rrdcached_stat_value(&rondi_tree_stats, "TreeNodesNumber"),
            rrdcached_stat_value(&upstream_tree_stats, "TreeNodesNumber")
        );
        assert_eq!(
            rrdcached_stat_value(&rondi_tree_stats, "TreeDepth"),
            rrdcached_stat_value(&upstream_tree_stats, "TreeDepth"),
            "AVL cache tree height differed after ordered insertions"
        );
        for (rondi_file, upstream_file) in tree_test_files {
            assert_eq!(
                rrdcached_request(&mut reader, &format!("FORGET {rondi_file}\n")),
                "0 Gone!\n"
            );
            assert_eq!(
                rrdcached_request(&mut upstream_reader, &format!("FORGET {upstream_file}\n")),
                "0 Gone!\n"
            );
            std::fs::remove_file(root.join(rondi_file)).unwrap();
            std::fs::remove_file(root.join(upstream_file)).unwrap();
        }
        let upstream_tree_after_remove = rrdcached_full_request(&mut upstream_reader, "STATS\n");
        let rondi_tree_after_remove = rrdcached_full_request(&mut reader, "STATS\n");
        assert_eq!(
            rrdcached_stat_value(&rondi_tree_after_remove, "TreeNodesNumber"),
            rrdcached_stat_value(&upstream_tree_after_remove, "TreeNodesNumber")
        );
        assert_eq!(
            rrdcached_stat_value(&rondi_tree_after_remove, "TreeDepth"),
            rrdcached_stat_value(&upstream_tree_after_remove, "TreeDepth"),
            "AVL cache tree height differed after removals"
        );
        assert_eq!(
            rrdcached_request(&mut reader, "FORGET stats.rrd\n"),
            rrdcached_request(&mut upstream_reader, "FORGET stats.rrd\n")
        );
        std::fs::remove_file(root.join("stats.rrd")).unwrap();
        assert_eq!(
            rrdcached_request(&mut reader, "UPDATE forget.rrd 1000000020:3\n"),
            rrdcached_request(&mut upstream_reader, "UPDATE forget.rrd 1000000020:3\n")
        );
        upstream_last_response = Some(rrdcached_request(&mut upstream_reader, "LAST forget.rrd\n"));
        upstream_duplicate_response = Some(rrdcached_request(
            &mut upstream_reader,
            "UPDATE forget.rrd 1000000020:4\n",
        ));
        for command in [
            "SUSPEND forget.rrd\n",
            "SUSPEND forget.rrd\n",
            "FLUSH forget.rrd\n",
            "PENDING forget.rrd\n",
            "RESUME forget.rrd\n",
            "RESUME forget.rrd\n",
            "FLUSH forget.rrd\n",
            "PENDING forget.rrd\n",
            "SUSPENDALL\n",
            "RESUMEALL\n",
        ] {
            upstream_suspend_responses.push(rrdcached_request(&mut upstream_reader, command));
            if command.contains("PENDING") {
                let count = upstream_suspend_responses
                    .last()
                    .unwrap()
                    .split_ascii_whitespace()
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                for _ in 0..count {
                    let mut sample = String::new();
                    upstream_reader.read_line(&mut sample).unwrap();
                    upstream_suspend_responses.push(sample);
                }
            }
        }
        upstream_forget_response = Some(rrdcached_request(
            &mut upstream_reader,
            "FORGET forget.rrd\n",
        ));
        drop(upstream_reader);
        // SAFETY: the upstream daemon is the live child spawned above.
        assert_eq!(
            unsafe { libc::kill(upstream.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        upstream.wait().unwrap();
        assert_eq!(
            rrdcached_full_request(&mut reader, "INFO poller.rrd\n"),
            upstream_response
        );
        assert_eq!(
            rrdcached_full_request(&mut reader, "LIST /\n"),
            upstream_list_response
        );
        assert_eq!(
            rrdcached_full_request(&mut reader, "LIST RECURSIVE /\n"),
            upstream_recursive_list_response
        );
    }
    let rondi_dump_responses = [
        "DUMP poller.rrd\n",
        "DUMP poller.rrd -h none\n",
        "DUMP poller.rrd -h xsd\n",
    ]
    .map(|command| rrdcached_dump(&socket, command));
    for response in &rondi_dump_responses {
        assert!(response.starts_with("<?xml version=\"1.0\""));
        assert!(response.ends_with("</rrd>\n"));
    }
    assert!(
        rondi_dump_responses
            .windows(2)
            .all(|pair| pair[0] == pair[1])
    );
    if let Some(upstream_dump_responses) = upstream_dump_responses {
        assert_eq!(rondi_dump_responses, upstream_dump_responses);
    }
    let rondi_fetchbin_response = rrdcached_fetchbin(
        &socket,
        "FETCHBIN poller.rrd AVERAGE 1000000000 1000000020 load\n",
    );
    assert!(rondi_fetchbin_response.starts_with(b"6 Success\n"));
    assert!(
        rondi_fetchbin_response
            .windows(b"DSName-load: BinaryData".len())
            .any(|window| window == b"DSName-load: BinaryData")
    );
    if let Some(upstream_fetchbin_response) = upstream_fetchbin_response {
        assert_eq!(rondi_fetchbin_response, upstream_fetchbin_response);
    }
    assert_eq!(
        rrdcached_request(&mut reader, "BATCH\n"),
        "0 Go ahead.  End with dot '.' on its own line.\n"
    );
    reader.get_mut().write_all(b"DUMP poller.rrd\n.\n").unwrap();
    let mut dump_batch_response = String::new();
    reader.read_line(&mut dump_batch_response).unwrap();
    assert_eq!(dump_batch_response, "1 errors\n");
    dump_batch_response.clear();
    reader.read_line(&mut dump_batch_response).unwrap();
    assert_eq!(dump_batch_response, "1 Can't use 'DUMP' here.\n");
    assert_eq!(
        rrdcached_request(&mut reader, "BATCH\n"),
        "0 Go ahead.  End with dot '.' on its own line.\n"
    );
    reader
        .get_mut()
        .write_all(b"FETCHBIN poller.rrd AVERAGE\n.\n")
        .unwrap();
    dump_batch_response.clear();
    reader.read_line(&mut dump_batch_response).unwrap();
    assert_eq!(dump_batch_response, "1 errors\n");
    dump_batch_response.clear();
    reader.read_line(&mut dump_batch_response).unwrap();
    assert_eq!(dump_batch_response, "1 Can't use 'FETCHBIN' here.\n");
    let last_response = rrdcached_request(&mut reader, "LAST forget.rrd\n");
    let expected_last = if upstream_last_response.is_some() {
        "0 1000000020\n"
    } else {
        "0 1000000010\n"
    };
    assert_eq!(last_response, expected_last);
    if let Some(upstream_response) = upstream_last_response {
        assert_eq!(last_response, upstream_response);
    }
    let duplicate_timestamp = if upstream_duplicate_response.is_some() {
        "1000000020"
    } else {
        "1000000010"
    };
    let duplicate_response = rrdcached_request(
        &mut reader,
        &format!("UPDATE forget.rrd {duplicate_timestamp}:4\n"),
    );
    if let Some(upstream_response) = upstream_duplicate_response {
        assert_eq!(duplicate_response, upstream_response);
    } else {
        assert!(duplicate_response.starts_with("-1 illegal attempt to update"));
    }
    let mut suspend_responses = Vec::new();
    for command in [
        "SUSPEND forget.rrd\n",
        "SUSPEND forget.rrd\n",
        "FLUSH forget.rrd\n",
        "PENDING forget.rrd\n",
        "RESUME forget.rrd\n",
        "RESUME forget.rrd\n",
        "FLUSH forget.rrd\n",
        "PENDING forget.rrd\n",
        "SUSPENDALL\n",
        "RESUMEALL\n",
    ] {
        suspend_responses.push(rrdcached_request(&mut reader, command));
        if command.contains("PENDING") && suspend_responses.last().unwrap().starts_with('1') {
            let mut sample = String::new();
            reader.read_line(&mut sample).unwrap();
            suspend_responses.push(sample);
        }
    }
    let expected_forget_pending = if upstream_suspend_responses.is_empty() {
        "1000000010:3\n"
    } else {
        "1000000020:3\n"
    };
    assert_eq!(
        suspend_responses,
        [
            format!("0 {} suspended\n", canonical_forget_rrd.display()),
            format!("0 {} already suspended\n", canonical_forget_rrd.display()),
            format!(
                "0 Successfully flushed {}.\n",
                canonical_forget_rrd.display()
            ),
            "1 updates pending\n".to_owned(),
            expected_forget_pending.to_owned(),
            format!("0 {} resumed\n", canonical_forget_rrd.display()),
            format!("0 {} not suspended\n", canonical_forget_rrd.display()),
            format!(
                "0 Successfully flushed {}.\n",
                canonical_forget_rrd.display()
            ),
            "0 updates pending\n".to_owned(),
            "0 1 rrds suspend\n".to_owned(),
            "0 1 rrds resumed\n".to_owned(),
        ]
    );
    if !upstream_suspend_responses.is_empty() {
        assert_eq!(suspend_responses, upstream_suspend_responses);
    }
    let forget_response = rrdcached_request(&mut reader, "FORGET forget.rrd\n");
    assert_eq!(forget_response, "0 Gone!\n");
    if let Some(upstream_response) = upstream_forget_response {
        assert_eq!(forget_response, upstream_response);
    }
    assert_eq!(
        rrdcached_request(&mut reader, "PENDING forget.rrd\n"),
        "0 updates pending\n"
    );
    assert_eq!(
        rrdcached_request(
            &mut reader,
            &format!("UPDATE {relative_rrd} 1000000010:4.25\n")
        ),
        "0 errors, enqueued 1 value(s).\n"
    );
    let pending = rrdcached_request(&mut reader, &format!("PENDING {relative_rrd}\n"));
    assert_eq!(pending, "1 updates pending\n");
    let mut pending_sample = String::new();
    reader.read_line(&mut pending_sample).unwrap();
    assert_eq!(pending_sample, "1000000010:4.25\n");
    assert_eq!(rrdcached_request(&mut reader, "QUEUE\n"), "0 in queue.\n");
    assert_eq!(
        rrdcached_request(&mut reader, &format!("LAST {relative_rrd}\n")),
        "0 1000000010\n"
    );
    let fetch_header = rrdcached_request(
        &mut reader,
        &format!("FETCH {relative_rrd} AVERAGE 1000000000 1000000010\n"),
    );
    let body_lines = fetch_header
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(fetch_header.ends_with(" Success\n"));
    let mut fetch_body = String::new();
    for _ in 0..body_lines {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        fetch_body.push_str(&line);
    }
    assert!(fetch_body.contains("DSName: load\n"), "{fetch_body}");
    assert_eq!(
        rrdcached_request(&mut reader, &format!("LAST {relative_rrd}\n")),
        "0 1000000010\n"
    );
    assert_eq!(
        rrdcached_request(&mut reader, &format!("PENDING {relative_rrd}\n")),
        "0 updates pending\n"
    );
    assert_eq!(rrdcached_request(&mut reader, "QUEUE\n"), "0 in queue.\n");
    assert_eq!(
        rrdcached_request(&mut reader, "BATCH\n"),
        "0 Go ahead.  End with dot '.' on its own line.\n"
    );
    reader.get_mut().write_all(b"PING\nINVALID\n.\n").unwrap();
    let mut batch_result = String::new();
    reader.read_line(&mut batch_result).unwrap();
    assert_eq!(batch_result, "1 errors\n");
    batch_result.clear();
    reader.read_line(&mut batch_result).unwrap();
    assert!(
        batch_result.starts_with("2 Unknown command: INVALID"),
        "{batch_result:?}"
    );
    assert!(rrdcached_request(&mut reader, "STATS\n").starts_with("9 Statistics follow\n"));
    for _ in 0..9 {
        let mut stat = String::new();
        reader.read_line(&mut stat).unwrap();
        assert!(stat.contains(':'));
    }
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE poller.rrd 1000000020:5\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    assert_eq!(rrdcached_request(&mut reader, "QUIT\n"), "");
    drop(reader);
    // SAFETY: `child.id()` is a live child process owned by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rrdcached did not stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists(), "rrdcached removes its socket on shutdown");
    assert_eq!(
        rondi::inspect_rrd_file(rrd).unwrap().last_update,
        1000000020
    );
}

#[test]
fn rrdcached_fractional_update_timestamps_flush_byte_for_byte_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping fractional rrdcached update differential: rrdtool is not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("rrds");
    let socket = dir.path().join("run/rrdcached.sock");
    let alias = dir.path().join("rrdcached");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let journal = dir.path().join("journal");
    std::fs::create_dir_all(&journal).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    let cached_file = root.join("fractional.rrd");
    let oracle_file = dir.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            cached_file.to_str().unwrap(),
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
    std::fs::copy(&cached_file, &oracle_file).unwrap();

    let mut child = Command::new(&alias)
        .args(["-g", "-b"])
        .arg(&root)
        .args(["-f", "2h", "-w", "5m", "-p"])
        .arg(dir.path().join("rrdcached.pid"))
        .args(["-j"])
        .arg(&journal)
        .arg("-l")
        .arg(format!("unix:{}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(
            &mut reader,
            "UPDATE fractional.rrd 1000000010.0000001:1 1000000010.0000019:2 1000000010.9999999:3 1000000020.1234567:4\n"
        ),
        "0 errors, enqueued 4 value(s).\n"
    );
    assert_eq!(
        rrdcached_request(&mut reader, "FLUSH fractional.rrd\n"),
        format!("0 Successfully flushed {}.\n", cached_file.display())
    );

    let updated = Command::new("rrdtool")
        .arg("update")
        .arg(&oracle_file)
        .args([
            "1000000010.0000001:1",
            "1000000010.0000019:2",
            "1000000010.9999999:3",
            "1000000020.1234567:4",
        ])
        .env_remove("RRDCACHED_ADDRESS")
        .output()
        .unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );
    let cached_bytes = std::fs::read(&cached_file).unwrap();
    let oracle_bytes = std::fs::read(&oracle_file).unwrap();
    let differing_offsets = cached_bytes
        .iter()
        .zip(&oracle_bytes)
        .enumerate()
        .filter_map(|(offset, (cached, oracle))| (cached != oracle).then_some(offset))
        .take(16)
        .collect::<Vec<_>>();
    let differing_values = differing_offsets
        .iter()
        .map(|offset| (*offset, cached_bytes[*offset], oracle_bytes[*offset]))
        .collect::<Vec<_>>();
    assert_eq!(
        cached_bytes, oracle_bytes,
        "differing bytes (offset, cached, oracle): {differing_values:?}"
    );
    drop(reader);
    drop(stream);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn rrdtool_update_daemon_and_environment_route_writes_through_rrdcached() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping rrdtool --daemon integration: RRDtool is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("rra");
    let socket = dir.path().join("run/rrdcached.sock");
    let rrdcached = dir.path().join("rrdcached");
    let rrdtool = dir.path().join("rrdtool");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rrdcached).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rrdtool).unwrap();

    let file = root.join("poller.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            file.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:load:GAUGE:30:U:U",
            "DS:counter:COUNTER:30:U:U",
            "DS:derive:DERIVE:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let oracle_file = root.join("oracle.rrd");
    std::fs::copy(&file, &oracle_file).unwrap();

    let mut daemon = Command::new(&rrdcached)
        .args(["-b", root.to_str().unwrap(), "-l"])
        .arg(format!("unix:{}", socket.display()))
        .args(["-w", "3600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for_socket(&mut daemon, &socket);

    let explicit = Command::new(&rrdtool)
        .args([
            "update",
            file.to_str().unwrap(),
            "--daemon",
            &format!("unix:{}", socket.display()),
            "1000000010:5:9007199254740993:-9007199254740993",
        ])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert!(explicit.stdout.is_empty());

    let from_environment = Command::new(&rrdtool)
        .current_dir(&root)
        .args([
            "update",
            "poller.rrd",
            "1000000020:7:9007199254740994:-9007199254740994",
        ])
        .env("RRDCACHED_ADDRESS", format!("unix:{}", socket.display()))
        .output()
        .unwrap();
    assert!(
        from_environment.status.success(),
        "{}",
        String::from_utf8_lossy(&from_environment.stderr)
    );
    for sample in [
        "1000000010:5:9007199254740993:-9007199254740993",
        "1000000020:7:9007199254740994:-9007199254740994",
    ] {
        let updated = Command::new("rrdtool")
            .args(["update", oracle_file.to_str().unwrap(), sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
    }

    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(&mut stream);
    let flushed = rrdcached_request(&mut reader, &format!("FLUSH {}\n", file.display()));
    assert!(flushed.starts_with("0 "), "{flushed}");
    drop(reader);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(&oracle_file).unwrap()
    );

    let fetched = Command::new("rrdtool")
        .args([
            "fetch",
            file.to_str().unwrap(),
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
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let fetched = String::from_utf8_lossy(&fetched.stdout);
    assert!(fetched.contains("5.0000000000e+00"), "{fetched}");
    assert!(fetched.contains("7.0000000000e+00"), "{fetched}");

    daemon.kill().unwrap();
    daemon.wait().unwrap();

    if Command::new("rrdcached")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        let upstream_socket = dir.path().join("run/upstream-rrdcached.sock");
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let mut upstream = Command::new("rrdcached")
            .args(["-g", "-b", canonical_root.to_str().unwrap(), "-l"])
            .arg(format!("unix:{}", upstream_socket.display()))
            .args(["-w", "3600", "-f", "7200", "-p"])
            .arg(dir.path().join("run/upstream-rrdcached.pid"))
            .args(["-j"])
            .arg(dir.path().join("run"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_for_socket(&mut upstream, &upstream_socket);
        let upstream_update = Command::new(&rrdtool)
            .args([
                "update",
                file.to_str().unwrap(),
                "--daemon",
                &format!("unix:{}", upstream_socket.display()),
                "1000000030:9:9007199254740995:-9007199254740995",
            ])
            .output()
            .unwrap();
        assert!(
            upstream_update.status.success(),
            "{}",
            String::from_utf8_lossy(&upstream_update.stderr)
        );
        let mut upstream_stream = UnixStream::connect(&upstream_socket).unwrap();
        let mut upstream_reader = BufReader::new(&mut upstream_stream);
        let pending = rrdcached_request(
            &mut upstream_reader,
            &format!("PENDING {}\n", file.display()),
        );
        assert_eq!(pending, "1 updates pending\n");
        let mut pending_sample = String::new();
        upstream_reader.read_line(&mut pending_sample).unwrap();
        assert_eq!(
            pending_sample,
            "1000000030:9:9007199254740995:-9007199254740995\n"
        );
        let flushed =
            rrdcached_request(&mut upstream_reader, &format!("FLUSH {}\n", file.display()));
        assert!(flushed.starts_with("0 "), "{flushed}");
        drop(upstream_reader);
        upstream.kill().unwrap();
        upstream.wait().unwrap();

        let fetched = Command::new("rrdtool")
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
        assert!(
            fetched.status.success(),
            "{}",
            String::from_utf8_lossy(&fetched.stderr)
        );
        assert!(
            String::from_utf8_lossy(&fetched.stdout).contains("9.0000000000e+00"),
            "{}",
            String::from_utf8_lossy(&fetched.stdout)
        );
    }
}

#[test]
fn rrdcached_recovers_journaled_update_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("rra");
    let socket = dir.path().join("run/rrdcached.sock");
    let journal_directory = dir.path().join("run/journal");
    let alias = dir.path().join("rrdcached");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&journal_directory).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();

    let start = || {
        Command::new(&alias)
            .args([
                "-b",
                root.to_str().unwrap(),
                "-j",
                journal_directory.to_str().unwrap(),
                "-l",
            ])
            .arg(format!("unix:{}", socket.display()))
            .arg("-w")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut child = start();
    wait_for_socket(&mut child, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(
            &mut reader,
            "CREATE crash.rrd -b 1000000000 -s 10 DS:load:GAUGE:20:U:U RRA:AVERAGE:0.5:1:8\n"
        ),
        "0 RRD created OK\n"
    );
    assert_eq!(
        rrdcached_request(&mut reader, "UPDATE crash.rrd 1000000010:9\n"),
        "0 errors, enqueued 1 value(s).\n"
    );
    assert!(journal_directory.join(".rrdcached.journal").is_file());
    assert_eq!(
        rrdcached_request(&mut reader, "LAST crash.rrd\n"),
        "0 1000000010\n"
    );
    drop(reader);
    drop(stream);

    // SIGKILL simulates loss before a graceful flush. The synced journal must
    // retain the accepted update and startup must recover it.
    // SAFETY: `child.id()` belongs to the still-running test child.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) },
        0
    );
    child.wait().unwrap();

    let mut restarted = start();
    wait_for_socket(&mut restarted, &socket);
    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(&mut stream);
    assert_eq!(
        rrdcached_request(&mut reader, "PENDING crash.rrd\n"),
        "1 updates pending\n"
    );
    let mut sample = String::new();
    reader.read_line(&mut sample).unwrap();
    assert_eq!(sample, "1000000010:9\n");
    assert!(
        rrdcached_request(&mut reader, "FLUSH crash.rrd\n").starts_with("0 Successfully flushed")
    );
    assert_eq!(
        rrdcached_request(&mut reader, "LAST crash.rrd\n"),
        "0 1000000010\n"
    );
    drop(reader);
    drop(stream);
    restarted.kill().unwrap();
    restarted.wait().unwrap();
}
