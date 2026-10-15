//! Random command lines and pipe-mode sessions must never crash or hang
//! the front end (docs/compatibility/cli.md CLI-78, pipe-mode.md PM-26 to
//! PM-28). The generator is seeded, so a failure reproduces.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::symlink;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// xorshift64*: small, deterministic and good enough to pick words.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a>(&mut self, pool: &[&'a str]) -> &'a str {
        pool[self.below(pool.len())]
    }
}

// No daemon addresses: a host name would wait on the resolver. No paths
// outside the work directory: `list -r /` would walk the whole disk.
const WORDS: &[&str] = &[
    "-",
    "--",
    "---",
    "help",
    "-h",
    "-?",
    "version",
    "--version",
    "-v",
    "last",
    "first",
    "info",
    "fetch",
    "lastupdate",
    "list",
    "update",
    "updatev",
    "tune",
    "dump",
    "restore",
    "resize",
    "xport",
    "create",
    "flushcached",
    "AVERAGE",
    "-s",
    "--start",
    "-e",
    "--end=now",
    "-sa",
    "--bogus",
    "--bogus=1",
    "-Q",
    "--rraindex",
    "--rraindex=1",
    "--noflush=",
    "-F",
    "-r",
    "-aFr",
    "-t",
    "--template",
    "x.rrd",
    "a.rrd",
    "",
    "é",
    "\u{1f600}",
    "-é",
    "'",
    "\"",
    "#",
    "cd",
    "quit",
    "ls",
    "DS:x:GAUGE:600:U:U",
    "RRA:AVERAGE:0.5:1:5",
    "1000000300:1",
    "N:1",
];

const LINE_WORDS: &[&str] = &[
    "last",
    "first",
    "info",
    "fetch",
    "a.rrd",
    "x.rrd",
    "AVERAGE",
    "--bogus",
    "-s",
    "help",
    "version",
    "-v",
    "ls",
    "pwd",
    "mkdir sub",
    "cd sub",
    "quit",
    "\"",
    "'",
    "\"a b\"",
    "''",
    "#",
    "-",
    "\t",
    "\r",
    "\0",
    "\u{7f}",
    "\u{e9}",
    "Last",
    "create",
    "update",
    "1000000300:1",
];

fn wait_bounded(mut child: std::process::Child, limit: Duration) -> ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("front end did not finish within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn front_end(bin: &Path, name: &str, work: &Path) -> Command {
    let mut command = Command::new(bin.join(name));
    command
        .current_dir(work)
        .env_remove("RRDCACHED_ADDRESS")
        .env_remove("RRD_LOCKING")
        .env("TZ", "UTC")
        .env("LC_ALL", "C");
    command
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let bin = temp.path().join("bin");
    let work = temp.path().join("work");
    std::fs::create_dir(&bin).expect("bin");
    std::fs::create_dir(&work).expect("work");
    for name in ["rrdtool", "rrdupdate", "rrdcreate", "rrdinfo"] {
        symlink(env!("CARGO_BIN_EXE_rondi"), bin.join(name)).expect("symlink");
    }
    (temp, bin, work)
}

#[test]
fn random_command_lines_finish_with_status_0_or_1() {
    let (_temp, bin, work) = setup();
    let mut rng = Rng(0x5eed_c11f_0000_0001);
    let long = "x".repeat(5000);
    for _ in 0..150 {
        let name = rng.pick(&[
            "rrdtool",
            "rrdtool",
            "rrdtool",
            "rrdupdate",
            "rrdcreate",
            "rrdinfo",
        ]);
        let count = rng.below(6);
        let args = (0..count)
            .map(|_| {
                if rng.below(40) == 0 {
                    long.clone()
                } else {
                    rng.pick(WORDS).to_owned()
                }
            })
            // `rrdtool -` reads standard input; it is covered below.
            .filter(|word| count != 1 || word != "-")
            .collect::<Vec<_>>();
        let child = front_end(&bin, name, &work)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let status = wait_bounded(child, Duration::from_secs(20));
        assert_eq!(status.signal(), None, "{name} {args:?} was killed");
        assert!(
            matches!(status.code(), Some(0 | 1)),
            "{name} {args:?} exited with {status:?}"
        );
    }
}

#[test]
fn random_pipe_sessions_finish_with_status_0() {
    let (_temp, bin, work) = setup();
    let mut rng = Rng(0x5eed_c11f_0000_0002);
    for _ in 0..60 {
        let mut input = Vec::new();
        for _ in 0..rng.below(12) {
            for index in 0..rng.below(5) {
                if index > 0 {
                    input.extend_from_slice(if rng.below(4) == 0 { b"  " } else { b" " });
                }
                // Each character is one byte, as the vectors feed input.
                input.extend(rng.pick(LINE_WORDS).chars().map(|c| c as u32 as u8));
            }
            if rng.below(10) != 0 {
                input.push(b'\n');
            }
        }
        let mut child = front_end(&bin, "rrdtool", &work)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        // A session that quits early closes its end; that is not an error.
        let _ = child.stdin.take().expect("stdin").write_all(&input);
        let status = wait_bounded(child, Duration::from_secs(20));
        assert_eq!(
            status.code(),
            Some(0),
            "{:?} gave {status:?}",
            String::from_utf8_lossy(&input)
        );
    }
}

#[test]
fn long_pipe_lines_are_handled() {
    let (_temp, bin, work) = setup();
    let mut line = b"last".to_vec();
    for index in 0..1200 {
        line.extend_from_slice(format!(" arg{index}").as_bytes());
    }
    line.extend_from_slice(&b"y".repeat(64 * 1024));
    line.push(b'\n');
    line.extend_from_slice(b"pwd\n");
    let mut child = front_end(&bin, "rrdtool", &work)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&line));
    let output = child.wait_with_output().expect("output");
    writer.join().expect("writer").expect("write");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(
        text.starts_with("-1\nERROR: Usage: rrdtool last "),
        "{text}"
    );
    // `pwd` on the next line still gets its reply.
    assert!(text.contains("\nOK u:"), "{text}");
}
