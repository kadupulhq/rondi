#!/usr/bin/env python3
"""Capture rrdcached protocol and daemon vectors from a reference rrdcached.

Usage:
  scripts/rrdcached-spec-vectors.py [--rrdcached PATH] [--out FILE]
  scripts/rrdcached-spec-vectors.py --make-fixtures [--rrdtool PATH]
  scripts/rrdcached-spec-vectors.py --check PATH [--only ID,...]
  scripts/rrdcached-spec-vectors.py --probe [--options=OPTS] 'REQUEST' ...
  scripts/rrdcached-spec-vectors.py --rrdcached PATH --record-platform KEY \
      --platform-out F
  scripts/rrdcached-spec-vectors.py --merge-platform KEY F
  scripts/rrdcached-spec-vectors.py --record-rondi PATH

Every case is a list of steps run against one fresh fixture directory (see
`layout`). The steps start a daemon, talk to it over one connection per
`conn` step, signal it, and list files. Text sent and received is stored as
JSON strings in which each character U+0000..U+00FF stands for one byte.

Normalization, applied to every recorded reply, stderr text and file
listing: the fixture root (the realpath of a fresh temporary directory) is
written `{ROOT}`, the TCP port of a `tcp` connection `{PORT}`, a process id
read from the pid file `{PID}`, and the time stamps in journal file names
`{T}`. Requests use the same placeholders and are expanded before sending.

Each case runs twice; a case whose two runs differ is tagged `unstable`.
The table is captured on macOS arm64 (/opt/homebrew/bin/rrdcached 1.11.0).
Linux results that differ are recorded as expect_platform.<KEY> (KEY
linux-x86_64 or linux-aarch64) with --record-platform inside the
rondi-restore-oracle image, then merged with --merge-platform.

--record-rondi PATH runs every case against Rondi and, where its result
differs from the reference (or the reference is undefined), stores it under
`rondi.expect` with the class and note of the first matching
DIVERGENCE_RULES entry, so a reimplementation keeps the same policy replies.
It fails on a divergence no rule explains. With --platform KEY it records
only per-platform variants of existing entries.

The output is crates/rondi-cli/tests/data/rrdcached/vectors.json, replayed
by crates/rondi-cli/tests/rrdcached_spec_vectors.rs and described by
docs/compatibility/rrdcached.md and docs/compatibility/rrdcached-daemon.md.
Regenerate only with rrdcached 1.11.0 and review the diff.
"""

from __future__ import annotations

import argparse
import contextlib
import glob
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DATA = REPO / 'crates/rondi-cli/tests/data/rrdcached'
TABLE = DATA / 'vectors.json'
NOBODY = 65534
CONNECT_TIMEOUT = 10.0
READ_TIMEOUT = 10.0
EXIT_TIMEOUT = 20.0
LINGER = 0.4
# Length of the fixture root path. Unix socket paths are limited to 104
# bytes on macOS, and the longest one used is {ROOT}/missing/s.sock.
ROOT_LENGTH = 80

FIXTURES = {
    'a.rrd': ['--start', '1000000000', '--step', '300',
              'DS:x:GAUGE:600:U:U', 'DS:y:COUNTER:600:0:U',
              'RRA:AVERAGE:0.5:1:10', 'RRA:MAX:0.5:2:10'],
    's7.rrd': ['--start', '1000000003', '--step', '7',
               'DS:v:GAUGE:20:U:U', 'RRA:LAST:0.5:1:5'],
}

# Standard daemon command line of a protocol session. OPTS is replaced by
# the session's own options.
STANDARD = ['-g', '-p', '{ROOT}/pid', '-b', '{ROOT}/b',
            '-w', '3600', '-f', '7200', 'OPTS', '-l', 'unix:{ROOT}/s.sock']


def layout(root: Path) -> None:
    """The fixture tree every case starts from."""
    base = root / 'b'
    for name in ('sub/deep', 'dir.rrd', 'empty.d/x'):
        (base / name).mkdir(parents=True)
    (root / 'j').mkdir(mode=0o700)
    os.chmod(root / 'j', 0o700)
    a = (DATA / 'fixtures/a.rrd').read_bytes()
    for name in ('a.rrd', 'sub/d.rrd', 'sub/deep/f.rrd', 's p.rrd',
                 'back\\slash.rrd'):
        (base / name).write_bytes(a)
    (base / 's7.rrd').write_bytes((DATA / 'fixtures/s7.rrd').read_bytes())
    (base / 'text.rrd').write_bytes(b'hello\n')
    (base / 'empty.rrd').write_bytes(b'')
    (base / 'notes.txt').write_bytes(b'x\n')
    (root / 'outside.rrd').write_bytes(a)
    (root / 'outside.txt').write_bytes(b'not an RRD file\n' * 200)
    os.symlink('a.rrd', base / 'link.rrd')
    os.symlink('sub', base / 'lnk')
    os.symlink('../outside.rrd', base / 'escape.rrd')


# ---------------------------------------------------------------------------
# Cases

CASES: list[dict] = []
_COUNTERS: dict[str, int] = {}


def case(group: str, rules: str, steps: list[dict], tags: tuple = ()) -> None:
    _COUNTERS[group] = _COUNTERS.get(group, 0) + 1
    # LIST prints entries in directory read order, which differs between
    # file systems, so replies holding a listing are compared line-sorted.
    if any(re.search(r'(?im)^(BATCH\n.*)?LIST\b', s.get('conn', ''))
           for s in steps):
        tags = tuple(tags) + ('unordered',)
    CASES.append({'id': f'{group}-{_COUNTERS[group]:03d}', 'group': group,
                  'rules': rules.split(), 'tags': sorted(set(tags)),
                  'steps': steps})


def daemon(opts: list[str] | None = None, *, argv: list[str] | None = None,
           start: str = 'serve') -> dict:
    if argv is None:
        argv = []
        for word in STANDARD:
            argv.extend((opts or []) if word == 'OPTS' else [word])
    return {'daemon': argv, 'start': start}


def conn(send: str, end: str = 'quit', via: str = 'unix') -> dict:
    return {'conn': send, 'end': end, 'via': via}


def stop(sig: str = 'TERM') -> dict:
    return {'signal': sig}


def quiet_stop() -> dict:
    """SIGTERM, recording only the exit status. In the foreground the
    reference also prints TUNE summaries and some request errors; that
    output is not part of the protocol (RC-140)."""
    return {'signal': 'TERM', 'quiet': True}


def session(group: str, rules: str, *sends: str | int,
            opts: list[str] | None = None, end: str = 'quit', tags: tuple = (),
            observe: list[str] | None = None) -> None:
    """One daemon, one connection per request text, then SIGTERM. An integer
    instead of a text waits that many milliseconds."""
    steps = [daemon(opts)]
    steps += [{'sleep': s} if isinstance(s, int) else conn(s, end) for s in sends]
    steps.append(quiet_stop())
    if observe:
        steps.append({'files': observe})
    case(group, rules, steps, tags)


D1 = 'DS:a:GAUGE:120:U:U RRA:AVERAGE:0.5:1:5'
COMMANDS = ['UPDATE', 'FLUSH', 'FLUSHALL', 'PENDING', 'FORGET', 'QUEUE',
            'STATS', 'HELP', 'PING', 'BATCH', 'FETCH', 'FETCHBIN', 'INFO',
            'FIRST', 'LAST', 'CREATE', 'LIST', 'TUNE', 'DUMP', 'SUSPEND',
            'RESUME', 'SUSPENDALL', 'RESUMEALL', 'QUIT', 'WROTE']


def build_cases() -> None:
    # ---- framing and parsing
    g = 'frame'
    session(g, 'RC-1 RC-4', 'PING\n')
    session(g, 'RC-5', 'ping\nPiNg\npInG\n')
    session(g, 'RC-6', '\n', 'PING\n\nPING\n')
    session(g, 'RC-7', ' PING\n', '  PING\n', '\tPING\n', 'PING \n',
            'PING\t\n', 'PING  \n')
    session(g, 'RC-8', 'PING\r\n', 'PING \r\n', 'LAST a.rrd\r\n')
    session(g, 'RC-9', 'PING extra\n', 'PING a b c d e f\n')
    session(g, 'RC-10', 'NOPE\n', 'nope x y\n', 'PINGX\n', 'PIN\n',
            'P\n', '.\n', 'WROTE a.rrd\n', 'wrote\n')
    session(g, 'RC-11', 'PING\0junk\n', 'PING\0\n', 'HELP\0PING\n',
            'HELP\0\0PING\n', '\0PING\n', 'PI\\\0NG\n', 'LAST\0a.rrd\n')
    session(g, 'RC-12', 'PI\\NG\n', 'P\\I\\N\\G\n', 'PING\\\n', '\\PING\n',
            'PI\\ NG\n')
    session(g, 'RC-12', 'LAST s\\ p.rrd\n', 'LAST s p.rrd\n',
            'LAST back\\\\slash.rrd\n', 'LAST back\\slash.rrd\n',
            'LAST "s p.rrd"\n', "LAST 's p.rrd'\n", 'LAST s\\\tp.rrd\n')
    session(g, 'RC-13', 'PING\nPING\nPING\n', 'PING\nNOPE\nPING\n')
    session(g, 'RC-13', 'PING', end='linger')
    session(g, 'RC-13', 'PING\nPING', end='linger')
    session(g, 'RC-14', 'QUIT\n', end='none')
    session(g, 'RC-14', 'QUIT\nPING\n', end='none')
    session(g, 'RC-14', 'quit\nPING\n', end='none')
    session(g, 'RC-14', 'QUIT extra\nPING\n', end='none')
    session(g, 'RC-14', 'PING\nQUIT\nPING\n', end='none')
    session(g, 'RC-15', 'P' * 1000 + '\n', 'PING ' + 'x' * 3000 + '\n')
    session(g, 'RC-15', 'PING ' + 'x' * 5000 + '\nPING\n')
    session(g, 'RC-15', 'PING ' + 'x' * 70000 + '\nPING\n')
    session(g, 'RC-15', 'UPDATE a.rrd ' + ' '.join(
        f'{1000000300 + 300 * i}:{i}:{i}' for i in range(300)) + '\nPENDING a.rrd\n')
    session(g, 'RC-15', 'HELP ' + 'y' * 4090 + '\n')
    session(g, 'RC-16', 'PING\xff\n', '\xc3\xa9\n', 'LAST \xe9.rrd\n',
            'NOPE\xff\xfe\n')
    session(g, 'RC-17', 'PING\nPING\n' * 50)
    session(g, 'RC-15', 'PING ' + 'x' * 8186 + '\nPING\n',
            'PING ' + 'x' * 8187 + '\nPING\n', 'PING\nPING ' + 'x' * 8187 + '\nPING\n')
    session(g, 'RC-15', 'PING\n' * 500)
    session(g, 'RC-15', 'HELP ' + 'x' * 8186 + '\n', 'NOPE' + 'x' * 4070 + '\n')
    session(g, 'RC-18', 'N' * 4075 + '\n', 'NOPE' + 'x' * 8187 + '\n',
            'UPDATE a.rrd ' + 'x' * 5000 + '\n', tags=('undefined',))
    session(g, 'RC-12a', 'PING\\\nPING\n', 'PING \\\nPING\n', 'HELP PING\\\n',
            'LAST a.rrd\\\n', 'PING x\\\n', tags=('undefined',))
    session(g, 'RC-8', 'PING\r\r\n', 'PI\rNG\n', 'PING\rx\n', 'HELP PING\r\n',
            '\r\n', 'LAST a.rrd \r\n')
    session(g, 'RC-13', 'PING\nPING\\', end='linger')

    # ---- HELP
    g = 'help'
    session(g, 'RC-20', 'HELP\n', 'help\n', 'HELP HELP\n', 'HELP help\n')
    for name in COMMANDS + ['.']:
        session(g, 'RC-21', f'HELP {name}\n', f'HELP {name.lower()}\n')
    session(g, 'RC-22', 'HELP NOPE\n', 'HELP PING x\n', 'HELP  PING\n',
            'HELP bogus PING\n', 'HELP update a b\n')

    # ---- path matrix: every file command against every target, with and
    # without -B.
    targets = ['a.rrd', 'missing.rrd', 'dir.rrd', 'text.rrd', 'empty.rrd',
               'notes.txt', '{ROOT}/b/a.rrd', '{ROOT}/outside.rrd',
               '../outside.rrd', 'sub/../a.rrd', './a.rrd', 'sub/d.rrd',
               '/sub/d.rrd', 'link.rrd', 'lnk/d.rrd', 'escape.rrd',
               'a.rrd/', 'missing/x.rrd', '{ROOT}/outside.txt', '']
    commands = [
        ('UPDATE', 'UPDATE {f} 1000000300:1:2'),
        ('FLUSH', 'FLUSH {f}'),
        ('PENDING', 'PENDING {f}'),
        ('FORGET', 'FORGET {f}'),
        ('FETCH', 'FETCH {f} AVERAGE 1000000000 1000000600'),
        ('FETCHBIN', 'FETCHBIN {f} AVERAGE 1000000000 1000000600'),
        ('INFO', 'INFO {f}'),
        ('LAST', 'LAST {f}'),
        ('FIRST', 'FIRST {f} 0'),
        ('TUNE', 'TUNE {f} 3 tune --heartbeat x:900'),
        ('DUMP', 'DUMP {f}'),
        ('SUSPEND', 'SUSPEND {f}\nPING'),
        ('RESUME', 'RESUME {f}\nPING'),
        ('CREATE', 'CREATE {f} -b 1000000000 ' + D1),
    ]
    for opts, tag in (([], 'p'), (['-B'], 'pB')):
        for name, template in commands:
            for target in targets:
                if name == 'CREATE' and target == '{ROOT}/outside.txt':
                    continue
                if name != 'CREATE' and target == 'missing/x.rrd':
                    continue
                if name in ('INFO', 'DUMP', 'FETCHBIN', 'TUNE') and target in (
                        'sub/../a.rrd', './a.rrd', '/sub/d.rrd', 'notes.txt'):
                    continue
                request = template.format(f=target) + '\n'
                session(f'{tag}-{name.lower()}', 'RC-30 RC-31', request,
                        opts=opts)

    # ---- UPDATE
    g = 'update'
    session(g, 'RC-40', 'UPDATE\n', 'UPDATE a.rrd\n', 'update a.rrd\n')
    session(g, 'RC-41', 'UPDATE a.rrd 1000000300:1:2\nPENDING a.rrd\n')
    session(g, 'RC-41', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
            'PENDING a.rrd\nQUEUE\n')
    session(g, 'RC-41', 'UPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd 1000000600:3:4\n'
            'PENDING a.rrd\n')
    session(g, 'RC-42', 'UPDATE a.rrd N:1:2\n', 'UPDATE a.rrd n:1:2\n',
            'UPDATE a.rrd 1000000300@1:2\n', 'UPDATE a.rrd x\n',
            'UPDATE a.rrd :1:2\n', 'UPDATE a.rrd 1000000300\n',
            'UPDATE a.rrd 1000000300:\n', 'UPDATE a.rrd -1:1:2\n',
            'UPDATE a.rrd 1000000300x:1:2\n')
    session(g, 'RC-43', 'UPDATE a.rrd 1000000300.5:1:2\nPENDING a.rrd\n',
            'UPDATE s7.rrd 1e9:1\nPENDING s7.rrd\n',
            'UPDATE sub/d.rrd +1000000300:1:2\nPENDING sub/d.rrd\n',
            'UPDATE sub/deep/f.rrd 0x3B9ACB2C:1:2\nPENDING sub/deep/f.rrd\n')
    session(g, 'RC-44', 'UPDATE a.rrd 1000000000:1:2\n',
            'UPDATE a.rrd 999999999:1:2\n', 'UPDATE a.rrd 1000000000.5:1:2\n',
            'UPDATE a.rrd 1000000001:1:2\nPENDING a.rrd\n')
    session(g, 'RC-44', 'UPDATE a.rrd 1000000300:1:2 1000000300:3:4\nPENDING a.rrd\n')
    session(g, 'RC-44', 'UPDATE a.rrd 1000000300:1:2 1000000200:3:4 1000000600:5:6\n'
            'PENDING a.rrd\n')
    session(g, 'RC-44', 'UPDATE a.rrd 1000000600:1:2\nUPDATE a.rrd 1000000300:3:4\n'
            'PENDING a.rrd\n')
    session(g, 'RC-44', 'UPDATE a.rrd 1000000300.25:1:2\n'
            'UPDATE a.rrd 1000000300.5:1:2\nUPDATE a.rrd 1000000300.5:1:2\n'
            'PENDING a.rrd\n')
    session(g, 'RC-45', 'UPDATE a.rrd 1000000300:1\nUPDATE a.rrd 1000000600:1:2:3\n'
            'UPDATE a.rrd 1000000900:x:y\nUPDATE a.rrd 1000001200:U:U\n'
            'PENDING a.rrd\n')
    session(g, 'RC-45', 'UPDATE a.rrd 1000000300:1\nFLUSH a.rrd\nPENDING a.rrd\n'
            'INFO a.rrd\n')
    session(g, 'RC-45', 'UPDATE a.rrd 1000000300:1:2 1000000600:x:2 1000000900:3:4\n'
            'FLUSH a.rrd\nINFO a.rrd\n')
    session(g, 'RC-46', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\nFLUSH a.rrd\n'
            'INFO a.rrd\nFETCH a.rrd AVERAGE 1000000000 1000000900\n')
    session(g, 'RC-46', 'UPDATE a.rrd 1000000300:1:2\nFLUSH a.rrd\n'
            'UPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd 1000000600:1:2\n')
    session(g, 'RC-47', 'UPDATE a.rrd 1000000300:1:2\n',
            'PENDING a.rrd\nUPDATE a.rrd 1000000600:3:4\n', 'PENDING a.rrd\n')
    session(g, 'RC-47', 'UPDATE a.rrd 1000000300:1:2\nUPDATE sub/d.rrd 1000000300:1:2\n'
            'UPDATE sub/../a.rrd 1000000600:1:2\nUPDATE ./a.rrd 1000000900:1:2\n'
            'UPDATE {ROOT}/b/a.rrd 1000001200:1:2\nPENDING a.rrd\nQUEUE\nSTATS\n')
    session(g, 'RC-47', 'UPDATE link.rrd 1000000300:1:2\nPENDING a.rrd\n'
            'PENDING link.rrd\nUPDATE a.rrd 1000000300:1:2\n')
    session(g, 'RC-48', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
            'UPDATE s7.rrd 1000000010:1\nSTATS\n')
    # FLUSHALL returns before the writes, so nothing after it in this
    # session may depend on whether they have happened.
    session(g, 'RC-51', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
            'UPDATE s7.rrd 1000000010:1\nFLUSHALL\nPING\n')
    session(g, 'RC-51 RC-55', 'FLUSHALL\nSTATS\n')
    session(g, 'RC-49', 'UPDATE a.rrd 1000000300:1:2 x 1000000600:3:4\nPENDING a.rrd\n')
    session(g, 'RC-49', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4 1000000600:5:6 '
            '1000000900:7:8\nPENDING a.rrd\n')
    session(g, 'RC-49', 'UPDATE a.rrd  1000000300:1:2\nUPDATE a.rrd 1000000300:1:2 \n'
            'PENDING a.rrd\n')
    session(g, 'RC-43', 'UPDATE a.rrd 1000000300.123456789:1:2\nPENDING a.rrd\n'
            'UPDATE a.rrd 1000000300.1234567891:1:2\nPENDING a.rrd\n')
    session(g, 'RC-43', 'UPDATE a.rrd 9999999999:1:2\nPENDING a.rrd\n'
            'UPDATE a.rrd 99999999999999999999:1:2\n')

    # ---- FLUSH, FLUSHALL, PENDING, FORGET, QUEUE, STATS
    g = 'cache'
    session(g, 'RC-50', 'FLUSH\n', 'FLUSH a.rrd extra\n', 'flush a.rrd\n')
    session(g, 'RC-50', 'FLUSH a.rrd\nSTATS\n')
    # FLUSH may answer before the write counters change, so the STATS after
    # it waits a little.
    session(g, 'RC-50', 'UPDATE a.rrd 1000000300:1:2\nFLUSH a.rrd\n', 300,
            'STATS\nLAST a.rrd\n', tags=('timing',))
    session(g, 'RC-50', 'UPDATE a.rrd 1000000300:1:2\nFLUSH {ROOT}/b/a.rrd\n'
            'PENDING a.rrd\n')
    session(g, 'RC-51', 'FLUSHALL\n', 'FLUSHALL x\n', 'flushall\n')
    session(g, 'RC-51', 'UPDATE a.rrd 1000000300:1:2\nUPDATE sub/d.rrd 1000000300:1:2\n'
            'FLUSHALL\n')
    session(g, 'RC-52', 'PENDING\n', 'PENDING a.rrd x\n', 'PENDING missing.rrd\n',
            'PENDING a.rrd\n')
    session(g, 'RC-53', 'FORGET\n', 'FORGET a.rrd\n', 'FORGET a.rrd x\n')
    session(g, 'RC-53', 'UPDATE a.rrd 1000000300:1:2\nFORGET a.rrd\nPENDING a.rrd\n'
            'FORGET a.rrd\nUPDATE a.rrd 1000000300:1:2\nSTATS\n')
    session(g, 'RC-53', 'UPDATE a.rrd 1000000300:1:2\nFORGET sub/../a.rrd\n'
            'PENDING a.rrd\n')
    session(g, 'RC-54', 'QUEUE\n', 'QUEUE x\n', 'queue\n')
    session(g, 'RC-54', 'UPDATE a.rrd 1000000300:1:2\nQUEUE\n')
    session(g, 'RC-55', 'STATS\n', 'STATS x\n', 'stats\n')
    session(g, 'RC-55', 'UPDATE a.rrd 1000000300:1:2\nUPDATE sub/d.rrd 1000000300:1:2\n'
            'UPDATE sub/deep/f.rrd 1000000300:1:2\nUPDATE s7.rrd 1000000010:1\n'
            'UPDATE s p.rrd 1000000300:1:2\nSTATS\nFORGET sub/d.rrd\nSTATS\n')
    session(g, 'RC-55', 'UPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd x\n'
            'UPDATE missing.rrd 1:1\nFLUSH a.rrd\nFLUSH missing.rrd\n', 300,
            'STATS\n', tags=('timing',))
    session(g, 'RC-55', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4 1000000900:5:6\n'
            'FLUSH a.rrd\n', 300, 'STATS\n', tags=('timing',))

    # ---- INFO, LAST, FIRST
    g = 'info'
    session(g, 'RC-60', 'INFO\n', 'INFO a.rrd x\n', 'INFO a.rrd\n',
            'INFO s7.rrd\n')
    session(g, 'RC-60', 'UPDATE a.rrd 1000000300:1:2\nINFO a.rrd\n')
    session(g, 'RC-61', 'LAST\n', 'LAST a.rrd x\n', 'LAST a.rrd\n', 'LAST s7.rrd\n')
    session(g, 'RC-61', 'UPDATE a.rrd 1000000310:1:2\nLAST a.rrd\nFLUSH a.rrd\n'
            'LAST a.rrd\n')
    session(g, 'RC-61', 'UPDATE s7.rrd 1000000010:1 1000000020:1\nFLUSH s7.rrd\n'
            'LAST s7.rrd\nINFO s7.rrd\n')
    session(g, 'RC-62', 'FIRST\n', 'FIRST a.rrd\n', 'FIRST a.rrd 0\n',
            'FIRST a.rrd 1\n', 'FIRST a.rrd 2\n', 'FIRST a.rrd -1\n',
            'FIRST a.rrd x\n', 'FIRST a.rrd 1x\n', 'FIRST a.rrd 0 x\n',
            'FIRST s7.rrd 0\n', 'FIRST a.rrd 99999999999999999999\n')

    # ---- FETCH and FETCHBIN
    g = 'fetch'
    session(g, 'RC-70', 'FETCH\n', 'FETCH a.rrd\n', 'FETCHBIN\n',
            'FETCHBIN a.rrd\n')
    session(g, 'RC-70', 'FETCH a.rrd AVERAGE 1000000000 1000000600\n',
            'FETCH a.rrd MAX 1000000000 1000003000\n',
            'FETCH a.rrd average 1000000000 1000000600\n',
            'FETCH a.rrd MIN 1000000000 1000000600\n',
            'FETCH a.rrd LAST 1000000000 1000000600\n',
            'FETCH s7.rrd LAST 1000000000 1000000035\n')
    session(g, 'RC-71', 'FETCH a.rrd AVERAGE x 1000000600\n',
            'FETCH a.rrd AVERAGE 1000000000 y\n', 'FETCH a.rrd BOGUS x\n',
            'FETCH a.rrd AVERAGE 0x3B9ACA00 1000000600\n',
            'FETCH a.rrd AVERAGE 1000000000abc 07346545000\n',
            'FETCH a.rrd AVERAGE 99999999999999999999 1\n',
            'FETCH a.rrd AVERAGE 1000000000 -99999999999999999999\n',
            'FETCH a.rrd AVERAGE 1000000100 1000000000\n',
            'FETCH a.rrd AVERAGE now-1h now\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000000\n')
    session(g, 'RC-72', 'FETCH a.rrd AVERAGE 1000000000 1000000600 x\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 y\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 y x\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 x x\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 x y x\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 x y \n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600  y\n',
            'FETCH a.rrd AVERAGE 1000000000 1000000600 \n')
    session(g, 'RC-73', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
            'FETCH a.rrd AVERAGE 1000000000 1000000900\nPENDING a.rrd\n')
    session(g, 'RC-73', 'UPDATE a.rrd 1000000300:0.1:2 1000000600:1e300:4 '
            '1000000900:-7.25:4\nFETCH a.rrd AVERAGE 1000000000 1000001200\n'
            'FETCH a.rrd MAX 1000000000 1000001200 x\n')
    session(g, 'RC-74', 'FETCHBIN a.rrd AVERAGE 1000000000 1000000600\n',
            'FETCHBIN a.rrd AVERAGE 1000000000 1000000600 y\n',
            'FETCHBIN a.rrd AVERAGE x\n',
            'FETCHBIN a.rrd AVERAGE 1000000000 1000000600 x x\n',
            'FETCHBIN a.rrd AVERAGE 1000000000 1000000600 x y y\n')
    session(g, 'RC-74', 'UPDATE a.rrd 1000000300:1.5:2 1000000600:3:4\n'
            'FETCHBIN a.rrd AVERAGE 1000000000 1000000900\n')
    # A start a step or more after the end asks the reader for a negative
    # number of rows; the reference's result depends on the allocator.
    session(g, 'RC-71a', 'FETCH a.rrd AVERAGE 1000000600 1000000000\n',
            'FETCH a.rrd MAX 1000003000 1000000000\n',
            'FETCHBIN a.rrd AVERAGE 1000000600 1000000000\n', tags=('undefined',))

    # ---- CREATE
    g = 'create'
    creates = [
        f'CREATE n.rrd -b 1000000007 -s x {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -s 5m {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -s 60x {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -s 0 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -s -5 {D1}\n',
        f'CREATE n.rrd -b 1000000007x {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -bogus 1000000007 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -s 60 -step 120 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000007 -Ox {D1}\nCREATE n.rrd -b 1000000007 -O {D1}\n',
        f'CREATE n.rrd -b x {D1}\n',
        f'CREATE n.rrd -b 100 {D1}\n',
        f'CREATE n.rrd -b 0 {D1}\n',
        f'CREATE n.rrd -b -5 {D1}\n',
        f'CREATE n.rrd -b 315359999 {D1}\n',
        f'CREATE n.rrd -b 315360000 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b1000000000 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b -1 {D1}\n',
        f'CREATE n.rrd --step 60 {D1}\n',
        f'CREATE n.rrd -z {D1}\n',
        f'CREATE n.rrd -b 1000000000 {D1} junk\n',
        f'CREATE n.rrd -b  1000000000 {D1}\n',
        'CREATE n.rrd -b 1000000000 -s\n',
        'CREATE n.rrd -b\n',
        'CREATE n.rrd -b 1000000000 -t\n',
        'CREATE n.rrd -b 1000000000 -r\n',
        'CREATE n.rrd\n',
        'CREATE\n',
        'CREATE n.rrd DS:a:GAUGE:120:U:U\n',
        'CREATE n.rrd RRA:AVERAGE:0.5:1:5\n',
        'CREATE a.rrd -O\n',
        'CREATE n.rrd -b 1000000000 DS:a:BOGUS:1:2:3 RRA:AVERAGE:0.5:1:5\n',
        'CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U RRA:BOGUS:0.5:1:5\n',
        'CREATE n.rrd -b 1000000000 DS:a:GAUGE RRA:AVERAGE:0.5:1:5\n',
        'CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U RRA:AVERAGE:2:1:5\n',
        f'CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U {D1}\n',
        f'CREATE n.rrd -b 1000000000 {D1} RRA:\n',
        f'CREATE n.rrd -b 1000000000 -s 60 {D1}\nLAST n.rrd\nFIRST n.rrd 0\n'
        + 'FETCH n.rrd AVERAGE 1000000000 1000000300\n',
        'CREATE n.rrd -b 1000000000 -s 60 DS:a:COUNTER:120:0:100 '
        + 'DS:b:DERIVE:120:U:U DS:c:ABSOLUTE:120:U:U RRA:MIN:0.5:1:3 '
        + 'RRA:MAX:0.5:2:3 RRA:LAST:0.5:3:3\nFIRST n.rrd 2\n'
        + 'FETCH n.rrd LAST 1000000000 1000000600\n',
        f'CREATE a.rrd -b 1000000000 {D1}\nFETCH a.rrd AVERAGE 1000000000 1000000300\n',
        f'CREATE a.rrd -b 1000000000 -O {D1}\nINFO a.rrd\n',
        f'create n.rrd -b 1000000000 {D1}\nLAST n.rrd\n',
        f'CREATE n.rrd -b 1000000000 {D1}\nUPDATE n.rrd 1000000060:5\n'
        + 'FLUSH n.rrd\nFETCH n.rrd AVERAGE 1000000000 1000000300\n',
        f'CREATE n.rrd -b 1000000000 -s 300 -r a.rrd {D1}\n',
        f'CREATE n.rrd -b 1000000000 -t a.rrd {D1}\n',
        'CREATE n.rrd -b 1000000000 -s 300 DS:a:COMPUTE:x,2,* RRA:AVERAGE:0.5:1:5\n',
        'CREATE n.rrd -b 1000000000 DS:a:GAUGE:120:U:U RRA:HWPREDICT:10:0.1:0.2:5\n',
        'UPDATE a.rrd 1000000300:1:2\nCREATE a.rrd -b 1000000000 ' + D1 +
        '\nPENDING a.rrd\nFETCH a.rrd AVERAGE 1000000000 1000000300\n',
        f'CREATE x/n.rrd -b 1000000000 {D1}\n',
        f'CREATE sub/n.rrd -b 1000000000 {D1}\nLIST /sub\n',
        f'CREATE "q.rrd" -b 1000000000 {D1}\nLIST /\n',
        f'CREATE s\\ q.rrd -b 1000000000 {D1}\nLAST s\\ q.rrd\n',
    ]
    for text in creates:
        session(g, 'RC-80', text, opts=['-B'])
    for text in (f'CREATE a.rrd -b 1000000000 {D1}\nFETCH a.rrd AVERAGE 1000000000 1000000300\n',
                 f'CREATE n.rrd -b 1000000000 {D1}\nCREATE n.rrd -b 1000000000 {D1}\n'):
        session(g, 'RC-81', text, opts=['-O'])
    for opts in (['-B'], ['-B', '-R'], ['-R'], []):
        session(g, 'RC-82', f'CREATE x/y/n.rrd -b 1000000000 {D1}\n',
                f'CREATE sub/new/n.rrd -b 1000000000 {D1}\n', opts=opts,
                observe=['b/x', 'b/x/y', 'b/x/y/n.rrd', 'b/sub/new/n.rrd'])

    # ---- TUNE
    g = 'tune'
    tunes = [
        'TUNE\n', 'TUNE a.rrd\n', 'TUNE a.rrd 1 tune\n',
        'TUNE a.rrd 2 tune -h\n', 'TUNE a.rrd 3 tune -h z:1\n',
        'TUNE a.rrd 2 tune -bogus\n', 'TUNE a.rrd 2 tune --bogus\n',
        'TUNE a.rrd 2 tune --bogus=1\n', 'TUNE a.rrd 2 tune -q\n',
        'TUNE a.rrd 2 tune --heartbeat\n', 'TUNE a.rrd 2 tune --heartbeat=z:5\n',
        'TUNE a.rrd 3 tune --heartbeat z:5\n', 'TUNE a.rrd 3 tune -h x\n',
        'TUNE a.rrd 3 tune -d z:GAUGE\n', 'TUNE a.rrd 3 tune -d x:BOGUS\n',
        'TUNE a.rrd 3 tune -d x:gauge\nINFO a.rrd\n', 'TUNE a.rrd 3 tune -r z:q\n',
        'TUNE a.rrd 3 tune -r x\n', 'TUNE a.rrd 3 tune -i z:1\n',
        'TUNE a.rrd 3 tune -i x:abc\n', 'TUNE a.rrd 3 tune -a x\n',
        'TUNE a.rrd 3 tune junk -h\n', 'TUNE a.rrd 2 tune junk\nINFO a.rrd\n',
        'TUNE a.rrd 2 tune -hx:900\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune -h x:900abc\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune -i x:1abc\nINFO a.rrd\n',
        'TUNE a.rrd 2 tune --maximum=y:5e3\nINFO a.rrd\n',
        'TUNE a.rrd 5 tune -r x:q -h q:77\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune -- -h\nINFO a.rrd\n',
        'TUNE a.rrd 4 tune -h x:900 -h z:1\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune --heartbeat x:900\nINFO a.rrd\n',
        'TUNE a.rrd 5 tune --minimum x:-5 --maximum x:5\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune -d y:DERIVE\nINFO a.rrd\n',
        'TUNE a.rrd 3 tune -r y:z\nINFO a.rrd\n',
        'TUNE a.rrd x tune -h x:900\nINFO a.rrd\n',
        'TUNE a.rrd 9 tune -h x:900\nINFO a.rrd\n',
        'TUNE a.rrd 0 tune -h x:900\nINFO a.rrd\n',
        'TUNE a.rrd 3 notune -h x:900\nINFO a.rrd\n',
        'UPDATE a.rrd 1000000300:1:2\nTUNE a.rrd 3 tune -h x:900\nPENDING a.rrd\n',
    ]
    for text in tunes:
        hangs = text.startswith('TUNE a.rrd 9 ')
        session(g, 'RC-90 RC-91' if hangs else 'RC-90', text, opts=['-B'],
                tags=('undefined',) if hangs else ())

    # ---- DUMP
    g = 'dump'
    session(g, 'RC-95', 'DUMP\n')
    session(g, 'RC-95', 'DUMP a.rrd\n')
    session(g, 'RC-95', 'DUMP s7.rrd -h dtd\n')
    session(g, 'RC-95', 'DUMP s7.rrd -h none\n')
    session(g, 'RC-95', 'DUMP s7.rrd x y z\n')
    session(g, 'RC-95', 'DUMP s7.rrd\nPING\n')
    session(g, 'RC-95', 'UPDATE s7.rrd 1000000010:4\nDUMP s7.rrd\n')
    session(g, 'RC-95', 'PING\nDUMP missing.rrd\nPING\n')

    # ---- LIST
    g = 'list'
    lists = ['LIST\n', 'LIST /\n', 'LIST RECURSIVE /\n', 'LIST /sub\n',
             'LIST /sub/\n', 'LIST RECURSIVE /sub\n', 'LIST RECURSIVE sub\n',
             'LIST /sub/deep\n', 'LIST /a.rrd\n', 'LIST /sub/d.rrd\n',
             'LIST RECURSIVE /sub/d.rrd\n', 'LIST /missing.rrd\n', 'LIST /dir.rrd\n',
             'LIST /a.rrd/x\n', 'LIST /sub extra\n', 'LIST RECURSIVE\n', 'LIST sub\n',
             'LIST recursive /\n', 'LIST /empty.d\n', 'LIST RECURSIVE /empty.d\n',
             'LIST /../\n', 'LIST /lnk\n', 'LIST /notes.txt\n', 'list /\n',
             'LIST /text.rrd\n', 'LIST //\n', 'LIST /./sub\n']
    for opts in (['-B'], []):
        for text in lists:
            session(g, 'RC-100', text, opts=opts)
    session(g, 'RC-100', 'CREATE n.rrd -b 1000000000 ' + D1 + '\nLIST /\n', opts=['-B'])

    # ---- SUSPEND and RESUME
    g = 'suspend'
    session(g, 'RC-110', 'SUSPEND\nPING\n', end='none')
    session(g, 'RC-110', 'RESUME\nPING\n', end='none')
    session(g, 'RC-110', 'SUSPEND a.rrd\nSUSPEND a.rrd\nRESUME a.rrd\nRESUME a.rrd\n')
    session(g, 'RC-110', 'SUSPEND a.rrd x\nRESUME a.rrd x\n')
    session(g, 'RC-111', 'SUSPENDALL\nRESUMEALL\n', 'SUSPENDALL x\nRESUMEALL x\n')
    session(g, 'RC-111', 'UPDATE a.rrd 1000000300:1:2\nUPDATE sub/d.rrd 1000000300:1:2\n'
            'SUSPENDALL\nSUSPENDALL\nRESUMEALL\nRESUMEALL\n')
    session(g, 'RC-111', 'UPDATE a.rrd 1000000300:1:2\nSUSPEND a.rrd\n'
            'UPDATE sub/d.rrd 1000000300:1:2\nSUSPENDALL\nRESUMEALL\n')
    session(g, 'RC-112', 'UPDATE a.rrd 1000000300:1:2\nSUSPEND a.rrd\nFLUSH a.rrd\n'
            'PENDING a.rrd\nINFO a.rrd\nRESUME a.rrd\nFLUSH a.rrd\nPENDING a.rrd\n')
    session(g, 'RC-112', 'UPDATE a.rrd 1000000300:1:2\nSUSPEND a.rrd\nFLUSHALL\nPING\n',
            'PENDING a.rrd\nQUEUE\n')
    session(g, 'RC-112', 'SUSPEND a.rrd\nUPDATE a.rrd 1000000300:1:2\nPENDING a.rrd\n'
            'FORGET a.rrd\nRESUME a.rrd\n')
    session(g, 'RC-112', 'SUSPEND a.rrd\n', 'RESUME a.rrd\n')
    session(g, 'RC-110', 'UPDATE a.rrd 1000000300:1:2\nSUSPEND a.rrd\nSUSPEND a.rrd\nPING\n',
            'RESUME a.rrd\nRESUME a.rrd\nPING\n')
    session(g, 'RC-110', 'UPDATE a.rrd 1000000300:1:2\nRESUME a.rrd\nPING\n')
    session(g, 'RC-112', 'BATCH\nSUSPEND\nPING\n.\nPING\n', end='none')

    # ---- BATCH
    g = 'batch'
    session(g, 'RC-120', 'BATCH\n.\n')
    session(g, 'RC-120', 'BATCH\nPING\nPING\n.\n')
    session(g, 'RC-120', 'BATCH\nUPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd x\nPING\n'
            'NOPE\nUPDATE missing.rrd 1:1\n.\nPENDING a.rrd\n')
    session(g, 'RC-120', 'batch\nnope\n.\n')
    session(g, 'RC-121', 'BATCH x\n.\n', 'BATCH x y\nPING\n.\n')
    session(g, 'RC-121', 'BATCH\nBATCH\n.\n.\n')
    session(g, 'RC-121', 'BATCH\nHELP\n.\n', 'BATCH\nHELP PING\nSTATS\nQUEUE\n.\n')
    session(g, 'RC-121', 'BATCH\nQUIT\n.\nPING\n', end='none')
    session(g, 'RC-121', 'BATCH\n\n\n.\n')
    session(g, 'RC-121', 'BATCH\n. \n.\n', 'BATCH\n.x\n.\n', 'BATCH\n .\n.\n')
    session(g, 'RC-121', '.\nPING\n')
    session(g, 'RC-121', 'BATCH\nPING\n', end='linger')
    session(g, 'RC-121', 'BATCH\nNOPE\n', end='linger')
    session(g, 'RC-122', 'BATCH\n' + 'NOPE\n' * 20 + '.\n')
    session(g, 'RC-122', 'BATCH\n' + 'UPDATE a.rrd x\n' * 3 + 'FETCH a.rrd\n'
            'FIRST a.rrd x\n.\n')
    session(g, 'RC-122', 'BATCH\nFETCH a.rrd AVERAGE 1000000000 1000000600\n'
            'INFO a.rrd\nLAST a.rrd\nFIRST a.rrd 0\nLIST /\n.\n')
    session(g, 'RC-122', 'BATCH\nCREATE n.rrd -b 1000000000 ' + D1 + '\n'
            'CREATE n.rrd -b x ' + D1 + '\n.\nLAST n.rrd\n')
    session(g, 'RC-122', 'BATCH\nSUSPEND a.rrd\nSUSPEND a.rrd\nRESUME missing.rrd\n'
            'SUSPENDALL\nRESUMEALL\n.\n')
    session(g, 'RC-122', 'BATCH\nFLUSH a.rrd\nFLUSHALL\nFORGET a.rrd\nPENDING a.rrd\n.\n')
    session(g, 'RC-122', 'BATCH\nTUNE a.rrd 2 tune -h\nTUNE a.rrd 3 tune -h x:900\n.\n'
            'INFO a.rrd\n')
    session(g, 'RC-122', 'BATCH\nDUMP a.rrd\n.\nPING\n')
    session(g, 'RC-122', 'BATCH\nWROTE a.rrd\n.\n')
    session(g, 'RC-121', 'BATCH\nRESUMEALL\nSUSPENDALL\nRESUMEALL\nFLUSH a.rrd\n'
            'UPDATE a.rrd 1000000300:1:2\nFORGET a.rrd\nFORGET a.rrd\n.\nPING\n')
    session(g, 'RC-122', 'BATCH\nCREATE x/n.rrd -b 1000000000 ' + D1 + '\nNOPE\n.\n')
    session(g, 'RC-121', 'BATCH\nRESUME a.rrd\n.\nPING\n')
    session(g, 'RC-121', 'BATCH\nUPDATE a.rrd 1000000300:1:2\nSUSPEND a.rrd\nRESUME a.rrd\n'
            '.\nPENDING a.rrd\n')
    session(g, 'RC-122', 'BATCH\nUPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd 1000000300:1:2\n'
            '.\nBATCH\nUPDATE a.rrd 1000000600:1:2\n.\nPENDING a.rrd\n')

    # ---- -P permissions
    g = 'perm'
    perms = [
        (['-P', 'PING'], 'PING\nSTATS\nHELP\nQUIT\n', 'none'),
        (['-P', 'FLUSH,PENDING'], 'PING\nFLUSH a.rrd\nPENDING a.rrd\nUPDATE a.rrd 1:1\n'
         'HELP PING\n', 'quit'),
        (['-P', 'flush,pending'], 'FLUSH a.rrd\nPENDING a.rrd\n', 'quit'),
        (['-P', 'BATCH'], 'BATCH\nPING\nUPDATE a.rrd 1000000300:1:2\n.\n', 'quit'),
        (['-P', 'BATCH,UPDATE'], 'BATCH\nUPDATE a.rrd 1000000300:1:2\nPING\n.\n', 'quit'),
        (['-P', 'UPDATE'], 'BATCH\nUPDATE a.rrd 1000000300:1:2\n.\nPING\n', 'quit'),
        (['-P', 'PING,'], 'PING\nSTATS\n', 'quit'),
        (['-P', ',PING'], 'PING\nSTATS\n', 'quit'),
        (['-P', 'PING,,STATS'], 'PING\nSTATS\n', 'quit'),
        (['-P', 'PING STATS'], 'PING\nSTATS\n', 'quit'),
        (['-P', 'NOPE,PING'], 'PING\n', 'quit'),
        (['-P', 'WROTE'], 'WROTE a.rrd\nPING\n', 'quit'),
        (['-P', 'QUIT'], 'PING\nQUIT\n', 'none'),
        (['-P', 'PING', '-P', 'STATS'], 'PING\nSTATS\n', 'quit'),
        (['-P', 'HELP'], 'HELP\nHELP STATS\nSTATS\n', 'quit'),
        (['-P', 'STATS'], 'NOPE\nnope\nSTATS\n', 'quit'),
        (['-P', 'FLUSHALL,SUSPENDALL'], 'FLUSHALL\nSUSPENDALL\nRESUMEALL\n', 'quit'),
        (['-P', 'LIST'], 'LIST /\nINFO a.rrd\n', 'quit'),
        (['-P', 'FETCH'], 'FETCH a.rrd AVERAGE 1000000000 1000000600\n'
         'FETCHBIN a.rrd AVERAGE 1000000000 1000000600\n', 'quit'),
        (['-P', 'CREATE'], 'CREATE n.rrd -b 1000000000 ' + D1 + '\nLAST n.rrd\n', 'quit'),
    ]
    for opts, text, end in perms:
        session(g, 'RC-130', text, opts=opts, end=end)

    # ---- no -b
    g = 'nobase'
    for text in ('LIST /\n', 'LIST RECURSIVE /\n', 'PING\n',
                 'LAST {ROOT}/b/a.rrd\n', 'INFO {ROOT}/b/s7.rrd\n',
                 'LAST missing-rondi-spec.rrd\n',
                 'UPDATE {ROOT}/b/a.rrd 1000000300:1:2\nPENDING {ROOT}/b/a.rrd\n',
                 'FLUSH {ROOT}/b/a.rrd\n'):
        steps = [daemon(argv=['-g', '-p', '{ROOT}/pid', '-w', '3600', '-f', '7200',
                              '-l', 'unix:{ROOT}/s.sock']), conn(text), stop()]
        case(g, 'RC-32', steps)

    # ---- STATS with a journal
    g = 'journal'
    jopts = ['-j', '{ROOT}/j']
    session(g, 'RC-55', 'STATS\n', 'UPDATE a.rrd 1000000300:1:2\nSTATS\n',
            'FLUSH a.rrd\n', 300, 'STATS\n', 'FORGET a.rrd\nSTATS\n', opts=jopts,
            tags=('timing',))
    session(g, 'RC-55', 'UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
            'UPDATE {ROOT}/b/sub/d.rrd 1000000300:1:2\nSTATS\n', opts=jopts)
    session(g, 'RC-14', 'WROTE a.rrd\nWROTE\n', opts=jopts)

    # ---- TCP transport
    g = 'tcp'
    for text in ('PING\nHELP PING\n', 'UPDATE a.rrd 1000000300:1:2\nPENDING a.rrd\n',
                 'LIST /\n', 'UPDATE /a.rrd 1000000300:1:2\nPENDING /a.rrd\n'):
        steps = [daemon(['-B', '-l', '127.0.0.1:{PORT}']), conn(text, via='tcp'),
                 stop()]
        case(g, 'RC-2', steps)

    # ---- connections sharing state
    g = 'multi'
    session(g, 'RC-3', 'UPDATE a.rrd 1000000300:1:2\n', 'PENDING a.rrd\n',
            'UPDATE a.rrd 1000000600:3:4\nPENDING a.rrd\nQUEUE\n', 'STATS\n')
    session(g, 'RC-3', 'BATCH\nUPDATE a.rrd 1000000300:1:2\n', 'PENDING a.rrd\n',
            end='linger')


def build_daemon_cases() -> None:
    """Startup, signal, pid file and journal cases (rrdcached-daemon.md)."""
    g = 'opt'
    base = ['-g', '-p', '{ROOT}/pid']
    listen = ['-l', 'unix:{ROOT}/s.sock']
    exits = [
        ['-h'], ['--help'], ['-x'], ['--bogus'], ['-w'],
        base + ['-b', '{ROOT}/b', '-w', '0'] + listen,
        base + ['-b', '{ROOT}/b', '-w', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-w', '-5'] + listen,
        base + ['-b', '{ROOT}/b', '-f', '0'] + listen,
        base + ['-b', '{ROOT}/b', '-f', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-t', '0'] + listen,
        base + ['-b', '{ROOT}/b', '-t', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-a', '0'] + listen,
        base + ['-b', '{ROOT}/b', '-a', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-z', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-z', '-1'] + listen,
        base + ['-b', '{ROOT}/b', '-m', '999'] + listen,
        base + ['-b', '{ROOT}/b', '-m', 'x'] + listen,
        base + ['-b', '{ROOT}/b', '-s', 'no-such-group-rondi'] + listen,
        base + ['-b', '{ROOT}/b', '-U', 'no-such-user-rondi'] + listen,
        base + ['-b', '{ROOT}/b', '-G', 'no-such-group-rondi'] + listen,
        base + ['-b', '{ROOT}/b', '-P', 'BOGUS'] + listen,
        base + ['-b', 'b'] + listen,
        base + ['-b', '.'] + listen,
        base + ['-b', '/'] + listen,
        base + ['-b', '{ROOT}/b/lnk'] + listen,
        base + ['-b', '{ROOT}/b/a.rrd'] + listen,
        base + ['-b', '{ROOT}/b', '-l', '[::1'],
        base + ['-b', '{ROOT}/b', '-l', 'no.such.host.invalid:1'],
        base + ['-b', '{ROOT}/b', '-l', '127.0.0.1:x'],
        base + ['-b', '{ROOT}/b', '-j', '{ROOT}/missing-j'] + listen,
        base + ['-b', '{ROOT}/b', '-j', '{ROOT}/b/a.rrd'] + listen,
    ]
    for argv in exits:
        case(g, 'RCD-1', [{'daemon': argv, 'start': 'exit'}])
    serves = [
        base + ['-b', '{ROOT}/b', '-w', '5m', '-f', '1h'] + listen,
        base + ['-b', '{ROOT}/b', '-w', '100', '-f', '150'] + listen,
        base + ['-b', '{ROOT}/b', '-w1800', '-f7200'] + listen,
        base + ['-gB', '-b', '{ROOT}/b'] + listen,
        base + ['-b', '{ROOT}/new/base'] + listen,
        base + ['-b', '{ROOT}/b', '-R'] + listen,
        base + ['-b', '{ROOT}/b', '-z', '10'] + listen,
        base + ['-b', '{ROOT}/b', '-V', 'LOG_DEBUG'] + listen,
        base + ['-b', '{ROOT}/b', '-t', '2', '-a', '4'] + listen,
        base + ['-b', '{ROOT}/b', '-P', 'BOGUS', '-P', 'PING'] + listen,
        base + ['-b', '{ROOT}/b', '-m', '0640'] + listen,
        base + ['-b', '{ROOT}/b', '-m', '0777'] + listen,
        base + ['-b', '{ROOT}/b', '-l', '{ROOT}/s.sock'],
        base + ['-b', '{ROOT}/b', '-o', '{ROOT}/log'] + listen,
        base + ['-b', '{ROOT}/b', '-P', ''] + listen,
        base + ['-b', '{ROOT}/b', '-V', 'LOG_BOGUS'] + listen,
        base + ['-b', '{ROOT}/b/'] + listen,
        base + ['-b', '{ROOT}/b', '-l', 'unix:{ROOT}/missing/s.sock'] + listen,
        ['-g', '-p', '{ROOT}/missing/pid', '-b', '{ROOT}/b'] + listen,
        base + ['-b', '{ROOT}/b', 'extra-word'] + listen,
    ]
    observed = ['s.sock', 'pid', 'new', 'new/base', 'log', 'missing',
                'missing/s.sock', 'missing/pid']
    for argv in serves:
        case(g, 'RCD-2', [{'daemon': argv, 'start': 'serve'},
                          conn('PING\nSTATS\nLIST /\n'), {'files': observed}, stop(),
                          {'files': observed}])

    # pid file
    g = 'pid'
    std = base + ['-b', '{ROOT}/b'] + listen
    case(g, 'RCD-10', [daemon(argv=std), {'files': ['pid:content']}, stop(),
                       {'files': ['pid', 's.sock']}])
    case(g, 'RCD-10', [{'write': 'pid', 'text': '99999999\n'}, daemon(argv=std),
                       {'files': ['pid:content']}, stop()])
    case(g, 'RCD-10', [{'write': 'pid', 'text': 'garbage'}, daemon(argv=std), stop(),
                       {'files': ['pid']}])
    case(g, 'RCD-11', [daemon(argv=std),
                       {'daemon2': base + ['-b', '{ROOT}/b', '-l',
                                           'unix:{ROOT}/s2.sock']},
                       stop(), {'files': ['s2.sock', 'pid']}])
    case(g, 'RCD-12', [daemon(argv=std), {'daemon2': ['-g', '-p', '{ROOT}/pid2', '-b',
                                                      '{ROOT}/b'] + listen},
                       stop(), {'files': ['pid2']}])

    # signals
    g = 'signal'
    pending = 'UPDATE a.rrd 1000000300:1:2\nPENDING a.rrd\n'
    after = 'INFO a.rrd\n'
    for sig in ('TERM', 'INT', 'USR1', 'USR2'):
        for extra in ([], ['-j', '{ROOT}/j'], ['-j', '{ROOT}/j', '-F'], ['-F']):
            opts = base + ['-b', '{ROOT}/b', '-w', '3600', '-f', '7200'] + extra + listen
            case(g, 'RCD-20', [daemon(argv=opts), conn(pending), stop(sig),
                               {'files': ['j/*']},
                               daemon(argv=base + ['-b', '{ROOT}/b'] + listen),
                               conn(after), stop()])
    for sig in ('HUP', 'PIPE', 'ALRM'):
        opts = base + ['-b', '{ROOT}/b'] + listen
        case(g, 'RCD-21', [daemon(argv=opts), {'signal_only': sig}, conn('PING\n'),
                           stop()])

    # journal
    g = 'journal'
    jl = base + ['-b', '{ROOT}/b', '-w', '3600', '-f', '7200', '-j', '{ROOT}/j'] + listen
    case(g, 'RCD-30', [daemon(argv=jl),
                       conn('UPDATE a.rrd 1000000300:1:2 1000000600:3:4\n'
                            'UPDATE sub/d.rrd 1000000300:5:6\n'),
                       {'files': ['j/*:content']}, stop('USR2'),
                       {'files': ['j/*:content']}])
    case(g, 'RCD-30', [daemon(argv=jl),
                       conn('UPDATE a.rrd 1000000300:1:2\nFLUSH a.rrd\n'
                            'UPDATE sub/d.rrd 1000000300:5:6\nFORGET sub/d.rrd\n'),
                       stop('USR2'), {'files': ['j/*:content']}])
    case(g, 'RCD-31', [daemon(argv=jl), conn('UPDATE a.rrd 1000000300:1:2\n'),
                       stop('USR2'), daemon(argv=jl),
                       conn('PENDING a.rrd\nSTATS\nFLUSH a.rrd\nINFO a.rrd\n'), stop()])
    case(g, 'RCD-31', [daemon(argv=jl), conn('UPDATE a.rrd 1000000300:1:2\n'),
                       stop('USR2'), daemon(argv=jl), conn('PENDING a.rrd\n'),
                       stop('TERM'), {'files': ['j/*']}])
    case(g, 'RCD-31', [daemon(argv=jl), conn('UPDATE a.rrd 1000000300:1:2\n'),
                       stop('USR2'), daemon(argv=jl + ['-F']),
                       conn('PENDING a.rrd\n'), stop('TERM'), {'files': ['j/*']},
                       daemon(argv=std), conn('INFO a.rrd\n'), stop()])
    for text in ('update {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\nwrote {ROOT}/b/a.rrd\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\nforget {ROOT}/b/a.rrd\n',
                 'UPDATE {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\nbogus line\n'
                 'update {ROOT}/b/a.rrd 1000000600:3:4\n',
                 'update a.rrd 1000000300:1:2\n',
                 'update {ROOT}/outside.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\nupdate {ROOT}/b/a.rrd '
                 '1000000300:3:4\n',
                 'ping\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 '',
                 'bogus line\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'PING\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\nflush {ROOT}/b/a.rrd\n'
                 'update {ROOT}/b/a.rrd 1000000600:3:4\n',
                 'update {ROOT}/b/a.rrd x\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/missing.rrd 1:1\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'wrote {ROOT}/b/a.rrd\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n',
                 'update {ROOT}/b/a.rrd 1000000300:1:2\r\n',
                 '\nupdate {ROOT}/b/a.rrd 1000000300:1:2\n'):
        for name in ('rrd.journal.1000000000.000000',):
            case(g, 'RCD-32', [{'write': f'j/{name}', 'text': text},
                               daemon(argv=jl),
                               conn('PENDING a.rrd\nPENDING {ROOT}/outside.rrd\nSTATS\n'),
                               stop('USR2'), {'files': ['j/*']}])
    case(g, 'RCD-32', [{'write': 'j/rrd.journal', 'text':
                        'update {ROOT}/b/a.rrd 1000000300:1:2\n'},
                       daemon(argv=jl), conn('PENDING a.rrd\n'), stop('USR2'),
                       {'files': ['j/*']}])
    case(g, 'RCD-32', [{'write': 'j/other.file', 'text':
                        'update {ROOT}/b/a.rrd 1000000300:1:2\n'},
                       daemon(argv=jl), conn('PENDING a.rrd\n'), stop('USR2'),
                       {'files': ['j/*']}])
    case(g, 'RCD-32', [{'write': 'j/rrd.journal.1000000002.000000', 'text':
                        'update {ROOT}/b/a.rrd 1000000600:3:4\n'},
                       {'write': 'j/rrd.journal.1000000001.000000', 'text':
                        'update {ROOT}/b/a.rrd 1000000300:1:2\n'},
                       daemon(argv=jl), conn('PENDING a.rrd\n'), stop('USR2'),
                       {'files': ['j/*']}])

    # timers
    g = 'timer'
    fast = base + ['-b', '{ROOT}/b', '-w', '1', '-f', '2'] + listen
    case(g, 'RCD-40', [daemon(argv=fast), conn('UPDATE a.rrd 1000000300:1:2\n'),
                       {'sleep': 3500}, conn('PENDING a.rrd\nINFO a.rrd\n'), stop()],
         tags=('timing',))
    case(g, 'RCD-40', [daemon(argv=fast),
                       conn('UPDATE a.rrd 1000000300:1:2\nUPDATE a.rrd 1000000600:1:2\n'),
                       {'sleep': 1500},
                       conn('UPDATE a.rrd 1000000900:1:2\n'), {'sleep': 500},
                       conn('PENDING a.rrd\nQUEUE\n'), stop()], tags=('timing',))
    slow = base + ['-b', '{ROOT}/b', '-w', '3600', '-f', '1'] + listen
    case(g, 'RCD-41', [daemon(argv=slow), conn('UPDATE a.rrd 1000000300:1:2\n'),
                       {'sleep': 2500}, conn('PENDING a.rrd\nSTATS\n'), stop()],
         tags=('timing',))


# Why Rondi differs from the reference in a case. `policy` is an intentional
# hardening difference that a reimplementation keeps; `defect` is a known gap
# to close. The first matching entry classifies a case whose Rondi result
# differs (or whose reference result is undefined); --record-rondi refuses to
# finish while a differing case matches none.

def _sends(item: dict) -> str:
    return '\n'.join(s['conn'] for s in item['steps'] if 'conn' in s)


def _argv(item: dict) -> list[str]:
    return [w for s in item['steps'] for w in s.get('daemon', [])]


def _has(item: dict, *needles: str) -> bool:
    text = _sends(item)
    return any(n in text for n in needles)


SYMLINKS = (' link.rrd', ' lnk/d.rrd', ' escape.rrd')
DIVERGENCE_RULES: list[tuple[object, str, str]] = [
    (lambda c: 'undefined' in c['tags'] and c['id'].startswith('tune'), 'policy',
     'The reference stops answering when TUNE names more words than it sends '
     '(RC-91); Rondi answers with the argument count error.'),
    (lambda c: 'undefined' in c['tags'] and '\\\n' in _sends(c), 'policy',
     'The reference reads past a line that ends in a backslash (RC-12a); Rondi '
     'answers deterministically.'),
    (lambda c: 'undefined' in c['tags'] and c['id'].startswith('fetch'), 'policy',
     'The reference result for a FETCH whose start is a step or more after its '
     'end depends on the allocator (RC-71a); Rondi answers with the reader '
     'error.'),
    (lambda c: 'undefined' in c['tags'], 'policy',
     'The reference sends uninitialized memory in replies longer than 4095 '
     'bytes (RC-18); Rondi sends the whole reply.'),
    (lambda c: c['id'].startswith('frame') and _has(c, '\xff', '\xe9'), 'policy',
     'Rondi closes a connection whose request line is not UTF-8 (RC-16).'),
    (lambda c: c['id'].startswith('frame') and len(_sends(c)) > 4096, 'policy',
     'Rondi refuses an UPDATE longer than 4095 bytes after the command word '
     '(RC-49).'),
    (lambda c: c['id'].startswith('pB-') and _has(c, *SYMLINKS), 'policy',
     'Under -B Rondi refuses a symbolic link below the base directory (RC-31).'),
    (lambda c: c['id'].startswith('pB-dump'), 'policy',
     'Rondi applies -B to DUMP (RC-31, RC-95).'),
    (lambda c: _has(c, 'CREATE dir.rrd '), 'policy',
     'CREATE refuses to replace a directory, link or special file (RC-31).'),
    (lambda c: _has(c, 'CREATE  -b'), 'policy',
     'CREATE refuses an empty file name (RC-31).'),
    (lambda c: _has(c, ' -r a.rrd', ' -t a.rrd', 'COMPUTE', 'HWPREDICT'), 'defect',
     'Rondi refuses CREATE -r, -t, COMPUTE and Holt-Winters archives (RC-80).'),
    (lambda c: _has(c, 'LIST RECURSIVE /\n') and '-B' in _argv(c), 'policy',
     'Under -B Rondi does not follow symbolic links when listing (RC-100).'),
    (lambda c: '{ROOT}/new/base' in _argv(c), 'policy',
     'Rondi creates missing -b components with mode 0750 and Unix sockets '
     'with mode 0600 (RCD-3, RCD-5).'),
    (lambda c: any('daemon2' in s for s in c['steps']) and 'pid2' in str(c['steps']),
     'policy', 'Rondi refuses a live socket instead of unlinking it (RCD-11).'),
    (lambda c: c['kind'] == 'daemon' and c['id'].startswith('opt'), 'policy',
     'Rondi creates Unix sockets with mode 0600 unless -m or -s is given '
     '(RCD-5).'),
    (lambda c: '-j' in _argv(c) or c['id'].startswith('journal'), 'policy',
     'Rondi creates journal files with mode 0600 (RCD-30).'),
]


# Divergences that appear on one platform only, by expect_platform key.
PLATFORM_NOTES = {
    'linux-x86_64':
        'Rondi prints and sends every unknown value with the platform default '
        'NaN sign; the reference keeps the sign bit stored in the file, so rows '
        'of a file written on another architecture show nan among -nan (RC-70).',
}


def classify(item: dict) -> tuple[str, str] | None:
    for predicate, klass, note in DIVERGENCE_RULES:
        if predicate(item):
            return klass, note
    return None


# ---------------------------------------------------------------------------
# Runner

def expand(text: str, ctx: dict) -> str:
    for key, value in ctx.items():
        if not key.startswith('_'):
            text = text.replace('{' + key + '}', value)
    return text


def normalize(data: bytes, ctx: dict) -> str:
    text = data.decode('latin-1')
    for alias in ctx['_roots']:
        text = text.replace(alias, '{ROOT}')
    if 'PORT' in ctx:
        text = text.replace(':' + ctx['PORT'], ':{PORT}')
    if ctx.get('_pid'):
        text = re.sub(rf"\b{re.escape(ctx['_pid'])}\b", '{PID}', text)
    return text


def free_port() -> str:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(('127.0.0.1', 0))
        return str(sock.getsockname()[1])


def give_to_nobody(root: Path) -> None:
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            os.lchown(os.path.join(dirpath, name), NOBODY, NOBODY)
    os.chown(root, NOBODY, NOBODY)


def connect(ctx: dict, via: str) -> socket.socket:
    if via == 'tcp':
        sock = socket.create_connection(('127.0.0.1', int(ctx['PORT'])),
                                        timeout=CONNECT_TIMEOUT)
    else:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(CONNECT_TIMEOUT)
        sock.connect(ctx['ROOT'] + '/s.sock')
    return sock


def answers(ctx: dict, via: str) -> bool:
    """Whether the daemon answers a request. A listening socket accepts
    connections before the daemon has finished starting (and installed its
    signal handlers), so a connect alone does not show it is ready."""
    try:
        with connect(ctx, via) as sock:
            sock.settimeout(1.0)
            sock.sendall(b'PING\n')
            return sock.recv(1) != b''
    except OSError:
        return False


def wait_listening(proc: subprocess.Popen, ctx: dict, via: str) -> str | None:
    deadline = time.monotonic() + CONNECT_TIMEOUT
    while time.monotonic() < deadline:
        if answers(ctx, via):
            return None
        if proc.poll() is not None:
            return 'exited'
        time.sleep(0.01)
    return 'timeout'


def talk(ctx: dict, step: dict) -> str:
    try:
        sock = connect(ctx, step['via'])
    except OSError as err:
        return f'<connect failed: {err.__class__.__name__}>'
    data = expand(step['conn'], ctx).encode('latin-1')
    reply = b''
    with sock:
        sock.settimeout(READ_TIMEOUT)
        try:
            sock.sendall(data)
            if step['end'] == 'quit':
                sock.sendall(b'QUIT\n')
        except OSError:
            # The daemon may close first (DUMP, QUIT, an over-long line);
            # whatever it sent before closing is still read below.
            pass
        if step['end'] == 'linger':
            sock.settimeout(LINGER)
        try:
            while True:
                chunk = sock.recv(65536)
                if not chunk:
                    break
                reply += chunk
        except TimeoutError:
            if step['end'] != 'linger':
                reply += b'<timeout>'
        except OSError:
            # A daemon that closes with unread input pending resets the
            # connection on Linux and closes it normally on macOS; both
            # end the reply.
            pass
    return normalize(reply, ctx)


def finish(proc: subprocess.Popen, ctx: dict) -> dict:
    if hasattr(proc, 'outcome'):
        return proc.outcome
    try:
        proc.wait(timeout=EXIT_TIMEOUT)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return {'status': 'timeout'}
    code = proc.returncode
    outputs = []
    for path in proc.captured:
        with open(path, 'rb') as handle:
            outputs.append(normalize(handle.read(), ctx))
    proc.outcome = {'status': code if code >= 0 else f'signal {-code}',
                    'stdout': outputs[0], 'stderr': outputs[1]}
    return proc.outcome


def files(root: Path, patterns: list[str], ctx: dict) -> dict:
    found: dict[str, str] = {}
    for pattern in patterns:
        content = pattern.endswith(':content')
        pattern = pattern.removesuffix(':content')
        paths = sorted(glob.glob(str(root / pattern))) if '*' in pattern else [
            str(root / pattern)]
        for path in paths:
            name = os.path.relpath(path, root)
            name = re.sub(r'rrd\.journal\.\d+\.\d+', 'rrd.journal.{T}', name)
            while name in found:
                name += "'"
            try:
                info = os.lstat(path)
            except FileNotFoundError:
                found[name] = 'missing'
                continue
            if stat.S_ISLNK(info.st_mode):
                kind = 'link'
            elif stat.S_ISDIR(info.st_mode):
                kind = 'dir'
            elif stat.S_ISSOCK(info.st_mode):
                kind = 'socket'
            else:
                kind = 'file'
            entry = f'{kind} {stat.S_IMODE(info.st_mode):04o}'
            if content and kind == 'file':
                with open(path, 'rb') as handle:
                    entry += '\n' + normalize(handle.read(), ctx)
            found[name] = entry
    return found


def padded_root(temp: str) -> tuple[Path, str]:
    """The fixture root inside `temp`, padded to ROOT_LENGTH bytes.

    Journal lines and so JournalBytes contain absolute paths, so the root's
    length must not depend on the host's temporary directory."""
    real = os.path.realpath(temp)
    pad = ROOT_LENGTH - len(real) - 1
    if pad < 1:
        raise RuntimeError(f'temporary directory {real} is too long')
    name = 'r' * pad
    return Path(real) / name, os.path.join(temp, name)


def run_case(program: str, item: dict) -> list:
    temp = tempfile.mkdtemp(prefix='rc')
    root, alias = padded_root(temp)
    root.mkdir()
    ctx = {'ROOT': str(root), '_logs': temp,
           '_roots': sorted({str(root), alias}, key=len, reverse=True)}
    procs: list[subprocess.Popen] = []
    results: list = []
    try:
        layout(root)
        if os.geteuid() == 0:
            os.chmod(temp, 0o755)
        if any('PORT' in json.dumps(s) for s in item['steps']):
            ctx['PORT'] = free_port()
        for step in item['steps']:
            if 'write' in step:
                target = root / step['write']
                target.write_bytes(expand(step['text'], ctx).encode('latin-1'))
                os.chmod(target, 0o644)
                results.append(None)
                continue
            if os.geteuid() == 0 and not procs:
                give_to_nobody(root)
            results.append(run_step(program, step, root, ctx, procs))
    finally:
        for proc in procs:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
        reap_pidfile(root)
        shutil.rmtree(temp, ignore_errors=True)
    return results


def reap_pidfile(root: Path) -> None:
    """A daemon that forked leaves its child running; stop it."""
    for name in ('pid', 'pid2'):
        try:
            pid = int((root / name).read_text().strip())
        except (OSError, ValueError):
            continue
        with contextlib.suppress(OSError):
            os.kill(pid, signal.SIGKILL)


def spawn(program: str, argv: list[str], ctx: dict) -> subprocess.Popen:
    """Output goes to files beside the root, so a chatty daemon never
    blocks. Under root the daemon runs as nobody with no extra groups."""
    args = [program] + [expand(word, ctx) for word in argv]
    env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'TZ': 'UTC',
           'LC_ALL': 'C'}
    ctx['_spawned'] = ctx.get('_spawned', 0) + 1
    paths = tuple(os.path.join(ctx['_logs'], f'{name}.{ctx["_spawned"]}')
                  for name in ('stdout', 'stderr'))
    drop = ({'user': NOBODY, 'group': NOBODY, 'extra_groups': []}
            if os.geteuid() == 0 else {})
    with open(paths[0], 'wb') as out, open(paths[1], 'wb') as err:
        proc = subprocess.Popen(args, stdin=subprocess.DEVNULL, stdout=out,
                                stderr=err, env=env, cwd=ctx['ROOT'],
                                umask=0o022, **drop)
    proc.captured = paths
    return proc


def run_step(program: str, step: dict, root: Path, ctx: dict,
             procs: list[subprocess.Popen]):
    if 'daemon' in step:
        proc = spawn(program, step['daemon'], ctx)
        procs.append(proc)
        if step['start'] == 'exit':
            return finish(proc, ctx)
        via = 'tcp' if any('{PORT}' in w for w in step['daemon']) else 'unix'
        problem = wait_listening(proc, ctx, via)
        if problem:
            if problem == 'exited':
                return finish(proc, ctx)
            return {'status': 'not listening'}
        try:
            ctx['_pid'] = (root / 'pid').read_text().strip()
        except OSError:
            ctx.pop('_pid', None)
        return None
    if 'daemon2' in step:
        proc = spawn(program, step['daemon2'], ctx)
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.send_signal(signal.SIGTERM)
        return finish(proc, ctx)
    if 'conn' in step:
        return talk(ctx, step)
    if 'signal' in step or 'signal_only' in step:
        proc = procs[-1]
        if proc.poll() is None:
            name = step.get('signal') or step['signal_only']
            proc.send_signal(getattr(signal, 'SIG' + name))
        if 'signal_only' in step:
            time.sleep(0.3)
            return {'running': proc.poll() is None}
        if step.get('quiet'):
            return {'status': finish(proc, ctx)['status']}
        return finish(proc, ctx)
    if 'files' in step:
        # The pid file may appear after the socket accepts connections.
        if not ctx.get('_pid'):
            with contextlib.suppress(OSError):
                ctx['_pid'] = (root / 'pid').read_text().strip()
        return files(root, step['files'], ctx)
    if 'sleep' in step:
        time.sleep(step['sleep'] / 1000)
        return None
    raise ValueError(f'unknown step {step}')


def all_cases() -> list[dict]:
    CASES.clear()
    _COUNTERS.clear()
    build_cases()
    for item in CASES:
        item['kind'] = 'protocol'
    protocol = list(CASES)
    CASES.clear()
    build_daemon_cases()
    for item in CASES:
        item['kind'] = 'daemon'
    combined = protocol + list(CASES)
    ids = [item['id'] for item in combined]
    assert len(ids) == len(set(ids)), 'duplicate case ids'
    return combined


def capture(program: str, cases: list[dict], jobs: int) -> dict[str, dict]:
    def one(item: dict) -> tuple[str, dict]:
        first = run_case(program, item)
        second = run_case(program, item)
        return item['id'], {'expect': first, 'stable': first == second}

    with ThreadPoolExecutor(max_workers=jobs) as pool:
        return dict(pool.map(one, cases))


def load_table(path: Path) -> dict:
    with open(path, encoding='utf-8') as handle:
        return json.load(handle)


def write_table(path: Path, table: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, 'w', encoding='utf-8') as handle:
        handle.write('{\n')
        for key in ('comment', 'oracle', 'normalization'):
            handle.write(f'{json.dumps(key)}: {json.dumps(table[key])},\n')
        handle.write('"cases": [\n')
        rows = [json.dumps(item, ensure_ascii=True) for item in table['cases']]
        handle.write(',\n'.join(rows))
        handle.write('\n]\n}\n')


def version(program: str) -> str:
    proc = subprocess.run([program, '-h'], capture_output=True, check=False)
    return (proc.stdout + proc.stderr).decode('latin-1').splitlines()[0]


def record(program: str, out: Path, jobs: int) -> int:
    cases = all_cases()
    old = {item['id']: item for item in load_table(out)['cases']} if out.exists() else {}
    results = capture(program, cases, jobs)
    rows = []
    unstable = 0
    for item in cases:
        result = results[item['id']]
        row = dict(item)
        if not result['stable']:
            row['tags'] = sorted(set(row['tags']) | {'unstable'})
            unstable += 1
        row['expect'] = result['expect']
        previous = old.get(item['id'], {})
        for key in ('expect_platform', 'rondi'):
            if key in previous:
                row[key] = previous[key]
        rows.append(row)
    table = {
        'comment': 'Generated by scripts/rrdcached-spec-vectors.py from rrdcached '
                   '1.11.0; see docs/compatibility/rrdcached.md.',
        'oracle': version(program),
        'normalization': '{ROOT} fixture root, {PORT} TCP port, {PID} daemon pid, '
                         '{T} journal name time; one char per byte (latin-1)',
        'cases': rows,
    }
    write_table(out, table)
    print(f'{len(rows)} cases, {unstable} unstable', file=sys.stderr)
    return 0


MMAP_ERROR = re.compile(r"(mmaping file '[^'\n]*': )[^\n]*")


def canonical(item: dict, results: list) -> list:
    """The form in which results are compared: the error text of mapping a
    directory depends on the file system (EINVAL, ENODEV), so it is masked,
    and the reply lines of a case that lists a directory are sorted."""
    out = [MMAP_ERROR.sub(r'\1{ERRNO}', r) if isinstance(r, str) else r
           for r in results]
    if 'unordered' not in item['tags']:
        return out
    return ['\n'.join(sorted(r.split('\n'))) if isinstance(r, str) else r
            for r in out]


def check(program: str, table_path: Path, only: set[str], jobs: int,
          platform: str | None, use_rondi: bool) -> int:
    table = load_table(table_path)
    cases = [c for c in table['cases'] if not only or c['id'] in only]
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        observed = list(pool.map(lambda c: run_case(program, c), cases))
    failures = 0
    for item, got in zip(cases, observed, strict=True):
        want = item['expect']
        if platform and platform in item.get('expect_platform', {}):
            want = item['expect_platform'][platform]
        frozen = not {'unstable', 'undefined'} & set(item['tags'])
        if 'rondi' in item and use_rondi:
            want = item['rondi'].get('expect', want)
            frozen = True
        want, got = canonical(item, want), canonical(item, got)
        if got != want and frozen:
            failures += 1
            print(f"== {item['id']}")
            for step, w, g in zip(item['steps'], want, got, strict=True):
                if w != g:
                    print(f'  step {json.dumps(step)[:200]}\n  want {json.dumps(w)[:1500]}'
                          f'\n  got  {json.dumps(g)[:1500]}')
    print(f'{failures} of {len(cases)} differ', file=sys.stderr)
    return 1 if failures else 0


def record_platform(program: str, key: str, out: Path, jobs: int) -> int:
    table = load_table(TABLE)
    results = capture(program, table['cases'], jobs)
    differing = {}
    for item in table['cases']:
        result = results[item['id']]
        if canonical(item, result['expect']) != canonical(item, item['expect']):
            differing[item['id']] = {'expect': result['expect'],
                                     'stable': result['stable']}
    with open(out, 'w', encoding='utf-8') as handle:
        json.dump({'key': key, 'oracle': version(program), 'cases': differing},
                  handle, indent=1)
    print(f'{len(differing)} cases differ on {key}', file=sys.stderr)
    return 0


def merge_platform(key: str, path: Path) -> int:
    table = load_table(TABLE)
    with open(path, encoding='utf-8') as handle:
        platform = json.load(handle)
    for item in table['cases']:
        item.get('expect_platform', {}).pop(key, None)
        found = platform['cases'].get(item['id'])
        if found:
            item.setdefault('expect_platform', {})[key] = found['expect']
            if not found['stable']:
                item['tags'] = sorted(set(item['tags']) | {'unstable'})
        if item.get('expect_platform') == {}:
            del item['expect_platform']
    write_table(TABLE, table)
    return 0


def record_rondi(program: str, jobs: int, platform: str | None) -> int:
    """Records Rondi's result for every case where it differs from the
    reference, classified by DIVERGENCE_RULES. With a platform key, only
    platform variants of existing entries are recorded."""
    table = load_table(TABLE)
    results = capture(program, table['cases'], jobs)
    problems = []
    for item in table['cases']:
        result = results[item['id']]
        if not result['stable']:
            problems.append(f"{item['id']}: unstable under Rondi")
        oracle = item.get('expect_platform', {}).get(platform, item['expect'])
        differs = (canonical(item, result['expect']) != canonical(item, oracle)
                   or 'undefined' in item['tags'])
        if platform:
            entry = item.get('rondi')
            if entry is None:
                if not differs or 'unstable' in item['tags']:
                    continue
                if platform not in PLATFORM_NOTES:
                    problems.append(f"{item['id']}: differs only on {platform}")
                    continue
                item['rondi'] = {'class': 'defect', 'note': PLATFORM_NOTES[platform],
                                 'expect': item['expect'],
                                 'expect_platform': {platform: result['expect']}}
                continue
            variants = entry.setdefault('expect_platform', {})
            variants.pop(platform, None)
            if canonical(item, result['expect']) != canonical(item, entry['expect']):
                variants[platform] = result['expect']
            if not variants:
                del entry['expect_platform']
            continue
        if not differs or ('unstable' in item['tags']
                           and 'undefined' not in item['tags']):
            entry = item.get('rondi', {})
            if entry.get('note') in PLATFORM_NOTES.values():
                # A divergence recorded on another platform only.
                entry['expect'] = item['expect']
            else:
                item.pop('rondi', None)
            continue
        found = classify(item)
        if found is None:
            problems.append(f"{item['id']}: unclassified divergence")
            continue
        entry = {'class': found[0], 'note': found[1], 'expect': result['expect']}
        if 'expect_platform' in item.get('rondi', {}):
            entry['expect_platform'] = item['rondi']['expect_platform']
        item['rondi'] = entry
    write_table(TABLE, table)
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems else 0


def make_fixtures(rrdtool: str) -> int:
    target = DATA / 'fixtures'
    target.mkdir(parents=True, exist_ok=True)
    for name, args in FIXTURES.items():
        path = target / name
        if path.exists():
            path.unlink()
        subprocess.run([rrdtool, 'create', str(path)] + args, check=True,
                       env={'TZ': 'UTC', 'PATH': os.environ.get('PATH', '')})
    return 0


def probe(program: str, options: str, requests: list[str]) -> int:
    steps = [daemon(options.split())] + [
        conn(r.encode('latin-1').decode('unicode_escape')) for r in requests] + [stop()]
    for result in run_case(program, {'steps': steps}):
        if isinstance(result, str):
            print(result, end='----\n')
        elif result is not None:
            print(json.dumps(result, indent=1))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--rrdcached', default='/opt/homebrew/bin/rrdcached')
    parser.add_argument('--rrdtool', default='/opt/homebrew/bin/rrdtool')
    parser.add_argument('--out', type=Path, default=TABLE)
    parser.add_argument('--jobs', type=int, default=8)
    parser.add_argument('--make-fixtures', action='store_true')
    parser.add_argument('--check', metavar='PATH')
    parser.add_argument('--only', default='')
    parser.add_argument('--platform', help='expect_platform key used by --check')
    parser.add_argument('--rondi', action='store_true',
                        help='with --check, expect rondi.expect where present')
    parser.add_argument('--probe', action='store_true',
                        help='send each REQUEST on its own connection')
    parser.add_argument('--options', default='', help='daemon options for --probe')
    parser.add_argument('requests', nargs='*', metavar='REQUEST')
    parser.add_argument('--record-platform', metavar='KEY')
    parser.add_argument('--platform-out', type=Path)
    parser.add_argument('--merge-platform', nargs=2, metavar=('KEY', 'FILE'))
    parser.add_argument('--record-rondi', metavar='PATH')
    parser.add_argument('--list', action='store_true', help='print case ids')
    args = parser.parse_args()
    # Daemons start in the fixture root, so a relative program path would
    # not resolve there.
    for name in ('rrdcached', 'check', 'record_rondi'):
        value = getattr(args, name)
        if value and os.sep in value:
            setattr(args, name, os.path.abspath(value))
    if args.make_fixtures:
        return make_fixtures(args.rrdtool)
    if args.list:
        for item in all_cases():
            first = next((s['conn'] for s in item['steps'] if 'conn' in s), '')
            print(item['id'], ','.join(item['rules']), json.dumps(first)[:100])
        return 0
    if args.probe:
        return probe(args.rrdcached, args.options, args.requests)
    if args.check:
        only = {x for x in args.only.split(',') if x}
        return check(args.check, args.out, only, args.jobs, args.platform,
                     args.rondi)
    if args.record_platform:
        if not args.platform_out:
            parser.error('--record-platform needs --platform-out')
        return record_platform(args.rrdcached, args.record_platform,
                               args.platform_out, args.jobs)
    if args.merge_platform:
        return merge_platform(args.merge_platform[0], Path(args.merge_platform[1]))
    if args.record_rondi:
        return record_rondi(args.record_rondi, args.jobs, args.platform)
    return record(args.rrdcached, args.out, args.jobs)


if __name__ == '__main__':
    sys.exit(main())
