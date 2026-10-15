#![cfg(unix)]
//! Frozen vectors for `rrdtool restore`. The behavior is specified in
//! docs/compatibility/restore.md; the vectors under tests/data/restore were
//! recorded from RRDtool 1.11.0 by scripts/gen-restore-vectors.py.
//!
//! Each case writes one input XML file, restores it from the named file or
//! from standard input and compares the exit status, output and the
//! resulting file in its normal form (RS-60: rows rotated oldest first, row
//! pointers zeroed, NaN signs folded). `expected/base.vectors` was recorded
//! on macOS arm64; on Linux the cases listed in
//! `expected/linux-<arch>.vectors` replace it. With an RRDtool 1.11.0 oracle
//! on PATH, or always under RONDI_REQUIRE_ORACLE=1, the same cases also run
//! live against it.
//!
//! Line numbers inside messages are library behavior (RS-43, RS-44). A
//! message that differs only in its line numbers is accepted and counted;
//! set RONDI_RESTORE_STRICT_LINES=1 to require them exactly.

mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/restore");

/// What Rondi must do for a case, relative to the recorded RRDtool result.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Status, stdout, stderr and the normal-form file all match.
    Same,
    /// Not well-formed XML. Status and file match; the message text is
    /// libxml2's and only its `ERROR: ` prefix is required (RS-40).
    XmlSyntax,
    /// A COMPUTE data source: refused with Rondi's own message (RS-70).
    Unsupported,
    /// `-f` over a longer file: RRDtool keeps the old tail, Rondi
    /// truncates (RS-56). Everything else matches.
    Truncates,
    /// Status and file match; the error text differs (RS-85).
    TextDiffers,
    /// A documented divergence or a reference crash: Rondi must exit 0 or
    /// 1, never crash or hang, and report failures as `ERROR: `.
    Diverge,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    File,
    Stdin,
}

struct Case {
    id: String,
    input: String,
    mode: Mode,
    target: String,
    pre: String,
    expect: Expect,
    opts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Code(i32),
    Signal,
    Hang,
}

#[derive(Clone, Debug, PartialEq)]
enum FileExpect {
    Absent,
    Digest(String),
    /// The reference result varies between runs; not compared.
    Undefined,
}

struct Vector {
    status: Status,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    file: FileExpect,
    flags: String,
}

struct Observed {
    status: Status,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Normal form of the produced file, if any.
    file: Option<Vec<u8>>,
    /// Row pointers of the produced file, with each archive's row count.
    pointers: Vec<(u64, u64)>,
}

fn load_cases() -> Vec<Case> {
    let manifest = std::fs::read_to_string(Path::new(DATA).join("cases.tsv")).unwrap();
    manifest
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            let mut next = || fields.next().unwrap().to_owned();
            let id = next();
            let input = next();
            let mode = match next().as_str() {
                "file" => Mode::File,
                "stdin" => Mode::Stdin,
                other => panic!("{id}: unknown mode {other}"),
            };
            let target = next();
            let pre = next();
            let expect = match next().as_str() {
                "same" => Expect::Same,
                "xml-syntax" => Expect::XmlSyntax,
                "unsupported" => Expect::Unsupported,
                "truncates" => Expect::Truncates,
                "text-differs" => Expect::TextDiffers,
                "diverge" => Expect::Diverge,
                other => panic!("{id}: unknown expectation {other}"),
            };
            Case {
                id,
                input,
                mode,
                target,
                pre,
                expect,
                opts: fields.map(str::to_owned).collect(),
            }
        })
        .collect()
}

/// Splits `@@ <id> <section> <length>` framed vectors by case and section.
fn parse_vectors(bytes: &[u8], into: &mut BTreeMap<String, BTreeMap<String, Vec<u8>>>) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let newline = rest.iter().position(|b| *b == b'\n').unwrap();
        let header = std::str::from_utf8(&rest[..newline]).unwrap();
        let mut words = header.split(' ');
        assert_eq!(words.next(), Some("@@"), "bad section header {header:?}");
        let id = words.next().unwrap().to_owned();
        let name = words.next().unwrap().to_owned();
        let length: usize = words.next().unwrap().parse().unwrap();
        let body = &rest[newline + 1..newline + 1 + length];
        into.entry(id).or_default().insert(name, body.to_vec());
        rest = &rest[newline + 1 + length + 1..];
    }
}

/// The platform whose override file applies, if any.
fn platform() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        None
    } else if cfg!(target_arch = "x86_64") {
        Some("linux-x86_64")
    } else if cfg!(target_arch = "aarch64") {
        Some("linux-aarch64")
    } else {
        None
    }
}

fn load_vectors() -> BTreeMap<String, Vector> {
    let dir = Path::new(DATA).join("expected");
    let mut raw = BTreeMap::new();
    parse_vectors(&std::fs::read(dir.join("base.vectors")).unwrap(), &mut raw);
    if let Some(platform) = platform() {
        let mut overrides = BTreeMap::new();
        parse_vectors(
            &std::fs::read(dir.join(format!("{platform}.vectors"))).unwrap(),
            &mut overrides,
        );
        raw.extend(overrides);
    }
    raw.into_iter()
        .map(|(id, mut parts)| {
            let status = match parts.remove("status").unwrap().as_slice() {
                b"signal" => Status::Signal,
                b"hang" => Status::Hang,
                code => Status::Code(std::str::from_utf8(code).unwrap().parse().unwrap()),
            };
            let file = match parts.remove("file").unwrap().as_slice() {
                b"none" => FileExpect::Absent,
                b"undefined" => FileExpect::Undefined,
                digest => FileExpect::Digest(String::from_utf8(digest.to_vec()).unwrap()),
            };
            let vector = Vector {
                status,
                stdout: parts.remove("stdout").unwrap(),
                stderr: parts.remove("stderr").unwrap(),
                file,
                flags: String::from_utf8(parts.remove("flags").unwrap()).unwrap(),
            };
            (id, vector)
        })
        .collect()
}

fn expected_output(digest: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(DATA)
            .join("expected/outputs")
            .join(format!("{digest}.rrd")),
    )
    .unwrap()
}

fn word(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Section sizes of the documented layout (RS-50): header, definitions,
/// last update, PDP and CDP state, then one row pointer per archive.
fn pointer_offset(bytes: &[u8]) -> Option<(usize, usize, usize)> {
    if bytes.len() < 128 || &bytes[..4] != b"RRD\0" {
        return None;
    }
    let ds = usize::try_from(word(bytes, 24)?).ok()?;
    let rra = usize::try_from(word(bytes, 32)?).ok()?;
    let offset = ds
        .checked_mul(120 + 112)?
        .checked_add(rra.checked_mul(120)?)?
        .checked_add(ds.checked_mul(rra)?.checked_mul(80)?)?
        .checked_add(128 + 16)?;
    (offset.checked_add(rra.checked_mul(8)?)? <= bytes.len()).then_some((ds, rra, offset))
}

/// Normal form (RS-60): each archive's rows rotated oldest first, row
/// pointers zeroed and the x86_64 negative quiet NaN folded into the
/// positive one. Bytes that do not parse as an RRD are returned as given.
fn normal_form(bytes: &[u8]) -> (Vec<u8>, Vec<(u64, u64)>) {
    let Some((ds, rra, pointers)) = pointer_offset(bytes) else {
        return (bytes.to_vec(), Vec::new());
    };
    let mut out = bytes[..pointers].to_vec();
    out.extend(std::iter::repeat_n(0, 8 * rra));
    let mut seen = Vec::new();
    let mut data = pointers + 8 * rra;
    for index in 0..rra {
        let Some(rows) = word(bytes, 128 + 120 * ds + 120 * index + 24) else {
            return (bytes.to_vec(), Vec::new());
        };
        let current = word(bytes, pointers + 8 * index).unwrap();
        let length = (rows as usize).saturating_mul(ds).saturating_mul(8);
        let Some(archive) = data
            .checked_add(length)
            .and_then(|end| bytes.get(data..end))
        else {
            return (bytes.to_vec(), Vec::new());
        };
        seen.push((current, rows));
        if rows > 0 {
            if current >= rows {
                return (bytes.to_vec(), seen);
            }
            let split = ((current + 1) % rows) as usize * ds * 8;
            out.extend_from_slice(&archive[split..]);
            out.extend_from_slice(&archive[..split]);
        }
        data += length;
    }
    out.extend_from_slice(&bytes[data..]);
    let whole = out.len() / 8 * 8;
    for chunk in out[..whole].chunks_mut(8) {
        if chunk == 0xfff8_0000_0000_0000_u64.to_le_bytes() {
            chunk.copy_from_slice(&0x7ff8_0000_0000_0000_u64.to_le_bytes());
        }
    }
    (out, seen)
}

fn pre_content(pre: &str) -> Option<Vec<u8>> {
    match pre {
        "none" => None,
        "old" => Some(b"old\n".to_vec()),
        "long" => Some(vec![0xab; 5000]),
        other => panic!("unknown pre-existing content {other}"),
    }
}

/// Runs one case in a fresh directory. A process still running after the
/// deadline is killed and reported as a hang.
fn observe(program: &Path, work: &Path, case: &Case) -> Observed {
    let dir = work.join(&case.id);
    std::fs::create_dir(&dir).unwrap();
    let xml = std::fs::read(
        Path::new(DATA)
            .join("inputs")
            .join(format!("{}.xml", case.input)),
    )
    .unwrap();
    std::fs::write(dir.join("in.xml"), &xml).unwrap();
    if let Some(content) = pre_content(&case.pre) {
        std::fs::write(dir.join(&case.target), content).unwrap();
    }
    let source = match case.mode {
        Mode::File => "in.xml",
        Mode::Stdin => "-",
    };
    let mut child = Command::new(program)
        .arg("restore")
        .args(&case.opts)
        .args([source, case.target.as_str()])
        .current_dir(&dir)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env_remove("RRDCACHED_ADDRESS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let feed = (case.mode == Mode::Stdin).then_some(xml);
    let feeder = std::thread::spawn(move || {
        if let Some(xml) = feed {
            // The reader may stop early on an error; a broken pipe is fine.
            let _ = stdin.write_all(&xml);
        }
    });
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout_pipe, &mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr_pipe, &mut buffer);
        buffer
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break match status.code() {
                Some(code) => Status::Code(code),
                None => Status::Signal,
            };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Status::Hang;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    feeder.join().unwrap();
    let mut stdout = out_reader.join().unwrap();
    let stderr = err_reader.join().unwrap();
    let produced = if case.target == "-" {
        (!stdout.is_empty()).then(|| std::mem::take(&mut stdout))
    } else {
        let path = dir.join(&case.target);
        path.is_file().then(|| std::fs::read(path).unwrap())
    };
    let (file, pointers) = match produced {
        Some(bytes) => {
            let (normal, pointers) = normal_form(&bytes);
            (Some(normal), pointers)
        }
        None => (None, Vec::new()),
    };
    let _ = std::fs::remove_dir_all(&dir);
    Observed {
        status,
        stdout,
        stderr,
        file,
        pointers,
    }
}

/// Replaces the digits of `line N`, `ling N` and `in.xml:N:` with `#`.
fn mask_lines(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        let prefix = [&b"line "[..], b"ling ", b"in.xml:"]
            .into_iter()
            .find(|prefix| rest.starts_with(prefix));
        if let Some(prefix) = prefix {
            out.extend_from_slice(prefix);
            index += prefix.len();
            if text.get(index).is_some_and(u8::is_ascii_digit) {
                out.push(b'#');
                while text.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
            }
        } else {
            out.push(text[index]);
            index += 1;
        }
    }
    out
}

fn last_line(text: &[u8]) -> &[u8] {
    let trimmed = text.strip_suffix(b"\n").unwrap_or(text);
    match trimmed.iter().rposition(|b| *b == b'\n') {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

fn show(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() > 300 {
        format!("{}...", &text[..300])
    } else {
        text.into_owned()
    }
}

fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(offset) => format!("first difference at byte {offset} (0x{offset:x})"),
        None => format!("lengths {} and {}", a.len(), b.len()),
    }
}

/// Failures and the number of messages accepted only after masking line
/// numbers.
#[derive(Default)]
struct Report {
    failures: Vec<String>,
    approximate_lines: usize,
}

fn strict_lines() -> bool {
    std::env::var_os("RONDI_RESTORE_STRICT_LINES").is_some_and(|value| value == "1")
}

fn check_file(expect: Expect, vector: &Vector, seen: &Observed, fail: &mut dyn FnMut(String)) {
    match (&vector.file, &seen.file) {
        (FileExpect::Undefined, _) => {}
        (FileExpect::Absent, None) => {}
        (FileExpect::Absent, Some(_)) => fail("wrote a file, expected none".into()),
        (FileExpect::Digest(_), None) => fail("wrote no file".into()),
        (FileExpect::Digest(digest), Some(got)) => {
            let want = expected_output(digest);
            let matches = if expect == Expect::Truncates {
                got.len() < want.len() && want.starts_with(got)
            } else {
                *got == want
            };
            if !matches {
                fail(format!(
                    "file differs from {digest}: {}",
                    first_difference(got, &want)
                ));
            }
        }
    }
}

/// Checks `seen` against the vector. `rondi` selects the case's Rondi
/// expectation; the oracle is always held to the vector exactly.
fn check(case: &Case, vector: &Vector, seen: &Observed, rondi: bool, report: &mut Report) {
    let who = if rondi { "rondi" } else { "rrdtool" };
    let mut failures = Vec::new();
    let mut fail = |what: String| {
        failures.push(format!(
            "{} [{who}] {:?} {:?}: {what}",
            case.id, case.mode, case.opts
        ))
    };
    for (current, rows) in &seen.pointers {
        if *rows > 0 && current >= rows {
            fail(format!("row pointer {current} outside {rows} rows"));
        }
    }
    let nondeterministic = vector
        .flags
        .split(' ')
        .any(|flag| flag == "nondeterministic");
    let crashed = matches!(vector.status, Status::Signal | Status::Hang);
    let expect = match (rondi, crashed) {
        (false, _) => Expect::Same,
        (true, true) => Expect::Diverge,
        (true, false) => case.expect,
    };
    let fails_cleanly = |seen: &Observed| {
        seen.status == Status::Code(0)
            || (seen.status == Status::Code(1) && last_line(&seen.stderr).starts_with(b"ERROR: "))
    };
    match expect {
        Expect::Same | Expect::Truncates => {
            if !rondi && nondeterministic {
                // The recorded runs disagreed; nothing is reproducible.
            } else if !rondi && crashed {
                if !matches!(seen.status, Status::Signal | Status::Hang) {
                    fail(format!(
                        "status {:?}, recorded {:?}",
                        seen.status, vector.status
                    ));
                }
            } else {
                if seen.status != vector.status {
                    fail(format!(
                        "status {:?}, expected {:?}; stderr {}",
                        seen.status,
                        vector.status,
                        show(&seen.stderr)
                    ));
                }
                if seen.stdout != vector.stdout {
                    fail(format!("stdout {}", show(&seen.stdout)));
                }
                if seen.stderr != vector.stderr {
                    let approximate = rondi
                        && !strict_lines()
                        && mask_lines(&seen.stderr) == mask_lines(&vector.stderr);
                    if approximate {
                        report.approximate_lines += 1;
                    } else {
                        fail(format!(
                            "stderr {:?}, expected {:?}",
                            show(&seen.stderr),
                            show(&vector.stderr)
                        ));
                    }
                }
                check_file(expect, vector, seen, &mut fail);
            }
        }
        Expect::XmlSyntax | Expect::TextDiffers => {
            if seen.status != vector.status {
                fail(format!(
                    "status {:?}, expected {:?}; stderr {}",
                    seen.status,
                    vector.status,
                    show(&seen.stderr)
                ));
            }
            if seen.status != Status::Code(0) && !last_line(&seen.stderr).starts_with(b"ERROR: ") {
                fail(format!("no ERROR: line in {}", show(&seen.stderr)));
            }
            check_file(expect, vector, seen, &mut fail);
        }
        Expect::Unsupported => {
            let refused = String::from_utf8_lossy(&seen.stderr).contains("not supported by Rondi");
            if seen.status != Status::Code(1) || !refused {
                fail(format!(
                    "expected an unsupported-feature error, got {:?} {}",
                    seen.status,
                    show(&seen.stderr)
                ));
            }
            if seen.file.is_some() && case.pre == "none" {
                fail("wrote a file while refusing".into());
            }
        }
        Expect::Diverge => {
            if !fails_cleanly(seen) {
                fail(format!(
                    "expected exit 0 or a clean error, got {:?} {}",
                    seen.status,
                    show(&seen.stderr)
                ));
            }
        }
    }
    report.failures.extend(failures);
}

fn finish(report: &Report, who: &str) {
    if report.approximate_lines > 0 {
        eprintln!(
            "{who}: {} messages matched only with line numbers masked (RS-43, RS-44)",
            report.approximate_lines
        );
    }
    assert!(
        report.failures.is_empty(),
        "{} failing checks:\n{}",
        report.failures.len(),
        report
            .failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A scratch directory and the Rondi binary under the `rrdtool` name.
struct Bench {
    _temp: tempfile::TempDir,
    work: PathBuf,
    rondi: PathBuf,
}

impl Bench {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let bin = temp.path().join("bin");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let rondi = bin.join("rrdtool");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
        Self {
            _temp: temp,
            work,
            rondi,
        }
    }
}

fn oracle_available() -> bool {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live restore vectors: rrdtool is not installed".into());
        return false;
    }
    common::require_oracle();
    true
}

/// The table is large enough to pin the specification: every class, both
/// input modes, every DS type and CF, the option set and enough failures.
#[test]
fn restore_vectors_cover_the_specification() {
    let cases = load_cases();
    assert!(cases.len() >= 300, "only {} cases", cases.len());
    let vectors = load_vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        let vector = vectors
            .get(&case.id)
            .unwrap_or_else(|| panic!("no vector for {}", case.id));
        if let FileExpect::Digest(digest) = &vector.file {
            assert!(
                Path::new(DATA)
                    .join("expected/outputs")
                    .join(format!("{digest}.rrd"))
                    .is_file(),
                "{}: missing output {digest}",
                case.id
            );
        }
    }
    assert_eq!(vectors.len(), cases.len(), "vectors without a case");
    for expect in [
        Expect::Same,
        Expect::XmlSyntax,
        Expect::Unsupported,
        Expect::Truncates,
        Expect::TextDiffers,
        Expect::Diverge,
    ] {
        assert!(
            cases.iter().any(|case| case.expect == expect),
            "no {expect:?} case"
        );
    }
    let stdin = cases.iter().filter(|case| case.mode == Mode::Stdin).count();
    assert!(stdin >= 150, "only {stdin} stdin cases");
    let errors = cases
        .iter()
        .filter(|case| vectors[&case.id].status == Status::Code(1))
        .count();
    assert!(errors >= 150, "only {errors} error cases");
    let options: String = cases.iter().flat_map(|case| case.opts.clone()).collect();
    for option in ["-r", "-f", "--range-check", "--force-overwrite"] {
        assert!(options.contains(option), "no case uses {option}");
    }
    let seed = std::fs::read_to_string(Path::new(DATA).join("inputs/seed-types.xml")).unwrap();
    for needle in [
        "GAUGE", "COUNTER", "DERIVE", "ABSOLUTE", "DCOUNTER", "DDERIVE", "AVERAGE", "MIN", "MAX",
        "LAST",
    ] {
        assert!(seed.contains(needle), "seed-types lacks {needle}");
    }
}

/// Rondi reproduces every frozen vector under its expectation.
#[test]
fn rondi_matches_frozen_restore_vectors() {
    let bench = Bench::new();
    let vectors = load_vectors();
    let mut report = Report::default();
    for case in load_cases() {
        let seen = observe(&bench.rondi, &bench.work, &case);
        check(&case, &vectors[&case.id], &seen, true, &mut report);
    }
    finish(&report, "rondi");
}

/// The pinned RRDtool still produces the frozen vectors on this platform.
#[test]
fn rrdtool_reproduces_restore_vectors() {
    if !oracle_available() {
        return;
    }
    let bench = Bench::new();
    let vectors = load_vectors();
    let mut report = Report::default();
    for case in load_cases() {
        // A recorded hang would only spin until the deadline again.
        if vectors[&case.id].status == Status::Hang {
            continue;
        }
        let seen = observe(Path::new("rrdtool"), &bench.work, &case);
        check(&case, &vectors[&case.id], &seen, false, &mut report);
    }
    finish(&report, "rrdtool");
}

fn restore_in(program: &Path, dir: &Path, args: &[&str], umask: Option<&str>) -> Status {
    let mut command = match umask {
        Some(mask) => {
            let mut shell = Command::new("/bin/sh");
            shell
                .arg("-c")
                .arg(format!("umask {mask}; exec \"$0\" \"$@\""))
                .arg(program)
                .arg("restore");
            shell
        }
        None => {
            let mut direct = Command::new(program);
            direct.arg("restore");
            direct
        }
    };
    let status = command
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    status.code().map_or(Status::Signal, Status::Code)
}

/// RS-55 and RS-57: a new file gets at most `0666 & ~umask`; `-f` writes an
/// existing file in place, keeping its inode. RS-61: row pointers stay
/// inside the archive, and RRDtool's vary between restores.
#[test]
fn restore_file_creation_and_row_pointers() {
    let bench = Bench::new();
    let mut programs = vec![("rondi", bench.rondi.clone())];
    if oracle_available() {
        programs.push(("rrdtool", PathBuf::from("rrdtool")));
    }
    let seed = std::fs::read(Path::new(DATA).join("inputs/seed-wide.xml")).unwrap();
    for (who, program) in programs {
        let dir = bench.work.join(who);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("in.xml"), &seed).unwrap();

        assert_eq!(
            restore_in(&program, &dir, &["in.xml", "new.rrd"], Some("027")),
            Status::Code(0),
            "{who}"
        );
        let mode = std::fs::metadata(dir.join("new.rrd")).unwrap().mode() & 0o7777;
        assert_eq!(mode & !0o640, 0, "{who}: mode {mode:o} exceeds 0666 & ~027");
        if who == "rrdtool" {
            assert_eq!(mode, 0o640, "{who}: mode");
        }

        std::fs::write(dir.join("old.rrd"), b"old\n").unwrap();
        let inode = std::fs::metadata(dir.join("old.rrd")).unwrap().ino();
        assert_eq!(
            restore_in(&program, &dir, &["-f", "in.xml", "old.rrd"], None),
            Status::Code(0),
            "{who}"
        );
        assert_eq!(
            std::fs::metadata(dir.join("old.rrd")).unwrap().ino(),
            inode,
            "{who}: -f replaced the inode"
        );

        let mut pointers = std::collections::BTreeSet::new();
        for attempt in 0..24 {
            let name = format!("p{attempt}.rrd");
            assert_eq!(
                restore_in(&program, &dir, &["in.xml", &name], None),
                Status::Code(0),
                "{who}"
            );
            let bytes = std::fs::read(dir.join(&name)).unwrap();
            let (_, seen) = normal_form(&bytes);
            assert_eq!(seen.len(), 1, "{who}");
            let (current, rows) = seen[0];
            assert!(current < rows, "{who}: pointer {current} of {rows}");
            pointers.insert(current);
        }
        // RRDtool places the pointer at random; a fixed choice is allowed
        // (RS-61) and Rondi uses the last row.
        if who == "rrdtool" {
            assert!(
                pointers.len() > 1,
                "{who}: 24 restores of a 120-row archive all used row {pointers:?}"
            );
        }
    }
}
