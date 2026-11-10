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
  `--add GROUPS` captures the named groups and keeps the rest of the
  table; a re-captured group keeps its place, and a case whose steps and
  environment are unchanged keeps its Linux expectations and tags. A case that runs `--lazy` on `/dev/null` is tagged `host`: its
  result depends on whether the host keeps the device's modification
  time current (GR-84), so the replay compares it with the live oracle
  only.

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
| The text after `mmaping file '<path>': ` becomes `{ERRNO}` | mapping a directory fails with EINVAL or ENODEV depending on the file system (GR-43) |

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
| `fit` | the fitted legend and frame rules of GR-60 to GR-66 at several font sizes, and random combinations from a fixed seed |
| `range` | the value range rules of GR-70 to GR-73 found by black-box probes |
| `up` | `\u` lines after and between other lines at several legend sizes (GR-66) |
| `prep` | data preparation: fetch steps, reduction, CDEF windows, the export grid, VDEF windows and SHIFT (GR-47 to GR-49) |
| `data` | `--imgformat` XML, XMLENUM, JSON, JSONTIME, CSV, TSV, SSV |
| `xport` | `xport` options, elements and output |
| `out` | output files, `-`, `graphv -`, `--imginfo` output, `--lazy` |
| `limit` | image size and fetch size limits (RD-016, RD-017) |
| `order` | the order in which option values and times are checked (GR-17) |
| `scan` | how `--font`, `--color`, `--x-grid`, `--y-grid`, `--right-axis`, `--grid-dash` and `--right-axis-range` read their values |
| `lazy` | `--lazy` against files the previous step wrote (GR-84) |
| `jsontime` | `--add-jsontime` with an output file (GR-87) |
| `xgridtime` | the `--x-grid` label format in data exports (GR-86) |
| `ruleexport` | `HRULE` and `VRULE` in the `<rules>` section of XML and JSON exports (GR-99) |
| `duration` | a value format under the `duration` axis formatter (GR-25) |
| `repeat` | options that may be given once, `--graph-type` and `-I` (GR-18, GR-19) |

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

**GR-17** (bb; compat). Each option value is checked when the option is
read, with these exceptions, which are checked after the last option in
this order:

1. `--left-axis-format`, then `--right-axis-format` (GR-25), each against
   the formatter given last for its axis, wherever it appears;
2. `--logarithmic` with a `--lower-limit` of 0 or less (GR-72;
   `order-018`, `-019`);
3. the start and end: relating them (`the start and end times cannot be
   specified relative to each other`), then
   `ERROR: the first entry to fetch should be after 1980 (<start>)` for a
   start below 315360000 (time-spec.md TS-61), then
   `start (<start>) should be less than end (<end>)`;
4. `missing filename` (GR-2);
5. `--imginfo` (GR-83), which is checked only after the data was fetched
   and the print elements were formatted, so an error in either wins
   (`order-016`).

A start or end that cannot be parsed fails when its option is read
(`--start abc -w 5` reports the start, `-w 5 --start abc` the width). The
same order holds for `xport` (`order-001` to `-019`; `order-017` is the
`xport` case).

**GR-18** (bb; compat). Three options may be given only once: a second
`--units` fails with `ERROR: --units can only be used once!`, a second
`--add-jsontime` with `ERROR: --add-jsontime can only be used once!` and
a second `--daemon` (also `graph --daemon ''` followed by another, and
in `xport`) with `ERROR: You cannot specify --daemon more than once.`,
each when the repeated option is read (`repeat-001` to `-006`, `-018`).
Other options may repeat (GR-15).

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
| `--graph-type` | | `TIME` or `XY` | GR-19 |
| | `-I` | flag | accepted, no observable effect (`repeat-014`); no long form |

**GR-19** (bb; compat). `--graph-type` accepts `TIME` (the default) and
`XY`, case sensitively; any other value fails when read with
`ERROR: unsupported graphics type '<v>'` (`repeat-008` to `-010`). `XY`
is accepted while reading options but fails once a graph is laid out,
before anything is printed, with `ERROR: XY diagram not implemented`
(`repeat-011`). It does not fail without graph elements or for a data
format, and a later `--graph-type TIME` cancels it (`repeat-012`,
`-013`, `-015` to `-017`).

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

The tag is a run of 1 to 10 upper-case letters followed by `#`; 11 or
more letters, or any other character before `#`, are
`invalid color def format`. The value after `#` is read like a C `%x`
conversion and its length is the number of characters that conversion
consumed: leading white space, a sign and a `0x` or `0X` prefix count
(`# 123` and `#0X1` have length 4 and 3 and are accepted, `#-1020` has
length 5 and is refused). A value with no hexadecimal digit after the
sign and prefix is `invalid color def format` (`#--1020`, `#-`). Text
after the digits is ignored, including a second `#` (`color-001` to
`-030`, `scan-029` to `-046`).

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
is the longest run of the characters `0123456789.e+-` after the tag's
`:`, and the whole run must convert as in GR-21; otherwise the error is
`invalid text property format` (`1.2.3`, `1-`, `1ee`, and an empty run
as in `nan`, ` 10` or `e5`). Upper-case `E` ends the run. What follows
the run:

- nothing, or at most two characters not starting with `:`: accepted,
  without a font name (`10y`, `1;:`);
- `:` and any text: the text is the font name, which may contain `:` and
  spaces;
- three or more characters not starting with `:`:
  `ERROR: expected : after font size in '<value>'` (`0x10`, `10;Arial`,
  `10x:Arial`, `1E5x`).

The tag is 1 to 10 upper-case letters; with 11 the value is
`invalid text property format`. The tag is checked after the size, so
`BOGUS:abc` is `invalid text property format` and `BOGUS:10yyy` is
`invalid fonttag 'BOGUS'`. A size of 0 or less is accepted and changes
nothing. `DEFAULT` sets the size of every text, and the options apply in
order, so `LEGEND:12 DEFAULT:10` makes everything 10 and
`DEFAULT:10 LEGEND:12` keeps the legend at 12 (`font-001` to `-019`,
`scan-001` to `-028`).

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
  finishes (`x-grid-001` to `-019`). An LFM of 120 characters is
  **undefined**: it is accepted, but the formatted label overruns the
  reference's label buffer, and on the CI runner host Pango then prints
  `Invalid UTF-8 string` warnings with a process id (`x-grid-020`).
  The value is read like the C `scanf` format
  `%10[A-Z]:%d:%10[A-Z]:%d:%10[A-Z]:%d:%d:`: keywords are 1 to 10
  upper-case letters, the numbers may have leading white space and a sign
  but no fraction, exponent or hexadecimal prefix, and anything else is
  `invalid x-grid format`. The three keywords are then looked up in order
  and the first unknown one is reported (`scan-047` to `-055`).
- `--y-grid none` turns the y grid off (GR-61). Otherwise `step:factor`
  read as a float and an integer; trailing text after them is ignored
  (`1:2:3`). The step is the longest run of `0123456789.e+-` at the start
  of the value, which must be followed by `:`; the run is then converted
  as in GR-21 with the option name `-y`
  (`ERROR: option -y - Converted '1-' to 1.000000, but cannot convert '-'`).
  The factor is read like C `%d` (leading white space and a sign allowed,
  at least one digit) and kept modulo 2^32 as a signed 32-bit integer, so
  `2147483648` is a factor below 1 and `4294967297` a factor of 1
  (`scan-056` to `-073`). Fewer fields, `NONE` or garbage fail with
  `ERROR: invalid y-grid format`; a step of 0 or less fails with
  `ERROR: grid step must be > 0`; a factor of 0 or less with
  `ERROR: label factor must be > 0` (`y-grid-001` to `-012`).
- `--grid-dash on:off`, two floats; anything else is
  `ERROR: invalid grid-dash format`. A third field is ignored. Negative
  values are accepted and make the renderer print
  `OOPS: Cairo has issues it can't even die: invalid value for a dash setting`
  on standard error while the graph still succeeds (`grid-dash-001` to
  `-012`). Each number is a run of `0123456789.e+-` as for `--y-grid`; a
  value without that shape is `invalid grid-dash format`, and a run that
  does not convert in full is
  `ERROR: expected grid-dash format float:float` (`1-:2`, `1:2-`;
  `scan-082` to `-088`).
- `--right-axis scale:shift`, two floats with trailing text ignored. Fewer
  fields or garbage give
  `ERROR: invalid right-axis format expected scale:shift`; a scale of 0
  gives `ERROR: the second_axis_scale  must not be 0` (two spaces).
  Both numbers are runs as for `--y-grid`; a run that does not convert in
  full is also `invalid right-axis format expected scale:shift`
  (`scan-074` to `-081`).
- `--right-axis-range min:max`, either side may be empty. A value without
  `:` gives `ERROR: invalid right-axis-range format expected min:max`; a
  side that is not a number gives the GR-21 conversion error with
  `--right-axis-range`; min greater than max gives
  `ERROR: right-axis-range min must not be larger than max`. `nan` is
  accepted (`right-axis-range-001` to `-010`). The value is split at the
  first `:` and each side is converted whole, so `1:2:3` reports `'2:3'`
  and `1 :2` reports the trailing space (`scan-089` to `-092`).

**GR-25** (bb; compat). Axis formatters and formats:

- With the `numeric` formatter (the default), `--left-axis-format` and
  `--right-axis-format` must be a value format as defined in
  format-strings.md (FS rules for axis formats); an empty format is
  accepted. Errors print
  `ERROR: invalid format string '<f>' (should match '^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*$')`.
- With `timestamp`, the format is a strftime text and is not checked.
- With `duration`, the format must be a valid duration format
  (format-strings.md FS-40 to FS-44; an empty format is valid). An
  invalid one does not stop the graph: the image is drawn and written and
  every line and key is printed as usual, including `legend[n]`,
  `coords[n]` and `image_info`, and the run then fails with
  `ERROR: Wrong duration format`, exit 1 (`formatter-007`, `-008`,
  `duration-001`, `-006`, `-008`, `-013` to `-015`). A failed save is
  reported instead (`duration-016`). The format is only used, and so only
  fails, when the axis labels are drawn: not with `--y-grid none`,
  `--only-graph` or `--logarithmic`, not for the right axis without
  `--right-axis`, not for an abandoned graph (GR-21) and not for a data
  format (`duration-002` to `-005`, `-010` to `-012`).
- The check uses the formatter given last, wherever it appears relative
  to the format (GR-17; `order-009`, `-010`).

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

**GR-95** (bb; compat). Named and positional fields. A field `key=value`
is a named field when the element knows `key`; when a known key appears
more than once, the last one is the named field and the earlier ones are
positional. Every other field is positional, in order. The keys are:

| Element | Keys |
|---|---|
| DEF | `vname`, `rrd`, `ds`, `cf`, `step`, `start`, `end`, `reduce`, `daemon` |
| CDEF, VDEF | `vname`, `rpn` |
| SHIFT | `vname`, `shift` |
| XPORT | `vname`, `legend`, `color` |
| PRINT, GPRINT | `vname`, `cf`, `format`, `vformatter`, `strftime` |
| COMMENT | `legend` |
| TEXTALIGN | `align` |
| HRULE, VRULE | `vname`, `legend`, `color`, `dashes`, `dash-offset` |
| TICK | `vname`, `legend`, `color`, `fraction` |
| AREA | `vname`, `legend`, `color`, `gradheight`, `skipscale`, `stack` |
| LINE | `vname`, `legend`, `color`, `dashes`, `dash-offset`, `skipscale`, `stack` |
| STACK | `vname`, `legend`, `color` |

`cmd` and `debug` are known to every element.

- The element name is the value of the last `cmd=` field, or else the
  first field without `=`; with neither the element fails with
  `ERROR: no command set in argument <element>`.
- More than 10 fields without `=`, the name among them, fail with
  `ERROR: too many positional arguments`, before the name is looked at.
- Each argument comes from its named field, or else from the next
  positional field. The legend, the format and the PRINT name take the
  whole positional field; the name of a drawing element or rule, the
  TICK fraction, the XPORT name, the PRINT CF, the TEXTALIGN word, the
  SHIFT name and offset and the DEF data source, CF and file take the
  text after its first `=`. For DEF, CDEF and VDEF a positional
  `<name>=<rest>` gives the name and the definition; a positional field
  without `=` gives an empty name.
- An empty named value is an empty argument (`vname=` gives
  `vname  not found`, `align=` gives `Unknown alignment type ''`, `cf=`
  gives `bad CF: `), except COMMENT `legend=` and PRINT `format=`, which
  count as absent.
- PRINT and GPRINT: an unknown name given by key fails with
  `ERROR: vname <name> not found`, a bad CF given by key with
  `ERROR: bad CF: <cf>`; a missing CF for a DEF or CDEF with
  `ERROR: No positional CDEF`. A `cf=` is ignored for a VDEF. Of
  `valstrftime`, `valstrfduration` and `vformatter=timestamp|duration`
  the last one given applies and the others are unused arguments;
  another `vformatter=` value fails with
  `ERROR: Unsupported vformatter: <value>`. `strftime=`, `stack=` and
  `skipscale=` set their flag whatever the value.
- `color=` takes one colour without `#` and replaces a `#colour` after
  the name; an invalid colour hides the legend as for the `#` form
  (GR-50). XPORT accepts and ignores `color=` and a `#` suffix. On the
  deprecated STACK an invalid `color=` fails with
  `ERROR: STACK should already be turned into LINE or AREA here`, after
  the variable is checked and before the earlier LINE or AREA is looked
  for.

Provenance: black-box compatibility tests (`graph_data_differential`
element grammar, `alias_modes` keyword cases) and probes of each key.

**GR-96** (bb; compat). `debug=<integer>` is accepted on every element; a
value that is not an integer fails with `ERROR: Bad debug value: <value>`
before the element's other fields are checked. A DEF with a value of 1,
once its fields passed their checks, writes this block to standard
error: a line of 33 `=`, then `DEF   : <element>`, `VNAME : <name>`,
`RRD   : <file>`, `DS    : <ds>`, `CF    : <n>` (0 AVERAGE, 1 MIN, 2 MAX,
3 LAST), `START : (<start>)`, `STEP  : (<step= or the image step>)`,
`END   : (<end>)`, `REDUCE: (<n of reduce=, else 0>)`,
`DAEMON: <address>`, and the line of `=` again. Other values and other
elements: Unsettled questions.

**GR-41** (doc, bb; compat). Variable names (vnames) are 1 to 255
characters from `A-Z a-z 0-9 _ -`; they may start with a digit and may
equal an RPN operator name (`DEF:MAX=...`, `DEF:PI=...` work). A CDEF or
VDEF whose name is already defined fails with
`ERROR: trying to reuse vname <name>`; a DEF does not (GR-34). A name is
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
`XPORT` with an unknown name fails like a drawing element (`element-044`).

**GR-34** (bb; compat). A DEF may take a name that an earlier DEF, CDEF
or VDEF already defined. This is not an error. The later DEF is still
read when the data is fetched, so its file and data source errors are
reported (GR-35), but every reference to the name, before or after it,
resolves to the first definition: a later `DEF:c=...` leaves a CDEF `c`
in place, a later `DEF:v=...` leaves the VDEF `v` in place (so
`PRINT:v:%lf` still works and `PRINT:v:MAX:%lf` is still an unused
argument), and a later `DEF:x=...` with another data source or `step=`
does not change what `x` reads (`def-065`, `-077` to `-084`).

**GR-35** (bb; compat). Evaluation order. A run goes through these
phases, and the first error stops it with that error only:

1. Options, wherever they stand among the elements (GR-17): an invalid
   option or option value after an invalid element is reported, not the
   element (`element-041`, `-042`).
2. Elements, in command-line order: the element name and fields, DEF
   option values (`start=abc`), RPN and VDEF syntax, references to names
   (GR-41, GR-42), SHIFT offsets and name reuse. No file is opened in
   this phase, so an invalid element after a DEF of a missing file wins
   over the missing file (`element-022`, `-023`, `-026` to `-030`, `-034`,
   `-035`, `-040`, `-043`, `-044`).
3. The DEFs are read in element order; the first DEF that fails ends the
   run, whatever comes before or after it (`element-024`, `-025`, `-036`,
   `def-082`, `-083`).
4. The formats of `PRINT` and `GPRINT`, in element order, both kinds
   together: the first invalid format wins (`element-031`, `-032`, `-037`,
   `-038`).
5. The `--imginfo` format (GR-17, GR-83): checked only after the print
   formats passed (`element-033`, `-039`).

`xport` follows the same order for its elements (`element-043`, `-044`).

**GR-36** (doc, bb; compat). The kinds of name each element accepts. Every
name must be defined by an earlier element (GR-41).

| Element | DEF or CDEF | VDEF | Number |
|---|---|---|---|
| CDEF expression | yes | yes, the same value in every row | yes; GR-45 needs one DEF or CDEF |
| VDEF | yes | no (GR-46) | no: `Not a valid vname: <n> in line <rpn>` (`element-055`) |
| `LINE`, `AREA`, `TICK`, `STACK` value | yes | yes (`element-045` to `-047`, `-051`) | yes (GR-50, `element-052`) |
| `HRULE`, `VRULE` | no (GR-54) | value or time | yes |
| `PRINT`, `GPRINT` | CF form (GR-58) | VDEF form | no: `undefined vname <n>` (`element-053`) |
| `SHIFT` series | yes | no (GR-55) | no: `vname <n> not found` (`element-054`) |
| `SHIFT` offset | no (GR-38) | its value | yes |
| `XPORT` | yes | no (GR-57) | no: `Encountered unknown type variable '' in line '<element>'` (`element-056`) |

A CDEF may mix DEFs, CDEFs and VDEFs (`element-048`, `-049`); a CDEF may
be shifted by a VDEF (`element-050`).

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
  file, `ERROR: mmaping file '<file>': <strerror>` for a directory,
  `ERROR: No DS called '<ds>' in '<file>'`. The directory error is
  platform-specific: `Invalid argument` (EINVAL) on macOS and on the Linux
  file system of the local oracle container, `No such device` (ENODEV) on
  the CI runners' Linux file system, so it follows the file system rather
  than the OS; a replacement reports the error the host's `mmap` returns,
  and the vectors mask that text (`def-019`).
- `step=` must be a positive integer (`atoi`-style digits only, no
  suffix): otherwise `ERROR: Bad step value: <v>`. `start=` and `end=`
  are at-style times with the GR-30 errors. `reduce=` takes a CF;
  otherwise `ERROR: bad reduce CF: <v>`. A repeated key, an unknown key
  or an empty field is positional (GR-95) and so usually an unused
  argument (GR-40). `daemon=` takes an address with escaped colons
  (GR-26).
- The checks run in this order: `debug=` (GR-96), `reduce=`, `step=`,
  `start=` and `end=`, the start, the window order, then the data source
  and the CF. Each DEF's own start must not be before 1980:
  `ERROR: the first entry to fetch should be after 1980 (<start>)` for a
  start below 315360000. A start after the end fails with
  `ERROR: start (<start>) should be less than end (<end>)`.
- A file name starting with `cb//` is never opened nor sent to a daemon:
  it fails when the DEFs are read with
  `ERROR: use rrd_fetch_cb_register to register your callback prior to calling rrd_fetch_fn_cb`,
  and with `-Z` gives unknown values.
- With `-Z`, a missing file or data source gives unknown values instead of
  an error (`def-072`); a bad CF is still an error (`def-074`), and so is
  a fetch over the GR-92 limit. A file that cannot be read under `-Z` also
  clears an earlier unreachable-daemon error (GR-26); a missing data
  source does not.

(`def-001` to `-076`.)

**GR-44** (doc, bb; compat). Each DEF keeps its own window and step
(`start=`, `end=`, `step=`); the data preparation is GR-47 to GR-49 and
the CDEF arithmetic rpn.md. A CDEF over DEFs with different windows or
steps works on their common window (`def-070`, `-071`, GR-48).

**GR-98** (bb; compat). Fetch reuse. A DEF takes the data an earlier DEF
read, instead of reading its file, when both have the same file name as
written, the same CF, the same `reduce=` CF (AVERAGE when not given),
the same window start and end compared in their low 32 bits, and the
same daemon address, and the later DEF's `step=` (or the image step)
equals the earlier DEF's step after reduction. The data source is not
compared: the later DEF reads its own data source from the earlier
fetch, reduced as the earlier DEF reduced it. The daemon request of
GR-37 is still made. Provenance: black-box compatibility test
(`graph_engine_parity` fetch reuse) and probes changing each part.

**GR-37** (bb; compat). Reading through rrdcached. With `--daemon`, every
DEF is read through that daemon; a DEF with `daemon=` is read through its
own address, and the other DEFs follow `--daemon` or read the local file.
Each DEF sends one `FETCH <file> <CF> <start> <end>` request, in element
order, after all elements are parsed (GR-35): `<file>` is the DEF's file
as an absolute path with `.` and `..` resolved, and `<start>` and
`<end>` are the DEF's own window (its `start=`/`end=` or the graph's
`--start`/`--end`) as given, not aligned to a step. The step, the reduce
CF and the data source name are not sent. Requests are never merged: two
identical DEFs, a DEF that reuses a name (GR-34) and DEFs of one file that
differ only in data source, CF, `step=`, `start=` or `reduce=` each send
their own request. Graph and xport send no `FLUSH`, so the daemon's
`STATS` counters `FlushesReceived` and `UpdatesWritten` do not move. In
these runs, DEFs that use the same address shared one connection. A fetch
the daemon refuses ends the run with
`ERROR: rrdcached@<address>: <daemon message>`; with `-Z` a missing file
gives unknown values as for a local file. A data source missing from the
file is reported after its fetch as `ERROR: No DS called '<ds>' in '<file>'`
with the file name as written. A daemon that cannot be reached is GR-26.
Provenance: black-box runs of the reference against `rrdcached` 1.11.0
through a logging proxy on its Unix socket; there are no vectors, because
the replay harness runs no daemon. Whether `RRDCACHED_ADDRESS` behaves
like `--daemon` here, and how DEFs with different addresses share
connections, were not checked.

**GR-47** (bb; compat). Fetch and reduction of a DEF. The image step is
the larger of `--step` and (end − start) / width for `graph` (the width
of the graph area in pixels) and (end − start) / `--maxrows` for `xport`.
The DEF reads its file for its window and for its `step=`, or without
one the image step, the way `rrdtool fetch` does (the CF's best archive;
rows stamped at the end of their interval). The wanted step of the DEF
is the larger of its `step=` and the image step: a `step=` finer than the
image step chooses the archive but does not refine the result, as
`--step` below one pixel is ignored (GR-32). With an 18000 s window,
`-m 10` or `-w 10` and `step=300`, the 300 s archive is read and reduced
to 1800 s, so the first and last buckets are unknown where the 1800 s
archive read without `step=` has values (`prep-096` to `-099` against
`prep-039`; `limit-020` to `-027` print `nan`, `0.000000` and 11 rows
for the same reason). When the archive step *a* is smaller than the
wanted step, rows are combined:

- the new step is f·*a* with f = ceil(wanted / *a*) (`step=1000` over a
  300 s archive gives 1200, `prep-007`);
- buckets end at multiples of the new step since the epoch, and each
  holds the f archive rows that end in it;
- a bucket that reaches before the first or after the last fetched row
  is unknown (`prep-005`, last row);
- otherwise it is the reduce CF (`reduce=`, by default the DEF's CF) of
  the bucket's known values: their average, maximum, minimum or last
  value; unknown when none is known (`prep-017` to `-024`).

When the window holds less than one new step, the DEF fails with
`ERROR: SANITY CHECK: <n> rows cannot be reduced by <f> ` (with a
trailing space and an empty line), where *n* is
(floor(*e*/new step) − ceil(*s*/new step))·new step / *a* for the
fetched start *s* and end *e*, printed as an unsigned 64-bit number
(`prep-015`, `-016`; GR-32).

**GR-48** (bb; compat). Series on a common grid. A CDEF's step is the
greatest common divisor of the steps of the series it names, and its
window runs from the latest start to the earliest end among them. Each
row at time *t* takes from each series the row whose interval contains
*t*; a series has no value outside its window (`prep-042` to `-059`).
When the latest start is after the earliest end the CDEF fails with
`ERROR: malloc im->gdes[gdi].data`; when they are equal it has no rows.
A DEF window that starts or ends inside an archive step covers the whole
step: with a 300 s archive, `start=1000007450` (inside the step ending at
1000007700) gives a first row stamped 1000007700, and `end=1000014450`
(inside the step ending at 1000014600) a last row stamped 1000014600;
`start=1000007400` also gives 1000007700 and `end=1000014600` gives
1000014900. That is, the first and last rows are stamped
floor(*t*/*a*)·*a* + *a* for *t* the start and the end (`prep-087` to
`-095`). The CDEF and export rules above then apply unchanged.
The exported series of `xport` and of the data formats share one grid:
its step is the greatest common divisor of their steps, it starts at the
image start rounded down to that step and ends at the image end rounded
up to it, and its rows are stamped at the end of each step (`<start>` is
the first stamp, GR-31). A series reads as unknown outside its own
window. The data formats of `graph` use the width for the image step
(`prep-080` to `-082`).

**GR-49** (bb; compat). A VDEF and the deprecated `PRINT`/`GPRINT` with a
CF read every row their DEF or CDEF holds, including rows outside the
image window, so a DEF with its own `start=`/`end=` or `step=` gives
values and times over that window (`prep-060` to `-062`, `-095`). `SHIFT`
(GR-55) moves the series by its offset truncated toward zero to whole
steps of that series: with a 300 s step, 150 and -150 do not move it and
450 moves it by 300 (`prep-063` to `-079`).

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
| a parameter with a function that takes none | `Function '<f>' needs no parameter in VDEF '<vname>'` |
| `PERCENT` without parameter | `Function 'PERCENT' needs parameter in VDEF '<vname>'` |
| parameter outside 0..100 | `Parameter '<%f>' out of range in VDEF '<vname>'` |

`MAXIMUM`, `MINIMUM`, `FIRST` and `LAST` carry a time; the others carry
none, so `:strftime` prints the format with every conversion replaced by
dashes of its width (`---------- --:--:--`, `vdef-001`). A VDEF over
data that is all unknown gives `nan` and no time (`vdef-030` to `-034`).
Values: format-strings.md and `vdef_differential.rs`.

The part after the first comma is read so: when it starts with digits,
`.`, `e`, `+` or `-` (up to 40 of them), those are the parameter. Without
a comma right after them the function is `''`. A parameter that is not a
decimal number with an optional sign and exponent (a bare `e` or `e+` at
the end is ignored), or that is not finite, makes the whole part an
unknown function string. The function is then the leading run of up to
29 upper-case letters, plus on macOS lower-case `b` to `z` (on Linux a
lower-case letter ends the run, so `95,PERCENTfoo` is `PERCENT`); anything
after it is ignored. Without a parameter the part must be such a run and nothing
else. The `Unknown function`, `needs parameter` and `needs no parameter`
errors end with an empty line. Provenance: black-box compatibility test
(`rpn_differential` VDEF parameter lexing) and probes of each form on the
pinned macOS and Linux aarch64 oracles.

**GR-100** (bb; compat, platform-specific). VDEF arithmetic. The pinned
builds do not all round each multiply and add separately: some products
are fused with the following add (rounded once), and which ones differs by
platform. Let *y* be the known (finite) values, *i* the index of each in
the series (unknown rows keep their index, so gaps count), *n* the number
of known values and *fma(a, b, c)* the once-rounded *a*·*b* + *c*. The
multiply-add of update.md UP-35 is fused on macOS arm64 and Linux aarch64
and separately rounded on x86_64; it is written *ma* below.

- `AVERAGE` and `TOTAL` sum the values starting from +0, so a series of
  only `-0` values sums to +0. `TOTAL` is that sum times the step.
- `STDEV` is the population deviation √(Σ(*y* − avg)² / *n*). On Linux
  aarch64 each square is fused with the running sum,
  *s* = fma(*d*, *d*, *s*); on macOS arm64 and on x86_64 the square is
  rounded before the add.
- `LSLSLOPE`, `LSLINT` and `LSLCORREL` accumulate Σ*i* and Σ*y* by plain
  addition, Σ*iy* = *ma*(*i*, *y*, Σ*iy*) and Σ*yy* = *ma*(*y*, *y*, Σ*yy*),
  and Σ*ii* by adding *i*² computed as an unsigned 64-bit integer (wrapping)
  and then converted to a double.
- The slope is (Σ*i*·Σ*y* − *n*·Σ*iy*) / (Σ*i*·Σ*i* − *n*·Σ*ii*). On
  Linux aarch64 the Σ*i* products are rounded and the *n* products fused:
  fma(−Σ*iy*, *n*, Σ*i*·Σ*y*) / fma(−Σ*ii*, *n*, Σ*i*·Σ*i*). On macOS
  arm64 it is the reverse: *ma*(Σ*i*, Σ*y*, −(*n*·Σ*iy*)) /
  *ma*(Σ*i*, Σ*i*, −(*n*·Σ*ii*)). On x86_64 nothing is fused, so both
  forms give the same result.
- The intercept is *ma*(−slope, Σ*i*, Σ*y*) / *n*.
- The correlation is (Σ*iy* − Σ*i*·Σ*y*/*n*) /
  √((Σ*ii* − Σ*i*·Σ*i*/*n*)·(Σ*yy* − Σ*y*·Σ*y*/*n*)), every operation
  rounded separately on every platform.

The differences are in the last bits, which `PRINT` and `GPRINT` show only
for cancellation-sensitive data. Provenance: black-box compatibility test
(`vdef_differential.rs` `vdef_aggregates_match_pinned_rrdtool_graphv`,
whose series are chosen so that each summation order and rounding is
visible in the printed values, on the macOS arm64 and Linux oracles) and the
unit tests of `crates/rondi/src/vdef.rs`
(`negative_zero_sums_start_from_positive_zero`,
`least_squares_uses_series_indices_including_unknown_gaps`).

**GR-101** (bb; compat, platform-specific). VDEF `PERCENT` and
`PERCENTNAN` over data that holds unknown or infinite values. Let *p* be
the parameter and *v* the series values in time order.

- `PERCENTNAN` first drops the unknown values; `PERCENT` keeps them. If no
  value is left the result is unknown.
- The remaining *m* values are sorted by the comparison of rpn.md
  ("Ordering used by SORT, MEDIAN, PERCENT and PREDICTPERC"), and the result
  is the value at zero-based position round(*p*·(*m* − 1)/100), halves
  rounded away from zero. For finite values this is the ordinary ascending
  order: `9,3,7,1,8,2,6,4,5,10` gives 1, 3, 6, 8, 10 for *p* of 0, 25, 50,
  75, 100. Unknown values sort first, so for `PERCENT` the series
  `UNKN,1` gives unknown for *p* of 0 and 25 and 1 above, while
  `PERCENTNAN` gives 1 everywhere.
- With an infinity present that comparison is not a consistent order, so
  the sorted sequence depends on the platform's `qsort` and a different
  sort algorithm gives different results. Rondi therefore calls the
  platform's `qsort` with that comparison. Observed on macOS arm64 for
  two-value series (the result for *p* of 0 and 100 is the sorted pair): the
  pair comes out in the reverse of the input order whenever either value is
  infinite (`1,INF` gives `inf, 1`; `INF,1` gives `1, inf`; `NEGINF,1` gives
  `1, -inf`; `1,NEGINF` gives `-inf, 1`; `NEGINF,INF` gives `inf, -inf`;
  `INF,NEGINF` gives `-inf, inf`). Longer series are pinned by example:
  `NEGINF,1,2` is read in the order `1, 2, -inf`; `3,INF,2,1,4` gives 1, 2,
  inf, 3, 4 for *p* of 0, 25, 50, 75, 100; `INF,NEGINF,1,2,3` gives
  1, 2, 3, -inf, inf; and `UNKN,INF,NEGINF,2,1` gives unknown, 1, 2, -inf,
  inf under `PERCENT` and 1, 2, -inf, -inf, inf under `PERCENTNAN`.
- On glibc the classification of `-inf` differs (rpn.md), so the results
  with a negative infinity differ from the macOS ones (vector `vdef-22`).

Provenance: black-box compatibility test, probes of each series above with
`graphv` and `PRINT:<vname>:%.0lf` on the pinned macOS arm64 oracle (every
value printed above is its output); the unit tests of
`crates/rondi/src/vdef.rs` (`percentile_nan_and_finite_semantics_are_distinct`,
`percentile_comparator_follows_the_libc_isinf_sign`) and
`vdef_percentiles_with_infinities_match_pinned_rrdtool_qsort` in
`crates/rondi-cli/tests/vdef_differential.rs`. The Linux claim comes from the
existing `vdef-22` record, not from a new probe.

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
integer (leading white space, an optional sign, then digits and nothing
else; `60.5`, `1e2`, empty and garbage fail with
`ERROR: error parsing number <offset>`; a value beyond the 64-bit range
is clamped to it) or a VDEF name, whose value is used. A positional
`<a>=<b>` offset uses `<b>` (GR-95). Shifting a VDEF fails with
`ERROR: Cannot shift a VDEF: '<vname>' in line '<element>'`; a DEF or
CDEF as offset fails with
`ERROR: Offset cannot be a (C)DEF: '<name>' in line '<element>'` followed
by an empty line (`shift-045`, `-046`); no offset
field gives `ERROR: No shift given`; a bare `SHIFT` gives
`ERROR: No argument for definition of vdef/rrd in SHIFT`. Shifting a CDEF
is accepted (`shift-021`). The shifted series keeps the graph's row times:
the data moves by whole steps when the offset is a multiple of the step,
and the offset is truncated toward zero to whole steps otherwise (GR-49) (`SHIFT:x:30`
behaves like `SHIFT:x:0`, a VDEF offset of 11260 like 11220; `shift-001`
to `-034`, `data-059`). Offsets and repeated SHIFTs: GR-38.

**GR-38** (bb; compat). SHIFT offsets and their order.

- A positive offset moves the data later: the row stamped *t* shows the
  value the series had at *t* − offset; a negative offset moves it
  earlier (`shift-036`, `-039`).
- A VDEF offset uses the VDEF's value, never its time, also for `FIRST`
  and `LAST` (`shift-037`). The value converts to seconds as a C cast
  does on the target: on macOS and Linux aarch64 a VDEF whose value is
  unknown shifts by 0, while on Linux x86_64 it shifts by the most
  negative time, so every row is unknown (`shift-035`). The VDEF must be defined before the SHIFT (GR-41): a
  name defined later fails with `error parsing number <name>`.
- A SHIFT replaces the offset an earlier SHIFT gave the same series; it
  does not add to it. `SHIFT:a:3600` then `SHIFT:a:1800` shifts by 1800,
  then `SHIFT:a:-1800` by -1800, then `SHIFT:a:0` not at all
  (`shift-038` to `-040`).
- A CDEF or VDEF sees the offset in force where it stands in element
  order: a CDEF or VDEF before the SHIFT reads the unshifted series, one
  after it the shifted series, and after a second SHIFT the new offset
  (`shift-042`, `-043`, `-047`, `-048`). A shifted CDEF is shifted the
  same way (`shift-044`).
- Drawing elements, `XPORT` and the data formats use the last offset of
  the series, wherever the SHIFT stands: an `XPORT` or `LINE` before the
  SHIFT is shifted (`shift-041`, `-043`, `-049`).
- The deprecated `PRINT`/`GPRINT` with a CF read every row of the series
  (GR-49), so a SHIFT does not change their value (`shift-047`, `-048`).


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
CDEFs, with CF `AVERAGE`, `MIN`, `MAX`, `LAST` or one of GR-97 (case
sensitive). A DEF or CDEF with the VDEF form, or any other CF, fails with
`ERROR: bad CF for DEF/CDEF: <field>`; a VDEF with the CF form fails as an
unused argument (`print-013` to `-024`). Formats and suffixes:
format-strings.md. Each PRINT produces one output line (GR-82). Literal
`\g`, `\l` and `\n` in a PRINT format are printed as is (`print-028` to
`-030`). The deprecated form with `:strftime` is accepted; it has no time
of its own and formats the current wall-clock time, so `print-035` is
tagged `clock` and only required to finish. A format longer than 200
bytes is cut to 200 (format-strings.md FS-7).

**GR-97** (bb; compat). The CF form also accepts the archive functions of
Holt-Winters data: `HWPREDICT`, `MHWPREDICT`, `SEASONAL`, `DEVPREDICT`
and `DEVSEASONAL` read as `AVERAGE`, `FAILURES` as `MAX`. The CF form
skips infinite rows as well as unknown ones. Provenance: black-box
compatibility test (`graph_data_differential` print formatters,
`alias_modes` CDEF and VDEF scalars) and probes of each name.

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

**GR-39** (bb; compat). The value a `PRINT` or `GPRINT` shows. The VDEF
form shows the VDEF's value or, with `:strftime`, its time. The CF form
shows what the matching VDEF of the same series gives: `AVERAGE` as
`AVERAGE`, `MIN` as `MINIMUM`, `MAX` as `MAXIMUM`, `LAST` as `LAST`,
over every row the series holds (GR-49). Unknown rows are skipped, so
`LAST` is the last known value also when the window ends in unknown rows
(`print-042`, `-044`). `GPRINT` shows the same values as `PRINT`
(`print-043`). The time of a VDEF is the stamp of its row (the end of the
row's interval) for `LAST`, `MAXIMUM` and `MINIMUM`, and the start of
the row's interval for `FIRST`: one step before the stamp of the first
known row (`print-042`, `prep-095`, `vdef-005`). The numbers come from
format-strings.md.

## 9. Image size and layout

The numbers in this section are what `graphv` reports and what the size
line prints. They hold for the default fonts of the pinned builds on both
macOS and Linux; text widths come from the font (GR-60).

**GR-60** (bb; platform-specific). Text is measured with the default font,
which in both pinned builds is DejaVu Sans Mono at 100 dots per inch. At
*s* points a character advances floor(*s*·100/72·1233/2048 + 0.5) px and
a line of text is ceil(*p*·1901/2048) + ceil(*p*·483/2048) px high, with
*p* = *s*·100/72 (the face's advance, ascent and descent in its 2048-unit
em). At the default legend size of 8 that is 7 px per character and 14 px
per line; at 12 it is 10 and 20 (`layout-043`). Characters are counted, not
bytes: UTF-8 text such as `café` is four characters. Characters outside
the default face (CJK) are measured through font fallback and their
metrics depend on the host (`legend-030`). A family such as `Courier`
changes the metrics in a host-dependent way (`layout-049`). (Fitted to
`graphv` coordinates at legend sizes from 1 to 40 points in steps of
0.25; `fit-001` to `-020`.)

The font sizes default to 9 (title), 7 (axis), 8 (unit), 8 (legend) and
5.5 (watermark); `--font DEFAULT:<size>:` sets all five.

**GR-61** (bb; compat). With a south legend, for a graph of width W and
height H, and with *t*, *a*, *u*, *g* and *w* the title, axis, unit,
legend and watermark sizes (GR-60):

- `graph_left` = 15 + floor(2·*u*) with `--vertical-label` + L·*c*, where
  L is `--units-length` (default 6) and *c* the character advance at the
  axis size, so 51 by default. `--y-grid none` sets L to 0 unless
  `--units-length` is given explicitly (`layout-039`, `-041`). L is not
  checked; a negative L that makes `graph_left` negative prints the value
  as an unsigned 64-bit number (`L=-5` gives `18446744073709551601`,
  `layout-038`).
- `graph_top` is 15 without a title (GR-63).
- `graph_width` = W and `graph_height` = H.
- `image_width` = `graph_left` + W + *r* + *e* + 15, where *r* is 15 + L·*c*
  with `--right-axis` and 0 without, *e* is floor(2·*u*) with
  `--right-axis-label` and 15 without, and the last 15 is the vertical
  RRDtool tag, which `--disable-rrdtool-tag` removes. An empty vertical
  label, right-axis label or watermark takes no room.
- `image_height` = `graph_top` + H + *x* + the legend block + floor(2·*w*)
  with a watermark, where *x* = floor(2.5·*a*) for the x-axis labels (0
  with `--x-grid none`). The legend block is the advance of its lines
  (GR-66, not below 0) plus floor(0.6·floor(2·*g*)), so 9 at the default
  size even without legend lines (141 for the default graph);
  `--no-legend` makes it 7 (139), or 8 in `--full-size-mode`.
- The size line prints `<image_width>x<image_height>`.

(`layout-001` to `-069`, `fit-031` to `-036`, `opt-*` size lines.)

**GR-62** (doc, bb; compat). `--only-graph` makes the image exactly W×H
with `graph_left` and `graph_top` 0 and no legend keys; the title,
labels and legend are dropped (`layout-005` to `-009`, `-066`).
`--full-size-mode` makes W×H the image size and fits the graph area inside
it: `graph_width` = W − (`graph_left` + *r* + *e* + 15), `graph_height` =
H − (the space above and below as in GR-61, including the legend). When
the remaining area is smaller than 1, it is 1 (`layout-010` to `-013`,
`legend-090` to `-094`).

**GR-63** (bb; compat). A title moves the graph down to
floor(1.6·*t*·(n + 1)), where n is the number of title lines: 28 for one
line at the default size, 43 for two. `\n`, `<br>` and `<br/>` break the
line, but only a break that follows some text adds one, and at most three
lines count (`layout-015` to `-017`, `fit-021` to `-030`). An empty title
adds nothing. The title font size changes the title height (`layout-045`).

**GR-64** (doc, bb; compat). `--pango-markup` interprets markup in legend
text: an item is measured as its text with the tags removed and the
predefined and numeric character entities decoded; `graphv` still reports
the legend text with the markup (`legend-075` to `-079`). Markup that
changes the font size is not modelled. Without `--pango-markup`, markup is
literal text.

Tab characters (literal or `\t`) advance to the next tab stop after the
current position, measured from the start of the item's text (after the
two spaces of a colour box, GR-66). The stops are the multiples of
floor(T + 2·*g*) px, where T is `--tabwidth` (default 40); with
`--y-grid none` and no `--units-length` they are the multiples of T
itself. Either way a stop at or before floor(2·*g*) is skipped
(`comment-016` to `-018`, `legend-009`, `-010`, `fit-037` to `-048`).

**GR-65** (doc, bb; compat). Legend position and direction:

- `south` (default): the first legend line has its bottom at
  floor(`graph_top` + H + *x* + 2·*g*), the area spans x = floor(2·*g*) to
  `image_width` − floor(2·*g*), and lines are floor(1.8·*g*) px apart
  (14 at the default size).
- `north`: the first line has its bottom at floor(`graph_top` + 2·*g*),
  where `graph_top` is the title space of GR-63, and the graph moves down
  by the legend block of GR-61 (`layout-029`, `legend-060` to `-064`).
- `east`/`west`: the legend is a column beside the graph whose lines end
  only at markers (`legend-050` to `-059`). Its width is the largest fill
  of a line (GR-66); every line is aligned within that width. For `east`,
  `graph_left` grows by the tag's 15 and the legend starts floor(2·*g*)
  right of the graph's right edge (`graph_left` + W + *r* + *e*); the image
  ends floor(2·*g*) after the legend. For `west`, `graph_left` grows by the
  legend width plus 2·floor(2·*g*) and the legend starts at floor(2·*g*).
  `image_height` is `graph_top` + H + *x* (+ the watermark). With
  `--no-legend`, `east` still moves the graph right by 15 when the tag is
  shown (`fit-069`, `-070`).
- `bottomup` mirrors the vertical positions of the legend items within the
  lines they occupy, so the first line comes last and the space after a
  line (for `\s`, for example) stays between the same two lines.
  `bottomup2` mirrors only the items from the first to the last graph
  element (an item that is not a `COMMENT`) within the lines those items
  occupy; comments before and after them keep their places, even on a line
  they share with a mirrored item (`legend-065` to `-074`, `fit-071` to
  `-080`).

The vectors pin the exact coordinates of each combination.

**GR-66** (doc, bb; compat). Legend items and lines:

- A drawing element with a colour and a legend shows its legend as two
  spaces (the colour box) followed by the text; so do `HRULE`, `VRULE`,
  `TICK` and `AREA` legends. `GPRINT` and `COMMENT` show the text only.
  Elements without a visible colour or without legend text add no item.
  Legend text longer than 200 bytes is cut to 200 bytes (`comment-020`,
  `-021`).
- `coords[n]` is `x0,y0,x1,y1` of the item's text box; x1 − x0 is the text
  width (GR-60) and y1 − y0 the line height.
- An item ends a line when its text ends with a marker: `\l` (or its alias
  `\n`) left, `\r` right, `\c` centered, `\j` justified, `\s` and `\u`
  left. `\g` removes the gap that normally follows an item and the
  trailing spaces of its text. `\.` at the end is removed and has no other
  effect (`COMMENT:a\.` shows `a`). A backslash before any other last
  character fails with `ERROR: Unknown control code at the end of
  '<text>'`. Only the last two characters are examined, so `COMMENT:b\.\l`
  ends the line and shows `b\.` (`legend-011`).
- The gap is floor(2·*g*) px (16 at the default size). A line's *fill* is
  the sum of its item widths plus one gap for each item after the first
  that has no `\g`. When drawn, each item without `\g` is followed by a
  gap.
- A line without a terminating marker takes the alignment `TEXTALIGN`
  gives at the time the line ends (default justified). Left starts at the
  area's left edge; right ends the fill at the right edge; center centers
  the fill. Justified spreads (room − fill)/(n − 1) extra between the n
  items; a single item is left-aligned (`legend-002`, `-031` to `-044`).
- Wrapping, with *room* the width of the legend area: an item without a
  line-ending marker starts a new line when the fill with it would exceed
  the room; it then loses its `\g`. An item with
  a line-ending marker always joins the current line. An item without a
  line-ending marker that is wider than the room on its own ends its line
  at once (`legend-012`, `-013`, `-014`, `-087`, `fit-061` to `-063`).
  Items are not cut.
- Vertical advance after a line: a line with items advances
  floor(1.8·*g*); `\s` advances floor(1.8·*g*) − ceil(*g*) (6 at the
  default size). `\u` moves back up over the line with items that it ends
  or follows: by ceil(1.8·*g*) when that line is not the first legend line,
  otherwise (and after an empty line) by floor(1.8·*g*). A line with items
  that ends in `\u` first advances as usual, so the next line starts 0 or
  1 px higher (`legend-005`, `up-001` to `-028`). The legend's height is the
  sum of the advances and may be negative. A legend that starts with empty
  `\u` lines, or ends with several, is open (RD-024, `comment-009`). An
  empty line advances a whole line only when right-aligned or centered
  (`COMMENT:\r` between two lines). Any element after the last marker,
  even a `DEF`, `CDEF` or an element without a legend, opens one more
  line, which is empty unless items follow (`fit-049` to `-056`).
- A right-aligned line takes no room when the area ends at or before x 0,
  and a justified line of two or more items takes none when the spread is
  below −(room + gap); the items are still placed (`fit-064` to `-068`).
- Rules: an `HRULE` above a given `--upper-limit` or below a given
  `--lower-limit` shows no legend, whatever the final value range; a
  `VRULE` outside the time range shows none. `--force-rules-legend` shows
  both (`fit-057` to `-060`).

`fit-081` to `-160` are random combinations of these options and entries
from a fixed seed.

## 10. Value range

**GR-70** (doc, bb; compat). Default autoscaling. Let *min* and *max* be
the smallest and largest values of the drawing elements that take part in
scaling (not rules, not `skipscale`, not `TICK`; STACK sums count,
including a `TICK` below the stack), ignoring unknown and infinite values,
and let `-u`/`-l` extend them when given (without `--rigid` the limits
only widen the range). Then:

1. If no value is known, the range is 0..1.
2. If min = max, max is increased by 1 % of its magnitude; 0..0 (also of
   negative zeros) becomes 0..1 (`range-021`). If min > max (inverted
   limits), min becomes max·0.99 when min is positive and max·1.01
   otherwise.
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
whatever the data (`scale-169`, `-176`, `range-014` to `-016`). Without
`--rigid`, `-u`/`-l` outside the data range widen it and are then rounded
like data (`scale-008`, `-019`, `-096`). `--rigid --allow-shrink`
intersects the data range with the limits and rounds the result like data:
`-l 1 -u 2` over data -70..60 gives 0.8..2, `-l 0 -u 100` gives 0..60
(`scale-170`, `-171`). Only an infinite limit abandons the graph
(GR-21); a finite limit that overflows the range is drawn and reported
(`-u 1.7e308` gives `value_max = inf`, `range-026` to `-029`).

**GR-71** (doc, bb; compat). `--alt-autoscale` sets the range to the data
range widened by 10 % of its width at each end (after step 2 of GR-70;
an all-zero range becomes -0.1..1.1). `--alt-autoscale-min` widens only
the minimum and `--alt-autoscale-max` only the maximum; the other bound is
the data value, unrounded. This contradicts the manual, which says the
other bound is 0 (`scale-002` to `-004`, `-182` to `-184`).

`--alt-autoscale` (not `-min` or `-max`) widens a narrow range further.
Let *m* be the larger magnitude of min and max, and *k* the largest power
of the base not above *m* (as in GR-70). The floor is
2·10^(floor(log10(*m*/*k*)) − 2): 0.02 when *m*/*k* is 1 to 9.99, 0.2 for
10 to 99.9 and 2 for 100 to 999. When max − min is below the floor, each
bound moves by 0.55·(floor − (max − min)) instead of 10 % of the width.
Data 0.1..0.2 gives -0.945..1.245, data 1e-9·(1.33..19.33) gives
-0.11..0.11, and with `--base 1024` data 1000..1001 gives 989.55..1011.45
(`scale-024`, `-079`, `range-030` to `-057`).

**GR-72** (doc, bb; compat). `--logarithmic` requires positive values: a
`-l` of 0 or less fails with
`ERROR: for a logarithmic yaxis you must specify a lower-limit > 0`. Let
*lo* and *hi* be the smallest positive and the largest data value, or
5.1e-9 and 5.1 when no value is positive. The minimum is `-l` when it is
below *lo* and *lo*/2 otherwise; the maximum is `-u` when it is at least
*hi* and *hi*·2 otherwise. With `--rigid` the limits are used as given.
Data -70..60 whose smallest positive value is 3.33 gives 1.67..113.3; with
no positive value the range is `2.5500000000e-09`..`1.0200000000e+01`;
`-u 100` over the same data gives a maximum of 100 (`scale-006`, `-050`,
`-072`, `-177` to `-181`, `range-001` to `-013`).

**GR-73** (bb; compat). `--units-exponent`, `--alt-y-grid` and
`--alt-y-mrtg` do not change the range. `--y-grid <step>:<factor>` does:
instead of sensible values, the bounds move out to whole multiples of
step·factor. With data -70..60, `-y 2:1` and `-y 0.5:4` give a maximum of
58 and `-y 1:1 -u 3` gives 57 (`scale-189` to `-191`, `range-017` to
`-019`).

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

**GR-84** (doc, bb; compat). `--lazy` computes the prints and the image
size and prints the size and print lines as usual, but leaves an existing
output file in place when that file is fresh: the file keeps its old size
and content even when the new graph would differ (`out-026` to `-031`,
`lazy-001`, `-002`). The size line reports the new graph.

- *Fresh* means that the file's modification time is at most
  `(end - start) / width` seconds (the time one pixel of the new graph
  covers) before the current time. The RRD files' times and their last
  update times play no part. Black-box runs that set the file times
  settled this: with a 4000 s range over 400 pixels a file 10 s old was
  kept and one 11 s old replaced, with 4400 s, 11 s was kept and 12 s
  replaced, and files dated after the last update but more than one pixel
  ago were replaced. Whether the division rounds was not settled.
  `out-032` replaces the fixture's `notes.txt`, dated 1000000000.
- A fresh file must begin with a PNG image whose chunks up to the first
  `IDAT` chunk can be read. Otherwise the reference prints
  `libpng error: Not a PNG file` (8 or more bytes that are not the PNG
  signature) or `libpng error: Read Error` (a shorter or truncated file)
  on standard error and draws and writes the graph as usual, exit 0
  (`lazy-004`, `-005`). `/dev/null` is fresh on hosts that update its
  time, so it gives `Read Error` there (`opt-145`, `-146`).
- When the file is kept, nothing after the layout is computed: `graphv`
  prints the print keys and the keys up to `graph_end`, then
  `image_info` (`out-029`, `lazy-001`), and an infinite limit no longer
  abandons the graph (`lazy-002`).
- `--lazy` does not apply to the data formats, which always write
  (`lazy-003`).

**GR-85** (bb; compat). Nothing is written on any error found before
rendering (GR-5). A rendering error from the PNG backend (GR-91) prints
`OOPS: Cairo has issues it can't even die: <reason>` and
`ERROR: Could not save png to '<name>'` on standard error after the size
line, exit 1.

**GR-86** (doc, bb; compat). `--imgformat` XML, XMLENUM, JSON, JSONTIME,
CSV, TSV, SSV turn the graph into a data export of every drawing element
with data (`LINE`, `AREA`, `STACK`, `XPORT`; a `TICK` is never
exported), using the legend as column name (column order: GR-94):

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
  changes the step (`data-006`). The times are formatted in the local
  zone of `TZ` even with `--utc`; JSON quotes them; an empty LFM gives
  empty text; the text is not escaped. `--x-grid none` alone keeps the
  numbers, but it does not cancel the LFM of another `--x-grid`, before
  or after it; and once `none` was given the x grid stays hidden whatever
  follows (`xgridtime-001` to `-008`, `-010` to `-012`). `xport` has no
  `--x-grid` and prints numbers (`xgridtime-009`).

**GR-99** (bb; compat). The `<rules>` section of the XML and JSON
exports of GR-86 and GR-89 lists each `HRULE` and `VRULE` in element
order. An `HRULE` prints as `0.0000000000e+00` and a `VRULE` as `0`,
whatever position, VDEF or legend the rule has (`ruleexport-001` to
`-024`). The section is left out when there is no rule.

**GR-87** (doc, bb; compat). `--add-jsontime` makes `graph` and `graphv`
export the data a second time, in the JSONTIME format of GR-86, once the
graph or data export finished:

- The export goes to the output of the image. With `-`, `graphv`
  appends, after its keys and the image bytes, the lines
  `graph_start = <n>`, `graph_end = <n>`, `graph_step = <n>` (the aligned
  data window), `datapoints = BLOB_SIZE:<n>` and the export (`data-062`,
  `jsontime-005`); `graph -` prints only the image (`data-064`).
- With a file name the export replaces the file's content, so the image
  (or the data of `--imgformat`) is lost, and `graphv` prints
  `datapoints = BLOB_SIZE:0` (`data-063`, `jsontime-001` to `-004`,
  `-006`). When the file cannot be opened the image save fails as in
  GR-80 and the export goes to standard output (`jsontime-007`).
- The three keys follow `image_info` (`jsontime-003`) and are printed
  also when `--lazy` stopped the key list early (`jsontime-010`).
- With no exportable element the file is truncated to zero bytes,
  `graphv` prints `datapoints = BLOB_SIZE:0` without the three keys, and
  the run fails with `ERROR: no XPORT found, nothing to do`
  (`jsontime-009`, `lazy-005`). An abandoned graph exports nothing and
  creates no file (`jsontime-008`).

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

**GR-94** (bb; compat). Export columns. `xport` writes one column per
`XPORT` element in element order, whatever the order of the definitions;
a name may be exported more than once, each time with its own legend; a
legend entry is the XPORT legend or empty (`xport-082` to `-085`). A
drawing element in `xport` adds no column; its legend appears under
`gprints` (`xport-084`). The data formats of `graph` write one column per
`LINE`, `AREA`, `STACK` and `XPORT` element in element order, mixed
freely; a `TICK` adds none, also when other columns exist; an element
without legend gives a column with an empty name; a stacked element's
column holds its own values, not the stacked sum (`data-065`, `-066`).

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
the result). The rows counted are those `rrdtool fetch` returns for the
DEF's window: from 1000000000 to 1599999940 a 60 s archive gives exactly
10,000,000 rows, which is within the limit, so the reference result is
the expectation (`limit-026`, `-027`). `rrdtool fetch` streams and is not
limited.

The same limit and text apply to the series Rondi builds after the
fetch: a DEF's unknown values under `-Z`, a reduced DEF, a CDEF's common
window, and the grid that exported and drawn series share (GR-48). That
grid's step is the greatest common divisor of the series' steps, so
series of 1800 s and 1801 s share a 1 s grid, and a window of more than
10,000,000 s is refused. The reference draws such graphs.

## Rondi policies

| Rule | Reference | Rondi |
|---|---|---|
| GR-93 | writes SVG, PDF, EPS | refuses with `ERROR: RRDtool graph format <F> is unsupported` (RD-006) |
| GR-91 | renders up to 32767 px per side | refuses a graph side above 32767 or an image above 2^24 pixels (RD-016) |
| GR-92 | fetches any range | refuses more than 10 million rows per fetch, series or shared grid (RD-017) |

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
| `grammar-055`, `-056`, `-061`, `out-008`, `order-009`, `-010`, `scan-001`, `-002`, `-009`, `-010`, `-012` to `-014`, `-016`, `-029`, `-031`, `-035`, `-038`, `-039`, `-048`, `-049`, `-057` to `-060`, `-062`, `-072`, `-073`, `-080`, `-087`, `-091`, `repeat-015` | GR-3 | defect | prints `0x0` for a graph with no elements |
| `def-073` | GR-43 | defect | `-Z` with a missing DS gives `nan`; the reference prints another value (`-5.438596`) |
| `grid-dash-011`, `line-044`, `-045`, `hrule-027`, `-028`, `vrule-027`, `-028` | GR-24, GR-50, GR-54 | defect | no Cairo warning for zero or negative dashes |
| `font-003`, `layout-049` (macOS); `font-019` (Linux) | GR-60 | defect | one font's metrics for every family |
| `legend-030` | GR-60 | defect | CJK fallback metrics |
| `comment-009` | GR-66 | defect | a legend that starts with an empty `\u` line is 1 px off (RD-024) |
| `layout-026` to `-028` (Linux) | GR-81 | defect | legend `coords` under `--zoom` |

`def-073` is listed as a defect, but the reference value comes from a
data source the DEF did not name; whether that is a reference defect to
specify as undefined is open.

## Unsettled questions

These were not settled by documentation or the black-box runs made for
this specification. The vectors pin the observed results; an
implementation that matches the vectors without a general rule is
acceptable, and a rule found later should be added here with its
provenance.

1. **Justification with `\g`** is settled by GR-66: the fill counts the
   gap of each later item without `\g`, while drawing puts the gap after
   each item without `\g` (`legend-031` to `-044`).
2. **Autoscale of tiny ranges with `--alt-autoscale`** is settled by
   GR-71.
3. **East and west legend widths** are settled by GR-65 (the widest fill
   plus a margin at each side).
4. **Legend coordinates under `--zoom` on Linux** were reported in #93 to
   differ from macOS; the `layout` zoom cases record both platforms.
5. **Fonts other than the default.** `--font` with a family name and
   `RRD_DEFAULT_FONT` change text metrics in a host-dependent way; only the
   default monospace metrics of GR-60 are specified.
6. **`--tabwidth`** below 1 and **`--x-grid` with a zero step** depend on
   undefined reference behavior (Pango warnings with a process id; a
   crash). Rondi finishes without a signal; the outputs are not compared.
7. **`--rigid --allow-shrink`** and **`--y-grid`** are settled by GR-70
   and GR-73.
8. **`--lazy` freshness** is settled by GR-84: the output file is
   compared with the current time minus one pixel's duration. Whether
   that duration is rounded was not settled.
9. **`-Z` with a missing data source** (`def-073`) prints a value from
   another data source of the same file; it may be a reference defect.
10. **Message length**: option errors are cut to 47 bytes after `ERROR: `
    (GR-12); whether `invalid option` messages are cut the same way was not
    tested with a long enough name.
11. **Runs of empty `\u` lines.** GR-66 covers `\u` after other lines.
    A legend that starts with empty `\u` lines, or ends with several, is
    1 to 4 px off when 1.8 times the legend size is not a whole number:
    `COMMENT:\u` alone gives an image 13 px lower than without a legend
    line at the default size (18 at size 10, `comment-009`), and two
    leading `\u` lines move the second line after them 15 px down, not 14.
    The exact values are in RD-024 of docs/COMPATIBILITY_DEFECTS.md.
12. **Tab stops with a fractional `--tabwidth`** under `--y-grid none` do
    not fall on multiples of the value (`-T 16.5` gives stops at 16, 33,
    50 and 67); only whole tab widths are specified there (GR-64).
13. **One-pixel graph areas.** `--width` below 10 is refused, so a graph
    area 1 px wide comes from `--full-size-mode` with a small width. Under
    `--alt-autoscale`, `-w 10` and `-w 60` give a value range around the
    data (72.70 to 73.57 and 73.60 to 74.48), while `-w 40` gives -0.1 to
    1.1, the range of a graph without data (`prep-083` to `-086`). Why the
    image width changes the result when the graph area does not was not
    settled; the vectors pin the three cases.
14. **rrdcached details** not covered by GR-37: `RRDCACHED_ADDRESS`, DEFs
    with different daemon addresses, and the exact text of daemon-side
    fetch errors, which comes from the daemon.
15. **`debug=` beyond a DEF at level 1.** Other values on a DEF, and
    `debug=` on other elements, write more diagnostic text; Rondi writes
    only the GR-96 block for a DEF with a non-zero value and nothing for
    other elements.
16. **`XPORT` of a number in other spellings.** The reference fails with
    `Encountered unknown type variable '' in line '<element>'` for every
    number (`5`, `1e3`, `+5`, `inf`; for some the quoted text is not
    empty). Rondi gives that error for numbers written with digits, a `-`
    sign and a decimal point only, and exports nothing for the other
    spellings, which `graph_xport_parity.rs` pins. For `XPORT:1e3`,
    `XPORT:+5`, `XPORT:inf` and `XPORT:nan` the reference exits 1 and Rondi
    exits 0 with output. Which behavior the test should pin is open.
