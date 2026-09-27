# Workload benchmark

The checked-in benchmark runs `mise exec -- cargo bench -p rondi --bench workload`. It performs 300 local updates into a 120-row archive, then fetches the archive 50 times. It measures the current implementation, including a complete JSON checkpoint rewrite, `sync_all`, atomic rename, and directory sync on every update.

Recorded 2026-09-25 on macOS 15.6, Apple Mac15,10 / M3 Max (arm64, 14 logical CPUs), Rust 1.98.0. One local optimized run:

| Operation | Count | Elapsed | Throughput |
| --- | ---: | ---: | ---: |
| Update | 300 | 2,615,120 µs | 114.7 updates/s |
| Fetch | 50 | 1,065 µs | 46,948.4 fetches/s |

This is a small local baseline, not a performance claim. Every update rewrites the bounded JSON archive, making its cost grow with retained rows. A log-structured or page-based `.rondi` format should be evaluated before production workloads. This measurement does not include network clients, the daemon write queue, or RRDtool.

Container run recorded 2026-09-26 UTC in Docker 29.4.0 on OrbStack, Debian 13 (trixie) arm64, Rust 1.98.0:

| Operation | Count | Elapsed | Throughput |
| --- | ---: | ---: | ---: |
| Update | 300 | 730,394 µs | 410.7 updates/s |
| Fetch | 50 | 395 µs | 126,478.2 fetches/s |

This run used the container's writable layer. It is an independent environment observation and is not directly comparable to the macOS filesystem run above.
