# rondi-server

The daemon components of [Rondi](https://github.com/kadupulhq/rondi), a Rust reimplementation of RRDtool 1.11.0. It contains two servers:

- an `rrdcached`-compatible adapter that speaks RRDtool's line protocol over Unix and TCP sockets, with upstream-format journals and signal handling;
- Rondi's own HTTP/JSON API over a Unix socket, which journals each update and syncs it to disk before acknowledging it.

Most users want the [`rondi-cli`](https://crates.io/crates/rondi-cli) crate, which runs both through the `rondi` executable. This crate is for embedding them in another program. It builds on Unix only.

## Install

```sh
cargo add rondi-server
```

## Example

Start the JSON API on `./run/rondi.sock`, storing data under `./data`. The `run` directory must exist and belong to the current user.

```rust
use rondi::StoreOptions;
use rondi_server::{ServerConfig, prepare_server, run};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Socket staging changes the working directory, so prepare before any
    // runtime threads exist.
    let prepared = prepare_server(ServerConfig {
        root: "./data".into(),
        socket: "./run/rondi.sock".into(),
        queue_capacity: 256,
        store: StoreOptions::default(),
    })?;
    tokio::runtime::Runtime::new()?.block_on(run(prepared))
}
```

The example also needs `rondi` and `tokio` (with the `rt-multi-thread` feature) as dependencies. `prepare_server` installs a JSON `tracing` subscriber if none is set. `SIGINT` or `SIGTERM` drains accepted writes and removes the socket. With the server running, `rondi --socket ./run/rondi.sock health` reports `"status": "ok"` and `"api_version": 1`. The endpoints and durability guarantees are described in [docs/api.md](https://github.com/kadupulhq/rondi/blob/main/docs/api.md).

## Compatibility

The `rrdcached` adapter targets the request handlers of RRDtool 1.11.0 and is tested against them for CREATE, UPDATE, FETCH and FETCHBIN, FLUSH, LIST, STATS, TUNE and SUSPEND/RESUME. It never forks, ignores `-z` and `-V`, writes a different `-o` log format, and, as upstream, has no authentication on TCP. Rondi also refuses some configurations upstream allows, such as serving as root without `-B -b`. The [compatibility matrix](https://github.com/kadupulhq/rondi/blob/main/docs/compatibility.md) lists the tested subset and the deliberate differences.

## Stability

This crate is at 0.x. Its Rust API can change in any release before 1.0, as described in [VERSIONING.md](https://github.com/kadupulhq/rondi/blob/main/VERSIONING.md). The contract is the wire behavior: rrdcached replies that match RRDtool 1.11.0, and Rondi's versioned `/v1` HTTP API.

## Minimum supported Rust version

The `rust-version` field in the crate manifest, shown on the crates.io page, is the minimum supported Rust version. CI builds with the toolchain pinned in the repository's `.mise.toml`.

## License

Licensed under either of the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option (SPDX `MIT OR Apache-2.0`).

Rondi is an independent implementation that is compatible with RRDtool 1.11.0, copyright Tobias Oetiker and contributors, which is licensed under GPL-2.0-or-later with the RRDtool FLOSS License Exception. Compatibility comes from documentation, behavioral specifications and black-box comparison with RRDtool 1.11.0 ([PROVENANCE](https://github.com/kadupulhq/rondi/blob/main/docs/PROVENANCE.md)). RRDtool's notice is reproduced unchanged in [COPYRIGHT](COPYRIGHT), and the GPL version 2 text in [LICENSE-GPL-2.0](LICENSE-GPL-2.0). RRDtool's code is not relicensed by this project.
