#!/usr/bin/env python3
"""Rebuild the frozen `rrdtool tune` modification vectors.

The vectors live in crates/rondi-cli/tests/data/tune_modify and are replayed
by crates/rondi-cli/tests/tune_modify_spec.rs. The behavior they pin is
described in docs/compatibility/tune-modify.md.

    scripts/gen-tune-modify-vectors.py write [--rrdtool PATH]
        Create the input files with the reference rrdtool, run every case
        and replace the vectors. Inputs get new random rows each time, so
        every expected file changes too.

    scripts/gen-tune-modify-vectors.py overrides --platform linux [--rrdtool PATH]
        Replay the committed inputs with the reference rrdtool on another
        platform and write expected/<input>.<platform>.vectors holding the
        cases whose outcome differs there. Run it inside a Linux container
        that has the pinned RRDtool 1.11.0 installed.

Only the standard library is used. Every reference run is repeated with a
different environment size so that bytes RRDtool leaves uninitialised show
up as variance; the script fails if any varying byte lies outside the mask
the specification documents.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "crates", "rondi-cli", "tests", "data", "tune_modify")
ENV = {"TZ": "UTC", "LC_ALL": "C", "PATH": os.environ.get("PATH", "/usr/bin:/bin")}


# --------------------------------------------------------------------------
# Inputs. Each is created and updated by the reference rrdtool exactly once;
# the resulting bytes are frozen because create picks random initial rows.

def series(start, step, count, fmt):
    return [fmt(i, start + step * i) for i in range(1, count + 1)]


def inputs():
    out = {}

    counter = [0]

    def base_sample(i, t):
        counter[0] += 7 * i
        return f"{t}:{i % 9}.5:{counter[0]}"

    out["base"] = (
        ["--start", "1000000000", "--step", "300",
         "DS:g:GAUGE:600:U:U", "DS:c:COUNTER:600:0:U",
         "RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.5:3:8",
         "RRA:LAST:0.5:1:5", "RRA:MIN:0.5:2:6"],
        [series(1000000000, 300, 40, base_sample) + ["1000012170:3:99999"]],
    )

    def types_sample(i, t):
        if i % 7 == 3:
            return f"{t}:U:U:U:U"
        return f"{t}:{i * i}:{i * 3}:{i * i * 5}:{i % 11}.5"

    out["types"] = (
        ["--start", "1000000000", "--step", "60",
         "DS:a:DERIVE:120:U:U", "DS:b:ABSOLUTE:120:0:1000",
         "DS:c:COUNTER:120:U:U", "DS:g:GAUGE:120:-5:100",
         "RRA:AVERAGE:0.5:1:12", "RRA:AVERAGE:0.3:2:12",
         "RRA:MIN:0.5:2:12", "RRA:MAX:0.5:2:12", "RRA:LAST:0.5:1:12",
         "RRA:AVERAGE:0.5:3:7", "RRA:MIN:0.5:1:9", "RRA:MAX:0.5:4:5",
         "RRA:LAST:0.9:3:6"],
        [series(1000000000, 60, 50, types_sample) + ["1000003037:3000:20:400:4"]],
    )

    ties = []
    t = 1000000003
    for i in range(1, 46):
        t += 7 + (i % 5) * 3
        ties.append(f"{t}:U:{i * 13}" if i % 4 == 0 else f"{t}:{i % 13}.5:{i * 13}")
    out["ties"] = (
        ["--start", "1000000003", "--step", "10",
         "DS:a:GAUGE:15:U:U", "DS:b:COUNTER:15:U:U",
         "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.1:1:10",
         "RRA:AVERAGE:0.9:2:10", "RRA:AVERAGE:0.1:2:10",
         "RRA:AVERAGE:0.5:1:5", "RRA:AVERAGE:0.2:3:4",
         "RRA:AVERAGE:0.8:3:4", "RRA:AVERAGE:0.99:1:10",
         "RRA:MAX:0.5:1:10", "RRA:MAX:0.1:1:10"],
        [ties],
    )

    out["fresh"] = (
        ["--start", "1000000007", "--step", "10", "DS:x:GAUGE:20:U:U",
         "RRA:AVERAGE:0.5:1:4", "RRA:MAX:0.5:2:3"],
        [],
    )

    out["sparse"] = (
        ["--start", "1000000000", "--step", "300",
         "DS:in:COUNTER:600:U:U", "DS:out:GAUGE:600:U:U",
         "RRA:AVERAGE:0.5:1:20", "RRA:AVERAGE:0.5:6:10", "RRA:LAST:0.5:12:4"],
        [["1000000300:100:1", "1000000600:400:2", "1000000900:900:U",
          "1000003000:1200:7"]],
    )

    out["single"] = (
        ["--start", "1100000000", "--step", "60", "DS:v:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:8"],
        [series(1100000000, 60, 12, lambda i, t: f"{t}:{i * 1.5}")],
    )

    def coarse_sample(i, t):
        return f"{t}:{(i * 37) % 101}.125:{i * i * 3600}"

    out["coarse"] = (
        ["--start", "1100001600", "--step", "3600",
         "DS:t:GAUGE:7200:U:U", "DS:r:COUNTER:7200:U:U",
         "RRA:AVERAGE:0.5:1:24", "RRA:AVERAGE:0.5:6:10",
         "RRA:MAX:0.5:24:5", "RRA:MIN:0.5:6:10", "RRA:LAST:0.5:1:12"],
        [series(1100001600, 3600, 60, coarse_sample) + ["1100219034:50:13000000"]],
    )

    out["neg"] = (
        ["--start", "1100000040", "--step", "60", "DS:v:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:20", "RRA:MAX:0.5:2:10",
         "RRA:MIN:0.5:2:10", "RRA:LAST:0.5:3:6"],
        [series(1100000040, 60, 30, lambda i, t: f"{t}:-{(i % 7) + 1}.25")],
    )

    out["maxonly"] = (
        ["--start", "1100000000", "--step", "60", "DS:v:GAUGE:120:U:U",
         "RRA:MAX:0.5:3:10", "RRA:LAST:0.5:2:10"],
        [series(1100000000, 60, 25, lambda i, t: f"{t}:{(i * 13) % 17}")],
    )

    out["v5"] = (
        ["--start", "1100000000", "--step", "60",
         "DS:d:DCOUNTER:120:U:U", "DS:e:DDERIVE:120:U:U", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.5:2:5"],
        [series(1100000000, 60, 14,
                lambda i, t: f"{t}:{i * 10.5}:{100 - i * 3.25}:{i}")],
    )

    # cur_row ends up 0 in the one-PDP archives: the tune rewrites rows
    # oldest first with cur_row on the last row, and one more update wraps.
    out["wrap0"] = (
        ["--start", "1100000000", "--step", "60", "DS:v:GAUGE:120:U:U",
         "DS:w:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:6", "RRA:AVERAGE:0.5:2:6", "RRA:MIN:0.5:1:6"],
        [series(1100000000, 60, 14, lambda i, t: f"{t}:{i * i}:{100 - i}"),
         ["tune", "RRA#0:+0"],
         ["1100000900:500:50"]],
    )

    # A three-PDP archive whose cur_row is 0 while the last update sits
    # inside its next slot, so a finer target slot can start inside the
    # candidate's newest row and end after it.
    out["wrap3"] = (
        ["--start", "1100000160", "--step", "60", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:3:4"],
        [series(1100000160, 60, 12, lambda i, t: f"{t}:{i * 3}"),
         ["tune", "RRA#0:+0"],
         ["1100000940:40", "1100001000:41", "1100001060:42", "1100001120:43"]],
    )

    out["wrap3b"] = (
        ["--start", "1100000160", "--step", "60", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:3:5", "RRA:MIN:0.5:3:6"],
        [series(1100000160, 60, 12, lambda i, t: f"{t}:{i * 3}"),
         ["tune", "RRA#0:+0"],
         ["1100000940:40", "1100001000:41", "1100001060:42", "1100001120:43"]],
    )

    out["usec"] = (
        ["--start", "1100000000", "--step", "60", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:5", "RRA:MAX:0.5:2:4"],
        [series(1100000000, 60, 9, lambda i, t: f"{t}.25:{i * 2}")
         + ["1100000590.75:7"]],
    )

    out["aligned"] = (
        ["--start", "1100000700", "--step", "60", "DS:g:GAUGE:120:U:U",
         "DS:h:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:30", "RRA:AVERAGE:0.5:4:10", "RRA:AVERAGE:0.5:15:4"],
        [series(1100000700, 60, 35, lambda i, t: f"{t}:{i}:{i * 0.5}")],
    )

    def unkn_sample(i, t):
        return f"{t}:{i}" if i % 5 else f"{t}:U"

    out["unkn"] = (
        ["--start", "1100000000", "--step", "600", "DS:g:GAUGE:1200:U:U",
         "RRA:AVERAGE:0.5:1:12", "RRA:AVERAGE:0.5:4:6", "RRA:MAX:0.8:3:6",
         "RRA:MIN:0.5:6:3"],
        [series(1100000000, 600, 20, unkn_sample)
         + ["1100016000:4", "1100016450:U"]],
    )
    return out


def build_inputs(rrdtool, workdir):
    made = {}
    for name, (create, steps) in inputs().items():
        path = os.path.join(workdir, f"{name}.rrd")
        run_checked(rrdtool, ["create", path] + create)
        for step in steps:
            if step and step[0] == "tune":
                run_checked(rrdtool, ["tune", path] + step[1:])
            else:
                for chunk in range(0, len(step), 40):
                    run_checked(rrdtool, ["update", path] + step[chunk:chunk + 40])
        made[name] = path
    return made


# --------------------------------------------------------------------------
# Cases: (input, args, rondi expectation). Expectations:
#   same        Rondi must reproduce the vector (after the documented mask).
#   refuse      RRDtool writes a file holding uninitialised bytes and then
#               fails; Rondi must fail with the same text and leave the file.
#   clean-error RRDtool crashes; Rondi must fail cleanly and leave the file.
#   unsupported Rondi refuses a feature it does not implement.

def cases():
    c = []

    def add(inp, *args, expect="same"):
        c.append((inp, list(args), expect))

    # Data sources: add each type, delete, delete all, re-add.
    for dst, spec in [("GAUGE", "600:U:U"), ("COUNTER", "600:0:U"),
                      ("DERIVE", "900:-5:7"), ("ABSOLUTE", "300:U:100"),
                      ("DCOUNTER", "600:U:U"), ("DDERIVE", "600:U:U")]:
        add("base", f"DS:n{dst.lower()[:3]}:{dst}:{spec}")
    add("base", "DS:n1:GAUGE:600:U:U", "DS:n2:COUNTER:600:U:U")
    add("base", "DEL:g")
    add("base", "DEL:c")
    add("base", "DEL:g", "DEL:c")
    add("base", "DEL:c", "DEL:g", "DS:z:GAUGE:600:U:U")
    add("base", "DEL:g", "DS:g:GAUGE:900:0:10")
    add("base", "DEL:g", "DEL:g")
    add("base", "DEL:nonexistent")
    add("base", "DEL:G")
    add("types", "DEL:a", "DEL:b", "DEL:c")
    add("types", "DEL:b", "DS:q:DERIVE:120:U:U")
    add("types", "DS:h:GAUGE:120:U:U", "DS:k:ABSOLUTE:60:U:U")
    add("single", "DEL:v")
    add("single", "DEL:v", "DS:w:GAUGE:120:U:U")
    add("single", "DEL:v", "DELRRA:0")
    add("single", "DEL:v", "RRA:AVERAGE:0.5:2:4")
    add("single", "DEL:v", "DELRRA:0", "RRA:AVERAGE:0.5:1:4")
    add("fresh", "DS:new:DCOUNTER:600:U:U")
    add("fresh", "DEL:x")
    add("v5", "DEL:d")
    add("v5", "DEL:d", "DEL:e")
    add("v5", "DS:x:GAUGE:120:U:U")
    add("unkn", "DS:x:COUNTER:1200:U:U")
    add("usec", "DS:x:GAUGE:120:U:U")
    add("wrap0", "DEL:v")

    # Archives: delete.
    for i in range(4):
        add("base", f"DELRRA:{i}")
    add("base", "DELRRA:9")
    add("base", "DELRRA:0", "DELRRA:0")
    add("base", "DELRRA:1", "DELRRA:0")
    add("base", "DELRRA:0", "DELRRA:1", "DELRRA:2", "DELRRA:3")
    add("base", "DELRRA:x")
    add("base", "DELRRA:2junk")
    add("single", "DELRRA:0")
    add("fresh", "DELRRA:0", "DELRRA:1")

    # Archives: resize existing ones.
    for i, rows in [(0, 10), (1, 8), (2, 5), (3, 6)]:
        add("base", f"RRA#{i}:+4")
        add("base", f"RRA#{i}:-2")
        add("base", f"RRA#{i}:={rows + 7}")
    add("base", "RRA#0:=10")
    add("base", "RRA#0:+0")
    add("base", "RRA#0:-10")
    add("base", "RRA#0:-11")
    add("base", "RRA#0:=0")
    add("base", "RRA#0:=1")
    add("base", "RRA#9:+3")
    add("base", "RRA#0:+5", "RRA#0:+7")
    add("base", "RRA#0:+5", "DELRRA:0")
    add("base", "DELRRA:0", "RRA#0:+5")
    add("base", "RRA#00:+1")
    add("base", "RRA# 1:+1")
    add("base", "RRA#1:+2junk")
    add("base", "RRA#1:-+1")
    add("base", "RRA#0:+4294967291", expect="clean-error")
    # Numbers wrap the way 32-bit C conversions do.
    add("base", "RRA#0:=4294967295")
    add("base", "RRA#0:=-1")
    add("base", "RRA#4294967296:+1")
    add("base", "RRA#-1:+1")
    add("base", "DELRRA:4294967296")
    add("base", "DELRRA:99999999999")
    add("base", "DELRRA: 1")
    add("base", "DELRRA:+1")
    add("base", "DELRRA:-0")
    for i in range(9):
        add("types", f"RRA#{i}:+6")
    add("types", "RRA#2:-11")
    add("types", "RRA#6:=2")
    add("types", "RRA#7:=20")
    add("ties", "RRA#1:+10")
    add("ties", "RRA#5:+10")
    add("ties", "RRA#9:+3")
    add("sparse", "RRA#0:+10")
    add("sparse", "RRA#1:+5")
    add("sparse", "RRA#2:=1")
    add("single", "RRA#0:+5")
    add("single", "RRA#0:-7")
    add("fresh", "RRA#0:+5")
    add("coarse", "RRA#0:+30")
    add("coarse", "RRA#2:+3")
    add("coarse", "RRA#3:+4")
    add("coarse", "RRA#4:=40")
    add("neg", "RRA#1:+5")
    add("neg", "RRA#2:+5")
    add("maxonly", "RRA#0:+5")
    add("maxonly", "RRA#1:+5")
    add("wrap0", "RRA#0:+3")
    add("wrap0", "RRA#1:+3")
    add("wrap0", "RRA#2:+3")
    add("unkn", "RRA#1:+4")
    add("wrap3", "RRA#0:+2")
    add("wrap3", "RRA:AVERAGE:0.5:2:6")
    add("wrap3", "RRA:MAX:0.5:2:6")
    add("wrap3", "RRA:LAST:0.5:1:12")
    add("wrap3b", "RRA:AVERAGE:0.5:2:6")
    add("wrap3b", "RRA:MIN:0.5:2:6")
    add("wrap3b", "DELRRA:0", "RRA:AVERAGE:0.5:2:6")
    add("aligned", "RRA#2:+3")
    # The grown archive's row times are taken from the output archive that
    # sits at the grown archive's input index.
    add("base", "DELRRA:0", "RRA#1:+4")
    add("base", "DELRRA:0", "RRA#2:+3")
    add("types", "DELRRA:1", "RRA#4:+5")
    add("types", "DELRRA:0", "DELRRA:1", "RRA#3:+4")

    # Archives: add new ones, every CF from several sources.
    for cf in ["AVERAGE", "MIN", "MAX", "LAST"]:
        for pdp, rows in [(1, 30), (2, 15), (3, 9), (5, 8), (7, 4)]:
            add("base", f"RRA:{cf}:0.5:{pdp}:{rows}")
    for cf in ["AVERAGE", "MIN", "MAX", "LAST"]:
        for pdp, rows in [(1, 20), (2, 20), (5, 6)]:
            add("types", f"RRA:{cf}:0.5:{pdp}:{rows}")
    add("types", "RRA:AVERAGE:0.5:1h:1d")
    add("types", "RRA:AVERAGE:0.5:1:1")
    for cf in ["AVERAGE", "MIN", "MAX", "LAST"]:
        add("coarse", f"RRA:{cf}:0.5:3:20")
        add("neg", f"RRA:{cf}:0.5:2:12")
        add("maxonly", f"RRA:{cf}:0.5:1:12")
        add("aligned", f"RRA:{cf}:0.5:5:8")
        add("unkn", f"RRA:{cf}:0.5:2:10")
        add("wrap0", f"RRA:{cf}:0.5:1:9")
    add("aligned", "RRA:AVERAGE:0.5:15:3")
    add("aligned", "RRA:AVERAGE:0.5:60:2")
    add("coarse", "RRA:AVERAGE:0.5:24:10")
    add("coarse", "RRA:MAX:0.5:48:4")
    add("ties", "RRA:AVERAGE:0.5:2:20")
    add("ties", "RRA:MAX:0.5:3:10")
    add("sparse", "RRA:AVERAGE:0.5:6:10")
    add("sparse", "RRA:LAST:0.5:3:12")
    add("fresh", "RRA:LAST:0.5:1:40")
    add("fresh", "RRA:MIN:0.5:3:5")
    add("single", "RRA:MAX:0.5:2:6")
    add("v5", "RRA:MIN:0.5:3:4")
    add("usec", "RRA:AVERAGE:0.5:3:4")
    add("base", "DELRRA:0", "DELRRA:1", "DELRRA:2", "DELRRA:3", "RRA:AVERAGE:0.5:1:5")
    add("base", "RRA:AVERAGE:0.5:1:5", "RRA:MAX:0.5:2:5", "RRA:AVERAGE:0.5:1:5")

    # Combinations.
    add("base", "DS:g2:GAUGE:600:U:U", "DEL:g", "RRA#3:+5")
    add("base", "DEL:c", "RRA#0:+5", "RRA:MIN:0.5:2:12")
    add("base", "DS:x:GAUGE:600:U:U", "RRA:AVERAGE:0.5:2:12")
    add("types", "DS:q:DDERIVE:300:U:U", "DEL:g", "RRA#8:-2", "RRA:LAST:0.5:1:5")
    add("types", "DS:n:DCOUNTER:300:0:U", "RRA#1:+5", "RRA:AVERAGE:0.5:2:30")
    add("types", "DEL:a", "DELRRA:0", "RRA#5:+4", "RRA:MAX:0.5:3:10")
    add("ties", "DELRRA:2", "RRA#5:+3")
    add("ties", "DS:c:GAUGE:15:U:U", "RRA:AVERAGE:0.5:2:20", "RRA#3:+6")
    add("sparse", "DEL:in", "RRA#2:=1")
    add("fresh", "RRA#0:+5", "RRA:LAST:0.5:1:40")
    add("wrap0", "DEL:w", "RRA#0:+2", "RRA:AVERAGE:0.5:2:8")
    add("base", "DEL:g", "DEL:c", "DELRRA:1", "DELRRA:2", "DELRRA:3")
    add("base", "DEL:g", "DEL:c", "RRA:AVERAGE:0.5:1:5")
    add("base", "--heartbeat", "g:900", "DEL:c")
    add("base", "-r", "g:gg", "RRA#0:+2")
    add("base", "-i", "g:0", "-a", "g:50", "DS:x:GAUGE:600:U:U")
    add("base", "-r", "g:gg", "DEL:gg")
    add("base", "-h", "zz:5", "FOO")

    # Step changes.
    for step in ["1800", "1200", "900", "600", "400", "360", "300", "120", "60"]:
        add("coarse", "--step", step, "RRA#0:+2")
    add("coarse", "--step", "1800", "DEL:r")
    add("coarse", "-t", "900", "RRA:AVERAGE:0.5:2:10", "DS:k:GAUGE:7200:U:U")
    add("coarse", "--step", "1200", "RRA:MIN:0.5:3:10")
    add("coarse", "--step", "7", "DEL:r")
    add("coarse", "--step", "3600", "DEL:r")
    add("coarse", "--step", "7200", "DEL:r")
    add("coarse", "--step", "0", "DEL:r")
    add("coarse", "--step", "-60", "DEL:r")
    add("coarse", "--step", "abc", "DEL:r")
    add("coarse", "--step", "1800")
    add("coarse", "DEL:r", "--step", "900")
    add("types", "--step", "30", "RRA#0:+3")
    add("types", "--step", "20", "DEL:a")
    add("types", "--step", "30")
    add("ties", "--step", "5", "RRA:AVERAGE:0.5:2:20")
    add("unkn", "--step", "300", "DEL:g", "DS:g:GAUGE:600:U:U")
    add("unkn", "--step", "200", "RRA#2:+2")
    add("aligned", "--step", "30", "RRA:AVERAGE:0.5:4:10")
    add("usec", "--step", "20", "RRA#1:+2")
    add("single", "--step", "30", "DEL:v")

    # Invalid arguments.
    for args in [
        ["DS:g:GAUGE:600:U:U"],
        ["DS:new:GAUGE:600:U:U", "DS:new:GAUGE:600:U:U"],
        ["DS:g=newg:GAUGE:600:U:U"],
        ["RRA#0:*3"],
        ["RRA#x"],
        ["RRA#"],
        ["RRA#0"],
        ["RRA#0:+"],
        ["RRA#0: +1"],
        ["FOO"],
        ["DS:"],
        ["DEL:"],
        ["DELRRA:"],
        ["RRA:"],
        ["del:g"],
        ["rra#0:+1"],
        ["DELRRA:-1"],
        ["RRA:FOO:0.5:1:3"],
        ["RRA:AVERAGE:1.5:1:5"],
        ["RRA:AVERAGE:0.5:1"],
        ["RRA:AVERAGE:0.5:0:5"],
        ["RRA:AVERAGE:0.5:1:0"],
        ["DS:m:FOO:600:U:U", "RRA:AVERAGE:0.5:1:3"],
        ["DS:m:FOO:600:U:U", "RRA:BOGUS:0.5:1:3"],
        ["DEL:g", "FOO"],
        ["RRA:AVERAGE:0.5:1:5", "FOO"],
        ["DS:abcdefghijklmnopqrstu:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:3"],
        ["DS:bad", "RRA:AVERAGE:0.5:1:3"],
        ["--step", "7", "DS:c:GAUGE:600:U:U"],
        ["DS:c:GAUGE:600:U:U", "RRA:FOO:0.5:1:1"],
        ["RRA:FOO:0.5:1:1", "DS:c:GAUGE:600:U:U"],
    ]:
        add("base", *args)
    add("base", "DS:bad", expect="refuse")
    add("base", "DS:9bad:GAUGE:x:U:U", expect="refuse")
    add("base", "DS:z:GAUGE:600:5:1", expect="refuse")
    # The failed definition reuses the previous one's name, so RRDtool
    # reports a duplicate; Rondi reports the parse error instead.
    add("base", "DS:x:GAUGE:600:U:U", "DS:bad", expect="clean-error")
    add("base", "RRA#0:+4294967295", expect="clean-error")
    add("base", "DS:y:COMPUTE:g,2,*", expect="unsupported")
    add("base", "RRA:HWPREDICT:100:0.1:0.0035:10", expect="unsupported")
    return c


# --------------------------------------------------------------------------

def run(rrdtool, args, cwd=None, pad=0):
    env = dict(ENV)
    if pad:
        env["RONDI_VECTOR_PAD"] = "x" * pad
    p = subprocess.run([rrdtool] + args, cwd=cwd, env=env, capture_output=True)
    return p.returncode, p.stdout, p.stderr


def run_checked(rrdtool, args):
    rc, out, err = run(rrdtool, args)
    if rc != 0:
        sys.exit(f"setup failed: rrdtool {' '.join(args)}: {err.decode()}")


def word(b, o):
    return struct.unpack_from("<Q", b, o)[0]


def added_names(args):
    names = []
    for a in args:
        if a.startswith("DS:") and len(a) > 3:
            names.append(a[3:].split(":")[0].split("=")[0])
    return names


def mask_offsets(b, args):
    """Offsets the specification lets differ: the parts of an added data
    source definition that RRDtool leaves uninitialised."""
    out = set()
    if len(b) < 128:
        return out
    names = [n.encode() for n in added_names(args)]
    for i in range(word(b, 24)):
        o = 128 + 120 * i
        if o + 120 > len(b):
            break
        if b[o:o + 20].split(b"\0")[0] in names:
            out.add(o + 39)
            out.update(range(o + 64, o + 120))
    return out


def run_case(rrdtool, src, args, workdir, pad):
    target = os.path.join(workdir, "case.rrd")
    shutil.copyfile(src, target)
    rc, out, err = run(rrdtool, ["tune", "case.rrd"] + args, cwd=workdir, pad=pad)
    with open(target, "rb") as handle:
        data = handle.read()
    info = dump = None
    if rc == 0:
        irc, info, _ = run(rrdtool, ["info", "case.rrd"], cwd=workdir)
        drc, dump, _ = run(rrdtool, ["dump", "case.rrd"], cwd=workdir)
        if irc != 0:
            info = None
        if drc != 0:
            dump = None
    return rc, out, err, data, info, dump


def status_text(rc):
    return "signal" if rc < 0 else str(rc)


def inspected(cid, args):
    """Whether the vector keeps the info and dump text. The file bytes are
    the contract and are kept for every case; the text is kept for a
    quarter of the cases and every step change to bound the data size."""
    return int(cid[:3]) % 4 == 0 or "--step" in args or "-t" in args


def render(cid, rc, out, err, data, info, dump, unchanged, inspect):
    """One case as `@@ <id> <section> <length>` framed sections."""
    parts = [("status", status_text(rc).encode()), ("stdout", out), ("stderr", err)]
    parts.append(("unchanged", b"") if unchanged else ("file", data))
    if inspect and info is not None:
        parts.append(("info", info))
    if inspect and dump is not None:
        parts.append(("dump", dump))
    blob = b""
    for name, body in parts:
        blob += b"@@ %s %s %d\n" % (cid.encode(), name.encode(), len(body)) + body + b"\n"
    return blob


def parse_vectors(b):
    """Parses framed vectors into {id: {section: bytes}}."""
    out = {}
    i = 0
    while i < len(b):
        nl = b.index(b"\n", i)
        _, cid, name, length = b[i:nl].decode().split(" ")
        body = b[nl + 1:nl + 1 + int(length)]
        out.setdefault(cid, {})[name] = body
        i = nl + 1 + int(length) + 1
    return out


def slug(args):
    s = "_".join(args).lower()
    keep = "".join(ch if ch.isalnum() else "-" for ch in s)
    while "--" in keep:
        keep = keep.replace("--", "-")
    return keep.strip("-")[:48] or "noargs"


def execute(rrdtool, case_list, input_paths, workdir):
    results = []
    seen = set()
    for n, (inp, args, expect) in enumerate(case_list):
        cid = f"{n:03d}-{inp}-{slug(args)}"
        runs = [run_case(rrdtool, input_paths[inp], args, workdir, pad) for pad in (0, 4096, 70001)]
        first = runs[0]
        allowed = mask_offsets(first[3], args)
        for other in runs[1:]:
            if other[:3] != first[:3] or other[4:] != first[4:]:
                sys.exit(f"{cid}: output varies between identical runs")
            if len(other[3]) != len(first[3]):
                sys.exit(f"{cid}: file length varies between identical runs")
            varying = {i for i, (x, y) in enumerate(zip(first[3], other[3])) if x != y}
            if expect == "same" and not varying <= allowed:
                sys.exit(f"{cid}: unmasked bytes vary: {sorted(varying - allowed)[:16]}")
            if expect == "same":
                seen.update((i - 128) % 120 for i in varying)
        results.append((cid, inp, args, expect, first))
    print(f"masked definition offsets seen varying: {sorted(seen)}")
    return results


def write(rrdtool):
    with tempfile.TemporaryDirectory() as tmp:
        made = build_inputs(rrdtool, tmp)
        if os.path.isdir(DATA):
            shutil.rmtree(DATA)
        os.makedirs(os.path.join(DATA, "inputs"))
        os.makedirs(os.path.join(DATA, "expected"))
        with open(os.path.join(DATA, ".gitattributes"), "w") as fh:
            fh.write("# Vectors are compared byte for byte; never normalize them.\n"
                     "* -text -diff\ncases.tsv text eol=lf diff\n")
        stored = {}
        for name, path in made.items():
            dest = os.path.join(DATA, "inputs", f"{name}.rrd")
            shutil.copyfile(path, dest)
            stored[name] = dest
        work = os.path.join(tmp, "work")
        os.makedirs(work)
        case_list = cases()
        results = execute(rrdtool, case_list, stored, work)
        lines = ["# id\tinput\texpect\targs... (tab separated)"]
        packed = {}
        for cid, inp, args, expect, (rc, out, err, data, info, dump) in results:
            with open(stored[inp], "rb") as handle:
                original = handle.read()
            packed.setdefault(inp, b"")
            packed[inp] += render(cid, rc, out, err, data, info, dump,
                                  data == original, inspected(cid, args))
            lines.append("\t".join([cid, inp, expect] + args))
        for inp, blob in packed.items():
            with open(os.path.join(DATA, "expected", f"{inp}.vectors"), "wb") as fh:
                fh.write(blob)
        with open(os.path.join(DATA, "cases.tsv"), "w") as fh:
            fh.write("\n".join(lines) + "\n")
        print(f"wrote {len(results)} cases")


def load_cases():
    out = []
    with open(os.path.join(DATA, "cases.tsv")) as fh:
        for line in fh:
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            cid, inp, expect, *args = line.split("\t")
            out.append((cid, inp, expect, args))
    return out


def overrides(rrdtool, platform):
    expected = os.path.join(DATA, "expected")
    stored = {name[:-4]: os.path.join(DATA, "inputs", name)
              for name in os.listdir(os.path.join(DATA, "inputs"))}
    for name in os.listdir(expected):
        if name.endswith(f".{platform}.vectors"):
            os.remove(os.path.join(expected, name))
    recorded = {}
    for name in stored:
        with open(os.path.join(expected, f"{name}.vectors"), "rb") as fh:
            recorded.update(parse_vectors(fh.read()))
    differing = []
    packed = {}
    with tempfile.TemporaryDirectory() as tmp:
        for cid, inp, expect, args in load_cases():
            rc, out, err, data, info, dump = run_case(rrdtool, stored[inp], args, tmp, 0)
            with open(stored[inp], "rb") as handle:
                original = handle.read()
            unchanged = data == original
            blob = render(cid, rc, out, err, data, info, dump, unchanged, inspected(cid, args))
            want = recorded[cid]
            got = parse_vectors(blob)[cid]
            want_file = want.get("file", original)
            mask = mask_offsets(data, args) if expect == "same" else set(range(len(data)))
            same_file = len(data) == len(want_file) and all(
                x == y or i in mask for i, (x, y) in enumerate(zip(data, want_file)))
            same_text = all(got.get(k) == want.get(k)
                            for k in ("status", "stdout", "stderr", "unchanged", "info", "dump"))
            if same_text and same_file:
                continue
            differing.append(cid)
            packed[inp] = packed.get(inp, b"") + blob
    for inp, blob in packed.items():
        with open(os.path.join(expected, f"{inp}.{platform}.vectors"), "wb") as fh:
            fh.write(blob)
    print(f"{len(differing)} {platform} overrides: {' '.join(differing)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=["write", "overrides"])
    parser.add_argument("--rrdtool", default="rrdtool")
    parser.add_argument("--platform", default="linux")
    opts = parser.parse_args()
    version = run(opts.rrdtool, ["--version"])[1].decode()
    if not version.startswith("RRDtool 1.11.0 "):
        sys.exit(f"expected RRDtool 1.11.0, found: {version.splitlines()[:1]}")
    if opts.mode == "write":
        write(opts.rrdtool)
    else:
        overrides(opts.rrdtool, opts.platform)


if __name__ == "__main__":
    main()
