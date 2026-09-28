# Rondi

Rondi is the working name for a Rust-native round-robin storage foundation for Kadupul. One Cargo workspace and coordinated release contain the storage library, CLI, and daemon. The goal is to replace RRDtool/librrd implementations over time, not to wrap librrd permanently.

The native `.rondi` store implements GAUGE values, fixed sampling steps, AVERAGE consolidation, bounded retention, durable server update journaling, and a versioned local HTTP/JSON API over a Unix socket. Separately, the `rrdtool` compatibility alias can create, inspect, fetch, and update a tested subset of existing `.rrd` binary files in place. `.rondi` files are not binary-compatible `.rrd` files.

## Quick start

```sh
cargo run -p rondi-cli -- create temperature --step 10 --heartbeat 25 --rows 6 --start 1700000000
cargo run -p rondi-cli -- update temperature 1700000010 21.5
cargo run -p rondi-cli -- update temperature 1700000020 22
cargo run -p rondi-cli -- fetch temperature
```

Run the daemon and use server mode:

```sh
cargo run -p rondi-cli -- --root ./data server --listen ./run/rondi.sock
cargo run -p rondi-cli -- --socket ./run/rondi.sock update temperature 1700000030 22.5
cargo run -p rondi-cli -- --socket ./run/rondi.sock fetch temperature
```

See [architecture](docs/architecture.md), [API and durability](docs/api.md), and [compatibility](docs/compatibility.md). Rondi is not currently a drop-in replacement. Its known compatibility defects are tracked in [docs/COMPATIBILITY_DEFECTS.md](docs/COMPATIBILITY_DEFECTS.md).

Build the single executable and create the requested compatibility symlink names with:

```sh
mise exec -- cargo build --release -p rondi-cli --locked
scripts/install-compat-links.sh "$PWD/target/release/rondi" "$HOME/.local/bin"
```

The names are installed for packaging and mode selection; legacy-compatible behavior remains incomplete as listed in the defect register.

The installer also installs `rrdtool-proxy.php` for service definitions that invoke the proxy through PHP. It delegates to the neighboring `rrdtool-proxy` alias; set `RONDI_BIN` when the executable is installed elsewhere. This adapter currently covers only the pinned launcher help/version/invalid-option responses, not the proxy daemon or wire protocol.

For systemd deployments, `packaging/systemd/rondi-rrdcached.service` is the packaged security profile. Provision a dedicated unprivileged `rondi` user and group first, install the compatibility symlink as `/usr/bin/rrdcached`, and enable the unit. It keeps the alias's upstream-compatible defaults unchanged while explicitly setting `-m 0660 -s rondi`; the service data and socket directories are group-owned and unavailable to other users. Do not use this profile with a shared `/tmp` base directory.

The current `rrdtool` alias supports narrow numeric-time `fetch` and `update` paths against existing version 0003-0005 `.rrd` layouts on 64-bit little-endian targets. It also accepts RRDtool's stdin command mode (`rrdtool -`), used by Kadupul's poller, with tested `update --template` handling. For example:

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

Fetch and `xport` use shared RRDtool-compatible locking. Update uses an exclusive RRDtool-compatible lock and accepts one value per supported data source, with one or more AVERAGE, MIN, MAX, or LAST archives. The raw-decimal update path preserves COUNTER/DERIVE input precision above 2^53; callers using the float-only library convenience API remain limited to exactly representable integers. `xport` supports DEF/XPORT XML and JSON exports and a row-wise CDEF/RPN subset covering arithmetic, comparisons, `IF`, numeric functions, stack operators, aggregate functions, and unknown handling. A byte-for-byte differential fixture exercises these operators against RRDtool 1.11.0. VDEF graph expressions remain partial. The `graph` command can write native PNG for a basic LINE/AREA/TICK/HRULE/VRULE subset, including STACK baselines and unknowns, alpha colors, AREA gradients, rule/line dashes, graph layout modes, ten color overrides, and basic limits. Selected option behavior and output dimensions are exercised against pinned RRDtool, but its pixels, typography, data-to-pixel mapping, and broader graph grammar still differ. The complete RPN vocabulary and some mixed-resolution and edge behavior remain unsupported or unverified. This does not imply full file, CLI, graph, or protocol compatibility; see the [compatibility matrix](docs/compatibility.md).

## Continuous integration and Docker validation

GitHub Actions builds the pinned RRDtool 1.11.0 oracle from its upstream release tarball, verifies the tarball SHA-256, and runs formatting, Clippy, the full workspace test suite, documentation generation, and release builds in the same container. This ensures the RRDtool differential tests execute instead of being skipped because the oracle is missing.

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
