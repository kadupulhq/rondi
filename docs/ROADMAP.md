# Roadmap: drop-in RRDtool 1.11.0 compatibility

Rondi is not a drop-in replacement today. This page lists what is done, what is left, in what order, and how each step is checked. The individual behavior differences are tracked in [COMPATIBILITY_DEFECTS.md](COMPATIBILITY_DEFECTS.md) (RD-001 to RD-023, plus the intentional differences ID-001 to ID-003). This page does not repeat them. Where a milestone closes a defect, the ID is named.

## Goal and decisions

The goal is exact RRDtool 1.11.0 behavior on every surface Rondi claims to be compatible with. Where output depends on the host (fonts, libc, locale), the bar is the equivalence class defined in each section.

The maintainer has decided the following. The work is in progress.

- rrdcached writes no journal unless `-j` is given, and the journal uses upstream's format. Done.
- Rondi adds no syncs beyond the ones upstream performs. Done for `.rrd` updates: no `sync` call remains in `crates/rondi/src/rrd_binary.rs` (EXECUTED: grep).
- rrdcached implements `-U`, `-G`, and multiple `-l` options as upstream does. Done (#50, #97).
- After "Unable to connect to rrdcached", the client prints upstream's local-fallback text. Done for `graph`, `graphv` and `xport` `daemon=` (#92); open for `fetch`, `first`, `last`, `info`, `lastupdate` and `dump` (RD-010).
- All behavior differences listed in COMPATIBILITY_DEFECTS.md are closed. Documented exceptions are limited to behavior that depends on undefined memory (see Risks).
- Rondi is licensed `MIT OR Apache-2.0`, replacing the GPL-2.0-or-later plus RRDtool FLOSS exception terms from #45. RRDtool's notice stays in `COPYRIGHT` as third-party attribution. Author audit, EXECUTED with `git log --format='%an <%ae>%n%(trailers:key=Signed-off-by,valueonly)'`: every human commit and sign-off is Thomas Vincent; the only other author is dependabot[bot], with two workflow SHA bumps (#33, #42).
- CI runs on GitHub-hosted runners.

## Method and labels

- Reference: `oetiker/rrdtool-1.x` tag `v1.11.0`. The tag object is `7a15cf6`; it points at commit `ddc7ca7`. Rondi pins the release tarball in `docker/Dockerfile.test` with its SHA-256.
- Local oracle for the probes below: Homebrew `rrdtool 1.11.0` on macOS arm64. This is not the pinned Linux build. Results from it are marked "macOS probe"; CI repeats the differential tests in the pinned Linux container on x86_64 and aarch64.
- Rondi under test: a debug build of `main` at `26d8acf` for the probes, re-run at `204f58a` for the VDEF rule forms; `RONDI_REQUIRE_ORACLE=1 cargo test --workspace` at `204f58a` for the suite. #79, merged between the two, changed graph PRINT, VDEF and rule handling only.
- **EXECUTED** means the command was run against `main` for this page, at the commit named above. **READ-ONLY** means the claim comes from reading code, docs, or a merged pull request. RRDtool claims name the source file at `v1.11.0`.
- The effort figures were estimated at `de820e9` and have not been re-estimated; the status lines next to them are current.

## Where Rondi stands

EXECUTED: `RONDI_REQUIRE_ORACLE=1 cargo test --workspace --no-fail-fast` at `204f58a` on macOS arm64 (Rust 1.98.0 from `.mise.toml`, Homebrew rrdtool 1.11.0): 560 passed, 0 failed, 0 ignored. CI run #366 of the Rust workflow on the same commit: 566 passed on Linux x86_64 and 566 on Linux aarch64, with none failed or ignored. The six extra Linux tests are root-only rrdcached privilege tests. Details are in [compatibility.md](compatibility.md#test-evidence).

### Done in #79 and #86 to #100

Each behavior change below merged to `main` with differential tests against RRDtool 1.11.0. Nothing has been released.

- #79: PRINT/GPRINT `%S` scaling and in-place rewrite as `print_calc` does (so graphv rescales and format errors quote the rewritten text), VDEF times in UTC under `--utc`, HRULE and VRULE painted after the data elements, all-unknown VDEF results filled with the target's DNAN bits, new differential tests for SHIFT, `%S`, rules and daemon FETCH, and the `rrd_queries` Criterion benchmark (results in [benchmarks.md](benchmarks.md)).
- #86: socket ancestors another account can replace are refused, daemon RRD parents are walked without following symlinks, rrdcached CREATE is capped at 64 MiB, and native journals are checked before they are modified.
- #87: command output may go to a character device such as `/dev/null`.
- #88: a data graph whose output file cannot be opened prints the data and exits 0, as `rrd_graph_xport` does.
- #91: `tune` ports `rrd_modify.c` (`DEL:`, `DS:`, `DELRRA:`, `RRA#n:[+-=]rows`, `RRA:`, `--step`), compared file-for-file with RRDtool, including the glibc `realloc(p, 0)` and Clang FMA platform differences.
- #92: graph and xport data preparation follow `data_fetch`, `rrd_reduce_data` and CDEF/VDEF arithmetic, with FMA contraction at the sites the pinned aarch64 builds fuse; graph falls back to the local file when rrdcached is unreachable.
- #93: `--zoom`, infinite `-u`/`-l`, negative `--units-length`, malformed `PREV(`, and a 2^24-pixel PNG limit in place of 4096 per side (RD-016).
- #94: dump values through `rrd_snprintf`, update times past 2^63, raw DS name bytes in `info`/`lastupdate`/`dump`, and damaged headers opened with `rrd_open`'s wrapping arithmetic (ID-001 to ID-003).
- #95: rrdcached request handlers (CREATE, FETCH/FETCHBIN, HELP, SUSPEND/RESUME, LIST, TUNE, empty names, NUL separators) match `rrd_daemon.c`; the half-close race is recorded as RD-015.
- #96: `--daemon` addresses parse like `rrd_client.c` (bare hosts use port 42217), `tune` re-parses its permuted argv, and `rrdtool fetch` streams past 10 million rows (the collected paths keep the limit as RD-017).
- #97: rrdcached `-b`, `-P`, `-p`, default logging, and SIGUSR1, SIGUSR2 and SIGHUP match `rrd_daemon.c`.
- #98: restore reads XML through a port of libxml2 2.9.14's `xmlTextReader`, so integer field errors, a byte order mark, trailing content and libxml2 error text match (RD-018 lists what is not modeled).

- #100: Rondi is licensed `MIT OR Apache-2.0`; no behavior change.

#89 and #90 were refactors with no behavior change.

## Surface: on-disk `.rrd` format (RD-001, RD-003, RD-004, RD-005)

### What RRDtool does

- Cookie `RRD`, versions 0003 to 0005 current; 0001 and 0002 are legacy (`src/rrd_format.h`, `src/rrd_open.c`). Versions above 5 are rejected.
- The header structs follow the C ABI of the host: `long` and `time_t` width, `double` alignment, byte order. A file from another architecture fails with `This RRD was created on another architecture`. Cross-architecture transfer is by dump and restore.
- Data source types: COUNTER, ABSOLUTE, GAUGE, DERIVE, COMPUTE, DCOUNTER, DDERIVE. Consolidation functions: AVERAGE, MIN, MAX, LAST, plus the Holt-Winters family HWPREDICT, MHWPREDICT, SEASONAL, DEVSEASONAL, DEVPREDICT, FAILURES (`src/rrd_hw*.c`).
- COMPUTE values are never supplied by the caller (`src/rrd_update.c`).
- The initial ring row is random (`src/rrd_open.c`).
- Writers `mmap` the file. No `fsync`, `fdatasync`, or `msync` appears in `rrd_open.c`, `rrd_update.c`, or `rrd_create.c`.
- Locking is a whole-file `fcntl` lock. Default `try` (`F_SETLK`); `$RRD_LOCKING` and `update --locking` select `try`, `block`, or `none`.

### What Rondi does

- Opens versions 0001 to 0005 wherever `rrd_open` accepts them, including the 0001/0002 `time_t` live head (READ-ONLY: `rrd_binary.rs` and RD-001).
- Requires a 64-bit little-endian target (`rrd_binary.rs`, `target_pointer_width` and `target_endian` check). READ-ONLY.
- Supports GAUGE, COUNTER, DERIVE, ABSOLUTE, DCOUNTER, DDERIVE with AVERAGE, MIN, MAX, LAST. COMPUTE and Holt-Winters still fail. EXECUTED (macOS probe): Rondi `create` refuses `DS:b:COMPUTE:a,2,*` and `RRA:HWPREDICT:100:0.1:0.0035:10`; on files upstream created with them, `update` and `info` fail.
- Updates no longer sync each write (EXECUTED: no `sync` call in `rrd_binary.rs`). `update --locking` and `RRD_LOCKING` are accepted (EXECUTED: `update --locking none` succeeds).
- `tune` performs the `rrd_modify` operations (#91). `resize` accepts v3 and v4 only (READ-ONLY).
- Damaged headers open as upstream does; three cases that would need undefined behavior are refused on purpose (ID-001 to ID-003).
- The NaN bit pattern follows the target architecture (`rrd_nan`, READ-ONLY); aarch64 update arithmetic fuses `a * b + c` where the pinned builds do.

### Gaps

1. Holt-Winters family: create, update, fetch, info, dump, restore, tune (`--alpha`, `--beta`, `--gamma`, `--gamma-deviation`, `--window-length`, `--failure-threshold`, `--deltapos`, `--deltaneg`, `--smoothing-window`, `--aberrant-reset`).
2. COMPUTE data sources. The RPN engine in `crates/rondi/src/rpn.rs` can be reused.
3. Non-64-bit and big-endian layouts (i386, armhf, ppc64el, s390x). This needs a layout descriptor computed for the build target, not fixed offsets.
4. `resize` for v5; `create --source` with several sources and mappings (RD-004).
5. A failed `tune -s`/`-S` does not raise the file to version 0004 (RD-002). EXECUTED (macOS probe).
6. `update` on a file with an unknown DS type or CF name (RD-021); `dump` with `cur_row` far past `row_cnt` (RD-023, not reproduced).

### Effort

Estimated at `de820e9`: 13 to 17 engineer-weeks, of which locking, sync policy and the tune modify forms are now done. Holt-Winters 5 to 7 (port about 1,500 lines of C state machines and their scratch layout, then difference each). Layout abstraction and multi-arch CI 3 to 4. COMPUTE 1. Resize and source handling 2 to 3.

### Verification

- Differential: for every DS and CF combination, create one file with upstream, copy it, apply the same seeded update stream to both, and `cmp` after each batch. `crates/rondi/tests/rrdtool_differential.rs` has the generator; extend it to Holt-Winters and COMPUTE.
- Cross-continuation: Rondi writes N updates, upstream writes M more, and the reverse; compare with an all-upstream file.
- Golden files: small upstream-created files for each version and target layout (built under QEMU with the pinned tarball). Rondi must read every one where RRDtool can.
- Fuzzing: the `rrd_file`, `restore_xml` and `update_args` targets under `fuzz/fuzz_targets/` exist; extend them with a corpus seeded from the golden files and compare the accept or reject decision with upstream.

## Surface: command line (RD-002, RD-010, RD-014)

### What RRDtool does

- `src/rrd_tool.c` dispatches create, dump, info, updatev, list, version, restore, resize, last, lastupdate, first, update, fetch, xport, graph, graphv, tune, flushcached. An unknown name sets `unknown function '%s'`.
- Errors print `ERROR: <msg>` to stderr and exit 1. A bare `rrdtool` or a single argument prints usage and exits 0.
- Pipe mode `rrdtool - [dir]`: optional chroot, extra commands `quit`, `cd`, `pwd`, `mkdir`, `ls`, errors on stdout, success line `OK u:%1.2f s:%1.2f r:%1.2f`.
- The pipe tokenizer `CreateArgs` splits on space only, honors single and double quotes, has no backslash escape, and treats tab as an ordinary character.
- Separate binaries: `rrdtool`, `rrdupdate`, optional `rrdcgi`, optional `rrdcached`.
- Environment: `RRDCACHED_ADDRESS`, `RRDCACHED_STRIPPATH`, `RRD_LOCKING`, `RRD_DEFAULT_FONT`, plus `TZ` and `LC_*`.

### What Rondi does

- `argv[0]` dispatch for `rrdtool`, `rrdcached`, `rrdupdate`, `rrdcreate`, `rrdinfo`, `rrdtool-proxy`, `rrdproxy`. No `rrdcgi`.
- Pipe mode runs every command through a port of `rrd_tool.c` dispatch and `CreateArgs`. EXECUTED (macOS probe): a session of `cd /tmp`, `pwd`, `xport ...` and `quit` gives the same output as upstream once the timing digits are masked, and `info no\:such.rrd` prints `opening 'no\:such.rrd'` from both.
- No-argument help for `create`, `last`, `flushcached`, `first` and `lastupdate` matches upstream byte for byte apart from the `Compiled` line. EXECUTED (macOS probe).
- `--daemon` and `RRDCACHED_ADDRESS` parse like `rrd_client.c`; `RRDCACHED_STRIPPATH` follows `get_path` (#76, #96). `RRD_DEFAULT_FONT` is not read (EXECUTED: grep).
- After a failed daemon connection, `fetch`, `first`, `last`, `info`, `lastupdate` and `dump` print only the error where RRDtool also prints a local read (RD-010). EXECUTED (macOS probe).
- `tune --daemon` drops `DS:`/`RRA:` words and exits 0 (RD-019). EXECUTED (macOS probe, each client against its own daemon).
- The upstream `tests/` shell suite has not been run against `main`; there is no CI job for it.

### Gaps

1. Local-fallback output after "Unable to connect to rrdcached" for the commands above (decided; RD-010).
2. `tune --daemon` with modification words (RD-019), in both the client and Rondi's rrdcached TUNE handler.
3. The pipe-mode chroot argument (not checked).
4. Option completeness outside graph (RD-002, RD-004) and an error text corpus beyond the tested set (RD-010).
5. Non-UTF-8 DS names in the `fetch` header (RD-022).
6. `rrdcgi` only if a consumer needs it.
7. TLS does not exist in RRDtool 1.11.0 and should not be added under the compatibility names.

### Effort

Estimated at `de820e9`: 9 to 12 engineer-weeks, of which pipe mode, the tokenizer and `rrdupdate` are done. The remaining items are mostly the error corpus (3 to 4 weeks) and option completeness (about 3).

### Verification

- Run the upstream `tests/` scripts in the pinned Linux container with `RRDTOOL` set to Rondi, as a CI job. Require every script that passes with upstream to pass with Rondi.
- Golden argv corpus (generated from option tables plus fuzz) with stdout, stderr, exit code, and output file hash recorded from upstream.
- Fuzz the time parser (`time_spec` target) against `rrd_parsetime` with fixed `TZ` and a frozen clock.

## Surface: graphing (RD-006, RD-016, RD-017, RD-020)

### What RRDtool does

- Image formats PNG, SVG, EPS, PDF; data formats XML, XMLENUM, CSV, TSV, SSV, JSON, JSONTIME.
- Elements: PRINT, GPRINT, COMMENT, HRULE, VRULE, LINE, AREA, STACK, TICK, TEXTALIGN, DEF, CDEF, VDEF, SHIFT, XPORT.
- Rendering uses Cairo and Pango, with a font list resolved at run time (`src/rrd_graph.c`, overridable by `RRD_DEFAULT_FONT`).
- `graphv` prints layout metadata: `graph_left`, `graph_top`, `graph_width`, `graph_height`, `image_width`, `image_height`, `value_min`, `value_max`, `legend[n]`, `coords[n]`.

### What Rondi does

- PNG through the `png` crate with its own rasterizer and font; CSV, TSV, SSV, XML and JSON data output. `--imgformat SVG`, `PDF` and `EPS` exit 1 with `RRDtool graph format X is unsupported`. EXECUTED (macOS probe).
- Every option in the `rrd_graph.c` table is accepted and validated. EXECUTED (macOS probe): `--logarithmic --slope-mode --right-axis 1:0 --watermark w --utc --x-grid none --units-exponent 0 --no-gridfit --legend-position=east --dynamic-labels` succeed for both and print the same `585x126`.
- `graphv` output for a 400x100 LINE graph is identical to upstream; the PNG has the same size but different bytes. EXECUTED (macOS probe).
- Elements parse through a port of `rrd_graph_script`; data preparation and PRINT values follow `data_fetch`, `data_calc` and `print_calc` (#92).
- Collected fetches stop at 10 million rows (RD-017). EXECUTED for `xport`.
- The last `xport` row of a PREDICT-family CDEF with shift 0 differs (RD-020). EXECUTED (macOS probe).

### The bar

Pixel identity is a measured property inside one pinned container, not a promise across hosts. Upstream pixels depend on Cairo, Pango, FreeType, fontconfig, and the installed fonts, so two builds of 1.11.0 can disagree.

1. **Exact**: `graphv` keys and values, PRINT values, image width and height, data-format outputs (XML, JSON, CSV, TSV, SSV), exit status, for the full option and element grammar.
2. **Exact inside the pinned container**: PNG bytes, when Rondi renders through the same Cairo and Pango versions and fonts. Track the share of identical PNGs in a corpus; treat drops as regressions.
3. **Perceptual elsewhere**: SSIM of at least 0.98 per image against upstream on the same host, and identical text in SVG output.

Reaching tier 2 requires Cairo and Pango bindings; a pure-Rust rasterizer cannot meet it. Keep the renderer in its own crate so the storage library stays free of C dependencies, as `docs/architecture.md` intends.

### Gaps

SVG, EPS, PDF; PNG pixels and fonts (tier 2); legend `coords` under `--zoom` on Linux (reported in #93); text in fonts other than the default; Cairo strokes and gradients; the 10 million row limit for collected fetches, which needs graph and xport to consume rows as a stream; the PREDICT last-row difference.

### Effort

Estimated at `de820e9`: 18 to 26 engineer-weeks, mostly a port of `rrd_graph.c` and `rrd_graph_helper.c` onto Cairo and Pango. Layout metadata and option parsing, which were part of that estimate, are now done, so the remaining work is concentrated in rendering. Budget 4 weeks for pixel work after the renderer lands and stop when the corpus metric plateaus. Streaming collected fetches is a separate 1 to 2 weeks (not estimated before).

### Verification

- Corpus of graph definitions from upstream `tests/graph*`, `doc/rrdgraph_examples.pod`, and real Cacti and Munin template output, with argv and upstream `graphv` output stored as golden files.
- Compare `graphv` text exactly, PNG by hash in the pinned container and by SSIM elsewhere, SVG after stripping generated IDs.
- Fuzz the graph argument parser with `cargo-fuzz` for no-panic and for accept or reject parity with a C harness calling `rrd_graph_v`.

## Surface: RPN, CDEF, VDEF, time (RD-005)

RRDtool has 61 named RPN operators plus `+ - * / %` (counted from the `match_op` calls in `src/rrd_rpncalc.c` in an earlier review; not recounted) and 12 VDEF functions.

What Rondi does:

- `crates/rondi/src/rpn.rs` ports `rpn_parse` and `rpn_calc`, including the per-variable data cursors that mixed-resolution `PREV`, `TREND` and `PREDICT` read; `vdef.rs` ports the 12 VDEF functions with the pinned compilers' FMA contraction (#92). The `rpn_diff` and `rpn_xport` fuzz targets exist.
- RPN is reachable from xport and graph, not from COMPUTE data sources.
- SORT, MEDIAN, PERCENT and PREDICTPERC call the host `qsort`, whose order with NaN and infinities is not defined by the comparator. Tested on macOS and Linux glibc.

Gaps: COMPUTE wiring; the PREDICT last-row difference (RD-020); rows near the ends of a mixed-resolution range where RRDtool reads outside its buffers (undefined, see Risks); `qsort` order emulation if parity beyond glibc and Apple libc is wanted.

Effort: 2 to 4 engineer-weeks remaining (estimated at `de820e9` as 3 to 5).

Verification: differential RPN fuzzing (random well-typed expressions over seeded data, compare `xport` JSON bit for bit); grammar-based fuzzing of the time parser against `rrd_parsetime` with `faketime` or an `LD_PRELOAD` clock.

## Surface: librrd C ABI (RD-009)

### What RRDtool does

- `src/librrd.sym` exports 97 symbols: 14 `_r` variants, 20 `rrdc_*` client functions, and the rest. There are no ELF symbol versions. `LIBVERS=11:0:3` gives soname `librrd.so.8` (soname arithmetic READ-ONLY).
- Public headers `rrd.h`, `rrd_format.h`, `rrd_client.h`. Exported `rrd_open`, `rrd_read`, `rrd_write`, and `rrd_seek` expose `rrd_t` and `rrd_file_t`, so the `rrd_format.h` layout is part of the ABI.
- `librrd.pc` reports `Name: librrd` and the package version.
- License: GPL-2.0-or-later with the FLOSS License Exception (`COPYRIGHT`).
- Upstream bindings (Perl, Python, Ruby, Lua) call: `rrd_create`, `rrd_update`, `rrd_update_v`, `rrd_fetch`, `rrd_fetch_cb_register`, `rrd_first`, `rrd_last`, `rrd_lastupdate_r`, `rrd_info`, `rrd_info_free`, `rrd_graph`, `rrd_graph_v`, `rrd_xport`, `rrd_dump`, `rrd_restore`, `rrd_resize`, `rrd_tune`, `rrd_flushcached`, `rrd_list`, `rrd_parsetime`, `rrd_proc_start_end`, `rrd_strversion`, `rrd_get_error`, `rrd_set_error`, `rrd_clear_error`, `rrd_test_error`, `rrd_freemem`.

### What Rondi does

Nothing. No `crate-type` in `crates/*/Cargo.toml`; no FFI, no headers (READ-ONLY). RD-009 records that the checked Kadupul tree uses the CLI only. That inventory does not cover PHP pecl `rrd`, collectd, Smokeping, Munin, Ganglia, or MRTG, which link `librrd` or its Perl bindings (READ-ONLY; not verified per consumer).

### Plan

- New crate `rondi-ffi` with `crate-type = ["cdylib", "staticlib"]`, soname `librrd.so.8` set by a linker argument. Generate headers with `cbindgen`, diff against upstream `rrd.h` and `rrd_client.h`, and fix by hand. Do not ship headers whose type layout differs from upstream.
- Export exactly the 97 symbols with a single anonymous version script (`{ global: ...; local: *; };`). A version node would stop binaries linked against Rondi from loading against upstream, which blocks rollback.
- Keep a per-thread error context with `rrd_get_context` semantics. `rrd_get_error` returns a pointer valid until the next call on that thread.
- Argc/argv entry points reuse the CLI option parser so behavior is identical, including `optind` reset.
- Allocate `rrd_info_t` lists and fetch buffers with `libc::malloc`, so `rrd_info_free` and `rrd_freemem` (which call `free`) work.
- `rrd_open` and `rrd_t` exposure sits on the layout descriptor from the format section. It is the hardest part; start with an error stub and inventory which consumers call it (`nm -D --undefined-only`).
- License: Rondi is `MIT OR Apache-2.0`, while upstream `librrd` is GPL-2.0-or-later with the FLOSS exception. Parts of Rondi translate RRDtool C source, so the terms a Rondi `librrd` can ship under need the owner's confirmation. This is not legal advice.

### Effort

10 to 14 engineer-weeks, after M1 and M2. Graph symbols wait for M5.

### Verification

- `nm -D --defined-only librrd.so.8 | sort` equals the 97-name list.
- A C file asserting `sizeof` and `offsetof` for every public struct against the upstream headers.
- Upstream binding test suites (Perl, Python, Ruby, Lua, Tcl) against the shim. Then pecl `rrd`, collectd's `rrdtool` and `rrdcached` plugins, and Smokeping with `LD_LIBRARY_PATH` set to the shim.
- Thread test: N threads calling `rrd_update_r` and `rrd_fetch_r` on distinct files with error injection; check per-thread errors.
- `abidiff` (libabigail) or `abi-compliance-checker` against upstream `librrd.so.8`.

## Surface: rrdcached (RD-007, RD-014, RD-015, RD-017)

### What RRDtool does

- 25 commands: UPDATE, WROTE, TUNE, DUMP, FLUSH, FLUSHALL, PENDING, FORGET, QUEUE, STATS, HELP, PING, BATCH, FETCH, FETCHBIN, INFO, FIRST, LAST, CREATE, LIST, SUSPEND, RESUME, SUSPENDALL, RESUMEALL, QUIT (`src/rrd_daemon.c`).
- Options `-a -B -b -F -f -g -G -h -j -L -l -m -O -o -P -p -R -s -t -U -V -w -z`.
- Listeners: Unix paths and TCP `host:port`, several `-l` allowed. systemd socket activation through `LISTEN_PID` and `LISTEN_FDS`. Without `-g` the daemon forks into the background.
- Journal: text lines `<cmd> <args>`, files `rrd.journal.%010d.%06d`, rotated at 1 GiB (`JOURNAL_MAX`), legacy names handled on replay. Written through stdio with no per-record `fsync`. It exists only when `-j` is given.

### What Rondi does

- Handles the 25 commands; the handlers for CREATE, FETCH/FETCHBIN, HELP, SUSPEND/RESUME, LIST and TUNE follow `rrd_daemon.c` argument parsing and error text (#95).
- Unix and TCP listeners, several `-l` and `-L`, each with the `-m`, `-s` and `-P` given before it; `-U` and `-G` drop privileges after binding (#50). Root container tests run in CI.
- SIGUSR1, SIGUSR2 and SIGHUP behave as upstream; `-b`, `-P`, `-p` and default logging match (#97).
- Journal: upstream format and file layout, written only with `-j`, buffered with no sync, replayed in both directions between upstream and Rondi.
- Never forks and has no socket activation (EXECUTED: no `fork` or `LISTEN_FDS` in `crates/`). `-z` and `-V` are validated but not applied, and `-o` writes Rondi's own log format (RD-007).
- Pending queue limit of 64 MiB (`DEFAULT_RRDCACHED_QUEUE_BYTES`), FETCH limit of 10 million rows (RD-017), and the safety deviations listed under RD-007.

### Gaps

Daemonizing without `-g`; socket activation; `-V` log levels through syslog and the `-o` format; TUNE with DS/RRA modification words (RD-019); FLUSH/FLUSHALL worker-queue scheduling; the half-close race (RD-015, platform-dependent, kept as a documented difference); the queue-limit decision.

### Effort

2 to 4 engineer-weeks remaining (estimated at `de820e9` as 4 to 7, before #50, #95 and #97).

### Verification

- Protocol differential: drive one command sequence at an upstream daemon and a Rondi daemon, compare every response line and the `.rrd` bytes after FLUSHALL, in all four client and daemon pairings over Unix and TCP. `rrdcached_protocol_parity.rs` already drives the same sessions at upstream rrdcached and Rondi's (#95).
- Journal handover in both directions (done, `rrdcached_journal_replays_across_upstream_and_rondi`).
- Fuzz the line parser and BATCH framing (`rrdcached_protocol` and `rrdcached_journal` targets exist) for no panic, bounded memory, and upstream accept or reject parity.
- Upstream `tests/` with `TESTS_STYLE=rrdcached-unix` and `rrdcached-tcp`.

## Surface: packaging

RRDtool ships `rrdtool`, `rrdupdate`, optional `rrdcgi` and `rrdcached`; `librrd.so.8`, `librrd.pc`, and three headers; 38 POD man pages.

Rondi ships one `rondi` executable with symlinks `rrdtool`, `rrdcached`, `rrdupdate`, `rrdcreate`, `rrdinfo`, `rrdtool-proxy`, `rrdproxy`, the `rrdtool-proxy.php` launcher (`scripts/install-compat-links.sh`), and a systemd unit (`packaging/systemd/rondi-rrdcached.service`). It ships no man pages, no `.pc`, no headers, no library, and no distro packages. No GitHub release has been published (EXECUTED: `gh release list` is empty).

Plan:

1. `rrdcgi`, if a consumer needs it.
2. Ship `librrd.so.8`, `librrd.pc` (`Version: 1.11.0`), and the three headers from `rondi-ffi`.
3. Write man pages from Rondi's own docs. Do not copy upstream POD text without handling license and attribution.
4. Build `rondi-rrdtool`, `rondi-rrdcached`, and `librrd8-rondi` packages that provide and conflict with the upstream names, with `update-alternatives` so rollback is one command. Do not take the upstream package names until the CLI, rrdcached, and ABI milestones pass.
5. Keep `rrdtool --version` identical to upstream on the first line. Whether to append a Rondi identifier is open: scripts that parse the version read the first line only (READ-ONLY assumption; verify against Cacti, Munin, Smokeping). Given the exact-behavior goal, the default is to add nothing.

Effort: 2 to 3 engineer-weeks, spread across milestones.

Verification: install the packages in clean Debian, Ubuntu, Fedora, and Alpine containers, run the upstream suite and the binding suites, then uninstall and reinstall upstream to prove rollback.

## Surface: performance and durability

- RRDtool updates through `mmap` and the page cache with no `fsync`. A crash can lose recent updates; rrdcached batches writes for that reason.
- Rondi's `.rrd` update path no longer syncs each update, matching upstream (EXECUTED: grep). The update throughput gap measured at `de820e9` has not been re-measured since.
- `docs/benchmarks.md` and the store notes in `docs/architecture.md` (formerly RD-012 and RD-013) cover the `.rondi` store, a different format. They do not describe `.rrd` update cost.
- `rrdtool fetch` streams rows: a 10,000,002-row fetch took 6 MB peak RSS in Rondi against 170 MB in RRDtool (READ-ONLY, #96).

Benchmark method:

- Extend `crates/rondi/benches/rrd_workload.rs` to drive both binaries on identical file copies: sequential updates (1, 10, and 100 data sources), round-robin updates over 10,000 files (a Cacti poller shape), fetch of 1 day and 1 year, `xport` with 20 DEFs, `graph` with 10 lines, and rrdcached throughput with 100 clients.
- Run on Linux with ext4 and XFS, on disk and on tmpfs. Use `hyperfine` for the CLI and a Rust client for the daemon. Record median and p99 over 10 runs, plus kernel, filesystem, and mount options.
- Pass criterion: Rondi within 1.2 times upstream.

Effort: 2 to 3 engineer-weeks.

## Milestones

Effort is for one engineer with IDE support, estimated at `de820e9` (58 to 82 engineer-weeks in total) and not re-estimated. The status line under each milestone is current at `204f58a`.

**M0: harness (1.5 to 2.5 weeks).** Run the upstream `tests/` scripts in the pinned container against Rondi as a CI job. Add the golden-file corpus format (argv, environment, stdin, expected stdout, stderr, exit code, output hashes). Add `cargo-fuzz` targets for the header parser, restore XML, rrdcached line parser, and time parser. Add a macOS job. Exit: the suite runs on every pull request and failures are visible, not skipped.
Status: partly done. Fuzz targets exist and CI replays their seeds; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle fail. No upstream `tests/` job, no golden corpus, no macOS CI job.

**M1: format, update, fetch byte parity (13 to 17 weeks).** All data source types including COMPUTE; all consolidation functions including Holt-Winters; non-64-bit and big-endian layouts; resize v5. Exit: seeded differential streams for every DS and CF pair give identical bytes on every target in CI; cross-continuation passes; 24 hours of fuzzing per format target without a crash; update throughput within 1.2 times upstream.
Status: in progress. Done: legacy 0001/0002 reads, `--locking`, no per-update sync, `tune` modify forms (#91), damaged-header handling (#94). Open: COMPUTE, Holt-Winters, other layouts, resize v5, RD-021, RD-023, the `tune -s`/`-S` version bump.

**M2: CLI parity (9 to 12 weeks).** Full pipe mode and tokenizer; all non-graph options; `rrdupdate`; `RRDCACHED_STRIPPATH`; local-fallback text; time parser; error corpus. Exit: every upstream `tests/` script that passes with upstream passes with Rondi (graph scripts wait for M5); a golden corpus of at least 2,000 argv cases matches; Cacti poller and maintenance flows pass unchanged in a disposable Kadupul install (closes RD-011 except graphs).
Status: in progress. Done: pipe mode and tokenizer, `rrdupdate`, `RRDCACHED_STRIPPATH`, daemon address parsing (#96), restore error text (#98). Open: local fallback (RD-010), `tune --daemon` modification words (RD-019), RD-022, the error corpus, and the exit criteria, none of which has been run.

**M3: rrdcached (4 to 7 weeks).** TCP and multiple listeners, `-U`/`-G`, daemonizing, socket activation, upstream journal format behind `-j` only, rotation, error parity. Exit: all four client and daemon pairings pass the protocol differential over Unix and TCP; journal handover works both ways; upstream `rrdcached-*` test styles pass.
Status: mostly done. Done: TCP and multiple listeners, `-U`/`-G`, journal format and handover, handler and signal parity (#95, #97). Open: daemonizing, socket activation, `-V` and `-o`, TUNE modification words, and the upstream test styles.

**M4: librrd ABI (10 to 14 weeks, graph symbols stubbed until M5).** `rondi-ffi`, headers, `.pc`. Prerequisites, in order: (1) argv command implementations move from `rondi-cli` into the library so the CLI and `rrd_*_r` entry points share them; (2) one `rrdc` client module for rrdcached connection, path and escaping rules; (3) a `rondi-graph` crate for graph and xport; (4) RPN parsed once into an expression tree instead of re-tokenized per row; (5) lock down the public Rust API to what the shim needs. Exit: 97 symbols exported without versions; struct layout checks pass; upstream binding tests pass; pecl `rrd` and collectd suites pass against the shim; `abidiff` reports no incompatible change.
Status: not started; the license question in the C ABI section is open.

**M5: graph (18 to 26 weeks).** Cairo and Pango renderer crate; all elements, options, formats; `graphv` metadata. Exit: `graphv` output identical for the whole corpus; data formats identical; PNG hash-identical for at least 95 percent of the corpus in the pinned container; SSIM of at least 0.98 elsewhere; Cacti, Munin, and Smokeping graph pages render with unchanged templates.
Status: data preparation, options and `graphv` layout match for the tested cases (#92, #93). Open: the Cairo and Pango renderer, SVG, PDF and EPS, streaming collected fetches past 10 million rows (RD-017), RD-020, and the corpus.

**M6: packaging and release (2 to 3 weeks, partly parallel).** Distro packages with alternatives, man pages, tested rollback. Exit: install, run all suites, uninstall, reinstall upstream, in clean containers for four distributions.
Status: not started; no release exists.

### Next steps

In rough order of value against effort:

1. Local-fallback output after a failed daemon connection (RD-010) and `tune --daemon` modification words (RD-019). Both are small, decided, and visible to rrdcached users.
2. The `tune -s`/`-S` version bump (RD-002), the `update` error on unknown DS types and CF names (RD-021), and the `fetch` header bytes (RD-022).
3. Trace RD-020 and decide whether it depends on reads past RRDtool's buffers; if so, record it as an intentional difference.
4. M0: an upstream `tests/` CI job, so the M2 exit criterion can be measured.
5. COMPUTE, then Holt-Winters (M1).

RRDProxy (RD-008) is a Cacti protocol, not part of RRDtool. Track it outside this roadmap.

`rrdcgi`: include it only if a consumer inventory finds users. It is small (`src/rrd_cgi.c`) but parses untrusted HTTP input and needs its own security review.

## Risks

1. **Graph pixels.** Upstream pixels depend on the host font stack. A faithful Cairo and Pango port still differs across hosts. Mitigation: the tiered bar above; never promise pixel identity outside the pinned container.
2. **Holt-Winters numerics.** The update code mixes float accumulation order, smoothing windows, and seasonal index arithmetic over `unsigned long` scratch slots. Small order changes alter bytes. Mitigation: port line by line before refactoring, keep C operation order, and compare every CDP scratch slot, not only archive rows.
3. **Platform-dependent output.** NaN bits, `qsort` order, FMA contraction, double-to-integer casts, `realloc(p, 0)`, `strtoll` errno and the libxml2 version differ by compiler, CPU or libc. [compatibility.md](compatibility.md#platform-notes) lists the cases Rondi reproduces. Mitigation: generate expectations from the matching upstream build for each target; never write them by hand.
4. **Time and locale.** `rrd_parsetime` and RPN calendar operators depend on `TZ`, DST tables, and `LC_TIME`. CI images often lack locales. Mitigation: a fixed locale and tzdata version in the CI image, and a frozen "now" in tests.
5. **Dropping per-update sync.** Matching upstream removed a durability guarantee Rondi used to give. Say so in the release notes.
6. **Licensing.** Rondi is `MIT OR Apache-2.0`. The author audit found no contributor besides the maintainer and Dependabot, so no other consent is needed for Rondi-authored code. Code translated from RRDtool keeps RRDtool's GPL-2.0-or-later plus FLOSS exception terms upstream; whether that is compatible with the dual license is for the owner to confirm.
7. **32-bit `time_t`.** Some 32-bit distributions moved to 64-bit `time_t`, which changes `live_head_t`. Measure upstream per distribution build; do not assume (READ-ONLY concern).
8. **Undefined upstream behavior.** `ROLL` above count 3 reads past the RPN stack, and mixed-resolution CDEFs read outside RRDtool's buffers near the ends of a range (RD-005). Do not chase results that depend on stray memory; document the deviation. This is the only class of difference the goal allows, together with the safety deviations already recorded as ID-001 to ID-003, RD-016 and RD-017.
9. **Hosted-runner cost.** GitHub-hosted runners limit long fuzz and benchmark jobs. Run those on a schedule with a time limit, or add a runner later by a separate decision.

## What not to do

- Do not claim drop-in for a surface until its exit criteria pass in CI. Keep the README sentence that Rondi is not drop-in until M5 completes.
- Do not hand-write expected outputs. Generate them from the pinned upstream build on the same target.
- Do not let differential tests skip silently when `rrdtool` is missing.
- Do not add ELF symbol versions, extra exported symbols, or changed struct layouts to the `librrd` shim.
- Do not add features under compatibility names (TLS in `rrdcached`, new graph options, new RPN operators). Put them behind `rondi` subcommands.
- Do not merge `.rondi` and `.rrd` code paths, and never write a `.rondi` file with an `.rrd` name.
- Do not use a pure-Rust rasterizer if pixel parity in the pinned container is a goal.
- Do not refactor ported update or Holt-Winters code for style until its differential coverage is complete.
