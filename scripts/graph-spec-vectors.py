#!/usr/bin/env python3
"""Capture graph and xport vectors from a reference rrdtool.

Usage:
  scripts/graph-spec-vectors.py [--rrdtool PATH] [--out FILE]
  scripts/graph-spec-vectors.py --only GROUP          # print one group
  scripts/graph-spec-vectors.py --check PATH          # diff PATH vs FILE
  scripts/graph-spec-vectors.py --rrdtool PATH --record-platform KEY \
      --platform-out F
  scripts/graph-spec-vectors.py --merge-platform KEY F

The cases are listed here; the output file is the frozen table that
crates/rondi-cli/tests/graph_spec_vectors.rs replays and that
docs/compatibility/graph.md describes.

Every vector runs in a fresh copy of a fixture directory built by running
`restore` on XML dumps captured from the reference at generation time, so
both programs read identical data. A vector is a list of steps; each step
names its arguments and, optionally, files to inspect after it ran. The
environment is TZ=UTC, LC_ALL=C and no RRDCACHED_ADDRESS unless the vector
sets `env`. Each case runs twice; one whose two runs differ is tagged
`unstable` and is not compared frozen.

Image bytes depend on the font stack and the renderer (RD-016 in
docs/compatibility/graph.md), so they are never stored. Normalization (see
`normalize_bytes`): a PNG image in standard output or in an inspected file
becomes `<PNG WxH>` with the width and height of its IHDR chunk; an SVG,
PDF or EPS image becomes `<SVG>`, `<PDF>` or `<EPS>`; the size after
`image = BLOB_SIZE:` becomes `*`; the work directory becomes `{CWD}`; the
build stamp after `Compiled ` is dropped.

The table is captured on macOS arm64 (/opt/homebrew/bin/rrdtool 1.11.0).
Results that differ on Linux are recorded per vector under
expect_platform.<KEY> (KEY linux-x86_64 or linux-aarch64) with
--record-platform in a container that has the pinned oracle, then
--merge-platform. Regenerate only with RRDtool 1.11.0 and review the diff.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PNG_MAGIC = b'\x89PNG\r\n\x1a\n'
BLOB = re.compile(r'BLOB_SIZE:\d+')

# ------------------------------------------------------------ fixture


def updates(start: int, step: int, count: int, value) -> list[str]:
    return [f'{start + step * (i + 1)}:{value(i + 1)}' for i in range(count)]


def wave(i: int) -> str:
    x = 50 + 40 * math.sin(i / 20)
    y = 1000 * i
    return f'{x:.2f}:{y}'


# Commands run with the reference to produce the XML the fixture restores.
SEED = {
    'f.xml': [
        ['create', 'f.rrd', '--start', '1000000000', '--step', '60',
         'DS:x:GAUGE:120:U:U', 'DS:y:GAUGE:120:U:U',
         'RRA:AVERAGE:0.5:1:30', 'RRA:MAX:0.5:1:30', 'RRA:MIN:0.5:1:30',
         'RRA:LAST:0.5:1:30'],
        ['update', 'f.rrd'] + updates(
            1000000000, 60, 20,
            lambda i: f'{i}:{(-1) ** i * 10 * i}' if i != 8 else 'U:U'),
    ],
    'd.xml': [
        ['create', 'd.rrd', '--start', '1000000000', '--step', '300',
         'DS:x:GAUGE:600:U:U', 'DS:y:GAUGE:600:U:U',
         'RRA:AVERAGE:0.5:1:600', 'RRA:AVERAGE:0.5:6:200',
         'RRA:MAX:0.5:6:200', 'RRA:MIN:0.5:6:200'],
        ['update', 'd.rrd'] + updates(1000000000, 300, 576, wave),
    ],
    'e.xml': [
        ['create', 'e.rrd', '--start', '1000000000', '--step', '60',
         'DS:x:GAUGE:120:U:U', 'RRA:AVERAGE:0.5:1:30'],
    ],
}

FIXTURE = [
    ['restore', 'f.xml', 'f.rrd'],
    ['restore', 'd.xml', 'd.rrd'],
    ['restore', 'e.xml', 'e.rrd'],
]
DIRS = ['sub']
FILE_TIME = 1000000000
TEXT_FILES = {'notes.txt': 'not an rrd\n'}


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
        # --lazy compares the output file's time with the data, so files a
        # case may overwrite get a fixed, old modification time.
        os.utime(fixture / name, (FILE_TIME, FILE_TIME))
    env = dict(os.environ, TZ='UTC', LC_ALL='C')
    env.pop('RRDCACHED_ADDRESS', None)
    for command in FIXTURE:
        subprocess.run([binary] + command, cwd=fixture, env=env, check=True)


# --------------------------------------------------------------- cases

CASES: list[dict] = []

# f.rrd: step 60, rows 1000000060..1000001200 (x = 1..20, y = +-10i, row
# 8 unknown). d.rrd: step 300, two days of a sine wave from 1000000000.
T0, T1 = '1000000000', '1000001200'
RANGE = ['--start', T0, '--end', T1]
DRANGE = ['--start', '1000000000', '--end', '1000172800']
DEFX = 'DEF:x=f.rrd:x:AVERAGE'
DEFY = 'DEF:y=f.rrd:y:AVERAGE'
G = ['graph', '/dev/null'] + RANGE + [DEFX]
GV = ['graphv', '/dev/null'] + RANGE + [DEFX]
DV = ['graphv', '/dev/null'] + DRANGE + ['DEF:a=d.rrd:x:AVERAGE',
                                         'DEF:b=d.rrd:y:AVERAGE']
LINE = 'LINE1:x#ff0000'


def case(group: str, *steps: list[str] | dict,
         env: dict[str, str] | None = None,
         tags: list[str] | None = None) -> None:
    norm = []
    for step in steps:
        norm.append(step if isinstance(step, dict) else {'args': list(step)})
    entry = {'group': group, 'steps': norm, 'tags': list(tags or [])}
    if env:
        entry['env'] = env
    CASES.append(entry)


def run(group: str, *args: str, env: dict[str, str] | None = None,
        tags: list[str] | None = None) -> None:
    case(group, list(args), env=env, tags=tags)


def written(group: str, args: list[str], *paths: str,
            tags: list[str] | None = None) -> None:
    case(group, {'args': args, 'inspect': list(paths)}, tags=tags)


# Every long option of graph with a value it accepts; `None` marks a flag.
GRAPH_OPTIONS: list[tuple[str, str, str | None]] = [
    ('s', 'start', T0), ('e', 'end', T1), ('S', 'step', '120'),
    ('t', 'title', 'A title'), ('v', 'vertical-label', 'bytes'),
    ('', 'vertical-label-angle', '270'), ('w', 'width', '300'),
    ('h', 'height', '150'), ('j', 'only-graph', None),
    ('D', 'full-size-mode', None), ('u', 'upper-limit', '100'),
    ('l', 'lower-limit', '-5'), ('r', 'rigid', None),
    ('', 'allow-shrink', None), ('A', 'alt-autoscale', None),
    ('J', 'alt-autoscale-min', None), ('M', 'alt-autoscale-max', None),
    ('N', 'no-gridfit', None),
    ('x', 'x-grid', 'MINUTE:1:MINUTE:5:MINUTE:5:0:%H:%M'),
    ('', 'week-fmt', 'W%V'), ('y', 'y-grid', '5:2'),
    ('', 'left-axis-formatter', 'numeric'),
    ('', 'left-axis-format', '%.1lf'), ('Y', 'alt-y-grid', None),
    ('o', 'logarithmic', None), ('X', 'units-exponent', '3'),
    ('L', 'units-length', '8'), ('', 'units', 'si'),
    ('', 'right-axis', '2:5'), ('', 'right-axis-label', 'right'),
    ('', 'right-axis-label-angle', '90'),
    ('', 'right-axis-range', '0:100'),
    ('', 'right-axis-formatter', 'numeric'),
    ('', 'right-axis-format', '%.2lf'), ('g', 'no-legend', None),
    ('F', 'force-rules-legend', None),
    ('', 'legend-position', 'east'), ('', 'legend-direction', 'bottomup'),
    ('z', 'lazy', None), ('f', 'imginfo', '%s %lu %lu'),
    ('c', 'color', 'BACK#102030'), ('', 'grid-dash', '1:3'),
    ('', 'border', '0'), ('', 'dynamic-labels', None),
    ('m', 'zoom', '2'), ('n', 'font', 'DEFAULT:9:'),
    ('R', 'font-render-mode', 'light'),
    ('B', 'font-smoothing-threshold', '5'), ('P', 'pango-markup', None),
    ('G', 'graph-render-mode', 'mono'), ('E', 'slope-mode', None),
    ('a', 'imgformat', 'PNG'), ('i', 'interlaced', None),
    ('T', 'tabwidth', '20'), ('b', 'base', '1024'),
    ('W', 'watermark', 'mark'),
    ('Z', 'use-nan-for-all-missing-data', None),
    ('', 'add-jsontime', None), ('', 'utc', None),
    ('', 'disable-rrdtool-tag', None), ('', 'alt-y-mrtg', None),
    ('', 'no-minor', None),
]


def option_cases() -> None:
    for short, name, value in GRAPH_OPTIONS:
        if value is None:
            run('opt', 'graph', '/dev/null', f'--{name}', *RANGE, DEFX, LINE)
            if short:
                run('opt', 'graph', '/dev/null', f'-{short}', *RANGE, DEFX,
                    LINE)
            run('opt', 'graph', '/dev/null', f'--{name}=x', *RANGE, DEFX,
                LINE)
            continue
        if name in ('start', 'end'):
            run('opt', 'graph', '/dev/null', *RANGE, f'--{name}={value}',
                DEFX, LINE)
            run('opt', 'graph', '/dev/null', *RANGE, f'-{short}{value}',
                DEFX, LINE)
            continue
        run('opt', 'graph', '/dev/null', *RANGE, f'--{name}', value, DEFX,
            LINE)
        run('opt', 'graph', '/dev/null', *RANGE, f'--{name}={value}', DEFX,
            LINE)
        if short:
            run('opt', 'graph', '/dev/null', *RANGE, f'-{short}', value,
                DEFX, LINE)
            run('opt', 'graph', '/dev/null', *RANGE, f'-{short}{value}',
                DEFX, LINE)
        # A value-taking option given last has no argument.
        run('opt', 'graph', '/dev/null', *RANGE, DEFX, LINE, f'--{name}')
        if short:
            run('opt', 'graph', '/dev/null', *RANGE, DEFX, LINE, f'-{short}')

    # Grammar: clusters, abbreviations, order, separators, repeats.
    for args in [
            ['-gjE'], ['-gw', '300'], ['-gw300'], ['-jgD'], ['-g', '-j'],
            ['-wh', '300'], ['-w', '300', '-w', '200'],
            ['--wid', '300'], ['--wi', '300'], ['--w', '300'],
            ['--al'], ['--alt'], ['--alt-a'], ['--alt-autoscale-m'],
            ['--right-axis-l', 'x'], ['--no'], ['--no-l'], ['--u', '5'],
            ['--units-e', '3'], ['--force'], ['--lo'], ['--log'],
            ['--legend-p', 'east'], ['--WIDTH', '300'], ['--Width', '300'],
            ['-?'], ['-'], ['--bogus'], ['--bogus=1'], ['-K'], ['-k'],
            ['-q'], ['-1'], ['--', 'LINE2:x#00ff00'],
            ['--', '--width', '300'], ['---width', '300'],
            ['--width=', '300'], ['--width='], ['-w', ''],
            ['--no-legend=1'], ['--only-graph=yes'],
            ['-w', '300', '--', '-h', '50'], ['-x'], ['-Hw', '5']]:
        run('grammar', 'graph', '/dev/null', *RANGE, *args, DEFX, LINE)
    # Options after the elements and between them.
    run('grammar', 'graph', '/dev/null', DEFX, LINE, *RANGE, '-w', '300')
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX, '-w', '300', LINE)
    run('grammar', 'graph', *RANGE, '/dev/null', DEFX, LINE)
    run('grammar', 'graph', '-w', '300', '/dev/null', *RANGE, DEFX, LINE)
    run('grammar', 'graph', '--', '/dev/null', *RANGE, DEFX, LINE)
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX, LINE, '--')
    run('grammar', 'graph', '/dev/null', *RANGE, '-w', '300', '/dev/null',
        DEFX, LINE)
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX, LINE, 'extra')
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX, LINE, '')
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX, '', LINE)
    run('grammar', 'graph', '/dev/null', *RANGE)
    run('grammar', 'graph', '/dev/null')
    run('grammar', 'graph')
    run('grammar', 'graph', '--width', '300')
    run('grammar', 'graph', '', *RANGE, DEFX, LINE)
    run('grammar', 'graphv', '/dev/null', *RANGE)
    run('grammar', 'graph', '/dev/null', *RANGE, DEFX)


# Values that a numeric option accepts or refuses.
NUMERIC = {
    'width': ['9', '10', '1e2', '0x20', ' 50', '50abc', '-1', '0', 'abc',
              '', '4294967306', '2.9', '+30', '99999999999999999999'],
    'height': ['9', '10', '1e2', '-1', '0', 'abc', '', '+30'],
    'upper-limit': ['abc', '1e400', '-1e400', 'nan', 'inf', '-inf', '5x',
                    '', '0x10', '1e-300', '-0'],
    'lower-limit': ['abc', '1e400', '-1e400', 'nan', '', '1,5'],
    'base': ['1000', '1024', '999', '0', '-1', 'abc', '1e3', '1024x', ''],
    'units-exponent': ['0', '3', '-18', '18', '21', '-21', '2', '1',
                       'abc', '', '3.5', '-3'],
    'units-length': ['0', '1', '-1', '-100', 'abc', '', '100', '2.5'],
    'zoom': ['0', '-1', 'abc', '0.5', '1e-3', '', '3', 'nan', 'inf'],
    'tabwidth': ['0', '-1', 'abc', '', '1', '100'],
    'border': ['-1', '0', '5', 'abc', '', '1000'],
    'step': ['0', '-1', 'abc', '', '1', '60', '1m', '86400', '1e3'],
    'vertical-label-angle': ['0', '45', '-90', 'abc', '', '360', '1e3'],
    'right-axis-label-angle': ['0', '45', 'abc', ''],
    'font-smoothing-threshold': ['0', '-1', 'abc', ''],
}


def numeric_cases() -> None:
    for name, values in NUMERIC.items():
        for value in values:
            run('num', 'graph', '/dev/null', *RANGE, f'--{name}', value,
                DEFX, LINE)


def value_cases() -> None:
    for value in ['PNG', 'png', 'Png', 'SVG', 'EPS', 'PDF', 'XML', 'xml',
                  'XMLENUM', 'JSON', 'JSONTIME', 'CSV', 'TSV', 'SSV',
                  'BOGUS', '', 'PNGX', 'PN', 'JPEG', 'GIF']:
        run('imgformat', 'graph', '/dev/null', *RANGE, '--imgformat', value,
            DEFX, LINE + ':x')
    for value in ['BACK#102030', 'CANVAS#ffffff', 'SHADEA#000000',
                  'SHADEB#000000', 'GRID#ff0000', 'MGRID#ff0000',
                  'FONT#000000', 'AXIS#000000', 'FRAME#000000',
                  'ARROW#00ff00', 'BACK#10203040', 'BACK#102', 'BACK#1020',
                  'BACK#10203', 'BACK#1020304', 'BACK#102030405',
                  'BACK#gg0000', 'BACK#', 'BACK', 'BACK102030', 'back#102030',
                  'BOGUS#102030', '#102030', '', 'BACK#102030x',
                  'BACK #102030', 'BACK#-10203', 'XBACK#102030', 'BAC#102030',
                  'BACKX#102030']:
        run('color', 'graph', '/dev/null', *RANGE, '--color', value, DEFX,
            LINE)
    for value in ['DEFAULT:10:', 'DEFAULT:10', 'DEFAULT:0:Courier',
                  'TITLE:13:Times', 'AXIS:8:', 'UNIT:8:', 'LEGEND:12:',
                  'WATERMARK:5:', 'BOGUS:10:', 'DEFAULT:abc:', 'DEFAULT',
                  'DEFAULT:', 'DEFAULT:-1:', 'default:10:', ':10:', '',
                  'LEGEND:10:Some Font', 'TITLE:1e1:', 'DEFAULT:10:x:y']:
        run('font', 'graph', '/dev/null', *RANGE, '--font', value, DEFX,
            LINE)
    for name, values in [
            ('font-render-mode', ['normal', 'light', 'mono', 'NORMAL',
                                  'bogus', '']),
            ('graph-render-mode', ['normal', 'mono', 'MONO', 'light',
                                   'bogus', '']),
            ('legend-position', ['north', 'south', 'east', 'west', 'North',
                                 'up', '']),
            ('legend-direction', ['topdown', 'bottomup', 'bottomup2',
                                  'TOPDOWN', 'down', '']),
            ('units', ['si', 'SI', 'Si', 'none', '']),
            ('left-axis-formatter', ['numeric', 'timestamp', 'duration',
                                     'NUMERIC', 'bogus', '']),
            ('right-axis-formatter', ['numeric', 'timestamp', 'duration',
                                      'bogus', '']),
            ('grid-dash', ['1:3', '1:0', '0:1', '0:0', '2.5:1.5', '1',
                           'abc', '', '1:', ':1', '-1:1', '1:3:5']),
            ('right-axis', ['1:0', '2:5', '0:0', '0:5', '-1:0', '1', 'abc',
                            '', '1:abc', '1e3:-1e3', '1:0:9']),
            ('right-axis-range', ['0:100', '0:', ':100', ':', '100:0',
                                  'abc', '', '1', '0:abc', 'nan:1']),
            ('left-axis-format', ['%lf', '%.2lf', '%le', '%lg', '%5.1lf %s',
                                  '%d', '%s', '%lf%lf', 'abc', '',
                                  '%%', '%lf %%', '%10.3le']),
            ('right-axis-format', ['%lf', '%d', 'abc', '']),
            ('y-grid', ['none', '1:2', '5:1', '0:1', '1:0', '-1:1', 'abc',
                        '', '1', '1:2:3', '0.5:2', 'NONE']),
            ('x-grid', ['none', 'NONE',
                        'MINUTE:1:MINUTE:5:MINUTE:5:0:%H:%M',
                        'SECOND:30:MINUTE:1:MINUTE:2:0:%M',
                        'HOUR:1:DAY:1:DAY:1:86400:%a',
                        'WEEK:1:MONTH:1:MONTH:1:2592000:%b',
                        'YEAR:1:YEAR:1:YEAR:1:0:%Y',
                        'minute:1:MINUTE:5:MINUTE:5:0:%H',
                        'MINUTE:0:MINUTE:5:MINUTE:5:0:%H',
                        'MINUTE:-1:MINUTE:5:MINUTE:5:0:%H',
                        'MINUTE:1:MINUTE:5:MINUTE:5:0',
                        'MINUTE:1:MINUTE:5:MINUTE:5',
                        'MINUTE:1:MINUTE:5', 'FOO:1:MINUTE:5:MINUTE:5:0:%H',
                        'MINUTE:x:MINUTE:5:MINUTE:5:0:%H',
                        'MINUTE:1:MINUTE:5:MINUTE:5:x:%H',
                        'MINUTE:1:MINUTE:5:MINUTE:5:0:', 'abc', '',
                        'MINUTE:1:MINUTE:5:MINUTE:5:0:' + 'x' * 120]),
            ('week-fmt', ['W%V', '', '%', 'x' * 200]),
            ('title', ['', 'x' * 300, 'a\\nb', 'a<br/>b']),
            ('vertical-label', ['', 'v' * 300]),
            ('watermark', ['', 'w' * 300]),
            ('right-axis-label', ['', 'r' * 100]),
            ('imginfo', ['%s', '%lu %lu', '%s %lu %lu %%', '', '%d', '%s%s',
                         '<IMG SRC="%s" WIDTH="%lu" HEIGHT="%lu">', 'x',
                         '%lu', '%lu %s']),
            ('daemon', ['', 'unix:/nonexistent/sock', 'bad host name x'])]:
        for value in values:
            run(name, 'graph', '/dev/null', *RANGE, f'--{name}', value,
                DEFX, LINE)
    # The size line is left out when an argument word is an imginfo option.
    for args in [['-t', '-f'], ['-t', '--imginfo'],
                 ['-t', '-f', '-f', '%s %lu %lu'],
                 ['--imginfo=%s %lu %lu'], ['-f%s %lu %lu'],
                 ['-W', '--imginfo=x']]:
        run('imginfo', 'graph', '/dev/null', *RANGE, *args, DEFX, LINE)
    # Formatters with matching and mismatching formats.
    for formatter, fmt in [('timestamp', '%Y-%m-%d'), ('timestamp', '%lf'),
                           ('duration', '%H:%m'), ('duration', '%lf'),
                           ('numeric', '%Y'), ('timestamp', '')]:
        run('formatter', 'graph', '/dev/null', *RANGE,
            '--left-axis-formatter', formatter, '--left-axis-format', fmt,
            DEFX, LINE)
        run('formatter', 'graph', '/dev/null', *RANGE, '--right-axis', '1:0',
            '--right-axis-formatter', formatter, '--right-axis-format', fmt,
            DEFX, LINE)
    run('formatter', 'graph', '/dev/null', *RANGE, '--right-axis-format',
        '%lf', DEFX, LINE)
    run('formatter', 'graph', '/dev/null', *RANGE, '--right-axis-formatter',
        'timestamp', DEFX, LINE)


def time_cases() -> None:
    for start, end in [(T0, T1), ('end-10min', T1), (T0, 'start+10min'),
                       ('end-1h', 'start+1h'), (T1, T0), (T0, T0),
                       ('abc', T1), (T0, 'abc'), ('', T1), (T0, ''),
                       ('1000000000.5', T1), ('20010909', '20010910'),
                       ('noon 20010909', 'start+1h'), ('epoch', T1),
                       ('end-1', T1), ('end-1d', '20010910'),
                       ('1000000000', '1000000001')]:
        run('time', 'graphv', '/dev/null', '--start', start, '--end', end,
            DEFX, LINE)
        run('time', 'xport', '--start', start, '--end', end, DEFX, 'XPORT:x')
    for step in ['60', '120', '300', '1', '59', '61', '3600', '86400']:
        run('time', 'graphv', '/dev/null', *RANGE, '--step', step, DEFX,
            LINE, '--imgformat', 'CSV')
        run('time', 'graphv', '/dev/null', *DRANGE, '--step', step,
            'DEF:a=d.rrd:x:AVERAGE', 'LINE1:a#f00', '--imgformat', 'CSV')
    for width in ['10', '20', '100', '576', '1000']:
        run('time', 'graphv', '/dev/null', *DRANGE, '-w', width,
            'DEF:a=d.rrd:x:AVERAGE', 'LINE1:a#f00', '--imgformat', 'JSON')
    for zone in ['UTC', 'America/New_York', 'Asia/Kolkata']:
        run('time', 'graphv', '/dev/null', '--start', '20010909', '--end',
            'start+1d', 'DEF:a=d.rrd:x:AVERAGE', 'LINE1:a#f00',
            env={'TZ': zone})
        run('time', 'graphv', '/dev/null', '--start', '20010909', '--end',
            'start+1d', '--utc', 'DEF:a=d.rrd:x:AVERAGE', 'LINE1:a#f00',
            env={'TZ': zone})


# ------------------------------------------------------------- elements

DEF_FORMS = [
    'DEF:x=f.rrd:x:AVERAGE', 'DEF:x=f.rrd:x:MAX', 'DEF:x=f.rrd:x:MIN',
    'DEF:x=f.rrd:x:LAST', 'DEF:x=f.rrd:x:average', 'DEF:x=f.rrd:x:Average',
    'DEF:x=f.rrd:x:BOGUS', 'DEF:x=f.rrd:x:', 'DEF:x=f.rrd:x',
    'DEF:x=f.rrd', 'DEF:x=', 'DEF:x', 'DEF:', 'DEF', 'DEF:=f.rrd:x:AVERAGE',
    'DEF:x=f.rrd:nope:AVERAGE', 'DEF:x=missing.rrd:x:AVERAGE',
    'DEF:x=notes.txt:x:AVERAGE', 'DEF:x=sub:x:AVERAGE',
    'DEF:x=:x:AVERAGE', 'DEF:x=f.rrd::AVERAGE',
    'DEF:x=f.rrd:x:AVERAGE:step=120', 'DEF:x=f.rrd:x:AVERAGE:step=60',
    'DEF:x=f.rrd:x:AVERAGE:step=1', 'DEF:x=f.rrd:x:AVERAGE:step=0',
    'DEF:x=f.rrd:x:AVERAGE:step=abc', 'DEF:x=f.rrd:x:AVERAGE:step=-60',
    'DEF:x=f.rrd:x:AVERAGE:step=2m', 'DEF:x=f.rrd:x:AVERAGE:step=',
    'DEF:x=f.rrd:x:AVERAGE:start=1000000300',
    'DEF:x=f.rrd:x:AVERAGE:end=1000000600',
    'DEF:x=f.rrd:x:AVERAGE:start=1000000300:end=1000000600',
    'DEF:x=f.rrd:x:AVERAGE:start=end-5min:end=1000000600',
    'DEF:x=f.rrd:x:AVERAGE:start=abc', 'DEF:x=f.rrd:x:AVERAGE:end=abc',
    'DEF:x=f.rrd:x:AVERAGE:start=1000000600:end=1000000300',
    'DEF:x=f.rrd:x:AVERAGE:reduce=MAX', 'DEF:x=f.rrd:x:AVERAGE:reduce=LAST',
    'DEF:x=f.rrd:x:AVERAGE:reduce=BOGUS', 'DEF:x=f.rrd:x:AVERAGE:reduce=',
    'DEF:x=f.rrd:x:AVERAGE:bogus=1', 'DEF:x=f.rrd:x:AVERAGE:step',
    'DEF:x=f.rrd:x:AVERAGE:', 'DEF:x=f.rrd:x:AVERAGE::',
    'DEF:x=f.rrd:x:AVERAGE:step=120:step=60',
    'DEF:x=f.rrd:x:AVERAGE:daemon=unix\\:/nonexistent/sock',
    'DEF:x=f\\:rrd:x:AVERAGE', 'DEF:x=f.rrd:x:AVERAGE:start=1000000300\\:x',
    'DEF:1x=f.rrd:x:AVERAGE', 'DEF:_x=f.rrd:x:AVERAGE',
    'DEF:x-y=f.rrd:x:AVERAGE', 'DEF:x.y=f.rrd:x:AVERAGE',
    'DEF:x y=f.rrd:x:AVERAGE', 'DEF:x!=f.rrd:x:AVERAGE',
    'DEF:' + 'v' * 255 + '=f.rrd:x:AVERAGE',
    'DEF:' + 'v' * 256 + '=f.rrd:x:AVERAGE',
    'DEF:MAX=f.rrd:x:AVERAGE', 'DEF:PI=f.rrd:x:AVERAGE',
    'DEF:x =f.rrd:x:AVERAGE', 'DEF: x=f.rrd:x:AVERAGE',
    'DEF:x=f.rrd:x:AVERAGE ', 'def:x=f.rrd:x:AVERAGE',
    'DEF:x=f.rrd:x:AVERAGE:step=120:reduce=MIN:start=1000000300',
]


def def_cases() -> None:
    for form in DEF_FORMS:
        vname = form[4:].split('=', 1)[0] if '=' in form else 'x'
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,255}', vname):
            vname = 'x'
        run('def', 'graph', '/dev/null', *RANGE, form,
            f'PRINT:{vname}:AVERAGE:%lf', f'LINE1:{vname}#ff0000')
    # Duplicate names and unknown references.
    for elements in [
            [DEFX, DEFX, LINE],
            [DEFX, 'DEF:x=f.rrd:y:AVERAGE', 'PRINT:x:AVERAGE:%lf'],
            [DEFX, 'CDEF:x=x,2,*', LINE],
            [DEFX, 'VDEF:x=x,AVERAGE', LINE],
            ['LINE1:x#ff0000', DEFX],
            ['CDEF:c=x,2,*', DEFX, 'LINE1:c#ff0000'],
            [DEFX, 'DEF:z=f.rrd:x:AVERAGE:start=1000000600',
             'CDEF:c=x,z,+', 'PRINT:c:AVERAGE:%lf', 'PRINT:z:AVERAGE:%lf'],
            [DEFX, 'DEF:z=f.rrd:x:AVERAGE:step=300', 'CDEF:c=x,z,+',
             'PRINT:c:AVERAGE:%lf', 'PRINT:c:MAX:%lf']]:
        run('def', 'graph', '/dev/null', *RANGE, *elements)
    run('def', 'graph', '/dev/null', *RANGE, '-Z',
        'DEF:x=missing.rrd:x:AVERAGE', 'PRINT:x:AVERAGE:%lf', LINE)
    run('def', 'graph', '/dev/null', *RANGE, '-Z',
        'DEF:x=f.rrd:nope:AVERAGE', 'PRINT:x:AVERAGE:%lf', LINE)
    run('def', 'graph', '/dev/null', *RANGE, '-Z',
        'DEF:x=f.rrd:x:BOGUS', 'PRINT:x:AVERAGE:%lf', LINE)
    run('def', 'graph', '/dev/null', *RANGE,
        'DEF:x=e.rrd:x:AVERAGE', 'PRINT:x:AVERAGE:%lf', LINE)
    run('def', 'graph', '/dev/null', *RANGE,
        'DEF:x=d.rrd:x:AVERAGE', 'PRINT:x:AVERAGE:%lf', LINE)


def cdef_cases() -> None:
    for form in ['CDEF:c=x,2,*', 'CDEF:c=x', 'CDEF:c=', 'CDEF:c',
                 'CDEF:=x,2,*', 'CDEF:c=x,+', 'CDEF:c=nope,2,*',
                 'CDEF:c=x,2,*,', 'CDEF:c=,x', 'CDEF:c=x,,2,+', 'CDEF:c=2',
                 'CDEF:c=x,BOGUS', 'CDEF:c=x 2 *', 'CDEF:c=x,2,*:extra',
                 'CDEF:c=v,2,*', 'CDEF:c=x,v,*', 'CDEF:c=x,y,+',
                 'CDEF:c=x,UN,0,x,IF', 'CDEF:c=c,2,*', 'CDEF:x=x,2,*',
                 'CDEF:1c=x,2,*', 'CDEF:c=x,' + ','.join(['1,+'] * 200)]:
        run('cdef', 'graph', '/dev/null', *RANGE, DEFX, DEFY,
            'VDEF:v=x,AVERAGE', form, 'PRINT:c:AVERAGE:%lf', 'LINE1:c#f00')


def vdef_cases() -> None:
    for form in ['VDEF:v=x,AVERAGE', 'VDEF:v=x,MAXIMUM', 'VDEF:v=x,MINIMUM',
                 'VDEF:v=x,LAST', 'VDEF:v=x,FIRST', 'VDEF:v=x,TOTAL',
                 'VDEF:v=x,STDEV', 'VDEF:v=x,LSLSLOPE', 'VDEF:v=x,LSLINT',
                 'VDEF:v=x,LSLCORREL', 'VDEF:v=x,95,PERCENT',
                 'VDEF:v=x,95,PERCENTNAN', 'VDEF:v=x,101,PERCENT',
                 'VDEF:v=x,-1,PERCENT', 'VDEF:v=x,abc,PERCENT',
                 'VDEF:v=x,PERCENT', 'VDEF:v=x,2,*', 'VDEF:v=x',
                 'VDEF:v=x,AVERAGE,2,*', 'VDEF:v=AVERAGE', 'VDEF:v=',
                 'VDEF:v', 'VDEF:v=nope,AVERAGE', 'VDEF:v=w,AVERAGE',
                 'VDEF:v=c,MAXIMUM', 'VDEF:v=x,average', 'VDEF:v=x,BOGUS',
                 'VDEF:v=x,AVERAGE:extra', 'VDEF:=x,AVERAGE',
                 'VDEF:v=u,AVERAGE', 'VDEF:v=u,MAXIMUM', 'VDEF:v=u,TOTAL',
                 'VDEF:v=u,95,PERCENT', 'VDEF:v=u,LSLSLOPE']:
        run('vdef', 'graph', '/dev/null', *RANGE, DEFX,
            'DEF:u=e.rrd:x:AVERAGE', 'VDEF:w=x,AVERAGE', 'CDEF:c=x,2,*',
            form, 'PRINT:v:%lf', 'PRINT:v:%Y-%m-%d %H\\:%M\\:%S:strftime')


COLORS = ['#ff0000', '#FF0000', '#f00', '#ff000080', '#ff00', '#ff00000',
          '#ff0000000', '#gg0000', '#', '', 'ff0000', '#ff0000#00ff00',
          '#-10000', '# ff0000', '#ff0000 ']


def line_cases() -> None:
    for kind in ['LINE', 'LINE1', 'LINE2', 'LINE0', 'LINE0.5', 'LINE1.5',
                 'LINE-1', 'LINEabc', 'LINE1e1', 'LINE 1', 'line1',
                 'LINE100', 'LINE1x']:
        run('line', *GV, f'{kind}:x#ff0000:leg')
    for color in COLORS:
        run('line', *GV, f'LINE1:x{color}:leg')
    for tail in ['', ':', ':leg', '::STACK', ':leg:STACK', ':leg:skipscale',
                 ':leg:STACK:skipscale', ':leg:skipscale:STACK',
                 ':leg:stack', ':leg:BOGUS', ':leg:dashes',
                 ':leg:dashes=5', ':leg:dashes=5,10',
                 ':leg:dashes=5,10,15', ':leg:dashes=5,10,15,20',
                 ':leg:dashes=0', ':leg:dashes=-1', ':leg:dashes=abc',
                 ':leg:dashes=', ':leg:dashes=5,', ':leg:dash-offset=3',
                 ':leg:dashes:dash-offset=3', ':leg:dashes:dash-offset=abc',
                 ':leg:dashes:dash-offset=-3', ':leg:dashes:dash-offset=',
                 ':leg:STACK:dashes=2,2:dash-offset=1',
                 ':leg:gradheight=10', ':leg:extra:more']:
        run('line', *GV, DEFY, 'LINE1:y#00ff00:first',
            f'LINE1:x#ff0000{tail}')
    for elements in [['LINE1:x'], ['LINE1:x:leg'], ['LINE1:x::STACK'],
                     ['LINE1:x#ff0000:leg', 'LINE1:x::STACK'],
                     ['LINE1:x::STACK'], ['LINE1:x#ff0000:leg:STACK'],
                     ['LINE1:nope#ff0000'], ['LINE1:#ff0000'], ['LINE1'],
                     ['LINE1:'], ['LINE1:3#ff0000:const'],
                     ['VDEF:v=x,AVERAGE', 'LINE1:v#ff0000:vdef'],
                     ['LINE1:x\\:y#ff0000'], ['LINE1:x#ff0000:a\\:b'],
                     ['LINE1:x#ff0000:leg\\'], ['LINE1:x#ff0000:\\:'],
                     [DEFY, 'LINE1:x#ff0000:a', 'LINE2:y#00ff00:b:STACK']]:
        run('line', *GV, *elements)


def area_cases() -> None:
    for tail in ['#ff0000', '#ff0000:leg', '#ff0000#0000ff:grad',
                 '#ff0000#0000ff:grad:gradheight=10',
                 '#ff0000#0000ff:grad:gradheight=-10',
                 '#ff0000#0000ff:grad:gradheight=0',
                 '#ff0000#0000ff:grad:gradheight=abc',
                 '#ff0000:leg:gradheight=10', '#ff0000:leg:STACK',
                 '#ff0000:leg:skipscale', '#ff0000:leg:dashes', '',
                 ':leg', '::STACK', '#ff0000#', '#ff0000#00ff', '#gg0000',
                 '#ff0000#0000ff#00ff00']:
        run('area', *GV, DEFY, 'AREA:y#00ff00:base', f'AREA:x{tail}')
    run('area', *GV, 'AREA:x#ff0000:first:STACK')
    run('area', *GV, DEFY, 'AREA:x#ff0000:a', 'STACK:y#00ff00:b')
    run('area', *GV, DEFY, 'STACK:y#00ff00:b')
    run('area', *GV, DEFY, 'LINE1:x#ff0000:a', 'STACK:y#00ff00')
    run('area', *GV, DEFY, 'LINE1:x#ff0000:a', 'STACK:y')
    run('area', *GV, DEFY, 'LINE1:x#ff0000:a', 'STACK:y#00ff00:b:c')
    run('area', *GV, DEFY, 'LINE1:x#ff0000:a', 'STACK:y:b')


def tick_cases() -> None:
    for form in ['TICK:x#ff0000', 'TICK:x#ff0000:0.5', 'TICK:x#ff0000:0.5:t',
                 'TICK:x#ff0000::t', 'TICK:x#ff0000:-0.5:t',
                 'TICK:x#ff0000:2:t', 'TICK:x#ff0000:0:t',
                 'TICK:x#ff0000:abc:t', 'TICK:x', 'TICK:x:0.5',
                 'TICK:x#ff000080:1', 'TICK:nope#ff0000', 'TICK:v#ff0000',
                 'TICK:x#ff0000:0.5:t:extra', 'TICK:3#ff0000']:
        run('tick', *GV, 'VDEF:v=x,AVERAGE', form)


def print_cases() -> None:
    for form in ['PRINT:v:%lf', 'PRINT:v:%6.2lf %s', 'PRINT:v:%lf:strftime',
                 'PRINT:v:%H\\:%M:strftime', 'PRINT:v:%lf:valstrftime',
                 'PRINT:v:%Y:valstrftime', 'PRINT:v:%H\\:%M:valstrfduration',
                 'PRINT:v:%lf:bogus', 'PRINT:v', 'PRINT:v:', 'PRINT:',
                 'PRINT', 'PRINT:x:%lf', 'PRINT:c:%lf', 'PRINT:x:AVERAGE:%lf',
                 'PRINT:x:MAX:%lf', 'PRINT:x:MIN:%lf', 'PRINT:x:LAST:%lf',
                 'PRINT:x:FIRST:%lf', 'PRINT:x:TOTAL:%lf',
                 'PRINT:x:BOGUS:%lf', 'PRINT:x:average:%lf',
                 'PRINT:c:AVERAGE:%lf', 'PRINT:v:AVERAGE:%lf',
                 'PRINT:nope:%lf', 'PRINT:x:AVERAGE:%lf:extra',
                 'PRINT:v:a\\:b %lf', 'PRINT:v:%lf\\g', 'PRINT:v:%lf\\l',
                 'PRINT:v:%lf\\n', 'PRINT:v:no format',
                 'PRINT:u:%lf', 'PRINT:u:%lf %s', 'PRINT:u:%lf:strftime',
                 'PRINT:x:AVERAGE:%lf:strftime']:
        # The deprecated form has no time of its own, so :strftime formats
        # the current time.
        clock = ['clock'] if form.startswith('PRINT:x:AVERAGE:%lf:s') \
            else None
        run('print', *G, 'CDEF:c=x,2,*', 'VDEF:v=x,MAXIMUM',
            'DEF:e=e.rrd:x:AVERAGE', 'VDEF:u=e,AVERAGE', form, LINE,
            tags=clock)
    for form in ['GPRINT:v:%lf', 'GPRINT:v:%6.2lf %s', 'GPRINT:v:%lf\\g',
                 'GPRINT:v:%lf:strftime', 'GPRINT:v:%H\\:%M:strftime',
                 'GPRINT:x:AVERAGE:%lf', 'GPRINT:x:%lf', 'GPRINT:v',
                 'GPRINT:v:', 'GPRINT:v:%d', 'GPRINT:x:BOGUS:%lf',
                 'GPRINT:v:%lf\\r', 'GPRINT:v:%lf\\c', 'GPRINT:u:%lf',
                 'GPRINT:v:%lf:valstrfduration']:
        run('gprint', *GV, 'VDEF:v=x,MAXIMUM', 'DEF:e=e.rrd:x:AVERAGE',
            'VDEF:u=e,AVERAGE', LINE, form)
    run('print', *G, 'VDEF:v=x,MAXIMUM', 'PRINT:v:%lf', '--imgformat',
        'CSV')
    run('print', 'graphv', '-', *RANGE, DEFX, 'VDEF:v=x,MAXIMUM',
        'PRINT:v:%lf', LINE)
    run('print', 'graph', '-', *RANGE, DEFX, 'VDEF:v=x,MAXIMUM',
        'PRINT:v:%lf', LINE)
    run('print', 'graph', '/dev/null', *RANGE, DEFX, 'VDEF:v=x,MAXIMUM',
        'PRINT:v:%lf', 'PRINT:v:%le', 'GPRINT:v:%lf', 'PRINT:v:%lg')
    run('print', 'graph', '/dev/null', *RANGE, '--utc', DEFX,
        'VDEF:v=x,MAXIMUM', 'PRINT:v:%H\\:%M %Z:strftime',
        env={'TZ': 'Asia/Kolkata'})
    run('print', 'graph', '/dev/null', *RANGE, DEFX, 'VDEF:v=x,MAXIMUM',
        'PRINT:v:%H\\:%M %Z:strftime', env={'TZ': 'Asia/Kolkata'})


def comment_cases() -> None:
    for form in ['COMMENT:hello', 'COMMENT:', 'COMMENT', 'COMMENT:a\\:b',
                 'COMMENT:a:b', 'COMMENT:a\\', 'COMMENT:\\n', 'COMMENT:\\s',
                 'COMMENT:\\u', 'COMMENT:x\\l', 'COMMENT:x\\r',
                 'COMMENT:x\\c', 'COMMENT:x\\j', 'COMMENT:x\\g',
                 'COMMENT:x\\.', 'COMMENT:x\\t', 'COMMENT:a\\tb',
                 'COMMENT:a\tb', 'COMMENT:%lf', 'COMMENT:' + 'c' * 300,
                 'COMMENT:' + 'c ' * 120, 'COMMENT:\xe9t\xe9',
                 'COMMENT:caf\u00e9', 'COMMENT:<b>bold</b>',
                 'COMMENT:a\\\\b', 'COMMENT:x\\q', 'COMMENT:\\']:
        run('comment', *GV, LINE, form)


def rule_cases() -> None:
    for form in ['HRULE:5#0000ff', 'HRULE:5#0000ff:five', 'HRULE:v#0000ff:v',
                 'HRULE:x#0000ff', 'HRULE:c#0000ff', 'HRULE:abc#0000ff',
                 'HRULE:5', 'HRULE:5:five', 'HRULE:500#0000ff:out',
                 'HRULE:-500#0000ff:out', 'HRULE:5#0000ff:five:dashes',
                 'HRULE:5#0000ff:five:dashes=3,1',
                 'HRULE:5#0000ff:five:dashes=3,1:dash-offset=2',
                 'HRULE:5#0000ff:five:dashes=0', 'HRULE:5#0000ff:five:STACK',
                 'HRULE:5#0000ff::dashes', 'HRULE:1e3#0000ff:sci',
                 'HRULE:5#0000ff:a\\:b', 'HRULE:nan#0000ff:nan',
                 'HRULE:inf#0000ff:inf', 'HRULE:5#gg0000:bad',
                 'HRULE:#0000ff', 'HRULE:', 'HRULE']:
        run('hrule', *GV, 'CDEF:c=x,2,*', 'VDEF:v=x,AVERAGE', LINE, form)
        run('hrule', *GV, 'CDEF:c=x,2,*', 'VDEF:v=x,AVERAGE', LINE, '-F',
            form)
    for form in ['VRULE:1000000300#0000ff', 'VRULE:1000000300#0000ff:t',
                 'VRULE:v#0000ff:v', 'VRULE:w#0000ff:w', 'VRULE:x#0000ff',
                 'VRULE:abc#0000ff', 'VRULE:1000000300', 'VRULE:0#0000ff:out',
                 'VRULE:2000000000#0000ff:out', 'VRULE:-1#0000ff:neg',
                 'VRULE:now#0000ff', 'VRULE:1000000300.5#0000ff:frac',
                 'VRULE:1000000300#0000ff:t:dashes=2,2:dash-offset=1',
                 'VRULE:1000000300#0000ff:t:dashes=-2',
                 'VRULE:1e9#0000ff:sci', 'VRULE:1000000000#0000ff:edge',
                 'VRULE:1000001200#0000ff:edge',
                 'VRULE:1000001201#0000ff:past']:
        run('vrule', *GV, 'VDEF:v=x,MAXIMUM', 'VDEF:w=x,AVERAGE', LINE, form)
        run('vrule', *GV, 'VDEF:v=x,MAXIMUM', 'VDEF:w=x,AVERAGE', LINE,
            '-F', form)


def shift_cases() -> None:
    for form in ['SHIFT:x:60', 'SHIFT:x:-60', 'SHIFT:x:0', 'SHIFT:x:600',
                 'SHIFT:x:v', 'SHIFT:x:w', 'SHIFT:x:abc', 'SHIFT:x:',
                 'SHIFT:x', 'SHIFT:nope:60', 'SHIFT:c:60', 'SHIFT:v:60',
                 'SHIFT:x:60.5', 'SHIFT:x:1e2', 'SHIFT:x:60:extra',
                 'SHIFT:x:30', 'SHIFT:x:86400000']:
        run('shift', *G, 'CDEF:c=x,2,*', 'VDEF:v=x,TOTAL',
            'VDEF:w=x,MAXIMUM', form, 'VDEF:m=x,MAXIMUM',
            'PRINT:m:%lf', 'PRINT:m:%s:strftime', 'VDEF:f=x,FIRST',
            'PRINT:f:%lf', 'PRINT:f:%s:strftime', 'PRINT:x:AVERAGE:%lf',
            LINE)
        run('shift', 'graph', '-', *RANGE, DEFX, 'CDEF:c=x,2,*',
            'VDEF:v=x,TOTAL', 'VDEF:w=x,MAXIMUM', form, LINE,
            '--imgformat', 'CSV')


def textalign_cases() -> None:
    for form in ['TEXTALIGN:left', 'TEXTALIGN:right', 'TEXTALIGN:center',
                 'TEXTALIGN:justified', 'TEXTALIGN:LEFT', 'TEXTALIGN:middle',
                 'TEXTALIGN:', 'TEXTALIGN', 'TEXTALIGN:left:x']:
        run('textalign', *GV, form, 'LINE1:x#ff0000:alpha',
            'COMMENT:beta', 'COMMENT:gamma\\n', 'COMMENT:delta')
    run('textalign', *GV, 'LINE1:x#ff0000:alpha', 'TEXTALIGN:right',
        'COMMENT:beta\\n', 'TEXTALIGN:center', 'COMMENT:gamma\\n',
        'COMMENT:delta')


def misc_element_cases() -> None:
    for form in ['FOO:x', 'foo', 'XPORT:x', 'XPORT:x:leg', 'line1:x#f00',
                 'Line1:x#f00', 'LINE1x#f00', ':x', 'GPRINT', 'STACK',
                 'AREA', 'TICK', 'SHIFT', 'VRULE', 'COMMENT',
                 'DEF:x=f.rrd:x:AVERAGE:step=60:start=1000000000']:
        run('element', *G, LINE, form)
    # Elements that refer to names defined later.
    run('element', *G, 'PRINT:v:%lf', 'VDEF:v=x,AVERAGE', LINE)
    run('element', *G, 'SHIFT:c:60', 'CDEF:c=x,2,*', LINE)
    run('element', 'xport', *RANGE, DEFX, 'XPORT:x:leg', 'LINE1:x#f00')
    run('element', 'xport', *RANGE, DEFX, 'XPORT:x:leg', 'PRINT:x:MAX:%lf')
    run('element', 'xport', *RANGE, DEFX, 'XPORT:x:leg', 'COMMENT:hi')


# ------------------------------------------------------------ layout

def layout_cases() -> None:
    base = ['LINE1:x#ff0000:alpha', 'GPRINT:x:AVERAGE:%6.2lf',
            'COMMENT:end\\n']
    for opts in [[], ['-w', '200'], ['-h', '200'], ['-w', '10', '-h', '10'],
                 ['-j'], ['-j', '-h', '20'], ['-j', '-h', '31'],
                 ['-j', '-h', '32'], ['-j', '-h', '100'],
                 ['-D', '-w', '400', '-h', '200'], ['-D', '-w', '100',
                                                    '-h', '60'],
                 ['-D', '-w', '50', '-h', '30'], ['-D', '-j', '-w', '300',
                                                  '-h', '100'],
                 ['-g'], ['-t', 'Title'], ['-t', 'One\\nTwo'],
                 ['-t', 'One<br/>Two'], ['-v', 'units'],
                 ['-v', 'units', '--vertical-label-angle', '0'],
                 ['--right-axis', '1:0'],
                 ['--right-axis', '1:0', '--right-axis-label', 'r'],
                 ['--right-axis-label', 'r'], ['-W', 'watermark'],
                 ['--border', '0'], ['--border', '5'], ['-m', '2'],
                 ['-m', '0.5'], ['-m', '1.5', '-w', '101'],
                 ['--legend-position', 'north'],
                 ['--legend-position', 'east'],
                 ['--legend-position', 'west'],
                 ['--legend-position', 'south'],
                 ['--legend-direction', 'bottomup'],
                 ['--legend-direction', 'bottomup2'],
                 ['-L', '2'], ['-L', '10'], ['-L', '0'], ['-L', '-5'],
                 ['-y', 'none'], ['-x', 'none'], ['-y', 'none', '-L', '6'],
                 ['--disable-rrdtool-tag'], ['-n', 'DEFAULT:12:'],
                 ['-n', 'LEGEND:12:'], ['-n', 'TITLE:20:', '-t', 'T'],
                 ['-n', 'AXIS:3:'], ['-n', 'UNIT:15:', '-v', 'u'],
                 ['-n', 'WATERMARK:15:', '-W', 'w'],
                 ['-n', 'DEFAULT:0:Courier'], ['-T', '10'], ['-T', '200'],
                 ['-P'], ['--dynamic-labels'], ['-X', '0'], ['-X', '3'],
                 ['-u', '1e6', '-r'], ['-o'], ['-b', '1024'], ['-Y'],
                 ['-A'], ['-E'], ['-N'], ['-G', 'mono'], ['-R', 'mono'],
                 ['-t', 'Title', '-v', 'units', '--right-axis', '1:0',
                  '--right-axis-label', 'r', '-W', 'w'],
                 ['-t', 'T', '-j'], ['-t', 'T', '-g'],
                 ['--legend-position', 'east', '-D', '-w', '600', '-h',
                  '150'],
                 ['--legend-position', 'north', '-t', 'T']]:
        run('layout', *GV, *opts, *base)
    for opts in [[], ['-j'], ['-g'], ['--legend-position', 'east'],
                 ['-D', '-w', '600', '-h', '300']]:
        run('layout', *GV, *opts)
        run('layout', *GV, *opts, 'COMMENT:only')
        run('layout', *GV, *opts, 'HRULE:5#0000ff:rule')
        run('layout', *GV, *opts, 'PRINT:x:MAX:%lf')


LEGENDS = [
    ['LINE1:x#ff0000:a'],
    ['LINE1:x#ff0000:a', 'LINE1:x#00ff00:b', 'LINE1:x#0000ff:c'],
    ['LINE1:x#ff0000:a\\l', 'LINE1:x#00ff00:b\\r', 'LINE1:x#0000ff:c\\c'],
    ['LINE1:x#ff0000:a\\j', 'LINE1:x#00ff00:b\\n', 'COMMENT:c\\j'],
    ['COMMENT:left\\l', 'COMMENT:\\u', 'COMMENT:right\\r'],
    ['COMMENT:left\\l', 'COMMENT:\\s', 'COMMENT:next\\l'],
    ['COMMENT:a\\g', 'COMMENT:b\\g', 'COMMENT:c'],
    ['COMMENT:a  \\g', 'COMMENT:b'],
    ['COMMENT:a\\t', 'COMMENT:b\\tc\\td'],
    ['COMMENT:a\tb\tc'],
    ['COMMENT:a\\.', 'COMMENT:b\\.\\l'],
    ['COMMENT:' + ' '.join(['word'] * 40)],
    ['COMMENT:' + 'w' * 120],
    ['LINE1:x#ff0000:' + 'x' * 30] * 6,
    ['LINE1:x#ff0000:alpha', 'LINE1:x#00ff00', 'LINE1:x#0000ff:gamma'],
    ['LINE1:x#ff0000:alpha', 'LINE1:x::STACK', 'LINE1:x#0000ff:gamma'],
    ['AREA:x#ff0000:area', 'TICK:x#00ff00:0.5:tick',
     'HRULE:5#0000ff:hrule', 'VRULE:1000000600#000000:vrule'],
    ['LINE1:x#ff0000:a', 'COMMENT:\\n', 'COMMENT:b'],
    ['COMMENT:\\n', 'COMMENT:\\n', 'COMMENT:x'],
    ['COMMENT:x\\l', 'COMMENT:\\l', 'COMMENT:y\\l'],
    ['COMMENT:', 'COMMENT:', 'COMMENT:x'],
    ['LINE1:x#ff0000:', 'COMMENT:x'],
    ['GPRINT:x:AVERAGE:avg %6.2lf%s', 'GPRINT:x:MAX:max %6.2lf%s\\g',
     'GPRINT:x:MIN:min %6.2lf%s\\l'],
    ['TEXTALIGN:right', 'COMMENT:a', 'COMMENT:b'],
    ['TEXTALIGN:center', 'COMMENT:a', 'COMMENT:b'],
    ['TEXTALIGN:left', 'COMMENT:a', 'COMMENT:b\\n', 'COMMENT:c'],
    ['TEXTALIGN:justified', 'COMMENT:a', 'COMMENT:b\\n', 'COMMENT:c'],
    ['COMMENT:a\\r', 'COMMENT:b\\l', 'COMMENT:c\\c', 'COMMENT:d\\j'],
    ['COMMENT:<b>bold</b>', 'COMMENT:&amp;'],
    ['COMMENT:\u00e9\u00e8\u00ea', 'COMMENT:\u65e5\u672c'],
    ['COMMENT:a', 'COMMENT:b', 'COMMENT:c'],
    ['COMMENT:a', 'COMMENT:b'],
    ['COMMENT:aaaa', 'COMMENT:b'],
    ['COMMENT:a', 'COMMENT:b', 'COMMENT:c', 'COMMENT:d'],
    ['COMMENT:a\\g', 'COMMENT:b', 'COMMENT:c'],
    ['COMMENT:a', 'COMMENT:b\\g', 'COMMENT:c'],
    ['COMMENT:a', 'COMMENT:b', 'COMMENT:c\\g'],
    ['COMMENT:a\\g', 'COMMENT:b\\g', 'COMMENT:c\\g'],
    ['COMMENT:a\\g', 'COMMENT:b\\l'],
    ['COMMENT:a', 'COMMENT:b\\g', 'COMMENT:c\\l'],
    ['COMMENT:iiii', 'COMMENT:WWWW', 'COMMENT:....\\l'],
    ['LINE1:x#ff0000:a\\g', 'LINE1:x#00ff00:b', 'LINE1:x#0000ff:c'],
    ['COMMENT:a\\r', 'COMMENT:b\\g', 'COMMENT:c\\r'],
    ['COMMENT:a\\g', 'COMMENT:b\\c'],
]


def legend_cases() -> None:
    for elements in LEGENDS:
        run('legend', *GV, LINE, *elements)
    for opts in [['-g'], ['--legend-position', 'east'],
                 ['--legend-position', 'west'],
                 ['--legend-position', 'north'],
                 ['--legend-direction', 'bottomup'],
                 ['--legend-direction', 'bottomup2'], ['-P'],
                 ['--dynamic-labels'], ['-w', '100'], ['-D', '-w', '200',
                                                      '-h', '200']]:
        for elements in [LEGENDS[1], LEGENDS[2], LEGENDS[13],
                         ['COMMENT:head\\l', 'LINE1:x#ff0000:a\\l',
                          'LINE1:x#00ff00:b\\l', 'COMMENT:foot\\l'],
                         ['COMMENT:<b>bold</b> x', 'LINE1:x#ff0000:a']]:
            run('legend', *GV, *opts, LINE, *elements)
    # Out-of-range rules and their legends.
    for opts in [[], ['-F']]:
        run('legend', *GV, *opts, LINE, 'HRULE:500#0000ff:high',
            'VRULE:900000000#0000ff:early', 'HRULE:5#0000ff:in')


def scale_cases() -> None:
    transforms = ['x', 'x,1000000,*', 'x,0.001,*', 'x,3,-', 'x,-1,*',
                  '0,x,*,7,+', '0,x,*', 'x,1e-9,*', 'x,1e12,*',
                  'x,UN,UNKN,x,IF,100,+', '0,x,*,UNKN,+', 'x,1,+,LOG',
                  'x,10,/,1,+', '0,x,*,INF,+', 'x,1024,*,1024,*']
    for expr in transforms:
        for opts in [[], ['-A'], ['-J'], ['-M'], ['-Y'], ['-o'],
                     ['-b', '1024'], ['-u', '1000', '-l', '-1000'],
                     ['-u', '1', '-l', '0', '-r'], ['-X', '3'],
                     ['--alt-y-mrtg']]:
            run('scale', *GV, f'CDEF:c={expr}', 'LINE1:c#ff0000', *opts)
    for opts in [['-u', '10'], ['-l', '-10'], ['-u', '10', '-l', '-10'],
                 ['-u', '2', '-l', '1', '-r'],
                 ['-u', '2', '-l', '1', '-r', '--allow-shrink'],
                 ['-u', '100', '-l', '0', '-r', '--allow-shrink'],
                 ['-u', '100', '-r'], ['-l', '-100', '-r'],
                 ['-u', '5', '-l', '10'], ['-u', '5', '-l', '5'],
                 ['-u', '5', '-l', '5', '-r'], ['-o', '-l', '0'],
                 ['-o', '-l', '-1'], ['-o', '-l', '1e-3'],
                 ['-o', '-u', '1e6'], ['-o', '--units=si'],
                 ['-A', '-u', '100'], ['-J', '-l', '-100'],
                 ['-M', '-u', '100'], ['-Y', '-u', '33'],
                 ['-X', '-3'], ['-X', '6'], ['-L', '3'],
                 ['-y', '2:1'], ['-y', '0.5:4'], ['-y', '1:1', '-u', '3']]:
        run('scale', *GV, DEFY, 'LINE1:y#ff0000', *opts)
    for elements in [['LINE1:x#ff0000:a', 'LINE1:y#00ff00:b:skipscale'],
                     ['AREA:x#ff0000:a', 'AREA:x#00ff00:b:STACK'],
                     ['LINE1:x#ff0000', 'LINE1:y#00ff00::STACK'],
                     ['LINE1:x', 'LINE1:y#00ff00::STACK'],
                     ['HRULE:500#0000ff', 'LINE1:x#ff0000'],
                     ['HRULE:5#0000ff'], ['VRULE:1000000300#0000ff'],
                     ['TICK:x#0000ff:1'], ['GPRINT:x:AVERAGE:%lf'],
                     ['LINE1:x#ff0000:a:skipscale'],
                     ['AREA:x#ff0000:a:skipscale', 'LINE1:y#00ff00']]:
        run('scale', *GV, DEFY, *elements)
    for opts in [[], ['-A'], ['-o'], ['-Y']]:
        run('scale', *DV, 'LINE1:a#ff0000', 'LINE1:b#0000ff', *opts)
        run('scale', *DV, 'CDEF:c=a,1000,*', 'LINE1:c#ff0000', *opts)
        run('scale', 'graphv', '/dev/null', *RANGE, 'DEF:e=e.rrd:x:AVERAGE',
            'LINE1:e#ff0000', *opts)


# ------------------------------------------------------- data formats

def data_cases() -> None:
    elements = [DEFY, 'CDEF:c=x,y,+', 'LINE1:x#ff0000:xx',
                'LINE1:y#00ff00:yy', 'AREA:c#0000ff', 'VDEF:v=x,MAXIMUM',
                'PRINT:v:%lf', 'GPRINT:v:%lf', 'COMMENT:hello']
    for fmt in ['XML', 'XMLENUM', 'JSON', 'JSONTIME', 'CSV', 'TSV', 'SSV']:
        run('data', 'graph', '-', *RANGE, DEFX, *elements, '-a', fmt)
        run('data', 'graphv', '-', *RANGE, DEFX, *elements, '-a', fmt)
        run('data', 'graph', '/dev/null', *RANGE, DEFX, *elements, '-a', fmt)
        written('data', ['graph', 'out.txt', *RANGE, DEFX, *elements,
                         '-a', fmt], 'out.txt')
        run('data', 'graph', '-', *RANGE, DEFX, *elements, '-a', fmt,
            '-x', 'MINUTE:1:MINUTE:5:MINUTE:5:0:%Y-%m-%d %H:%M:%S')
        run('data', 'graph', '-', *RANGE, DEFX, *elements, '-a', fmt, '-S',
            '300')
        written('data', ['graph', 'nodir/out.txt', *RANGE, DEFX, *elements,
                         '-a', fmt], 'nodir/out.txt')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#ff0000', '-a', 'CSV',
        '-x', 'none')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#ff0000:a,b"c',
        '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#ff0000:a<b>&c',
        '-a', 'XML')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#ff0000:a"b\\c',
        '-a', 'JSON')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x', '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'TICK:x#ff0000:0.5:t', '-a',
        'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'XPORT:x:xp', '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'PRINT:x:MAX:%lf', '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'SHIFT:x:120', 'LINE1:x#f00:s',
        '-a', 'CSV')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#f00:s', '-a', 'CSV',
        '-w', '5')
    run('data', 'graph', '-', *DRANGE, 'DEF:a=d.rrd:x:AVERAGE',
        'LINE1:a#f00:s', '-a', 'JSON', '-w', '10')
    run('data', 'graphv', '-', *RANGE, DEFX, 'LINE1:x#ff0000:xx',
        '--add-jsontime')
    run('data', 'graphv', '/dev/null', *RANGE, DEFX, 'LINE1:x#ff0000:xx',
        '--add-jsontime')
    run('data', 'graph', '-', *RANGE, DEFX, 'LINE1:x#ff0000:xx',
        '--add-jsontime')


def xport_cases() -> None:
    xp = ['xport'] + RANGE
    for opts in [[], ['--json'], ['-t'], ['--showtime'], ['--enumds'],
                 ['--json', '-t'], ['--json', '--enumds'], ['-m', '5'],
                 ['--maxrows', '5'], ['-m', '1'], ['-m', '0'], ['-m', '-1'],
                 ['-m', 'abc'], ['-m', '10000'], ['--step', '120'],
                 ['-S', '120'], ['--step', '0'], ['--step', 'abc'],
                 ['--step', '1'], ['--step', '600'], ['--bogus'], ['-w', '5'],
                 ['--width', '5'], ['-d', 'unix:/nonexistent/sock'],
                 ['--daemon', ''], ['-a', 'CSV'], ['--imgformat', 'JSON'],
                 ['--json=1'], ['-x'], ['--title', 'x'], ['-j'],
                 ['--utc'], ['-e', 'start+5min'], ['--end', T0]]:
        run('xport', *xp, *opts, DEFX, DEFY, 'CDEF:c=x,y,+', 'XPORT:x:xx',
            'XPORT:c:sum')
    for elements in [[DEFX], [DEFX, 'XPORT:x'], [DEFX, 'XPORT:x:'],
                     [DEFX, 'XPORT:x:a:b'], [DEFX, 'XPORT:x:a\\:b'],
                     [DEFX, 'XPORT:nope'], [DEFX, 'XPORT:'], [DEFX, 'XPORT'],
                     [DEFX, 'VDEF:v=x,AVERAGE', 'XPORT:v'],
                     [DEFX, 'XPORT:x:a<b>&"c'], [DEFX, 'XPORT:x:\xe9'],
                     ['XPORT:x', DEFX], [DEFX, 'XPORT:x', 'XPORT:x'],
                     [DEFX, 'DEF:z=d.rrd:x:AVERAGE', 'XPORT:x', 'XPORT:z'],
                     [DEFX, 'DEF:z=f.rrd:x:AVERAGE:step=300', 'XPORT:x',
                      'XPORT:z'],
                     [DEFX, 'SHIFT:x:60', 'XPORT:x'],
                     ['DEF:x=missing.rrd:x:AVERAGE', 'XPORT:x'],
                     [DEFX, 'CDEF:c=x,+', 'XPORT:c'],
                     ['DEF:x=e.rrd:x:AVERAGE', 'XPORT:x'],
                     [DEFX, 'FOO:x', 'XPORT:x'],
                     [DEFX] + [f'XPORT:x:c{i}' for i in range(12)]]:
        run('xport', *xp, *elements)
        run('xport', *xp, '--json', '-t', *elements)
    run('xport', 'xport')
    run('xport', 'xport', DEFX)
    run('xport', 'xport', *RANGE)
    run('xport', 'xport', *RANGE, 'XPORT:x')
    run('xport', 'xport', '-s', T0, '-e', T1, DEFX, 'XPORT:x', '-a', 'XML')


def output_cases() -> None:
    elements = [DEFX, LINE + ':x', 'VDEF:v=x,MAXIMUM', 'PRINT:v:%lf']
    written('out', ['graph', 'out.png', *RANGE, *elements], 'out.png')
    written('out', ['graphv', 'out.png', *RANGE, *elements], 'out.png')
    written('out', ['graph', 'sub/out.png', *RANGE, *elements],
            'sub/out.png')
    written('out', ['graph', 'nodir/out.png', *RANGE, *elements],
            'nodir/out.png')
    written('out', ['graph', 'sub', *RANGE, *elements], 'sub')
    written('out', ['graph', 'out.png', *RANGE, DEFX, 'FOO:x'], 'out.png')
    written('out', ['graph', 'out.png', *RANGE, DEFX,
                    'PRINT:x:MAX:%d'], 'out.png')
    written('out', ['graph', 'out.png', *RANGE, DEFX], 'out.png')
    written('out', ['graph', 'out.png', *RANGE, DEFX, 'PRINT:x:MAX:%lf'],
            'out.png')
    written('out', ['graph', 'out.png', *RANGE, '-w', '0', *elements],
            'out.png')
    written('out', ['graph', 'out.png', *RANGE, '-m', '2', *elements],
            'out.png')
    written('out', ['graph', 'out.png', *RANGE, '-j', '-h', '20',
                    *elements], 'out.png')
    written('out', ['graph', 'out.svg', *RANGE, '-a', 'SVG', *elements],
            'out.svg', tags=['rd-006'])
    run('out', 'graph', '-', *RANGE, *elements)
    run('out', 'graphv', '-', *RANGE, *elements)
    run('out', 'graph', '-', *RANGE, '-a', 'SVG', *elements,
        tags=['rd-006'])
    run('out', 'graph', '-', *RANGE, '-a', 'PDF', *elements,
        tags=['rd-006'])
    run('out', 'graph', '-', *RANGE, '-a', 'EPS', *elements,
        tags=['rd-006'])
    run('out', 'graphv', '-', *RANGE, '-a', 'SVG', *elements,
        tags=['rd-006'])
    run('out', 'graph', '-', *RANGE, '-m', '2', *elements)
    run('out', 'graph', '-', *RANGE, '-f', '%s %lu %lu', *elements)
    run('out', 'graph', 'out.png', *RANGE, '-f', '%s %lu %lu', *elements)
    run('out', 'graph', 'out.png', *RANGE, '-f',
        '<IMG SRC="%s" WIDTH="%lu" HEIGHT="%lu">', '-m', '2', *elements)
    run('out', 'graphv', 'out.png', *RANGE, '-f', '%s %lu %lu', *elements)
    run('out', 'graph', '/dev/null', *RANGE, '-f', '%s', '-a', 'CSV',
        *elements)
    # --lazy: an existing, newer file is left alone.
    case('out', {'args': ['graph', 'out.png', *RANGE, *elements]},
         {'args': ['graph', 'out.png', '-z', *RANGE, *elements],
          'inspect': ['out.png']})
    case('out', {'args': ['graph', 'out.png', *RANGE, '-w', '200',
                          *elements]},
         {'args': ['graph', 'out.png', '-z', *RANGE, *elements],
          'inspect': ['out.png']})
    case('out', {'args': ['graph', 'out.png', '-z', *RANGE, *elements],
                 'inspect': ['out.png']})
    case('out', {'args': ['graph', 'out.png', *RANGE, *elements]},
         {'args': ['graphv', 'out.png', '-z', *RANGE, *elements],
          'inspect': ['out.png']})
    case('out', {'args': ['graph', 'out.png', *RANGE, *elements]},
         {'args': ['graph', 'out.png', '-z', '-f', '%s %lu %lu', *RANGE,
                   *elements],
          'inspect': ['out.png']})
    case('out', {'args': ['graph', 'out.png', *RANGE, '-j', *elements]},
         {'args': ['graph', 'out.png', '-z', '-w', '200', *RANGE,
                   *elements],
          'inspect': ['out.png']})
    written('out', ['graph', 'notes.txt', '-z', *RANGE, *elements],
            'notes.txt')
    # An existing file is replaced.
    written('out', ['graph', 'notes.txt', *RANGE, *elements], 'notes.txt')


def limit_cases() -> None:
    elements = [DEFX, LINE]
    for opts in [['-w', '32686', '-h', '10'], ['-w', '32687', '-h', '10'],
                 ['-w', '32767', '-h', '10'], ['-w', '32768', '-h', '10'],
                 ['-D', '-w', '32767', '-h', '100'],
                 ['-D', '-w', '32768', '-h', '100'],
                 ['-D', '-w', '100', '-h', '32767'],
                 ['-j', '-w', '32767', '-h', '10'],
                 ['-j', '-w', '32768', '-h', '10'],
                 ['-h', '32000'], ['-m', '100'], ['-m', '68'], ['-m', '69'],
                 ['-D', '-w', '4096', '-h', '4096'],
                 ['-D', '-w', '4097', '-h', '4096'],
                 ['-D', '-w', '5000', '-h', '4000'],
                 ['-a', 'CSV', '-w', '100000']]:
        run('limit', 'graph', '/dev/null', *RANGE, *opts, *elements)
    for opts in [['-D', '-w', '4096', '-h', '4096'],
                 ['-D', '-w', '4097', '-h', '4096']]:
        run('limit', 'graphv', '/dev/null', *RANGE, *opts, *elements)
    # Collected fetches: 10,000,000 rows of a 60 s archive.
    for end in ['1600000000', '1600000060', '1600000120']:
        run('limit', 'graph', '/dev/null', '--start', T0, '--end', end,
            'DEF:x=f.rrd:x:AVERAGE:step=60', 'PRINT:x:AVERAGE:%lf')
        run('limit', 'xport', '--start', T0, '--end', end, '-m', '10',
            'DEF:x=f.rrd:x:AVERAGE:step=60', 'XPORT:x')


def all_cases() -> list[dict]:
    CASES.clear()
    option_cases()
    numeric_cases()
    value_cases()
    time_cases()
    def_cases()
    cdef_cases()
    vdef_cases()
    line_cases()
    area_cases()
    tick_cases()
    print_cases()
    comment_cases()
    rule_cases()
    shift_cases()
    textalign_cases()
    misc_element_cases()
    layout_cases()
    legend_cases()
    scale_cases()
    data_cases()
    xport_cases()
    output_cases()
    limit_cases()
    counters: dict[str, int] = {}
    for entry in CASES:
        n = counters.get(entry['group'], 0) + 1
        counters[entry['group']] = n
        entry['id'] = f'graph-{entry["group"]}-{n:03d}'
    return CASES


# ------------------------------------------------------------ execution

def image_tag(data: bytes) -> tuple[str, int] | None:
    """The placeholder for an image at the start of `data` and its length."""
    if data.startswith(PNG_MAGIC):
        width = int.from_bytes(data[16:20], 'big')
        height = int.from_bytes(data[20:24], 'big')
        end = data.find(b'IEND')
        length = end + 8 if end >= 0 else len(data)
        return f'<PNG {width}x{height}>', length
    if data.startswith(b'%PDF'):
        end = data.rfind(b'%%EOF')
        return '<PDF>', (end + 6 if end >= 0 else len(data))
    if data.startswith(b'%!PS'):
        end = data.rfind(b'%%EOF')
        return '<EPS>', (end + 6 if end >= 0 else len(data))
    if data.startswith(b'<?xml') and b'<svg' in data[:400]:
        end = data.find(b'</svg>')
        return '<SVG>', (end + 7 if end >= 0 else len(data))
    return None


def normalize_bytes(data: bytes, cwd: Path) -> str:
    """Replaces image bytes with placeholders, then normalizes the text."""
    out = bytearray()
    position = 0
    while position < len(data):
        found = None
        for magic in (PNG_MAGIC, b'%PDF', b'%!PS', b'<?xml'):
            index = data.find(magic, position)
            if index >= 0 and (found is None or index < found):
                head = data[index:index + 400]
                if magic == b'<?xml' and b'<svg' not in head:
                    continue
                found = index
        if found is None:
            out += data[position:]
            break
        tag = image_tag(data[found:])
        out += data[position:found]
        if tag is None:
            out += data[found:found + 1]
            position = found + 1
            continue
        out += tag[0].encode()
        position = found + tag[1]
        # The image ends the output of `graph -`; keep any text after it.
    text = out.decode('latin-1')
    text = BLOB.sub('BLOB_SIZE:*', text)
    paths = sorted({str(cwd), os.path.realpath(cwd)}, key=len, reverse=True)
    lines = []
    for line in text.split('\n'):
        position = line.find('Compiled ')
        if position >= 0:
            line = line[:position + len('Compiled ')]
        for path in paths:
            line = line.replace(path, '{CWD}')
        lines.append(line)
    return '\n'.join(lines)


def inspect(path: Path, cwd: Path) -> str:
    if path.is_dir():
        return '<dir>'
    if not path.exists():
        return '<missing>'
    return normalize_bytes(path.read_bytes(), cwd)


def run_case(binary: str, fixture: Path, entry: dict) -> list[dict]:
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / 'w'
        shutil.copytree(fixture, work, symlinks=True)
        env = {k: v for k, v in os.environ.items()
               if k not in ('RRDCACHED_ADDRESS', 'TZ', 'LANG', 'RRD_LOCKING',
                            'RRD_DEFAULT_FONT')
               and not k.startswith('LC_')}
        env.update(TZ='UTC', LC_ALL='C')
        env.update(entry.get('env', {}))
        results = []
        for step in entry['steps']:
            try:
                proc = subprocess.run(
                    [binary] + step['args'], cwd=work, env=env,
                    capture_output=True, timeout=120, stdin=subprocess.DEVNULL)
            except subprocess.TimeoutExpired:
                results.append({'status': None, 'timeout': True})
                break
            if proc.returncode < 0:
                results.append({'status': None, 'crash': True})
                break
            result = {'status': proc.returncode,
                      'stdout': normalize_bytes(proc.stdout, work),
                      'stderr': normalize_bytes(proc.stderr, work)}
            if 'inspect' in step:
                result['files'] = {name: inspect(work / name, work)
                                   for name in step['inspect']}
            results.append(result)
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
    keys = ['id', 'group', 'env', 'steps', 'tags', 'expect',
            'expect_platform']
    return {key: entry[key] for key in keys if key in entry}


def read_table(path: str) -> dict:
    with open(path, encoding='utf-8') as handle:
        return json.load(handle)


def write_text(path: str, text: str) -> None:
    with open(path, 'w', encoding='utf-8') as handle:
        handle.write(text)


def add_tags(entry: dict, result: list[dict]) -> None:
    """Tags a case whose reference result cannot be compared: a crash or
    timeout is `undefined`; Pango and GLib warnings carry a process id and
    a clock time, so such a case is `unstable`."""
    tags = entry['tags']
    if any(step.get('crash') or step.get('timeout') for step in result):
        if 'undefined' not in tags:
            tags.append('undefined')
    if any('(process:' in step.get('stderr', '') for step in result):
        if 'unstable' not in tags:
            tags.append('unstable')


def merge_platform(out: str, key: str, source: str) -> int:
    table = read_table(out)
    results = read_table(source)
    for entry in table['vectors']:
        entry.get('expect_platform', {}).pop(key, None)
        if entry['id'] in results:
            entry.setdefault('expect_platform', {})[key] = results[entry['id']]
            add_tags(entry, results[entry['id']])
        if entry.get('expect_platform') == {}:
            del entry['expect_platform']
    table['vectors'] = [ordered(entry) for entry in table['vectors']]
    write_text(out, render(table))
    print(f'{len(results)} {key} expectations merged')
    return 0


def compare(binary: str, fixture: Path, table: dict, check: bool,
            platform_out: str | None, only: str | None) -> int:
    results = {}
    diverged = 0
    for entry in table['vectors']:
        if 'unstable' in entry['tags']:
            continue
        if only and entry['group'] != only:
            continue
        got = run_case(binary, fixture, entry)
        if got != entry['expect']:
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
        root / 'crates/rondi-cli/tests/data/graph_spec_vectors.json'))
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
                binary = str(link)
            setup(binary, fixture, table['files'])
            return compare(binary, fixture, table, bool(options.check),
                           options.platform_out, options.only)

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
            add_tags(entry, entry['expect'])
            vectors.append(ordered(entry))
            if options.only:
                print(json.dumps(ordered(entry), ensure_ascii=False))
        if options.only:
            return 0
        table = {
            'comment': 'Generated by scripts/graph-spec-vectors.py from '
                       'RRDtool 1.11.0; see docs/compatibility/graph.md.',
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
