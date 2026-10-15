# rrdcached daemon behavior

This specification describes the process-level behavior of `rrdcached`
1.11.0: command-line options and their errors, base directory checks,
listeners and socket files, the pid file, signals, the journal and its
replay, the write timers and logging. The request protocol is in
[rrdcached.md](rrdcached.md), which also defines the provenance tags, rule
classes, the fixture tree, normalization and the vector table; the same
conventions apply here. Rule identifiers use the prefix `RCD-` so they do
not clash with the `RD-` entries of `docs/COMPATIBILITY_DEFECTS.md`.

The daemon cases are the `opt`, `pid`, `signal`, `journal-0nn` (daemon
kind) and `timer` groups of
`crates/rondi-cli/tests/data/rrdcached/vectors.json`. A daemon case starts
the program with an explicit argument list (`{ROOT}` expanded), in the
fixture root as working directory, with `TZ=UTC`, `LC_ALL=C`, umask 022 and
only `PATH` in the environment. A `start: exit` step waits up to 20 s for
the process to exit and records its status, standard output and standard
error; a `start: serve` step waits until `{ROOT}/s.sock` accepts a
connection. A `signal` step sends the signal and records the exit status
and both output streams. A `files` step records, for each listed path, its
type and permission bits (`socket 0755`, `file 0644`, `dir 0755`,
`missing`) and, for `:content`, the file's bytes.

Existing tests that cover parts of this area and are not replaced:
`rrdcached_startup.rs`, `rrdcached_listeners.rs`, `rrdcached_privileges.rs`
(root-only privilege drops) and `server_roundtrip.rs`.

## 1. Command line

**RCD-1** (documentation, black-box; must). Options are single letters as
listed by `rrdcached -h`: `-a`, `-B`, `-b`, `-F`, `-f`, `-G`, `-g`, `-j`,
`-L`, `-l`, `-m`, `-O`, `-o`, `-P`, `-p`, `-R`, `-s`, `-t`, `-U`, `-V`,
`-w`, `-z`, plus `-h` and `--help`. Flags may be clustered (`-gB`) and an
argument may be attached (`-w1800`). Words that are not options are
ignored (`extra-word`). Each case below is pinned by an `opt` vector; the
texts are on standard error followed by LF unless stated.

| Input | Exit | Output |
|---|---|---|
| `-h`, `--help` | 1 | usage text on standard output (pinned byte for byte by `opt-001`) |
| unknown option `-x`, `--bogus` | 0 | `invalid option -- 'x'` (`'bogus'` for a long one) on standard error, usage on standard output |
| option without its argument (`-w`) | 0 | `option requires an argument -- 'w'`, usage on standard output |
| `-w 0` | 2 | `Invalid write interval 0: value must be positive` |
| `-w x`, `-w -5` | 2 | `Invalid write interval <v>: value must be (suffixed) positive number` |
| `-f 0`, `-f x` | 3 | as `-w` with `flush interval` |
| `-z x`, `-z -1` | 2 | `Invalid write jitter <v>: value must be (suffixed) positive number` |
| `-t 0`, `-t x` | 1 | `Invalid thread count: -t <v>` |
| `-a 0`, `-a x` | 10 | `Invalid allocation size: <v>` |
| `-m 999`, `-m x` | 5 | `read_options: Invalid file mode "<v>".` |
| `-s`, `-G` unknown group | 5 | `read_options: couldn't map "<name>" to a group, Sorry` |
| `-U` unknown user | 5 | `read_options: couldn't map "<name>" to a user, Sorry` |
| `-P BOGUS` | 4 | `read_options: Adding permission "BOGUS" to socket failed. Most likely, this permission doesn't exist. Check your command line.` |
| `-V LOG_BOGUS` | serves | `Unrecognized log level 'LOG_BOGUS'; falling back to default LOG_ERR.` |

`-w` and `-f` accept a duration with a suffix (`5m`, `1h`). A valid `-P`
after a failing one clears the failure: `-P BOGUS -P PING` prints the
`-P` message and then serves with only `PING` allowed (`opt-042`). An
empty `-P ''` serves without restriction (`opt-047`).

With `-V LOG_INFO` or above the lifecycle messages (`starting up`,
`listening for connections`, `caught SIGTERM`, `starting shutdown`,
`clean shutdown; all RRDs flushed`, `goodbye`) appear on standard error in
the foreground; `caught <signal>` is at LOG_NOTICE (`opt-040`, RCD-50).

**RCD-1a** (black-box; must). The options are read in order and an error
does not always stop the reading:

- `-w`, `-f` and `-z` errors, and each unknown `-P` name, set the exit
  status and reading goes on; each known `-P` name sets it back to 0.
- `-h` sets the status to 1 and an unknown option or missing argument to 0;
  each of them prints the usage text once more, and the process then exits
  with the final status (`-w 0 -h` exits 1, `-h -w 0` exits 2, `-hx` prints
  the usage twice).
- `-a`, `-b`, `-G`, `-j`, `-m`, `-o`, `-s`, `-t` and `-U` errors stop the
  reading at once; the RCD-2 warnings are then not printed. An empty `-t`
  or `-a` prints `Missing argument for -t` (`-a`).
- After an unknown letter in a group the rest of the word is skipped and
  the next option word is read from that letter's position instead of its
  first letter: `-gx -abc` reads `-b c`, and `-gx -h` reports an empty name
  (`invalid option -- '` and a newline, the text ending at the NUL byte).
- `-U` and `-G` take a name or a number read like C `strtoul` over the whole
  value (white space and a sign allowed, an empty value is 0, wrapped to
  32 bits); `-s` takes a name or a number read like C `atoi`, where 0 is
  refused. The id must exist in the account database.
- A pid file that cannot be created is still opened: `-p ''` prints both
  `can't create pid file '' (No such file or directory)` and
  `can't open pid file '' (No such file or directory)`.

**RCD-2** (black-box; must). Warnings that do not stop startup, on
standard error: `WARNING: flush interval (-f) should be at least 2x write
interval (-w) !` when `-f` is less than twice `-w` (`opt-034`, `timer-003`);
`WARNING: write delay (-z) should NOT be larger than write interval (-w) !`
when `-z` exceeds `-w`; `WARNING: -B does not make sense without -b!` for
`-B` without `-b`, or else `WARNING: -R does not make sense without -B!`
for `-R` without `-B` (`opt-038`), each followed by
`  Consult the rrdcached documentation`. They print in that order after
the options are read, and the comparisons use the values as C `int`s, so
`-w 2147483648` trips the `-z` warning. `-w`, `-f` and `-z` take decimal
digits and an optional unit letter (`s`, `m`, `h`, `d`, `w`, `M` for 31
days, `y` for 366 days); text after the unit letter is ignored.
The defaults are `-w 300`, `-f 3600`, `-t 4`, `-a 1`.

## 2. Base directory

**RCD-3** (documentation, black-box; must). `-b <dir>` must be the
directory's own real path. A relative name, `.`, or a path through a
symbolic link exits 5 with three lines:

```
Base directory (-b) resolved via file system links!
Please consult rrdcached '-b' documentation!
Consider specifying the real directory (<realpath>)
```

`-b /` exits 4 with `Invalid base directory: /`. A trailing slash is
accepted (`opt-049`). A missing directory is created with its missing
parents (mode 0777 less the umask: 0755); a path through a regular file
exits 3 with `Failed to create base directory '<dir>': Not a directory`.
Without `-b` there is no base directory (rrdcached.md RC-32).

*Rondi policy (intentional difference)*: missing `-b` components are
created with mode 0750; an existing directory keeps its mode (`opt-037`).
Rondi also needs write access to `-b` for its ownership lock and stops with
a plain error when it is not writable.

## 3. Listeners and sockets

**RCD-4** (documentation, black-box; must). `-l <address>` opens a
listener: `unix:<path>` or a path starting with `/` is a Unix socket;
anything else is resolved with `getaddrinfo` as `host`, `host:port` or
`[v6]:port`, default port 42217; `-L` listens on the default port on all
interfaces. Without any `-l` the address is `unix:/tmp/rrdcached.sock`.
`-m`, `-s` and `-P` apply to the listeners given after them. A listener
that cannot be opened prints a line `rrdcached: <reason>` (for example
`rrdcached: Malformed address: [::1` or the `getaddrinfo` failure text,
which differs between platforms) and, when no listener opened,
`rrdcached: FATAL: cannot open any listen sockets` and
`rrdcached: daemonize failed, exiting.`, exit 1. A missing directory for a
Unix socket is created (mode 0755 under umask 022) (`opt-050`).

**RCD-5** (documentation, black-box; must). Without `-m` or `-s` a Unix
socket gets the mode the umask leaves (0755 under umask 022). `-m <octal>`
sets the mode; `-s <group>` sets the group and mode 0760 unless `-m`
overrides it. Socket files are removed at shutdown.

*Rondi policy (intentional difference)*: a Unix socket is created with mode
0600 unless `-m` or `-s` is given; it is bound inside a private directory
and renamed into place; a daemon running as root requires `-B` with `-b`;
any listener failure stops startup and removes the sockets already made;
a missing socket directory is created 0755. These are recorded in the
`opt-*` divergences and in `docs/COMPATIBILITY_DEFECTS.md` (RD-007).

## 4. Pid file

**RCD-10** (documentation, black-box; must). `-p <file>` writes the
daemon's pid and a newline (mode 0644 under umask 022) before serving and
removes the file at shutdown. A missing directory for it is created
(`opt-051`). If the file exists and names no live process, the daemon
prints `rrdcached: can't create pid file '<file>' (File exists)`,
`rrdcached: removed stale PID file (no rrdcached on pid <n>)` and
`rrdcached: starting normally.`, then serves. If the file holds something
other than a number it prints the first line, then `FATAL: PID file is
corrupted` and `rrdcached: daemonize failed, exiting.`, exit 1, leaving the
file. (`pid-001` to `pid-003`.)

**RCD-11** (black-box; must). A second daemon given the pid file of a
running one prints `rrdcached: can't create pid file '<file>' (File
exists)`, `FATAL: Another rrdcached daemon is running?? (pid <n>)` and
`rrdcached: daemonize failed, exiting.`, exit 1 (`pid-004`). A second
daemon with its own pid file and the same Unix socket path replaces the
first daemon's socket and serves.

*Rondi policy (intentional difference)*: a live socket at the path is
refused (`rrdcached: socket is already active: <path>`, then the FATAL
lines, exit 1) rather than unlinked (`pid-005`). A stale pid file is
replaced by rename; pid and `-o` files that are symbolic links or have
more than one link are refused. Rondi does not use the reference's default
pid path when `-p` is absent.

**RCD-12** (documentation, black-box; should). Without `-g` the reference
forks: the parent exits 0 once the child is set up, and later errors go to
syslog. Rondi never forks and stays in the foreground, so startup errors
always reach standard error with their exit status. The vectors always pass
`-g`.

**RCD-13** (black-box; rondi-policy). The reference creates the pid file,
the `-o` file and missing pid or socket directories with the mode the umask
leaves (0644 and 0755 under umask 022, 0666 and 0777 under umask 0).
*Rondi policy*: it removes group and other write permission from each of
them, so the umask can only tighten the result. The only difference from
the reference is under a umask looser than 022. New `-b` directories stay
0750 and Unix sockets 0600 without `-m`/`-s` whatever the umask (RCD-3,
RCD-5). Pinned by `rrdcached_startup.rs`
(`rrdcached_files_are_never_group_or_world_writable_under_umask_zero`).

## 5. Signals and shutdown

**RCD-20** (documentation, black-box; must). The effect of each signal on
pending values, after an `UPDATE` that is still pending (`signal-*`):

| Signal | No `-j` | `-j` | `-j -F` | `-F` |
|---|---|---|---|---|
| SIGTERM, SIGINT | written | kept in the journal, not written | written, journals removed | written |
| SIGUSR1 | written | written, journals removed | written, journals removed | written |
| SIGUSR2 | lost | kept in the journal, not written | kept in the journal, not written | lost |

The exit status is 0 in every case and nothing is printed at the default
log level.

**RCD-21** (black-box; must). SIGHUP, SIGPIPE and SIGALRM do not stop the
daemon; it keeps serving (`signal-017` to `signal-019`).

## 6. Journal

**RCD-30** (documentation, black-box; must). With `-j <dir>` the daemon
appends to a journal file named `rrd.journal.<seconds>.<microseconds>` in
*dir*. Each accepted `UPDATE` request adds one line `update <name as sent>
<values as sent separated by spaces>`; a completed write adds
`wrote <resolved path>`; `FORGET` adds `forget <resolved path>`
(`journal-004`, `journal-005`). Lines are buffered: the file may be empty
until the buffer is written, at the latest at shutdown. `JournalBytes` in
`STATS` counts the bytes once written out (rrdcached.md RC-55). A missing
`-j` directory exits 6 with `Unable to resolve journal path (<dir>,No such
file or directory)`; a path through a file exits 6 with `Failed to create
journal directory '<dir>': Not a directory`.

*Rondi policy (intentional difference)*: journal files are created with
mode 0600 (the reference uses 0644 under umask 022), opened without
following symbolic links, and must be single-link regular files owned by
the daemon user; a `-j` directory writable by group or others is neither
replayed nor written unless it is sticky and owned by the daemon user.
Every journal case records this as a `policy` divergence on the file mode.

**RCD-31** (documentation, black-box; must). At startup every file in the
journal directory whose name starts with `rrd.journal` is replayed in name
order before the daemon accepts connections; other files are ignored
(`journal-029`, `journal-030`). Replayed updates count in `UpdatesReceived`
but not in `JournalBytes`. The replayed files stay in place and a new
journal file is started (`journal-007`). A file named exactly
`rrd.journal` is renamed to `rrd.journal.0001` and replayed
(`journal-028`). On a flushing shutdown (RCD-20) journal files are removed.

**RCD-32** (black-box; must). Replay reads each LF-terminated line as a
request: `update` (any case) queues values as an `UPDATE` would, with a
relative name resolved against `-b`; `wrote` drops the entry's pending
values; `forget` removes the entry. Every `update` line counts in
`UpdatesReceived`, including one whose value is refused (bad or repeated
timestamp) or whose file does not exist; those queue nothing. Blank lines,
unknown words and other commands (`ping`, `PING`, `flush`) are skipped
and replay continues. A carriage return before the LF is kept as part of
the last value (`journal-026`; unlike a connection, RC-8). A last line
without LF is not replayed (`journal-016`). Names are used as recorded,
without `-B` checks. (`journal-009` to `journal-030`; each case's journal
text is in its `write` step.)

## 7. Timers

**RCD-40** (documentation, black-box; should). An entry is queued for
writing when a new value arrives and its oldest pending value is older than
`-w` seconds; the `-f` pass also queues entries whose oldest pending value
is older than `-w`, so idle entries are written within `-w + -f` seconds.
With `-w 1 -f 2` a single update is written within 3.5 s (`timer-001`,
`timer-002`).

**RCD-41** (documentation, black-box; should). The `-f` pass alone does
not write values younger than `-w` (`timer-003`). Timing cases carry the
tag `timing` and are retried once by the replay test.

## 8. Logging

**RCD-50** (documentation, black-box; should). In the foreground the
reference logs at `LOG_ERR` and above to standard error by default; `-V`
raises or lowers the level; `-o <file>` logs to that file instead of
syslog (created 0644). Without `-g` messages go to syslog after the fork.
Log text is not part of the contract.

*Rondi difference (known defect)*: `-o` writes Rondi's structured log
format, JSON lines whose message names the event (`rrdcached_started`,
`rrdcached_stopped`), not the reference's text (RD-007).

## Resource bounds

**RCD-60** (requirement; rondi-policy). A replacement keeps memory bounded:
pending values are capped (Rondi rejects updates past an estimated 64 MiB
before acknowledging them), a request line is at most 8191 bytes
(rrdcached.md RC-15), a `BATCH` error list at most 16 MiB, a created file at
most 64 MiB (RC-80), and no request may make the daemon panic, hang a
connection or stop answering signals (RC-91).

## Platform notes

- `getaddrinfo` and `strerror` texts differ between macOS and glibc; the
  vectors that contain them are recorded per platform.
- Run as root (as in CI), every daemon in the vectors runs as `nobody`, so
  root-only paths (privilege drop with `-U`/`-G`, the root `-B`
  requirement) are covered by `rrdcached_privileges.rs`, not here.

## Rules by provenance

- From reference-source inspection: none.
- From the manual, confirmed by black-box runs: the option list, `-b`
  symbolic-link warning, listener address forms and default port, `-m`,
  `-s` mode 0760, `-P` per listener, the pid file, signal effects, journal
  rotation with `-f`, fast shutdown with `-j` and `-F`.
- Everything else is black-box: exit statuses and texts, `-V` fallback,
  directory creation modes, stale and corrupted pid handling, SIGHUP being
  ignored, journal line format, replay order and skipping rules, and timer
  behavior.
