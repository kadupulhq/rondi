//! Replays the graph and xport vectors in `data/graph_spec_vectors.json`.
//!
//! The vectors were captured from RRDtool 1.11.0 by
//! `scripts/graph-spec-vectors.py` and are described in
//! `docs/compatibility/graph.md`. Every vector is replayed against the Rondi
//! binary under the `rrdtool` name. When the pinned oracle is installed,
//! each vector is also run through it and Rondi must match the live result
//! as well; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure.
//!
//! Image bytes are never compared. Both sides are normalized as the capture
//! script does: a PNG image becomes `<PNG WxH>` from its IHDR chunk, an SVG,
//! PDF or EPS image becomes `<SVG>`, `<PDF>` or `<EPS>`, the size after
//! `BLOB_SIZE:` becomes `*`, the work directory becomes `{CWD}` and the
//! build stamp after `Compiled ` is dropped. Output bytes are read as
//! Latin-1 so that every byte maps to one character.
#![cfg(unix)]

#[macro_use]
mod common;

use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

const VECTORS: &str = include_str!("data/graph_spec_vectors.json");
/// Modification time of the fixture's text files, in seconds since the
/// epoch, as set by the capture script, so that `--lazy` sees them as older
/// than the data.
const FILE_TIME_SECS: u64 = 1_000_000_000;
const STEP_TIMEOUT: Duration = Duration::from_secs(120);
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Platform list of a divergence seen on every platform.
const ALL: &[&str] = &[];
const MACOS: &[&str] = &["macos"];
const LINUX: &[&str] = &["linux-x86_64", "linux-aarch64"];
/// Where the reference reports a libpng read error for `--lazy /dev/null`;
/// the pinned x86_64 build prints nothing.
const LIBPNG_READ: &[&str] = &["macos", "linux-aarch64"];

/// Why Rondi differs from RRDtool on a vector.
#[derive(Clone, Copy)]
enum Class {
    /// An intentional difference a reimplementation keeps (graph.md, Rondi
    /// policies). Rondi's standard error must contain this text.
    Policy(&'static str),
    /// A gap to close; the vector fails once it matches, so the entry is
    /// removed.
    Defect,
}

struct Divergence {
    id: &'static str,
    /// Platforms where the difference shows; empty means all.
    platforms: &'static [&'static str],
    class: Class,
    reason: &'static str,
}

const fn div(
    id: &'static str,
    platforms: &'static [&'static str],
    class: Class,
    reason: &'static str,
) -> Divergence {
    Divergence {
        id,
        platforms,
        class,
        reason,
    }
}

const RD006: Class = Class::Policy("RRDtool graph format ");
const RD016: Class = Class::Policy("PNG graph exceeds the 16777216 pixel safety limit");
const RD017: Class = Class::Policy("RRD fetch range exceeds the 10 million row safety limit");
const DEFECT: Class = Class::Defect;

/// Vectors where Rondi differs from RRDtool 1.11.0. The frozen expectation
/// stays RRDtool's; listed ids are reported instead of failing. The same
/// list, grouped, is in graph.md under "Known divergences".
const KNOWN_DIVERGENCES: &[Divergence] = &[
    div("graph-imgformat-004", ALL, RD006, "SVG is refused"),
    div("graph-imgformat-005", ALL, RD006, "EPS is refused"),
    div("graph-imgformat-006", ALL, RD006, "PDF is refused"),
    div("graph-out-013", ALL, RD006, "SVG is refused"),
    div("graph-out-016", ALL, RD006, "SVG is refused"),
    div("graph-out-017", ALL, RD006, "PDF is refused"),
    div("graph-out-018", ALL, RD006, "EPS is refused"),
    div("graph-out-019", ALL, RD006, "SVG is refused"),
    div("graph-limit-004", ALL, RD016, "graph width above 32767"),
    div("graph-limit-006", ALL, RD016, "image width above 32767"),
    div("graph-limit-009", ALL, RD016, "graph width above 32767"),
    div(
        "graph-limit-012",
        ALL,
        RD016,
        "zoomed image above 2^24 pixels",
    ),
    div("graph-limit-015", ALL, RD016, "image above 2^24 pixels"),
    div("graph-limit-016", ALL, RD016, "image above 2^24 pixels"),
    div("graph-limit-019", ALL, RD016, "image above 2^24 pixels"),
    div("graph-limit-020", ALL, RD017, "fetch above 10 million rows"),
    div("graph-limit-021", ALL, RD017, "fetch above 10 million rows"),
    div("graph-limit-022", ALL, RD017, "fetch above 10 million rows"),
    div("graph-limit-023", ALL, RD017, "fetch above 10 million rows"),
    div("graph-limit-024", ALL, RD017, "fetch above 10 million rows"),
    div("graph-limit-025", ALL, RD017, "fetch above 10 million rows"),
    div(
        "graph-grammar-055",
        ALL,
        DEFECT,
        "GR-3: prints 0x0 without elements",
    ),
    div(
        "graph-grammar-056",
        ALL,
        DEFECT,
        "GR-3: prints 0x0 without elements",
    ),
    div(
        "graph-grammar-061",
        ALL,
        DEFECT,
        "GR-3: prints 0x0 without elements",
    ),
    div(
        "graph-out-008",
        ALL,
        DEFECT,
        "GR-3: prints 0x0 without elements",
    ),
    div(
        "graph-opt-145",
        LIBPNG_READ,
        DEFECT,
        "GR-84: no libpng read error for --lazy /dev/null",
    ),
    div(
        "graph-opt-146",
        LIBPNG_READ,
        DEFECT,
        "GR-84: no libpng read error for --lazy /dev/null",
    ),
    div(
        "graph-out-027",
        ALL,
        DEFECT,
        "GR-84: --lazy replaces the file",
    ),
    div(
        "graph-out-031",
        ALL,
        DEFECT,
        "GR-84: --lazy replaces the file",
    ),
    div(
        "graph-out-029",
        ALL,
        DEFECT,
        "GR-84: graphv --lazy prints every key",
    ),
    div(
        "graph-vertical-label-002",
        ALL,
        DEFECT,
        "GR-61: long vertical label panics",
    ),
    div(
        "graph-formatter-007",
        ALL,
        DEFECT,
        "GR-25: duration format not checked",
    ),
    div(
        "graph-formatter-008",
        ALL,
        DEFECT,
        "GR-25: duration format not checked",
    ),
    div("graph-def-073", ALL, DEFECT, "GR-43: -Z with a missing DS"),
    div(
        "graph-cdef-019",
        ALL,
        DEFECT,
        "GR-45: self-referencing CDEF",
    ),
    div(
        "graph-grid-dash-011",
        ALL,
        DEFECT,
        "GR-24: no Cairo dash warning",
    ),
    div(
        "graph-line-044",
        ALL,
        DEFECT,
        "GR-50: no Cairo dash warning",
    ),
    div(
        "graph-line-045",
        ALL,
        DEFECT,
        "GR-50: no Cairo dash warning",
    ),
    div(
        "graph-hrule-027",
        ALL,
        DEFECT,
        "GR-54: no Cairo dash warning",
    ),
    div(
        "graph-hrule-028",
        ALL,
        DEFECT,
        "GR-54: no Cairo dash warning",
    ),
    div(
        "graph-vrule-027",
        ALL,
        DEFECT,
        "GR-54: no Cairo dash warning",
    ),
    div(
        "graph-vrule-028",
        ALL,
        DEFECT,
        "GR-54: no Cairo dash warning",
    ),
    div(
        "graph-font-003",
        MACOS,
        DEFECT,
        "GR-60: font family metrics",
    ),
    div(
        "graph-layout-049",
        MACOS,
        DEFECT,
        "GR-60: font family metrics",
    ),
    div(
        "graph-font-019",
        LINUX,
        DEFECT,
        "GR-60: font family metrics",
    ),
    div(
        "graph-layout-026",
        LINUX,
        DEFECT,
        "GR-81: legend coords under zoom",
    ),
    div(
        "graph-layout-027",
        LINUX,
        DEFECT,
        "GR-81: legend coords under zoom",
    ),
    div(
        "graph-layout-028",
        LINUX,
        DEFECT,
        "GR-81: legend coords under zoom",
    ),
    div(
        "graph-legend-030",
        ALL,
        DEFECT,
        "GR-60: fallback font metrics",
    ),
    div(
        "graph-legend-079",
        ALL,
        DEFECT,
        "GR-64: markup measured as text",
    ),
    div(
        "graph-data-005",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-012",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-019",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-026",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-033",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-040",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-047",
        ALL,
        DEFECT,
        "GR-86: --x-grid time format ignored",
    ),
    div(
        "graph-data-062",
        ALL,
        DEFECT,
        "GR-87: --add-jsontime ignored",
    ),
    div(
        "graph-data-063",
        ALL,
        DEFECT,
        "GR-87: --add-jsontime ignored",
    ),
];

struct Vector {
    id: String,
    env: Vec<(String, String)>,
    steps: Vec<Step>,
    tags: Vec<String>,
    expect: Value,
    expect_platform: Value,
}

struct Step {
    args: Vec<String>,
    inspect: Vec<String>,
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

/// A named zone needs the host's zoneinfo file, which slim images may lack.
fn zone_available(zone: &str) -> bool {
    !zone.contains('/') || Path::new("/usr/share/zoneinfo").join(zone).is_file()
}

impl Vector {
    fn tagged(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    /// The frozen expectation for this build target, if one applies.
    fn frozen(&self) -> Option<&Value> {
        if self.tagged("unstable") {
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

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn load() -> (Value, Vec<Vector>) {
    let table: Value = serde_json::from_str(VECTORS).expect("graph_spec_vectors.json parses");
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
                    args: strings(&step["args"]),
                    inspect: strings(&step["inspect"]),
                })
                .collect(),
            tags: strings(&case["tags"]),
            expect: case.get("expect").cloned().unwrap_or(Value::Null),
            expect_platform: case.get("expect_platform").cloned().unwrap_or(Value::Null),
        })
        .collect();
    (table, vectors)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| index + from)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn starts_svg(data: &[u8]) -> bool {
    data.starts_with(b"<?xml") && find(&data[..data.len().min(400)], b"<svg", 0).is_some()
}

/// The placeholder for an image at the start of `data` and its length.
fn image_tag(data: &[u8]) -> Option<(String, usize)> {
    if data.starts_with(PNG_MAGIC) && data.len() >= 24 {
        let width = u32::from_be_bytes(data[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(data[20..24].try_into().unwrap());
        let length = find(data, b"IEND", 0).map_or(data.len(), |end| end + 8);
        return Some((format!("<PNG {width}x{height}>"), length.min(data.len())));
    }
    let tail = |marker: &[u8], extra: usize| {
        rfind(data, marker).map_or(data.len(), |end| (end + extra).min(data.len()))
    };
    if data.starts_with(b"%PDF") {
        return Some(("<PDF>".into(), tail(b"%%EOF", 6)));
    }
    if data.starts_with(b"%!PS") {
        return Some(("<EPS>".into(), tail(b"%%EOF", 6)));
    }
    if starts_svg(data) {
        let length = find(data, b"</svg>", 0).map_or(data.len(), |end| (end + 7).min(data.len()));
        return Some(("<SVG>".into(), length));
    }
    None
}

/// Replaces image bytes with placeholders, then normalizes the text.
fn normalize_bytes(data: &[u8], work: &Path) -> String {
    let mut out: Vec<u8> = Vec::new();
    let mut position = 0;
    while position < data.len() {
        let mut found: Option<usize> = None;
        for magic in [PNG_MAGIC, b"%PDF", b"%!PS", b"<?xml"] {
            if let Some(index) = find(data, magic, position) {
                if magic == b"<?xml" && !starts_svg(&data[index..]) {
                    continue;
                }
                if found.is_none_or(|current| index < current) {
                    found = Some(index);
                }
            }
        }
        let Some(start) = found else {
            out.extend_from_slice(&data[position..]);
            break;
        };
        out.extend_from_slice(&data[position..start]);
        match image_tag(&data[start..]) {
            Some((tag, length)) => {
                out.extend_from_slice(tag.as_bytes());
                position = start + length;
            }
            None => {
                out.push(data[start]);
                position = start + 1;
            }
        }
    }
    let text: String = out.iter().map(|&b| char::from(b)).collect();
    normalize_text(&text, work)
}

fn normalize_text(text: &str, work: &Path) -> String {
    let given = work.to_string_lossy().into_owned();
    let real = std::fs::canonicalize(work)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| given.clone());
    let mut paths = vec![given, real];
    paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
    paths.dedup();
    text.split('\n')
        .map(|raw| {
            let mut line = mask_mmap_error(&blob_size(raw));
            if let Some(position) = line.find("Compiled ") {
                line.truncate(position + "Compiled ".len());
            }
            for path in &paths {
                line = line.replace(path.as_str(), "{CWD}");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mapping a directory fails with EINVAL or ENODEV depending on the file
/// system, so the text after `mmaping file '<path>': ` becomes `{ERRNO}`.
fn mask_mmap_error(line: &str) -> String {
    const MARK: &str = "mmaping file '";
    let Some(at) = line.find(MARK) else {
        return line.to_owned();
    };
    let Some(end) = line[at..].find("': ") else {
        return line.to_owned();
    };
    format!("{}{{ERRNO}}", &line[..at + end + 3])
}

/// `BLOB_SIZE:<digits>` becomes `BLOB_SIZE:*`.
fn blob_size(line: &str) -> String {
    const KEY: &str = "BLOB_SIZE:";
    let mut out = String::new();
    let mut rest = line;
    while let Some(index) = rest.find(KEY) {
        let after = &rest[index + KEY.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..index + KEY.len()]);
        if digits == 0 {
            rest = after;
        } else {
            out.push('*');
            rest = &after[digits..];
        }
    }
    out.push_str(rest);
    out
}

fn inspect(path: &Path, work: &Path) -> String {
    if path.is_dir() {
        return "<dir>".into();
    }
    match std::fs::read(path) {
        Ok(bytes) => normalize_bytes(&bytes, work),
        Err(_) if !path.exists() => "<missing>".into(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            // --lazy compares modification times, so the copy keeps them,
            // as the capture script's copy does.
            std::fs::copy(entry.path(), &target).unwrap();
            let modified = entry.metadata().unwrap().modified().unwrap();
            set_modified(&target, modified);
        }
    }
}

fn set_modified(path: &Path, time: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

struct Ran {
    status: Option<i32>,
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs one process with a deadline so that a hang fails the vector
/// instead of the whole test binary.
fn run_bounded(mut command: Command) -> Ran {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
    Ran {
        status: status.code(),
        timed_out,
        stdout: out_reader.join().unwrap(),
        stderr: err_reader.join().unwrap(),
    }
}

fn run_vector(program: &Path, fixture: &Path, vector: &Vector) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("w");
    copy_tree(fixture, &work);
    let mut results = Vec::new();
    for step in &vector.steps {
        let mut command = Command::new(program);
        command.args(&step.args).current_dir(&work).env_clear();
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if matches!(
                name.as_ref(),
                "RRDCACHED_ADDRESS" | "TZ" | "LANG" | "RRD_LOCKING" | "RRD_DEFAULT_FONT"
            ) || name.starts_with("LC_")
            {
                continue;
            }
            command.env(key, value);
        }
        command.env("TZ", "UTC").env("LC_ALL", "C");
        for (key, value) in &vector.env {
            command.env(key, value);
        }
        let ran = run_bounded(command);
        if ran.timed_out {
            results.push(json!({"status": null, "timeout": true}));
            break;
        }
        let Some(code) = ran.status else {
            results.push(json!({"status": null, "crash": true}));
            break;
        };
        let mut result = Map::new();
        result.insert("status".into(), json!(code));
        result.insert("stdout".into(), json!(normalize_bytes(&ran.stdout, &work)));
        result.insert("stderr".into(), json!(normalize_bytes(&ran.stderr, &work)));
        if !step.inspect.is_empty() {
            let files: Map<String, Value> = step
                .inspect
                .iter()
                .map(|name| (name.clone(), json!(inspect(&work.join(name), &work))))
                .collect();
            result.insert("files".into(), Value::Object(files));
        }
        results.push(Value::Object(result));
    }
    Value::Array(results)
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
fn setup(program: &Path, dir: &Path, table: &Value) {
    for name in strings(&table["dirs"]) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
    for (name, text) in table["files"].as_object().unwrap() {
        std::fs::write(dir.join(name), text.as_str().unwrap()).unwrap();
        set_modified(
            &dir.join(name),
            SystemTime::UNIX_EPOCH + Duration::from_secs(FILE_TIME_SECS),
        );
    }
    for command in table["fixture"].as_array().unwrap() {
        let output = Command::new(program)
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

/// The pinned oracle on `PATH`.
fn live_oracle() -> Option<PathBuf> {
    let pinned = Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).starts_with("RRDtool 1.11.0 "));
    if !pinned {
        oracle_skip!(
            "graph_spec_vectors: pinned RRDtool 1.11.0 not found, live comparison skipped"
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("rrdtool"))
        .find(|candidate| candidate.is_file())
}

#[test]
fn normalization_helpers() {
    let mut png = PNG_MAGIC.to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&481_u32.to_be_bytes());
    png.extend_from_slice(&141_u32.to_be_bytes());
    png.extend_from_slice(b"\x08\x06\x00\x00\x00crc!IEND\xaeB`\x82");
    let mut data = b"image = BLOB_SIZE:1234\n".to_vec();
    data.extend_from_slice(&png);
    data.extend_from_slice(b"\ntail");
    assert_eq!(
        normalize_bytes(&data, Path::new("/nonexistent-dir")),
        "image = BLOB_SIZE:*\n<PNG 481x141>\ntail"
    );
    assert_eq!(
        normalize_bytes(b"<?xml version=\"1.0\"?>\n<xport>", Path::new("/x")),
        "<?xml version=\"1.0\"?>\n<xport>"
    );
    assert_eq!(
        normalize_bytes(b"%PDF-1.5 body %%EOF\n", Path::new("/x")),
        "<PDF>"
    );
    assert_eq!(
        blob_size("BLOB_SIZE:x BLOB_SIZE:12"),
        "BLOB_SIZE:x BLOB_SIZE:*"
    );
}

/// The result of checking one vector. A vector whose expectation does not
/// apply here and that has no live oracle is not compared, so it neither
/// passes nor fails, and a known divergence is not reported as fixed.
enum Outcome {
    Matches(String),
    /// The id, a description of the difference and Rondi's result.
    Differs(String, String, Value),
    NotCompared,
}

fn check(vector: &Vector, rondi: &Path, ro: &Path, up: &Path, oracle: Option<&Path>) -> Outcome {
    let id = vector.id.clone();
    let got = run_vector(rondi, ro, vector);
    if failed_to_finish(&got) {
        // Whatever the reference does, Rondi must exit on its own.
        return Outcome::Differs(id, format!("rondi  {got}"), got);
    }
    // The reference crashes, hangs, prints a process id, or formats the
    // wall clock for these; Rondi only has to finish.
    if vector.tagged("undefined") || vector.tagged("unstable") || vector.tagged("clock") {
        return Outcome::Matches(id);
    }
    let mut compared = false;
    if let Some(frozen) = vector.frozen() {
        compared = true;
        if *frozen != got {
            let detail = format!("frozen {frozen}\n  rondi  {got}");
            return Outcome::Differs(id, detail, got);
        }
    }
    if let Some(program) = oracle {
        let reference = run_vector(program, up, vector);
        if !failed_to_finish(&reference) {
            compared = true;
            if reference != got {
                let detail = format!("live   {reference}\n  rondi  {got}");
                return Outcome::Differs(id, detail, got);
            }
        }
    }
    if compared {
        Outcome::Matches(id)
    } else {
        Outcome::NotCompared
    }
}

/// A policy entry must still produce its policy error text, so that a
/// policy cannot silently turn into some other difference.
fn divergence_holds(divergence: &Divergence, got: &Value) -> Result<(), String> {
    let Class::Policy(text) = divergence.class else {
        return Ok(());
    };
    let stderr = got
        .as_array()
        .and_then(|steps| steps.last())
        .and_then(|step| step["stderr"].as_str())
        .unwrap_or_default();
    if stderr.contains(text) {
        Ok(())
    } else {
        Err(format!(
            "{}: policy text {text:?} missing from Rondi stderr {stderr:?}",
            divergence.id
        ))
    }
}

#[test]
fn graph_spec_vectors_match_rrdtool() {
    let (table, vectors) = load();
    assert!(vectors.len() >= 1500, "only {} vectors", vectors.len());
    for divergence in KNOWN_DIVERGENCES {
        assert!(
            vectors.iter().any(|vector| vector.id == divergence.id),
            "{}: listed divergence has no vector",
            divergence.id
        );
    }
    let temp = tempfile::tempdir().unwrap();
    let rondi = temp.path().join("bin").join("rrdtool");
    std::fs::create_dir(rondi.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rondi"), &rondi).unwrap();
    let ro = temp.path().join("ro");
    setup(&rondi, &ro, &table);
    let oracle = live_oracle();
    let up = temp.path().join("up");
    if let Some(program) = &oracle {
        setup(program, &up, &table);
    }

    // The slow cases (large images, long fetches) sit together at the end of
    // the table, so workers take the next vector from a shared counter
    // rather than a fixed slice.
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    let next = AtomicUsize::new(0);
    let outcomes: Vec<Outcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (rondi, ro, up, oracle, next) = (&rondi, &ro, &up, &oracle, &next);
                let vectors = &vectors;
                scope.spawn(move || {
                    let mut done = Vec::new();
                    while let Some(vector) = vectors.get(next.fetch_add(1, Ordering::Relaxed)) {
                        done.push(check(vector, rondi, ro, up, oracle.as_deref()));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });

    let here = platform_key().unwrap_or("macos");
    let mut failures = Vec::new();
    let mut divergent = Vec::new();
    for outcome in outcomes {
        let (id, detail, got) = match outcome {
            Outcome::Matches(id) => (id, None, Value::Null),
            Outcome::Differs(id, detail, got) => (id, Some(detail), got),
            Outcome::NotCompared => continue,
        };
        let known = KNOWN_DIVERGENCES.iter().find(|divergence| {
            divergence.id == id
                && (divergence.platforms.is_empty() || divergence.platforms.contains(&here))
        });
        match (detail, known) {
            (None, Some(divergence)) => failures.push(format!(
                "{id}: now matches RRDtool; remove it from KNOWN_DIVERGENCES ({})",
                divergence.reason
            )),
            (Some(_), Some(divergence)) => match divergence_holds(divergence, &got) {
                Ok(()) => divergent.push(id),
                Err(problem) => failures.push(problem),
            },
            (Some(detail), None) => failures.push(format!("{id}\n  {detail}")),
            (None, None) => {}
        }
    }
    if !divergent.is_empty() {
        eprintln!(
            "known graph divergences still present ({}): {divergent:?}",
            divergent.len()
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} graph vectors failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}
