#!/usr/bin/env python3
"""Capture at-style time specification vectors from a reference rrdtool.

Usage:
  scripts/time-spec-vectors.py [--rrdtool PATH] [--out FILE]
  scripts/time-spec-vectors.py --probe TZ START [END]     # one graphv result
  scripts/time-spec-vectors.py --check PATH               # diff PATH vs FILE
  scripts/time-spec-vectors.py --rrdtool PATH --record-platform KEY \
      --platform-out F
  scripts/time-spec-vectors.py --merge-platform KEY F
  scripts/time-spec-vectors.py --now-check                # needs faketime

The cases are listed here; the output file is the frozen table that
crates/rondi-cli/tests/time_spec_vectors.rs replays and that
docs/compatibility/time-spec.md describes. Every case runs in a fresh copy
of the fixture directory with its own TZ and LC_ALL=C, and runs twice; a
case whose two runs differ is tagged `unstable` and is not compared frozen.

Two kinds of case exist:

- fixed: the result does not depend on the current time. The expectation
  is the filtered stdout, stderr and exit status of every step.
- equiv: the result depends on the current time. The case names a second
  argument list built from placeholders that the replay test fills in from
  `date` in the same time zone ({now}, {Y}, {m}, {d}, {H}, {M}, {S}, {D+k}
  for today plus k days as MM/DD/YYYY, {W:k} for weekday k of the current
  Sunday-based week). The rule is that both argument lists produce the same
  result. The capture checks it against the reference at capture time.

--now-check runs every fixed case under faketime(1) at several fixed
instants and fails if any result changes, which proves the fixed cases do
not depend on the current time; it also checks every equiv case at those
instants. Run it inside the CI image (docker/Dockerfile.test) after
installing faketime, since macOS does not ship it.

The table is captured on macOS arm64 (/opt/homebrew/bin/rrdtool 1.11.0).
Results that differ on Linux are recorded per vector under
expect_platform.<KEY> (KEY linux-x86_64 or linux-aarch64) with
--record-platform in the CI image, then --merge-platform. Regenerate only
with RRDtool 1.11.0 and review the diff.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ZONES = ['UTC', 'America/New_York', 'Europe/London', 'Asia/Kolkata',
         'Pacific/Auckland', 'Australia/Lord_Howe']
BASE = 1_000_000_000

FIXTURE = [
    ['create', 'f.rrd', '--start', str(BASE), '--step', '1',
     'DS:x:GAUGE:2:U:U', 'RRA:AVERAGE:0.5:1:4'],
    ['create', 'h.rrd', '--start', str(BASE), '--step', '3600',
     'DS:x:GAUGE:7200:U:U', 'RRA:AVERAGE:0.5:1:48'],
]

def graph(start: str | None, end: str | None = 'start+1',
          rrd: str = 'f.rrd') -> list[str]:
    args = ['graphv', '/dev/null']
    if start is not None:
        args += ['--start', start]
    if end is not None:
        args += ['--end', end]
    return args + [f'DEF:x={rrd}:x:AVERAGE', 'LINE1:x#000']


# ---------------------------------------------------------------- cases

DATE_SPECS = [
    '12:00 01/02/2003', '01/02/2003', '1/2/03', '01/02/03 12:30',
    '20030102', '20030102 13:00', '02.01.2003', '2.1.03 08:15', '02.01.03',
    '12:00 Jan 2 2003', 'Jan 2 2003 12:00', '12:00 January 2 2003',
    '12:00 jan 2 03', '12:00 Jan 2, 2003', '12:00 Sep 30 2003',
    '12:00 Dec 31 1999', '23:59 31.12.1999', '12/31/99 11:59pm',
    '12am 01/01/01', '19970703 12:45', 'noon 07/04/2004',
    'midnight 02/29/2004', 'teatime 20031225', '4:30pm 06/15/2025',
    '6am 07/01/2025', '12:00 02/29/2003', '12:00 31.04.2025',
    '12:00 Feb 30 2024', '12:00 Jan 32 2025', '0:00 01/01/2038',
    '931200300', '1000000000', '1700000000', '315360001', '2147483647',
    '2147483648', '4102444800', '12:00 Jun 15 2025', '23:59 12/31/2025',
    '1:00 20250101', 'noon 1.7.2025',
]

OFFSET_UNITS = [
    '+1s', '+1sec', '+1second', '+2seconds', '+1secs', '+1se', '+1m', '+5m',
    '+6m', '-5m', '-6m', '+1min', '+1mins', '+1minute', '+1minutes', '+1mi',
    '+1h', '+1hr', '+1hrs', '+1hour', '+1hours', '+1ho', '+1d', '+1day',
    '+1days', '+1da', '+1w', '+1wk', '+1wks', '+1week', '+1weeks', '+1we',
    '+1mon', '+1mons', '+1month', '+1months', '+1mo', '+1y', '+1yr',
    '+1yrs', '+1year', '+1years', '+1ye', '+1M', '+1H', '+1D', '+1MONTH',
    '+1', '+1x', '+1 d', ' + 1 d', '+1d2h', '+1d 2h', '+1y6m', '+3h20m',
    '+3w1m', '+5s2m', '-5mon1w2d', '+1d6m', '+1h3m', '+1m2m', '+6m2m',
    '+5 m', '+1noon', '+1foo2h', '+1ms', '-1-1', '-1d+1d', '+0d',
    '+00001d', '+1.5d', '+1e3s', '-5h45min', '-6h+15min', '-7h+1h30m-15min',
    '+1h-5m', '-1h-5m', '-1d-10m', '-1h 5m', '+1d,2h', '+1d_2h', '-0s',
    '+12mon', '-12mon', '+13mon', '+60m', '+5min', '-1w-1d-1h-1m-1s',
    '+1y1mon1w1d1h1min1s', '+86400', '-86400s', '+1d-24h', '+30d-1mon',
    '+1 2h', '+1 2', '+1 2x', '+1 2 3', '+1 2 3 4', '+1:5', '+1/5',
    '+1 noon 2h', '+1h 1x 7m', '+1d 1x 2m', '+1h 7 7m', '+1 m', '+1x 2m',
    '+1 2min', '+1h 2 3h',
]

CAL_OFFSETS = [
    'Jan 31 2025 10:00 +1mon', 'Jan 31 2024 10:00 +1mon',
    'Mar 31 2025 10:00 -1mon', 'Feb 29 2024 10:00 +1y',
    'Feb 29 2024 10:00 -1y', 'May 31 2025 10:00 -1month',
    'Jan 31 2003 10:00 +1mon-1mon', 'May 31 2003 10:00 -1mon-1mon',
    '1041397200+180d', '1741500000+1d', '1741500000+24h', '1743868800+1d',
    '1743868800+24h', 'Mar 8 2025 02:30 +1d', 'Apr 5 2025 02:30 +1d',
    'Nov 1 2025 01:30 +1 day', 'Oct 4 2025 02:15 +1d',
    'Sep 27 2025 02:30 +1d', '8:00 Mar 8 2025 +2 days',
    '8:00 Mar 8 2025 +48 hours', 'Jan 1 2025 00:00 +1y-1s',
    '12:00 01/01/2025 +1w2d3h4m5s', 'Apr 7 2003 03:30 -1h-1d',
    'Mar 30 2025 12:00 -1w', 'Oct 26 2025 12:00 -1d', '1761451200-1d',
    '1761451200-24h', '1762074000+1d', 'Jun 30 2025 23:00 +1h',
    'Dec 31 2025 23:59 +1min', '1735689599+1s', 'Jan 1 2025 12:00 +365d',
]

TOD_SPECS = [
    '12:00 01/02/2003', '12:30:45 01/02/2003', '12.30 01/02/2003',
    '12 01/02/2003', '12pm 01/02/2003', '12am 01/02/2003',
    '12:30am 01/02/2003', '12:30pm 01/02/2003', '0am 01/02/2003',
    '13am 01/02/2003', '0pm 01/02/2003', '11pm 01/02/2003',
    '1 pm 01/02/2003', '1p 01/02/2003', '1PM 01/02/2003', '1Am 01/02/2003',
    '1a.m. 01/02/2003', '24:00 01/02/2003', '23:59 01/02/2003',
    '25:00 01/02/2003', '9:5 01/02/2003', '09:05 01/02/2003',
    '009:05 01/02/2003', '1:2:3 01/02/2003', '12:60 01/02/2003',
    '12:99 01/02/2003', '12:123 01/02/2003', '0:00 01/02/2003',
    'midnight 01/02/2003', 'noon 01/02/2003', 'teatime 01/02/2003',
    'MIDNIGHT 01/02/2003', 'Noon 01/02/2003', '01/02/2003 noon',
    '01/02/2003 12:00', '01/02/2003 12:30pm', '01/02/2003 1pm',
    '01/02/2003 13', '20030102 13', '20030102 1300', '1230 01/02/2003',
    '12:30 2.1.2003', '13:00 2 Jan 2003', 'noon pm 01/02/2003',
    '12:00pm 01/02/2003', '12:00 am 01/02/2003', '1:00am 20030102',
    '8:00 20030102 pm', '01/02/2003 pm', '01/02/2003 +1d',
    '01/02/2003 8', '01/02/2003 08:30 +1h', 'noon 01/02/2003 +1h',
    'teatime 01/02/2003 -1d', 'midnight 01/02/2003 +1d -1s',
]

YEAR_SPECS = [
    '0', '00', '000', '1', '9', '37', '38', '69', '70', '71', '99', '100',
    '137', '138', '139', '199', '999', '1000', '1900', '1901', '1938',
    '1969', '1970', '1971', '1979', '1980', '2037', '2038', '2106', '9999',
    '10000', '99999', '0000000000002003', '2147483647', '2147485547',
    '2147485548', '4294967295', '4294967296', '99999999999',
]

ERROR_SPECS = [
    '12:60', '13pm', '01/32/2003', '13/01/2003', '01/02/1969', '01/02/50',
    'Jan 1 1970', '1 jan', 'now foo', 'now+', 'end-1h', 'epoch',
    '1000000000.5', 'Jan 1 2004 noon', '@x', '25', '0019700102', 'foo',
    '', ' ', 'monday', 'mon', 'today', 'yesterday', 'tomorrow',
    'today 13:00', 'monday 13:00', 'Janu 2 2003', 'Ja 2 2003',
    'sept 2 2003', 'Sept 2 2003', '2 Jan', 'Jan', '13:00 Jan', '86400',
    '315360000', '19700101', '19691231', '2003012', '200301021',
    '99991231', '10000101', '09991231', '1000000000 1d', '1000000000+',
    '1000000000+d', '1000000000++1d', '1000000000+-1d', '1000000000+1d-',
    '1000000000/+1d', '1000000000.+1d', '1000000000:+1d', 'en-1',
    'no-1d', 'sta+1', 'epo+1000000000', '12:00 00/01/2003',
    '12:00 01/00/2003', '12:00 20031301', '12:00 20030100',
    '12:00 20030132', '12:00 32.01.2003', '12:00 01.13.2003',
    '12:00 01/02/', '12:00 01/02/x', '12:00 01.02.', '12:00 01.', '12:00 01/',
    '12:00 Jan 0 2003', 'midnight noon', '12:00 13:00', 'noon noon',
    '12:00 01/02/2003 01/03/2003', 'now now', 'now 12:00', 'now Jan 2',
    '12:00 now', 'start', 'end', 'epoch+1d', '12:00 01-02-2003',
    '2003-01-02', '2003-01-02 10:00', 'T12:00', '12h', '12:00:00',
    '-', '+', '+-', '1000000000 +', 's', 'e', 'n+', 'Jan 2 13:00',
]

TOKEN_SPECS = [
    '1000000000,+1d', '1000000000_+_1_d', '1000000000;+1d',
    '1000000000\t+1d', '1000000000\n+1d', '1000000000#+1d',
    '1000000000!+1d', '1000000000@+1d', '1000000000(+1d)',
    '1000000000$+1d', '1000000000%+1d', '1000000000&+1d',
    '1000000000*+1d', '1000000000=+1d', '1000000000?+1d',
    '1000000000\\+1d', "1000000000'+1d", '1000000000"+1d',
    '1000000000[+1d]', '1000000000{+1d}', '1000000000<+1d',
    '1000000000|+1d', '1000000000~+1d', '1000000000^+1d',
    '1000000000`+1d', ' 1000000000', '1000000000 ', '  12:00   01/02/2003  ',
    '12:00_01/02/2003', '12:00,01/02/2003', 'noon,01/02/2003',
    '12:00 JAN 2 2003', '12:00 jAnUaRy 2 2003', 'NOON 01/02/2003',
    '12:00\t01/02/2003', '12:00 Jan_2_2003', '12:00 Jan,2,2003',
    '12:00 Jan2 2003', '12:00 Jan 2 2003+1d', '12:00Jan 2 2003',
    'noon01/02/2003', '12:00 01/02/2003+1d', '12:00 01/02/2003-1d+1h',
    '12:00 01/02/2003 + 1 d', '1000000000\u00a0+1d', '1000000000\u00e9',
    '12:00 01/02/2003 \u00e9', '12:00 01/02/2003#comment',
]

MONTH_NAMES = [
    'jan', 'feb', 'mar', 'apr', 'may', 'jun', 'jul', 'aug', 'sep', 'oct',
    'nov', 'dec', 'january', 'february', 'march', 'april', 'june', 'july',
    'august', 'september', 'october', 'november', 'december', 'sept',
    'janu', 'febr', 'decem', 'ja', 'j',
]

REF_PAIRS = [
    ('1000000000', 's+1'), ('1000000000', 'S+1'), ('1000000000', 'START+1'),
    ('1000000000', 'sta+1'), ('1000000000', 'star+1'), ('1000000000', 'e+1'),
    ('e-1', '1000000000'), ('end-1', '1000000000'), ('END-1H', '1000000000'),
    ('en-1', '1000000000'), ('1000000000', 'start'),
    ('start', '1000000000'), ('1000000000', 'end'), ('1000000000', 'end+1'),
    ('end-1d', 'start+1d'), ('start+1', 'end-1'), ('1000000000', 'start-1'),
    ('1000000000', 'start+0'), ('end+0', '1000000000'),
    ('1000000000', '999999999'), ('1000000000', '1000000000'),
    ('end-1w-3h', '1049630400'), ('end-1mon', '1049630400'),
    ('end-1d', '1049630400'), ('1049544000', 'start+2d'),
    ('1049544000', 'start+1d+90min'), ('end-1y', '1000000000'),
    ('end-3weeks', '1000000000'), ('e-3w', '1000000000'),
    ('1000000000', 'start+6hours'), ('1000000000', 's+6h'),
    ('end-1d', '1741500000'), ('end-1d', '1743307200'),
    ('end-1d', '1743915600'), ('end-1d', '1759635000'),
    ('end-1d', '1761451200'), ('end-1d', '1762074000'),
    ('1741410000', 'start+1d'), ('1743220800', 'start+1d'),
    ('1743829200', 'start+1d'), ('1759548600', 'start+1d'),
    ('1761364800', 'start+1d'), ('1761987600', 'start+1d'),
    ('end-1mon', '1743307200'), ('1741410000', 'start+1mon'),
    ('end-1d', '12:00 03/10/2025'), ('12:00 03/08/2025', 'start+1d'),
    ('end-24h', '12:00 03/10/2025'), ('12:00 03/08/2025', 'start+24h'),
    ('end-1h', 'start+1h'), ('epoch+1000000000', 'start+1'),
    ('epoch+1000000000s', 'start+1'), ('epoch + 1000000000', 'start+1'),
    ('epoch+19711205s', 'start+1'), ('epoch-1', 'start+1'),
    ('epoch+1000000000+1d', 'start+1'), ('epoch+1000000000-1h', 'start+1'),
    ('1000000000', 'epoch+1000000001'), ('EPOCH+1000000000', 'start+1'),
    ('epoch+1000000000+1mon', 'start+1'), ('epoch+1d+1000000000s', 'start+1'),
    ('end-1d', 'epoch+1000000000'), ('end-1d', 'epoch+1000000000+1d'),
    ('1000000000', 'start+1d-1d'), (None, '1000000000'),
    ('1000000000', 'start+1s 2s'), ('end-1', 'end-2'), ('start+1', 'start+2'),
    ('start+1', '1000000000'), ('315360002-2s', 'start+1'),
    ('315360002-3s', 'start+1'),
]

DST_SPECS = [
    'Mar 9 2025 02:30', 'Mar 9 2025 02:00', 'Mar 9 2025 03:00',
    'Mar 9 2025 01:59', 'Nov 2 2025 01:30', 'Nov 2 2025 01:00',
    'Nov 2 2025 02:00', 'Mar 30 2025 01:30', 'Mar 30 2025 02:30',
    'Oct 26 2025 01:30', 'Oct 26 2025 02:30', 'Sep 28 2025 02:30',
    'Apr 6 2025 02:30', 'Apr 6 2025 01:45', 'Oct 5 2025 02:15',
    'Apr 6 2025 01:30', 'midnight 03/09/2025', '20250309 02:00',
    '20251102 01:00', '02:00 03/09/2025', '2:30am 03/09/2025 +1h',
    'Mar 8 2025 02:30 +1d', 'Nov 1 2025 01:30 +1d', 'Oct 4 2025 02:15 +1d',
    'Mar 29 2025 01:30 +1d', 'Sep 27 2025 02:30 +1d',
    'Oct 5 2025 01:59 +1min', 'Oct 5 2025 2:00am -30min',
    '4/6/25 2:30am', '12pm Apr 6 2025', '20251026 02:30',
    'Mar 9 2025 02:30 -1h', 'Mar 9 2025 02:30 +0d', 'Mar 9 2025 02:30 +1h',
]

RANGE_SPECS = [
    '1000000000+99999999999s', '1000000000+9999999999999999999s',
    '1000000000+99999999999999999999s', '1000000000+2147483648s',
    '1000000000+2147483647s', '1000000000+4294967296s',
    '1000000000+2147483647d', '1000000000+100000d', '1000000000+25000000d',
    '1000000000+2147483647mon', '1000000000+178956970y',
    '1000000000+2147483647y', '1000000000-1000000000s',
    '1000000000-1000000001s', '1000000000-40y', '99999999999',
    '999999999999', '9999999999999', '99999999999999999999',
    '9223372036854775807', '9223372036854775808', '4294967296',
    '4294967295', '2147483648+1d', '67767976233532799',
    '67767976233532800', '12:00 12/31/2147485547 +1d',
    '12:00 01/02/99999999999', '1000000000+1d-1d+2147483647s',
    '1000000000+21474836470s', '1000000000+999999999999999h',
    '1000000000+2562047788015215h', '1000000000-2147483648s',
    '1000000000+3000000000s', '1000000000+5000000000s',
]

WEEKDAYS = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sunday',
            'monday', 'tuesday', 'wednesday', 'thursday', 'friday',
            'saturday', 'SUN', 'Sat', 'FriDay']
# Weekday-like words that are rejected whatever the current day is.
WEEKDAY_ERRORS = ['noon tues', 'noon thur', 'noon thurs', 'noon wednes',
                  'noon su', 'noon sundays', 'noon sun 01/02/2003',
                  'noon 01/02/2003 sun', 'noon yesterday tomorrow',
                  'noon today 01/02/2003', 'noon Jan 2 2003 sun',
                  'sun', 'sunday noon', 'noon sun mon', 'noon today today']


# Wall-clock times inside a repeated (fall-back) hour. glibc resolves them
# by the daylight-saving state of the current time, so they are frozen on
# macOS only; --now-check finds them.
OVERLAP = {
    'America/New_York': ['Nov 2 2025 01:30', 'Nov 2 2025 01:00',
                         '20251102 01:00', 'Nov 1 2025 01:30 +1d',
                         'Nov 1 2025 01:30 +1 day'],
    'Europe/London': ['Oct 26 2025 01:30'],
    'Pacific/Auckland': ['Apr 6 2025 02:30', '4/6/25 2:30am',
                         'Apr 5 2025 02:30 +1d'],
    'Australia/Lord_Howe': ['Apr 6 2025 01:45', 'Apr 6 2025 01:30'],
}

# Numbers beyond the 64-bit range: macOS falls back to the current time and
# glibc to a value that depends on it.
UNDEFINED = ['99999999999999999999', '9223372036854775807',
             '9223372036854775808']


def entry_cases() -> list[dict]:
    """Option entry points other than graphv --start/--end."""
    out: list[dict] = []

    def add(group: str, steps: list[list[str]], tz: str = 'UTC',
            tags: list[str] | None = None) -> None:
        out.append({'group': group, 'tz': tz, 'steps': steps,
                    'tags': list(tags or [])})

    ds = ['DS:x:GAUGE:300000:U:U', 'RRA:AVERAGE:0.5:1:10']
    for spec in ['noon 01/02/2003', '1000000000+1d', '20030102 13:00',
                 'Jan 1 2004 12:00', 'foo', '01/02/70 00:00', '315360001',
                 '315360000', '12/31/1979 23:59 +1min', '12/31/1979 23:59',
                 'end-1d', 'start+1d', 'epoch+1000000000', '01/02/1969',
                 '12:00 Jan 2 2003 -1y']:
        add('create', [['create', 't.rrd', '--start', spec] + ds,
                       ['last', 't.rrd']])
        add('create', [['create', 't.rrd', '-b', spec] + ds,
                       ['last', 't.rrd']])
    for value in ['noon 01/02/2003@5', 'foo@5', 'end-1h@5',
                  'Jan 1 2004 12:00@5', '1000000100+1d@5', '1000000100@5',
                  '1000000100:5', '1000000100.5:5', '1000000100.25@5',
                  '1000000100+1d:5', 'noon:5', '20030102@5', '20030102:5',
                  '@5', '1000000100@', '1000000100@5@6',
                  '1000000100 +1d@5', 'start+1d@5', 'epoch+1000000100@5',
                  '1000000100.5', '1e9:5', '0x3B9ACA64:5', '1000000100.:5',
                  '-1000000100:5', '+1000000100:5', ' 1000000100:5',
                  '1000000100.999999999:5', '01/02/2003:5', '1000000100.5@5']:
        add('update', [['create', 'u.rrd', '--start', str(BASE)] + ds,
                       ['update', 'u.rrd', value], ['last', 'u.rrd']])
    for start, end in [('foo', None), ('1000000000', 'bar'), ('100', None),
                       ('end-1d', '1049630400'), ('1049544000', 'start+2d'),
                       ('end-10h', 'start+10h'), ('1000000000', '999999999'),
                       ('1000000000', '1000000000'), ('end-1w', '1000000000'),
                       ('12:00 01/02/2003', 'start+5h'),
                       ('1000000000', 'e+1'), ('e-1h', '1000000000'),
                       ('start-1h', '1000000000'), ('1000000000', 'end-1h')]:
        args = ['fetch', 'h.rrd', 'AVERAGE', '-r', '3600', '--start', start]
        if end is not None:
            args += ['--end', end]
        add('fetch', [args])
        add('fetch', [args], tz='America/New_York')
    for start, end in [('foo', '1000000000'), ('1000000000', 'bar'),
                       ('end-10h', '1000003600'), ('1000000000', 'start+5h'),
                       ('1000000000', '999999999'), ('e-1h', 's+1h'),
                       ('12:00 01/02/2003', 'start+3h')]:
        add('xport', [['xport', '--start', start, '--end', end,
                       'DEF:x=h.rrd:x:AVERAGE', 'XPORT:x']])
    for start, end in [('end-2h', '1000007200'), ('1000000000', 'start+2h'),
                       ('foo', '1000007200'), ('1000000000', 'bar'),
                       ('end-2h', 'start+2h'), ('1000007200', '1000000000'),
                       ('noon 01/02/2003', 'start+2h')]:
        add('def', [['graphv', '/dev/null', '--start', '1000000000',
                     '--end', '1000086400',
                     f'DEF:x=h.rrd:x:AVERAGE:start={start}:end={end}',
                     'PRINT:x:AVERAGE:%lf']])
    for zone in ['UTC', 'America/New_York', 'Asia/Kolkata', 'Pacific/Auckland',
                 'Australia/Lord_Howe', 'Europe/London']:
        add('dump', [['create', 'd.rrd', '--start', '1741400000', '--step',
                      '3600', 'DS:x:GAUGE:7200:U:U', 'RRA:AVERAGE:0.5:1:6',
                      'RRA:AVERAGE:0.5:24:3'],
                     ['update', 'd.rrd'] + [f'{1741400000 + 3600 * k}:{k}'
                                            for k in range(1, 61)],
                     ['dump', 'd.rrd']], tz=zone)
    add('tz', [graph('12:00 01/02/2003')], tz='Not/A_Zone')
    add('tz', [graph('12:00 01/02/2003')], tz='')
    add('tz', [graph('12:00 01/02/2003')], tz='EST5EDT')
    add('tz', [graph('12:00 07/02/2003')], tz='EST5EDT')
    add('tz', [graph('12:00 07/02/2003')], tz='<+0530>-5:30')
    add('tz', [graph('12:00 07/02/2003')], tz='Asia/Kathmandu')
    add('tz', [graph('Mar 9 2025 02:30')], tz='EST5EDT')
    return out


def fixed_cases() -> list[dict]:
    out: list[dict] = []

    def add(group: str, start: str | None, end: str | None = 'start+1',
            tz: str = 'UTC', tags: list[str] | None = None) -> None:
        # Ranges longer than a few months read the hourly file so that the
        # graph does not fetch tens of millions of one-second rows.
        long = start is not None and start.startswith('end-1y')
        rrd = 'h.rrd' if long else 'f.rrd'
        tags = list(tags or [])
        if start in OVERLAP.get(tz, []):
            tags.append('dst-overlap')
        if start in UNDEFINED:
            tags.append('undefined')
        out.append({'group': group, 'tz': tz,
                    'steps': [graph(start, end, rrd)], 'tags': tags})

    for zone in ZONES:
        for spec in DATE_SPECS:
            add('date', spec, tz=zone)
    for unit in OFFSET_UNITS:
        add('offset', f'{BASE}{unit}')
    for zone in ZONES:
        for spec in CAL_OFFSETS:
            add('calendar', spec, tz=zone)
    for spec in TOD_SPECS:
        add('tod', spec)
    for year in YEAR_SPECS:
        add('year', f'13:00 01/02/{year}')
    for year in ['0', '37', '38', '99', '100', '138', '1970', '1971', '2003']:
        add('year', f'13:00 Jan 2 {year}')
        add('year', f'13:00 02.01.{year}')
    for spec in ERROR_SPECS + WEEKDAY_ERRORS:
        add('error', spec)
    for spec in TOKEN_SPECS:
        add('token', spec)
    for name in MONTH_NAMES:
        add('month', f'12:00 {name} 2 2003')
    for start, end in REF_PAIRS:
        add('ref', start, end)
        add('ref', start, end, tz='America/New_York')
    for zone in ZONES:
        for spec in DST_SPECS:
            add('dst', spec, tz=zone)
    for spec in RANGE_SPECS:
        add('range', spec)
    # A month name or date without a year, and a date before the time of
    # day, are fixed results with an odd year; see TS rules on missing years.
    for spec in ['13:00 Jan 2', '13:00 01/02', '13:00 02.01', '13:00 Feb 2',
                 '13:00 Jan 3', '12:00 Dec 31', '1:00 2.1',
                 '16:30 01/02']:
        add('noyear', spec)
        add('noyear', spec, tz='America/New_York')
    out.extend(entry_cases())
    return out


def equiv_cases() -> list[dict]:
    """Cases whose result depends on the current time, stated as an
    equivalence with an absolute specification."""
    pairs = [
        ('now', '{now}'), ('n', '{now}'), ('NOW', '{now}'),
        ('now-1h', '{now}-1h'), ('-1h', '{now}-1h'), ('-1', '{now}-1s'),
        ('-0', '{now}'), ('+1h', '{now}+1h'), ('n-90min', '{now}-90min'),
        ('-5h45min', '{now}-5h45min'),
        ('now-1d', '{H}:{M} {D-1} +{S}s'),
        ('-1d', '{H}:{M} {D-1} +{S}s'),
        ('-1w', '{H}:{M} {D-7} +{S}s'),
        ('now-1mon', '{H}:{M} {D+0} -1mon +{S}s'),
        ('-1m', '{H}:{M} {D+0} -1mon +{S}s'),
        ('-1y', '{H}:{M} {D+0} -1y +{S}s'),
        ('now+1d-1d', '{now}'),
        ('5', '05:00 {D+0}'), ('0', '00:00 {D+0}'), ('23', '23:00 {D+0}'),
        ('12', '12:00 {D+0}'), ('9am', '09:00 {D+0}'), ('9pm', '21:00 {D+0}'),
        ('12:34', '12:34 {D+0}'),
        ('noon', '12:00 {D+0}'), ('midnight', '00:00 {D+0}'),
        ('teatime', '16:00 {D+0}'), ('NOON', '12:00 {D+0}'),
        ('midnight + 1 day', '00:00 {D+1}'),
        ('noon yesterday', '12:00 {D-1}'), ('noon today', '12:00 {D+0}'),
        ('noon tomorrow', '12:00 {D+1}'),
        ('noon yesterday -3hours', '09:00 {D-1}'),
        ('9am-1day', '09:00 {D-1}'), ('13:00 today', '13:00 {D+0}'),
        ('13:00 tomorrow', '13:00 {D+1}'),
        ('13:00 yesterday -1d', '13:00 {D-2}'),
        ('midnight tomorrow -1s', '00:00 {D+1} -1s'),
        ('teatime,yesterday', '16:00 {D-1}'), ('midnight_tomorrow', '00:00 {D+1}'),
        ('13:00 sun', '13:00 {W:0}'), ('13:00 mon', '13:00 {W:1}'),
        ('13:00 tue', '13:00 {W:2}'), ('13:00 wed', '13:00 {W:3}'),
        ('13:00 thu', '13:00 {W:4}'), ('13:00 fri', '13:00 {W:5}'),
        ('13:00 sat', '13:00 {W:6}'), ('midnight Thu', '00:00 {W:4}'),
        ('noon sunday', '12:00 {W:0}'), ('noon saturday', '12:00 {W:6}'),
        ('noon SAT', '12:00 {W:6}'), ('noon sun -1d', '12:00 {W:0} -1d'),
        ('noon Sat +1w', '12:00 {W:6} +1w'),
        ('noon Jan 2', '12:00 01/02/{Y}'), ('noon 01/02', '12:00 01/02/{Y}'),
        ('teatime 2.1', '16:00 01/02/{Y}'), ('noon Dec 31', '12:00 12/31/{Y}'),
        ('midnight Feb 29', '00:00 02/29/{Y}'),
        ('Jan 2', '{H}:{M} 01/02/{Y} +{S}s'),
        ('Oct 12', '{H}:{M} 10/12/{Y} +{S}s'),
        ('Jan 2 2003', '{H}:{M} 01/02/2003 +{S}s'),
        ('Jan 2, 2003', '{H}:{M} 01/02/2003 +{S}s'),
        ('Jan 2003', '{H}:{M} 01/01/{Y} +2002d +{S}s'),
        ('Jan 1 04', '{H}:{M} 01/01/2004 +{S}s'),
        ('Jan 1 99', '{H}:{M} 01/01/1999 +{S}s'),
        ('1/2/03', '00:00 01/02/2003'),
        ('31.12.2024 23:59', '23:59 12/31/2024'),
    ]
    days = ['sun', 'mon', 'tue', 'wed', 'thu', 'fri', 'sat']
    for name in WEEKDAYS:
        pairs.append((f'09:30 {name}',
                      '09:30 {W:%d}' % days.index(name[:3].lower())))
    out = []
    for zone in ['UTC', 'Asia/Kolkata', 'America/New_York', 'Pacific/Auckland']:
        for spec, same in pairs:
            out.append({'group': 'now', 'tz': zone, 'steps': [graph(spec)],
                        'same_as': [graph(same)], 'tags': []})
    for zone in ['UTC', 'America/New_York']:
        out.append({'group': 'now', 'tz': zone, 'steps': [graph(None, None)],
                    'same_as': [graph('{now}-24h', '{now}')], 'tags': []})
        out.append({'group': 'now', 'tz': zone,
                    'steps': [graph('end-2h', None)],
                    'same_as': [graph('{now}-2h', '{now}')], 'tags': []})
        out.append({'group': 'now', 'tz': zone,
                    'steps': [graph(None, '1000086400')],
                    'same_as': [graph('1000000000', '1000086400')],
                    'tags': []})
    return out


# ------------------------------------------------------------ execution

def fill(text: str, sample: dict[str, str]) -> str:
    def date_of(day: dt.date) -> str:
        return day.strftime('%m/%d/%Y')
    today = dt.date(int(sample['Y']), int(sample['m']), int(sample['d']))

    def repl(match: re.Match[str]) -> str:
        key = match.group(1)
        if key.startswith('D'):
            return date_of(today + dt.timedelta(days=int(key[1:])))
        if key.startswith('W:'):
            weekday = int(sample['w'])
            return date_of(today + dt.timedelta(days=int(key[2:]) - weekday))
        return sample[key]
    return re.sub(r'\{([A-Za-z]+|D[+-]\d+|W:\d)\}', repl, text)


def date_sample(tz: str, pre: list[str]) -> dict[str, str]:
    env = dict(os.environ, TZ=tz, LC_ALL='C')
    text = subprocess.run(pre + ['date', '+%Y %m %d %H %M %S %s %w'],
                          env=env, capture_output=True, text=True,
                          check=True).stdout.split()
    return dict(zip(['Y', 'm', 'd', 'H', 'M', 'S', 'now', 'w'], text))


def filtered(args: list[str], stdout: str) -> str:
    if args[0] == 'graphv':
        keep = [line for line in stdout.splitlines()
                if line.startswith(('graph_start', 'graph_end', 'print['))]
        return '\n'.join(keep)
    if args[0] == 'dump':
        keep = [line.strip() for line in stdout.splitlines()
                if '<!--' in line and ' / ' in line]
        return '\n'.join(keep)
    return stdout


def run_steps(binary: str, fixture: Path, steps: list[list[str]], tz: str,
              pre: list[str] | None = None) -> list[dict]:
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp) / 'w'
        shutil.copytree(fixture, work)
        env = {k: v for k, v in os.environ.items()
               if k not in ('RRDCACHED_ADDRESS', 'TZ')}
        env.update(TZ=tz, LC_ALL='C')
        results = []
        for args in steps:
            proc = subprocess.run((pre or []) + [binary] + args, cwd=work,
                                  env=env, capture_output=True, text=True,
                                  errors='replace')
            stderr = proc.stderr
            if pre:
                # libfaketime makes rrdtool fault while exiting after an
                # error; rrdtool reports it but keeps its exit status.
                stderr = stderr.replace('Caught Segmentation fault\n', '')
            if proc.returncode < 0:
                results.append({'status': None, 'crash': True})
                break
            results.append({'status': proc.returncode,
                            'stdout': filtered(args, proc.stdout),
                            'stderr': stderr})
        return results


def run_equiv(binary: str, fixture: Path, case: dict,
              pre: list[str] | None = None) -> tuple[bool, list, list]:
    """True when the case and its placeholder form agree. Retries when
    the clock moves between the date sample and the runs."""
    for _ in range(5):
        before = date_sample(case['tz'], pre or [])
        got = run_steps(binary, fixture, case['steps'], case['tz'], pre)
        same = [[fill(arg, before) for arg in step]
                for step in case['same_as']]
        want = run_steps(binary, fixture, same, case['tz'], pre)
        after = date_sample(case['tz'], pre or [])
        if before == after:
            return got == want, got, want
    return False, got, want


def setup(binary: str, fixture: Path) -> None:
    fixture.mkdir(parents=True, exist_ok=True)
    for command in FIXTURE:
        subprocess.run([binary] + command, cwd=fixture, check=True,
                       env=dict(os.environ, TZ='UTC', LC_ALL='C'))


def all_cases() -> list[dict]:
    cases = fixed_cases() + equiv_cases()
    counters: dict[str, int] = {}
    for case in cases:
        n = counters.get(case['group'], 0) + 1
        counters[case['group']] = n
        case['id'] = f'{case["group"]}-{n:03d}'
    return cases


def render(table: dict) -> str:
    """JSON with one vector per line, so a regenerated table diffs by
    vector."""
    def items(values: list) -> str:
        return ',\n'.join(json.dumps(value, ensure_ascii=False)
                          for value in values)
    return ('{\n'
            f'"comment": {json.dumps(table["comment"])},\n'
            f'"oracle": {json.dumps(table["oracle"])},\n'
            f'"zones": {json.dumps(table["zones"])},\n'
            f'"fixture": [\n{items(table["fixture"])}\n],\n'
            f'"vectors": [\n{items(table["vectors"])}\n]\n'
            '}\n')


def ordered(case: dict) -> dict:
    keys = ['id', 'group', 'tz', 'steps', 'same_as', 'tags', 'expect',
            'expect_platform']
    return {key: case[key] for key in keys if key in case}


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser()
    parser.add_argument('--rrdtool', default='rrdtool')
    parser.add_argument('--out', default=str(
        root / 'crates/rondi-cli/tests/data/time_spec_vectors.json'))
    parser.add_argument('--probe', nargs='+')
    parser.add_argument('--check')
    parser.add_argument('--record-platform', metavar='KEY')
    parser.add_argument('--platform-out')
    parser.add_argument('--merge-platform', nargs=2, metavar=('KEY', 'FILE'))
    parser.add_argument('--now-check', action='store_true')
    parser.add_argument('--only', help='print results of one group only')
    options = parser.parse_args()

    if options.merge_platform:
        key, source = options.merge_platform
        table = json.loads(Path(options.out).read_text())
        results = json.loads(Path(source).read_text())
        for case in table['vectors']:
            case.get('expect_platform', {}).pop(key, None)
            if case['id'] in results:
                case.setdefault('expect_platform', {})[key] = \
                    results[case['id']]
            if case.get('expect_platform') == {}:
                del case['expect_platform']
        table['vectors'] = [ordered(case) for case in table['vectors']]
        Path(options.out).write_text(render(table))
        print(f'{len(results)} {key} expectations merged')
        return

    with tempfile.TemporaryDirectory() as tmp:
        fixture = Path(tmp) / 'fixture'
        binary = options.rrdtool
        if options.check:
            link = Path(tmp) / 'bin' / 'rrdtool'
            link.parent.mkdir()
            link.symlink_to(Path(options.check).resolve())
            binary = str(link)
        setup(binary, fixture)

        if options.probe:
            zone, start, *rest = options.probe
            print(json.dumps(run_steps(binary, fixture, [
                graph(start, rest[0] if rest else 'start+1')], zone)))
            return

        if options.check or options.record_platform:
            table = json.loads(Path(options.out).read_text())
            results = {}
            diverged = 0
            for case in table['vectors']:
                if {'undefined', 'unstable', 'dst-overlap'} & set(case['tags']):
                    continue
                if 'same_as' in case:
                    ok, got, want = run_equiv(binary, fixture, case)
                    if not ok:
                        diverged += 1
                        print(json.dumps({'id': case['id'], 'got': got,
                                          'same_as': want}))
                    continue
                got = run_steps(binary, fixture, case['steps'], case['tz'])
                if got != case['expect']:
                    diverged += 1
                    results[case['id']] = got
                    if options.check:
                        print(json.dumps({'id': case['id'], 'tz': case['tz'],
                                          'steps': case['steps'][-1][:6],
                                          'expect': case['expect'],
                                          'got': got}, ensure_ascii=False))
            if options.record_platform:
                Path(options.platform_out).write_text(
                    json.dumps(results, indent=1, sort_keys=True,
                               ensure_ascii=False) + '\n')
            print(f'{diverged} divergent of {len(table["vectors"])}',
                  file=sys.stderr)
            return

        if options.now_check:
            table = json.loads(Path(options.out).read_text())
            instants = ['2024-03-03 10:20:30', '2024-07-10 23:59:58',
                        '2025-03-09 01:30:00', '2025-11-02 01:30:00',
                        '2026-01-01 00:00:05', '2025-04-06 02:15:00']
            bad = 0
            for case in table['vectors']:
                if {'undefined', 'unstable', 'dst-overlap'} & set(case['tags']):
                    continue
                for instant in instants:
                    pre = ['faketime', '-f', f'@{instant}']
                    if 'same_as' in case:
                        ok, got, want = run_equiv(binary, fixture, case, pre)
                    else:
                        want = run_steps(binary, fixture, case['steps'],
                                         case['tz'])
                        got = run_steps(binary, fixture, case['steps'],
                                        case['tz'], pre)
                        ok = got == want
                    if not ok:
                        bad += 1
                        print(json.dumps({'id': case['id'], 'tz': case['tz'],
                                          'at': instant, 'got': got,
                                          'want': want}, ensure_ascii=False))
                        break
            print(f'{bad} cases depend on the current time', file=sys.stderr)
            sys.exit(1 if bad else 0)

        version = subprocess.run([binary, '--version'], capture_output=True,
                                 text=True).stdout
        if not version.startswith('RRDtool 1.11.0 '):
            sys.exit(f'expected RRDtool 1.11.0, found {version[:40]!r}')
        vectors = []
        broken = []
        for case in all_cases():
            if options.only and case['group'] != options.only:
                continue
            if 'same_as' in case:
                first = run_equiv(binary, fixture, case)
                second = run_equiv(binary, fixture, case)
                if not (first[0] and second[0]):
                    broken.append(f'{case["id"]} {case["tz"]} '
                                  f'{case["steps"][0][3]!r}: '
                                  f'{json.dumps(first[1:])}')
            else:
                case['expect'] = run_steps(binary, fixture, case['steps'],
                                           case['tz'])
                again = run_steps(binary, fixture, case['steps'], case['tz'])
                if any(step.get('crash') for step in case['expect']):
                    case['tags'].append('undefined')
                elif again != case['expect']:
                    case['tags'].append('unstable')
            vectors.append(ordered(case))
            if options.only:
                print(json.dumps(ordered(case), ensure_ascii=False))
        if broken:
            sys.exit('equivalences that do not hold:\n' + '\n'.join(broken))
        if options.only:
            return
        table = {
            'comment': 'Generated by scripts/time-spec-vectors.py from '
                       'RRDtool 1.11.0; see docs/compatibility/time-spec.md.',
            'oracle': version.splitlines()[0],
            'zones': ZONES,
            'fixture': FIXTURE,
            'vectors': vectors,
        }
        Path(options.out).parent.mkdir(parents=True, exist_ok=True)
        Path(options.out).write_text(render(table))
        print(f'{len(vectors)} vectors written to {options.out}')


if __name__ == '__main__':
    main()
