# rrdcached protocol and commands

This specification describes the request protocol of `rrdcached` 1.11.0 as
a client observes it over a Unix or TCP socket: how request lines are framed
and split into fields, the reply format, every command with its arguments,
replies and error texts, `BATCH` mode, the `-P` permission filter, and how
file names are resolved and confined. It is written so that the daemon's
request handling can be implemented from it and from the vectors in
`crates/rondi-cli/tests/data/rrdcached/vectors.json` without reading
RRDtool's source.

Daemon startup, command-line options, listeners, signals, the pid file, the
journal and the write timers are in
[rrdcached-daemon.md](rrdcached-daemon.md).

Each rule carries a provenance tag:

- **documentation**: stated in `rrdcached(1)` (sections PROTOCOL, Valid
  Commands, OPTIONS, SECURITY CONSIDERATIONS) or in the daemon's `HELP`
  text.
- **black-box**: observed by running `/opt/homebrew/bin/rrdcached` 1.11.0
  (macOS arm64) and the pinned 1.11.0 build in the Linux CI images, driven
  over a Unix socket by `scripts/rrdcached-spec-vectors.py`, and pinned by
  the vectors named in the rule.
- **reference-source-inspection**: none. Every rule below was settled by
  documentation or black-box runs.

Each rule also has a class:

- **must**: a replacement reproduces it exactly, byte for byte for reply
  text.
- **should**: expected for compatibility, but it depends on the platform or
  on timing, or it is pinned only on some platforms.
- **defect-or-undefined**: the reference behaves inconsistently, contradicts
  its manual, hangs or depends on memory. The rule states what a
  replacement does instead (a clean reply or error, never a panic or hang).
- **rondi-policy**: Rondi intentionally differs from the reference for
  safety. The rule gives the upstream behavior and, separately, the Rondi
  policy that a reimplementation must keep. These rules are listed again
  under [Rondi divergences](#rondi-divergences).

Where black-box results contradict the manual, the black-box result is the
contract and the conflict is noted.

## Compatibility tests

- `crates/rondi-cli/tests/data/rrdcached/vectors.json` holds the frozen
  cases (counts in [Vector table](#vector-table)). A case is a list of
  steps on one fresh fixture tree: start a daemon, open one connection per
  `conn` step and send its bytes, signal the daemon, write a file, list
  files. Each step's expected result is recorded beside it.
- `crates/rondi-cli/tests/rrdcached_spec_vectors.rs` replays every case
  against the Rondi binary under the name `rrdcached`. With rrdcached 1.11.0
  on `PATH` it also replays each case against it and requires the frozen
  result; `RONDI_REQUIRE_ORACLE=1` makes a missing oracle a failure. Run as
  root, every daemon runs as `nobody` (uid 65534) on a tree owned by it.
- `scripts/rrdcached-spec-vectors.py` regenerates the table (default
  `--rrdcached /opt/homebrew/bin/rrdcached`), runs every case twice and
  tags a case `unstable` when the runs differ, `--check PATH` diffs another
  daemon against the table (`--rondi` expects the recorded Rondi result
  where a divergence is listed), `--probe` sends ad hoc requests,
  `--record-platform`/`--merge-platform` record Linux results (run inside
  the `rondi-restore-oracle` images, which hold the pinned 1.11.0 build), and
  `--record-rondi PATH` records Rondi's result wherever it differs from the
  reference, classified by the script's `DIVERGENCE_RULES` (with
  `--platform KEY`, per-platform variants of those results).
- `--make-fixtures` rebuilds the two fixture files with `rrdtool create`.

### Fixture tree

Every case starts from this tree in a fresh directory called the *root* and
written `{ROOT}`. The root is a directory of `r` characters inside a new
temporary directory, named so that the root's real path is exactly 80
bytes long: journal lines, and so the `JournalBytes` counter, contain
absolute paths, and their length must not depend on the host.

| Path | Content |
|---|---|
| `b/` | the base directory given to `-b` |
| `b/a.rrd`, `b/sub/d.rrd`, `b/sub/deep/f.rrd`, `b/s p.rrd`, `b/back\slash.rrd` | copies of fixture `a.rrd`: step 300, last update 1000000000, `DS:x:GAUGE:600:U:U`, `DS:y:COUNTER:600:0:U`, `RRA:AVERAGE:0.5:1:10`, `RRA:MAX:0.5:2:10` |
| `b/s7.rrd` | fixture `s7.rrd`: step 7, last update 1000000003, `DS:v:GAUGE:20:U:U`, `RRA:LAST:0.5:1:5` |
| `b/text.rrd` | the six bytes `hello\n` |
| `b/empty.rrd` | empty file |
| `b/notes.txt` | `x\n` |
| `b/dir.rrd/`, `b/empty.d/x/` | directories |
| `b/link.rrd` | symbolic link to `a.rrd` |
| `b/lnk` | symbolic link to `sub` |
| `b/escape.rrd` | symbolic link to `../outside.rrd` |
| `outside.rrd` | copy of `a.rrd` outside the base directory |
| `j/` | empty directory, mode 0700, for `-j` |

A protocol session starts the daemon as
`rrdcached -g -p {ROOT}/pid -b {ROOT}/b -w 3600 -f 7200 <options> -l
unix:{ROOT}/s.sock` with `TZ=UTC`, `LC_ALL=C` and umask 022, sends each
connection's bytes followed by `QUIT\n` (unless the case says otherwise),
reads until the daemon closes the connection, and finally sends SIGTERM,
recording only the exit status. A connection marked `linger` sends no
`QUIT` and stops reading after 0.4 s of silence; one marked `none` sends
its bytes as they are.

### Normalization

Reply bytes are stored one character per byte (U+0000 to U+00FF). Before
comparison the root path is replaced by `{ROOT}`, a TCP port by `{PORT}`
(after a colon), the daemon's pid (read from the pid file) by `{PID}` where
it is a whole word, and the time stamps in journal file names by `{T}`
(`rrd.journal.{T}`). A reply of a case tagged `unordered`, which holds a
`LIST` result, is compared with its lines sorted, because `LIST` prints
entries in directory read order, which differs between file systems. A
read that times out (10 s) records `<timeout>`. A connection reset ends the
reply like a normal close: a daemon that closes a connection with unread
input resets it on Linux and closes it normally on macOS.

### Vector table

961 cases: 855 protocol sessions and 106 daemon cases.

| Group | Cases | Covers | Rules |
|---|---|---|---|
| `frame` | 32 | framing, field splitting, escapes, limits | RC-1 to RC-18 |
| `help` | 28 | `HELP` for every command | RC-20 to RC-22 |
| `p-*` | 250 | every file command against up to 19 path forms | RC-30 |
| `pB-*` | 250 | the same with `-B` | RC-31 |
| `nobase` | 8 | no `-b` | RC-32 |
| `update`, `cache`, `info`, `fetch` | 27, 16, 6, 9 | `UPDATE`, cache commands, `INFO`/`LAST`/`FIRST`, `FETCH`/`FETCHBIN` | RC-40 to RC-74 |
| `create`, `tune`, `dump`, `list` | 57, 39, 8, 55 | per command | RC-80 to RC-100 |
| `suspend`, `batch`, `perm` | 14, 27, 20 | `SUSPEND`/`RESUME`, `BATCH`, `-P` | RC-110 to RC-130 |
| `journal-001` to `journal-003`, `tcp`, `multi` | 3, 4, 2 | `STATS` with `-j`, TCP, shared state | RC-2, RC-3, RC-55 |
| `opt`, `pid`, `signal`, `journal-004` on, `timer` | 52, 5, 19, 27, 3 | daemon behavior | rrdcached-daemon.md |

Case tags:

| Tag | Meaning | Replay rule |
|---|---|---|
| `unordered` | The case lists a directory. | Reply lines compared sorted. |
| `timing` | The case waits for a timer or a write. | Retried once. |
| `undefined` | The reference result hangs, crashes or depends on memory. | Not compared with the reference; Rondi must answer cleanly and match its `rondi.expect`. |
| `unstable` | Two recordings differed. | Not compared frozen. |

`scripts/rrdcached-spec-vectors.py --list` prints every case with its rules
and first request; the replay test requires at least 400 protocol sessions.

## 1. Transport and connections

**RC-1** (documentation, black-box; must). The protocol is line based. A
client sends a request line terminated by LF (0x0A); the daemon answers
each request before reading the next one on the same connection. Requests
from one connection are answered in order. (`frame-001`, `frame-011`.)

**RC-2** (documentation, black-box; must). Unix-socket and TCP listeners
carry the same protocol with the same replies. Paths in requests are
resolved by the daemon in the same way for both; the client-side path
rewriting that `rrdtool --daemon` applies for remote daemons is not part of
the daemon. (`tcp-*`.)

**RC-3** (black-box; must). Cache state is shared by all connections: an
update sent on one connection is visible to `PENDING`, `QUEUE` and `STATS`
on later connections. A connection that ends inside `BATCH` without the
terminating `.` loses nothing that was already executed. (`multi-*`.)

**RC-4** (documentation; must). Each reply begins with a status line
`<code> <message>`: one decimal integer, one space and a message. A
negative code is an error. Zero or a positive code is success, and a
positive code is the number of further lines that follow the status line.
Every line ends with LF. The status line of a normal error is
`-1 <message>`.

## 2. Line framing

**RC-5** (black-box; must). The command word is compared without regard to
ASCII case (`ping`, `PiNg`). Argument keywords that the rules below name in
upper case (`RECURSIVE`) are case sensitive.

**RC-6** (black-box; must). An empty line is a request with an empty
command word and is answered `-1 Unknown command: ` (with the trailing
space). It does not close the connection.

**RC-7** (black-box; must). Fields are separated by exactly one space
(0x20). A tab is an ordinary character. Two consecutive spaces produce an
empty field; a leading space gives an empty command word
(`-1 Unknown command: `). A single trailing space after the last field is
ignored for commands that read a fixed number of fields; where a command
reads every remaining field (`UPDATE` values, `FETCH` columns) a trailing
space produces a final empty field (RC-49, RC-72).

**RC-8** (black-box; must). One carriage return immediately before the LF
is removed, so `PING\r\n` is `PING`. Only one is removed: `PING\r\r\n`
leaves `PING\r` (unknown command). A CR elsewhere is ordinary.
(`frame-005`, `frame-031`.)

**RC-9** (black-box; must). Fields after those a command uses are ignored
(`PING extra`, `FLUSH a.rrd extra`, `LIST /sub extra`), except where a rule
says otherwise (`CREATE` RC-80, `TUNE` RC-90, `FETCH` columns RC-72).

**RC-10** (black-box; must). An unrecognized command word gives
`-1 Unknown command: <word>`, echoing the word as sent (any length, any
byte). `WROTE` and `.` are recognized words that are not valid on a client
connection: they give `-1 Can't use '<word>' here.` with the word as sent
(`wrote` stays lower case). (`frame-007`.)

**RC-11** (black-box; must). A NUL byte ends a field like a space: `PING\0junk`
is `PING`, `HELP\0PING` is `HELP PING`, `\0PING` has an empty command word,
and `LAST\0a.rrd` is `LAST a.rrd`. A NUL directly after a backslash ends the
field too (`PI\\\0NG` is the command `PI`). (`frame-008`.)

**RC-12** (black-box; must). A backslash escapes the next byte: the pair is
replaced by that byte, which is then never a separator. `PI\NG` is `PING`,
`s\ p.rrd` is the field `s p.rrd`, `back\\slash.rrd` is `back\slash.rrd`,
and `s\<TAB>p.rrd` keeps the tab. A backslash before an ordinary character
simply disappears (`back\slash.rrd` names `backslash.rrd`). Quotes have no
meaning: `"s p.rrd"` is the two fields `"s` and `p.rrd"`.
(`frame-009`, `frame-010`.)

**RC-12a** (black-box; defect-or-undefined). A backslash that is the last
byte of a line escapes the line's terminator, and the reference then reads
past the end of the line. Observed results vary between runs and
connections: `HELP PING\` gives the command overview, `PING x\` a plain
`PONG`, while `LAST a.rrd\` gave the `LAST` usage reply in one run and
closed the connection without a reply in another, and `PING\` closes the
connection without a reply. A replacement must answer deterministically and
must not wait for further input; Rondi's replies are pinned as a `policy`
divergence (`frame-030`, tagged `undefined`).

**RC-13** (black-box; must). A request is processed only once its LF has
arrived. Bytes after the last LF wait for more input; a connection that
closes with an incomplete line gets no reply for it. (`frame-012`,
`frame-013`, `frame-032`.)

**RC-14** (documentation, black-box; must). `QUIT` (any case, any further
fields) closes the connection without a reply; requests after it on the
same connection are not processed. `QUIT` inside `BATCH` also closes the
connection, without the batch summary (RC-121).

**RC-15** (black-box; must). A request line may hold up to 8191 bytes
before its LF. A line of 8192 or more bytes before the LF closes the
connection without a reply for it; lines answered before it on the same
connection keep their replies. There is no limit on the number of lines in
one write or on one connection (500 `PING` lines in one write are all
answered). (`frame-019` to `frame-023`, `frame-026` to `frame-028`.)

**RC-16** (black-box; must). Request bytes are not required to be UTF-8.
Bytes 0x80 to 0xFF are ordinary characters, echoed unchanged in error
replies (`-1 Unknown command: PING\xff`) and used unchanged in file names.
(`frame-024`.)
*Rondi policy (intentional difference)*: Rondi closes a connection whose
request line is not valid UTF-8, without a reply. This is listed in
`docs/COMPATIBILITY_DEFECTS.md` under RD-007 and pinned by `frame-024`.

**RC-17** (black-box; must). There is no per-connection request count limit
and replies are not batched or delayed: 100 requests in one write produce
100 replies in order. (`frame-025`.)

**RC-18** (black-box; defect-or-undefined). A reply line longer than 4095
bytes including its LF is sent with its full length, but only the first
4095 bytes are the intended text; the rest is uninitialized memory
(`-1 Unknown command: ` with a 4075-byte word is the first case that shows
it). A replacement sends the whole intended line. Rondi does, and its
replies are pinned as a `policy` divergence (`frame-029`, tagged
`undefined`).

## 3. HELP

**RC-20** (documentation, black-box; must). `HELP` without an argument, or
with an argument that names no command, prints the command overview: the
status line `24 Command overview` followed by these 24 lines:

```
UPDATE <filename> <values> [<values> ...]
TUNE <filename> [options]
DUMP <filename> [-h none|xsd|dtd]
FLUSH <filename>
FLUSHALL
PENDING <filename>
FORGET <filename>
QUEUE
STATS
HELP [<command>]
PING
BATCH
FETCH <file> <CF> [<start> [<end>] [<column>...]]
FETCHBIN <file> <CF> [<start> [<end>] [<column>...]]
INFO <filename>
FIRST <filename> <rra index>
LAST <filename>
CREATE <filename> [-b start] [-s step] [-O] <DS definitions> <RRA definitions>
LIST [RECURSIVE] /[<path>]
SUSPEND <filename>
RESUME <filename>
SUSPENDALL
RESUMEALL
QUIT
```

`WROTE` and `.` count as naming no command here, and so do an empty first
argument (`HELP  PING`) and an unreadable one (RC-12a).

**RC-21** (black-box; must). `HELP <command>` with a known command word (any
case) prints `<n> Help for <COMMAND>` followed by *n* lines: `Usage: ` and
the command's overview line, then the command's description, and the reply
ends with an empty line that is counted in *n*. The exact text for every
command is pinned by `help-002` to `help-025`; the replacement must
reproduce it byte for byte. Only the first argument is read
(`HELP update a b` is help for `UPDATE`; `HELP bogus PING` is the overview).
(`help-*`.)

**RC-22** (documentation, black-box; must). `HELP` and `QUIT` are accepted
on every listener regardless of `-P` (RC-130).

## 4. File names

**RC-30** (documentation, black-box; must). A file argument is resolved
against the base directory given with `-b`. A name starting with `/` is
taken as an absolute path; any other name is appended to the base directory
with one `/`. The result is not normalized: `sub/../a.rrd`, `./a.rrd` and
`a.rrd` are different names, and an empty name (two spaces) is the base
directory followed by `/`. Error replies that show a path show this joined
form (`{ROOT}/b/missing.rrd`). The man page states that `-b` defaults to
`/tmp`; without `-b` the daemon has no base directory (RC-32).

**RC-31** (documentation, black-box; must). With `-B`, a resolved name is
refused unless it starts with the base directory followed by `/`, and any
name containing `../` is refused. The refusal is `-1 <name>: Permission
denied`, where `<name>` is the joined path (`{ROOT}/b/../outside.rrd`) or,
for an absolute name, the name as sent (`/etc/hosts`). The check is on the
string only; symbolic links inside the base directory that point elsewhere
(`escape.rrd`, `lnk/`) are followed. `PENDING` and `DUMP` do not apply the
check in the reference (RC-95). `./a.rrd` passes; `sub/../a.rrd` is
refused although it stays inside. Without `-B` any path is used as given. The per-command
results for 20 path forms with and without `-B` are pinned by the `p-*` and
`pB-*` groups.

*Rondi policy (intentional difference)*: under `-B` Rondi walks the path
below the base directory without following symbolic links and refuses a
symlinked final component, so `link.rrd`, `lnk/d.rrd` and `escape.rrd` get
`-1 {ROOT}/b/<name>: Permission denied` for every command; `DUMP` is
confined like the other commands; `CREATE` with an empty name is refused
with `-1 {ROOT}/b/: Permission denied`; and `CREATE` refuses to replace a
directory, symbolic link or other special file with
`-1 RRD Error: refusing to replace a linked or special target`. Each case is
listed under [Rondi divergences](#rondi-divergences).

**RC-32** (black-box; must). Without `-b`, relative names resolve from the
daemon's working directory, nothing is confined, and `LIST` answers
`-1 No base directory defined`. (`nobase-*`.)

## 5. UPDATE

**RC-40** (documentation, black-box; must). `UPDATE` without a file name
gives `-1 Usage: UPDATE <filename> <values> [<values> ...]`. With a file
name and no values it gives `-1 No values updated.`.

**RC-41** (documentation, black-box; must). `UPDATE <file> <v1> <v2> ...`
appends the values to the file's cache entry and answers
`0 errors, enqueued <n> value(s).` where *n* is the number of values
accepted by this request. Nothing is written to the file yet. Each value is
stored exactly as sent (`+1000000300:1:2` stays so in `PENDING`). When the
name has no cache entry the file is checked first: a missing file gives
`-1 No such file: <path>`, a directory or other non-regular file
`-1 Not a regular file: <path>`, any other `stat` failure `-1 stat failed
with error <errno>.` (decimal errno, 20 for ENOTDIR), and a file that is
not a readable RRD the reader's error (`-1 RRD Error: reached EOF while
loading header rrd->stat_head`). (`p-update-*`.)

**RC-42** (documentation, black-box; must). A value must start with a
timestamp that is a decimal floating-point number followed by `:`. `N`,
`n`, the at-style `@` form, a missing `:`, hexadecimal and trailing text
before the `:` are refused with `-1 Cannot find timestamp in '<value>'!`.
The text after the first `:` is not checked when queueing
(`1000000300:` and `1000000300:x:y` are accepted).

**RC-43** (black-box; must). The timestamp may have a fraction
(`1000000300.5`, `1000000300.123456789`), an exponent (`1e9`), or a leading
`+`. Values larger than any RRD time are accepted when queueing
(`9999999999`, `99999999999999999999`).

**RC-44** (black-box; must). Each value's timestamp must be greater than the
previous accepted timestamp for that cache entry; the first value is
compared with the file's last update time, read when the entry is created.
A value that is not later is refused with `-1 illegal attempt to update
using time <t> when last update time is <last> (minimum one second step)`,
both times printed with six decimals (`1000000300.000000`). Fractions
count: `1000000300.25` then `1000000300.5` is accepted.

**RC-45** (black-box; must). The number of values after the timestamp is
not checked when queueing; a value with too few or too many fields, or with
text that is not a number, is accepted and fails later, when the cache
entry is written: the write stops at the first bad value, values before it
in the same write are applied, and the error is only logged. (`update-012`
to `update-014`.)

**RC-46** (black-box; must). After a flush, later updates are compared with
the timestamp of the last value written (`update-016`).

**RC-47** (black-box; must). The cache is keyed by the resolved name of RC-30
without normalization. `a.rrd` and `{ROOT}/b/a.rrd` share an entry;
`sub/../a.rrd`, `./a.rrd` and `link.rrd` each get their own entry, which
reads the file's last update time on creation. (`update-018`, `update-019`.)

*Rondi difference (known defect)*: Rondi keys entries by the canonical
path, so these spellings share one entry and the second update with the
same timestamp is refused. Listed under RD-007 and pinned as a `defect`
divergence (`update-018`, `update-019`, `cache-010`).

**RC-48** (documentation, black-box; must). `UPDATE` increments the STATS
counter `UpdatesReceived` once per request, whether or not any value is
accepted (RC-55).

**RC-49** (black-box; must). Values are processed left to right. At the
first refused value the request stops: values before it stay queued, the
reply is that value's error, and the values after it are dropped.
A trailing space produces an empty last value, refused with
`-1 Cannot find timestamp in ''!`. (`update-023` to `update-025`.)

*Rondi policy (intentional difference)*: Rondi refuses an `UPDATE` request
whose text after the command word is longer than 4095 bytes or contains a
NUL, with `-1 Request must be under 4096 bytes without NUL`, and queues
nothing from it, because the journal must record exactly what was
acknowledged. The reference accepts any line up to RC-15's limit
(`frame-022`).

## 6. FLUSH, FLUSHALL, PENDING, FORGET, QUEUE, STATS

**RC-50** (documentation, black-box; must). `FLUSH <file>` writes the
entry's pending values to the file and answers only after the write:
`0 Successfully flushed <path>.`. With no entry for the name it answers
`0 Nothing to flush: <path>.` if the file exists and `-1 No such file:
<path>.` if it does not. Without a name: `-1 Usage: FLUSH <filename>`.
`FLUSH` increments `FlushesReceived`. (should) When the entry is already
queued for a write thread, for example after `FLUSHALL`, the reference may
answer before that write has finished; the vectors do not depend on it.

**RC-51** (documentation, black-box; must). `FLUSHALL` answers
`0 Started flush.` at once and schedules every entry with pending values
for writing; it does not wait. Further fields are ignored. `FLUSHALL` does
not count in `FlushesReceived` (`update-022`).

*Rondi difference (known defect)*: Rondi counts `FLUSHALL` in
`FlushesReceived`.

**RC-52** (documentation, black-box; must). `PENDING <file>` answers
`<n> updates pending` followed by the *n* queued values in arrival order,
each exactly as sent. A name without an entry, or a missing file, gives
`0 updates pending`. Without a name: `-1 Usage: PENDING <filename>`.

**RC-53** (documentation, black-box; must). `FORGET <file>` removes the
entry and its pending values: `0 Gone!`. A name without an entry gives
`-1 No such file or directory`. Without a name: `-1 Usage: FORGET
<filename>`.

**RC-54** (documentation, black-box; must). `QUEUE` answers `<n> in queue.`
followed by one line `<count> <path>` for each entry that is waiting for a
write thread. Entries are queued when their oldest pending value is older
than `-w` or by `FLUSHALL`; with `-w 3600` a fresh update is not queued
(`0 in queue.`).

**RC-55** (documentation, black-box; must). `STATS` answers
`9 Statistics follow` and these nine lines in this order, each `<Name>:
<value>` with one space: `QueueLength`, `UpdatesReceived`, `FlushesReceived`,
`UpdatesWritten`, `DataSetsWritten`, `TreeNodesNumber`, `TreeDepth`,
`JournalBytes`, `JournalRotate`. `UpdatesWritten` counts file writes and
`DataSetsWritten` the values written. `TreeNodesNumber` is the number of
cache entries; `TreeDepth` is the height of a height-balanced (AVL) search
tree over the entries' names (1 entry: 1, 2: 2, 3: 2, 4: 3).
`JournalBytes` is the number of journal bytes written since startup, and
counts only once the journal buffer is written out (with `-j`, an update
of `a.rrd` adds 28 bytes; see rrdcached-daemon.md). (`cache-013` to
`cache-016`, `journal-001`, `journal-002`.)

## 7. INFO, LAST, FIRST

**RC-60** (documentation, black-box; must). `INFO <file>` answers
`<n> Info for <path> follows` and *n* lines `<key> <type> <value>`, where
type 0 is a floating-point value printed like `%.10e` or `NaN`, 1 an
integer and 2 a string. The keys are those of `rrdtool info`. The cache is
not flushed first, so pending values are not shown. Without a name:
`-1 Usage: INFO <filename>`; file errors come from the RRD reader
(`-1 RRD Error: opening '<path>': No such file or directory`).

**RC-61** (documentation, black-box; must). `LAST <file>` answers
`0 <time>` where *time* is the file's last update time rounded down to a
multiple of the file's step (fixture `a.rrd`, last update 1000000000,
gives `0 999999900`; after a written update at 1000000300 it gives
`0 1000000200`). This differs from `rrdtool last`. The cache is not
flushed first. Without a name: `-1 Usage: LAST <filename>`.

**RC-62** (documentation, black-box; must). `FIRST <file> <index>` answers
`0 <time>`, the time of the oldest row of archive *index*. A missing index
gives `-1 Usage: FIRST <filename> <rra index>`; an index that is not a
non-negative decimal integer, or does not fit, gives
`-1 Invalid index specified: <text>`; an index past the last archive gives
`-1 RRD Error: invalid rraindex number`. The manual's "default is RRA zero"
does not hold: the index is required (doc conflict).

## 8. FETCH and FETCHBIN

**RC-70** (documentation, black-box; must). `FETCH <file> <CF> [<start>
[<end>] [<column>...]]` flushes the file's pending values, then answers
`<n> Success` followed by `FlushVersion: 1`, `Start: <t>`, `End: <t>`,
`Step: <s>`, `DSCount: <k>`, `DSName: <names separated by spaces>` and one
line per row `<time>: <v> <v> ...`. Known values are printed like
`%0.17e`; unknown values as `nan` or `-nan` according to the sign bit of
the NaN (see the platform notes). *n* counts every line after the status line. The CF is
case sensitive; an unknown CF gives `-1 rrd_fetch_r failed: unknown
consolidation function '<cf>'`. Fewer than two fields gives
`-1 Usage: FETCH <file> <CF> [<start> [<end>] [<column>...]]`.

**RC-71** (black-box; must). *start* and *end* are integers read with an
optional base prefix (`0x` hexadecimal, leading `0` octal); trailing text
after the digits is ignored. A field that does not start with a number, or
whose value does not fit, gives `-1 Cannot parse start time `<text>':
Only simple integers are allowed.` (or `end time`). Values of zero or below
count back from the current time. Without times the range is the day before
now. A start after the end but within one step returns that one row
(`FETCH a.rrd AVERAGE 1000000100 1000000000`).

**RC-71a** (black-box; defect-or-undefined). A start one step or more after
the end asks the reader for a negative number of rows. On macOS and Linux
aarch64 the reference answers `-1 rrd_fetch_r failed: malloc fetch data
area`; under x86_64 emulation the daemon crashed. A replacement answers
with that error and keeps serving; Rondi's replies are pinned as a `policy`
divergence (`fetch-009`, tagged `undefined`).

**RC-72** (black-box; must). Column names after *end* select and order the
returned data sources. A repeated name gives `-1 field <name> already used`;
more names than data sources gives `-1 too many fields given -
duplicates!`; an empty field (two spaces) is skipped; an unknown name gives
`-1 field <name> not found in <path>` (only the prefix up to `in ` is
stable in the reference; see the defect note in
`crates/rondi-cli/tests/rrdcached_protocol_parity.rs`).

**RC-73** (black-box; must). `FETCH` flushes the entry first, so after it
`PENDING` shows nothing (`fetch-005`).

**RC-74** (black-box; must). `FETCHBIN` takes the same arguments and has the
same errors. Its reply is the status line `<n> Success`, the header lines up
to `DSCount`, then for each selected data source a line
`DSName-<name>: BinaryData <rows> 8 <ENDIAN>` (`LITTLE` on the tested
targets) followed by *rows* native 8-byte IEEE-754 doubles and an LF. *n*
counts the text lines only (header lines plus one per data source).

## 9. CREATE

**RC-80** (documentation, black-box; must). `CREATE <file> [options]
<DS...> <RRA...>` creates an RRD file and answers `0 RRD created OK`.
Options are read until the first word that does not start with `-`:
`-b <start>`, `-s <step>`, `-r <source>`, `-t <template>` take the next
field as their argument; `-O` refuses to overwrite. An option is
recognized by its first letter after the `-`, so `-bogus` is `-b`, `-step`
is `-s` and `-Ox` is `-O`. `-b` and `-s` read their argument with C `atol`
semantics: leading digits, the rest ignored, 0 for no digits. A start
before 315360000 (including 0 from a non-number) gives
`-1 The first entry must be after 1980.`; `-b -1` means now minus 10
seconds, as does a missing `-b` (pinned by
`rrdcached_protocol_parity.rs`, since it depends on the current time). A
step of 0, including a non-number, uses the default 300; a negative step is
accepted (`create-005`). Any other option letter, an option without its argument, an
empty field before the definitions, or a word after the definitions that
is neither `DS:` nor `RRA:` gives
`-1 Usage: CREATE <filename> [-b start] [-s step] [-O] <DS definitions> <RRA definitions>`.
Errors from the creation itself carry the prefix `RRD Error: `
(`-1 RRD Error: you must define at least one Round Robin Archive`). An
existing file is replaced unless `-O` is given or the daemon runs with
`-O`, which gives `-1 RRD Error: creating '<path>': File exists`. A new
file has mode 0644 (umask 022). Pending values of a replaced file are kept
(`create-047`). (`create-*`.)

*Rondi difference (known defect)*: Rondi refuses `-r` and `-t`
(`-1 unsupported CREATE option: -r`), COMPUTE data sources and Holt-Winters
archives (`create-043` to `create-046`).

*Rondi policy (intentional difference)*: Rondi limits the size of a file
created through `CREATE` to 64 MiB and refuses larger layouts before writing
anything.

**RC-81** (documentation, black-box; must). With the daemon option `-O`,
`CREATE` never replaces an existing file.

**RC-82** (documentation, black-box; must). A missing parent directory is
refused with a two-line error, the status line
`-1 No permission to recursively create: <directory>` followed by the line
`Did you pass -R to the daemon?`, where *directory* is the joined parent
path. With the daemon option `-R`, missing parent directories are created
(mode 0755 under umask 022). `-R` works with or without `-B`; the manual
says it requires `-B` (doc conflict), and the daemon prints a warning at
startup without `-B` (rrdcached-daemon.md).

## 10. TUNE

**RC-90** (black-box; must). `TUNE <file> <argc> <argv0> <args...>` runs the
`rrdtool tune` option parser on the given argument vector: *argc* is the
number of words that follow, starting with an arbitrary program name
(`tune`). It answers `0 Success`, or `-1 Got error <message>` with the
message of `rrdtool tune`; a non-numeric or zero *argc* gives
`-1 Invalid argument count specified: <argc>`; more words than *argc*
gives `-1 Too many arguments (expected <argc>)`. Settings before a failing
option are written. Pending values are not flushed. (`tune-*`.)

*Rondi difference (known defect)*: Rondi supports heartbeat, minimum,
maximum, data source type and rename options and refuses Holt-Winters
parameters, `--step` and the `DS:`/`RRA:` modification words (RD-007,
RD-019).

**RC-91** (black-box; defect-or-undefined). An *argc* larger than the
number of words sent makes the reference stop answering on that connection
and stop responding to SIGTERM on macOS, and crash with SIGSEGV on Linux
(`tune-036`). A replacement must answer
`-1 Invalid argument count specified: <argc>` and keep serving. This is
pinned as a `policy` divergence.

**RC-140** (black-box; should). In the foreground the reference prints a
summary of the data sources to its standard output after each successful
`TUNE` (`DS[x] typ: GAUGE\thbt: 900\tmin: nan\tmax: nan`) and logs some
request errors on standard error (`handle_request_update: Could not read RRD
file.`). This output is not part of the protocol; Rondi writes neither and
the vectors do not compare the daemon's output streams in protocol
sessions.

## 11. DUMP

**RC-95** (black-box; must). `DUMP <file>` sends the XML that `rrdtool dump`
writes for the file, with no status line, and then closes the connection.
Further fields (`-h dtd`, `-h none`, anything) are ignored and the header is
always the DTD form. Pending values are flushed first (`dump-007`). An
error keeps the connection open and answers `-1 <name as sent>: failed to
stat: <strerror>` or `-1 <name>: failed to relay dump: <message>`. A name
that is not a regular file (a directory, or the empty name) also gives
`failed to stat:`, followed by the text for errno 0 (`Undefined error: 0`
on macOS, `Success` on glibc); this is a reference defect that the vectors
pin per platform. Without a
name: `-1 Usage: DUMP <filename> [-h none|xsd|dtd]`. The reference does not
apply `-B` to `DUMP` (`pB-dump-007`); Rondi does (RC-31).

## 12. LIST

**RC-100** (documentation, black-box; must). `LIST [RECURSIVE] <path>`
requires *path* to start with `/` and names a path below the base
directory. It answers `<n> RRDs` followed by one name per line. For a
directory, the entries are its subdirectories and its regular files whose
names end in `.rrd` (whatever their content), not recursing; entries that
are symbolic links are not listed. With `RECURSIVE` (upper case only) the
reply lists the `.rrd` regular files of the whole subtree as paths relative
to *path*; directories are not listed themselves, symbolic links to files
are skipped and symbolic links to directories are descended into
(`lnk/d.rrd`). A *path* that is itself a symbolic link to a directory is
listed. For a path naming a regular file whose name ends in `.rrd` the
reply is `1 RRDs` and the file's name; any other file gives the `Not a
directory` error below. Errors: a missing or relative path, or lower-case
`recursive`, gives `-1 Usage: LIST [RECURSIVE] /[<path>]`; a missing entry
gives `-1 List <path>: No such file or directory`; a path through a file
gives `-1 List <path>: Not a directory`; a path escaping the base (`/../`)
gives `-1 Cannot read: <path>` with `-B` and `-1 List <path>: Permission
denied` without it. A directory whose name ends in `.rrd` gives
`-1 List <path>: ` and the `strerror` text of ENXIO (`Device not
configured` on macOS). *path* in these replies is the base directory joined
with the requested path. Order of entries is not specified (see
Normalization).

*Rondi difference (known defect)*: Rondi does not follow symbolic links
when listing (`list-003`, `list-030`) and gives `-1 Cannot read: <path>`
for `/../` without `-B` (`list-048`).

## 13. SUSPEND and RESUME

**RC-110** (documentation, black-box; must). `SUSPEND <file>` stops writes
of an entry: `0 <path> suspended`. `RESUME <file>`: `0 <path> resumed`.
Without a name both give their usage reply and then close the connection
(`-1 Usage: SUSPEND <filename>`). A name without a cache entry gives
`-1 <path> - No such file or directory` and also closes the connection.
Suspending an already suspended entry answers `0 <path> already
suspended`; resuming one that is not suspended answers `0 <path> not
suspended`. (`suspend-001` to `suspend-004`, `suspend-012`,
`suspend-013`.)

**RC-111** (documentation, black-box; must). `SUSPENDALL` answers
`0 <n> rrds suspend` and `RESUMEALL` `0 <n> rrds resumed`, where *n* counts
the entries whose state changed. Further fields are ignored.

**RC-112** (documentation, black-box; must). A suspended entry keeps its
values: `FLUSH` answers success without writing, `FLUSHALL` skips it, and
`PENDING` still lists the values. `RESUME` makes it writable again. A
failing `SUSPEND` inside `BATCH` closes the connection without the batch
summary (`batch-018`, `batch-025`, `suspend-014`).

## 14. BATCH

**RC-120** (documentation, black-box; must). `BATCH` (further fields
ignored) answers `0 Go ahead.  End with dot '.' on its own line.` (two
spaces after `ahead.`). Each following line is a command executed as usual
but without its success reply; error replies are collected. The line whose
command word is `.` (trailing fields ignored, so `. ` also ends the batch)
ends the batch, and the daemon answers `<n> errors` followed by one line
`<k> <message>` per error, where *k* is the line number within the batch
starting at 1 and *message* is the error text without its `-1 `.

**RC-121** (documentation, black-box; must). Inside `BATCH` only `UPDATE`,
`CREATE`, `FLUSH`, `FORGET`, `SUSPEND`, `RESUME`, `SUSPENDALL`,
`RESUMEALL` and `QUIT` are executed (`QUIT` closes the connection, RC-14).
Every other recognized command, including `BATCH`, `HELP`, `PING`, `STATS`,
`QUEUE`, `PENDING`, `FLUSHALL`, `FETCH`, `INFO`, `LAST`, `FIRST`, `LIST`,
`TUNE`, `DUMP` and `WROTE`, is an error `Can't use '<word>' here.` Empty
lines and unknown words are errors as on a normal connection. A `.`
outside `BATCH` is `-1 Can't use '.' here.`. A connection that ends inside
`BATCH` gets no summary.

**RC-122** (black-box; must). The count in `<n> errors` is the number of
lines that follow it. A multi-line error message such as `CREATE`'s
missing-directory reply contributes all its lines: the first is prefixed
with the batch line number and the following ones are sent as they are, and
each counts (`batch-024`: one failing `CREATE` and one unknown command give
`3 errors`). Successive batches on one connection are numbered
independently.

*Rondi policy (intentional difference)*: Rondi drops a `BATCH` whose
collected error text exceeds 16 MiB (RD-007).

## 15. Permissions (-P)

**RC-130** (documentation, black-box; must). `-P <list>` restricts the
listeners that follow it to the listed commands. The list is split on
commas and spaces; empty items are ignored; names are case insensitive. A
later `-P` replaces an earlier one. `HELP` and `QUIT` are always allowed;
allowing `BATCH` also allows `.`. A recognized command that is not allowed
gives `-1 Permission denied.` (with the period). An unknown command word is
still `-1 Unknown command: <word>`, and `WROTE` is still `Can't use 'WROTE'
here.` even when listed. Inside `BATCH`, each line is checked the same way
and a denied line is a batch error `Permission denied.`; when `BATCH`
itself is denied, the lines after it are processed as ordinary requests.
(`perm-*`.)

## Platform notes

- On Linux x86_64 an unknown value that the reference computes is the
  negative quiet NaN (`-nan` in `FETCH`, `00 00 00 00 00 00 f8 ff` in
  `FETCHBIN`); macOS and Linux aarch64 use the positive one (`nan`). A NaN
  read from the file keeps its stored sign: the fixtures were written on
  macOS, so on x86_64 a row stored in the file prints `nan` next to
  computed rows that print `-nan` (`fetch-002`). These are recorded per
  platform as `expect_platform`.
  *Rondi difference (known defect)*: on x86_64 Rondi prints and sends every
  unknown value with the negative sign, so the cases that read stored
  unknown rows carry a `defect` divergence for `linux-x86_64` only.
- The x86_64 results were recorded under emulation (the
  `rondi-restore-oracle:amd64` image on an arm64 host); CI replays them on
  native x86_64.
- `strerror` texts differ between macOS and glibc (`Device not configured`
  for ENXIO on macOS is `No such device or address` on glibc; errno 0 is
  `Undefined error: 0` on macOS and `Success` on glibc), and mapping a
  directory fails with EINVAL on macOS and ENODEV on Linux; vectors that
  show them are recorded per platform.

## Rondi divergences

The cases below carry a `rondi` entry in the vector table: the frozen
`expect` stays the reference result and `rondi.expect` is what Rondi must
produce. `policy` entries are intentional hardening differences that a
reimplementation keeps; `defect` entries are known gaps that a later change
may close (the test fails once Rondi matches, so the entry is then
removed).

`scripts/rrdcached-spec-vectors.py --record-rondi` assigns each case to the
first matching entry of `DIVERGENCE_RULES` and fails on a divergence no
entry explains; the note of each entry is stored with the case. At the time
of writing (129 cases):

| Class | Cases | Rule | Difference |
|---|---|---|---|
| policy | 33 `pB-*` cases naming `link.rrd`, `lnk/d.rrd`, `escape.rrd` | RC-31 | symbolic links below `-b` refused under `-B` |
| policy | `pB-dump-007`, `pB-dump-008`, `pB-dump-014` | RC-31, RC-95 | `-B` applies to `DUMP` |
| policy | `p-create-003`, `pB-create-003` | RC-31 | `CREATE` does not replace a directory, link or special file |
| policy | `p-create-019`, `pB-create-019` | RC-31 | `CREATE` with an empty name refused |
| policy | `frame-024` | RC-16 | a request line that is not UTF-8 closes the connection |
| policy | `frame-022` | RC-49 | `UPDATE` longer than 4095 bytes refused |
| policy | `frame-029` | RC-18 | replies longer than 4095 bytes sent whole (reference undefined) |
| policy | `frame-030` | RC-12a | trailing backslash answered deterministically (reference undefined) |
| policy | `tune-036` | RC-91 | `TUNE` with too large an argument count answered (reference hangs) |
| policy | `fetch-009` | RC-71a | `FETCH` with start a step after end answered with the reader error (reference undefined) |
| policy | `list-003` | RC-100 | no symbolic links followed by `LIST RECURSIVE` under `-B` |
| policy | 16 `opt-*` cases, `opt-037` | RCD-3, RCD-5 | Unix sockets 0600 without `-m`/`-s`; new `-b` directories 0750 |
| policy | `pid-005` | RCD-11 | a live socket is refused, not replaced |
| policy | 29 `signal-*` and `journal-*` cases | RCD-30 | journal files 0600 |
| defect | 18 cases naming `a.rrd/` | RC-30 | trailing slash after a file name ignored |
| defect | `update-018`, `update-019`, `cache-010` | RC-47 | cache keyed by canonical path |
| defect | `update-022` | RC-51 | `FLUSHALL` counted in `FlushesReceived` |
| defect | `create-043` to `create-046` | RC-80 | `CREATE -r`, `-t`, COMPUTE and Holt-Winters refused |
| defect | `list-030` | RC-100 | `LIST RECURSIVE` does not follow directory links |
| defect | `list-048` | RC-100 | wrong error text for `/../` without `-B` |
| defect | `opt-032` | RCD-1 | errno text for a `-j` path that is a file |
| defect | `opt-040` | RCD-1 | `-V` not applied |
| defect | `update-015`, `fetch-002`, `fetch-005`, `fetch-006`, `fetch-008` (x86_64 only) | RC-70 | stored NaN sign not kept |

Some entries also carry per-platform variants of Rondi's result
(`rondi.expect_platform`): on Linux the trailing-slash cases and the NaN
signs differ from macOS.

## Rules by provenance

- From reference-source inspection: none.
- From the manual, confirmed by black-box runs: the reply status line
  (RC-4), the command set and their purpose, `BATCH` framing, the `-P`
  always-allowed commands, `FLUSH` waiting for the write, `FLUSHALL`
  returning at once, `STATS` names, `-B` confinement and `-R`. Conflicts:
  `FIRST` requires the index (RC-62), `-R` works without `-B` (RC-82).
- Everything else is black-box: field splitting, escapes, CR and NUL
  handling, line limit, every error text, the cache key, `LAST` rounding,
  the timestamp grammar, `CREATE` option handling, `TUNE`'s argument count,
  `LIST` output, `SUSPEND` closing the connection, batch error numbering
  and the `-P` list syntax.
