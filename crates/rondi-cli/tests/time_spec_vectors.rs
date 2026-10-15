//! Replays the at-style time specification vectors in
//! `data/time_spec_vectors.json`.
//!
//! The vectors were captured from RRDtool 1.11.0 by
//! `scripts/time-spec-vectors.py` and are described in
//! `docs/compatibility/time-spec.md`. Every vector is replayed against the
//! Rondi binary under the `rrdtool` name. When the pinned oracle is
//! installed, each vector is also run through it and Rondi must match the
//! live result as well.
//!
//! A vector with `same_as` depends on the current time. Its rule is that the
//! two argument lists give the same result once the placeholders in
//! `same_as` are filled from `date` in the vector's time zone.
#![cfg(unix)]

#[macro_use]
mod common;

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const VECTORS: &str = include_str!("data/time_spec_vectors.json");

/// Vectors where Rondi currently differs from RRDtool 1.11.0. The frozen
/// expectation stays RRDtool's; these ids are reported instead of failing.
/// A listed vector that starts matching fails the test so the entry is
/// removed.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[];

struct Vector {
    id: String,
    tz: String,
    steps: Vec<Vec<String>>,
    same_as: Option<Vec<Vec<String>>>,
    tags: Vec<String>,
    expect: Value,
    expect_platform: Value,
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

/// A named zone needs the host's zoneinfo file, which slim images may lack;
/// the empty value, POSIX rule strings and the deliberately unknown name do
/// not.
fn zone_available(zone: &str) -> bool {
    !zone.contains('/')
        || zone == "Not/A_Zone"
        || zone.starts_with('<')
        || Path::new("/usr/share/zoneinfo").join(zone).is_file()
}

impl Vector {
    fn tagged(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// The frozen expectation for this build target, if one applies. A
    /// per-platform recording wins. A repeated local hour is resolved by
    /// glibc from the current time, so `dst-overlap` vectors are frozen on
    /// macOS only; the live oracle comparison covers Linux.
    fn frozen(&self) -> Option<&Value> {
        if !zone_available(&self.tz) {
            return None;
        }
        if self.tagged("dst-overlap") && !cfg!(target_os = "macos") {
            return None;
        }
        if let Some(recorded) = platform_key().and_then(|key| self.expect_platform.get(key)) {
            return Some(recorded);
        }
        Some(&self.expect)
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_owned())
        .collect()
}

fn lists(value: &Value) -> Vec<Vec<String>> {
    value.as_array().unwrap().iter().map(strings).collect()
}

fn load() -> (Vec<Vec<String>>, Vec<Vector>) {
    let table: Value = serde_json::from_str(VECTORS).expect("time_spec_vectors.json parses");
    let vectors = table["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| Vector {
            id: case["id"].as_str().unwrap().to_owned(),
            tz: case["tz"].as_str().unwrap().to_owned(),
            steps: lists(&case["steps"]),
            same_as: case.get("same_as").map(lists),
            tags: strings(&case["tags"]),
            expect: case.get("expect").cloned().unwrap_or(Value::Null),
            expect_platform: case.get("expect_platform").cloned().unwrap_or(Value::Null),
        })
        .collect();
    (lists(&table["fixture"]), vectors)
}

/// Keeps the part of stdout the vectors record, as the capture script does.
fn filtered(args: &[String], stdout: &str) -> String {
    let keep: Vec<&str> = match args[0].as_str() {
        "graphv" => stdout
            .lines()
            .filter(|line| {
                line.starts_with("graph_start")
                    || line.starts_with("graph_end")
                    || line.starts_with("print[")
            })
            .collect(),
        "dump" => stdout
            .lines()
            .filter(|line| line.contains("<!--") && line.contains(" / "))
            .map(str::trim)
            .collect(),
        _ => return stdout.to_owned(),
    };
    keep.join("\n")
}

fn run_steps(program: &Path, fixture: &Path, steps: &[Vec<String>], tz: &str) -> Value {
    let work = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(fixture).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), work.path().join(entry.file_name())).unwrap();
    }
    let mut results = Vec::new();
    for args in steps {
        let output = Command::new(program)
            .args(args)
            .current_dir(work.path())
            .env("TZ", tz)
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        let Some(code) = output.status.code() else {
            results.push(json!({"status": null, "crash": true}));
            break;
        };
        let mut result = Map::new();
        result.insert("status".into(), json!(code));
        result.insert(
            "stdout".into(),
            json!(filtered(args, &String::from_utf8_lossy(&output.stdout))),
        );
        result.insert(
            "stderr".into(),
            json!(String::from_utf8_lossy(&output.stderr)),
        );
        results.push(Value::Object(result));
    }
    Value::Array(results)
}

fn crashed(result: &Value) -> bool {
    result
        .as_array()
        .unwrap()
        .iter()
        .any(|step| step["status"].is_null())
}

/// Local date fields of the current instant in `tz`.
fn date_sample(tz: &str) -> Vec<String> {
    let output = Command::new("date")
        .arg("+%Y %m %d %H %M %S %s %w")
        .env("TZ", tz)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Fills `{now}`, `{Y}`, `{m}`, `{d}`, `{H}`, `{M}`, `{S}`, `{D+k}` and
/// `{W:k}` from a `date` sample.
fn fill(text: &str, sample: &[String]) -> String {
    let field = |i: usize| sample[i].clone();
    let num = |i: usize| sample[i].parse::<i64>().unwrap();
    let today = days_from_civil(num(0), num(1), num(2));
    let date = |days: i64| {
        let (y, m, d) = civil_from_days(days);
        format!("{m:02}/{d:02}/{y:04}")
    };
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = open + rest[open..].find('}').unwrap();
        let key = &rest[open + 1..close];
        let value = match key {
            "Y" => field(0),
            "m" => field(1),
            "d" => field(2),
            "H" => field(3),
            "M" => field(4),
            "S" => field(5),
            "now" => field(6),
            _ if key.starts_with("W:") => date(today + key[2..].parse::<i64>().unwrap() - num(7)),
            _ if key.starts_with('D') => date(today + key[1..].parse::<i64>().unwrap()),
            _ => panic!("unknown placeholder {key}"),
        };
        out.push_str(&value);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Runs a time-dependent vector and its placeholder form, retrying when the
/// clock moves between the two runs. Returns both results.
fn run_equivalent(program: &Path, fixture: &Path, vector: &Vector) -> (Value, Value) {
    let same_as = vector.same_as.as_ref().unwrap();
    let mut last = (Value::Null, Value::Null);
    for _ in 0..5 {
        let before = date_sample(&vector.tz);
        let got = run_steps(program, fixture, &vector.steps, &vector.tz);
        let filled: Vec<Vec<String>> = same_as
            .iter()
            .map(|step| step.iter().map(|arg| fill(arg, &before)).collect())
            .collect();
        let want = run_steps(program, fixture, &filled, &vector.tz);
        if before == date_sample(&vector.tz) {
            if got == want {
                return (got, want);
            }
            last = (got, want);
        }
    }
    last
}

/// Runs a time-dependent vector through two programs, retrying when the
/// clock moves between them.
fn run_live_pair(rondi: &Path, ro: &Path, up: &Path, vector: &Vector) -> (Value, Value) {
    let oracle = Path::new("rrdtool");
    let mut last = (Value::Null, Value::Null);
    for _ in 0..5 {
        let before = date_sample(&vector.tz);
        let reference = run_steps(oracle, up, &vector.steps, &vector.tz);
        let got = run_steps(rondi, ro, &vector.steps, &vector.tz);
        if before == date_sample(&vector.tz) {
            if reference == got {
                return (reference, got);
            }
            last = (reference, got);
        }
    }
    last
}

fn setup(program: &Path, dir: &Path, fixture: &[Vec<String>]) {
    std::fs::create_dir_all(dir).unwrap();
    for command in fixture {
        let output = Command::new(program)
            .args(command)
            .current_dir(dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture {command:?}: {output:?}");
    }
}

fn live_oracle() -> bool {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).starts_with("RRDtool 1.11.0 "));
    if !pinned {
        oracle_skip!("time_spec_vectors: pinned RRDtool 1.11.0 not found, live comparison skipped");
    }
    pinned
}

#[test]
fn civil_date_helpers_round_trip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    assert_eq!(
        civil_from_days(days_from_civil(2024, 2, 29) + 1),
        (2024, 3, 1)
    );
    assert_eq!(civil_from_days(-1), (1969, 12, 31));
    let sample: Vec<String> = ["2024", "03", "01", "09", "05", "07", "1709283907", "5"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    assert_eq!(
        fill("{D-1} {W:0} {W:6}", &sample),
        "02/29/2024 02/25/2024 03/02/2024"
    );
    assert_eq!(
        fill("{H}:{M} +{S}s {now}", &sample),
        "09:05 +07s 1709283907"
    );
}

#[test]
fn time_spec_vectors_match_rrdtool() {
    let (fixture, vectors) = load();
    assert!(vectors.len() >= 500, "only {} vectors", vectors.len());
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let rondi: PathBuf = bin.join("rrdtool");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
    let ro = temp.path().join("ro");
    setup(&rondi, &ro, &fixture);
    let up = temp.path().join("up");
    let live = live_oracle();
    if live {
        setup(Path::new("rrdtool"), &up, &fixture);
    }

    let mut failures = Vec::new();
    let mut divergent = Vec::new();
    for vector in &vectors {
        let mut ok = true;
        let mut detail = String::new();
        if vector.tagged("undefined") {
            // RRDtool crashes or its result depends on memory contents;
            // Rondi only has to finish without a signal.
            if crashed(&run_steps(&rondi, &ro, &vector.steps, &vector.tz)) {
                failures.push(format!("{}: Rondi died on a signal", vector.id));
            }
            continue;
        }
        if vector.same_as.is_some() {
            if zone_available(&vector.tz) {
                let (got, want) = run_equivalent(&rondi, &ro, vector);
                if got != want || crashed(&got) {
                    ok = false;
                    detail = format!("spec   {got}\n  same_as {want}");
                }
            }
            if live {
                let (reference, got) = run_live_pair(&rondi, &ro, &up, vector);
                if reference != got {
                    ok = false;
                    detail = format!("live   {reference}\n  rondi  {got}");
                }
            }
        } else {
            let got = run_steps(&rondi, &ro, &vector.steps, &vector.tz);
            if crashed(&got) {
                ok = false;
                detail = format!("rondi  {got}");
            } else if !vector.tagged("unstable")
                && let Some(frozen) = vector.frozen()
                && *frozen != got
            {
                ok = false;
                detail = format!("frozen {frozen}\n  rondi  {got}");
            }
            if live {
                let reference = run_steps(Path::new("rrdtool"), &up, &vector.steps, &vector.tz);
                if !crashed(&reference) && reference != got {
                    ok = false;
                    detail = format!("live   {reference}\n  rondi  {got}");
                }
            }
        }
        let known = KNOWN_DIVERGENCES.iter().any(|(id, _)| *id == vector.id);
        match (ok, known) {
            (true, true) => failures.push(format!(
                "{}: now matches RRDtool; remove it from KNOWN_DIVERGENCES",
                vector.id
            )),
            (false, true) => divergent.push(vector.id.clone()),
            (false, false) => failures.push(format!(
                "{} TZ={} {:?}\n  {detail}",
                vector.id,
                vector.tz,
                vector.steps.last().unwrap()
            )),
            (true, false) => {}
        }
    }
    if !divergent.is_empty() {
        eprintln!("known time specification divergences still present: {divergent:?}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} time specification vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
