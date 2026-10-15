# `rrdtool restore`

This specification describes what RRDtool 1.11.0 does when `rrdtool restore`
turns an XML dump into a binary RRD file: the command line, how the XML is
read and walked, the grammar of every field, the error messages, the file it
writes and when it writes one. It is written so that the restore path can be
implemented from it and from the vectors in `crates/rondi-cli/tests/data/restore`
without reading RRDtool's source.

Every rule carries a provenance tag:

- **documentation**: stated in `rrdrestore(1)` or `rrddump(1)`, or visible in
  the XML that `rrdtool dump` writes.
- **black-box**: observed by running `/opt/homebrew/bin/rrdtool` 1.11.0
  (macOS arm64, Apple libxml2) and the pinned 1.11.0 build on Linux aarch64
  and x86_64 (Debian, libxml2 2.9.14) against frozen inputs, and pinned by
  the vectors.
- **reference-source-inspection**: the outputs were known from the vectors,
  but the rule that produces them could not reasonably be recovered from
  outputs alone, so the reference source was read to state the rule. These
  rules are written here in neutral terms and every one is pinned by
  vectors. They are listed in [the provenance summary](#provenance-summary).

Each rule is also classed as **must** (a replacement reproduces it exactly,
byte for byte where it concerns file content), **should** (expected for
compatibility but allowed to differ in the way the rule states, usually
because the behavior belongs to the XML library or the C library rather than
to RRDtool), or **defect** (a reference bug or undefined behavior; the rule
says what a replacement does instead, or that the vectors pin the reference
behavior for now).

## Terms

- *Event stream*: the sequence of nodes an XML pull parser reports for the
  document: start tags, end tags, text, CDATA sections, comments, processing
  instructions and whitespace-only text. A self-closing tag `<a/>` is
  reported as a start tag with no matching end tag.
- *Message*: the single error text the command keeps while it runs. Setting
  a message replaces any earlier one. It is printed at the end as
  `ERROR: <message>`.
- *Token*: the first run of non-whitespace characters of a text node, where
  whitespace is space, tab, carriage return and line feed (the only C
  `isspace` characters XML text can carry). A text node with no such run
  gives the empty token.
- *Parser line*: the line number the XML parser has reached when a message is
  set (section 5). It appears in messages as `line N` or `ling N`.
- *Normal form* of a file: each archive's rows rotated so the oldest row is
  first, every row pointer set to zero and every 8-byte word equal to
  `0xfff8000000000000` replaced by `0x7ff8000000000000` (RS-60).

## 1. Command line

**RS-1** (documentation; must). The synopsis is
`rrdtool restore [--range-check|-r] [--force-overwrite|-f] file.xml file.rrd`.
The manual lists the options after the file names; both orders work.

**RS-2** (black-box; must). Options take no argument and may be combined
(`-rf`). `--` ends option processing. Long options must be spelled in full:
`--range` fails with `invalid option -- 'range'`. Unknown options fail with
`invalid option -- 'z'` or `invalid option -- 'bogus'` (the name without
dashes). `-h`, `-?` and `--help` are not help options and fail the same way.
`--range-check=1` fails with `option takes no arguments -- 'range-check'`.

**RS-3** (black-box; must). Unless exactly two operands remain, the command
fails with
`usage rrdtool restore [--range-check|-r] [--force-overwrite|-f] file.xml file.rrd`.

**RS-4** (black-box; should). `rrdtool restore` with no further arguments
prints RRDtool's general banner followed by the restore help text on standard
output and exits 0. The banner contains build details and is not pinned.

**RS-5** (black-box; must). A failure prints `ERROR: <message>` and a newline
on standard error and exits 1. Nothing is written to standard output unless
the output file is `-`.

**RS-6** (reference-source-inspection, confirmed black-box; must). The exit
status depends only on whether a message is set when the command ends, not
on whether the file was written. A message set by a recoverable problem
(section 3) therefore gives exit 1 even though the file was written, and a
failure that sets no message (RS-27) gives exit 0 with no file and no output.

**RS-7** (documentation; should). In pipe mode (`rrdtool -`) the flags of an
earlier `restore` stay set for later restores in the same process, and the
end of a stdin dump can be marked with a line containing ctrl-Z so that more
commands can follow. Not covered by the vectors.

## 2. Input

**RS-8** (documentation, black-box; must). The XML is read from the named
file, or from standard input when the name is `-`. A file that cannot be
opened fails with `Could not create xml reader for: <name>`.

**RS-9** (reference-source-inspection, confirmed black-box; must). Standard
input is read a line at a time. A ctrl-Z byte (0x1a) anywhere in a line ends
the input at that byte; the rest of that line and of the stream is not read.
So a dump followed by `\x1a\n` and arbitrary text restores normally from
standard input (vector `doc-ctrl-z@stdin`), while from a named file the same
bytes are an XML error.

**RS-10** (black-box; must). For a named file, the XML library's own
diagnostics for malformed input (lines such as
`in.xml:6: parser error : Opening and ending tag mismatch: step line 6 and stp`,
the offending source line and a caret line) are printed on standard error
before the `ERROR:` line. Warnings, for example
`parser warning : Unsupported version '1.1'`, are printed the same way and
do not by themselves fail the restore. For standard input these
diagnostics are suppressed. The diagnostic text is library behavior (RS-40).

**RS-11** (black-box; must). The document is parsed as XML 1.0 with these
observable properties:

- A UTF-8 byte order mark is skipped. The XML declaration and the DOCTYPE
  (including the `SYSTEM` and `PUBLIC` forms `rrdtool dump` writes) are
  optional; the external DTD is never fetched. The `--header xsd` and
  `--header none` forms of `rrdtool dump` restore identically.
- Declared encodings are honored: `ISO-8859-1` input is converted to UTF-8
  before it reaches any field, so a Latin-1 `é` in a name is stored as the
  two bytes `c3 a9`. An unknown encoding fails with
  `error reading/parsing XML: Unsupported encoding <name>`. Invalid UTF-8 in
  a UTF-8 document is a syntax error.
- From a named file, UTF-16 with a byte order mark is accepted. From
  standard input the same bytes fail (RS-9 reads lines, which breaks the
  encoding detection).
- Character references (`&#54;`, `&#x36;`) and the predefined entities are
  expanded. An undefined entity is reported as a library error but the
  restore continues without the reference: `<step>&nope;60</step>` gives
  step 60 and exit 0. An entity declared in an internal DTD subset is not
  substituted; the text around it is read as if it were absent.
- Comments and processing instructions are ignored wherever they appear.
  Attributes are ignored. A namespace prefix is part of the element name, so
  `<p:step>` is an unknown element.
- Element names are compared without regard to ASCII case: `<RRD>`, `<STEP>`
  and `<Cf>` are accepted.
- Whitespace after the root element, and comments or processing instructions
  there, are allowed. Anything else after the root element is a syntax
  error (RS-41).

## 3. The element walk

RRDtool walks the event stream with three read operations and a small set of
container rules. Every message text in this section is required for
compatibility (must); the `line N` numbers inside them follow section 5.

### 3.1 Read operations

**RS-12** (reference-source-inspection, confirmed black-box; must). *Read an
element* takes events until one of these happens:

- a start tag: the result is its name;
- an end tag: the result is its name with a leading `/` (for `</ds>` the
  result is `/ds`);
- a text node: the message becomes
  `line N: expected element but found text '<text>'` with the whole text of
  the node, and the read fails;
- the end of the document: the message becomes
  `the xml ended while we were looking for an element` and the read fails;
- an XML syntax error: the message becomes
  `error reading/parsing XML: <library message>` (RS-40) and the read fails.

Comments, processing instructions, CDATA sections and whitespace-only text
are skipped.

**RS-13** (reference-source-inspection, confirmed black-box; must). *Expect
element E* reads an element and fails unless the result equals E (ignoring
case). On a mismatch the message is
`line N: expected <F> element but found <E>`, where F is what was found and
E what was expected. The two names are swapped relative to their meaning;
this is the observable text. Examples: a root `<x>` gives
`line 1: expected <x> element but found <rrd>`; a row with no value gives
`line N: expected </row> element but found <v>`.

**RS-14** (reference-source-inspection, confirmed black-box; must). *Expect
the end of E* succeeds at once if the current event is already an end tag
named E. Otherwise it reads an element and fails unless the result is `/E`.
On a mismatch the message is `line N: expected </E> end element but found
<F>`, in natural order, with F as returned by RS-12 (for example `<x>` or
`</params>`).

**RS-15** (reference-source-inspection, confirmed black-box; must). *Read a
value* takes events after a start tag:

- a start tag: the message becomes
  `line N: expected a value but found a <name> element` and the read fails;
- an end tag (any name): the value is the empty token;
- a text node: the value is its token (so `" 7 "` gives `7` and `"2 3"` gives
  `2`);
- the end of the document: the message becomes
  `file ended while looking for text` and the read fails;
- an XML syntax error: the read is retried, and the event the reader last
  delivered is examined again by the rules above. If that event was a start
  tag the message becomes `line N: expected a value but found a <name>
  element` (this is what truncated input usually reports, naming the last
  start tag); if it was an end tag the value is the empty token; if it was a
  comment, processing instruction or whitespace the reference retries
  forever.

**RS-15a** (black-box; defect). The endless retry of RS-15 happens, for
example, when standard input holds `<step><!-- c -->` and the next line
`</stp>` (vector `walk-spin-after-comment@stdin`, recorded as a hang). A
replacement must not hang: it fails with
`error reading/parsing XML: <library message>` and writes nothing.

CDATA sections, comments and processing instructions are skipped, so
`<step><![CDATA[x]]>60</step>` reads `60` and `<step><![CDATA[60]]></step>`
reads the empty token. After a successful value read the walk expects the
end of the element (RS-14). A second text node after a comment
(`<step>6<!-- c -->0</step>`) therefore fails with
`line N: expected element but found text '0'`, and a self-closing value
element (`<step/>`) fails because the next event is the following start tag.

### 3.2 Containers

The walk starts with *expect element* `rrd`. On failure nothing is written.

**RS-16** (reference-source-inspection, confirmed black-box; must). Inside
`<rrd>`, `<ds>`, `<rra>` and a `<ds>` of `<cdp_prep>`, the walk repeatedly
reads an element and dispatches on its name. A recognised field element is
read with the field's grammar (section 4) and then its end is expected
(RS-14). Elements may appear in any order and any number of times; the last
occurrence of a field wins, and an absent field keeps its zero value
(RS-24). The container ends at its own end tag.

**RS-17** (reference-source-inspection, confirmed black-box; must). The
container loops differ in how a failed *read an element* (RS-12) ends them.
In `<rrd>`, `<ds>`, `<rra>` and `<database>`, and in `<params>` and a
`<cdp_prep>` `<ds>` once at least one field element has been read, the loop
stops and the container counts as complete; the message stays set and the
walk continues in the enclosing container. In `<params>` and a `<cdp_prep>`
`<ds>` before any field element, the failure is fatal. Examples, all
pinned:

- text directly inside `<rrd>` ends the walk as if `</rrd>` had been read;
  the file is written and the exit status is 1 with
  `expected element but found text` (vectors `walk-text-in-rrd`,
  `walk-text-after-value`);
- text inside `<ds>` ends that data source, and the enclosing loop then
  meets the next field element as an unknown tag (`walk-text-in-ds` reports
  `parse_tag_rrd: Unknown tag: type`);
- truncated input usually fails in a value read (RS-15) and reports
  `expected a value but found a <name> element` for the start tag the
  parser saw last;
- a syntax error after the last `</rra>` leaves the message set but lets
  the walk finish, so the file is written and the exit status is 1
  (`doc-tail-element@stdin`, `cut-150`, `tiny-self-closing-root`);
- text after the last field of a `<cdp_prep>` `<ds>` ends that `<ds>`, and
  the walk then fails expecting `</cdp_prep>`
  (`walk-text-after-cdp-value`).

**RS-18** (black-box; must). The elements recognised in each container are:

| Container | Field elements (grammar) | Other elements |
|---|---|---|
| `rrd` | `version` (string, 5), `step` (unsigned), `lastupdate` (signed) | `ds`, `rra` |
| `ds` | `name` (string, 20), `type` (type), `minimal_heartbeat` (unsigned), `min`, `max`, `value` (double), `last_ds` (string, 30), `unknown_sec` (unsigned), `cdef` (RS-70) | none |
| `rra` | `cf` (CF), `pdp_per_row` (unsigned), `xff` (double, version 1 only) | `params` (version 2 or later), `cdp_prep`, `database` |
| `params` | `xff`, `hw_alpha`, `hw_beta`, `seasonal_gamma`, `smoothing_window`, `delta_pos`, `delta_neg` (double); `dependent_rra_idx`, `seasonal_smooth_idx`, `window_len`, `failure_threshold` (unsigned); `value` (RS-28) | none |
| `ds` in `cdp_prep` | `primary_value`, `secondary_value`, `value`, `intercept`, `last_intercept`, `slope`, `last_slope`, `seasonal`, `last_seasonal` (double); `unknown_datapoints`, `nan_count`, `last_nan_count`, `init_flag` (unsigned); `history` (RS-26) | none |

String limits are in bytes; see RS-32.

**RS-19** (black-box; must). An unrecognised element fails with a message
that depends on the container:

| Container | Message |
|---|---|
| `rrd` | `parse_tag_rrd: Unknown tag: <name>` |
| `ds` | `parse_tag_ds: Unknown tag: <name>` |
| `rra` | `line N: parse_tag_rra: Unknown tag: <name>` |
| `params` | `line N: parse_tag_rra_params: Unknown tag: <name>` (RS-27) |
| `ds` in `cdp_prep` | `parse_tag_rra_cdp_prep: Unknown tag: <name>` |
| `database` | `line N: found unexpected tag: <name>` |

The name is as returned by RS-12, so an unexpected end tag shows with a
leading `/` (`parse_tag_rra: Unknown tag: /params`).

**RS-20** (black-box; must). The version gates two elements of `<rra>`,
using the leading decimal digits of the `version` field as a number (no
digits counts as 0, so a missing version or `abc` counts as 0):

- `<xff>` directly inside `<rra>` is recognised only when the number is 1;
- `<params>` is recognised only when the number is 2 or more.

Otherwise each is an unknown tag (RS-19). The `version` field must therefore
come before the first `<rra>`; a dump with its header after the archives
fails at `params` (`tiny-header-last`). A `<ds>` after an `<rra>` fails
with
`parse_tag_ds: All data source definitions MUST precede the RRA definitions!`.

**RS-21** (reference-source-inspection, confirmed black-box; must).
`<cdp_prep>` holds exactly one `<ds>` per data source defined so far, then
`</cdp_prep>`, each checked with *expect element* (RS-13). Too few gives
`expected </cdp_prep> element but found <ds>`; too many gives
`expected <ds> element but found </cdp_prep>`. With no data sources, an
empty `<cdp_prep></cdp_prep>` is required.

**RS-22** (reference-source-inspection, confirmed black-box; must).
`<database>` holds any number of `<row>` elements and ends at
`</database>`. Each row appends one row to the archive (the archive's row
count is the number of rows read; there is no row-count field) and holds
exactly one `<v>` per data source, each checked with *expect element* `v`,
read as a double and closed by *expect element* `/v`, then
*expect element* `/row`. A `<v>` that is a self-closing tag or empty reads
the empty token and fails its grammar. A text node or the end of input
where the next `<row>` is expected ends the row loop as in RS-17.

**RS-23** (black-box; must). With `-r`, a `<v>` value below the data source's
`min` or above its `max` is stored as unknown (NaN). A NaN limit is not
checked; the bounds are inclusive; infinities are out of range when the
corresponding limit is set; an inverted range (`min` above `max`) makes every
known value unknown. Only row values are checked: CDP and PDP state are kept
as given. Without `-r` nothing is checked.

### 3.3 Fatal and recoverable failures

**RS-24** (black-box; must). Every field starts at zero (all bytes zero), so
a missing `name`, `type`, `cf` or `last_ds` stores an empty string (no type
or CF check happens), a missing number stores 0 or 0.0, and a missing
`<params>` leaves `xff` at 0.0. A missing `version` stores `0003` (RS-51).

**RS-25** (reference-source-inspection, confirmed black-box; must). A failed
field read, a failed end check or an unknown element is fatal in `<rrd>`,
`<ds>`, `<rra>`, `<cdp_prep>`, a `<cdp_prep>` `<ds>`, `<database>` and
`<row>`: the walk stops, nothing is written and the exit status is 1. In
`<params>` a failed field read and an unknown element are not fatal: the
message is set, the walk then expects the end of that element and goes on,
the field keeps its last good value (or 0.0) and the file is written with
exit status 1. So `<xff>U</xff>` writes a file with `xff` 0.0 and exits 1
with `ling N: get_xml_double from 'U' ...`, and
`<params><bogus></bogus></params>` writes the file and exits 1 with
`parse_tag_rra_params: Unknown tag: bogus`. A failed end check is fatal
everywhere, so an unknown element in `<params>` that is self-closing or has
content fails (`expected </bogus> end element but found <xff>`,
`expected element but found text '1'`).

**RS-26** (black-box; must). `<history>` (FAILURES state) reads a token and
stores one byte per character, 1 for `1` and 0 for anything else, for at
most the first 10 characters, starting at byte 0 of the CDP state record.
The remaining bytes keep their earlier value.

**RS-27** (reference-source-inspection, confirmed black-box; defect).
A `<params>` element or a `<cdp_prep>` `<ds>` element whose first event (after
skipped comments and whitespace) is its own end tag ends the whole restore
with no message: nothing is written, nothing is printed and the exit status
is 0. `<params></params>`, `<params> </params>`, `<params><!-- c --></params>`
and the same forms of `<ds></ds>` inside `<cdp_prep>` all do this, whatever
follows. The vectors pin this behavior; a replacement that reports an error
instead is a deliberate divergence that must update them.

**RS-28** (black-box; defect). Inside `<params>`, `<value>` is an old form
of the parameter list that no longer works: `<params><value>0.5</value></params>`
stores 0.5 in parameter slot 0, writes the file and exits 1 with
`ling N: get_xml_double from '' ...` (recoverable, RS-25), and with two
`<value>` elements the second one is stored in slot 0 instead. The vectors
`walk-params-legacy-one` and `walk-params-legacy-values` pin this.

**RS-29** (black-box; must). A row value or field value read stops at the
first problem. Data already read is not kept: a fatal failure never writes
a partial file.

**RS-30** (black-box; must). The message from an earlier recoverable
failure is replaced by any later message, so only the last one is printed.

## 4. Field grammars

**RS-31** (black-box; must). Every field is read from the token of RS-15.
Text after the first whitespace is ignored, and a field of only whitespace
gives the empty token.

**RS-32** (black-box; must). String fields keep the token as bytes. Each has
a byte limit L (`version` 5, `name` 20, `last_ds` 30, and 20 for the `cf`
and `type` names): a token of L bytes or more fails with
`'<token>' is longer than L`. So a 19-byte name is accepted and a 20-byte
name, including ten two-byte UTF-8 characters, fails. Names are not
otherwise checked: `g:h`, `a&b` and duplicate names are stored as given.

**RS-33** (black-box; must). `type` must be one of `GAUGE`, `COUNTER`,
`DERIVE`, `ABSOLUTE`, `DCOUNTER`, `DDERIVE` or `COMPUTE`, matched
case-sensitively; anything else, including the empty token, fails with
`parse_tag_ds_type: Unknown data source type: <token>`. `cf` must be one of
`AVERAGE`, `MIN`, `MAX`, `LAST`, `HWPREDICT`, `MHWPREDICT`, `SEASONAL`,
`DEVSEASONAL`, `DEVPREDICT` or `FAILURES`, case-sensitively, else
`parse_tag_rra_cf: Unknown consolidation function: <token>`. The checks do
not look at the other fields: a `GAUGE` archive set and a `HWPREDICT` CF
without Holt-Winters parameters both restore.

**RS-34** (black-box; must). Unsigned fields (`step`, `minimal_heartbeat`,
`unknown_sec`, `pdp_per_row`, `unknown_datapoints` and the unsigned fields of
RS-18) follow C `strtoul` with base 0 on a 64-bit platform: optional sign,
then `0x` hexadecimal, a leading `0` for octal or decimal, digits up to the
first character that does not belong. Trailing text is ignored (`12abc`
gives 12, `1e3` gives 1, `09` gives 0, `1.5` gives 1). A minus sign negates
modulo 2^64 (`-1` gives 18446744073709551615). A value above
18446744073709551615 fails with
`ling N: get_xml_ulong from '<token>' <ERANGE text>`.

**RS-35** (black-box; must). `lastupdate` follows C `strtoll` with base 0: as
RS-34, signed 64-bit. A value outside the signed 64-bit range fails with
`ling N: get_xml_time_t from '<token>' <ERANGE text>`.

**RS-36** (black-box; must, platform specific). A token with no digits (empty,
`abc`, `0x` alone) depends on the C library: on macOS it fails with
`ling N: get_xml_ulong from '<token>' Invalid argument` (or `get_xml_time_t`),
on Linux it is accepted as 0. The Linux overrides pin this.

**RS-37** (black-box; must). Doubles are read in this order. If the token
contains `nan` in any case anywhere, the value is NaN. Otherwise if it
contains `-inf` in any case, the value is negative infinity. Otherwise if it
contains `inf` in any case, the value is positive infinity. So `banana`,
`nanx` and `-nan` are NaN, `-INF` and `-Infinity` are negative infinity, and
`xinfx` and `information` are positive infinity. The NaN stored is the
platform's quiet NaN (RS-52). Otherwise the token is converted by RS-38.

**RS-38** (black-box; must). Decimal conversion, which is not correctly
rounded:

1. Skip leading whitespace (none is left after RS-31). Read an optional
   sign.
2. Read decimal digits into a double accumulator *m*, starting at 0.0: for
   each digit *d*, *m* becomes *m* × 10 + *d*. Then, if a `.` follows, read
   more digits the same way and count them as *f*.
3. If no digit was read in step 2, the conversion fails with the ERANGE
   condition.
4. Negate *m* if the sign was `-`.
5. If `e` or `E` follows, read an optional sign and decimal digits as an
   integer *x* (no digits gives 0). The exponent is *x* (or −*x*) − *f*.
6. If the exponent is above 1024 or below −1021, the conversion fails with
   the ERANGE condition.
7. Scale by binary powers of ten: with *p* = 10.0 and *n* the absolute value
   of the exponent, for each bit of *n* from the lowest, divide (negative
   exponent) or multiply (positive exponent) *m* by *p* when the bit is set,
   then square *p*.
8. If the result is infinite, the ERANGE condition is recorded but the
   result is still used.
9. If characters remain after step 5, the conversion fails.

`1e`, `1e+`, `5.` and `.5` are accepted; `-`, `e5` and `.` fail at step 3;
`0x10`, `2junk`, `1,5` and `1_0` fail at step 9; `1e309` is accepted as
infinity; `0e400` is accepted as 0 × infinity, which is NaN.

**RS-39** (black-box; must, platform specific). Step 2's multiply-add is
fused (one rounding) on macOS arm64 and Linux aarch64 and is a separate
multiply and add on Linux x86_64. The two differ in the last bits for long
digit strings; the `numbers-*` vectors carry Linux x86_64 overrides for this.

**RS-39a** (black-box; must). A failed double conversion sets the message
`ling N: get_xml_double from '<token>' <error text>`. The error text is the
C library's description of the current error state: the ERANGE text when
the failure came from step 3 or step 6, and otherwise the text of whatever
error state was left by the most recent earlier conversion. Integer
conversions (RS-34, RS-35) reset that state to "no error" before they run
and leave it at EINVAL or ERANGE when they fail or overflow; a double
conversion sets it to ERANGE at steps 3, 6 and 8 and otherwise leaves it
alone. So `<min>1e309</min><max>2junk</max>` reports the ERANGE text for
`2junk`, while `<min>1</min><max>2junk</max>` reports the "no error" text.
The error texts are:

| Condition | macOS | Linux |
|---|---|---|
| no error | `Undefined error: 0` | `Success` |
| ERANGE | `Result too large` | `Numerical result out of range` |
| EINVAL | `Invalid argument` | (not reached, RS-36) |

## 5. Libxml2 messages and line numbers

**RS-40** (black-box; should). Syntax errors are detected by the XML
library. The message is `error reading/parsing XML: <text>` where `<text>`
is the library's message with each newline replaced by a space, cut at the
first non-printable byte after the first, and usually ending in a space.
The library text is library behavior: a replacement must report a syntax
error with the `ERROR: error reading/parsing XML: ` prefix or, where the
reference reports a walk message for the same input (RS-41), an `ERROR:`
line, with the same exit status and the same file outcome; the wording
after the prefix may differ. The vectors classify every input the library
reports as malformed as `xml-syntax` and compare only those properties.
Texts observed with both libxml2 builds, which a replacement may reuse:

| Input | Library text |
|---|---|
| empty file, whitespace only, declaration or comment only | `Extra content at the end of the document` |
| non-XML text or binary | `Document is empty` |
| end tag mismatch | `Opening and ending tag mismatch: step line 6 and stp` |
| unterminated end tag | `expected '>'` |
| truncated start tag | `Couldn't find end of Start Tag v` |
| `<` in text | `StartTag: invalid element name` |
| `]]>` in text | `Sequence ']]>' not allowed in content` |
| duplicate attribute | `Attribute a redefined` |
| `--` in a comment | `Double hyphen within comment: <!-- a` (followed by the next text) |
| control character | `PCDATA invalid Char value 1` |
| NUL byte | `Char 0x0 out of allowed range` |
| `&#1;` | `xmlParseCharRef: invalid xmlChar value 1` |
| XML version 2.0 | `Unsupported version '2.0'` |
| unknown encoding | `Unsupported encoding no-such-charset` |
| declaration not at the start | `XML declaration allowed only at the start of the document` |
| DOCTYPE inside the root | `internal error: detected an error in element content` |
| truncated DOCTYPE | `DOCTYPE improperly terminated` |
| invalid UTF-8 | `Input is not proper UTF-8, indicate encoding ! Bytes: 0xFF 0x3C 0x2F 0x6E` |

**RS-41** (black-box; should). The library reports a syntax error only when
the walk asks for an event at or past it. Which walk step meets the error
depends on how far the parser had read (RS-43), so the same malformed tail
gives different results by input source. Content after `</rrd>` read from a
named file makes the walk fail inside the last archive with a walk message
such as `line 65: expected a value but found a <unknown_datapoints> element`
and nothing is written; read from standard input it is met after `</rrd>`,
the file is written and the exit status is 1 with
`error reading/parsing XML: Extra content at the end of the document`.

**RS-42** (black-box; should, platform specific). The two libxml2 builds
differ for `<!-->` read from standard input: Apple's library reports
`Double hyphen within comment: <!--> <rrd>` (the comment is taken to run
across the line boundary) and libxml2 2.9.14 reports
`Comment not terminated`. Both fail with exit 1 and no file.

**RS-43** (black-box; should). A `line N` or `ling N` number is the parser
line, one more than the number of line feeds the parser has consumed when
the message is set. The parser consumes input ahead of the walk:

- From standard input, input arrives one line at a time. A value error is
  reported on the line holding the first non-whitespace character after the
  value's end tag (so `<step>zz</step> <!-- Seconds -->` reports its own
  line and `<max>zz</max>` followed by a blank line reports two lines
  later); an element error is reported on the line of the offending tag.
- From a named file, input arrives in 512-byte blocks. The parser stops at
  the start of the first construct (tag, text run, comment) that is not
  complete in the blocks it has; a value is checked once the block holding
  the first non-whitespace character after its end tag has been consumed,
  and the reported line is the line where the parser stopped. When all
  input has been consumed the line is one more than the number of line
  feeds in the file. This model reproduces 365 of 386 sampled value errors;
  the others report one or two lines later, because the reader sometimes
  takes one more block before handing out a text node.

**RS-44** (black-box; should). The libxml2 diagnostics of RS-10 carry their
own `file:line:` numbers, which follow the same consumption model.
Replacements should follow RS-43; the vectors hold the exact reference
numbers and the replay test accepts a message that differs only in these
numbers, counting it, unless `RONDI_RESTORE_STRICT_LINES=1` is set.

## 6. The written file

**RS-50** (black-box; must). The file uses the layout `rrdtool create`
writes on a 64-bit little-endian platform:

| Part | Size | Content |
|---|---|---|
| header | 128 | `RRD\0`, the version (RS-51) in bytes 4 to 8, zero to byte 15, the float cookie 8.642135e130 at byte 16, data source count at 24, archive count at 32, step at 40, zero bytes 48 to 127 |
| data source definitions | 120 each | name (20 bytes), type (20 bytes), then ten 8-byte slots: `minimal_heartbeat` (unsigned) in slot 0, `min` in slot 1, `max` in slot 2 |
| archive definitions | 120 each | CF name (20 bytes), 4 zero bytes, row count (8), `pdp_per_row` (8), then ten 8-byte parameter slots (RS-53) |
| last update | 16 | `lastupdate` (signed 8), then 8 zero bytes |
| PDP state | 112 each | `last_ds` (30 bytes), 2 zero bytes, `unknown_sec` (unsigned 8), `value` (double 8), zero to the end |
| CDP state | 80 each, archive-major | ten 8-byte slots (RS-53) |
| row pointers | 8 each | RS-61 |
| rows | 8 per value | each archive's rows in turn, one double per data source |

Strings are stored with zero bytes after their last byte. Every byte not
named here is zero. Nothing follows the last row.

**RS-51** (black-box; must). The stored version is the `version` token,
unless its leading decimal number (RS-20) is below 3, in which case `0003`
is stored. So `0001`, `0002`, an absent version and `abc` (if the walk gets
that far) store `0003`, while `3` stores `3\0\0\0\0` and `0006` stores
`0006`.

**RS-52** (black-box; must, platform specific). A NaN read from the XML
(RS-37) or set by the range check (RS-23) is stored as `0x7ff8000000000000`
on arm64 and aarch64 and as `0xfff8000000000000` on x86_64. The normal form
folds the two, so the vectors do not distinguish them.

**RS-53** (black-box; must). Parameter and CDP slots, by 8-byte slot index:

| Element | Archive parameter slot | | Element | CDP slot |
|---|---|---|---|---|
| `xff` | 0 (double) | | `value` | 0 (double) |
| `hw_alpha`, `seasonal_gamma`, `delta_pos` | 1 (double) | | `unknown_datapoints` | 1 (unsigned) |
| `hw_beta`, `smoothing_window`, `delta_neg` | 2 (double) | | `intercept`, `seasonal` | 2 (double) |
| `dependent_rra_idx` | 3 (unsigned) | | `last_intercept`, `last_seasonal` | 3 (double) |
| `seasonal_smooth_idx`, `window_len` | 4 (unsigned) | | `slope` | 4 (double) |
| `failure_threshold` | 5 (unsigned) | | `last_slope` | 5 (double) |
| | | | `nan_count`, `init_flag` | 6 (unsigned) |
| | | | `last_nan_count` | 7 (unsigned) |
| | | | `primary_value` | 8 (double) |
| | | | `secondary_value` | 9 (double) |

Elements that share a slot overwrite each other in document order. Doubles
are stored as IEEE 754 little-endian, unsigned values as 64-bit
little-endian.

**RS-54** (black-box; must). Before writing, the target is opened for
writing and created if missing, with mode 0666 reduced by the umask. Without
`-f` an existing target (or a directory) fails with
`creating '<name>': File exists`; other open failures fail with
`creating '<name>': <C library error text>` (for example
`No such file or directory` or, with `-f` and a directory,
`Is a directory`). The target is opened only after the walk completed
(RS-59), so a failure that stops the walk never creates or changes the
target.

**RS-55** (black-box; should). A new file gets mode `0666 & ~umask`, as the
reference does. A replacement may create it with fewer permission bits.

**RS-56** (black-box; defect). With `-f` the existing file is overwritten in
place from offset 0 without truncation: a longer old file keeps its tail
after the new content. A replacement should truncate the file to the new
length.

**RS-57** (black-box; must). With `-f` the file is written in place, so its
inode is kept.

**RS-58** (black-box; must). The target `-` writes the file to standard
output, for both input sources.

**RS-59** (black-box; must). The file is written when the walk completes,
even if a recoverable failure left a message set (RS-17, RS-25); the exit
status is then 1 (RS-6). The cases are: text where an element is expected
in `<rrd>`, a syntax error met after the last archive, a self-closing root
`<rrd/>` (`the xml ended while we were looking for an element`, and a file
with no data sources or archives), and the recoverable `<params>` failures.

### 6.1 Rows and pointers

**RS-60** (black-box; must). For comparison the vectors use the normal form
of each file. Two files are equivalent when their normal forms are equal.

**RS-61** (black-box; should). Each archive's row pointer (`cur_row`) is a
value in `[0, rows)` that RRDtool chooses at random for every restore, and
the rows are stored rotated so that the dump order is preserved: the last
`<row>` of the archive is stored at the pointer, and the first at the slot
after it. A replacement may choose any pointer in range, including a fixed
one, as long as the rotation matches it.

**RS-62** (black-box; must). An archive without a `<database>` element has
zero rows. Its pointer is undefined (a random value on arm64); a
replacement should store 0. The archive contributes no row bytes.

**RS-63** (black-box; defect, platform specific). A `<database>` element
that is empty or self-closing, or whose row loop ends before its first
`<row>` (RS-22), makes the reference compute a pointer modulo zero rows. On macOS arm64 and
Linux aarch64 the restore then fails with `parse_tag_rra: malloc failed.`
and nothing is written; on Linux x86_64 the reference dies from SIGFPE. A
replacement must fail with an `ERROR:` message and write nothing; the
vectors pin the aarch64 text.

**RS-64** (black-box; defect). A second `<database>` in one archive makes the
reference corrupt memory: it dies from a signal on Linux and on macOS either
dies or fails with `the xml ended while we were looking for an element`,
varying from run to run. A replacement must not crash; the vectors only
require exit 0 or 1 (`dup-database`).

**RS-65** (black-box; defect, platform specific). Rows given for a document
with no data sources (`<row></row>` with no `<ds>`) fail on macOS with
`line N: expected <v> element but found </row>` and kill the reference with
a signal on Linux. The vectors pin the macOS text; on Linux only a clean
exit is required.

## 7. Unsupported inputs

**RS-70** (black-box; should). A `COMPUTE` data source carries a `<cdef>`
element with an RPN expression over the other data sources. RRDtool
compiles it into the definition's parameter slots in its internal RPN
encoding and restores the file; a `<cdef>` that does not compile fails with
`failed to parse computed data source`. Rondi does not implement this and
fails with its own `... not supported by Rondi` message, writing nothing.
The vectors mark these cases `unsupported`.

**RS-71** (black-box; must). Holt-Winters archives (`HWPREDICT`,
`MHWPREDICT`, `SEASONAL`, `DEVSEASONAL`, `DEVPREDICT`, `FAILURES`) restore
like any other archive with the parameter and state slots of RS-53. No
consistency check is made between the CF, its parameters and the
`dependent_rra_idx` links.

## 8. Resource bounds

**RS-72** (black-box; should). RRDtool sets no limit on the number of data
sources, archives or rows; memory grows with the input. A replacement
should bound memory by a small multiple of the restored file size, must not
hang on any input and must not crash: where the reference crashes or hangs
(RS-63 to RS-65) a replacement fails with an `ERROR:` message and exit 1 or
exits 0. A 2000-row archive (`tiny-many-rows`) is within the vectors.

## Summary by class

Must reproduce exactly: RS-1 to RS-3, RS-5, RS-6, RS-8 to RS-15, RS-16 to
RS-26, RS-29 to
RS-39, RS-39a, RS-50 to RS-54, RS-57 to RS-60, RS-71. Where these touch file
bytes the vectors compare normal forms.

Should reproduce: RS-4, RS-7, RS-40 to RS-44 (library text and line
numbers), RS-55, RS-61, RS-70 and RS-72.

Reference defects: RS-15a (endless retry; a replacement fails cleanly),
RS-27 and RS-28 (pinned for now), RS-56 (replacement
truncates), RS-62 (undefined pointer), RS-63 to RS-65 (crashes and
platform-dependent failures; a replacement fails cleanly). The swapped names
in the RS-13 message and the misspelt `ling` prefix are reference wording
that is reproduced.

## Provenance summary

Rules that needed reference-source inspection: RS-6 (exit status follows the
message, not the result), RS-9 (line-based stdin reading and ctrl-Z),
RS-12 to RS-17 (read operations, the swapped RS-13 wording explained, and
how a failed element read ends each container), RS-21 and RS-22 (exact
`<cdp_prep>` and row structure), RS-25 (which failures are fatal) and RS-27
(the silent stop). Each was afterwards confirmed by black-box vectors
(`walk-*`, `tiny-*`, `doc-*`). All other rules come from documentation and
black-box runs. No source text, comments or structure were copied; the
error strings quoted here are the observable output, which compatibility
requires.

## Known divergences

Current Rondi against RRDtool 1.11.0, as of this specification:

- **RS-81** COMPUTE data sources are refused (RS-70; vectors `seed-compute`,
  `type-compute`, `walk-cdef-*`, expectation `unsupported`).
- **RS-82** UTF-16 input with a byte order mark from a named file is refused
  with `Document is empty`; RRDtool restores it (`doc-utf16le-bom`,
  `doc-utf16be-bom`, expectation `diverge`).
- **RS-83** A Latin-1 declared document is not transcoded, so a non-ASCII
  name is stored as its Latin-1 bytes (`doc-latin1-name`); invalid UTF-8 in
  a UTF-8 document is accepted instead of failing (`doc-bad-utf8`); an
  entity declared in an internal DTD subset is reported as undefined
  instead of being dropped (`value-internal-entity`). Expectation `diverge`.
- **RS-84** The NUL-byte syntax error uses libxml2's other wording
  (`PCDATA invalid Char value 0`), and `<!-->` from standard input uses the
  Linux wording on every platform (RS-42). Both are within `xml-syntax`.
- **RS-85** Opening a directory with `-f` reports
  `I/O error: Is a directory (os error 21)` instead of
  `creating '.': Is a directory` (`target-is-dir-force`, expectation
  `text-differs`).
- **RS-86** `-f` over a longer file truncates it (RS-56; `opt-force-longer-file`,
  expectation `truncates`).
- **RS-87** A second `<database>` restores with exit 0 instead of crashing
  (RS-64), and the endless retry of RS-15a ends with the library error.
- **RS-88** New files are created with mode 0600 rather than
  `0666 & ~umask` (RS-55), restores always place the row pointer at the last
  row (RS-61), and symbolic links and hard-linked files are refused as
  targets where RRDtool writes through them (outside this table; see
  `crates/rondi-cli/tests/csrc_mutate.rs`).

No other difference was found: Rondi matches every other vector on macOS
arm64, including the exact line numbers.

## Tests

- `crates/rondi-cli/tests/restore_spec_vectors.rs` replays every case
  against Rondi under the `rrdtool` name, runs them live against the pinned
  RRDtool when it is on `PATH` (always under `RONDI_REQUIRE_ORACLE=1`), and
  checks file creation mode, in-place `-f` writes and row pointer ranges.
- `crates/rondi-cli/tests/data/restore/inputs/*.xml` are the inputs. The
  `seed-*` files are dumps written by RRDtool 1.11.0 (every DS type and CF,
  Holt-Winters and COMPUTE, the three header styles); the others are
  derived from them or written by hand.
- `cases.tsv` lists each case: id, input, input source (`file` or `stdin`),
  target, pre-existing target content (`none`, `old`, `long`), Rondi
  expectation (`same`, `xml-syntax`, `unsupported`, `truncates`,
  `text-differs`, `diverge`) and the options.
- `expected/base.vectors` holds, for every case, the exit status (or
  `signal`), standard output, standard error, the digest of the resulting
  file's normal form (or `none`, or `undefined` when the reference result
  varies) and flags, framed as `@@ <id> <section> <length>` followed by the
  bytes and a newline. It was recorded on macOS arm64.
  `expected/linux-aarch64.vectors` and `expected/linux-x86_64.vectors`
  replace individual cases on Linux. `expected/outputs/<digest>.rrd` holds
  each distinct normal form.
- `scripts/gen-restore-vectors.py write` rebuilds the inputs and the base
  vectors from a local RRDtool 1.11.0, running every case three times with
  different environment sizes and flagging any that vary, crash or hang.
  `scripts/gen-restore-vectors.py overrides --platform linux-aarch64` (or
  `linux-x86_64`), run in a Linux container with the pinned build, records
  the overrides. `compare --program <path>` lists the cases where another
  implementation differs.
