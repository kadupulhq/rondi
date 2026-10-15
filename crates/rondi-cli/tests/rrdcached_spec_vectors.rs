//! Replays the rrdcached protocol and daemon vectors in
//! `data/rrdcached/vectors.json`.
//!
//! The vectors were captured from rrdcached 1.11.0 by
//! `scripts/rrdcached-spec-vectors.py` and are described in
//! `docs/compatibility/rrdcached.md` and
//! `docs/compatibility/rrdcached-daemon.md`. Every case is replayed against
//! the Rondi binary under the `rrdcached` name. When the pinned rrdcached is
//! installed, each case also runs through it and must give the frozen
//! result; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure.
//!
//! A case is a list of steps on one fresh fixture tree: start a daemon,
//! talk to it on one connection per `conn` step, signal it, write or list
//! files. Bytes are carried in JSON strings, one character U+0000..U+00FF
//! per byte. Results are normalized as the script documents: the fixture
//! root becomes `{ROOT}`, the TCP port `{PORT}`, the daemon pid `{PID}` and
//! journal name times `{T}`.
//!
//! A case with a `rondi` entry is one where Rondi knowingly differs. Its
//! `rondi.expect` is the result Rondi must give instead; for `policy` cases
//! this is an intentional hardening difference that a reimplementation must
//! keep. The frozen RRDtool result stays in `expect`.
#![cfg(unix)]

#[macro_use]
mod common;

use serde_json::{Map, Value, json};
use std::io::{ErrorKind, Read, Seek, Write};
use std::net::TcpStream;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VECTORS: &str = include_str!("data/rrdcached/vectors.json");
const FIXTURE_A: &[u8] = include_bytes!("data/rrdcached/fixtures/a.rrd");
const FIXTURE_S7: &[u8] = include_bytes!("data/rrdcached/fixtures/s7.rrd");
const NOBODY: u32 = 65534;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const LINGER: Duration = Duration::from_millis(400);
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Length of the fixture root path, as in the generator.
const ROOT_LENGTH: usize = 80;

/// The `expect_platform` key for this build target; macOS uses `expect`.
fn platform_key() -> Option<&'static str> {
    if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Some("linux-x86_64")
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        Some("linux-aarch64")
    } else {
        None
    }
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Bytes as a string with one char per byte, the vectors' encoding.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("vector text above U+00FF"))
        .collect()
}

/// The fixture tree, as `layout` in the generator builds it.
fn layout(root: &Path) {
    let base = root.join("b");
    for name in ["sub/deep", "dir.rrd", "empty.d/x"] {
        std::fs::create_dir_all(base.join(name)).unwrap();
    }
    std::fs::create_dir(root.join("j")).unwrap();
    std::fs::set_permissions(root.join("j"), std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in [
        "a.rrd",
        "sub/d.rrd",
        "sub/deep/f.rrd",
        "s p.rrd",
        "back\\slash.rrd",
    ] {
        std::fs::write(base.join(name), FIXTURE_A).unwrap();
    }
    std::fs::write(base.join("s7.rrd"), FIXTURE_S7).unwrap();
    std::fs::write(base.join("text.rrd"), b"hello\n").unwrap();
    std::fs::write(base.join("empty.rrd"), b"").unwrap();
    std::fs::write(base.join("notes.txt"), b"x\n").unwrap();
    std::fs::write(root.join("outside.rrd"), FIXTURE_A).unwrap();
    std::fs::write(root.join("outside.txt"), b"not an RRD file\n".repeat(200)).unwrap();
    std::os::unix::fs::symlink("a.rrd", base.join("link.rrd")).unwrap();
    std::os::unix::fs::symlink("sub", base.join("lnk")).unwrap();
    std::os::unix::fs::symlink("../outside.rrd", base.join("escape.rrd")).unwrap();
}

fn give_to_nobody(path: &Path) {
    std::os::unix::fs::lchown(path, Some(NOBODY), Some(NOBODY)).unwrap();
    let metadata = std::fs::symlink_metadata(path).unwrap();
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            give_to_nobody(&entry.unwrap().path());
        }
    }
}

struct Daemon {
    child: Child,
    out: std::fs::File,
    err: std::fs::File,
}

struct Context {
    root: PathBuf,
    /// The canonical root, used to expand `{ROOT}` in requests.
    root_text: String,
    /// Spellings of the fixture root, longest first.
    aliases: Vec<String>,
    port: Option<String>,
    pid: Option<String>,
}

impl Context {
    fn expand(&self, text: &str) -> String {
        let mut text = text.replace("{ROOT}", &self.root_text);
        if let Some(port) = &self.port {
            text = text.replace("{PORT}", port);
        }
        text
    }

    fn normalize(&self, data: &[u8]) -> String {
        let mut text = latin1(data);
        for alias in &self.aliases {
            text = text.replace(alias.as_str(), "{ROOT}");
        }
        if let Some(port) = &self.port {
            text = text.replace(&format!(":{port}"), ":{PORT}");
        }
        if let Some(pid) = &self.pid {
            text = replace_word(&text, pid, "{PID}");
        }
        text
    }
}

/// Replaces `word` where it is not part of a longer run of word characters.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let is_word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(word) {
        out.push_str(&rest[..at]);
        let after = rest[at + word.len()..].chars().next();
        if is_word(out.chars().next_back()) || is_word(after) {
            out.push_str(word);
        } else {
            out.push_str(with);
        }
        rest = &rest[at + word.len()..];
    }
    out.push_str(rest);
    out
}

/// `rrd.journal.<digits>.<digits>` becomes `rrd.journal.{T}`.
fn journal_name(name: &str) -> String {
    let prefix = "rrd.journal.";
    let Some(at) = name.find(prefix) else {
        return name.to_owned();
    };
    let tail = &name[at + prefix.len()..];
    let digits = |s: &str| s.bytes().take_while(u8::is_ascii_digit).count();
    let first = digits(tail);
    if first == 0 || tail.as_bytes().get(first) != Some(&b'.') {
        return name.to_owned();
    }
    let second = digits(&tail[first + 1..]);
    if second == 0 {
        return name.to_owned();
    }
    format!(
        "{}rrd.journal.{{T}}{}",
        &name[..at],
        &tail[first + 1 + second..]
    )
}

fn free_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port().to_string()
}

fn spawn(program: &Path, argv: &[String], ctx: &Context) -> Daemon {
    let out = tempfile::tempfile().unwrap();
    let err = tempfile::tempfile().unwrap();
    let mut command = Command::new(program);
    command
        .args(argv.iter().map(|word| ctx.expand(word)))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .current_dir(&ctx.root)
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(err.try_clone().unwrap());
    // SAFETY: umask is async-signal-safe and touches only this process.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    if running_as_root() {
        command.uid(NOBODY).gid(NOBODY);
    }
    Daemon {
        child: command.spawn().unwrap(),
        out,
        err,
    }
}

fn status_value(status: std::process::ExitStatus) -> Value {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => json!(code),
        None => json!(format!("signal {}", status.signal().unwrap_or(0))),
    }
}

/// Waits for exit and returns status, stdout and stderr.
fn finish(daemon: &mut Daemon, ctx: &Context) -> Value {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = daemon.child.kill();
            let _ = daemon.child.wait();
            return json!({"status": "timeout"});
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let read = |file: &mut std::fs::File| {
        let mut data = Vec::new();
        file.rewind().unwrap();
        file.read_to_end(&mut data).unwrap();
        ctx.normalize(&data)
    };
    let stdout = read(&mut daemon.out);
    let stderr = read(&mut daemon.err);
    json!({"status": status_value(status), "stdout": stdout, "stderr": stderr})
}

enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn connect(ctx: &Context, via: &str) -> std::io::Result<Self> {
        if via == "tcp" {
            let port: u16 = ctx.port.as_ref().unwrap().parse().unwrap();
            TcpStream::connect(("127.0.0.1", port)).map(Stream::Tcp)
        } else {
            UnixStream::connect(ctx.root.join("s.sock")).map(Stream::Unix)
        }
    }

    fn io(&mut self) -> &mut dyn ReadWrite {
        match self {
            Stream::Unix(s) => s,
            Stream::Tcp(s) => s,
        }
    }

    fn set_read_timeout(&self, timeout: Duration) {
        match self {
            Stream::Unix(s) => s.set_read_timeout(Some(timeout)).unwrap(),
            Stream::Tcp(s) => s.set_read_timeout(Some(timeout)).unwrap(),
        }
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// Whether the daemon answers a request. A listening socket accepts
/// connections before the daemon has finished starting (and installed its
/// signal handlers), so a connect alone does not show it is ready.
fn answers(ctx: &Context, via: &str) -> bool {
    let Ok(mut stream) = Stream::connect(ctx, via) else {
        return false;
    };
    stream.set_read_timeout(Duration::from_secs(1));
    let mut byte = [0u8; 1];
    stream.io().write_all(b"PING\n").is_ok() && matches!(stream.io().read(&mut byte), Ok(1))
}

fn wait_listening(daemon: &mut Daemon, ctx: &Context, via: &str) -> Option<&'static str> {
    let deadline = Instant::now() + common::io_timeout();
    while Instant::now() < deadline {
        if answers(ctx, via) {
            return None;
        }
        if daemon.child.try_wait().unwrap().is_some() {
            return Some("exited");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Some("timeout")
}

fn talk(ctx: &Context, step: &Map<String, Value>) -> Value {
    let via = step["via"].as_str().unwrap();
    let end = step["end"].as_str().unwrap();
    let mut stream = match Stream::connect(ctx, via) {
        Ok(stream) => stream,
        Err(_) => return json!("<connect failed: ConnectionRefusedError>"),
    };
    stream.set_read_timeout(READ_TIMEOUT);
    let data = bytes(&ctx.expand(step["conn"].as_str().unwrap()));
    if stream.io().write_all(&data).is_ok() && end == "quit" {
        let _ = stream.io().write_all(b"QUIT\n");
    }
    if end == "linger" {
        stream.set_read_timeout(LINGER);
    }
    let mut reply = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match stream.io().read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&chunk[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if end != "linger" {
                    reply.extend_from_slice(b"<timeout>");
                }
                break;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            // A daemon that closes with unread input pending resets the
            // connection on Linux and closes it normally on macOS; both end
            // the reply.
            Err(_) => break,
        }
    }
    json!(ctx.normalize(&reply))
}

fn list_files(ctx: &Context, patterns: &[Value]) -> Value {
    let mut found = Map::new();
    for pattern in patterns {
        let pattern = pattern.as_str().unwrap();
        let (pattern, content) = match pattern.strip_suffix(":content") {
            Some(p) => (p, true),
            None => (pattern, false),
        };
        let paths: Vec<PathBuf> = match pattern.strip_suffix("/*") {
            Some(dir) => {
                let mut names: Vec<PathBuf> = std::fs::read_dir(ctx.root.join(dir))
                    .map(|entries| {
                        entries
                            .map(|e| e.unwrap().path())
                            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                names
            }
            None => vec![ctx.root.join(pattern)],
        };
        for path in paths {
            let relative = path.strip_prefix(&ctx.root).unwrap().to_string_lossy();
            let mut name = journal_name(&relative);
            while found.contains_key(&name) {
                name.push('\'');
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                found.insert(name, json!("missing"));
                continue;
            };
            let file_type = metadata.file_type();
            use std::os::unix::fs::FileTypeExt;
            let kind = if file_type.is_symlink() {
                "link"
            } else if file_type.is_dir() {
                "dir"
            } else if file_type.is_socket() {
                "socket"
            } else {
                "file"
            };
            let mut entry = format!("{kind} {:04o}", metadata.mode() & 0o7777);
            if content && kind == "file" {
                entry.push('\n');
                entry.push_str(&ctx.normalize(&std::fs::read(&path).unwrap()));
            }
            found.insert(name, json!(entry));
        }
    }
    Value::Object(found)
}

fn signal_number(name: &str) -> libc::c_int {
    match name {
        "TERM" => libc::SIGTERM,
        "INT" => libc::SIGINT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "HUP" => libc::SIGHUP,
        "PIPE" => libc::SIGPIPE,
        "ALRM" => libc::SIGALRM,
        other => panic!("unknown signal {other}"),
    }
}

fn send_signal(daemon: &mut Daemon, name: &str) {
    if daemon.child.try_wait().unwrap().is_none() {
        // SAFETY: the child is owned by this test and has not been reaped.
        unsafe { libc::kill(daemon.child.id() as libc::pid_t, signal_number(name)) };
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// Runs one case with `program` and returns one result per step.
fn run_case(program: &Path, case: &Value) -> Vec<Value> {
    let temp = tempfile::tempdir().unwrap();
    // Journal lines, and so JournalBytes, contain absolute paths, so the
    // root is padded to a length that does not depend on the host.
    let real = std::fs::canonicalize(temp.path()).unwrap();
    let pad = ROOT_LENGTH
        .checked_sub(real.as_os_str().len() + 1)
        .filter(|&pad| pad > 0)
        .unwrap_or_else(|| panic!("temporary directory {} is too long", real.display()));
    let name = "r".repeat(pad);
    let root = real.join(&name);
    std::fs::create_dir(&root).unwrap();
    let root_text = root.to_string_lossy().into_owned();
    let mut aliases = vec![
        root_text.clone(),
        temp.path().join(&name).to_string_lossy().into_owned(),
    ];
    aliases.sort_by_key(|alias| std::cmp::Reverse(alias.len()));
    aliases.dedup();
    let steps = case["steps"].as_array().unwrap();
    let mut ctx = Context {
        root: root.clone(),
        root_text,
        aliases,
        port: uses_tcp(steps).then(free_port),
        pid: None,
    };
    layout(&root);
    if running_as_root() {
        // The temporary directory itself must be reachable by nobody.
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut daemons: Vec<Daemon> = Vec::new();
    let mut results = Vec::new();
    for step in steps {
        let step = step.as_object().unwrap();
        if let Some(path) = step.get("write") {
            let text = ctx.expand(step["text"].as_str().unwrap());
            let target = root.join(path.as_str().unwrap());
            std::fs::write(&target, bytes(&text)).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            results.push(Value::Null);
            continue;
        }
        if running_as_root() && daemons.is_empty() {
            give_to_nobody(&root);
        }
        let result = if let Some(argv) = step.get("daemon") {
            let argv = strings(argv);
            let mut daemon = spawn(program, &argv, &ctx);
            let outcome = if step["start"] == "exit" {
                finish(&mut daemon, &ctx)
            } else {
                let via = if argv.iter().any(|w| w.contains("{PORT}")) {
                    "tcp"
                } else {
                    "unix"
                };
                match wait_listening(&mut daemon, &ctx, via) {
                    None => {
                        ctx.pid = std::fs::read_to_string(root.join("pid"))
                            .ok()
                            .map(|text| text.trim().to_owned())
                            .filter(|text| !text.is_empty());
                        Value::Null
                    }
                    Some("exited") => finish(&mut daemon, &ctx),
                    Some(_) => json!({"status": "not listening"}),
                }
            };
            daemons.push(daemon);
            outcome
        } else if let Some(argv) = step.get("daemon2") {
            let mut daemon = spawn(program, &strings(argv), &ctx);
            let deadline = Instant::now() + Duration::from_secs(3);
            while daemon.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            send_signal(&mut daemon, "TERM");
            finish(&mut daemon, &ctx)
        } else if step.contains_key("conn") {
            talk(&ctx, step)
        } else if let Some(name) = step.get("signal") {
            let daemon = daemons.last_mut().unwrap();
            send_signal(daemon, name.as_str().unwrap());
            let outcome = finish(daemon, &ctx);
            if step.get("quiet") == Some(&Value::Bool(true)) {
                json!({"status": outcome["status"]})
            } else {
                outcome
            }
        } else if let Some(name) = step.get("signal_only") {
            let daemon = daemons.last_mut().unwrap();
            send_signal(daemon, name.as_str().unwrap());
            std::thread::sleep(Duration::from_millis(300));
            json!({"running": daemon.child.try_wait().unwrap().is_none()})
        } else if let Some(patterns) = step.get("files") {
            // The pid file may appear after the socket accepts connections.
            if ctx.pid.is_none() {
                ctx.pid = std::fs::read_to_string(root.join("pid"))
                    .ok()
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty());
            }
            list_files(&ctx, patterns.as_array().unwrap())
        } else if let Some(ms) = step.get("sleep") {
            std::thread::sleep(Duration::from_millis(ms.as_u64().unwrap()));
            Value::Null
        } else {
            panic!("unknown step {step:?}");
        };
        results.push(result);
    }
    for daemon in &mut daemons {
        if daemon.child.try_wait().unwrap().is_none() {
            let _ = daemon.child.kill();
            let _ = daemon.child.wait();
        }
    }
    results
}

fn uses_tcp(steps: &[Value]) -> bool {
    steps.iter().any(|step| step.to_string().contains("PORT"))
}

/// Replaces the error text after `mmaping file '<path>': ` with `{ERRNO}`:
/// mapping a directory fails with EINVAL or ENODEV depending on the file
/// system.
fn mask_mmap_error(text: &str) -> String {
    const MARK: &str = "mmaping file '";
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        let after = &rest[at + MARK.len()..];
        let Some(end) = after
            .find("': ")
            .filter(|&end| !after[..end].contains('\n'))
        else {
            out.push_str(&rest[..at + MARK.len()]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..at + MARK.len() + end + 3]);
        out.push_str("{ERRNO}");
        let tail = &after[end + 3..];
        rest = &tail[tail.find('\n').unwrap_or(tail.len())..];
    }
    out.push_str(rest);
    out
}

/// The form in which results are compared: mapping errors masked, and the
/// lines of every reply sorted when the case lists a directory, whose order
/// depends on the file system.
fn canonical(case: &Value, results: &[Value]) -> Vec<Value> {
    let unordered = tagged(case, "unordered");
    results
        .iter()
        .map(|result| match result.as_str() {
            Some(text) => {
                let text = mask_mmap_error(text);
                if unordered {
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    lines.sort_unstable();
                    json!(lines.join("\n"))
                } else {
                    json!(text)
                }
            }
            None => result.clone(),
        })
        .collect()
}

fn tagged(case: &Value, tag: &str) -> bool {
    case["tags"].as_array().unwrap().iter().any(|t| t == tag)
}

/// `expect`, or its `expect_platform` variant for `key` where one exists.
fn variant<'a>(entry: &'a Value, key: Option<&str>) -> &'a Value {
    key.and_then(|key| entry.get("expect_platform").and_then(|p| p.get(key)))
        .unwrap_or(&entry["expect"])
}

/// The frozen oracle expectation for this build target.
fn oracle_expect(case: &Value) -> &Value {
    variant(case, platform_key())
}

/// What Rondi must produce: the documented divergence, if any, else the
/// oracle result.
fn rondi_expect(case: &Value) -> &Value {
    match case.get("rondi") {
        Some(divergence) => variant(divergence, platform_key()),
        None => oracle_expect(case),
    }
}

/// Neither implementation may hang a connection or crash.
fn clean(results: &[Value]) -> bool {
    results.iter().all(|r| {
        let text = r.to_string();
        !text.contains("<timeout>") && !text.contains("\"timeout\"") && !text.contains("signal 11")
    })
}

fn live_oracle() -> Option<PathBuf> {
    let output = Command::new("rrdcached").arg("-h").output();
    let found = output.is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout).starts_with("RRDCacheD 1.11.0")
            || String::from_utf8_lossy(&o.stderr).starts_with("RRDCacheD 1.11.0")
    });
    if !found {
        oracle_skip!("rrdcached 1.11.0 is not on PATH; comparing against frozen vectors only");
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("rrdcached"))
        .find(|p| p.is_file())
}

fn describe(case: &Value, want: &[Value], got: &[Value]) -> String {
    let mut text = format!("== {}\n", case["id"].as_str().unwrap());
    for ((step, w), g) in case["steps"].as_array().unwrap().iter().zip(want).zip(got) {
        if w != g {
            text.push_str(&format!("  step {step}\n  want {w}\n  got  {g}\n"));
        }
    }
    text
}

fn table() -> Vec<Value> {
    let table: Value = serde_json::from_str(VECTORS).unwrap();
    table["cases"].as_array().unwrap().clone()
}

#[test]
fn journal_names_and_words_normalize() {
    assert_eq!(
        journal_name("j/rrd.journal.1760.123456"),
        "j/rrd.journal.{T}"
    );
    assert_eq!(journal_name("j/rrd.journal"), "j/rrd.journal");
    assert_eq!(journal_name("j/rrd.journal.1."), "j/rrd.journal.1.");
    assert_eq!(
        replace_word("pid 42 x420 42", "42", "{PID}"),
        "pid {PID} x420 {PID}"
    );
}

#[test]
fn vector_table_is_well_formed() {
    let cases = table();
    let protocol = cases.iter().filter(|c| c["kind"] == "protocol").count();
    assert!(protocol >= 400, "only {protocol} protocol sessions");
    for case in &cases {
        let steps = case["steps"].as_array().unwrap().len();
        assert_eq!(
            case["expect"].as_array().unwrap().len(),
            steps,
            "{}",
            case["id"]
        );
        if let Some(divergence) = case.get("rondi") {
            let class = divergence["class"].as_str().unwrap();
            assert!(matches!(class, "policy" | "defect"), "{}", case["id"]);
            assert!(!divergence["note"].as_str().unwrap().is_empty());
            assert_eq!(
                divergence["expect"].as_array().unwrap().len(),
                steps,
                "{}",
                case["id"]
            );
            // A divergence may show on one platform only.
            let differs = [None, Some("linux-aarch64"), Some("linux-x86_64")]
                .into_iter()
                .any(|key| {
                    canonical(case, variant(divergence, key).as_array().unwrap())
                        != canonical(case, variant(case, key).as_array().unwrap())
                });
            assert!(
                differs || tagged(case, "undefined"),
                "{}: a divergence must differ from the oracle",
                case["id"]
            );
        }
    }
}

#[test]
fn rrdcached_spec_vectors_match() {
    let temp = tempfile::tempdir().unwrap();
    let rondi = temp.path().join("rrdcached");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
    if running_as_root() {
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let oracle = live_oracle();
    let only = std::env::var("RONDI_RRDCACHED_ONLY").unwrap_or_default();
    let cases: Vec<Value> = table()
        .into_iter()
        .filter(|c| only.is_empty() || only.split(',').any(|id| c["id"] == id))
        .collect();
    let failures = Mutex::new(Vec::<String>::new());
    let next = Mutex::new(0usize);
    // A case spends most of its time waiting: the reference daemon takes
    // about a second to exit after SIGTERM. Many more workers than cores
    // keep the whole table well under a minute.
    let workers = std::env::var("RONDI_RRDCACHED_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse().ok())
        .unwrap_or(32usize)
        .max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut next = next.lock().unwrap();
                        *next += 1;
                        *next - 1
                    };
                    let Some(case) = cases.get(index) else { break };
                    if let Some(problem) = check_case(&rondi, oracle.as_deref(), case) {
                        failures.lock().unwrap().push(problem);
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    for failure in failures.iter().take(40) {
        eprintln!("{failure}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} rrdcached vectors failed",
        failures.len(),
        cases.len()
    );
}

/// Checks one case; timing cases get one retry because a loaded host can
/// delay a timer past the vector's sleep.
fn check_case(rondi: &Path, oracle: Option<&Path>, case: &Value) -> Option<String> {
    let attempts = if tagged(case, "timing") { 2 } else { 1 };
    let mut problem = None;
    for _ in 0..attempts {
        problem = check_once(rondi, oracle, case);
        if problem.is_none() {
            break;
        }
    }
    problem
}

fn check_once(rondi: &Path, oracle: Option<&Path>, case: &Value) -> Option<String> {
    // The reference result of an `undefined` case hangs or depends on
    // memory, and an `unstable` one varied between recordings; neither is
    // frozen.
    let frozen = !tagged(case, "unstable") && !tagged(case, "undefined");
    let got = run_case(rondi, case);
    if !clean(&got) {
        return Some(format!(
            "{}: Rondi hung or crashed: {}",
            case["id"],
            Value::Array(got)
        ));
    }
    // A listed divergence pins Rondi's own result, also where the reference
    // is undefined.
    if frozen || case.get("rondi").is_some() {
        let want = canonical(case, rondi_expect(case).as_array().unwrap());
        let got = canonical(case, &got);
        if want != got {
            return Some(describe(case, &want, &got));
        }
    }
    if let Some(oracle) = oracle
        && frozen
    {
        let live = canonical(case, &run_case(oracle, case));
        let want = canonical(case, oracle_expect(case).as_array().unwrap());
        if want != live {
            return Some(format!(
                "live rrdcached differs from the frozen vector\n{}",
                describe(case, &want, &live)
            ));
        }
    }
    None
}
