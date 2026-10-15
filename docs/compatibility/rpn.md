# RPN expressions (CDEF, VDEF, xport)

This specification describes the observable behavior of RRDtool 1.11.0's
RPN language as used by `CDEF` in `graph`, `graphv` and `xport`, and the
`VDEF` functions that share the same definitions. It is written for an
implementer who has not seen RRDtool's source. It says what results and
errors a user can observe, not how RRDtool computes them.

Each rule carries a provenance tag:

- **[doc]** the `rrdgraph_rpn(1)` or `rrdgraph_data(1)` manual page;
- **[bb]** black-box runs of the RRDtool 1.11.0 executable, recorded in the
  vector table;
- **[rsi]** reference-source inspection, used only where black-box testing
  could not reasonably settle the rule. These rules are stated as behavior;
  where practical a vector confirms them.

Where black-box results contradict the manual, the black-box result is the
contract and the conflict is noted.

## Compatibility tests

- `crates/rondi-cli/tests/data/rpn_vectors.json`: 805 vectors, each an
  `rrdtool` argument list, an environment, tags, and the expected result
  (exit status, stderr, and either the `xport` rows or the `graphv`
  `print[...]` lines, or stdout on failure). The fixture commands that build
  the input files are stored in the same file.
- `crates/rondi-cli/tests/rpn_spec_vectors.rs` replays every vector against
  Rondi under the `rrdtool` name. When the pinned RRDtool 1.11.0 is on
  `PATH` it also runs each vector through it and requires the same result;
  `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure.
- `scripts/rpn-vectors.py` regenerates the table from the reference
  executable (`scripts/rpn-vectors.py --rrdtool /path/to/rrdtool`), runs each
  case twice, and tags a case `undefined` when it crashes or the two runs
  differ. `--check PATH` diffs another binary against the table, and
  `--probe EXPR` prints one ad-hoc result.

Vector tags:

| Tag | Meaning | Replay rule |
|---|---|---|
| `undefined` | The reference crashed, or its result depends on memory contents. | Not compared; the replacement must exit without a signal. |
| `host-sort` | The result depends on the C library sort with an inconsistent comparator (see [Ordering](#ordering-used-by-sort-median-percent-and-predictperc)). | Compared against `expect_platform` where recorded, otherwise on macOS only; the live oracle comparison runs everywhere. |
| `first-weekday` | `NEWWEEK`, whose first weekday comes from the platform. | As `host-sort`. |
| `fma` | The last bit depends on fused multiply-add. | Exact on aarch64; relative tolerance 1e-9 elsewhere. |

The main `expect` was captured on macOS arm64. Where the Linux builds of
the reference produce a different result, the vector also carries
`expect_platform.linux-aarch64` or `expect_platform.linux-x86_64`, and the
replay test uses that value on the matching target. Those entries are
produced by `scripts/rpn-vectors.py --record-platform` inside the CI image
and merged with `--merge-platform`. The current recordings:

- `linux-aarch64` (13 vectors, recorded with the oracle in the CI image):
  every `host-sort` vector whose input mixes infinities with other values
  (`sort-10`, `sort-12`, `sort-14`, `sort-18`, `sort-19`, `sort-23`,
  `median-02`, `median-04`, `percent-17` to `percent-19`,
  `predictperc-15`, `vdef-22`). All other vectors, including
  `first-weekday` and every time zone vector, match the macOS capture.
- `linux-x86_64` (3 vectors, taken from the CI x86_64 job, where Rondi's
  output matched the live oracle): `rounding-06`, `vdef-21` (`-nan`, the
  sign of the platform's unknown value) and `vdef-22`. The other `host-sort`
  vectors are covered on x86_64 by the live oracle comparison only.

Known divergences are listed in `KNOWN_DIVERGENCES` in the test file. Their
expected value stays RRDtool's.

### Fixture

All vectors run with `TZ=UTC` and `LC_ALL=C` unless the vector sets
otherwise. The fixture has three files:

- `f.rrd`, step 10 s, five GAUGE data sources over 300 rows starting at
  `1000000010`: `a` (assorted finite values), `b` (small integers with an
  unknown every fifth row), `c` (the row number), `d` and `e` (a cycle of
  `inf`, `-inf`, unknown, `0`, `-2.5`, `1`, `3`, `0.5`, `1e-300`, `1e300`,
  `-7`, `2`, in two different orders so that `d,e` pairs vary per row).
- `g.rrd`, step 30 s, data source `z`, for mixed-resolution cases.
- `h.rrd`, step 3600 s from 2020-12-27 00:00 UTC for about 100 days, with a
  6-hour consolidated archive, for calendar operators.

Most vectors export 12 rows (`--start 1000002000 --end 1000002120`).
Constant expressions use one row and are written `a,POP,...` because of the
rule in [Variables are required](#variables-are-required).

## Data model

- A CDEF evaluates its expression once per output row. Each evaluation
  starts with an empty stack and must end with exactly one value, which
  becomes the row's value. [doc]
- Values are IEEE 754 binary64. *Unknown* is NaN; `inf` and `-inf` are
  ordinary values. The sign of a NaN is not observable in output. [bb]
- `xport` prints a value as `%.10e` (for example `1.2500000000e+00`), NaN as
  `NaN`, and infinities as `inf` and `-inf`. A negative zero prints like
  zero; its sign is observable only through `1,EXC,/`, which yields `-inf`.
  Subnormal results print in RRDtool's own style (`0.0000000001e-308`);
  that formatting belongs to `xport`, not to RPN. [bb]
- Row context available to operators: the row's timestamp, the row's
  zero-based index, the step width, and the previous value of this CDEF.
  [doc]

## Grammar

An expression is a sequence of tokens separated by single commas. [doc]

- Separators: exactly one `,` between tokens. Whitespace is not a separator
  and is not trimmed: `a, 1,+`, ` a`, `a ` and `a,1,+ ` are rejected with
  `don't understand '...'`. An empty token (`a,,1,+`, `,a`) is rejected the
  same way. [bb]
- A single trailing comma is accepted: `a,` and `a,1,+,` evaluate as if it
  were absent. [bb]
- An empty expression (`CDEF:r=`) fails with
  `can not parse an empty rpn expression`. [bb]
- Tokens are tried in this order: number, operator, `PREV(name)`, variable
  name. A token that matches none fails the whole expression with
  `don't understand '<rest>'`, where `<rest>` is the remaining expression
  text from the failing token to the end. [bb]

### Numbers

- A number token consists only of the characters `0-9 . e + -`, is at most
  40 characters long, and must be followed by a comma. A number in the last
  position is therefore never a number: `a,POP,5` and `a,1,2` fail with
  `don't understand '5'` / `'2'`. [bb]
- The token must convert completely and in range as a decimal floating
  point literal: `1.5`, `.5`, `5.`, `1e3`, `1e+3`, `1e-3`, `+1`, `-1`,
  `00012`, 40-digit integers. `1e` is accepted and means `1`. Rejected (as
  `don't understand`): `1E3` (upper-case exponent), `1e1100`, `-1e1100` and
  `1e-1100` (overflow and underflow), `1..2`, `1-2`, `--1`, `1.5e2.5`,
  `0x10`, `inf`, `nan`, `.`, and 41-character numbers. [bb]
- A lone `-` or `+` is the subtraction or addition operator. [bb]

### Operators

- Operator names are upper-case and must be followed by a comma or the end
  of the expression. `dup`, `add`, `DUPX`, `INFX`, `TRENDNANX`, `+x` and
  `COUNT(a)` are not operators. [bb]
- The operator set is exactly the one listed in `rrdgraph_rpn(1)`:
  `+ - * / %`, `ADDNAN`, `POW`, `SIN COS LOG EXP SQRT ATAN ATAN2`,
  `FLOOR CEIL ROUND`, `DEG2RAD RAD2DEG ABS`, `LT LE GT GE EQ NE`, `UN ISINF`,
  `IF`, `MIN MAX MINNAN MAXNAN LIMIT`, `UNKN INF NEGINF`, `PREV`,
  `PREV(name)`, `COUNT`, `NOW`, `TIME`, `LTIME`, `STEPWIDTH`,
  `NEWDAY NEWWEEK NEWMONTH NEWYEAR`, `DUP POP EXC DEPTH COPY INDEX ROLL`,
  `SORT REV AVG SMIN SMAX MEDIAN STDEV PERCENT`, `TREND TRENDNAN`,
  `PREDICT PREDICTSIGMA PREDICTPERC`. [doc]
- An operator name takes precedence over a variable of the same name: a DEF
  named `MAX` or `PREV` cannot be referenced. [bb]

### Variables

- A variable token names an earlier `DEF`, `CDEF` or `VDEF`. Names may
  contain letters, digits, `_` and `-` (`x1`, `a-b`, `MAXX`, `e`, `e1`).
  [bb]
- A name that also parses as a number (`12`, `1e5`) is read as the number,
  so such a DEF cannot be referenced. [bb]
- A name defined later on the command line is unknown at this point and
  fails with `don't understand`. [bb]
- `PREV(name)`: the name in parentheses must be an existing variable, else
  `variable 'name' not found`. `PREV(a)` followed by anything other than a
  comma or the end (`PREV(a)x`), or missing its closing parenthesis
  (`PREV(a,+`), fails with `garbage in RPN: '<rest>'`. `PREV()` and
  `PREV( a)` fail with `don't understand`. [bb]

### Variables are required

A CDEF expression that references no DEF, CDEF or VDEF variable (`1,2,+`,
`NOW`, `UNKN`, `PREV`, a DEF whose name reads as a number) fails with
`rpn expressions without DEF or CDEF variables are not supported`. A lone
number fails earlier with `don't understand`. [bb]

## Evaluation and stack errors

- Every operator checks that the stack holds enough operands before it
  acts; otherwise the evaluation fails with `RPN stack underflow`. For
  operators with a count operand, the count is popped first and then the
  stack must hold that many further values. [bb]
- At the end of each row, a stack depth other than one fails with
  `RPN final stack size != 1`. [bb]
- The stack has no observable fixed limit (3000 duplicated values evaluate
  normally). [bb]
- Count and index operands are truncated toward zero to an integer
  (`2.9` behaves as `2`). [bb]
- All errors print `ERROR: <message>` on stderr, exit with status 1 and
  print nothing on stdout. [bb]

## Arithmetic

| Operator | Result | Provenance |
|---|---|---|
| `x,y,+` `-` `*` `/` | IEEE 754 sum, difference, product, quotient. `x/0` is `±inf` by the signs of `x` and the zero; `0/0` is unknown. | [doc] [bb] |
| `x,y,%` | Floating-point remainder with the sign of `x` (C `fmod`): `5.5,2,%` = 1.5, `-5,3,%` = -2, `5,-3,%` = 2; `x,0,%`, `inf,y,%` are unknown; `1,inf,%` = 1. | [bb] |
| `x,y,ADDNAN` | `x+y`; if exactly one is unknown, the other; if both, unknown. `inf,-inf,ADDNAN` is unknown. | [doc] [bb] |
| `x,y,POW` | C `pow`: `0,0` = 1, `NaN,0` = 1, `1,NaN` = 1, `-8,3` = -512, negative base with non-integer exponent unknown, `0,-1` = inf, overflow to inf. | [bb] |
| `SIN COS LOG EXP SQRT ATAN` | C library functions on one value; `LOG` is natural log, `0,LOG` = -inf, negative inputs to `LOG` and `SQRT` are unknown, `inf,SIN` is unknown. | [doc] [bb] |
| `y,x,ATAN2` | `atan2(y, x)` with `x` popped first; signed zero selects ±pi. | [doc] [bb] |
| `FLOOR CEIL` | Round down / up; infinities and unknown pass through. | [doc] [bb] |
| `ROUND` | Round half away from zero (`0.5` -> 1, `2.5` -> 3, `-0.5` -> -1). | [bb] |
| `DEG2RAD` | Multiply by the constant `0.0174532952` (not pi/180): `1,DEG2RAD` prints `1.7453295200e-02`, `180,DEG2RAD` prints `3.1415931360e+00`. | [bb] |
| `RAD2DEG` | Multiply by the constant `57.29577951`: `1,RAD2DEG` prints `5.7295779510e+01`. | [bb] |
| `ABS` | Absolute value; clears the sign of zero. | [doc] [bb] |

## Comparison and selection

- `x,y,LT` (and `LE GT GE EQ NE`): if `x` is unknown the result is unknown;
  otherwise if `y` is unknown the result is unknown; otherwise 1 or 0 by
  ordinary IEEE comparison. Infinities compare as numbers: `inf,0,GT` = 1,
  `inf,inf,EQ` = 1, `0,-0,EQ` = 1. [bb] This contradicts the manual, which
  says comparing an infinite value returns unknown. [doc]
- `UN`: 1 if unknown, else 0. `ISINF`: 1 for `inf` or `-inf`, else 0. [doc]
- `c,t,f,IF`: pushes `f` when `c` is 0 (either sign) or unknown, otherwise
  `t`; `inf` and tiny values are true. [doc] [bb]
- `x,y,MIN` / `MAX`: unknown if either is unknown; otherwise the smaller /
  larger. When `x` and `y` compare equal (`0` and `-0`), the result is `x`.
  [doc] [bb]
- `MINNAN` / `MAXNAN`: as `MIN` / `MAX` but an unknown operand is ignored;
  both unknown gives unknown. Equal operands give `x`. [doc] [bb]
- `x,lo,hi,LIMIT`: unknown if any of the three is unknown; unknown if
  `x < lo` or `x > hi`; otherwise `x`. Infinite bounds and an infinite `x`
  inside them are allowed (`d,NEGINF,INF,LIMIT` passes `inf` through).
  Inverted bounds always give unknown. [bb] The manual says an infinite
  operand always gives unknown; it does not. [doc]

## Special values and row context

- `UNKN`, `INF`, `NEGINF` push unknown, `inf`, `-inf`. [doc]
- `COUNT` pushes the one-based row number. [doc]
- `PREV` pushes this CDEF's value in the previous row, unknown in the first
  row. [doc] [bb]
- `PREV(name)` pushes the named variable's previous sample (see
  [Variable cursors](#variable-cursors)), unknown in the first row. A CDEF
  name refers to that CDEF's previous output row. [doc] [bb]
- `STEPWIDTH` pushes the CDEF's step in seconds. A CDEF over variables of
  different steps runs at the smallest step. [doc] [bb]
- `TIME` pushes the row's timestamp. [doc]
- `NOW` pushes the current wall-clock time in whole seconds; within one row
  two `NOW` tokens are equal. Vectors only test relations (`NOW,TIME,GT`).
  [doc] [bb]

### LTIME

`LTIME` pushes `TIME + offset`, where the offset is computed from the local
and UTC broken-down times of the row timestamp as follows: [rsi]

1. Start with `(local seconds - UTC seconds) + 60 * (local minutes - UTC
   minutes) + 3600 * (local hours - UTC hours)`.
2. If the local day of year is greater than the UTC day of year, or the
   local year is greater than the UTC year, add 86400.
3. Otherwise, if the local day of year is smaller or the local year is
   smaller, subtract 86400.

Step 2 is checked first, so when the local date is 31 December and UTC is
already 1 January (any zone west of UTC around New Year), the offset comes
out 48 hours too large: in `America/New_York` at 2021-01-01 00:00 UTC,
`LTIME,TIME,-` is `154800` rather than `-18000`. Vectors `ltime-02`,
`ltime-07`, `ltime-08`, `ltime-10` and `ltime-11` pin this. Elsewhere the
offset is the zone's UTC offset at that instant, including half- and
quarter-hour zones and DST transitions (`ltime-03` to `ltime-06`,
`ltime-13`, `ltime-14`). An unknown `TZ` name behaves as UTC. [bb]

### NEWDAY, NEWWEEK, NEWMONTH, NEWYEAR

- Each pushes 1 when the local calendar day / month / year of the row time
  differs from that of `row time - STEPWIDTH`, else 0. The local zone comes
  from `TZ`. With a 6-hour step, the first row of a local day may be at a
  non-midnight hour (for `Europe/Berlin` with rows at 00:00, 06:00, ...
  UTC, the 01:00 local row). [doc] [bb]
- `NEWWEEK` pushes 1 when the row's local weekday is the first day of the
  week and differs from the weekday of `row time - STEPWIDTH`. [bb]
- The first day of the week is platform dependent: on macOS it is always
  Sunday, independent of locale (`newweek-05` uses `LC_ALL=en_GB.UTF-8`
  and still starts on Sunday). On glibc it is derived from the locale's
  week definition (`LC_TIME`); the C locale is expected to start on Sunday
  but this is established by the live oracle comparison in CI, not by the
  frozen vectors. [rsi] [bb]
- The manual says the periods follow `LC_TIME`; on macOS they do not. [doc]

## Variable cursors

Each variable reference reads from that variable's own series at its own
step. [bb]

- For a variable whose step equals the CDEF step, the value pushed in row
  `t` is the variable's sample for row `t`.
- For a coarser variable, the pushed value is held for several rows. The
  variable keeps a position in its series; after any row whose timestamp
  is an exact multiple of the variable's step (in absolute epoch seconds),
  the position moves to the next sample. With a 30 s variable in a 10 s
  CDEF the value changes on the row after each multiple of 30
  (`mixedstep-01`).
- The position at the first rows depends on where the variable's fetched
  series begins, which can precede the CDEF start; RRDtool reads outside
  its data in that region. The `mixedstep` vectors mask the first six rows
  with `COUNT,6,GT,...,UNKN,IF`. The replacement must not crash there; the
  values are not a contract.
- When a DEF is given an earlier `:start=`, the CDEF is evaluated from that
  earlier time and `xport` prints only the rows inside the export range.
  Row indexes used by `COUNT`, `TREND` and the PREDICT family count from
  the start of evaluation, so window operators already have history in the
  first printed row (`trend-12`, `predict-17`).

## Stack operators

- `DUP`, `POP`, `EXC`: duplicate, remove, swap the top. [doc]
- `DEPTH` pushes the depth before the push (`a,b,DEPTH` -> `a,b,2`). [doc]
- `n,COPY` pushes copies of the top `n` values in order
  (`1,2,3,2,COPY` -> `1,2,3,2,3`). `n` = 0 copies nothing. More than the
  stack holds is `RPN stack underflow`. A negative `n` crashes the
  reference (`copy-06`, tagged `undefined`); the replacement may reject it.
  [doc] [bb]
- `n,INDEX` replaces `n` with a copy of the `n`th value below it, counting
  from 1 (`1,2,3,4,3,INDEX` -> `1,2,3,4,2`). `0,INDEX` leaves the 0 in
  place. Too large an `n` is `RPN stack underflow`. A negative `n` reads
  above the stack top and is not a contract. [doc] [bb]

### ROLL

`n,m,ROLL` does not match the manual's example (`a,b,c,d,3,1,ROLL =>
a,d,b,c`). Its observable behavior is: [rsi] [bb]

1. Pop `m`, then `n`, both truncated to integers. If the remaining depth is
   less than `n`, fail with `RPN stack underflow`.
2. If `n` is negative, fail with `RPN out of memory (allocating <n>
   objects)`. If `n` is 0 or 1 the stack is unchanged.
3. Form a window `W[0..n-1]` from the current top value followed by the
   `n-1` stack slots above it. Those slots hold the values last written
   there: the popped `n` and `m` are the first two of them, as written
   (`2.9` stays `2.9` in the window although it counts as 2).
4. For `j` from 0 to `n-1`, the value at depth position `n-1-j` below and
   including the top (that is, the `j`th of the top `n` slots, counting
   from the lowest) becomes `W[(m - 1 - j) mod n]`, with a non-negative
   modulus.

With `1,2,3,4,5,6,3,1,ROLL` the window is `6, 3, 1` and the stack becomes
`1,2,3,6,1,3` (`roll-09`). Vectors use only `n <= 3`, where every window
slot holds a known value. For `n > 3` the window reaches slots whose
contents depend on earlier evaluation history, and RRDtool shows
uninitialised data there; the replacement must not crash but the values are
not specified. A very large `m` wraps in 32-bit arithmetic (see
`rpn_differential.rs`).

## Set operators

All take a count `n` from the top of the stack, then operate on the next
`n` values. A count of 0 consumes nothing. More values than the stack holds
is `RPN stack underflow`. [doc] [bb]

- `n,SORT` sorts the `n` values in place, smallest deepest, using the
  [ordering](#ordering-used-by-sort-median-percent-and-predictperc) below.
  `n,REV` reverses them. A negative `REV` count is treated as 0. A
  negative `SORT` count leaves the stack unchanged on macOS but crashes the
  glibc builds (`sort-06`, tagged `undefined`); the replacement may treat
  it as 0 or reject it. [doc] [bb]
- `n,AVG`: mean of the non-unknown values; unknown when none. A negative
  count pops nothing and pushes unknown. The sum starts at +0, so the mean
  of negative zeros is +0 (pinned in `rpn_differential.rs`). [doc] [bb]
- `n,SMIN` / `n,SMAX`: minimum / maximum, ignoring unknown values; unknown
  when all are unknown or `n` is 0. [doc] [bb]
- `n,MEDIAN`: median of the non-unknown values (mean of the two middle
  values when their number is even); unknown when none. [doc] [bb] With
  infinities present the sorted order follows the platform sort; before
  sorting, each unknown value found while scanning from the deepest value
  upward is replaced by the value currently in the highest not-yet-scanned
  position, and the remaining values are sorted. [rsi] A negative count
  crashes the reference; the replacement may reject it with
  `RPN stack underflow`.
- `n,STDEV`: sample standard deviation (divisor `count - 1`) of the
  non-unknown values; unknown with fewer than two values; any infinity
  gives unknown. The values are accumulated in pop order (top first) with a
  running mean: for each value `v`, `k += 1`, `delta = v - mean`,
  `mean += delta / k`, `m2 += delta * (v - mean)`; the result is
  `sqrt(m2 / (k - 1))`. On aarch64 the `m2` update is a fused multiply-add.
  [rsi]
- `p,n,PERCENT`: pop `n`, then `p`. Unless `0 <= p <= 100` (unknown fails
  too), fail with `percentile argument must be between 0 and 100`. Then
  sort the `n` values and replace them with the value of rank
  `r = round_half_away(p * n / 100)` counting from 1 at the smallest. When
  `r` is 0 the result is a copy of the value immediately below the `n`
  values, which itself stays on the stack (`7,1,2,3,0,3,PERCENT` ->
  `7,7`). With nothing below, the result is not specified. [bb]

### Ordering used by SORT, MEDIAN, PERCENT and PREDICTPERC

The four operators sort with the platform C library `qsort` and this
comparison of `x` against `y`: [rsi] [bb]

1. Both unknown: equal. `x` unknown: `x` is smaller. `y` unknown: `x` is
   larger.
2. Otherwise, if `x` is infinite, return the platform's infinity
   classification of `x`; else if `y` is infinite, return that of `y`.
   macOS classifies both infinities as +1, so any pair involving an
   infinity reports "larger", whichever operand it is. glibc builds
   classify `-inf` as -1 and `inf` as +1.
3. Otherwise compare numerically (equal values are equal).

Unknown values therefore always sort first. Infinities make the comparison
inconsistent, so the final order depends on the sort algorithm: two-value
results on macOS are pinned by `sort-15` to `sort-28`, and every vector
whose result depends on this is tagged `host-sort`. The glibc results for
the same two-value vectors (`expect_platform.linux-aarch64` of `sort-18`,
`sort-19`, `sort-23`) confirm the signed classification in step 2: for
example `NEGINF,-1,2,SORT` leaves `-inf` deepest on glibc and `-1` deepest
on macOS. The VDEF `PERCENT` sort is also platform dependent once
infinities are present (`vdef-22`: `-inf` on macOS, `inf` on Linux).

## Window operators

### TREND and TRENDNAN

`x,w,TREND` and `x,w,TRENDNAN`: [doc] [bb]

- The token immediately before `w` must be a variable reference;
  otherwise fail with `malformed trend arguments` (`a,1,+,30,TREND`,
  `30,a,TREND`). The window operand `w` is itself popped from the stack
  and must be present (`a,TREND` is `RPN stack underflow`).
- Let `S` be the variable's step and `N = ceil(w / S)` (at least 1;
  `w <= 0` gives 1). An unknown `w` is undefined: the aarch64 builds
  behave as `N = 1` and the x86_64 build crashes (`trend-10`,
  `trendnan-10`, tagged `undefined`); the replacement may reject it. In rows whose zero-based index is less
  than `N - 1` the result is unknown. Otherwise the result is the mean of
  the `N` most recent samples of the variable ending at the current one.
- `TREND` includes unknown samples, so one unknown makes the result
  unknown. `TRENDNAN` skips them and gives unknown only when all are
  unknown.
- The variable value pushed before `w` is replaced by the result.

### PREDICT, PREDICTSIGMA, PREDICTPERC

Operands, from deepest to top: [doc]

```
shift_1 ... shift_k, k, window, x, PREDICT
shift_1 ... shift_k, k, window, x, PREDICTSIGMA
shift_1 ... shift_k, k, window, percentile, x, PREDICTPERC
multiplier, -k, window, x, PREDICT          (and the other two)
```

Rules: [bb] unless marked

- `x` must be a variable reference directly before the operator. If it is
  not, the result depends on a step value RRDtool reads from the wrong
  token; those vectors are tagged `undefined` and the replacement may
  reject the expression. [rsi]
- With a positive `k`, the `k` shifts are popped; with a negative `k` the
  shifts are `0, m, 2m, ..., (|k|-1)m` for the multiplier `m` (the first
  shift is 0, and `|k| m` is not used). `k` = 0 consumes no shift and
  leaves the shift operand on the stack, so `60,0,30,a,PREDICT` ends with
  `RPN final stack size != 1`. Too few values is `RPN stack underflow`.
- Shifts are truncated to integers. A negative shift fails with
  `negative shift step not allowed: <shift>`, the shift printed as an
  integer.
- `PREDICTPERC`: a percentile with magnitude above 100 fails with
  `unsupported percentile: <p>` where `<p>` uses `%f` (`101.000000`). An
  unknown percentile yields unknown in every row.
- Let `S` be `x`'s step, `L = ceil(window / S)` and, per shift,
  `D = ceil(shift / S)`. The sample set for output row `t` (zero-based)
  contains, for each shift and each `i` in `0..=L`, the sample at offset
  `o = D + i`, provided `0 <= o < t`. Offset `o` names the variable's
  sample `o - 1` positions before the current one, so `o = 0` is the next
  row's sample: `0,1,0,a,PREDICT` in row `t` equals `a` in row `t + 1`, and
  row 0 is always unknown. A negative window gives an empty set. Unknown
  samples are skipped.
- `PREDICT`: mean of the set; unknown when empty.
- `PREDICTSIGMA`: with `n` samples, sum `s` and sum of squares `q`,
  `sqrt((n*q - s*s) / (n*(n-1)))`; unknown when `n < 2` or the radicand is
  negative. On aarch64 the sum of squares is accumulated with fused
  multiply-add. [rsi]
- `PREDICTPERC`: sort the set with the [ordering](#ordering-used-by-sort-median-percent-and-predictperc).
  With `f = (p / 100) * (n - 1)`: for a negative `p`, take the element at
  index `round_half_away(|f|)`; for a non-negative `p`, take the element at
  `floor(f)` and, when `f` has a fractional part, add
  `frac(f) * (next - this)`. Unknown when the set is empty. [rsi]

## VDEF

A VDEF is `VDEF:name=variable,FUNCTION` or
`VDEF:name=variable,parameter,FUNCTION`. It does not run the CDEF
evaluator. [doc]

- Functions: `MAXIMUM`, `MINIMUM` (value and time of the first
  occurrence), `AVERAGE`, `STDEV`, `LAST`, `FIRST` (value and time),
  `TOTAL`, `PERCENT`, `PERCENTNAN` (parameter 0 to 100), `LSLSLOPE`,
  `LSLINT`, `LSLCORREL`. Functions without a time component print `-` for
  `PRINT:v:%s:strftime`. [doc] [bb]
- Errors: an unknown name, `Unknown function 'NAME' in VDEF 'v'` (an RPN
  operator such as `PREV` or `+` is unknown here; `a,1,+` reports an empty
  name); trailing text, `Unknown function string 'MAXIMUM,1' in VDEF 'v'`;
  no comma, `Comma expected in VDEF definition a`; a parameter out of
  range, `Parameter '101.000000' out of range in VDEF 'v'`. [bb]
- A VDEF name used in a CDEF pushes the VDEF value in every row. [bb]
- `STDEV` and the least-squares functions are platform sensitive in the
  last bit (tag `fma`). [bb]

## Error messages

These texts are part of the contract; scripts match on them. Each is
printed as `ERROR: <text>` with exit status 1. [bb]

| Text | Trigger |
|---|---|
| `don't understand '<rest>'` | unrecognised token |
| `garbage in RPN: '<rest>'` | text after `PREV(name)` that is not a separator |
| `variable '<name>' not found` | `PREV(name)` with an unknown name |
| `can not parse an empty rpn expression` | empty CDEF expression |
| `rpn expressions without DEF or CDEF variables are not supported` | no variable reference |
| `RPN stack underflow` | too few operands |
| `RPN final stack size != 1` | depth other than one at the end of a row |
| `RPN out of memory (allocating <n> objects)` | negative `ROLL` count |
| `malformed trend arguments` | TREND window not preceded by a variable |
| `percentile argument must be between 0 and 100` | `PERCENT` operand |
| `unsupported percentile: <p>` | `PREDICTPERC` operand |
| `negative shift step not allowed: <shift>` | PREDICT family shift |
| VDEF messages | see [VDEF](#vdef) |

## Floating point and platform notes

- Results of single IEEE operations (`+ - * /`, comparisons, `FLOOR`,
  `CEIL`, `ROUND`, `ABS`, `SQRT`) are exact and identical on every
  platform. Transcendental functions follow the platform C library. The
  frozen vectors were captured on macOS arm64. The Linux aarch64 oracle
  agrees on every vector outside the sort ordering cases. In the x86_64 CI
  job the frozen table failed only for the vectors now listed under
  `expect_platform.linux-x86_64` and the two TREND cases now tagged
  `undefined`; `host-sort` vectors were not frozen-compared there. [bb]
- Unknown results print as `NaN` in `xport` on every
  platform, but VDEF `PRINT` with `%le` shows the sign of the platform's
  unknown value: `nan` on aarch64 and `-nan` on x86_64 (`vdef-21`). [bb]
- `0.49999999999999994,ROUND` gives 0 on aarch64 and 1 on x86_64
  (`rounding-06`). Whether the difference lies in reading the literal or
  in rounding is not settled; numeric literals with more than 15
  significant digits should be treated as platform dependent in the last
  bit. [bb]
- Vectors with a named `TZ` are compared against the frozen table only when
  the host has that zone in `/usr/share/zoneinfo`.
- Results that accumulate products (`STDEV`, `PREDICTSIGMA`, VDEF `STDEV`
  and least squares) depend on fused multiply-add: the pinned aarch64
  builds fuse and x86_64 builds do not; see
  [platform notes](../compatibility.md#platform-notes). Vectors tagged
  `fma` were captured on aarch64 and compare within 1e-9 relative
  elsewhere. [rsi]
- Ordering with infinities and the first day of the week depend on the C
  library (tags `host-sort`, `first-weekday`).

## Undefined reference behavior

The reference crashes or reads memory it does not own in these cases. The
replacement need not reproduce them and must not crash; returning an error
such as `RPN stack underflow` is acceptable.

- `COPY` with a negative count (crash).
- `SORT` with a negative count (crash on glibc; no effect on macOS).
- `TREND` and `TRENDNAN` with an unknown window (crash on x86_64).
- `MEDIAN` with a negative count (crash), `INDEX` with a negative index.
- `ROLL` windows that reach stack slots never written in the current
  evaluation.
- `PERCENT` with rank 0 and no value below the counted values.
- The first rows of mixed-resolution CDEFs (see
  [Variable cursors](#variable-cursors)).
- PREDICT family operators whose `x` operand is not a variable reference.

## Rules by provenance

- From reference-source inspection: the `LTIME` offset rule; `NEWWEEK`'s
  first-weekday source per platform; `ROLL`'s window construction; the sort
  comparison including platform infinity classification; `MEDIAN`'s
  unknown-compaction order; the `STDEV` accumulation order; the
  `PREDICTSIGMA` formula and fused accumulation; the `PREDICTPERC` index and
  interpolation formula; PREDICT with a non-variable operand being
  undefined; fused multiply-add sensitivity. Each is restated here as
  behavior, and all but the last two are confirmed by vectors.
- From the manual: the operator set and the documented meaning of each
  operator, with the two contradictions noted above (comparisons and
  `LIMIT` with infinities, NEWWEEK and `LC_TIME` on macOS).
- Everything else is black-box.
