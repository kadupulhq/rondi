# Holt-Winters archives

Status: complete for create-time state (section 1), `info` and `dump`
output (section 1a), `update` (sections 2 and 3) and `tune` (section 4).
Rondi implements `info`, `dump` and `update` for these archives; `create`
and `tune` still refuse them (CR-80, HW-36).
`fetch` and `restore` of these archives are covered by the vectors and by
the specifications of those commands, not restated here.

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
| `DEVPREDICT` | value (slot 0) unknown |
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
(CR-46): the `create` suite's masked form zeroes parameter slot 4 of every
`SEASONAL` and `DEVSEASONAL` archive. Rondi stores 0 for an archive
defined on the command line (a template's stored value is copied,
`create.md` CR-52). See "Open questions".

## 1a. `info` and `dump`

`info` (`cli.md` CLI-68) and `dump` print a file with Holt-Winters
archives as they print a classic file, except for the parameter and CDP
items of the Holt-Winters archives given here. The file and data source
items are unchanged; `header_size` counts the Holt-Winters archives'
definitions, CDP records and row pointers like any others (1624 for
`hwimp`). Every line of a classic archive is the same as in a file
without Holt-Winters archives: the probes compared an `AVERAGE` archive
alone, before an implicit `HWPREDICT` group and after it, after the same
30 updates, and its `info` and `dump` lines differed only in the archive
index and `cur_row` (CR-46). Neither command changes the file (the
`same:` records of vectors `000` to `015`). Slot names are those of
`restore.md` RS-53; *w* is the FAILURES `window_len`.

**HW-3** (black-box; must). `info`. For a Holt-Winters archive *i*, the
lines `rra[i].cf`, `.rows`, `.cur_row` and `.pdp_per_row` are as for a
classic archive. Then, instead of `rra[i].xff` and the per-DS
`rra[i].cdp_prep[j].value` and `.unknown_datapoints`, the archive prints
its parameter keys `rra[i].<key>`, then for each DS *j* in DS order its
CDP keys `rra[i].cdp_prep[j].<key>`, all of one DS before the next:

| CF | Parameter keys, in order | CDP keys per DS, in order |
|---|---|---|
| `HWPREDICT`, `MHWPREDICT` | `alpha` (`hw_alpha`), `beta` (`hw_beta`) | `intercept`, `slope`, `NaN_count` (`nan_count`) |
| `SEASONAL` | `gamma` (`seasonal_gamma`), then `smoothing_window` if the rule below allows | `seasonal` |
| `DEVSEASONAL` | `gamma` (`seasonal_gamma`), then `smoothing_window` if the rule below allows | `deviation` (CDP slot 2) |
| `DEVPREDICT` | value (slot 0) unknown | none |
| `FAILURES` | `delta_pos`, `delta_neg`, `failure_threshold`, `window_length` (`window_len`) | `history` |

`info` never prints `dependent_rra_idx`, `seasonal_smooth_idx`,
`last_intercept`, `last_slope`, `last_nan_count`, `last_seasonal`,
`init_flag`, `primary_value` or `secondary_value`. Doubles use the
CLI-68 form: C `%.10e` (`1.4484406767e+01`, `-0.0000000000e+00` for −0,
`1.5000000000e-310` for a subnormal), `NaN` for any NaN on every
platform, and `inf` / `-inf`. The CLI-49 locale comma applies
(`rra[1].alpha = 1,0000000000e-01`). Integers are unsigned 64-bit
decimal (`18446744073709551615`).

`history` is a quoted string of *w* characters. Character *k* is `1` when
byte *k* of the DS's CDP record is 1 and `0` for any other byte value
(2, 10, `0x30`, `0x80` and `0xff` all print `0`). Byte 0 holds the newest
result (HW-14), so the text reads newest first: in the probe with
*w* = 28, the results 1, 1, 1, 1, 0, 0, 0, 0, 0, 1 (oldest to newest,
after only zeros) print `"1000001111000000000000000000"`. *w* = 1
prints one character; a new file with *w* = 9 prints `"000000000"`
(vectors `195` and `197`: *w* = 1 and 28 after spikes).

`smoothing_window` (parameter slot 2) is printed, by `info` and by `dump`
(HW-4), when the file's version string read as C `atoi` (leading blanks,
an optional sign, then decimal digits up to the first other byte) is 4 or
more. It is not printed otherwise, whatever value the slot holds. So it is
present in `0004`, `0005`, `4`, `04`, ` 4`, `+4`, `4a` and `4x`, and absent
in `0003`, `0002`, `3`, `1`, `-4`, `x4` and `abc`. The stored value is
kept: restoring `hwsw5`'s dump with `<version>0003</version>` hides 0.5,
and setting the version bytes back to `0004` shows it again. Which files
`create` writes as `0004` is CR-37, so a `MHWPREDICT` file or an explicit
`smoothing-window=` (even `=0`) shows the key and an implicit `HWPREDICT`
group or explicit archives without the option do not; `tune -s` and
`-S` raise the version (HW-34) and so make the key appear (vectors `187`
to `192`: `hwsw5` patched to `0003` hides its 0.5, `hwimp` and `hwp40`
patched to `0004` show their 0).

Vectors: `000` (`hwimp`, `0003`, no `smoothing_window`), `002`
(`mhwimp`, `0004`, `smoothing_window = 0.0000000000e+00`), `004`
(`hwexp`, explicit archives at `0003`), `006` (`hwsw0`), `008` (`hwsw5`,
`5.0000000000e-01`), `010` (`hwpdp3`, beside an `AVERAGE` archive of 3
PDPs per row), `012` (`hw2ds`, two DSs) and `014` (`hwrun`, state after
updates). The Linux x86_64 and aarch64 builds print the same bytes for
all eight.

*w* = 0 (only in a crafted file) prints `""`. *w* above 28 is HW-5.

**HW-4** (black-box; must). `dump`. A Holt-Winters `<rra>` has the
classic layout: `<cf>`, `<pdp_per_row>` with its `<!-- N seconds -->`
comment, an empty line, `<params>`, `<cdp_prep>` with one `<ds>` per
data source in DS order, and `<database>`. Each parameter is one line
`\t\t<name>value</name>`, each CDP item one line
`\t\t\t<name>value</name>`, as for classic archives:

| CF | `<params>` elements, in order |
|---|---|
| `HWPREDICT`, `MHWPREDICT` | `hw_alpha`, `hw_beta`, `dependent_rra_idx` |
| `SEASONAL`, `DEVSEASONAL` | `seasonal_gamma`, `seasonal_smooth_idx`, `smoothing_window` (only under the version rule of HW-3), `dependent_rra_idx` |
| `DEVPREDICT` | `dependent_rra_idx` |
| `FAILURES` | `delta_pos`, `delta_neg`, `window_len`, `failure_threshold`, `dependent_rra_idx` |

Each `<cdp_prep>` `<ds>` starts with `primary_value` and
`secondary_value`, for every CF, classic or Holt-Winters, and continues:

| CF | Further `<ds>` elements, in order |
|---|---|
| `HWPREDICT`, `MHWPREDICT` | `intercept`, `last_intercept`, `slope`, `last_slope`, `nan_count`, `last_nan_count` |
| `SEASONAL`, `DEVSEASONAL` | `seasonal`, `last_seasonal`, `init_flag` (`DEVSEASONAL` uses the same names for its deviation slots) |
| `DEVPREDICT` | value (slot 0) unknown |
| `FAILURES` | `history` |

No Holt-Winters archive prints `<xff>`, `<value>` or
`<unknown_datapoints>`. Note that `FAILURES` gives `window_len` before
`failure_threshold`, the reverse of `info`.

Value forms. Doubles use RRDtool's own `%0.10e` (`format-strings.md`
FS-52: `-0` prints `0.0000000000e+00`, a subnormal 1.5e-310 prints
`0.0150000000e-308`, the locale is ignored), with `inf` and `-inf`. A NaN
prints `NaN` in `<cdp_prep>` but `nan` in `<params>`, for any NaN on every
build; this also holds for a classic `<xff>` (defect, reproduce;
vectors `193` and `194`, `hwsw5` with every double parameter and every
`primary_value` and `secondary_value` patched to NaN). Integers are
unsigned 64-bit decimal.
`<history>` has no quotes: each of the *w* bytes from byte 0 of the DS's
CDP record, newest first as in HW-3, written as a decimal integer, with
no separator. A file written by `create`, `update`, `tune` or `restore`
holds only bytes 0 and 1 (`restore.md` RS-26), so the text is one digit
per byte (`11100` in the probe of `hwfail`; vectors `196` and `198`).
For other bytes, macOS arm64
and Linux x86_64 print the byte as signed (`-1` for `0xff`, `-128` for
`0x80`) and Linux aarch64 as unsigned (`255`, `128`) (platform specific;
should, not pinned). *w* = 0 gives `<history></history>`; *w* above 28
is HW-5.

The `<database>` section is as for a classic archive (`time-spec.md`
TS-71 comments, rows oldest first from the row after `cur_row`, one `<v>`
per DS, the slot of each row derived from `last_update`, the archive's
`pdp_per_row` and step, unknowns as `NaN`). No comment is specific to
Holt-Winters archives. The `<version>` element shows the stored version
text, so a file with Holt-Winters archives may say `0003`.

Vectors: `001`, `003`, `005`, `007`, `009`, `011`, `013` and `015`, for
the same files as HW-3; all eight are the same bytes on Linux x86_64 and
aarch64 (`expected/hw.linux-x86_64.vectors` holds no `info` or `dump`
case).

**HW-5** (black-box; should, defect). A FAILURES `window_len`
above 28 occurs only in a crafted file: `create` (CR-33) and `tune`
(HW-33) refuse it, but `restore` stores any `<window_len>`. RRDtool then
treats the history as *w* bytes from byte 0 of the DS's 80-byte CDP
record and beyond it:

- `info` prints *w* characters (HW-3) while *w* is small enough and then
  crashes with no output: from *w* = 32 on macOS arm64 (abort, signal 6),
  from *w* = 33 on Linux aarch64 and from *w* = 34 on Linux x86_64
  (segmentation fault, signal 11). Every *w* from the threshold to 130,
  and 200, 255, 256, 1000, 100000 and 2^32 − 1, crashed in the probes.
- `dump` exits 0 for every *w* up to at least 1000, printing *w* bytes:
  up to 80 from the record (including the bytes of `primary_value` and
  `secondary_value`), then the bytes that follow it in the file. It
  crashes at *w* = 100000 on every build.
- `update` exits 0 for every *w* up to at least 1000, shifting *w* bytes
  of history and so overwriting the record's other slots and, above 80,
  the next records. It crashes at *w* = 100000.

A replacement should instead (defect in the reference) fail `info`, `dump` and `update` of such a
file with exit status 1 and an `ERROR: ` line, before writing anything,
and leave the file unchanged. RRDtool prints no error for any nearby
window (1 to 28 succeed, and `create`'s and `tune`'s range errors are
messages of those commands), so the text after `ERROR: ` is
implementation-defined. Vectors `199` to `204` (*w* = 29, which RRDtool
still prints, and *w* = 40, which crashes `info`) record RRDtool's result
and carry the expectation `clean-error`.

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
*a* ⊖ *b* = *a* / *b* when *b* > 0 and unknown otherwise (HW-12a), and
*a* ⊗ *b* = *a* × *b*.

**HW-11** (black-box; must). Row positions. Every Holt-Winters archive
advances its own current row by one per PDP and writes that row
(`update.md` UP-30). The SEASONAL and DEVSEASONAL rows written are the
coefficient positions; their current rows start wherever `create`'s random
choice put them (CR-46), so position 0 of the season is not tied to time.

**HW-12** (black-box; must). Prediction and learning. Let *s* = *σ* and
*d* = *δ*, the values prefetched by the previous PDP (HW-16), and set
*n'* = *n*. The first case that applies is taken. Besides the prediction
*p* that the prediction archive writes, each case gives the prediction
*p̂* that the deviation (HW-13) and the failure check (HW-14) use, or
says there is none.

1. If *I* is unknown: when *x* is known, *I* = *I'* = *x*, *T* = *T'* = 0
   and *n* = *n'* = 1 (*n* may have grown in runs, HW-21); *p* is
   unknown, *p̂* = *x* ⊗ *s* (unknown when *s* is), and the new
   coefficient is *s'* = *blend*(γs, *x* ⊖ *I*, *s*) with the new *I*
   when *s* is known, and *s'* = *x* ⊖ *I* when it is not (so 0, or 1 for
   a positive first value of `MHWPREDICT`). When *x* is unknown, *I'* = *I*
   and *T'* = *T*; *I*, *T* and *n* are unchanged, *p* is unknown, there
   is no *p̂*, and *s'* = *s*.
2. Else if *T* is unknown: *p* is unknown and there is no *p̂*; *I'* = *I*
   and *T'* = *T*; *I*, *T* and *n* are unchanged, known *x* or not; *s'*
   is unknown when *x* is known and *s* when *x* is unknown.
3. Else if *s* is unknown (the first season): *p* is unknown and there is
   no *p̂*; *I'* = *I*, *T'* = *T*; *I*, *T* and *n* are unchanged, and
   *s'* = *x* ⊖ *I*.
4. Otherwise let *base* = *I* + *n*·*T* (an fma, HW-19) and *p* = *p̂* =
   *base* ⊗ *s*; set *I'* = *I* and *T'* = *T*. When *x* is known:
   *I* = *blend*(α, *x* ⊖ *s*, *base*), then *T* = *blend*(β, *I* − *I'*,
   *T'*), then *s'* = *blend*(γs, *x* ⊖ *I*, *s*) with the new *I*, and
   *n* = 1. When *x* is unknown: *n* = *n* + 1, *s'* = *s*, and *I*, *T*
   are unchanged. So after a gap of *k* unknown PDPs the trend is
   projected *k* + 1 steps (vectors `022` to `031`, `055` to `057`,
   `060`; the replayed model also matches per-PDP recordings with gaps,
   which the vectors freeze only in runs, section 3).

Case 1 tests the value of *I*, not whether the file ever had state: an
intercept made unknown by learning (a NaN α, which `tune` accepts,
HW-31, or *s* ≤ 0 under HW-12a) restarts the level from the next known
value as a new file or `tune --aberrant-reset` does, except that the
season rows it already holds stay and are blended. With a NaN α, a known
*x* in case 4 makes *I*, *T* and *s'* unknown, and the next known *x*
takes case 1, so the two cases alternate. A NaN β alone makes *T*
unknown while *I* stays known, after which every PDP takes case 2: the
prediction stays unknown and *I* and *T* do not change again (vectors
`205` to `210`, where `205` also differs on x86_64 because of the NaN
smoothing windows of `hwnan`, HW-18a; probes that restored files with
each of *I*, *T*, *s* and
*d* unknown, 0, negative, very large and infinite, in additive and
multiplicative groups).

**HW-12a** (black-box; must). Multiplicative division. For `MHWPREDICT`,
*a* ⊖ *b* is unknown whenever *b* is not greater than 0 (negative, 0 or
−0), and otherwise *a* / *b*; *a* ⊗ *b* is the plain product. This makes
these results unknown, with everything else as in HW-12 to HW-18:

- *x* ⊖ *s* with *s* ≤ 0 in case 4: *I* and then *T* are unknown, as is
  *s'*; *n* = 1 and *I'*, *T'* are set as usual. *p* = *base* × *s* is
  still written, and the deviation and failure check use it.
- *x* ⊖ *I* with *I* ≤ 0 (the new *I* in cases 1 and 4, the kept one in
  case 3): *s'* is unknown; *I* and *T* keep the values computed. So a first value of 0 or below gives
  an unknown first coefficient, and a level that learning drives to 0 or
  below gives unknown coefficients from then on, while *p*, *d'* and the
  rows of the other archives are still computed.
- The smoothing pass (HW-18) with a SEASONAL mean *m* ≤ 0: every smoothed
  SEASONAL row of that DS becomes unknown; *I* still becomes *I* + *m*.

`HWPREDICT` is unaffected: its ⊖ is a subtraction for every sign
(30 restored states with *I*, *T*, *s* and *x* negative, 0 and −0 matched
the plain formulas). The reviewer's reproduction, `mhwsm10` given 97, 94,
… 25, then 30 unknown PDPs and then 40, one PDP each, ends with *p* =
−72.05, *I* = −36.10 and *s'* unknown (vector `211` is that series;
`212` to `215` are the other multiplicative cases, and `216` is a
smoothing pass with *m* < 0).

**HW-13** (black-box; must). Deviation. With the *p̂* of HW-12: when
there is no *p̂*, *d'* is unknown, even when *x* is unknown and *d* known;
else when *x* is unknown, *d'* = *d*; else when *d* is unknown,
*d'* = |*x* − *p̂*|; otherwise *d'* = *blend*(γd, |*x* − *p̂*|, *d*). An
unknown *p̂* makes *d'* unknown through the arithmetic. The deviation is
|*x* − *p̂*| for `MHWPREDICT` too.

**HW-14** (black-box; must). Failures. Let δ₊, δ₋ be FAILURES
`delta_pos` and `delta_neg`, *w* its `window_len` and *k* its
`failure_threshold`. The PDP is a violation (1) when *d* is known and
either *x* is unknown or *x* > *p̂* + δ₊·*d* or *x* < *p̂* − δ₋·*d* (fma on
fused platforms); comparisons with an unknown *p̂*, or when HW-12 gives no
*p̂*, are false, so an unknown *x* with a known *d* is a violation in every
case of HW-12. A value exactly on a bound is
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
| `HWPREDICT`, `MHWPREDICT` | *p* (unknown while HW-12 cases 1 to 3 apply) |
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
archive's `seasonal_smooth_idx`, the archive triggers. There is one counter
per archive: the slot of the first DS. The slots of the other DSs are never
changed by `update` (vector `058`, and a two-DS series of 12 one-PDP
arguments crossing row 0 several times, which RRDtool and a first-DS-only
model leave byte-identical).

**HW-18** (black-box; must). Smoothing pass. When either archive has
triggered on any PDP of an `update` command (arguments given together
count as one command), one pass runs after the command's last argument
has been applied, however many triggers there were; within the command,
later PDPs see the rows and *I* as they were before the pass. Each DS's
SEASONAL and DEVSEASONAL rows are smoothed:

- The window fraction *f* depends on the file's version string read as C
  `atoi` (as for the `info` key, HW-3). At 4 or more (`0004`, `0005`) *f*
  is the archive's stored `smoothing_window`. At 3 (`0003`, `3`, `+3`,
  ` 3`, `3x`) *f* is 0.05 for both SEASONAL and DEVSEASONAL, whatever the
  slot holds: 0 in an implicit definition (`HWPREDICT` or `MHWPREDICT`
  with no dependent archives named), an explicit `smoothing-window` in a
  file restored or patched to `0003`, or anything else. Files whose
  version reads below 3 are not covered here (versions 1 and 2 use an
  older header layout). So an implicit definition is smoothed once the
  season is long enough, contrary to what its stored 0 suggests. Probed
  for `HWPREDICT` and `MHWPREDICT`, implicit and explicit, *P* = 40, 100
  and 200, with one argument per command and all arguments in one
  command: each `0003` result matched the same file at `0004` holding
  0.05, and differed from it holding the stored value. Vectors `181` to
  `186` update `hwp40` (implicit, `0003`), `hwp40v4` (the same bytes at
  `0004`, so its stored 0 applies) and `hwx40v3` (an explicit 0.5 patched
  to `0003`), each with 60 arguments and with one argument that completes
  45 PDPs (a run, HW-21) and so passes row 0; every `0003` result equals
  the `0004` file holding 0.05.
- The half-width is *h* = trunc(*f*·*P* / 2) (*f*·*P* and the division in
  doubles); the window is 2*h* + 1 rows, centred and circular. When
  *h* = 0 that archive is not smoothed at all. This includes *f* = 0 in a
  version-4 file and *f* = 0.05 for *P* < 40, which covers every `0003`
  input of the earlier vectors.
- When the rows of an archive contain an unknown for any DS, that archive
  is left unchanged for every DS (the SEASONAL and DEVSEASONAL archives
  are judged separately). So one DS still in its first season, or reset
  by `tune --aberrant-reset`, holds back smoothing for all the others, as
  `rrdtune(1)` says.
- The smoothed value of row *i* is a running sum divided by 2*h* + 1: the
  sum for row 0 adds rows −*h* … *h* in that order; for each next row *i*
  it subtracts row *i* − *h* − 1 and then adds row *i* + *h*. The sums
  read the rows before the pass.
- SEASONAL, `HWPREDICT`: *m* = (sum of the smoothed rows 0 … *P* − 1, in
  order) / *P*; each row becomes its smoothed value − *m*, and the
  prediction archive's *I* becomes *I* + *m*.
- SEASONAL, `MHWPREDICT`: *m* as above; each row becomes its smoothed
  value ⊖ *m* (unknown when *m* ≤ 0, HW-12a), and *I* becomes *I* + *m*
  (added, not multiplied; defect, reproduce).
- DEVSEASONAL: each row becomes its smoothed value; nothing else changes.

Divergence (HW-18, documented). With a smoothing window *f* ≥ 2, which
only crafted files can hold because `tune -s` caps *f* at 1, RRDtool
still smooths and gives a result Rondi does not reproduce. Rondi leaves
such archives unsmoothed.

**HW-18a** (black-box; must, platform specific). Out-of-range windows on
x86_64. On Linux x86_64, when *f*·*P* / 2 is NaN (of either sign) or at
most −2^63 (including −∞), the pass of HW-18 still runs for that archive
but does not average: with *k* = 2^63 mod *P*, row *i* takes the value
row *i* − *k* (mod *P*) held before the pass, and row *k* takes a value
that is not a row (0 for *P* = 8 and 12; a tiny subnormal for *P* = 5 and
7, which Rondi writes as 0). SEASONAL then has its mean removed and added
to *I* exactly as in HW-18, and DEVSEASONAL rows are only moved. The
unknown-row rule of HW-18 still holds the pass back. On macOS arm64 and
Linux aarch64 these windows leave the archive unsmoothed, as does +∞ or
any value of 2^64 or more on every build. Values between −2^63 and −1
crash RRDtool on x86_64 or give rows Rondi does not reproduce (divergence,
documented; Rondi leaves them unsmoothed). Evidence: vector `205`
(`hwnan`, whose stored windows are NaN; RRDtool's x86_64 intercept is
3.075e+01 after 40 updates against 2.7e+01 on arm64, and the bytes first
differ at the first pass after one season), and probes on `hwnan` with
α, β and both γ set to 0.5 and the windows set to NaN, −NaN, −∞, −1e308
and −2^62 (*f*·*P* / 2 = −2^63), each matching NaN, and to 0, +∞,
1e308 and −2^62 + 1024, each unsmoothed; and on fresh *P* = 5, 7 and 12
files given `tune -s nan -S nan`, compared with the same files given
`tune -s 0 -S 0`. One `update` command per argument for 40 arguments,
dumped after each, matched RRDtool on x86_64 in every probe except that
row *k* value for *P* = 5 and 7.

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

## 3. Update: runs of two or more PDPs

`update.md` UP-23 splits the PDPs an argument completes into runs of equal
values: a whole update gives one run of *n* PDPs, a split update a run of
one (*F*, section 2) followed by a run of *n* − 1. The Holt-Winters
archives treat a run of *m* ≥ 2 PDPs of value *z* as a unit, not as *m*
single PDPs. Let *r0* be an archive's current row before the run.

**HW-20** (black-box; must). A run of two PDPs is processed as two PDPs by
HW-11 to HW-14 and HW-17, each with *x* = *z*, with these differences:

- The prefetch after each PDP (HW-16) reads the row two positions after
  the one just reached (*r* + 2 mod *P*) instead of one. So the second
  PDP uses the coefficient and deviation at *r0* + 3, not *r0* + 2, and
  after the run slot 2 holds row *r0* + 4 and slot 3 the second PDP's
  coefficient.
- SEASONAL and DEVSEASONAL rows do not change. The new coefficients and
  deviations of the first and second PDP go only to those archives'
  *primary_value* and *secondary_value* (words 8 and 9).
- The prediction archive's *primary_value* and *secondary_value* are the
  two predictions; its two rows, and the DEVPREDICT and FAILURES rows, are
  as HW-15 gives them for each PDP, and the failure history shifts twice.

This holds with or without state, and in the first season: a run of two
that reaches a known coefficient learns from it (vectors `052`, `070`,
`071`).

**HW-21** (black-box; must). A run of *m* ≥ 3 PDPs ignores *z*, known or
not:

- Each archive writes *K* = min(*m*, *rows*) rows (`update.md` UP-29),
  advancing its current row *K* positions: unknown in the prediction and
  DEVPREDICT archives, 0.0 in FAILURES, and the existing values in SEASONAL
  and DEVSEASONAL (no change). The season counter and triggers (HW-17)
  follow those *K* positions.
- *I*, *I'*, *T* and *T'* are unchanged, unknown or not; *n* becomes
  *n* + *m* and *n'* becomes *n'* + *m* − 1 (not the *n* before the last
  PDP). The next known value is therefore predicted *n* steps ahead.
- The failure history is cleared to all zero bytes.
- SEASONAL slot 3 becomes the row at (*r0* + *m*) mod *P* and slot 2 the
  row after it, using the full *m* even when the rows written were capped
  at *K* = *P*; DEVSEASONAL likewise. Both read the rows as they were
  before any smoothing pass of the command (HW-18).
- The prediction archive's *primary_value* and *secondary_value* become
  unknown; those of SEASONAL and DEVSEASONAL are unchanged.

(vectors `053`, `054`, `072` to `075`; the 43-PDP gaps and the runs before
any state in the recordings described under "Vectors").

## 4. `tune`

`rrdtune(1)` lists eleven Holt-Winters options. They are header options in
the sense of `tune-modify.md` TM-4: they run in argument order, before any
structure change, and an option that fails stops the command with exit
status 1 after the options before it have been written (`cli.md` CLI-31
covers a missing argument). Error texts below are the message after
`ERROR: `.

**HW-30** (black-box; must). Each option writes one parameter
(`restore.md` RS-53 numbering) of the archive of its type, and nothing
else unless stated:

| Option | Archive | Parameter |
|---|---|---|
| `--alpha`, `-x` | `HWPREDICT` or `MHWPREDICT` | 1 (`hw_alpha`) |
| `--beta`, `-y` | `HWPREDICT` or `MHWPREDICT` | 2 (`hw_beta`) |
| `--gamma`, `-z` | `SEASONAL` | 1 (`seasonal_gamma`) |
| `--gamma-deviation`, `-v` | `DEVSEASONAL` | 1 (`seasonal_gamma`) |
| `--smoothing-window`, `-s` | `SEASONAL` | 2 (`smoothing_window`) |
| `--smoothing-window-deviation`, `-S` | `DEVSEASONAL` | 2 (`smoothing_window`) |
| `--deltapos`, `-p` | `FAILURES` | 1 (`delta_pos`) |
| `--deltaneg`, `-n` | `FAILURES` | 2 (`delta_neg`) |
| `--window-length`, `-w` | `FAILURES` | 4 (`window_len`, integer) |
| `--failure-threshold`, `-f` | `FAILURES` | 5 (`failure_threshold`, integer) |

`--window-length` and `--failure-threshold` also clear the failure history
of every DS (HW-14) to zero bytes, even when the value is unchanged; the
other options leave it (vectors `076`, `085`, `094`, `103`, `112` to
`114`, `118` to `120`, `124`, `125`, `131`, `132`, `138`, `139`, `142`,
`144` to `146`, `149`, `151` to `155`, `163`, `176`; history: `172` to
`174`).

**HW-31** (black-box; must). The values of `-x`, `-y`, `-z` and `-v` are
read as the longest leading decimal number, ignoring leading blanks and
trailing text (`0.5x` is 0.5). No number at all fails with
`Unable to parse Holt-Winters parameter`. A number not strictly between
0 and 1 fails with `Holt-Winters parameter must be between 0 and 1`; this
includes `inf`. `nan` passes the check and is stored as a NaN whose sign
is the opposite of the platform's own (`0xfff8000000000000` on arm64 and
aarch64, `0x7ff8000000000000` on x86_64; defect, reproduce). `-s` and `-S`
read the same way but accept 0 and 1 (vectors `077` to `084`, `086` to
`093`, `095` to `102`, `104` to `111`, `115` to `117`, `121` to `123`).

**HW-32** (black-box; must, defect). `-p` and `-n` read a decimal number.
Trailing text after a number fails with
`Unable to parse parameter in set_deltaarg`; a number below 0.1 fails with
`Parameter specified is too small` (checked first, so `0.05x` reports
that). A value with no number at all (`x`) is accepted and stores 0.0
(defect, reproduce) (vectors `124` to `137`).

**HW-33** (black-box; must). `-w` and `-f` read the leading integer as C
`atoi` does (`3.7` is 3, `x` is 0). A value outside 1 to 28 fails with
`Parameter must be between 1 and 28`. Neither is checked against the
other, so the threshold may exceed the window (vectors `138` to `151`,
`155`).

**HW-34** (black-box; must). After its value passes, an option whose
archive is missing fails: `-x`, `-y`, `-z`, `-v`, `-s` and `-S` with
`Holt-Winters RRA does not exist in this RRD`, `-p`, `-n`, `-w` and `-f`
with `Failures RRA does not exist in this RRD`. A file with only some
Holt-Winters archives fails for the options of the missing ones
(`--gamma-deviation` on a file created with `HWPREDICT` and an explicit
`SEASONAL` only). `-s` and `-S` raise the file's version to `0004` before
looking for the archive, so they leave it changed even when they fail
(vectors `165`, `166`, `168`, `169`, `177` to `179`; `170` shows that an
implicit definition has all the archives).

**HW-35** (black-box; must). `--aberrant-reset` (`-b`) names a DS. An
empty name fails with `invalid argument for aberrant-reset` and an unknown
one with `unknown data source name '<name>'`. For the named DS only it
sets to unknown the prediction archive's intercept, last intercept, slope
and last slope (CDP slots 2 to 5), the SEASONAL and DEVSEASONAL CDP slots
2 and 3 and every SEASONAL and DEVSEASONAL row, and clears its failure
history. It does not change the NaN counts, the season counters, the
prediction, DEVPREDICT or FAILURES rows, or any other DS. Unknowns are the
platform's NaN (`create.md` CR-74). A file without Holt-Winters archives is
left unchanged with exit status 0. Each `-b` resets its DS, so `-b x -b y`
resets both. Afterwards the reset DS starts again from HW-12 case 1, and
its unknown rows hold back smoothing for every DS until they are filled
(HW-18) (vectors `156` to `160`, `175`; `167` and `171` change nothing,
the second because a fresh file has nothing to reset; `180` updates a file
whose first DS was reset).

**HW-36** (black-box; should). Rondi does not yet implement these options
on files with Holt-Winters archives (`tune-modify.md` TM-60). Until it
does it refuses them and leaves the file unchanged: after the value
checks, the CLI and rrdcached's `TUNE` both report
`--<long option> is not supported by Rondi` (for example
`--alpha is not supported by Rondi`) when the file has the archive the
option tunes. In particular `--aberrant-reset` on such a file is refused
rather than accepted without effect. The value checks that fail before any archive
is read (HW-31 to HW-33) and the effects on files without Holt-Winters
archives already match (vectors `164` to `167`).

## Open questions

- Dependency indexes that name an archive of the wrong type (for example
  `RRA:DEVPREDICT:12:4` when archive 4 is the `DEVPREDICT` itself, as in
  the `hwsw0` and `hwsw5` inputs) are accepted by `create`, and
  `rrdtool create` can produce a misnumbered index, so these files are
  not only crafted. For the DEVPREDICT and FAILURES archives the
  behavior is now pinned and implemented (vectors `037` to `046`, test
  `dev_predict_and_failures_follow_their_links`):
  - DEVPREDICT writes word 3 of the archive its link names, read after
    the PDP's prediction and season updates: the deviation used for
    DEVSEASONAL, the coefficient used for SEASONAL, `last_intercept` for
    the prediction archive, and whatever word 3 holds for a classic
    archive, itself or FAILURES.
  - FAILURES checks values only when the archive its link names itself
    links back to the prediction archive, with that archive's word 3 as
    the deviation. Linked anywhere else, it stores the PDP value, sets
    *primary_value* to it and keeps its history.
  - In runs of three or more PDPs (HW-21) DEVPREDICT writes unknown and
    FAILURES writes 0 whatever the link.
  Still open: a wrong-type link on the prediction, SEASONAL or
  DEVSEASONAL archive also changes RRDtool's results. No vector pins
  these, and Rondi refuses an `update` on such a file before writing
  anything.

- Holt-Winters archives with more than one PDP per row. `create` gives
  every Holt-Winters archive one PDP per row, but `restore` stores the
  `<pdp_per_row>` of an edited dump. No vector pins how RRDtool updates
  such a file; Rondi writes one row per PDP, so it refuses an `update`
  on such a file before writing anything (test
  `hw_archive_with_several_pdps_per_row_is_refused_untouched`). The
  same holds for a group without exactly one prediction archive and at
  most one archive of each other type. Each of these refusals says
  "… not supported by Rondi" and leaves the file unchanged.

- Whether a gap of many PDPs ever resets the model: none observed for gaps
  of single unknown PDPs up to 7 with *P* = 5, nor for runs of 43 PDPs
  (HW-21 keeps *I* and *T*).
- Words 8 and 9 after runs are settled (not open): a run of two stores
  the first and second PDP's result in all five archives; a run of three
  or more sets the prediction and DEVPREDICT words to unknown and the
  FAILURES words to 0, and leaves the season archives' unchanged (HW-20,
  HW-21; vectors `052` to `054`, `072` to `075`).

- HW-2: the exact derivation of the initial `seasonal_smooth_idx`. It
  could not be settled black-box: it is stable for a given file name but
  not a simple function of the name's characters. Whether byte-identical
  create output for Holt-Winters files is a requirement is a maintainer
  decision; if it is, this is a candidate for reference-source inspection
  on the spec side under AGENTS.md.

## Vectors

`crates/rondi-cli/tests/hw_spec_vectors.rs` replays the `hw` suite
(`crates/rondi-cli/tests/data/hw`), which `scripts/gen-create-resize-vectors.py`
records; `expected/hw.linux-x86_64.vectors` holds the 47 cases whose bytes
differ on the unfused x86_64 build (HW-19), and the Linux aarch64 build
matches macOS arm64 in every case. The inputs are fresh files with each
Holt-Winters archive type and smoothing window, and one file that has
already been updated. The cases read them (`info`, `dump`, `fetch` of each
archive) and update them with seasonal, spiking, gapped, irregular,
multi-source, failure-bound, smoothing and run series, and `tune` them
with every option, valid and invalid. Cases `181` to `204` add files
byte-patched by the generator: version changes (HW-3, HW-18 at *P* = 40),
NaN parameters (HW-4), failure windows of 1 and 28, and crafted windows
of 29 and 40 (HW-5, expectation `clean-error`). Cases `205` to `210`
update `hwnan` and two copies of `hwimp` given `tune --alpha nan` and
`tune --beta nan` (`hwimpna`, `hwimpnb`; HW-12), and `211` to `216` give
multiplicative groups values and levels at or below 0 (HW-12a).

Sections 2 and 3 were checked with a black-box model, a script outside the
repository written from these rules and `update.md` UP-10 and UP-19 to
UP-25, never derived from RRDtool or Rondi code. Started from the committed
input and given each case's arguments, it reproduces the final file state
(every CDP slot it models, the Holt-Winters rows and the failure history)
of all 51 single-source `hw` update cases, including split, irregular,
gapped and multi-PDP updates, on macOS arm64 and on Linux x86_64, with two
exceptions explained elsewhere: the DEVPREDICT and FAILURES rows of the
`hwsw0` and `hwsw5` cases (Open questions), and on x86_64 cases `062` and
`063`, whose long readings convert to a slightly different value there
(`update.md` UP-35), so the bound tie is pinned on the fused builds only.
The model also matches 37 recordings made one `update` command per
argument (27 on macOS arm64, 10 on Linux x86_64), of up to 60 updates,
with one and two data sources, gaps of up to 43 PDPs and runs before any
state exists, and case `180` (two DSs after a reset) when given the PDP
values RRDtool converts from its readings (`restore.md` RS-38's digit loop
is not correctly rounded, so a few three-decimal readings differ from a
correctly rounded parse in the last bit). Every `tune` case's
status, error text and changed fields were checked against section 4,
and the probes behind it were run on all three builds. Rondi implements `info`, `dump` and `update` for these archives;
`fetch` reads their stored rows as it does any classic archive's, and its
six cases match. 111 cases are `same`, 6 are `clean-error` (`info`, `dump` and `update` on the
crafted files with window lengths above 28, HW-5) and none is `divergent`. The 100 `tune` cases
stay `unsupported` (HW-36).

Black-box evidence for the dependency links: the probes
patched the dependency index of one or two archives in the committed
inputs (`hwimp`, `hwfail`, `hwsw0`, `hww28`) and ran `updatev`, `dump` and
`info` on RRDtool and Rondi, for one command and one command per
argument, with runs of 2, 3 and 5 PDPs and spikes. All 262 runs (68
single-link variants and 194 DEVPREDICT by FAILURES link pairs) matched
in output, exit status and file bytes. A further 264 random series on all
22 `hw` inputs matched except 11 that involve NaN parameters, which were
then specified (HW-12, HW-12a) and fixed.

Damaged files (documented divergences). When SEASONAL rows run past the
file's end into later archives' bytes, RRDtool reads the overlapping bytes
and then fails. Rondi treats past-end rows as unknown and fails with the
`writing rrd` error of `update.md` UP-38. The moves of season archives in
runs (HW-20, HW-21) are not checked against the row count either, so a
damaged file whose season row lies past the end does not fail there; this
is unpinned, because RRDtool rejects the truncated files that would show
it.

The `hw.linux-x86_64` overrides for cases `205` to `216` were recorded in
the `rondi-oracle:x86_64` image; `205`, `211`, `214` and `215` differ from
arm64; `205` differs by HW-18a.
