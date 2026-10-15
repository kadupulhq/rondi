//! Replays the command-line front end vectors in
//! `data/cli_spec_vectors.json`.
//!
//! The vectors were captured from RRDtool 1.11.0 by
//! `scripts/cli-spec-vectors.py` and are described in
//! `docs/compatibility/cli.md`, `docs/compatibility/pipe-mode.md` and
//! `docs/compatibility/format-strings.md`. Every vector is replayed against
//! the Rondi binary under the `rrdtool` name (and under `rrdupdate`,
//! `rrdcreate` and `rrdinfo` for alias vectors). When the pinned oracle is
//! installed, each vector is also run through it and Rondi must match the
//! live result as well; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a
//! failure.
//!
//! Output bytes are read as Latin-1 so that every byte maps to one
//! character, and standard input is written the same way. Both sides are
//! normalized as the capture script does: the build stamp after
//! `Compiled `, the pipe-mode timing figures, archive `cur_row` values, the
//! program directory, the work directory and the order of pipe-mode `ls`
//! lines.
#![cfg(unix)]

#[macro_use]
mod common;

use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VECTORS: &str = include_str!("data/cli_spec_vectors.json");
const ALIASES: &[&str] = &["rrdtool", "rrdupdate", "rrdcreate", "rrdinfo"];
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

/// Vectors where Rondi currently differs from RRDtool 1.11.0, with the
/// platforms where they differ (empty means all). The frozen expectation
/// stays RRDtool's; these ids are reported instead of failing. A listed
/// vector that starts matching fails the test so the entry is removed.
/// The reasons are summarized in the specifications' known divergence
/// sections.
const KNOWN_DIVERGENCES: &[(&str, &[&str], &str)] = &[
    (
        "cli-env-001",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-002",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-003",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-004",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-005",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-007",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-009",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-017",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-018",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-019",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-020",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-021",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-023",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-024",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-025",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-alias-032",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "pipe-env-001",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "pipe-env-002",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "pipe-session-132",
        ALL,
        "daemon unreachable: Rondi does not read the file and print the result before the error",
    ),
    (
        "cli-env-012",
        ALL,
        "create applies RRDCACHED_ADDRESS and fails; RRDtool create ignores it",
    ),
    (
        "cli-env-013",
        ALL,
        "tune reports the connection error instead of `Cannot connect to daemon`",
    ),
    (
        "cli-optparse-204",
        ALL,
        "create --source requires the start time to match the source last update",
    ),
    (
        "cli-optparse-205",
        ALL,
        "create --source requires the start time to match the source last update",
    ),
    (
        "cli-commands-160",
        ALL,
        "graph /dev/null with no graph elements prints 0x0",
    ),
    (
        "cli-locale-002",
        ALL,
        "info ignores a comma decimal separator in the numeric locale",
    ),
    (
        "cli-locale-006",
        ALL,
        "info ignores a comma decimal separator in the numeric locale",
    ),
    (
        "cli-locale-010",
        ALL,
        "info ignores a comma decimal separator in the numeric locale",
    ),
    (
        "pipe-session-117",
        ALL,
        "non-UTF-8 bytes in error messages are replaced by U+FFFD",
    ),
    (
        "pipe-session-145",
        ALL,
        "non-UTF-8 bytes in error messages are replaced by U+FFFD",
    ),
    (
        "format-badprint-041",
        ALL,
        "very large field widths and precisions are rejected instead of printed",
    ),
    (
        "format-badgprint-041",
        ALL,
        "very large field widths and precisions are rejected instead of printed",
    ),
    (
        "format-badprint-042",
        ALL,
        "very large field widths and precisions are rejected instead of printed",
    ),
    (
        "format-badgprint-042",
        ALL,
        "very large field widths and precisions are rejected instead of printed",
    ),
    (
        "format-bounds-020",
        ALL,
        "very large field widths and precisions are rejected instead of printed",
    ),
    (
        "format-duration-048",
        &["macos", "linux-aarch64"],
        "duration fields of 1e20 ms differ in the last digits",
    ),
    (
        "pipe-session-144",
        &["linux-aarch64"],
        "non-UTF-8 bytes in error messages are replaced by U+FFFD",
    ),
    (
        "format-bounds-003",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-006",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-009",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-012",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-015",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-018",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-021",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-032",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-033",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-034",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
    (
        "format-bounds-035",
        ALL,
        "strftime output longer than the reference buffer prints an empty line",
    ),
];

/// Platform list of a divergence seen on every platform.
const ALL: &[&str] = &[];

struct Vector {
    id: String,
    env: Vec<(String, String)>,
    steps: Vec<Step>,
    tags: Vec<String>,
    expect: Value,
    expect_platform: Value,
}

struct Step {
    argv0: String,
    args: Vec<String>,
    stdin: Option<Vec<u8>>,
}

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

fn german_locale_available() -> bool {
    Command::new("locale")
        .arg("-a")
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.eq_ignore_ascii_case("de_DE.UTF-8") || line == "de_DE.utf8")
        })
}

/// A named zone needs the host's zoneinfo file, which slim images may lack;
/// POSIX rule strings, the empty value and the deliberately unknown name do
/// not.
fn zone_available(zone: &str) -> bool {
    !zone.contains('/')
        || zone == "Not/AZone"
        || zone.starts_with('<')
        || Path::new("/usr/share/zoneinfo").join(zone).is_file()
}

impl Vector {
    fn tagged(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// The frozen expectation for this build target, if one applies.
    fn frozen(&self, locale: bool) -> Option<&Value> {
        if self.tagged("unstable") || (self.tagged("locale") && !locale) {
            return None;
        }
        // The zone name of an empty TZ comes from the host's time zone
        // database on glibc, so it is frozen on macOS only.
        if self.tagged("host-tz") && !cfg!(target_os = "macos") {
            return None;
        }
        let zone_missing = self
            .env
            .iter()
            .any(|(key, zone)| key == "TZ" && !zone_available(zone));
        if zone_missing {
            return None;
        }
        if let Some(recorded) = platform_key().and_then(|key| self.expect_platform.get(key)) {
            return Some(recorded);
        }
        Some(&self.expect)
    }
}

fn latin1_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).expect("stdin is Latin-1"))
        .collect()
}

fn latin1_text(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

fn load() -> (Value, Vec<Vector>) {
    let table: Value = serde_json::from_str(VECTORS).expect("cli_spec_vectors.json parses");
    let vectors = table["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| Vector {
            id: case["id"].as_str().unwrap().to_owned(),
            env: case
                .get("env")
                .and_then(Value::as_object)
                .map(|env| {
                    env.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect()
                })
                .unwrap_or_default(),
            steps: case["steps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|step| Step {
                    argv0: step
                        .get("argv0")
                        .and_then(Value::as_str)
                        .unwrap_or("rrdtool")
                        .to_owned(),
                    args: strings(&step["args"]),
                    stdin: step.get("stdin").and_then(Value::as_str).map(latin1_bytes),
                })
                .collect(),
            tags: strings(&case["tags"]),
            expect: case.get("expect").cloned().unwrap_or(Value::Null),
            expect_platform: case.get("expect_platform").cloned().unwrap_or(Value::Null),
        })
        .collect();
    (table, vectors)
}

/// Line-wise normalization shared with the capture script.
fn normalize(text: &str, work: &Path, bindir: &Path) -> String {
    let given = work.to_string_lossy().into_owned();
    let real = std::fs::canonicalize(work)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| given.clone());
    let mut paths = vec![given, real];
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
    paths.dedup();
    let bin = format!("{}/", bindir.display());

    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let mut line = raw.to_owned();
        if let Some(position) = line.find("Compiled ") {
            line.truncate(position + "Compiled ".len());
        }
        if is_timing(&line) {
            line = "OK u:* s:* r:*".to_owned();
        }
        if let Some(prefix) = cur_row_prefix(&line) {
            line = format!("{prefix}*");
        }
        line = line.replace(&bin, "{BIN}/");
        for path in &paths {
            line = line.replace(path.as_str(), "{CWD}");
        }
        if line.starts_with("d ") || line.starts_with("- ") {
            run.push(line);
            continue;
        }
        run.sort();
        out.append(&mut run);
        out.push(line);
    }
    run.sort();
    out.append(&mut run);
    out.join("\n")
}

/// `OK u:<d>.<dd> s:<d>.<dd> r:<d>.<dd>`
fn is_timing(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("OK ") else {
        return false;
    };
    let fields: Vec<&str> = rest.split(' ').collect();
    fields.len() == 3
        && fields.iter().zip(["u:", "s:", "r:"]).all(|(field, key)| {
            field.strip_prefix(key).is_some_and(|number| {
                number.split_once('.').is_some_and(|(whole, frac)| {
                    !whole.is_empty()
                        && whole.bytes().all(|b| b.is_ascii_digit())
                        && frac.len() == 2
                        && frac.bytes().all(|b| b.is_ascii_digit())
                })
            })
        })
}

/// `rra[<n>].cur_row = <digits>` returns the text before the digits.
fn cur_row_prefix(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("rra[")?;
    let close = rest.find(']')?;
    if close == 0 || !rest[..close].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = rest[close..].strip_prefix("].cur_row = ")?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(&line[..line.len() - value.len()])
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Ran {
    status: Option<i32>,
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs one process with a deadline so that a hang fails the vector
/// instead of the whole test binary.
fn run_bounded(mut command: Command, stdin: &[u8]) -> Ran {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    // A child that exits without reading its input closes the pipe; that
    // write error is expected.
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        out.read_to_end(&mut buffer).unwrap();
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        err.read_to_end(&mut buffer).unwrap();
        buffer
    });
    let deadline = Instant::now() + STEP_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            timed_out = true;
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    writer.join().unwrap();
    Ran {
        status: status.code(),
        timed_out,
        stdout: out_reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}

fn run_vector(bindir: &Path, fixture: &Path, vector: &Vector) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("w");
    copy_tree(fixture, &work);
    let mut results = Vec::new();
    for step in &vector.steps {
        let mut command = Command::new(bindir.join(&step.argv0));
        command.args(&step.args).current_dir(&work).env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name == "RRDCACHED_ADDRESS"
                || name == "TZ"
                || name == "LANG"
                || name == "RRD_LOCKING"
                || name.starts_with("LC_")
            {
                continue;
            }
            command.env(key, value);
        }
        command.env("TZ", "UTC").env("LC_ALL", "C");
        for (key, value) in &vector.env {
            command.env(key, value);
        }
        let ran = run_bounded(command, step.stdin.as_deref().unwrap_or_default());
        if ran.timed_out {
            results.push(json!({"status": null, "timeout": true}));
            break;
        }
        let Some(code) = ran.status else {
            results.push(json!({"status": null, "crash": true}));
            break;
        };
        let stdout = if vector.tagged("binary-stdout") {
            format!("<{} bytes>", ran.stdout.len())
        } else {
            normalize(&latin1_text(&ran.stdout), &work, bindir)
        };
        let mut result = Map::new();
        result.insert("status".into(), json!(code));
        result.insert("stdout".into(), json!(stdout));
        result.insert(
            "stderr".into(),
            json!(normalize(&latin1_text(&ran.stderr), &work, bindir)),
        );
        results.push(Value::Object(result));
    }
    Value::Array(results)
}

/// The form of a result that is compared. Directory enumeration order
/// differs between file systems, so `dir-order` vectors compare the sorted
/// lines of standard output.
fn comparable(vector: &Vector, result: &Value) -> Value {
    if !vector.tagged("dir-order") {
        return result.clone();
    }
    let steps = result
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            let mut step = step.clone();
            if let Some(stdout) = step.get("stdout").and_then(Value::as_str) {
                let mut lines: Vec<&str> = stdout.split('\n').collect();
                lines.sort_unstable();
                step["stdout"] = json!(lines.join("\n"));
            }
            step
        })
        .collect();
    Value::Array(steps)
}

fn failed_to_finish(result: &Value) -> bool {
    result
        .as_array()
        .unwrap()
        .iter()
        .any(|step| step["status"].is_null())
}

/// Builds the fixture directory the way the capture script does: the
/// directories, the text and XML files, then `restore` with the program
/// under test.
fn setup(bindir: &Path, dir: &Path, table: &Value) {
    for name in strings(&table["dirs"]) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    for (name, text) in table["files"].as_object().unwrap() {
        std::fs::write(dir.join(name), text.as_str().unwrap()).unwrap();
    }
    for command in table["fixture"].as_array().unwrap() {
        let output = Command::new(bindir.join("rrdtool"))
            .args(strings(command))
            .current_dir(dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture {command}: {output:?}");
    }
}

/// The directory holding the pinned oracle and its alias executables.
fn live_oracle() -> Option<PathBuf> {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).starts_with("RRDtool 1.11.0 "));
    if !pinned {
        oracle_skip!("cli_spec_vectors: pinned RRDtool 1.11.0 not found, live comparison skipped");
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("rrdtool").is_file())
}

#[test]
fn normalization_helpers() {
    assert!(is_timing("OK u:0.01 s:0.00 r:12.34"));
    assert!(!is_timing("OK u:0.1 s:0.00 r:0.00"));
    assert!(!is_timing("OK"));
    assert_eq!(
        cur_row_prefix("rra[12].cur_row = 7"),
        Some("rra[12].cur_row = ")
    );
    assert_eq!(cur_row_prefix("rra[1].cur_row = x"), None);
    let text = "OK u:0.01 s:0.00 r:0.00\n- b.rrd\nd .\nrra[0].cur_row = 3\nCompiled Jan  1";
    assert_eq!(
        normalize(text, Path::new("/nonexistent-dir"), Path::new("/bin")),
        "OK u:* s:* r:*\n- b.rrd\nd .\nrra[0].cur_row = *\nCompiled "
    );
    assert_eq!(latin1_text(&latin1_bytes("a\u{ff}\u{e9}")), "a\u{ff}\u{e9}");
}

#[test]
fn cli_spec_vectors_match_rrdtool() {
    let (table, vectors) = load();
    assert!(vectors.len() >= 600, "only {} vectors", vectors.len());
    let locale = german_locale_available();
    let temp = tempfile::tempdir().unwrap();
    let bindir = temp.path().join("bin");
    std::fs::create_dir(&bindir).unwrap();
    for name in ALIASES {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), bindir.join(name)).unwrap();
    }
    let ro = temp.path().join("ro");
    setup(&bindir, &ro, &table);
    let oracle = live_oracle();
    let up = temp.path().join("up");
    if let Some(dir) = &oracle {
        setup(dir, &up, &table);
    }

    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(6));
    let chunk = vectors.len().div_ceil(threads);
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = vectors
            .chunks(chunk)
            .map(|part| {
                let (bindir, ro, up, oracle) = (&bindir, &ro, &up, &oracle);
                scope.spawn(move || {
                    part.iter()
                        .map(|vector| check(vector, bindir, ro, up, oracle.as_deref(), locale))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });

    let mut failures = Vec::new();
    let mut divergent = Vec::new();
    for outcome in outcomes {
        let (id, ok, detail) = match outcome {
            Outcome::Matches(id) => (id, true, String::new()),
            Outcome::Differs(id, detail) => (id, false, detail),
            Outcome::NotCompared => continue,
        };
        let here = platform_key().unwrap_or("macos");
        let known = KNOWN_DIVERGENCES.iter().any(|(known, platforms, _)| {
            *known == id && (platforms.is_empty() || platforms.contains(&here))
        });
        match (ok, known) {
            (true, true) => failures.push(format!(
                "{id}: now matches RRDtool; remove it from KNOWN_DIVERGENCES"
            )),
            (false, true) => divergent.push(id),
            (false, false) => failures.push(format!("{id}\n  {detail}")),
            (true, false) => {}
        }
    }
    if !divergent.is_empty() {
        eprintln!("known front end divergences still present: {divergent:?}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} front end vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

fn check(
    vector: &Vector,
    bindir: &Path,
    ro: &Path,
    up: &Path,
    oracle: Option<&Path>,
    locale: bool,
) -> Outcome {
    let id = vector.id.clone();
    let got = run_vector(bindir, ro, vector);
    if failed_to_finish(&got) {
        // Whatever the reference does, Rondi must exit on its own.
        return Outcome::Differs(id, format!("rondi  {got}"));
    }
    if vector.tagged("undefined") {
        return Outcome::Matches(id);
    }
    let mut compared = false;
    if let Some(frozen) = vector.frozen(locale) {
        compared = true;
        if comparable(vector, frozen) != comparable(vector, &got) {
            return Outcome::Differs(id, format!("frozen {frozen}\n  rondi  {got}"));
        }
    }
    if let Some(dir) = oracle
        && (locale || !vector.tagged("locale"))
    {
        let reference = run_vector(dir, up, vector);
        if !failed_to_finish(&reference) {
            compared = true;
            if comparable(vector, &reference) != comparable(vector, &got) {
                return Outcome::Differs(id, format!("live   {reference}\n  rondi  {got}"));
            }
        }
    }
    if compared {
        Outcome::Matches(id)
    } else {
        Outcome::NotCompared
    }
}

/// The result of checking one vector. A vector whose expectation does not
/// apply here and that has no live oracle is not compared, so it neither
/// passes nor fails, and a known divergence is not reported as fixed.
enum Outcome {
    Matches(String),
    Differs(String, String),
    NotCompared,
}
