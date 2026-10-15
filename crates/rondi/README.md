# rondi

The storage library behind [Rondi](https://github.com/kadupulhq/rondi), a Rust reimplementation of RRDtool 1.11.0. It reads and writes RRDtool `.rrd` files in place, prepares graph data (DEF, CDEF, VDEF, RPN, xport), and provides Rondi's own durable `.rondi` store. It does not link librrd or call the `rrdtool` program.

The command-line tool is in [`rondi-cli`](https://crates.io/crates/rondi-cli) and the daemons are in [`rondi-server`](https://crates.io/crates/rondi-server).

## Install

```sh
cargo add rondi
```

## Example

Create an `.rrd` file, update it, and fetch the averages back:

```rust
use rondi::{create_rrd_file, fetch_rrd_file, update_rrd_values};

fn main() -> Result<(), rondi::StoreError> {
    let path = std::env::temp_dir().join("sample.rrd");
    let _ = std::fs::remove_file(&path);

    create_rrd_file(
        &path,
        1_700_000_100,
        300,
        &["DS:traffic_in:GAUGE:600:0:U".to_string()],
        &["RRA:AVERAGE:0.5:1:12".to_string()],
        false,
    )?;
    update_rrd_values(&path, 1_700_000_400, &[Some(42.0)])?;
    update_rrd_values(&path, 1_700_000_700, &[Some(43.0)])?;

    let fetched = fetch_rrd_file(&path, "AVERAGE", 1_700_000_100, 1_700_000_700, 300)?;
    for row in &fetched.rows {
        println!("{}: {:?}", row.timestamp, row.values);
    }
    Ok(())
}
```

It prints:

```text
1700000400: [Some(42.0)]
1700000700: [Some(43.0)]
1700001000: [None]
```

The same create, update and fetch run through RRDtool 1.11.0 returns the same rows.

## Compatibility

The contract is the behavior of RRDtool 1.11.0: file bytes, values, and error text. Rondi checks this by running the same input through both implementations. The tested subset covers version 0003 to 0005 `.rrd` layouts on 64-bit little-endian targets, the GAUGE, COUNTER, DERIVE, ABSOLUTE, DCOUNTER and DDERIVE source types, and AVERAGE, MIN, MAX and LAST archives. COMPUTE sources and Holt-Winters archives are refused. The [compatibility matrix](https://github.com/kadupulhq/rondi/blob/main/docs/compatibility.md) lists what is tested and what is not, and the [defect register](https://github.com/kadupulhq/rondi/blob/main/docs/COMPATIBILITY_DEFECTS.md) lists known differences.

Rondi is pre-alpha and not a drop-in replacement for librrd.

## Stability

This crate is at 0.x. Its Rust API can change in any release, including a patch release before 1.0, as described in [VERSIONING.md](https://github.com/kadupulhq/rondi/blob/main/VERSIONING.md). What a release does promise is behavior parity with RRDtool 1.11.0 on the surfaces the compatibility matrix lists; a change that moves closer to that behavior is treated as a fix.

## Minimum supported Rust version

The `rust-version` field in the crate manifest, shown on the crates.io page, is the minimum supported Rust version. CI builds with the toolchain pinned in the repository's `.mise.toml`.

## License

Licensed under either of the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option (SPDX `MIT OR Apache-2.0`).

Rondi ports behavior and code from RRDtool 1.11.0, copyright Tobias Oetiker and contributors, which is licensed under GPL-2.0-or-later with the RRDtool FLOSS License Exception. RRDtool's notice is reproduced unchanged in [COPYRIGHT](COPYRIGHT), and the GPL version 2 text in [LICENSE-GPL-2.0](LICENSE-GPL-2.0). RRDtool's code is not relicensed by this project.
