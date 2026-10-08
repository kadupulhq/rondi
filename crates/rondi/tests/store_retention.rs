//! Found by the cargo-fuzz `http_api` target.

use rondi::{DatabaseConfig, Store, Update};
use std::time::{Duration, Instant};

/// Retention trims with `Vec::remove(0)` once per appended
/// point, so a gap just under `rows` steps costs O(rows^2) element moves
/// while the store mutation lock is held (about 7 s at 100,000 rows).
#[test]
fn large_gap_update_is_linear_in_rows() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let rows = 100_000;
    store
        .create(
            "q",
            DatabaseConfig {
                step: 1,
                heartbeat: 1_000_000,
                rows,
                start: 1_000_000_000,
            },
        )
        .unwrap();
    let fill = Update {
        timestamp: 1_000_000_000 + rows as i64,
        value: Some(1.0),
    };
    store.update("q", fill).unwrap();
    let started = Instant::now();
    let gap = Update {
        timestamp: 1_000_000_000 + 2 * rows as i64 - 1,
        value: Some(2.0),
    };
    store.update("q", gap).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "one update took {:?}",
        started.elapsed()
    );
    let points = store.fetch("q").unwrap().points;
    assert_eq!(points.len(), rows);
    assert_eq!(
        points.first().unwrap().timestamp,
        1_000_000_000 + rows as i64
    );
    assert_eq!(points.last().unwrap().value, Some(2.0));
}
