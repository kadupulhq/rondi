//! Replay of the frozen `create` and `resize` vectors recorded by
//! scripts/gen-create-resize-vectors.py. Shared by create_spec_vectors.rs
//! and resize_spec_vectors.rs; each suite lives under tests/data/<suite>.
//!
//! A case copies its inputs into an empty directory, runs one command there
//! and records the exit status, standard output, standard error and every
//! entry the directory holds afterwards: `same:<path>` for an input left
//! untouched, `file:<path>` with the bytes of a new or changed file,
//! `link:<path>` with a symbolic link's target and `dir:<path>`. Created
//! files are compared in masked form (each archive rotated oldest row
//! first, current-row words zeroed) because `create` picks a random current
//! row. Unknowns written as the x86_64 negative quiet NaN are folded into
//! the positive one before comparing.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What Rondi must do for a case, relative to the recorded RRDtool result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Expect {
    /// Reproduce status, output and every resulting file.
    Same,
    /// A feature Rondi refuses: exit 1 with an `ERROR:` line, nothing
    /// created or changed.
    Unsupported,
    /// A documented divergence. Rondi must still differ; a case that starts
    /// matching fails so that its expectation is promoted to `same`.
    Divergent,
    /// RRDtool crashes or hangs. Rondi fails cleanly and changes nothing.
    CleanError,
    /// RRDtool's result varies between runs. Rondi exits 0 or 1.
    Undefined,
}

pub struct Case {
    pub id: String,
    pub expect: Expect,
    pub note: String,
    pub inputs: Vec<String>,
    pub args: Vec<String>,
}

/// Ordered (section name, body) pairs, as framed in the vector files.
pub type Sections = Vec<(String, Vec<u8>)>;

pub struct Suite {
    pub name: &'static str,
    dir: PathBuf,
    /// Whether resulting RRD files are masked before comparison.
    mask: bool,
}

impl Suite {
    pub fn new(name: &'static str, mask: bool) -> Self {
        Self {
            name,
            dir: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/data")
                .join(name),
            mask,
        }
    }

    pub fn cases(&self) -> Vec<Case> {
        let manifest = std::fs::read_to_string(self.dir.join("cases.tsv")).unwrap();
        manifest
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let id = fields.next().unwrap().to_owned();
                let expect = match fields.next().unwrap() {
                    "same" => Expect::Same,
                    "unsupported" => Expect::Unsupported,
                    "divergent" => Expect::Divergent,
                    "clean-error" => Expect::CleanError,
                    "undefined" => Expect::Undefined,
                    other => panic!("{id}: unknown expectation {other}"),
                };
                let note = fields.next().unwrap().to_owned();
                let inputs = match fields.next().unwrap() {
                    "-" => Vec::new(),
                    list => list.split(',').map(str::to_owned).collect(),
                };
                Case {
                    id,
                    expect,
                    note,
                    inputs,
                    args: fields.map(str::to_owned).collect(),
                }
            })
            .collect()
    }

    /// Recorded results by case id: the macOS recording, with the cases
    /// that differ on this Linux architecture replaced by
    /// `<suite>.linux-<arch>.vectors`.
    pub fn vectors(&self) -> BTreeMap<String, Sections> {
        let expected = self.dir.join("expected");
        let mut vectors =
            parse(&std::fs::read(expected.join(format!("{}.vectors", self.name))).unwrap());
        if cfg!(target_os = "linux") {
            let path = expected.join(format!(
                "{}.linux-{}.vectors",
                self.name,
                std::env::consts::ARCH
            ));
            if let Ok(bytes) = std::fs::read(path) {
                vectors.extend(parse(&bytes));
            }
        }
        vectors
    }

    fn input(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.join("inputs").join(format!("{name}.rrd"))).unwrap()
    }

    /// Runs one case with `program` in a fresh directory under `work`.
    pub fn observe(&self, program: &Path, work: &Path, case: &Case) -> Sections {
        let dir = work.join(&case.id);
        std::fs::create_dir_all(&dir).unwrap();
        for item in &case.inputs {
            if let Some(spec) = item.strip_prefix("@link:") {
                let (name, target) = spec.split_once('>').unwrap();
                std::os::unix::fs::symlink(target, dir.join(name)).unwrap();
            } else if let Some(name) = item.strip_prefix("@dir:") {
                std::fs::create_dir_all(dir.join(name)).unwrap();
            } else if let Some(name) = item.strip_prefix("@text:") {
                std::fs::write(dir.join(name), b"hello\n").unwrap();
            } else if let Some((name, source)) = item.split_once('=') {
                std::fs::write(dir.join(name), self.input(source)).unwrap();
            } else {
                std::fs::write(dir.join(format!("{item}.rrd")), self.input(item)).unwrap();
            }
        }
        let before = self.snapshot(&dir);
        let output = Command::new(program)
            .args(&case.args)
            .current_dir(&dir)
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env_remove("RRDCACHED_ADDRESS")
            .output()
            .unwrap();
        let after = self.snapshot(&dir);
        let status = match output.status.code() {
            Some(code) => code.to_string(),
            None => "signal".to_owned(),
        };
        let stdout = if output.stdout.starts_with(b"RRD\0") && self.mask {
            masked(&output.stdout)
        } else {
            output.stdout
        };
        let mut sections = vec![
            ("status".to_owned(), status.into_bytes()),
            ("stdout".to_owned(), stdout),
            ("stderr".to_owned(), output.stderr),
        ];
        for (path, entry) in after {
            if entry.0 == "file" && before.get(&path) == Some(&entry) {
                sections.push((format!("same:{path}"), Vec::new()));
            } else {
                sections.push((format!("{}:{path}", entry.0), entry.1));
            }
        }
        sections
    }

    /// Every entry under `dir` by relative path, sorted, as (kind, body).
    fn snapshot(&self, dir: &Path) -> BTreeMap<String, (&'static str, Vec<u8>)> {
        let mut out = BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).unwrap() {
                let path = entry.unwrap().path();
                let relative = path
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                let entry = if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(&path).unwrap();
                    ("link", target.to_string_lossy().into_owned().into_bytes())
                } else if meta.is_dir() {
                    pending.push(path.clone());
                    ("dir", Vec::new())
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    ("file", if self.mask { masked(&bytes) } else { bytes })
                };
                out.insert(relative, entry);
            }
        }
        out
    }
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

fn word(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// A created file in comparison form: each archive rotated so its oldest
/// row comes first and the current-row words zeroed. Anything that is not
/// a complete 64-bit RRD is returned unchanged.
pub fn masked(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() < 128 || !bytes.starts_with(b"RRD\0") {
        return bytes.to_vec();
    }
    let (ds, rra) = (word(bytes, 24) as usize, word(bytes, 32) as usize);
    if ds > 4096 || rra > 4096 {
        return bytes.to_vec();
    }
    let pointers = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra;
    if pointers + 8 * rra > bytes.len() {
        return bytes.to_vec();
    }
    let mut out = bytes[..pointers].to_vec();
    out.extend(std::iter::repeat_n(0, 8 * rra));
    let mut data = pointers + 8 * rra;
    for index in 0..rra {
        let rows = word(bytes, 128 + 120 * ds + 120 * index + 24);
        let length = rows
            .checked_mul(ds as u64 * 8)
            .and_then(|length| usize::try_from(length).ok());
        let Some(length) = length.filter(|length| rows > 0 && data + length <= bytes.len()) else {
            return bytes.to_vec();
        };
        let archive = &bytes[data..data + length];
        let split = ((word(bytes, pointers + 8 * index) + 1) % rows) as usize * ds * 8;
        out.extend_from_slice(&archive[split..]);
        out.extend_from_slice(&archive[..split]);
        data += length;
    }
    out.extend_from_slice(&bytes[data..]);
    out
}

/// Drops the build timestamp from the usage banner's `Compiled` line, which
/// says when each RRDtool binary was built.
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

/// Folds the x86_64 negative quiet NaN into the positive one in file
/// bodies and standard output, and drops the usage banner's build time.
pub fn folded(sections: &Sections) -> Sections {
    sections
        .iter()
        .map(|(name, body)| {
            if name == "stdout" && body.starts_with(b"RRDtool ") {
                (name.clone(), without_build_time(body))
            } else if name.starts_with("file:") || name == "stdout" {
                let mut body = body.clone();
                for chunk in body.as_chunks_mut::<8>().0 {
                    if *chunk == 0xfff8_0000_0000_0000_u64.to_le_bytes() {
                        chunk.copy_from_slice(&0x7ff8_0000_0000_0000_u64.to_le_bytes());
                    }
                }
                (name.clone(), body)
            } else {
                (name.clone(), body.clone())
            }
        })
        .collect()
}

fn section<'a>(sections: &'a Sections, name: &str) -> &'a [u8] {
    sections
        .iter()
        .find(|(section, _)| section == name)
        .map_or(&[], |(_, body)| body.as_slice())
}

/// A short description of where two section lists first differ.
pub fn difference(got: &Sections, want: &Sections) -> String {
    let names = |sections: &Sections| -> Vec<String> {
        sections.iter().map(|(name, _)| name.clone()).collect()
    };
    if names(got) != names(want) {
        return format!("entries {:?}, expected {:?}", names(got), names(want));
    }
    for ((name, a), (_, b)) in got.iter().zip(want) {
        if a != b {
            return match a.iter().zip(b).position(|(x, y)| x != y) {
                _ if !name.starts_with("file:") => format!(
                    "{name}: {:?}, expected {:?}",
                    String::from_utf8_lossy(a),
                    String::from_utf8_lossy(b)
                ),
                Some(offset) => format!("{name}: first difference at byte {offset}"),
                None => format!("{name}: lengths {} and {}", a.len(), b.len()),
            };
        }
    }
    "no difference".to_owned()
}

/// Checks `got` against the recorded vector according to `expect`.
pub fn check(case: &Case, expect: Expect, got: &Sections, want: &Sections) -> Option<String> {
    let (got, want) = (folded(got), folded(want));
    let fail = |what: String| Some(format!("{} {:?}: {what}", case.id, case.args));
    match expect {
        Expect::Same => (got != want)
            .then(|| difference(&got, &want))
            .and_then(fail),
        Expect::Divergent => (got == want).then(|| {
            format!(
                "{} {:?}: now matches RRDtool; change its expectation to `same` \
                 and drop divergence {} from the specification",
                case.id, case.args, case.note
            )
        }),
        Expect::Undefined => {
            let status = section(&got, "status");
            (status != b"0" && status != b"1")
                .then(|| format!("exit status {:?}", String::from_utf8_lossy(status)))
                .and_then(fail)
        }
        Expect::Unsupported | Expect::CleanError => {
            let clean =
                section(&got, "status") == b"1" && section(&got, "stderr").starts_with(b"ERROR: ");
            let changed: Vec<&String> = got
                .iter()
                .map(|(name, _)| name)
                .filter(|name| name.starts_with("file:"))
                .collect();
            if !clean {
                return fail(format!(
                    "expected a clean ERROR exit, got status {:?} stderr {:?}",
                    String::from_utf8_lossy(section(&got, "status")),
                    String::from_utf8_lossy(section(&got, "stderr"))
                ));
            }
            (!changed.is_empty())
                .then(|| format!("wrote {changed:?} while refusing"))
                .and_then(fail)
        }
    }
}

/// A scratch directory and the Rondi binary under the `rrdtool` name.
pub struct Bench {
    _temp: tempfile::TempDir,
    pub work: PathBuf,
    pub rondi: PathBuf,
}

impl Bench {
    pub fn new() -> Self {
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

pub fn report(failures: &[String]) {
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

/// Rondi against every frozen vector of `suite`.
pub fn replay_rondi(suite: &Suite) {
    let bench = Bench::new();
    let vectors = suite.vectors();
    let mut failures = Vec::new();
    for case in suite.cases() {
        let got = suite.observe(&bench.rondi, &bench.work, &case);
        failures.extend(check(&case, case.expect, &got, &vectors[&case.id]));
    }
    report(&failures);
}

/// The pinned RRDtool against every vector it recorded deterministically,
/// and Rondi directly against RRDtool for the `same` cases.
pub fn replay_oracle(suite: &Suite) {
    let bench = Bench::new();
    let oracle = PathBuf::from("rrdtool");
    let (upstream, rondi) = (bench.work.join("upstream"), bench.work.join("rondi"));
    let vectors = suite.vectors();
    let mut failures = Vec::new();
    for case in suite.cases() {
        if case.expect == Expect::Undefined {
            continue;
        }
        let reference = suite.observe(&oracle, &upstream, &case);
        failures.extend(
            check(&case, Expect::Same, &reference, &vectors[&case.id])
                .map(|failure| format!("[rrdtool] {failure}")),
        );
        if case.expect == Expect::Same {
            let got = suite.observe(&bench.rondi, &rondi, &case);
            failures.extend(
                check(&case, Expect::Same, &got, &reference)
                    .map(|failure| format!("[rondi vs rrdtool] {failure}")),
            );
        }
    }
    report(&failures);
}
