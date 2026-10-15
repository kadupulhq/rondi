# rondi-cli

The `rondi` executable from [Rondi](https://github.com/kadupulhq/rondi), a Rust reimplementation of RRDtool 1.11.0. One binary provides Rondi's own commands and, when invoked under the names `rrdtool`, `rrdcached`, `rrdupdate`, `rrdcreate` or `rrdinfo`, the matching RRDtool command line. It does not link librrd.

## Install

```sh
cargo install rondi-cli
```

This installs the `rondi` binary. To use the RRDtool command line, link it under the RRDtool names somewhere on your `PATH`:

```sh
ln -s "$(command -v rondi)" ~/.local/bin/rrdtool
ln -s "$(command -v rondi)" ~/.local/bin/rrdcached
```

The repository's `scripts/install-compat-links.sh` creates the full set of names and the `rrdtool-proxy.php` launcher; it is not part of this crate.

## Example

As `rrdtool`:

```sh
rrdtool create sample.rrd --start 1700000100 --step 300 \
    DS:traffic_in:GAUGE:600:0:U RRA:AVERAGE:0.5:1:12
rrdtool update sample.rrd 1700000400:42 1700000700:43
rrdtool fetch sample.rrd AVERAGE --start 1700000100 --end 1700000700
```

```text
                     traffic_in

1700000400: 4.2000000000e+01
1700000700: 4.3000000000e+01
1700001000: nan
```

RRDtool 1.11.0 prints the same bytes for these commands.

As `rondi`, with the native `.rondi` store under `./data`:

```sh
rondi create temperature --step 10 --heartbeat 25 --rows 6 --start 1700000000
rondi update temperature 1700000010 21.5
rondi fetch temperature
```

`rondi server --listen ./run/rondi.sock` serves the same store over a Unix socket, and `rondi --socket ./run/rondi.sock ...` sends commands to it.

## Compatibility

The contract is the behavior of RRDtool 1.11.0, including output, error text and exit status. Rondi checks it by running the same commands through both implementations. Supported today: create, update and updatev, fetch, info, first, last and lastupdate, dump and restore, list, tune (including `rrd_modify` changes), v3 and v4 resize, xport, graph data export, PNG graphs, flushcached, the `rrdtool -` stdin mode, and a tested subset of `rrdcached`. Known gaps include COMPUTE sources, Holt-Winters archives, SVG, PDF and EPS output, PNG bytes and fonts, and the RRDProxy daemon. The [compatibility matrix](https://github.com/kadupulhq/rondi/blob/main/docs/compatibility.md) and [defect register](https://github.com/kadupulhq/rondi/blob/main/docs/COMPATIBILITY_DEFECTS.md) have the details.

Rondi is pre-alpha and not a drop-in replacement for RRDtool.

## Stability

This crate is at 0.x. Before 1.0, Rondi's own subcommands, flags and output can change in a minor release, as described in [VERSIONING.md](https://github.com/kadupulhq/rondi/blob/main/VERSIONING.md). The RRDtool-compatible command lines follow RRDtool 1.11.0; a change that moves closer to it is a fix.

## Minimum supported Rust version

The `rust-version` field in the crate manifest, shown on the crates.io page, is the minimum supported Rust version. CI builds with the toolchain pinned in the repository's `.mise.toml`.

## License

Licensed under either of the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option (SPDX `MIT OR Apache-2.0`).

Rondi ports behavior and code from RRDtool 1.11.0, copyright Tobias Oetiker and contributors, which is licensed under GPL-2.0-or-later with the RRDtool FLOSS License Exception. The help text this binary prints is RRDtool's, reproduced verbatim with its notice. RRDtool's notice is reproduced unchanged in [COPYRIGHT](COPYRIGHT), and the GPL version 2 text in [LICENSE-GPL-2.0](LICENSE-GPL-2.0). RRDtool's code is not relicensed by this project.
