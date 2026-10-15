# Command-line front end

This specification describes the observable behavior of the `rrdtool`
command-line front end of RRDtool 1.11.0: how the first argument selects a
command, the usage, help and version output, how command options are
parsed, the exit status and the split between standard output and
standard error, the alias executables `rrdupdate`, `rrdcreate` and
`rrdinfo`, the environment variables the front end reads, and the text
formats of `info`, `first`, `last`, `lastupdate`, `fetch` and `list`. It is
written for an implementer who has not seen RRDtool's source and says what
a user can observe, not how RRDtool computes it.

Pipe mode (`rrdtool -`) is in [pipe-mode.md](pipe-mode.md). `PRINT`,
`GPRINT` and the other format strings are in
[format-strings.md](format-strings.md). The grammar of at-style times given
to `--start`, `--end` and similar options is in
[time-spec.md](time-spec.md). Command semantics (what `create`, `tune`,
`restore` or `resize` do to a file) are in their own specifications; this
document only covers how their arguments reach them and how their results
and errors are reported.

Each rule carries a provenance tag:

- **[doc]** the manual pages `rrdtool(1)`, `rrdfetch(1)`, `rrdinfo(1)`,
  `rrdlist(1)`, `rrdupdate(1)`, `rrdfirst(1)`, `rrdlast(1)`,
  `rrdflushcached(1)` and `rrdgraph(1)`, or the help text RRDtool prints;
- **[bb]** black-box runs of RRDtool 1.11.0 recorded in the vector table;
- **[rsi]** reference-source inspection. No rule in this document needed
  it: every rule below was settled by documentation or black-box runs.

Each rule also has a class:

- **must**: a replacement reproduces it exactly;
- **should**: expected for compatibility, but it depends on the C library
  or the platform and is pinned per platform;
- **defect**: the reference contradicts its manual or reports in a way that
  looks unintended. The result is deterministic and the vectors pin it, so
  a replacement reproduces it for compatibility; it is marked so that a
  later decision to diverge is a deliberate one;
- **undefined**: the reference crashes or its result is meaningless. A
  replacement must give a clean error (or a clean result) and must never
  crash, hang or exhaust memory. Such vectors are not compared.

Where black-box results contradict the manual, the black-box result is the
contract and the conflict is noted.

## Compatibility tests

- `crates/rondi-cli/tests/data/cli_spec_vectors.json` holds the vectors
  for this document and for [pipe-mode.md](pipe-mode.md) and
  [format-strings.md](format-strings.md). Vector ids start with `cli-`,
  `pipe-` or `format-` and name a group, for example `cli-optparse-012`.
  Each vector has a list of steps (program name, arguments, optional
  standard input), an optional environment, tags, and the expected exit
  status, standard output and standard error of every step.
- Every vector runs in a fresh copy of a fixture directory. The fixture is
  built from the table's `dirs`, `files` (text files and RRD XML dumps
  captured from the reference) and `fixture` (`restore` commands run with
  the program under test), so both programs start from identical data.
- `crates/rondi-cli/tests/cli_spec_vectors.rs` replays every vector against
  Rondi under the names `rrdtool`, `rrdupdate`, `rrdcreate` and `rrdinfo`
  (symbolic links to the Rondi binary). With the pinned RRDtool 1.11.0 on
  `PATH` it also runs each vector through it and requires the same result;
  `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure.
- `scripts/cli-spec-vectors.py` regenerates the table (`--rrdtool PATH`),
  runs every case twice and tags a case `unstable` when the runs differ
  (none do), `--only GROUP` prints one group, `--check PATH` diffs another
  binary against the table, and `--record-platform KEY` /
  `--merge-platform KEY FILE` record and merge Linux results.

The default environment of a step is `TZ=UTC`, `LC_ALL=C`, with
`RRDCACHED_ADDRESS`, `RRD_LOCKING`, `LANG` and every other `LC_*` variable
removed; a vector's `env` is applied on top. Standard input is written as
Latin-1 bytes (each character of the stored string is one byte) and output
is read the same way, so non-UTF-8 bytes survive.

Before comparison both sides are normalized, line by line:

| Normalization | Reason |
|---|---|
| Text after `Compiled ` on a line is removed | the build date and time in the banner |
| `OK u:<a> s:<b> r:<c>` with two decimals each becomes `OK u:* s:* r:*` | pipe-mode CPU and run time (PM-14) |
| `rra[<n>].cur_row = <digits>` becomes `rra[<n>].cur_row = *` | `restore` and `create` pick a random current row |
| The program directory followed by `/` becomes `{BIN}/` | `rrdinfo` echoes its own path (CLI-62) |
| The work directory, given and canonical spellings, becomes `{CWD}` | `pwd` and `chdir` errors |
| Consecutive lines starting with `d ` or `- ` are sorted | pipe-mode `ls` order is directory order (PM-22) |

Vectors tagged `dir-order` run `list`, which prints entries in directory
order; their standard output is compared as a sorted set of lines
(CLI-75). Vectors tagged `locale` need the `de_DE.UTF-8` locale and are
skipped where it is missing. A vector whose `TZ` names a zone that
`/usr/share/zoneinfo` lacks is compared only live, not frozen, and so is a
vector tagged `host-tz` on Linux (FS-35). A vector that is neither frozen-
nor live-compared on a host is skipped there, and a known divergence that
was not compared is not reported as fixed. A vector tagged
`binary-stdout` stores only
the byte count of standard output. Vectors tagged `undefined` only require
Rondi to finish without a signal or a timeout.

The main `expect` was captured on macOS arm64 (`/opt/homebrew/bin/rrdtool`
1.11.0). Where a Linux build differs, the vector carries
`expect_platform.linux-x86_64` or `expect_platform.linux-aarch64`, recorded
with the 1.11.0 oracle built as in `docker/Dockerfile.test` (Debian, glibc),
and the replay test uses it on the matching target.

## 1. Command dispatch

Let *n* be the number of arguments after the program name.

**CLI-1** (doc, bb; must). With *n* = 0 the program prints the generic
usage text (CLI-8) on standard output and exits 0.

**CLI-2** (doc, bb; must). With *n* = 1 and the argument `-`, the program
enters pipe mode ([pipe-mode.md](pipe-mode.md)).

**CLI-3** (bb; must). With *n* = 1 and any other argument, the program
prints help and exits 0, whatever the argument is. If the argument is
exactly one of the command names `create`, `update`, `updatev`, `graph`,
`graphv`, `dump`, `restore`, `last`, `lastupdate`, `first`, `info`, `list`,
`fetch`, `tune`, `resize`, `xport`, `flushcached` or one of the pipe-mode
names `cd`, `mkdir`, `pwd`, `ls`, `quit`, it prints that command's help
(CLI-9); otherwise (`help`, `--help`, `-h`, `version`, `--version`, `-v`,
`foo`, `--`, `-x`, `--foo`, `#`, an empty string, `Create`, `cre`) it
prints the generic usage. So `rrdtool last` prints the help of `last` and
does not run it. Pinned by `cli-dispatch-*` and `cli-noargs-*`.

**CLI-4** (bb; must). With *n* ≥ 2 the first argument is examined:

1. `help`, `--help`, `-h` or `-?`: help is printed and the status is 0. If
   *n* = 2, the first argument is exactly `help` and the second is a
   command or pipe-mode name (CLI-3), that command's help is printed;
   every other case prints the generic usage (`help last x`,
   `--help fetch`, `-h info`, `help LAST`, `help version`).
2. `version`, `--version` or `-v`: the line
   `RRDtool 1.11.0  Copyright by Tobi Oetiker (1.011000)` (two spaces after
   the version) is printed on standard output and the status is 0. The
   remaining arguments are ignored (`version last a.rrd` does not run
   `last`).
3. `-`: with *n* = 2 the second argument is a work directory and pipe mode
   starts (PM-3); with *n* ≥ 3 it is an unknown function (item 5).
4. A command name (CLI-3, first list only): the command runs with the
   remaining arguments.
5. Anything else, including the pipe-mode names, `-V`, `HELP`, `Last`,
   `las`, `lastx`, `--`, `-x` and an empty string: the program writes
   `ERROR: unknown function '<arg>'` to standard error and exits 1.

**CLI-5** (bb; must). Command names are matched exactly: case matters and
prefixes are not accepted.

**CLI-6** (doc, bb; defect). `rrdtool(1)` documents `rrdtool help cd` for
the pipe-mode commands; it works as described. The pipe-mode commands
cannot be run from the command line (CLI-4 item 5).

**CLI-7** (bb; must). The program name in `argv[0]` does not change this
dispatch when it is `rrdtool`. The alias names of section 6 select a
different front end.

## 2. Usage, help and version text

**CLI-8** (doc, bb; must). The generic usage text is: the banner line
`RRDtool 1.11.0  Copyright by Tobias Oetiker <tobi@oetiker.ch>`, a line of
15 spaces followed by `Compiled ` and the build date and time, an empty
line, `Usage: rrdtool [options] command command_options`, the list of valid
commands over three lines, an empty line, the two-line GPL notice, an empty
line, `For more information read the RRD manpages`, and an empty line. The
exact bytes are required; they are pinned by `cli-dispatch-001` and the
other usage vectors. Pipe mode inserts one more paragraph (PM-9).

**CLI-9** (doc, bb; must). A command's help is the banner, the
`Compiled` line, the empty line and the `Usage:` line of CLI-8, then that
command's help paragraph, then the GPL notice and the closing lines of
CLI-8. The paragraphs are RRDtool's own text and must be reproduced byte
for byte, including tabs, the doubled space in `graph, graphv,  dump`, the
leading space of the `tune`, `resize`, `cd`, `mkdir`, `pwd`, `ls` and
`quit` paragraphs, the unbalanced bracket in `[--border width` in the
`graph` paragraph, and blank lines. The required text is pinned by
`cli-help-001` to `cli-help-022` (via `help <command>`) and
`cli-noargs-001` to `cli-noargs-017` (the command alone); Rondi keeps the
paragraphs under `crates/rondi-cli/src/help/` (and the remaining ones in
the front end), and these vectors are the acceptance test for them.

**CLI-10** (bb; must). Help and usage go to standard output, standard
error stays empty, and the exit status is 0.

**CLI-11** (bb; must). The `Compiled` stamp is the only build-dependent
text. A replacement may print any date and time after `Compiled `; the
vectors drop it.

## 3. Option parsing

All commands except `resize` and `tune` parse options with the same rules
(CLI-12 to CLI-30); `tune` and `dump` differ in how errors are reported
(CLI-31, CLI-32), and `resize` takes only positional arguments (CLI-33).

**CLI-12** (doc, bb; must). Each command has a fixed table of options.
An option has a long name and usually a one-letter short name, and either
takes an argument or is a flag. The tables are the ones in the command
help (CLI-9) and manual pages. For example `fetch` has `--resolution|-r`,
`--start|-s`, `--end|-e`, `--daemon|-d` with arguments and the flag
`--align-start|-a`; `first` has `--rraindex` (no short name) and
`--daemon|-d`; `info` has `--daemon|-d` and the flag `--noflush|-F`; `list`
has `--daemon|-d` and the flags `--noflush|-F` and `--recursive|-r`;
`update` has `--template|-t`, `--locking|-L`, `--daemon|-d` and the flag
`--skip-past-updates|-s`; `xport` has `--step` with no short name.
`first -r` is an invalid option (`first` has no `-r`).

**CLI-13** (bb; must). Long options are matched exactly. Abbreviations
are not accepted, even when unique (`--res`, `--a`, `--st`, `--sta`,
`--rrainde`, `--heartbea`), and case matters (`--START`, `--DAEMON`). An
unknown long option fails with `invalid option -- '<text>'`, where
`<text>` is everything after the leading `--`, including any `=value`
part: `--bogus` gives `'bogus'`, `---x` gives `'-x'`, `--=x` gives `'=x'`
and `--skip-past-update=1` gives `'skip-past-update=1'`.

**CLI-14** (bb; must). An unknown short option fails with
`invalid option -- '<c>'`.

**CLI-15** (doc, bb; must). A long option that takes an argument accepts
it as `--name value` (the next argument, whatever it looks like, including
one starting with `-`, an empty string, or `--`) or as `--name=value`
(everything after the first `=`, which may be empty). If the option is the
last argument and has no `=`, it fails with
`option requires argument -- '<name>'` (no "an").

**CLI-16** (bb; must). A long flag given as `--name=value` fails with
`option takes no arguments -- '<name>'`, also when the value is empty
(`--noflush=`).

**CLI-17** (bb; must). A short option that takes an argument uses the rest
of its argument word if that is not empty (`-s1000000000`, `-hnone`,
`-r60`; `-s=1000000000` gives the value `=1000000000`), otherwise the next
argument whatever it is (`-s -e` gives the value `-e`). If there is no next
argument it fails with `option requires an argument -- '<c>'`.

**CLI-18** (bb; must). Short flags can be grouped in one word: `-jg`
sets both. When a letter that takes an argument appears in a group, the
rest of the word is its argument (`-sa` gives `--start a`); if it is the
last letter, the next argument is used (`-as 1000000000`, `-tm 3`,
`-jgs 1000000000`).

**CLI-19** (bb; must). Options and positional arguments may be mixed in
any order; positional arguments keep their relative order
(`fetch -s T f.rrd -e T AVERAGE` is `fetch f.rrd AVERAGE -s T -e T`).

**CLI-20** (bb; must). The first argument that is exactly `--` ends
option processing; it is removed and every later argument is positional,
including a second `--` and words starting with `-`
(`first -- -- a.rrd` opens a file named `--`).

**CLI-21** (bb; must). A lone `-` is a positional argument.

**CLI-22** (bb; must). Repeating an option is allowed; for options with a
value the last one wins (`-s 1000000000 -s 999999000`).

**CLI-23** (bb; must). Parsing stops at the first error; the error text is
reported (CLI-36) and the command does nothing else.

**CLI-24** (bb; must). The option error texts of CLI-13 to CLI-17 are cut
to 47 bytes: `invalid option -- '` followed by a 28-character name ends
without the closing quote, and so does
`option takes no arguments -- 'skip-past-updates`. Pinned by the
`cli-truncate-*` vectors.

**CLI-25** (bb; must). Numbers in option values are parsed per option, not
by the parser. Examples pinned by `cli-commands-*`: `first --rraindex`
reads a leading decimal integer and ignores the rest (`1x`, ` 1`, `+1`,
`1.9`, `0x1` and `abc` are accepted, the last as 0), rejects negative and
out-of-range values with `invalid rraindex number`, and a value beyond 64
bits is out of range; `fetch --resolution` and `create --step` accept a
positive integer with an optional unit suffix (`s`, `m` minutes, `h`, `d`,
`w`, `M` months, `y`) and report `resolution: value must be positive`,
`... value must be (suffixed) positive number` or
`... value has trailing garbage` (the prefix is `step size:` for
`create`); `graph -w` and `-h` read a leading integer (`1e2` is 1,
`10.9` is 10, `abc` is 0) and reject results below 10 with
`width below 10 pixels`; `xport -m` rejects results below 10 with
`maxrows below 10 rows`.

**CLI-26** (bb; must). Positional argument counts are checked by each
command after parsing. Examples: `last`, `lastupdate` and `info` with more
than one file fail with their usage error
(`Usage: rrdtool last [--daemon|-d <addr>] <file>`,
`Usage: rrdtool lastupdate [--daemon|-d <addr>] <file>`,
`Usage: rrdtool info [--daemon |-d <addr> [--noflush|-F]] <file>`);
`first` ignores extra files; `fetch` with fewer than two positional
arguments fails with `Usage: rrdtool fetch <file> <CF> [options]` and
ignores extra ones; `list` with two directories fails with
`Usage: rrdtool list [--daemon <addr> [--noflush]] [--recursive] <directory>`.
These texts are part of the contract.

**CLI-27** (bb; must). A command given only options but no positional
argument behaves as for missing positional arguments (an error), not as
the help of CLI-3.

**CLI-28** (bb; must). `fetch` consolidation function names are
case-sensitive: `average` fails with
`unknown consolidation function 'average'`.

**CLI-29** (bb; must). Time options (`--start`, `--end`, `-s`, `-e`) pass
their value to the at-style parser; its errors carry the prefix
`start time: ` or `end time: ` ([time-spec.md](time-spec.md) TS-2).

**CLI-30** (bb; must). `--daemon` with an empty value (`--daemon ''`) means
no daemon and also overrides `RRDCACHED_ADDRESS` (CLI-46).

**CLI-31** (bb; must). `tune` reports a long option without its argument
as `missing file name` and accepts some short options without arguments
(`tune a.rrd -t` and `tune a.rrd -D` print the data source summary;
`tune a.rrd -h` fails with `invalid arguments for heartbeat`). Unknown
options still fail with `invalid option -- '<text>'`. The `tune` vectors
in `cli-commands-*` and `cli-truncate-*` pin these texts; the semantics of
the options are in [tune-modify.md](tune-modify.md).

**CLI-32** (bb; must). `dump` replaces every option parsing error, and a
wrong number of positional arguments, with one error whose text is its
usage over four lines:
`usage rrdtool dump [--header|-h {none,xsd,dtd}]`, `[--no-header|-n]`,
`[--daemon|-d address]`, `file.rrd [file.xml]`. `--header` accepts any
value; values other than `xsd` and `dtd` behave as `none`.

**CLI-33** (bb; must). `resize` takes exactly four positional arguments
and does no option parsing: `--` and `--bogus` count as arguments, and any
other count fails with `wrong number of parameters`.

## 4. Output streams and exit status

**CLI-34** (bb; must). A command that succeeds writes its result to
standard output and exits 0. Commands without a result (`update`,
`create`, `tune`, `restore`, `resize`, `dump` to a file) print nothing.

**CLI-35** (bb; must). A command that fails writes one line
`ERROR: <message>` to standard error and exits 1. The message is cut to
4095 bytes before `ERROR: ` and the newline are added, even in the middle
of a multibyte character (`cli-truncate-014` to `-017`, file names of 4070
to 5000 characters).

**CLI-36** (bb; must). `last` and `first` print `-1` and a newline on
standard output before the error line when they fail, including option
errors and usage errors; `lastupdate` and `info` print nothing.

**CLI-37** (bb; defect). Some errors are written without a trailing
newline: the `flushcached` error when no daemon is known
(`Daemon address "(null)" unknown. Please use the "--daemon" option to set
an address on the command line or set the "RRDCACHED_ADDRESS" environment
variable.`), and the `list` errors of CLI-74. The bytes are pinned.

**CLI-38** (bb; must). `update` with an invalid `--locking` value fails
with `unsupported locking mode '<value>'` followed by an empty line (two
newlines in total).

**CLI-39** (bb; must). `graph` with no output file name, `graph` with only
a file name, and the other commands with no arguments print help
(CLI-3); `graph /dev/null` with no graph elements succeeds with empty
output. A graph that has elements prints `<width>x<height>` first, then
one line per `PRINT` ([format-strings.md](format-strings.md));
`graph /dev/null` with `PRINT` only and no graph elements prints `0x0`.

**CLI-40** (bb; must). The exit status of a command is 0 or 1. Help, usage
and version output always exit 0. The only other statuses come from pipe
mode (PM-4).

## 5. Environment

**CLI-41** (doc, bb; must). `RRDCACHED_ADDRESS` names a caching daemon for
`last`, `first`, `lastupdate`, `info`, `fetch`, `update`, `dump`,
`flushcached`, `list`, `xport`, `graph` and `tune` when no `--daemon` is
given. An empty value means no daemon. `create`, `resize` and `restore`
ignore it.

**CLI-42** (bb; defect). When the daemon cannot be reached, the reading
commands `last`, `first`, `lastupdate`, `info`, `fetch`, `dump`, `list` and
`graph` still read the file directly and print the full result on standard
output, then write `ERROR: Unable to connect to rrdcached: <reason>` to
standard error and exit 1. `update`, `xport` and `flushcached` print only
the error. `tune` reports `Cannot connect to daemon`. Pinned by
`cli-env-001` to `-025`.

**CLI-43** (bb; should). The `<reason>` is the system error text for the
failed connection (`No such file or directory` for a missing socket path,
`Connection refused`), or for a name that does not resolve
`failed to resolve address '<host>' (port 42217): <resolver text> (<code>)`.
The resolver text and code come from the C library and differ between
platforms (macOS `nodename nor servname provided, or not known (8)`, glibc
`Name or service not known (-2)`), as does the reason for `unix:` with an
empty path (macOS `No such file or directory`, glibc
`Connection refused`). These are recorded per platform.

**CLI-44** (doc, bb; must). An address is a socket path, `unix:<path>`, or
`<host>[:<port>]`; the default port is 42217.

**CLI-45** (bb; must). `--daemon <address>` (or `-d`) overrides
`RRDCACHED_ADDRESS` for that command.

**CLI-46** (bb; must). `--daemon ''` disables the daemon even when
`RRDCACHED_ADDRESS` is set.

**CLI-47** (doc, bb; must). `RRD_LOCKING` selects the file lock mode:
`try` (default), `block` or `none`; empty means the default. Any other
value prints `unsupported locking mode '<value>' in $RRD_LOCKING; assuming
'try'` and a newline on standard error and the command continues normally
with exit status 0. The `update --locking` option overrides it.

**CLI-48** (bb; must). `TZ` affects only the formatting of local times
(`strftime` items of `PRINT`, graph labels, `dump` date comments). `info`,
`fetch`, `first`, `last` and `lastupdate` print epoch seconds and are not
affected (`cli-env-036` to `-049`). Names the C library does not know fall
back to UTC ([format-strings.md](format-strings.md) FS-40).

**CLI-49** (bb; must). The program takes its locale from the environment
(`LC_ALL`, then the category variable, then `LANG`). With a numeric locale
whose decimal separator is a comma, floating point values that `fetch`,
`info` and `PRINT` print use a comma (`1,3333333333e+00`,
`ds[x].value = 1,1000000000e+03`, `2,333333`). `xport`, `dump` and
`lastupdate` keep a point. Pinned by the `cli-locale-*` and `format-locale-*`
vectors (tag `locale`, which need `de_DE.UTF-8`).

**CLI-50** (bb; must). `LC_TIME` changes month and day names in `strftime`
output ([format-strings.md](format-strings.md) FS-41) and nothing else
the front end prints.

**CLI-51** (doc; should). `RRD_DEFAULT_FONT` and other graph rendering
variables are outside this document.

## 6. Alias executables

RRDtool installs three small programs, `rrdupdate`, `rrdcreate` and
`rrdinfo`, each running one command. Rondi provides them by being invoked
under those names; the vectors run the real executables of the oracle
installation for the reference side.

**CLI-52** (bb; must). `rrdupdate`, `rrdcreate` and `rrdinfo` take the
arguments of `rrdtool update`, `rrdtool create` and `rrdtool info`
without the command word and with the same option rules (section 3).
`--help` and `--version` are invalid options there.

**CLI-53** (bb; must). On success they behave like the `rrdtool` command
(no output for `rrdupdate` and `rrdcreate`; the `info` listing for
`rrdinfo`) and exit 0.

**CLI-54** (bb; must). On any error they write to **standard output**,
not standard error: a short banner `RRDtool 1.11.0  Copyright by Tobi
Oetiker`, an empty line, their own usage text, and then
`ERROR: <message>`. The exit status is 1 and standard error is empty.

**CLI-55** (bb; must). The usage texts are, for `rrdupdate`, the lines
`Usage: rrdupdate <filename>` and the tab-indented lists of `--locking`,
`--template`, `--skip-past-updates` and value forms followed by an empty
line; for `rrdcreate`, `Usage: rrdcreate <filename>` with its option list
and an empty line; for `rrdinfo`, the single line
`Usage: rrdinfo <filename>`. The exact bytes are pinned by
`cli-alias-001` to `-012`.

**CLI-56** (bb; must). With no arguments, `rrdupdate` reports
`Not enough arguments`, `rrdcreate` reports
`need name of an rrd file to create`, and `rrdinfo` reports the `info`
usage error of CLI-62.

**CLI-57** (bb; must). `rrdupdate` with a file and no values reports
`Not enough arguments`; `rrdupdate a.rrd x` reports the update error for
`x`.

**CLI-58** (bb; defect). `rrdinfo --daemon <unreachable>` prints the full
listing on standard output and exits 1 without the banner or an error
line (compare CLI-42).

**CLI-59** (bb; should). A single binary that provides these programs
selects the front end from the final path component of `argv[0]`; the
vectors invoke Rondi through symbolic links named `rrdupdate`,
`rrdcreate` and `rrdinfo`, and through `rrdtool` for everything else.

**CLI-60** (bb; must). The `rrdtool` command `update` is not affected by
these programs; `rrdtool update` errors keep the CLI-35 format.

**CLI-61** (bb; must). `rrdcreate` without `--start` uses the current time
like `rrdtool create`; the vectors avoid depending on it.

**CLI-62** (bb; defect). `rrdinfo` with no file or with two files reports
`Usage: rrdtool <argv0> [--daemon |-d <addr> [--noflush|-F]] <file>`,
inserting the program path exactly as it was invoked (the vectors replace
the directory with `{BIN}/`).

## 7. Output formats

**CLI-63** (doc, bb; must). `last` prints the last update time as decimal
epoch seconds and a newline.

**CLI-64** (doc, bb; must). `first` prints the time of the oldest row of
the selected archive (`--rraindex`, default 0) as decimal epoch seconds:
the last update time rounded down to the archive's step, minus
(rows − 1) × step. An index at or beyond the number of archives fails with
`invalid rraindex number`.

**CLI-65** (doc, bb; must). `lastupdate` prints a header line made of a
space and each data source name separated by single spaces, an empty
line, then `<time>: ` followed by the last value written for each data
source, separated by single spaces, exactly as it was given to `update`
(`5.5`, `2500`, or `U` when none). The last value is text, not
reformatted, so it does not follow the locale.

**CLI-66** (doc, bb; must). `fetch` prints a header line of 11 spaces
followed by each data source name right-aligned in a 20-character field
(so the first name ends in column 31), an empty line, then
one line per row: the row time right-aligned in 10 characters, `: `, and
each value separated by single spaces in the C `%.10e` form
(`1.3333333333e+00`), or `nan` for unknown values. The value form follows
the platform's printing of NaN (`-nan` on Linux x86_64) and the numeric
locale (CLI-49). A row time below 10^9 gets a leading space.

**CLI-67** (doc, bb; must). `fetch` reports a start after the end as
`start (<start>) should be less than end (<end>)`. Row selection and
alignment are command semantics outside this document; the vectors pin
the observed rows.

**CLI-68** (doc, bb; must). `info` prints one `key = value` line per item
in a fixed order: `filename`, `rrd_version`, `step`, `last_update`,
`header_size`, then per data source `ds[<name>].index`, `.type`,
`.minimal_heartbeat`, `.min`, `.max`, `.last_ds`, `.value`,
`.unknown_sec`, then per archive `rra[<i>].cf`, `.rows`, `.cur_row`,
`.pdp_per_row`, `.xff` and per data source
`rra[<i>].cdp_prep[<j>].value` and `.unknown_datapoints`, and a final
empty line. Strings are quoted (`"GAUGE"`), integers are plain, and
floating point values use `%.10e` (`5.0000000000e-01`) except that NaN is
`NaN` and infinities are `inf` / `-inf`. `filename` is the path as given.

**CLI-69** (bb; must). `updatev` prints `return_value = 0` and one
`[<time>]RRA[<cf>][<rows>]DS[<name>] = <value>` line per consolidated
value written, with `%.10e` values.

**CLI-70** (doc, bb; must). `list <dir>` prints, one per line and without
a path prefix, the name of every entry of `<dir>` that is a subdirectory
or a regular file whose name ends in `.rrd`. Other files are not listed.
Directories and files are not grouped: entries appear in directory order
(CLI-75).

**CLI-71** (doc, bb; must). `list -r <dir>` (`--recursive`) instead prints
every `.rrd` file below `<dir>` as a path relative to `<dir>`
(`sub/deep/c.rrd`). Directory names are not printed. A subdirectory's
files are printed, recursively, at the position where the subdirectory
appears in its parent's directory order.

**CLI-72** (bb; must). A trailing `/` or a leading `./` in the directory
does not change the output.

**CLI-73** (bb; must). `--noflush|-F` has no effect without a daemon.

**CLI-74** (bb; defect). `list` on a path that is missing, not a
directory, an empty string, or a directory with nothing to list writes only
the system error text, with no `ERROR: ` prefix and no newline, to
standard error and exits 0. For an empty directory the text is the
platform's message for error number 0 (macOS `Undefined error: 0`, glibc
`Success`).

**CLI-75** (bb; should). Entries appear in the order the operating
system enumerates the directory, which differs between file systems
(macOS APFS and the overlay file systems of the recording and CI
containers gave different orders for the
same fixture). The reference does not sort. Every `list` vector carries
the tag `dir-order` and is compared as a set of sorted lines, so the
vectors pin which entries are printed, not their order. A replacement
should keep the enumeration order rather than sort, so that it matches the
reference on the same file system.

**CLI-76** (bb; must). Opening a path that is not a valid RRD file fails
with the error of the file layer: `opening '<path>': <system error>` for a
missing file, `reached EOF while loading header rrd->stat_head` for a short
file, and `mmaping file '<path>': <system error>` for a directory, whose
system error text differs per platform (macOS `Invalid argument`, glibc
`No such device`).

## 8. Resource bounds and undefined reference behavior

**CLI-77** (bb; undefined). `graphv` with a width or height of
99999999999 crashes the reference (`cli-commands-170`, tag `undefined`).
A replacement must fail cleanly with exit status 1 and an `ERROR:` line
(Rondi reports that the image exceeds its pixel limit).

**CLI-78** (bb; must). Arguments of several kilobytes are accepted; error
messages are cut as in CLI-35. A replacement must not crash on arguments
of any length the operating system can pass.

## Known divergences

These vectors fail against Rondi at the time of writing and are listed in
`KNOWN_DIVERGENCES` in `crates/rondi-cli/tests/cli_spec_vectors.rs`; the
expectation stays RRDtool's.

| Vectors | Rule | Rondi today |
|---|---|---|
| `cli-env-001` to `-005`, `-007`, `-009`, `-017` to `-021`, `-023` to `-025`, `cli-alias-032`, `pipe-env-001`, `pipe-env-002`, `pipe-session-132` | CLI-42, CLI-58 | does not fall back to reading the file when the daemon is unreachable |
| `cli-env-012` | CLI-41 | `create` honours `RRDCACHED_ADDRESS` and fails |
| `cli-env-013` | CLI-42 | `tune` reports the connection error text instead of `Cannot connect to daemon` |
| `cli-optparse-204`, `-205` | (create semantics) | `create --source` requires the start time to match the source |
| `cli-commands-160` | CLI-39 | `graph /dev/null` with no elements prints `0x0` |
| `cli-locale-002`, `-006`, `-010` | CLI-49 | `info` ignores the numeric locale |
| `pipe-session-144` (Linux aarch64 only) | PM-10, PM-27 | keeps the trailing byte but passes it to the command as U+FFFD |

Pipe-mode and format divergences are listed in
[pipe-mode.md](pipe-mode.md) and [format-strings.md](format-strings.md).
Each entry in `KNOWN_DIVERGENCES` names the platforms it applies to; an
entry that starts matching fails the test so that it is removed.
