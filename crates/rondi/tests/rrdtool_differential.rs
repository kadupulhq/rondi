use rondi::{DatabaseConfig, Store, Update};
use std::process::Command;

#[test]
fn gauge_irregular_sample_average_matches_rrdtool() {
    if Command::new("rrdtool").arg("--version").output().is_err() {
        eprintln!("skipping RRDtool differential test: rrdtool is not installed");
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
        eprintln!("skipping RRDtool differential test: rrdtool is not installed");
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
        eprintln!("skipping RRDtool differential test: rrdtool is not installed");
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
