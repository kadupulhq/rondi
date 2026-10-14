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

## Native RRD query baselines

`mise exec -- cargo bench -p rondi --bench rrd_queries --locked` runs Criterion over two fixtures with four GAUGE sources and one AVERAGE archive of 120 or 7,200 rows. Each fixture holds known value blocks and has wrapped its ring. Creation and durable updates happen before timing. `rrd_fetch` times `fetch_rrd_file` over the retained rows; `graph_calc` times the graph engine's script parsing, `data_fetch`, `data_calc`, and `xport` for four DEFs, one CDEF (arithmetic `x0,x1,+,x2,+,x3,+,4,/` or sequential `x0,PREV,ADDNAN`), and a MAXIMUM VDEF. Returned data and its disposal are timed. Each workload warms up for one second and collects 50 samples over at least three seconds; intervals use Criterion's default 95% level. Save a baseline with `-- --save-baseline <name>` and compare a later run on the same quiet host with `-- --baseline <name>`. Reports land under `target/criterion`.

Recorded 2026-10-14 on macOS 27.0.1, Mac15,10 / M3 Max (arm64, 14 logical CPUs), Rust 1.98.0, after merging `main` into the compatibility branch:

| Workload | Time estimate | 95% confidence interval |
| --- | ---: | ---: |
| Fetch, 120 rows | 20.269 µs | 19.817–20.783 µs |
| Fetch, 7,200 rows | 159.95 µs | 159.19–160.79 µs |
| Arithmetic graph, 120 rows | 94.872 µs | 93.682–96.220 µs |
| PREV graph, 120 rows | 92.560 µs | 90.461–95.629 µs |
| Arithmetic graph, 7,200 rows | 968.22 µs | 959.02–978.16 µs |
| PREV graph, 7,200 rows | 839.33 µs | 833.67–844.84 µs |

This is one cached-file, single-request run on a shared development machine. It excludes PNG rendering, CLI startup, daemon traffic, and concurrent graphs, and is not a capacity claim. The `rrd_workload` benchmark separately times single and batched updates and 1,440-row fetches.

### Historical: pre-merge engine measurements

The measurements below were recorded on the compatibility branch before `main` was merged. They measured a per-row buffered fetch loop and the shared-grid xport engine. `main` has since replaced both: fetch reads the archive's circular run in at most two reads, and graph calculation runs the `rrd_graph.c` port in `graph.rs`. The numbers are kept as a record and do not describe the current code.

The native `.rrd` fetch loop then sought and read each row separately. A five-second macOS `sample` profile attributed 3,445 of 3,577 samples (96%) to `read` and `lseek`. A 64 KiB read buffer that sought initially and at the ring boundary changed a four-source, 7,200-row fetch of 7,201 rows from 220.9 to 5,058.9 fetches/s over 200 fetches (905,386 µs to 39,534 µs), recorded 2026-10-13 on the macOS host above.

Graph calculation through the shared-grid engine split each CDEF into tokens and allocated a new stack for every row. Splitting once per CDEF and reusing the stack changed the median of three 1,000-calculation runs from 2,866,374 µs to 1,852,547 µs. The disposable Linux ARM64 `rondi-ci-core` container also completed that benchmark with Rust 1.98.0 and RRDtool 1.11.0: 300 RRD updates in 191,000 µs, 200 fetches in 45,034 µs, and 200 graph calculations in 338,417 µs. The complete 194-test workspace suite passed on both hosts after the optimizations.

The first Criterion baseline on that engine, recorded 2026-10-13 on the macOS host above, estimated 18.374 µs and 193.47 µs for 120-row and 7,200-row fetches, 97.435 µs and 1.7361 ms for the arithmetic graph, and 89.477 µs and 1.2985 ms for the PREV graph.
