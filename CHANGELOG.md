# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- Container image `ghcr.io/kadupulhq/rondi` for linux/amd64 and linux/arm64, built from a root `Dockerfile` and published, attested and signed by the Release workflow.
- Release archives of the `rondi` executable for x86_64 and aarch64 Linux and macOS, with SHA-256 files and build provenance attestations.

## [0.1.0] - 2026-11-10

First release. Rondi is a behavior-compatible implementation of RRDtool
1.11.0, written from documentation, the behavioral specifications in
`docs/compatibility/` and black-box tests against the pinned RRDtool. The
provenance of each part is recorded in `docs/provenance-audit.md`.

### Known limitations

- `create` and `tune` refuse Holt-Winters archives, and `update` refuses Holt-Winters layouts it does not model, leaving the file unchanged. Holt-Winters `create` and `tune` are planned for 0.2.
- The `tune` header options, the `xport` and `graphv` writers in `rondi-cli` and parts of `rrd_binary.rs` predate the clean-room process and are scheduled for respecification and rewrite in 0.2 (see the 2026-11-10 re-audit in `docs/provenance-audit.md`).

### Added

- `COMPUTE` data sources, specified in `docs/compatibility/compute.md` (#144).
- `info`, `dump` and `update` handle Holt-Winters archives (`HWPREDICT`, `MHWPREDICT`, `SEASONAL`, `DEVSEASONAL`, `DEVPREDICT`, `FAILURES`) as RRDtool 1.11.0 does, from `docs/compatibility/holt-winters.md`. `update` refuses Holt-Winters groups it does not model and leaves the file unchanged. `rondi::holt_winters` exposes `Role` and `info_fields`.
- `tune` performs the structure operations `DEL:`, `DS:`, `DELRRA:`, `RRA#n:[+-=]rows`, `RRA:` and `--step` (#91).
- Criterion fetch and graph calculation benchmarks for 120-row and 7,200-row archives (#79).
- Independent-implementation rules for contributors and coding agents in `AGENTS.md` and `docs/PROVENANCE.md`, with a provenance checklist in the pull request template.

### Changed

- Reimplemented clean-room from behavioral specifications, replacing the earlier code: RPN (#109), `create` and `resize` (#114), `restore` (#115), at-style time parsing (#116), option scanning, dispatch and pipe mode (#120), PRINT/GPRINT, duration and dump number formatting (#121), the rrdcached daemon, protocol, cache and journal (#122, #124), graph options, layout and engine (#125, #128, #141), `list` (#143), `update` and `updatev` (#151), and the COUNTER and DERIVE difference (#157).
- The rrdcached client path and address handling and the decimal number conversion derive from permissively licensed RRDtool files and carry their notices in `THIRD-PARTY-NOTICES.md` (#152).
- `RrdCdpPrepInfo` gains a crate-private field holding the raw CDP words, so code outside the crate can no longer build it with a struct literal (breaking API change).
- `tune` structure changes are reimplemented in `tune_modify.rs` from `docs/compatibility/tune-modify.md`, replacing the earlier `rrd_modify.rs`. `modify_rrd_file` now takes a list of `RrdStructureOp` values (breaking API change).
- Rondi is licensed `MIT OR Apache-2.0`. `COPYRIGHT` carries RRDtool's unmodified notice as third-party attribution, and the license text that notice refers to is in `LICENSE-GPL-2.0` (#100).
- Replace the library entry point `run(config)` with `prepare_server(config)`, called before threads start, and `run(prepared)`. Library callers must update (breaking API change) (#80).

### Fixed

- Scale PRINT/GPRINT `%S` and rewrite it in place as RRDtool does, format VDEF times in UTC under `--utc`, paint HRULE and VRULE after the data elements, and fill all-unknown VDEF results with the target's DNAN bits (#79).
- Check Unix socket directory ownership for every daemon, publish the JSON socket from private staging, and keep daemon RRD reads on checked `O_NOFOLLOW` descriptors (#80).
- Refuse root rrdcached without `-B -b` confinement, default Unix sockets to `0600`, restrict command outputs to owned, singly linked regular files created `0600`, and bound XML restore allocations (#81).
- Match rrdtool for `fetch -a -r`, output file errors and non-UTF-8 argv (#82).
- Stop panics on malformed RRD headers and DS bounds (#83).
- Handle RPN count, xport constant and overflow edge cases in graph (#84).
- Refuse device files in rrdcached before mapping an RRD (#85).
- Reject replaceable socket ancestors, walk daemon RRD parents without following symlinks, cap rrdcached CREATE at 64 MiB, and check native journal files before modifying them (#86).
- Allow command output to character devices such as `/dev/null` (#87).
- Print data graphs to stdout when the output file cannot be opened, as RRDtool does (#88).
- Match RRDtool 1.11.0 fetch, data reduction and CDEF/VDEF arithmetic, including aarch64 FMA contraction and the local fallback for an unreachable rrdcached in graph (#92).
- Match rrdtool for `--zoom`, infinite limits, negative `--units-length` and malformed `PREV(`; cap PNG canvases at 2^24 pixels (#93).
- Match RRDtool for dump values, update times past 2^63 and damaged headers (#94).
- Match rrdcached request handler replies for CREATE, FETCH/FETCHBIN, HELP, SUSPEND/RESUME, LIST and TUNE (#95).
- Parse `--daemon` addresses as RRDtool 1.11.0 does, re-parse `tune` argv as RRDtool 1.11.0 does, and stream `rrdtool fetch` past 10 million rows (#96).
- Match rrdcached startup flags and SIGUSR1, SIGUSR2 and SIGHUP handling to RRDtool 1.11.0 (#97).
- Match restore integer field errors, a UTF-8 byte order mark, content after `</rrd>`, and libxml2 error text (#98).
