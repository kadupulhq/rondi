#!/usr/bin/env python3
"""Record the `rrdtool update` and `updatev` compatibility vectors.

The vectors live in crates/rondi-cli/tests/data/update and are replayed by
crates/rondi-cli/tests/update_spec_vectors.rs. The behavior they pin is
described in docs/compatibility/update.md; each case names the UP rules it
pins.

    python3 scripts/gen-update-vectors.py [--rrdtool PATH]
        Run every case against the reference rrdtool and rewrite cases.json
        and expected/update.vectors. Seed files under inputs/ are created
        only when missing, so a rerun on the same platform changes nothing.
        The seeds in SEED_PATCHES have header words changed after `create`
        to model damaged files; cases.json lists those patches.

    python3 scripts/gen-update-vectors.py --reseed [--rrdtool PATH]
        Recreate every seed file first. `create` picks a random current row,
        so every expected file changes too.

    python3 scripts/gen-update-vectors.py --linux-overrides [--rrdtool PATH]
        Replay the committed seeds on Linux and write
        expected/update.linux-<arch>.vectors with the cases whose recorded
        outcome differs from the macOS recording (after folding the x86_64
        negative quiet NaN). Run it inside a Linux container that has the
        pinned RRDtool 1.11.0 installed.

Each case copies one seed to `t.rrd` in an empty directory, runs its steps
there with TZ=UTC and LC_ALL=C, and records every step's exit status,
standard output (without the usage banner's build date) and standard error, then the final bytes of `t.rrd`
(`same` when unchanged). Cases that use `N` depend on the wall clock: their
output is recorded with every current-time number masked and their file is
not recorded. Every case is run twice; the script fails if the two runs
differ.

Only the standard library is used.
"""

import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "crates", "rondi-cli", "tests", "data", "update")
ENV = {"TZ": "UTC", "LC_ALL": "C", "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
# A current-time number: ten digits 1700000000..1999999999 not adjacent to
# an ASCII letter, digit or underscore (the Rust test uses the same rule).
NOW = re.compile(rb"(?<![A-Za-z0-9_])1[7-9][0-9]{8}(?![A-Za-z0-9_])")

# --------------------------------------------------------------------------
# Seeds. Each is created once by the reference; the bytes are frozen because
# `create` picks a random current row.

SEEDS = {
    # One GAUGE, one archive per step.
    "g1": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:5"],
    # Two GAUGEs, a per-step and a two-step archive.
    "g2": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:600:U:U", "DS:b:GAUGE:600:U:U",
           "RRA:AVERAGE:0.5:1:5", "RRA:MAX:0.5:2:5"],
    # Three GAUGEs for templates.
    "g3": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:600:U:U", "DS:b:GAUGE:600:U:U", "DS:cc:GAUGE:600:U:U",
           "RRA:AVERAGE:0.5:1:5"],
    # Every consolidation function over three steps, plus a per-step one.
    "cf": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:25:U:U", "DS:b:GAUGE:600:U:U",
           "RRA:AVERAGE:0.5:3:4", "RRA:MIN:0.5:3:4", "RRA:MAX:0.5:3:4",
           "RRA:LAST:0.5:3:4", "RRA:AVERAGE:0.5:1:6"],
    # The xff limit at several values.
    "xff": ["--start", "1000000000", "--step", "10", "DS:a:GAUGE:15:U:U",
            "RRA:AVERAGE:0:3:4", "RRA:AVERAGE:0.34:3:4", "RRA:AVERAGE:0.99:3:4",
            "RRA:LAST:0.5:3:4", "RRA:MAX:0.67:3:4"],
    # Range limits on GAUGE and ABSOLUTE.
    "mm": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:600:0:100", "DS:b:ABSOLUTE:600:U:5",
           "RRA:AVERAGE:0.5:1:5"],
    # Every non-computed type (file version 0005).
    "types": ["--start", "1000000000", "--step", "10",
              "DS:g:GAUGE:600:U:U", "DS:c:COUNTER:600:U:U",
              "DS:d:DERIVE:600:U:U", "DS:ab:ABSOLUTE:600:U:U",
              "DS:dc:DCOUNTER:600:U:U", "DS:dd:DDERIVE:600:U:U",
              "RRA:AVERAGE:0.5:1:5"],
    # One COUNTER, a long step so updates stay in one interval.
    "cnt": ["--start", "1000000000", "--step", "300",
            "DS:c:COUNTER:600:U:U", "RRA:AVERAGE:0.5:1:5"],
    "der": ["--start", "1000000000", "--step", "300",
            "DS:d:DERIVE:600:U:U", "RRA:AVERAGE:0.5:1:5"],
    "dcn": ["--start", "1000000000", "--step", "300",
            "DS:dc:DCOUNTER:600:U:U", "DS:dd:DDERIVE:600:U:U",
            "RRA:AVERAGE:0.5:1:5"],
    # COUNTER with range limits.
    "cmm": ["--start", "1000000000", "--step", "300",
            "DS:c:COUNTER:600:0:1", "RRA:AVERAGE:0.5:1:5"],
    # Unaligned start (initial unknown seconds 7).
    "un": ["--start", "1000000007", "--step", "10",
           "DS:a:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:6", "RRA:AVERAGE:0.5:4:3",
           "RRA:LAST:0.5:4:3"],
    # A short heartbeat.
    "hb": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:5:U:U", "DS:b:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:5"],
    # Long gaps: row capping and wrap.
    "cap": ["--start", "1000000000", "--step", "10",
            "DS:a:GAUGE:100000:U:U",
            "RRA:AVERAGE:0.5:1:3", "RRA:AVERAGE:0.5:2:2", "RRA:MAX:0.5:5:2",
            "RRA:LAST:0.5:1:1"],
    # Odd step and start for apportioning.
    "s7": ["--start", "1000000003", "--step", "7",
           "DS:a:GAUGE:100:U:U", "DS:b:ABSOLUTE:100:U:U",
           "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.5:4:5", "RRA:MAX:0.5:4:5",
           "RRA:MIN:0.5:4:5", "RRA:LAST:0.5:4:5"],
    # A computed source after a GAUGE (CP-12 to CP-16).
    "cmp": ["--start", "1000000000", "--step", "10",
            "DS:a:GAUGE:600:U:U", "DS:c:COMPUTE:a,2,*", "DS:d:COMPUTE:a,0,/",
            "RRA:AVERAGE:0.5:1:5"],
    # Floating-point order pins found by search (see update.md section 12).
    "fp10": ["--start", "1000000009", "--step", "10", "DS:a:GAUGE:200:U:U",
             "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.5:3:5"],
    "fp300": ["--start", "1000000010", "--step", "300", "DS:a:GAUGE:6000:U:U",
              "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.5:3:5"],
    "fp7": ["--start", "1000000007", "--step", "10", "DS:a:GAUGE:200:U:U",
            "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.5:3:5"],
    "fpden": ["--start", "1000000117", "--step", "300", "DS:a:GAUGE:300:-5:50",
              "RRA:AVERAGE:0.5:1:3"],
    # Signed zeros through every consolidation function.
    "zero": ["--start", "1000000000", "--step", "10", "DS:a:GAUGE:600:U:U",
             "RRA:AVERAGE:0.5:1:5", "RRA:AVERAGE:0.5:2:5", "RRA:MAX:0.5:2:5",
             "RRA:LAST:0.5:2:5", "RRA:MIN:0.5:2:5"],
    # GAUGE bounds equal to the readings.
    "m3": ["--start", "1000000000", "--step", "10",
           "DS:a:GAUGE:600:U:0.1", "DS:b:GAUGE:600:0.7:U",
           "RRA:AVERAGE:0.5:1:10"],
    "fplead": ["--start", "1000000010", "--step", "10", "DS:a:GAUGE:600:U:U",
               "RRA:AVERAGE:0.5:1:10"],
    # A start far in the future, for `N` before the last update.
    "fut": ["--start", "4000000000", "--step", "10",
            "DS:a:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:5"],
}


def U(*args):
    return ["update", "t.rrd", *args]


def V(*args):
    return ["updatev", "t.rrd", *args]


# (id, rules, expect, seed, steps). `expect` is `same` or `now`.
CASES = [
    # --- command line and options -------------------------------------------
    ("arg-usage", ["UP-1"], "same", "g1", [["update"]]),
    ("arg-usage-v", ["UP-1"], "same", "g1", [["updatev"]]),
    ("arg-none", ["UP-1"], "same", "g1", [U()]),
    ("arg-none-v", ["UP-1", "UP-31"], "same", "g1", [V()]),
    ("arg-options-only", ["UP-1", "UP-2"], "same", "g3", [U("-t", "zz")]),
    ("arg-missing", ["UP-3"], "same", "g1",
     [["update", "missing.rrd", "1000000010:1"]]),
    ("arg-missing-v", ["UP-3", "UP-31"], "same", "g1",
     [["updatev", "missing.rrd", "1000000010:1"]]),
    ("arg-not-rrd", ["UP-3"], "same", "g1",
     [["update", "cases.txt", "1000000010:1"]]),
    ("arg-long-options", ["UP-2"], "same", "g3",
     [U("--template=cc", "1000000010:7"),
      U("--skip-past-updates", "1000000005:1:2:3", "1000000020:1:2:3")]),
    ("arg-option-after", ["UP-2"], "same", "g3",
     [U("1000000010:1:2:3", "-t", "b", "1000000020:4")]),
    # --- templates ----------------------------------------------------------
    ("tpl-one", ["UP-4", "UP-5"], "same", "g3", [U("-t", "b", "1000000010:5")]),
    ("tpl-reorder", ["UP-4", "UP-5"], "same", "g3",
     [U("-t", "cc:b:a", "1000000010:1:2:3")]),
    ("tpl-duplicate", ["UP-5"], "same", "g3", [U("-t", "b:b", "1000000010:5:6")]),
    ("tpl-last-wins", ["UP-2", "UP-4"], "same", "g3",
     [U("-t", "a", "-t", "b", "1000000010:9")]),
    ("tpl-unknown", ["UP-4"], "same", "g3", [U("-t", "zz", "1000000010:5")]),
    ("tpl-empty", ["UP-4"], "same", "g3", [U("-t", "", "1000000010:5")]),
    ("tpl-trailing-colon", ["UP-4"], "same", "g3",
     [U("-t", "a:", "1000000010:1:2")]),
    ("tpl-too-many", ["UP-4"], "same", "g3",
     [U("-t", "a:b:cc:a", "1000000010:1:2:3:4")]),
    ("tpl-unknown-first", ["UP-4"], "same", "g2",
     [U("-t", "zz:a:b:a", "1000000010:5")]),
    ("tpl-count-before-name", ["UP-4"], "same", "g2",
     [U("-t", "a:b:zz", "1000000010:5")]),
    ("tpl-count-mismatch", ["UP-4", "UP-7"], "same", "g3",
     [U("-t", "b:a", "1000000010:5"), U("-t", "b", "1000000010:5:6")]),
    ("tpl-after-open", ["UP-3", "UP-4"], "same", "g3",
     [["update", "missing.rrd", "-t", "zz", "1000000010:1"],
      ["updatev", "missing.rrd", "-t", "zz"]]),
    ("tpl-updatev-error", ["UP-4", "UP-31"], "same", "g3",
     [V("-t", "zz", "1000000010:1")]),
    ("tpl-updatev", ["UP-4", "UP-31"], "same", "g2",
     [V("-t", "b", "1000000025:4")]),
    # --- argument splitting and counts ---------------------------------------
    ("split-no-separator", ["UP-6"], "same", "g2",
     [U("1000000015"), U("abc")]),
    ("split-at", ["UP-6", "UP-8"], "same", "g2",
     [U("1000000012@5:6"), U("1000000030@1:2")]),
    ("split-at-extra-at", ["UP-6", "UP-7"], "same", "g2",
     [U("1000000012@5@6"), U("1000000013@5:6@7")]),
    ("split-at-wins", ["UP-6", "UP-7"], "same", "g2", [U("1000000014:5@6")]),
    ("split-at-fraction", ["UP-8"], "same", "g2", [U("1000000040.5@1:2")]),
    ("count-few", ["UP-7"], "same", "g2",
     [U("1000000010.5:5"), U("1000000016:"), U("abc:5")]),
    ("count-many", ["UP-7"], "same", "g2",
     [U("1000000011.25:5:6:7"), U("1000000005:1:2:3:4"), U("1000000005:1:2::"),
      U("1000000018:U:U:")]),
    ("count-empty-values", ["UP-7", "UP-11"], "same", "g2", [U("1000000017::")]),
    ("count-before-order", ["UP-7", "UP-9"], "same", "g2",
     [U("1000000019:1:2"), U("1000000005:1"), U("1000000005:1:2:3")]),
    ("time-parse", ["UP-8"], "same", "g2",
     [U("abc:5:6"), U("1000000010x:5:6"), U("+1000000010:5:6")]),
    ("time-before-value", ["UP-8", "UP-9", "UP-11"], "same", "g2",
     [U("1000000019:1:2"), U("1000000005:zz:1")]),
    # --- time order ---------------------------------------------------------
    ("order-equal", ["UP-9"], "same", "g2",
     [U("1000000019:1:2"), U("1000000019:1:2")]),
    ("order-before", ["UP-9"], "same", "g2",
     [U("1000000019:1:2"), V("1000000005:1:2")]),
    ("order-start", ["UP-9"], "same", "g2", [U("1000000000:1:2")]),
    ("time-microseconds", ["UP-8", "UP-14", "UP-33"], "same", "g1",
     [U("1000000001.0000005:1"), U("1000000002.9999999:2"),
      U("1000000003.4:3"), U("1000000004.000001:4")]),
    ("order-fraction", ["UP-9", "UP-14"], "same", "g1",
     [U("1000000004.5:1", "1000000004.7:2"), U("1000000004.6:3"),
      U("1000000004.7:3")]),
    ("skip-past", ["UP-9"], "same", "g2",
     [U("1000000019:1:2"),
      U("-s", "1000000005:1:2", "1000000020:1:2", "1000000006:1:2")]),
    ("skip-past-bad-value", ["UP-9", "UP-11"], "same", "g2",
     [U("1000000019:1:2"), U("-s", "1000000005:zz:1")]),
    ("skip-past-bad-count", ["UP-7", "UP-9"], "same", "g2",
     [U("1000000019:1:2"), U("-s", "1000000005:1")]),
    ("skip-past-bad-time", ["UP-8", "UP-9"], "same", "g2",
     [U("-s", "abc:1:2")]),
    ("skip-past-v", ["UP-9", "UP-31"], "same", "g2",
     [U("1000000019:1:2"), V("-s", "1000000001:1:2", "1000000031:3:4")]),
    ("now-ok", ["UP-8"], "now", "g1", [V("N:5")]),
    ("now-twice", ["UP-8", "UP-9"], "now", "g1", [U("N:5", "N:6")]),
    ("now-before-last", ["UP-8", "UP-9"], "now", "fut", [U("N:5")]),
    ("now-at", ["UP-8"], "now", "g1", [U("N@5")]),
    # --- values: syntax -----------------------------------------------------
    ("val-gauge-forms", ["UP-11", "UP-13"], "same", "g1",
     [U("1000000001:1e", "1000000002:1e+", "1000000003:.5", "1000000004:5.",
        "1000000005:+5", "1000000006:1e400", "1000000007:-1e400")]),
    ("val-gauge-specials", ["UP-11", "UP-15", "UP-26", "UP-31"], "same", "g2",
     [V("1000000006:nan:inf", "1000000016:-inf:1", "1000000026:1:1",
        "1000000046:1:1")]),
    ("val-gauge-nan-text", ["UP-11"], "same", "g1",
     [U("1000000001:-nan", "1000000002:NaN", "1000000003:INF")]),
    ("val-gauge-reject", ["UP-11"], "same", "g1",
     [U("1000000001:+inf"), U("1000000002:e5"), U("1000000003:."),
      U("1000000004:-"), U("1000000005:+"), U("1000000006:unknown"),
      U("1000000007:0x10"), U("1000000008:1,5"), U("1000000009:12abc"),
      U("1000000010: 5")]),
    ("val-out-of-range", ["UP-11"], "same", "g1",
     [U("1000000001:1e2000"), U("1000000001:1e1025"), U("1000000001:1e-1022"),
      U("1000000001:1e1024")]),
    ("val-tiny-and-zero-scale", ["UP-11"], "same", "g1",
     [U("1000000001:1e-400"), U("1000000002:0e600")]),
    ("val-unknown-prefix", ["UP-10", "UP-13"], "same", "types",
     [U("1000000010:Uxyz:Unknown:U-1:U:Ufoo:Ubar")]),
    ("val-absolute-reject", ["UP-11"], "same", "types",
     [U("1000000010:1:1:1:zz:1:1"), U("1000000010:1:1:1:2x:1:1")]),
    ("val-counter-reject", ["UP-12"], "same", "types",
     [U("1000000010:1:1.5:1:1:1:1"), U("1000000010:1:-5:1:1:1:1"),
      U("1000000010:1:+5:1:1:1:1"), U("1000000010:1:1e3:1:1:1:1"),
      U("1000000010:1:0x10:1:1:1:1"), U("1000000010:1:-0:1:1:1:1"),
      U("1000000010:1:12abc:1:1:1:1"), U("1000000010:1:inf:1:1:1:1"),
      U("1000000010:1:u:1:1:1:1")]),
    ("val-derive-reject", ["UP-12"], "same", "types",
     [U("1000000010:1:1:+5:1:1:1"), U("1000000010:1:1:-+5:1:1:1"),
      U("1000000010:1:1:--5:1:1:1"), U("1000000010:1:1:1e3:1:1:1"),
      U("1000000010:1:1:1.0:1:1:1")]),
    ("val-integer-accept", ["UP-12", "UP-16"], "same", "types",
     [U("1000000010:1::-:1:1:1", "1000000020:1:0:-0:1:1:1")]),
    ("val-heartbeat-no-parse", ["UP-10"], "same", "types",
     [U("1000000700:zz:zz:zz:zz:zz:zz")]),
    ("val-dcounter-after-unknown", ["UP-17"], "same", "dcn",
     [U("1000000010:U:U", "1000000020:12abc:zz", "1000000030:13:14")]),
    ("val-dcounter-new-first", ["UP-17"], "same", "dcn",
     [U("1000000010:U:U", "1000000020:12abc:1"), U("1000000030:zz:1")]),
    ("val-dcounter-bad-new", ["UP-17"], "same", "dcn",
     [U("1000000010:5:5"), U("1000000020:zz:5"), U("1000000030:5:5x")]),
    ("val-partial-last-ds", ["UP-13", "UP-34"], "same", "g3",
     [U("1000000010:1:2:3"), U("1000000020:7:zz:9")]),
    ("val-partial-counter", ["UP-13", "UP-34"], "same", "types",
     [U("1000000010:1:1:1:1:1:1"), U("1000000020:7:zz:1:1:1:1")]),
    ("val-last-ds-truncate", ["UP-13"], "same", "g1",
     [U("1000000010:1.000000000000000000000000000000000000001",
        "1000000020:2.0000000000000000000000000000000000000000000000000001e0")]),
    ("val-last-ds-shorter", ["UP-13"], "same", "g2",
     [U("1000000010:123456.789:U", "1000000020:7:1")]),
    # --- rates by type ------------------------------------------------------
    ("rate-gauge", ["UP-14", "UP-15", "UP-19"], "same", "g1",
     [U("1000000002:3", "1000000005:4", "1000000009.5:5")]),
    ("rate-absolute", ["UP-15", "UP-19"], "same", "types",
     [U("1000000002:1:1:1:30:1:1", "1000000007:1:1:1:7.5:1:1")]),
    ("rate-counter-basic", ["UP-16"], "same", "cnt",
     [U("1000000010:100", "1000000020:200", "1000000030:200",
        "1000000040:12345678901234567890")]),
    ("rate-counter-wrap32", ["UP-16"], "same", "cnt",
     [U("1000000010:4294967290", "1000000020:5", "1000000030:4294967296",
        "1000000040:5", "1000000050:200", "1000000060:100")]),
    ("rate-counter-wrap-edges", ["UP-16"], "same", "cnt",
     [U("1000000010:4294967295", "1000000020:0", "1000000030:8589934592",
        "1000000040:0", "1000000050:18446744073709551615", "1000000060:5")]),
    ("rate-counter-wrap64", ["UP-16"], "same", "cnt",
     [U("1000000010:18446744073709551615", "1000000020:18446744073709551614")]),
    ("rate-counter-big", ["UP-16"], "same", "cnt",
     [U("1000000010:0", "1000000020:9007199254740993",
        "1000000030:123456789012345678901", "1000000040:123456789012345678999",
        "1000000050:0", "1000000060:12345678901234567890123")]),
    ("rate-counter-long", ["UP-13", "UP-16"], "same", "cnt",
     [U("1000000010:7", "1000000020:1111111111111111111111111111111",
        "1000000030:9", "1000000040:222222222222222222222222222222",
        "1000000050:333333333333333333333333333333")]),
    ("rate-counter-zeros", ["UP-13", "UP-16"], "same", "cnt",
     [U("1000000010:000000000000000000000000000001",
        "1000000020:000000000000000000000000000003", "1000000030:0007",
        "1000000040:9")]),
    ("rate-counter-empty", ["UP-16"], "same", "cnt",
     [U("1000000010:7", "1000000020:", "1000000030:9", "1000000040:12")]),
    ("rate-counter-range", ["UP-16", "UP-18"], "same", "cmm",
     [U("1000000010:0", "1000000020:5", "1000000030:15", "1000000040:16",
        "1000000050:15")]),
    ("rate-derive", ["UP-16"], "same", "der",
     [U("1000000010:100", "1000000020:200", "1000000030:100",
        "1000000040:-5", "1000000050:-3", "1000000060:-5", "1000000070:-5")]),
    ("rate-derive-signs", ["UP-16"], "same", "der",
     [U("1000000010:3", "1000000020:-5", "1000000030:4", "1000000040:-0",
        "1000000050:-0", "1000000060:0", "1000000070:-", "1000000080:-5")]),
    ("rate-derive-zero", ["UP-16"], "same", "der",
     [U("1000000010:-5", "1000000020:-5", "1000000030:5", "1000000040:5")]),
    ("rate-dcounter", ["UP-17"], "same", "dcn",
     [U("1000000010:10:10", "1000000020:12.5:12.5", "1000000030:15:15",
        "1000000040:14:14", "1000000050:11:11", "1000000060:12:12",
        "1000000070:12.25:1e3")]),
    ("rate-dcounter-down", ["UP-17"], "same", "dcn",
     [U("1000000010:20:20", "1000000020:18:18", "1000000030:16:16",
        "1000000040:17:17")]),
    ("rate-dcounter-fraction", ["UP-14", "UP-17"], "same", "dcn",
     [U("1000000010.25:0.1:0.1", "1000000020.5:0.3:0.7",
        "1000000033.125:0.7:0.2")]),
    ("rate-all-types", ["UP-15", "UP-16", "UP-17", "UP-31", "UP-33"], "same",
     "types",
     [V("1000000005:1:100:100:5:1.5:1.5", "1000000017:2:220:90:6:2.5:0.5",
        "1000000031:3:221:95:7:2.5:-1", "1000000064:4:300:100:8:9:9")]),
    ("range-gauge", ["UP-18"], "same", "mm",
     [V("1000000010:-1:1", "1000000020:0:5", "1000000030:100:50",
        "1000000040:100.000001:51", "1000000050:50:0")]),
    ("range-gauge-reading", ["UP-18"], "same", "m3",
     [V("1000000003:0.1:0.7"), V("1000000005:0.1000001:0.6999999")]),
    ("range-absolute-interval", ["UP-18"], "same", "mm",
     [V("1000000005:1:25", "1000000010:1:26", "1000000030:1:100",
        "1000000040:1:51")]),
    # --- heartbeat ----------------------------------------------------------
    ("hb-equal", ["UP-10"], "same", "hb",
     [V("1000000005:1:1", "1000000010:2:2", "1000000015.000001:3:3",
        "1000000020:4:4")]),
    ("hb-long-gap", ["UP-10", "UP-24"], "same", "hb",
     [V("1000000002:1:1", "1000000037:2:2", "1000000039:3:3")]),
    ("hb-split-subinterval", ["UP-22", "UP-24"], "same", "hb",
     [V("1000000004:1:1", "1000000029:2:2")]),
    ("hb-split-known-part", ["UP-22", "UP-24"], "same", "hb",
     [V("1000000003:1:1", "1000000006:1:1", "1000000008:1:1"),
      V("1000000031:5:5")]),
    # --- one interval, no boundary -----------------------------------------
    ("acc-unknown", ["UP-19"], "same", "g2",
     [U("1000000002:U:1", "1000000004.5:2:U", "1000000007.25:U:U",
        "1000000009.999999:3:4")]),
    ("acc-first-known", ["UP-19"], "same", "un",
     [U("1000000008:5", "1000000009:6")]),
    # --- one boundary -------------------------------------------------------
    ("one-step", ["UP-20", "UP-21", "UP-25", "UP-28"], "same", "g1",
     [V("1000000004:2", "1000000013:3", "1000000027:4")]),
    ("one-step-unknown", ["UP-21", "UP-24", "UP-25"], "same", "g2",
     [V("1000000004:U:1", "1000000013:3:U", "1000000027:4:4")]),
    ("one-step-half-unknown", ["UP-24"], "same", "g2",
     [V("1000000005:U:U", "1000000012:1:1"),
      V("1000000016:U:U", "1000000022:1:1")]),
    ("one-step-start-unknown", ["UP-24"], "same", "un",
     [V("1000000012:5", "1000000019.5:6", "1000000021:7")]),
    ("one-step-fraction", ["UP-14", "UP-20", "UP-21", "UP-25"], "same", "g1",
     [V("1000000004.25:2", "1000000013.75:3", "1000000020.000001:4")]),
    ("one-step-exact", ["UP-21"], "same", "g1",
     [V("1000000010:2", "1000000020:3", "1000000030.5:4")]),
    # --- several boundaries -------------------------------------------------
    ("multi-aligned", ["UP-20", "UP-23", "UP-29"], "same", "g2",
     [V("1000000010:1:1", "1000000045:2:3")]),
    ("multi-split", ["UP-20", "UP-22", "UP-28", "UP-29"], "same", "g2",
     [V("1000000013:1:1", "1000000047:2:3")]),
    ("multi-split-unknown", ["UP-22", "UP-25"], "same", "g2",
     [V("1000000013:1:1", "1000000047:U:3", "1000000068:4:U")]),
    ("multi-lead-under-one", ["UP-20", "UP-23"], "same", "g1",
     [V("1000000009.5:1", "1000000035:2")]),
    ("multi-fraction-split", ["UP-22", "UP-32"], "same", "g2",
     [V("1000000013.5:1:1", "1000000047.25:2:3", "1000000061:4:4")]),
    ("multi-fraction-one", ["UP-21", "UP-32"], "same", "g2",
     [V("1000000013.5:1:1", "1000000021:2:3")]),
    ("multi-start-unknown", ["UP-22", "UP-24"], "same", "un",
     [V("1000000013:1", "1000000039:2")]),
    ("multi-s7", ["UP-22", "UP-26", "UP-29"], "same", "s7",
     [V("1000000005.5:3:1", "1000000050.75:4:9", "1000000061:5:2",
        "1000000099.3:U:3", "1000000140:6:U")]),
    ("multi-s7-aligned", ["UP-23", "UP-29"], "same", "s7",
     [V("1000000010:1:1", "1000000080:2:30", "1000000101:3:3")]),
    # --- consolidation ------------------------------------------------------
    ("cf-basic", ["UP-26", "UP-27", "UP-28"], "same", "cf",
     [V("1000000010:1:1", "1000000020:5:2", "1000000030:3:3",
        "1000000040:2:4", "1000000050:U:5", "1000000060:7:6")]),
    ("cf-last-unknown", ["UP-27"], "same", "cf",
     [V("1000000010:1:1", "1000000020:5:2", "1000000030:U:3",
        "1000000040:2:4", "1000000050:3:U", "1000000060:4:U")]),
    ("cf-bulk", ["UP-26", "UP-29"], "same", "cf",
     [V("1000000010:1:1", "1000000080:2:2", "1000000200:3:3")]),
    ("cf-bulk-unknown", ["UP-27", "UP-29"], "same", "cf",
     [V("1000000010:1:1", "1000000020:2:2", "1000000075:3:U",
        "1000000081:4:4")]),
    ("xff-limits", ["UP-27"], "same", "xff",
     [V("1000000010:1", "1000000020:U", "1000000030:3", "1000000040:U",
        "1000000050:U", "1000000060:6", "1000000070:U", "1000000080:U",
        "1000000090:U", "1000000100:1", "1000000110:2", "1000000120:3")]),
    ("xff-bulk", ["UP-27", "UP-29"], "same", "xff",
     [V("1000000010:1", "1000000080:2", "1000000101:U", "1000000140:4")]),
    ("cdp-alignment", ["UP-28", "UP-29"], "same", "un",
     [V("1000000013:1", "1000000024:2", "1000000037:3", "1000000101:4")]),
    # --- rows: capping, wrap, timestamps -------------------------------------
    ("rows-wrap", ["UP-28", "UP-29", "UP-30"], "same", "cap",
     [V("1000000010:1", "1000000020:2", "1000000030:3", "1000000040:4",
        "1000000050:5", "1000000060:6", "1000000070:7")]),
    ("rows-capped", ["UP-29", "UP-30", "UP-32"], "same", "cap",
     [V("1000000010:1", "1000000250:2")]),
    ("rows-capped-split", ["UP-29", "UP-32"], "same", "cap",
     [V("1000000013:1", "1000000247:2", "1000000253:3")]),
    ("rows-capped-long", ["UP-29", "UP-30"], "same", "cap",
     [V("1000000010:1", "1000100010:2", "1000100015.5:3",
        "1000200020.25:4")]),
    ("rows-secondary-times", ["UP-31", "UP-32"], "same", "s7",
     [V("1000000010:1:1", "1000000300:2:2")]),
    # --- multiple arguments -------------------------------------------------
    ("multi-arg-fail", ["UP-34", "UP-31"], "same", "g2",
     [V("1000000025:3:3", "1000000031:zz:1", "1000000041:5:5")]),
    ("multi-arg-fail-order", ["UP-34", "UP-9"], "same", "g2",
     [U("1000000025:3:3", "1000000020:1:1", "1000000041:5:5")]),
    ("multi-arg-fail-count", ["UP-34", "UP-7"], "same", "g2",
     [U("1000000050.7:1:2", "1000000051:1")]),
    ("multi-arg-ok", ["UP-34"], "same", "g2",
     [U("1000000005:1:1", "1000000012:2:2", "1000000031:3:3",
        "1000000032.5:4:4")]),
    # --- updatev details ----------------------------------------------------
    ("updatev-none", ["UP-31"], "same", "g1", [V("1000000005:1")]),
    ("updatev-error", ["UP-31"], "same", "g1",
     [V("1000000005:1"), V("1000000005:1")]),
    ("updatev-formats", ["UP-31"], "same", "g2",
     [V("1000000010:0:-0", "1000000020:1e-300:1.23456789012345e300",
        "1000000030:-123.456789:0.000001234567890123")]),
    ("zero-aligned", ["UP-19", "UP-21", "UP-26"], "same", "zero",
     [V("1000000010:-0"), V("1000000020:-0", "1000000030:-0"),
      V("1000000060:-0")]),
    ("zero-split", ["UP-19", "UP-22", "UP-26"], "same", "zero",
     [V("1000000003:-0", "1000000005:-0"), V("1000000037:-0"),
      V("1000000040:-0")]),
    # --- computed sources ---------------------------------------------------
    ("compute-basic", ["UP-36"], "same", "cmp",
     [V("1000000006:3", "1000000016:4", "1000000026:U")]),
    ("compute-template", ["UP-36"], "same", "cmp",
     [U("-t", "c", "1000000030:5"), U("1000000031:1:2:3"),
      U("1000000032:1")]),
    # --- floating-point order pins (section 12) -----------------------------
    ("fp-interval", ["UP-14", "UP-35"], "same", "fp10",
     [V("1000000043.771754:9.38595867742349e-06"),
      V("1000000044.771754:0.0005911534350013039")]),
    ("fp-fused-first", ["UP-22", "UP-35"], "same", "fp300",
     [V("1000000529.692611:0.05447021635789045"),
      V("1000001338.692611:575.84596")]),
    ("fp-fused-average", ["UP-26", "UP-35"], "same", "fp7",
     [V("1000000021.931505:69.8"), V("1000000079.237859:635.9725")]),
    ("fp-lead-ratio", ["UP-22", "UP-35"], "same", "fplead",
     [V("1000000014.295626:4.094", "1000000069:369.25357")]),
    ("fp-unknown-denominator", ["UP-21", "UP-35"], "same", "fpden",
     [V("1000017240:U"), V("1000017389:607"),
      V("1000017505.795224:0.00038331942765209925"),
      V("1000017646.606297:-43")]),
]


# --------------------------------------------------------------------------
# Random sequences. They exercise the arithmetic of several rules at once and
# use a fixed seed, so the case list is stable.

def random_cases():
    import random

    out = []
    seeds = {}
    for number in range(12):
        rng = random.Random(4100 + number)
        step = rng.choice([1, 5, 7, 10, 60, 300])
        start = 1000000000 + rng.randint(0, 3 * step)
        types = ["GAUGE", "COUNTER", "DERIVE", "ABSOLUTE", "DCOUNTER", "DDERIVE"]
        dss = []
        for i in range(rng.randint(1, 3)):
            kind = rng.choice(types if number % 2 else ["GAUGE", "ABSOLUTE"])
            heartbeat = rng.choice([step, 2 * step, 3 * step + 1, 10 * step,
                                    100 * step, max(1, step // 2)])
            low = rng.choice(["U", "U", "0", "-5"])
            high = rng.choice(["U", "U", "1000", "1e9"])
            dss.append((kind, f"DS:d{i}:{kind}:{heartbeat}:{low}:{high}"))
        rras = []
        for _ in range(rng.randint(1, 4)):
            cf = rng.choice(["AVERAGE", "MIN", "MAX", "LAST"])
            pdps = rng.choice([1, 1, 2, 3, 4, 5, 12])
            rows = rng.choice([1, 2, 3, 5, 8])
            xff = rng.choice(["0", "0.1", "0.5", "0.75", "0.99"])
            rras.append(f"RRA:{cf}:{xff}:{pdps}:{rows}")
        name = f"rnd{number:02d}"
        seeds[name] = (["--start", str(start), "--step", str(step)]
                       + [d[1] for d in dss] + rras)
        time = float(start)
        counters = [rng.randint(0, 10**6) for _ in dss]
        steps = []
        for _ in range(rng.randint(2, 4)):
            args = []
            for _ in range(rng.randint(1, 4)):
                roll = rng.random()
                if roll < 0.45:
                    time += rng.randint(1, 3 * step)
                elif roll < 0.7:
                    time += rng.uniform(0.001, 2 * step)
                elif roll < 0.9:
                    time += rng.randint(1, 40 * step)
                else:
                    time += rng.randint(1, 3)
                stamp = str(int(time)) if time == int(time) else f"{time:.6f}"
                values = []
                for j, (kind, _) in enumerate(dss):
                    if rng.random() < 0.1:
                        values.append("U")
                    elif kind == "GAUGE":
                        values.append(repr(rng.choice([
                            rng.randint(-100, 100),
                            round(rng.uniform(-1000, 1000), rng.randint(0, 9)),
                            rng.uniform(0, 1) * 10 ** rng.randint(-5, 10)])))
                    elif kind == "ABSOLUTE":
                        values.append(str(rng.choice([
                            rng.randint(0, 1000), round(rng.uniform(0, 500), 3)])))
                    elif kind in ("COUNTER", "DERIVE"):
                        counters[j] += rng.randint(-100 if kind == "DERIVE" else 0,
                                                   100000)
                        if rng.random() < 0.1:
                            counters[j] = rng.randint(0, 2**64 - 1)
                        counters[j] = max(0, counters[j])
                        values.append(str(counters[j]))
                    else:
                        counters[j] += rng.uniform(-10 if kind == "DDERIVE" else -0.5,
                                                   300)
                        values.append(repr(round(counters[j], rng.randint(0, 6))))
                args.append(stamp + ":" + ":".join(values))
            steps.append(V(*args))
        out.append((f"rnd-{number:02d}", ["UP-35"], "same", name, steps))
    return seeds, out


# --------------------------------------------------------------------------
# Later cases. They follow the random cases so that the earlier case list
# keeps its order.

LATER_SEEDS = {
    # A GAUGE and an ABSOLUTE for infinite amounts.
    "carry": ["--start", "1000000000", "--step", "10",
              "DS:a:GAUGE:600:U:U", "DS:b:ABSOLUTE:600:U:U",
              "RRA:AVERAGE:0.5:1:5", "RRA:LAST:0.5:2:3"],
    # Heartbeats long enough for gaps of 2^33 steps.
    "wide": ["--start", "1000000000", "--step", "10",
             "DS:a:GAUGE:1000000000000:U:U", "DS:b:ABSOLUTE:1000000000000:U:U",
             "RRA:AVERAGE:0.5:1:3", "RRA:AVERAGE:0.5:3:2", "RRA:MAX:0.5:2:2",
             "RRA:LAST:0.5:5:4"],
    # Damaged files; SEED_PATCHES changes them after `create`.
    "dmg": ["--start", "1000000000", "--step", "10",
            "DS:a:GAUGE:600:U:U", "DS:b:GAUGE:600:U:U", "DS:c:ABSOLUTE:15:U:U",
            "RRA:AVERAGE:0.5:1:3", "RRA:LAST:0.5:1:4"],
    "dmgshort": ["--start", "1000000000", "--step", "10",
                 "DS:a:GAUGE:600:U:U", "DS:b:GAUGE:600:U:U",
                 "DS:c:ABSOLUTE:15:U:U", "RRA:AVERAGE:0.5:1:3",
                 "RRA:LAST:0.5:1:4"],
    "dmgfirst": ["--start", "1000000000", "--step", "10", "DS:a:GAUGE:600:U:U",
                 "RRA:AVERAGE:0.5:1:3", "RRA:AVERAGE:0.5:2:3",
                 "RRA:MAX:0.5:1:2"],
    "curhigh": ["--start", "1000000000", "--step", "10", "DS:a:GAUGE:600:U:U",
                "RRA:AVERAGE:0.5:1:3", "RRA:AVERAGE:0.5:2:3",
                "RRA:LAST:0.5:1:4"],
}

# (field, archive, value) written after `create`: `rows` is the row count in
# the archive definition, `cur_row` the archive's current-row word. A row
# count of k + 2^61 makes the header's implied file size wrap back to the
# real size, so the file opens although the archive claims more rows than
# the file holds.
SEED_PATCHES = {
    "dmg": [("rows", 1, 4 + 2**61), ("cur_row", 1, 2)],
    "dmgshort": [("rows", 1, 5)],
    "dmgfirst": [("rows", 0, 3 + 2**61), ("cur_row", 0, 1000)],
    "curhigh": [("cur_row", 0, 7), ("cur_row", 1, 6),
                ("cur_row", 2, 2**64 - 1)],
}

# Gaps of k steps around the 32-bit limits of the step count.
STEP_GAPS = [("2p31m1", 2**31 - 1), ("2p31", 2**31), ("2p31p1", 2**31 + 1),
             ("2p32m1", 2**32 - 1), ("2p32", 2**32), ("2p32p1", 2**32 + 1),
             ("2p33p1", 2**33 + 1)]

LATER_CASES = [
    # DCOUNTER rejects a change whose sign differs from the new reading's;
    # DDERIVE accepts every change.
    ("rate-dcounter-signs", ["UP-17"], "same", "dcn",
     [V("1000000300:3:3", "1000000600:-5:-5", "1000000900:-3:-3",
        "1000001200:1:1", "1000001500:0:0", "1000001800:-2:-2",
        "1000002100:-0:-0", "1000002400:5:5", "1000002700:3:3",
        "1000003000:3:3")]),
    # A known infinite amount whose carried share is NaN.
    ("carry-nan-split", ["UP-25"], "same", "carry",
     [V("1000000003:1:1", "1000000027:inf:-inf", "1000000033:2:2")]),
    ("carry-nan-state", ["UP-25"], "same", "carry",
     [U("1000000003:1:1", "1000000027.5:inf:5")]),
    # Times at and beyond the range of whole seconds.
    ("time-beyond-range", ["UP-8", "UP-9"], "same", "g1",
     [U("1000000003:1"), U("9223372036854775807:U"), U("inf:U")]),
    ("time-beyond-range-v", ["UP-8", "UP-32"], "same", "g1",
     [V("9.22337203685478e18:U")]),
    ("time-saturated", ["UP-8"], "same", "g1", [U("1e19:U")]),
    ("time-nan", ["UP-8", "UP-9"], "same", "g1", [U("nan:1")]),
    # lead comes from W, so it changes when W is rounded.
    ("lead-rounded-w", ["UP-20", "UP-22", "UP-32"], "same", "wide",
     [V("1000000003.0000012:1:1", "41000000000:5:5")]),
    ("lead-huge-low", ["UP-20", "UP-22", "UP-32"], "same", "wide",
     [V("1000000003:1:1"), V("10000000000000000:5:7")]),
    ("lead-huge-high", ["UP-20", "UP-22", "UP-32"], "same", "wide",
     [V("1000000003:1:1"), V("10000000000000010:5:7")]),
    *[(f"steps-wrap-{label}-split",
       ["UP-20", "UP-22", "UP-23", "UP-32", "UP-37"], "same", "wide",
       [V("1000000003:1:1"), V(f"{1000000004 + 10 * k}:5:7")])
      for label, k in STEP_GAPS],
    *[(f"steps-wrap-{label}-whole",
       ["UP-20", "UP-21", "UP-23", "UP-32", "UP-37"], "same", "wide",
       [V("1000000010:1:1"), V(f"{1000000010 + 10 * k}:5:7")])
      for label, k in STEP_GAPS],
    ("steps-wrap-2p31-unknown", ["UP-21", "UP-23", "UP-37"], "same", "wide",
     [V("1000000010:1:1"), V(f"{1000000010 + 10 * 2**31}:U:U")]),
    # Damaged files.
    ("damaged-short", ["UP-38"], "same", "dmgshort", [V("1000000010:1:1:1")]),
    ("damaged-rows-whole", ["UP-38"], "same", "dmg",
     [V("1000000010:1:1:1", "1000000037:5:-5:7", "1000000050:9:9:9"),
      U("1000000060:1:1:1")]),
    ("damaged-rows-split-first", ["UP-38"], "same", "dmg",
     [V("1000000003:1:1:1", "1000000012:2:2:2", "1000000047.25:5:-5:7")]),
    ("damaged-rows-split-specials", ["UP-38"], "same", "dmg",
     [V("1000000003:1:1:1", "1000000012:2:2:2", "1000000047.5:inf:-5:U")]),
    ("damaged-rows-split-second", ["UP-38"], "same", "dmg",
     [V("1000000003:1:1:1", "1000000033.5:5:-5:3")]),
    ("damaged-rows-first-archive", ["UP-38"], "same", "dmgfirst",
     [V("1000000003:1", "1000000047:5")]),
    ("damaged-rows-update", ["UP-38"], "same", "dmg",
     [U("1000000010:1:1:1", "1000000020:2:2:2")]),
    # Stored current rows at or past the row count.
    ("rows-current-high", ["UP-30"], "same", "curhigh",
     [V("1000000010:1", "1000000020:2", "1000000030:3")]),
]


# --------------------------------------------------------------------------

def run_case(rrdtool, seed_bytes, steps, now):
    work = tempfile.mkdtemp(prefix="rondi-update-")
    try:
        target = os.path.join(work, "t.rrd")
        with open(target, "wb") as handle:
            handle.write(seed_bytes)
        with open(os.path.join(work, "cases.txt"), "wb") as handle:
            handle.write(b"not an rrd file\n")
        sections = []
        for index, step in enumerate(steps):
            result = subprocess.run([rrdtool] + step, cwd=work, env=ENV,
                                    capture_output=True, check=False)
            stdout, stderr = without_build_time(result.stdout), result.stderr
            if now:
                stdout, stderr = NOW.sub(b"<now>", stdout), NOW.sub(b"<now>", stderr)
            sections.append((f"{index}.status", str(result.returncode).encode()))
            sections.append((f"{index}.stdout", stdout))
            sections.append((f"{index}.stderr", stderr))
        if not now:
            with open(target, "rb") as handle:
                final = handle.read()
            if final == seed_bytes:
                sections.append(("same", b""))
            else:
                sections.append(("file", final))
        return sections
    finally:
        shutil.rmtree(work)


def without_build_time(body):
    """Drops the build date from the usage banner's `Compiled` line."""
    return re.sub(rb"(\n               Compiled )[^\n]*", rb"\1", body, count=1)


def frame(case_id, sections):
    out = bytearray()
    for name, body in sections:
        out += f"@@ {case_id} {name} {len(body)}\n".encode()
        out += body + b"\n"
    return bytes(out)


def parse(data):
    out = {}
    rest = data
    while rest:
        newline = rest.index(b"\n")
        _, case_id, name, length = rest[:newline].decode().split(" ")
        length = int(length)
        body = rest[newline + 1:newline + 1 + length]
        out.setdefault(case_id, []).append((name, body))
        rest = rest[newline + 1 + length + 1:]
    return out


def folded(sections):
    out = []
    for name, body in sections:
        if name == "file":
            body = bytearray(body)
            for offset in range(0, len(body) - 7, 8):
                if body[offset:offset + 8] == bytes.fromhex("000000000000f8ff"):
                    body[offset:offset + 8] = bytes.fromhex("000000000000f87f")
            body = bytes(body)
        out.append((name, body))
    return out


def patched(data, patches):
    """Applies SEED_PATCHES entries (create.md section 10 layout)."""
    data = bytearray(data)
    ds_count = int.from_bytes(data[24:32], "little")
    rra_count = int.from_bytes(data[32:40], "little")
    rra_defs = 128 + 120 * ds_count
    cur_rows = rra_defs + 120 * rra_count + 16 + 112 * ds_count + 80 * ds_count * rra_count
    for field, archive, value in patches:
        offset = {"rows": rra_defs + 120 * archive + 24,
                  "cur_row": cur_rows + 8 * archive}[field]
        data[offset:offset + 8] = value.to_bytes(8, "little")
    return bytes(data)


def create_seed(rrdtool, name, args):
    work = tempfile.mkdtemp(prefix="rondi-update-seed-")
    try:
        path = os.path.join(work, "seed.rrd")
        subprocess.run([rrdtool, "create", path] + args, env=ENV, check=True)
        with open(path, "rb") as handle:
            data = handle.read()
    finally:
        shutil.rmtree(work)
    data = patched(data, SEED_PATCHES.get(name, []))
    with open(os.path.join(DATA, "inputs", f"{name}.rrd"), "wb") as handle:
        handle.write(data)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--rrdtool", default="rrdtool")
    parser.add_argument("--reseed", action="store_true")
    parser.add_argument("--linux-overrides", action="store_true")
    options = parser.parse_args()

    banner = subprocess.run([options.rrdtool, "--version"], capture_output=True,
                            check=True).stdout
    if not banner.startswith(b"RRDtool 1.11.0 "):
        sys.exit(f"expected RRDtool 1.11.0, found {banner[:40]!r}")

    random_seeds, random_list = random_cases()
    seeds = {**SEEDS, **random_seeds, **LATER_SEEDS}
    cases = CASES + random_list + LATER_CASES
    ids = [case[0] for case in cases]
    if len(ids) != len(set(ids)):
        sys.exit("duplicate case id")

    os.makedirs(os.path.join(DATA, "inputs"), exist_ok=True)
    os.makedirs(os.path.join(DATA, "expected"), exist_ok=True)
    if not options.linux_overrides:
        for name, args in seeds.items():
            path = os.path.join(DATA, "inputs", f"{name}.rrd")
            if options.reseed or not os.path.exists(path):
                create_seed(options.rrdtool, name, args)
        manifest = {
            "seeds": {name: args for name, args in sorted(seeds.items())},
            "seed_patches": {
                name: [{"field": field, "archive": archive, "value": str(value)}
                       for field, archive, value in patches]
                for name, patches in sorted(SEED_PATCHES.items())
            },
            "cases": [
                {"id": case_id, "rules": rules, "expect": expect, "seed": seed,
                 "steps": steps}
                for case_id, rules, expect, seed, steps in cases
            ],
        }
        with open(os.path.join(DATA, "cases.json"), "w") as handle:
            json.dump(manifest, handle, indent=1)
            handle.write("\n")

    recorded = {}
    for case_id, _, expect, seed, steps in cases:
        with open(os.path.join(DATA, "inputs", f"{seed}.rrd"), "rb") as handle:
            seed_bytes = handle.read()
        first = run_case(options.rrdtool, seed_bytes, steps, expect == "now")
        second = run_case(options.rrdtool, seed_bytes, steps, expect == "now")
        if first != second:
            sys.exit(f"{case_id}: two reference runs differ")
        recorded[case_id] = first

    if options.linux_overrides:
        with open(os.path.join(DATA, "expected", "update.vectors"), "rb") as handle:
            base = parse(handle.read())
        arch = platform.machine()
        arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(arch, arch)
        out = b"".join(frame(case_id, sections)
                       for case_id, sections in recorded.items()
                       if folded(sections) != folded(base[case_id]))
        path = os.path.join(DATA, "expected", f"update.linux-{arch}.vectors")
        if out:
            with open(path, "wb") as handle:
                handle.write(out)
        elif os.path.exists(path):
            os.remove(path)
        print(f"{arch}: {out.count(b' 0.status ')} overriding cases")
        return

    with open(os.path.join(DATA, "expected", "update.vectors"), "wb") as handle:
        for case_id, sections in recorded.items():
            handle.write(frame(case_id, sections))
    print(f"{len(cases)} cases, {len(seeds)} seeds")


if __name__ == "__main__":
    main()
