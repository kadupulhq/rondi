#![cfg(unix)]
//! Frozen vectors for `rrdtool tune` structure changes (DS:, DEL:, RRA:,
//! DELRRA:, RRA#n:[+-=]rows and --step). The behavior is specified in
//! docs/compatibility/tune-modify.md; the vectors under
//! tests/data/tune_modify were recorded from RRDtool 1.11.0 by
//! scripts/gen-tune-modify-vectors.py.
//!
//! Each case copies a frozen input file, runs `tune` on it and compares the
//! exit status, output, resulting file bytes and, for a subset, the `info`
//! and `dump` text of the result. Bytes the specification leaves
//! unspecified (the uninitialised tail of an added data source definition)
//! are masked. With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, the same cases also run live against it.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tune_modify");

/// What Rondi must do for a case, relative to the recorded RRDtool result.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Reproduce the vector: status, output and masked bytes.
    Same,
    /// RRDtool writes a file holding uninitialised bytes and then fails.
    /// Rondi fails with the same status and text and leaves the file alone.
    Refuse,
    /// RRDtool crashes. Rondi fails cleanly and leaves the file alone.
    CleanError,
    /// A feature Rondi does not implement: refused with its own message.
    Unsupported,
}

struct Case {
    id: String,
    input: String,
    expect: Expect,
    args: Vec<String>,
}

/// One recorded or observed result.
#[derive(Debug, PartialEq)]
struct Outcome {
    /// Exit code, or None when the process died from a signal.
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct Vector {
    outcome: Outcome,
    /// Resulting file; None when the tune left the input untouched.
    file: Option<Vec<u8>>,
    info: Option<Vec<u8>>,
    dump: Option<Vec<u8>>,
}

fn load_cases() -> Vec<Case> {
    let manifest = std::fs::read_to_string(Path::new(DATA).join("cases.tsv")).unwrap();
    manifest
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().unwrap().to_owned();
            let input = fields.next().unwrap().to_owned();
            let expect = match fields.next().unwrap() {
                "same" => Expect::Same,
                "refuse" => Expect::Refuse,
                "clean-error" => Expect::CleanError,
                "unsupported" => Expect::Unsupported,
                other => panic!("{id}: unknown expectation {other}"),
            };
            Case {
                id,
                input,
                expect,
                args: fields.map(str::to_owned).collect(),
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

/// The recorded results by case id. `expected/<input>.vectors` were recorded
/// on macOS; on Linux, `expected/<input>.linux.vectors` replace the cases
/// where the reference behaves differently there, for example the glibc
/// zero-size reallocation failure or unfused AVERAGE back-fill arithmetic.
fn load_vectors() -> BTreeMap<String, Vector> {
    let dir = Path::new(DATA).join("expected");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut raw = BTreeMap::new();
    for name in names.iter().filter(|name| name.matches('.').count() == 1) {
        parse_vectors(&std::fs::read(dir.join(name)).unwrap(), &mut raw);
    }
    if cfg!(target_os = "linux") {
        for name in names.iter().filter(|name| name.ends_with(".linux.vectors")) {
            let mut overrides = BTreeMap::new();
            parse_vectors(&std::fs::read(dir.join(name)).unwrap(), &mut overrides);
            raw.extend(overrides);
        }
    }
    raw.into_iter()
        .map(|(id, mut parts)| {
            let status = match parts.remove("status").unwrap().as_slice() {
                b"signal" => None,
                code => Some(std::str::from_utf8(code).unwrap().parse().unwrap()),
            };
            let vector = Vector {
                outcome: Outcome {
                    status,
                    stdout: parts.remove("stdout").unwrap(),
                    stderr: parts.remove("stderr").unwrap(),
                },
                file: parts.remove("file"),
                info: parts.remove("info"),
                dump: parts.remove("dump"),
            };
            (id, vector)
        })
        .collect()
}

fn input_bytes(case: &Case) -> Vec<u8> {
    std::fs::read(
        Path::new(DATA)
            .join("inputs")
            .join(format!("{}.rrd", case.input)),
    )
    .unwrap()
}

fn word(bytes: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}

/// Names of the data sources `args` adds.
fn added_names(args: &[String]) -> Vec<&str> {
    args.iter()
        .filter_map(|arg| arg.strip_prefix("DS:"))
        .filter(|def| !def.is_empty())
        .map(|def| def.split([':', '=']).next().unwrap())
        .collect()
}

/// The comparison form of a file: the unspecified bytes of every added data
/// source definition are zeroed (byte 39, the last byte of the type field,
/// and bytes 64..120, the parameters the type does not use), and on x86_64
/// the negative quiet NaN the reference writes for new unknowns is folded
/// into the positive one the recording platform writes.
fn comparable(bytes: &[u8], args: &[String]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    if out.len() >= 128 {
        let added = added_names(args);
        for index in 0..word(bytes, 24) {
            let offset = 128 + 120 * index;
            if offset + 120 > out.len() {
                break;
            }
            let field = &bytes[offset..offset + 20];
            let name = &field[..field.iter().position(|b| *b == 0).unwrap_or(20)];
            if added.iter().any(|added| added.as_bytes() == name) {
                out[offset + 39] = 0;
                out[offset + 64..offset + 120].fill(0);
            }
        }
    }
    if cfg!(target_arch = "x86_64") {
        let (words, _) = out.as_chunks_mut::<8>();
        for word in words {
            if *word == 0xfff8_0000_0000_0000_u64.to_le_bytes() {
                *word = 0x7ff8_0000_0000_0000_u64.to_le_bytes();
            }
        }
    }
    out
}

fn run(program: &Path, dir: &Path, args: &[&str]) -> Outcome {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env_remove("RRDCACHED_ADDRESS")
        .output()
        .unwrap();
    Outcome {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    }
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

/// Everything one implementation did for one case.
struct Observed {
    outcome: Outcome,
    file: Vec<u8>,
    info: Outcome,
    dump: Outcome,
    leftovers: Vec<String>,
}

fn observe(program: &Path, work: &Path, case: &Case) -> Observed {
    let dir = work.join(&case.id);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("case.rrd"), input_bytes(case)).unwrap();
    let mut command = vec!["tune", "case.rrd"];
    command.extend(case.args.iter().map(String::as_str));
    let outcome = run(program, &dir, &command);
    let info = run(program, &dir, &["info", "case.rrd"]);
    let dump = run(program, &dir, &["dump", "case.rrd"]);
    let leftovers = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "case.rrd")
        .collect();
    Observed {
        outcome,
        file: std::fs::read(dir.join("case.rrd")).unwrap(),
        info,
        dump,
        leftovers,
    }
}

fn show(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() > 400 {
        format!("{}...", &text[..400])
    } else {
        text.into_owned()
    }
}

/// First differing offset, for failure messages.
fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(offset) => format!("first difference at byte {offset} (0x{offset:x})"),
        None => format!("lengths {} and {}", a.len(), b.len()),
    }
}

/// Checks `seen` against the vector according to the case's expectation.
/// `who` names the implementation in failure messages.
fn check(case: &Case, vector: &Vector, seen: &Observed, who: &str, rondi: bool) -> Vec<String> {
    let mut failures = Vec::new();
    let mut fail =
        |what: String| failures.push(format!("{} [{who}] {:?}: {what}", case.id, case.args));
    let input = input_bytes(case);
    if !seen.leftovers.is_empty() {
        fail(format!("left files behind: {:?}", seen.leftovers));
    }
    let expect = if rondi { case.expect } else { Expect::Same };
    match expect {
        Expect::Same => {
            if seen.outcome != vector.outcome {
                fail(format!(
                    "outcome {:?} / {} / {}, expected {:?} / {} / {}",
                    seen.outcome.status,
                    show(&seen.outcome.stdout),
                    show(&seen.outcome.stderr),
                    vector.outcome.status,
                    show(&vector.outcome.stdout),
                    show(&vector.outcome.stderr)
                ));
            }
            // Non-"same" cases recorded garbage or a crash upstream; for
            // the live oracle only the outcome is reproducible there.
            if case.expect != Expect::Same {
                return failures;
            }
            let want = vector.file.as_deref().unwrap_or(&input);
            let (got, want) = (
                comparable(&seen.file, &case.args),
                comparable(want, &case.args),
            );
            if got != want {
                fail(format!("file differs: {}", first_difference(&got, &want)));
            }
            for (name, recorded, observed) in [
                ("info", &vector.info, &seen.info),
                ("dump", &vector.dump, &seen.dump),
            ] {
                if let Some(recorded) = recorded
                    && (observed.status != Some(0) || &observed.stdout != recorded)
                {
                    fail(format!(
                        "{name} differs: {}",
                        first_difference(&observed.stdout, recorded)
                    ));
                }
            }
        }
        Expect::Refuse => {
            if seen.outcome != vector.outcome {
                fail(format!(
                    "outcome {:?}, expected {:?}",
                    seen.outcome, vector.outcome
                ));
            }
            if seen.file != input {
                fail("changed the file while refusing".into());
            }
        }
        Expect::CleanError => {
            if seen.outcome.status != Some(1) || !seen.outcome.stderr.starts_with(b"ERROR: ") {
                fail(format!("expected a clean error, got {:?}", seen.outcome));
            }
            if seen.file != input {
                fail("changed the file while failing".into());
            }
        }
        Expect::Unsupported => {
            let refused =
                String::from_utf8_lossy(&seen.outcome.stderr).contains("not supported by Rondi");
            if seen.outcome.status != Some(1) || !refused {
                fail(format!(
                    "expected an unsupported-feature error, got {:?}",
                    seen.outcome
                ));
            }
            if seen.file != input {
                fail("changed the file while refusing".into());
            }
        }
    }
    failures
}

fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} failing checks:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The vector set is large enough to pin the behavior the specification
/// describes: every DS type and CF, each operation and the error paths.
#[test]
fn tune_modify_vectors_cover_the_specification() {
    let cases = load_cases();
    assert!(cases.len() >= 150, "only {} cases", cases.len());
    let joined: Vec<String> = cases.iter().map(|case| case.args.join(" ")).collect();
    for needle in [
        "DS:",
        "DEL:",
        "DELRRA:",
        "RRA:AVERAGE",
        "RRA:MIN",
        "RRA:MAX",
        "RRA:LAST",
        ":+",
        ":-",
        ":=",
        "--step",
        ":GAUGE:",
        ":COUNTER:",
        ":DERIVE:",
        ":ABSOLUTE:",
    ] {
        assert!(
            joined.iter().any(|args| args.contains(needle)),
            "no case uses {needle}"
        );
    }
    let vectors = load_vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
    }
    assert_eq!(vectors.len(), cases.len(), "vectors without a case");
    let errors = vectors
        .values()
        .filter(|vector| vector.outcome.status != Some(0))
        .count();
    assert!(errors >= 25, "only {errors} error cases");
}

/// Rondi reproduces every frozen vector.
#[test]
fn rondi_matches_frozen_tune_modify_vectors() {
    let bench = Bench::new();
    let vectors = load_vectors();
    let mut failures = Vec::new();
    for case in load_cases() {
        let seen = observe(&bench.rondi, &bench.work, &case);
        failures.extend(check(&case, &vectors[&case.id], &seen, "rondi", true));
    }
    report(&failures);
}

/// The pinned RRDtool still produces the frozen vectors on this platform,
/// and Rondi matches it directly, including `info` and `dump` for the cases
/// whose vectors do not keep that text.
#[test]
fn rrdtool_reproduces_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live tune vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    let bench = Bench::new();
    let oracle = PathBuf::from("rrdtool");
    let upstream_work = bench.work.join("upstream");
    let rondi_work = bench.work.join("rondi");
    std::fs::create_dir(&upstream_work).unwrap();
    std::fs::create_dir(&rondi_work).unwrap();
    let vectors = load_vectors();
    let mut failures = Vec::new();
    for case in load_cases() {
        let upstream = observe(&oracle, &upstream_work, &case);
        failures.extend(check(
            &case,
            &vectors[&case.id],
            &upstream,
            "rrdtool",
            false,
        ));
        if case.expect != Expect::Same {
            continue;
        }
        let rondi = observe(&bench.rondi, &rondi_work, &case);
        let mut fail = |what: String| {
            failures.push(format!(
                "{} [rondi vs rrdtool] {:?}: {what}",
                case.id, case.args
            ))
        };
        if rondi.outcome != upstream.outcome {
            fail(format!(
                "outcome {:?}, rrdtool {:?}",
                rondi.outcome, upstream.outcome
            ));
        }
        let (got, want) = (
            comparable(&rondi.file, &case.args),
            comparable(&upstream.file, &case.args),
        );
        if got != want {
            fail(format!("file differs: {}", first_difference(&got, &want)));
        }
        // RRDtool's own info and dump abort on a file without archives.
        if upstream.info.status == Some(0) && rondi.info != upstream.info {
            fail(format!(
                "info differs: {}",
                first_difference(&rondi.info.stdout, &upstream.info.stdout)
            ));
        }
        if upstream.dump.status == Some(0) && rondi.dump != upstream.dump {
            fail(format!(
                "dump differs: {}",
                first_difference(&rondi.dump.stdout, &upstream.dump.stdout)
            ));
        }
    }
    report(&failures);
}

/// File replacement: a successful structure change writes a new file and
/// renames it over the old name, so the inode changes, hard links keep the
/// old content, a symbolic link is replaced by a regular file and the mode
/// is carried over. Header options are written in place before the
/// structure change and stay written when it fails. Nothing is left behind.
#[test]
fn tune_modify_replaces_the_file() {
    let bench = Bench::new();
    let mut programs = vec![("rondi", bench.rondi.clone())];
    if Command::new("rrdtool").arg("--version").output().is_ok() {
        common::require_oracle();
        programs.push(("rrdtool", PathBuf::from("rrdtool")));
    } else {
        common::skipped("tune_modify_replaces_the_file: checking Rondi only".into());
    }
    let base = std::fs::read(Path::new(DATA).join("inputs/base.rrd")).unwrap();
    for (who, program) in programs {
        let dir = bench.work.join(who);
        std::fs::create_dir(&dir).unwrap();
        let path = |name: &str| dir.join(name);
        let inode = |name: &str| std::fs::metadata(path(name)).unwrap().ino();
        let mode = |name: &str| std::fs::symlink_metadata(path(name)).unwrap().mode();

        // A successful change replaces the inode and keeps the mode.
        std::fs::write(path("a.rrd"), &base).unwrap();
        std::fs::set_permissions(
            path("a.rrd"),
            std::os::unix::fs::PermissionsExt::from_mode(0o640),
        )
        .unwrap();
        std::fs::hard_link(path("a.rrd"), path("a-link.rrd")).unwrap();
        let before = inode("a.rrd");
        let outcome = run(&program, &dir, &["tune", "a.rrd", "DEL:c"]);
        assert_eq!(outcome.status, Some(0), "{who}: {outcome:?}");
        assert_ne!(inode("a.rrd"), before, "{who}: inode kept");
        assert_eq!(mode("a.rrd") & 0o7777, 0o640, "{who}: mode");
        assert_eq!(
            std::fs::read(path("a-link.rrd")).unwrap(),
            base,
            "{who}: hard link changed"
        );

        // A symbolic link is replaced by a regular file; its target stays.
        std::fs::write(path("target.rrd"), &base).unwrap();
        std::os::unix::fs::symlink("target.rrd", path("sym.rrd")).unwrap();
        let outcome = run(&program, &dir, &["tune", "sym.rrd", "DEL:c"]);
        assert_eq!(outcome.status, Some(0), "{who}: {outcome:?}");
        assert!(
            std::fs::symlink_metadata(path("sym.rrd"))
                .unwrap()
                .file_type()
                .is_file(),
            "{who}: symlink kept"
        );
        assert_eq!(
            std::fs::read(path("target.rrd")).unwrap(),
            base,
            "{who}: symlink target changed"
        );

        // No structure argument, or only ignored ones: nothing is rewritten.
        std::fs::write(path("n.rrd"), &base).unwrap();
        let before = inode("n.rrd");
        for args in [&["tune", "n.rrd"][..], &["tune", "n.rrd", "--step", "100"]] {
            let outcome = run(&program, &dir, args);
            assert_eq!(outcome.status, Some(0), "{who} {args:?}: {outcome:?}");
            assert_eq!(inode("n.rrd"), before, "{who} {args:?}: rewritten");
        }

        // A failed change leaves the inode, but header options given with
        // it were already written in place.
        std::fs::write(path("h.rrd"), &base).unwrap();
        let before = inode("h.rrd");
        let outcome = run(
            &program,
            &dir,
            &["tune", "h.rrd", "-h", "g:900", "DS:g:GAUGE:600:U:U"],
        );
        assert_eq!(outcome.status, Some(1), "{who}: {outcome:?}");
        assert_eq!(outcome.stderr, b"ERROR: Duplicate DS name: g\n", "{who}");
        assert_eq!(inode("h.rrd"), before, "{who}: replaced on failure");
        let info = run(&program, &dir, &["info", "h.rrd"]);
        assert!(
            String::from_utf8_lossy(&info.stdout).contains("ds[g].minimal_heartbeat = 900\n"),
            "{who}: heartbeat not written in place"
        );

        // A directory that cannot hold the temporary file.
        std::fs::create_dir(path("ro")).unwrap();
        std::fs::write(path("ro/r.rrd"), &base).unwrap();
        std::fs::set_permissions(
            path("ro"),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let outcome = run(&program, &dir, &["tune", "ro/r.rrd", "DEL:c"]);
        std::fs::set_permissions(
            path("ro"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        // Root can write into a read-only directory, so only check the
        // failure where the permission actually applies.
        if outcome.status != Some(0) {
            assert_eq!(
                outcome.stderr, b"ERROR: Cannot create temporary file\n",
                "{who}"
            );
            assert_eq!(std::fs::read(path("ro/r.rrd")).unwrap(), base, "{who}");
        }

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "a-link.rrd",
                "a.rrd",
                "h.rrd",
                "n.rrd",
                "ro",
                "sym.rrd",
                "target.rrd"
            ],
            "{who}: leftover files"
        );
    }
}
