# `COMPUTE` data sources

This specification describes what RRDtool 1.11.0 does with `COMPUTE` data
sources: the definition grammar, how the expression is stored in the file,
how `update` fills a `COMPUTE` source, and how the reading commands and
`restore` show it. It is written so that `COMPUTE` can be implemented from
it, from [`rpn.md`](rpn.md) and from the vectors without reading RRDtool's
source.

Provenance tags and classes are as in [`create.md`](create.md):
**documentation** (`rrdcreate(1)`), **black-box** (observed with
`/opt/homebrew/bin/rrdtool` 1.11.0 on macOS arm64 and pinned by the
vectors, with Linux overrides recorded in the pinned build), and
**reference-source-inspection** (none in this document); **must**,
**should** and **defect**.

## Terms

- A *computed source* is a data source of type `COMPUTE`; the others are
  *input sources*.
- *L*, *S*, PDP and CDP are as in `create.md`. File layout terms are those
  of `create.md` section 10.
- An *expression* is the text after `DS:<name>:COMPUTE:`. A *token* is a
  comma-separated part of it. A *node* is one stored element of the
  compiled expression (CP-5).

## 1. Definition grammar

**CP-1** (documentation, black-box; must). The definition is
`DS:<name>:COMPUTE:<expression>`. Names follow CR-17. The expression uses
the RPN grammar of `rpn.md` with these differences, each pinned by the
`create` vectors:

1. A variable is the name of a data source defined *earlier* in the same
   command, input or computed. A later source, the source itself, an
   unknown name and a name in another case fail with
   `failed to parse computed data source`.
2. `TIME LTIME STEPWIDTH PREV NEW* COUNT TREND TRENDNAN PREDICT
   PREDICTSIGMA PREDICTPERC` fail with
   `operators TIME LTIME STEPWIDTH PREV NEW* COUNT TREND TRENDNAN PREDICT PREDICTSIGMA PREDICTPERC are not supported with DS COMPUTE`.
   (`NEWDAY`, `NEWWEEK`, `NEWMONTH` and `NEWYEAR` all give this text.)
3. `PREV(<name>)` with an earlier source is accepted at create time (CP-6,
   CP-16). With an unknown name it fails with
   `failed to parse computed data source`.
4. Every other operator of `rpn.md` is accepted, including `NOW`, the
   stack operators, the set operators and `UNKN`, `INF`, `NEGINF`.
5. Numbers are read as by `rpn.md` and must then be integers in
   [−32768, 32767]: `-0`, `+02`, `1.0`, `1e2` and `-32768` are accepted;
   `32768`, `-32769`, `1.5` and `1e10` fail with
   `constants must be integers in the interval (-32768, 32767)` (the
   message's bounds are as printed, not the accepted range). `0x10` fails
   with `failed to parse computed data source`.
6. An expression whose last token is a number fails with
   `failed to parse computed data source` (`1`, `a,1`, `a,1,b,1`), even
   though the same numbers elsewhere are accepted (`7,a,+`, `a,1,1,+,+`).
7. An empty token (`a,,+`), a space inside a token (`a, b,+`) and text after
   a further `:` (`a,b,+:x`) fail with `failed to parse computed data source`.
   `DS:c:COMPUTE` and `DS:c:COMPUTE:` fail with `invalid DS format` (CR-21).

**CP-2** (black-box; must). Create does not check the stack: `a,b`, `+`,
`a,+` and `a,POP` are accepted. They fail at update time (CP-16).

**CP-3** (black-box; must). At most 19 nodes are allowed. With more, create
fails with `Maximum 19 RPN nodes permitted. Got <n> RPN nodes at present.`,
where *n* is the node count. `PREV(<name>)` is one node.

**CP-4** (black-box; must). Checks within one definition run in four
passes over the whole expression, the first failing pass deciding wherever
in the expression its fault is:

1. parsing: syntax, names and the last-token rule (CP-1 items 1, 3, 6, 7,
   and `0x10`), giving `failed to parse computed data source`;
2. the operator restriction (CP-1 item 2);
3. the node limit (CP-3);
4. the constant range (CP-1 item 5).

So `TIME,z,+` and `40000,z,+` report the parse error, `40000,TIME,+` the
operator error, `TIME` with 20 more nodes the operator error and `40000`
with 20 more nodes the node limit. Across definitions, the first failing
word decides, as in CR-12. Errors are one `ERROR: <message>` line and exit
status 1; no file is written.

## 2. Storage

**CP-5** (black-box; must). A computed source's 120-byte definition holds
the name at 0 and `COMPUTE` at 20, as in CR-39. The 80 bytes at 40 hold the
compiled expression as up to 20 nodes of 4 bytes each, followed by zeros:

- byte 0: the operator code (CP-6);
- byte 1: zero;
- bytes 2–3: a little-endian signed 16-bit value: the index of the
  referenced source for a variable or `PREV(<name>)`, the integer for a
  constant, otherwise zero.

The node list ends with one node of code 32 (*end*). A 19-node expression
therefore fills the 80 bytes exactly. The heartbeat, minimum and maximum
fields that other sources keep at 40 to 63 do not exist for a computed
source.

**CP-6** (black-box; must). Operator codes, as decimal byte values:

| Code | Token | Code | Token | Code | Token | Code | Token |
|---:|---|---:|---|---:|---|---:|---|
| 0 | constant | 1 | variable | 2 | `INF` | 4 | `NEGINF` |
| 5 | `UNKN` | 6 | `NOW` | 8 | `+` | 9 | `%` |
| 10 | `-` | 11 | `*` | 12 | `/` | 13 | `SIN` |
| 14 | `DUP` | 15 | `EXC` | 16 | `POP` | 17 | `COS` |
| 18 | `LOG` | 19 | `EXP` | 20 | `LT` | 21 | `LE` |
| 22 | `GT` | 23 | `GE` | 24 | `EQ` | 25 | `IF` |
| 26 | `MIN` | 27 | `MAX` | 28 | `LIMIT` | 29 | `FLOOR` |
| 30 | `CEIL` | 31 | `UN` | 32 | end | 34 | `NE` |
| 35 | `ISINF` | 36 | `PREV(<name>)` | 38 | `ATAN` | 39 | `SQRT` |
| 40 | `SORT` | 41 | `REV` | 44 | `ATAN2` | 45 | `RAD2DEG` |
| 46 | `DEG2RAD` | 49 | `AVG` | 50 | `ABS` | 51 | `ADDNAN` |
| 52 | `MINNAN` | 53 | `MAXNAN` | 54 | `MEDIAN` | 56 | `DEPTH` |
| 57 | `COPY` | 58 | `ROLL` | 59 | `INDEX` | 65 | `SMIN` |
| 66 | `SMAX` | 67 | `STDEV` | 68 | `PERCENT` | 69 | `POW` |
| 70 | `ROUND` | | | | | | |

The codes are part of the file format: a file written by one
implementation must be read by the other.

**CP-7** (black-box; must). The PDP state of a computed source (112 bytes,
CR-42) starts as for any source: last value `U`, PDP value unknown, unknown
seconds *L mod S*. Its CDP state and rows are those of any source.

## 3. Showing the expression

**CP-8** (black-box; must). `info`, `dump` and the `cdef` of a restored
file show the expression rebuilt from the nodes, not the text given to
create: tokens joined by `,`, a variable by its source's name, a constant
as a decimal integer without sign for non-negative values (`+02` → `2`,
`1e2` → `100`, `-0` → `0`), an operator by its name. `PREV(<name>)` is
shown as an empty token (`a,PREV(b),+` → `a,,+`).

**CP-9** (black-box; must). `info` prints, for a computed source,
`ds[<name>].index`, `.type = "COMPUTE"`, `.cdef = "<expression>"`,
`.last_ds`, `.value` and `.unknown_sec`, in that order, without
`minimal_heartbeat`, `min` and `max`. `dump` prints `<name>`, `<type>`,
`<cdef> <expression> </cdef>`, a blank line, then `<last_ds>`, `<value>`
and `<unknown_sec>` as for other sources. `lastupdate` prints `U` for a
computed source. `fetch` and `xport` treat a computed source like any
other.

## 4. Update

**CP-10** (documentation, black-box; must). `update` takes values only for
input sources, in definition order. Too few values fail with
`<file>: expected <n> data source readings (got <m>) from <time>` and too
many with `<file>: found extra data on update argument: <value>`, where *n*
counts input sources only.

**CP-11** (black-box; must). A template may name a computed source. The
value given for it is read and ignored; input sources the template leaves
out are unknown as usual.

**CP-12** (documentation, black-box; must). The input sources are updated
as without computed sources. Each time a step completes, every computed
source's PDP for that step is the expression evaluated with each variable
bound to that source's PDP for the same step, in definition order, so a
computed source sees the PDPs of earlier computed sources. When one update
completes several steps, the expression is evaluated for each of them.

**CP-13** (black-box; must). Evaluation follows `rpn.md` for values:
unknown inputs propagate as there, division by zero gives `inf`, `-inf` or
`nan`, `SQRT` of a negative is `nan`. A `nan` result is stored as the
target's unknown.

**CP-14** (black-box; must). The archives consolidate a computed source's
PDPs like any other source's, with unknown PDPs counted against `xff`.

**CP-15** (black-box; must). Between step boundaries, a computed source's
PDP state is kept as for a source updated with `U` each time: its last value
stays `U`, its PDP value stays unknown, and its unknown seconds grow by the
elapsed time and are reset at each completed step.

**CP-16** (black-box; must). If evaluation fails when a step completes,
the update stops with one of these messages and exit status 1:

- `<file>: RPN final stack size != 1` when the expression leaves more or
  less than one value;
- `<file>: RPN stack underflow` when an operator lacks arguments;
- `<file>: VDEF made it into rpn_calc... aborting` for `PREV(<name>)`.

Updates before the failing one in the same command are kept. For the
failing update itself, the input sources' last values and PDP state are
written as if it had succeeded, but the last update time, the computed
sources' PDP state, the CDP state and the rows are not. Later updates in
the same command are not applied. The vectors pin the resulting bytes.
An update that completes no step does not evaluate the expression and
succeeds.

**CP-17** (black-box; should). `NOW` evaluates to the wall-clock time of
the update, so its results are not pinned by vectors.

## 5. Other commands

**CP-18** (black-box; must). `resize` keeps computed definitions unchanged.

**CP-19** (black-box; must). `restore` reads `<cdef>` and compiles it under
CP-1 to CP-3, with the computed source's own name and later sources
unresolvable as at create. A `<cdef>` that does not compile, a missing or
empty `<cdef>`, and the other errors are pinned by the restore vectors
(`walk-cdef-*`, `seed-compute`, `type-compute`). Elements such as
`<minimal_heartbeat>` given for a computed source and a `<cdef>` given for
an input source are handled as the vectors show. Restore otherwise follows
`restore.md`.

**CP-20** (black-box; must). `tune` is out of scope for this document;
`tune` cases with computed sources stay as in `tune-modify.md`.

## Platform notes

- The unknown written for a `nan` result is the target's NaN bit pattern
  (`create.md` CR-74); the vectors fold the two patterns.

## Tests

- `crates/rondi-cli/tests/data/create`: definition grammar, node limit,
  constant range and one case per operator (CP-1 to CP-6), recorded by
  `scripts/gen-create-resize-vectors.py`.
- `crates/rondi-cli/tests/data/compute`: update, `updatev`, `info`, `dump`,
  `lastupdate`, `last`, `first`, `fetch`, `xport` and `resize` on files with
  computed sources (CP-7 to CP-18), recorded by the same script and
  replayed by `crates/rondi-cli/tests/compute_spec_vectors.rs`.
- `crates/rondi-cli/tests/data/restore`: `<cdef>` handling (CP-19),
  recorded by `scripts/gen-restore-vectors.py`.
