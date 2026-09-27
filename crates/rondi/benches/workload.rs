use rondi::{DatabaseConfig, Store, Update};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    const UPDATES: usize = 300;
    const FETCHES: usize = 50;
    const RETAINED_ROWS: usize = 120;
    let root = std::env::temp_dir().join(format!(
        "rondi-bench-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Store::open(&root).unwrap();
    let start = 1_700_000_000_i64;
    store
        .create(
            "load",
            DatabaseConfig {
                step: 1,
                heartbeat: 2,
                rows: RETAINED_ROWS,
                start,
            },
        )
        .unwrap();
    let update_started = Instant::now();
    for index in 1..=UPDATES {
        store
            .update(
                "load",
                Update {
                    timestamp: start + index as i64,
                    value: Some((index % 100) as f64),
                },
            )
            .unwrap();
    }
    let update_elapsed = update_started.elapsed();
    let fetch_started = Instant::now();
    for _ in 0..FETCHES {
        std::hint::black_box(store.fetch("load").unwrap());
    }
    let fetch_elapsed = fetch_started.elapsed();
    report("update", UPDATES, update_elapsed);
    report("fetch", FETCHES, fetch_elapsed);
    println!("retained_rows={RETAINED_ROWS}");
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

fn report(label: &str, operations: usize, elapsed: Duration) {
    println!(
        "{label}: operations={operations} elapsed_us={} ops_per_sec={:.1}",
        elapsed.as_micros(),
        operations as f64 / elapsed.as_secs_f64()
    );
}
