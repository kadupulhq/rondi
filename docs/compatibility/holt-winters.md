# Holt-Winters archives

Status: draft, stage 1 of 3 (create-time state, `info` and `dump`). Stage 2
covers `update`; stage 3 covers `tune`. Until all three are done this file
is not a complete specification.

This specification describes what RRDtool 1.11.0 does with the Holt-Winters
archives `HWPREDICT`, `MHWPREDICT`, `SEASONAL`, `DEVSEASONAL`, `DEVPREDICT`
and `FAILURES`. The definition grammar is in [`create.md`](create.md)
section 8 (CR-32 to CR-36); the byte layout of the parameter and CDP slots
is in [`restore.md`](restore.md) RS-53. This document adds what those leave
out.

Provenance tags and classes are as in `create.md`: **documentation**
(`rrdcreate(1)`, `rrdtune(1)`, `rrdgraph(1)`), **published method**
(J. Brutlag, "Aberrant Behavior Detection in Time Series for Network
Monitoring", USENIX LISA 2000), **black-box** (observed with
`/opt/homebrew/bin/rrdtool` 1.11.0 on macOS arm64 and the pinned 1.11.0
build on Linux), and **reference-source-inspection** (none in this
document).

## 1. State written by create

**HW-1** (black-box; must). A newly created file holds, for each data
source, this CDP state in each Holt-Winters archive (slot names as in
RS-53; every other slot zero):

| Archive | State |
|---|---|
| `HWPREDICT`, `MHWPREDICT` | intercept, last intercept, slope, last slope unknown; NaN count 1; last NaN count 1 |
| `SEASONAL` | seasonal, last seasonal unknown; init flag 1 |
| `DEVSEASONAL` | deviation (slot 2) and last deviation unknown; init flag 1 |
| `DEVPREDICT` | none |
| `FAILURES` | failure history all zero (`000000000` for window length 9) |

Rows are all unknown, as for classic archives (CR-44).

**HW-2** (black-box; should). The `seasonal_smooth_idx` parameter of
`SEASONAL` and `DEVSEASONAL` is set at create to a value in
[0, seasonal period). RRDtool's choice is the same on macOS and Linux and
does not depend on the clock, the start time, the step, the row counts or
the inode, but it does depend on the output file name (`s.rrd`, `h3.rrd`,
`abc.rrd` give different values for the same definitions). The function is
not specified here. A replacement may choose any value in range;
comparisons of created files mask this slot as they mask the current rows
(CR-46). See "Open questions".

## Open questions

- HW-2: the exact derivation of the initial `seasonal_smooth_idx`. It
  could not be settled black-box: it is stable for a given file name but
  not a simple function of the name's characters. Whether byte-identical
  create output for Holt-Winters files is a requirement is a maintainer
  decision; if it is, this is a candidate for reference-source inspection
  on the spec side under AGENTS.md.

## Vectors

`crates/rondi-cli/tests/hw_spec_vectors.rs` replays the `hw` suite
(`crates/rondi-cli/tests/data/hw`), which `scripts/gen-create-resize-vectors.py`
records. The inputs are fresh files with each Holt-Winters archive type and
smoothing window, and one file that has already been updated. The cases
read them (`info`, `dump`, `fetch` of each archive) and update them with
seasonal, spiking, gapped, irregular and multi-source series, so the
resulting files pin every archive and CDP slot for stage 2. Until Rondi
implements these archives, the cases that read or write their state are
expected to be refused; the `fetch` cases already match.
