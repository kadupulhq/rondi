# Behavioral specifications

This directory holds implementation-neutral specifications of RRDtool
behavior that Rondi reproduces, one file per area (for example `update.md`).
The specifications are being written; none are published yet.

Each specification states the observable behavior, how it was established
(documentation, black-box test, or reference-source inspection), and the
tests that cover it. It must not contain RRDtool source excerpts, C line
numbers, or translated pseudocode. See [`../PROVENANCE.md`](../PROVENANCE.md).

The compatibility matrix lives in [`../compatibility.md`](../compatibility.md).
