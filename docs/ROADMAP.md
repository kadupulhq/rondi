# Roadmap: drop-in RRDtool 1.11.0 compatibility

Rondi is not a drop-in replacement today. This page lists what is left, in what order, how long it should take, and how each step is checked. The individual behavior differences are tracked in [COMPATIBILITY_DEFECTS.md](COMPATIBILITY_DEFECTS.md) (RD-001 to RD-015). This page does not repeat them. Where a milestone closes a defect, the ID is named.

## Goal and decisions

The goal is exact RRDtool 1.11.0 behavior on every surface Rondi claims to be compatible with. Where output depends on the host (fonts, libc, locale), the bar is the equivalence class defined in each section.

The maintainer has decided the following. The work is in progress.

- rrdcached writes no journal unless `-j` is given, and the journal uses upstream's format.
- Rondi adds no syncs beyond the ones upstream performs.
- rrdcached implements `-U`, `-G`, and multiple `-l` options as upstream does.
- After "Unable to connect to rrdcached", the client prints upstream's local-fallback text.
- All behavior differences listed in COMPATIBILITY_DEFECTS.md are closed. Documented exceptions are limited to behavior that depends on undefined memory (see Risks).
- Rondi is relicensed to match RRDtool: GPL-2.0-or-later with the RRDtool FLOSS exception. The license change merged in #45. Author audit, EXECUTED with `git log --format='%an <%ae>%n%(trailers:key=Signed-off-by,valueonly)'`: every human commit and sign-off is Thomas Vincent; the only other author is dependabot[bot], with two workflow SHA bumps (#33, #42).
- CI runs on GitHub-hosted runners.

## Method and labels

- Reference: `oetiker/rrdtool-1.x` tag `v1.11.0`. The tag object is `7a15cf6`; it points at commit `ddc7ca7`. Rondi pins the release tarball in `docker/Dockerfile.test` with its SHA-256.
- Local oracle for the probes below: Homebrew `rrdtool 1.11.0` on macOS arm64. This is not the pinned Linux build. Results from it are marked "macOS probe" and must be repeated in the CI container before anyone relies on them.
- Rondi under test: `main` at `de820e9` (release build for probes, `cargo test --workspace` for the suite).
- **EXECUTED** means the command was run against `main` for this page. **READ-ONLY** means the claim comes from reading code or docs. RRDtool claims name the source file at `v1.11.0`. Line numbers were dropped on purpose; they differ between the tag object and later checkouts.
- This page replaces an earlier draft written against an unmerged 29-PR stack. Every statement about Rondi was re-checked on `main`.

## Where Rondi stands

EXECUTED: `cargo test --workspace` on macOS arm64 (Rust from `.mise.toml`, Homebrew rrdtool 1.11.0): all 238 tests pass. The earlier draft reported 4 failures on macOS; none remain.

CI builds the pinned RRDtool oracle in `docker/Dockerfile.test` and runs the workspace tests inside it, on x86_64 (`ubuntu-latest`) and aarch64 (`ubuntu-24.04-arm`) (`.github/workflows/rust.yml`). Differential tests need the oracle, so a missing `rrdtool` must fail the job, not skip.

Closed since the earlier draft (all EXECUTED unless noted):

- `rrdtool` with no arguments, `-v`, `--version`, `version`, `help`, and a one-argument unknown name match upstream stdout, stderr, and exit status. `rrdtool bogus x` prints `ERROR: unknown function 'bogus'`, exit 1, like upstream.
- Pipe-mode `version` matches except the `OK u: s: r:` timing digits.
- `RRD_LOCKING` is read (`crates/rondi/src/rrd_binary.rs`). With another process holding the lock, `update` exits 1 with `ERROR: could not lock RRD` at once, like upstream.
- RPN number parsing, xport output, and graph output fixes (#37, #38); the graph and xport differential tests pass.
- rrdcached option parsing: `-G`, `-U`, `-V`, `-z`, `-L`, and `-s`, `-m`, `-P` validation messages (#36, #40). READ-ONLY for the message text; EXECUTED only that the options parse.
- Journal compaction at each flush interval (#40).
- Update math fixes and a seeded randomized update differential (#43).

## Surface: on-disk `.rrd` format (RD-001, RD-003, RD-004, RD-005)

### What RRDtool does

- Cookie `RRD`, versions 0003 to 0005 current; 0001 and 0002 are legacy (`src/rrd_format.h`, `src/rrd_open.c`). Versions above 5 are rejected.
- The header structs follow the C ABI of the host: `long` and `time_t` width, `double` alignment, byte order. A file from another architecture fails with `This RRD was created on another architecture`. Cross-architecture transfer is by dump and restore.
- Data source types: COUNTER, ABSOLUTE, GAUGE, DERIVE, COMPUTE, DCOUNTER, DDERIVE. Consolidation functions: AVERAGE, MIN, MAX, LAST, plus the Holt-Winters family HWPREDICT, MHWPREDICT, SEASONAL, DEVSEASONAL, DEVPREDICT, FAILURES (`src/rrd_hw*.c`).
- COMPUTE values are never supplied by the caller (`src/rrd_update.c`).
- The initial ring row is random (`src/rrd_open.c`).
- Writers `mmap` the file. No `fsync`, `fdatasync`, or `msync` appears in `rrd_open.c`, `rrd_update.c`, or `rrd_create.c` (EXECUTED earlier: `grep -n sync`).
- Locking is a whole-file `fcntl` lock. Default `try` (`F_SETLK`); `$RRD_LOCKING` and `update --locking` select `try`, `block`, or `none`.

### What Rondi does

- Reads and writes 0003 to 0005 only. EXECUTED: `grep -n '"000' crates/rondi/src/rrd_binary.rs` shows no 0001 or 0002 path.
- Requires a 64-bit little-endian target for create and inspect (`rrd_binary.rs`, `target_pointer_width` check). EXECUTED (grep).
- Supports GAUGE, COUNTER, DERIVE, ABSOLUTE, DCOUNTER, DDERIVE with AVERAGE, MIN, MAX, LAST.
- HWPREDICT and COMPUTE still fail. EXECUTED (macOS probe): files made by upstream `create` with `RRA:HWPREDICT:100:0.1:0.0035:10` and `DS:b:COMPUTE:a,2,*` give, from Rondi, `info`: `rrdtool info output for this data-source or consolidation type is not implemented`; `update`: `unsupported RRD operation: in-place update requires one value per supported data source and basic archives` (HW) and `expected 2 data source readings (got 1)` (COMPUTE); `dump`: unsupported. Rondi `create` rejects both definitions.
- Basic update parity holds. EXECUTED (macOS probe): 1,000 updates through `rrdtool -` into copies of one upstream-created file (step 10, AVERAGE 1x1000) gave byte-identical files (`cmp`).
- Locking is fail-fast by default and honors `RRD_LOCKING`. EXECUTED (macOS probe): a Python `fcntl.lockf` holder made both implementations exit 1 with `ERROR: could not lock RRD`. Not checked: `block` and `none` modes, and the `update --locking` option. EXECUTED: `grep -n '"--locking"' crates/*/src/*.rs` found no match.
- Durability and speed differ. Rondi still calls `sync_data` after each in-place update (`rrd_binary.rs`, READ-ONLY for the call site). EXECUTED (macOS probe, noisy host, two runs, 1,000 updates through `rrdtool -`): upstream 2.5 s and 1.7 s; Rondi 11.1 s and 15.6 s, so about 5 to 9 times slower. Files were identical. The earlier draft measured 24 times on a quiet host with 2,000 updates; re-measure in the benchmark harness below before quoting any number.
- `resize` accepts only v3 and v4 (`rrd_binary.rs`); `restore` accepts v3 and v5 (`rrd_binary.rs`). READ-ONLY for the upstream side of resize.
- Create picks a deterministic initial row. This is allowed because upstream's choice is random, so byte comparison on create covers structure, not `cur_row`.
- NaN text for daemon FETCH is chosen by `target_arch` (`rrd_binary.rs`). It passes on x86_64 and aarch64 Linux CI and on macOS arm64 locally. Other targets are untested.

### Gaps

1. Holt-Winters family: create, update, fetch, info, dump, restore, tune (`--alpha`, `--beta`, `--gamma`, `--gamma-deviation`, `--window-length`, `--failure-threshold`, `--deltapos`, `--deltaneg`, `--smoothing-window`, `--aberrant-reset`).
2. COMPUTE data sources. The RPN engine in `crates/rondi/src/xport.rs` can be reused.
3. Read support for versions 0001 and 0002.
4. Non-x86_64 and non-64-bit layouts (i386, armhf, ppc64el, s390x, aarch64). This needs a layout descriptor computed for the build target, not fixed offsets.
5. `update --locking`, and tests for the `block` and `none` modes.
6. No per-update sync by default, as decided. Provide stronger durability only as an explicit opt-in under a non-RRDtool name, or not at all.
7. `resize` for v5; the `tune` positional forms (`DEL:`, `DS:`, `RRA#`); `create --source` with several sources and mappings (RD-004).
8. A verified NaN table per target.

### Effort

13 to 17 engineer-weeks. Holt-Winters 5 to 7 (port about 1,500 lines of C state machines and their scratch layout, then difference each). Layout abstraction and multi-arch CI 3 to 4. COMPUTE 1. Legacy read 1. Modify, resize, and source handling 3 to 4. Locking tests and sync policy 1.

### Verification

- Differential: for every DS and CF combination, create one file with upstream, copy it, apply the same seeded update stream to both, and `cmp` after each batch. `crates/rondi/tests/rrdtool_differential.rs` has the generator; extend it to Holt-Winters and COMPUTE.
- Cross-continuation: Rondi writes N updates, upstream writes M more, and the reverse; compare with an all-upstream file.
- Golden files: small upstream-created files for each version and target layout (built under QEMU with the pinned tarball). Rondi must read every one where RRDtool can.
- Fuzzing: `cargo-fuzz` targets for the header parser, `fetch`, and restore XML. Assert no panic, bounded memory, and the same accept or reject decision as upstream on a corpus seeded from the golden files.
- Lock test: hold the lock from a C helper; assert exit 1 and the exact error text for `try`, blocking for `block`, success for `none`.

## Surface: command line (RD-002, RD-010, RD-014)

### What RRDtool does

- `src/rrd_tool.c` dispatches create, dump, info, updatev, list, version, restore, resize, last, lastupdate, first, update, fetch, xport, graph, graphv, tune, flushcached. An unknown name sets `unknown function '%s'`.
- Errors print `ERROR: <msg>` to stderr and exit 1. A bare `rrdtool` or a single argument prints usage and exits 0.
- Pipe mode `rrdtool - [dir]`: optional chroot, extra commands `quit`, `cd`, `pwd`, `mkdir`, `ls`, errors on stdout, success line `OK u:%1.2f s:%1.2f r:%1.2f`.
- The pipe tokenizer `CreateArgs` splits on space only, honors single and double quotes, has no backslash escape, and treats tab as an ordinary character.
- Separate binaries: `rrdtool`, `rrdupdate`, optional `rrdcgi`, optional `rrdcached`.
- Environment: `RRDCACHED_ADDRESS`, `RRDCACHED_STRIPPATH`, `RRD_LOCKING`, `RRD_DEFAULT_FONT`, plus `TZ` and `LC_*`.

### What Rondi does

- `argv[0]` dispatch for `rrdtool`, `rrdcached`, `rrdtool-proxy`, `rrdproxy`. `scripts/install-compat-links.sh` installs those four names only. No `rrdupdate` and no `rrdcgi`. EXECUTED (read script).
- Usage, version, and unknown-name handling match (see "Closed since the earlier draft"). EXECUTED with `cmp` on stdout and stderr and a check of the exit status.
- Pipe mode still lacks commands. EXECUTED: `pwd`, `ls`, `cd /tmp`, `xport`, `graph`, `graphv`, and `flushcached` each print `ERROR: unknown function '<name>'`; upstream runs them. The chroot argument is not implemented (READ-ONLY).
- The pipe tokenizer is still `shell_words::split` (`crates/rondi-cli/src/main.rs`). EXECUTED: input `info no\:such.rrd` prints `opening 'no:such.rrd'` from Rondi and `opening 'no\:such.rrd'` from upstream. This breaks `COMMENT` text with `\:` once graph commands reach pipe mode.
- Help text for 14 commands is stored under `crates/rondi-cli/src/help/` (dump, fetch, first, graph, graphv, info, lastupdate, list, resize, restore, tune, update, updatev, xport). EXECUTED (file listing). `create`, `last`, and `flushcached` help are not in that set; whether they match was not checked.
- `RRDCACHED_ADDRESS` is read in 13 places (EXECUTED: `grep -c`). `RRDCACHED_STRIPPATH` and `RRD_DEFAULT_FONT` are not read; `RRD_LOCKING` is.
- Upstream shell suite against Rondi (exploratory, macOS probe, 31 scripts from `tests/` at `v1.11.0`, run with `RRDTOOL=<binary>`). Upstream passed 15 on this host. Rondi passed 10 of those 15 and failed `graph1`, `graph2`, `graph4`, `rpn1`, and `vformatter1`. `rpn1` stops at a `graphv` call (EXECUTED: rerun with `bash -x`); the other four were not traced. The 16 scripts that fail with upstream on macOS (the `create-with-source-*`, `modify*`, `tune*`, `list1`, and `graph5` scripts, among others) say nothing about Rondi and must be rerun in the Linux container. The earlier draft listed `tune2` and `pdp-calc1` as Rondi failures; `pdp-calc1` now passes, and `tune2` fails for upstream too here.

### Gaps

1. Full pipe mode: all commands, `cd/pwd/mkdir/ls`, the chroot argument, the `CreateArgs` tokenizer.
2. `rrdupdate` name. `rrdcgi` only if a consumer needs it (see Roadmap).
3. Option completeness outside graph (RD-002, RD-004).
4. Upstream's local-fallback text after "Unable to connect to rrdcached" (decided).
5. `RRDCACHED_STRIPPATH`.
6. Error text corpus beyond the tested set (RD-010).
7. Time parser completeness. The parser is a subset with differential tests (READ-ONLY).
8. TCP paths for `xport`, `graphv`, and other daemon commands. TLS does not exist in RRDtool 1.11.0 (READ-ONLY) and should not be added under the compatibility names.

### Effort

9 to 12 engineer-weeks. Pipe mode and tokenizer 1. Option completeness outside graph 3. Error corpus 3 to 4. Time parser and fuzzing 2. `rrdupdate` 1 day.

### Verification

- Run the upstream `tests/` scripts in the pinned Linux container with `RRDTOOL` set to Rondi, as a CI job. Require every script that passes with upstream to pass with Rondi.
- Golden argv corpus (generated from option tables plus fuzz) with stdout, stderr, exit code, and output file hash recorded from upstream.
- Fuzz the pipe tokenizer against a C harness that links upstream's `CreateArgs`.
- Fuzz the time parser against `rrd_parsetime` from `librrd` with fixed `TZ` and a frozen clock.

## Surface: graphing (RD-006)

### What RRDtool does

- Image formats PNG, SVG, EPS, PDF; data formats XML, XMLENUM, CSV, TSV, SSV, JSON, JSONTIME.
- Elements: PRINT, GPRINT, COMMENT, HRULE, VRULE, LINE, AREA, STACK, TICK, TEXTALIGN, DEF, CDEF, VDEF, SHIFT, XPORT, XAXIS, YAXIS.
- Rendering uses Cairo and Pango, with a font list resolved at run time (`src/rrd_graph.c`, overridable by `RRD_DEFAULT_FONT`).
- `graphv` prints layout metadata: `graph_left`, `graph_top`, `graph_width`, `graph_height`, `image_width`, `image_height`, `value_min`, `value_max`, `legend[n]`, `coords[n]`.

### What Rondi does

- PNG through the `png` crate and a bitmap font; CSV, TSV and SSV through a port of `rrd_xport_format_sv`. `--imgformat SVG`, `PDF` and `EPS` exit 1 with `RRDtool graph format X is unsupported`.
- Elements parse through a port of `rrd_graph_script` (`crates/rondi/src/graph.rs`), so every element except XAXIS and YAXIS is accepted with upstream's grammar and error texts; data preparation and PRINT values follow `data_fetch`, `data_calc` and `print_calc` (`crates/rondi-cli/tests/graph_data_differential.rs`).
- Graph options fail rather than being ignored. EXECUTED: `--logarithmic`, `--slope-mode`, `--lazy`, `--right-axis`, `--watermark`, `--utc`, `--font`, `--pango-markup`, `--x-grid`, `--units-exponent`, `--no-gridfit`, `--zoom`, `--legend-position`, and `--dynamic-labels` each exit 1 with `unsupported graph option`. That is 14 of the roughly 64 options upstream accepts; the rest were not probed one by one.
- Image size still differs. EXECUTED (macOS probe, one `LINE1` over a short series): upstream 481x141, Rondi 481x139. The earlier draft saw equal sizes (481x155) on a different input; size parity is input dependent.
- `graphv` with a PNG target prints `graph_start`, `graph_end`, and `graph_step`. EXECUTED: `diff` against upstream shows upstream's `graph_left`, `graph_top`, `graph_width`, `graph_height`, `image_width`, `image_height`, `value_min`, and `value_max` missing from Rondi, and `graph_step` missing from upstream.
- PRINT and GPRINT formatting, SI scaling, and the 12 VDEF functions are covered by differential tests (`crates/rondi-cli/tests/graph_xport_parity.rs`, `vdef_differential.rs`).

### The bar

Pixel identity is a measured property inside one pinned container, not a promise across hosts. Upstream pixels depend on Cairo, Pango, FreeType, fontconfig, and the installed fonts, so two builds of 1.11.0 can disagree.

1. **Exact**: `graphv` keys and values, PRINT values, image width and height, data-format outputs (XML, JSON, CSV, TSV, SSV), exit status, for the full option and element grammar.
2. **Exact inside the pinned container**: PNG bytes, when Rondi renders through the same Cairo and Pango versions and fonts. Track the share of identical PNGs in a corpus; treat drops as regressions.
3. **Perceptual elsewhere**: SSIM of at least 0.98 per image against upstream on the same host, and identical text in SVG output.

Reaching tier 2 requires Cairo and Pango bindings; a pure-Rust rasterizer cannot meet it. Keep the renderer in its own crate so the storage library stays free of C dependencies, as `docs/architecture.md` intends.

### Gaps

XAXIS, YAXIS; SVG, EPS, PDF; the remaining graph options; grid and tick selection; `graphv` layout metadata; Pango text layout; Cairo strokes and gradients; `--lazy`; `-` as stdout target; graph commands in pipe mode.

### Effort

18 to 26 engineer-weeks. A port of `rrd_graph.c` (about 5,000 lines) and `rrd_graph_helper.c` onto Cairo and Pango is most of it. Budget 4 weeks for pixel work after metadata parity and stop when the corpus metric plateaus.

### Verification

- Corpus of graph definitions from upstream `tests/graph*`, `doc/rrdgraph_examples.pod`, and real Cacti and Munin template output, with argv and upstream `graphv` output stored as golden files.
- Compare `graphv` text exactly, PNG by hash in the pinned container and by SSIM elsewhere, SVG after stripping generated IDs.
- Fuzz the graph argument parser with `cargo-fuzz` for no-panic and for accept or reject parity with a C harness calling `rrd_graph_v`.

## Surface: RPN, CDEF, VDEF, time (RD-005)

RRDtool has 61 named RPN operators plus `+ - * / %` (counted from the `match_op` calls in `src/rrd_rpncalc.c` in the earlier review; not recounted) and 12 VDEF functions.

What Rondi does:

- `crates/rondi/src/rpn.rs` ports `rpn_parse` and `rpn_calc`, including the per-variable data cursors that mixed-resolution `PREV`, `TREND` and `PREDICT` read. Number parsing, `NEWWEEK`, `AVG`/`STDEV`, `PERCENT`/`SORT`, `NOW`, `INDEX`/`COUNT`, `COPY`/`ROLL`, `LIMIT`, `TREND` windows, and mixed-resolution xport were fixed in the merged stack (#1 to #43); their differential tests pass (EXECUTED: workspace tests).
- RPN is reachable from xport and graph, not from COMPUTE data sources.
- Percentile with infinities depends on libc `qsort` order. The test passes on Linux CI and on macOS arm64 here.

Gaps: COMPUTE wiring; the remaining mixed-resolution `PREDICT`, `TREND`, and `PREV` alignment cases; error text for malformed RPN; `qsort` order emulation if parity beyond glibc is wanted.

Effort: 3 to 5 engineer-weeks.

Verification: differential RPN fuzzing (random well-typed expressions over seeded data, compare `xport` JSON bit for bit); grammar-based fuzzing of the time parser against `rrd_parsetime` with `faketime` or an `LD_PRELOAD` clock.

## Surface: librrd C ABI (RD-009)

### What RRDtool does

- `src/librrd.sym` exports 97 symbols (EXECUTED: `wc -l`): 14 `_r` variants, 20 `rrdc_*` client functions, and the rest. There are no ELF symbol versions. `LIBVERS=11:0:3` gives soname `librrd.so.8` (soname arithmetic READ-ONLY).
- Public headers `rrd.h`, `rrd_format.h`, `rrd_client.h`. Exported `rrd_open`, `rrd_read`, `rrd_write`, and `rrd_seek` expose `rrd_t` and `rrd_file_t`, so the `rrd_format.h` layout is part of the ABI.
- `librrd.pc` reports `Name: librrd` and the package version.
- License: GPL-2.0-or-later with the FLOSS License Exception (`COPYRIGHT`).
- Upstream bindings (Perl, Python, Ruby, Lua) call: `rrd_create`, `rrd_update`, `rrd_update_v`, `rrd_fetch`, `rrd_fetch_cb_register`, `rrd_first`, `rrd_last`, `rrd_lastupdate_r`, `rrd_info`, `rrd_info_free`, `rrd_graph`, `rrd_graph_v`, `rrd_xport`, `rrd_dump`, `rrd_restore`, `rrd_resize`, `rrd_tune`, `rrd_flushcached`, `rrd_list`, `rrd_parsetime`, `rrd_proc_start_end`, `rrd_strversion`, `rrd_get_error`, `rrd_set_error`, `rrd_clear_error`, `rrd_test_error`, `rrd_freemem` (EXECUTED earlier by grep of the binding sources).

### What Rondi does

Nothing. EXECUTED: no `crate-type` in `crates/*/Cargo.toml`; no FFI, no headers. RD-009 records that the checked Kadupul tree uses the CLI only. That inventory does not cover PHP pecl `rrd`, collectd, Smokeping, Munin, Ganglia, or MRTG, which link `librrd` or its Perl bindings (READ-ONLY; not verified per consumer).

### Plan

- New crate `rondi-ffi` with `crate-type = ["cdylib", "staticlib"]`, soname `librrd.so.8` set by a linker argument. Generate headers with `cbindgen`, diff against upstream `rrd.h` and `rrd_client.h`, and fix by hand. Do not ship headers whose type layout differs from upstream.
- Export exactly the 97 symbols with a single anonymous version script (`{ global: ...; local: *; };`). A version node would stop binaries linked against Rondi from loading against upstream, which blocks rollback.
- Keep a per-thread error context with `rrd_get_context` semantics. `rrd_get_error` returns a pointer valid until the next call on that thread.
- Argc/argv entry points reuse the CLI option parser so behavior is identical, including `optind` reset.
- Allocate `rrd_info_t` lists and fetch buffers with `libc::malloc`, so `rrd_info_free` and `rrd_freemem` (which call `free`) work.
- `rrd_open` and `rrd_t` exposure sits on the layout descriptor from the format section. It is the hardest part; start with an error stub and inventory which consumers call it (`nm -D --undefined-only`).
- License: with the relicense to GPL-2.0-or-later plus the RRDtool FLOSS exception (#45; author audit above), a Rondi `librrd` can carry the same terms as upstream. This is not legal advice.

### Effort

10 to 14 engineer-weeks, after M1 and M2. Graph symbols wait for M5.

### Verification

- `nm -D --defined-only librrd.so.8 | sort` equals the 97-name list.
- A C file asserting `sizeof` and `offsetof` for every public struct against the upstream headers.
- Upstream binding test suites (Perl, Python, Ruby, Lua, Tcl) against the shim. Then pecl `rrd`, collectd's `rrdtool` and `rrdcached` plugins, and Smokeping with `LD_LIBRARY_PATH` set to the shim.
- Thread test: N threads calling `rrd_update_r` and `rrd_fetch_r` on distinct files with error injection; check per-thread errors.
- `abidiff` (libabigail) or `abi-compliance-checker` against upstream `librrd.so.8`.

## Surface: rrdcached (RD-007, RD-013, RD-014, RD-015)

### What RRDtool does

- 25 commands: UPDATE, WROTE, TUNE, DUMP, FLUSH, FLUSHALL, PENDING, FORGET, QUEUE, STATS, HELP, PING, BATCH, FETCH, FETCHBIN, INFO, FIRST, LAST, CREATE, LIST, SUSPEND, RESUME, SUSPENDALL, RESUMEALL, QUIT (`src/rrd_daemon.c`).
- Options `-a -B -b -F -f -g -G -h -j -L -l -m -O -o -P -p -R -s -t -U -V -w -z`.
- Listeners: Unix paths and TCP `host:port`, several `-l` allowed. systemd socket activation through `LISTEN_PID` and `LISTEN_FDS`. Without `-g` the daemon forks into the background.
- Journal: text lines `<cmd> <args>`, files `rrd.journal.%010d.%06d`, rotated at 1 GiB (`JOURNAL_MAX`), legacy names handled on replay. Written through stdio with no per-record `fsync`. It exists only when `-j` is given (decided; matches upstream).

### What Rondi does

- Handles the 25 commands (`crates/rondi-server/src/lib.rs`; READ-ONLY for the full list).
- Unix sockets only. EXECUTED (read): `-l` with a non-Unix address returns `rrdcached network listeners are not enabled; use a Unix socket`, and a second `-l` returns `rrdcached mode supports one listener; give -l only once` (`crates/rondi-cli/src/main.rs`).
- Parses all 23 upstream options. `-g` is accepted and ignored; `-F` controls the shutdown flush as upstream. `-U` and `-G` do not switch accounts: if the requested user or group differs from the current one, startup fails with `rrdcached -U/-G cannot switch accounts in Rondi` (READ-ONLY). No fork and no socket activation (EXECUTED: `grep -rn "LISTEN_FDS\|fork()\|daemonize" crates` is empty).
- Journal: upstream format and file layout, written only with `-j`, buffered like stdio with no sync, rotated each flush interval, and replayed at startup. Journals hand over between upstream and Rondi in both directions (`rrdcached_journal_replays_across_upstream_and_rondi`).
- Client side: Rondi `rrdtool` talks to upstream `rrdcached` over Unix and TCP (tests in `crates/rondi-cli/tests/server_roundtrip.rs`, run in CI). Upstream `rrdtool` talks to Rondi `rrdcached` over Unix only.
- Pending queue limit of 64 MiB (`DEFAULT_RRDCACHED_QUEUE_BYTES`), a deliberate deviation that rejects updates before journaling them. Under "exact behavior" it needs a decision: match upstream (no limit) or document it as the one allowed exception.

### Gaps

TCP listeners and several `-l` options; real `-U` and `-G` privilege drop; daemonizing without `-g`; socket activation; `-V` log levels through syslog; journal only with `-j`, in upstream's format, readable at start-up and rotated; no per-update `fsync`; RD-015 enqueue-time versus flush-time validation; error-text parity; the queue-limit decision.

### Effort

4 to 7 engineer-weeks.

### Verification

- Protocol differential: drive one command sequence at an upstream daemon and a Rondi daemon, compare every response line and the `.rrd` bytes after FLUSHALL, in all four client and daemon pairings over Unix and TCP.
- Journal handover: kill an upstream daemon mid-stream, start Rondi on its journal directory, FLUSHALL, compare the files with an uninterrupted upstream run; then the reverse.
- Fuzz the line parser and BATCH framing for no panic, bounded memory, and upstream accept or reject parity.
- Upstream `tests/` with `TESTS_STYLE=rrdcached-unix` and `rrdcached-tcp`.

## Surface: packaging

RRDtool ships `rrdtool`, `rrdupdate`, optional `rrdcgi` and `rrdcached`; `librrd.so.8`, `librrd.pc`, and three headers; 38 POD man pages (EXECUTED: `ls doc/*.pod | wc -l`).

Rondi ships one `rondi` executable with symlinks `rrdtool`, `rrdcached`, `rrdtool-proxy`, `rrdproxy`, the `rrdtool-proxy.php` launcher (`scripts/install-compat-links.sh`), and a systemd unit (`packaging/systemd/rondi-rrdcached.service`). It ships no man pages, no `.pc`, no headers, no library, and no distro packages. EXECUTED (file listing). The release workflow builds from merged pull requests (#28).

Plan:

1. Add `rrdupdate` and, if a consumer needs it, `rrdcgi`.
2. Ship `librrd.so.8`, `librrd.pc` (`Version: 1.11.0`), and the three headers from `rondi-ffi`.
3. Write man pages from Rondi's own docs. Do not copy upstream POD text without handling license and attribution.
4. Build `rondi-rrdtool`, `rondi-rrdcached`, and `librrd8-rondi` packages that provide and conflict with the upstream names, with `update-alternatives` so rollback is one command. Do not take the upstream package names until the CLI, rrdcached, and ABI milestones pass.
5. Keep `rrdtool --version` identical to upstream on the first line. Whether to append a Rondi identifier is open: scripts that parse the version read the first line only (READ-ONLY assumption; verify against Cacti, Munin, Smokeping). Given the exact-behavior goal, the default is to add nothing.

Effort: 2 to 3 engineer-weeks, spread across milestones.

Verification: install the packages in clean Debian, Ubuntu, Fedora, and Alpine containers, run the upstream suite and the binding suites, then uninstall and reinstall upstream to prove rollback.

## Surface: performance and durability (RD-012)

- RRDtool updates through `mmap` and the page cache with no `fsync`. A crash can lose recent updates; rrdcached batches writes for that reason.
- Rondi's `.rrd` update path syncs after each update (see the format section for the measurement). The decision to add no syncs beyond upstream's applies here too.
- RD-012 and `docs/benchmarks.md` cover the `.rondi` store, a different format. They do not describe `.rrd` update cost.

Plan: remove the per-update sync from the `.rrd` and rrdcached paths. Consider `mmap` for large archives only after measuring; `pread` may be enough.

Benchmark method:

- Extend `crates/rondi/benches/workload.rs` to drive both binaries on identical file copies: sequential updates (1, 10, and 100 data sources), round-robin updates over 10,000 files (a Cacti poller shape), fetch of 1 day and 1 year, `xport` with 20 DEFs, `graph` with 10 lines, and rrdcached throughput with 100 clients.
- Run on Linux with ext4 and XFS, on disk and on tmpfs. Use `hyperfine` for the CLI and a Rust client for the daemon. Record median and p99 over 10 runs, plus kernel, filesystem, and mount options.
- Pass criterion: Rondi within 1.2 times upstream.

Effort: 2 to 3 engineer-weeks.

## Milestones

Effort is for one engineer with IDE support. The total is 58 to 82 engineer-weeks, about 13 to 19 months for one person. The ranges are wide because the Holt-Winters numerics and the graph port hold unknowns that only porting will resolve. The earlier draft estimated 66 to 94; the reduction comes from work merged in #36 to #43.

**M0: harness (1.5 to 2.5 weeks).** Run the upstream `tests/` scripts in the pinned container against Rondi as a CI job. Add the golden-file corpus format (argv, environment, stdin, expected stdout, stderr, exit code, output hashes). Add `cargo-fuzz` targets for the header parser, restore XML, rrdcached line parser, and time parser. Add a macOS job. Exit: the suite runs on every pull request and failures are visible, not skipped.

**M1: format, update, fetch byte parity (13 to 17 weeks).** All data source types including COMPUTE; all consolidation functions including Holt-Winters; read 0001 to 0005, write 0003 to 0005; native layouts for x86_64, aarch64, i386, armhf, ppc64el, s390x; `--locking`; no per-update sync; resize v5; tune and modify forms. Exit: seeded differential streams for every DS and CF pair give identical bytes on every target in CI; cross-continuation passes; 24 hours of fuzzing per format target without a crash; update throughput within 1.2 times upstream.

**M2: CLI parity (9 to 12 weeks).** Full pipe mode and tokenizer; all non-graph options; `rrdupdate`; `RRDCACHED_STRIPPATH`; local-fallback text; time parser; error corpus. Exit: every upstream `tests/` script that passes with upstream passes with Rondi (graph scripts wait for M5); a golden corpus of at least 2,000 argv cases matches; Cacti poller and maintenance flows pass unchanged in a disposable Kadupul install (closes RD-011 except graphs).

**M3: rrdcached (4 to 7 weeks).** TCP and multiple listeners, `-U`/`-G`, daemonizing, socket activation, upstream journal format behind `-j` only, rotation, error parity, RD-015. Exit: all four client and daemon pairings pass the protocol differential over Unix and TCP; journal handover works both ways; upstream `rrdcached-*` test styles pass.

**M4: librrd ABI (10 to 14 weeks, graph symbols stubbed until M5).** `rondi-ffi`, headers, `.pc`. Prerequisites, in order: (1) argv command implementations move from `rondi-cli` into the library so the CLI and `rrd_*_r` entry points share them; (2) one `rrdc` client module for rrdcached connection, path and escaping rules; (3) a `rondi-graph` crate for graph and xport; (4) RPN parsed once into an expression tree instead of re-tokenized per row; (5) lock down the public Rust API to what the shim needs. The typed `RrdError` layer that emits exact `rrd_set_error` strings is done earlier, with the error-text work. Exit: 97 symbols exported without versions; struct layout checks pass; upstream binding tests pass; pecl `rrd` and collectd suites pass against the shim; `abidiff` reports no incompatible change.

**M5: graph (18 to 26 weeks).** Cairo and Pango renderer crate; all elements, options, formats; `graphv` metadata. Exit: `graphv` output identical for the whole corpus; data formats identical; PNG hash-identical for at least 95 percent of the corpus in the pinned container; SSIM of at least 0.98 elsewhere; Cacti, Munin, and Smokeping graph pages render with unchanged templates.

**M6: packaging and release (2 to 3 weeks, partly parallel).** Distro packages with alternatives, man pages, tested rollback. Exit: install, run all suites, uninstall, reinstall upstream, in clean containers for four distributions.

RRDProxy (RD-008) is a Cacti protocol, not part of RRDtool. Track it outside this roadmap.

`rrdcgi`: include it only if a consumer inventory finds users. It is small (`src/rrd_cgi.c`) but parses untrusted HTTP input and needs its own security review.

## Risks

1. **Graph pixels.** Upstream pixels depend on the host font stack. A faithful Cairo and Pango port still differs across hosts. Mitigation: the tiered bar above; never promise pixel identity outside the pinned container.
2. **Holt-Winters numerics.** The update code mixes float accumulation order, smoothing windows, and seasonal index arithmetic over `unsigned long` scratch slots. Small order changes alter bytes. Mitigation: port line by line before refactoring, keep C operation order, and compare every CDP scratch slot, not only archive rows.
3. **Platform-dependent output.** NaN text, `qsort` order, `printf` of `%le`, `strftime`, and locale week start differ by libc. Mitigation: generate expectations from the matching upstream build for each target; never write them by hand.
4. **Time and locale.** `rrd_parsetime` and RPN calendar operators depend on `TZ`, DST tables, and `LC_TIME`. CI images often lack locales. Mitigation: a fixed locale and tzdata version in the CI image, and a frozen "now" in tests.
5. **Dropping per-update sync.** Matching upstream removes a durability guarantee Rondi gives today. This is decided. Say so in the release notes.
6. **Licensing.** Rondi is GPL-2.0-or-later with the RRDtool FLOSS exception (#45). The author audit found no contributor besides the maintainer and Dependabot, so no other consent is needed.
7. **32-bit `time_t`.** Some 32-bit distributions moved to 64-bit `time_t`, which changes `live_head_t`. Measure upstream per distribution build; do not assume (READ-ONLY concern).
8. **Undefined upstream behavior.** `ROLL` above count 3 reads past the RPN stack (RD-005). Do not chase results that depend on stray memory; document the deviation. This is the only class of difference the goal allows.
9. **Hosted-runner cost.** Moving CI to GitHub-hosted runners removes the self-hosted option for long fuzz and benchmark jobs. Run those on a schedule with a time limit, or add a runner later by a separate decision.

## What not to do

- Do not claim drop-in for a surface until its exit criteria pass in CI. Keep the README sentence that Rondi is not drop-in until M5 completes.
- Do not hand-write expected outputs. Generate them from the pinned upstream build on the same target.
- Do not let differential tests skip silently when `rrdtool` is missing.
- Do not add ELF symbol versions, extra exported symbols, or changed struct layouts to the `librrd` shim.
- Do not add features under compatibility names (TLS in `rrdcached`, new graph options, new RPN operators). Put them behind `rondi` subcommands.
- Do not merge `.rondi` and `.rrd` code paths, and never write a `.rondi` file with an `.rrd` name.
- Do not use a pure-Rust rasterizer if pixel parity in the pinned container is a goal.
- Do not refactor ported update or Holt-Winters code for style until its differential coverage is complete.
