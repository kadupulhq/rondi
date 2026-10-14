#![cfg(unix)]
//! Request-level differential tests of the rrdcached line protocol: each
//! session runs on its own connection against upstream rrdcached and against
//! the Rondi alias, started with the same options on identical fixtures.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Work {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Work {
    /// None when the oracle is missing and skipping is allowed.
    fn new() -> Option<Self> {
        let available = ["rrdcached", "rrdtool"].iter().all(|tool| {
            Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success())
        });
        if !available {
            oracle_skip!(
                "skipping rrdcached protocol differential: upstream tools are not installed"
            );
            return None;
        }
        common::require_oracle();
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let created = Command::new("rrdtool")
            .arg("create")
            .arg(root.join("baseline.rrd"))
            .args(["--start", "1000000000", "--step", "300"])
            .args(["DS:x:GAUGE:600:U:U", "DS:y:COUNTER:600:0:U"])
            .args(["RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.5:2:10"])
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), root.join("rrdcached")).unwrap();
        Some(Self { _temp: temp, root })
    }

    fn base(&self) -> PathBuf {
        self.root.join("b")
    }

    /// `b/a.rrd`, `b/sub/d.rrd` and a fresh socket path.
    fn reset(&self) {
        let base = self.base();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::copy(self.root.join("baseline.rrd"), base.join("a.rrd")).unwrap();
        std::fs::copy(self.root.join("baseline.rrd"), base.join("sub/d.rrd")).unwrap();
        let _ = std::fs::remove_file(self.root.join("s.sock"));
    }

    /// Upstream (`rondi == false`) or Rondi rrdcached on `-b b`. Under root
    /// both run as nobody, as the daemon-spawning tests in CI must.
    fn start(&self, rondi: bool, options: &[&str]) -> Child {
        let program: PathBuf = if rondi {
            self.root.join("rrdcached")
        } else {
            "rrdcached".into()
        };
        let mut command = Command::new(program);
        if unsafe { libc::geteuid() } == 0 {
            use std::os::unix::process::CommandExt;
            fn own(path: &Path) {
                let metadata = std::fs::symlink_metadata(path).unwrap();
                std::os::unix::fs::lchown(path, Some(65534), Some(65534)).unwrap();
                if metadata.is_dir() {
                    for entry in std::fs::read_dir(path).unwrap() {
                        own(&entry.unwrap().path());
                    }
                }
            }
            own(&self.root);
            command.uid(65534).gid(65534);
        }
        let socket = self.root.join("s.sock");
        let mut child = command
            .arg("-g")
            .arg("-p")
            .arg(self.root.join("rrdcached.pid"))
            .arg("-b")
            .arg(self.base())
            .args(["-w", "3600", "-f", "7200"])
            .args(options)
            .arg("-l")
            .arg(format!("unix:{}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + common::io_timeout();
        while UnixStream::connect(&socket).is_err() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("rrdcached exited during startup: {status}");
            }
            assert!(Instant::now() < deadline, "rrdcached did not listen");
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    /// Sends `session` then QUIT on one connection and reads until the
    /// daemon closes it, so a daemon that closes first is visible.
    fn talk(&self, session: &[u8]) -> Vec<u8> {
        let mut stream = UnixStream::connect(self.root.join("s.sock")).unwrap();
        stream.set_read_timeout(Some(common::io_timeout())).unwrap();
        stream.write_all(session).unwrap();
        let _ = stream.write_all(b"QUIT\n");
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        reply
    }

    /// Each session on a fresh fixture and connection, upstream first.
    fn differential(&self, options: &[&str], sessions: &[&[u8]]) -> [Vec<String>; 2] {
        [false, true].map(|rondi| {
            self.reset();
            let mut daemon = self.start(rondi, options);
            let replies = sessions
                .iter()
                .map(|session| {
                    format!(
                        "[{}]\n{}",
                        String::from_utf8_lossy(session),
                        String::from_utf8_lossy(&self.talk(session))
                    )
                })
                .collect();
            // SAFETY: the child is still running and owned by this test.
            assert_eq!(
                unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) },
                0
            );
            daemon.wait().unwrap();
            replies
        })
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn create_without_start_uses_ten_seconds_ago() {
    let Some(work) = Work::new() else { return };
    let before = now();
    let observed = work.differential(
        &[],
        &[b"CREATE n.rrd -s 1 DS:a:GAUGE:120:U:U RRA:AVERAGE:0.5:1:5\nLAST n.rrd\n"],
    );
    let after = now();
    for replies in &observed {
        let reply = &replies[0];
        let last = reply
            .strip_suffix('\n')
            .and_then(|reply| reply.rsplit_once("\n0 "))
            .and_then(|(head, last)| {
                head.ends_with("0 RRD created OK")
                    .then(|| last.parse::<i64>().ok())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("unexpected reply {reply:?}"));
        assert!(
            (before - 10..=after - 10).contains(&last),
            "last update {last} outside {}..={}",
            before - 10,
            after - 10
        );
    }
}
