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

    /// `b/a.rrd`, `b/sub/d.rrd`, `b/sub/deep/f.rrd`, a directory named
    /// `b/dir.rrd`, and a fresh socket path.
    fn reset(&self) {
        let base = self.base();
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::create_dir_all(base.join("dir.rrd")).unwrap();
        for name in ["a.rrd", "sub/d.rrd", "sub/deep/f.rrd"] {
            std::fs::copy(self.root.join("baseline.rrd"), base.join(name)).unwrap();
        }
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

fn assert_same(observed: &[Vec<String>; 2]) {
    for (upstream, rondi) in observed[0].iter().zip(&observed[1]) {
        assert_eq!(rondi, upstream);
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

#[test]
fn create_arguments_parse_like_handle_request_create() {
    let Some(work) = Work::new() else { return };
    const D: &str = " DS:a:GAUGE:120:U:U RRA:AVERAGE:0.5:1:5";
    let sessions = [
        // atol values, prefix-matched options, and the -1 "now" sentinel.
        format!("CREATE n.rrd -b 1000000007 -s x{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -s 5m{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -s 60x{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -s 0{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -s -5{D}\n"),
        format!("CREATE n.rrd -b 1000000007x{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -bogus 1000000007{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -s 60 -step 120{D}\nLAST n.rrd\n"),
        format!("CREATE n.rrd -b 1000000007 -Ox{D}\nCREATE n.rrd -b 1000000007 -O{D}\n"),
        format!("CREATE n.rrd -b x{D}\n"),
        format!("CREATE n.rrd -b 100{D}\n"),
        format!("CREATE n.rrd -b 0{D}\n"),
        format!("CREATE n.rrd -b -5{D}\n"),
        format!("CREATE n.rrd -b 315359999{D}\n"),
        format!("CREATE n.rrd -b1000000000{D}\n"),
        format!("CREATE n.rrd -b -1{D}\n"),
        // Usage replies.
        format!("CREATE n.rrd --step 60{D}\n"),
        format!("CREATE n.rrd -z{D}\n"),
        format!("CREATE n.rrd -b 1000000000{D} junk\n"),
        format!("CREATE n.rrd -b  1000000000{D}\n"),
        "CREATE n.rrd -b 1000000000 -s\n".to_owned(),
        "CREATE n.rrd -b\n".to_owned(),
        "CREATE n.rrd -b 1000000000 -t\n".to_owned(),
        // rrd_create_r2 errors carry the RRD Error prefix.
        "CREATE n.rrd\n".to_owned(),
        "CREATE n.rrd DS:a:GAUGE:120:U:U\n".to_owned(),
        "CREATE n.rrd RRA:AVERAGE:0.5:1:5\n".to_owned(),
        "CREATE a.rrd -O\n".to_owned(),
        "CREATE n.rrd -b 1000000000 DS:a:BOGUS:1:2:3 RRA:AVERAGE:0.5:1:5\n".to_owned(),
        "CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U RRA:BOGUS:0.5:1:5\n".to_owned(),
        "CREATE n.rrd -b 1000000000 DS:a:GAUGE RRA:AVERAGE:0.5:1:5\n".to_owned(),
        "CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U RRA:AVERAGE:2:1:5\n".to_owned(),
        format!("CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U{D}\n"),
        format!("CREATE n.rrd -b 1000000000{D} RRA:\n"),
    ];
    let sessions = sessions.iter().map(String::as_bytes).collect::<Vec<_>>();
    let observed = work.differential(&["-B"], &sessions);
    assert!(observed[0][9].ends_with("\n-1 The first entry must be after 1980.\n"));
    assert!(
        observed[0][23]
            .ends_with("\n-1 RRD Error: you must define at least one Round Robin Archive\n")
    );
    assert_same(&observed);
}

#[test]
fn fetch_times_parse_like_handle_request_fetch_parse() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 12] = [
        b"FETCH a.rrd AVERAGE x 1000000600\n",
        b"FETCH a.rrd AVERAGE 1000000000 y\n",
        b"FETCH a.rrd BOGUS x\n",
        b"FETCH a.rrd AVERAGE 0x3B9ACA00 1000000600\n",
        b"FETCH a.rrd AVERAGE 1000000000abc 07346545000\n",
        b"FETCH a.rrd AVERAGE 99999999999999999999 1\n",
        b"FETCH a.rrd AVERAGE 1000000000 -99999999999999999999\n",
        b"FETCH a.rrd AVERAGE 1000000600 1000000000\n",
        b"FETCH a.rrd AVERAGE 1000000100 1000000000\n",
        b"FETCH a.rrd MAX 1000003000 1000000000\n",
        b"FETCHBIN a.rrd AVERAGE x\n",
        b"FETCHBIN a.rrd AVERAGE 1000000600 1000000000\n",
    ];
    let observed = work.differential(&["-B"], &sessions);
    assert!(
        observed[0][0]
            .ends_with("\n-1 Cannot parse start time `x': Only simple integers are allowed.\n")
    );
    assert!(observed[0][7].ends_with("\n-1 rrd_fetch_r failed: malloc fetch data area\n"));
    assert_same(&observed);

    // Without times the range is the last day before now, and values up to
    // zero count back from now.
    let before = now();
    let observed = work.differential(
        &["-B"],
        &[b"FETCH a.rrd AVERAGE\n", b"FETCH a.rrd AVERAGE -3600 0\n"],
    );
    let after = now();
    for replies in &observed {
        for (reply, span) in replies.iter().zip([86_400, 3_600]) {
            let header = |name: &str| {
                reply
                    .lines()
                    .find_map(|line| line.strip_prefix(name))
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or_else(|| panic!("no {name} in {reply:?}"))
            };
            let (start, end) = (header("Start: "), header("End: "));
            let aligned = |time: i64| time - time % 300;
            assert!(
                (aligned(before - span)..=aligned(after - span)).contains(&start),
                "{reply:?}"
            );
            assert!(
                (aligned(before) + 300..=aligned(after) + 300).contains(&end),
                "{reply:?}"
            );
        }
    }
}

#[test]
fn fetch_columns_select_like_handle_request_fetch_parse() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 8] = [
        b"FETCH a.rrd AVERAGE 1000000000 1000000600 x x\n",
        b"FETCH a.rrd AVERAGE 1000000000 1000000600 x y x\n",
        b"FETCH a.rrd AVERAGE 1000000000 1000000600 x y \n",
        b"FETCH a.rrd AVERAGE 1000000000 1000000600  y\n",
        b"FETCH a.rrd AVERAGE 1000000000 1000000600 y x\n",
        b"FETCH a.rrd AVERAGE 1000000000 1000000600 \n",
        b"FETCHBIN a.rrd AVERAGE 1000000000 1000000600 x x\n",
        b"FETCHBIN a.rrd AVERAGE 1000000000 1000000600 x y y\n",
    ];
    let observed = work.differential(&["-B"], &sessions);
    assert!(observed[0][0].ends_with("\n-1 field x already used\n"));
    assert!(observed[0][1].ends_with("\n-1 too many fields given - duplicates!\n"));
    assert_same(&observed);

    // Upstream prints the file name after freeing it, so only the prefix is
    // stable; Rondi prints the name it meant to.
    let observed = work.differential(&["-B"], &[b"FETCH a.rrd AVERAGE 1000000000 1000000600 z\n"]);
    let prefix = "\n-1 field z not found in ";
    assert!(observed[0][0].contains(prefix), "{:?}", observed[0][0]);
    assert!(
        observed[1][0].ends_with(&format!("{prefix}{}/a.rrd\n", work.base().display())),
        "{:?}",
        observed[1][0]
    );
}

#[test]
fn help_reads_only_its_first_field() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 4] = [
        b"HELP PING x\n",
        b"HELP update a b\n",
        b"HELP  PING\n",
        b"HELP bogus PING\n",
    ];
    let observed = work.differential(&[], &sessions);
    assert!(
        observed[0][0]
            .ends_with("]\n4 Help for PING\nUsage: PING\n\nPING given, PONG returned\n\n")
    );
    assert_same(&observed);
}

#[test]
fn suspend_and_resume_usage_close_the_connection() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 3] = [
        b"SUSPEND\nPING\n",
        b"RESUME\nPING\n",
        b"BATCH\nSUSPEND\nPING\n.\nPING\n",
    ];
    let observed = work.differential(&[], &sessions);
    assert_eq!(
        observed[0][0],
        "[SUSPEND\nPING\n]\n-1 Usage: SUSPEND <filename>\n"
    );
    assert_same(&observed);
}

#[test]
fn list_names_entries_relative_to_the_listed_path() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 14] = [
        b"LIST /sub\n",
        b"LIST /sub/\n",
        b"LIST RECURSIVE /sub\n",
        b"LIST RECURSIVE sub\n",
        b"LIST /sub/deep\n",
        b"LIST /a.rrd\n",
        b"LIST /sub/d.rrd\n",
        b"LIST RECURSIVE /sub/d.rrd\n",
        b"LIST /missing.rrd\n",
        b"LIST /dir.rrd\n",
        b"LIST /a.rrd/x\n",
        b"LIST /sub extra\n",
        b"LIST RECURSIVE\n",
        b"LIST sub\n",
    ];
    for options in [&["-B"][..], &[][..]] {
        let observed = work.differential(options, &sessions);
        let sub = &observed[0][0];
        assert!(
            sub.contains("]\n2 RRDs\n") && sub.contains("\nd.rrd\n"),
            "{sub:?}"
        );
        assert!(observed[0][5].ends_with("]\n1 RRDs\na.rrd\n"));
        assert_same(&observed);
    }
}

#[test]
fn empty_file_name_names_the_base_directory() {
    let Some(work) = Work::new() else { return };
    // Two spaces leave an empty file field, which get_abs_path turns into
    // "<base>/", inside -b by check_file_access's string test.
    let sessions: [&[u8]; 14] = [
        b"UPDATE  1000000300:1:2\n",
        b"INFO  \n",
        b"LAST  \n",
        b"FIRST  0\n",
        b"FLUSH  \n",
        b"FORGET  \n",
        b"PENDING  \n",
        b"FETCH  AVERAGE\n",
        b"FETCHBIN  AVERAGE\n",
        b"TUNE  2 tune -h\n",
        b"DUMP  \n",
        b"SUSPEND  \nPING\n",
        b"RESUME  \nPING\n",
        b"BATCH\nUPDATE  1000000300:1:2\n.\n",
    ];
    for options in [&["-B"][..], &[][..]] {
        let observed = work.differential(options, &sessions);
        assert_eq!(
            observed[0][0],
            format!(
                "[UPDATE  1000000300:1:2\n]\n-1 Not a regular file: {}/\n",
                work.base().display()
            )
        );
        assert_same(&observed);
    }

    // Upstream passes "<base>/" to rrd_create_r2; Rondi refuses it.
    work.reset();
    let mut daemon = work.start(true, &["-B"]);
    let reply = work.talk(b"CREATE  -b 1000000000 DS:a:GAUGE:120:U:U RRA:AVERAGE:0.5:1:5\n");
    unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) };
    daemon.wait().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&reply),
        format!("-1 {}/: Permission denied\n", work.base().display())
    );
    assert!(work.base().join("a.rrd").is_file());
}

#[test]
fn nul_separates_fields_like_a_space() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 8] = [
        b"PING\0junk\n",
        b"PING\0\n",
        b"HELP\0PING\n",
        b"HELP\0\0PING\n",
        b"\0PING\n",
        b"PI\\\0NG\n",
        b"LAST\0a.rrd\n",
        b"BATCH\nPING\0x\nNOPE\0x\n.\n",
    ];
    let observed = work.differential(&["-B"], &sessions);
    assert!(observed[0][0].ends_with("]\n0 PONG\n"));
    assert_same(&observed);
}

#[test]
fn tune_parses_options_like_rrd_tune_r() {
    let Some(work) = Work::new() else { return };
    let sessions: [&[u8]; 27] = [
        b"TUNE a.rrd 2 tune -h\n",
        b"TUNE a.rrd 3 tune -h z:1\n",
        b"TUNE a.rrd 2 tune -bogus\n",
        b"TUNE a.rrd 2 tune --bogus\n",
        b"TUNE a.rrd 2 tune --bogus=1\n",
        b"TUNE a.rrd 2 tune -q\n",
        b"TUNE a.rrd 2 tune --heartbeat\n",
        b"TUNE a.rrd 2 tune --heartbeat=z:5\n",
        b"TUNE a.rrd 3 tune --heartbeat z:5\n",
        b"TUNE a.rrd 3 tune -h x\n",
        b"TUNE a.rrd 3 tune -d z:GAUGE\n",
        b"TUNE a.rrd 3 tune -d x:BOGUS\n",
        b"TUNE a.rrd 3 tune -d x:gauge\n",
        b"TUNE a.rrd 3 tune -r z:q\n",
        b"TUNE a.rrd 3 tune -r x\n",
        b"TUNE a.rrd 3 tune -i z:1\n",
        b"TUNE a.rrd 3 tune -i x:abc\n",
        b"TUNE a.rrd 3 tune -a x\n",
        b"TUNE a.rrd 3 tune junk -h\n",
        b"TUNE a.rrd 2 tune junk\nINFO a.rrd\n",
        b"TUNE a.rrd 2 tune -hx:900\nINFO a.rrd\n",
        b"TUNE a.rrd 3 tune -h x:900abc\nINFO a.rrd\n",
        b"TUNE a.rrd 3 tune -i x:1abc\nINFO a.rrd\n",
        b"TUNE a.rrd 2 tune --maximum=y:5e3\nINFO a.rrd\n",
        b"TUNE a.rrd 5 tune -r x:q -h q:77\nINFO a.rrd\n",
        b"TUNE a.rrd 3 tune -- -h\nINFO a.rrd\n",
        b"TUNE a.rrd 4 tune -h x:900 -h z:1\nINFO a.rrd\n",
    ];
    let observed = work.differential(&["-B"], &sessions);
    assert!(observed[0][0].ends_with("]\n-1 Got error option requires an argument -- 'h'\n"));
    assert!(observed[0][1].ends_with("]\n-1 Got error unknown data source name 'z'\n"));
    assert!(observed[0][3].ends_with("]\n-1 Got error invalid option -- 'bogus'\n"));
    assert_same(&observed);
}

/// RD-015: upstream may drop requests sent just before a half-close,
/// depending on platform and scheduling. Rondi answers them every time.
#[test]
fn requests_before_a_half_close_are_answered() {
    let Some(work) = Work::new() else { return };
    work.reset();
    let mut daemon = work.start(true, &["-B"]);
    for _ in 0..10 {
        let mut stream = UnixStream::connect(work.root.join("s.sock")).unwrap();
        stream.set_read_timeout(Some(common::io_timeout())).unwrap();
        stream.write_all(b"PING\nPENDING a.rrd\n").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert_eq!(reply, "0 PONG\n0 updates pending\n");
    }
    // SAFETY: the child is still running and owned by this test.
    assert_eq!(
        unsafe { libc::kill(daemon.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    daemon.wait().unwrap();
}

/// Rules the protocol rewrite settled by black-box runs
/// (docs/compatibility/rrdcached.md, "Rules settled by black-box runs").
#[test]
fn settled_rules_match_upstream() {
    let Some(work) = Work::new() else { return };
    const D: &str = "DS:a:GAUGE:120:U:U RRA:AVERAGE:0.5:1:5";
    let sessions = [
        "UPDATE a.rrd 1000000310:1:2\nLAST a.rrd\n".to_owned(),
        "UPDATE\nFLUSH\nSTATS\n".to_owned(),
        "UPDATE sub/d.rrd\nSTATS\nPENDING sub/d.rrd\n".to_owned(),
        "UPDATE sub/deep/f.rrd \t1000000300:1:2\nUPDATE sub/deep/f.rrd inf:1\n\
         UPDATE sub/deep/f.rrd nan:1\nUPDATE sub/deep/f.rrd 1000000300.123456789:1\n\
         UPDATE sub/deep/f.rrd 1000000300.1234567891:1\n\
         UPDATE sub/deep/f.rrd 1000000300.1234567892:1\n"
            .to_owned(),
        "BATCH\nFLUSH a.rrd\n.\nSTATS\n".to_owned(),
        "CREATE n1.rrd DS:a:GAUGE:120:U:U -b 100 RRA:AVERAGE:0.5:1:5\n".to_owned(),
        "CREATE n2.rrd -b 1000000000 RRA:AVERAGE:0.5:1:5 DS:a:GAUGE:120:U:U -s 60\n\
         LAST n2.rrd\n"
            .to_owned(),
        format!("CREATE n3.rrd -b 1000000000 - {D}\n"),
        format!("CREATE n4.rrd -b 100 -z {D}\n"),
        format!("CREATE a.rrd/ -b 1000000000 {D}\n"),
        format!("CREATE missing.rrd/ -b 1000000000 {D}\n"),
        "TUNE missing.rrd 2 tune -q\n".to_owned(),
        // Upstream repeats an earlier failure's text for -b on the same
        // connection, so it runs first on its own (RC-90 note).
        "TUNE a.rrd 3 tune -b x\n".to_owned(),
        "TUNE a.rrd 3 tune -x 0.5\nTUNE a.rrd 3 tune -x 2\nTUNE a.rrd 3 tune -x abc\n\
         TUNE a.rrd 3 tune -s 1\nTUNE a.rrd 3 tune -f 29\nTUNE a.rrd 3 tune -p x\n\
         TUNE a.rrd 3 tune -D foo\nTUNE a.rrd 3 tune -t 0\n\
         TUNE a.rrd 3 tune --heart x:5\nTUNE a.rrd 3 tune -i x:U5\n\
         TUNE a.rrd 3 tune -h abcdefghijklmnopqrst:5\n"
            .to_owned(),
        "TUNE sub/d.rrd 3 tune -r x:abcdefghijklmnopqrstuvwxyz\nINFO sub/d.rrd\n".to_owned(),
        "TUNE sub/deep/f.rrd 3 tune -r x:bad!\nINFO sub/deep/f.rrd\n".to_owned(),
        "LIST RECURSIVE sub\nLIST /x..y\nLIST /sub/..\nLIST RECURSIVE missing\n".to_owned(),
    ];
    let sessions = sessions.iter().map(String::as_bytes).collect::<Vec<_>>();
    for options in [&["-B"][..], &[][..]] {
        let observed = work.differential(options, &sessions);
        assert!(
            observed[0][0].ends_with("\n0 1000000200\n"),
            "{}",
            observed[0][0]
        );
        assert_same(&observed);
    }
}
