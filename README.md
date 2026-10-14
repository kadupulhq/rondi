# Rondi

[![Security](https://github.com/kadupulhq/rondi/actions/workflows/security.yml/badge.svg?branch=main)](https://github.com/kadupulhq/rondi/actions/workflows/security.yml)
[![CodeQL](https://github.com/kadupulhq/rondi/actions/workflows/codeql.yml/badge.svg?branch=main)](https://github.com/kadupulhq/rondi/actions/workflows/codeql.yml)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](https://github.com/kadupulhq/rondi/pulls)

Rondi is a Rust-native round-robin time-series storage and RRDtool compatibility project for Kadupul. One Cargo workspace and coordinated release contain the storage library, CLI, and daemon. On every surface it shares with RRDtool (the `.rrd` format, the `rrdtool` and `rrdcached` command lines and protocol) the target is the exact behavior of RRDtool 1.11.0; it reimplements that behavior in Rust rather than wrapping librrd.

The native `.rondi` store implements GAUGE values, fixed sampling steps, AVERAGE consolidation, bounded retention, durable server update journaling, and a versioned local HTTP/JSON API over a Unix socket. Separately, the `rrdtool` compatibility alias can create, inspect, fetch, and update a tested subset of existing `.rrd` binary files in place. `.rondi` files are not binary-compatible `.rrd` files.

## Current support

Rondi is pre-alpha. Compatibility is checked against pinned RRDtool 1.11.0 C behavior, including file bytes, command output, errors, and daemon replies on disposable fixtures. Passing those cases does not establish complete drop-in compatibility.

| Surface | Implemented scope | Remaining limits |
| --- | --- | --- |
| Native `.rondi` storage | GAUGE, fixed steps, heartbeat, AVERAGE, bounded retention, journal recovery, local JSON API | Separate format from `.rrd`; no general RRDtool interchange |
| `.rrd` storage and CLI | Tested v3-v5 basic DS/CF layouts; create, update/updatev, fetch, info, first/last, dump/restore, basic tune, v3/v4 resize, stdin command mode | Specialized layouts and complete option/error parity remain incomplete |
| Graph calculations and exports | DEF/CDEF/VDEF/SHIFT, mixed-step data preparation, tested RPN operators, XML/JSON xport, numeric PRINT/GPRINT and SI scaling | Complete graph grammar and edge behavior remain unverified |
| Graph rendering | Native PNG with LINE/AREA/STACK/TICK and rules, tested dimensions, layout keys, colors, and options | Pixels, typography, sampling, vector output, and complete Cacti graph coverage remain incomplete |
| `rrdcached` | Unix/TCP and multiple listeners, privilege drop, daemonization, socket activation, journals, tested update/read/flush/list/stats/suspend commands | Full configuration and lifecycle parity remain incomplete; TCP has no authentication |
| RRDProxy | Direct and PHP launcher aliases with tested help/version/error behavior | Proxy daemon and wire protocol are unimplemented; no librrd C ABI is provided |

The [compatibility matrix](docs/compatibility.md) and [defect register](docs/COMPATIBILITY_DEFECTS.md) record the detailed boundaries. The [roadmap](docs/ROADMAP.md) tracks the work needed for a drop-in replacement.

## Quick start

```sh
mise exec -- cargo run -p rondi-cli -- create temperature --step 10 --heartbeat 25 --rows 6 --start 1700000000
mise exec -- cargo run -p rondi-cli -- update temperature 1700000010 21.5
mise exec -- cargo run -p rondi-cli -- update temperature 1700000020 22
mise exec -- cargo run -p rondi-cli -- fetch temperature
```

Run the daemon and use server mode:

```sh
mise exec -- cargo run -p rondi-cli -- --root ./data server --listen ./run/rondi.sock
mise exec -- cargo run -p rondi-cli -- --socket ./run/rondi.sock update temperature 1700000030 22.5
mise exec -- cargo run -p rondi-cli -- --socket ./run/rondi.sock fetch temperature
```

The daemon and local commands above use the same `./data` store. Run the server in a separate terminal. It publishes its socket with mode `0600`. Both daemons require the socket's parent directory to belong to the account that binds it; group or other write permission requires the sticky bit. An unprivileged process should use its own `./run` directory rather than root-owned `/tmp`. Existing symlinks, non-socket entries, and live sockets are rejected, while stale sockets can be replaced safely.

JSON startup binds and sets permissions inside a private staging directory before publishing the socket with `renameat`. Every socket ancestor must be owned by root or the binding account; shared writable ancestors require the sticky bit to protect directory entries. Daemon RRD reads and writes walk parent directories through descriptors without following symlinks, and CREATE publishes through its opened parent. Native journals must be regular files owned by the daemon, have exactly one link, and have no group or other write permission before recovery, append, or compaction can modify them. rrdcached CREATE accepts at most 64 MiB of encoded archive data and headers per operation. These safeguards deliberately differ from upstream. Library callers prepare the JSON server with `prepare_server(config)` before starting threads, then pass the result to async `run`; see [API and durability](docs/api.md).

See [architecture](docs/architecture.md), [API and durability](docs/api.md), and [compatibility](docs/compatibility.md). Rondi is not currently a drop-in replacement. Its known compatibility defects are tracked in [docs/COMPATIBILITY_DEFECTS.md](docs/COMPATIBILITY_DEFECTS.md). The plan for closing them is in [docs/ROADMAP.md](docs/ROADMAP.md).

Build the single executable and create the requested compatibility symlink names with:

```sh
mise exec -- cargo build --release -p rondi-cli --locked
scripts/install-compat-links.sh "$PWD/target/release/rondi" "$HOME/.local/bin"
```

The names are installed for packaging and mode selection; legacy-compatible behavior remains incomplete as listed in the defect register.

The installer also installs `rrdtool-proxy.php` for service definitions that invoke the proxy through PHP. It delegates to the neighboring `rrdtool-proxy` alias; set `RONDI_BIN` when the executable is installed elsewhere. This adapter currently covers only the pinned launcher help/version/invalid-option responses, not the proxy daemon or wire protocol.

For systemd deployments, `packaging/systemd/rondi-rrdcached.service` is the packaged security profile. Provision a dedicated unprivileged `rondi` user and group first, install the compatibility symlink as `/usr/bin/rrdcached`, and enable the unit. It explicitly sets `-m 0660 -s rondi` and keeping the journal outside the base directory with `-j /var/lib/rondi-journal`; the service data and socket directories are group-owned and unavailable to other users. Do not use this profile with a shared `/tmp` base directory.

The unauthenticated `rrdcached` adapter refuses to serve as root unless `-B` with `-b` confines its paths. A Unix socket defaults to `0600`; use `-m` or `-s` to explicitly configure broader access.

Graph, export, dump, restore, and resize outputs reject final symlinks, multiple hard links, special files, and files owned by another user. New outputs are mode `0600` less the umask. A failed forced restore preserves a pre-existing destination, although an I/O failure can leave partially written contents. XML restore is limited to 4096 data sources, 4096 archives, and one million rows and values; oversized dumps fail before destination creation. These are deliberate safety differences from RRDtool.

The current `rrdtool` alias supports tested fetch and update paths against version 0003-0005 `.rrd` layouts on 64-bit little-endian targets. Time parsing includes tested RRDtool absolute, relative, and calendar forms. It also accepts RRDtool's stdin command mode (`rrdtool -`), used by Kadupul's poller, with tested `update --template` handling. For example:

```sh
rrdtool fetch /tmp/sample.rrd AVERAGE --start 1700000000 --end 1700003600 --resolution 300
rrdtool update /tmp/sample.rrd 1700003610:42.5
rrdtool update /tmp/sample.rrd --daemon unix:/run/rrdcached.sock 1700003620:43
rrdtool updatev /tmp/sample.rrd 1700003630:43.5
rrdtool xport --start 1700000000 --end 1700003600 --step 300 DEF:load=/tmp/sample.rrd:traffic_in:AVERAGE XPORT:load:Traffic
rrdtool graph /tmp/sample.png --imgformat=PNG --width 400 --height 100 --start 1700000000 --end 1700003600 DEF:load=/tmp/sample.rrd:traffic_in:AVERAGE LINE1:load#00aa44:Traffic
rrdtool tune /tmp/sample.rrd --heartbeat traffic_in:600 --minimum traffic_in:0
printf 'update /tmp/sample.rrd --template traffic_in:traffic_out 1700003640:100:200\nquit\n' | rrdtool -
```

For supported basic data sources, `tune` also accepts `--maximum`, `--data-source-type`, and `--data-source-rename`.

`resize` supports tested v3/v4 archives and follows RRDtool's fixed output filename. Run it from the directory where `resize.rrd` should be created; the destination must not already exist:

```sh
cd /tmp
rrdtool resize sample.rrd 0 GROW 10
```

The v5 resize path and uncommon archive layouts remain unsupported or unverified.

The tested `dump`/`restore` subset can also round-trip basic v3/v5 files through RRDtool XML:

```sh
rrdtool dump /tmp/sample.rrd /tmp/sample.xml
rrdtool restore /tmp/sample.xml /tmp/sample-restored.rrd
```

Restore stages a new output file and refuses to replace an existing path unless `--force-overwrite` is specified. XML from specialized data-source or archive types remains unsupported.

Fetch and `xport` use shared RRDtool-compatible locking. Updates use exclusive locking and support one value per basic data source with AVERAGE, MIN, MAX, or LAST archives. The raw-decimal update path preserves COUNTER/DERIVE input precision above 2^53; float-only library callers remain limited to exactly representable integers.

Graph data preparation supports per-DEF windows and steps, consolidation, dependent CDEFs, VDEF substitutions, and ordered SHIFT operations. Differential tests exercise RPN numeric parsing, stack operations, unknown handling, trend/prediction calculations, numeric PRINT/GPRINT formatting, and SI scaling. Native PNG rendering supports the primitives and limits listed above, but exact RRDtool images remain a compatibility gap. See the [compatibility matrix](docs/compatibility.md) for the tested subsets.

## Performance measurements

Run the checked-in workloads with the pinned toolchain:

```sh
mise exec -- cargo bench -p rondi --bench workload
mise exec -- cargo bench -p rondi --bench rrd_workload
```

`workload` measures durable native-store updates and fetches. `rrd_workload` compares individual and batched `.rrd` updates, checks identical file bytes, and measures 1,440-row fetches. These are local elapsed-time measurements rather than statistical performance guarantees. Recorded environments and native-store baseline results are in [benchmarks](docs/benchmarks.md).

## Continuous integration and Docker validation

GitHub Actions runs the Rust checks on Linux x86_64 and ARM64. It builds the pinned RRDtool 1.11.0 oracle from its upstream release tarball, verifies the tarball SHA-256, and runs formatting, Clippy, the full workspace test suite, documentation generation, and release builds in the same container. This ensures the RRDtool differential tests execute instead of being skipped because the oracle is missing. A separate fuzz job builds every target and replays committed seeds; its RPN differential target needs a local RRDtool oracle and is not replayed in that job. The security workflow runs Semgrep and RustSec `cargo-audit` against `Cargo.lock` and `cargo-deny` against dependency advisories, licenses, duplicate versions, and registry sources. CodeQL is configured for Rust and GitHub Actions; GitHub only runs its code scanning upload where repository visibility and plan settings enable it.

Run the same checks locally in a disposable Linux container with:

```sh
docker build --progress=plain -f docker/Dockerfile.test -t rondi-test .
```

The test image builds RRDtool 1.11.0 from the upstream release tarball and verifies its SHA-256 before compiling it as the differential oracle.

To run the checked-in Kadupul poller stream smoke against disposable files (with the Kadupul checkout next to this repository by default):

```sh
mise exec -- bash scripts/smoke-kadupul-poller.sh
```

Set `KADUPUL_ROOT`, `RRDTOOL_BIN`, or `RONDI_BIN` to override the checkout or executable paths. This exercises Kadupul's real `rrd_init()` process/session helper and update-template command stream; it does not run the database-backed poller loop.

## Releases

Version tags (`vMAJOR.MINOR.PATCH`, with optional SemVer prerelease or build metadata) use GitHub-generated release notes, grouped by the pull request labels in [.github/release.yml](.github/release.yml). Label pull requests with `enhancement`, `bug`, or `documentation` so they appear under the matching section; unlabeled changes appear under “Other Changes.”

## Contributing

Changes are reviewed through pull requests using the repository's [pull request template](.github/PULL_REQUEST_TEMPLATE.md). Report compatibility differences with the exact RRDtool version, command, output, exit status, and a disposable `.rrd` fixture where possible.

## License

GPL-2.0-or-later with the RRDtool FLOSS License Exception (SPDX `GPL-2.0-or-later WITH RRDtool-FLOSS-exception-2.0`), the same terms as RRDtool 1.11.0. See [COPYRIGHT](COPYRIGHT) for the notice and exception text, copied from RRDtool's `COPYRIGHT`, and [LICENSE](LICENSE) for the GPL version 2 text.
