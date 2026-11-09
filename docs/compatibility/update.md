# `rrdtool update` and `updatev`

This specification describes what RRDtool 1.11.0 does when `rrdtool update`
or `rrdtool updatev` feeds readings into an existing RRD file: the command
line and the order of its checks, the reading syntax per data source type
with its error texts, how a reading becomes an amount, how amounts are
apportioned into primary data points (PDPs) across step boundaries, how
PDPs are consolidated into archive rows, which bytes of the file change, and
what `updatev` prints. It is written so that `update` can be implemented
from it and from the vectors in `crates/rondi-cli/tests/data/update`
without reading RRDtool's or Rondi's update code.

Every rule carries a provenance tag:

- **documentation**: stated in `rrdupdate(1)`, `rrdcreate(1)`,
  `rrd_pdpcalc(7)` or the command's usage text.
- **black-box**: observed by running `/opt/homebrew/bin/rrdtool` 1.11.0
  (macOS arm64) and the pinned 1.11.0 build on Linux aarch64 and x86_64
  (Debian containers) against chosen inputs, inspecting the files with
  `info`, `dump` and their raw bytes, and pinned by the vectors the rule
  names.
- **reference-source-inspection**: none. No rule in this document needed the
  reference source.

Each rule is also classed as **must** (a replacement reproduces it exactly,
byte for byte where it concerns file content), **should** (expected for
compatibility but allowed to differ in the way the rule states), or
**defect** (a reference bug or a conflict with the documentation; the rule
says what a replacement does, which so far is always to reproduce it,
because files written by both implementations must agree).

The recipes below were checked with a black-box model, a script outside
the repository that implements sections 2 to 9 and was never derived from
RRDtool or Rondi code. It reproduces all 132 recorded vectors it can replay
(the others need `@`, `N`, `COMPUTE`, the usage text or a missing file),
both the macOS arm64 recording and, with UP-35's x86_64 arithmetic, the
Linux x86_64 recording. It also agrees with RRDtool on several thousand
further random update sequences over all six non-computed types.

Out of scope: Holt-Winters archives (Rondi refuses them at create time,
`create.md` CR-80), `--daemon` and the cache daemon (`rrdcached.md`), and
`--locking` and file locking (`cli.md` CLI-12 and CLI-38).

## Terms

- *S* is the file's step in seconds. Data sources (DSs) and archives are
  numbered in file order from 0.
- A *time* is a pair (*s*, *µ*) of whole seconds and microseconds,
  0 ≤ *µ* < 1000000. Times compare as pairs. The file's *last update* is
  (*s0*, *µ0*); the time of the argument being applied is (*s1*, *µ1*).
- A *reading* is the text given for one DS in one argument. The *stored
  text* of a DS is the reading text kept in the file (UP-13); a new file
  stores `U` (`create.md` CR-42).
- An *amount* is the per-DS quantity an argument contributes (UP-15 to
  UP-18); it is a double or *unknown*.
- *Unknown* is a NaN. Files hold the platform's default quiet NaN:
  `0x7ff8000000000000` on arm64 and aarch64, `0xfff8000000000000` on x86_64
  (`create.md` CR-74). Any NaN amount or value, including one read from the
  text `nan`, is treated as unknown.
- The *PDP state* of a DS (`create.md` CR-42) is its stored text, its
  *unknown seconds* *u* (an unsigned integer) and its *PDP value* *v* (a
  double, possibly unknown).
- The *CDP state* of an archive and DS (`create.md` CR-43, CR-45) holds the
  *accumulator* *cv* (word 0, a double), the *unknown-PDP count* *cu*
  (word 1, an integer) and two further doubles RRDtool maintains, *word 8*
  and *word 9* (bytes 64 and 72 of the 80-byte state). An archive has a
  consolidation function (CF), *p* PDPs per row, *rows* rows and xff *x*.
  Its *slot length* is *L* = *p*·*S*.
- *fma(a, b, c)* is *a*·*b* + *c* computed with a single rounding;
  UP-35 says where the reference computes it with two roundings instead.
  All other arithmetic is IEEE binary64 with round-to-nearest, evaluated
  in exactly the order written, one rounding per operation. *trunc* rounds
  toward zero.
- Error output is `ERROR: <message>` and a newline on standard error,
  exit status 1. Messages prefixed with `<file>: ` carry the file name as
  given on the command line. The rules give only `<message>`.

## 1. Command line

**UP-1** (documentation, black-box; must). The synopsis is
`rrdtool update|updatev filename [--template|-t ds-name[:ds-name]...]
[--skip-past-updates|-s] [--daemon|-d address] [--locking|-L]
time:value[:value...]|at-time@value[:value...] ...`. With no word after the
command, the command's usage text goes to standard output and the exit
status is 0 (its banner's build date is not part of the contract, as in
`create.md` CR-2). With a file name but no argument, also when only
options follow, the result is `Not enough arguments` (vectors `arg-usage`,
`arg-usage-v`, `arg-none`, `arg-none-v`, `arg-options-only`).

**UP-2** (black-box; must). Options follow `cli.md` CLI-12 and may appear
before, between or after arguments (`1000000010:1:2:3 -t b 1000000020:4`
applies the template to both arguments). `--template=cc` is the long form
with an attached value. When `-t` is given more than once the last one is
used (vectors `arg-options-only`, `arg-long-options`, `arg-option-after`,
`tpl-last-wins`).

**UP-3** (black-box; must). Checks run in this order: the argument count
(UP-1); opening the file, failing with `opening '<file>': No such file or
directory` (no file prefix) or the header errors of other commands
(`reached EOF while loading header rrd->stat_head` for a short non-RRD
file); the template (UP-4); then each argument in turn (UP-6 to UP-18). For
`updatev`, every failure after the argument count is preceded by
`return_value = -1` on standard output (vectors `arg-missing`,
`arg-missing-v`, `arg-not-rrd`, `tpl-after-open`).

## 2. Templates

**UP-4** (black-box; must). The template is split at every `:` into names.
Names are checked in order; for the name at index *k*: if *k* is at least
the number of DSs, the result is `tmplt contains more DS definitions than
RRD`; otherwise if no DS has that name, the result is `unknown DS name
'<name>'`. Both messages have no file prefix. An empty name (`-t ""`,
`a:`, `:a`) is unknown. So with DSs `a` and `b`, `zz:a:b:a` reports `zz`,
while `a:b:zz` and `a:b:a:zz` report the count (vectors `tpl-unknown`,
`tpl-empty`, `tpl-trailing-colon`, `tpl-too-many`, `tpl-unknown-first`,
`tpl-count-before-name`, `tpl-count-mismatch`, `tpl-after-open`,
`tpl-updatev-error`, `tpl-updatev`, `tpl-one`, `tpl-reorder`,
`tpl-last-wins`).

**UP-5** (black-box; must). With a template of *k* names, each argument
must carry exactly *k* readings (UP-7). Reading *j* goes to the DS named by
name *j*; a DS named twice takes the later reading; a DS not named gets the
reading `U`. Without a template, reading *j* goes to DS *j*. The readings
are then processed in DS order, not template order (vectors `tpl-one`,
`tpl-reorder`, `tpl-duplicate`).

## 3. Arguments

**UP-6** (black-box; must). If the argument contains `@`, it is split at
its first `@`; otherwise it is split at its first `:`. The part before is
the *time text*, the part after the *value text*. An argument with neither
character fails with `<file>: expected timestamp not found in data source
from <argument>`. So `1000000014:5@6` has the time text `1000000014:5` and
the value text `6` (vectors `split-no-separator`, `split-at`,
`split-at-extra-at`, `split-at-wins`).

**UP-7** (black-box; must). The value text is split at every `:` into
readings (an empty value text is one empty reading). With *k* readings
expected (the number of DSs, or the template length): fewer give
`<file>: expected <k> data source readings (got <m>) from <time text>`;
more give `<file>: found extra data on update argument: <rest>`, where
*rest* is the value text after the *k*-th reading and its `:` (`3:4` for
`1000000005:1:2:3:4` with two DSs; `:` for `1000000005:1:2::`; empty for
`1000000018:U:U:`). This check runs before the time is parsed
(`abc:5` reports the count) (vectors `count-few`, `count-many`,
`count-empty-values`, `count-before-order`, `tpl-count-mismatch`,
`split-at-extra-at`, `split-at-wins`, `skip-past-bad-count`,
`multi-arg-fail-count`).

**UP-8** (documentation, black-box; must). The time text is parsed as
`time-spec.md` TS-67 to TS-70 state (`N`, the `:` form's number with its
error texts, the `@` form's at-style time with `ds time:` errors). For the
`:` form the number *t* is read with the decimal conversion of `restore.md`
RS-38 (with UP-35's platform arithmetic), and the time is
*s* = floor(*t*), *µ* = trunc((*t* − *s*) × 1000000.0): `1000000003.4`
gives *µ* = 399999 and `1000000002.9999999` gives *µ* = 999999. The `@`
form always has *µ* = 0. `N` is the current time including microseconds,
so `N:5 N:6` in one command normally succeeds twice (vectors
`time-parse`, `time-before-value`, `time-microseconds`, `split-at`,
`split-at-fraction`, `skip-past-bad-time`, `now-ok`, `now-twice`,
`now-before-last`, `now-at`).

**UP-9** (black-box; must). If (*s1*, *µ1*) ≤ (*s0*, *µ0*), the argument
fails with `<file>: illegal attempt to update using time <s1> when last
update time is <s0> (minimum one second step)`, whole seconds only, before
any reading is examined. So 4.7 after 4.5 is accepted while 4.6 after 4.7
fails with both times printed as 1000000004, and an update at the file's
start time fails. With `--skip-past-updates`, such an argument is skipped
silently and changes nothing, even if its readings are invalid; the count
check (UP-7) and time parsing (UP-8) still fail (vectors `order-equal`,
`order-before`, `order-start`, `order-fraction`, `count-before-order`,
`time-before-value`, `skip-past`, `skip-past-bad-value`,
`skip-past-bad-count`, `skip-past-bad-time`, `skip-past-v`, `now-twice`,
`now-before-last`, `multi-arg-fail-order`).

## 4. Readings

The *interval* *i* of an argument is defined in UP-14. Each DS's reading is
processed in DS order by UP-10 to UP-18, which yield the DS's amount *a*;
UP-13 then stores its text.

**UP-10** (black-box; must). If *i* is greater than the DS's heartbeat, or
the reading's first character is `U` (`U`, `Uxyz`, `Unknown`, `U-1`), the
amount is unknown and the reading is not examined further: no syntax check
runs, whatever the type (`zz` passes for every type when the heartbeat is
exceeded). An interval equal to the heartbeat is accepted (vectors
`val-unknown-prefix`, `val-heartbeat-no-parse`, `hb-equal`,
`hb-long-gap`).

**UP-11** (black-box; must). For `GAUGE`, `ABSOLUTE`, `DCOUNTER` and
`DDERIVE` a reading is converted as follows. If it starts, ignoring case,
with `nan` or `-nan`, the value is NaN; with `inf` or `-inf`, the value is
positive or negative infinity; the rest of the text is ignored (`nanx`,
`infinity`, `Inf5`, `-INFx`). Otherwise leading blanks are skipped and the
text is converted by `restore.md` RS-38. Failures read
`<file>: Function update_pdp_prep, case DST_<TYPE> - Cannot convert
'<reading>' to float` when no number starts the text (`+inf`, `e5`, `.`,
`-`, `+`, the empty reading, `unknown`, `xnan`), and `<file>: Function
update_pdp_prep, case DST_<TYPE> - Converted '<reading>' to <value>, but
cannot convert '<rest>'` when text follows the number, where *value* is
printed with six decimals (`0.000000` for `0x10`, `12.000000` for
`12abc`). `1e`, `1e+`, `.5`, `5.`, `+5`, ` 5` and `1e400` (infinity) are
accepted; `1e-400` is 0 and `0e600` is NaN, as RS-38 states (vectors
`val-gauge-forms`, `val-gauge-specials`, `val-gauge-nan-text`,
`val-gauge-reject`, `val-absolute-reject`, `count-empty-values`,
`time-before-value`, `skip-past-bad-value`).

**UP-12** (black-box; must). A `COUNTER` reading must consist of decimal
digits only and a `DERIVE` reading of an optional `-` followed by decimal
digits only; the empty reading, and `-` for `DERIVE`, are accepted.
Anything else fails with `<file>: not a simple unsigned integer:
'<reading>'` (`COUNTER`: `1.5`, `-5`, `+5`, `1e3`, `0x10`, `-0`, `12abc`,
`inf`, `u`) or `<file>: not a simple signed integer: '<reading>'`
(`DERIVE`: `+5`, `-+5`, `--5`, `1e3`, `1.0`) (vectors `val-counter-reject`,
`val-derive-reject`, `val-integer-accept`).

**UP-13** (black-box; must). After a DS's amount has been determined
without error, its stored text becomes the first 29 bytes of the reading,
NUL-padded to 30 bytes (the remaining two bytes of the field stay zero).
This happens for every DS whose reading was processed, including `U`
readings and readings skipped by UP-10. When a later DS of the same
argument fails, the earlier DSs keep their new stored text although the
argument fails (UP-34) (vectors `val-gauge-forms`, `val-unknown-prefix`,
`val-last-ds-truncate`, `val-last-ds-shorter`, `val-partial-last-ds`,
`val-partial-counter`, `rate-counter-long`, `rate-counter-zeros`).

## 5. Amounts

**UP-14** (black-box; must). The interval is
*i* = double(*s1* − *s0*) + double(*µ1* − *µ0*) / 1000000.0: the whole
seconds difference and the microseconds difference are each converted
exactly, the second is divided by 10⁶, and the two are added. Adding the
converted fractional times instead changes the last bits
(vectors `fp-interval`, `time-microseconds`, `order-fraction`,
`rate-gauge`, `rate-dcounter-fraction`, `one-step-fraction`).

**UP-15** (black-box; must). For `GAUGE` the amount is *x* × *i*, where *x*
is the converted reading. For `ABSOLUTE` it is *x* itself (vectors
`rate-gauge`, `rate-absolute`, `rate-all-types`, `val-gauge-specials`).

**UP-16** (black-box; must, defect). For `COUNTER` and `DERIVE` the amount
is the difference between the reading and the stored text, computed as
follows. The difference is unknown when the reading is longer than 30
characters, when either text is not an optional `-` followed by at least
one digit (an empty text, `-`, or a stored `U`), or when exactly one of the
two texts starts with `-`. Otherwise the exact integer difference
reading − stored text is formed (leading zeros allowed, any length; the
stored text is the 29-byte prefix of UP-13, so a 30-digit previous reading
is cut by one digit). For two negative texts it is written as `-` followed
by the decimal digits of |reading| − |stored|, so `-5` after `-5` gives the
text `-0`; otherwise it is written as a plain decimal integer with a
leading `-` when negative. That text is converted by RS-38, so differences
beyond 2⁵³ round as RS-38 rounds (`12345678901234567890123` gives
1.234567890123457e22, not 1.2345678901234568e22), and `-0` gives −0. For
`COUNTER`, if the result *d* is negative, *d* becomes *d* + 4294967295.0,
and if that is still negative, it then becomes *d* + 18446744069414584320.0
(each a double addition). The first constant is 2³² − 1, so `5` after
`4294967296` gives 4 and `0` after `4294967295` gives 0, and the second
step works in doubles, so `5` after `18446744073709551615` gives 0 (defect:
the documentation describes adding the counter's range; a replacement
reproduces these values) (vectors `rate-counter-basic`,
`rate-counter-wrap32`, `rate-counter-wrap-edges`, `rate-counter-wrap64`,
`rate-counter-big`, `rate-counter-long`, `rate-counter-zeros`,
`rate-counter-empty`, `rate-counter-range`, `rate-derive`,
`rate-derive-signs`, `rate-derive-zero`, `val-integer-accept`,
`rate-all-types`).

**UP-17** (black-box; must, defect). For `DCOUNTER` and `DDERIVE`, if the
stored text starts with `U`, the amount is unknown and the reading is not
converted (so `12abc` is accepted and stored). Otherwise the reading is
converted (UP-11), then the stored text is converted the same way, and the
amount is reading − stored. A conversion failure of the stored text reports
the stored text (`Converted '12abc' to 12.000000, but cannot convert
'abc'`), so such a DS fails every later update until a `U` reading or an
exceeded heartbeat replaces the text (defect; reproduce). For `DCOUNTER` a
negative amount is unknown: every decrease is unknown and there is no
direction detection or reset, although `rrdcreate(1)` describes a
downward-counting mode (documentation conflict; reproduce the reference)
(vectors `val-dcounter-after-unknown`, `val-dcounter-new-first`,
`val-dcounter-bad-new`, `rate-dcounter`, `rate-dcounter-down`,
`rate-dcounter-fraction`, `rate-all-types`).

**UP-18** (black-box; must). When the amount *a* is known, the rate
*a* / *i* is compared with the DS's minimum and maximum (bounds stored as
NaN are not checked): a rate below the minimum or above the maximum makes
the amount unknown. For `ABSOLUTE` the rate depends on the interval, so
the same reading passes or fails with the interval length (vectors
`range-gauge`, `range-absolute-interval`, `rate-counter-range`).

## 6. Primary data points

Let *proc* = *s0* − (*s0* mod *S*) and *occu* = *s1* − (*s1* mod *S*), the
starts of the steps holding the last update and the new time, and
*n* = (*occu* − *proc*) / *S*, the number of step boundaries crossed.
UP-19 to UP-25 apply per DS, with that DS's *u*, *v*, heartbeat and amount
*a*.

**UP-19** (black-box; must). If *n* = 0: when *a* is unknown, *u* becomes
*u* + trunc(*i*); otherwise *v* becomes *a* when *v* is unknown and
*v* + *a* otherwise (so `-0` into an unknown value stores −0) (vectors
`rate-gauge`, `rate-absolute`, `acc-unknown`, `acc-first-known`,
`zero-aligned`, `zero-split`).

**UP-20** (black-box; must). If *n* ≥ 1, define

- *pre* = double(*proc* + *S* − *s0*) − double(*µ0*) / 1000000.0, the time
  from the last update to the first boundary;
- *pre1* = floor(*pre*), that is *proc* + *S* − *s0*, minus 1 when *µ0* > 0;
- *post* = double(*s1* − *occu*) + double(*µ1*) / 1000000.0, the time after
  the last boundary;
- *W* = double(*occu* − *s0*) − double(*µ0*) / 1000000.0, the time from the
  last update to the last boundary.

The update is *split* when *n* ≥ 2, *pre1* ≥ 1, and not both
*s0* mod *S* = 0 and *µ0* = 0. Otherwise it is *whole* (vectors
`one-step`, `one-step-fraction`, `multi-aligned`, `multi-split`,
`multi-pre-under-one`).

**UP-21** (black-box; must). A whole update yields one value *V* for all
*n* PDPs. Let *b* be +0 when *v* is unknown and *v* otherwise.

- When *a* is known: *r* = *a* / *i*, *vt* = fma(*r*, *W*, *b*),
  *e* = 0.
- When *a* is unknown: *vt* = *v* (possibly unknown), *e* = *W*.

*V* is unknown when 2·*u* > *S* or *i* is greater than the heartbeat.
Otherwise *V* = *vt* / ((double(*n*·*S*) − *u*) − *e*), subtracting *u*
first and *e* second; a zero divisor gives the IEEE result. Subtracting
*u* + *e* at once changes the last bits (vectors `one-step`,
`one-step-unknown`, `one-step-fraction`, `one-step-exact`,
`multi-fraction-one`, `zero-aligned`, `fp-unknown-denominator`).

**UP-22** (black-box; must, defect). A split update yields a first PDP *F*
and *n* − 1 further PDPs of one value *R*. Let *b* be +0 when *v* is
unknown and *v* otherwise.

- When *a* is known: *pp* = (*a* × *pre1*) / *i*;
  *vt* = fma(*pp* / *pre1*, *pre1*, *b*); *ut* = *u*;
  *rem* = *a* − *pp*; *r2* = *rem* / (*i* − *pre1*);
  *R* = (*r2* × (*W* − *pre1*)) / double((*n* − 1)·*S*).
- When *a* is unknown: *vt* = *v* (possibly unknown); *ut* = *u* + *pre1*;
  *R* is unknown.

*F* is unknown when 2·*u* > *S* or *pre1* is greater than the heartbeat;
otherwise *F* = *vt* / (double(*S*) − *ut*). Only the whole seconds *pre1*
go to *F*; when *µ0* > 0 the fraction *pre* − *pre1* is counted twice,
once in *rem* and again because *W* − *pre1* exceeds (*n* − 1)·*S* by it,
so *R* is slightly too large (defect; reproduce). `rrd_pdpcalc(7)`'s worked
example computes the intermediate PDPs differently and does not match
1.11.0 (vectors `multi-split`, `multi-split-unknown`,
`multi-fraction-split`, `multi-start-unknown`, `multi-s7`,
`hb-split-subinterval`, `hb-split-known-part`, `zero-split`, `fp-fused-first`).

**UP-23** (black-box; must). The PDPs an update completes, in time order,
are consolidated (section 7) as *runs* of equal values: for a split update
a run of one *F* starting at *proc*, then a run of *n* − 1 *R* starting at
*proc* + *S*; for a whole update with *n* = 1 a run of one *V* starting at
*proc*; for a whole update with *n* ≥ 2 one run of *n* *V* starting at
*proc*. A PDP *starts* at the beginning of its step (vectors
`multi-aligned`, `multi-pre-under-one`, `multi-s7-aligned`).

**UP-24** (black-box; must). The unknown-seconds limit tests the *u*
stored before the update (2·*u* > *S*, so exactly half the step unknown
still gives a value), not the seconds the update itself adds. The
heartbeat test of a split update uses *pre1*, of a whole update *i*; an
unknown amount with *pre1* within the heartbeat therefore still gives a
known *F* from the earlier known part of the step (vectors `hb-long-gap`,
`hb-split-subinterval`, `hb-split-known-part`, `one-step-unknown`, `one-step-half-unknown`,
`one-step-start-unknown`, `multi-start-unknown`).

**UP-25** (black-box; must). After an update with *n* ≥ 1, the PDP state
starts the step *occu*: when *a* is unknown, *v* is unknown and
*u* = trunc(*post*); otherwise *u* = 0 and *v* = *r* × *post* for a whole
update or *r2* × *post* for a split one (vectors `one-step`,
`one-step-unknown`, `one-step-fraction`, `multi-split-unknown`).

## 7. Consolidation

**UP-26** (black-box; must). *Folding* *k* ≥ 1 copies of a PDP value *z*
into an archive's accumulator: if *z* is unknown, *cu* becomes *cu* + *k*
and *cv* is unchanged. Otherwise, for `AVERAGE`, *cv* becomes *z* × *k* if
*cv* is unknown and fma(*z*, *k*, *cv*) otherwise (one operation for all *k*
copies; adding *z* *k* times differs in the last bits); for `MIN` and
`MAX`, *cv* becomes *z* if *cv* is unknown or *z* is smaller (`MIN`) or
larger (`MAX`) than *cv*, so −0 and +0 keep whichever came first; for
`LAST`, *cv* becomes *z*. The *reset* accumulator is 0.0 for `AVERAGE`,
+∞ for `MIN`, −∞ for `MAX` and unknown for `LAST`, with *cu* = 0 (vectors
`cf-basic`, `cf-bulk`, `multi-s7`, `val-gauge-specials`, `zero-aligned`,
`zero-split`, `fp-fused-average`).

**UP-27** (black-box; must). A row's value is *finished* from the
accumulator after folding: unknown if *cu* > *x*·*p*; otherwise, for
`LAST`, unknown if the PDP folded last was unknown and *cv* otherwise; for
`AVERAGE`, *cv* / double(*p* − *cu*); for `MIN` and `MAX`, *cv* (vectors
`cf-basic`, `cf-last-unknown`, `cf-bulk-unknown`, `xff-limits`,
`xff-bulk`).

**UP-28** (black-box; must). A run of *m* PDPs of value *z* starting at
time *B* is consolidated into each archive in archive order, and within it
for each DS. Let *c* = (*B* / *S*) mod *p*, the run's first position in its
row, and *N* = (*c* + *m*) div *p*, the number of rows the run completes.
If *N* = 0, *m* copies of *z* are folded (UP-26) and nothing else changes.
A run of one PDP that completes a row (*c* = *p* − 1) folds it, writes the
finished value (for *p* = 1, the PDP itself) as one row (UP-30), and sets
word 8 to the written value; for *p* > 1 it also resets the accumulator
and sets word 9 to *z* (vectors `one-step`, `multi-split`, `cf-basic`,
`cdp-alignment`).

**UP-29** (black-box; must). A run with *N* ≥ 1 writes
*K* = min(*N*, *rows*) rows. The first, the *primary*, is the accumulator
with *p* − *c* copies of *z* folded and finished (for *p* = 1, *z*). Each of
the other *K* − 1 rows is *z* itself, whatever the CF and *p*. When *N*
exceeds *rows*, the primary is still written first, followed by
*rows* − 1 copies of *z*. Then, for *p* > 1, the accumulator is reset and
(*c* + *m*) mod *p* copies of *z* are folded into it. Word 8 becomes the
primary for *p* > 1, and the value written last for *p* = 1; word 9
becomes *z* when *p* > 1 or *N* > 1 and is unchanged otherwise (vectors
`multi-aligned`, `multi-split`, `multi-s7`, `multi-s7-aligned`, `cf-bulk`,
`cf-bulk-unknown`, `xff-bulk`, `cdp-alignment`, `rows-capped`,
`rows-capped-split`, `rows-capped-long`).

**UP-30** (black-box; must). Writing a row advances the archive's current
row *r* to (*r* + 1) mod *rows* and stores one value per DS, in DS order,
in that row. Rows are written in the order UP-28 and UP-29 produce them,
so a run longer than the archive overwrites rows it wrote earlier in the
same update (vectors `rows-wrap`, `rows-capped`, `rows-capped-long`).

## 8. `updatev` output

**UP-31** (black-box; must). `updatev` prints `return_value = 0` on
success and `return_value = -1` on any failure after the argument count
(UP-3), then one line per value written, for every argument applied
before a failure as well:
`[<time>]RRA[<cf>][<p>]DS[<name>] = <value>`. The bracket after the CF is
the archive's PDPs per row (`cli.md` CLI-69 calls it rows). The value is
printed as C `%.10e` (`-0.0000000000e+00` for −0,
`1.2345678901e+300`), `NaN` for unknown, `inf` and `-inf`. Lines come per
argument, per run in UP-23 order, per archive in archive order, per row
written and per DS in DS order. A row's time is normally the end of its
slot (UP-32 lists the exceptions). Arguments that write no rows and
arguments skipped by UP-9 print nothing. `update` prints nothing (vectors
`updatev-none`, `updatev-error`, `updatev-formats`, `arg-none-v`,
`arg-missing-v`, `tpl-updatev-error`, `tpl-updatev`, `skip-past-v`,
`val-gauge-specials`, `rate-all-types`, `rows-secondary-times`,
`multi-arg-fail`).

**UP-32** (black-box; defect). The printed times differ from the slot ends
in three cases, which a replacement reproduces in `updatev` output only
(the file is not affected). Let *E* be the end of the slot the run
completes first, (*B* − (*B* mod *L*)) + *L*.

- The primary of UP-29 is printed at *E* + (*N* − *K*)·*L*, so when rows
  are capped the primary (the oldest data) carries a recent time.
- The *j*-th following row (*j* = 1 … *K* − 1) is printed at
  *E* + (*N* − *K* + *j*)·*L* + *L*, one slot late.
- In a split update with *µ0* > 0, a row completed by the first run (*F*)
  is printed at *proc* + *S* − *L*, one slot early.

(vectors `rows-secondary-times`, `rows-capped`, `rows-capped-split`,
`multi-fraction-split`, `multi-fraction-one`).

## 9. The file

**UP-33** (black-box; must). Each applied argument writes the last update
(*s1*, *µ1*) into the 16-byte live head (`create.md` CR-41) and, for each
DS, the stored text (30 bytes), *u* (bytes 32 to 39) and *v* (bytes 40 to
47) of the PDP state; for each archive and DS, the CDP words 0, 1, 8 and 9
where section 7 changes them; the current-row words; and the rows written.
No other byte changes, and an argument that fails or is skipped changes
nothing except UP-13's stored texts. The layout is the same for version
`0003` and `0005` files (`create.md` CR-37; the `types` seed is `0005`).
Version `0004` files need Holt-Winters archives, which are out of scope,
and version `0001` files cannot be produced by RRDtool 1.11.0
(`restore` rewrites the version), so neither is pinned (vectors
`time-microseconds`, `rate-all-types`, and every successful case).

**UP-34** (black-box; must, defect). Arguments are applied one after the
other, each completely before the next is checked. When one fails, the
command stops with its error and exit status 1, the arguments before it
remain applied (and printed by `updatev`), and within the failing argument
the DSs before the failing reading keep their new stored text (UP-13)
while nothing else of that argument is written (vectors `multi-arg-fail`,
`multi-arg-fail-order`, `multi-arg-fail-count`, `multi-arg-ok`,
`val-partial-last-ds`, `val-partial-counter`).

## 10. Platforms

**UP-35** (black-box; must, platform specific). On macOS arm64 and Linux
aarch64, every fma in this document and the multiply-add of `restore.md`
RS-38's digit loop (used by UP-8, UP-11, UP-16 and UP-17) use one rounding;
the two platforms gave identical results in every vector. On Linux x86_64,
each of them is a rounded multiplication followed by a rounded addition:
*fma(a, b, c)* is (*a* × *b*) + *c*, and UP-26's `AVERAGE` fold is
*cv* + (*z* × *k*). The x86_64 results differ in the last bits;
`expected/update.linux-x86_64.vectors` holds the cases that differ there,
and the independent model reproduces them with this arithmetic. Unknowns
are written as each platform's NaN pattern (Terms); the tests fold the
x86_64 pattern into the arm64 one. `updatev` prints `NaN` on every
platform. The fp-* vectors were chosen so that the alternative orders
named in UP-14, UP-21, UP-22 and UP-26 give different bytes (vectors
`fp-interval`, `fp-fused-first`, `fp-fused-average`,
`fp-unknown-denominator`, `rnd-00` to `rnd-15`).

## 11. Computed sources

**UP-36** (black-box; must). `COMPUTE` data sources follow `compute.md`
CP-12 to CP-16, which are normative for update, including CP-16's
ordering when an evaluation fails. As observed here: readings are given
only for the non-computed DSs (a file with `a` and two computed sources
expects one reading), and a template may name a computed source (vectors
`compute-basic`, `compute-template`).

## 12. Floating-point order summary

The operations whose order or rounding changes the result, and the vectors
that fail when they are done differently:

| Rule | Operation | Vector |
|---|---|---|
| UP-14 | interval from integer differences | `fp-interval` |
| UP-21 | (*n*·*S* − *u*) − *e*, not *n*·*S* − (*u* + *e*) | `fp-unknown-denominator` |
| UP-22 | fma(*pp*/*pre1*, *pre1*, *b*), not *pp* + *b* or a separate multiply-add | `fp-fused-first` |
| UP-26 | fma(*z*, *k*, *cv*), not *k* additions | `fp-fused-average` |
| UP-35 | fused on arm64 and aarch64, unfused on x86_64 | x86_64 overrides |

## Summary by class

Must reproduce exactly: UP-1 to UP-15, UP-18 to UP-21, UP-23 to UP-31,
UP-33, UP-35 and UP-36, and the values of UP-16, UP-17, UP-22 and UP-34.

Should reproduce: none beyond the usage banner's build date (UP-1).

Reference defects that a replacement reproduces: UP-16 (the 2³² − 1 wrap
constant and the second wrap in doubles), UP-17 (stuck stored texts, no
`DCOUNTER` direction), UP-22 (the double-counted fraction), UP-32 (printed
times) and UP-34 (partial stored texts).

## Behavior not pinned

- Version `0001` and `0004` files (UP-33).
- Wall-clock results of `N`: the `now-*` vectors compare masked output only
  and no file.
- Readings longer than the argument limits of the operating system, and
  files damaged between updates.
- Consolidation with an `xff` stored as NaN (`create.md` CR-77) is not
  specified and no vector pins it.

## Known divergences

Current Rondi against RRDtool 1.11.0, as of this specification:

- UP-36 (`compute-basic`, `compute-template`): Rondi expects a reading for
  every DS including `COMPUTE` ones, so updates fail with UP-7's count
  error, and it refuses templates and multi-reading updates on such files
  with `unsupported RRD operation: in-place update requires one value per
  supported data source and basic archives (AVERAGE, MIN, MAX, LAST)`.

## Tests

- `crates/rondi-cli/tests/update_spec_vectors.rs` replays every case
  against Rondi under the `rrdtool` name, runs them live against the pinned
  RRDtool when it is on `PATH` (always under `RONDI_REQUIRE_ORACLE=1`),
  checks that RRDtool reproduces the recording and that Rondi matches the
  live result, and checks that every UP rule has a vector. Its
  `KNOWN_DIVERGENCES` list holds the divergent cases with their rule; an
  entry that starts matching fails the test.
- `crates/rondi-cli/tests/data/update/cases.json` lists the seeds (their
  create commands) and the cases: id, UP rules, expectation (`same`, or
  `now` for wall-clock cases), seed and steps.
- `inputs/*.rrd` are the seeds, created once by RRDtool 1.11.0 on macOS
  and frozen because `create` picks random current rows.
- `expected/update.vectors` holds, per case and step, the exit status,
  standard output (usage banner build date removed) and standard error,
  then `file` with the final bytes of `t.rrd` or `same` when unchanged,
  framed as `@@ <id> <section> <length>`, the bytes and a newline.
  `expected/update.linux-x86_64.vectors` replaces the cases that differ on
  Linux x86_64 (UP-35); Linux aarch64 needs none.
- `scripts/gen-update-vectors.py` regenerates the vectors; run without
  arguments it reuses the seeds and reproduces the committed files exactly.
  `--linux-overrides` writes the per-architecture overrides inside a Linux
  container with the pinned RRDtool.
