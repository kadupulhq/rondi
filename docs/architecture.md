# Architecture and storage format decision

The workspace has three packages: `rondi` owns data-source semantics and storage, `rondi-cli` produces the single `rondi` executable for local and daemon commands, and `rondi-server` hosts the HTTP API and the partial `rrdcached` adapter. GraphQL, graph rendering, and FFI are outside the core dependency graph. The library exposes RRDtool 1.11.0 VDEF aggregate calculations, and graph/graphv can print VDEF numeric results. The CLI handles XML/JSON export for XPORT and basic LINE/AREA series, plus numeric PRINT/GPRINT reductions for AVERAGE/MIN/MAX/LAST. It includes a native PNG renderer for basic LINE/AREA/TICK/HRULE/VRULE elements, cumulative STACK values and AREA baselines, explicit colors and alpha, approximate AREA gradients, plot dimensions, axes, tick labels, title, vertical label, a simple legend, and basic lower/upper bounds. RRDtool PNG canvas dimensions match for tested graph layouts. Text metrics, grid scaling, pixel output, and broad graph-definition behavior are not yet compatible. Complex stacked sequences, pixel parity, and full format-string support remain incomplete. Symlink invocation selects `rrdtool` and `rrdcached` modes from `argv[0]`; both currently implement only documented subsets. The `rrdtool-proxy.php` alias matches the pinned help/version responses when executed directly; the PHP-script launcher and daemon protocol are unsupported.

## First format decision

The first format is versioned JSON with the distinct `.rondi` extension. It records the format version, step, heartbeat, retention bound, last update, pending PDP accumulator, and retained AVERAGE points. A write serializes to a temporary file, syncs it, atomically renames it, and syncs the storage directory. This keeps the first format inspectable and easy to validate while semantics are under differential test. It is not an `.rrd` file and must never be named `.rrd`.

The `.rondi` store is an additional native format. It is not a conversion of RRDtool's binary format. Rondi can create, inspect, fetch, and update the common version 0003-0005 RRDtool layout on 64-bit little-endian targets for GAUGE, COUNTER, DERIVE, ABSOLUTE, DCOUNTER, and DDERIVE sources with AVERAGE/MIN/MAX/LAST archives, including multi-PDP consolidation. Counter-family values use the current `f64` update API. These operations use POSIX `fcntl` locks; tests verify byte equality for the supported update subset and upstream continuation. Specialized v4/v5 features and other ABIs remain unverified. Never label a `.rondi` file `.rrd`.

## Ownership and processing

Opening a storage root takes an OS advisory exclusive lock on `.rondi.lock`. The daemon holds it for its lifetime. Local CLI invocations acquire the same lock, so they fail while the daemon owns the root. A crash releases the OS lock.

Each GAUGE update is assigned to fixed step intervals. Values are weighted by elapsed seconds. A sample gap greater than heartbeat, or an explicit unknown value, contributes unknown time. A PDP is unknown when less than half of its seconds are known. The last incomplete step is kept as accumulator state and is not returned by fetch until its boundary is complete. Out-of-order and duplicate timestamps are rejected by local update calls.

The daemon uses a bounded write channel. Full queues fail immediately with HTTP 429; clients should retry. Server updates append a journal record and sync it before checkpointing storage. The HTTP success response is sent only after both the journal and storage checkpoint have been synced. Startup replays journal records that are newer than each database checkpoint.

## Store limits and journal compaction

These limits apply only to the `.rondi` store and its HTTP API. They do not touch `.rrd` files, the `rrdtool` alias, or the `rrdcached` adapter.

Each `.rondi` update rewrites and syncs the whole JSON file, so update cost grows with retained rows. One optimized local run measured 114.7 updates/s for 300 updates and 120 retained rows on the documented Mac environment. `--max-rows` (default 100000, must be positive) caps `rows` for `create` and snapshot import to bound that cost and the work a long gap can cause.

The journal records each server update with its request ID and acceptance time. After startup recovery, and whenever the journal reaches twice the records kept by the last compaction (at least 1024), the store rewrites it without records that are both applied to their database and older than `--idempotency-window` (default 86400 seconds, must be positive). The rewrite goes to `rondi.journal.compact.tmp`, which is synced, renamed over `rondi.journal`, and followed by a directory sync; a crash leaves the old or the new journal, and a leftover temporary file is ignored and later overwritten. Records not yet applied are always kept for replay.

The trade-off: a retry with the same request ID is deduplicated for at least the window. After the window its record may be gone, and the retry is handled as a new update: the same timestamp is rejected as out of order, and different contents are accepted instead of being reported as a request ID conflict. Journal size and the in-memory request ID index are bounded by the window times the update rate, plus at most one doubling before the next compaction. Records written before this change carry no acceptance time and are dropped by the first compaction once applied.

## Format migration roadmap

1. Continue expanding differential tests for `.rrd` creation, update, fetch, and every exposed command; the common v3-v5 64-bit little-endian codec and basic DS/CF subset already work.
2. Add unsupported RRDtool data-source and archive semantics, specialized v4/v5 layouts, additional ABIs, remaining tune options, and resize/modify behavior.
3. Expand dump/restore beyond the basic tested subset, then implement remaining import/export commands and specialized XML fields.
4. Preserve complete RRDtool CLI parsing, stdin, output, error, environment, and exit behavior; compare commands against the pinned executable.
5. Add graph expression calculations and output formats needed by existing Cacti definitions.
6. Implement the pinned rrdcached and Cacti RRDProxy protocols, connection behavior, security, config, and service lifecycle in isolated adapters.
7. Build and verify the `librrd`-compatible C ABI for discovered native consumers; the standalone executable cannot replace dynamically linked libraries.

The `rrdtool` compatibility alias updates existing `.rrd` files in place under RRDtool-compatible file locking. Its supported subset is not yet a drop-in replacement; use disposable copies until the compatibility matrix and the workflows you depend on have passed.
