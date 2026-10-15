# `rrdtool resize`

This specification describes what RRDtool 1.11.0 does when `rrdtool resize`
adds rows to or removes rows from one archive: the arguments and the order
of their checks, the error texts, where rows are inserted or removed, the
resulting file, how inputs that do not match their own header are handled,
and the fixed `resize.rrd` output file. It is written so that `resize` can be
implemented from it and from the vectors in
`crates/rondi-cli/tests/data/resize` without reading RRDtool's source.

Provenance tags and classes are as in [`create.md`](create.md):
**documentation** (`rrdresize(1)` or the usage text), **black-box**
(observed with `/opt/homebrew/bin/rrdtool` 1.11.0 on macOS arm64 and the
pinned 1.11.0 build on Linux aarch64 and x86_64, pinned by the vectors), and
**reference-source-inspection** (none in this document); **must**,
**should** and **defect**.

## Terms

- The *input* is the file named on the command line, the *target archive*
  the one at index *i*, with *n* rows and current row *c*.
- File layout terms (header, definitions, PDP and CDP state, current-row
  words, rows) are those of `create.md` section 10.
- *Physical* row *r* is the *r*-th row stored in the file. The oldest row of
  an archive is the one after *c*, wrapping to row 0.

## 1. Arguments

**RZ-1** (documentation; must). The synopsis is
`rrdtool resize filename rra-num GROW|SHRINK rows`. `rrdtool resize` with no
further argument prints the usage text on standard output and exits 0 (its
build line differs per platform; Linux overrides).

**RZ-2** (black-box; must). Errors are one `ERROR: <message>` line on
standard error with exit status 1 and nothing on standard output. Success
prints nothing (except the warnings of RZ-15) and exits 0. There are no
options; `resize` takes its words exactly as given.

**RZ-3** (black-box; must). Checks, in order, the first failure deciding:

1. If the file name is exactly `resize.rrd`: `resize.rrd is a reserved name`,
   whatever the other words are (also with no other word). `./resize.rrd`
   is not caught (RZ-21).
2. Unless there are exactly four words after `resize`:
   `wrong number of parameters`.
3. Unless the third word is exactly `GROW` or `SHRINK` (case-sensitive, no
   blanks): `I can only GROW or SHRINK`.
4. The row count is read like C `strtol` with base 0 (RZ-4). Unless it is
   at least 1: `Please grow or shrink with at least 1 row` (`0`, `-1`,
   `abc`).
5. The input is opened: `opening '<name>': <system error text>` when it
   cannot be (`No such file or directory`). A file that is not an RRD fails
   with the RRD reader's message, for example
   `reached EOF while loading header rrd->stat_head` for a short text file.
   Header and size checks follow (section 4).
6. The archive index is read like `strtol` with base 0 and taken as an
   unsigned 64-bit number. Unless it is below the archive count:
   `no such RRA in this RRD` (`3` with three archives, `-1`, `4294967296`,
   `18446744073709551616`, and `010`, which is octal 8).
7. For `SHRINK`, unless the row count is below *n*:
   `This RRA is not that big` (removing every row, or more, is refused).
8. The output file is opened (section 3).

So `missing.rrd 9 BAD 0` reports the action error and an existing
`resize.rrd` is left untouched by any failure in steps 1 to 7.

**RZ-4** (black-box; must). `strtol` with base 0: leading white space, an
optional sign, then `0x` hexadecimal, `0` octal or decimal digits, stopping
at the first character that does not fit; no digits gives 0; values beyond
the signed 64-bit range saturate. So `0x2` is 2, `2y` is 2, ` 2` is 2, `+1`
is 1, an index of `x` or the empty word is 0, and a row count of
`99999999999999999999` saturates to 2^63−1 (and fails RZ-3 step 7).

## 2. Rows

**RZ-5** (documentation, black-box; must). `GROW k` inserts *k* unknown rows
into the target archive immediately after physical row *c*, so they become
the oldest rows; *c* is unchanged. With *c* = *n* − 1 the rows are appended
after the last physical row. The unknown value is the platform's default
quiet NaN (`create.md` CR-74).

**RZ-6** (documentation, black-box; must). `SHRINK k` removes the *k* oldest
rows: first the physical rows after *c*, in order, then, if fewer than *k*
follow *c*, the remaining number from physical row 0 upward. *c* decreases
by the number of rows removed from before it. Examples with *n* = 5:
*c* = 0, `SHRINK 4` keeps physical row 0 with *c* = 0; *c* = 2, `SHRINK 4`
removes rows 3, 4, 0 and 1 and keeps row 2 with *c* = 0; *c* = 4,
`SHRINK 1` removes row 0 and gives *c* = 3 (vectors `ptr0`, `ptrmid`,
`ptrlast`).

**RZ-7** (black-box; must). Every other archive's rows and current row, and
the target archive's other rows, are copied unchanged, in physical order.

**RZ-8** (black-box; must). Any archive may be resized, including
Holt-Winters archives (vectors `hw.rrd`); the rules are the same and no
Holt-Winters parameter or linked archive is adjusted.

**RZ-9** (black-box; must). A current row outside 0..*n*−1 in the input is
not corrected; the vectors `currow.rrd` pin the result.

## 3. The output file

**RZ-10** (documentation, black-box; must). The result is written to the
file `resize.rrd` in the current working directory, wherever the input is.
The input is opened read-only and never changed (same bytes and inode).

**RZ-11** (black-box; must). The output equals the input with the target
archive's row count in its definition changed to *n* + *k* or *n* − *k*,
its rows and current row changed by RZ-5 or RZ-6, and everything else
(header, version string, all definitions, last update, PDP and CDP state,
other archives) copied byte for byte. The output length is the input's
layout length plus or minus *k* times the data source count times 8.

**RZ-12** (black-box; should). The output is opened for writing with
create-and-truncate semantics, not written to a temporary file:

- A new `resize.rrd` gets mode 0666 minus the umask.
- An existing regular file is truncated and rewritten in place: it keeps its
  inode, its permission bits and every hard link to it.
- A symbolic link named `resize.rrd` is followed and its target rewritten.
- If it cannot be opened, the command fails with
  `Can't create 'resize.rrd': <system error text>`, for example
  `Permission denied` for a read-only file and `Is a directory`.

A replacement should reproduce the mode rule and the in-place rewrite of an
existing regular file. Following a symbolic link is a defect (a fixed name
in the working directory followed through a link can overwrite any file the
user can write); a replacement may refuse it. Rondi diverges from all of
RZ-12 (RZ-div-existing-output).

**RZ-13** (black-box; defect). Because of RZ-12, an input named
`./resize.rrd` (or any other spelling of the output's path) is truncated
by the output open before it is read: RRDtool then fails with
`Do not know how to handle RRD version ` and leaves a damaged 1728-byte
`resize.rrd` (vector `007`). The original data is lost. A replacement must
not destroy its input; refusing the command (as Rondi does) is the expected
behavior.

## 4. Inputs that do not match their header

**RZ-14** (black-box; must). Only versions `0001` to `0004` are resized. A
version `0005` file (any file with a `DCOUNTER` or `DDERIVE` data source,
`create.md` CR-37) fails with `Do not know how to handle RRD version 0005`
after the output was opened, leaving a `resize.rrd` of the resized length
whose content the vector `v5.rrd` pins. This is a defect (RZ-18); the vectors pin
it and Rondi reproduces it.

**RZ-15** (black-box; must). An input longer than its header implies is
resized as if the excess were absent, and the excess is not copied. For
*E* excess bytes, one pair of lines is printed on standard error per 8
bytes, the last pair naming what remains:
`WARNING: ignoring last <m> bytes` followed by
`WARNING: if you see this message multiple times for a single file you're in trouble`,
with *m* = 8 for each full 8 bytes and the remainder for the last pair.
Pinned for *E* = 5, 7, 16 and 24 (one pair of 5, one of 7, two of 8, three
of 8). The exit status is 0.

**RZ-16** (black-box; must). An input shorter than its header implies fails
with `'<name>' is too small (should be <length> bytes)` before the output is
opened.

**RZ-17** (black-box; must). A version `0001` input (whose live header is 8
bytes, without microseconds) is written as version `0003` but with the
version 1 layout, so the output is 8 bytes shorter than a version 3 file
and RZ-15's warning is printed once for the missing microseconds (vector
`v1.rrd 0 GROW 1`). `SHRINK` on such a file fails with
`attempting to write beyond end of file (<a> + 8 > <a>)` and leaves a
partial `resize.rrd` (vector `v1.rrd 1 SHRINK 1`). Both are reference
defects that the vectors pin and Rondi reproduces.

**RZ-18** (black-box; defect). RZ-14 and RZ-17 leave a partial or invalid
`resize.rrd` behind on failure. A replacement should not leave a partial
output; reproducing it is accepted while byte compatibility is the goal.

## 5. Sizes

**RZ-19** (black-box; defect). `GROW` with a huge row count (such as
4294967296 or 2^63−1) makes RRDtool write unknown rows until it finishes
or the disk fills; it ran for minutes without finishing. This is not
pinned. A replacement should refuse a result it cannot write, with an
`ERROR:` line, before creating `resize.rrd`.

**RZ-20** (black-box; should). When a declared row count is so large that
the file's implied length overflows (for example 2^61 + 10 rows), Rondi
refuses the input with `invalid RRD file: RRA rows extend past the end of
the file` and writes nothing, while RRDtool would copy rows until it fails.
The existing test `huge_row_count_resize_is_refused` in
`crates/rondi-cli/tests/csrc_header.rs` covers this; it is not part of the
vectors.

## 6. Defects not to copy

**RZ-21** (black-box; defect). The reserved-name check of RZ-3 compares the
argument text only, so `./resize.rrd`, an absolute path or a link to
`resize.rrd` pass it and lead to RZ-13.

## Summary by class

Must reproduce exactly: RZ-1 to RZ-11, RZ-14 to RZ-17.

Should reproduce: RZ-12 (except following links), RZ-20.

Reference defects not to copy: RZ-12's symbolic link following, RZ-13,
RZ-18 (partial outputs, currently reproduced), RZ-19, RZ-21.

## Known divergences

Current Rondi against RRDtool 1.11.0, as of this specification:

- RZ-div-existing-output (RZ-12, RZ-13): Rondi creates `resize.rrd`
  exclusively with mode 0600. When the name already exists (regular file,
  symbolic link or directory) it fails with
  `Can't create '<absolute path>': File exists` and changes nothing, where
  RRDtool rewrites the file in place, follows the link, or reports
  `Can't create 'resize.rrd': Is a directory`. A new file gets 0600 instead
  of 0666 minus the umask. The first part is intentional (safer); the mode
  and the absolute path in the message are not pinned by vectors.

No other difference was found: Rondi matches the other 87 vectors on macOS
arm64.

## Tests

- `crates/rondi-cli/tests/resize_spec_vectors.rs` replays every vector
  against Rondi under the `rrdtool` name, runs them live against the pinned
  RRDtool when it is on `PATH` (always under `RONDI_REQUIRE_ORACLE=1`) and
  compares Rondi with it directly, and checks RZ-10 for both.
- `crates/rondi-cli/tests/data/resize/inputs/*.rrd` are frozen inputs: files
  created and updated by RRDtool 1.11.0, some with current rows and row
  values set by the generator so that row moves are visible
  (`ptr0`, `ptrmid`, `ptrlast`, `hw`, `v5`), a version 1 conversion, files
  with trailing bytes, a truncated file, a file whose current row is out of
  range, and a text file.
- The case table and the vector format are those of `create.md`; resize
  outputs are compared byte for byte, not masked.
  `expected/resize.linux-<arch>.vectors` hold the Linux overrides.
- `scripts/gen-create-resize-vectors.py` rebuilds both suites.
