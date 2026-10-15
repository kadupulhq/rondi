# `rrdtool create`

This specification describes what RRDtool 1.11.0 does when `rrdtool create`
sets up a new RRD file: the command line, the order of its checks, the
grammar of data source (DS) and round robin archive (RRA) definitions with
their error texts, the numbers they accept, the file it writes and its
initial state, templates, sources, the output file, resource limits and the
cache daemon. It is written so that `create` can be implemented from it and
from the vectors in `crates/rondi-cli/tests/data/create` without reading
RRDtool's source.

Every rule carries a provenance tag:

- **documentation**: stated in `rrdcreate(1)`, `librrd(3)` (for durations)
  or the command's usage text.
- **black-box**: observed by running `/opt/homebrew/bin/rrdtool` 1.11.0
  (macOS arm64) and the pinned 1.11.0 build on Linux aarch64 and x86_64
  (Debian containers) against chosen inputs, and pinned by the vectors where
  the rule names them.
- **reference-source-inspection**: none. No rule in this document needed the
  reference source.

Each rule is also classed as **must** (a replacement reproduces it exactly,
byte for byte where it concerns file content), **should** (expected for
compatibility but allowed to differ in the way the rule states, or not
pinned byte for byte), or **defect** (a reference bug or undefined behavior;
the rule says what a replacement does instead, or that the vectors pin the
reference behavior for now).

## Terms

- *S* is the step in seconds and *L* the last update time in whole seconds.
- An archive has a consolidation function (CF), *p* primary data points
  (PDPs) per row and *n* rows. Its *slot length* is *T = p·S*.
- *Classic* CFs are `AVERAGE`, `MIN`, `MAX` and `LAST`. *Holt-Winters* CFs
  are `HWPREDICT`, `MHWPREDICT`, `SEASONAL`, `DEVSEASONAL`, `DEVPREDICT` and
  `FAILURES`.
- *Unknown* is the platform's default quiet NaN: `0x7ff8000000000000` on
  arm64 and aarch64, `0xfff8000000000000` on x86_64 (CR-74).
- *Masked form* of a created file: every archive's rows rotated so the row
  after the current row comes first, and every current-row word set to 0.
  The vectors compare created files in masked form (CR-60).
- Error output is always `ERROR: <message>` and a newline on standard error,
  nothing on standard output, exit status 1 (CR-3). The rules below give
  only `<message>`.

## 1. Command shape

**CR-1** (documentation; must). The synopsis is
`rrdtool create filename [--start|-b start] [--step|-s step]
[--template|-t file] [--source|-r file] [--no-overwrite|-O]
[--daemon|-d address] [DS:...] [RRA:...]`.

**CR-2** (black-box; must). `rrdtool create` with no further argument
prints the create usage text on standard output and exits 0. The text
starts with the RRDtool banner, whose second line is `Compiled` followed by
the build's date and time (empty in the macOS build). That build time is
not part of the contract; the tests drop it before comparing (vector
`000-noargs`).

**CR-3** (black-box; must). On success nothing is written to standard output
or standard error (unless the file name is `-`, CR-63) and the exit status
is 0. On any failure the output is one `ERROR: <message>` line on standard
error and the exit status is 1.

**CR-4** (black-box; must). Options and positional words may be interleaved
in any order (`-b 1000000000 o.rrd DS:... RRA:...` and
`o.rrd DS:... RRA:... -b 1000000000` are equivalent). `--` ends option
processing; every later word is positional, so `o.rrd DS:... RRA:... -- -s 20`
fails with `can't parse argument '-s'`.

**CR-5** (black-box; must). Option syntax:

- Short options take their value attached or as the next word (`-b100`,
  `-b 100`) and may be clustered (`-Ob 1000000000`).
- Long options must be spelled in full; abbreviations are not accepted:
  `--sta` and `--st` fail with `invalid option -- 'sta'` /
  `invalid option -- 'st'`. A long option takes its value as `--start=V` or
  as the next word.
- An unknown option fails with `invalid option -- '<name>'` (the long name
  without dashes, or the single letter).
- A missing value fails with `option requires an argument -- '<letter>'`
  for a short option (`-s`, `-b`) and with
  `option requires argument -- '<name>'` for a long one (`--step`,
  `--template`, `--start`); note the different wording.
- `--no-overwrite=1` fails with `option takes no arguments -- 'no-overwrite'`.
- Repeating `-b` or `-s` keeps the last value; repeating `-O` is harmless.

**CR-6** (black-box; must). Options are processed left to right and each is
checked when it is read, so the first failing option decides the error:
`-s 0 -b end-1h` reports the step error and `-b end-1h -s 0` the start error.
The per-option checks are CR-8 to CR-10 for `-b`, CR-12 for `-s`, CR-55 for
`-r` and, for `-t`, only that it was not given before
(`template already set`).

**CR-7** (black-box; must). After the options, the first positional word is
the file name. With none, the command fails with
`need name of an rrd file to create` (`create --step 10`, `create -O`). The
remaining positional words are definitions (section 3).

## 2. Start and step

**CR-8** (documentation, black-box; must). `--start` is parsed by the
at-style time parser of `rrdfetch(1)` relative to the current time (that
parser is specified separately). A parse failure fails with
`start time: <parser message>`, for example `start time: unparsable time: garbage`,
`start time: did you really mean month 100?` and
`start time: unparsable trailing text: '....5'`.

**CR-9** (black-box; must). A start relative to `start` or `end`
(`end-1h`, `e-1h`, `start-1h`, `s+5min`) fails with
`specifying time relative to the 'start' or 'end' makes no sense here`.

**CR-10** (black-box; must). A start below 315360000 (1979-12-30 00:00 UTC)
fails with `the first entry to the RRD should be after 1980` as soon as the
option is read. 315360000 itself is accepted (`00:00 30.12.1979` with
`TZ=UTC`); `00:00 29.12.1979 +86399 seconds` is rejected.

**CR-11** (documentation, black-box; must). Without `--start` the last update
time is ten seconds before the current time (test
`create_defaults_start_and_step`), except with `--template` (CR-49) or
`--source` (CR-56). The microseconds of the last update are always 0.

**CR-12** (documentation, black-box; must). `--step` is a duration (CR-14)
with divisor 1. Errors read `step size: <duration message>`. Without
`--step` the step is 300 (or the template's, CR-49).

**CR-13** (black-box; must). The step is an unsigned 64-bit value. Values
beyond 2^64−1 saturate to 2^64−1, and a suffix multiplication wraps modulo
2^64 (`300000000000000000y` gives 2244748946633129984). There is no upper
bound and no relation to heartbeats is checked.

## 3. Durations

**CR-14** (documentation, black-box; must). A duration token is read as:

1. The first character must be a decimal digit; otherwise the token fails
   with `value must be (suffixed) positive number` (`-5`, `+5`, ` 5`, `m`,
   empty).
2. Decimal digits are read as an unsigned 64-bit count, saturating at
   2^64−1. Leading zeros are allowed (`05`).
3. If the next character is one of `s m h d w M y`, the count is multiplied
   (modulo 2^64) by 1, 60, 3600, 86400, 604800, 2678400 or 31622400 and is a
   *duration in seconds*. Everything after that suffix character is ignored
   (`5ss`, `5sx`, `5mm`, `5m5` and `5hs` are valid).
4. Otherwise, if any character other than the field terminator follows,
   the token fails with `value has trailing garbage` (`1x`, `1.5`, `0x10`,
   `5S`, `5H`, `5:3`, `5 `). The terminator is the end of the word for
   `--step` and `:` inside a definition.
5. Without a suffix the value is a plain count and is used unscaled.
6. A duration in seconds is divided by the field's divisor (1 for the step
   and heartbeat, *S* for RRA steps, *S·p* for RRA rows). A remainder fails
   with `value would truncate when scaled`.
7. A result of 0 fails with `value must be positive` (`0`, `0s`, `0m`).

## 4. Definitions: order and errors

**CR-15** (black-box; must). After the option checks the order is:

1. `--no-overwrite` (CR-62).
2. Opening the template, when given (CR-48).
3. Each definition word in argument order: a word starting with `DS:` is a
   data source (section 5), one starting with `RRA:` an archive (sections 7
   and 8), anything else fails with `can't parse argument '<word>'`
   (`foo`, the empty word, `ds:...`, `rra:...`, `DS`, `RRA`, a second file
   name). The first failing word decides the error; later words are not
   examined (`DS:bad RRA:FOO:...` reports the DS error, `RRA:FOO:... DS:bad`
   the RRA error).
4. If no archive was defined: `you must define at least one Round Robin Archive`.
5. If no data source was defined: `you must define at least one Data Source`.
6. Opening the sources (CR-56), building and writing the file.

**CR-16** (black-box; must). A data source name equal to an earlier one
(including template names, CR-50) fails with `Duplicate DS name: <name>`
when the later definition is read. The check uses the target name before
any `=` mapping: `DS:x=z:... DS:x=y:...` is a duplicate, `DS:x DS:y=x` and
`DS:x=z DS:y=z` are not.

## 5. Data source definitions

**CR-17** (documentation, black-box; must). A DS word is
`DS:<name>[=<source-name>[[<index>]]]:<type>:<arguments>`. The part up to
the type and its colon must match: name of 1 to 19 characters from
`A-Z a-z 0-9 _ -`; optionally `=` and a source name of 1 to 19 characters
from the same set, optionally followed by `[` decimal digits `]`; then `:`;
then the type, a run of upper-case letters `A-Z`; then `:` and at least one
more character. Anything else fails with `invalid DS format`: an empty or
20-character name, `.`, space or non-ASCII in a name, `x=`, `x=y[`,
`x=y[a]`, `x=y[-1]`, `x[1]`, `x=y=z`, a lower-case type (`gauge`), an empty
type, and `DS:x`, `DS:x:GAUGE`, `DS:x:GAUGE:`. The documented character set
omits `-`, but RRDtool accepts it (`DS:a-b`, `DS:-`).

**CR-18** (black-box; must). The mapping and index are only used by
`--source` (CR-57). Without sources they are accepted and ignored:
`DS:x=y[99]:...` succeeds.

**CR-19** (documentation, black-box; must). The type must be `GAUGE`,
`COUNTER`, `DERIVE`, `ABSOLUTE`, `DCOUNTER`, `DDERIVE` or `COMPUTE`; any
other upper-case word fails with `invalid DS type specified (<type>)`
(`BOGUS`, `GAUGEX`, `COMPUTEX`).

**CR-20** (documentation, black-box; must). For every type except
`COMPUTE`, the arguments are `heartbeat:min:max`. Text after `max` and a
further `:` is ignored (`DS:x:GAUGE:20:U:U:extra:more`). Let *A* be the
argument text (everything after the type and its colon). Errors that concern
*A* read `failed to parse data source <A>: <reason>`. In order:

1. The heartbeat is a duration (CR-14) with divisor 1, terminated by `:`.
   Its failures give `<reason>` = the duration message
   (`failed to parse data source 0:U:U: value must be positive`,
   `failed to parse data source 20x:U:U: value has trailing garbage`). The
   heartbeat saturates and wraps like the step (CR-13); there is no upper
   bound and no comparison with the step.
2. If no `:` follows the heartbeat: `<reason>` = `missing separator`
   (`DS:x:GAUGE:20`).
3. The remainder must consist of a minimum field of 1 to 18 characters
   other than `:`, a `:`, and a maximum field of 1 to 18 characters other
   than `:`. If not: `<reason>` = `failed to extract min:max`
   (`20:`, `20:U`, `20::U`, `20:U:`, a minimum of 19 or more characters).
   A maximum longer than 18 characters is cut to its first 18
   (`U:1234567890123456789` stores 123456789012345678 and
   `U:123456789012345678x` is valid).
4. The minimum is read: `U` is unknown; otherwise the number grammar of
   CR-23. A failure here reads `parsing min val - <number message>`,
   without the `failed to parse data source` prefix.
5. The maximum likewise, with `parsing max val - <number message>`.
6. If both are known and minimum ≥ maximum: `<reason>` =
   `min must be less than max in DS definition` (`5:5`, `5:1`, `-0:0`,
   `1e-400:-0`, `inf:-inf`). An unknown or NaN bound is never compared
   (`nan:5`, `1:nan`, `nan:-5` are valid).

**CR-21** (documentation, black-box; should). For `COMPUTE` the argument is
an RPN expression (`DS:y:COMPUTE:x,2,*`). RRDtool compiles it with the RPN
grammar of `rpn.md` restricted to names of data sources defined earlier in
the same command and without `TIME LTIME STEPWIDTH PREV NEW* COUNT TREND
TRENDNAN PREDICT PREDICTSIGMA PREDICTPERC`. Observed errors:
`failed to parse computed data source` (unknown or later name, a
self-reference, text after another `:`) and
`operators TIME LTIME STEPWIDTH PREV NEW* COUNT TREND TRENDNAN PREDICT PREDICTSIGMA PREDICTPERC are not supported with DS COMPUTE`.
`DS:y:COMPUTE` and `DS:y:COMPUTE:` fail with `invalid DS format` (CR-17).
The compiled expression is stored in the definition's parameter area; its
bytes are pinned by the vectors but not specified here. A replacement that
does not implement `COMPUTE` must refuse every valid `COMPUTE` definition
with an `ERROR:` line and exit 1 without writing (CR-80).

**CR-22** (black-box; must). DS names are stored exactly as given, in
argument order. The mapped source name and index are not stored.

## 6. Numbers in min, max and xff

**CR-23** (black-box; must). A number field (DS minimum and maximum, RRA
`xff`, Holt-Winters parameters) is read as follows. This is the same
decimal conversion that `restore.md` specifies as RS-38 (including its
fused multiply-add on arm64, RS-39); it was checked against 100 random
17-digit and exponent literals in `xff` and `min`.

1. If the text starts with `nan` or `-nan` in any letter case, the value is
   NaN, whatever follows (`nanx`, `NaN`, `-NaNq`).
2. Otherwise if it starts with `inf` or `-inf` in any case, the value is
   positive or negative infinity, whatever follows (`inf5`, `INF`,
   `infinity`, `-Infinity`, `-infinityx`). `+inf` and `+nan` are not
   recognized here.
3. Otherwise leading white space is skipped and the RS-38 decimal procedure
   runs: optional sign, digits with an optional `.` part, an optional `e`
   or `E` exponent whose digits may be missing (`1e`, `1e+`, `5e` mean the
   mantissa alone), the exponent range check (written exponent minus the
   number of fraction digits must lie in −1021..1024) and binary scaling.
   An infinite result is accepted (`1e400`). `0e400` gives 0 and `0e1024`
   gives NaN, as the scaling procedure implies.
4. If no digit was read, or the exponent is out of range, the field fails
   with `Cannot convert '<text>' to float` (`u`, `UNKN`, `banana`, `+nan`,
   `+inf`, `e5`, `.`, `-`, `+`, `1e-1022`, `1e1025`).
5. If characters remain, the field fails with
   `Converted '<text>' to <value printed as %f>, but cannot convert '<rest>'`
   (`0x10` → `Converted '0x10' to 0.000000, but cannot convert 'x10'`,
   `1,5`, `5 `, `1x`).

The result is not always the correctly rounded value:
`0.123456789012345678` reads as 0.1234567890123457 and
`0.77921045355292324` as 0.7792104535529232 (vectors pin both).

**CR-24** (black-box; must). An `xff` field uses CR-23 with these
differences: there is no `U`; any CR-23 failure, and any value that is not
in [0, 1) (that is, below 0 or 1 or above, including infinities), fails with
`Invalid xff: must be between 0 and 1` (`1`, `1.0`, `-0.1`, `U`, `inf`,
`0.5x`, `0x0.8`, `abc`, and `0.99999999999999999`, which reads as 1). NaN
is accepted and stored (`nan`, `nanx`) (CR-77). `-0` is accepted and stored
as negative zero.

**CR-25** (black-box; must). Bounds are stored as the double values read:
`U` and NaN texts as the platform's unknown, infinities as infinities.

## 7. Classic archive definitions

**CR-26** (documentation, black-box; must). An RRA word is split at `:`
into fields, and empty fields are dropped before anything is counted
(`RRA::AVERAGE:0.5:1:5`, `RRA:AVERAGE::0.5:1:5`, `RRA:AVERAGE:0.5:1:5:` and
`RRA:AVERAGE:0.5:1:5::` are all valid). The first remaining field after
`RRA` is the CF field.

**CR-27** (black-box; must). The CF is the leading run of upper-case letters
`A-Z` of the CF field, at most 19 of them; the rest of the field is ignored.
It must name one of the ten CFs, or the word fails with
`Unrecognized consolidation function <run>` (`AVG`, `AVERAGEX`, `FOO`;
`Average` reports `A`; `average` and `1AVERAGE` report an empty run, that
is `Unrecognized consolidation function ` with a trailing space; a
26-letter word reports its first 19 letters). `AVERAGE1` is valid
`AVERAGE`.

**CR-28** (black-box; must). Field count: a classic CF needs 4 fields
(CF, xff, steps, rows). With fewer, the word fails with
`Expected at least 4 arguments for RRA but got <k>`, where *k* counts the
remaining fields including the CF (`RRA:` and `RRA::::` give 0,
`RRA:AVERAGE` 1, `RRA:LAST:0.5` 2, `RRA:AVERAGE:0.5:1` 3). A fifth field
fails with `Unexpected extra argument for consolidation function <CF>`.
The count is checked before the xff.

**CR-29** (black-box; must). The fields are checked in order xff (CR-24),
steps, rows:

- steps: a duration (CR-14) with divisor *S*. Errors read
  `Invalid step <field>: <duration message>`
  (`Invalid step 7s: value would truncate when scaled` with step 10,
  `Invalid step 0: value must be positive`).
- rows: a duration with divisor *S·p*. Errors read
  `Invalid row count <field>: <duration message>`
  (`1m:65s` → `Invalid row count 65s: value would truncate when scaled`).
  With step 10, `RRA:AVERAGE:0.5:1m:1h` has 6 PDPs per row and 60 rows.

**CR-30** (black-box; must). PDPs per row and rows are unsigned 64-bit
values that saturate like the step (CR-13): `99999999999999999999` steps
store 2^64−1. Large values are stored without further checks; a file too
large to build fails per CR-70.

**CR-31** (black-box; must). Archives are stored in argument order, after
any template archives. Duplicate archives are allowed.

## 8. Holt-Winters archives

These rules record what RRDtool does. Rondi refuses every Holt-Winters
archive (CR-80); the vectors pin RRDtool's results with expectation
`unsupported`.

**CR-32** (documentation, black-box; should). Argument shapes and minimum
field counts (CF included):

| CF | Fields | Minimum |
| --- | --- | --- |
| `HWPREDICT`, `MHWPREDICT` | rows, alpha, beta, seasonal period[, rra-num] | 5 |
| `SEASONAL`, `DEVSEASONAL` | seasonal period, gamma, rra-num[, smoothing-window=f] | 4 |
| `DEVPREDICT` | rows, rra-num | 3 |
| `FAILURES` | rows, threshold, window length, rra-num | 5 |

Too few fields fail with `Expected at least <m> arguments for RRA but got <k>`.

**CR-33** (black-box; should). Parameter checks and texts:
`Invalid alpha: must be between 0 and 1`, `Invalid beta: must be between 0 and 1`,
`Invalid gamma: must be between 0 and 1` (alpha must lie strictly between 0
and 1; beta 1 is accepted); rows use CR-29's
`Invalid row count <f>: <message>`;
`Invalid smoothing-window <value printed as %f>: must be between 0 and 1`;
an unknown `name=value` option fails with `Invalid option <text>`;
`Failure threshold is out of range 1, 28`,
`Window length is out of range 1, 28`,
`Window length is shorter than the failure threshold`.

**CR-34** (documentation, black-box; should). `HWPREDICT` or `MHWPREDICT`
without rra-num creates four more archives after it: `SEASONAL` and
`DEVSEASONAL` with the seasonal period as rows and gamma equal to alpha,
`DEVPREDICT` with the same rows, and `FAILURES` with the seasonal period as
rows, threshold 7, window length 9 and deviation multipliers 2.0. Seasonal
period and rows accept durations (`5m` with step 10 gives 30 rows).

**CR-35** (black-box; defect). The rra-num link is not validated: an index
of 0 or beyond the archive count is stored as given and the file is
written. A seasonal period of 0 creates archives with 0 rows (vector
`314`, recorded as `undefined` because such a file's current rows vary).
`HWPREDICT` with a sixth field fails with `Unknown error`. A replacement
that implements Holt-Winters archives should reject these cases.

**CR-36** (black-box; should). The CDP state of Holt-Winters archives is
not the classic one of CR-45; its bytes, and the archives' parameter bytes,
are pinned by the vectors but not specified here.

## 9. File versions

**CR-37** (black-box; must). The version string is `0003`, raised to `0004`
when any archive is `MHWPREDICT` or any archive has a `smoothing-window`
option, and to `0005` when any data source given on the command line is
`DCOUNTER` or `DDERIVE` (the highest applies: `DCOUNTER` with
`MHWPREDICT` gives `0005`). Data sources copied from a
template never raise it: a template holding `DCOUNTER` gives `0003`
(vector `-t v5.rrd`).

## 10. File layout and initial state

All integers are 64-bit little endian, all reals IEEE binary64 little
endian, on every platform the vectors cover (arm64, aarch64 and x86_64).
Unused bytes are zero.

**CR-38** (documentation, black-box; must). Header, 128 bytes: `RRD` and a
NUL at 0; the version string and a NUL at 4; zeros to 16; the float cookie
8.642135e130 (bytes `2f 25 c0 c7 43 2b 1f 5b`) at 16; the DS count at 24;
the archive count at 32; the step at 40; 80 zero bytes at 48.

**CR-39** (black-box; must). Then one 120-byte DS definition per data
source: the name NUL-padded to 20 bytes at 0; the type NUL-padded to 20
bytes at 20; the heartbeat at 40; minimum at 48; maximum at 56; zeros to
120. (`COMPUTE` definitions differ, CR-21.)

**CR-40** (black-box; must). Then one 120-byte archive definition per
archive: the CF name NUL-padded to 20 bytes at 0; 4 zero bytes; rows at 24;
PDPs per row at 32; xff at 40; zeros to 120. (Holt-Winters archives store
other parameters there, CR-36.)

**CR-41** (black-box; must). Then 16 bytes: the last update *L* and its
microseconds (0).

**CR-42** (black-box; must). Then 112 bytes of PDP state per data source:
`U` and NUL padding to 30 bytes; 2 zero bytes; the unknown seconds
*L mod S* at 32; the PDP value, unknown, at 40; zeros to 112.

**CR-43** (black-box; must). Then 80 bytes of CDP state for each archive
and, within it, each data source (archive-major order).

**CR-44** (black-box; must). Then the current-row word of each archive
(CR-46), then each archive's rows in turn, *n* rows of one value per data
source, all unknown. Nothing follows the last row.

**CR-45** (black-box; must). Classic CDP state: the value, unknown, at 0;
the unknown-PDP count at 8, equal to
*((L − (L mod S)) mod (S·p)) / S*; zeros to 80. For example with
*L* = 1000000123, *S* = 7: 4 for *p* = 13 and 0 for *p* = 5.

**CR-46** (black-box; should). Every archive's current row is chosen at
random, independently, in 0..*n*−1 (each row of a 3-row archive is seen
across 60 files; test `create_picks_initial_rows_at_random`). Because the
row contents are all unknown, the choice is not observable through `fetch`,
`dump` or `update` results. A replacement may choose any row in range;
comparisons use the masked form.

**CR-47** (black-box; must). Apart from the current rows, two runs of the
same command produce identical bytes: no uninitialised memory is written.
Every vector was recorded twice with different environment sizes.

## 11. Templates

**CR-48** (documentation, black-box; must). `--template` names an existing
RRD. It is opened after the `--no-overwrite` check (CR-62) and before any
definition is read. If it cannot be opened or is not an RRD, the command
fails with `Cannot open template RRD <name>` (missing file, directory, text
file), regardless of the later words.

**CR-49** (documentation, black-box; must). The template supplies the step
(unless `--step` is given) and the last update time (unless `--start` is
given). Its DS and archive definitions are copied byte for byte, in its
order, before the command's own; its rows, PDP state and CDP state are not
used: the new file is initialised by CR-41 to CR-46 as if every definition
had been given on the command line. Command-line RRA durations use the
final step (`-t tpl.rrd -s 7 RRA:AVERAGE:0.5:1m:1h` fails with
`Invalid step 1m: value would truncate when scaled`).

**CR-50** (black-box; must). Template DS names take part in the duplicate
check (`-t tpl.rrd DS:a:...` fails with `Duplicate DS name: a`).

**CR-51** (black-box; must). The template and the output may be the same
file (`create tpl.rrd -t tpl.rrd`): the template is read before the output
is replaced.

**CR-52** (black-box; must). Holt-Winters archives copied from a template
get the Holt-Winters CDP state of CR-36, not the classic one.

## 12. Sources (prefill)

Rondi supports only a narrow subset of `--source` (one source with the
same structure and last update) and loses precision in it; every other use
is refused. These rules record RRDtool's observable behavior; detailed row
and CDP content is pinned by the vectors (expectation `unsupported`) but
not specified, and needs its own specification before it is implemented.

**CR-55** (black-box; must). Each `--source` value is checked when the
option is read: if it cannot be examined, the command fails with
`error checking for source RRD <name>: <system error text>`; if it is not
a regular file, with `Not a regular file: <name>`. Several sources may be
given.

**CR-56** (black-box; should). After the definitions are read, each source
is opened; a failure gives `Cannot open source RRD <name>`. Without
`--start` the last update of the new file is the latest last update among
the sources. `--template` and `--source` may be combined.

**CR-57** (documentation, black-box; should). A target data source takes its
data from source data sources with its own name, or with the mapped name of
`DS:name=mapped[index]`; an index restricts it to the index-th source,
counted from 1. An index of 0 or beyond the number of sources, a mapped
name no source has, and a type that differs from the source's, all give an
unfilled data source without error.

**CR-58** (black-box; should). When a target data source is filled and
`--start` is not given, its PDP state (`last_ds`, value, unknown seconds)
is taken from the source data source; when `--start` is given, it is
`U` / unknown with unknown seconds 0. An unfilled data source gets the
fresh state of CR-42. Rows and CDP state are filled by a best-effort
procedure similar to, but not the same as, the back-fill that
`tune-modify.md` specifies for added archives (TM-38 to TM-47); the
vectors `-r src.rrd ... RRA:...` pin the results, and the AVERAGE
arithmetic is fused on arm64 and not on x86_64 (Linux overrides).

**CR-59** (documentation, black-box; should). The output may be one of the
sources (`create src.rrd -r src.rrd ...`); the source is read before it is
replaced.

## 13. The output file

**CR-60** (black-box; must). The vectors compare created files in masked
form, so a replacement's choice of current rows (CR-46) does not matter.
Every other byte must match.

**CR-61** (documentation, black-box; must). The file is first written in
full to a new temporary file in the same directory as the target and then
renamed over the target name, so a reader never sees a partial file. If the
temporary file cannot be created (missing directory `sub/o.rrd`, a name
ending in `/`, a name too long for the file system, an unwritable
directory) the command fails with `Cannot create temporary file`. If the
rename fails (the target is a directory, the name is empty or `.`), it fails
with `Cannot rename temporary file to final file!`. In every case no
temporary file is left behind.

**CR-62** (documentation, black-box; must). With `--no-overwrite`, if the
target name resolves to an existing file system object (following symbolic
links: a regular file, a directory, or a symbolic link to an existing
file), the command fails with `creating '<name>': File exists` before the
template is opened or any definition is read (`-O DS:bad ...` with an
existing target reports `File exists`). Option errors still come first
(CR-6).

**CR-63** (black-box; must). The file name `-` writes the file to standard
output instead of creating a file. With `--no-overwrite`, a file named `-`
in the working directory still triggers CR-62.

**CR-64** (documentation, black-box; must). A new file gets mode 0644
whatever the umask. When the target exists, its permission bits (including
set-user-ID and mode 000) are copied to the new file. The owner is the user
running the command. Test `create_file_mode_and_replacement`.

**CR-65** (black-box; must). Replacing an existing target replaces its
directory entry: the new file has a new inode, a hard link to the old file
keeps the old content, and a symbolic link given as the target name is
replaced by a regular file while its target is left alone. Without
`--no-overwrite` a dangling symbolic link is replaced the same way.

**CR-66** (black-box; should). With `--no-overwrite`, a dangling symbolic
link counts as absent (CR-62), so RRDtool replaces the link with the new
file. The existence check and the rename are separate steps, so an object
created in between is replaced too. A replacement should make the
no-overwrite creation atomic; refusing a dangling link is then acceptable
(divergence CR-div-noclobber-dangling).

## 14. Sizes and resources

**CR-70** (black-box; must, platform specific). RRDtool builds the whole
file in memory before writing it. If the total size cannot be allocated, or
its computation overflows 64 bits, the command fails with
`cannot allocate memory` and writes nothing. The vectors pin row counts of
2^61, 2^61+1, 2^62 and 2^64−1 with one data source and 2^60 with two, which
fail on every platform. Smaller impossible sizes behave differently per
platform: macOS fails cleanly from about 2^45 rows; a Linux kernel that
overcommits memory can let the allocation succeed and then kill the
process while it fills the rows; under x86_64 emulation the process traps.
A replacement should fail cleanly with `cannot allocate memory` (or another
`ERROR:` line) without writing, and should check the size before
allocating rather than rely on the allocator.

**CR-71** (black-box; should). There is no other limit on data source or
archive counts, rows or PDPs per row. Large valid files are written in full
(a request for 2^32 rows with one data source succeeded on macOS).

## 15. The cache daemon

**CR-72** (documentation, black-box; should). With `--daemon <address>`,
or the `RRDCACHED_ADDRESS` environment variable when `--daemon` is absent,
the create request is sent to rrdcached, with the file name made absolute
against the client's working directory. Errors reported by the daemon are
printed as `rrdcached@<address>: <daemon message>`, for example
`rrdcached@unix:/run/s.sock: RRD Error: invalid DS format` or
`rrdcached@<address>: Too many DS/RRA definitions (max 128)`. The daemon
accepts at most 128 DS and RRA words in one request; 127 data sources and
one archive are accepted. Not pinned by the vectors (they never set the
variable); observed with a local rrdcached.

**CR-73** (black-box; must). If the daemon cannot be reached (no such
socket, unresolvable host), RRDtool silently creates the file locally as if
no daemon had been named, and reports local errors without the
`rrdcached@` prefix (vectors `--daemon unix:/nonexistent/rondi.sock`).
Rondi instead fails with
`Unable to connect to rrdcached: <reason>` (divergence
CR-div-daemon-fallback).

## 16. Platform differences

**CR-74** (black-box; must). Unknowns written by `create` (DS bounds given
as `U`, PDP and CDP values, rows) use the platform's default quiet NaN:
`0x7ff8000000000000` on arm64 and aarch64, `0xfff8000000000000` on x86_64.
The tests fold the x86_64 pattern into the arm64 one before comparing.

**CR-75** (black-box; must, platform specific). The prefill AVERAGE
arithmetic (CR-58) differs between arm64 and x86_64;
`expected/create.linux-x86_64.vectors` holds those cases.
Huge allocations differ as CR-70 states. No other platform difference was
observed in the 466 cases on macOS arm64, Linux aarch64 and Linux x86_64
(under emulation; CI confirms native x86_64).

## 17. Defects and undefined behavior

**CR-76** (black-box; defect). CR-17's acceptance of `-` in names and of an
unused mapping index, CR-27's silent truncation of a CF word (`AVERAGE1`),
CR-20's silent cut of an over-long maximum and the ignored text after a
duration suffix (CR-14) are accepted quirks that a replacement reproduces
(must), because existing scripts may rely on them.

**CR-77** (black-box; defect). An `xff` of NaN is stored. Consolidation
with such an archive is not specified here. A replacement reproduces the
stored value.

**CR-78** (black-box; defect). CR-35 (Holt-Winters links and zero periods).

## 18. Refused features

**CR-80** (black-box; must for Rondi). RRDtool accepts `COMPUTE` data
sources and all Holt-Winters archives. Rondi does not implement them: a
create with a definition word whose type is `COMPUTE` or whose CF is a
Holt-Winters CF fails with an `ERROR:` line naming the feature
(`... not supported by Rondi`), exit status 1, and writes nothing. Rondi
refuses such a word before reporting errors in other words and also when
the word itself is malformed, so its message differs from RRDtool's there
(`RRA:HWPREDICT:20:0:0.01:5`, `RRA:... DS:y:COMPUTE:2,3,+ foo`); the
vectors accept either, as `unsupported`. Templates holding such archives
are a known divergence (CR-div-template-hw).

## 19. Behavior other commands depend on

The DS and RRA parsers are shared with other commands. A replacement must
keep these entry points consistent with this specification:

- `tune` structure changes (`crates/rondi/src/tune_modify.rs`) parse
  `DS:` words with the DS parser of section 5 (CR-17 to CR-20, the error
  texts, the version rule of CR-37 for the definition only) and `RRA:` words
  with the archive parser of sections 7 and 8, with the file's step (or the
  new step) as *S* for durations. `tune-modify.md` TM-15, TM-17 and TM-24
  rely on these texts and on the stored definition bytes of CR-39 and CR-40.
- The library entry `create_rrd_file` (`crates/rondi/src/rrd_binary.rs`)
  and the daemon's `CREATE` (`create_rrd_at`, used by
  `crates/rondi-server/src/lib.rs`) build files with sections 4 to 10 and
  CR-61 to CR-64. The daemon passes its own start and step after its own
  argument handling (CR-72), applies the 128-definition cap, and does not
  support templates or sources.
- The CLI (`crates/rondi-cli/src/main.rs`) implements sections 1 to 3 and
  dispatches templates, sources and the daemon.

## Summary by class

Must reproduce exactly: CR-1 to CR-20, CR-22 to CR-31, CR-37 to CR-45,
CR-47 to CR-51, CR-55, CR-60 to CR-65, CR-70 (pinned sizes), CR-73
(RRDtool's behavior; Rondi diverges), CR-74, CR-75, CR-76, CR-77 and, for
Rondi, CR-80.

Should reproduce: CR-21, CR-32 to CR-34, CR-36, CR-46, CR-52, CR-56 to
CR-59, CR-66, CR-71 and CR-72.

Reference defects not to copy: CR-35 and CR-78 (unchecked Holt-Winters
links, zero periods, `Unknown error`), CR-66 (non-atomic no-overwrite),
CR-70 (allocation behavior below the pinned sizes).

## Known divergences

Current Rondi against RRDtool 1.11.0, as of this specification:

- CR-div-initial-row (CR-46): Rondi always sets every current row to 0.
  Allowed; the test pins Rondi's choice.
- CR-div-noclobber-dangling (CR-66): with `-O`, Rondi refuses a dangling
  symbolic link (`Cannot rename temporary file to final file!`) where
  RRDtool replaces it. Intentional (safer).
- CR-div-daemon-fallback (CR-73): Rondi fails when the daemon cannot be
  reached instead of creating locally; it also prints daemon errors without
  the `rrdcached@<address>: ` prefix and checks `--template` locally before
  contacting the daemon (CR-72, not pinned).
- CR-div-template-hw (CR-52): with a template holding Holt-Winters
  archives, Rondi writes classic CDP state for them instead of the
  Holt-Winters state. Not intentional; a replacement should either
  implement CR-52 or refuse such templates (CR-80).
- CR-div-source (section 12): Rondi refuses every `--source` use except one
  source with the same definitions and last update, and in that case it
  copies data through the text dump, losing precision (vector `371`).
  Other source cases are `unsupported`.
- CR-80: `COMPUTE` and Holt-Winters definitions are refused (expectation
  `unsupported`).

## Tests

- `crates/rondi-cli/tests/create_spec_vectors.rs` replays every vector
  against Rondi under the `rrdtool` name, runs them live against the pinned
  RRDtool when it is on `PATH` (always under `RONDI_REQUIRE_ORACLE=1`) and
  compares Rondi with it directly, and checks the rules that vectors cannot
  pin: file modes and replacement (CR-64, CR-65), the default start and step
  (CR-11, CR-12) and the random current row (CR-46).
- `crates/rondi-cli/tests/data/create/cases.tsv` lists each case: id,
  Rondi expectation (`same`, `unsupported`, `divergent`, `clean-error`,
  `undefined`), divergence note, inputs and the command words. A `divergent`
  case fails once Rondi starts to match, so the expectation and this list
  are updated together.
- `inputs/*.rrd` are templates and sources created and updated by RRDtool
  1.11.0 on macOS, frozen because `create` picks random current rows.
- `expected/create.vectors` holds, per case, the exit status, standard
  output, standard error and every directory entry afterwards: `same:` for
  an untouched input, `file:` with the masked bytes of a new or changed
  file, `link:` and `dir:`. Sections are framed as
  `@@ <id> <section> <length>`, the bytes and a newline.
  `expected/create.linux-<arch>.vectors`, when present, replaces the cases
  that differ on that Linux architecture (only x86_64 has any). The usage
  banner's build time is dropped before comparing (CR-2), and x86_64 NaNs
  are folded (CR-74).
- `scripts/gen-create-resize-vectors.py write` rebuilds the vectors from a
  local RRDtool 1.11.0, running each case twice and marking cases whose
  output varies as `undefined`; with `--rondi` it also checks every
  expectation against a Rondi binary. `overrides --platform linux-<arch>`,
  run in a Linux container with the pinned build, records the overrides.
