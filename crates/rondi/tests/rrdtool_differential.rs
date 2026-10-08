#[macro_use]
mod common;

use rondi::{DatabaseConfig, Store, Update};
use std::process::Command;

#[test]
fn gauge_irregular_sample_average_matches_rrdtool() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        oracle_skip!("skipping RRDtool differential test: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let rrd = temp.path().join("oracle.rrd");
    let start = 1_000_000_000_i64;
    let output = Command::new("rrdtool")
        .args([
            "create",
            rrd.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:15:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("rrdtool")
        .args([
            "update",
            rrd.to_str().unwrap(),
            "1000000004:2",
            "1000000010:8",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("rrdtool")
        .args([
            "fetch",
            rrd.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000010",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let upstream = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .map(|(_, value)| value.trim().to_owned())
        })
        .and_then(|value| value.parse::<f64>().ok())
        .expect("RRDtool returned a first PDP");

    let root = temp.path().join("rondi");
    let store = Store::open(&root).unwrap();
    store
        .create(
            "oracle",
            DatabaseConfig {
                step: 10,
                heartbeat: 15,
                rows: 8,
                start,
            },
        )
        .unwrap();
    for (timestamp, value) in [(start + 4, 2.0), (start + 10, 8.0)] {
        store
            .update(
                "oracle",
                Update {
                    timestamp,
                    value: Some(value),
                },
            )
            .unwrap();
    }
    let actual = store.fetch("oracle").unwrap().points[0].value.unwrap();
    assert!(
        (actual - upstream).abs() < 1e-10,
        "Rondi={actual}, RRDtool={upstream}"
    );
}

#[test]
fn heartbeat_and_unknown_intervals_match_rrdtool() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        oracle_skip!("skipping RRDtool differential test: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let rrd = temp.path().join("unknown-oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            rrd.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:15:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let update = Command::new("rrdtool")
        .args([
            "update",
            rrd.to_str().unwrap(),
            "1000000010:1",
            "1000000020:U",
            "1000000040:5",
            "1000000050:5",
        ])
        .output()
        .unwrap();
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let fetched = Command::new("rrdtool")
        .args([
            "fetch",
            rrd.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000050",
        ])
        .output()
        .unwrap();
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    let upstream = String::from_utf8(fetched.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let timestamp = fields.next()?.trim_end_matches(':').parse::<i64>().ok()?;
            let value = fields
                .next()?
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite());
            Some((timestamp, value))
        })
        .filter(|(timestamp, _)| (1_000_000_010..=1_000_000_050).contains(timestamp))
        .collect::<Vec<_>>();

    let store = Store::open(temp.path().join("rondi")).unwrap();
    store
        .create(
            "unknown",
            DatabaseConfig {
                step: 10,
                heartbeat: 15,
                rows: 8,
                start: 1_000_000_000,
            },
        )
        .unwrap();
    for (timestamp, value) in [
        (1_000_000_010, Some(1.0)),
        (1_000_000_020, None),
        (1_000_000_040, Some(5.0)),
        (1_000_000_050, Some(5.0)),
    ] {
        store
            .update("unknown", Update { timestamp, value })
            .unwrap();
    }
    let actual = store
        .fetch("unknown")
        .unwrap()
        .points
        .into_iter()
        .map(|point| (point.timestamp, point.value))
        .collect::<Vec<_>>();
    assert_eq!(actual.len(), upstream.len());
    for ((actual_ts, actual_value), (oracle_ts, oracle_value)) in actual.iter().zip(upstream) {
        assert_eq!(*actual_ts, oracle_ts);
        match (actual_value, oracle_value) {
            (None, None) => {}
            (Some(a), Some(b)) => assert!((a - b).abs() < 1e-10, "Rondi={a}, RRDtool={b}"),
            pair => panic!("Rondi/RRDtool unknown mismatch: {pair:?}"),
        }
    }
}

#[test]
fn duplicate_and_out_of_order_timestamps_are_rejected_by_both() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        oracle_skip!("skipping RRDtool differential test: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let rrd = temp.path().join("timestamps.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            rrd.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let accepted = Command::new("rrdtool")
        .args(["update", rrd.to_str().unwrap(), "1000000010:1"])
        .output()
        .unwrap();
    assert!(accepted.status.success());
    for rejected_update in ["1000000010:2", "1000000009:2"] {
        let rejected = Command::new("rrdtool")
            .args(["update", rrd.to_str().unwrap(), rejected_update])
            .output()
            .unwrap();
        assert!(
            !rejected.status.success(),
            "RRDtool unexpectedly accepted {rejected_update}"
        );
    }

    let store = Store::open(temp.path().join("rondi")).unwrap();
    store
        .create(
            "timestamps",
            DatabaseConfig {
                step: 10,
                heartbeat: 20,
                rows: 8,
                start: 1_000_000_000,
            },
        )
        .unwrap();
    store
        .update(
            "timestamps",
            Update {
                timestamp: 1_000_000_010,
                value: Some(1.0),
            },
        )
        .unwrap();
    for timestamp in [1_000_000_010, 1_000_000_009] {
        assert!(
            store
                .update(
                    "timestamps",
                    Update {
                        timestamp,
                        value: Some(2.0),
                    },
                )
                .is_err()
        );
    }
}

/// splitmix64: a fixed, dependency-free generator so every run and platform
/// draws the same cases.
struct CaseRng(u64);

impl CaseRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + self.below((high - low + 1) as u64) as i64
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

struct UpdateCase {
    create: Vec<String>,
    updates: Vec<(i64, u64, Vec<Option<String>>)>,
}

fn random_update_case(rng: &mut CaseRng) -> UpdateCase {
    let step = [1_i64, 5, 10, 60, 300][rng.below(5) as usize];
    let start = 1_000_000_000 + rng.range(0, 600);
    let kinds = [
        "GAUGE", "COUNTER", "DERIVE", "ABSOLUTE", "DCOUNTER", "DDERIVE",
    ];
    let mut create = vec![
        "--start".to_owned(),
        start.to_string(),
        "--step".to_owned(),
        step.to_string(),
    ];
    let mut sources = Vec::new();
    for index in 0..rng.range(1, 3) {
        let kind = rng.pick(&kinds);
        let heartbeat = step * [1, 2, 3, 10][rng.below(4) as usize];
        let minimum = rng.pick(&["U", "U", "0", "-100"]);
        let maximum = rng.pick(&["U", "U", "1000", "100000"]);
        create.push(format!(
            "DS:d{index}:{kind}:{heartbeat}:{minimum}:{maximum}"
        ));
        sources.push((kind, rng.range(0, 1000)));
    }
    for _ in 0..rng.range(1, 4) {
        create.push(format!(
            "RRA:{}:{}:{}:{}",
            rng.pick(&["AVERAGE", "MIN", "MAX", "LAST"]),
            rng.pick(&["0", "0.5", "0.9", "0.25"]),
            rng.range(1, 6),
            rng.range(1, 8)
        ));
    }
    let mut timestamp = start;
    let mut updates = Vec::new();
    for _ in 0..rng.range(1, 25) {
        let gap = match rng.below(7) {
            0 => 1,
            1 => (step / 2).max(1),
            2 => step,
            3 => step + 1,
            4 => 2 * step,
            5 => 3 * step + rng.range(0, step),
            _ => rng.range(1, 20 * step),
        };
        timestamp += gap;
        let usec = if rng.below(10) < 3 {
            rng.range(1, 999_999) as u64
        } else {
            0
        };
        let values = sources
            .iter_mut()
            .map(|(kind, last)| {
                if rng.below(100) < 15 {
                    return None;
                }
                Some(match *kind {
                    "COUNTER" | "DCOUNTER" => {
                        *last = if rng.below(100) < 95 {
                            *last + rng.range(0, 5000)
                        } else {
                            (*last - rng.range(1, 100)).max(0)
                        };
                        last.to_string()
                    }
                    "DERIVE" => {
                        *last += rng.range(-500, 5000);
                        last.to_string()
                    }
                    _ => match rng.below(3) {
                        0 => rng.range(0, 2000).to_string(),
                        1 => rng.range(-50, 50).to_string(),
                        _ => {
                            let thousandths = rng.range(0, 500_000);
                            format!("{}.{:03}", thousandths / 1000, thousandths % 1000)
                        }
                    },
                })
            })
            .collect();
        updates.push((timestamp, usec, values));
    }
    UpdateCase { create, updates }
}

fn update_argument(timestamp: i64, usec: u64, values: &[Option<String>]) -> String {
    let mut argument = if usec == 0 {
        timestamp.to_string()
    } else {
        format!("{timestamp}.{usec:06}")
    };
    for value in values {
        argument.push(':');
        argument.push_str(value.as_deref().unwrap_or("U"));
    }
    argument
}

/// Randomized byte-level differential for in-place updates. Each case is
/// created by RRDtool, then the same updates are applied by both
/// implementations and the files are compared after every chunk.
#[test]
fn seeded_random_updates_match_rrdtool_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping randomized update differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }
    const SEED: u64 = 0x5eed_2026_1007;
    const CASES: usize = 100;
    let temp = tempfile::tempdir().unwrap();
    let mut rng = CaseRng(SEED);
    for case_index in 0..CASES {
        let case = random_update_case(&mut rng);
        let oracle = temp.path().join(format!("case-{case_index}-oracle.rrd"));
        let ours = temp.path().join(format!("case-{case_index}-rondi.rrd"));
        let created = Command::new("rrdtool")
            .arg("create")
            .arg(&oracle)
            .args(&case.create)
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "seed {SEED:#x} case {case_index}: create {:?}: {}",
            case.create,
            String::from_utf8_lossy(&created.stderr)
        );
        std::fs::copy(&oracle, &ours).unwrap();
        let mut applied = Vec::new();
        for chunk in case.updates.chunks(5) {
            let arguments = chunk
                .iter()
                .map(|(timestamp, usec, values)| update_argument(*timestamp, *usec, values))
                .collect::<Vec<_>>();
            applied.extend(arguments.iter().cloned());
            let context = format!(
                "seed {SEED:#x} case {case_index}: rrdtool create {} then update {}",
                case.create.join(" "),
                applied.join(" ")
            );
            let updated = Command::new("rrdtool")
                .arg("update")
                .arg(&oracle)
                .args(&arguments)
                .output()
                .unwrap();
            assert!(
                updated.status.success(),
                "{context}: {}",
                String::from_utf8_lossy(&updated.stderr)
            );
            for (argument, (_, _, values)) in arguments.iter().zip(chunk) {
                // get_time_from_reading converts the text to a double and
                // derives microseconds from it, which can differ by one.
                let (time_text, _) = argument.split_once(':').unwrap();
                let time = rondi::parse_rrd_number(time_text).unwrap();
                let seconds = time.floor();
                let usec = ((time - seconds) * 1_000_000.0) as u64;
                let values = values.iter().map(Option::as_deref).collect::<Vec<_>>();
                if let Err(error) =
                    rondi::update_rrd_raw_values_precise(&ours, seconds as i64, usec, &values)
                {
                    panic!("{context}: Rondi rejected an update RRDtool accepted: {error}");
                }
            }
            let expected = std::fs::read(&oracle).unwrap();
            let actual = std::fs::read(&ours).unwrap();
            let first_difference = expected
                .iter()
                .zip(&actual)
                .position(|(left, right)| left != right);
            assert!(
                expected.len() == actual.len() && first_difference.is_none(),
                "{context}: files differ first at byte {first_difference:?}"
            );
        }
    }
}

/// DCOUNTER/DDERIVE samples that are infinite, NaN, or not numbers, in every
/// ordered pair and across multi-step spans. update_pdp_prep converts the
/// sample and the previous one only when the previous is known, so text that
/// does not convert is accepted after an unknown sample and fails one update
/// later.
#[test]
fn dcounter_dderive_special_samples_match_rrdtool_byte_for_byte() {
    use std::io::Write;
    use std::process::Stdio;

    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        oracle_skip!(
            "skipping special-sample differential: pinned RRDtool 1.11.0 is not installed"
        );
        return;
    }
    let samples = [
        "inf",
        "-inf",
        "-Infinity",
        "nan",
        "5",
        "-5",
        "0",
        "1e308",
        "abc",
        "+inf",
        "1x",
    ];
    let temp = tempfile::tempdir().unwrap();
    let mut case_index = 0;
    for (kind, bounds) in [
        ("DCOUNTER", "U:U"),
        ("DDERIVE", "U:U"),
        ("DCOUNTER", "-100:100"),
        ("DDERIVE", "-100:100"),
    ] {
        for seed in ["3", "U"] {
            for first in samples {
                for second in samples {
                    let oracle = temp.path().join(format!("special-{case_index}-oracle.rrd"));
                    let ours = temp.path().join(format!("special-{case_index}-rondi.rrd"));
                    case_index += 1;
                    let create = [
                        "--start".to_owned(),
                        "1000000000".to_owned(),
                        "--step".to_owned(),
                        "10".to_owned(),
                        format!("DS:d:{kind}:100:{bounds}"),
                        "DS:g:GAUGE:100:U:U".to_owned(),
                        "RRA:AVERAGE:0.5:1:10".to_owned(),
                        "RRA:MIN:0.5:2:10".to_owned(),
                        "RRA:MAX:0.9:3:10".to_owned(),
                        "RRA:LAST:0:1:10".to_owned(),
                    ];
                    let created = Command::new("rrdtool")
                        .arg("create")
                        .arg(&oracle)
                        .args(&create)
                        .output()
                        .unwrap();
                    assert!(
                        created.status.success(),
                        "{}",
                        String::from_utf8_lossy(&created.stderr)
                    );
                    std::fs::copy(&oracle, &ours).unwrap();
                    // The pair spans several steps; the finite samples after
                    // it consume whatever prep state the pair left.
                    let updates = [
                        (1_000_000_003_i64, 0_u64, seed),
                        (1_000_000_027, 500_000, first),
                        (1_000_000_061, 0, second),
                        (1_000_000_064, 0, "7"),
                        (1_000_000_095, 0, "9"),
                    ];
                    let arguments = updates
                        .iter()
                        .map(|(timestamp, usec, value)| {
                            let value = (*value != "U").then(|| (*value).to_owned());
                            update_argument(*timestamp, *usec, &[value, Some("1".to_owned())])
                        })
                        .collect::<Vec<_>>();
                    let context = format!(
                        "rrdtool create {} then update {}",
                        create.join(" "),
                        arguments.join(" ")
                    );
                    // One pipe-mode session per case: each line reports its own
                    // status and a failed line does not stop the next one.
                    let mut session = Command::new("rrdtool")
                        .arg("-")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    let mut input = String::new();
                    for argument in &arguments {
                        input.push_str(&format!("update {} {argument}\n", oracle.display()));
                    }
                    session
                        .stdin
                        .take()
                        .unwrap()
                        .write_all(input.as_bytes())
                        .unwrap();
                    let output = session.wait_with_output().unwrap();
                    let upstream = String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter(|line| line.starts_with("OK") || line.starts_with("ERROR"))
                        .map(|line| line.starts_with("OK"))
                        .collect::<Vec<_>>();
                    let local = updates
                        .iter()
                        .map(|(timestamp, usec, value)| {
                            let value = (*value != "U").then_some(*value);
                            rondi::update_rrd_raw_values_precise(
                                &ours,
                                *timestamp,
                                *usec,
                                &[value, Some("1")],
                            )
                            .is_ok()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(upstream, local, "{context}: accepted updates differ");
                    let expected = std::fs::read(&oracle).unwrap();
                    let actual = std::fs::read(&ours).unwrap();
                    let first_difference = expected
                        .iter()
                        .zip(&actual)
                        .position(|(left, right)| left != right);
                    assert!(
                        expected.len() == actual.len() && first_difference.is_none(),
                        "{context}: files differ first at byte {first_difference:?}"
                    );
                }
            }
        }
    }
}
