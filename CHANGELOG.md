# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](VERSIONING.md).

## [Unreleased]

### Added

- Add Criterion fetch and graph calculation benchmarks for 120-row and 7,200-row archives.

### Changed

- License Rondi under `MIT OR Apache-2.0`, replacing `GPL-2.0-or-later WITH RRDtool-FLOSS-exception-2.0`. `COPYRIGHT` now carries RRDtool's unmodified notice as third-party attribution, and the GPL version 2 text moves to `LICENSE-GPL-2.0`.
- Replace the library entry point `run(config)` with `prepare_server(config)`, called before threads start, and `run(prepared)`. Library callers must update (breaking API change).

### Fixed

- Fill all-unknown VDEF aggregates with RRDtool's target-specific DNAN bits.
- Scale PRINT/GPRINT values when `%%S` or `%%s` appears literally, and rewrite `%S` in place so graphv metadata rescales and graph format errors quote the rewritten text like RRDtool.
- Format PRINT/GPRINT VDEF times in UTC under `--utc`.
- Paint HRULE and VRULE after the data elements regardless of their script position.
- Reject Unix socket paths with ancestors another account can replace, and traverse daemon RRD parents through directory descriptors without following symlinks.
- Bound rrdcached CREATE allocations to 64 MiB, including headers, and check archive size arithmetic before allocating. Local `rrdtool create` keeps upstream's unbounded size.
- Validate native journal ownership, file type, link count, and group or other write permission before recovery, append, or compaction can modify it.
- Refuse root rrdcached startup without effective `-B -b` confinement and default Unix sockets to `0600` unless explicitly configured.
- Restrict output writes to owned, singly linked regular files and create new outputs with mode `0600`.
- Require a new `resize.rrd` destination, preserve existing forced-restore destinations on write failure, and bound XML restore allocations.
- Check Unix socket directory ownership and shared-write permissions for unprivileged daemons as well as root.
- Publish the JSON daemon socket from private staging with mode `0600` already set.
- Keep daemon RRD reads on checked `O_NOFOLLOW` descriptors through inspection, fetch, and dump; reuse checked descriptors for update and tune metadata.
- Limit the initial CodeQL matrix to workflow analysis until application source is added.
- Handle SIGUSR1 (shut down after flushing), SIGUSR2 (shut down without flushing), and SIGHUP (ignored) in rrdcached as upstream does, removing the pid file and sockets on exit.
- Validate rrdcached `-b` during option parsing with upstream messages and exit statuses, and report an unusable base directory without Rust debug formatting.
- Repeat an unknown rrdcached `-P` permission name as given instead of uppercased.
- Report an uncreatable rrdcached pid file directory twice and end with `FATAL: Fail to create/open PID file`, as check_pidfile does.
- Without `-o`, write only error-level rrdcached log lines, to stderr instead of stdout, matching upstream's default foreground logging.
