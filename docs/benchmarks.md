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

## Native RRD fetch profiling

The native `.rrd` fetch loop previously sought and read each row separately. A five-second macOS `sample` profile attributed 3,445 of 3,577 samples (96%) to `read` and `lseek`. The loop now uses a 64 KiB read buffer, reuses its row scratch space, and seeks initially and at the ring boundary, following `rrd_fetch.c`. Shared advisory locking, archive selection, padding, and returned values remain unchanged.

Recorded 2026-10-13 on macOS 27.0.1, Mac15,10 (arm64, 14 logical CPUs), Rust 1.98.0, optimized benchmark build. The fixture has four GAUGE sources, one 7,200-row AVERAGE archive, and 300 persisted updates. Each fetch returns 7,201 rows, including the final unknown row. Most retained rows are unknown; the archive fits in the filesystem cache.

| Measurement | Fetches | Elapsed | Fetches/s |
| --- | ---: | ---: | ---: |
| Before buffering | 200 | 905,386 µs | 220.9 |
| After buffering | 200 | 39,534 µs | 5,058.9 |
| Before buffering, longer run | 3,000 | 13,634,457 µs | 220.0 |
| After buffering, repeat 1 | 3,000 | 597,746 µs | 5,018.9 |
| After buffering, repeat 2 | 3,000 | 613,136 µs | 4,892.9 |
| After buffering, repeat 3 | 3,000 | 606,080 µs | 4,949.8 |

This workload improved by about 23 times. A second profile shifted the dominant cost to result allocation and disposal; `read` and `lseek` accounted for 230 of 3,746 samples (6%). Update sync boundaries were preserved. These are local cached-file measurements, with no concurrency, daemon traffic, or PNG rendering, and do not establish production capacity or a general graph speedup.

Run `mise exec -- cargo bench -p rondi --bench rrd_workload`. Set `RONDI_BENCH_FETCHES=3000` for a longer fetch run. For sampling, increase it to 30000 and run `sample <printed-pid> 5 1 -file /tmp/rondi-fetch-profile.txt` from another terminal during the fetch phase. The benchmark also measures four DEFs, one arithmetic CDEF, and one MAXIMUM VDEF through the graph calculation/export path; `RONDI_BENCH_GRAPHS` controls those iterations.

A differential test uses four sources and a 5,000-row archive to cross both the 64 KiB buffer boundary and the ring boundary, with distinct values, historical/future padding, and an entirely future range. It compares every timestamp and value to RRDtool 1.11.0 and verifies that fetching leaves the file unchanged.

## Graph calculation profiling

The same fixture measures the graph calculation/export path with four DEFs, arithmetic CDEF `x0,x1,+,x2,+,x3,+,4,/`, and a MAXIMUM VDEF. Each result contains 7,200 rows. A five-second profile showed allocation and reallocation below `evaluate_rpn`: every row split the expression into tokens and allocated a new stack. The evaluator now splits once per CDEF and clears a reusable stack between rows, following RRDtool's parsed program and reusable `rpnstack` model.

The saved baseline executable already includes buffered fetching, so these paired measurements isolate the CDEF change. Each run performs 1,000 graph calculations; the environment is the macOS host above.

| Repeat | Before | After |
| --- | ---: | ---: |
| 1 | 3,014,143 µs | 1,852,547 µs |
| 2 | 2,866,374 µs | 1,870,363 µs |
| 3 | 2,714,195 µs | 1,702,815 µs |
| Median | 2,866,374 µs | 1,852,547 µs |

The median improved by about 1.55 times, or 35% less elapsed time. A second profile confirmed that the repeated token/stack allocation disappeared; result allocation, resampling, variable lookups, and arithmetic remain. These runs share a development machine with other activity and are observations rather than a performance gate. They measure calculation and export data, excluding PNG rasterization, text layout, CLI startup, and network traffic. Existing differential tests cover the supported RPN operators, PREV, prediction, mixed grids, scalar VDEFs, and SHIFT after stack reuse.

Set `RONDI_BENCH_GRAPHS=1000` for the paired workload or `RONDI_BENCH_GRAPHS=6000` for a sampling run. Sample the printed PID during the `graph_calc` phase.

The disposable Linux ARM64 `rondi-ci-core` container also completed the benchmark with Rust 1.98.0 and RRDtool 1.11.0: 300 RRD updates in 191,000 µs, 200 fetches in 45,034 µs, and 200 graph calculations in 338,417 µs. This is a separate environment observation with no Linux before/after baseline. The complete 194-test workspace suite passed on both hosts after the optimizations.

## Criterion baselines

`mise exec -- cargo bench -p rondi --bench rrd_queries --locked -- --save-baseline buffered-rpn-reuse` records statistical baselines. After a change, compare on the same quiet host with `mise exec -- cargo bench -p rondi --bench rrd_queries --locked -- --baseline buffered-rpn-reuse`. Add `graph_calc/arithmetic/7200` to select one workload. Keep baseline artifacts under `target/criterion`; the combined HTML report is `target/criterion/report/index.html`. The simple `rrd_workload` profiler remains available.

The suite measures four-source fetches at 120 and 7,200 retained rows, then graph calculations with arithmetic or sequential PREV CDEFs and a MAXIMUM VDEF. Fixtures contain known value blocks and wrapped ring buffers. Creation and durable updates happen before timing; returned data and its disposal are timed. Each workload warms up for one second and collects 50 samples over at least three seconds. Confidence intervals use Criterion's default 95% level. This is a cached-file, single-request workload, excluding PNG rendering, CLI startup, daemon traffic, and concurrent graphs.

First baseline recorded 2026-10-13 local time on the macOS/Rust host above:

| Workload | Time estimate | 95% confidence interval |
| --- | ---: | ---: |
| Fetch, 120 rows | 18.374 µs | 18.350–18.403 µs |
| Fetch, 7,200 rows | 193.47 µs | 191.15–195.77 µs |
| Arithmetic graph, 120 rows | 97.435 µs | 97.123–97.850 µs |
| PREV graph, 120 rows | 89.477 µs | 88.926–90.258 µs |
| Arithmetic graph, 7,200 rows | 1.7361 ms | 1.7332–1.7387 ms |
| PREV graph, 7,200 rows | 1.2985 ms | 1.2970–1.3004 ms |

An unchanged-code arithmetic repeat first reported a 6.5% slowdown during other development activity. A quieter repeat estimated 1.7383 ms and reported a change within the default 1% noise threshold. Preserve both observations: statistical significance alone does not establish a code regression on a shared machine. Compare repeated runs under equivalent conditions before making optimization decisions.

Criterion 0.7 is a development dependency with Rust 1.80 as its declared minimum, preserving this workspace's Rust 1.85 requirement. Its Rayon feature is disabled. No runtime thread pool or memory mapping is introduced. See the [Criterion guide](https://bheisler.github.io/criterion.rs/book/) for baseline and profiling options. These measurements establish a baseline; the earlier manual before/after measurements are not retroactively Criterion comparisons.
