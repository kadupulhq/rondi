#![cfg(unix)]

//! rrdcached `-l`, `-L`, `-m`, `-P`, `-U`, and `-G` behavior compared with
//! the pinned upstream daemon.

#[macro_use]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn upstream_available(test: &str) -> bool {
    let found = Command::new("rrdcached")
        .arg("-h")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("RRDCacheD 1.11.0"));
    if !found {
        oracle_skip!("skipping {test}: pinned rrdcached 1.11.0 is not installed");
    }
    found
}

/// The upstream executable, or a `rrdcached` symlink to Rondi in `dir`.
fn daemon(name: &str, dir: &Path) -> PathBuf {
    if name == "upstream" {
        return PathBuf::from("rrdcached");
    }
    let alias = dir.join("rrdcached");
    if !alias.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    }
    alias
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[derive(Clone, Debug)]
enum Endpoint {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl Endpoint {
    /// The first reply line to `command`, or None when nothing accepts.
    fn first_line(&self, command: &str) -> Option<String> {
        fn exchange<S: std::io::Read + Write>(mut stream: S, command: &str) -> Option<String> {
            stream.write_all(command.as_bytes()).ok()?;
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).ok()?;
            Some(line)
        }
        let timeout = Some(common::io_timeout());
        match self {
            Endpoint::Unix(path) => {
                let stream = UnixStream::connect(path).ok()?;
                stream.set_read_timeout(timeout).unwrap();
                exchange(stream, command)
            }
            Endpoint::Tcp(address) => {
                let stream = TcpStream::connect_timeout(address, Duration::from_secs(2)).ok()?;
                stream.set_read_timeout(timeout).unwrap();
                exchange(stream, command)
            }
        }
    }
}

fn wait_until_serving(child: &mut Child, endpoint: &Endpoint) {
    let deadline = Instant::now() + common::io_timeout();
    while endpoint.first_line("PING\n").is_none() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before serving {endpoint:?}: {status}");
        }
        assert!(Instant::now() < deadline, "{endpoint:?} never answered");
        thread::sleep(Duration::from_millis(20));
    }
}

fn stop(mut child: Child) -> Output {
    // SAFETY: the child is a daemon started by this test.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let deadline = Instant::now() + common::io_timeout();
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn rrdcached_listeners_take_the_options_before_them_like_upstream() {
    if !upstream_available("rrdcached multi-listener differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = std::fs::canonicalize(dir.path()).unwrap().join(name);
        let root = run.join("data");
        std::fs::create_dir_all(&root).unwrap();
        // Rondi creates the missing socket directory, as upstream does.
        let first = run.join("sockets/first.sock");
        let second = run.join("second.sock");
        let tcp: SocketAddr = format!("127.0.0.1:{}", free_tcp_port()).parse().unwrap();
        let mut command = Command::new(daemon(name, &run));
        command
            .args(["-g", "-B", "-b", root.to_str().unwrap()])
            .arg("-p")
            .arg(run.join("rrdcached.pid"))
            .args(["-m", "0600", "-P", "PING", "-l"])
            .arg(format!("unix:{}", first.display()))
            .args(["-m", "0640", "-P", "PING,STATS", "-l"])
            .arg(&second)
            .args(["-l", &tcp.to_string()]);
        let mut child = command
            // Options after the last listener apply to nothing.
            .args(["-m", "0666", "-P", "UPDATE"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let endpoints = [
            Endpoint::Unix(first.clone()),
            Endpoint::Unix(second.clone()),
            Endpoint::Tcp(tcp),
        ];
        for endpoint in &endpoints {
            wait_until_serving(&mut child, endpoint);
        }
        let mut replies = vec![mode(&first), mode(&second)]
            .into_iter()
            .map(|mode| format!("{mode:o}"))
            .collect::<Vec<_>>();
        for endpoint in &endpoints {
            for command in ["PING\n", "STATS\n", "UPDATE missing.rrd 1:1\n"] {
                replies.push(format!("{:?}", endpoint.first_line(command)));
            }
        }
        let output = stop(child);
        replies.push(String::from_utf8_lossy(&output.stderr).into_owned());
        observed.push(replies);
    }
    assert_eq!(observed[0][..2], ["600", "640"]);
    assert_eq!(
        observed[0][2..5],
        [
            "Some(\"0 PONG\\n\")",
            "Some(\"-1 Permission denied.\\n\")",
            "Some(\"-1 Permission denied.\\n\")",
        ]
    );
    assert_eq!(observed[1], observed[0]);
}

#[test]
fn rrdcached_bracketed_ipv6_listener_matches_upstream() {
    if !upstream_available("rrdcached [v6]:port differential") {
        return;
    }
    // Whether [::1] resolves under AI_ADDRCONFIG depends on the host, so the
    // outcome (served reply, or exit status and stderr) is compared.
    let dir = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = std::fs::canonicalize(dir.path()).unwrap().join(name);
        std::fs::create_dir_all(&run).unwrap();
        let port = free_tcp_port();
        let mut child = Command::new(daemon(name, &run))
            .args(["-g", "-b", run.to_str().unwrap(), "-P", "PING"])
            .args(["-l", &format!("[::1]:{port}"), "-p"])
            .arg(run.join("rrdcached.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let endpoint = Endpoint::Tcp(format!("[::1]:{port}").parse().unwrap());
        let deadline = Instant::now() + common::io_timeout();
        let outcome = loop {
            if let Some(reply) = endpoint.first_line("STATS\n") {
                stop(child);
                break format!("served {reply:?}");
            }
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                break format!(
                    "exited {:?} {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                Instant::now() < deadline,
                "{name} neither served nor exited"
            );
            thread::sleep(Duration::from_millis(20));
        };
        observed.push(outcome);
    }
    assert_eq!(observed[1], observed[0]);
}

#[test]
fn rrdcached_dash_capital_l_listens_on_every_interface_like_upstream() {
    if !upstream_available("rrdcached -L differential") {
        return;
    }
    // -L always uses the default port, so the test needs it free.
    if std::net::TcpListener::bind("0.0.0.0:42217").is_err() {
        oracle_skip!("skipping rrdcached -L differential: port 42217 is in use");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = std::fs::canonicalize(dir.path()).unwrap().join(name);
        let root = run.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let mut child = Command::new(daemon(name, &run))
            .args(["-g", "-b", root.to_str().unwrap()])
            .arg("-p")
            .arg(run.join("rrdcached.pid"))
            .args(["-P", "PING", "-L"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let endpoint = Endpoint::Tcp("127.0.0.1:42217".parse().unwrap());
        wait_until_serving(&mut child, &endpoint);
        let replies = (
            endpoint.first_line("PING\n"),
            endpoint.first_line("STATS\n"),
            Endpoint::Tcp("[::1]:42217".parse().unwrap()).first_line("PING\n"),
        );
        let output = stop(child);
        observed.push((
            replies,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
        // The port must be released before the next daemon binds it.
        let deadline = Instant::now() + common::io_timeout();
        while TcpStream::connect("127.0.0.1:42217").is_ok() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
    assert_eq!(observed[0].0.0.as_deref(), Some("0 PONG\n"));
    assert_eq!(observed[0].0.1.as_deref(), Some("-1 Permission denied.\n"));
    assert_eq!(observed[1], observed[0]);
}

#[test]
fn rrdcached_listener_address_errors_match_upstream() {
    if !upstream_available("rrdcached listener error differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    for (case, listeners) in [
        ("malformed", vec!["-l", "[::1"]),
        ("garbage", vec!["-l", "[::1]x"]),
        ("unresolvable", vec!["-l", "rondi-no-such-host.invalid:5"]),
        ("partial", vec!["-l", "[::1", "-l", "[::1]x"]),
    ] {
        let mut outputs = Vec::new();
        for name in ["upstream", "rondi"] {
            let run = dir.join(case).join(name);
            std::fs::create_dir_all(&run).unwrap();
            let pid_file = run.join("rrdcached.pid");
            let output = Command::new(daemon(name, &run))
                .args(["-g", "-b", run.to_str().unwrap()])
                .arg("-p")
                .arg(&pid_file)
                .args(&listeners)
                .output()
                .unwrap();
            assert!(!pid_file.exists(), "{case} {name} left its pid file");
            outputs.push((
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        assert_eq!(outputs[0].0, Some(1), "{case}");
        assert!(
            outputs[0]
                .2
                .ends_with("rrdcached: FATAL: cannot open any listen sockets\nrrdcached: daemonize failed, exiting.\n"),
            "{case}: {}",
            outputs[0].2
        );
        assert_eq!(outputs[1], outputs[0], "{case}");
    }
}

#[test]
fn rrdcached_valid_permission_clears_an_earlier_option_error_like_upstream() {
    if !upstream_available("rrdcached -P status differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = std::fs::canonicalize(dir.path()).unwrap().join(name);
        let root = run.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let socket = run.join("rrdcached.sock");
        // read_options stores each permission's result in its one status,
        // so the valid PING clears both the -w error and the unknown FOO.
        let mut child = Command::new(daemon(name, &run))
            .args(["-g", "-b", root.to_str().unwrap(), "-w", "bad"])
            .args(["-P", "FOO,PING", "-l"])
            .arg(&socket)
            .arg("-p")
            .arg(run.join("rrdcached.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let endpoint = Endpoint::Unix(socket);
        wait_until_serving(&mut child, &endpoint);
        let reply = endpoint.first_line("STATS\n");
        let output = stop(child);
        observed.push((reply, String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    assert_eq!(observed[0].0.as_deref(), Some("-1 Permission denied.\n"));
    assert_eq!(observed[1], observed[0]);
}

/// Run `daemon` as an unprivileged account: the current one when not root,
/// otherwise the `rondi-test` user the CI image creates.
fn unprivileged(command: &mut Command) -> Option<(libc::uid_t, libc::gid_t)> {
    use std::os::unix::process::CommandExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        // SAFETY: as above.
        return Some(unsafe { (libc::geteuid(), libc::getegid()) });
    }
    // SAFETY: the name is NUL terminated; the result is libc-owned data
    // copied before the next lookup.
    let user = unsafe { libc::getpwnam(c"rondi-test".as_ptr()) };
    if user.is_null() {
        return None;
    }
    // SAFETY: non-null passwd entry.
    let (uid, gid) = unsafe { ((*user).pw_uid, (*user).pw_gid) };
    command.uid(uid).gid(gid);
    Some((uid, gid))
}

#[test]
fn rrdcached_failed_privilege_change_exits_without_serving_like_upstream() {
    if !upstream_available("rrdcached privilege failure differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    // setgid(0) and setuid(0) fail with EPERM for an unprivileged caller.
    for (case, option) in [("setgid", ["-G", "0"]), ("setuid", ["-U", "0"])] {
        let mut outputs = Vec::new();
        for name in ["upstream", "rondi"] {
            let run = dir.join(case).join(name);
            std::fs::create_dir_all(&run).unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.join(case), std::fs::Permissions::from_mode(0o777))
                    .unwrap();
                std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
            }
            let socket = run.join("rrdcached.sock");
            let pid_file = run.join("rrdcached.pid");
            let mut command = Command::new(daemon(name, &run));
            if unprivileged(&mut command).is_none() {
                panic!("running as root needs the rondi-test user from docker/Dockerfile.test");
            }
            let output = command
                .args(["-g", "-b", run.to_str().unwrap()])
                .args(option)
                .arg("-l")
                .arg(&socket)
                .arg("-p")
                .arg(&pid_file)
                .output()
                .unwrap();
            // The listener was bound before the change failed; nothing may
            // answer on it once the process has exited.
            assert!(Endpoint::Unix(socket).first_line("PING\n").is_none());
            assert!(!pid_file.exists(), "{case} {name} left its pid file");
            outputs.push((
                output.status.code(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        assert_eq!(
            outputs[0],
            (
                Some(1),
                String::new(),
                format!("daemonize: failed to {case}(0)\nrrdcached: daemonize failed, exiting.\n"),
            ),
            "{case}"
        );
        assert_eq!(outputs[1], outputs[0], "{case}");
    }
}

#[test]
fn rrdcached_refuses_to_start_when_a_later_listener_fails() {
    // Upstream serves the listeners that opened. Rondi stops instead, so a
    // configured listener (and its -P scope) is never silently missing.
    let dir = tempfile::tempdir().unwrap();
    let run = std::fs::canonicalize(dir.path()).unwrap();
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let socket = run.join("first.sock");
    let pid_file = run.join("rrdcached.pid");
    let output = Command::new(daemon("rondi", &run))
        .args(["-g", "-b", run.to_str().unwrap(), "-l"])
        .arg(&socket)
        .args(["-l", &format!("127.0.0.1:{port}"), "-p"])
        .arg(&pid_file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "rrdcached: bind(127.0.0.1:{port}) failed: Address already in use.\nrrdcached: daemonize failed, exiting.\n"
        )
    );
    assert!(!socket.exists(), "the first socket was left behind");
    assert!(Endpoint::Unix(socket).first_line("PING\n").is_none());
    assert!(!pid_file.exists());
    drop(busy);
}

/// BATCH reply for one command sent inside a batch.
fn batch_reply(endpoint: &Endpoint, command: &str) -> Option<String> {
    let Endpoint::Unix(path) = endpoint else {
        unreachable!("batch test uses Unix sockets")
    };
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(common::io_timeout())).unwrap();
    stream
        .write_all(format!("BATCH\n{command}\n.\n").as_bytes())
        .ok()?;
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    for _ in 0..3 {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        reply.push_str(&line);
        if line.starts_with("0 errors") {
            break;
        }
    }
    Some(reply)
}

#[test]
fn rrdcached_batch_commands_use_the_permissions_of_their_listener_like_upstream() {
    if !upstream_available("rrdcached per-listener BATCH permission differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut observed = Vec::new();
    for name in ["upstream", "rondi"] {
        let run = std::fs::canonicalize(dir.path()).unwrap().join(name);
        let root = run.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let narrow = run.join("narrow.sock");
        let wide = run.join("wide.sock");
        let mut child = Command::new(daemon(name, &run))
            .args(["-g", "-B", "-b", root.to_str().unwrap()])
            .arg("-p")
            .arg(run.join("rrdcached.pid"))
            .args(["-P", "PING,BATCH", "-l"])
            .arg(&narrow)
            .args(["-P", "PING,BATCH,UPDATE,FORGET", "-l"])
            .arg(&wide)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let narrow = Endpoint::Unix(narrow);
        let wide = Endpoint::Unix(wide);
        wait_until_serving(&mut child, &narrow);
        wait_until_serving(&mut child, &wide);
        let mut replies = Vec::new();
        for endpoint in [&narrow, &wide] {
            for command in [
                "UPDATE missing.rrd 1:1",
                "update missing.rrd 1:1",
                "FORGET missing.rrd",
                "QUIT",
            ] {
                replies.push(batch_reply(endpoint, command));
            }
        }
        stop(child);
        observed.push(replies);
    }
    assert_eq!(
        observed[0][0].as_deref(),
        Some("0 Go ahead.  End with dot '.' on its own line.\n1 errors\n1 Permission denied.\n")
    );
    // The narrow listener's replies match byte for byte. On the wide one the
    // commands pass the permission check; the missing-file text that follows
    // is a separate known difference (RD-007).
    assert_eq!(observed[1][..4], observed[0][..4]);
    for replies in &observed {
        for reply in &replies[4..] {
            assert!(!reply.as_deref().unwrap().contains("Permission denied"));
        }
    }
}

#[test]
fn rrdcached_socket_setup_never_follows_a_swapped_path() {
    // Another process keeps swapping a symlink to a victim file in and out
    // of the socket path while the daemon starts with -m. The chmod happens
    // in a private directory, so the victim keeps its mode.
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let run = std::fs::canonicalize(dir.path()).unwrap();
    let victim = run.join("victim");
    std::fs::write(&victim, "").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();
    let socket = run.join("rrdcached.sock");
    let stop_racing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let racer = {
        let stop_racing = std::sync::Arc::clone(&stop_racing);
        let (victim, socket) = (victim.clone(), socket.clone());
        thread::spawn(move || {
            while !stop_racing.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::remove_file(&socket);
                let _ = std::os::unix::fs::symlink(&victim, &socket);
            }
        })
    };
    for attempt in 0..20 {
        let mut child = Command::new(daemon("rondi", &run))
            .args(["-g", "-b", run.to_str().unwrap(), "-m", "0666", "-l"])
            .arg(&socket)
            .arg("-p")
            .arg(run.join(format!("rrdcached-{attempt}.pid")))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        let _ = child.kill();
        child.wait().unwrap();
        assert_eq!(mode(&victim), 0o600, "attempt {attempt} changed the victim");
    }
    stop_racing.store(true, std::sync::atomic::Ordering::Relaxed);
    racer.join().unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "");
}

#[test]
fn rrdcached_refuses_linked_pid_and_log_files() {
    let dir = tempfile::tempdir().unwrap();
    let run = std::fs::canonicalize(dir.path()).unwrap();
    let victim = run.join("victim");
    std::fs::write(&victim, "2147483647\n").unwrap();
    let socket = run.join("rrdcached.sock");

    // A stale pid file that is a hard link to another file is not replaced.
    let pid_file = run.join("hard.pid");
    std::fs::hard_link(&victim, &pid_file).unwrap();
    let output = Command::new(daemon("rondi", &run))
        .args(["-g", "-b", run.to_str().unwrap(), "-l"])
        .arg(&socket)
        .arg("-p")
        .arg(&pid_file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "rrdcached: can't open pid file '{}' (2 links)\nFATAL: Fail to create/open PID file \n",
            pid_file.display()
        )),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A symlinked pid file is not followed.
    let pid_link = run.join("link.pid");
    std::os::unix::fs::symlink(&victim, &pid_link).unwrap();
    let output = Command::new(daemon("rondi", &run))
        .args(["-g", "-b", run.to_str().unwrap(), "-l"])
        .arg(&socket)
        .arg("-p")
        .arg(&pid_link)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!socket.exists());

    // A hard-linked log file is refused before anything is written.
    let log = run.join("rrdcached.log");
    std::fs::hard_link(&victim, &log).unwrap();
    let output = Command::new(daemon("rondi", &run))
        .args(["-g", "-o"])
        .arg(&log)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        format!(
            "Failed to open log file '{}': Too many links\n",
            log.display()
        )
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "2147483647\n");
}

#[test]
fn rrdcached_pid_file_diagnostics_match_upstream() {
    if !upstream_available("rrdcached pid file differential") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let live = std::process::id().to_string();
    // (case, existing pid file contents, daemon starts)
    for (case, contents, starts) in [
        ("live", format!("{live}\n"), false),
        ("empty", String::new(), false),
        ("corrupt", "12x\n".to_owned(), false),
        ("zero", "0\n".to_owned(), false),
        ("stale", "2147483647\n".to_owned(), true),
    ] {
        let mut outputs = Vec::new();
        for name in ["upstream", "rondi"] {
            let run = base.join(case).join(name);
            std::fs::create_dir_all(&run).unwrap();
            let pid_file = run.join("rrdcached.pid");
            std::fs::write(&pid_file, &contents).unwrap();
            let socket = run.join("rrdcached.sock");
            let mut child = Command::new(daemon(name, &run))
                .args(["-g", "-b", run.to_str().unwrap(), "-l"])
                .arg(&socket)
                .arg("-p")
                .arg(&pid_file)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let output = if starts {
                wait_until_serving(&mut child, &Endpoint::Unix(socket));
                stop(child)
            } else {
                child.wait_with_output().unwrap()
            };
            outputs.push((
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).replace(&*run.to_string_lossy(), "RUN"),
            ));
        }
        if !starts {
            assert_eq!(outputs[0].0, Some(1), "{case}");
        }
        assert_eq!(outputs[1], outputs[0], "{case}");
    }
}
