//! Replays the RPN compatibility vectors in `data/rpn_vectors.json`.
//!
//! The vectors were captured from RRDtool 1.11.0 by `scripts/rpn-vectors.py`
//! and are described in `docs/compatibility/rpn.md`. Every vector is
//! replayed against the Rondi binary under the `rrdtool` name. When the
//! pinned oracle is installed, each vector is also run through it and Rondi
//! must match the live result as well.
#![cfg(unix)]

#[macro_use]
mod common;

use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

const VECTORS: &str = include_str!("data/rpn_vectors.json");

/// Vectors where Rondi currently differs from RRDtool 1.11.0. The frozen
/// expectation stays RRDtool's; these ids are reported instead of failing.
/// A listed vector that starts matching fails the test so the entry is
/// removed.
const KNOWN_DIVERGENCES: &[(&str, &str)] = &[(
    "predictperc-19",
    "an unknown PREDICTPERC percentile yields unknown rows in RRDtool; Rondi panics",
)];

struct Vector {
    id: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    tags: Vec<String>,
    expect: Value,
    /// Expectations recorded on other platforms, keyed `linux-x86_64` or
    /// `linux-aarch64`, where the reference differs from the macOS capture.
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

impl Vector {
    fn tagged(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// The frozen expectation for this build target, if one applies.
    /// A per-platform recording wins. Otherwise `host-sort` and
    /// `first-weekday` results, which depend on the C library, apply on
    /// macOS only, and a named time zone needs the host's zoneinfo file,
    /// which slim container images may lack. `fma` results are compared
    /// with a tolerance off aarch64. The live oracle comparison covers
    /// every platform.
    fn frozen(&self) -> Option<&Value> {
        if let Some(recorded) = platform_key().and_then(|key| self.expect_platform.get(key)) {
            return Some(recorded);
        }
        if (self.tagged("host-sort") || self.tagged("first-weekday")) && !cfg!(target_os = "macos")
        {
            return None;
        }
        let zones_present = self.env.iter().all(|(key, zone)| {
            key != "TZ"
                || zone == "UTC"
                || zone == "Not/A_Zone"
                || Path::new("/usr/share/zoneinfo").join(zone).is_file()
        });
        zones_present.then_some(&self.expect)
    }
}

fn load() -> (Vec<Vec<String>>, Vec<Vector>) {
    let table: Value = serde_json::from_str(VECTORS).expect("rpn_vectors.json parses");
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    };
    let fixture = table["fixture"]
        .as_array()
        .unwrap()
        .iter()
        .map(strings)
        .collect();
    let vectors = table["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| Vector {
            id: case["id"].as_str().unwrap().to_owned(),
            args: strings(&case["args"]),
            env: case["env"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
                .collect(),
            tags: strings(&case["tags"]),
            expect: case["expect"].clone(),
            expect_platform: case.get("expect_platform").cloned().unwrap_or(Value::Null),
        })
        .collect();
    (fixture, vectors)
}

fn run(program: &Path, dir: &Path, args: &[String], env: &[(String, String)]) -> Value {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env_remove("RRDCACHED_ADDRESS")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    let status = output.status.code();
    let Some(code) = status else {
        return json!({"status": null, "crash": true});
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Map::new();
    result.insert("status".into(), json!(code));
    result.insert(
        "stderr".into(),
        json!(String::from_utf8_lossy(&output.stderr)),
    );
    if code != 0 {
        result.insert("stdout".into(), json!(stdout));
    } else if args[0] == "xport" {
        let tag = |name: &str| -> i64 {
            let open = format!("<{name}>");
            let close = format!("</{name}>");
            let rest = &stdout[stdout.find(&open).expect("xport meta") + open.len()..];
            rest[..rest.find(&close).unwrap()].parse().unwrap()
        };
        let meta: Vec<i64> = ["start", "end", "step", "rows"]
            .iter()
            .map(|n| tag(n))
            .collect();
        let rows: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("<row>"))
            .map(|line| {
                line.split("<v>")
                    .skip(1)
                    .map(|cell| cell.split("</v>").next().unwrap())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        result.insert("meta".into(), json!(meta));
        result.insert("rows".into(), json!(rows));
    } else {
        let prints: Vec<&str> = stdout.lines().filter(|l| l.starts_with("print[")).collect();
        result.insert("prints".into(), json!(prints));
    }
    Value::Object(result)
}

/// Numeric cells compare within a relative 1e-9 when `loose` is set, for
/// results whose last bit depends on fused multiply-add.
fn same(expected: &Value, got: &Value, loose: bool) -> bool {
    if expected == got {
        return true;
    }
    if !loose {
        return false;
    }
    match (expected, got) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| same(value, other, true)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y, true))
        }
        (Value::String(a), Value::String(b)) => {
            let numbers = |text: &str| -> Vec<String> {
                text.split([' ', '"', '='])
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect()
            };
            let (left, right) = (numbers(a), numbers(b));
            left.len() == right.len()
                && left.iter().zip(&right).all(|(x, y)| {
                    x == y
                        || match (x.parse::<f64>(), y.parse::<f64>()) {
                            (Ok(p), Ok(q)) => (p - q).abs() <= 1e-9 * p.abs().max(q.abs()),
                            _ => false,
                        }
                })
        }
        _ => false,
    }
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
        oracle_skip!("rpn_spec_vectors: pinned RRDtool 1.11.0 not found, live comparison skipped");
    }
    pinned
}

#[test]
fn rpn_vectors_match_rrdtool() {
    let (fixture, vectors) = load();
    assert!(vectors.len() >= 400, "only {} vectors", vectors.len());
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
    let loose = !cfg!(target_arch = "aarch64");

    let mut failures = Vec::new();
    let mut divergent = Vec::new();
    for vector in &vectors {
        let got = run(&rondi, &ro, &vector.args, &vector.env);
        if vector.tagged("undefined") {
            // RRDtool's result depends on memory contents or it crashes;
            // Rondi only has to finish without a signal.
            if got["status"].is_null() {
                failures.push(format!("{}: Rondi died on a signal", vector.id));
            }
            continue;
        }
        let mut ok = true;
        let mut detail = String::new();
        if let Some(frozen) = vector.frozen()
            && !same(frozen, &got, loose && vector.tagged("fma"))
        {
            ok = false;
            detail = format!("frozen {frozen}\n  rondi  {got}");
        }
        if live {
            let reference = run(Path::new("rrdtool"), &up, &vector.args, &vector.env);
            if !reference["status"].is_null() && reference != got {
                ok = false;
                detail = format!("live   {reference}\n  rondi  {got}");
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
                "{} {:?}\n  {detail}",
                vector.id,
                vector.args.iter().rev().take(2).collect::<Vec<_>>()
            )),
            (true, false) => {}
        }
    }
    if !divergent.is_empty() {
        eprintln!("known RPN divergences still present: {divergent:?}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} RPN vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
