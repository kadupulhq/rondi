# Compatibility notes

Rondi targets the observable behavior of RRDtool 1.11.0 (tag
[`v1.11.0`](https://github.com/oetiker/rrdtool-1.x/tree/v1.11.0)). That
behavior is now written down as implementation-neutral rules in
[`compatibility/`](compatibility/README.md), each with its provenance and the
tests that pin it. This page is an index from the topics these notes used to
cover to those rules.

Earlier versions of this page described RRDtool's C implementation. That
text is kept in the git history as a record of how the legacy modules were
written; [`provenance-audit.md`](provenance-audit.md) assesses it. Do not
use it as a source for new work.

## Index

| Topic | Rules |
|---|---|
| No-argument help and usage text for each command | [cli.md](compatibility/cli.md) CLI-3, CLI-8 to CLI-10; [create.md](compatibility/create.md) CR-2; [resize.md](compatibility/resize.md) RZ-1 |
| `update` and `updatev`: arguments, timestamps, rates, PDP and CDP state, rows written, output | [update.md](compatibility/update.md) UP-1 to UP-38 |
| A long interval with an open PDP (split update) | [update.md](compatibility/update.md) UP-22, UP-23, UP-29 |
| `COUNTER` and `DERIVE` differences of decimal text, and the decimal conversion | [update.md](compatibility/update.md) UP-16; [numbers.md](compatibility/numbers.md) NM-1 to NM-5; [restore.md](compatibility/restore.md) RS-38 |
| Platform multiply-add rounding | [update.md](compatibility/update.md) UP-35; [graph.md](compatibility/graph.md) GR-100 |
| `COMPUTE` data sources | [compute.md](compatibility/compute.md) |
| Holt-Winters archives | [holt-winters.md](compatibility/holt-winters.md) |
| Lock modes (`--locking`, `RRD_LOCKING`) | [cli.md](compatibility/cli.md) CLI-38, CLI-47; [tune-modify.md](compatibility/tune-modify.md) TM-3, TM-56 |
| `create`: definitions, file layout, random initial rows, unknown bit patterns | [create.md](compatibility/create.md); CR-46 (initial rows), CR-74 (unknown bits) |
| `fetch` output | [cli.md](compatibility/cli.md) CLI-66, CLI-67 |
| `info`, `first`, `last`, `lastupdate` output | [cli.md](compatibility/cli.md) CLI-63 to CLI-65, CLI-68 |
| `list` | [cli.md](compatibility/cli.md) CLI-70 to CLI-75, CLI-79 |
| NaN and infinity text, locale decimal point | [format-strings.md](compatibility/format-strings.md) FS-14; [cli.md](compatibility/cli.md) CLI-49; [restore.md](compatibility/restore.md) RS-52 |
| `restore` | [restore.md](compatibility/restore.md) |
| `resize` | [resize.md](compatibility/resize.md) |
| `tune` | [tune-modify.md](compatibility/tune-modify.md); Holt-Winters parameters in [holt-winters.md](compatibility/holt-winters.md) |
| At-style times, `update` timestamps, `dump` date comments | [time-spec.md](compatibility/time-spec.md); TS-47 and TS-57 for daylight-saving flags, TS-67 to TS-71 |
| Pipe mode (`rrdtool -`) | [pipe-mode.md](compatibility/pipe-mode.md) |
| `rrdcached` requests: `HELP`, `STATS`, `SUSPEND`/`RESUME`, `DUMP`, `TUNE`, `FETCH`, `FETCHBIN` | [rrdcached.md](compatibility/rrdcached.md) RC-20, RC-21, RC-55, RC-110 to RC-112, RC-95, RC-90, RC-70, RC-74 |
| `rrdcached` options, journal and timers | [rrdcached-daemon.md](compatibility/rrdcached-daemon.md); RCD-30 to RCD-32 for the journal |
| Client use of a daemon (`--daemon`, `RRDCACHED_ADDRESS`) | [cli.md](compatibility/cli.md) CLI-41 to CLI-46; [create.md](compatibility/create.md) CR-72, CR-73 |
| `PRINT` and `GPRINT` formats, SI scaling, `strftime` and duration suffixes, `--imginfo` | [format-strings.md](compatibility/format-strings.md) FS-1 to FS-52; [graph.md](compatibility/graph.md) GR-83 |
| Graph options, elements, value range, image size and legend layout | [graph.md](compatibility/graph.md) sections 2 to 11 (GR-60 to GR-73 for layout) |
| `xport` | [graph.md](compatibility/graph.md) section 12 (GR-88 to GR-94) |
| VDEF functions and their arithmetic | [graph.md](compatibility/graph.md) GR-46, GR-49, GR-100 |
| RPN operators in CDEF, VDEF and xport, including sort order with unknowns and infinities | [rpn.md](compatibility/rpn.md) |

## Behavior pinned by tests without a specification rule

These are observed behaviors that Rondi reproduces and tests pin, but that
no specification rule states yet.

- `fetch` chooses an archive by consolidation function, coverage of the
  requested range and resolution, and pads rows outside the archive with
  unknown values. Tests: `fetch_matches_rrdtool_for_ring_order_and_padded_range`
  and `fetch_uses_rrdtool_resolution_choice_across_archives` in
  `crates/rondi/tests/rrd_binary_probe.rs`.
- `update` changes the file in place (same inode) and takes a whole-file
  POSIX record lock that excludes a concurrent RRDtool writer. Tests:
  `rrdtool_update_alias_mutates_the_original_rrd_in_place` and
  `held_rrd_lock_fails_fast_and_honors_rrd_locking_like_rrdtool` in
  `crates/rondi-cli/tests/alias_modes.rs`.
- Differential update tests copy one RRDtool-created file before applying
  updates on both sides, because independently created files differ in
  their random initial rows (CR-46).

## Rondi limits

- The daemon client reads at most 1 MiB for one response line and 64 MiB
  for a multi-line response, so a peer that never sends a newline cannot
  grow the client buffer without limit. Unit tests check acceptance at the
  line limit and rejection past it.
- Known deviations and unsupported features are listed in
  [`COMPATIBILITY_DEFECTS.md`](COMPATIBILITY_DEFECTS.md) and in each
  specification's divergence section.
