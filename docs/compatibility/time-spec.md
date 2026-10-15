# At-style time specifications

This specification describes how RRDtool 1.11.0 turns a time specification
such as `now-1d`, `noon yesterday`, `20030102 13:00` or `end-3w` into a
timestamp, as observed through the `rrdtool` command. It covers the options
that take such a specification, the accepted grammar, how each part is
evaluated, calendar and time zone arithmetic, how `start` and `end` refer to
each other, the error texts, numeric range handling, the timestamp field of
`rrdtool update`, and the date comments that `rrdtool dump` writes. It is
written for an implementer who has not seen RRDtool's source and says what a
user can observe, not how RRDtool computes it.

It does not cover the `strftime` formats of `GPRINT`/`PRINT` with
`%s`/`strftime`, graph axis labels, or the duration formats of `PRINT`; those
live outside the time parser (see the audit entry for `main.rs` helpers in
`docs/provenance-audit.md`).

Each rule carries a provenance tag:

- **[doc]** the AT-STYLE TIME SPECIFICATION section of `rrdfetch(1)`, or
  `rrdgraph(1)`, `rrdcreate(1)`, `rrdupdate(1)`;
- **[bb]** black-box runs of RRDtool 1.11.0, recorded in the vector table.
  Rules about the current time were established by running the Linux build
  under `faketime(1)` at fixed instants (see `--now-check` below) and are
  pinned by equivalence vectors;
- **[rsi]** reference-source inspection. No rule in this document needed it:
  every rule below was settled by documentation or black-box runs.

Each rule also has a class:

- **must**: a replacement reproduces it exactly;
- **should**: expected for compatibility, but it depends on the C library or
  is pinned only on some platforms;
- **defect**: the reference contradicts its manual or produces a
  meaningless value. The result is deterministic and the vectors pin it, so
  a replacement reproduces it for compatibility; it is marked so that a
  later decision to diverge is a deliberate one;
- **undefined**: the reference result depends on memory or on the current
  time in a way no user can rely on. A replacement must return a clean
  result or error, never crash or hang, and is not compared.

Where black-box results contradict the manual, the black-box result is the
contract and the conflict is noted.

## Compatibility tests

- `crates/rondi-cli/tests/data/time_spec_vectors.json`: 1,697 vectors in six
  time zones (UTC, America/New_York, Europe/London, Asia/Kolkata,
  Pacific/Auckland, Australia/Lord_Howe) plus POSIX and invalid `TZ`
  values. Each vector stores its `TZ`, the command steps run in a fresh copy
  of the fixture directory, tags, and either the expected result of every
  step (exit status, stderr, and filtered stdout) or, for 342 vectors whose
  result depends on the current time, a `same_as` argument list that must
  give the same result.
- `crates/rondi-cli/tests/time_spec_vectors.rs` replays every vector against
  Rondi under the `rrdtool` name. With the pinned RRDtool 1.11.0 on `PATH`
  it also runs each vector through it and requires the same result;
  `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure.
- `scripts/time-spec-vectors.py` regenerates the table
  (`--rrdtool /path/to/rrdtool`), runs every case twice and tags a case
  `unstable` when the runs differ (none are), `--check PATH` diffs another
  binary against the table, `--probe TZ START [END]` prints one result,
  `--record-platform`/`--merge-platform` record Linux results, and
  `--now-check` reruns the table under `faketime` at six instants (including
  instants inside DST transitions and a year boundary) to prove that every
  fixed vector is independent of the current time and every `same_as`
  equivalence holds at each instant.

The echo used by most vectors is
`rrdtool graphv /dev/null --start S --end E DEF:x=f.rrd:x:AVERAGE LINE1:x#000`,
whose `graph_start` and `graph_end` lines are the parsed times. With
`--end start+1` the end is fixed relative to the start. Results below
315360000 cannot be printed by `graph` (TS-61), but its error message shows
the value, so the vectors still pin them.

Placeholders in `same_as`, filled from `date` in the vector's zone:
`{now}` (epoch seconds), `{Y}`, `{m}`, `{d}`, `{H}`, `{M}`, `{S}` (local
fields, zero padded), `{D+k}`/`{D-k}` (local date k days from today as
`MM/DD/YYYY`), `{W:k}` (the date of weekday k, 0 = Sunday, in the current
week that starts on Sunday).

Vector tags:

| Tag | Meaning | Replay rule |
|---|---|---|
| `undefined` | Reference result depends on the current time in a meaningless way (TS-66). | Not compared; Rondi must exit without a signal. |
| `dst-overlap` | A wall-clock time inside a repeated hour (TS-52). | Frozen comparison on macOS only; the live oracle comparison runs everywhere. |
| `unstable` | Two capture runs differed. | Not frozen-compared. No vector carries it at present. |

The main `expect` was captured on macOS arm64 (`/opt/homebrew/bin/rrdtool`
1.11.0). Where the Linux builds differ, the vector carries
`expect_platform.linux-aarch64` or `expect_platform.linux-x86_64` and the
replay test uses that value on the matching target. Both were recorded with
the 1.11.0 oracle built as in `docker/Dockerfile.test`. A vector with a
named zone is frozen-compared only where `/usr/share/zoneinfo` has that
zone.

## 1. Where specifications are read

**TS-1** (doc, bb; must). These options take an at-style specification:
`graph`, `graphv` and `xport` `--start`/`-s` and `--end`/`-e`; the
`start=` and `end=` fields of a graph `DEF`; `fetch` `--start`/`-s` and
`--end`/`-e`; `create` `--start`/`-b`; and the timestamp of an `update`
value that is written with `@` (TS-70).

**TS-2** (bb; must). A parse error is reported on stderr as
`ERROR: start time: <message>` or `ERROR: end time: <message>` for the
start and end options of `graph`, `graphv`, `xport` and `fetch`, for a
`DEF`'s `start=`/`end=`, and for `create --start` (always `start time`).
Nothing is written to stdout and the exit status is 1.

**TS-3** (bb; must). Errors from relating start and end (TS-56 to TS-60) and
range checks of the command (TS-61) carry no `start time:`/`end time:`
prefix.

**TS-4** (doc, bb; must). Defaults: `graph`, `graphv` and `xport` use
`end-24h` for the start and `now` for the end; a `DEF` with no `start=`
or `end=` uses the graph's range. Pinned by `ref-129`, `ref-130` and the
`now` group.

**TS-5** (bb; must). `create` rejects `start` and `end` references with
`specifying time relative to the 'start' or 'end' makes no sense here` and
rejects a result before 1980 with `the first entry to the RRD should be
after 1980`; values from 315360001 on are accepted. `create` does not use
the `start time:` prefix for these two.

## 2. Lexical rules

**TS-6** (doc, bb; must). Letters are compared without regard to case.

**TS-7** (doc, bb; must). Blanks (space, tab, newline), `_` and `,` separate
tokens and are otherwise ignored. Separators may also be omitted wherever a
token changes kind: `12:00Jan 2 2003`, `noon01/02/2003`, `Jan2 2003` and
`+1d2h` parse.

**TS-8** (bb; must). Tokens are: a run of decimal digits (a number); a run
of letters (a word); and the single characters `+`, `-`, `:`, `.` and `/`.

**TS-9** (bb; must). Any other byte (for example `;`, `#`, `!`, `@`, `(`,
`$`, `%`, `&`, `*`, `=`, `?`, `\`, quotes, brackets, `|`, `~`, `^`, a
backtick, and any byte of a non-ASCII character) ends the specification:
the text from that byte on is ignored without error. So `1000000000#+1d`
is 1000000000 and `12:00 01/02/2003#comment` is accepted. Exception: a
specification that starts with such a byte is unparsable; `@x` fails with
`unparsable time: @@x`, the input echoed with an extra `@` in front
[defect in the echo]. Only `@` was tested in that position.

**TS-10** (bb; must). A word is a keyword only when the whole letter run
matches an entry of the table below exactly; prefixes and other
abbreviations are not accepted (`Janu`, `Ja`, `sept`, `tues`, `thur`,
`mins`, `hrs`, `sta`, `epo`). Two words are not split: `sundays` is one
unknown word.

| Kind | Words |
|---|---|
| reference | `now`, `n`, `start`, `s`, `end`, `e`, `epoch` |
| time of day | `midnight` (00:00), `noon` (12:00), `teatime` (16:00), `am`, `pm` |
| relative day | `yesterday`, `today`, `tomorrow` |
| month | `jan` ... `dec` (three letters) and the full English names `january` ... `december` |
| weekday | `sun` ... `sat` (three letters) and `sunday` ... `saturday` |
| offset unit | seconds: `s`, `sec`, `second`, `seconds`; minutes: `min`, `minute`, `minutes`; hours: `h`, `hr`, `hour`, `hours`; days: `d`, `day`, `days`; weeks: `w`, `wk`, `week`, `weeks`; months: `mon`, `month`, `months`; years: `y`, `yr`, `year`, `years`; and `m` (months or minutes, TS-40) |

`may` is both the three-letter and the full name. `s` and `e` mean
`start`/`end` in reference position and seconds/an unknown unit in offset
position (TS-38).

**TS-11** (bb; must). An empty specification, or one of separators only,
fails with `unparsable time: ` followed by the input as given (empty for
`""` and `" "`).

## 3. Overall shape

**TS-12** (doc, bb; must). A specification is an optional *reference*
followed by zero or more *offsets*. When the reference is omitted it is
`now`. The reference is one of:

1. a reference keyword: `now`, `start`, `end` or `epoch` (TS-14 to TS-17);
2. a time of day, optionally followed by a day (TS-18 to TS-27);
3. a day without a time of day, optionally followed by a time of day
   (TS-28 to TS-30);
4. a number of seconds since the epoch (TS-31).

**TS-13** (bb; must). After the reference, every remaining token must
belong to an offset (TS-35). The first token that does not fails with
`unparsable trailing text: '...<rest>'`, where `<rest>` is the input from
that token on (including the `:` or `.` that starts it), for example
`unparsable trailing text: '...:45 01/02/2003'` for `12:30:45 01/02/2003`
and `'...noon'` for `01/02/2003 noon`. A specification whose first token
cannot start a reference fails with `unparsable time: <input>` (the whole
input, for example `unparsable time: monday 13:00`, `unparsable time:
today`, `unparsable time: T12:00`).

## 4. Reference keywords

**TS-14** (doc, bb; must). `now` or `n` is the current time. If any token
follows it, the next token must be `+` or `-`, otherwise the parse fails
with `if 'now' is followed by a token it must be +|- offset` (`now foo`,
`now 12:00`, `now now`). [doc says nothing about this error]

**TS-15** (doc, bb; must). `start`/`s` and `end`/`e` refer to the other
option's time (TS-56). They must be followed by `+` or `-`; otherwise the
parse fails with `'start' or 'end' MUST be followed by +|- offset`.

**TS-16** (doc, bb; defect). `epoch` must be followed by `+` or `-` and
fails with the same message as TS-15 when it is not. The manual says
`epoch` is timestamp 0, so `epoch+19711205s` is 19711205. That is not what
happens:

- on macOS, when no day, week, month or year offset follows, the reference
  value is −1, so `epoch+N` and `epoch+Ns` are N−1 in every zone
  (`epoch+1000000000` is 999999999, `epoch-1` is −2);
- on Linux, and on macOS once a day, week, month or year offset is
  present, the reference is local midnight at the start of 31 December 1899
  plus those calendar offsets, so `epoch+1000000000` is
  1000000000 − 2209075200 = −1209075200 in UTC and `epoch+1d+1000000000s`
  is −1208988800 in UTC on both platforms.

The vectors `ref-101` to `ref-126`, `create-025`, `create-026` and
`update-019` pin both platforms. The difference comes from the C library's
conversion of a date before 1900 (TS-53).

**TS-17** (bb; must). A reference keyword cannot be followed by a time of
day or a date, and a time of day cannot be followed by a reference keyword:
`12:00 now` fails with `unparsable trailing text: '...now'`.

## 5. Time of day

**TS-18** (doc, bb; must). `midnight`, `noon` and `teatime` set 00:00,
12:00 and 16:00. They may be followed by a day (TS-23) or by offsets, but
not by `am`/`pm` (`noon pm 01/02/2003` fails with trailing text `...pm
01/02/2003`) or by another time of day.

**TS-19** (bb; must). A number is read as an hour when it has at most two
characters, its value is at most 23, and it is not followed by `/` or `.`.
`0` to `23` and `00` to `09` qualify; `24`, `25` and any number of three or
more characters (including `009` and `001`) do not and are read as a date
(TS-28), which typically fails with `did you really mean month <n>?` or
`I'm afraid that 0 is not a valid day of the month`.

**TS-20** (bb; must). An hour may be followed by `:` and a number of
minutes. The minutes number may have any number of digits; a value above
59 fails with `parsing HH:MM syntax, got MM = <n> (>59!)`, `<n>` being the
decimal value (`12:123` reports 123). A second `:` is not accepted
(`12:30:45` leaves `:45` as trailing text). `HH.MM` is documented but does
not exist: a number followed by `.` is a `DD.MM` date (`12.30` fails with
`did you really mean month 30?`). [doc conflict]

**TS-21** (doc, bb; must). `am` or `pm` may follow the hour or the hour and
minutes, with or without a separator. An hour above 12 fails with `there
cannot be more than 12 AM or PM hours`. With `am` the hour 12 becomes 0;
with `pm` the hour becomes hour mod 12 plus 12 (`0pm` and `12pm` are 12:00,
`12am` and `0am` are 00:00). `p`, `a`, `a.m.` are not accepted.

**TS-22** (bb; must). Without a following day, a time of day applies to the
current local date: `5` is 05:00 today, `12:34` is 12:34 today, `teatime`
is 16:00 today, `9pm` is 21:00 today, even when that is in the future.
Seconds are 0. (`now` group.)

## 6. Day after a time of day

**TS-23** (doc, bb; must). After a time of day the day may be:

- `today`, `yesterday` or `tomorrow`: the current local date, minus or plus
  one day;
- a weekday name: the day with that weekday in the current week, weeks
  starting on Sunday. On a Wednesday `sun` is three days ago and `sat` three
  days ahead; on a Sunday `sun` is today;
- a month name followed by a day number and an optional year (TS-25);
- a numeric date (TS-28).

At most one day may follow; a second day word or date is trailing text
(`noon yesterday tomorrow`, `noon sun mon`, `noon today 01/02/2003`).

**TS-24** (doc, bb; must). A weekday name or `today`/`yesterday`/`tomorrow`
is only accepted after a time of day. On its own, or before a time of day,
it fails with `unparsable time: <input>` (`monday`, `today 13:00`). The
manual's NOTE2 says a time of day is required with these day forms; the
reference enforces it this way. A weekday name cannot be followed by a date.

**TS-25** (doc, bb; must). A month name must be followed by a number, the
day of the month, else the parse fails with `the day of the month should
follow month name` (`Jan`, `13:00 Jan`, and `1 jan`, where `1` is an hour).
The day is not range-checked: `Jan 0`, `Jan 32`, `Jan 99`, `Feb 30` are
accepted and normalized (`Jan 32` is 1 February, `Feb 30 2003` is 2 March
2003, `Jan 0` is 31 December of the previous year). The day may be followed
by a year number (TS-33), with or without a comma separator (`Jan 2, 2003`).

**TS-26** (bb; defect). When a date with no year follows a numeric time of
day (an hour, `HH:MM`, or either with `am`/`pm`), the year is 30000:
`13:00 Jan 2`, `13:00 01/02`, `13:00 02.01` and `9am Jan 2` are all in
January 30000, local time (`13:00 Jan 2` is 884541474000 in UTC). After
`midnight`, `noon` or `teatime`, a date with no year uses the current year
(`noon Jan 2`, `noon 01/02`, `teatime 2.1`).

**TS-27** (bb; must). The number right after a month name is always the
day, whatever its size: `Jan 2003` is day 2003 of January of the current
year, normalized as in TS-25 (26 June 2031 for 2026), keeping the current
time of day. A number after the day is the year (TS-33); a third number is
trailing text (`Jan 2 2003 2004`).

## 7. Days and dates without a time of day

**TS-28** (doc, bb; must). A number that is not an hour (TS-19) starts a
date. In this order:

1. a value above 315360000 is seconds since the epoch (TS-31), even after a
   time of day (`12:00 1000000000` is 1000000000);
2. a value above 19700101 and below 24000101 is `YYYYMMDD`, split by
   character position: the first four characters are the year, the next two
   the month, the rest the day. `0019700102` therefore reads month `70` and
   fails with `did you really mean month 70?`;
3. a number followed by `/` is `MM/DD`, optionally followed by `/` and a
   year: `1/2/03`, `01/02/2003`;
4. a number followed by `.` is `DD.MM`, optionally followed by `.` and a
   year: `2.1.03`, `31.12.2024`;
5. any other number is a month with no day, which fails (with `did you
   really mean month <n>?` when the value is not 1 to 12, otherwise with
   `I'm afraid that 0 is not a valid day of the month`). This is why
   `19700101`, `29991231`, `315360000`, `86400`, `1230` and `2003-01-02`
   fail with `did you really mean month <n>?`, `<n>` being the decimal value
   of the whole number without leading zeros.

Leading zeros do not change a number's value (`001/02/2003` is 2 January).

**TS-29** (bb; must). Numeric dates are validated in this order: the month
must be 1 to 12, else `did you really mean month <m>?`; then the day must be
1 to 31, else `I'm afraid that <d> is not a valid day of the month`. The day
is not checked against the month: `02/30/2003` and `31.04.2025` normalize to
the next month. For `DD.MM` a day above 31 is reported as a month
(`32.01.2003` fails with `did you really mean month 32?`) [defect in the
message]. Missing parts fail with `there should be day number after '/'`,
`there should be month number after '.'`, `there should be year number
after '/'` or `there should be year number after '.'` (also when a
non-number follows the separator). A third separator or a mixed pair
(`1/2.2003`, `1.2/2003`, `1/2/2003/4`) leaves trailing text.

**TS-30** (doc, bb; must). A date may be followed by a time of day: an hour
or `HH:MM`, each optionally with `am`/`pm` (`01/02/2003 12:30pm`,
`20030102 13`, `Jan 2 2003 12:00`, `01/02/2003 8`). `midnight`, `noon` and
`teatime` are not accepted after a date (`01/02/2003 noon` leaves
`...noon`). A lone `am`/`pm` after a date gives 00:00 for both
(`01/02/2003 pm` is midnight) [defect]. Without a following time of day, a
numeric date (`01/02/2003`, `20030102`, `02.01.03`) is at 00:00:00, while a
month-name date (`Jan 2 2003`, `Oct 12`) keeps the current local time of
day including seconds (`now` group, for example `Jan 2` matches
`{H}:{M} 01/02/{Y} +{S}s`).

## 8. Seconds since the epoch

**TS-31** (doc, bb; must). A number whose value is above 315360000 is a
timestamp in seconds and is taken as is, in every zone: `315360001`,
`931200300`, `2147483648`, `4294967296`, `99999999999` and
`67767976233532800` parse to themselves. It may be followed by offsets
only; a time of day or date after it is trailing text
(`1000000000 12:00`). A fraction is trailing text (`1000000000.5` leaves
`....5`).

**TS-32** (bb; undefined). A number too large for a signed 64-bit integer
(`9223372036854775808`, `99999999999999999999`), and `9223372036854775807`
itself, gives a value that depends on the current time on both macOS and
Linux. Vectors `range-019` to `range-021` are tagged `undefined`; a
replacement may reject these with an error.

## 9. Years

**TS-33** (doc, bb; must). A year number Y written after a date is read as
follows:

| Y | Year |
|---|---|
| 0 to 37 | 2000 + Y (`0`, `00` and `000` are 2000) |
| 38 to 69 | fails: `won't handle dates before epoch (01/01/1970), sorry` |
| 70 to 99 | 1900 + Y |
| 100 to 138 | 1900 + Y (`100` is 2000, `138` is 2038) |
| 139 to 1970 | fails: `invalid year <Y> (should be either 00-99 or >1900)` |
| 1971 and above | Y |

The message shows Y as a decimal value without leading zeros. The checks
apply to the year as written, before any offset, so `12/31/69 +20y` still
fails. The manual's "2- or 4-digit year" is narrower than what is accepted.

**TS-34** (bb; should). Years far beyond 9999 are accepted (`99999` gives
3093496578000 for 13:00 UTC on 2 January). A year number is reduced to a
signed 32-bit value before use; a year up to 2147485547 is representable
(1900 + the largest signed 32-bit value). Beyond that the result depends
on the C library: macOS fails with `the specified time is incorrect (out of
range?)`, glibc returns a wrapped value (vectors `year-036` to `year-038`
record both). `99999999999` reduces to 1215752191 and is accepted on both.

## 10. Offsets

**TS-35** (doc, bb; must). An offset is `+` or `-` followed by one or more
*amounts*. An amount is a number optionally followed by a unit word. The
amounts after one sign continue until the next `+` or `-` or the end, and
all take that sign: `-5mon1w2d`, `+1d 2h`, `+1d,2h`, `-1h 5m`. A sign must
be followed by a number, else the parse fails with `There should be number
after '+'` or `There should be number after '-'` (`now+`, `+d`, `++1d`,
`+-1d`, `+1d-`, a lone `-`).

**TS-36** (doc, bb; must). An amount without a unit word is seconds:
`+86400`, `-1-1` (two seconds back).

**TS-37** (bb; must). The token after an amount's number is always taken
as its unit, unless it is `+`, `-` or the end. If it is a unit word of
TS-10, that is the unit. Otherwise the amount is seconds and the token is
consumed silently, whatever it is: a word (`+1x`, `+1mins`, `+1hrs`,
`+1noon` and `+1ms` are each one second; `+1foo2h` is one second plus two
hours), a `.`, `:` or `/` (`+1.5d` is one second plus five days, `+1:5` six
seconds), or another number (`+1 2` is one second, `+1 2 3` four seconds,
`+1 2 3 4` four seconds). A word that then follows a swallowed number is
trailing text: `+1 2h` fails with `unparsable trailing text: '...h'`.
[doc conflict: the manual implies plural abbreviations are accepted]

**TS-38** (bb; must). In offset position `s` means seconds, and `e` is an
unknown word (seconds): `+1e3s` is four seconds.

**TS-39** (doc, bb; must). Seconds, minutes and hours are exact: they add
1, 60 and 3600 seconds per unit to the final timestamp. Days and weeks add
1 and 7 to the calendar day, months add 1 to the calendar month, and years
add 1 to the calendar year (TS-45).

**TS-40** (doc, bb; must). `m` alone (any case) means months or minutes.
Within one sign, after an amount in years, months, weeks or days it means
months; after hours, minutes or seconds it means minutes; as the first
amount after a sign it means months when the number is below 6 and minutes
otherwise (`+5m` is five months, `+6m` six minutes, `-5m` five months back).
An earlier `m` sets the context for a later one: `+1m2m` is three months,
`+6m2m` eight minutes. An amount with an unknown unit word does not change
the context (`+1h 1x 7m` is one hour, one second and seven minutes). Each sign starts afresh: in `-1h-5m` the `5m` is five
months, in `-1h 5m` five minutes.

**TS-41** (bb; should). An amount's number is read as a decimal integer,
saturating at the largest signed 64-bit value, and then reduced to its low
32 bits as a signed value. For seconds, minutes and hours that count is
multiplied by 1, 60 or 3600 and the product is again reduced to a signed
32-bit value before it is added. So `+4294967296s` adds 0,
`+99999999999s` adds 1215752191, `+3000000000s` subtracts 1294967296,
`+999999999999999h` adds 661123568 seconds, and
`+9999999999999999999s` subtracts one second. The vectors in the `range`
group pin these. [defect: a large offset silently wraps]

**TS-42** (bb; should). Calendar offsets whose sum overflows the calendar
field fail on macOS with `the specified time is incorrect (out of range?)`
(`+2147483647d`, `+2147483647mon`, `+2147483647y`) and give a wrapped
value on glibc; both are recorded. Large but representable offsets work
everywhere (`+25000000d`, `+178956970y`).

## 11. Evaluation

**TS-43** (bb; must). The reference sets a local calendar date and time.
All day, week, month and year amounts of all offsets are added to that
calendar date and time first, in any order, and the result is converted to
a timestamp once. Then all second, minute and hour amounts are added to the
timestamp. Consequences: `Jan 31 2003 10:00 +1mon-1mon` is 31 January
(the month sum is zero), `May 31 2003 10:00 -1mon-1mon` is 31 March, and
`+1d-24h` and `+30d-1mon` mix freely. Because hours are added after the
conversion, `8:00 Mar 8 2025 +48 hours` in New York is 9:00 on 10 March,
while `+2 days` is 8:00 (the manual's EET example).

**TS-44** (doc, bb; must). Normalization of an out-of-range calendar date
follows the proleptic Gregorian calendar with carries: 31 January plus one
month is 3 March (2 March in a leap year), 29 February 2024 plus one year
is 1 March 2025 and minus one year 1 March 2023, 31 May minus one month is
1 May (the manual's example).

**TS-45** (bb; must). A calendar amount whose date already lies in another
part of the reference is still added to the calendar fields, so
`midnight + 1 day`, `noon yesterday -3hours` (09:00 yesterday) and
`9am-1day` behave as the manual's examples show.

## 12. Time zones and daylight saving time

**TS-46** (bb; must). All calendar fields are local time in the zone named
by `TZ`. An unknown zone name (`Not/A_Zone`) and an empty `TZ` are treated
as UTC. POSIX rule strings (`EST5EDT`, `<+0530>-5:30`) are honored as the C
library reads them.

**TS-47** (bb; must). Each reference carries a daylight-saving flag into
the single conversion of TS-43:

- a date or time of day written in the specification, `today`,
  `yesterday`, `tomorrow`, a weekday, and `now`: the flag is "unknown", so
  the conversion picks the offset in force at the resulting local time. So
  `now-1d` across a spring-forward keeps the wall-clock time (`now` group,
  `now-1d` matches `{H}:{M} {D-1} +{S}s`), and `noon yesterday` is noon;
- seconds since the epoch, and the `start`/`end` anchor (TS-57): the flag
  is the one in force at that instant. A calendar offset therefore keeps
  the instant's UTC offset: `1041397200+180d` in New York (1 January 2003
  00:00 EST) is 30 June 2003 01:00 EDT, and `1741500000+1d` equals
  `1741500000+24h` across the March 2025 change. `end-1d` is exactly 86400
  seconds before the end and `start+1d` exactly 86400 after the start in
  every tested zone, and `end-1mon` across the change keeps the anchor's
  offset.

**TS-48** (bb; must). A local time that falls in a spring-forward gap is
moved forward by the length of the gap: `Mar 9 2025 02:30` in New York is
03:30 EDT, `02:00` is 03:00 EDT; `Oct 5 2025 02:15` on Lord Howe Island
(a 30-minute gap) is 02:45. Macs and glibc agree.

**TS-49** (bb; must). Hours, minutes and seconds added after the conversion
cross transitions as plain seconds: `Mar 9 2025 02:30 -1h` is one hour
before 03:30 EDT.

**TS-50** (bb; must). Zones without daylight saving time, and fractional
offsets (`Asia/Kolkata` +05:30, `Asia/Kathmandu` +05:45, Lord Howe's
+10:30/+11:00), need no special handling beyond TS-46.

**TS-51** (bb; must). A time of day written without a date, and the
current date used by `today` and friends, is the local date in `TZ`.

**TS-52** (bb; should). A local time that occurs twice (the repeated hour
after a fall-back) is resolved by the C library. macOS resolves the
vectors below to a fixed instant (recorded in `expect`). glibc picks the
first or the second instance depending on whether daylight saving time is
in force at the current moment, so the result varies with the date the
command runs (shown with `--now-check`). Affected vectors are tagged
`dst-overlap`: New York `Nov 2 2025 01:00`, `01:30`, `20251102 01:00`,
`Nov 1 2025 01:30 +1d`, `+1 day`; London `Oct 26 2025 01:30`; Auckland
`Apr 6 2025 02:30`, `4/6/25 2:30am`, `Apr 5 2025 02:30 +1d`; Lord Howe
`Apr 6 2025 01:30`, `01:45`. A replacement should use the host C library's
resolution so that it agrees with the reference on the same host.

**TS-53** (bb; should). Conversion uses the host C library's 64-bit
`time_t` range. The `epoch` results of TS-16 are consistent with macOS
failing to convert a local date before 1900 (giving −1, which the
reference does not detect) while glibc converts it; this is an inference
from the outputs, not a tested rule for other inputs, since the year table
of TS-33 keeps other dates from reaching 1899. Values up to year 2147485547 are
representable on both.

## 13. Current-time references

**TS-54** (bb; must). `now`, the default reference, a time of day without a
date, `today`, `yesterday`, `tomorrow`, weekdays, and a month-name date
without a year take the current time when the command starts. `graph` uses
the same current time for start and end.

**TS-55** (bb; must). The equivalences in the `now` group hold at every
instant tested by `--now-check` and at capture time, in UTC, Asia/Kolkata,
America/New_York and Pacific/Auckland. They are the contract for the forms
listed in TS-22, TS-23, TS-26 and TS-30.

## 14. `start` and `end`

**TS-56** (doc, bb; must). In a command that takes both a start and an end,
`start`/`s` in the end specification refers to the start time and
`end`/`e` in the start specification refers to the end time. Each
specification is first parsed on its own; the cross-reference is resolved
after both parsed.

**TS-57** (bb; must). The anchor is resolved first and the relative
specification's offsets are then applied to it as in TS-43, with the
anchor's daylight-saving flag (TS-47). Offsets after `start`/`end` may be
chained (`start+1d+90min`, `end-1w-3h`, `start+1s 2s`).

**TS-58** (bb; must). Errors, checked in this order after both parsed:

- the start refers to itself (`start+1` as start, whatever the end is):
  `the start time cannot be specified relative to itself`;
- the end refers to itself (`e+1` as end): `the end time cannot be
  specified relative to itself`;
- the start refers to the end and the end to the start: `the start and end
  times cannot be specified relative to each other`.

These carry no `start time:`/`end time:` prefix. The same rules apply to a
`DEF`'s `start=`/`end=`.

**TS-59** (bb; must). `create` and `update` reject `start` and `end` with
the messages in TS-5 and TS-70.

**TS-60** (bb; must). After resolution, `graph`, `graphv`, `xport` and
`fetch` fail with `start (<s>) should be less than end (<e>)` when the
start is greater than the end. Equal times are accepted by `graph`
(`ref-041`).

## 15. Range checks after parsing

**TS-61** (bb; must). `graph`, `graphv` and `xport` fail with `the first
entry to fetch should be after 1980 (<s>)` when the start is below
315360000, `<s>` being the parsed start (315360000 itself is accepted,
315359999 is not). `create` uses TS-5. Within those checks,
timestamps up to at least 67767976233532800 are accepted.

**TS-62** (bb; must). `fetch` aligns start and end to the resolution of
the archive it reads, as `rrdfetch(1)` describes; the vectors in the
`fetch` group pin the rows, including relative ranges.

## 16. Error texts

**TS-63** (bb; must). The complete set of parse messages, each after the
`start time: `/`end time: ` prefix of TS-2:

| Message | Rule |
|---|---|
| `unparsable time: <input>` | TS-9, TS-11, TS-13 |
| `unparsable trailing text: '...<rest>'` | TS-13 |
| `if 'now' is followed by a token it must be +\|- offset` | TS-14 |
| `'start' or 'end' MUST be followed by +\|- offset` | TS-15, TS-16 |
| `There should be number after '<sign>'` | TS-35 |
| `parsing HH:MM syntax, got MM = <n> (>59!)` | TS-20 |
| `there cannot be more than 12 AM or PM hours` | TS-21 |
| `the day of the month should follow month name` | TS-25 |
| `did you really mean month <n>?` | TS-28, TS-29 |
| `I'm afraid that <d> is not a valid day of the month` | TS-28, TS-29 |
| `there should be <day\|month\|year> number after '<sep>'` | TS-29 |
| `won't handle dates before epoch (01/01/1970), sorry` | TS-33 |
| `invalid year <Y> (should be either 00-99 or >1900)` | TS-33 |
| `the specified time is incorrect (out of range?)` | TS-34, TS-42 |

**TS-64** (bb; must). Only the first error is reported. A specification is
read left to right and the first failing token decides the message.

**TS-65** (bb; must). Every error exits with status 1 and writes nothing to
stdout.

**TS-66** (requirement; must). The parser never needs unbounded time or memory: the
input is consumed token by token once. A replacement must not crash, hang
or panic on any input, including very long digit runs, very long
specifications, non-UTF-8 bytes and values that overflow; where the
reference is undefined (TS-32) it may return an error instead.

## 17. `update` timestamps

**TS-67** (doc, bb; must). An `update` value is `timestamp:value...` or
`timestamp@value...`. With `@`, the text before the first `@` is an at-style
specification (TS-1). With `:`, the text before the first `:` is `N` (now)
or a number.

**TS-68** (bb; must). The `:` form reads the number as a floating-point
value: a decimal point, an exponent and a leading `+` or blanks are
accepted (`1e9`, `1000000100.`, `+1000000100`, ` 1000000100`). A fraction
is kept below one second; `rrdtool last` shows whole seconds
(`1000000100.5` reads back as 1000000100, while `1000000100.999999999`
reads back as 1000000101, consistent with the fraction being kept to the
microsecond). Trailing text fails with `error while parsing time in
get_time_from_reading - Converted '<text>' to <value>, but cannot convert
'<rest>'`; text that does not start with a number fails with `Cannot
convert '<text>' to float`. A value starting with `-` is taken as an
option by the argument parser (`invalid option -- '1'`). An at-style form
is not accepted with `:`.

**TS-69** (bb; must). The `@` form does not accept a fraction (trailing
text `'....25'`) and reports errors as `<file>: ds time: <text>: <message>`
(`ERROR: u.rrd: ds time: foo: unparsable time: foo`; an empty text gives
`ds time: : unparsable time: `).

**TS-70** (bb; must). `start` and `end` in an `@` timestamp fail with
`<file>: specifying time relative to the 'start' or 'end' makes no sense
here: <text>`. An `@` timestamp with offsets after a number works
(`1000000100+1d@5`, `1000000100 +1d@5`). Errors from the value part
(`1000000100@`, `1000000100@5@6`) come from value parsing, not from the
time parser.

## 18. `dump` date comments

**TS-71** (bb; must). Each row that `rrdtool dump` writes is preceded by a
comment `<!-- YYYY-MM-DD HH:MM:SS ±hhmm / <epoch> -->` giving the row's
time in the local zone of `TZ` with its numeric UTC offset (`+0000`,
`-0400`, `+0530`, `+1300`, `+1100`), followed by the epoch seconds. The
offset is the one in force at that instant, so it changes across a DST
transition within a dump. Linux and macOS write the same text. (`dump`
group.)

## 19. Platform notes

- macOS and glibc differ in four places, all recorded per platform in the
  vectors: the `epoch` reference (TS-16), year and calendar overflow
  (TS-34, TS-42), the resolution of repeated local times (TS-52), and the
  conversion of dates before 1900 (TS-53). Every other vector gives the
  same result on macOS arm64, Linux aarch64 and Linux x86_64.
- Huge numbers (TS-32) depend on the current time on both.
- Outside the parser, the x86_64 glibc build prints unknown values as
  `-nan` where macOS and Linux aarch64 print `nan`; the `fetch` and `def`
  vectors record this per platform.
- A replacement that converts local time with the host C library gets the
  platform-specific results for free; one with its own time zone code must
  choose a platform and will diverge on the other for these vectors.

## Known divergences

`KNOWN_DIVERGENCES` in `crates/rondi-cli/tests/time_spec_vectors.rs` lists
the vectors where Rondi currently differs; their frozen expectation stays
RRDtool's, and a listed vector that starts matching fails the test so the
entry is removed.

- `ref-139`, `ref-140` (TS-61): `graphv --start 315360002-2s --end
  start+1` is accepted by RRDtool with `graph_start = 315360000`. Rondi
  fails with `ERROR: start time: did you really mean month 315360000?`, as
  if the resolved value were parsed again as a lone number (TS-28, rule 5).
  A start of 315360001 or more, or one below 315360000, behaves the same in
  both.

Two findings while capturing are not time-parsing divergences and are
avoided by the vectors rather than listed:

- Rondi's graph refuses ranges that would read more than ten million rows
  (`RRD fetch range exceeds the 10 million row safety limit`), where
  RRDtool reads them. A one-year range over a one-second file hits it, so
  the `end-1y` vectors read the hourly fixture.
- The `@@` echo of TS-9 and the year-30000 rule of TS-26 are reproduced by
  Rondi and pinned; they are noted only because they are defects a future
  change may decide not to keep.

## Rules by provenance

- From reference-source inspection: none.
- From the manual, confirmed by black-box runs: the reference/offset
  structure, the keyword set, numeric date formats, units and the `m` rule
  of NOTE4, the calendar-versus-exact distinction of NOTE3, case and
  separator handling, `start`/`end` cross-references, and the examples.
  Conflicts with the manual are marked "doc conflict" (TS-14, TS-16,
  TS-20, TS-37, TS-33's year range).
- Everything else is black-box, including every error text, the token
  classification of numbers (TS-19, TS-28), the year table, the year-30000
  defect, offset wrapping, the daylight-saving flag of each reference kind,
  gap and overlap resolution, and the `update` and `dump` rules. Rules that
  depend on the current time were established with `faketime` and are
  pinned by equivalence vectors.
