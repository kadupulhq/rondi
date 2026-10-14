# Fuzzing

cargo-fuzz (libFuzzer) targets for the code that reads untrusted input. The
crate is outside the main workspace, so `cargo test` at the repository root
does not build it.

| Target | Input |
| --- | --- |
| `rrd_file` | Bytes written as an `.rrd`, then `info`, `first`, `fetch` for every archive, `dump` (no header and XSD header) fed back to `restore`, one `update`, and `resize`. |
| `restore_xml` | Text passed to `restore`; a successful restore must be readable again. |
| `rrdcached_protocol` | A client byte stream through the real rrdcached connection loop (BATCH, FETCHBIN, DUMP, CREATE and the rest), then a flush of whatever is still queued. The first byte turns on a `-P` permission list. |
| `rrdcached_journal` | Journal contents replayed at daemon start-up and flushed. `ROOT` in the input is replaced with the base directory. |
| `rpn_xport` | Lines appended as `xport` or `graph` arguments after three fixed DEFs: CDEF/VDEF/PRINT/GPRINT and graph elements. The first byte picks xport or graph and the format. |
| `rpn_diff` | The `rpn_xport` input format, xport modes only, run through Rondi and `rrdtool xport`; differences are logged. |
| `time_spec` | AT-style time specifications, range pairs, update timestamps and scaled durations. |
| `update_args` | `update`, `updatev` and `create` argument vectors, one argument per line. |
| `http_api` | Raw HTTP/1 request bytes through the JSON API service. |

`src/lib.rs` compiles the CLI and rrdcached sources a second time so the
targets can reach their private functions. Time zone is fixed to UTC.

## Running

Requires a nightly toolchain and `cargo install cargo-fuzz`. Seeds are in
`seeds/<target>`; pass a separate, writable corpus directory first so the
committed seeds stay unchanged.

```sh
cd fuzz
cargo +nightly fuzz build -O -a        # -a keeps overflow checks on
cargo +nightly fuzz run -O -a rrd_file corpus/rrd_file seeds/rrd_file -- \
    -max_len=8192 -timeout=10 -rss_limit_mb=2048
cargo +nightly fuzz run -O -a rpn_xport corpus/rpn_xport seeds/rpn_xport -- \
    -max_len=1024 -dict=dict/rpn.dict
cargo +nightly fuzz run -O -a rrdcached_protocol corpus/rrdcached_protocol \
    seeds/rrdcached_protocol -- -max_len=4096 -dict=dict/rrdcached.dict
```

Suggested `-max_len`: 8192 for `rrd_file`, 16384 for `restore_xml`, 4096 for
the rrdcached and HTTP targets, 1024 for `rpn_xport`, 512 for `update_args`
and 128 for `time_spec`.

The stateful targets sync files on every update. On macOS `sync_all` is
`F_FULLFSYNC`, which drops throughput to a few executions per second. A RAM
disk restores it:

```sh
diskutil erasevolume APFS rondifuzz "$(hdiutil attach -nomount ram://4194304)"
TMPDIR=/Volumes/rondifuzz cargo +nightly fuzz run ...
```

Differential RPN run over an existing corpus (needs `rrdtool` 1.11.0 on
`PATH`, or set `RRDTOOL`):

```sh
RONDI_DIFF_LOG=rpn_diff.log cargo +nightly fuzz run -O rpn_diff corpus/rpn_xport -- -runs=0
```

ROLL with a count above 3 reads outside RRDtool's stack and is expected to
differ.

`rpn_xport` skips LINE widths above 64 because wider lines currently hang the
PNG renderer (`crates/rondi-cli/tests/fuzz_regressions.rs`); set
`RONDI_FUZZ_WIDE_LINES=1` to fuzz that path anyway.

## Seeds

`mkseeds.sh <dir>` regenerates the `.rrd` seeds with a local `rrdtool`: every
basic DS type, COMPUTE, all CFs including the Holt-Winters family, and a
wrapped archive. The `386_*` and `arm_v7_*` files are the same script run
under i386 and armhf Debian (`rrdtool` 1.7.2), which gives the 32-bit layouts.
Rondi rejects those layouts, so they exercise the header checks. Rondi itself
writes only the 64-bit little-endian layout.

The `seeds/<target>` text inputs are hand-written starting points; the HTTP
seeds are complete requests and the rrdcached seeds are protocol sessions.
`dict/` holds libFuzzer dictionaries for the RPN and rrdcached grammars.

Crashes should be minimised (`cargo fuzz tmin`) and kept as a regression test
in the crate that owns the code.
