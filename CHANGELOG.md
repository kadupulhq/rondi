# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- `tune` performs the structure operations `DEL:`, `DS:`, `DELRRA:`, `RRA#n:[+-=]rows`, `RRA:` and `--step` (#91).
- Criterion fetch and graph calculation benchmarks for 120-row and 7,200-row archives (#79).
- Independent-implementation rules for contributors and coding agents in `AGENTS.md` and `docs/PROVENANCE.md`, with a provenance checklist in the pull request template.

### Changed

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
