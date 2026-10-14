#![cfg(unix)]
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::process::Command;

fn alias(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &path).unwrap();
    path
}

fn seed(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("seed.rrd");
    rondi::create_rrd_file(
        &path,
        1_000_000_000,
        10,
        &["DS:v:GAUGE:20:U:U".to_owned()],
        &["RRA:AVERAGE:0.5:1:8".to_owned()],
        true,
    )
    .unwrap();
    path
}

#[test]
fn dump_to_fifo_fails_promptly_without_a_reader() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let seed = seed(dir.path());
    let fifo = dir.path().join("output");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let mut child = Command::new(alias(dir.path(), "rrdtool"))
        .args(["dump", seed.to_str().unwrap(), fifo.to_str().unwrap()])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("dump blocked on a FIFO");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn resize_preserves_a_precreated_destination() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    seed(dir.path());
    let output = dir.path().join("resize.rrd");
    std::fs::write(&output, b"keep").unwrap();
    let result = Command::new(alias(dir.path(), "rrdtool"))
        .current_dir(dir.path())
        .args(["resize", "seed.rrd", "0", "GROW", "2"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(std::fs::read(output).unwrap(), b"keep");
}

#[test]
fn force_restore_write_failure_keeps_an_existing_destination() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let seed = seed(dir.path());
    let xml = dir.path().join("seed.xml");
    std::fs::write(&xml, rondi::dump_rrd_file(seed).unwrap()).unwrap();
    let output = dir.path().join("existing.rrd");
    std::fs::write(&output, b"keep").unwrap();
    let mut command = Command::new(alias(dir.path(), "rrdtool"));
    command.args([
        "restore",
        "-f",
        xml.to_str().unwrap(),
        output.to_str().unwrap(),
    ]);
    // Only the child receives this file-size limit and signal disposition.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            Ok(())
        });
    }
    let result = command.output().unwrap();
    assert!(!result.status.success());
    assert!(output.exists());
    assert_eq!(std::fs::read(output).unwrap(), b"keep");
}

#[test]
fn root_without_base_confinement_never_publishes_a_listener() {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let socket = dir.path().join("daemon.sock");
    let daemon = alias(dir.path(), "rrdcached");
    for extra in [vec![], vec!["-B"]] {
        let result = Command::new(&daemon)
            .arg("-g")
            .args(extra)
            .arg("-l")
            .arg(&socket)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("refuses to run as root"));
        assert!(!socket.exists());
    }
}

#[test]
fn unprivileged_daemon_rejects_a_nonsticky_shared_socket_directory() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let run = dir.path().join("shared");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
    let socket = run.join("daemon.sock");
    let mut command = Command::new(alias(dir.path(), "rrdcached"));
    if unsafe { libc::geteuid() } == 0 {
        let name = std::ffi::CString::new(run.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(name.as_ptr(), 65534, 65534) }, 0);
        command.uid(65534).gid(65534);
    }
    let result = command.args(["-g", "-l"]).arg(&socket).output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("refusing socket directory"));
    assert!(!socket.exists());
}
