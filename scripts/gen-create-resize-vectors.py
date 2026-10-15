#!/usr/bin/env python3
"""Rebuild the frozen `rrdtool create` and `rrdtool resize` vectors.

The vectors live in crates/rondi-cli/tests/data/create and
crates/rondi-cli/tests/data/resize and are replayed by
crates/rondi-cli/tests/create_spec_vectors.rs and resize_spec_vectors.rs.
The behavior they pin is described in docs/compatibility/create.md and
docs/compatibility/resize.md.

    scripts/gen-create-resize-vectors.py write [--rrdtool PATH] [--rondi PATH] [--keep-inputs]
        Build the input files with the reference rrdtool (or, with
        --keep-inputs, reuse the committed ones), run every case twice and
        replace the vectors. With --rondi, also run every case
        against Rondi and fail if its result differs from the recorded one
        in a way the case table does not already declare.

    scripts/gen-create-resize-vectors.py overrides --platform linux [--rrdtool PATH]
        Replay the committed cases with the reference rrdtool on another
        platform and write expected/<suite>.<platform>.vectors holding the
        cases whose outcome differs there. Run it inside a Linux container
        that has the pinned RRDtool 1.11.0 installed.

Only the standard library is used. `create` picks a random current row for
each archive, so created files are compared in masked form: every archive is
rotated so its oldest row comes first and the current-row words are zeroed.
Resize output is deterministic for a frozen input and kept as is.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TESTS = os.path.join(ROOT, "crates", "rondi-cli", "tests", "data")
ENV = {"TZ": "UTC", "LC_ALL": "C", "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
B = ["-b", "1000000000", "-s", "10"]
DS = "DS:x:GAUGE:20:U:U"
RRA = "RRA:AVERAGE:0.5:1:5"
TIMEOUT = 60


def read_bytes(path):
    with open(path, "rb") as handle:
        return handle.read()


def write_bytes(path, data):
    with open(path, "wb") as handle:
        handle.write(data)


def word(b, o):
    return struct.unpack_from("<Q", b, o)[0]


def masked(b):
    """Created file in comparison form: archives rotated oldest row first
    and current-row words zeroed. Anything that does not look like a
    complete 64-bit RRD is returned unchanged."""
    if len(b) < 128 or b[:4] != b"RRD\0":
        return b
    ds, rra = word(b, 24), word(b, 32)
    if ds > 4096 or rra > 4096:
        return b
    pointers = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra
    if pointers + 8 * rra > len(b):
        return b
    out = bytearray(b[:pointers]) + bytes(8 * rra)
    data = pointers + 8 * rra
    for i in range(rra):
        rows = word(b, 128 + 120 * ds + 120 * i + 24)
        length = rows * ds * 8
        if rows == 0 or data + length > len(b):
            return b
        archive = b[data:data + length]
        split = ((word(b, pointers + 8 * i) + 1) % rows) * ds * 8
        out += archive[split:] + archive[:split]
        data += length
    out += b[data:]
    return bytes(out)


def fold_nan(b):
    """x86_64 writes new unknowns as 0xfff8...; fold them into the quiet
    NaN the recording platform writes."""
    neg = struct.pack("<Q", 0xFFF8000000000000)
    pos = struct.pack("<Q", 0x7FF8000000000000)
    out = bytearray(b)
    for o in range(0, len(out) - 7, 8):
        if out[o:o + 8] == neg:
            out[o:o + 8] = pos
    return bytes(out)


def run(rrdtool, args, cwd, pad=0):
    env = dict(ENV)
    if pad:
        env["RONDI_VECTOR_PAD"] = "x" * pad
    try:
        p = subprocess.run([rrdtool] + args, cwd=cwd, env=env, capture_output=True,
                           timeout=TIMEOUT)
    except subprocess.TimeoutExpired:
        return "timeout", b"", b""
    return ("signal" if p.returncode < 0 else str(p.returncode)), p.stdout, p.stderr


def run_checked(rrdtool, args, cwd):
    status, _, err = run(rrdtool, args, cwd)
    if status != "0":
        sys.exit(f"setup failed: rrdtool {' '.join(args)}: {err.decode()}")


# --------------------------------------------------------------------------
# Inputs. Each is built by the reference rrdtool once and frozen, because
# create picks random rows. Some are patched afterwards to craft states that
# are hard to reach through updates (a given current row, a short file).

def series(start, step, count, fmt):
    return [fmt(i, start + step * i) for i in range(1, count + 1)]


def patch_pointers(b, values):
    out = bytearray(b)
    ds, rra = word(b, 24), word(b, 32)
    pointers = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra
    for i, value in enumerate(values):
        struct.pack_into("<Q", out, pointers + 8 * i, value)
    return bytes(out)


def number_rows(b):
    """Fills every row with a value naming its archive, physical row and
    data source, so row moves are visible in the output."""
    out = bytearray(b)
    ds, rra = word(b, 24), word(b, 32)
    o = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra + 8 * rra
    for i in range(rra):
        for r in range(word(b, 128 + 120 * ds + 120 * i + 24)):
            for k in range(ds):
                struct.pack_into("<d", out, o, (i + 1) * 1000 + r + k / 10)
                o += 8
    return bytes(out)


def legacy_v1(b):
    """Version 0001 layout: the live header is a 4-byte-padded time_t
    without microseconds."""
    ds, rra = word(b, 24), word(b, 32)
    live = 128 + 120 * ds + 120 * rra
    return b[:4] + b"0001" + b[8:live + 8] + b[live + 16:]


def build_inputs(rrdtool, tmp):
    """Returns {suite: {name: bytes}}."""
    made = {"create": {}, "resize": {}}

    def make(suite, name, steps, post=None):
        d = os.path.join(tmp, f"{suite}-{name}")
        os.makedirs(d)
        for step in steps:
            run_checked(rrdtool, step, d)
        data = read_bytes(os.path.join(d, "in.rrd"))
        made[suite][name] = post(data) if post else data

    def cr(*defs):
        return ["create", "in.rrd"] + list(defs)

    def up(values):
        return ["update", "in.rrd"] + values

    make("create", "tpl", [
        cr("-b", "1000000123", "-s", "60", "DS:a:GAUGE:120:nan:U", "DS:b:COUNTER:5m:0:1e9",
           "RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.3:3:4"),
        up(["1000000183:5:100", "1000000243:7:400", "1000000303:9:700"])])
    make("create", "hw", [cr("-b", "1000000000", "-s", "10", DS, RRA,
                             "RRA:HWPREDICT:20:0.1:0.01:5")])
    make("create", "mhw", [cr("-b", "1000000000", "-s", "10", DS,
                              "RRA:MHWPREDICT:12:0.1:0.01:4")])
    make("create", "v5", [cr("-b", "1000000000", "-s", "10", "DS:d:DCOUNTER:20:U:U",
                             "DS:e:DDERIVE:20:0:U", RRA)])
    make("create", "cp", [cr("-b", "1000000000", "-s", "10", DS,
                             "DS:y:COMPUTE:x,2,*", RRA)])
    make("create", "src", [
        cr("-b", "1000000000", "-s", "60", "DS:a:GAUGE:120:U:U", "DS:b:COUNTER:120:U:U",
           "RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.5:3:4", "RRA:MIN:0.5:2:5", "RRA:LAST:0.5:1:7"),
        up(series(1000000000, 60, 29,
                  lambda i, t: f"{t + (i % 3) * 7}:{(i * 7) % 11 if i % 5 else 'U'}:{i * i * 60}"))])
    make("create", "src2", [
        cr("-b", "1000000000", "-s", "60", "DS:a:GAUGE:120:U:U", "DS:c:GAUGE:120:U:U",
           "RRA:AVERAGE:0.5:1:10", "RRA:AVERAGE:0.5:4:5"),
        up(series(1000000000, 60, 24, lambda i, t: f"{t}:{100 + i}:{200 + i}"))])
    made["create"]["text"] = b"not an rrd file\n"

    make("resize", "base", [
        cr("-b", "1000000000", "-s", "60", "DS:a:GAUGE:120:U:U", "DS:b:COUNTER:120:U:U",
           "RRA:AVERAGE:0.5:1:6", "RRA:MAX:0.5:3:4", "RRA:LAST:0.5:2:5"),
        up(series(1000000000, 60, 19, lambda i, t: f"{t}:{i}:{i * i * 60}"))])
    make("resize", "single", [
        cr("-b", "1100000000", "-s", "300", "DS:v:GAUGE:600:U:U", "RRA:AVERAGE:0.5:1:4"),
        up(series(1100000000, 300, 9, lambda i, t: f"{t}:{i * 1.5}"))])
    for name, pointers in [("ptr0", [0, 0]), ("ptrmid", [2, 1]), ("ptrlast", [4, 2])]:
        make("resize", name, [cr("-b", "1000000000", "-s", "60", "DS:a:GAUGE:120:U:U",
                                 "DS:b:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:5",
                                 "RRA:MAX:0.5:2:3")],
             post=lambda b, p=pointers: number_rows(patch_pointers(b, p)))
    make("resize", "hw", [cr("-b", "1000000000", "-s", "10", DS, "RRA:AVERAGE:0.5:1:4",
                             "RRA:HWPREDICT:6:0.1:0.01:3")],
         post=lambda b: number_rows(patch_pointers(b, [1, 2, 0, 1, 3, 2])))
    make("resize", "v5", [cr("-b", "1000000000", "-s", "10", "DS:d:DCOUNTER:20:U:U",
                             "RRA:AVERAGE:0.5:1:3")],
         post=lambda b: number_rows(patch_pointers(b, [1])))
    base = made["resize"]["ptrmid"]
    made["resize"]["v1"] = legacy_v1(base)
    made["resize"]["trailing"] = base + b"TRAILING-BYTES!!"
    made["resize"]["trailing5"] = base + b"12345"
    made["resize"]["trailing7"] = base + b"1234567"
    made["resize"]["trailing24"] = base + b"x" * 24
    made["resize"]["short"] = base[:-20]
    made["resize"]["currow"] = patch_pointers(base, [7, 1])
    made["resize"]["text"] = b"not an rrd file\n"
    return made


# --------------------------------------------------------------------------
# Cases. Each is (expect, note, inputs, args). `inputs` names files copied
# into the working directory before the run: `name` copies inputs/name.rrd,
# `as=name` copies inputs/name.rrd as `as`, `@link:path>target` makes a
# symbolic link, `@dir:path` a directory and `@text:path` a small text file.
# `expect` is what Rondi must do relative to the recorded RRDtool result:
#   same         reproduce status, output and (masked) files
#   unsupported  a feature Rondi refuses: exit 1, ERROR: text, no change
#   divergent    a documented divergence (note names it); Rondi still differs
#   clean-error  RRDtool crashes or hangs; Rondi fails with ERROR: instead
#   undefined    RRDtool is nondeterministic; Rondi exits 0 or 1

def create_cases():
    c = []

    def add(*args, inputs=(), expect="same", note="-"):
        c.append((expect, note, list(inputs), ["create"] + list(args)))

    def mk(*defs, **kwargs):
        add("o.rrd", *B, *defs, **kwargs)

    # 1. Command shape and options.
    add()
    add("o.rrd")
    add("o.rrd", *B)
    add("o.rrd", *B, DS)
    add("o.rrd", *B, RRA)
    add("o.rrd", *B, "foo")
    add("o.rrd", *B, DS, "foo", RRA)
    add("o.rrd", *B, DS, RRA, "")
    add("o.rrd", *B, "rra:AVERAGE:0.5:1:5", DS)
    add("o.rrd", *B, "ds:x:GAUGE:20:U:U", RRA)
    add("o.rrd", *B, DS, "RRA")
    add("o.rrd", *B, "DS", RRA)
    add("o.rrd", "p.rrd", *B, DS, RRA)
    add("-b", "1000000000", "o.rrd", DS, RRA)
    add("o.rrd", DS, RRA, "-b", "1000000000", "-s", "10")
    add("o.rrd", "-b", "1000000000", "-s", "10", "--", DS, RRA)
    add("o.rrd", "-b", "1000000000", DS, RRA, "--", "-s", "20")
    add("--", "o.rrd", "-b", "1000000000", DS, RRA)
    add("o.rrd", "--start", "1000000000", "--step", "10", DS, RRA)
    add("o.rrd", "--start=1000000000", "--step=10", DS, RRA)
    add("o.rrd", "-b1000000000", "-s10", DS, RRA)
    add("o.rrd", "--sta", "1000000000", DS, RRA)
    add("o.rrd", "--st", "1000000000", DS, RRA)
    add("o.rrd", "--bogus", *B, DS, RRA)
    add("o.rrd", "-x", *B, DS, RRA)
    add("o.rrd", *B, "-s")
    add("o.rrd", "-b")
    add("o.rrd", "-b", "1000000000", "-b", "1100000000", DS, RRA)
    add("o.rrd", "-b", "1000000000", "-s", "10", "-s", "20", DS, RRA)
    add("o.rrd", "-Ob", "1000000000", DS, RRA)
    add("o.rrd", *B, "-O", "-O", DS, RRA)
    add("o.rrd", *B, "--no-overwrite", DS, RRA)
    add("o.rrd", *B, "--no-overwrite=1", DS, RRA)
    add("o.rrd", *B, "-t", "a.rrd", "-t", "b.rrd", DS, RRA)
    add("o.rrd", "--template", "a.rrd", "--template=b.rrd")

    # 2. Start time.
    for start in ["1000000000", "315360001", "999999999", "9999999999", "99999999999999",
                  "20010909", "00:00 30.12.1979", "00:00 29.12.1979 +86399 seconds",
                  "00:00 01.01.1979", "00:00 01.01.1980", "midnight 01.01.1980 -1 seconds",
                  "end-1h", "start-1h", "e-1h", "s+5min", "garbage", "1000000000.5",
                  "@1000000000", "100", "315360000", "now-60y", "12:00 15.03.2005",
                  "noon 2005-03-15", "", " "]:
        add("o.rrd", "-b", start, "-s", "10", DS, RRA)

    # 3. Step.
    for step in ["1", "10", "05", "1s", "1m", "3h", "2d", "1w", "1M", "1y", "5ss", "5sx",
                 "5mm", "5m5", "5hs", "4294967296", "18446744073709551615",
                 "18446744073709551616", "300000000000000000y", "0", "0m", "-5", "+5",
                 " 5", "5 ", "1x", "1.5", "0x10", "5:3", "5S", "5H", "5D", "5W", "5Y",
                 "m", ""]:
        add("o.rrd", "-b", "1000000000", "-s", step, DS, RRA)

    # 4. Data source names and the definition shape.
    for ds in ["DS:abcdefghijklmnopqrs:GAUGE:20:U:U", "DS:abcdefghijklmnopqrst:GAUGE:20:U:U",
               "DS::GAUGE:20:U:U", "DS:a-b:GAUGE:20:U:U", "DS:A_9:GAUGE:20:U:U",
               "DS:-:GAUGE:20:U:U", "DS:é:GAUGE:20:U:U", "DS:a b:GAUGE:20:U:U",
               "DS:a.b:GAUGE:20:U:U", "DS:x:gauge:20:U:U", "DS:x:GAUGE:20:U:U:5",
               "DS:x:GAUGE:20:U:U:extra:more", "DS:x::20:U:U", "DS:x", "DS:", "DS:x:GAUGE",
               "DS:x:GAUGE:", "DS:x:GAUGE:20", "DS:x:GAUGE:20:", "DS:x:GAUGE:20:U",
               "DS:x:GAUGE:20::U", "DS:x:GAUGE:20:U:", "DS:x:GAUGE::U:U", "DS:x:BOGUS:20:U:U",
               "DS:x:GAUGEX:20:U:U", "DS:x:COMPUTEX:20:U:U", "DS:x=y:GAUGE:20:U:U",
               "DS:x=:GAUGE:20:U:U", "DS:x=y[0]:GAUGE:20:U:U", "DS:x=y[1]:GAUGE:20:U:U",
               "DS:x=y[99]:GAUGE:20:U:U", "DS:x=y[:GAUGE:20:U:U", "DS:x=y[a]:GAUGE:20:U:U",
               "DS:x=y[-1]:GAUGE:20:U:U", "DS:x[1]:GAUGE:20:U:U",
               "DS:x=abcdefghijklmnopqrst:GAUGE:20:U:U", "DS:x=y-z:GAUGE:20:U:U",
               "DS:x=y=z:GAUGE:20:U:U"]:
        mk(ds, RRA)
    mk(DS, "DS:x:COUNTER:20:U:U", RRA)
    mk(DS, "DS:y=x:COUNTER:20:U:U", RRA)
    mk("DS:x=z:GAUGE:20:U:U", "DS:y=z:COUNTER:20:U:U", RRA)
    mk("DS:x=z:GAUGE:20:U:U", "DS:x=y:COUNTER:20:U:U", RRA)
    mk(DS, "DS:x:GAUGE:20:U:U", "DS:bad", RRA)
    mk(DS, "DS:bad", "DS:x:GAUGE:20:U:U", RRA)
    mk("DS:bad", "DS:x:FOO:1:U:U", RRA)
    mk("DS:x:FOO:1:U:U", "DS:bad", RRA)
    mk(RRA, DS, "DS:x:GAUGE:20:U:U")
    mk(*[f"DS:d{i}:GAUGE:20:U:U" for i in range(40)], RRA)

    # 5. Every type with ranges, and the heartbeat grammar.
    for dst in ["GAUGE", "COUNTER", "DERIVE", "ABSOLUTE", "DCOUNTER", "DDERIVE"]:
        for hb, lo, hi in [("20", "U", "U"), ("600", "0", "U"), ("1m", "-1.5", "100"),
                           ("2h", "U", "1e9")]:
            mk(f"DS:x:{dst}:{hb}:{lo}:{hi}", RRA)
    for hb in ["1", "7s", "1y", "999999999999y", "0", "0s", "-1", "+20", " 20", "20x", "1q",
               "0x10", "4294967296", "18446744073709551615", "18446744073709551616",
               "99999999999999999999999", "5ss", "5mm", "20 "]:
        mk(f"DS:x:GAUGE:{hb}:U:U", RRA)

    # 6. Minimum and maximum.
    for lo, hi in [("nan", "nan"), ("NaN", "U"), ("u", "U"), ("UNKN", "U"), ("inf", "U"),
                   ("-inf", "inf"), ("-Infinity", "U"), ("1e3", "1e400"), ("0x10", "U"),
                   (" 5", "U"), ("5 ", "U"), ("5", "5"), ("-0", "0"), ("1e-400", "U"),
                   ("1e-400", "-0"), (".5", "U"), ("5.", "U"), ("1,5", "U"),
                   ("123456789012345678", "U"), ("1234567890123456789", "U"),
                   ("U", "1234567890123456789"), ("U", "12345678901234567x"),
                   ("U", "123456789012345678x"), ("U", "inf"), ("inf", "-inf"), ("nan", "5"),
                   ("1", "nan"), ("nan", "-5"), ("-1e308", "1e308"), ("5", "1"),
                   ("0.1", "0.30000000000000004"), ("1e", "U"), ("+3", "U"), ("U", "+"),
                   ("0.123456789012345678", "U"), ("4.9406564584124654e-324", "U")]:
        mk(f"DS:x:GAUGE:20:{lo}:{hi}", RRA)

    # 7. Archives: CF, xff, steps and rows.
    for rra in ["RRA:MIN:0.5:1:5", "RRA:MAX:0.5:1:5", "RRA:LAST:0.5:1:5",
                "RRA:AVERAGE:0:1:5", "RRA:AVERAGE:0.999999:1:5", "RRA:AVERAGE:1.0:1:5",
                "RRA:AVERAGE:1:1:5", "RRA:AVERAGE:-0:1:5", "RRA:AVERAGE:-0.1:1:5",
                "RRA:AVERAGE:nan:1:5", "RRA:AVERAGE:U:1:5", "RRA:AVERAGE:inf:1:5",
                "RRA:AVERAGE:0.5x:1:5", "RRA:AVERAGE:.5:1:5", "RRA:AVERAGE:5e-1:1:5",
                "RRA:AVERAGE:0x0.8:1:5", "RRA:AVERAGE: 0.5:1:5", "RRA:AVERAGE:0.5 :1:5",
                "RRA:AVERAGE:0.123456789012345678:1:5",
                "RRA:AVERAGE:0.11111111111111111111111:1:5",
                "RRA:AVERAGE:0.5:1:5:", "RRA:AVERAGE:0.5:1:5::", "RRA:AVERAGE:0.5:1:5:x",
                "RRA:AVERAGE:0.5:1:5:9", "RRA::AVERAGE:0.5:1:5", "RRA:AVERAGE::0.5:1:5",
                "RRA:AVERAGE:0.5::1:5", "RRA:AVERAGE:0.5:1::5", "RRA:AVERAGE", "RRA:",
                "RRA::::", "RRA:AVERAGE:0.5:1", "RRA:LAST:0.5", "RRA:AVERAGEX:0.5:1:5",
                "RRA:AVERAGE1:0.5:1:5", "RRA:AVG:0.5:1:5", "RRA:Average:0.5:1:5",
                "RRA:average:0.5:1:5", "RRA:ABCDEFGHIJKLMNOPQRSTUVWXYZ:0.5:1:5",
                "RRA:FOO:0.5:1:5", "RRA:1AVERAGE:0.5:1:5",
                "RRA:AVERAGE:0.5:1:5x", "RRA:AVERAGE:0.5:1x:5", "RRA:AVERAGE:0.5:1m:1h",
                "RRA:AVERAGE:0.5:1m:1hx", "RRA:AVERAGE:0.5:1m:65s", "RRA:AVERAGE:0.5:1m:59s",
                "RRA:AVERAGE:0.5:1m:120s", "RRA:AVERAGE:0.5:2:30s", "RRA:AVERAGE:0.5:2:40s",
                "RRA:AVERAGE:0.5:15s:1m", "RRA:AVERAGE:0.5:7s:5", "RRA:AVERAGE:0.5:0s:1m",
                "RRA:AVERAGE:0.5:1:0s", "RRA:AVERAGE:0.5:1:5s", "RRA:AVERAGE:0.5:0:5",
                "RRA:AVERAGE:0.5:1:0", "RRA:AVERAGE:0.5:-1:5", "RRA:AVERAGE:0.5:1:-5",
                "RRA:AVERAGE:0.5:+1:5", "RRA:AVERAGE:0.5:1:+5", "RRA:AVERAGE:0.5:01:05",
                "RRA:AVERAGE:0.5:4294967296:5", "RRA:AVERAGE:0.5:18446744073709551615:5",
                "RRA:AVERAGE:0.5:99999999999999999999:5", "RRA:AVERAGE:0.5:1d:1y",
                "RRA:MAX:0.5:1w:1M", "RRA:LAST:0.5:10s:100s", "RRA:MIN:0.5:3:1",
                "RRA:AVERAGE:0.5:1:1"]:
        mk(DS, rra)
    mk(DS, RRA, RRA)
    mk(DS, "RRA:FOO:0.5:1:5", "RRA:AVERAGE:2:1:5")
    mk(DS, "RRA:AVERAGE:2:1:5", "RRA:FOO:0.5:1:5")
    mk("RRA:FOO:0.5:1:5", DS)
    mk("RRA:FOO:0.5:1:5", "DS:bad")
    mk("DS:bad", "RRA:FOO:0.5:1:5")
    mk(DS, *[f"RRA:{cf}:0.5:{p}:{r}" for cf in ["AVERAGE", "MIN", "MAX", "LAST"]
             for p, r in [(1, 3), (2, 4), (7, 2)]])

    # 8. Initial state: the start time against the step and each archive's
    # consolidation interval.
    for start, step in [("1000000007", "10"), ("1000000000", "7"), ("1000000123", "60"),
                        ("1000000299", "300"), ("1000000001", "1"), ("1099999999", "86400"),
                        ("1000000000", "1y")]:
        add("o.rrd", "-b", start, "-s", step, "DS:a:GAUGE:600:U:U", "DS:b:COUNTER:600:0:U",
            "RRA:AVERAGE:0.5:1:3", "RRA:MAX:0.5:3:2", "RRA:MIN:0.5:5:2",
            "RRA:LAST:0.5:12:2", "RRA:AVERAGE:0.25:13:2")
    mk("DS:a:DCOUNTER:20:U:U", "DS:b:GAUGE:20:U:U", RRA)
    mk("DS:a:GAUGE:20:U:U", "DS:b:DDERIVE:20:U:U", RRA)
    mk(DS, "RRA:AVERAGE:0.5:1:2000")

    # 9. Holt-Winters archives and computed sources (Rondi refuses them).
    hw = dict(expect="unsupported", note="CR-unsupported")
    for rra in ["RRA:HWPREDICT:20:0.1:0.01:5", "RRA:MHWPREDICT:20:0.1:0.01:5",
                "RRA:HWPREDICT:20:0.1:0.01:5:2", "RRA:HWPREDICT:20:0.1:1:5",
                "RRA:HWPREDICT:20:0.1:0.01:0", "RRA:HWPREDICT:20:0.1:0.01:1m",
                "RRA:HWPREDICT:5m:0.1:0.01:1m", "RRA:HWPREDICT:20:0.1:0.01:5:9",
                "RRA:HWPREDICT:20:0.1:0.01:5:0", "RRA:SEASONAL:5:0.1:1",
                "RRA:SEASONAL:5:0.1:1:smoothing-window=0.3", "RRA:DEVSEASONAL:5:0.1:1",
                "RRA:DEVSEASONAL:5:0.1:1:smoothing-window=0", "RRA:DEVPREDICT:20:1",
                "RRA:FAILURES:20:7:9:1", "RRA:FAILURES:20:1:28:1"]:
        mk(DS, RRA, rra, **hw)
    mk(DS, "RRA:HWPREDICT:20:0.1:0.01:5:2", "RRA:SEASONAL:5:0.2:1", **hw)
    for rra in ["RRA:HWPREDICT:20:0:0.01:5", "RRA:HWPREDICT:20:1:0.01:5",
                "RRA:HWPREDICT:20:0.1:-0.1:5", "RRA:HWPREDICT:0:0.1:0.01:5",
                "RRA:HWPREDICT:20:0.1:0.01", "RRA:HWPREDICT:20:0.1:0.01:5:2:3",
                "RRA:SEASONAL:5:0.1", "RRA:SEASONAL:5:1.5:1",
                "RRA:SEASONAL:5:0.2:1:smoothing-window=2", "RRA:SEASONAL:5:0.2:1:foo=1",
                "RRA:DEVPREDICT:20", "RRA:FAILURES:20:10:9:1", "RRA:FAILURES:20:7:29:1",
                "RRA:FAILURES:20:0:9:1", "RRA:FAILURES:20:7:9"]:
        mk(DS, rra, **hw)
    for ds in ["DS:y:COMPUTE:x,2,*", "DS:y:COMPUTE:x,1,+,x,*",
               "DS:y:COMPUTE:z,2,*", "DS:y:COMPUTE:x,TIME,+", "DS:y:COMPUTE:y,1,+",
               "DS:y:COMPUTE:x,1,+:extra", "DS:y:COMPUTE:x,PREV,+", "DS:y:COMPUTE:x,COUNT,+"]:
        mk(DS, ds, RRA, **hw)
    mk("DS:y:COMPUTE:2,3,+", RRA, **hw)
    mk(DS, "DS:y:COMPUTE", RRA)
    mk(DS, "DS:y:COMPUTE:", RRA)

    # 10. Templates.
    for args in [[], ["-b", "1000000000"], ["-s", "30"], ["DS:c:GAUGE:60:U:U"],
                 ["DS:c:DDERIVE:60:U:U"], ["DS:a:GAUGE:60:U:U"], ["RRA:LAST:0.5:1:3"],
                 ["-s", "7", "RRA:AVERAGE:0.5:1m:1h"]]:
        add("o.rrd", "-t", "tpl.rrd", *args, inputs=["tpl"])
    add("o.rrd", "-t", "v5.rrd", inputs=["v5"])
    add("o.rrd", "-t", "v5.rrd", "DS:z:GAUGE:20:U:U", inputs=["v5"])
    add("o.rrd", "-t", "hw.rrd", inputs=["hw"], expect="divergent", note="CR-div-template-hw")
    add("o.rrd", "-t", "mhw.rrd", inputs=["mhw"], expect="divergent",
        note="CR-div-template-hw")
    add("o.rrd", "-t", "cp.rrd", inputs=["cp"])
    add("o.rrd", "-t", "text.rrd", inputs=["text"])
    add("o.rrd", "-t", "missing.rrd")
    add("o.rrd", "-t", "d.rrd", inputs=["@dir:d.rrd"])
    add("o.rrd", "-t", "tpl.rrd", "-t", "tpl.rrd", inputs=["tpl"])
    add("tpl.rrd", "-t", "tpl.rrd", "-b", "1000000000", inputs=["tpl"])

    # 11. Sources (prefill). Rondi supports only a narrow subset.
    src = dict(inputs=["src", "src2"], expect="unsupported", note="CR-div-source")
    s_defs = ["DS:a:GAUGE:120:U:U", "DS:b:COUNTER:120:U:U"]
    s_rras = ["RRA:AVERAGE:0.5:1:10", "RRA:MAX:0.5:3:4", "RRA:MIN:0.5:2:5", "RRA:LAST:0.5:1:7"]
    add("o.rrd", "-r", "src.rrd", "-s", "60", *s_defs, *s_rras, inputs=["src", "src2"],
        expect="divergent", note="CR-div-source")
    for extra in ["RRA:AVERAGE:0.5:2:8", "RRA:MAX:0.5:1:12", "RRA:MIN:0.5:4:3",
                  "RRA:LAST:0.5:3:5"]:
        add("o.rrd", "-r", "src.rrd", "-s", "60", *s_defs, *s_rras, extra, **src)
    add("o.rrd", "-r", "src.rrd", "-s", "60", "DS:a:GAUGE:120:U:U", "RRA:AVERAGE:0.5:2:6",
        **src)
    add("o.rrd", "-r", "src.rrd", "-r", "src2.rrd", "-s", "60", "DS:a:GAUGE:120:U:U",
        "DS:c:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:12", **src)
    add("o.rrd", "-r", "src2.rrd", "-r", "src.rrd", "-s", "60", "DS:a:GAUGE:120:U:U",
        "DS:c:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:12", **src)
    add("o.rrd", "-r", "src.rrd", "-r", "src2.rrd", "-s", "60", "DS:a=a[2]:GAUGE:120:U:U",
        "DS:x=b:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:12", **src)
    for ds in ["DS:a=a[3]:GAUGE:120:U:U", "DS:a=a[0]:GAUGE:120:U:U", "DS:a=zz:GAUGE:120:U:U",
               "DS:a:COUNTER:120:U:U", "DS:q=b:COUNTER:120:U:U"]:
        add("o.rrd", "-r", "src.rrd", "-s", "60", ds, "RRA:AVERAGE:0.5:1:12", **src)
    for opts in [["-b", "1000000500"], ["-b", "1000005000"], ["-s", "30"], []]:
        add("o.rrd", "-r", "src.rrd", *opts, "DS:a:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:12",
            **src)
    add("src.rrd", "-r", "src.rrd", "-s", "60", "DS:a:GAUGE:120:U:U", "RRA:AVERAGE:0.5:1:12",
        **src)
    add("o.rrd", "-r", "src.rrd", "-t", "src2.rrd", **src)
    add("o.rrd", "-r", "missing.rrd", *B, DS, RRA)
    add("o.rrd", "-r", "text.rrd", *B, DS, RRA, inputs=["text"])
    add("o.rrd", "-r", "d.rrd", *B, DS, RRA, inputs=["@dir:d.rrd"])
    add("o.rrd", "-r", "missing.rrd", "-s", "0", *B, DS, RRA)
    add("o.rrd", "-s", "0", "-r", "missing.rrd", DS, RRA)

    # 12. The output file.
    add("-", *B, DS, RRA)
    add("-", "-O", *B, DS, RRA, inputs=["@text:-"])
    add("o.rrd", *B, DS, RRA, inputs=["@text:o.rrd"])
    add("o.rrd", "-O", *B, DS, RRA, inputs=["@text:o.rrd"])
    add("o.rrd", "-O", inputs=["@text:o.rrd"])
    add("o.rrd", "-O", "-t", "missing.rrd", inputs=["@text:o.rrd"])
    add("o.rrd", "-O", "-r", "missing.rrd", *B, DS, RRA, inputs=["@text:o.rrd"])
    add("o.rrd", "-O", "-s", "0", *B, DS, RRA, inputs=["@text:o.rrd"])
    add("o.rrd", "-O", *B, "DS:bad", RRA, inputs=["@text:o.rrd"])
    add("o.rrd", "-O", *B, DS, RRA, inputs=["@text:t.rrd", "@link:o.rrd>t.rrd"])
    add("o.rrd", "-O", *B, DS, RRA, inputs=["@link:o.rrd>nowhere.rrd"],
        expect="divergent", note="CR-div-noclobber-dangling")
    add("o.rrd", "-O", *B, DS, RRA, inputs=["@dir:o.rrd"])
    add("o.rrd", *B, DS, RRA, inputs=["@dir:o.rrd"])
    add("o.rrd", *B, DS, RRA, inputs=["@text:t.rrd", "@link:o.rrd>t.rrd"])
    add("o.rrd", *B, DS, RRA, inputs=["@link:o.rrd>nowhere.rrd"])
    add("sub/o.rrd", *B, DS, RRA)
    add("o.rrd/", *B, DS, RRA)
    add(".", *B, DS, RRA)
    add("", *B, DS, RRA)
    add("", "-O", *B, DS, RRA)
    add("a" * 300 + ".rrd", *B, DS, RRA)

    # 13. Sizes the reference cannot allocate.
    for rows in ["2305843009213693952", "2305843009213693953", "4611686018427387904",
                 "18446744073709551615"]:
        mk(DS, f"RRA:AVERAGE:0.5:1:{rows}")
    mk("DS:a:GAUGE:20:U:U", "DS:b:GAUGE:20:U:U", "RRA:AVERAGE:0.5:1:1152921504606846976")

    # 14. An unreachable daemon.
    add("o.rrd", "--daemon", "unix:/nonexistent/rondi.sock", *B, DS, RRA,
        expect="divergent", note="CR-div-daemon-fallback")
    add("o.rrd", "-d", "unix:/nonexistent/rondi.sock", *B, "DS:bad", RRA,
        expect="divergent", note="CR-div-daemon-fallback")

    # 15. Order of the early checks.
    add("--step", "10")
    add("-O")
    add("o.rrd", "-b", "00:00 01.01.1979", "DS:bad", RRA)
    add("o.rrd", "-b", "00:00 01.01.1979", "-s", "0", DS, RRA)
    add("o.rrd", "-b", "00:00 01.01.1979", "-t", "missing.rrd")
    add("o.rrd", "-b", "end-1h", "-s", "0", DS, RRA)
    add("o.rrd", "-s", "0", "-b", "end-1h", DS, RRA)
    add("o.rrd", "-b", "1000000000", "-t", "missing.rrd", "DS:bad")
    add("o.rrd", "-b", "1000000000", DS, RRA, "-t", "missing.rrd")
    # 16. Number forms of the minimum, maximum and xff.
    for lo, hi in [("1e", "+3e+"), ("1.5E2", "2e3"), ("infinity", "U"), ("INF", "U"),
                   ("-nan", "U"), ("nanx", "U"), ("inf5", "U"), ("-infinityx", "U"),
                   ("banana", "U"), ("+nan", "U"), ("+inf", "U"), ("1e400", "U"),
                   ("1e-1022", "U"), ("1e1025", "U"), ("e5", "U"), (".", "U"), ("-", "U"),
                   ("0e400", "U"), ("0e1024", "U"), ("5e", "1e+")]:
        mk(f"DS:x:GAUGE:20:{lo}:{hi}", RRA)
    for xff in ["nanx", "1e-400", "0.5e", "abc", "0.99999999999999999", "0.77921045355292324"]:
        mk(DS, f"RRA:AVERAGE:{xff}:1:5")
    # 17. Long options without their value, and refusals before later errors.
    add("o.rrd", "-b", "1000000000", "--step")
    add("o.rrd", "--template")
    add("o.rrd", "--start")
    mk("RRA:HWPREDICT:20:0:0.01:5", DS, expect="unsupported", note="CR-unsupported")
    mk(RRA, "DS:y:COMPUTE:2,3,+", "foo", expect="unsupported", note="CR-unsupported")
    mk(DS, "DS:x:DCOUNTER:20:U:U", "RRA:MHWPREDICT:20:0.1:0.01:5", expect="unsupported",
       note="CR-unsupported")
    mk("DS:d:DCOUNTER:20:U:U", "RRA:MHWPREDICT:20:0.1:0.01:5", expect="unsupported",
       note="CR-unsupported")
    return c


def resize_cases():
    c = []

    def add(*args, inputs=("base",), expect="same", note="-"):
        c.append((expect, note, list(inputs), ["resize"] + list(args)))

    add(inputs=())
    add("base.rrd")
    add("base.rrd", "0")
    add("base.rrd", "0", "GROW")
    add("base.rrd", "0", "GROW", "1", "extra")
    add("resize.rrd", inputs=())
    add("resize.rrd", "0", "GROW", "1", inputs=["resize.rrd=base"])
    add("./resize.rrd", "0", "GROW", "1", inputs=["resize.rrd=base"],
        expect="divergent", note="RZ-div-existing-output")
    add("base.rrd", "0", "BAD", "0")
    add("base.rrd", "0", "grow", "1")
    add("base.rrd", "0", "GROWX", "1")
    add("base.rrd", "0", "SHRINK ", "1")
    add("base.rrd", "0", "GROW", "0")
    add("base.rrd", "0", "SHRINK", "0")
    add("base.rrd", "0", "GROW", "-1")
    add("base.rrd", "0", "SHRINK", "-3")
    add("base.rrd", "0", "GROW", "abc")
    add("base.rrd", "0", "BAD", "x")
    add("missing.rrd", "0", "GROW", "1", inputs=())
    add("missing.rrd", "9", "BAD", "0", inputs=())
    add("text.rrd", "0", "GROW", "1", inputs=["text"])
    add("base.rrd", "3", "GROW", "1")
    add("base.rrd", "-1", "GROW", "1")
    add("base.rrd", "4294967296", "GROW", "1")
    add("base.rrd", "18446744073709551616", "GROW", "1")
    add("base.rrd", "010", "GROW", "010")
    add("base.rrd", "0x1", "GROW", "0x2")
    add("base.rrd", "1x", "GROW", "2y")
    add("base.rrd", " 1", "GROW", " 2")
    add("base.rrd", "", "GROW", "1")
    add("base.rrd", "x", "GROW", "1")
    add("base.rrd", "+2", "SHRINK", "+1")
    add("base.rrd", "0", "SHRINK", "6")
    add("base.rrd", "0", "SHRINK", "7")
    add("base.rrd", "0", "SHRINK", "9223372036854775807")
    add("base.rrd", "0", "SHRINK", "99999999999999999999")
    for rra, rows in [(0, 6), (1, 4), (2, 5)]:
        for op, n in [("GROW", 1), ("GROW", 3), ("SHRINK", 1), ("SHRINK", rows - 1)]:
            add("base.rrd", str(rra), op, str(n))
    add("single.rrd", "0", "GROW", "4", inputs=["single"])
    add("single.rrd", "0", "SHRINK", "3", inputs=["single"])
    add("single.rrd", "0", "SHRINK", "4", inputs=["single"])
    for name in ["ptr0", "ptrmid", "ptrlast"]:
        for rra, op, n in [(0, "GROW", 2), (0, "SHRINK", 1), (0, "SHRINK", 2),
                           (0, "SHRINK", 4), (1, "GROW", 1), (1, "SHRINK", 2)]:
            add(f"{name}.rrd", str(rra), op, str(n), inputs=[name])
    for rra, op, n in [(0, "GROW", 2), (1, "GROW", 2), (1, "SHRINK", 3), (2, "GROW", 1),
                       (3, "SHRINK", 1), (5, "GROW", 2)]:
        add("hw.rrd", str(rra), op, str(n), inputs=["hw"])
    add("v5.rrd", "0", "GROW", "2", inputs=["v5"])
    add("v1.rrd", "0", "GROW", "1", inputs=["v1"])
    add("v1.rrd", "1", "SHRINK", "1", inputs=["v1"])
    add("trailing.rrd", "0", "GROW", "1", inputs=["trailing"])
    add("trailing.rrd", "1", "SHRINK", "1", inputs=["trailing"])
    add("short.rrd", "0", "GROW", "1", inputs=["short"])
    add("short.rrd", "1", "SHRINK", "1", inputs=["short"])
    add("currow.rrd", "0", "SHRINK", "1", inputs=["currow"])
    add("currow.rrd", "1", "GROW", "1", inputs=["currow"])
    add("base.rrd", "0", "GROW", "1", inputs=["base", "@text:resize.rrd"],
        expect="divergent", note="RZ-div-existing-output")
    add("base.rrd", "0", "GROW", "1", inputs=["base", "@text:t.rrd", "@link:resize.rrd>t.rrd"],
        expect="divergent", note="RZ-div-existing-output")
    add("base.rrd", "0", "GROW", "1", inputs=["base", "@dir:resize.rrd"],
        expect="divergent", note="RZ-div-existing-output")
    add("base.rrd", "0", "SHRINK", "9", inputs=["base", "@text:resize.rrd"])
    for name in ["trailing5", "trailing24", "trailing7"]:
        add(f"{name}.rrd", "0", "GROW", "1", inputs=[name])
    # A huge GROW is not pinned: the reference writes rows until the disk
    # fills (docs/compatibility/resize.md RZ-defect-huge-grow).
    return c


SUITES = {"create": create_cases, "resize": resize_cases}


# --------------------------------------------------------------------------

def prepare(workdir, inputs, stored):
    for item in inputs:
        if item.startswith("@link:"):
            name, target = item[6:].split(">")
            os.symlink(target, os.path.join(workdir, name))
        elif item.startswith("@dir:"):
            os.makedirs(os.path.join(workdir, item[5:]))
        elif item.startswith("@text:"):
            write_bytes(os.path.join(workdir, item[6:]), b"hello\n")
        else:
            name, _, source = item.partition("=")
            write_bytes(os.path.join(workdir, name if source else f"{name}.rrd"),
                        stored[source or name])


def snapshot(workdir, suite):
    """Every entry under workdir as {path: (kind, bytes)}."""
    out = {}
    for root, dirs, files in os.walk(workdir):
        for name in sorted(dirs + files):
            path = os.path.join(root, name)
            rel = os.path.relpath(path, workdir)
            if os.path.islink(path):
                out[rel] = ("link", os.readlink(path).encode())
            elif os.path.isdir(path):
                out[rel] = ("dir", b"")
            else:
                data = read_bytes(path)
                out[rel] = ("file", masked(data) if suite == "create" else data)
    return out


def run_case(rrdtool, suite, case, stored, tmp, pad):
    _, _, inputs, args = case
    workdir = tempfile.mkdtemp(dir=tmp)
    prepare(workdir, inputs, stored)
    before = snapshot(workdir, suite)
    status, out, err = run(rrdtool, args, workdir, pad)
    after = snapshot(workdir, suite)
    for root, dirs, _ in os.walk(workdir):
        for name in dirs:
            os.chmod(os.path.join(root, name), 0o755)
    shutil.rmtree(workdir)
    if out[:4] == b"RRD\0":
        out = masked(out)
    sections = [("status", status.encode()), ("stdout", out), ("stderr", err)]
    for rel in sorted(after):
        kind, body = after[rel]
        if before.get(rel) == after[rel] and kind == "file":
            sections.append((f"same:{rel}", b""))
        else:
            sections.append((f"{kind}:{rel}", body))
    return sections


def render(cid, sections):
    blob = b""
    for name, body in sections:
        blob += b"@@ %s %s %d\n" % (cid.encode(), name.encode(), len(body)) + body + b"\n"
    return blob


def parse_vectors(b):
    out = {}
    i = 0
    while i < len(b):
        nl = b.index(b"\n", i)
        _, cid, name, length = b[i:nl].decode().split(" ")
        out.setdefault(cid, []).append((name, b[nl + 1:nl + 1 + int(length)]))
        i = nl + 1 + int(length) + 1
    return out


def folded(sections):
    return [(n, fold_nan(b) if n.startswith("file:") or n == "stdout" else b)
            for n, b in sections]


def case_id(n, args):
    s = "-".join(args[1:]).lower()
    keep = "".join(ch if ch.isalnum() else "-" for ch in s)
    while "--" in keep:
        keep = keep.replace("--", "-")
    return f"{n:03d}-{keep.strip('-')[:40] or 'noargs'}"


def suite_dir(suite):
    return os.path.join(TESTS, suite)


def rondi_check(rondi, suite, case, stored, tmp, want):
    """Whether Rondi's result is consistent with the case's expectation."""
    expect = case[0]
    got = run_case(rondi, suite, case, stored, tmp, 0)
    if expect == "same":
        return folded(got) == folded(want)
    if expect == "divergent":
        return folded(got) != folded(want)
    status = dict(got)["status"]
    if expect == "undefined":
        return status in (b"0", b"1")
    clean = status == b"1" and dict(got)["stderr"].startswith(b"ERROR: ")
    untouched = all(n.startswith(("same:", "dir:", "link:")) or n in ("status", "stdout", "stderr")
                    for n, _ in got)
    return clean and untouched


def write(rrdtool, rondi, keep_inputs):
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        made = build_inputs(rrdtool, tmp)
        for suite, build in SUITES.items():
            base = suite_dir(suite)
            if keep_inputs:
                # Committed inputs carry their own random rows; keep them so
                # that existing vectors stay valid.
                folder = os.path.join(base, "inputs")
                for name in os.listdir(folder):
                    made[suite][name[:-4]] = read_bytes(os.path.join(folder, name))
            if os.path.isdir(base):
                shutil.rmtree(base)
            os.makedirs(os.path.join(base, "inputs"))
            os.makedirs(os.path.join(base, "expected"))
            with open(os.path.join(base, ".gitattributes"), "w") as handle:
                handle.write("# Vectors are compared byte for byte; never normalize them.\n"
                             "* -text -diff\ncases.tsv text eol=lf diff\n")
            stored = made[suite]
            for name, data in stored.items():
                write_bytes(os.path.join(base, "inputs", f"{name}.rrd"), data)
            lines = ["# id\texpect\tnote\tinputs\targs... (tab separated)"]
            blob = b""
            undefined = []
            for n, case in enumerate(build()):
                expect, note, inputs, args = case
                cid = case_id(n, args)
                first = run_case(rrdtool, suite, case, stored, tmp, 0)
                second = run_case(rrdtool, suite, case, stored, tmp, 70001)
                if first != second:
                    undefined.append(cid)
                    expect, note = "undefined", "varies-between-runs"
                    first = [s for s in first if s[0] == "status"]
                    first += [("stdout", b""), ("stderr", b"")]
                    case = (expect, note, inputs, args)
                if dict(first)["status"] in (b"signal", b"timeout") and expect == "same":
                    failures.append(f"{suite} {cid}: reference crashed; mark clean-error")
                if rondi and not rondi_check(rondi, suite, case, stored, tmp, first):
                    failures.append(f"{suite} {cid} [{expect}]: Rondi disagrees: {args}")
                blob += render(cid, first)
                lines.append("\t".join([cid, expect, note, ",".join(inputs) or "-"] + args))
            write_bytes(os.path.join(base, "expected", f"{suite}.vectors"), blob)
            with open(os.path.join(base, "cases.tsv"), "w") as handle:
                handle.write("\n".join(lines) + "\n")
            print(f"{suite}: {len(lines) - 1} cases, undefined: {undefined}")
    for failure in failures:
        print(failure)
    return 1 if failures else 0


def load_cases(suite):
    out = []
    with open(os.path.join(suite_dir(suite), "cases.tsv")) as handle:
        for line in handle:
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            cid, expect, note, inputs, *args = line.split("\t")
            out.append((cid, (expect, note, [] if inputs == "-" else inputs.split(","), args)))
    return out


def overrides(rrdtool, platform):
    differing = []
    with tempfile.TemporaryDirectory() as tmp:
        for suite in SUITES:
            base = suite_dir(suite)
            expected = os.path.join(base, "expected")
            stored = {name[:-4]: read_bytes(os.path.join(base, "inputs", name))
                      for name in os.listdir(os.path.join(base, "inputs"))}
            recorded = parse_vectors(read_bytes(os.path.join(expected, f"{suite}.vectors")))
            blob = b""
            for cid, case in load_cases(suite):
                if case[0] == "undefined":
                    continue
                got = run_case(rrdtool, suite, case, stored, tmp, 0)
                if folded(got) == folded(recorded[cid]):
                    continue
                differing.append(f"{suite}/{cid}")
                blob += render(cid, got)
            path = os.path.join(expected, f"{suite}.{platform}.vectors")
            if blob:
                write_bytes(path, blob)
            elif os.path.exists(path):
                os.remove(path)
    print(f"{len(differing)} {platform} overrides: {' '.join(differing)}")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=["write", "overrides"])
    parser.add_argument("--rrdtool", default="rrdtool")
    parser.add_argument("--rondi")
    parser.add_argument("--keep-inputs", action="store_true",
                        help="reuse the committed input files instead of new ones")
    parser.add_argument("--platform", default="linux")
    opts = parser.parse_args()
    version = run(opts.rrdtool, ["--version"], None)[1].decode()
    if not version.startswith("RRDtool 1.11.0 "):
        sys.exit(f"expected RRDtool 1.11.0, found: {version.splitlines()[:1]}")
    if opts.mode == "write":
        return write(opts.rrdtool, opts.rondi, opts.keep_inputs)
    return overrides(opts.rrdtool, opts.platform)


if __name__ == "__main__":
    sys.exit(main())
