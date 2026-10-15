#!/usr/bin/env python3
"""Capture RPN compatibility vectors from a reference rrdtool binary.

Usage:
  scripts/rpn-vectors.py [--rrdtool PATH] [--out FILE]
  scripts/rpn-vectors.py --probe 'CDEF expression'   # print one xport result
  scripts/rpn-vectors.py --check PATH                # diff PATH against FILE
  scripts/rpn-vectors.py --rrdtool PATH --record-platform KEY --platform-out F
  scripts/rpn-vectors.py --merge-platform KEY F

The fixture commands and every case are listed here; the output file is the
frozen table that crates/rondi-cli/tests/rpn_spec_vectors.rs replays. Each
case runs in a fresh copy of the fixture directory with TZ (default UTC) and
LC_ALL=C. Regenerate only with RRDtool 1.11.0 and review the diff.

The table is captured on macOS arm64. Results that differ on another
platform are recorded per vector under expect_platform.<KEY>, where KEY is
linux-x86_64 or linux-aarch64: run --record-platform with that build of the
oracle (for example inside docker/Dockerfile.test), then --merge-platform.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

BASE = 1_000_000_000
HOUR0 = 1_609_027_200  # 2020-12-27 00:00:00 UTC, a Sunday


# Edge values stored in d and e, cycling every 12 rows; e is a permutation so
# that d,e pairs differ row by row.
SPECIAL = ['inf', '-inf', 'U', '0', '-2.5', '1', '3', '0.5', '1e-300',
           '1e300', '-7', '2']


def fixture_commands() -> list[list[str]]:
    """Deterministic create and update commands for the fixture files."""
    commands: list[list[str]] = []
    commands.append([
        'create', 'f.rrd', '--start', str(BASE), '--step', '10',
        'DS:a:GAUGE:30:U:U', 'DS:b:GAUGE:30:U:U', 'DS:c:GAUGE:30:U:U',
        'DS:d:GAUGE:30:U:U', 'DS:e:GAUGE:30:U:U', 'RRA:AVERAGE:0.5:1:400',
    ])
    update = ['update', 'f.rrd']
    for k in range(1, 301):
        a = ((k * 37) % 23 - 7) + (k % 4) * 0.25
        b = 'U' if k % 5 == 0 else str(k % 7 - 3)
        c = str(k)
        d = SPECIAL[k % 12]
        e = SPECIAL[(k * 7 + 5) % 12]
        update.append(f'{BASE + 10 * k}:{a:g}:{b}:{c}:{d}:{e}')
    commands.append(update)
    commands.append([
        'create', 'g.rrd', '--start', str(BASE), '--step', '30',
        'DS:z:GAUGE:90:U:U', 'RRA:AVERAGE:0.5:1:200',
    ])
    update = ['update', 'g.rrd']
    for k in range(1, 101):
        z = 'U' if k % 6 == 0 else f'{(k * 13) % 11 - 3}.25'
        update.append(f'{BASE + 30 * k}:{z}')
    commands.append(update)
    commands.append([
        'create', 'h.rrd', '--start', str(HOUR0), '--step', '3600',
        'DS:h:GAUGE:7200:U:U', 'RRA:AVERAGE:0.5:1:3000',
        'RRA:AVERAGE:0.5:6:600',
    ])
    update = ['update', 'h.rrd']
    for k in range(1, 2401):
        update.append(f'{HOUR0 + 3600 * k}:{k % 10}')
    commands.append(update)
    return commands


F_DEFS = [
    'DEF:a=f.rrd:a:AVERAGE',
    'DEF:b=f.rrd:b:AVERAGE',
    'DEF:c=f.rrd:c:AVERAGE',
    'DEF:d=f.rrd:d:AVERAGE',
    'DEF:e=f.rrd:e:AVERAGE',
]
F_WINDOW = ['--start', str(BASE + 2000), '--end', str(BASE + 2120)]


def xport(expr: str, window: list[str] | None = None,
          defs: list[str] | None = None, extra: list[str] | None = None,
          export: str = 'r') -> list[str]:
    args = ['xport'] + (window if window is not None else F_WINDOW)
    args += defs if defs is not None else F_DEFS
    args += extra or []
    if expr is not None:
        args.append(f'CDEF:r={expr}')
    args.append(f'XPORT:{export}:{export}')
    return args


def graph(elements: list[str], window: list[str] | None = None,
          defs: list[str] | None = None) -> list[str]:
    args = ['graphv', '-'] + (window if window is not None else F_WINDOW)
    args += defs if defs is not None else F_DEFS
    args += ['LINE1:a#ff0000'] + elements
    return args


def h_window(start: int, hours: int, step: int | None = None) -> list[str]:
    window = ['--start', str(start), '--end', str(start + 3600 * hours)]
    if step is not None:
        window += ['--step', str(step)]
    return window


H_DEFS = ['DEF:h=h.rrd:h:AVERAGE']
G_DEFS = F_DEFS + ['DEF:z=g.rrd:z:AVERAGE']


ONE = ['--start', str(BASE + 2000), '--end', str(BASE + 2010)]
THREE = ['--start', str(BASE + 2000), '--end', str(BASE + 2030)]
# Calendar instants inside h.rrd.
NEW_YEAR_UTC = 1_609_459_200   # 2021-01-01 00:00 UTC, a Friday
SUNDAY_UTC = 1_609_632_000     # 2021-01-03 00:00 UTC
US_DST = 1_615_705_200         # 2021-03-14 07:00 UTC, New York springs forward
EU_DST = 1_616_893_200         # 2021-03-28 01:00 UTC, Berlin springs forward
MONTH_UTC = 1_614_556_800      # 2021-03-01 00:00 UTC


def dump(depth: int) -> str:
    """Suffix folding a stack of small integers into one decimal number:
    a stack s1..sn (bottom to top) becomes s1*10^(n-1) + ... + sn."""
    return ''.join(f',EXC,{10 ** k},*,+' for k in range(1, depth))


def cases() -> list[dict]:
    """Every vector: id, operator under test, class, argv, env, tags."""
    out: list[dict] = []

    def add(op: str, cls: str, args: list[str], env: dict | None = None,
            tags: list[str] | None = None) -> None:
        index = sum(1 for case in out if case['op'] == op) + 1
        out.append({
            'id': f'{op.lower()}-{index:02d}',
            'op': op,
            'class': cls,
            'args': args,
            'env': env or {},
            'tags': tags or [],
        })

    def x(op: str, cls: str, expr: str, window: list[str] | None = None,
          defs: list[str] | None = None, env: dict | None = None,
          tags: list[str] | None = None,
          extra: list[str] | None = None) -> None:
        add(op, cls, xport(expr, window=window, defs=defs, extra=extra),
            env, tags)

    def k(op: str, cls: str, expr: str, **kw) -> None:
        """A constant expression evaluated on a one-row window."""
        x(op, cls, 'a,POP,' + expr, window=kw.pop('window', ONE), **kw)

    # Grammar
    for expr in ['a,1,+', 'a,1.5,+', 'a,.5,+', 'a,5.,+', 'a,1e3,+',
                 'a,1e+3,+', 'a,1e-3,+', 'a,+1,+', 'a,-1,+', 'a,1E3,+',
                 'a,1e1100,+', 'a,-1e1100,+', 'a,1e-1100,+', 'a,1..2,+',
                 'a,1-2,+', 'a,--1,+', 'a,1e,+', 'a,e5,+', 'a,0x10,+',
                 'a,inf,+', 'a,nan,+', 'a,00012,+', 'a,1.5e2.5,+',
                 'a,' + '1' * 40 + ',+', 'a,' + '1' * 41 + ',+',
                 'a,' + '0' * 39 + '1,+', 'a,0.' + '0' * 36 + '1,+',
                 'a,-,+', 'a,.,+', 'a,+,+']:
        x('NUMBER', 'boundary', expr, window=THREE)
    for expr in ['a,POP,5', 'a,1,2', 'a,', ',a', 'a,,1,+', 'a, 1,+', ' a',
                 'a ', 'a,1,+ ', 'a,1,add', 'a,1,+x', 'a,DUPX,+', 'a,dup,+',
                 'a,1,+,', '', 'a;1;+', 'a 1 +', 'a,UNKNOWN',
                 'a,PREV(nope),+', 'a,PREV(a,+', 'a,PREV(),+',
                 'a,PREV( a),+', 'a,PREV(a)x,+', 'a,COUNT(a),+', 'A', 'aa',
                 'a,TRENDNANX', 'a,INFX,+']:
        x('PARSE', 'error', expr, window=THREE)
    for expr in ['5', '1,2,+', 'NOW', 'TIME', 'COUNT', 'UNKN', 'PREV',
                 'STEPWIDTH', 'NEWDAY', 'INF']:
        x('CONSTCDEF', 'error', expr, window=THREE)
    # Variable names that look like numbers or operators.
    x('NAMES', 'boundary', 'e,1,+', window=THREE)
    for name in ['x1', 'a-b', '1e5', '12', 'MAXX', 'MAX', 'PREV', 'e1']:
        x('NAMES', 'boundary', f'{name},2,*', window=THREE,
          defs=[f'DEF:{name}=f.rrd:a:AVERAGE'])
    x('NAMES', 'boundary', 'later,1,+', window=THREE,
      extra=['CDEF:later=a,2,*'])
    add('NAMES', 'error', ['xport'] + THREE + F_DEFS
        + ['CDEF:r=later,1,+', 'CDEF:later=a,2,*', 'XPORT:r:r'])
    x('NAMES', 'normal', 'a,b,c,d,e,+,+,+,+')
    x('NAMES', 'normal', 'q,1,+', window=THREE, extra=['CDEF:q=a,b,+'])

    # Final stack and underflow
    for expr in ['a,POP', 'a,a', 'a,DUP', 'a,1,2,+', 'a,DEPTH']:
        x('FINALSTACK', 'error', expr, window=THREE)
    unary = ['UN', 'ISINF', 'SIN', 'COS', 'LOG', 'EXP', 'SQRT', 'ATAN',
             'FLOOR', 'CEIL', 'ROUND', 'DEG2RAD', 'RAD2DEG', 'ABS', 'DUP',
             'POP', 'SORT', 'REV', 'AVG', 'SMIN', 'SMAX', 'MEDIAN', 'STDEV',
             'INDEX']
    binary = ['+', '-', '*', '/', '%', 'ADDNAN', 'POW', 'ATAN2', 'LT', 'LE',
              'GT', 'GE', 'EQ', 'NE', 'MIN', 'MAX', 'MINNAN', 'MAXNAN',
              'EXC', 'ROLL', 'PERCENT']
    ternary = ['IF', 'LIMIT', 'PREDICT', 'PREDICTSIGMA']
    for op in unary:
        x('UNDERFLOW', 'error', f'a,POP,{op}', window=ONE)
    for op in binary:
        x('UNDERFLOW', 'error', f'a,POP,1,{op}', window=ONE)
    for op in ternary:
        x('UNDERFLOW', 'error', f'a,POP,1,2,{op}', window=ONE)
    x('UNDERFLOW', 'error', 'a,POP,1,2,3,PREDICTPERC', window=ONE)
    # A deep stack: no fixed limit is observable.
    x('DEEPSTACK', 'boundary', 'a' + ',DUP' * 3000 + ',+' * 3000,
      window=THREE)
    x('DEEPSTACK', 'boundary',
      'a,' + ','.join(['1'] * 2500) + ',2500,AVG,+', window=THREE)

    # Arithmetic
    names = {'+': 'ADD', '-': 'SUB', '*': 'MUL', '/': 'DIV', '%': 'MOD'}
    for op in ['+', '-', '*', '/', '%', 'ADDNAN', 'POW', 'ATAN2']:
        label = names.get(op, op)
        x(label, 'normal', f'a,b,{op}')
        x(label, 'boundary', f'd,e,{op}')
        x(label, 'boundary', f'e,d,{op}')
        x(label, 'boundary', f'd,d,{op}')
        x(label, 'boundary', f'b,d,{op}')
    for expr in ['5,3,%', '-5,3,%', '5,-3,%', '5.5,2,%', '-5.5,2,%',
                 '5,0,%', '0,5,%', '1e300,1e-300,%', 'INF,1,%', '1,INF,%']:
        k('MOD', 'boundary', expr)
    for expr in ['0,0,/', '1,0,/', '-1,0,/', '1,0,-1,*,/',
                 '0,-1,*,1,EXC,/', 'INF,INF,-', 'INF,0,*', 'NEGINF,INF,+',
                 '1e308,10,*', '-1e308,10,*', '1e-308,1e-10,*',
                 '1e-320,2,/']:
        k('ARITHEDGE', 'boundary', expr)
    for expr in ['0,0,POW', 'UNKN,0,POW', '1,UNKN,POW',
                 '-8,0.3333333333,POW', '-8,3,POW', '2,-1,POW', '0,-1,POW',
                 'INF,0,POW', '2,0.5,POW', '-1,INF,POW', '0.5,NEGINF,POW',
                 '10,309,POW', '-2,0.5,POW', 'NEGINF,3,POW',
                 'NEGINF,2,POW']:
        k('POW', 'boundary', expr)
    for expr in ['0,0,ATAN2', '1,0,ATAN2', '0,-1,ATAN2', '0,-1,*,-1,ATAN2',
                 'INF,INF,ATAN2', 'UNKN,1,ATAN2', '-1,-1,ATAN2']:
        k('ATAN2', 'boundary', expr)
    for expr in ['UNKN,1,ADDNAN', '1,UNKN,ADDNAN', 'UNKN,UNKN,ADDNAN',
                 'INF,NEGINF,ADDNAN', 'NEGINF,UNKN,ADDNAN']:
        k('ADDNAN', 'boundary', expr)
    for op in ['SIN', 'COS', 'LOG', 'EXP', 'SQRT', 'ATAN', 'FLOOR', 'CEIL',
               'ROUND', 'DEG2RAD', 'RAD2DEG', 'ABS']:
        x(op, 'normal', f'a,{op}')
        x(op, 'boundary', f'd,{op}')
        x(op, 'boundary', f'e,{op}')
    for expr in ['0.5,ROUND', '1.5,ROUND', '2.5,ROUND', '-0.5,ROUND',
                 '-1.5,ROUND', '0.49999999999999994,ROUND',
                 '-0.4,ROUND,1,EXC,/', '-0.5,CEIL,1,EXC,/', '0.5,FLOOR',
                 '-0.5,FLOOR', '4503599627370497,ROUND', '1e300,FLOOR']:
        k('ROUNDING', 'boundary', expr)
    for expr in ['0,LOG', '-1,LOG', '1,LOG', '710,EXP', '-750,EXP',
                 '-1,SQRT', '0,-1,*,SQRT,1,EXC,/', '0,SQRT', '1e308,SIN',
                 'INF,SIN', 'INF,COS', 'INF,ATAN', 'NEGINF,ATAN',
                 'NEGINF,EXP', '0,-1,*,ABS,1,EXC,/', 'NEGINF,ABS',
                 '1,RAD2DEG', '1,DEG2RAD', '180,DEG2RAD',
                 '3.141592653589793,RAD2DEG', '90,DEG2RAD,SIN',
                 '0,-1,*,1,EXC,/']:
        k('UNARYEDGE', 'boundary', expr)

    # Comparison and logic
    for op in ['LT', 'LE', 'GT', 'GE', 'EQ', 'NE']:
        x(op, 'normal', f'a,b,{op}')
        x(op, 'boundary', f'd,e,{op}')
        x(op, 'boundary', f'e,d,{op}')
        x(op, 'boundary', f'd,d,{op}')
        k(op, 'boundary', f'0,0,-1,*,{op}')
        k(op, 'boundary', f'UNKN,UNKN,{op}')
    for op in ['MIN', 'MAX', 'MINNAN', 'MAXNAN']:
        x(op, 'normal', f'a,c,{op}')
        x(op, 'boundary', f'd,e,{op}')
        x(op, 'boundary', f'e,d,{op}')
        x(op, 'boundary', f'b,a,{op}')
        x(op, 'boundary', f'a,b,{op}')
        k(op, 'boundary', f'UNKN,UNKN,{op}')
        k(op, 'boundary', f'0,0,-1,*,{op},1,EXC,/')
        k(op, 'boundary', f'0,-1,*,0,{op},1,EXC,/')
    for expr in ['a,-5,5,LIMIT', 'a,5,-5,LIMIT', 'd,-3,5,LIMIT',
                 'd,NEGINF,INF,LIMIT', 'a,d,e,LIMIT', 'a,e,d,LIMIT',
                 'b,-2,2,LIMIT', 'a,UNKN,5,LIMIT', 'a,-5,UNKN,LIMIT',
                 'a,a,a,LIMIT', 'c,201,205,LIMIT']:
        x('LIMIT', 'normal' if expr == 'a,-5,5,LIMIT' else 'boundary', expr)
    for expr in ['b,10,20,IF', 'd,10,20,IF', 'e,d,a,IF', 'a,0,GT,a,UNKN,IF',
                 'b,UN,0,b,IF', 'c,2,%,c,0,-1,*,IF']:
        x('IF', 'normal', expr)
    for expr in ['0,-1,*,1,2,IF', 'INF,1,2,IF', 'NEGINF,1,2,IF',
                 'UNKN,1,2,IF', '1e-300,1,2,IF', '1,UNKN,2,IF']:
        k('IF', 'boundary', expr)
    for op in ['UN', 'ISINF']:
        x(op, 'normal', f'a,{op}')
        x(op, 'boundary', f'b,{op}')
        x(op, 'boundary', f'd,{op}')
        x(op, 'boundary', f'e,{op}')
        k(op, 'boundary', f'0,0,/,{op}')

    # Special values and row context
    for expr in ['UNKN', 'INF', 'NEGINF', 'UNKN,UN', 'INF,ISINF',
                 'NEGINF,ISINF', 'INF,NEGINF,+', 'INF,0,GT',
                 'NEGINF,INF,LT']:
        k('SPECIAL', 'normal', expr)
    x('COUNT', 'normal', 'a,POP,COUNT')
    x('COUNT', 'normal', 'a,POP,COUNT,COUNT,*')
    x('COUNT', 'boundary', 'h,POP,COUNT', window=h_window(NEW_YEAR_UTC, 5),
      defs=H_DEFS)
    early = ['DEF:a=f.rrd:a:AVERAGE:start=' + str(BASE + 1900)]
    x('COUNT', 'boundary', 'a,POP,COUNT', window=THREE, defs=early)
    x('PREV', 'boundary', 'a,POP,PREV,UN,0,PREV,IF,1,+', window=THREE,
      defs=early)
    x('PREV', 'normal', 'a,PREV,ADDNAN')
    x('PREV', 'normal', 'PREV,1,+,a,POP')
    x('PREV', 'boundary', 'PREV,UN,0,PREV,IF,1,+,a,POP')
    x('PREV', 'boundary', 'b,PREV,ADDNAN')
    x('PREV', 'boundary', 'a,PREV,PREV,+,+')
    x('PREVNAME', 'normal', 'PREV(a)')
    x('PREVNAME', 'normal', 'a,PREV(a),-')
    x('PREVNAME', 'normal', 'PREV(b),b,ADDNAN')
    x('PREVNAME', 'boundary', 'a,PREV(q),+', extra=['CDEF:q=c,2,*'])
    x('PREVNAME', 'boundary', 'PREV(a),PREV(a),+')
    x('PREVNAME', 'boundary', 'PREV(a),a,POP', window=ONE)
    x('STEPWIDTH', 'normal', 'a,POP,STEPWIDTH')
    x('STEPWIDTH', 'normal', 'z,POP,STEPWIDTH', window=THREE,
      defs=['DEF:z=g.rrd:z:AVERAGE'])
    x('STEPWIDTH', 'normal', 'h,POP,STEPWIDTH',
      window=h_window(NEW_YEAR_UTC, 3), defs=H_DEFS)
    x('STEPWIDTH', 'boundary', 'h,POP,STEPWIDTH',
      window=h_window(NEW_YEAR_UTC, 24, 21600), defs=H_DEFS)
    x('STEPWIDTH', 'boundary', 'a,z,POP,POP,STEPWIDTH', window=THREE,
      defs=G_DEFS)
    x('TIME', 'normal', 'a,POP,TIME')
    x('TIME', 'normal', 'a,POP,TIME,PREV,-')
    x('TIME', 'normal', 'h,POP,TIME', window=h_window(NEW_YEAR_UTC, 3),
      defs=H_DEFS)
    x('NOW', 'normal', 'a,POP,NOW,TIME,GT', window=THREE)
    x('NOW', 'normal', 'a,POP,NOW,1e9,GT,NOW,1e11,LT,*', window=THREE)
    x('NOW', 'boundary', 'a,POP,NOW,DUP,-', window=THREE)
    zones = ['UTC', 'America/New_York', 'Europe/Berlin', 'Asia/Kolkata',
             'Pacific/Chatham', 'Australia/Lord_Howe', 'America/St_Johns',
             'Etc/GMT+12', 'Etc/GMT-14']
    for zone in zones:
        x('LTIME', 'normal', 'h,POP,LTIME,TIME,-',
          window=h_window(NEW_YEAR_UTC - 7200, 8), defs=H_DEFS,
          env={'TZ': zone})
    for zone in ['America/New_York', 'Pacific/Honolulu', 'Asia/Tokyo']:
        x('LTIME', 'boundary', 'h,POP,LTIME,TIME,-',
          window=h_window(NEW_YEAR_UTC - 3600, 14), defs=H_DEFS,
          env={'TZ': zone})
    x('LTIME', 'boundary', 'h,POP,LTIME,TIME,-',
      window=h_window(US_DST - 4 * 3600, 8), defs=H_DEFS,
      env={'TZ': 'America/New_York'})
    x('LTIME', 'boundary', 'h,POP,LTIME,TIME,-',
      window=h_window(EU_DST - 3 * 3600, 6), defs=H_DEFS,
      env={'TZ': 'Europe/Berlin'})
    x('LTIME', 'normal', 'h,POP,LTIME', window=h_window(NEW_YEAR_UTC, 2),
      defs=H_DEFS, env={'TZ': 'Asia/Kolkata'})
    x('LTIME', 'boundary', 'a,POP,LTIME,TIME,-', window=THREE,
      env={'TZ': 'Not/A_Zone'})
    for op in ['NEWDAY', 'NEWMONTH', 'NEWYEAR']:
        for zone in ['UTC', 'America/New_York', 'Asia/Kolkata']:
            x(op, 'normal', f'h,POP,{op}',
              window=h_window(NEW_YEAR_UTC - 3 * 3600, 30), defs=H_DEFS,
              env={'TZ': zone})
        for zone in ['UTC', 'Europe/Berlin']:
            x(op, 'boundary', f'h,POP,{op}',
              window=h_window(NEW_YEAR_UTC - 2 * 86400, 24 * 4, 21600),
              defs=H_DEFS, env={'TZ': zone})
    x('NEWMONTH', 'normal', 'h,POP,NEWMONTH',
      window=h_window(MONTH_UTC - 3 * 3600, 6), defs=H_DEFS)
    x('NEWDAY', 'boundary', 'h,POP,NEWDAY',
      window=h_window(US_DST - 10 * 3600, 30), defs=H_DEFS,
      env={'TZ': 'America/New_York'})
    x('NEWDAY', 'boundary', 'a,POP,NEWDAY', window=THREE)
    for zone in ['UTC', 'America/New_York', 'Europe/Berlin']:
        x('NEWWEEK', 'normal', 'h,POP,NEWWEEK',
          window=h_window(SUNDAY_UTC - 2 * 86400, 24 * 4, 21600),
          defs=H_DEFS, env={'TZ': zone}, tags=['first-weekday'])
    x('NEWWEEK', 'boundary', 'h,POP,NEWWEEK',
      window=h_window(SUNDAY_UTC - 3 * 3600, 30), defs=H_DEFS,
      tags=['first-weekday'])
    x('NEWWEEK', 'boundary', 'h,POP,NEWWEEK',
      window=h_window(SUNDAY_UTC - 3 * 3600, 30), defs=H_DEFS,
      env={'LC_ALL': 'en_GB.UTF-8'}, tags=['first-weekday'])

    # Stack manipulation
    for expr in ['a,DUP,*', 'a,b,EXC,-', 'a,b,POP', 'a,b,DEPTH,+,+',
                 'a,POP,DEPTH', 'a,POP,1,2,3,DEPTH' + dump(4)]:
        x('STACKOPS', 'normal', expr, window=THREE)
    for expr, depth in [('1,2,3,2,COPY', 5), ('1,2,3,3,COPY', 6),
                        ('1,2,3,0,COPY', 3), ('1,2,3,1.9,COPY', 4),
                        ('1,2,3,4,COPY', 3), ('1,2,3,4,1,INDEX', 5),
                        ('1,2,3,4,3,INDEX', 5), ('1,2,3,4,4,INDEX', 5),
                        ('1,2,3,4,0,INDEX', 5), ('1,2,3,4,2.9,INDEX', 5),
                        ('1,2,3,4,5,INDEX', 5)]:
        op = 'COPY' if 'COPY' in expr else 'INDEX'
        cls = 'error' if expr in ('1,2,3,4,COPY', '1,2,3,4,5,INDEX') \
            else 'normal'
        k(op, cls, expr + dump(depth))
    k('COPY', 'error', '1,-1,COPY', tags=['undefined'])
    # ROLL windows that reach only the stack top and the two parameter
    # slots; see the specification for why larger windows are excluded.
    for n, m in [(0, 1), (1, 1), (1, -3), (2, 1), (2, 0), (2, -1), (2, 2),
                 (2, 3), (3, 1), (3, -1), (3, 0), (3, 2), (3, 4), (3, -4),
                 (2.9, 1), (3, 1.9), (3, -1.9)]:
        k('ROLL', 'boundary', f'1,2,3,4,5,6,{n},{m},ROLL' + dump(6))
    for n, m in [(-2, 1), (7, 1)]:
        k('ROLL', 'error', f'1,2,3,4,5,6,{n},{m},ROLL' + dump(6))

    # Set operations
    for op in ['SORT', 'REV']:
        k(op, 'normal', f'4,3,22.1,1,4,{op}' + dump(4))
        k(op, 'normal', f'4,3,22.1,1,3,{op}' + dump(4))
        k(op, 'normal', f'4,3,2,1,2.9,{op}' + dump(4))
        k(op, 'boundary', f'4,3,2,1,1,{op}' + dump(4))
        k(op, 'boundary', f'4,3,2,1,0,{op}' + dump(4))
        # A negative SORT count crashes the glibc build of the reference.
        k(op, 'boundary', f'4,3,2,1,-1,{op}' + dump(4),
          tags=['undefined'] if op == 'SORT' else [])
        k(op, 'error', f'4,3,2,1,5,{op}' + dump(4))
        x(op, 'normal', f'a,b,c,3,{op},+,+')
    for slot in ['POP,POP', 'POP,EXC,POP', 'EXC,POP,EXC,POP']:
        x('SORT', 'boundary', f'a,b,c,3,SORT,{slot}')
        x('SORT', 'boundary', f'a,d,e,3,SORT,{slot}', tags=['host-sort'])
    for pair in ['UNKN,1', '1,UNKN', 'UNKN,UNKN', 'NEGINF,-1', '-1,NEGINF',
                 'INF,1', '1,INF', 'INF,NEGINF', 'NEGINF,INF', 'INF,UNKN',
                 'UNKN,NEGINF', '2,1', '1,2', '1,1']:
        k('SORT', 'boundary', f'{pair},2,SORT,POP', tags=['host-sort'])
    for op in ['AVG', 'SMIN', 'SMAX', 'MEDIAN', 'STDEV']:
        tags = ['fma'] if op == 'STDEV' else []
        sort_tags = ['host-sort'] if op == 'MEDIAN' else []
        x(op, 'normal', f'a,b,c,3,{op}', tags=tags)
        x(op, 'normal', f'a,b,c,d,e,5,{op}', tags=tags + sort_tags)
        x(op, 'boundary', f'b,1,{op}', tags=tags)
        x(op, 'boundary', f'a,d,e,3,{op}', tags=tags + sort_tags)
        x(op, 'boundary', f'b,b,2,{op}', tags=tags)
        k(op, 'boundary', f'0,{op}')
        k(op, 'boundary', f'UNKN,UNKN,2,{op}')
        k(op, 'boundary', f'1,2,3,4,2.9,{op},+,+')
        k(op, 'error', f'1,2,3,{op}')
        k(op, 'boundary', f'7,1,2,3,0,{op},+,+,+')
        k(op, 'error', f'1,2,4,{op}')
        k(op, 'boundary', f'1,2,3,4,4,{op}', tags=tags)
        k(op, 'boundary', f'1e308,1e308,2,{op}', tags=tags)
        k(op, 'boundary', f'1,INF,2,{op}', tags=tags)
        k(op, 'boundary', f'NEGINF,INF,2,{op}', tags=tags + sort_tags)
    k('AVG', 'boundary', '1,2,-1,AVG,POP,+')
    k('SMIN', 'boundary', 'UNKN,3,1,UNKN,4,SMIN')
    k('SMAX', 'boundary', '3,UNKN,2,SMAX')
    k('MEDIAN', 'boundary', '5,1,UNKN,9,UNKN,3,6,MEDIAN')
    k('MEDIAN', 'boundary', '5,1,UNKN,9,UNKN,3,2,7,MEDIAN')
    k('STDEV', 'boundary', '1,2,3,4,5,6,7,8,9,10,10,STDEV', tags=['fma'])
    k('STDEV', 'boundary', '1e9,1e9,1,+,1e9,2,+,3,STDEV', tags=['fma'])
    k('STDEV', 'boundary', '5,1,STDEV')
    for expr in ['1,2,3,4,5,50,5,PERCENT', '1,2,3,4,5,0,5,PERCENT,+',
                 '1,2,3,4,5,100,5,PERCENT', '1,2,3,4,5,10,5,PERCENT',
                 '1,2,3,4,5,9.99,5,PERCENT,+', '1,2,3,4,5,10.01,5,PERCENT',
                 '1,2,3,4,5,30,5,PERCENT', '1,2,3,4,5,70,5,PERCENT',
                 '1,2,3,4,5,95,5,PERCENT', '9,8,7,6,50,4,PERCENT',
                 '9,8,7,6,12.5,4,PERCENT', '9,8,7,6,37.5,4,PERCENT',
                 '9,8,7,6,62.5,4,PERCENT', '1,2,3,4,5,50,4.9,PERCENT,+']:
        k('PERCENT', 'normal', expr)
    for expr in ['a,b,c,50,3,PERCENT', 'a,b,c,100,3,PERCENT']:
        x('PERCENT', 'normal', expr)
    for expr in ['a,d,e,50,3,PERCENT', 'a,d,e,100,3,PERCENT',
                 'a,d,e,34,3,PERCENT']:
        x('PERCENT', 'boundary', expr, tags=['host-sort'])
    for expr in ['1,2,3,101,3,PERCENT', '1,2,3,-1,3,PERCENT',
                 '1,2,3,UNKN,3,PERCENT', '1,2,3,100.0001,3,PERCENT',
                 '1,2,3,50,4,PERCENT', '1,PERCENT']:
        k('PERCENT', 'error', expr)
    k('PERCENT', 'boundary', '7,1,2,3,0,3,PERCENT' + dump(2))
    k('PERCENT', 'boundary', '7,UNKN,2,3,50,3,PERCENT' + dump(2))
    k('PERCENT', 'boundary', '7,1,2,3,1,3,PERCENT' + dump(2))

    # TREND and TRENDNAN
    for op in ['TREND', 'TRENDNAN']:
        for window in ['10', '20', '30', '35', '60', '5', '0', '-10', '1e9',
                       'UNKN']:
            # An unknown window crashes the x86_64 build of the reference.
            x(op, 'normal' if window in ('30', '60') else 'boundary',
              f'b,{window},{op}',
              tags=['undefined'] if window == 'UNKN' else [])
        x(op, 'normal', f'a,40,{op}')
        x(op, 'boundary', f'a,40,{op}',
          defs=['DEF:a=f.rrd:a:AVERAGE:start=' + str(BASE + 1900)])
        x(op, 'error', f'a,1,+,30,{op}')
        x(op, 'error', f'30,a,{op}')
        x(op, 'error', f'a,{op}')
        x(op, 'boundary', f'a,POP,b,30,{op}')
        x(op, 'boundary', f'a,b,30,{op},+')
        x(op, 'boundary', f'b,30,{op},b,60,{op},+')
    # PREDICT family
    long_window = ['--start', str(BASE + 1500), '--end', str(BASE + 1620)]
    pdefs = ['DEF:a=f.rrd:a:AVERAGE:start=' + str(BASE + 1000),
             'DEF:b=f.rrd:b:AVERAGE:start=' + str(BASE + 1000)]
    for op in ['PREDICT', 'PREDICTSIGMA']:
        tags = ['fma'] if op == 'PREDICTSIGMA' else []
        for args in ['60,1,30,a', '60,1,0,a', '60,1,5,a', '120,60,2,30,a',
                     '60,-3,20,a', '30,-1,10,a', '0,1,30,a', '25,1,10,a',
                     '60,1,30,b', '60,-4,30,b', '0,-2,0,a', '1000,1,30,a',
                     '60,0,30,a', '60,1,-10,a', '0,1,0,a']:
            x(op, 'normal', f'{args},{op}', window=long_window, tags=tags)
        x(op, 'boundary', f'60,1,30,a,{op}', tags=tags)
        x(op, 'boundary', f'60,1,30,a,{op}', window=long_window,
          defs=pdefs, tags=tags)
        x(op, 'boundary', f'60,-3,20,b,{op}', window=long_window,
          defs=pdefs, tags=tags)
        x(op, 'error', f'-60,1,30,a,{op}', window=long_window)
        x(op, 'error', f'60,10,-2,30,a,{op}', window=long_window)
        x(op, 'error', f'60,2,30,a,{op}', window=long_window)
        # The token before the operator is not a variable: the reference
        # reads its step from a non-variable token, so the result is not a
        # contract (see the specification).
        x(op, 'boundary', f'60,1,30,a,1,+,{op}', window=THREE,
          tags=['undefined'])
        x(op, 'boundary', f'60,1,30,3,{op},a,POP', window=THREE,
          tags=['undefined'])
        x(op, 'boundary', f'a,POP,10,20,2,30,b,{op}', window=long_window,
          tags=tags)
    for perc in ['50', '0', '100', '-50', '-100', '25', '-25', '99.9',
                 '-0.5', '1e-9']:
        x('PREDICTPERC', 'normal' if perc in ('50', '-50') else 'boundary',
          f'60,-4,30,{perc},a,PREDICTPERC', window=long_window,
          tags=['host-sort'])
    for args in ['60,1,30,50,b', '60,-4,30,50,b', '30,10,2,40,75,a',
                 '0,1,0,50,a', '60,1,30,50,d']:
        x('PREDICTPERC', 'boundary', f'{args},PREDICTPERC',
          window=long_window, tags=['host-sort'])
    for perc in ['101', '-101', '100.5']:
        x('PREDICTPERC', 'error', f'60,-4,30,{perc},a,PREDICTPERC',
          window=long_window)
    x('PREDICTPERC', 'boundary', '60,-4,30,UNKN,a,PREDICTPERC',
      window=long_window)
    x('PREDICTPERC', 'error', '-60,1,30,50,a,PREDICTPERC',
      window=long_window)
    x('PREDICTPERC', 'boundary', '60,-4,30,50,a,PREDICTPERC',
      window=long_window, defs=pdefs, tags=['host-sort'])

    # Mixed resolutions; the first rows are masked, see the specification.
    for expr in ['z', 'a,z,+', 'PREV(z)', 'z,PREV,ADDNAN', 'z,60,TREND',
                 'z,90,TRENDNAN', '30,-2,60,z,PREDICT',
                 'a,POP,z,STEPWIDTH,*', 'z,COUNT,+',
                 'z,TIME,+,1000000000,-']:
        x('MIXEDSTEP', 'boundary', f'a,POP,COUNT,6,GT,{expr},UNKN,IF',
          defs=G_DEFS)

    # VDEF functions
    for vdef in ['a,MAXIMUM', 'a,MINIMUM', 'a,AVERAGE', 'a,STDEV', 'a,LAST',
                 'a,FIRST', 'a,TOTAL', 'a,50,PERCENT', 'a,95,PERCENTNAN',
                 'a,LSLSLOPE', 'a,LSLINT', 'a,LSLCORREL', 'b,AVERAGE',
                 'b,50,PERCENT', 'b,50,PERCENTNAN', 'b,FIRST', 'b,LAST',
                 'd,MAXIMUM', 'd,MINIMUM', 'd,AVERAGE', 'd,0,PERCENT',
                 'd,100,PERCENT', 'b,TOTAL', 'b,STDEV', 'b,LSLSLOPE']:
        tags = ['fma'] if any(f in vdef for f in ('STDEV', 'LSL')) else []
        if vdef.startswith('d,') and 'PERCENT' in vdef:
            tags.append('host-sort')
        add('VDEF', 'normal', graph([f'VDEF:v={vdef}', 'PRINT:v:%.10le',
                                     'PRINT:v:%s:strftime']), tags=tags)
    for vdef in ['a,PREV', 'a,1,+', 'a,FOO', 'a,101,PERCENT', 'a',
                 'a,MAXIMUM,1']:
        add('VDEF', 'error', graph([f'VDEF:v={vdef}', 'PRINT:v:%.10le']))
    add('VDEF', 'normal', graph(['VDEF:v=a,MAXIMUM', 'CDEF:w=a,v,-',
                                 'VDEF:m=w,MINIMUM', 'PRINT:m:%.10le']))
    add('VDEF', 'error', graph(['VDEF:v=a,MAXIMUM', 'CDEF:w=a,PREV(v),-',
                                'VDEF:m=w,MINIMUM', 'PRINT:m:%.10le']))
    return out


def setup(rrdtool: str, directory: Path) -> None:
    for command in fixture_commands():
        result = subprocess.run([rrdtool] + command, cwd=directory,
                                env=child_env({}), capture_output=True)
        if result.returncode != 0:
            sys.exit(f'fixture {command[:2]} failed: {result.stderr!r}')


def child_env(extra: dict) -> dict:
    env = {k: v for k, v in os.environ.items() if k != 'RRDCACHED_ADDRESS'}
    env['TZ'] = 'UTC'
    env['LC_ALL'] = 'C'
    env.update(extra)
    return env


def extract(args: list[str], status: int | None, stdout: str) -> dict:
    """The comparable part of stdout: xport rows or graph print lines."""
    if status != 0:
        return {'stdout': stdout}
    if args[0] == 'xport':
        def tag(name: str) -> str:
            return stdout.split(f'<{name}>', 1)[1].split(f'</{name}>', 1)[0]
        meta = [int(tag(name)) for name in ('start', 'end', 'step', 'rows')]
        rows = []
        for line in stdout.splitlines():
            line = line.strip()
            if line.startswith('<row>'):
                cells = line.replace('<row>', '').replace('</row>', '')
                values = [part.split('</v>')[0] for part in cells.split('<v>')[1:]]
                rows.append(' '.join(values))
        return {'meta': meta, 'rows': rows}
    prints = [line for line in stdout.splitlines() if line.startswith('print[')]
    return {'prints': prints}


def run_case(rrdtool: str, fixture: Path, case: dict) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / 'w'
        shutil.copytree(fixture, work)
        try:
            result = subprocess.run([rrdtool] + case['args'], cwd=work,
                                    env=child_env(case['env']),
                                    capture_output=True, timeout=60)
            status = result.returncode if result.returncode >= 0 else None
            stdout = result.stdout.decode('latin-1')
            stderr = result.stderr.decode('latin-1')
        except subprocess.TimeoutExpired:
            status, stdout, stderr = None, '', 'timeout'
    expect = {'status': status, 'stderr': stderr}
    expect.update(extract(case['args'], status, stdout))
    if status is None:
        expect = {'status': None, 'crash': True}
    return expect


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument('--rrdtool', default='rrdtool')
    parser.add_argument('--out', default=str(
        Path(__file__).resolve().parent.parent
        / 'crates/rondi-cli/tests/data/rpn_vectors.json'))
    parser.add_argument('--probe', nargs='*')
    parser.add_argument('--check')
    parser.add_argument('--tz', default='UTC')
    parser.add_argument('--record-platform', metavar='KEY')
    parser.add_argument('--merge-platform', nargs=2, metavar=('KEY', 'FILE'))
    parser.add_argument('--platform-out')
    options = parser.parse_args()

    if options.merge_platform:
        key, source = options.merge_platform
        table = json.loads(Path(options.out).read_text())
        results = json.loads(Path(source).read_text())
        for case in table['vectors']:
            case.get('expect_platform', {}).pop(key, None)
            if case['id'] in results:
                result = results[case['id']]
                if result.get('crash') or result.get('unstable'):
                    sys.exit(f'{case["id"]} is not stable on {key}; tag it')
                case.setdefault('expect_platform', {})[key] = result
            if case.get('expect_platform') == {}:
                del case['expect_platform']
        Path(options.out).write_text(render(table))
        print(f'{len(results)} {key} expectations merged')
        return

    with tempfile.TemporaryDirectory() as tmp:
        if options.record_platform:
            # Run the frozen table through another build of the reference
            # and keep the results that differ from the frozen expectation.
            table = json.loads(Path(options.out).read_text())
            fixture = Path(tmp) / 'fixture'
            fixture.mkdir()
            setup(options.rrdtool, fixture)
            results = {}
            for case in table['vectors']:
                if 'undefined' in case['tags']:
                    continue
                got = run_case(options.rrdtool, fixture, case)
                if got != run_case(options.rrdtool, fixture, case):
                    got = {'unstable': True}
                if got != case['expect']:
                    results[case['id']] = got
            Path(options.platform_out).write_text(
                json.dumps(results, indent=1, sort_keys=True) + '\n')
            print(f'{len(results)} differ on {options.record_platform}')
            return
        fixture = Path(tmp) / 'fixture'
        fixture.mkdir()
        if options.probe is not None:
            setup(options.rrdtool, fixture)
            for expr in options.probe:
                args = xport(expr) if not expr.startswith('[') else json.loads(expr)
                case = {'args': args, 'env': {'TZ': options.tz}}
                print(expr, json.dumps(run_case(options.rrdtool, fixture, case)))
            return
        if options.check:
            table = json.loads(Path(options.out).read_text())
            link = Path(tmp) / 'bin' / 'rrdtool'
            link.parent.mkdir()
            link.symlink_to(Path(options.check).resolve())
            setup(str(link), fixture)
            diverged = 0
            for case in table['vectors']:
                if case['expect'].get('crash'):
                    continue
                got = run_case(str(link), fixture, case)
                if got != case['expect']:
                    diverged += 1
                    print(json.dumps({'id': case['id'], 'args': case['args'][-2:],
                                      'expect': case['expect'], 'got': got}))
            print(f'{diverged} divergent of {len(table["vectors"])}', file=sys.stderr)
            return
        version = subprocess.run([options.rrdtool, '--version'],
                                 capture_output=True, text=True).stdout
        if not version.startswith('RRDtool 1.11.0 '):
            sys.exit(f'expected RRDtool 1.11.0, found {version.splitlines()[:1]}')
        setup(options.rrdtool, fixture)
        vectors = []
        for case in cases():
            case['expect'] = run_case(options.rrdtool, fixture, case)
            # A crash, or a result that changes between runs, is reference
            # behavior that depends on memory contents; record it, but the
            # replay test does not compare it.
            again = run_case(options.rrdtool, fixture, case)
            if case['expect'].get('crash') or again != case['expect']:
                if 'undefined' not in case['tags']:
                    case['tags'].append('undefined')
            vectors.append(case)
        table = {
            'comment': 'Generated by scripts/rpn-vectors.py from RRDtool 1.11.0; '
                       'see docs/compatibility/rpn.md.',
            'oracle': version.splitlines()[0],
            'fixture': fixture_commands(),
            'vectors': vectors,
        }
        Path(options.out).parent.mkdir(parents=True, exist_ok=True)
        Path(options.out).write_text(render(table))
        print(f'{len(vectors)} vectors written to {options.out}')


def render(table: dict) -> str:
    """JSON with one fixture command and one vector per line, so a
    regenerated table diffs by vector."""
    def items(values: list) -> str:
        return ',\n'.join(json.dumps(value) for value in values)
    return ('{\n'
            f'"comment": {json.dumps(table["comment"])},\n'
            f'"oracle": {json.dumps(table["oracle"])},\n'
            f'"fixture": [\n{items(table["fixture"])}\n],\n'
            f'"vectors": [\n{items(table["vectors"])}\n]\n'
            '}\n')


if __name__ == '__main__':
    main()
