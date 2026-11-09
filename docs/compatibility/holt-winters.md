# Holt-Winters archives

Status: draft. Stage 1 (create-time state) and stage 2 (`update` for
runs of one PDP, section 2) are written; stage 2b (runs of two or more
PDPs, section 3) is recorded but not yet specified, and stage 3 (`tune`) is
not started. Until those are done this file is not a complete
specification.

This specification describes what RRDtool 1.11.0 does with the Holt-Winters
archives `HWPREDICT`, `MHWPREDICT`, `SEASONAL`, `DEVSEASONAL`, `DEVPREDICT`
and `FAILURES`. The definition grammar is in [`create.md`](create.md)
section 8 (CR-32 to CR-36); the byte layout of the parameter and CDP slots
is in [`restore.md`](restore.md) RS-53. This document adds what those leave
out.

Terms are those of [`update.md`](update.md) (step *S*, PDP, run, CDP
state, *primary_value*, fma, *unknown*). Provenance tags and classes are as in `create.md`: **documentation**
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

## 2. Update: runs of one PDP

This section covers PDPs consolidated in runs of length one (`update.md`
UP-23): every update that completes exactly one step, and the first PDP
*F* of a split update. Section 3 covers longer runs.

**HW-10** (black-box; must). The Holt-Winters archives of a file are
updated together, PDP by PDP, after the PDP's value *x* is known
(`update.md` UP-21, UP-22; *x* is the PDP value, not the reading). For
each DS independently, one PDP is processed in the order HW-11 to HW-17,
across all six archive types at once; HW-17 records triggers, and HW-18
runs at most once per `update` command, after its last argument. The archives are linked by `dependent_rra_idx`
(`create.md` CR-35); a file has at most one archive of each type. Classic
archives are consolidated as `update.md` section 7 says, independently.

Let *I*, *I'*, *T*, *T'*, *n*, *n'* be the prediction archive's CDP
`intercept`, `last_intercept`, `slope`, `last_slope`, `nan_count` and
`last_nan_count` (RS-53 slots 2 to 7), *σ* and *δ* the SEASONAL and
DEVSEASONAL CDP slot 2 (`seasonal`, the *prefetched* coefficient and
deviation), α and β the prediction archive's `hw_alpha` and `hw_beta`,
γs and γd the SEASONAL and DEVSEASONAL `seasonal_gamma`, *P* the seasonal
period (the SEASONAL row count), and *blend*(*w*, *a*, *b*) =
*w*·*a* + (1 − *w*)·*b* with the rounding of HW-19. For `HWPREDICT`,
*a* ⊖ *b* = *a* − *b* and *a* ⊗ *b* = *a* + *b*; for `MHWPREDICT`,
*a* ⊖ *b* = *a* / *b* and *a* ⊗ *b* = *a* × *b*.

**HW-11** (black-box; must). Row positions. Every Holt-Winters archive
advances its own current row by one per PDP and writes that row
(`update.md` UP-30). The SEASONAL and DEVSEASONAL rows written are the
coefficient positions; their current rows start wherever `create`'s random
choice put them (CR-46), so position 0 of the season is not tied to time.

**HW-12** (black-box; must). Prediction and learning. Let *s* = *σ* and
*d* = *δ*, the values prefetched by the previous PDP (HW-16), and set
*n'* = *n*.

1. If *I* is unknown (no state yet): when *x* is known, *I* = *I'* = *x*
   and *T* = *T'* = 0. The prediction *p* is unknown, and the new
   coefficient is *s'* = *x* ⊖ *I* (so 0 or 1 for the first value; unknown
   when *x* is unknown).
2. Else if *s* is unknown (the first season): *p* is unknown, *I'* = *I*,
   *T'* = *T*, *I* and *T* are unchanged, and *s'* = *x* ⊖ *I*.
3. Otherwise let *base* = *I* + *n*·*T* (an fma, HW-19) and *p* =
   *base* ⊗ *s*; set *I'* = *I* and *T'* = *T*. When *x* is known:
   *I* = *blend*(α, *x* ⊖ *s*, *base*), then *T* = *blend*(β, *I* − *I'*,
   *T'*), then *s'* = *blend*(γs, *x* ⊖ *I*, *s*) with the new *I*, and
   *n* = 1. When *x* is unknown: *n* = *n* + 1, *s'* = *s*, and *I*, *T*
   are unchanged. So after a gap of *k* unknown PDPs the trend is
   projected *k* + 1 steps (vectors `022` to `031`, `055` to `057`,
   `060`; the replayed model also matches per-PDP recordings with gaps,
   which the vectors freeze only in runs, section 3).

**HW-13** (black-box; must). Deviation. With the *p* of HW-12: when *x* is
unknown, *d'* = *d*; else when *p* is unknown, *d'* is unknown; else when
*d* is unknown, *d'* = |*x* − *p*|; otherwise *d'* = *blend*(γd,
|*x* − *p*|, *d*). The deviation is |*x* − *p*| for `MHWPREDICT` too.

**HW-14** (black-box; must). Failures. Let δ₊, δ₋ be FAILURES
`delta_pos` and `delta_neg`, *w* its `window_len` and *k* its
`failure_threshold`. The PDP is a violation (1) when *p* and *d* are
known and either *x* is unknown or *x* > *p* + δ₊·*d* or
*x* < *p* − δ₋·*d* (fma on fused platforms); a value exactly on a bound is
not a violation (vectors `062` and `064` are on a bound, `063` and `065`
just outside it). Otherwise it is 0. The FAILURES
CDP holds the last *w* results as bytes from byte 0 of the CDP, newest
first; each PDP shifts them one byte up, dropping the oldest, and stores
the new result in byte 0. The row value is 1.0 when at least *k* of the
*w* bytes are 1, else 0.0.

**HW-15** (black-box; must). Rows and *primary_value*. Each archive writes
one row per PDP and sets its CDP *primary_value* (word 8) to the value
written; *secondary_value* is unchanged:

| Archive | Row value |
|---|---|
| `HWPREDICT`, `MHWPREDICT` | *p* (unknown while HW-12 cases 1 and 2 apply) |
| `SEASONAL` | *s'* |
| `DEVSEASONAL` | *d'* |
| `DEVPREDICT` | *d* (the deviation used, before this PDP's update) |
| `FAILURES` | the HW-14 flag |

**HW-16** (black-box; must). Prefetch. After the writes, SEASONAL CDP
slot 3 (`last_seasonal`) becomes *s* and slot 2 (`seasonal`) becomes the
value of the SEASONAL row after the one just written (position
*r* + 1 mod *P*); DEVSEASONAL slots 3 and 2 likewise with *d*. The next PDP
uses these slot values, not the rows, so a smoothing pass (HW-18) that
rewrites the rows does not change the coefficient the next PDP uses.

**HW-17** (black-box; must). Season counter. SEASONAL and DEVSEASONAL
CDP slot 6 (`init_flag`) starts at 1 (HW-1). When an archive's current row
becomes 0 and its counter is below 4, the counter increases by one and the
archive *triggers*. When the counter is 4 and the current row becomes the
archive's `seasonal_smooth_idx`, the archive triggers. The counter is the
same for every DS.

**HW-18** (black-box; must). Smoothing pass. When either archive has
triggered on any PDP of an `update` command (arguments given together
count as one command), one pass runs after the command's last argument
has been applied, however many triggers there were; within the command,
later PDPs see the rows and *I* as they were before the pass. Each DS's
SEASONAL and DEVSEASONAL rows are smoothed:

- The half-width is *h* = trunc(*f*·*P* / 2) for the archive's
  `smoothing_window` *f* (*f*·*P* and the division in doubles); the window
  is 2*h* + 1 rows, centred and circular. When *h* = 0 that archive is not
  smoothed at all. This includes *f* = 0, the value an implicit definition
  (`HWPREDICT` with no dependent archives named) stores, and the
  explicit-definition default 0.05 for *P* < 40.
- An archive whose rows contain an unknown is left unchanged.
- The smoothed value of row *i* is a running sum divided by 2*h* + 1: the
  sum for row 0 adds rows −*h* … *h* in that order; for each next row *i*
  it subtracts row *i* − *h* − 1 and then adds row *i* + *h*. The sums
  read the rows before the pass.
- SEASONAL, `HWPREDICT`: *m* = (sum of the smoothed rows 0 … *P* − 1, in
  order) / *P*; each row becomes its smoothed value − *m*, and the
  prediction archive's *I* becomes *I* + *m*.
- SEASONAL, `MHWPREDICT`: *m* as above; each row becomes its smoothed
  value / *m*, and *I* becomes *I* + *m* (added, not multiplied; defect,
  reproduce).
- DEVSEASONAL: each row becomes its smoothed value; nothing else changes.

A command of one argument per PDP therefore smooths right after the
triggering PDP. The vectors `066` to `069` (*h* = 1 additive, *h* = 2
multiplicative, with and without gaps) pass the whole series in one command
and are reproduced only with the deferred pass; per-PDP recordings agree
with the same rule. Whether a command that fails part-way (`update.md`
UP-34) still runs the pass is not pinned.

**HW-19** (black-box; must, platform specific). On the fused builds of
`update.md` UP-35, *blend*(*w*, *a*, *b*) = fma(*w*, *a*,
(1 − *w*) × *b*), *base* = fma(*n*, *T*, *I*), and the failure bounds are
fma(δ₊, *d*, *p*) and fma(−δ₋, *d*, *p*). On the unfused build each is a
product followed by a sum. All other operations are single roundings in
the order written. The prediction, deviation and smoothing results
differ in the last bits between the platforms.

## 3. Update: runs of two or more PDPs (stage 2b, not yet specified)

Observed, for whole updates aligned to the step (one run of *m* equal
PDPs, `update.md` UP-23); to be specified:

- *m* ≥ 3: the prediction and DEVPREDICT rows are unknown, FAILURES rows
  are 0 and its history unchanged, SEASONAL and DEVSEASONAL rows are
  rewritten with their current coefficients, *I*, *I'*, *T*, *T'* are
  unchanged, *n* increases by *m* and *n'* becomes *m*. The PDP value is
  ignored, known or not.
- *m* = 2: the prediction archive learns from the value twice, the
  second time with the SEASONAL coefficient two positions ahead of the
  first; SEASONAL and DEVSEASONAL rows keep their coefficients.

The `hw` vectors with irregular and gapped updates freeze the results.

## Open questions

- Dependency indexes that name an archive of the wrong type (for example
  `RRA:DEVPREDICT:12:4` when archive 4 is the `DEVPREDICT` itself, as in
  the `hwsw0` and `hwsw5` inputs) are accepted by `create`. The rows such
  archives write are frozen by vectors `037` to `046` but not specified.

- Stage 2b: the rules for runs of two or more PDPs (section 3), including
  split updates and runs before any state exists.
- Whether a gap of many PDPs ever resets the model: none observed for gaps
  up to 7 PDPs with *P* = 5 (beyond the season), nor in 43-PDP gaps, which
  are runs (section 3).

- HW-2: the exact derivation of the initial `seasonal_smooth_idx`. It
  could not be settled black-box: it is stable for a given file name but
  not a simple function of the name's characters. Whether byte-identical
  create output for Holt-Winters files is a requirement is a maintainer
  decision; if it is, this is a candidate for reference-source inspection
  on the spec side under AGENTS.md.

## Vectors

`crates/rondi-cli/tests/hw_spec_vectors.rs` replays the `hw` suite
(`crates/rondi-cli/tests/data/hw`), which `scripts/gen-create-resize-vectors.py`
records; `expected/hw.linux-x86_64.vectors` holds the 42 cases whose bytes
differ on the unfused x86_64 build (HW-19), and the Linux aarch64 build
matches macOS arm64 in every case. The inputs are fresh files with each
Holt-Winters archive type and smoothing window, and one file that has
already been updated. The cases read them (`info`, `dump`, `fetch` of each
archive) and update them with seasonal, spiking, gapped, irregular,
multi-source, failure-bound, smoothing and run series.

Section 2 was checked with a black-box model, a script outside the
repository written from these rules and never derived from RRDtool or
Rondi code. Started from the committed input and given each case's PDP
values, it reproduces the final file state (every CDP slot, the
Holt-Winters rows and the failure history) of the 42 single-source `hw`
update cases whose PDPs come one per argument, on macOS arm64 and on Linux
x86_64, with two exceptions explained elsewhere: the DEVPREDICT and FAILURES rows of
the `hwsw0` and `hwsw5` cases (Open questions), and on x86_64 cases `062`
and `063`, whose long readings convert to a slightly different value there
(`update.md` UP-35), so the bound tie is pinned on the fused builds only.
The model also matches 26 recordings made one `update` command per
argument (18 on macOS arm64, 8 on Linux x86_64), of up to 60 updates, with
one and two data sources. Until Rondi
implements these archives, the cases that read or write their state are
expected to be refused; the `fetch` cases already match.
