//! Frozen vectors for `rrdtool update` and `updatev`: argument forms and
//! their checks, value syntax per data source type, rates, the heartbeat,
//! step apportioning, consolidation, row writes and `updatev` output. The
//! behavior is specified in docs/compatibility/update.md; the vectors under
//! tests/data/update were recorded from RRDtool 1.11.0 by
//! scripts/gen-update-vectors.py.
//!
//! A case copies one seed file to `t.rrd` in an empty directory and runs its
//! steps there. Each step's exit status, standard output and standard error
//! are compared, then the final bytes of `t.rrd`. Seeds were created once by
//! RRDtool and are shared by both sides because `create` picks a random
//! current row. Cases whose `expect` is `now` use the wall clock: their
//! current-time numbers are masked and their file is not compared. Unknowns
//! written as the x86_64 negative quiet NaN are folded into the positive one.
//!
//! With an RRDtool 1.11.0 oracle on PATH, or always under
//! RONDI_REQUIRE_ORACLE=1, every case also runs live: RRDtool must reproduce
//! the recording and Rondi must match RRDtool.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Cases where Rondi differs from RRDtool 1.11.0, with the UP rule and what
/// Rondi does instead. The architecture limits an entry to one Linux target
/// (`None` applies everywhere). A listed case that starts matching fails the
/// test so the entry is removed.
const KNOWN_DIVERGENCES: &[(&str, Option<&str>, &str)] = &[
    (
        "compute-basic",
        None,
        "UP-36: Rondi expects a reading for every data source, COMPUTE \
         included, so the update fails with a count error",
    ),
    (
        "compute-template",
        None,
        "UP-36: Rondi refuses templates and updates on files with COMPUTE \
         sources",
    ),
];

type Sections = Vec<(String, Vec<u8>)>;

struct Case {
    id: String,
    expect: String,
    seed: String,
    steps: Vec<Vec<String>>,
}

fn data() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/update")
}

fn cases() -> (Vec<Case>, BTreeMap<String, Vec<String>>) {
    let manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(data().join("cases.json")).unwrap()).unwrap();
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    };
    let cases = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| Case {
            id: case["id"].as_str().unwrap().to_owned(),
            expect: case["expect"].as_str().unwrap().to_owned(),
            seed: case["seed"].as_str().unwrap().to_owned(),
            steps: case["steps"]
                .as_array()
                .unwrap()
                .iter()
                .map(strings)
                .collect(),
        })
        .collect();
    let rules = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["id"].as_str().unwrap().to_owned(),
                strings(&case["rules"]),
            )
        })
        .collect();
    (cases, rules)
}

fn parse(bytes: &[u8]) -> BTreeMap<String, Sections> {
    let mut out: BTreeMap<String, Sections> = BTreeMap::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let newline = rest.iter().position(|b| *b == b'\n').unwrap();
        let header = std::str::from_utf8(&rest[..newline]).unwrap();
        let mut words = header.split(' ');
        assert_eq!(words.next(), Some("@@"), "bad section header {header:?}");
        let id = words.next().unwrap().to_owned();
        let name = words.next().unwrap().to_owned();
        let length: usize = words.next().unwrap().parse().unwrap();
        let body = rest[newline + 1..newline + 1 + length].to_vec();
        out.entry(id).or_default().push((name, body));
        rest = &rest[newline + 1 + length + 1..];
    }
    out
}

/// The macOS recording, with the cases that differ on this Linux
/// architecture replaced by `update.linux-<arch>.vectors`.
fn vectors() -> BTreeMap<String, Sections> {
    let expected = data().join("expected");
    let mut vectors = parse(&std::fs::read(expected.join("update.vectors")).unwrap());
    if cfg!(target_os = "linux") {
        let path = expected.join(format!("update.linux-{}.vectors", std::env::consts::ARCH));
        if let Ok(bytes) = std::fs::read(path) {
            vectors.extend(parse(&bytes));
        }
    }
    vectors
}

/// Replaces every ten-digit number from 1700000000 to 1999999999 with
/// `<now>`, as the generator does for cases that use `N`.
fn mask_now(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let boundary_before = index == 0 || !text[index - 1].is_ascii_alphanumeric();
        let end = index + 10;
        if boundary_before
            && end <= text.len()
            && text[index] == b'1'
            && (b'7'..=b'9').contains(&text[index + 1])
            && text[index..end].iter().all(u8::is_ascii_digit)
            && (end == text.len() || !text[end].is_ascii_alphanumeric())
        {
            out.extend_from_slice(b"<now>");
            index = end;
        } else {
            out.push(text[index]);
            index += 1;
        }
    }
    out
}

/// Folds the x86_64 negative quiet NaN into the positive one in file bodies.
fn folded(sections: &Sections) -> Sections {
    sections
        .iter()
        .map(|(name, body)| {
            let mut body = body.clone();
            if name == "file" {
                for chunk in body.as_chunks_mut::<8>().0 {
                    if *chunk == 0xfff8_0000_0000_0000_u64.to_le_bytes() {
                        chunk.copy_from_slice(&0x7ff8_0000_0000_0000_u64.to_le_bytes());
                    }
                }
            }
            (name.clone(), body)
        })
        .collect()
}

fn observe(program: &Path, work: &Path, case: &Case) -> Sections {
    let dir = work.join(&case.id);
    std::fs::create_dir_all(&dir).unwrap();
    let seed = std::fs::read(data().join("inputs").join(format!("{}.rrd", case.seed))).unwrap();
    std::fs::write(dir.join("t.rrd"), &seed).unwrap();
    std::fs::write(dir.join("cases.txt"), b"not an rrd file\n").unwrap();
    let now = case.expect == "now";
    let mut sections = Vec::new();
    for (index, step) in case.steps.iter().enumerate() {
        let output = Command::new(program)
            .args(step)
            .current_dir(&dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        let status = match output.status.code() {
            Some(code) => code.to_string(),
            None => "signal".to_owned(),
        };
        let (stdout, stderr) = if now {
            (mask_now(&output.stdout), mask_now(&output.stderr))
        } else {
            (output.stdout, output.stderr)
        };
        sections.push((format!("{index}.status"), status.into_bytes()));
        sections.push((format!("{index}.stdout"), without_build_time(&stdout)));
        sections.push((format!("{index}.stderr"), stderr));
    }
    if !now {
        let last = std::fs::read(dir.join("t.rrd")).unwrap();
        if last == seed {
            sections.push(("same".to_owned(), Vec::new()));
        } else {
            sections.push(("file".to_owned(), last));
        }
    }
    sections
}

/// Drops the build timestamp from the usage banner's `Compiled` line.
fn without_build_time(body: &[u8]) -> Vec<u8> {
    const MARK: &[u8] = b"\n               Compiled ";
    let Some(start) = body.windows(MARK.len()).position(|window| window == MARK) else {
        return body.to_vec();
    };
    let from = start + MARK.len();
    let end = body[from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(body.len(), |offset| from + offset);
    [&body[..from], &body[end..]].concat()
}

/// Where two section lists first differ, or None when they are equal.
fn difference(got: &Sections, want: &Sections) -> Option<String> {
    let (got, want) = (folded(got), folded(want));
    let want: Vec<(String, Vec<u8>)> = want
        .into_iter()
        .map(|(name, body)| {
            let body = if name.ends_with(".stdout") {
                without_build_time(&body)
            } else {
                body
            };
            (name, body)
        })
        .collect();
    if got == want {
        return None;
    }
    let names = |sections: &Sections| -> Vec<String> {
        sections.iter().map(|(name, _)| name.clone()).collect()
    };
    if names(&got) != names(&want) {
        return Some(format!(
            "entries {:?}, expected {:?}",
            names(&got),
            names(&want)
        ));
    }
    for ((name, a), (_, b)) in got.iter().zip(&want) {
        if a != b {
            return Some(match a.iter().zip(b).position(|(x, y)| x != y) {
                _ if name != "file" => format!(
                    "{name}: {:?}, expected {:?}",
                    String::from_utf8_lossy(a),
                    String::from_utf8_lossy(b)
                ),
                Some(offset) => format!("file: first difference at byte {offset}"),
                None => format!("file: lengths {} and {}", a.len(), b.len()),
            });
        }
    }
    None
}

fn known(id: &str) -> bool {
    KNOWN_DIVERGENCES.iter().any(|(known, arch, _)| {
        *known == id
            && arch.is_none_or(|arch| cfg!(target_os = "linux") && arch == std::env::consts::ARCH)
    })
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

/// Checks Rondi's result for one case against `want`, honoring the
/// known-divergence list.
fn judge(case: &Case, got: &Sections, want: &Sections, failures: &mut Vec<String>) {
    match (difference(got, want), known(&case.id)) {
        (Some(detail), false) => failures.push(format!("{} {:?}: {detail}", case.id, case.steps)),
        (None, true) => failures.push(format!(
            "{}: now matches RRDtool; remove it from KNOWN_DIVERGENCES and the \
             specification's divergence list",
            case.id
        )),
        _ => {}
    }
}

#[test]
fn update_vectors_cover_the_specification() {
    let (cases, rules) = cases();
    assert!(cases.len() >= 80, "only {} cases", cases.len());
    let vectors = vectors();
    let mut ids = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate id {}", case.id);
        assert!(vectors.contains_key(&case.id), "no vector for {}", case.id);
        assert!(
            matches!(case.expect.as_str(), "same" | "now"),
            "{}: unknown expectation {}",
            case.id,
            case.expect
        );
    }
    for (id, _, reason) in KNOWN_DIVERGENCES {
        assert!(ids.contains(&id.to_string()), "unknown divergence id {id}");
        assert!(reason.starts_with("UP-"), "{id}: reason names no rule");
    }
    let spec = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/compatibility/update.md"),
    )
    .unwrap();
    let mut pinned = std::collections::BTreeSet::new();
    for list in rules.values() {
        pinned.extend(list.iter().cloned());
    }
    for line in spec.lines() {
        let Some(rest) = line.strip_prefix("**UP-") else {
            continue;
        };
        let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let rule = format!("UP-{number}");
        assert!(pinned.contains(&rule), "no vector pins {rule}");
    }
}

#[test]
fn rondi_matches_frozen_update_vectors() {
    let bench = Bench::new();
    let vectors = vectors();
    let mut failures = Vec::new();
    for case in cases().0 {
        let got = observe(&bench.rondi, &bench.work, &case);
        judge(&case, &got, &vectors[&case.id], &mut failures);
    }
    report(&failures);
}

#[test]
fn rrdtool_reproduces_update_vectors_and_rondi_matches_it() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        common::skipped("skipping live update vectors: rrdtool is not installed".into());
        return;
    }
    common::require_oracle();
    let bench = Bench::new();
    let oracle = PathBuf::from("rrdtool");
    let (upstream, rondi) = (bench.work.join("upstream"), bench.work.join("rondi"));
    let vectors = vectors();
    let mut failures = Vec::new();
    for case in cases().0 {
        let reference = observe(&oracle, &upstream, &case);
        if let Some(detail) = difference(&reference, &vectors[&case.id]) {
            failures.push(format!("[rrdtool] {} {:?}: {detail}", case.id, case.steps));
        }
        let got = observe(&bench.rondi, &rondi, &case);
        judge(&case, &got, &reference, &mut failures);
    }
    report(&failures);
}
