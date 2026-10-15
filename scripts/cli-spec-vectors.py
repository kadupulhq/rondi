#!/usr/bin/env python3
"""Capture command-line front end vectors from a reference rrdtool.

Usage:
  scripts/cli-spec-vectors.py [--rrdtool PATH] [--out FILE]
  scripts/cli-spec-vectors.py --only GROUP          # print one group
  scripts/cli-spec-vectors.py --check PATH          # diff PATH vs FILE
  scripts/cli-spec-vectors.py --rrdtool PATH --record-platform KEY \
      --platform-out F
  scripts/cli-spec-vectors.py --merge-platform KEY F

The cases are listed here; the output file is the frozen table that
crates/rondi-cli/tests/cli_spec_vectors.rs replays and that
docs/compatibility/cli.md, pipe-mode.md and format-strings.md describe.

Every vector runs in a fresh copy of a fixture directory. The fixture is
built by running `restore` on XML files that are themselves captured from
the reference at generation time, so both programs start from identical
row positions. A vector is a list of steps; each step names the program
(`rrdtool` unless an alias such as `rrdupdate` is given), its arguments and
optional standard input. The environment is TZ=UTC, LC_ALL=C and no
RRDCACHED_ADDRESS unless the vector sets `env`. Each case runs twice; one
whose two runs differ is tagged `unstable` and is not compared frozen.

Output is normalized before it is stored or compared (see `normalize`):
the build stamp after `Compiled ` is dropped, the pipe-mode timing line
`OK u:<f> s:<f> r:<f>` becomes `OK u:* s:* r:*`, the work directory path
becomes `{CWD}`, the program directory becomes `{BIN}`, archive `cur_row`
values become `*`, and runs of pipe-mode `ls` lines are sorted. Vectors
tagged `dir-order` list a directory with several entries of one kind; their
standard output is compared as a sorted set of lines.

The table is captured on macOS arm64 (/opt/homebrew/bin/rrdtool 1.11.0).
Results that differ on Linux are recorded per vector under
expect_platform.<KEY> (KEY linux-x86_64 or linux-aarch64) with
--record-platform in a container that has the pinned oracle, then
--merge-platform. Regenerate only with RRDtool 1.11.0 and review the diff.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

TIMING = re.compile(r'^OK u:\d+\.\d\d s:\d+\.\d\d r:\d+\.\d\d$')
LS_LINE = re.compile(r'^[d-] ')
# restore and create pick the current row of each archive at random.
CUR_ROW = re.compile(r'^(rra\[\d+\]\.cur_row = )\d+$')
ALIASES = ('rrdupdate', 'rrdcreate', 'rrdinfo')

# ------------------------------------------------------------ fixture

# Commands run with the reference to produce the XML the fixture restores.
SEED = {
    'a.xml': [
        ['create', 'a.rrd', '--start', '1700000000', '--step', '300',
         'DS:x:GAUGE:600:U:U', 'DS:y:COUNTER:600:0:1000',
         'RRA:AVERAGE:0.5:1:10', 'RRA:MAX:0.5:2:5'],
        ['update', 'a.rrd', '1700000300:1:100', '1700000600:2:400',
         '1700000900:3:1000', '1700001200:U:1600', '1700001500:5.5:2500'],
    ],
    'f.xml': [
        ['create', 'f.rrd', '--start', '1000000000', '--step', '60',
         'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:20',
         'RRA:MAX:0.5:1:20'],
        ['update', 'f.rrd', '1000000060:1', '1000000120:2',
         '1000000180:3', '1000000240:4'],
    ],
    'e.xml': [
        ['create', 'e.rrd', '--start', '1000000000', '--step', '60',
         'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:3'],
    ],
}

FIXTURE = [
    ['restore', 'a.xml', 'a.rrd'],
    ['restore', 'f.xml', 'f.rrd'],
    ['restore', 'e.xml', 'e.rrd'],
    ['restore', 'a.xml', 'sub/b.rrd'],
    ['restore', 'e.xml', 'sub/deep/c.rrd'],
]
DIRS = ['sub', 'sub/deep', 'empty']
TEXT_FILES = {'notes.txt': 'not an rrd\n', 'sub/readme': 'text\n'}

A = 'a.rrd'
T0 = '1000000000'
T1 = '1000000300'
GRAPH = ['graph', '/dev/null', '--start', T0, '--end', T1,
         'DEF:x=f.rrd:x:AVERAGE']


def build_seed(binary: str) -> dict[str, str]:
    files: dict[str, str] = {}
    with tempfile.TemporaryDirectory() as tmp:
        env = dict(os.environ, TZ='UTC', LC_ALL='C')
        env.pop('RRDCACHED_ADDRESS', None)
        for name, commands in SEED.items():
            for command in commands:
                subprocess.run([binary] + command, cwd=tmp, env=env,
                               check=True)
            rrd = commands[0][1]
            files[name] = subprocess.run(
                [binary, 'dump', rrd], cwd=tmp, env=env, check=True,
                capture_output=True, text=True).stdout
    files.update(TEXT_FILES)
    return files


def setup(binary: str, fixture: Path, files: dict[str, str]) -> None:
    for name in DIRS:
        (fixture / name).mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        with open(fixture / name, 'w', encoding='utf-8') as handle:
            handle.write(text)
    env = dict(os.environ, TZ='UTC', LC_ALL='C')
    env.pop('RRDCACHED_ADDRESS', None)
    for command in FIXTURE:
        subprocess.run([binary] + command, cwd=fixture, env=env, check=True)


# --------------------------------------------------------------- cases

CASES: list[dict] = []


def case(area: str, group: str, *steps: list[str] | dict,
         env: dict[str, str] | None = None,
         tags: list[str] | None = None) -> None:
    norm = []
    for step in steps:
        if isinstance(step, dict):
            norm.append(step)
        else:
            norm.append({'args': list(step)})
    entry = {'area': area, 'group': group, 'steps': norm,
             'tags': list(tags or [])}
    if env:
        entry['env'] = env
    CASES.append(entry)


def cli(group: str, *args: str, env: dict[str, str] | None = None,
        tags: list[str] | None = None) -> None:
    case('cli', group, list(args), env=env, tags=tags)


def alias(group: str, name: str, *args: str) -> None:
    case('cli', group, {'argv0': name, 'args': list(args)})


def pipe(group: str, text: str, *args: str,
         env: dict[str, str] | None = None,
         tags: list[str] | None = None) -> None:
    case('pipe', group, {'args': ['-'] + list(args), 'stdin': text},
         env=env, tags=tags)


def fmt(group: str, *args: str, env: dict[str, str] | None = None,
        tags: list[str] | None = None) -> None:
    case('format', group, list(args), env=env, tags=tags)


COMMANDS = ['create', 'update', 'updatev', 'graph', 'graphv', 'dump',
            'restore', 'last', 'lastupdate', 'first', 'info', 'list',
            'fetch', 'tune', 'resize', 'xport', 'flushcached']
PIPE_ONLY = ['cd', 'mkdir', 'pwd', 'ls', 'quit']


def dispatch_cases() -> None:
    cli('dispatch')
    for word in ['help', '--help', '-h', '-?', 'version', '--version', '-v',
                 '-V', 'foo', 'Help', 'HELP', '--', '-', '-x', '--foo', '',
                 '#', '1', 'cd', 'pwd', 'ls', 'quit', 'mkdir', 'helpx',
                 'versions', '--h', '--vers', 'Create', 'CREATE', 'cre',
                 'creates', 'lastupdat']:
        cli('dispatch', word)
    for pair in [('help', 'last'), ('help', 'foo'), ('help', 'help'),
                 ('--help', 'fetch'), ('-h', 'info'), ('help', 'LAST'),
                 ('help', 'las'), ('help', 'last', 'x'), ('-h', 'x', 'y'),
                 ('version', 'x'), ('--version', 'x'), ('-v', 'x'),
                 ('-v', 'last', 'x'), ('foo', 'bar'), ('Last', A),
                 ('LAST', A), ('las', A), ('lastx', A), ('-', 'x', 'y'),
                 ('-V', 'x'), ('-?', 'x'), ('HELP', 'last'), ('help', '-'),
                 ('help', 'version'), ('--help', '--help'),
                 ('--', 'last', A), ('-x', 'last', A), ('--foo', 'last'),
                 ('cd', '/'), ('pwd', 'x'), ('ls', 'x'), ('quit', 'x'),
                 ('mkdir', 'newdir'), ('help', 'cd'), ('help', 'quit'),
                 ('help', 'graphv'), ('version', 'last', A),
                 ('', 'last'), ('last', A, '--', 'x')]:
        cli('dispatch', *pair)
    for name in COMMANDS + PIPE_ONLY:
        cli('help', 'help', name)
    for name in COMMANDS:
        cli('noargs', name)


# Options of each command, as (long, short, takes_argument, sample value).
OPTIONS = {
    'fetch': [('resolution', 'r', True, '60'), ('start', 's', True, T0),
              ('end', 'e', True, T1), ('align-start', 'a', False, None),
              ('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'first': [('rraindex', None, True, '1'),
              ('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'last': [('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'lastupdate': [('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'info': [('daemon', 'd', True, 'unix:/nonexistent/sock'),
             ('noflush', 'F', False, None)],
    'dump': [('header', 'h', True, 'none'), ('no-header', 'n', False, None),
             ('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'list': [('daemon', 'd', True, 'unix:/nonexistent/sock'),
             ('noflush', 'F', False, None), ('recursive', 'r', False, None)],
    'flushcached': [('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'xport': [('start', 's', True, T0), ('end', 'e', True, T1),
              ('maxrows', 'm', True, '10'), ('step', None, True, '60'),
              ('showtime', 't', False, None), ('enumds', None, False, None),
              ('json', None, False, None),
              ('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'restore': [('range-check', 'r', False, None),
                ('force-overwrite', 'f', False, None)],
    'create': [('start', 'b', True, T0), ('step', 's', True, '60'),
               ('template', 't', True, 'a.rrd'),
               ('source', 'r', True, 'a.rrd'),
               ('no-overwrite', 'O', False, None),
               ('daemon', 'd', True, 'unix:/nonexistent/sock')],
    'update': [('template', 't', True, 'x'),
               ('skip-past-updates', 's', False, None),
               ('locking', 'L', True, 'none'),
               ('daemon', 'd', True, 'unix:/nonexistent/sock')],
}

# A base invocation per command where an option can be added anywhere.
BASE = {
    'fetch': ['fetch', 'f.rrd', 'AVERAGE', '-s', T0, '-e', T1],
    'first': ['first', A],
    'last': ['last', A],
    'lastupdate': ['lastupdate', A],
    'info': ['info', 'e.rrd'],
    'dump': ['dump', 'e.rrd'],
    'list': ['list', 'sub'],
    'flushcached': ['flushcached', A],
    'xport': ['xport', '-s', T0, '-e', T1, 'DEF:x=f.rrd:x:AVERAGE',
              'XPORT:x'],
    'restore': ['restore', 'a.xml', 'new.rrd'],
    'create': ['create', 'new.rrd', '--start', T0, 'DS:x:GAUGE:120:U:U',
               'RRA:AVERAGE:0.5:1:5'],
    'update': ['update', 'f.rrd', '1000000300:5'],
}


def option_cases() -> None:
    for command, options in OPTIONS.items():
        base = BASE[command]
        cli('optparse', *base, '--bogus')
        cli('optparse', *base, '-Q')
        cli('optparse', *base, '--')
        cli('optparse', *base, '---x')
        cli('optparse', *base, '--=x')
        for long, short, takes, value in options:
            # missing argument at the end
            if takes:
                cli('optparse', *base, f'--{long}')
                if short:
                    cli('optparse', *base, f'-{short}')
            else:
                cli('optparse', *base, f'--{long}=1')
                cli('optparse', *base, f'--{long}=')
            # no abbreviation, no case folding
            if len(long) > 2:
                cli('optparse', *base, f'--{long[:-1]}', *(
                    [value] if takes else []))
            cli('optparse', *base, f'--{long.upper()}', *(
                [value] if takes else []))
            if takes and long != 'daemon':
                cli('optparse', *base, f'--{long}={value}')
                if short:
                    cli('optparse', *base, f'-{short}{value}')
    # option error messages are cut to a fixed length
    for name in ['x' * 26, 'x' * 27, 'x' * 28, 'x' * 29, 'x' * 60]:
        cli('truncate', 'last', A, f'--{name}')
    for long in ['smoothing-window-deviation', 'data-source-rename',
                 'failure-threshold', 'aberrant-reset', 'gamma-deviation',
                 'data-source-type']:
        cli('truncate', 'tune', A, f'--{long}')
    for long in ['skip-past-updates', 'skip-past-update']:
        cli('truncate', 'update', 'f.rrd', f'--{long}=1', '1000000300:5')
    cli('truncate', 'last', 'x' * 5000)
    cli('truncate', 'last', 'x' * 4070)
    cli('truncate', 'last', 'x' * 4080)
    cli('truncate', 'last', 'é' * 3000)
    # positional counts and `--`
    for command in ['first', 'last', 'lastupdate', 'info', 'flushcached']:
        cli('positional', command, A, A)
        cli('positional', command, '--', A)
        cli('positional', command, A, '--')
        cli('positional', command, '--', '--', A)
        cli('positional', command, '-', A)
        cli('positional', command, 'missing.rrd')
        cli('positional', command, 'notes.txt')
        cli('positional', command, 'sub')
        cli('positional', command, '')
    for args in [['fetch', 'f.rrd'], ['fetch', 'f.rrd', 'AVERAGE', 'x', '-s',
                                       T0, '-e', T1],
                 ['fetch', '-s', T0, 'f.rrd', '-e', T1, 'AVERAGE'],
                 ['fetch', '-s', T0, '-e', T1, '--', 'f.rrd', 'AVERAGE'],
                 ['fetch', 'f.rrd', 'AVERAGE', '-s', T0, '-e', T1, '--',
                  '-a'],
                 ['fetch', 'f.rrd', 'average', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'BOGUS', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MIN', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-as', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-sa', '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-ase', T0, T1],
                 ['fetch', 'f.rrd', 'MAX', '-a', '-a', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-s', '999999000',
                  '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-s', '-e', '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-s=' + T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '--start', '', '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '0'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '-60'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1m'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '120'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '60x'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', 'abc'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1h'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '2m'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1d'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1w'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1y'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '60s'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1.5m'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', ' 60'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '+60'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r', '1M'],
                 ['fetch', 'f.rrd', 'MAX', '-s', T0, '-e', T1, '-r',
                  '99999999999999999999'],
                 ['first', A, '--rraindex', '0'],
                 ['first', A, '--rraindex', '1'],
                 ['first', A, '--rraindex', '2'],
                 ['first', A, '--rraindex', '-1'],
                 ['first', A, '--rraindex', 'abc'],
                 ['first', A, '--rraindex', '1x'],
                 ['first', A, '--rraindex', ' 1'],
                 ['first', A, '--rraindex', '+1'],
                 ['first', A, '--rraindex', '0x1'],
                 ['first', A, '--rraindex', '1.9'],
                 ['first', A, '--rraindex', ''],
                 ['first', A, '--rraindex', '4294967296'],
                 ['first', A, '--rraindex', '99999999999999999999'],
                 ['first', '--rraindex', '1', A],
                 ['first', A, '--rraindex=1'],
                 ['first', A, '-r', '1'],
                 ['dump', '--header', 'xsd', 'e.rrd'],
                 ['dump', '--header', 'dtd', 'e.rrd'],
                 ['dump', '--header', 'none', 'e.rrd'],
                 ['dump', '--header', 'bogus', 'e.rrd'],
                 ['dump', '-h', 'none', 'e.rrd'],
                 ['dump', '-hnone', 'e.rrd'],
                 ['dump', '-n', 'e.rrd'],
                 ['dump', '--no-header', 'e.rrd'],
                 ['dump', 'e.rrd', 'out.xml'],
                 ['dump', 'e.rrd', 'out.xml', 'x'],
                 ['dump', 'e.rrd', '-'],
                 ['dump'],
                 ['dump', 'missing.rrd'],
                 ['dump', 'notes.txt'],
                 ['list', '.'],
                 ['list', '-r', '.'],
                 ['list', '--recursive', '.'],
                 ['list', 'sub'],
                 ['list', '-r', 'sub'],
                 ['list', 'empty'],
                 ['list', 'missing'],
                 ['list', A],
                 ['list', 'notes.txt'],
                 ['list', '.', 'sub'],
                 ['list', ''],
                 ['list', 'sub/'],
                 ['list', './sub'],
                 ['list', '-rF', 'sub'],
                 ['list', '-F', 'sub'],
                 ['resize'],
                 ['resize', A],
                 ['resize', A, '0'],
                 ['resize', A, '0', 'GROW'],
                 ['resize', A, '0', 'GROW', '1', 'x'],
                 ['resize', A, '0', 'grow', '1'],
                 ['resize', A, '0', 'BOGUS', '1'],
                 ['resize', A, 'x', 'GROW', '1'],
                 ['resize', A, '0', 'GROW', 'x'],
                 ['resize', A, '0', 'GROW', '-1'],
                 ['resize', A, '9', 'GROW', '1'],
                 ['resize', A, '0', 'SHRINK', '100'],
                 ['resize', '--', A, '0', 'GROW', '1'],
                 ['resize', A, '0', 'GROW', '1', '--bogus'],
                 ['restore', 'a.xml'],
                 ['restore', 'a.xml', A],
                 ['restore', '-f', 'a.xml', 'new.rrd', 'x'],
                 ['restore', 'missing.xml', 'new.rrd'],
                 ['restore', '-rf', 'a.xml', 'new.rrd'],
                 ['tune', A],
                 ['tune', A, '--bogus'],
                 ['tune', A, '--heartbeat'],
                 ['tune', A, '-h'],
                 ['tune', A, '--heartbeat', 'x:900'],
                 ['tune', A, '--heartbeat=x:900'],
                 ['tune', A, '-hx:900'],
                 ['tune', A, '--heartbea', 'x:900'],
                 ['tune', A, '--step'],
                 ['tune', A, '-t'],
                 ['tune', A, '--daemon'],
                 ['tune', A, '-D'],
                 ['tune', 'missing.rrd', '--heartbeat', 'x:900'],
                 ['tune', '--heartbeat', 'x:900', A],
                 ['tune', '--heartbeat', 'x:900'],
                 ['update', 'f.rrd'],
                 ['update'],
                 ['update', 'f.rrd', '1000000300:5', '--bogus'],
                 ['update', '-t', 'x', 'f.rrd', '1000000300:5'],
                 ['update', 'f.rrd', '-t', 'x', '1000000300:5'],
                 ['update', 'f.rrd', '--template=x', '1000000300:5'],
                 ['update', 'f.rrd', '-L', 'bogus', '1000000300:5'],
                 ['update', 'f.rrd', '--locking', 'block', '1000000300:5'],
                 ['update', 'f.rrd', '--locking', 'try', '1000000300:5'],
                 ['update', 'f.rrd', '--', '1000000300:5'],
                 ['update', 'f.rrd', '--', '-5:5'],
                 ['updatev', 'f.rrd', '1000000300:5'],
                 ['updatev', 'f.rrd', '-s', '1000000300:5'],
                 ['updatev', 'f.rrd'],
                 ['updatev', 'f.rrd', '--bogus', '1000000300:5'],
                 ['create', 'new.rrd'],
                 ['create', 'new.rrd', '--step', '1m', '--start', T0,
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--step', 'abc', '--start', T0,
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--step', '0', '--start', T0,
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--step', '-60', '--start', T0,
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--step', '60x', '--start', T0,
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', '-O', A, 'DS:x:GAUGE:120:U:U',
                  'RRA:AVERAGE:0.5:1:5'],
                 ['create', A, '-O', '--start', T0, 'DS:x:GAUGE:120:U:U',
                  'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--start', T0, '--',
                  'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['create', 'new.rrd', '--start', T0, 'DS:x:GAUGE:120:U:U',
                  'RRA:AVERAGE:0.5:1:5', '--bogus'],
                 ['xport', '-s', T0, '-e', T1, 'DEF:x=f.rrd:x:AVERAGE'],
                 ['xport', '-s', T0, '-e', T1],
                 ['xport', '-s', T0, '-e', T1, '-m', 'abc',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '-m', '0',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '-m', '3',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '--step', '120',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '--step', '2m',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '--step', 'x',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '-t', '--json',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x:legend'],
                 ['xport', '-s', T0, '-e', T1, '--enumds',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['xport', '-s', T0, '-e', T1, '-tm', '3',
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x'],
                 ['flushcached', A],
                 ['flushcached'],
                 ['graph', '/dev/null', '--bogus'],
                 ['graph', '/dev/null', '-Q'],
                 ['graph', '/dev/null', '--start'],
                 ['graph', '/dev/null', '--st', T0],
                 ['graph', '/dev/null', '--sta', T0],
                 ['graph', '/dev/null', '--width'],
                 ['graph', '/dev/null', '-w'],
                 ['graph', '/dev/null', '--only-graph=1'],
                 ['graph', '/dev/null', '--only-graph=', '-s', T0, '-e',
                  T1],
                 ['graph', '/dev/null'],
                 ['graph'],
                 ['graph', '/dev/null', '-s', T0, '-e', T1, '--', '-j'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', '100',
                  '-h', '50', '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-jgs', T0, '-e', T1, '-w100',
                  '-h50', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', '1e2',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', '100x',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', 'abc',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', '-5',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w', '0',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-w',
                  '99999999999', '-j', 'DEF:x=f.rrd:x:AVERAGE',
                  'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-h', '10.9',
                  '-j', 'DEF:x=f.rrd:x:AVERAGE', 'LINE1:x#f00'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '-S', '120',
                  'DEF:x=f.rrd:x:AVERAGE', 'VDEF:v=x,AVERAGE',
                  'PRINT:v:%lf'],
                 ['graphv', '/dev/null', '-s', T0, '-e', T1, '--step=2m',
                  'DEF:x=f.rrd:x:AVERAGE', 'VDEF:v=x,AVERAGE',
                  'PRINT:v:%lf'],
                 ]:
        cli('commands', *args)


def output_cases() -> None:
    for args in [['info', A], ['info', 'e.rrd'], ['info', 'f.rrd'],
                 ['info', '-F', 'e.rrd'], ['info', 'sub/b.rrd'],
                 ['last', A], ['last', 'e.rrd'], ['last', 'f.rrd'],
                 ['first', A], ['first', 'e.rrd'], ['first', 'f.rrd'],
                 ['lastupdate', A], ['lastupdate', 'e.rrd'],
                 ['lastupdate', 'f.rrd'],
                 ['fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
                  '1700001800'],
                 ['fetch', A, 'MAX', '-s', '1700000000', '-e', '1700003000'],
                 ['fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
                  '1700001800', '-r', '600'],
                 ['fetch', A, 'AVERAGE', '-s', '1700000100', '-e',
                  '1700001800', '-a'],
                 ['fetch', A, 'AVERAGE', '-s', '1700001800', '-e',
                  '1700000000'],
                 ['fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
                  '1700000000'],
                 ['fetch', 'e.rrd', 'AVERAGE', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'AVERAGE', '-s', T0, '-e', T1],
                 ['fetch', 'f.rrd', 'MAX', '-s', '999990000', '-e', T1],
                 ['dump', 'e.rrd'], ['dump', '-n', 'f.rrd'],
                 ['xport', '-s', T0, '-e', T1, 'DEF:x=f.rrd:x:AVERAGE',
                  'XPORT:x:value'],
                 ['xport', '--json', '-s', T0, '-e', T1,
                  'DEF:x=f.rrd:x:AVERAGE', 'XPORT:x:value'],
                 ['updatev', 'f.rrd', '1000000300:5', '1000000360:6'],
                 ['update', 'f.rrd', '1000000240:5'],
                 ['update', 'f.rrd', '-s', '1000000240:5', '1000000300:6'],
                 ]:
        cli('output', *args)
    # A file written by one step and inspected by the next.
    case('cli', 'output',
         ['create', 'n.rrd', '--start', T0, '--step', '60',
          'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
         ['last', 'n.rrd'], ['first', 'n.rrd'], ['lastupdate', 'n.rrd'])
    case('cli', 'output', ['update', 'f.rrd', '1000000300:5'],
         ['last', 'f.rrd'], ['lastupdate', 'f.rrd'])
    case('cli', 'output', ['dump', 'e.rrd', 'out.xml'],
         ['restore', 'out.xml', 'r.rrd'], ['last', 'r.rrd'])
    case('cli', 'output', ['resize', A, '0', 'GROW', '2'],
         ['info', 'resize.rrd'])


def alias_cases() -> None:
    for name in ALIASES:
        alias('alias', name)
        alias('alias', name, '--help')
        alias('alias', name, '--bogus')
        alias('alias', name, '--version')
    for args in [[A, '1700001800:7:3000'], [A], [A, 'x'],
                 ['-t', 'y', A, '1700001800:3000'],
                 [A, '--template', 'y', '1700001800:3000'],
                 ['missing.rrd', '1:1'], ['--', A, '1700001800:7:3000'],
                 ['-s', A, '1700000300:1:1'], [A, '-Q', '1:1']]:
        alias('alias', 'rrdupdate', *args)
    for args in [['n.rrd', 'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:5'],
                 ['n.rrd'], ['n.rrd', '-Z'], ['-O', A, 'DS:x:GAUGE:120:U:U',
                                               'RRA:AVERAGE:0.5:1:5'],
                 ['n.rrd', '--step', 'x', 'DS:x:GAUGE:120:U:U',
                  'RRA:AVERAGE:0.5:1:5']]:
        alias('alias', 'rrdcreate', *args)
    for args in [[A], ['e.rrd'], ['missing.rrd'], [A, 'e.rrd'],
                 ['-F', 'e.rrd'], ['--daemon', 'unix:/nonexistent/sock',
                                   'e.rrd']]:
        alias('alias', 'rrdinfo', *args)


def env_cases() -> None:
    bad = 'unix:/nonexistent/sock'
    for args in [['last', A], ['first', A], ['lastupdate', A], ['info', A],
                 ['fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
                  '1700001800'],
                 ['update', A, '1700001800:1:1'], ['dump', 'e.rrd'],
                 ['flushcached', A], ['list', 'sub'],
                 ['xport', '-s', T0, '-e', T1, 'DEF:x=f.rrd:x:AVERAGE',
                  'XPORT:x'],
                 GRAPH + ['VDEF:v=x,AVERAGE', 'PRINT:v:%lf'],
                 ['create', 'n.rrd', '--start', T0, 'DS:x:GAUGE:120:U:U',
                  'RRA:AVERAGE:0.5:1:5'],
                 ['tune', A, '--heartbeat', 'x:900'],
                 ['resize', A, '0', 'GROW', '1'],
                 ['restore', 'a.xml', 'n.rrd']]:
        cli('env', *args, env={'RRDCACHED_ADDRESS': bad})
    for value in ['', 'unix:', '/nonexistent/sock', 'localhost:1',
                  'bogus host name']:
        cli('env', 'last', A, env={'RRDCACHED_ADDRESS': value})
    cli('env', 'last', A, '--daemon', 'unix:/other/sock',
        env={'RRDCACHED_ADDRESS': bad})
    cli('env', 'last', A, '--daemon', '', env={'RRDCACHED_ADDRESS': bad})
    cli('env', 'info', '--daemon', bad, '-F', A)
    cli('env', 'info', '-d', bad, A)
    cli('env', 'info', f'-d{bad}', A)
    for value in ['none', 'try', 'block', 'bogus', '']:
        cli('env', 'update', A, '1700001800:1:1', env={'RRD_LOCKING': value})
        cli('env', 'last', A, env={'RRD_LOCKING': value})
    for zone in ['UTC', 'America/New_York', 'Asia/Kolkata', '', 'Not/AZone',
                 'EST5EDT', '<+0530>-5:30']:
        fmt('env', *GRAPH, 'VDEF:v=x,MAXIMUM',
            'PRINT:v:%Y-%m-%d %H\\:%M\\:%S %Z:strftime', env={'TZ': zone})
        cli('env', 'fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
            '1700001200', env={'TZ': zone})
        cli('env', 'info', 'e.rrd', env={'TZ': zone})
    for name in ['LC_ALL', 'LC_NUMERIC', 'LANG', 'LC_TIME']:
        env = {'LC_ALL': '', name: 'de_DE.UTF-8'} if name != 'LC_ALL' \
            else {'LC_ALL': 'de_DE.UTF-8'}
        fmt('locale', *GRAPH, 'VDEF:v=x,AVERAGE', 'PRINT:v:%lf',
            'PRINT:v:%le', 'PRINT:v:%lg %s', env=env, tags=['locale'])
        fmt('locale', *GRAPH, 'VDEF:v=x,MAXIMUM',
            'PRINT:v:%A %B %c:strftime', env=env, tags=['locale'])
        cli('locale', 'fetch', A, 'AVERAGE', '-s', '1700000000', '-e',
            '1700001200', env=env, tags=['locale'])
        cli('locale', 'info', A, env=env, tags=['locale'])
        cli('locale', 'lastupdate', A, env=env, tags=['locale'])
        cli('locale', 'xport', '-s', T0, '-e', T1, 'DEF:x=f.rrd:x:AVERAGE',
            'CDEF:y=x,3,/', 'XPORT:y', env=env, tags=['locale'])
    cli('locale', 'dump', 'e.rrd', env={'LC_ALL': 'de_DE.UTF-8'},
        tags=['locale'])


# ------------------------------------------------------------ pipe mode

def pipe_cases() -> None:
    simple = [
        '', '\n', 'last a.rrd\n', 'last a.rrd', 'last a.rrd\n\n',
        '   \n', '\t\n', '\r\n', 'last a.rrd\r\n', 'last\ta.rrd\n',
        '  last   a.rrd  \n', 'last "a.rrd"\n', "last 'a.rrd'\n",
        'last a\\.rrd\n', 'last "a .rrd"\n', 'last a#.rrd\n',
        "last 'a'.rrd\n", 'last "a.r"\'rd\'\n', "last 'a\".rrd'\n",
        'last "a.rrd\n', "last 'a.rrd\n", 'last a.rrd"\n',
        'last "a\\"b.rrd"\n', "last 'it'\\''s.rrd'\nlast a.rrd\n",
        'last ""\n', "last ''\n", 'last "" a.rrd\n', '""\n', '"last" a.rrd\n',
        "'last a.rrd'\n", '# comment\n', '#\n', '# last a.rrd\n',
        'foo\n', 'foo bar\n', 'FOO\n', 'Last a.rrd\n', 'last\n',
        'last a.rrd extra\n', 'last missing.rrd\n', 'help\n', 'help last\n',
        'help cd\n', 'help foo\n', '--help\n', '-h\n', 'version\n',
        '--version x\n', 'version x\n', 'info\n', 'quit\n', 'quit\nlast a.rrd\n',
        'quit x\nlast a.rrd\n', 'QUIT\n', ' quit\nlast a.rrd\n',
        'quit \nlast a.rrd\n', '"quit"\nlast a.rrd\n', 'exit\n',
        'pwd\n', 'pwd x\n', 'pwd x y\n', 'cd sub\npwd\n', 'cd\npwd\n',
        'cd sub deep\npwd\n', 'cd missing\npwd\n', 'cd notes.txt\npwd\n',
        'cd sub\ncd deep\npwd\ncd ..\npwd\n', 'cd sub\nlast b.rrd\n',
        'cd /\npwd\n', 'cd ""\npwd\n', 'mkdir n\ncd n\npwd\n',
        'mkdir\n', 'mkdir n m\n', 'mkdir sub\n', 'mkdir missing/x\n',
        'mkdir n\nmkdir n\n', 'mkdir notes.txt\n', 'mkdir ""\n',
        'mkdir sub/n\ncd sub/n\npwd\n',
        'ls\n', 'ls x\n', 'cd sub\nls\n', 'cd empty\nls\n',
        'cd sub/deep\nls\n', 'LS\n', 'Ls\n',
        'last a.rrd\nlast missing.rrd\nlast a.rrd\n',
        'first a.rrd\nlastupdate a.rrd\ninfo e.rrd\n',
        'fetch a.rrd AVERAGE -s 1700000000 -e 1700001800\n',
        'fetch a.rrd AVERAGE --bogus\nlast a.rrd\n',
        'fetch a.rrd AVERAGE -s\nlast a.rrd\n',
        'update f.rrd 1000000300:5\nlast f.rrd\nlastupdate f.rrd\n',
        'updatev f.rrd 1000000300:5\n',
        'update f.rrd 1000000000:5\nlast f.rrd\n',
        'create n.rrd --start 1000000000 DS:x:GAUGE:120:U:U '
        'RRA:AVERAGE:0.5:1:5\nlast n.rrd\n',
        'create n.rrd\n', 'graph /dev/null -s 1000000000 -e 1000000300 '
        'DEF:x=f.rrd:x:AVERAGE VDEF:v=x,AVERAGE PRINT:v:%lf\n',
        'graphv /dev/null -s 1000000000 -e 1000000300 '
        'DEF:x=f.rrd:x:AVERAGE VDEF:v=x,AVERAGE PRINT:v:%lf\n',
        'graph /dev/null -s 1000000000 -e 1000000300 '
        'DEF:x=f.rrd:x:AVERAGE VDEF:v=x,AVERAGE "PRINT:v:avg %lf"\n',
        'graph /dev/null -s 1000000000 -e 1000000300 '
        'DEF:x=f.rrd:x:AVERAGE VDEF:v=x,AVERAGE PRINT:v:%d\n',
        'xport -s 1000000000 -e 1000000300 DEF:x=f.rrd:x:AVERAGE XPORT:x\n',
        'xport\n', 'dump e.rrd\n', 'dump e.rrd out.xml\nrestore out.xml '
        'r.rrd\nlast r.rrd\n', 'restore a.xml\n', 'tune a.rrd -h x:900\n'
        'info a.rrd\n', 'tune a.rrd --bogus\n', 'resize a.rrd 0 GROW 1\n',
        'resize\n', 'flushcached a.rrd\n', 'list sub\n', 'list -r .\n',
        'list missing\n', 'first a.rrd --rraindex 9\n',
        'last a.rrd\n' * 3, 'last\xff.rrd\nlast a.rrd\n',
        'last \xe9.rrd\nlast a.rrd\n', 'last a.rrd\x00x\nlast a.rrd\n',
        '\x00\nlast a.rrd\n', 'last ' + 'x' * 300 + '\nlast a.rrd\n',
        'last ' + 'x' * 5000 + '\nlast a.rrd\n',
        'last ' + ' '.join(['a.rrd'] * 200) + '\nlast a.rrd\n',
        'last a.rrd ' + ' ' * 3000 + '\nlast a.rrd\n',
        'update f.rrd ' + ' '.join(f'{1000000300 + 60 * k}:{k}'
                                   for k in range(500)) + '\nlast f.rrd\n',
        '-\n', '- x\n', '-v\n', 'cd -\n', 'help\nquit\n',
        'info "e.rrd"\n', 'info e.rrd --daemon\nlast a.rrd\n',
        'info e.rrd -d unix:/nonexistent/sock\nlast a.rrd\n',
        'last a.rrd\n\x04last a.rrd\n',
        '\x04\x04last a.rrd\n', '\tlast a.rrd\n', 'last a.rrd\x04\n',
        'last a.rrd\t\n', 'last  \t a.rrd\n', '\x7flast a.rrd\n',
        '\x01last a.rrd\n', ' last\t\n', 'last\x04a.rrd\n',
        '\xfflast a.rrd\n', 'last a.rrd\xe9\n', 'last \xa0a.rrd\n',
        '\x80last a.rrd\xa0\n', 'last a.rrd\x00\nlast a.rrd\n',
        'last a.rrd\x00 junk\nlast a.rrd\nlast a.rrd\n',
        'last\x00 a.rrd\n', 'last a.rrd\x00', '\x00\x00\x00',
    ]
    for text in simple:
        pipe('session', text)
    pipe('workdir', 'last b.rrd\nls\n', 'sub')
    pipe('workdir', 'last a.rrd\n', 'missing')
    pipe('workdir', 'last a.rrd\n', 'notes.txt')
    pipe('workdir', 'last a.rrd\n', '')
    pipe('workdir', 'last a.rrd\n', '.')
    pipe('workdir', 'cd ..\nlast a.rrd\n', 'sub')
    pipe('workdir', 'last c.rrd\n', 'sub/deep')
    case('pipe', 'workdir', {'args': ['-', 'a', 'b'], 'stdin': 'last a.rrd\n'})
    case('pipe', 'workdir', {'args': ['-', 'sub', '--'],
                             'stdin': 'last a.rrd\n'})
    pipe('env', 'last a.rrd\ninfo e.rrd\n',
         env={'RRDCACHED_ADDRESS': 'unix:/nonexistent/sock'})
    pipe('env', 'last a.rrd -d unix:/nonexistent/sock\nlast a.rrd\n')
    pipe('env', 'graph /dev/null -s 1000000000 -e 1000000300 '
         'DEF:x=f.rrd:x:MAX VDEF:v=x,MAXIMUM "PRINT:v:%c:strftime"\n',
         env={'TZ': 'America/New_York'})


# --------------------------------------------------------- format strings

VALUES = {
    'zero': '0', 'negzero': '=x,0,*,-1,*', 'one': '1', 'neg': '-1',
    'half': '0.5', 'frac': '1234.5678', 'tiny': '1e-9', 'tiny2': '1e-30',
    'huge': '1e30', 'big': '123456789', 'unkn': 'UNKN', 'inf': 'INF',
    'neginf': 'NEGINF', 'round': '999.9995', 'k': '1024',
    'mi': '1048576', 'pi': '3.14159265358979',
}

FORMATS = [
    '%lf', '%le', '%lg', '%lE', '%lG', '%lF', '%.0lf', '%.3lf', '%10.2lf',
    '%-10.2lf|', '%+lf', '% lf', '%05.1lf', '%#.0lf', '%#lg', '%.10le',
    '%.20lf', '%.0le', '%1.1lg', '%lf %s', '%.2lf%s', '%6.2lf %S',
    '%lf%%', 'x%lfy', '%%%lf%%', 'value: %8.3lf %s units',
]

BAD_FORMATS = [
    '%d', '%i', '%x', '%f', '%e', '%g', '%s', '%S', '%lf %lf', '%c', '%ld',
    '%llf', '%Lf', '%hf', '%*lf', '%.*lf', '%n', '%p', '%', 'abc%', '%lf%',
    '%lf %s %s', '%s %lf', '%10s', '%ls', '%lf%d', '%-+lf', '%+-lf',
    '% +lf', '%00lf', '%lfx%', '%.lf', '%1.lf', '%l', '%lfe', '',
    'plain text', '%%', '%lf%S%s', '%lf %S %S', '%99999lf', '%.99999lf',
    '%le%le',
]

STRFTIME = [
    '%a', '%A', '%b', '%B', '%c', '%d', '%e', '%H', '%I', '%j', '%m', '%M',
    '%p', '%S', '%U', '%V', '%w', '%W', '%x', '%X', '%y', '%Y', '%Z', '%z',
    '%s', '%G', '%g', '%u', '%C', '%D', '%F', '%T', '%R', '%r', '%h', '%k',
    '%l', '%P', '%n', '%t', '%%', '%Q', '%', 'x%', '%5Y', '%-d', '%_d',
    '%0e', '%Ex', '%Oy', '%+', '%E', '%O', 'plain', '', '%c %Z',
    '%Y-%m-%dT%H\\:%M\\:%S', '%%Y', '%%%Y', '%lf',
]

DURATION = [
    '%W', '%d', '%D', '%h', '%H', '%m', '%M', '%s', '%S', '%f', '%%',
    '%02H:%02m:%02s', '%.3S', '%5D', '%05d', '%0.1H', '%.2f', '%3.1W',
    '%W weeks %d days %h:%m:%s.%f', 'plain', '', '%x', '%', '%lf', '%0',
    '%5', '%.', '%-5D', '%+D', '%10.3M', '%%%D%%', '%05.2s', '%1S',
]

DUR_VALUES = ['0', '1', '999', '1000', '61001', '3600000', '90061001',
              '694861001', '-90061001', '1.5', '-1', 'UNKN', 'INF',
              '1e15', '1e20', '0.0004']

DUR_EXTRA = [
    ('1999', '%.0S'), ('1999', '%.2S'), ('1999', '%.4S'),
    ('-90061001', 'x%D y%H'), ('-90061001', '%%x'), ('59999', '%.1M'),
    ('-1500', '%S %f'), ('90061001', '%00010D'), ('90061001', '%.10H'),
    ('90061001', '%5%'), ('-0.5', '%f'), ('UNKN', 'x%D'), ('1.9e3', '%.3S'),
    ('3600000', '%5.2h'), ('90061001', '%0.0H'),
]

SI_VALUES = ['1e-24', '1e-21', '1e-18', '1e-15', '1e-12', '1e-9', '1e-6',
             '1e-3', '0.01', '0.999', '1', '999', '1000', '1500', '1e6',
             '1e9', '1e12', '1e15', '1e18', '1e21', '1e24', '1e27', '1e-27',
             '1023', '1024', '1048575', '1048576', '-1500', '-1e-6', '0',
             'UNKN', 'INF', 'NEGINF', '999999', '999.9999', '1e300',
             '1e-300', '5e-324']


def cdef(value: str) -> str:
    # A value starting with `=` is a complete expression.
    if value.startswith('='):
        return 'CDEF:c=' + value[1:]
    return f'CDEF:c=x,0,*,{value},+'


def print_graph(value: str, *items: str, extra: list[str] | None = None
                ) -> list[str]:
    return GRAPH[:2] + (extra or []) + GRAPH[2:] + [
        cdef(value), 'VDEF:v=c,MAXIMUM'] + list(items)


def format_cases() -> None:
    for value in VALUES.values():
        for spec in FORMATS:
            fmt('print', *print_graph(value, f'PRINT:v:{spec}'))
    for spec in BAD_FORMATS:
        fmt('badprint', *print_graph('1', f'PRINT:v:{spec}'))
        fmt('badgprint', *print_graph('1', f'GPRINT:v:{spec}',
                                      extra=['-j']))
    for spec in ['%lf', '%5.2lf %s', '%le', 'x %lg y']:
        fmt('gprint', *print_graph('1234.5', f'GPRINT:v:{spec}',
                                   extra=['-j']))
        fmt('gprint', *print_graph('1234.5', f'GPRINT:v:{spec}',
                                   extra=['-g']))
    for value in SI_VALUES:
        fmt('si', *print_graph(value, 'PRINT:v:%lf %s', 'PRINT:v:%.2le%s',
                               'PRINT:v:%6.2lf %S'))
        fmt('si', *print_graph(value, 'PRINT:v:%lf %s', 'PRINT:v:%6.2lf %S',
                               extra=['--base', '1024']))
    for extra in [['--base', '1000'], ['--base', '1024'], ['--base', '1000x'],
                  ['--base', '999'], ['--base', 'abc'], ['--base', '0'],
                  ['--base', '-1024'], ['--base', '2048'],
                  ['--base', '1024.5'], ['-b', '1024'], ['-X', '3'],
                  ['--units-exponent', '-3'], ['--units=si'],
                  ['--units-length', '5'], ['--units', 'bogus']]:
        fmt('base', *print_graph('1536000', 'PRINT:v:%lf %s',
                                 'PRINT:v:%lf %S', extra=extra))
    # %S follows a magnitude set by an earlier PRINT
    fmt('si', *GRAPH, 'CDEF:a=x,0,*,1500000,+', 'CDEF:b=x,0,*,2500,+',
        'CDEF:z=x,0,*', 'VDEF:va=a,MAXIMUM', 'VDEF:vb=b,MAXIMUM',
        'VDEF:vz=z,MAXIMUM', 'PRINT:vb:%lf %S', 'PRINT:va:%lf %s',
        'PRINT:vb:%lf %S', 'PRINT:vz:%lf %S', 'PRINT:vb:%lf %s',
        'PRINT:vb:%lf %S')
    fmt('si', *GRAPH, 'CDEF:z=x,0,*', 'CDEF:b=x,0,*,2500,+',
        'VDEF:vz=z,MAXIMUM', 'VDEF:vb=b,MAXIMUM', 'PRINT:vz:%lf %S',
        'PRINT:vb:%lf %S', 'PRINT:vz:%lf %S')
    fmt('si', *GRAPH, 'CDEF:b=x,0,*,2500,+', 'VDEF:vb=b,MAXIMUM',
        'GPRINT:vb:%lf %s', 'PRINT:vb:%lf %S', '-j')
    # strftime on the time component, and valstrftime on the value
    for spec in STRFTIME:
        fmt('strftime', *GRAPH, 'VDEF:v=x,MAXIMUM',
            f'PRINT:v:{spec}:strftime')
    for zone in ['America/New_York', 'Asia/Kolkata', 'Australia/Lord_Howe']:
        for spec in ['%c', '%H:%M %Z %z', '%s']:
            fmt('strftime', *GRAPH, 'VDEF:v=x,MAXIMUM',
                f'PRINT:v:{spec.replace(":", chr(92) + ":")}:strftime',
                env={'TZ': zone})
    for value in ['0', '1000000000', '1.9', '-1', 'UNKN', 'INF', '1e12',
                  '2147483648', '-2147483649', '67767976233532799',
                  '1e20']:
        fmt('valstrftime', *print_graph(value, 'PRINT:v:%Y-%m-%d %H\\:%M\\:%S'
                                        ':valstrftime'))
    for spec in ['%lf', '%c', '', '%', '%Q']:
        fmt('valstrftime', *print_graph('1000000000',
                                        f'PRINT:v:{spec}:valstrftime'))
    for spec in DURATION:
        fmt('duration', *print_graph('90061001',
                                     f'PRINT:v:{spec}:valstrfduration'))
    for value in DUR_VALUES:
        fmt('duration', *print_graph(
            value, 'PRINT:v:%W/%d/%D/%h/%H/%m/%M/%s/%S/%f:valstrfduration'))
    for value, spec in DUR_EXTRA:
        fmt('duration', *print_graph(value, f'PRINT:v:{spec}:valstrfduration'))
    for zone in ['America/New_York', 'Asia/Kolkata']:
        fmt('valstrftime', *print_graph(
            '1000000000', 'PRINT:v:%Y-%m-%d %H\\:%M %Z %s:valstrftime'),
            env={'TZ': zone})
    for suffix in ['strftime', 'valstrftime', 'valstrfduration', 'bogus',
                   'STRFTIME', '', 'strftime:x']:
        fmt('suffix', *print_graph('1000000000', f'PRINT:v:%lf:{suffix}'))
    for item in ['GPRINT:v:%c:strftime', 'GPRINT:v:%lf:valstrfduration',
                 'GPRINT:v:%H:valstrftime', 'GPRINT:v:%lf:bogus']:
        fmt('suffix', *print_graph('1000000000', item, extra=['-j']))
    # deprecated forms and non-VDEF names
    for item in ['PRINT:c:MAX:%lf', 'PRINT:c:AVERAGE:%6.2lf %s',
                 'PRINT:c:LAST:%le', 'PRINT:c:MIN:%d', 'PRINT:c:BOGUS:%lf',
                 'PRINT:c:%lf', 'PRINT:x:%lf', 'PRINT:nope:%lf',
                 'PRINT:v', 'PRINT:v:', 'PRINT:', 'PRINT', 'PRINT:v:%lf:',
                 'PRINT:v:a\\:b %lf', 'PRINT:v:%lf\\', 'PRINT:c:MAX:%lf:x',
                 'GPRINT:c:MAX:%lf', 'GPRINT:c:BOGUS:%lf', 'GPRINT:v',
                 'PRINT:v:%lf\\n', 'PRINT:v:%lf\\l', 'PRINT:v:%lf\\g']:
        fmt('items', *print_graph('2.5', item, extra=['-j']))
    # bounds
    for length in [100, 254, 255, 256, 300, 1000, 5000]:
        fmt('bounds', *print_graph('1', 'PRINT:v:' + 'a' * length + '%lf'))
        fmt('bounds', *print_graph('1', 'PRINT:v:%' + str(length) + 'lf'))
        fmt('bounds', *GRAPH, 'VDEF:v=x,MAXIMUM',
            'PRINT:v:' + '%Y' * (length // 2) + ':strftime')
    # The format field is cut to 200 bytes before it is checked.
    for text in ['a' * 196 + '%lf', 'a' * 197 + '%lf', 'a' * 198 + '%lf',
                 'a' * 196 + '%lf %s', 'a' * 195 + '%lf%s']:
        fmt('bounds', *print_graph('1500', f'PRINT:v:{text}'))
        fmt('bounds', *print_graph('1500', f'GPRINT:v:{text}', extra=['-j']))
    for text in ['x' * 197 + '%Y', 'x' * 198 + '%Y', 'x' * 199 + '%Y',
                 'x' * 300 + '%Y']:
        fmt('bounds', *GRAPH, 'VDEF:v=x,MAXIMUM', f'PRINT:v:{text}:strftime')
    for length in [10, 60, 64, 120, 400]:
        fmt('bounds', *print_graph('1', 'PRINT:v:%.' + str(length) + 'lf'))
        fmt('bounds', *print_graph('1e300', 'PRINT:v:%.' + str(length)
                                   + 'lf'))
        fmt('bounds', *print_graph('90061001', 'PRINT:v:%0' + str(length)
                                   + 'D:valstrfduration'))
    # imginfo
    for info in ['%s %lu %lu', '<IMG SRC="%s" WIDTH="%lu" HEIGHT="%lu">',
                 '%s', '%lu', 'plain', '', '%d %d %d', '%s %s %s',
                 '%s %lu %lu %lu', '%%', '%x', '%5s|%-5lu|%05lu',
                 '%s%lu%lu%s']:
        fmt('imginfo', *GRAPH[:2], '-f', info, *GRAPH[2:],
            'VDEF:v=x,MAXIMUM', 'PRINT:v:%lf')
    for target in ['out.png', 'dir.x/a.b.png', 'noext', '-']:
        fmt('imginfo', 'graph', target, '-s', T0, '-e', T1, '-j', '-w', '20',
            '-h', '10', '-f', '%s %lu %lu', 'DEF:x=f.rrd:x:AVERAGE',
            'VDEF:v=x,MAXIMUM', 'PRINT:v:%lf', tags=['binary-stdout'] if
            target == '-' else None)
    # axis format options are validated with the same rules
    for opt in ['--left-axis-format', '--right-axis-format']:
        for spec in ['%lf', '%5.1lf', '%d', '%s', '%lf %s', '']:
            extra = ['--right-axis', '1:0'] if 'right' in opt else []
            fmt('axisformat', *GRAPH[:2], '-j', *extra, opt, spec,
                *GRAPH[2:], 'LINE1:x#f00')


def all_cases() -> list[dict]:
    CASES.clear()
    dispatch_cases()
    option_cases()
    output_cases()
    alias_cases()
    env_cases()
    pipe_cases()
    format_cases()
    for entry in CASES:
        # Directory enumeration order is not portable; see DIR_ORDER.
        for step in entry['steps']:
            text = step.get('stdin', '')
            if (step['args'][:1] == ['list'] and '.' in step['args']) \
                    or 'list -r .' in text:
                entry['tags'].append('dir-order')
    counters: dict[str, int] = {}
    for entry in CASES:
        key = f'{entry["area"]}-{entry["group"]}'
        n = counters.get(key, 0) + 1
        counters[key] = n
        entry['id'] = f'{key}-{n:03d}'
    return CASES


# ------------------------------------------------------------ execution

def normalize(text: str, cwd: Path, bindir: str) -> str:
    lines = text.split('\n')
    out: list[str] = []
    run: list[str] = []
    # The longer spelling first: on macOS the real path of a temporary
    # directory is the given one with /private in front.
    paths = sorted({str(cwd), os.path.realpath(cwd)}, key=len, reverse=True)
    for line in lines:
        position = line.find('Compiled ')
        if position >= 0:
            line = line[:position + len('Compiled ')]
        if TIMING.match(line):
            line = 'OK u:* s:* r:*'
        line = CUR_ROW.sub(r'\1*', line)
        line = line.replace(bindir + '/', '{BIN}/')
        for path in paths:
            line = line.replace(path, '{CWD}')
        if LS_LINE.match(line):
            run.append(line)
            continue
        out.extend(sorted(run))
        run = []
        out.append(line)
    out.extend(sorted(run))
    return '\n'.join(out)


def program(binary: str, name: str | None) -> str:
    if not name:
        return binary
    return str(Path(binary).parent / name)


def run_case(binary: str, fixture: Path, entry: dict) -> list[dict]:
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / 'w'
        shutil.copytree(fixture, work, symlinks=True)
        env = {k: v for k, v in os.environ.items()
               if k not in ('RRDCACHED_ADDRESS', 'TZ', 'LANG', 'RRD_LOCKING')
               and not k.startswith('LC_')}
        env.update(TZ='UTC', LC_ALL='C')
        env.update(entry.get('env', {}))
        results = []
        bindir = str(Path(binary).parent)
        for step in entry['steps']:
            stdin = step.get('stdin')
            try:
                proc = subprocess.run(
                    [program(binary, step.get('argv0'))] + step['args'],
                    cwd=work, env=env, capture_output=True, timeout=60,
                    input=stdin.encode('latin-1') if stdin is not None
                    else b'')
            except subprocess.TimeoutExpired:
                results.append({'status': None, 'timeout': True})
                break
            if proc.returncode < 0:
                results.append({'status': None, 'crash': True})
                break
            if 'binary-stdout' in entry['tags']:
                stdout = f'<{len(proc.stdout)} bytes>'
            else:
                stdout = normalize(proc.stdout.decode('latin-1'), work,
                                   bindir)
            results.append({
                'status': proc.returncode, 'stdout': stdout,
                'stderr': normalize(proc.stderr.decode('latin-1'), work,
                                    bindir)})
        return results


def render(table: dict) -> str:
    def items(values: list) -> str:
        return ',\n'.join(json.dumps(value, ensure_ascii=False)
                          for value in values)
    return ('{\n'
            f'"comment": {json.dumps(table["comment"])},\n'
            f'"oracle": {json.dumps(table["oracle"])},\n'
            f'"dirs": {json.dumps(table["dirs"])},\n'
            f'"files": {json.dumps(table["files"], ensure_ascii=False)},\n'
            f'"fixture": [\n{items(table["fixture"])}\n],\n'
            f'"vectors": [\n{items(table["vectors"])}\n]\n'
            '}\n')


def ordered(entry: dict) -> dict:
    keys = ['id', 'area', 'group', 'env', 'steps', 'tags', 'expect',
            'expect_platform']
    return {key: entry[key] for key in keys if key in entry}


def read_table(path: str) -> dict:
    with open(path, encoding='utf-8') as handle:
        return json.load(handle)


def write_text(path: str, text: str) -> None:
    with open(path, 'w', encoding='utf-8') as handle:
        handle.write(text)


def merge_platform(out: str, key: str, source: str) -> int:
    table = read_table(out)
    results = read_table(source)
    for entry in table['vectors']:
        entry.get('expect_platform', {}).pop(key, None)
        if entry['id'] in results:
            entry.setdefault('expect_platform', {})[key] = results[entry['id']]
        if entry.get('expect_platform') == {}:
            del entry['expect_platform']
    table['vectors'] = [ordered(entry) for entry in table['vectors']]
    write_text(out, render(table))
    print(f'{len(results)} {key} expectations merged')
    return 0


def comparable(entry: dict, result: list[dict]) -> list[dict]:
    """The form of a result that is compared. Directory enumeration order
    differs between file systems, so `dir-order` vectors compare the sorted
    lines of standard output."""
    if 'dir-order' not in entry['tags']:
        return result
    return [dict(step, stdout='\n'.join(sorted(step['stdout'].split('\n'))))
            if 'stdout' in step else step for step in result]


def compare(binary: str, fixture: Path, table: dict, check: bool,
            platform_out: str | None) -> int:
    results = {}
    diverged = 0
    for entry in table['vectors']:
        if 'unstable' in entry['tags']:
            continue
        got = run_case(binary, fixture, entry)
        if comparable(entry, got) != comparable(entry, entry['expect']):
            diverged += 1
            results[entry['id']] = got
            if check:
                print(json.dumps({'id': entry['id'], 'steps': entry['steps'],
                                  'expect': entry['expect'], 'got': got},
                                 ensure_ascii=False))
    if platform_out:
        write_text(platform_out, json.dumps(results, indent=1, sort_keys=True,
                                            ensure_ascii=False) + '\n')
    print(f'{diverged} divergent of {len(table["vectors"])}', file=sys.stderr)
    return 0


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser()
    parser.add_argument('--rrdtool', default='rrdtool')
    parser.add_argument('--out', default=str(
        root / 'crates/rondi-cli/tests/data/cli_spec_vectors.json'))
    parser.add_argument('--check')
    parser.add_argument('--record-platform', metavar='KEY')
    parser.add_argument('--platform-out')
    parser.add_argument('--merge-platform', nargs=2, metavar=('KEY', 'FILE'))
    parser.add_argument('--only', help='print results of one group only')
    options = parser.parse_args()

    if options.merge_platform:
        return merge_platform(options.out, *options.merge_platform)

    binary = shutil.which(options.rrdtool) or options.rrdtool
    with tempfile.TemporaryDirectory() as tmp:
        fixture = Path(tmp) / 'fixture'
        if options.check or options.record_platform:
            table = read_table(options.out)
            if options.check:
                link = Path(tmp) / 'bin' / 'rrdtool'
                link.parent.mkdir()
                link.symlink_to(Path(options.check).resolve())
                for name in ALIASES:
                    (link.parent / name).symlink_to(
                        Path(options.check).resolve())
                binary = str(link)
            setup(binary, fixture, table['files'])
            return compare(binary, fixture, table, bool(options.check),
                           options.platform_out)

        version = subprocess.run([binary, '--version', 'x'],
                                 capture_output=True, text=True).stdout
        if not version.startswith('RRDtool 1.11.0 '):
            sys.exit(f'expected RRDtool 1.11.0, found {version[:40]!r}')
        files = build_seed(binary)
        setup(binary, fixture, files)
        vectors = []
        for entry in all_cases():
            if options.only and entry['group'] != options.only:
                continue
            entry['expect'] = run_case(binary, fixture, entry)
            again = run_case(binary, fixture, entry)
            if again != entry['expect']:
                entry['tags'].append('unstable')
            if any(step.get('crash') or step.get('timeout')
                   for step in entry['expect']):
                entry['tags'].append('undefined')
            vectors.append(ordered(entry))
            if options.only:
                print(json.dumps(ordered(entry), ensure_ascii=False))
        if options.only:
            return 0
        table = {
            'comment': 'Generated by scripts/cli-spec-vectors.py from '
                       'RRDtool 1.11.0; see docs/compatibility/cli.md, '
                       'pipe-mode.md and format-strings.md.',
            'oracle': version.splitlines()[0],
            'dirs': DIRS,
            'files': files,
            'fixture': FIXTURE,
            'vectors': vectors,
        }
        Path(options.out).parent.mkdir(parents=True, exist_ok=True)
        write_text(options.out, render(table))
        print(f'{len(vectors)} vectors written to {options.out}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
