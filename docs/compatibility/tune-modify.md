# `rrdtool tune`: data source and archive changes

This specification describes what RRDtool 1.11.0 does when `rrdtool tune`
adds or deletes data sources, adds or deletes round-robin archives, changes
an archive's row count, or changes the step with `--step`. It covers the
argument syntax, validation and error text, the resulting file, and how new
rows and consolidation state are filled. It does not cover the header-only
options (`--heartbeat`, `--minimum`, `--maximum`, `--data-source-type`,
`--data-source-rename` and the Holt-Winters parameters) except where they
interact with a structure change.

Every rule carries a provenance tag:

- **documentation**: stated in `rrdtune(1)` or `rrdcreate(1)`.
- **black-box**: observed by running `/opt/homebrew/bin/rrdtool` 1.11.0
  (macOS, arm64) and the pinned 1.11.0 build on Linux aarch64 against frozen
  inputs, and pinned by the vectors in
  `crates/rondi-cli/tests/data/tune_modify`.
- **reference-source-inspection**: the black-box result was known, but the
  rule that produces it could not reasonably be recovered from outputs alone,
  so the reference source was read to state the rule. The rule is written
  here in neutral terms and every such rule is pinned by vectors.

Each rule is also classed as **must** (a replacement reproduces it exactly,
byte for byte where it concerns file content), **should** (expected for
compatibility but not pinned byte for byte, or pinned only on some
platforms), or **defect** (a reference bug or undefined behavior that a
replacement must not copy; the rule says what to do instead).

## Terms

- *S* is the file's step in seconds and *L* its last update time in whole
  seconds.
- An archive has a consolidation function (CF), *p* PDPs per row, *n* rows
  and a `cur_row` pointer. Its *slot length* is *T = p·S*.
- The newest row of an archive covers the slot that ends at
  *E = L − (L mod T)*. The row *a* places older covers
  *(E − (a+1)·T, E − a·T]*. A row "covers" time *t* when *t* lies in that
  interval.
- *Rotated order* lists an archive's rows oldest first: the row after
  `cur_row` first, `cur_row` last.
- The file layout is the 64-bit little-endian version 3 layout that
  `rrdtool create` writes: a 128-byte header, 120 bytes per data source
  definition, 120 bytes per archive definition, 16 bytes of last-update
  time, 112 bytes of PDP state per data source, 80 bytes of consolidation
  (CDP) state per archive and data source, 8 bytes of `cur_row` per
  archive, then the rows of each archive in turn, one 8-byte double per
  data source.
- Within a CDP state record, `value` is at byte 0, `unknown_datapoints` at
  byte 8, `primary_value` at byte 64 and `secondary_value` at byte 72, as
  `rrdtool info` names them. Within PDP state, `last_ds` is the first 30
  bytes, `unknown_sec` is at byte 32 and `value` at byte 40.

## 1. Command shape

**TM-1** (documentation; must). The synopsis is
`rrdtool tune filename [options] [DEL:ds-name] [DS:ds-spec] [DELRRA:index]
[RRA:rra-spec] [RRA#index:[+-=]number]`. Any number of operation words may
be given.

**TM-2** (black-box; must). Options and operation words may be interleaved.
`DEL:r --step 900` and `--step 900 DEL:r` are equivalent.

**TM-3** (black-box; must). The file is opened for writing and locked before
any argument is examined. A missing file fails with
`opening '<name>': No such file or directory`; a file without write
permission fails with `opening '<name>': Permission denied`; a file locked
by another process fails with `could not lock RRD`. These checks come before
any argument error, so `tune missing.rrd FOO` reports the open error.

**TM-4** (black-box; must). Header options are applied first and written into
the existing file in place. They stay written even if the structure change
that follows fails: `tune f -h g:900 DS:g:GAUGE:600:U:U` fails with a
duplicate-name error and leaves the heartbeat at 900 in the original inode.
The structure change then works on the file as modified by those options, so
`-r g:gg DEL:gg` deletes the renamed source.

**TM-5** (black-box; must). An option error is reported before any operation
word is examined: `-h zz:5 FOO` reports `unknown data source name 'zz'`
whichever order the words are in.

**TM-6** (black-box; must). When no operation word is given, the file is not
rewritten and its inode is kept. This holds even with `--step`, whose value
is then not checked.

**TM-7** (black-box; must). Errors are written to standard error as
`ERROR: <message>` followed by a newline, nothing is written to standard
output, and the exit status is 1. On success nothing is printed and the exit
status is 0.

## 2. Operation words

**TM-8** (black-box; must). Each word is classified by its prefix, matched
case-sensitively in this order: `DEL:`, `DS:`, `RRA#`, `RRA:`, `DELRRA:`.
A word matches only if at least one character follows the prefix. Any other
word, including `DEL:`, `DS:`, `RRA#`, `RRA:` and `DELRRA:` with nothing
after them and lower-case spellings, fails with
`unparsable argument: <word>`.

**TM-9** (black-box; must). All words are classified and parsed, in argument
order, before the file changes. The first word that fails decides the error
and the file is left as it was.

**TM-10** (black-box; must). `DEL:name` names a data source by its exact,
case-sensitive name. A name that does not exist is ignored without error.
Naming the same source twice deletes it once.

**TM-11** (black-box; must). `DELRRA:index` reads the index the way C `atoi`
does on a 64-bit platform: optional leading blanks, an optional sign, then
decimal digits up to the first non-digit; no digits means 0; the value is
then reduced to a signed 32-bit integer. So `DELRRA:x` and `DELRRA:-0`
delete archive 0, `DELRRA:2junk` deletes archive 2, `DELRRA: 1` and
`DELRRA:+1` delete archive 1, `DELRRA:4294967296` deletes archive 0 and
`DELRRA:99999999999` names archive 1215752191. A negative result fails with
`DELRRA requires a non-negative, integer argument`.

**TM-12** (black-box; must). `RRA#index:<op><number>` reads an unsigned
decimal index, a colon, one operator character and an unsigned decimal
number. Each number may have leading blanks and an optional sign; a minus
sign negates it modulo 2^32, and a value of 2^32 or more is reduced modulo
2^32. Text after the number is ignored. `RRA#00:+1`, `RRA# 1:+1` and
`RRA#1:+2junk` are valid; `RRA#1:-+1` removes one row; `RRA#4294967296:+1`
grows archive 0; `RRA#0:=-1` sets the row count to 4294967295.

**TM-13** (black-box; must). If the index, the colon, the operator or the
number cannot be read, the word fails with `Failed to parse RRA# command`
(for example `RRA#x`, `RRA#0`, `RRA#0:+`). If the operator is not `=`, `+`
or `-`, it fails with `Failed to parse RRA# command: invalid operation: <c>`
where `<c>` is the operator character, including a blank (`RRA#0: +1`).

**TM-14** (black-box; must). An `RRA#` index is compared with archive
positions as a signed 32-bit value, so an index of 2^31 or more (such as
`RRA#-1`) matches nothing.

**TM-15** (black-box; must). `DS:spec` and `RRA:spec` use the `rrdcreate(1)`
syntax and produce the same validation errors as `create`, for example
`invalid DS format`, `invalid DS type specified (FOO)`,
`Unrecognized consolidation function FOO`,
`Invalid xff: must be between 0 and 1`,
`Expected at least 4 arguments for RRA but got 3`,
`Invalid step 0: value must be positive` and
`Invalid row count 0: value must be positive`. Durations such as
`RRA:AVERAGE:0.5:1h:1d` are converted with the file's step (the new step
when `--step` is given). The texts come from the shared `create` parser and
are specified with it.

**TM-16** (black-box; must). `RRA#` and `DELRRA:` indices refer to the
archive positions of the file before the change, whatever other words are
given. Several words may name the same index; only the first of them in
argument order has any effect. `RRA#0:+5 RRA#0:+7` grows by 5;
`RRA#0:+5 DELRRA:0` grows by 5 and keeps the archive; `DELRRA:0 RRA#0:+5`
deletes it. An index with no archive is ignored without error.

## 3. Order of checks and errors

**TM-17** (black-box; must). After the words are parsed, the change is
checked in this order, and the first failure is reported with the file left
unchanged:

1. `--step` (section 8).
2. Data source deletions, then additions in argument order. An added name
   that equals a remaining source, or an earlier added one, fails with
   `Duplicate DS name: <name>`. A name freed by `DEL:` in the same command
   may be added again (`DEL:g DS:g:GAUGE:900:0:10` gives a new, empty `g`).
   The mapping syntax `DS:g=newg:...` reports `Duplicate DS name: g`.
3. Archive resizes and deletions.
4. Added archive definitions in argument order (`RRA:` parse errors).

So `DS:c:GAUGE:600:U:U RRA:FOO:0.5:1:1` and the reverse both report
`Duplicate DS name: c`, and `--step 7 DS:c:...` reports the step error.

**TM-18** (black-box; defect). When a `DS:` definition fails to parse and no
`RRA:` word is present, the reference still writes a new file containing the
partly parsed definition (uninitialised bytes) and then reports the parse
error with exit status 1. A replacement must report the same error text and
status and must leave the file unchanged. When an `RRA:` word is present the
reference reports the parse error without writing, which a replacement
reproduces.

**TM-19** (black-box; defect). When a valid `DS:` word is followed by one that
fails to parse (`DS:x:GAUGE:600:U:U DS:bad`), the reference reports
`Duplicate DS name: x`, because the failed definition still carries the
previous name. A replacement should report the parse error of the bad word
(`invalid DS format`) instead; either way the exit status is 1 and the file
is unchanged.

## 4. The rewritten file

A successful structure change writes a complete new file. These rules give
each part of it.

**TM-20** (black-box; must). Header: the cookie `RRD`, the version string
copied from the input, the standard float cookie, the data source count,
the archive count and the step. All other header bytes, including the bytes
after the version string and the 80 unused bytes at offset 48, are zero.

**TM-21** (black-box; must). The version string is never changed. Adding a
`DCOUNTER` or `DDERIVE` source to a version `0003` file leaves it `0003`.

**TM-22** (black-box; must). The last update time, seconds and microseconds,
is copied unchanged.

**TM-23** (black-box; must). Data source definitions: the kept sources, in
their original order, byte for byte, followed by the added sources in
argument order.

**TM-24** (black-box; must, with a mask). An added definition holds the name,
type, heartbeat, minimum and maximum that `create` would store for the same
specification, with zero bytes after the name and after the type string.
Byte 39 (the last byte of the type field) and bytes 64 to 119 (parameters
the basic types do not use) are not defined by the reference: they hold
leftover memory that varies from run to run (observed varying at offsets
64-67, 72-75, 88-91, 96-99 and 113-115). Comparisons mask these bytes. A
replacement should write zeros there.

**TM-25** (black-box; must). PDP state: kept sources keep their 112 bytes
unchanged. An added source gets `last_ds` = `U`, `unknown_sec` = *L mod S*
(using the step after any `--step` change), `value` = 0.0, and zero bytes
everywhere else.

**TM-26** (black-box; must). Archive definitions: the kept archives in their
original order, byte for byte except for the row count (and PDPs per row
under `--step`), followed by the added archives in argument order.

**TM-27** (black-box; must). Every archive in the result, kept or added, has
`cur_row` = *n − 1*, and its rows are stored in rotated order: the oldest
row first.

**TM-28** (black-box; must). CDP state of a kept archive: for a kept source
the 80 bytes are copied unchanged. For an added source, `value` is NaN,
`unknown_datapoints` is the number of whole steps already elapsed in the
current consolidation interval, `((L − (L mod S)) mod T) / S`, and all
other bytes are zero.

**TM-29** (black-box; must). Unknown values written by the change (added
source columns, unfilled rows, `value` fields set to NaN) use the platform's
default quiet NaN: `0x7ff8000000000000` on arm64 and aarch64, and
`0xfff8000000000000` on x86_64 (observed by the project's x86_64 oracle
runs). NaN values copied from the input keep their bits.

**TM-30** (black-box; must). The result has no bytes after the last row,
whatever the input had there.

## 5. Rows of kept archives

**TM-31** (black-box; must). The new row count of an archive is computed with
32-bit two's-complement arithmetic: `=N` gives *N*, `+N` gives *n + N* and
`-N` gives *n − N*, each taken modulo 2^32 and read as a signed 32-bit
value. A result of zero or less deletes the archive. So `RRA#0:-10` on a
10-row archive, `RRA#0:=0`, `RRA#0:=4294967295` and `RRA#0:=-1` all delete
it.

**TM-32** (black-box; must). Kept rows are written in rotated order. A source
deleted by `DEL:` loses its column; an added source gets an unknown column.

**TM-33** (black-box; must). Shrinking removes the oldest rows and keeps the
newest *n'* rows.

**TM-34** (black-box; must). Growing inserts the new rows before the oldest
kept row. They are filled from the input file by the back-fill rules of
section 6 (row times from TM-40) and are unknown where no value is found.

**TM-35** (reference-source-inspection, confirmed black-box; must, flagged as a
reference defect). When an archive is deleted, the rows of every later kept
archive are read from the wrong place in the input. The input's row data is
treated as one sequence of values, archive after archive. The rows of the
*k*-th kept archive are read starting at the position equal to the total
size (rows × input data sources) of the kept archives before it, not of all
the archives before it, and are then rotated using that archive's own row
count and `cur_row`. With `DELRRA:0` on a file whose archive 0 has 10 rows,
archive 1's rows therefore come from the first rows of the deleted archive's
data. The vectors pin this (`DELRRA:0`, `RRA#0:-10`, `DELRRA:1 DELRRA:0`).
The reads stay inside the input. A replacement reproduces it while byte
compatibility is the goal; fixing it is a deliberate divergence that must
update the vectors and this section.

**TM-36** (black-box; defect). `RRA#i:+N` with an *N* so large that *n + N*
reaches 2^31 or wraps (for example `+4294967291` or `+4294967295`) makes the
reference write past its buffers and die from a signal (SIGBUS on macOS,
SIGSEGV on Linux) with the file untouched. A replacement must fail with an
`ERROR:` message and exit status 1 and leave the file unchanged.

**TM-37** (reference-source-inspection; defect). `RRA#i:-N` whose
subtraction wraps to a positive count (for example `-4294967295` on a
10-row archive gives 11 rows) makes the reference write an archive whose
rows were never filled. Their content is undefined. A replacement should
reject such a count with a clean error; tests do not pin this case.

## 6. Back-fill of new rows

These rules fill the rows added by `RRA#i:+N` (or `=N` above the current
count) and every row of an archive added by `RRA:`. They read only the input
file (after the `--step` pass of section 8, when given).

**TM-38** (black-box; must). Only AVERAGE, MIN, MAX and LAST targets are
filled. A target with another CF keeps all new rows unknown. If the input has
no archives, nothing is filled.

**TM-39** (reference-source-inspection, confirmed black-box; must).
Candidates are the input archives whose CF equals the target's CF or is
AVERAGE. The archive being grown is itself a candidate (its input rows). The
candidates are tried in order of fewer PDPs per row first and, for equal
PDPs per row, more rows first.

**TM-40** (reference-source-inspection, confirmed black-box; must). New row
*r* (counting from 0 at the oldest new row) of a target with *n'* rows and
slot length *T* ends at *E − ((c − r) mod n')·T*, where *E* is the end of
the target's newest slot and *c* is a row pointer. For an added archive and
for a grown archive with no deleted archive before it, *c = n' − 1*, so the
new rows are the oldest slots of the target. For a grown archive at input
position *j* when archives before it were deleted, *c* is the `cur_row` of
the output archive at position *j*, which is a different archive. The
vectors pin this (`DELRRA:0 RRA#1:+4`, `DELRRA:1 RRA#4:+5`). If there is no
output archive at position *j*, the reference reads an unrelated value and
the result varies between runs: a replacement may use *n' − 1* there, and
tests do not pin it.

**TM-41** (reference-source-inspection, confirmed black-box; must). The row
interval is *(start − 1, end]* with *start = end − T + 1*. For one candidate
with slot length *Tc*, *n_c* rows and pointer *c_c*, a time *t* is looked up
by rounding it up to a multiple of *Tc*; the result is the row that covers
it, or none if the rounded time is later than *L* or lies before the
candidate's oldest row. Then:

- If neither *start* nor *end* has a row, the candidate is skipped.
- If only *end* has a row, the range starts at the candidate's newest row
  (*c_c*) and runs forward, wrapping, to the end row. The newest row is
  therefore included even though it lies after the interval.
- If only *start* has a row, the range ends at the row before the newest,
  *c_c − 1*. When *c_c* is 0 that index is computed as
  *(2^64 − 1) mod n_c*: the last row when *n_c* is a power of two, but row
  0 for *n_c* = 5 and row 3 for *n_c* = 6. The `wrap3` and `wrap3b` vectors
  pin these.
- Otherwise the range runs from the start row to the end row.

The number of rows is *end_row − start_row + 1*, plus *n_c* if that is
negative; rows are read forward from the start row, wrapping at *n_c*.

**TM-42** (reference-source-inspection, confirmed black-box; must). For each
data source of the target and each new row, candidates are tried in order
until one yields a known value; later candidates are not consulted for that
source and row. A source added by `DS:` stays unknown. Within the chosen
range, unknown values are skipped and the rest are combined by the target's
CF:

- AVERAGE: an accumulator starts at the first known value times *Tc*; each
  later known value adds *value·Tc*; the result is the accumulator divided
  by *Tc* times the number of known values. Rows are weighted by the whole
  candidate slot even when only part of it overlaps the target row.
- MIN and MAX: the smallest or largest known value.
- LAST: the last known value in the range's reading order, which with the
  wrap of TM-41 is not always the newest.

If every value in the range is unknown, the next candidate is tried. The
`xff` of target and candidate is not used.

**TM-43** (black-box; should, platform specific). The AVERAGE accumulation
of TM-42 is computed with a fused multiply-add (`value·Tc + accumulator`
rounded once) by the macOS arm64 build and with a separate multiply and add
by the Linux aarch64 build. Baseline x86_64 builds have no fused
instruction, so they are expected to round like Linux aarch64; the x86_64
CI run checks this. The two results differ in the last bit for some inputs
(vectors `base-rra-average-0-5-3-9`, `types-rra-average-0-5-2-20`,
`types-rra-average-0-5-5-6` and others carry Linux overrides for this). A
replacement matches the platform it runs on.

**TM-44** (black-box; should, platform specific). Candidates with the same
PDPs per row and the same row count have no defined order in the reference:
it is whatever the C library sort leaves. On Linux the input order is kept.
On macOS it is not always kept: in `ties.rrd`, whose 8 AVERAGE candidates
include two tied 2-PDP archives at input positions 2 and 3, position 3 was
tried first for `RRA#5:+10`. A replacement keeps input order on Linux; the
`ties` vectors carry Linux overrides where the platforms differ.

## 7. Consolidation state of added archives

**TM-45** (reference-source-inspection, confirmed black-box; must). For an
added archive, the *chosen candidate* is the first candidate of TM-39 that
either has one PDP per row or uses AVERAGE. For each data source:

- If there is no chosen candidate, or the source was added by `DS:`, the CDP
  state is as in TM-28: `value` NaN, `unknown_datapoints` =
  `((L − u) mod T) / S` where *u* is that source's `unknown_sec` after the
  change, and zero elsewhere.
- Otherwise all 80 bytes start at zero and are set as follows.

**TM-46** (reference-source-inspection, confirmed black-box; must). The
`primary_value` is read from the new file's row data treated as one
sequence of values: at position *R + D·(n' − 1) + i*, where *R* is the total
number of rows (not values) of the archives written before this one, *D* is
the output data source count and *i* the source index. With one data source
this is the archive's newest row; otherwise it is usually a value from an
earlier archive or a different source. The position always lies inside the
data already written.

**TM-47** (reference-source-inspection, confirmed black-box; must). Let
*end* be *L* when *L* is a multiple of *T*, else the next multiple of *T*
after *L*, and *start = end − T + 1*. Look up *start* in the chosen candidate
by the rule of TM-41; the end row is always the candidate's newest row.

- If *start* has no row, `secondary_value` is the chosen candidate's own
  `primary_value` for that source, and `value` and `unknown_datapoints`
  stay 0.
- Otherwise `secondary_value` is the candidate's value in the row before the
  start row (wrapping to the last row). Then each candidate row from the
  start row to the newest row, in order, is folded in as one PDP: an unknown
  value adds 1 to `unknown_datapoints`; a known value updates `value`, which
  starts at 0.0 rather than unknown: AVERAGE adds it, MIN and MAX keep the
  smaller or larger of `value` and it, LAST replaces `value`. Starting from
  0.0 means MIN of positive values stays 0.0 and MAX of negative values
  stays 0.0, as the `neg` vectors show.

**TM-48** (black-box; must). Rows of an added archive come only from TM-38 to
TM-44; its `cur_row` is *n' − 1* (TM-27).

## 8. `--step`

**TM-49** (black-box; must). The value of `--step` (or `-t`) is read like C
`atoi`. Zero, negative and non-numeric values disable the step change
without error.

**TM-50** (black-box; must). With a positive value *S'* and at least one
operation word, the old step must be a multiple of *S'* with *S / S' > 1*;
otherwise the command fails with
`invalid 'newstep' parameter. The newsize must divide the old step parameter without a remainder.`
An equal step is therefore an error.

**TM-51** (black-box and reference-source-inspection; must). The change runs
in two passes. First the whole file is rewritten with no operations (rows in
rotated order, `cur_row` = *n − 1*) and then stretched with *k = S / S'*:
the step becomes *S'*, every archive's PDPs per row is multiplied by *k*,
and for AVERAGE, MIN, MAX and LAST archives every `unknown_datapoints` is
multiplied by *k*. PDP state, including `unknown_sec` and `value`, is not
changed. Slot lengths, and so row times, stay the same. The second pass
applies the operation words to that stretched file using the rules above,
with archive indices still meaning the original positions. Back-fill and CDP
preparation in the second pass read the stretched file, whose `cur_row`
pointers are all *n − 1*.

## 9. Replacing the file

**TM-52** (black-box; must). The new file is written under a temporary name
in the same directory and renamed over the original. The original's inode is
replaced, so a hard link keeps the old content and a symbolic link given as
the file name is replaced by a regular file while its target is left alone.
The permission bits of the original are applied to the new file; the owner
is the user running the command.

**TM-53** (black-box; must). If the temporary file cannot be created (for
example in a read-only directory), the command fails with
`Cannot create temporary file` and the original is unchanged.

**TM-54** (black-box; must). On success or failure no temporary file is left
in the directory.

**TM-55** (documentation; should). With `--daemon`, the cache daemon is asked
to flush the file before the change and to forget it afterwards. Concurrent
updates through the daemon may be lost. This is not covered by the vectors.

**TM-56** (black-box; should). The original is held under an exclusive lock
from open until the replacement. A replacement uses a lock that conflicts
with both `flock` and POSIX record locks held by other processes, as
RRDtool's does on macOS. Linux lock semantics were not measured.

## 10. Edge cases

**TM-57** (black-box; should, platform specific). Deleting every data source
works on macOS and writes a file with no data sources. On Linux the
reference build fails in some of these cases, because it asks the C library
to resize a non-empty buffer to zero bytes and glibc returns no buffer. The
observed Linux results are:

| Result after the change | Linux outcome |
| --- | --- |
| no data sources, 2 or more kept archives | `Cannot allocate memory`, file unchanged |
| no data sources, 1 kept archive, no added archive | success, as on macOS |
| no data sources, 0 kept archives, no added archive | success, as on macOS |
| no data sources, 1 or more kept archives and an added archive | `out of memory` from the archive step when only one is kept; `Cannot allocate memory` when two or more are kept |
| no data sources, 0 kept archives and an added archive | `out of memory`, file unchanged |

A replacement states this table as an explicit rule per platform rather than
emulating an allocator. The Linux vectors carry overrides for these cases.

**TM-58** (black-box; must). Deleting every archive writes a valid file with
no archives. RRDtool's own `info` and `dump` abort on such a file; that is
their behavior, not tune's.

**TM-59** (black-box; must). Growing or shrinking an archive that does not
exist, deleting a data source that does not exist and `RRA#i:+0` or `=n`
with the current count all still rewrite the file (TM-27 rotation and the
other rules apply), with exit status 0.

**TM-60** (black-box; should). Rondi refuses `COMPUTE` data sources and
Holt-Winters archives (`HWPREDICT`, `MHWPREDICT`, `SEASONAL`,
`DEVSEASONAL`, `DEVPREDICT`, `FAILURES`) with its own
`... not supported by Rondi` error and leaves the file unchanged. The
reference accepts them. This specification does not describe their effect.

## Summary by class

Must reproduce exactly: TM-1 to TM-17, TM-20 to TM-35, TM-38 to TM-42,
TM-45 to TM-54, TM-58 and TM-59. Where these touch file bytes the vectors
compare whole files, masking only TM-24's bytes and folding x86_64 NaN signs
(TM-29).

Should reproduce: TM-43, TM-44 and TM-57 (per platform), TM-55, TM-56 and
TM-60.

Reference defects not to copy: TM-18 (write garbage then fail), TM-19
(misleading duplicate-name text), TM-36 (crash on huge growth), TM-37
(unfilled rows after a wrapped shrink), the unrelated read in TM-40 when no
archive sits at the grown archive's position, and the uninitialised bytes of
TM-24. TM-35 is a deterministic reference defect that is reproduced for now.

## Known divergences

Current Rondi against RRDtool 1.11.0, as of this specification:

- TM-18: Rondi reports the parse error without writing the file
  (intentional; vectors `base-ds-bad`, `base-ds-9bad-gauge-x-u-u`,
  `base-ds-z-gauge-600-5-1`, expectation `refuse`).
- TM-19: Rondi reports `invalid DS format` where RRDtool reports
  `Duplicate DS name: x` (intentional; expectation `clean-error`).
- TM-36: Rondi fails with `ERROR: out of memory` where RRDtool crashes
  (intentional; expectation `clean-error`).
- TM-37: both write 11 rows for `RRA#0:-4294967295`, with different row
  content (undefined in the reference; not pinned).
- TM-40, no archive at the grown position: RRDtool's result varies between
  runs; Rondi's is stable (not pinned).
- TM-60: COMPUTE and Holt-Winters additions are refused (expectation
  `unsupported`).

No other difference was found on macOS arm64: Rondi matches all 282
vectors there. Linux results are taken from CI on x86_64 and aarch64.

## Tests

- `crates/rondi-cli/tests/tune_modify_spec.rs` replays every vector against
  Rondi, runs them live against the pinned RRDtool when it is on `PATH`
  (always under `RONDI_REQUIRE_ORACLE=1`) and compares Rondi with it
  directly, and checks the file replacement rules of section 9.
- `crates/rondi-cli/tests/data/tune_modify/inputs/*.rrd` are input files
  created and updated by RRDtool 1.11.0 on macOS. They are frozen because
  `create` picks random initial rows.
- `cases.tsv` lists each case: id, input, Rondi expectation (`same`,
  `refuse`, `clean-error`, `unsupported`) and the tune arguments.
- `expected/<input>.vectors` holds, for every case on that input, the exit
  status, standard output, standard error, the resulting file (or an
  `unchanged` marker) and, for about a quarter of the cases and every
  `--step` case, the `info` and `dump` text of the result. Sections are
  framed as `@@ <id> <section> <length>` followed by the bytes and a
  newline. `expected/<input>.linux.vectors` replace individual cases on
  Linux.
- `scripts/gen-tune-modify-vectors.py write` rebuilds everything from a
  local RRDtool 1.11.0. It runs each case three times with different
  environment sizes and fails if any byte outside the TM-24 mask varies.
  `scripts/gen-tune-modify-vectors.py overrides --platform linux`, run in a
  Linux container with the pinned build, records the Linux overrides.
