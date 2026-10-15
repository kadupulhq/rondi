# Behavioral specifications

This directory holds implementation-neutral specifications of RRDtool
behavior that Rondi reproduces, one file per area (for example `update.md`).
The specifications are being written.

| Area | Specification | Tests |
|---|---|---|
| RPN expressions in CDEF, VDEF and xport | [rpn.md](rpn.md) | `crates/rondi-cli/tests/rpn_spec_vectors.rs` |
| `rrdtool tune` data source and archive changes | [tune-modify.md](tune-modify.md) | `crates/rondi-cli/tests/tune_modify_spec.rs` |
| `rrdtool restore` from an XML dump | [restore.md](restore.md) | `crates/rondi-cli/tests/restore_spec_vectors.rs` |
| `rrdtool create` definitions, layout and output file | [create.md](create.md) | `crates/rondi-cli/tests/create_spec_vectors.rs` |
| `rrdtool resize` row changes and `resize.rrd` | [resize.md](resize.md) | `crates/rondi-cli/tests/resize_spec_vectors.rs` |
| At-style time specifications, `update` timestamps, `dump` date comments | [time-spec.md](time-spec.md) | `crates/rondi-cli/tests/time_spec_vectors.rs` |
| Command dispatch, option parsing, help, exit status, environment, alias executables, `info`/`fetch`/`list` output | [cli.md](cli.md) | `crates/rondi-cli/tests/cli_spec_vectors.rs` |
| Pipe mode (`rrdtool -`): line reading, quoting, built-in commands, replies | [pipe-mode.md](pipe-mode.md) | `crates/rondi-cli/tests/cli_spec_vectors.rs` |
| `PRINT`/`GPRINT` formats, SI scaling, `strftime` and duration suffixes, `--imginfo` | [format-strings.md](format-strings.md) | `crates/rondi-cli/tests/cli_spec_vectors.rs` |
| `graph`, `graphv` and `xport` options, elements, value range, image size, legend layout, output and limits | [graph.md](graph.md) | `crates/rondi-cli/tests/graph_spec_vectors.rs` |
| `rrdcached` request protocol and commands | [rrdcached.md](rrdcached.md) | `crates/rondi-cli/tests/rrdcached_spec_vectors.rs` |
| `rrdcached` options, listeners, pid file, signals, journal and timers | [rrdcached-daemon.md](rrdcached-daemon.md) | `crates/rondi-cli/tests/rrdcached_spec_vectors.rs` |

Each specification states the observable behavior, how it was established
(documentation, black-box test, or reference-source inspection), and the
tests that cover it. It must not contain RRDtool source excerpts, C line
numbers, or translated pseudocode. See [`../PROVENANCE.md`](../PROVENANCE.md).

The compatibility matrix lives in [`../compatibility.md`](../compatibility.md).
