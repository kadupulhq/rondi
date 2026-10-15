#![cfg(unix)]
//! rrdcached startup options and process signals compared with the pinned
//! upstream daemon.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn upstream_rrdcached_available() -> bool {
    ["rrdcached", "rrdtool"].iter().all(|tool| {
        Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// The program to run: upstream `rrdcached` or a `rrdcached` symlink to
/// Rondi inside `work`.
fn rrdcached_program(work: &Path, rondi: bool) -> std::ffi::OsString {
    if !rondi {
        return "rrdcached".into();
    }
    let alias = work.join("rrdcached");
    if !alias.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &alias).unwrap();
    }
    alias.into_os_string()
}

/// CI runs as root, where both daemons refuse to serve unconfined; run them
/// as nobody and hand that account the fixtures.
fn run_unprivileged(command: &mut Command, work: &Path) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    use std::os::unix::process::CommandExt;
    give_to_nobody(work);
    command.uid(65534).gid(65534);
}

fn give_to_nobody(work: &Path) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    fn own_fixture(path: &Path) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::lchown(name.as_ptr(), 65534, 65534) }, 0);
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                own_fixture(&entry.unwrap().path());
            }
        }
    }
    own_fixture(work);
}

fn wait_for_socket(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + common::io_timeout();
    while Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("rrdcached exited during startup: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("rrdcached did not create {}", socket.display());
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + common::io_timeout();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("rrdcached did not exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn exchange(socket: &Path, script: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(common::io_timeout())).unwrap();
    stream.write_all(script.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn create_rrd(path: &Path) {
    let created = Command::new("rrdtool")
        .arg("create")
        .arg(path)
        .args(["--start", "1000000000", "--step", "300"])
        .args(["DS:v:GAUGE:600:U:U", "RRA:LAST:0.5:1:10"])
        .output()
        .unwrap();
    assert!(created.status.success());
}

fn lastupdate(path: &Path) -> String {
    let output = Command::new("rrdtool")
        .arg("lastupdate")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[derive(Debug, PartialEq)]
struct SignalOutcome {
    survived_with: Option<String>,
    status: Option<i32>,
    lastupdate: String,
    pid_file: bool,
    socket: bool,
    journals: Option<usize>,
}

fn signal_outcome(rondi: bool, signal: libc::c_int, options: &[&str]) -> SignalOutcome {
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    let base = work.join("b");
    std::fs::create_dir(&base).unwrap();
    create_rrd(&base.join("a.rrd"));
    let journal = work.join("j");
    let socket = work.join("s.sock");
    let pid_file = work.join("rrdcached.pid");
    let mut command = Command::new(rrdcached_program(&work, rondi));
    command
        .arg("-g")
        .arg("-p")
        .arg(&pid_file)
        .arg("-b")
        .arg(&base)
        .args(["-w", "3600", "-f", "7200"]);
    for option in options {
        if *option == "-j" {
            std::fs::create_dir(&journal).unwrap();
            command.arg("-j").arg(&journal);
        } else {
            command.arg(option);
        }
    }
    command
        .arg("-l")
        .arg(format!("unix:{}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_unprivileged(&mut command, &work);
    let mut child = command.spawn().unwrap();
    wait_for_socket(&mut child, &socket);
    let updated = exchange(&socket, "UPDATE a.rrd 1000000600:2\nQUIT\n");
    assert!(updated.starts_with("0 "), "{updated}");
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
    // A signal that only gets logged must leave the daemon serving.
    let survived_with = if matches!(signal, libc::SIGUSR1 | libc::SIGUSR2) {
        None
    } else {
        thread::sleep(Duration::from_millis(300));
        assert!(child.try_wait().unwrap().is_none(), "rrdcached exited");
        let pong = exchange(&socket, "PING\nQUIT\n");
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        Some(pong)
    };
    let status = wait_for_exit(&mut child);
    SignalOutcome {
        survived_with,
        status: status.code(),
        lastupdate: lastupdate(&base.join("a.rrd")),
        pid_file: pid_file.exists(),
        socket: socket.exists(),
        journals: options
            .contains(&"-j")
            .then(|| std::fs::read_dir(&journal).unwrap().count()),
    }
}

#[test]
fn rrdcached_usr1_usr2_and_hup_match_upstream() {
    if !upstream_rrdcached_available() {
        oracle_skip!("skipping rrdcached signal differential: upstream tools are not installed");
        return;
    }
    common::require_oracle();
    for options in [&[][..], &["-j"], &["-j", "-F"]] {
        for (name, signal) in [
            ("USR1", libc::SIGUSR1),
            ("USR2", libc::SIGUSR2),
            ("HUP", libc::SIGHUP),
        ] {
            let upstream = signal_outcome(false, signal, options);
            let observed = signal_outcome(true, signal, options);
            assert_eq!(observed, upstream, "SIG{name} with {options:?}");
            assert!(!upstream.pid_file && !upstream.socket);
            // USR1 flushes at shutdown, USR2 never does, and HUP leaves the
            // -j/-F choice to the following SIGTERM.
            let flushed = match signal {
                libc::SIGUSR1 => true,
                libc::SIGUSR2 => false,
                _ => options == ["-j", "-F"] || options.is_empty(),
            };
            assert_eq!(
                upstream.lastupdate.ends_with("1000000600: 2\n"),
                flushed,
                "SIG{name} with {options:?}: {}",
                upstream.lastupdate
            );
        }
    }
}

fn startup_outcome(work: &Path, rondi: bool, args: &[&str]) -> common::Outcome {
    let mut command = Command::new(rrdcached_program(work, rondi));
    command
        .arg("-g")
        .args(args)
        .arg("-l")
        .arg(format!("unix:{}", work.join("s.sock").display()))
        .current_dir(work)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_unprivileged(&mut command, work);
    let mut child = command.spawn().unwrap();
    // A daemon that wrongly starts is stopped and reported as still running
    // (no status) instead of hanging the test.
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            wait_for_exit(&mut child);
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    common::Outcome {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn rrdcached_base_directory_validation_matches_upstream() {
    use std::os::unix::fs::PermissionsExt;
    if !upstream_rrdcached_available() {
        oracle_skip!("skipping rrdcached -b differential: upstream tools are not installed");
        return;
    }
    common::require_oracle();
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(work.join("real/base")).unwrap();
    std::os::unix::fs::symlink(work.join("real"), work.join("link")).unwrap();
    std::fs::write(work.join("file"), b"").unwrap();
    std::fs::create_dir(work.join("sealed")).unwrap();
    give_to_nobody(&work);
    std::fs::set_permissions(work.join("sealed"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let path = |name: &str| work.join(name).to_str().unwrap().to_owned();
    // Under root both daemons run as nobody, which cannot write to sealed.
    let cases = [
        "rel".to_owned(),
        ".".to_owned(),
        "/".to_owned(),
        "//".to_owned(),
        path("link/base"),
        path("real/../real/base"),
        path("file"),
        path("file/below"),
        path("sealed/new"),
    ];
    for base in &cases {
        let upstream = startup_outcome(&work, false, &["-b", base]);
        let observed = startup_outcome(&work, true, &["-b", base]);
        assert_eq!(observed, upstream, "-b {base}");
        assert!(
            matches!(upstream.status, Some(3..=5)),
            "-b {base}: {upstream:?}"
        );
        assert!(!work.join("s.sock").exists(), "-b {base}");
    }
}

#[test]
fn rrdcached_unwritable_base_directory_error_is_plain_and_leaves_no_socket() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    let base = work.join("base");
    std::fs::create_dir(&base).unwrap();
    give_to_nobody(&work);
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Rondi's ownership lock needs write access to -b, which upstream does
    // not, so this is checked against Rondi alone.
    let pid = work.join("pid");
    let observed = startup_outcome(
        &work,
        true,
        &["-p", pid.to_str().unwrap(), "-b", base.to_str().unwrap()],
    );
    assert_eq!(observed.status, Some(1), "{observed:?}");
    assert!(
        observed.stderr.ends_with(&format!(
            "rrdcached: cannot lock base directory '{}': I/O error: Permission denied (os error 13)\n",
            base.display()
        )),
        "{observed:?}"
    );
    assert!(!work.join("s.sock").exists());
    assert!(!pid.exists());
}

#[test]
fn rrdcached_unknown_permission_keeps_its_spelling_like_upstream() {
    if !upstream_rrdcached_available() {
        oracle_skip!("skipping rrdcached -P differential: upstream tools are not installed");
        return;
    }
    common::require_oracle();
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    // Each list ends with an unknown name so both daemons stop with status 4.
    for permissions in ["nosuch", "ping,Bogus", "Flush nosuch,pInG,Nope"] {
        let upstream = startup_outcome(&work, false, &["-P", permissions]);
        let observed = startup_outcome(&work, true, &["-P", permissions]);
        assert_eq!(observed, upstream, "-P {permissions}");
        assert_eq!(upstream.status, Some(4), "-P {permissions}: {upstream:?}");
    }
}

#[test]
fn rrdcached_pid_directory_failure_matches_upstream() {
    use std::os::unix::fs::PermissionsExt;
    if !upstream_rrdcached_available() {
        oracle_skip!("skipping rrdcached -p differential: upstream tools are not installed");
        return;
    }
    common::require_oracle();
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(work.join("file"), b"").unwrap();
    std::fs::create_dir(work.join("sealed")).unwrap();
    give_to_nobody(&work);
    std::fs::set_permissions(work.join("sealed"), std::fs::Permissions::from_mode(0o555)).unwrap();
    for pid in [work.join("sealed/sub/pid"), work.join("file/sub/pid")] {
        let pid = pid.to_str().unwrap();
        let upstream = startup_outcome(&work, false, &["-p", pid]);
        let observed = startup_outcome(&work, true, &["-p", pid]);
        assert_eq!(observed, upstream, "-p {pid}");
        assert!(
            upstream.stderr.ends_with(
                "FATAL: Fail to create/open PID file \nrrdcached: daemonize failed, exiting.\n"
            ),
            "-p {pid}: {upstream:?}"
        );
        assert!(!work.join("s.sock").exists());
    }
}

#[test]
fn rrdcached_foreground_writes_nothing_to_stdout_like_upstream() {
    if !upstream_rrdcached_available() {
        oracle_skip!("skipping rrdcached -g stdout differential: upstream tools are not installed");
        return;
    }
    common::require_oracle();
    let mut outputs = Vec::new();
    for rondi in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(dir.path()).unwrap();
        let base = work.join("b");
        std::fs::create_dir(&base).unwrap();
        create_rrd(&base.join("a.rrd"));
        let socket = work.join("s.sock");
        let mut command = Command::new(rrdcached_program(&work, rondi));
        command
            .arg("-g")
            .arg("-p")
            .arg(work.join("pid"))
            .arg("-b")
            .arg(&base)
            .arg("-l")
            .arg(format!("unix:{}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        run_unprivileged(&mut command, &work);
        let mut child = command.spawn().unwrap();
        wait_for_socket(&mut child, &socket);
        let replies = exchange(
            &socket,
            "PING\nUPDATE a.rrd 1000000600:2\nFLUSH a.rrd\nSTATS\nQUIT\n",
        );
        assert!(replies.starts_with("0 PONG\n"), "{replies}");
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        wait_for_exit(&mut child);
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        outputs.push(stdout);
    }
    assert_eq!(outputs[1], outputs[0]);
    assert_eq!(outputs[0], "");
}

#[test]
fn rrdcached_creates_missing_base_directories_without_access_for_others() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    let base = work.join("new/base");
    // -b is created as it is parsed; the invalid -t then stops startup.
    let observed = startup_outcome(&work, true, &["-b", base.to_str().unwrap(), "-t", "0"]);
    assert_eq!(observed.status, Some(1), "{observed:?}");
    for created in [work.join("new"), base] {
        let mode = std::fs::metadata(&created).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o007, 0, "{} is {mode:o}", created.display());
    }
}

#[test]
fn rrdcached_files_are_never_group_or_world_writable_under_umask_zero() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let work = std::fs::canonicalize(dir.path()).unwrap();
    let mode = |path: &Path| {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    let pid_file = work.join("run/pid/rrdcached.pid");
    let log_file = work.join("log");
    let base = work.join("new/base");
    let socket = work.join("sockets/s.sock");
    let mut command = Command::new(rrdcached_program(&work, true));
    command
        .arg("-g")
        .arg("-p")
        .arg(&pid_file)
        .arg("-o")
        .arg(&log_file)
        .arg("-b")
        .arg(&base)
        .arg("-l")
        .arg(format!("unix:{}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: umask is async-signal-safe and touches only the child.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0);
            Ok(())
        });
    }
    run_unprivileged(&mut command, &work);
    let mut child = command.spawn().unwrap();
    wait_for_socket(&mut child, &socket);
    // Rondi policy (RCD-13): the umask can only tighten these.
    for path in [
        pid_file.as_path(),
        pid_file.parent().unwrap(),
        work.join("run").as_path(),
        log_file.as_path(),
        socket.parent().unwrap(),
    ] {
        assert_eq!(
            mode(path) & 0o022,
            0,
            "{} is {:o}",
            path.display(),
            mode(path)
        );
    }
    assert!(
        std::fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&base), 0o750);
    assert_eq!(mode(base.parent().unwrap()), 0o750);
    // SAFETY: the child is a daemon started by this test.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    assert!(wait_for_exit(&mut child).success());
    assert!(!pid_file.exists() && !socket.exists());
}
