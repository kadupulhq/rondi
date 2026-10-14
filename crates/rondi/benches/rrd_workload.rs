use rondi::{
    RrdRawUpdate, create_rrd_file, fetch_rrd_file, update_rrd_raw_batch,
    update_rrd_raw_values_precise,
};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UPDATES: usize = 20_000;
const FETCHES: usize = 1_000;
const START: i64 = 1_700_000_000;

fn main() {
    let root = std::env::temp_dir().join(format!(
        "rondi-rrd-bench-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let values = (1..=UPDATES)
        .map(|index| (index % 97).to_string())
        .collect::<Vec<_>>();

    let single = root.join("single.rrd");
    create(&single);
    let started = Instant::now();
    for (index, value) in values.iter().enumerate() {
        update_rrd_raw_values_precise(&single, START + index as i64 + 1, 0, &[Some(value)])
            .unwrap();
    }
    report("update_single", UPDATES, started.elapsed());

    let batch = root.join("batch.rrd");
    create(&batch);
    let updates = values
        .iter()
        .enumerate()
        .map(|(index, value)| RrdRawUpdate {
            timestamp: START + index as i64 + 1,
            timestamp_usec: 0,
            values: vec![Some(value.as_str())],
        })
        .collect::<Vec<_>>();
    let started = Instant::now();
    update_rrd_raw_batch(&batch, &updates, false).unwrap();
    report("update_batch", UPDATES, started.elapsed());
    assert_eq!(
        std::fs::read(&single).unwrap(),
        std::fs::read(&batch).unwrap()
    );

    let end = START + UPDATES as i64;
    let started = Instant::now();
    for _ in 0..FETCHES {
        std::hint::black_box(fetch_rrd_file(&single, "AVERAGE", end - 1440, end, 1).unwrap());
    }
    report("fetch_1440_rows", FETCHES, started.elapsed());
    std::fs::remove_dir_all(root).unwrap();
}

fn create(path: &Path) {
    create_rrd_file(
        path,
        START,
        1,
        &["DS:x:GAUGE:600:U:U".to_owned()],
        &[
            "RRA:AVERAGE:0.5:1:1440".to_owned(),
            "RRA:MAX:0.5:60:1440".to_owned(),
            "RRA:AVERAGE:0.5:3600:720".to_owned(),
        ],
        false,
    )
    .unwrap();
}

fn report(label: &str, operations: usize, elapsed: Duration) {
    println!(
        "{label}: operations={operations} elapsed_us={} ops_per_sec={:.1}",
        elapsed.as_micros(),
        operations as f64 / elapsed.as_secs_f64()
    );
}
