# Graph and xport: options, elements, layout and output

This specification describes `rrdtool graph`, `graphv` and `xport` as
RRDtool 1.11.0 behaves from the outside: how the command line is split into
options, an output name and elements; every option with its accepted values
and error texts; the syntax and meaning of `DEF`, `CDEF`, `VDEF`, `LINE`,
`AREA`, `STACK`, `TICK`, `GPRINT`, `PRINT`, `COMMENT`, `HRULE`, `VRULE`,
`SHIFT`, `TEXTALIGN` and `XPORT`; the time range and step; the value range
of the y axis; the image size and legend layout that `graphv` reports; what
is written to standard output, standard error and the output file; and the
two resource limits Rondi adds (RD-016, RD-017). It is written so that the
graph front end can be implemented from it and from the vectors without
reading RRDtool's source or Rondi's current graph modules.

Related specifications, which this one does not repeat:

- [format-strings.md](format-strings.md): the `PRINT`/`GPRINT` format
  grammar, `%s`/`%S` SI scaling, `:strftime`, `:valstrftime`,
  `:valstrfduration`, the `--imginfo` format check and the
  `--left-axis-format`/`--right-axis-format` check.
- [rpn.md](rpn.md): the RPN language of `CDEF` and the values it produces.
- [time-spec.md](time-spec.md): at-style times in `--start`, `--end` and
  the `start=`/`end=` fields of `DEF`.
- [cli.md](cli.md): command dispatch, `rrdtool graph` with no arguments
  (help), pipe mode, `RRDCACHED_ADDRESS` handling and exit status in
  general.

## Conventions

Each rule has an identifier `GR-n` (stable; new rules get new numbers), a
provenance tag and a class.

Provenance:

- **doc**: stated in `rrdgraph(1)`, `rrdgraph_graph(1)`, `rrdgraph_data(1)`
  or `rrdxport(1)` as shipped with 1.11.0, or in the help text the program
  prints.
- **bb**: observed by running `/opt/homebrew/bin/rrdtool` 1.11.0 (macOS
  arm64) and the pinned 1.11.0 build of `docker/Dockerfile.test` (Linux
  aarch64 and x86_64), and pinned by the vectors named in the rule.
- **rsi** (reference-source inspection): none. Every rule below was settled
  by documentation or black-box runs. Rules whose detail the black-box runs
  did not settle are listed under [Unsettled questions](#unsettled-questions)
  instead of being filled in from the source.

Where a black-box result contradicts the manual, the black-box result is
the contract and the rule says so.

Class:

- **compat**: a replacement reproduces it exactly (text byte for byte,
  numbers digit for digit).
- **rondi-policy**: Rondi intentionally differs from the reference. The rule
  gives the reference behavior and, separately, the behavior Rondi keeps.
  These rules are listed again under [Rondi policies](#rondi-policies).
- **platform-specific**: the reference result depends on the host (libc
  messages, fonts, file system errors). The vectors carry per-platform
  expectations; a replacement matches the expectation of the platform it
  runs on.

A rule may also be marked **undefined** when the reference crashes, hangs
or reads uninitialized memory. A replacement then only has to finish
without a signal or a hang; the rule says what Rondi does.

Text in `code` is exact. `ERROR: ` followed by a message means the
message is printed on standard error as `ERROR: <message>` and a line feed,
and the process exits with status 1, unless the rule says otherwise.

## Compatibility tests

- `crates/rondi-cli/tests/data/graph_spec_vectors.json` holds the frozen
  cases. Each case is one or more steps run in one fresh copy of the
  fixture directory; each step records the exit status, standard output,
  standard error and, for steps with `inspect`, the content of the named
  files after the step.
- `crates/rondi-cli/tests/graph_spec_vectors.rs` replays every case
  against the Rondi binary under the name `rrdtool`. With RRDtool 1.11.0 on
  `PATH` it also replays each case through it and requires the same
  result; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure. Cases
  where Rondi differs are listed in its `KNOWN_DIVERGENCES` with a class
  (`policy` or `defect`); a listed case that starts matching fails the
  test so the entry is removed.
- `scripts/graph-spec-vectors.py` regenerates the table
  (`--rrdtool PATH`), runs every case twice and tags a case `unstable`
  when the two runs differ, `--only GROUP` prints one group,
  `--check PATH` compares another binary with the table, and
  `--record-platform KEY` / `--merge-platform KEY FILE` record and merge
  Linux results (run inside an image built like `docker/Dockerfile.test`).

### Fixture

The fixture is built from XML dumps captured from the reference at
generation time and restored with the program under test:

| File | Content |
|---|---|
| `f.rrd` | step 60, `DS:x:GAUGE:120:U:U`, `DS:y:GAUGE:120:U:U`, `RRA:AVERAGE`, `MAX`, `MIN`, `LAST` `:0.5:1:30`; 20 updates at 1000000060 + 60(i-1), x = i, y = (-1)^i·10·i, the 8th update unknown |
| `d.rrd` | step 300, two GAUGE sources, `RRA:AVERAGE:0.5:1:600`, `AVERAGE`/`MAX`/`MIN` `:0.5:6:200`; 576 updates from 1000000300, x a sine wave between 10 and 90, y = 1000·i |
| `e.rrd` | step 60, one GAUGE source, never updated |
| `notes.txt`, `sub/` | a text file and a directory |

Most cases use `--start 1000000000 --end 1000001200 DEF:x=f.rrd:x:AVERAGE`.

### Environment and normalization

Each step runs with `TZ=UTC`, `LC_ALL=C` and without `RRDCACHED_ADDRESS`,
`RRD_LOCKING`, `RRD_DEFAULT_FONT`, `LANG` or any other `LC_*` variable; a
case's `env` is applied on top. Standard input is empty. Output bytes are
read as Latin-1 so every byte maps to one character.

Image bytes are never compared (see GR-90). Before comparison:

| Normalization | Reason |
|---|---|
| A PNG image in standard output or in an inspected file becomes `<PNG WxH>` with the width and height of its IHDR chunk | pixels depend on the font stack and renderer |
| An SVG, PDF or EPS image becomes `<SVG>`, `<PDF>` or `<EPS>` | the same; Rondi does not write them (RD-006) |
| The number after `image = BLOB_SIZE:` becomes `*` | the image size in bytes |
| The work directory becomes `{CWD}` | absolute paths in errors |
| Text after `Compiled ` is dropped | the build stamp in the help banner |

An inspected path that does not exist is recorded as `<missing>`, a
directory as `<dir>`.

### Groups

| Group | Content |
|---|---|
| `opt`, `grammar` | every option in long, `--name=value`, short and attached form, missing arguments, flags given a value, abbreviations, clusters, `--`, option order |
| `num` | numeric option values: range, garbage, signs, overflow |
| `imgformat`, `color`, `font`, `font-render-mode`, `graph-render-mode`, `units`, `left-axis-formatter`, `right-axis-formatter`, `grid-dash`, `right-axis`, `right-axis-range`, `left-axis-format`, `right-axis-format`, `y-grid`, `x-grid`, `week-fmt`, `title`, `vertical-label`, `watermark`, `right-axis-label`, `imginfo`, `daemon`, `legend-position`, `legend-direction`, `formatter` | per-option value tables |
| `time` | `--start`/`--end` forms, `--step`, width and resolution, `--utc` and time zones |
| `def`, `cdef`, `vdef` | data definitions |
| `line`, `area`, `tick`, `hrule`, `vrule`, `shift`, `textalign`, `comment`, `print`, `gprint`, `element` | graph and print elements |
| `layout` | `graphv` geometry for size, title, labels, axes, fonts, borders, legends |
| `legend` | legend placement: markers, alignment, spacing, wrapping, direction, position |
| `scale` | `value_min`/`value_max` under autoscale and the scaling options |
| `data` | `--imgformat` XML, XMLENUM, JSON, JSONTIME, CSV, TSV, SSV |
| `xport` | `xport` options, elements and output |
| `out` | output files, `-`, `graphv -`, `--imginfo` output, `--lazy` |
| `limit` | image size and fetch size limits (RD-016, RD-017) |

## 1. Command line

**GR-1** (doc, bb; compat). `rrdtool graph|graphv <args>`: every argument
that is not an option or an option's value is a *word*. The first word is
the output file name; every later word is an element. Options may appear
before, between or after the words (`graph -w 300 /dev/null ...` and
`graph /dev/null DEF:... LINE... -w 300` are the same). A word of exactly
`--` ends the options; every later argument is a word even if it starts
with `-` (`grammar-034`, `-042`, `-049`, `-050`).

**GR-2** (bb; compat). With no word at all (`graph --width 300`) the
error is `ERROR: missing filename`. `rrdtool graph` with no argument at
all prints the help text (cli.md CLI-3). An empty file name is a file
name: `graph '' ...` renders and then fails with
`ERROR: Could not save png to ''` after printing the size line
(`grammar-059`).

**GR-3** (bb; compat). A graph with no elements, or with only `DEF`,
`CDEF`, `VDEF` and `COMMENT` elements and no print element, prints nothing
and exits 0; no file is written (`grammar-055`, `-056`, `-060`, `-061`,
`out-008`, `layout-070`, `-071`). `COMMENT` alone does not count as a graph
element (`layout-071`). `HRULE`, `VRULE`, `LINE`, `AREA`, `TICK` and
`GPRINT` do (`layout-072`, `scale-196` to `-200`).

**GR-4** (bb; compat). A graph with print elements (`PRINT`) and no graph
element computes the prints, writes no image and prints `0x0` as the size
line followed by the print lines (`def-065`, `out-009`). `graphv` prints
only the `print[n]` keys (`layout-073`).

**GR-5** (bb; compat). Errors print `ERROR: <message>` on standard error
and exit 1. An error found while parsing options or elements prints
nothing on standard output and writes no file (`out-006`, `-007`,
`-010`). An error found after the image was laid out (saving the image,
connecting to rrdcached) is printed after the size line and the print
lines, which are still printed (GR-82).

## 2. Option grammar

**GR-10** (bb; compat). Long options are matched exactly and case
sensitively. No abbreviation is accepted: `--wid`, `--w`, `--alt`,
`--log`, `--legend-p`, `--WIDTH` are all `ERROR: invalid option -- '<as
given>'` with the text after `--` up to the end of the argument, including
any `=value` (`--bogus=1` gives `'bogus=1'`; `---width` gives `'-width'`)
(`grammar-008` to `-029`, `-036`).

**GR-11** (bb; compat). A long option that takes a value accepts it as
`--name=value` or as the next argument (`--name value`). An empty value
`--name=` is a value (the empty string), not a missing one
(`grammar-037`, `-038`). The next argument is taken as the value even if it
starts with `-`.

**GR-12** (bb; compat). A long flag given a value with `=` fails with
`ERROR: option takes no arguments -- '<name>'` (every flag in `opt`,
`grammar-040`, `-041`). A long option that needs a value and is the last
argument fails with `ERROR: option requires argument -- '<name>'`
(note: no `an`; the short-option text in GR-13 has it). The message
after `ERROR: ` is cut to 47 bytes, so long names lose their end and the
closing quote: `option requires argument -- 'vertical-label-ang`,
`option takes no arguments -- 'alt-autoscale-max` (`opt-*` cases with the
option last or given `=x`).

**GR-13** (bb; compat). A short option that needs a value takes the rest
of its argument if any (`-w300`, `-tA title`), else the next argument.
Short flags may be clustered (`-gjE`), and a value-taking letter inside a
cluster takes the rest of the cluster or the next argument (`-gw300`,
`-gw 300`; `-wh 300` gives `-w` the value `h`, so `ERROR: width below 10
pixels`) (`grammar-001` to `-006`). A value-taking short option that is the
last argument fails with `ERROR: option requires an argument -- '<c>'`.
An unknown letter fails with `ERROR: invalid option -- '<c>'`, including
`-?`, `-K`, `-1` and the first unknown letter of a cluster (`-Hw 5` gives
`'H'`) (`grammar-026` to `-033`, `-044`).

**GR-14** (bb; compat). A lone `-` is a word, not an option: as the
first word it names standard output (GR-80), elsewhere it is an element
and fails as `ERROR: '-' is not a valid function name in -`
(`grammar-027`).

**GR-15** (bb; compat). When an option is given more than once, the last
value wins (`-w 300 -w 200` is 200 wide, `grammar-007`). Options are
processed in order and the first invalid one ends the run with its error.

### Option table

**GR-16** (doc, bb; compat). The options of `graph` and `graphv`, with
short forms where they exist. *Flag* means no value. The vectors in `opt`
cover every row in every accepted form.

| Long | Short | Value | Effect (see rule) |
|---|---|---|---|
| `--start` | `-s` | time | GR-30 |
| `--end` | `-e` | time | GR-30 |
| `--step` | `-S` | seconds | GR-32 |
| `--title` | `-t` | text | GR-63 |
| `--vertical-label` | `-v` | text | GR-63 |
| `--vertical-label-angle` | | degrees | any number (GR-21); layout unchanged |
| `--width` | `-w` | pixels | GR-20 |
| `--height` | `-h` | pixels | GR-20 |
| `--only-graph` | `-j` | flag | GR-62 |
| `--full-size-mode` | `-D` | flag | GR-62 |
| `--upper-limit` | `-u` | number | GR-21, GR-70 |
| `--lower-limit` | `-l` | number | GR-21, GR-70 |
| `--rigid` | `-r` | flag | GR-70 |
| `--allow-shrink` | | flag | GR-70 |
| `--alt-autoscale` | `-A` | flag | GR-71 |
| `--alt-autoscale-min` | `-J` | flag | GR-71 |
| `--alt-autoscale-max` | `-M` | flag | GR-71 |
| `--no-gridfit` | `-N` | flag | rendering only |
| `--x-grid` | `-x` | spec or `none` | GR-24 |
| `--week-fmt` | | strftime text | accepted unchecked; rendering only |
| `--y-grid` | `-y` | `step:factor` or `none` | GR-24 |
| `--left-axis-formatter` | | `numeric`, `timestamp`, `duration` | GR-25 |
| `--left-axis-format` | | format | GR-25 |
| `--alt-y-grid` | `-Y` | flag | rendering only (value range unchanged, `scale` `-Y` cases) |
| `--logarithmic` | `-o` | flag | GR-72 |
| `--units-exponent` | `-X` | integer | accepted unchecked; layout and range unchanged |
| `--units-length` | `-L` | integer | GR-61 |
| `--units` | | `si` | GR-23 |
| `--right-axis` | | `scale:shift` | GR-24, GR-61 |
| `--right-axis-label` | | text | GR-61 |
| `--right-axis-label-angle` | | degrees | as `--vertical-label-angle` |
| `--right-axis-range` | | `min:max` | GR-24 |
| `--right-axis-formatter` | | as left | GR-25 |
| `--right-axis-format` | | format | GR-25 |
| `--no-legend` | `-g` | flag | GR-61 |
| `--force-rules-legend` | `-F` | flag | GR-53 |
| `--legend-position` | | `north`, `south`, `east`, `west` | GR-23, GR-65 |
| `--legend-direction` | | `topdown`, `bottomup`, `bottomup2` | GR-23, GR-65 |
| `--lazy` | `-z` | flag | GR-84 |
| `--daemon` | `-d` | address | GR-26 |
| `--imginfo` | `-f` | format | GR-83 |
| `--color` | `-c` | `TAG#rrggbb[aa]` | GR-22 |
| `--grid-dash` | | `on:off` | GR-24 |
| `--border` | | integer | accepted unchecked; layout unchanged |
| `--dynamic-labels` | | flag | rendering only |
| `--zoom` | `-m` | number | GR-21, GR-81 |
| `--font` | `-n` | `TAG:size[:font]` | GR-22, GR-60 |
| `--font-render-mode` | `-R` | `normal`, `light`, `mono` | GR-23 |
| `--font-smoothing-threshold` | `-B` | number | accepted unchecked |
| `--pango-markup` | `-P` | flag | GR-64 |
| `--graph-render-mode` | `-G` | `normal`, `mono` | GR-23 |
| `--slope-mode` | `-E` | flag | rendering only |
| `--imgformat` | `-a` | format name | GR-23, GR-86 |
| `--interlaced` | `-i` | flag | ignored |
| `--tabwidth` | `-T` | number | GR-21, GR-64 |
| `--base` | `-b` | `1000` or `1024` | GR-21 |
| `--watermark` | `-W` | text | GR-63 |
| `--use-nan-for-all-missing-data` | `-Z` | flag | GR-41 |
| `--add-jsontime` | | flag | GR-87 |
| `--utc` | | flag | GR-33 |
| `--disable-rrdtool-tag` | | flag | GR-61 |
| `--alt-y-mrtg` | | flag | accepted; range unchanged |
| `--no-minor` | | flag | rendering only |

*Rendering only* means the option is validated as stated and changes no
observable output other than the image pixels.

### Option values and their errors

**GR-20** (bb; compat). `--width` and `--height` read a leading decimal
integer like C `atoi` (leading white space and a sign allowed, the rest
ignored: `' 50'`, `50abc`, `+30` are accepted; `1e2` is 1, `0x20` is 0,
`abc` and `''` are 0). A width below 10 fails with
`ERROR: width below 10 pixels`, a height below 10 with
`ERROR: height below 10 pixels` (`num-001` to `-022`). Values that do not
fit a C `long` are **undefined** (the reference crashes, `num-011`,
`-014`); Rondi refuses them through RD-016.

**GR-21** (bb; compat). Floating-point options (`-u`, `-l`, `-m`, `-T`,
`--vertical-label-angle`, `--right-axis-label-angle`, the numbers of
`--right-axis-range`) are read like C `strtod` over the whole value:

- nothing converted: `ERROR: option <opt> - Cannot convert '<value>' to float`;
- trailing text: `ERROR: option <opt> - Converted '<value>' to <%f of the number>, but cannot convert '<rest>'`
  (`-u 5x` gives `Converted '5x' to 5.000000, but cannot convert 'x'`;
  `0x10` gives `Converted '0x10' to 0.000000, but cannot convert 'x10'`, so
  hexadecimal is not accepted; `1,5` stops at the comma);

where `<opt>` is `-u`, `-l`, `-m`, `-T` for those short options and
`--vertical-label-angle`, `--right-axis-label-angle`, `--right-axis-range`
for the long-only ones (`num-023` to `-039`, `-069` to `-083`, `-099` to
`-109`, `right-axis-range-009`). `inf`, `nan` and overflowing values
(`1e400`) are accepted. Then:

- `--zoom` must be greater than 0: `ERROR: zoom factor must be > 0`.
- `--base` is read like `atoi` and must be 1000 or 1024:
  `ERROR: the only sensible value for base apart from 1000 is 1024`
  (`1024x` is accepted, `1e3` is not; `num-040` to `-048`).
- An upper limit of +infinity or a lower limit of -infinity abandons the
  graph silently: nothing is printed, status 0 (`num-024`, `-027`,
  `-036`). `-u -inf`, `-l +inf` and `nan` render normally.
- `--units-exponent`, `--units-length`, `--border`, `--step` and
  `--font-smoothing-threshold` are read like `atoi` and never fail
  (`num-049` to `-068`, `-084` to `-098`, `-110` to `-113`).
- `--tabwidth -1` makes the reference print Pango warnings with a process
  id; the case is `unstable` (`num-079`).

**GR-22** (bb; compat). `--color TAG#hex`:

- `TAG` is one of `BACK`, `CANVAS`, `SHADEA`, `SHADEB`, `GRID`, `MGRID`,
  `FONT`, `AXIS`, `FRAME`, `ARROW`, exactly and case sensitively. Another
  run of upper-case letters before `#` fails with
  `ERROR: invalid color name '<TAG>'` (`BOGUS`, `XBACK`, `BAC`, `BACKX`).
  Anything else that does not look like `LETTERS#` (no `#`, lower case,
  a space, empty) fails with `ERROR: invalid color def format`.
- After `#`, the hexadecimal digits are counted: 3 (`#rgb`), 4 (`#rgba`),
  6 or 8 digits are accepted, and so are values whose digit run is 9 or
  longer or followed by other text (`#102030405`, `#102030x`, `#-10203`
  are accepted). 5 or 7 digits fail with
  `ERROR: the color format is #RRGGBB[AA]`. No digit at all (`#gg0000`,
  `#`) fails with `ERROR: invalid color def format`.

(`color-001` to `-030`.)

**GR-23** (bb; compat). Keyword values are matched exactly and case
sensitively:

| Option | Accepted | Error for anything else |
|---|---|---|
| `--imgformat` | `PNG`, `SVG`, `EPS`, `PDF`, `XML`, `XMLENUM`, `JSON`, `JSONTIME`, `CSV`, `TSV`, `SSV` | `unsupported graphics format '<v>'` |
| `--font-render-mode` | `normal`, `light`, `mono` | `unknown font-render-mode '<v>'` |
| `--graph-render-mode` | `normal`, `mono` | `unknown graph-render-mode '<v>'` |
| `--legend-position` | `north`, `south`, `east`, `west` | `unknown legend-position '<v>'` |
| `--legend-direction` | `topdown`, `bottomup`, `bottomup2` | `unknown legend-position '<v>'` (sic) |
| `--units` | `si` | `invalid argument for --units: <v>` |
| `--left-axis-formatter` | `numeric`, `timestamp`, `duration` | `Unknown left axis formatter` |
| `--right-axis-formatter` | `numeric`, `timestamp`, `duration` | `Unknown right axis formatter` |

`--font TAG:size[:font]` (doc says `FONTTAG:size[:font]`): the tag is
`DEFAULT`, `TITLE`, `AXIS`, `UNIT`, `LEGEND` or `WATERMARK`. An unknown
upper-case tag fails with `ERROR: invalid fonttag '<TAG>'`; a value
without `:` after the tag, a lower-case tag, an empty tag or a size that
is not a number fails with `ERROR: invalid text property format`. The size
is read like `strtod` and a negative size is accepted. Text after a second
`:` is the font name and may contain `:` and spaces (`font-001` to
`-019`).

**GR-24** (bb; compat). Structured values:

- `--x-grid none` turns the x grid and its labels off. Otherwise the value
  is `GTM:GST:MTM:MST:LTM:LST:LPR:LFM`, with `?TM` one of `SECOND`,
  `MINUTE`, `HOUR`, `DAY`, `WEEK`, `MONTH`, `YEAR` (case sensitive), the
  `?ST` and `LPR` numbers, and `LFM` the rest of the value (it may contain
  `:` and may be empty). An unknown upper-case keyword fails with
  `ERROR: unknown keyword <KW>`; fewer than eight fields, a non-number,
  `NONE`, lower case or an empty value fail with
  `ERROR: invalid x-grid format`. A grid step of 0 or less is
  **undefined**: the reference crashes for 0, and for a negative step it
  renders on macOS and crashes on Linux (`x-grid-009`, `-010`). Rondi
  finishes. An LFM of 120 characters is accepted (`x-grid-001` to
  `-020`).
- `--y-grid none` turns the y grid off (GR-61). Otherwise `step:factor`
  read as a float and an integer; trailing text after them is ignored
  (`1:2:3`). Fewer fields, `NONE` or garbage fail with
  `ERROR: invalid y-grid format`; a step of 0 or less fails with
  `ERROR: grid step must be > 0`; a factor of 0 or less with
  `ERROR: label factor must be > 0` (`y-grid-001` to `-012`).
- `--grid-dash on:off`, two floats; anything else is
  `ERROR: invalid grid-dash format`. A third field is ignored. Negative
  values are accepted and make the renderer print
  `OOPS: Cairo has issues it can't even die: invalid value for a dash setting`
  on standard error while the graph still succeeds (`grid-dash-001` to
  `-012`).
- `--right-axis scale:shift`, two floats with trailing text ignored. Fewer
  fields or garbage give
  `ERROR: invalid right-axis format expected scale:shift`; a scale of 0
  gives `ERROR: the second_axis_scale  must not be 0` (two spaces).
- `--right-axis-range min:max`, either side may be empty. A value without
  `:` gives `ERROR: invalid right-axis-range format expected min:max`; a
  side that is not a number gives the GR-21 conversion error with
  `--right-axis-range`; min greater than max gives
  `ERROR: right-axis-range min must not be larger than max`. `nan` is
  accepted (`right-axis-range-001` to `-010`).

**GR-25** (bb; compat). Axis formatters and formats:

- With the `numeric` formatter (the default), `--left-axis-format` and
  `--right-axis-format` must be a value format as defined in
  format-strings.md (FS rules for axis formats); an empty format is
  accepted. Errors print
  `ERROR: invalid format string '<f>' (should match '^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*$')`.
- With `timestamp`, the format is a strftime text and is not checked.
- With `duration`, the format must be a valid duration format; a value
  format such as `%lf` fails after layout with `ERROR: Wrong duration
  format`, the size line already printed (`formatter-007`, `-008`).
- The vectors give the formatter before the format; whether the check
  depends on their order was not tested.

**GR-26** (bb; compat). `--daemon` accepts any text. An empty value means
no daemon. Otherwise the graph is drawn from the local files, the size and
print lines are printed, and the run then fails with the connection error
(`ERROR: Unable to connect to rrdcached: <reason>`, exit 1); see cli.md
for address parsing (`daemon-001` to `-003`; class platform-specific for
the resolver text).

## 3. Time range and step

**GR-30** (doc, bb; compat). `--start` and `--end` take at-style times
(time-spec.md); the defaults are `end-1day` and `now`. A start or end
that cannot be parsed fails with `ERROR: start time: <parser message>` or
`ERROR: end time: <parser message>` (`unparsable time: abc`,
`unparsable trailing text: '....5'`, `'start' or 'end' MUST be followed by +|- offset`).
Both relative to each other fails with
`ERROR: the start and end times cannot be specified relative to each other`.
A start after the end fails with
`ERROR: start (<start>) should be less than end (<end>)`; a start equal to
the end is accepted (`time-001` to `-038`).

**GR-31** (bb; compat). `graph_start` and `graph_end` in `graphv` for an
image are the requested start and end. The data window used for the DEFs
and the data formats is aligned to the step: it starts at the step
boundary at or before `--start` and ends at the step boundary at or after
`--end`; each row is stamped with the end of its interval. With step 60,
`--start 1000000000 --end 1000001200` gives the window
999999960..1000001220 (`graph_start`/`graph_end` of the data formats)
and rows stamped 1000000020 to 1000001220 (xport `<start>`, `<end>`;
`time-002`, `-012`, `-039`, `data-002`).

**GR-32** (doc, bb; compat). Without `--step`, the graph asks for one row
per pixel: the step is the archive step that best fits
`(end - start) / width`, and the resulting rows are consolidated further
when there are more rows than pixels. `--step` sets the requested
resolution; values smaller than the step one pixel needs are ignored
(the doc: "a step smaller than one pixel will silently be ignored").
`--step` values that `atoi` reads as 0 or less are ignored too. The data
formats report the effective step (`graph_step`, `<step>`) (`time-039` to
`-059`). A `--step` far larger than the range can make the reference fail
with `ERROR: SANITY CHECK: 18446744073709550176 rows cannot be reduced by 1440 `
(trailing space; `num-097`, `time-053`).

**GR-33** (doc, bb; compat). `--utc` makes the graph use UTC regardless of
`TZ` for strftime output and calendar alignment; it does not change how
`--start`/`--end` are parsed (they follow `TZ`: `time-060` to `-065`).

## 4. Elements: common syntax

**GR-40** (doc, bb; compat). An element is `NAME:field:field...`. Fields
are separated by `:`; `\:` puts a literal colon into a field. The name is
matched exactly and case sensitively against `DEF`, `CDEF`, `VDEF`,
`LINE<width>`, `AREA`, `STACK`, `TICK`, `PRINT`, `GPRINT`, `COMMENT`,
`HRULE`, `VRULE`, `SHIFT`, `TEXTALIGN`, `XPORT`. Any other name fails with
`ERROR: '<name>' is not a valid function name in <element>`, where
`<name>` is the text up to the first `:` (or the whole word)
(`element-001` to `-008`, `def-062`, `line-011`). A field left over after
the element is complete fails with
`ERROR: Unused Arguments "<rest>" in command : <element>`, where `<rest>`
is the unused text with its separators (`def-041` to `-045`,
`print-008`, `-026`, `comment-005`, `tick-014`, `shift-029`).

**GR-41** (doc, bb; compat). Variable names (vnames) are 1 to 255
characters from `A-Z a-z 0-9 _ -`; they may start with a digit and may
equal an RPN operator name (`DEF:MAX=...`, `DEF:PI=...` work). A name
defined twice fails with `ERROR: trying to reuse vname <name>`. A name is
usable only after its definition; elements are processed in order
(`element-017`, `-018`, `def-066` to `-069`). A DEF whose name contains
another character (`x.y`, `x y`, `x!`, a space before or after the name)
is accepted without error but does not define `x`; a later reference to
`x` fails with `undefined vname x` (`def-052` to `-054`, `-059`, `-060`).
A DEF with a 256-character name is accepted (`def-056`); whether the
name is kept whole was not checked.

**GR-42** (bb; compat). An element that refers to an unknown name fails
with `ERROR: <name> is not a vname nor a number` for drawing elements and
rules, `ERROR: undefined vname <name>` for `PRINT` and definitions, and
`ERROR: vname <name> not found` for `SHIFT`. An element whose first field
is missing fails with `ERROR: No positional VNAME`, or for `PRINT`,
`GPRINT` and `COMMENT` without format `ERROR: No positional CF/FORMAT`.

## 5. DEF

**GR-43** (doc, bb; compat).
`DEF:<vname>=<file>:<ds>:<CF>[:step=<s>][:start=<t>][:end=<t>][:reduce=<CF>][:daemon=<addr>]`.

- Missing parts: `ERROR: No argument for definition of DS in <element>`
  when there is no `:` after the file, `... of CF in <element>` when there
  is no CF field. A bare `DEF` is **undefined** (the reference crashes,
  `def-014`).
- CF is `AVERAGE`, `MIN`, `MAX`, `LAST` (case sensitive); anything else
  fails with `ERROR: bad CF: <cf>` (`def-001` to `-008`). Which archive
  serves a CF the file lacks is not covered here; `f.rrd` has all four.
- The file is opened when the graph is computed, after all elements are
  parsed: `ERROR: opening '<file>': <strerror>`,
  `ERROR: reached EOF while loading header rrd->stat_head` for a short
  file, `ERROR: mmaping file '<file>': <strerror>` for a directory
  (platform-specific text), `ERROR: No DS called '<ds>' in '<file>'`.
- `step=` must be a positive integer (`atoi`-style digits only, no
  suffix): otherwise `ERROR: Bad step value: <v>`. `start=` and `end=`
  are at-style times with the GR-30 errors. `reduce=` takes a CF;
  otherwise `ERROR: bad reduce CF: <v>`. Each key may appear once; a
  repeated key, an unknown key or an empty field is an unused argument
  (GR-40). `daemon=` takes an address with escaped colons (GR-26).
- With `-Z`, a missing file or data source gives unknown values instead of
  an error (`def-072`); a bad CF is still an error (`def-074`).

(`def-001` to `-076`.)

**GR-44** (doc, bb; compat). Each DEF keeps its own window and step
(`start=`, `end=`, `step=`); the data preparation, consolidation and the
values of PRINT/GPRINT follow the rules already pinned by
`graph_data_differential.rs` and rpn.md. A CDEF over DEFs with different
windows or steps works on their common window (`def-070`, `-071`).

## 6. CDEF and VDEF

**GR-45** (doc, bb; compat). `CDEF:<vname>=<rpn>` evaluates the RPN of
rpn.md per row. Errors: an empty expression
`ERROR: can not parse an empty rpn expression`; no `=`
`ERROR: don't understand '<text>'`; an unknown token
`ERROR: don't understand '<rest of expression from that token>'`
(`nope,2,*`, `BOGUS`, `x 2 *`, `,x`); `ERROR: RPN stack underflow`; an
expression that names only VDEFs and constants
`ERROR: rpn expressions without DEF or CDEF variables are not supported`.
A trailing comma is accepted (`x,2,*,`). A CDEF that names itself reads
unknown values and gives `nan` (`cdef-019`).

**GR-46** (doc, bb; compat). `VDEF:<vname>=<vname>,<FUNCTION>` or
`=<vname>,<param>,<FUNCTION>`; only one function, no arithmetic (the doc
calls this "only look like RPN expressions"). Functions: `AVERAGE`,
`MAXIMUM`, `MINIMUM`, `LAST`, `FIRST`, `TOTAL`, `STDEV`, `LSLSLOPE`,
`LSLINT`, `LSLCORREL`, and with a parameter `PERCENT`, `PERCENTNAN`.
Errors:

| Input | Error |
|---|---|
| no comma (`x`, `AVERAGE`, empty) | `Comma expected in VDEF definition <rpn>` |
| unknown first name | `Not a valid vname: <name> in line <rpn>` |
| first name is a VDEF | `variable '<name>' not DEF nor CDEF in VDEF '<rpn>'` |
| unknown function | `Unknown function '<f>' in VDEF '<vname>'` |
| anything after the function, or a lower-case function | `Unknown function string '<rest>' in VDEF '<vname>'` |
| `x,2,*` | `Unknown function '' in VDEF '<vname>'` |
| `PERCENT` without parameter | `Function 'PERCENT' needs parameter in VDEF '<vname>'` |
| parameter outside 0..100 | `Parameter '<%f>' out of range in VDEF '<vname>'` |

`MAXIMUM`, `MINIMUM`, `FIRST` and `LAST` carry a time; the others carry
none, so `:strftime` prints the format with every conversion replaced by
dashes of its width (`---------- --:--:--`, `vdef-001`). A VDEF over
data that is all unknown gives `nan` and no time (`vdef-030` to `-034`).
Values: format-strings.md and `vdef_differential.rs`.

## 7. Drawing elements

**GR-50** (doc, bb; compat).
`LINE[width]:<value>[#color[#color2]][:[legend][:STACK][:skipscale][:dashes[=list]][:dash-offset=n]]`.

- `width` is a float read with `strtod` over the rest of the name; `LINE`
  without width is width 1; `LINE 1` and `LINE1e1` are accepted; a
  negative width or trailing text fails with `ERROR: Bad line width: <w>`
  (`line-001` to `-013`).
- `<value>` is a DEF, CDEF or VDEF name or a number (`LINE1:3#ff0000`);
  the empty value fails (GR-42).
- A colour is `#` followed by exactly 3, 4, 6 or 8 hexadecimal digits,
  then the end of the field or a second `#` colour, which is accepted.
  This is stricter than `--color` (GR-22): 7 or 9 digits and trailing
  text are invalid here. An invalid colour is not an error: the element
  is drawn without colour and its legend is dropped (`#ff00000`,
  `#ff0000000`, `#gg0000`, `#`, `#-10000`, `# ff0000`, `#ff0000 `;
  `line-014` to `-028`).
- Without a colour the line is invisible but still counts for scaling and
  stacking (GR-52). Contrary to the manual ("When you do not specify a
  colour, you cannot specify a legend"), a legend given after a missing
  colour is shown (`line-023`, `-058`).
- Flags after the legend: `STACK`, `skipscale`, `dashes`, `dashes=<list>`
  (one value or an even number of comma-separated values),
  `dash-offset=<n>`, `gradheight=<n>`. They are case sensitive; anything
  else is an unused argument. A dash list containing 0 or a negative
  value is accepted and makes the renderer print the GR-24 Cairo warning.

**GR-51** (doc, bb; compat). `AREA` takes the same fields; `dashes` is not
accepted on `AREA` (`ERROR: Unused Arguments "dashes" ...`).
`gradheight=<n>` takes a number; otherwise `ERROR: Bad gradheight: <v>`
(`area-001` to `-025`). The deprecated
`STACK:<vname>[#color][:legend]` stacks on the previous `LINE` or `AREA`;
with none before it the error is
`ERROR: No previous LINE or AREA found for <element>` (`area-021`).

**GR-52** (doc, bb; compat). `:STACK` adds the element's values to the
values of the previous drawing element; the first element may carry
`STACK` and then stacks on zero (`area-019`, `line-062`). Stacked sums
take part in scaling (`line-060`, `scale-193` to `-195`). `skipscale`
leaves the element out of autoscaling (`scale-192`, `-201`, `-202`).

**GR-53** (doc, bb; compat). `TICK:<vname>#color:<fraction>[:legend]`.
Contrary to the manual, the fraction is required: without it the error is
`ERROR: No positional FRACTION`, also for an unknown name (`tick-001`,
`-009`, `-012`, `-013`, `-015`). An empty or non-numeric fraction fails
with `ERROR: error parsing number <vname>` (sic: the vname, not the
fraction; `tick-004`, `-008`). Any number is accepted, including 0,
negative and above 1. A TICK sets the value range to 0..1 when it is the
only data element (`scale-199`).

**GR-54** (doc, bb; compat). `HRULE:<value>#color[:legend[:dashes...]]`
and `VRULE:<time>#color[:legend[:dashes...]]`:

- The value is a number (any `strtod` form, `nan`, `inf`) or a VDEF name.
  A DEF or CDEF name fails with
  `ERROR: Using vname <name> of wrong type in line <element>`. A VRULE
  time is a number of seconds (`1e9` and fractions are accepted), not an
  at-style time: `now` is `ERROR: now is not a vname nor a number`. For a
  VDEF, VRULE uses its time and HRULE its value.
- The colour may be omitted or invalid (the rule is then invisible), as
  for LINE. `STACK` is not accepted on rules (`hrule-029`).
- An HRULE does not take part in autoscaling (`scale-196`); with rules as
  the only elements the range is 0..1 (`scale-197`, `-198`).
- Legends: an HRULE legend is always shown, also when the value is outside
  the y range. A VRULE legend is shown only when the time is within
  `[start, end]`, or with `--force-rules-legend`. This contradicts the
  manual for HRULE ("it will not appear in the legend by default")
  (`hrule-017` to `-020`, `vrule-015` to `-036`, `legend-095`, `-096`).

**GR-55** (doc, bb; compat). `SHIFT:<vname>:<offset>` shifts the data of a
DEF or CDEF by `offset` seconds for everything after it. The offset is an
integer (digits with an optional sign; `60.5`, `1e2`, empty and garbage
fail with `ERROR: error parsing number <offset>`) or a VDEF name, whose
value is used. Shifting a VDEF fails with
`ERROR: Cannot shift a VDEF: '<vname>' in line '<element>'`; no offset
field gives `ERROR: No shift given`; a bare `SHIFT` gives
`ERROR: No argument for definition of vdef/rrd in SHIFT`. Shifting a CDEF
is accepted (`shift-021`). The shifted series keeps the graph's row times:
the data moves by whole steps when the offset is a multiple of the step,
and the offset is rounded down to whole steps otherwise (`SHIFT:x:30`
behaves like `SHIFT:x:0`, a VDEF offset of 11260 like 11220; `shift-001`
to `-034`, `data-059`).

**GR-56** (doc, bb; compat). `TEXTALIGN:left|right|center|justified`
changes the default alignment of legend lines that follow it (GR-66).
Another word fails with `ERROR: Unknown alignment type '<word>'`, no field
with `ERROR: No alignment given` (`textalign-001` to `-010`).

**GR-57** (doc, bb; compat). `XPORT:<vname>[:legend]` is accepted by
`graph` and used by the data formats (GR-86). In a PNG graph an XPORT
with a legend adds one legend line (the image grows by 14 px,
`element-004`) and one without legend adds nothing (`element-003`); the
legend item itself was not inspected with `graphv`. A VDEF name fails with
`ERROR: Cannot shift a VDEF: '<vname>' in line 'XPORT:<vname>'` and a
trailing blank line (`xport-051`).

## 8. Print and text elements

**GR-58** (doc, bb; compat). `PRINT:<vname>:<format>[:strftime|:valstrftime|:valstrfduration]`
for VDEFs and the deprecated `PRINT:<vname>:<CF>:<format>` for DEFs and
CDEFs, with CF `AVERAGE`, `MIN`, `MAX`, `LAST` (case sensitive). A DEF or
CDEF with the VDEF form, or any other CF, fails with
`ERROR: bad CF for DEF/CDEF: <field>`; a VDEF with the CF form fails as an
unused argument (`print-013` to `-024`). Formats and suffixes:
format-strings.md. Each PRINT produces one output line (GR-82). Literal
`\g`, `\l` and `\n` in a PRINT format are printed as is (`print-028` to
`-030`). The deprecated form with `:strftime` is accepted; it has no time
of its own and formats the current wall-clock time, so `print-035` is
tagged `clock` and only required to finish.

**GR-59** (doc, bb; compat). `GPRINT` takes the same fields and errors and
produces a legend entry instead of a print line. `graphv` shows it as
`legend[n]` without the two-space colour-box prefix (`gprint-*`).
`COMMENT:<text>` adds `<text>` to the legend; an empty COMMENT adds
nothing; `COMMENT` without a field fails with `ERROR: No positional CF/FORMAT`.
Legend text and the markers of GR-66 apply to `COMMENT`, `GPRINT` and the
legends of drawing elements alike. A backslash followed by a character
other than the markers at the end of the text fails with
`ERROR: Unknown control code at the end of '<text>'` (`a\\b`, `x\q`); a
lone trailing backslash is kept as text (`comment-006`, `-027`).

## 9. Image size and layout

The numbers in this section are what `graphv` reports and what the size
line prints. They hold for the default fonts of the pinned builds on both
macOS and Linux; text widths come from the font (GR-60).

**GR-60** (bb; platform-specific). Text is measured with the default font,
which in both pinned builds is a monospace face: legend text advances
7 px per character and the y-axis label width is 6 px per character of
`--units-length`. A legend line is 14 px high. Characters are counted, not
bytes: UTF-8 text such as `café` is four characters. Characters outside
the default face (CJK) are measured through font fallback and their
metrics depend on the host (`legend-030`). `--font` changes these metrics
(`--font DEFAULT:12:` gives 10 px legend characters and 20 px lines,
`layout-043`; a family such as `Courier` changes them in a host-dependent
way, `layout-049`).

**GR-61** (bb; compat). With the default south legend and no title, for a
graph of width W and height H:

- `graph_left` = 15 + 6·L, where L is `--units-length` (default 6), so 51.
  `--vertical-label` adds 16. `--y-grid none` sets L to 0 unless
  `--units-length` is given explicitly (`layout-039`, `-041`). L is not
  checked; a negative L that makes `graph_left` negative prints the value
  as an unsigned 64-bit number (`L=-5` gives `18446744073709551601`,
  `layout-038`).
- `graph_top` = 15.
- `graph_width` = W and `graph_height` = H.
- `image_width` = `graph_left` + W + 30; the 30 is 15 of margin and 15 for
  the vertical RRDtool tag, which `--disable-rrdtool-tag` removes (15).
  `--right-axis` adds 15 + 6·L; `--right-axis-label` adds 1 more.
- `image_height` = 15 + H + 26 with no legend line, + 14 per legend line;
  `--no-legend` removes 2 more (139 for the default graph), `--x-grid none`
  removes 17. `COMMENT:\s` adds 6 (GR-66).
- The size line prints `<image_width>x<image_height>`.

(`layout-001` to `-069`, `opt-*` size lines.)

**GR-62** (doc, bb; compat). `--only-graph` makes the image exactly W×H
with `graph_left` and `graph_top` 0 and no legend keys; the title,
labels and legend are dropped (`layout-005` to `-009`, `-066`).
`--full-size-mode` makes W×H the image size and fits the graph area inside
it: `graph_width` = W − (`graph_left` + 30), `graph_height` = H − (the
space above and below as in GR-61, including the legend). When the
remaining area is smaller than 1, it is 1 (`layout-010` to `-013`,
`legend-090` to `-094`).

**GR-63** (bb; compat). A title adds 13 px above the graph for one line
and 15 px for each further line (`\n` and `<br/>` both break the line,
`layout-015` to `-017`); an empty title adds nothing. A watermark adds
11 px at the bottom (`layout-023`). The title font size changes the title
height (`layout-045`).

**GR-64** (doc, bb; compat). `--pango-markup` interprets markup in legend
text for measuring and drawing; `graphv` still reports the legend text
with the markup (`legend-075` to `-079`). Without it, markup is literal
text. Tab characters (literal or `\t`) advance to the next multiple of
`--tabwidth` (default 40 px) from the start of the legend item
(`comment-016` to `-018`, `legend-009`, `-010`).

**GR-65** (doc, bb; compat). Legend position and direction:

- `south` (default): legend lines below the graph, the first line top at
  `graph_top` + H + 19; lines are 14 px apart.
- `north`: legend lines start at y 17 and the graph moves down by the
  legend height plus 9 (`layout-029`, `legend-060` to `-064`).
- `east`/`west`: the legend is a column beside the graph, one item per
  line unless items are on one line without markers (`legend-050` to
  `-059`); `graph_left` grows by 15 for `east` and by the legend width
  for `west`, and `image_height` is the graph height plus 32.
- `bottomup` places the legend lines in reverse order; `bottomup2` does
  the same but keeps the leading and trailing COMMENT lines in place
  (`legend-065` to `-074`).

The vectors pin the exact coordinates of each combination.

**GR-66** (doc, bb; compat). Legend items and lines (default south
legend, legend area from x = 16 to x = `image_width` − 16):

- A drawing element with a colour and a legend shows its legend as two
  spaces (the colour box) followed by the text; so do `HRULE`, `VRULE`,
  `TICK` and `AREA` legends. `GPRINT` and `COMMENT` show the text only.
  Elements without a visible colour or without legend text add no item.
  Legend text longer than 200 bytes is cut to 200 bytes (`comment-020`,
  `-021`).
- `coords[n]` is `x0,y0,x1,y1` of the item's text box; x1 − x0 is the text
  width (GR-60).
- An item ends a line when its text ends with a marker: `\l` (or its alias
  `\n`) left, `\r` right, `\c` centered, `\j` justified. `\s` ends the
  line and adds 6 px of vertical space; `\u` moves back up one line so the
  next items share the previous line (`legend-005`, `-006`). `\g` removes
  the gap that normally follows an item and the trailing spaces of its
  text. `\.` at the end is removed and has no other effect
  (`COMMENT:a\.` shows `a`). Only the last two characters are examined,
  so `COMMENT:b\.\l` ends the line and shows `b\.` (`legend-011`).
- On a left-aligned line, items are separated by 16 px.
- Items on a line without a terminating marker are justified across the
  legend area when there are two or more: the first starts at 16, the
  last ends at `image_width` − 16, and the space between is shared equally
  (`legend-002`, `-031`). A single unterminated item is left-aligned.
  `TEXTALIGN` changes the alignment of unterminated lines that follow it.
- An item that does not fit after the items already on the line starts a
  new line (`legend-014`). An item wider than the legend area gets a line
  of its own and extends past the image edge; it is not cut or wrapped
  (`legend-012`, `-013`, `-087`).

The interaction of `\g` with justification is pinned by the vectors but
not reduced to a rule; see [Unsettled questions](#unsettled-questions).

## 10. Value range

**GR-70** (doc, bb; compat). Default autoscaling. Let *min* and *max* be
the smallest and largest values of the drawing elements that take part in
scaling (not rules, not `skipscale`; STACK sums count), and let `-u`/`-l`
extend them when given (without `--rigid` the limits only widen the
range). Then:

1. If no value is known, the range is 0..1.
2. If min = max, max is increased by 1 % of its magnitude (0..0 becomes
   0..1).
3. Each bound is rounded to a "sensible" value. Let *s* be the larger of
   |min| and |max|, and *k* the largest integer power of the base (1000,
   or 1024 with `--base 1024`) not above *s*. With *v* = bound / *k*, the
   candidates for |v| are 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 1.0, 1.2,
   1.5, 1.8, 2, 2.5, 3, 3.5, 4, 5, 6, 7, 8, 9, 10, 20, 25, 30, 40, 50, 60,
   70, 75, 80, 90, 100, 125, 200, 250, 300, 400, 500, 600, 700, 750, 800,
   900, 1000. The maximum moves up and the minimum moves down to a
   candidate; moving away from zero is inclusive (a value already on a
   candidate stays), moving toward zero is strict (it goes to the next
   candidate). If no candidate lies in the direction of movement (|v|
   below 0.1 toward zero), the bound is kept.
4. The result times *k* is reported as `value_min` and `value_max`.

Examples pinned by black-box sweeps and the `scale` vectors: data 1.333 to
19.333 gives 1.2 to 20; 0 to 1 gives 0 to 1; 3 to 3 gives 2.5 to 3.5;
-3 to -3 gives -3 to -2.5; 1.5 to 2000 gives 1.5 to 2000; 900 to 2000
gives 800 to 2000; 5e-4 computed as a double may round up to 6e-4.

With `--rigid`, the range is exactly `-l`..`-u` (still applying step 2),
whatever the data (`scale-169`, `-176`). Without `--rigid`, `-u`/`-l`
outside the data range widen it and are then rounded like data
(`scale-008`, `-019`, `-096`). `--rigid --allow-shrink` lets the range
shrink below the limits toward the data: `-l 1 -u 2` over data -70..60
gives 0.8..2, `-l 0 -u 100` gives 0..60 (`scale-170`, `-171`); the
general rule is open (see Unsettled questions).

**GR-71** (doc, bb; compat). `--alt-autoscale` sets the range to the data
range widened by 10 % of its width at each end (after step 2 of GR-70;
an all-zero range becomes -0.1..1.1). `--alt-autoscale-min` widens only
the minimum and `--alt-autoscale-max` only the maximum; the other bound is
the data value, unrounded. This contradicts the manual, which says the
other bound is 0 (`scale-002` to `-004`, `-182` to `-184`). Tiny values
(1e-9) give a range that the vectors pin but this rule does not explain
(`scale-079`).

**GR-72** (doc, bb; compat). `--logarithmic` requires positive values: a
`-l` of 0 or less fails with
`ERROR: for a logarithmic yaxis you must specify a lower-limit > 0`. The
range is the smallest positive data value divided by 2 to the largest
value multiplied by 2 (data -70..60 whose smallest positive value is
3.33 gives 1.67..113.3); when no value is positive the range is
`2.5500000000e-09`..`1.0200000000e+01` (`scale-006`, `-050`, `-072`,
`-177` to `-181`).

**GR-73** (bb; compat). `--units-exponent`, `--alt-y-grid` and
`--alt-y-mrtg` do not change the range. `--y-grid` does: with data
-70..60, `-y 2:1` and `-y 0.5:4` give a maximum of 58 and `-y 1:1 -u 3`
gives 57 (`scale-189` to `-191`); the rule is open.

## 11. Output

**GR-80** (doc, bb; compat). The output name `-` writes the image to
standard output and nothing else: no size line, no print lines, no
`--imginfo` line (`out-014`, `-020`, `-021`, `print-038`). `graphv -`
prints its keys and then `image = BLOB_SIZE:<n>`, a line feed and the
image bytes (`out-015`, `print-037`). Any other name is a path; the file
is created or replaced (`out-033`). An existing directory or a path whose
parent does not exist fails after the size and print lines with
`ERROR: Could not save png to '<name>'` and leaves no file (`out-004`,
`-005`). `/dev/null` is accepted.

**GR-81** (doc, bb; compat). The image is `image_width`×`image_height`
pixels, multiplied by `--zoom` and truncated; `graphv` keys and the size
line are not zoomed, `image_info` is (`out-011`, `-020`, `-023`). The
PNG IHDR is the only part of the image bytes the vectors check. Under
`--zoom` the legend `coords` are measured through the zoomed renderer and
differ on Linux from the unzoomed values (`layout-026` to `-028`,
platform-specific; #93).

**GR-82** (bb; compat). Standard output of `graph` with a file name: the
size line, then one line per `PRINT` in element order. GPRINTs print
nothing. Of `graphv`: `print[n] = "<text>"` for each PRINT first, then
`graph_left`, `graph_top`, `graph_width`, `graph_height`, `image_width`,
`image_height`, `graph_start`, `graph_end`, `value_min`, `value_max` (the
last two in `%.10e`), then `legend[n] = "<text>"` and
`coords[n] = "<x0>,<y0>,<x1>,<y1>"` for each legend item in order, then
`image_info = "<text>"` when `--imginfo` is given and the name is not
`-`. Integer keys are printed unquoted.

**GR-83** (doc, bb; compat). `--imginfo <format>` prints one line formed
from the format with the base name of the output file (`%s`), the image
width and height (`%lu`, zoomed). The format check is in
format-strings.md. With the name `-` nothing is printed (GR-80).

Whether the size line is printed does not depend on `--imginfo` itself but
on the argument words: it is left out whenever some argument is exactly
`-f` or `--imginfo`, whatever role that word plays. So `--imginfo=FMT`
and `-fFMT` print the size line and then the imginfo line (`opt-149`,
`-151`, `imginfo-014`, `-015`); `--imginfo FMT` prints only the imginfo
line (`opt-148`, `imginfo-003`); `-t -f` (a title of `-f`, no imginfo)
prints nothing at all (`imginfo-011`, `-012`); `-W --imginfo=x` (a
watermark) prints the size line (`imginfo-016`). An empty format prints
no line at all, so `--imginfo ''` prints nothing (`imginfo-004`).

**GR-84** (doc, bb; compat). `--lazy` computes everything and prints the
size and print lines as usual, but leaves an existing output file in
place when that file is newer than the data: the file keeps its old size
and content even when the new graph would differ (`out-026` to `-031`).
The size line reports the new graph. `graphv --lazy` stops its key list
after `graph_end` (`out-029`). An existing file older than the data is
replaced (`out-032`: the fixture's `notes.txt` is dated 1000000000,
before the last update of `f.rrd`). A newer file that is not a PNG makes
the reference print `libpng error: Not a PNG file` on standard error and
replace it, exit 0 (observed outside the vectors); `/dev/null` gives
`libpng error: Read Error` (`opt-145`, `-146`). Whether "newer than the
data" means the RRD file times or their last update times was not
settled.

**GR-85** (bb; compat). Nothing is written on any error found before
rendering (GR-5). A rendering error from the PNG backend (GR-91) prints
`OOPS: Cairo has issues it can't even die: <reason>` and
`ERROR: Could not save png to '<name>'` on standard error after the size
line, exit 1.

**GR-86** (doc, bb; compat). `--imgformat` XML, XMLENUM, JSON, JSONTIME,
CSV, TSV, SSV turn the graph into a data export of every drawing element
with data (`LINE`, `AREA`, `STACK`, `XPORT`; a `TICK` alone is not
exported), using the legend as column name:

- With a file name the data is written to the file and the run prints
  `0x0` and the print lines (`data-003`, `-004`). With `-` or with a file
  that cannot be opened, the data goes to standard output and the run
  exits 0 (`data-007`). CSV, TSV and SSV lines end in CRLF in both cases;
  fields are quoted with `"`, the time column is not.
- `graphv` prints `graph_start`, `graph_end`, `graph_step` (the aligned
  data window, GR-31), the `print[n]` keys, `image = BLOB_SIZE:<n>` and
  the data (`data-002`).
- With no exportable element the error is
  `ERROR: no XPORT found, nothing to do` (`data-055`, `-057`, `-058`,
  `print-036`).
- XML and JSON add `<prints>`/`"prints"` and `<gprints>`/`"gprints"` with
  the print values and the legend entries by kind (`line`, `area`,
  `gprint`, `comment`). The JSON is not valid JSON as produced (missing
  and doubled commas); it is reproduced byte for byte (`data-015`).
- Legend text is not escaped in XML or CSV and is backslash-escaped in
  JSON (`data-051` to `-053`).
- `--x-grid` with an LFM changes the time column and the `start`/`end`
  meta entries to that strftime format (`data-005`, `-033`); `--step`
  changes the step (`data-006`).

**GR-87** (doc, bb; compat). `--add-jsontime` makes `graphv` append,
after its keys (and after the image bytes with `-`), the lines
`graph_start = <n>`, `graph_end = <n>`, `graph_step = <n>` (the aligned
data window), `datapoints = BLOB_SIZE:<n>` and the JSONTIME export
(`data-062`, `-063`). `graph` ignores it (`data-064`).

## 12. xport

**GR-88** (doc, bb; compat). `xport` options: `--start`/`-s`, `--end`/`-e`,
`--maxrows`/`-m`, `--step`/`-S`, `--json`, `--showtime`/`-t`, `--enumds`,
`--daemon`/`-d`, with the grammar of GR-10 to GR-15. Graph options such as
`-w`, `-a`, `--title`, `-j`, `--utc` are invalid options. `--maxrows`
is read like `atoi` and must be at least 10:
`ERROR: maxrows below 10 rows` (`xport-008` to `-014`). `--step` is read
like `atoi`; 0, negative and garbage are ignored (`xport-017`, `-018`).
`--json=1` fails as a flag with a value.

**GR-89** (doc, bb; compat). `xport` elements are `DEF`, `CDEF`, `VDEF`,
`SHIFT` and `XPORT:<vname>[:legend]`, with the errors of sections 4 to 6.
Drawing, print and comment elements are accepted; they draw nothing but
add `<prints>` and `<gprints>` meta sections as in GR-86 (`element-019`
to `-021`). Without XPORT the
error is `ERROR: no XPORT found, nothing to do`. The XML output starts
with `<?xml version="1.0" encoding="ISO-8859-1"?>`, a blank line and
`<xport>`; `--showtime` adds `<t>` to each row, `--enumds` numbers the
value tags (`<v0>`), `--json` writes the JSON form of GR-86. The legend
is the XPORT legend or empty. Values print as `%.10e` and unknown as
`NaN` (XML) or `null` (JSON) (`xport-001` to `-063`).

## 13. Limits

**GR-90** (bb; platform-specific). Image bytes are not part of the
contract: PNG output differs between hosts and between RRDtool builds
because it depends on Cairo, Pango, FreeType and the installed fonts (the
same `graph -` is 6569 bytes on macOS and 6132 bytes on Linux). What is
compared is the PNG width and height, the size line, the print lines, the
`graphv` keys, the data formats and the error text.

**GR-93** (bb; rondi-policy). Vector formats. The reference writes SVG,
PDF and EPS for `--imgformat SVG|PDF|EPS`, with a size line computed from
the vector backend's text metrics (481x155 where PNG gives 481x141 for
the same elements; `imgformat-004` to `-006`, `out-013`, `-016` to
`-019`). Rondi policy (RD-006): these formats are refused before any
output with `ERROR: RRDtool graph format <FORMAT> is unsupported`,
exit 1.

**GR-91** (bb; rondi-policy). PNG size. The reference renders any image
whose sides are at most 32767 pixels; one pixel more on a side fails in
Cairo with GR-85 after the size line is printed (`limit-001` to `-016`).
It allocates per-pixel memory before that point; RD-016 records 1.64 GB
of peak memory for `-w 20000 -h 20000` (read from
COMPATIBILITY_DEFECTS.md, not re-measured here). Rondi policy (RD-016):
before any data is read, a PNG graph is refused with
`ERROR: PNG graph exceeds the 16777216 pixel safety limit`, nothing on
standard output, when `--width` or `--height` is above 32767 or when the
image after zoom has more than 16777216 (2^24) pixels (`limit-004`,
`-006`, `-009`, `-012`, `-015`, `-016`, `-019`). Below those bounds Rondi
matches the reference, including the Cairo failure of an image side
above 32767 pixels (`limit-002`, `-003`, `-011`). Data formats are not
limited by this rule (`limit-017`).

**GR-92** (bb; rondi-policy). Fetch size. The reference reads any range it
can allocate; `graph`, `graphv` and `xport` fetch the whole range of each
DEF into memory (10 million rows of a 60 s archive need 170 MB upstream).
Rondi policy (RD-017): a DEF whose fetch would return more than 10,000,000
rows fails before any data is read with
`ERROR: unsupported RRD operation: RRD fetch range exceeds the 10 million row safety limit`,
nothing on standard output (`limit-020` to `-025`; the reference prints
the result). `rrdtool fetch` streams and is not limited.

## Rondi policies

| Rule | Reference | Rondi |
|---|---|---|
| GR-93 | writes SVG, PDF, EPS | refuses with `ERROR: RRDtool graph format <F> is unsupported` (RD-006) |
| GR-91 | renders up to 32767 px per side | refuses a graph side above 32767 or an image above 2^24 pixels (RD-016) |
| GR-92 | fetches any range | refuses more than 10 million rows per fetch (RD-017) |

The replay test requires Rondi's standard error to contain the policy
text for each of these vectors.

## Platform notes

The Linux expectations (`expect_platform`) differ from macOS in these
cases, all recorded from the reference:

| Vectors | Linux result |
|---|---|
| `daemon-003` | glibc resolver text `Name or service not known` |
| `print-040` | `%Z` under `--utc` prints `GMT` instead of `UTC` |
| `def-072`, `-075`, `cdef-019`, `print-032`, `-033`, `vdef-030` to `-034`, `limit-020`, `-022`, `-024` (x86_64) | unknown values print as `-nan` (format-strings.md) |
| `gprint-014` (x86_64) | `-nan` in a legend |
| `font-003`, `font-019`, `layout-049` | other font-family metrics |
| `legend-030` | fallback-font metrics for CJK text |
| `layout-026` to `-028` | legend `coords` under `--zoom` |
| `opt-145`, `-146` (x86_64) | no `libpng error: Read Error` for `--lazy /dev/null` |
| `num-076` (x86_64) | a different Cairo error for `--zoom nan` |
| `num-078` (x86_64) | Pango warnings with a process id: tagged `unstable` |
| `x-grid-009` (x86_64), `x-grid-010` | the reference crashes: tagged `undefined` |

## Known divergences

Vectors that fail against Rondi at the time of writing are listed in
`KNOWN_DIVERGENCES` in `crates/rondi-cli/tests/graph_spec_vectors.rs` with
their class; the expectation stays RRDtool's. They are summarized here.

| Vectors | Rule | Class | Rondi today |
|---|---|---|---|
| `imgformat-004` to `-006`, `out-013`, `-016` to `-019` | GR-93 | policy | refuses SVG, PDF, EPS |
| `limit-004`, `-006`, `-009`, `-012`, `-015`, `-016`, `-019` | GR-91 | policy | refuses large PNG graphs |
| `limit-020` to `-025` | GR-92 | policy | refuses fetches over 10 million rows |
| `grammar-055`, `-056`, `-061`, `out-008` | GR-3 | defect | prints `0x0` for a graph with no elements |
| `opt-145`, `-146` (macOS, Linux aarch64) | GR-84 | defect | no `libpng error: Read Error` |
| `out-027`, `-031` | GR-84 | defect | `--lazy` replaces a newer output file |
| `out-029` | GR-84 | defect | `graphv --lazy` prints every key |
| `vertical-label-002` | GR-61 | defect | a 300-character vertical label panics (subtract overflow) |
| `formatter-007`, `-008` | GR-25 | defect | a value format under the duration formatter is not rejected |
| `def-073` | GR-43 | defect | `-Z` with a missing DS gives `nan`; the reference prints another value (`-5.438596`) |
| `cdef-019` | GR-45 | defect | a CDEF that names itself is refused instead of giving `nan` |
| `grid-dash-011`, `line-044`, `-045`, `hrule-027`, `-028`, `vrule-027`, `-028` | GR-24, GR-50, GR-54 | defect | no Cairo warning for zero or negative dashes |
| `font-003`, `layout-049` (macOS); `font-019` (Linux) | GR-60 | defect | one font's metrics for every family |
| `legend-030` | GR-60 | defect | CJK fallback metrics |
| `legend-079` | GR-64 | defect | Pango markup measured as text |
| `layout-026` to `-028` (Linux) | GR-81 | defect | legend `coords` under `--zoom` |
| `data-005`, `-012`, `-019`, `-026`, `-033`, `-040`, `-047` | GR-86 | defect | `--x-grid` time format ignored in data exports |
| `data-062`, `-063` | GR-87 | defect | `--add-jsontime` ignored |

`def-073` is listed as a defect, but the reference value comes from a
data source the DEF did not name; whether that is a reference defect to
specify as undefined is open.

## Unsettled questions

These were not settled by documentation or the black-box runs made for
this specification. The vectors pin the observed results; an
implementation that matches the vectors without a general rule is
acceptable, and a rule found later should be added here with its
provenance.

1. **Justification with `\g`.** For three COMMENT items `a b c` on an
   unterminated line, `\g` on the first moves the second and third 16 px
   left (b at 221 instead of 237, c ending at 449); on the second it moves
   b 8 px right and leaves c; on the third it moves b 8 px right and c
   16 px past the legend edge (each after an unlabelled `LINE1`). The
   vectors `legend-031` to `-044` pin these and related cases; the general
   formula is open.
2. **Autoscale of tiny ranges with `--alt-autoscale`**: data near 1e-9
   gives -0.11..0.11 (`scale-079`).
3. **East and west legend widths** are pinned by `legend-050` to `-059`
   and `layout-030`, `-031`, `-068`; the width formula (longest line plus
   margins) was not derived.
4. **Legend coordinates under `--zoom` on Linux** were reported in #93 to
   differ from macOS; the `layout` zoom cases record both platforms.
5. **Fonts other than the default.** `--font` with a family name and
   `RRD_DEFAULT_FONT` change text metrics in a host-dependent way; only the
   default monospace metrics of GR-60 are specified.
6. **`--tabwidth`** below 1 and **`--x-grid` with a zero step** depend on
   undefined reference behavior (Pango warnings with a process id; a
   crash). Rondi finishes without a signal; the outputs are not compared.
7. **`--rigid --allow-shrink`** and **`--y-grid`** change the value range
   in ways pinned by `scale-170`, `-171` and `scale-189` to `-191` but not
   reduced to a rule.
8. **`--lazy` freshness**: whether the output file is compared with the
   RRD files' modification times or with their last update times.
9. **`-Z` with a missing data source** (`def-073`) prints a value from
   another data source of the same file; it may be a reference defect.
10. **Message length**: option errors are cut to 47 bytes after `ERROR: `
    (GR-12); whether `invalid option` messages are cut the same way was not
    tested with a long enough name.
