use rondi::{Store, StoreError, update_rrd_values};
use std::process::Command;

#[test]
fn inspects_disposable_rrdtool_v3_file_without_modifying_it() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool binary probe: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cpu.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:cpu:GAUGE:20:U:U",
            "DS:load:GAUGE:30:0:100",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.25:5:4",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let original = std::fs::read(&path).unwrap();
    let store = Store::open(temp.path()).unwrap();
    let info = store.inspect_rrd("cpu").unwrap();
    assert_eq!(info.version, "0003");
    assert_eq!(info.step, 10);
    assert_eq!(info.last_update, 1_000_000_000);
    assert_eq!(info.data_sources.len(), 2);
    assert_eq!(info.data_sources[0].name, "cpu");
    assert_eq!(info.data_sources[0].kind, "GAUGE");
    assert_eq!(info.data_sources[0].heartbeat, 20);
    assert_eq!(info.data_sources[0].minimum, None);
    assert_eq!(info.data_sources[1].name, "load");
    assert_eq!(info.data_sources[1].minimum, Some(0.0));
    assert_eq!(info.data_sources[1].maximum, Some(100.0));
    assert_eq!(info.archives.len(), 2);
    assert_eq!(info.archives[0].consolidation, "AVERAGE");
    assert_eq!(info.archives[0].rows, 8);
    assert_eq!(info.archives[0].pdp_per_row, 1);
    assert_eq!(info.archives[0].xff, 0.5);
    assert_eq!(info.archives[1].consolidation, "MAX");
    assert_eq!(info.archives[1].rows, 4);
    assert_eq!(info.archives[1].pdp_per_row, 5);
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn truncated_rrd_is_reported_as_a_format_error() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool binary probe: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:cpu:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(100);
    std::fs::write(&path, bytes).unwrap();
    let store = Store::open(temp.path()).unwrap();
    assert!(matches!(
        store.inspect_rrd("bad"),
        Err(StoreError::RrdFormat(_))
    ));
}

#[test]
fn metadata_inspection_does_not_read_a_large_archive_payload() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool binary probe: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:cpu:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());

    // Make a sparse, disposable file whose valid header declares an 8 GB
    // archive. The inspector should validate the length from metadata without
    // allocating or reading the archive payload.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    use std::io::{Seek, SeekFrom, Write};
    let row_count = 1_000_000_000_u64.to_le_bytes();
    file.seek(SeekFrom::Start(272)).unwrap(); // first RRA row_cnt in v3
    file.write_all(&row_count).unwrap();
    let header_len = 584_u64; // one DS, one RRA, common 64-bit v3 layout
    file.set_len(header_len + 8 * 1_000_000_000).unwrap();
    file.flush().unwrap();
    drop(file);
    let store = Store::open(temp.path()).unwrap();
    let info = store.inspect_rrd("large").unwrap();
    assert_eq!(info.archives[0].rows, 1_000_000_000);
}

#[test]
fn fetch_matches_rrdtool_for_ring_order_and_padded_range() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool differential fetch: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fetch.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let update = Command::new("rrdtool")
        .args([
            "update",
            path.to_str().unwrap(),
            "1000000010:1",
            "1000000020:2",
            "1000000030:3",
        ])
        .output()
        .unwrap();
    assert!(update.status.success());

    let oracle = Command::new("rrdtool")
        .args([
            "fetch",
            path.to_str().unwrap(),
            "AVERAGE",
            "--start",
            "1000000000",
            "--end",
            "1000000040",
            "--resolution",
            "10",
        ])
        .output()
        .unwrap();
    assert!(oracle.status.success());
    let oracle_rows = String::from_utf8(oracle.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (timestamp, values) = line.trim().split_once(':')?;
            let timestamp = timestamp.parse::<i64>().ok()?;
            let value = values.trim().parse::<f64>().ok();
            Some((timestamp, value.filter(|value| value.is_finite())))
        })
        .collect::<Vec<_>>();

    let store = Store::open(temp.path()).unwrap();
    let result = store
        .fetch_rrd("fetch", "AVERAGE", 1_000_000_000, 1_000_000_040, 10)
        .unwrap();
    let actual = result
        .rows
        .into_iter()
        .map(|row| (row.timestamp, row.values[0]))
        .collect::<Vec<_>>();
    assert_eq!(actual, oracle_rows);
}

#[test]
fn fetch_uses_rrdtool_resolution_choice_across_archives() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool differential fetch: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("resolutions.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:30:U:U",
            "RRA:AVERAGE:0.5:1:16",
            "RRA:AVERAGE:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    let updates = (1..=10)
        .map(|index| format!("{}:{}", 1_000_000_000 + index * 10, index))
        .collect::<Vec<_>>();
    let mut command = Command::new("rrdtool");
    command.arg("update").arg(&path).args(&updates);
    let updated = command.output().unwrap();
    assert!(
        updated.status.success(),
        "{}",
        String::from_utf8_lossy(&updated.stderr)
    );

    let options = [
        "--start",
        "1000000000",
        "--end",
        "1000000100",
        "--resolution",
        "18",
    ];
    let oracle = Command::new("rrdtool")
        .arg("fetch")
        .arg(&path)
        .arg("AVERAGE")
        .args(options)
        .output()
        .unwrap();
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8(oracle.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let (timestamp, value) = line.trim().split_once(':')?;
            Some((
                timestamp.parse::<i64>().ok()?,
                value
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite()),
            ))
        })
        .collect::<Vec<_>>();
    let result = rondi::fetch_rrd_file(&path, "AVERAGE", 1_000_000_000, 1_000_000_100, 18).unwrap();
    assert_eq!(result.step, 20);
    let actual = result
        .rows
        .into_iter()
        .map(|row| (row.timestamp, row.values[0]))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn in_place_gauge_update_is_byte_compatible_and_upstream_can_continue() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let rondi_path = temp.path().join("rondi.rrd");
    let oracle_path = temp.path().join("oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            rondi_path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    std::fs::copy(&rondi_path, &oracle_path).unwrap();
    for path in [&rondi_path, &oracle_path] {
        let initial = Command::new("rrdtool")
            .args(["update", path.to_str().unwrap(), "1000000010:1"])
            .output()
            .unwrap();
        assert!(
            initial.status.success(),
            "{}",
            String::from_utf8_lossy(&initial.stderr)
        );
    }

    rondi::update_rrd_file(&rondi_path, 1_000_000_020, Some(2.0)).unwrap();
    let oracle_update = Command::new("rrdtool")
        .args(["update", oracle_path.to_str().unwrap(), "1000000020:2"])
        .output()
        .unwrap();
    assert!(oracle_update.status.success());
    let rondi_bytes = std::fs::read(&rondi_path).unwrap();
    let oracle_bytes = std::fs::read(&oracle_path).unwrap();
    let first_difference = rondi_bytes
        .iter()
        .zip(&oracle_bytes)
        .position(|(left, right)| left != right);
    assert!(
        rondi_bytes == oracle_bytes,
        "RRD differs from upstream at byte {first_difference:?}; pointers: ours={}, upstream={}",
        u64::from_le_bytes(rondi_bytes[576..584].try_into().unwrap()),
        u64::from_le_bytes(oracle_bytes[576..584].try_into().unwrap())
    );

    // The same upstream binary then reopens both files and advances them.
    for path in [&rondi_path, &oracle_path] {
        let next = Command::new("rrdtool")
            .args(["update", path.to_str().unwrap(), "1000000030:3"])
            .output()
            .unwrap();
        assert!(
            next.status.success(),
            "{}",
            String::from_utf8_lossy(&next.stderr)
        );
    }
    assert_eq!(
        std::fs::read(&rondi_path).unwrap(),
        std::fs::read(&oracle_path).unwrap()
    );
}

#[test]
fn multipdp_average_update_matches_rrdtool_in_place() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:16",
            "RRA:AVERAGE:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    for (timestamp, value) in [
        (1_000_000_010, Some(1.0)),
        (1_000_000_020, Some(3.0)),
        (1_000_000_030, None),
        (1_000_000_040, Some(5.0)),
        (1_000_000_050, Some(7.0)),
    ] {
        rondi::update_rrd_file(&ours, timestamp, value).unwrap();
        let sample = value.map_or_else(|| format!("{timestamp}:U"), |v| format!("{timestamp}:{v}"));
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        let ours_bytes = std::fs::read(&ours).unwrap();
        let oracle_bytes = std::fs::read(&oracle).unwrap();
        let differences = ours_bytes
            .iter()
            .zip(&oracle_bytes)
            .enumerate()
            .filter_map(|(offset, (ours, oracle))| {
                (ours != oracle).then_some((offset, *ours, *oracle))
            })
            .take(12)
            .collect::<Vec<_>>();
        assert!(
            differences.is_empty(),
            "file differs after update {sample}: {differences:?}"
        );
    }
}

#[test]
fn multipdp_min_max_last_updates_match_rrdtool_in_place() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for cf in ["MIN", "MAX", "LAST"] {
        let ours = temp.path().join(format!("ours-{cf}.rrd"));
        let oracle = temp.path().join(format!("oracle-{cf}.rrd"));
        let archive = format!("RRA:{cf}:0.5:2:8");
        let created = Command::new("rrdtool")
            .args([
                "create",
                ours.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                "DS:a:GAUGE:20:U:U",
                archive.as_str(),
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        std::fs::copy(&ours, &oracle).unwrap();
        for (timestamp, value) in [
            (1_000_000_010, Some(1.0)),
            (1_000_000_020, Some(3.0)),
            (1_000_000_030, Some(2.0)),
            (1_000_000_040, Some(4.0)),
            (1_000_000_050, None),
            (1_000_000_060, Some(9.0)),
        ] {
            rondi::update_rrd_file(&ours, timestamp, value).unwrap();
            let sample =
                value.map_or_else(|| format!("{timestamp}:U"), |v| format!("{timestamp}:{v}"));
            let updated = Command::new("rrdtool")
                .args(["update", oracle.to_str().unwrap(), &sample])
                .output()
                .unwrap();
            assert!(
                updated.status.success(),
                "{}",
                String::from_utf8_lossy(&updated.stderr)
            );
            assert_eq!(
                std::fs::read(&ours).unwrap(),
                std::fs::read(&oracle).unwrap(),
                "{cf} differs after {sample}"
            );
        }
    }
}

#[test]
fn multiple_gauge_sources_update_matches_rrdtool_in_place() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:0:U",
            "DS:b:GAUGE:15:U:100",
            "RRA:AVERAGE:0.5:1:16",
            "RRA:MAX:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    for (timestamp, a, b) in [
        (1_000_000_010, Some(1.0), Some(8.0)),
        (1_000_000_020, Some(3.0), Some(9.0)),
        (1_000_000_030, None, Some(120.0)),
        (1_000_000_040, Some(5.0), Some(7.0)),
        (1_000_000_050, Some(6.0), None),
    ] {
        rondi::update_rrd_values(&ours, timestamp, &[a, b]).unwrap();
        let value = |v: Option<f64>| v.map_or_else(|| "U".to_owned(), |n| n.to_string());
        let sample = format!("{timestamp}:{}:{}", value(a), value(b));
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&oracle).unwrap(),
            "multiple DS differ after {sample}"
        );
    }
}

#[test]
fn counter_derive_and_absolute_updates_match_rrdtool_in_place() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:c:COUNTER:20:U:U",
            "DS:d:DERIVE:20:-10:10",
            "DS:a:ABSOLUTE:20:U:U",
            "DS:g:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:16",
            "RRA:MAX:0.5:2:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    let cases = [
        (
            1_000_000_010,
            Some(4_294_967_294.0),
            Some(100.0),
            Some(20.0),
            Some(3.0),
        ),
        (1_000_000_020, Some(3.0), Some(90.0), Some(40.0), Some(4.0)),
        (1_000_000_030, Some(15.0), Some(105.0), Some(5.0), None),
        (1_000_000_040, Some(25.0), Some(95.0), Some(25.0), Some(8.0)),
    ];
    for (timestamp, counter, derive, absolute, gauge) in cases {
        let values = [counter, derive, absolute, gauge];
        rondi::update_rrd_values(&ours, timestamp, &values).unwrap();
        let value = |v: Option<f64>| v.map_or_else(|| "U".to_owned(), |n| n.to_string());
        let sample = format!(
            "{timestamp}:{}:{}:{}:{}",
            value(counter),
            value(derive),
            value(absolute),
            value(gauge)
        );
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&oracle).unwrap(),
            "counter family differs after {sample}"
        );
    }
}

#[test]
fn large_counter_and_derive_inputs_keep_decimal_precision_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping large integer differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:c:COUNTER:20:U:U",
            "DS:d:DERIVE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();

    for (timestamp, counter, derive) in [
        (1_000_000_010, "9007199254740993", "-9007199254740993"),
        (1_000_000_020, "9007199254740994", "-9007199254740994"),
    ] {
        rondi::update_rrd_raw_values(&ours, timestamp, &[Some(counter), Some(derive)]).unwrap();
        let sample = format!("{timestamp}:{counter}:{derive}");
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&oracle).unwrap(),
            "integer sample differs after {sample}"
        );
    }
}

#[test]
fn dcounter_and_dderive_reset_semantics_match_rrdtool_byte_for_byte() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping RRDtool DCOUNTER differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:dc:DCOUNTER:30:U:U",
            "DS:dd:DDERIVE:30:U:U",
            "DS:reset:DCOUNTER:30:U:U",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    for (timestamp, values) in [
        (1_000_000_010, [Some(100.5), Some(10.0), Some(100.0)]),
        (1_000_000_020, [Some(110.75), Some(15.25), Some(110.0)]),
        (1_000_000_030, [Some(105.5), Some(12.5), Some(90.0)]),
        (1_000_000_040, [Some(107.25), Some(13.5), Some(92.5)]),
    ] {
        update_rrd_values(&ours, timestamp, &values).unwrap();
        let sample = format!(
            "{timestamp}:{}:{}:{}",
            values[0].unwrap(),
            values[1].unwrap(),
            values[2].unwrap()
        );
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        assert_eq!(
            std::fs::read(&ours).unwrap(),
            std::fs::read(&oracle).unwrap(),
            "after {sample}"
        );
    }
}

#[test]
fn bulk_updates_cross_multiple_pdp_boundaries_and_roll_archives_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping RRDtool bulk update differential: rrdtool is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let ours = temp.path().join("ours.rrd");
    let oracle = temp.path().join("oracle.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            ours.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:g:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:AVERAGE:0.25:2:5",
            "RRA:MAX:0.5:3:5",
            "RRA:LAST:0.5:1:10",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    std::fs::copy(&ours, &oracle).unwrap();
    let initial_rows = rondi::inspect_rrd_file(&ours)
        .unwrap()
        .archives
        .iter()
        .map(|archive| archive.current_row)
        .collect::<Vec<_>>();
    for (timestamp, value) in [
        (1_000_000_005, Some(1.0)),
        (1_000_000_045, Some(9.0)),
        (1_000_000_075, None),
        (1_000_001_005, Some(3.5)),
        (1_000_001_035, Some(7.0)),
    ] {
        update_rrd_values(&ours, timestamp, &[value]).unwrap();
        let sample = value.map_or_else(
            || format!("{timestamp}:U"),
            |value| format!("{timestamp}:{value}"),
        );
        let updated = Command::new("rrdtool")
            .args(["update", oracle.to_str().unwrap(), &sample])
            .output()
            .unwrap();
        assert!(
            updated.status.success(),
            "{}",
            String::from_utf8_lossy(&updated.stderr)
        );
        let ours_bytes = std::fs::read(&ours).unwrap();
        let oracle_bytes = std::fs::read(&oracle).unwrap();
        let differences = ours_bytes
            .iter()
            .zip(&oracle_bytes)
            .enumerate()
            .filter_map(|(offset, (ours, oracle))| {
                (ours != oracle).then_some((offset, *ours, *oracle))
            })
            .take(12)
            .collect::<Vec<_>>();
        let upstream_info = Command::new("rrdtool")
            .args(["info", oracle.to_str().unwrap()])
            .output()
            .unwrap();
        let upstream_rows = String::from_utf8_lossy(&upstream_info.stdout)
            .lines()
            .filter_map(|line| line.split_once(".cur_row = ")?.1.parse::<u64>().ok())
            .collect::<Vec<_>>();
        assert!(
            differences.is_empty(),
            "byte mismatch after {sample}: {differences:?}; initial pointers={initial_rows:?}; Rondi pointers={:?}; upstream pointers={:?}",
            rondi::inspect_rrd_file(&ours)
                .map(|rrd| rrd
                    .archives
                    .iter()
                    .map(|archive| archive.current_row)
                    .collect::<Vec<_>>())
                .map_err(|error| error.to_string()),
            upstream_rows
        );
    }
}

#[test]
fn in_place_update_matches_irregular_unknown_and_heartbeat_semantics() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let cases: [(&str, &[(&str, &str)]); 3] = [
        (
            "20",
            &[
                ("1000000004", "1"),
                ("1000000007", "3"),
                ("1000000010", "7"),
                ("1000000015", "9"),
                ("1000000020", "11"),
            ],
        ),
        (
            "20",
            &[
                ("1000000004", "U"),
                ("1000000010", "5"),
                ("1000000020", "7"),
            ],
        ),
        (
            "5",
            &[
                ("1000000006", "2"),
                ("1000000010", "4"),
                ("1000000020", "8"),
            ],
        ),
    ];
    for (case_index, (heartbeat, updates)) in cases.into_iter().enumerate() {
        let rondi_path = temp.path().join(format!("case-{case_index}.rrd"));
        let oracle_path = temp.path().join(format!("oracle-{case_index}.rrd"));
        let create = Command::new("rrdtool")
            .args([
                "create",
                rondi_path.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                &format!("DS:a:GAUGE:{heartbeat}:U:U"),
                "RRA:AVERAGE:0.5:1:8",
            ])
            .output()
            .unwrap();
        assert!(
            create.status.success(),
            "{}",
            String::from_utf8_lossy(&create.stderr)
        );
        std::fs::copy(&rondi_path, &oracle_path).unwrap();
        for (timestamp, value) in updates {
            rondi::update_rrd_file(
                &rondi_path,
                timestamp.parse().unwrap(),
                if *value == "U" {
                    None
                } else {
                    Some(value.parse().unwrap())
                },
            )
            .unwrap();
            let update = Command::new("rrdtool")
                .args([
                    "update",
                    oracle_path.to_str().unwrap(),
                    &format!("{timestamp}:{value}"),
                ])
                .output()
                .unwrap();
            assert!(
                update.status.success(),
                "{}",
                String::from_utf8_lossy(&update.stderr)
            );
            assert_eq!(
                std::fs::read(&rondi_path).unwrap(),
                std::fs::read(&oracle_path).unwrap(),
                "differential mismatch at {timestamp}:{value} in case {case_index}"
            );
        }
    }
}

#[test]
fn unsupported_in_place_update_is_rejected_without_mutating_the_rrd() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool update probe: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("unsupported.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "DS:c:COMPUTE:a,2,*",
            "RRA:AVERAGE:0.5:1:8",
        ])
        .output()
        .unwrap();
    assert!(create.status.success());
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        rondi::update_rrd_values(&path, 1_000_000_030, &[Some(3.0), Some(6.0)]),
        Err(StoreError::RrdUnsupported(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn in_place_update_advances_each_basic_base_step_archive() {
    let version = Command::new("rrdtool").arg("--version").output();
    let Ok(version) = version else {
        eprintln!("skipping RRDtool differential update: rrdtool is not installed");
        return;
    };
    assert!(version.status.success());
    let temp = tempfile::tempdir().unwrap();
    let rondi_path = temp.path().join("multi.rrd");
    let oracle_path = temp.path().join("multi-oracle.rrd");
    let create = Command::new("rrdtool")
        .args([
            "create",
            rondi_path.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:a:GAUGE:20:U:U",
            "RRA:AVERAGE:0.5:1:8",
            "RRA:MAX:0.5:1:5",
        ])
        .output()
        .unwrap();
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );
    std::fs::copy(&rondi_path, &oracle_path).unwrap();
    for (timestamp, value) in [
        (1_000_000_010, 1.0),
        (1_000_000_020, 2.0),
        (1_000_000_030, 3.0),
    ] {
        rondi::update_rrd_file(&rondi_path, timestamp, Some(value)).unwrap();
        let update = Command::new("rrdtool")
            .args([
                "update",
                oracle_path.to_str().unwrap(),
                &format!("{timestamp}:{value}"),
            ])
            .output()
            .unwrap();
        assert!(
            update.status.success(),
            "{}",
            String::from_utf8_lossy(&update.stderr)
        );
        assert_eq!(
            std::fs::read(&rondi_path).unwrap(),
            std::fs::read(&oracle_path).unwrap(),
            "archive state diverged at {timestamp}"
        );
    }
}

fn create_single_source(path: &std::path::Path, start: i64, step: u64, ds: &str, rra: &str) {
    rondi::create_rrd_file(
        path,
        start,
        step,
        &[ds.to_owned()],
        &[rra.to_owned()],
        false,
    )
    .unwrap();
}

fn fetched_value(path: &std::path::Path, cf: &str, timestamp: i64, step: u64) -> Option<f64> {
    let fetched =
        rondi::fetch_rrd_file(path, cf, timestamp - step as i64, timestamp - 1, step).unwrap();
    fetched
        .rows
        .iter()
        .find(|row| row.timestamp == timestamp)
        .unwrap()
        .values[0]
}

// Expected values in the tests below come from RRDtool 1.11.0 `update` and
// `dump` on the same inputs.
#[test]
fn split_open_pdp_is_unknown_when_more_than_half_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("split.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:30:U:U",
        "RRA:AVERAGE:0.5:1:10",
    );
    rondi::update_rrd_file(&path, 1_000_000_006, None).unwrap();
    rondi::update_rrd_file(&path, 1_000_000_008, Some(5.0)).unwrap();
    rondi::update_rrd_file(&path, 1_000_000_035, Some(7.0)).unwrap();
    assert_eq!(fetched_value(&path, "AVERAGE", 1_000_000_010, 10), None);
    assert_eq!(
        fetched_value(&path, "AVERAGE", 1_000_000_020, 10),
        Some(7.0)
    );
}

#[test]
fn split_open_pdp_truncates_fractional_seconds_like_rrdtool() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fraction.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:100:U:U",
        "RRA:AVERAGE:0.5:1:10",
    );
    rondi::update_rrd_file_precise(&path, 1_000_000_005, 500_000, Some(10.0)).unwrap();
    rondi::update_rrd_file(&path, 1_000_000_025, Some(20.0)).unwrap();
    assert_eq!(
        fetched_value(&path, "AVERAGE", 1_000_000_010, 10),
        Some(13.5)
    );
    assert_eq!(
        fetched_value(&path, "AVERAGE", 1_000_000_020, 10),
        Some(21.0)
    );
}

#[test]
fn last_archive_carries_the_pdp_into_the_next_cdp() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("last.rrd");
    create_single_source(
        &path,
        1_000_000_440,
        10,
        "DS:x:GAUGE:100:U:U",
        "RRA:LAST:0.9:5:2",
    );
    rondi::update_rrd_file(&path, 1_000_000_470, Some(1057.0)).unwrap();
    let info = rondi::inspect_rrd_file(&path).unwrap();
    assert_eq!(info.archives[0].cdp_prep[0].value, 1057.0);
}

#[test]
fn verbose_update_reports_rrdtool_row_times() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("verbose.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:100:U:U",
        "RRA:AVERAGE:0.5:1:10",
    );
    rondi::update_rrd_raw_values_verbose(&path, 1_000_000_005, &[Some("1")]).unwrap();
    let rows = rondi::update_rrd_raw_values_verbose(&path, 1_000_000_045, &[Some("2")]).unwrap();
    // write_to_rras derives each row time from a step count it decrements
    // while writing, so RRDtool reports 40 and 50 rather than 30 and 40.
    assert_eq!(
        rows.iter()
            .map(|row| (row.timestamp, row.values[0]))
            .collect::<Vec<_>>(),
        [
            (1_000_000_010, 1.5),
            (1_000_000_020, 2.0),
            (1_000_000_040, 2.0),
            (1_000_000_050, 2.0)
        ]
    );
}

#[test]
fn verbose_update_parses_numbers_like_plain_update() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("plain.rrd");
    let verbose = temp.path().join("verbose.rrd");
    create_single_source(
        &plain,
        1_000_000_000,
        10,
        "DS:x:GAUGE:20:U:U",
        "RRA:LAST:0:1:5",
    );
    std::fs::copy(&plain, &verbose).unwrap();
    // rrd_strtodbl and str::parse round this decimal to different doubles.
    rondi::update_rrd_raw_values(&plain, 1_000_000_010, &[Some("1234567.891")]).unwrap();
    rondi::update_rrd_raw_values_verbose(&verbose, 1_000_000_010, &[Some("1234567.891")]).unwrap();
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&verbose).unwrap()
    );
}

#[test]
fn infinite_ds_bounds_survive_inspect_dump_and_restore() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bounds.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:20:-inf:inf",
        "RRA:LAST:0:1:5",
    );
    let info = rondi::inspect_rrd_file(&path).unwrap();
    assert_eq!(info.data_sources[0].minimum, Some(f64::NEG_INFINITY));
    assert_eq!(info.data_sources[0].maximum, Some(f64::INFINITY));
    let xml = rondi::dump_rrd_file(&path).unwrap();
    assert!(xml.contains("<min>-inf</min>"), "{xml}");
    assert!(xml.contains("<max>inf</max>"), "{xml}");
    let restored = temp.path().join("restored.rrd");
    rondi::restore_rrd_file(&xml, &restored, false, false).unwrap();
    let info = rondi::inspect_rrd_file(&restored).unwrap();
    assert_eq!(info.data_sources[0].minimum, Some(f64::NEG_INFINITY));
    assert_eq!(info.data_sources[0].maximum, Some(f64::INFINITY));
}

#[test]
fn create_parses_ds_bounds_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        eprintln!("skipping DS bound differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for bounds in [
        "U:U",
        "nan:-nan",
        "NaN:INF",
        "-inf:inf",
        "infinity:U",
        "U:1e400",
        "-1e400:0",
        "5:5",
        "6:5",
        "1x:U",
        "+inf:U",
    ] {
        let definition = format!("DS:x:GAUGE:20:{bounds}");
        let ours = temp.path().join("ours.rrd");
        let oracle = temp.path().join("oracle.rrd");
        let _ = std::fs::remove_file(&ours);
        let _ = std::fs::remove_file(&oracle);
        let created = rondi::create_rrd_file(
            &ours,
            1_000_000_000,
            10,
            std::slice::from_ref(&definition),
            &["RRA:LAST:0:1:5".to_owned()],
            false,
        );
        let upstream = Command::new("rrdtool")
            .args([
                "create",
                oracle.to_str().unwrap(),
                "--start",
                "1000000000",
                "--step",
                "10",
                &definition,
                "RRA:LAST:0:1:5",
            ])
            .output()
            .unwrap();
        assert_eq!(
            created.is_ok(),
            upstream.status.success(),
            "{bounds}: {created:?} {}",
            String::from_utf8_lossy(&upstream.stderr)
        );
        if created.is_ok() {
            // The DS definition holds the bounds; cur_row is random upstream.
            assert_eq!(
                std::fs::read(&ours).unwrap()[128..248],
                std::fs::read(&oracle).unwrap()[128..248],
                "{bounds}"
            );
        }
    }
}

#[test]
fn trailing_bytes_after_the_archives_are_accepted_like_rrd_open() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("trailing.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:20:U:U",
        "RRA:LAST:0:1:5",
    );
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"XXXXXXXX");
    std::fs::write(&path, &bytes).unwrap();
    rondi::inspect_rrd_file(&path).unwrap();
    rondi::update_rrd_file(&path, 1_000_000_010, Some(3.0)).unwrap();
    assert_eq!(fetched_value(&path, "LAST", 1_000_000_010, 10), Some(3.0));
    assert!(std::fs::read(&path).unwrap().ends_with(b"XXXXXXXX"));
}

#[test]
fn extreme_fetch_ranges_return_errors_instead_of_overflowing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("range.rrd");
    create_single_source(
        &path,
        1_000_000_000,
        10,
        "DS:x:GAUGE:20:U:U",
        "RRA:LAST:0:1:5",
    );
    for (start, end) in [
        (-9_000_000_000_000_000_000, 9_000_000_000_000_000_000),
        (i64::MIN, i64::MIN + 10),
    ] {
        let result =
            std::panic::catch_unwind(|| rondi::fetch_rrd_file(&path, "LAST", start, end, 10));
        assert!(matches!(result, Ok(Err(_))), "{start}..{end}: {result:?}");
    }
}

#[test]
fn dderive_previous_sample_is_parsed_like_rrdtool() {
    if !Command::new("rrdtool")
        .arg("--version")
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1.11.0")
        })
    {
        eprintln!("skipping DDERIVE differential: pinned RRDtool 1.11.0 is not installed");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let oracle = temp.path().join("oracle.rrd");
    let ours = temp.path().join("ours.rrd");
    let created = Command::new("rrdtool")
        .args([
            "create",
            oracle.to_str().unwrap(),
            "--start",
            "1000000000",
            "--step",
            "10",
            "DS:x:DDERIVE:100:U:U",
            "RRA:LAST:0:1:5",
        ])
        .output()
        .unwrap();
    assert!(created.status.success());
    std::fs::copy(&oracle, &ours).unwrap();
    // str::parse and rrd_strtod round "444.636" to neighbouring doubles.
    for (timestamp, value) in [(1_000_000_005, "444.636"), (1_000_000_010, "43")] {
        let updated = Command::new("rrdtool")
            .args([
                "update",
                oracle.to_str().unwrap(),
                &format!("{timestamp}:{value}"),
            ])
            .output()
            .unwrap();
        assert!(updated.status.success());
        rondi::update_rrd_raw_values(&ours, timestamp, &[Some(value)]).unwrap();
    }
    assert_eq!(
        std::fs::read(&ours).unwrap(),
        std::fs::read(&oracle).unwrap()
    );
}
