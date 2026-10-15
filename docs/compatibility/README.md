# Behavioral specifications

This directory holds implementation-neutral specifications of RRDtool
behavior that Rondi reproduces, one file per area (for example `update.md`).
The specifications are being written.

| Area | Specification | Tests |
|---|---|---|
| RPN expressions in CDEF, VDEF and xport | [rpn.md](rpn.md) | `crates/rondi-cli/tests/rpn_spec_vectors.rs` |
| `rrdtool tune` data source and archive changes | [tune-modify.md](tune-modify.md) | `crates/rondi-cli/tests/tune_modify_spec.rs` |
| `rrdtool restore` from an XML dump | [restore.md](restore.md) | `crates/rondi-cli/tests/restore_spec_vectors.rs` |

Each specification states the observable behavior, how it was established
(documentation, black-box test, or reference-source inspection), and the
tests that cover it. It must not contain RRDtool source excerpts, C line
numbers, or translated pseudocode. See [`../PROVENANCE.md`](../PROVENANCE.md).

The compatibility matrix lives in [`../compatibility.md`](../compatibility.md).
