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
    command.uid(65534).gid(65534);
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
