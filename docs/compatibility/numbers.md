# Decimal numbers

Status: complete for the readers listed in NM-2. Reference: RRDtool 1.11.0,
black-box only.

RRDtool reads most decimal numbers with one conversion that is not
correctly rounded. This file names that conversion, lists every reader that
uses it and what each reader does around it, and specifies the readers no
other file covers. The conversion itself is specified once, in
[`restore.md`](restore.md) RS-38 and RS-39; this file does not restate it.

## 1. The conversion

**NM-1** (black-box; must). *The decimal conversion* is `restore.md` RS-38
with the multiply-add of RS-39: fused on arm64 and aarch64, separate on
x86_64. Consequences that the vectors pin:

- The digit accumulator counts every fraction digit, leading zeros
  included, so `0.` followed by 1020 zeros and `1` is 0 while `0.` followed
  by 1021 zeros and `1` fails the exponent check (the exponent is −1022).
  `10e-1022` fails although its value is in range; `0.1e-1020` is accepted.
- The accumulator is a double, so long digit strings round as they are
  read and can reach infinity: `1` followed by 400 zeros is infinity, and
  `1234567890123456789012345678901234567` differs in the last bits between
  arm64 and x86_64.
- Scaling divides or multiplies by powers of ten that are themselves
  rounded and can overflow: `0e600` is NaN, `0e400` is 0 and `1e-400` is
  0. The largest double, `1.7976931348623157e308`, reads as infinity;
  `2.2250738585072014e-308` reads as 2.2250738585072004e-308; `71.037`
  reads as 71.03699999999999; 33 of the 40 seeded 17-digit literals differ
  from the correctly rounded value. `0.1`, `5e-324` and `1e-320` are
  correctly rounded.

A model built only from RS-38 and RS-39 reproduces every value and every
accept or reject decision of the 179 conversion cases of the `numbers`
vectors (all but the time cases), with fused arithmetic against the macOS
recording and separate arithmetic against the Linux x86_64 overrides.

## 2. Readers

**NM-2** (black-box; must). These readers use the conversion. They differ
in the special spellings they recognize, in whether leading white space is
skipped, and in what they do with text after the number.

| Reader | Specified in |
|---|---|
| `update` readings of `GAUGE`, `ABSOLUTE`, `DCOUNTER`, `DDERIVE` | `update.md` UP-10, UP-11, UP-17 |
| `update` time text, `:` form | `update.md` UP-8; NM-4 |
| `COUNTER` and `DERIVE` differences | `update.md` UP-12, UP-16; NM-5, NM-7 |
| `create` DS minimum and maximum | `create.md` CR-20, CR-23, CR-25 |
| `create` RRA `xff` and Holt-Winters parameters | `create.md` CR-23, CR-24 |
| `restore` doubles | `restore.md` RS-37 to RS-39a |
| `tune --minimum`, `--maximum` | NM-6 |
| `tune` Holt-Winters options | `holt-winters.md` HW-31, HW-32 |

Integers are read by other procedures: `restore.md` RS-34 to RS-36,
durations in `create.md` CR-14, `tune-modify.md` TM-11 and TM-12, and
`holt-winters.md` HW-33. `graph` and `xport` arguments use the C library's
`strtod`, which is correctly rounded (`graph.md`, `rpn.md`).

**NM-3** (black-box; must). The special spellings, summarized from the
rules cited in NM-2:

- `update` readings and `create` fields: `nan`, `-nan`, `inf` and `-inf`
  in any letter case at the start of the text, whatever follows; `+inf`
  and `+nan` are not recognized and fail.
- `restore`: `nan`, `-inf` and `inf` anywhere in the token, in that order
  of precedence.
- `tune --minimum` and `--maximum`: none (NM-6).

## 3. Times and counters

**NM-4** (black-box; must). An `update` time in the `:` form is converted
by NM-1 and then split as `update.md` UP-8 states:
*s* = floor(*t*), *µ* = trunc((*t* − *s*) × 1000000.0). Because *t* is a
double near 10⁹, only about seven fraction digits survive, and NM-1's
rounding shows in the result:

| Time text | Stored *s*.*µ* |
|---|---|
| `1000000003.4` | 1000000003.399999 |
| `1000000002.9999999` | 1000000003.000000 |
| `1000000001.0000005` | 1000000001.000000 |
| `1000000001.9999995` | 1000000001.999999 |
| `1000000001.123456789` | 1000000001.123456 |
| `100000000.15e1` | 1000000001.500000 |
| `1000000001.0000000000000000001` | 1000000000.999999 |

The last row reads as slightly less than 1000000001 because its 29 digits
are accumulated before the division. Text after the number fails with
`<file>: error while parsing time in get_time_from_reading - Converted
'<time>' to <value>, but cannot convert '<rest>'` (`1000000001x`, and
`1000000001 ` with a trailing blank).

**NM-5** (black-box; must). The difference of two `COUNTER` or `DERIVE`
readings is formed exactly as decimal text and then converted by NM-1, as
`update.md` UP-16 specifies, with its 2³² − 1 and 2⁶⁴ wrap constants. The
`update` vectors pin it; NM-7 settles one case UP-16 leaves open.

**NM-7** (black-box; must). When both `DERIVE` texts are negative, the
difference has the sign of the true difference reading − stored text:
`-3` after `-5` gives 2, `-5` after `-3` gives −2, `-007` after `-0012`
gives 5, and `-0` after `-7` gives 7. Only two negative texts of equal
magnitude produce the `-0` text of UP-16. Leading zeros never change the
value, and digit strings longer than 20 digits are subtracted exactly
before the conversion: `-1` after `-123456789012345678901234567` gives
1.2345678901e+26 (RRDtool 1.11.0 on macOS arm64, step-1 `DERIVE`, two
updates, stored value read with `rrdtool dump`).

## 4. `tune --minimum` and `--maximum`

**NM-6** (black-box; must). The value of `-i` (`--minimum`) and `-a`
(`--maximum`) is `<ds>:<bound>`. The bound is read as follows:

1. Take the longest prefix of the text after the `:` made of the
   characters `0` to `9`, `.`, `e`, `+`, `-` and `U`. `E` and white space
   are not among them. Keep at most its first 40 characters. The rest of
   the text is ignored.
2. If the prefix is empty, fail.
3. If the prefix is exactly `U`, the bound is unknown, stored as the
   platform's NaN (`create.md` CR-74).
4. Otherwise convert the whole prefix by NM-1, without skipping white
   space and without special spellings. A failure at RS-38 step 3, 6 or 9
   fails. A NaN result (`0e600`) is stored as in step 3.

A failure reads `invalid arguments for minimum ds value` or
`invalid arguments for maximum ds value`. Examples:

- Accepted: `5x` and `5u` are 5, `0x10` is 0, `5E5` is 5, `5e5x` is
  500000, `+.5e+2x` is 50, `5e+` is 5, `-0` is negative zero, `1e1024` is
  infinity, and `Ux`, `U `, `UNKN` and `UE` are unknown.
- Failing: `U5`, `UU`, `Ue`, `U-`, `U.`, `U+`, `u`, `5U`, `5+`, `5-3`,
  `5.5.5`, ` 5`, the empty bound, `nan`, `inf`, `-inf`, `e5`, `.`,
  `1e-1022` and `1e1025`.
- Cut to 40 characters: `0.` followed by 37 zeros and `1` is 1e-38, but
  with 38 zeros it is 0, and 45 ones read as their first 40.

Rondi's `tune` used to convert this bound with a separate routine that
always used separate arithmetic, so long digit strings differed on arm64.
It now uses the conversion that `update` and `create` use.

## Vectors

`scripts/gen-create-resize-vectors.py` writes the `numbers` suite to
`crates/rondi-cli/tests/data/numbers/` (196 cases on one input file), and
`crates/rondi-cli/tests/numbers_spec_vectors.rs` replays it. It holds the
following cases:

- 130 `update` readings: special spellings, syntax forms and failures,
  exponent limits, rounding cases, and 40 seeded 17-digit literals with
  exponents from −330 to 308.
- 15 `update` times.
- 51 `tune -i` and `-a` bounds.

18 cases have Linux x86_64 overrides: rounding cases for the multiply-add,
and the NaN sign of unknown bounds. Linux aarch64 has none.
