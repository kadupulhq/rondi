# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- `tune` performs the `rrd_modify` operations `DEL:`, `DS:`, `DELRRA:`, `RRA#n:[+-=]rows`, `RRA:` and `--step` (#91).

### Changed

- Replace the library entry point `run(config)` with `prepare_server(config)`, called before threads start, and `run(prepared)`. Library callers must update (breaking API change) (#80).

### Fixed

- Check Unix socket directory ownership for every daemon, publish the JSON socket from private staging, and keep daemon RRD reads on checked `O_NOFOLLOW` descriptors (#80).
- Refuse root rrdcached without `-B -b` confinement, default Unix sockets to `0600`, restrict command outputs to owned, singly linked regular files created `0600`, and bound XML restore allocations (#81).
- Match rrdtool for `fetch -a -r`, output file errors and non-UTF-8 argv (#82).
- Stop panics on malformed RRD headers and DS bounds (#83).
- Handle RPN count, xport constant and overflow edge cases in graph (#84).
- Refuse device files in rrdcached before mapping an RRD (#85).
- Reject replaceable socket ancestors, walk daemon RRD parents without following symlinks, cap rrdcached CREATE at 64 MiB, and check native journal files before modifying them (#86).
- Allow command output to character devices such as `/dev/null` (#87).
- Print data graphs to stdout when the output file cannot be opened, as `rrd_graph_xport` does (#88).
- Match RRDtool `data_fetch`, `rrd_reduce_data` and CDEF/VDEF arithmetic, including aarch64 FMA contraction and the local fallback for an unreachable rrdcached in graph (#92).
- Match rrdtool for `--zoom`, infinite limits, negative `--units-length` and malformed `PREV(`; cap PNG canvases at 2^24 pixels (#93).
- Match RRDtool for dump values, update times past 2^63 and damaged headers (#94).
- Match rrdcached request handler replies for CREATE, FETCH/FETCHBIN, HELP, SUSPEND/RESUME, LIST and TUNE (#95).
- Parse `--daemon` addresses like `rrd_client.c`, re-parse `tune` argv like `rrd_tune.c`, and stream `rrdtool fetch` past 10 million rows (#96).
- Match rrdcached startup flags and SIGUSR1, SIGUSR2 and SIGHUP handling to `rrd_daemon.c` (#97).
- Match restore integer field errors, a UTF-8 byte order mark, content after `</rrd>`, and libxml2 error text (#98).
