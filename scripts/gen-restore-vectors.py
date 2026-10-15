#!/usr/bin/env python3
"""Rebuild the frozen `rrdtool restore` vectors.

The vectors live in crates/rondi-cli/tests/data/restore and are replayed by
crates/rondi-cli/tests/restore_spec_vectors.rs. The behavior they pin is
described in docs/compatibility/restore.md.

    scripts/gen-restore-vectors.py write [--rrdtool PATH]
        Create the seed dumps with the reference rrdtool, derive every input
        XML file from them, run every case and replace cases.tsv, inputs/,
        expected/base.vectors and expected/outputs/.

    scripts/gen-restore-vectors.py write --check
        Rebuild the case table from the committed seed dumps and compare it
        with cases.tsv and inputs/ without running any reference; exits 1 on
        a difference.

    scripts/gen-restore-vectors.py overrides --platform NAME [--rrdtool PATH]
        Replay the committed inputs with the reference rrdtool on another
        platform and write expected/NAME.vectors holding only the cases
        whose recorded result differs there. NAME is linux-x86_64 or
        linux-aarch64; run it inside a Linux container that has the pinned
        RRDtool 1.11.0 installed.

    scripts/gen-restore-vectors.py compare --program PATH [--platform NAME]
        Run every case against another implementation (for example Rondi
        under the rrdtool name) and print the cases whose result differs
        from the recorded vectors. This is a development aid; the Rust test
        is the gate.

Every reference run is repeated, with a different environment size each
time. A restored file places each archive's current row at random, so files
are compared in a normal form: each archive's rows rotated into
oldest-first order and the row pointers zeroed (see RS-60 in the
specification). A case whose result still varies, hangs or dies from a
signal is recorded with that flag and is never pinned byte for byte.

Only the standard library is used.
"""

import argparse
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "crates", "rondi-cli", "tests", "data", "restore")
ENV = {"TZ": "UTC", "LC_ALL": "C", "PATH": os.environ.get("PATH", "/usr/bin:/bin")}
TIMEOUT = 5


# --------------------------------------------------------------------------
# Seeds: real dumps written by the reference rrdtool.

def series(start, step, count, fmt):
    return [fmt(i, start + step * i) for i in range(1, count + 1)]


SEEDS = {
    "mini": (
        ["--start", "1000000000", "--step", "60", "DS:g:GAUGE:120:0:100",
         "RRA:AVERAGE:0.5:1:3", "RRA:MAX:0.5:2:2"],
        ["1000000060:10", "1000000120:20", "1000000170:35"],
        [],
    ),
    "pair": (
        ["--start", "1000000000", "--step", "300", "DS:g:GAUGE:600:0:100",
         "DS:c:COUNTER:600:U:U", "RRA:AVERAGE:0.5:1:4", "RRA:MAX:0.5:2:3",
         "RRA:LAST:0.5:1:2"],
        ["1000000300:1:100", "1000000600:2:200", "1000000900:U:300"],
        [],
    ),
    "types": (
        ["--start", "1000000000", "--step", "60",
         "DS:g:GAUGE:120:-5:100", "DS:c:COUNTER:120:U:U",
         "DS:d:DERIVE:120:-10:U", "DS:a:ABSOLUTE:120:0:1000",
         "DS:dc:DCOUNTER:120:U:U", "DS:dd:DDERIVE:120:U:U",
         "RRA:AVERAGE:0.5:1:6", "RRA:MIN:0.3:2:4", "RRA:MAX:0.7:2:4",
         "RRA:LAST:0.5:3:3"],
        series(1000000000, 60, 14,
               lambda i, t: f"{t}:{i * 1.5}:{i * i * 7}:{i * 3}:{i * 11}:{i * 0.25}:{-i * 0.5}"),
        [],
    ),
    "fresh": (
        ["--start", "1000000007", "--step", "10", "DS:x:GAUGE:20:U:U",
         "RRA:AVERAGE:0.5:1:4", "RRA:MAX:0.5:2:3"],
        [],
        [],
    ),
    "partial": (
        ["--start", "1000000000", "--step", "60", "DS:x:GAUGE:120:U:U",
         "DS:y:COUNTER:120:0:U", "RRA:AVERAGE:0.5:5:4", "RRA:MIN:0.5:5:4",
         "RRA:LAST:0.5:7:3"],
        series(1000000000, 60, 23, lambda i, t: f"{t}:{(i * 13) % 17}:{i * 100}")
        + ["1000001427:U:2400"],
        [],
    ),
    "wide": (
        ["--start", "1000000000", "--step", "60", "DS:w:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:120"],
        series(1000000000, 60, 130, lambda i, t: f"{t}:{(i * 37) % 101}.125"),
        [],
    ),
    "compute": (
        ["--start", "1000000000", "--step", "60", "DS:g:GAUGE:120:U:U",
         "DS:cp:COMPUTE:g,2,*", "RRA:AVERAGE:0.5:1:3"],
        ["1000000060:1", "1000000120:2"],
        [],
    ),
    "hw": (
        ["--start", "1000000000", "--step", "60", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:4", "RRA:HWPREDICT:6:0.1:0.0035:3"],
        ["1000000060:1", "1000000120:2", "1000000180:4"],
        [],
    ),
    "mhw": (
        ["--start", "1000000000", "--step", "60", "DS:g:GAUGE:120:U:U",
         "RRA:AVERAGE:0.5:1:4", "RRA:MHWPREDICT:6:0.1:0.0035:3"],
        ["1000000060:1", "1000000120:2"],
        [],
    ),
}

# Seeds also dumped with the other header styles of `rrdtool dump --header`.
HEADER_VARIANTS = {"mini": ["none", "xsd"], "pair": ["none", "xsd"]}


def make_seeds(rrdtool, work):
    seeds = {}
    for name, (create, updates, _) in SEEDS.items():
        path = os.path.join(work, f"{name}.rrd")
        subprocess.run([rrdtool, "create", path, *create], env=ENV, check=True)
        if updates:
            subprocess.run([rrdtool, "update", path, *updates], env=ENV, check=True)
        seeds[name] = subprocess.run(
            [rrdtool, "dump", path], env=ENV, check=True, capture_output=True
        ).stdout
        for header in HEADER_VARIANTS.get(name, []):
            seeds[f"{name}-hdr-{header}"] = subprocess.run(
                [rrdtool, "dump", "--header", header, path],
                env=ENV, check=True, capture_output=True,
            ).stdout
    return seeds


# --------------------------------------------------------------------------
# Cases.

class Case:
    def __init__(self, cid, xml, mode="file", opts=(), target="out.rrd", pre="none",
                 expect="same", input_name=None):
        self.id = cid
        self.xml = xml
        self.mode = mode
        self.opts = list(opts)
        self.target = target
        self.pre = pre
        self.expect = expect
        self.input = input_name or cid


PRE_CONTENT = {"none": None, "old": b"old\n", "long": b"\xab" * 5000}


def sub(xml, old, new, count=1):
    assert old in xml, (old, xml[:80])
    return xml.replace(old, new, count)


def element_spans(xml, tag):
    """Byte spans of every `<tag>...</tag>` (or `<tag/>`) in xml."""
    spans = []
    start = 0
    open_tag = f"<{tag}>".encode()
    close_tag = f"</{tag}>".encode()
    while True:
        at = xml.find(open_tag, start)
        if at < 0:
            return spans
        end = xml.find(close_tag, at)
        spans.append((at, end + len(close_tag)))
        start = at + 1


INT_FIELDS = {
    # name: (text in mini as it appears between the tags)
    "step": b"60",
    "lastupdate": b"1000000170",
    "minimal_heartbeat": b"120",
    "unknown_sec": b" 0 ",
    "pdp_per_row": b"1",
    "unknown_datapoints": b"0",
}

INT_VALUES = [
    ("empty", b""), ("blank", b"  "), ("alpha", b"abc"), ("padded", b" 7 "),
    ("hex", b"0x1f"), ("octal", b"010"), ("bad-octal", b"09"), ("neg", b"-1"),
    ("plus", b"+5"), ("exp", b"1e3"), ("trail", b"12abc"), ("two", b"3 4"),
    ("zero", b"0"), ("u32", b"4294967296"), ("i64max", b"9223372036854775807"),
    ("i64over", b"9223372036854775808"), ("u64max", b"18446744073709551615"),
    ("u64over", b"18446744073709551616"), ("negbig", b"-9223372036854775809"),
    ("frac", b"1.5"),
]

FLOAT_FIELDS = {
    "min": (b"<min>", b"0.0000000000e+00"),
    "max": (b"<max>", b"1.0000000000e+02"),
    "pdpvalue": (b"<value>", b"1.0500000000e+03"),
    "xff": (b"<xff>", b"5.0000000000e-01"),
    "primary": (b"<primary_value>", b"2.5000000000e+01"),
    "secondary": (b"<secondary_value>", b"0.0000000000e+00"),
    "cdpvalue": (b"<value>", b"NaN"),
    "v": (b"<v>", b"1.3333333333e+01"),
}

FLOAT_VALUES = [
    ("nan", b"NaN"), ("nan-lc", b"nan"), ("nan-uc", b"NAN"), ("nan-neg", b"-nan"),
    ("nan-word", b"banana"), ("nan-tail", b"nanx"), ("inf", b"inf"), ("inf-neg", b"-inf"),
    ("inf-word", b"Infinity"), ("inf-cap", b"-INF"), ("inf-mid", b"xinfx"),
    ("u", b"U"), ("empty", b""), ("alpha", b"abc"), ("dash", b"-"), ("exp-only", b"e5"),
    ("padded", b" 2.5 "), ("two", b"2 3"), ("trail", b"2junk"), ("comma", b"1,5"),
    ("hex", b"0x10"), ("hexfloat", b"0x1p4"), ("dot-lead", b".5"), ("dot-trail", b"5."),
    ("plus", b"+.5e1"), ("exp-cut", b"1e"), ("exp-sign", b"1e+"), ("zeros", b"00012"),
    ("huge", b"1e309"), ("huge-neg", b"-1e309"), ("tiny", b"1e-400"), ("negzero", b"-0"),
    ("max", b"1.7976931348623157e308"), ("long", b"3.14159265358979323846264338327950288"),
    ("epoch-frac", b"1000000170.123456789"), ("nbsp", b"\xc2\xa01"),
]


def mini_cases(mini):
    cases = []
    add = cases.append

    # Integer field grammar.
    for field, text in INT_FIELDS.items():
        old = f"<{field}>".encode() + text + f"</{field}>".encode()
        for label, value in INT_VALUES:
            new = f"<{field}>".encode() + value + f"</{field}>".encode()
            add(Case(f"int-{field}-{label}", sub(mini, old, new)))

    # Float field grammar.
    for field, (tag, text) in FLOAT_FIELDS.items():
        end = b"</" + tag[1:]
        old = tag + text + end
        values = FLOAT_VALUES if field in ("v", "min", "xff") else FLOAT_VALUES[::3]
        for label, value in values:
            add(Case(f"float-{field}-{label}", sub(mini, old, tag + value + end)))

    # Names, types, CFs, last_ds.
    name = b"<name> g </name>"
    for label, value in [
        ("empty", b""), ("blank", b"   "), ("spaced", b" a b "), ("n19", b"abcdefghijklmnopqrs"),
        ("n20", b"abcdefghijklmnopqrst"), ("n21", b"abcdefghijklmnopqrstu"),
        ("punct", b"g:h"), ("utf8", b"\xc3\xa9t\xc3\xa9"), ("utf8-20-bytes", b"\xc3\xa9" * 10),
        ("tab", b"\tg\t"), ("newline", b"\ng\n"),
    ]:
        add(Case(f"name-{label}", sub(mini, name, b"<name>" + value + b"</name>")))
    dstype = b"<type> GAUGE </type>"
    for value in [b"COUNTER", b"DERIVE", b"ABSOLUTE", b"DCOUNTER", b"DDERIVE", b"gauge",
                  b"FOO", b"", b"GAUGE GAUGE", b"COMPUTE"]:
        label = value.decode().replace(" ", "-").lower() or "empty"
        if value == b"gauge":
            label = "gauge-lc"
        add(Case(f"type-{label}", sub(mini, dstype, b"<type>" + value + b"</type>")))
    cf = b"<cf>AVERAGE</cf>"
    for value in [b"MIN", b"MAX", b"LAST", b"average", b"AVG", b"", b" AVERAGE ",
                  b"HWPREDICT", b"SEASONAL", b"FAILURES"]:
        label = value.decode().strip().lower() or "empty"
        if value == b" AVERAGE ":
            label = "padded"
        if value == b"average":
            label = "average-lc"
        add(Case(f"cf-{label}", sub(mini, cf, b"<cf>" + value + b"</cf>")))
    lastds = b"<last_ds>35</last_ds>"
    for label, value in [("u", b"U"), ("empty", b""), ("n29", b"1" * 29), ("n30", b"1" * 30),
                         ("n31", b"1" * 31), ("text", b"hello"), ("spaced", b" 5 6 "),
                         ("float", b"1.5e3")]:
        add(Case(f"lastds-{label}", sub(mini, lastds, b"<last_ds>" + value + b"</last_ds>")))

    # Versions.
    version = b"<version>0003</version>"
    for label, value in [("0001", b"0001"), ("0002", b"0002"), ("0004", b"0004"),
                         ("0005", b"0005"), ("0006", b"0006"), ("3", b"3"), ("abc", b"abc"),
                         ("long", b"00031"), ("empty", b"")]:
        add(Case(f"version-{label}", sub(mini, version, b"<version>" + value + b"</version>")))
    add(Case("version-absent", sub(mini, version, b"")))
    # Version 1 and 2 dumps carry xff directly in the archive.
    flat = sub(mini, b"<params>\n\t\t<xff>5.0000000000e-01</xff>\n\t\t</params>",
               b"<xff>5.0000000000e-01</xff>", 2)
    for label in ("0001", "0002", "0003"):
        add(Case(f"version-{label}-flat-xff",
                 sub(flat, version, b"<version>" + label.encode() + b"</version>")))
    add(Case("version-absent-flat-xff", sub(flat, version, b"")))

    # Remove, duplicate or rename each element.
    for tag in ["version", "step", "lastupdate", "name", "type", "minimal_heartbeat", "min",
                "max", "last_ds", "value", "unknown_sec", "cf", "pdp_per_row", "params", "xff",
                "cdp_prep", "primary_value", "secondary_value", "unknown_datapoints",
                "database", "row", "v", "ds", "rra"]:
        spans = element_spans(mini, tag)
        a, b = spans[0]
        add(Case(f"drop-{tag}", mini[:a] + mini[b:]))
        add(Case(f"dup-{tag}", mini[:b] + mini[a:b] + mini[b:]))
        add(Case(f"bogus-after-{tag}", mini[:b] + b"<bogus>1</bogus>" + mini[b:]))
        upper = mini[a:b].replace(f"<{tag}>".encode(), f"<{tag.upper()}>".encode(), 1)
        upper = upper.replace(f"</{tag}>".encode(), f"</{tag.upper()}>".encode(), 1)
        add(Case(f"upper-{tag}", mini[:a] + upper + mini[b:]))
    # Every second archive's elements too.
    for tag in ["cdp_prep", "database", "row"]:
        a, b = element_spans(mini, tag)[-1]
        add(Case(f"drop-last-{tag}", mini[:a] + mini[b:]))
    a, b = element_spans(mini, "ds")[-1]
    add(Case("drop-cdp-ds", mini[:a] + mini[b:]))
    add(Case("dup-cdp-ds", mini[:b] + mini[a:b] + mini[b:]))

    # Swaps of adjacent siblings.
    def swap(first, second, label):
        a1, b1 = element_spans(mini, first)[0]
        a2, b2 = element_spans(mini, second)[0]
        assert b1 <= a2
        add(Case(f"swap-{label}", mini[:a1] + mini[a2:b2] + mini[b1:a2] + mini[a1:b1] + mini[b2:]))
    swap("version", "step", "version-step")
    swap("step", "lastupdate", "step-lastupdate")
    swap("name", "type", "name-type")
    swap("min", "max", "min-max")
    swap("last_ds", "value", "lastds-value")
    swap("cf", "pdp_per_row", "cf-pdp")
    swap("cdp_prep", "database", "cdp-database")
    swap("primary_value", "secondary_value", "primary-secondary")
    ds_a, ds_b = element_spans(mini, "ds")[0]
    add(Case("swap-ds-lastupdate", sub(mini[:ds_a] + mini[ds_b:], b"<lastupdate>",
                                        mini[ds_a:ds_b] + b"<lastupdate>")))
    rra_a, rra_b = element_spans(mini, "rra")[0]
    add(Case("swap-rra-ds", mini[:ds_a] + mini[rra_a:rra_b] + mini[ds_b:rra_a] + mini[ds_a:ds_b] + mini[rra_b:]))

    # Structural edits.
    row = b"<row><v>NaN</v></row>"
    add(Case("row-two-values", sub(mini, row, b"<row><v>NaN</v><v>1</v></row>")))
    add(Case("row-no-values", sub(mini, row, b"<row></row>")))
    add(Case("row-self-closing", sub(mini, row, b"<row/>")))
    add(Case("v-self-closing", sub(mini, row, b"<row><v/></row>")))
    add(Case("v-empty", sub(mini, row, b"<row><v></v></row>")))
    add(Case("row-extra", sub(mini, row, row + row)))
    add(Case("rows-all-gone", b"".join(
        part for part in [mini[:element_spans(mini, "database")[0][0]],
                          b"<database></database>",
                          mini[element_spans(mini, "database")[0][1]:]])))
    add(Case("database-self-closing", mini[:element_spans(mini, "database")[0][0]] + b"<database/>"
             + mini[element_spans(mini, "database")[0][1]:]))
    add(Case("rrd-empty", b"<rrd></rrd>"))
    add(Case("rrd-header-only", b"<rrd><version>0003</version><step>60</step><lastupdate>1000000000</lastupdate></rrd>"))
    no_rra = mini[:element_spans(mini, "rra")[0][0]] + b"</rrd>\n"
    add(Case("no-rra", no_rra))
    ds_only = mini[:ds_a] + mini[ds_b:]
    add(Case("no-ds", ds_only))
    no_ds_anywhere = ds_only
    for _ in range(2):
        spans = element_spans(no_ds_anywhere, "ds")
        if spans:
            a, b = spans[0]
            no_ds_anywhere = no_ds_anywhere[:a] + no_ds_anywhere[b:]
    add(Case("no-ds-at-all", sub(no_ds_anywhere, row, b"<row></row>")))
    add(Case("two-roots", mini + mini[mini.index(b"<rrd>"):]))
    add(Case("nested-rrd", sub(mini, b"<step>", b"<rrd></rrd><step>")))
    add(Case("attr-on-step", sub(mini, b"<step>", b'<step unit="s">')))
    add(Case("attr-on-rrd", sub(mini, b"<rrd>", b'<rrd x="1">')))
    add(Case("ns-prefix", sub(mini, b"<step>60</step>", b'<p:step xmlns:p="urn:x">60</p:step>')))
    add(Case("ds-attr", sub(mini, b"<ds>", b'<ds a="b">')))
    add(Case("value-split-comment", sub(mini, b"<step>60<", b"<step>6<!-- c -->0<")))
    add(Case("value-split-pi", sub(mini, b"<step>60<", b"<step>6<?p x?>0<")))
    add(Case("value-comment-first", sub(mini, b"<step>60<", b"<step><!-- c -->60<")))
    add(Case("value-cdata", sub(mini, b"<step>60<", b"<step><![CDATA[60]]><")))
    add(Case("value-cdata-mixed", sub(mini, b"<step>60<", b"<step>6<![CDATA[0]]><")))
    add(Case("value-charref", sub(mini, b"<step>60<", b"<step>&#54;0<")))
    add(Case("value-hexref", sub(mini, b"<step>60<", b"<step>&#x36;0<")))
    add(Case("value-amp", sub(mini, b"<name> g </name>", b"<name>a&amp;b</name>")))
    add(Case("value-lt", sub(mini, b"<name> g </name>", b"<name>a&lt;b</name>")))
    add(Case("value-undefined-entity", sub(mini, b"<step>60<", b"<step>&nope;60<")))
    add(Case("value-internal-entity", sub(
        sub(mini, b'<!DOCTYPE rrd SYSTEM "https://oss.oetiker.ch/rrdtool/rrdtool.dtd">',
            b'<!DOCTYPE rrd [<!ENTITY s "60">]>'), b"<step>60<", b"<step>&s;<")))
    add(Case("value-nested-element", sub(mini, b"<step>60<", b"<step><b>60</b><")))
    add(Case("value-newlines", sub(mini, b"<step>60<", b"<step>\n60\n<")))
    add(Case("value-crlf-lines", mini.replace(b"\n", b"\r\n")))
    add(Case("value-cr-lines", mini.replace(b"\n", b"\r")))
    add(Case("no-newlines", mini.replace(b"\n", b"").replace(b"\t", b"")))
    add(Case("no-final-newline", mini.rstrip(b"\n")))
    add(Case("comments-everywhere", mini.replace(b"><", b"><!-- c --><")))
    add(Case("pis-everywhere", mini.replace(b"\n\t", b"<?x y?>\n\t")))

    # Documents and prologs.
    root = mini[mini.index(b"<rrd>"):]
    for label, xml in [
        ("bare-root", root),
        ("no-doctype", sub(mini, b'<!DOCTYPE rrd SYSTEM "https://oss.oetiker.ch/rrdtool/rrdtool.dtd">\n', b"")),
        ("decl-1-1", sub(mini, b'version="1.0"', b'version="1.1"')),
        ("decl-2-0", sub(mini, b'version="1.0"', b'version="2.0"')),
        ("decl-standalone", sub(mini, b'encoding="utf-8"', b'encoding="utf-8" standalone="yes"')),
        ("decl-latin1", sub(mini, b"utf-8", b"ISO-8859-1")),
        ("decl-ascii", sub(mini, b"utf-8", b"US-ASCII")),
        ("decl-unknown-charset", sub(mini, b"utf-8", b"no-such-charset")),
        ("decl-late", b"\n" + mini),
        ("decl-twice", b'<?xml version="1.0"?>\n' + mini),
        ("bom", b"\xef\xbb\xbf" + mini),
        ("bom-bare", b"\xef\xbb\xbf" + root),
        ("bom-twice", b"\xef\xbb\xbf\xef\xbb\xbf" + mini),
        ("utf16le-bom", b"\xff\xfe" + sub(mini, b"utf-8", b"utf-16").decode().encode("utf-16-le")),
        ("utf16be-bom", b"\xfe\xff" + sub(mini, b"utf-8", b"utf-16").decode().encode("utf-16-be")),
        ("latin1-name", sub(sub(mini, b"utf-8", b"ISO-8859-1"), b"<name> g </name>", b"<name>\xe9</name>")),
        ("bad-utf8", sub(mini, b"<name> g </name>", b"<name>\xff</name>")),
        ("nul-in-value", sub(mini, b"<step>60<", b"<step>6\x000<")),
        ("ctrl-in-value", sub(mini, b"<step>60<", b"<step>6\x010<")),
        ("charref-1", sub(mini, b"<step>60<", b"<step>&#1;60<")),
        ("comment-hyphens", sub(mini, b"<!-- Round Robin Database Dump -->", b"<!-- a -- b -->")),
        ("comment-short", sub(mini, b"<!-- Round Robin Database Dump -->", b"<!-->")),
        ("comment-unclosed", sub(mini, b"<!-- Round Robin Database Dump -->", b"<!-- open")),
        ("doctype-public", sub(mini, b'SYSTEM "https://oss.oetiker.ch/rrdtool/rrdtool.dtd"',
                               b'PUBLIC "-//x//y//EN" "rrdtool.dtd"')),
        ("doctype-other", sub(mini, b"<!DOCTYPE rrd ", b"<!DOCTYPE other ")),
        ("doctype-after-root", sub(mini, b'<!DOCTYPE rrd SYSTEM "https://oss.oetiker.ch/rrdtool/rrdtool.dtd">\n', b"").replace(b"<rrd>", b"<rrd><!DOCTYPE x>", 1)),
        ("empty", b""),
        ("blank", b"\n\n  \n"),
        ("text", b"hello world\n"),
        ("not-xml-binary", bytes(range(256))),
        ("decl-only", b'<?xml version="1.0"?>\n'),
        ("comment-only", b"<!-- nothing -->\n"),
        ("tail-ws", mini + b"\n \t\n"),
        ("tail-comment", mini + b"<!-- c -->\n"),
        ("tail-pi", mini + b"<?pi x?>\n"),
        ("tail-element", mini + b"<x/>\n"),
        ("tail-text", mini + b"junk\n"),
        ("tail-char", mini + b"x"),
        ("tail-nul", mini + b"\x00"),
        ("mismatch", sub(mini, b"</step>", b"</stp>")),
        ("unclosed-end", sub(mini, b"</step>", b"</step")),
        ("lt-in-text", sub(mini, b"<step>60<", b"<step>60<<")),
        ("cdata-end-in-text", sub(mini, b"<step>60<", b"<step>6]]>0<")),
        ("dup-attr", sub(mini, b"<step>", b"<step a='1' a='2'>")),
        ("unquoted-attr", sub(mini, b"<step>", b"<step a=1>")),
        ("missing-root-end", mini.replace(b"</rrd>", b"")),
        ("ctrl-z", mini + b"\x1a\ntrailing garbage <x>\n"),
        ("ctrl-z-mid", sub(mini, b"<step>60</step>", b"<step>60</step>\x1a")),
    ]:
        add(Case(f"doc-{label}", xml))
    for cut in [1, 40, 100, 150, 300, 511, 512, 513, 700, 1023, 1024, 1025, 1300, len(mini) - 8, len(mini) - 1]:
        add(Case(f"cut-{cut}", mini[:cut]))

    # Range check and value limits.
    big = sub(mini, b"<v>1.3333333333e+01</v>", b"<v>1.5e+03</v>")
    big = sub(big, b"<v>2.5000000000e+01</v>", b"<v>-5</v>")
    add(Case("range-off", big))
    add(Case("range-on", big, opts=["-r"], input_name="range-off"))
    add(Case("range-on-long", big, opts=["--range-check"], input_name="range-off"))
    edge = sub(sub(mini, b"<v>1.3333333333e+01</v>", b"<v>100</v>"), b"<v>2.5000000000e+01</v>", b"<v>0</v>")
    add(Case("range-edges", edge, opts=["-r"]))
    tiny = sub(mini, b"<v>1.3333333333e+01</v>", b"<v>-1e-300</v>")
    add(Case("range-just-below", tiny, opts=["-r"]))
    infs = sub(sub(mini, b"<v>1.3333333333e+01</v>", b"<v>inf</v>"), b"<v>2.5000000000e+01</v>", b"<v>-inf</v>")
    add(Case("range-infinities", infs, opts=["-r"]))
    add(Case("range-infinities-off", infs, input_name="range-infinities"))
    nolimit = sub(sub(big, b"<min>0.0000000000e+00</min>", b"<min>NaN</min>"),
                  b"<max>1.0000000000e+02</max>", b"<max>NaN</max>")
    add(Case("range-unbounded", nolimit, opts=["-r"]))
    inverted = sub(sub(mini, b"<min>0.0000000000e+00</min>", b"<min>50</min>"),
                   b"<max>1.0000000000e+02</max>", b"<max>10</max>")
    add(Case("range-inverted", inverted, opts=["-r"]))
    add(Case("range-inverted-off", inverted, input_name="range-inverted"))
    cdp_out = sub(mini, b"<primary_value>2.5000000000e+01</primary_value>",
                  b"<primary_value>5e3</primary_value>")
    add(Case("range-cdp-state", cdp_out, opts=["-r"]))

    # Options and targets.
    add(Case("opt-force", mini, opts=["-f"], pre="old", input_name="seed-mini"))
    add(Case("opt-force-long", mini, opts=["--force-overwrite"], pre="old", input_name="seed-mini"))
    add(Case("opt-no-force", mini, pre="old", input_name="seed-mini"))
    add(Case("opt-force-longer-file", mini, opts=["-f"], pre="long", input_name="seed-mini", expect="truncates"))
    add(Case("opt-force-no-file", mini, opts=["-f"], input_name="seed-mini"))
    add(Case("opt-rf", mini, opts=["-rf"], pre="old", input_name="seed-mini"))
    add(Case("opt-fr-separate", mini, opts=["-f", "-r"], pre="old", input_name="seed-mini"))
    add(Case("opt-dashdash", mini, opts=["--"], input_name="seed-mini"))
    add(Case("opt-bad-short", mini, opts=["-z"], input_name="seed-mini"))
    add(Case("opt-bad-long", mini, opts=["--bogus"], input_name="seed-mini"))
    add(Case("opt-abbrev", mini, opts=["--range"], input_name="seed-mini"))
    add(Case("opt-help-h", mini, opts=["-h"], input_name="seed-mini"))
    add(Case("opt-help-q", mini, opts=["-?"], input_name="seed-mini"))
    add(Case("opt-help-long", mini, opts=["--help"], input_name="seed-mini"))
    add(Case("opt-arg-to-flag", mini, opts=["--range-check=1"], input_name="seed-mini"))
    add(Case("target-stdout", mini, target="-", input_name="seed-mini"))
    add(Case("target-missing-dir", mini, target="nodir/out.rrd", input_name="seed-mini"))
    add(Case("target-is-dir", mini, target=".", input_name="seed-mini"))
    add(Case("target-is-dir-force", mini, target=".", opts=["-f"], input_name="seed-mini"))
    add(Case("error-keeps-existing", sub(mini, b"<step>60<", b"<step>abc<"), pre="old",
             opts=["-f"], input_name="int-step-alpha"))
    add(Case("syntax-error-keeps-existing", sub(mini, b"</step>", b"</stp>"), pre="old",
             opts=["-f"], input_name="doc-mismatch"))
    add(Case("trailing-error-existing", mini + b"<x/>\n", pre="old", opts=["-f"],
             input_name="doc-tail-element"))
    add(Case("xff-error-existing", sub(mini, b"<xff>5.0000000000e-01</xff>", b"<xff>U</xff>"),
             pre="old", opts=["-f"], input_name="float-xff-u"))
    add(Case("xff-error-no-force", sub(mini, b"<xff>5.0000000000e-01</xff>", b"<xff>U</xff>"),
             pre="old", input_name="float-xff-u"))
    add(Case("trailing-stdin-existing", mini + b"<x/>\n", mode="stdin", pre="old", opts=["-f"],
             input_name="doc-tail-element"))
    add(Case("stdin-target-stdout", mini, mode="stdin", target="-", input_name="seed-mini"))
    add(Case("stdin-no-force", mini, mode="stdin", pre="old", input_name="seed-mini"))
    add(Case("silent-existing", sub(mini, b"<xff>5.0000000000e-01</xff>", b""), pre="old",
             opts=["-f"], input_name="drop-xff"))
    return cases


def seed_cases(seeds):
    cases = []
    for name, xml in sorted(seeds.items()):
        cases.append(Case(f"seed-{name}", xml))
    types = seeds["types"]
    # Every type and CF with values that cover each row slot.
    cases.append(Case("types-range", types, opts=["-r"], input_name="seed-types"))
    partial = seeds["partial"]
    cases.append(Case("partial-udp-big", sub(partial, b"<unknown_datapoints>", b"<unknown_datapoints>7", 1)))
    wide = seeds["wide"]
    # Errors at many places in a longer file, for the line number rule.
    pos = 0
    count = 0
    while count < 24:
        at = wide.find(b"<v>", pos)
        if at < 0:
            break
        pos = at + 1 + 211 * (count % 3 + 1)
        end = wide.find(b"</v>", at)
        cases.append(Case(f"wide-bad-v-{at}", wide[:at + 3] + b"zz" + wide[end:]))
        count += 1
    for tag in ["step", "minimal_heartbeat", "pdp_per_row", "unknown_datapoints", "xff"]:
        a, b = element_spans(wide, tag)[0]
        inner = wide[a:b]
        bad = inner[: inner.index(b">") + 1] + b"zz" + inner[inner.rindex(b"<"):]
        cases.append(Case(f"wide-bad-{tag}", wide[:a] + bad + wide[b:]))
    cases.append(Case("wide-unknown-tag-late", sub(wide, b"</database>", b"<bogus/></database>")))
    cases.append(Case("wide-mismatch-late", sub(wide, b"</database>", b"</databse>")))
    cases.append(Case("wide-cut-mid", wide[: len(wide) // 2]))
    cases.extend(number_cases(wide))
    cases.extend(tiny_cases())
    cases.extend(walk_cases())
    return cases


def walk_cases():
    """Probes of the element walk (section 3), the error state (RS-39a) and
    the Holt-Winters and COMPUTE elements (RS-53, RS-70)."""
    head = TINY_HEAD
    ds = TINY_DS
    params = b"<params><xff>0.5</xff></params>"
    cdp = b"<cdp_prep><ds><value>1</value></ds></cdp_prep>"
    database = b"<database><row><v>1</v></row></database>"

    def rra(cf=b"<cf>AVERAGE</cf>", pdp=b"<pdp_per_row>1</pdp_per_row>", par=params, prep=cdp, db=database):
        return b"<rra>" + cf + pdp + par + prep + db + b"</rra>"

    def doc(h=head, d=ds, r=None, tail=b"</rrd>"):
        return h + d + (rra() if r is None else r) + tail

    def ds_with(body):
        return b"<ds><name>a</name><type>GAUGE</type>" + body + b"</ds>"

    probes = {
        "root-other": b"<x/>",
        "root-other-pair": b"<x></x>",
        "root-upper": doc().replace(b"<rrd>", b"<RRD>").replace(b"</rrd>", b"</RRD>"),
        "text-in-rrd": doc(h=head.replace(b"<step>", b"junk<step>")),
        "text-in-ds": doc(d=ds.replace(b"<type>", b"junk<type>")),
        "text-in-rra": doc(r=rra(pdp=b"junk<pdp_per_row>1</pdp_per_row>")),
        "text-in-params": doc(r=rra(par=b"<params>junk<xff>0.5</xff></params>")),
        "text-after-xff": doc(r=rra(par=b"<params><xff>0.5</xff>junk</params>")),
        "text-in-cdp": doc(r=rra(prep=b"<cdp_prep>junk<ds><value>1</value></ds></cdp_prep>")),
        "text-in-cdp-ds": doc(r=rra(prep=b"<cdp_prep><ds>junk<value>1</value></ds></cdp_prep>")),
        "text-after-cdp-value": doc(r=rra(prep=b"<cdp_prep><ds><value>1</value>junk</ds></cdp_prep>")),
        "text-in-database": doc(r=rra(db=b"<database>junk<row><v>1</v></row></database>")),
        "text-after-row": doc(r=rra(db=b"<database><row><v>1</v></row>junk</database>")),
        "text-in-row": doc(r=rra(db=b"<database><row>junk<v>1</v></row></database>")),
        "text-after-v": doc(r=rra(db=b"<database><row><v>1</v>junk</row></database>")),
        "text-after-value": doc(h=head.replace(b"</step>", b"</step>junk")),
        "text-after-ds": doc(d=ds + b"junk"),
        "text-after-rra": doc(tail=b"junk</rrd>"),
        "params-bogus-empty": doc(r=rra(par=b"<params><bogus/><xff>0.5</xff></params>")),
        "params-bogus-pair": doc(r=rra(par=b"<params><bogus></bogus><xff>0.5</xff></params>")),
        "params-bogus-text": doc(r=rra(par=b"<params><bogus>1</bogus><xff>0.5</xff></params>")),
        "params-bogus-last": doc(r=rra(par=b"<params><xff>0.5</xff><bogus></bogus></params>")),
        "params-bad-xff-then-ok": doc(r=rra(par=b"<params><xff>x</xff><xff>0.25</xff></params>")),
        "params-legacy-values": doc(r=rra(par=b"<params><value>0.5</value><value>0.25</value></params>")),
        "params-legacy-one": doc(r=rra(par=b"<params><value>0.5</value></params>")),
        "cdp-ds-bogus": doc(r=rra(prep=b"<cdp_prep><ds><bogus>1</bogus></ds></cdp_prep>")),
        "cdp-ds-bad-value-then-ok": doc(r=rra(prep=b"<cdp_prep><ds><value>x</value><value>1</value></ds></cdp_prep>")),
        "cdp-ds-empty-then-garbage": doc(r=rra(prep=b"<cdp_prep><ds></ds></cdp_prep>"), tail=b"<bogus/></rrd>"),
        "cdp-other-elem": doc(r=rra(prep=b"<cdp_prep><x><value>1</value></x></cdp_prep>")),
        "database-other-elem": doc(r=rra(db=b"<database><x/></database>")),
        "database-other-after-row": doc(r=rra(db=b"<database><row><v>1</v></row><x/></database>")),
        "row-other-elem": doc(r=rra(db=b"<database><row><x>1</x></row></database>")),
        "row-v-nested": doc(r=rra(db=b"<database><row><v><x/></v></row></database>")),
        "row-v-bad-range": doc(r=rra(db=b"<database><row><v>x</v></row></database>")),
        "value-nested-pair": doc(h=head.replace(b"<step>60</step>", b"<step><x></x></step>")),
        "value-nested-after-text": doc(h=head.replace(b"<step>60</step>", b"<step>60<x/></step>")),
        "value-text-comment": doc(h=head.replace(b"<step>60</step>", b"<step>60<!--c--></step>")),
        "value-cdata-then-text": doc(h=head.replace(b"<step>60</step>", b"<step><![CDATA[x]]>60</step>")),
        "name-cdata": doc(d=ds.replace(b"<name>a</name>", b"<name><![CDATA[q]]></name>")),
        "value-pi-first": doc(h=head.replace(b"<step>60</step>", b"<step><?p?>60</step>")),
        "eof-in-rrd": head,
        "eof-in-ds": head + b"<ds><name>a</name>",
        "eof-in-rra": head + ds + b"<rra><cf>AVERAGE</cf>",
        "eof-in-row": head + ds + b"<rra><cf>AVERAGE</cf><database><row><v>1</v>",
        "ds-after-rra-no-cdp": head + b"<rra><cf>AVERAGE</cf></rra>" + ds + b"</rrd>",
        # A syntax error met while a value read sits on a comment.
        "spin-after-comment": doc(h=head.replace(b"<step>60</step>", b"<step><!-- c -->\n</stp>")),
        "error-after-text": doc(h=head.replace(b"<step>60</step>", b"<step>60\n</stp>")),
        # The error state (RS-39a): conversions leave errno behind.
        "errno-huge-then-junk": doc(d=ds_with(b"<min>1e309</min><max>2junk</max>")),
        "errno-plain-then-junk": doc(d=ds_with(b"<min>1</min><max>2junk</max>")),
        "errno-int-resets": doc(d=ds_with(b"<min>1e309</min><minimal_heartbeat>1</minimal_heartbeat><max>2junk</max>")),
        "errno-tiny-then-junk": doc(d=ds_with(b"<min>1e-400</min><max>2junk</max>")),
        "errno-exp-1024": doc(d=ds_with(b"<min>1e1024</min>")),
        "errno-exp-1025": doc(d=ds_with(b"<min>1e1025</min>")),
        "errno-exp-m1021": doc(d=ds_with(b"<min>1e-1021</min>")),
        "errno-exp-m1022": doc(d=ds_with(b"<min>1e-1022</min>")),
        "errno-frac-exp-m1022": doc(d=ds_with(b"<min>1.5e-1021</min>")),
        "errno-exp-many-digits": doc(d=ds_with(b"<min>1e0000000000000000000001</min>")),
        # Holt-Winters parameters and state.
        "hw-params": doc(r=rra(cf=b"<cf>HWPREDICT</cf>", par=b"<params><hw_alpha>2.5</hw_alpha><hw_beta>3.5</hw_beta>"
                               b"<dependent_rra_idx>11</dependent_rra_idx></params>")),
        "seasonal-params": doc(r=rra(cf=b"<cf>SEASONAL</cf>", par=b"<params><seasonal_gamma>4.5</seasonal_gamma>"
                                     b"<seasonal_smooth_idx>12</seasonal_smooth_idx><smoothing_window>5.5</smoothing_window>"
                                     b"<dependent_rra_idx>3</dependent_rra_idx></params>")),
        "failures-params": doc(r=rra(cf=b"<cf>FAILURES</cf>", par=b"<params><delta_pos>6.5</delta_pos><delta_neg>7.5</delta_neg>"
                                     b"<window_len>13</window_len><failure_threshold>14</failure_threshold>"
                                     b"<dependent_rra_idx>4</dependent_rra_idx></params>")),
        "hw-cdp": doc(r=rra(prep=b"<cdp_prep><ds><primary_value>2.25</primary_value><secondary_value>3.25</secondary_value>"
                            b"<intercept>4.25</intercept><last_intercept>5.25</last_intercept><slope>6.25</slope>"
                            b"<last_slope>7.25</last_slope><nan_count>22</nan_count><last_nan_count>23</last_nan_count>"
                            b"</ds></cdp_prep>")),
        "seasonal-cdp": doc(r=rra(prep=b"<cdp_prep><ds><seasonal>8.25</seasonal><last_seasonal>9.25</last_seasonal>"
                                  b"<init_flag>24</init_flag><unknown_datapoints>21</unknown_datapoints></ds></cdp_prep>")),
        "history-long": doc(r=rra(prep=b"<cdp_prep><ds><history>1011000000000000000000000000000001</history></ds></cdp_prep>")),
        "history-short": doc(r=rra(prep=b"<cdp_prep><ds><history>1x1</history></ds></cdp_prep>")),
        "history-then-value": doc(r=rra(prep=b"<cdp_prep><ds><history>11</history><value>2.5</value></ds></cdp_prep>")),
        # COMPUTE definitions.
        "cdef-ok": doc(d=ds + b"<ds><name>c</name><type>COMPUTE</type><cdef>a,2,*</cdef></ds>",
                       r=rra(prep=b"<cdp_prep>" + b"<ds><value>1</value></ds>" * 2 + b"</cdp_prep>",
                             db=b"<database><row><v>1</v><v>2</v></row></database>")),
        "cdef-bad": doc(d=ds + b"<ds><name>c</name><type>COMPUTE</type><cdef>a,FOO</cdef></ds>",
                        r=rra(prep=b"<cdp_prep>" + b"<ds><value>1</value></ds>" * 2 + b"</cdp_prep>",
                              db=b"<database><row><v>1</v><v>2</v></row></database>")),
        "cdef-unknown-ds": doc(d=ds + b"<ds><name>c</name><type>COMPUTE</type><cdef>zz,2,*</cdef></ds>",
                               r=rra(prep=b"<cdp_prep>" + b"<ds><value>1</value></ds>" * 2 + b"</cdp_prep>",
                                     db=b"<database><row><v>1</v><v>2</v></row></database>")),
    }
    cases = [Case(f"walk-{name}", xml) for name, xml in probes.items()]
    return cases


# Decimal conversion: values whose result depends on the conversion recipe
# of RS-38, including whether the multiply-add is fused (RS-39).
NUMBER_EDGES = [
    "0e400", "0e-400", "1e-330", "123456789e-320", "4.9e-324", "2.2250738585072011e-308",
    "1e22", "1e23", "9007199254740993", "0.1", "0.3", "1e308", "1.8e308", "-0e5",
    "infnan", "naninf", "-xinf", "1inf", "information", "-information", "Nan1", "nAn",
    "1.0e+0", "1E5", "1e05", "1e+05", "0.000001", "100000000000000000000000",
    "1000000000.000001", "1234567890.1234567", "3.14159265358979323846", "2.5e-1",
]


def number_cases(wide):
    import random
    import re

    slots = len(re.findall(rb"<v>[^<]*</v>", wide))
    cases = []
    for seed in range(4):
        rng = random.Random(seed)
        values = []
        for _ in range(slots):
            kind = rng.randrange(6)
            if kind == 0:
                text = "0." + "".join(rng.choice("0123456789") for _ in range(rng.randrange(15, 30)))
            elif kind == 1:
                text = "".join(rng.choice("123456789") for _ in range(rng.randrange(16, 25)))
            elif kind == 2:
                text = "%de%d" % (rng.randrange(1, 10**8), rng.randrange(-300, 300))
            elif kind == 3:
                text = "1%09d.%09d" % (rng.randrange(10**9), rng.randrange(10**9))
            elif kind == 4:
                text = "%.10e" % rng.uniform(-1e6, 1e6)
            else:
                text = repr(rng.random() * 10 ** rng.randrange(-300, 300))
            values.append(text.encode())
        if seed == 0:
            values[: len(NUMBER_EDGES)] = [edge.encode() for edge in NUMBER_EDGES]
        feed = iter(values)
        xml = re.sub(rb"<v>[^<]*</v>", lambda _, feed=feed: b"<v>" + next(feed) + b"</v>", wide)
        cases.append(Case(f"numbers-{seed}", xml))
    return cases


TINY_HEAD = b"<rrd><version>0003</version><step>60</step><lastupdate>0</lastupdate>"
TINY_DS = b"<ds><name>a</name><type>GAUGE</type></ds>"
TINY_RRA = (b"<rra><cf>AVERAGE</cf><pdp_per_row>1</pdp_per_row><params><xff>0.5</xff></params>"
            b"<cdp_prep><ds><value>1</value></ds></cdp_prep>"
            b"<database><row><v>1</v></row><row><v>2</v></row></database></rra>")


def tiny_cases():
    """Hand-written one-line documents for the walk rules of section 3."""
    def doc(head=TINY_HEAD, ds=TINY_DS, rra=TINY_RRA, tail=b"</rrd>"):
        return head + ds + rra + tail

    cases = []
    add = lambda cid, xml: cases.append(Case(f"tiny-{cid}", xml))
    add("ok", doc())
    add("empty-params", doc(rra=TINY_RRA.replace(b"<xff>0.5</xff>", b"")))
    add("blank-params", doc(rra=TINY_RRA.replace(b"<xff>0.5</xff>", b" \n ")))
    add("comment-params", doc(rra=TINY_RRA.replace(b"<xff>0.5</xff>", b"<!-- c -->")))
    add("self-closing-params", doc(rra=TINY_RRA.replace(b"<params><xff>0.5</xff></params>", b"<params/>")))
    add("empty-cdp-ds", doc(rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"<ds></ds>")))
    add("blank-cdp-ds", doc(rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"<ds> </ds>")))
    add("self-closing-cdp-ds", doc(rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"<ds/>")))
    add("empty-cdp-prep", doc(rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"")))
    add("empty-ds", doc(ds=b"<ds></ds>"))
    add("self-closing-ds", doc(ds=b"<ds/>"))
    add("empty-rra", doc(rra=b"<rra></rra>"))
    add("self-closing-rra", doc(rra=b"<rra/>"))
    add("empty-name", doc(ds=TINY_DS.replace(b"<name>a</name>", b"<name></name>")))
    add("self-closing-name", doc(ds=TINY_DS.replace(b"<name>a</name>", b"<name/>")))
    add("self-closing-step", doc(head=TINY_HEAD.replace(b"<step>60</step>", b"<step/>")))
    add("self-closing-version", doc(head=TINY_HEAD.replace(b"<version>0003</version>", b"<version/>")))
    add("self-closing-root", b"<rrd/>")
    add("self-closing-database", doc(rra=TINY_RRA.replace(
        b"<database><row><v>1</v></row><row><v>2</v></row></database>", b"<database/>")))
    add("no-database", doc(rra=TINY_RRA.replace(
        b"<database><row><v>1</v></row><row><v>2</v></row></database>", b"")))
    add("database-before-cdp", doc(rra=TINY_RRA.replace(
        b"<cdp_prep><ds><value>1</value></ds></cdp_prep>", b"").replace(
        b"</database>", b"</database><cdp_prep><ds><value>1</value></ds></cdp_prep>")))
    add("two-ds", doc(ds=TINY_DS + TINY_DS.replace(b">a<", b">b<"),
                      rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"<ds><value>1</value></ds>" * 2)
                      .replace(b"<v>1</v>", b"<v>1</v><v>3</v>").replace(b"<v>2</v>", b"<v>2</v><v>4</v>")))
    add("same-names", doc(ds=TINY_DS + TINY_DS,
                          rra=TINY_RRA.replace(b"<ds><value>1</value></ds>", b"<ds><value>1</value></ds>" * 2)
                          .replace(b"<v>1</v>", b"<v>1</v><v>3</v>").replace(b"<v>2</v>", b"<v>2</v><v>4</v>")))
    add("ds-after-rra", TINY_HEAD + TINY_RRA + TINY_DS + b"</rrd>")
    add("header-after-ds", b"<rrd>" + TINY_DS + TINY_HEAD[5:] + TINY_RRA + b"</rrd>")
    add("header-last", b"<rrd>" + TINY_DS + TINY_RRA + TINY_HEAD[5:] + b"</rrd>")
    add("two-rra-mixed", doc(rra=TINY_RRA + TINY_RRA.replace(b"AVERAGE", b"LAST").replace(
        b"<row><v>2</v></row>", b"")))
    add("zero-step", doc(head=TINY_HEAD.replace(b"<step>60</step>", b"<step>0</step>")))
    add("zero-pdp", doc(rra=TINY_RRA.replace(b"<pdp_per_row>1<", b"<pdp_per_row>0<")))
    add("many-rows", doc(rra=TINY_RRA.replace(
        b"<row><v>1</v></row><row><v>2</v></row>",
        b"".join(b"<row><v>%d</v></row>" % i for i in range(2000)))))
    # Duplicates with different values: which one is kept.
    add("dup-step", doc(head=TINY_HEAD.replace(b"<step>60</step>", b"<step>60</step><step>120</step>")))
    add("dup-lastupdate", doc(head=TINY_HEAD.replace(
        b"<lastupdate>0</lastupdate>", b"<lastupdate>5</lastupdate><lastupdate>7</lastupdate>")))
    add("dup-version", doc(head=TINY_HEAD.replace(
        b"<version>0003</version>", b"<version>0003</version><version>0005</version>")))
    add("dup-version-low", doc(head=TINY_HEAD.replace(
        b"<version>0003</version>", b"<version>0003</version><version>0001</version>")))
    add("dup-name", doc(ds=TINY_DS.replace(b"<name>a</name>", b"<name>a</name><name>b</name>")))
    add("dup-type", doc(ds=TINY_DS.replace(b"<type>GAUGE</type>", b"<type>GAUGE</type><type>COUNTER</type>")))
    add("dup-cf", doc(rra=TINY_RRA.replace(b"<cf>AVERAGE</cf>", b"<cf>AVERAGE</cf><cf>MAX</cf>")))
    add("dup-xff", doc(rra=TINY_RRA.replace(b"<xff>0.5</xff>", b"<xff>0.5</xff><xff>0.25</xff>")))
    add("dup-params", doc(rra=TINY_RRA.replace(
        b"<params><xff>0.5</xff></params>", b"<params><xff>0.5</xff></params><params><xff>0.25</xff></params>")))
    add("dup-cdp-value", doc(rra=TINY_RRA.replace(b"<value>1</value>", b"<value>1</value><value>9</value>")))
    add("dup-pdp", doc(rra=TINY_RRA.replace(b"<pdp_per_row>1</pdp_per_row>",
                                            b"<pdp_per_row>1</pdp_per_row><pdp_per_row>4</pdp_per_row>")))
    # Values of the pre-version-3 layout.
    add("v1-flat-xff", doc(head=TINY_HEAD.replace(b"0003", b"0001"),
                           rra=TINY_RRA.replace(b"<params><xff>0.5</xff></params>", b"<xff>0.5</xff>")))
    add("v1-params", doc(head=TINY_HEAD.replace(b"0003", b"0001")))
    add("v1-no-xff", doc(head=TINY_HEAD.replace(b"0003", b"0001"),
                         rra=TINY_RRA.replace(b"<params><xff>0.5</xff></params>", b"")))
    add("v3-no-params", doc(rra=TINY_RRA.replace(b"<params><xff>0.5</xff></params>", b"")))
    add("v1-xff-after-cdp", doc(head=TINY_HEAD.replace(b"0003", b"0001"),
                                rra=TINY_RRA.replace(b"<params><xff>0.5</xff></params>", b"")
                                .replace(b"</cdp_prep>", b"</cdp_prep><xff>0.5</xff>")))
    return cases


def all_cases(seeds):
    cases = seed_cases(seeds) + mini_cases(seeds["mini"])
    # Every input once from a named file and once from standard input,
    # except cases that exercise options or targets.
    out = []
    for case in cases:
        out.append(case)
        if case.opts or case.target != "out.rrd" or case.pre != "none":
            continue
        out.append(Case(case.id + "@stdin", case.xml, mode="stdin", expect=case.expect,
                        input_name=case.input))
    seen = set()
    for case in out:
        assert case.id not in seen, case.id
        seen.add(case.id)
    return out


# --------------------------------------------------------------------------
# Running.

QUIET_NAN = struct.pack("<Q", 0x7FF8000000000000)
NEGATIVE_QUIET_NAN = struct.pack("<Q", 0xFFF8000000000000)


def fold_nan(data):
    """x86_64 builds write unknowns as the negative quiet NaN (RS-52)."""
    words = [data[i:i + 8] for i in range(0, len(data) - len(data) % 8, 8)]
    folded = b"".join(QUIET_NAN if word == NEGATIVE_QUIET_NAN else word for word in words)
    return folded + data[len(folded):]


def normal_form(data):
    """Rows rotated oldest first, row pointers zeroed and NaN signs folded.

    See RS-60 and RS-52. Anything that does not parse as an RRD is
    returned unchanged.
    """
    if data is None or len(data) < 128 or data[:4] != b"RRD\0":
        return data
    try:
        ds, rra = struct.unpack_from("<QQ", data, 24)
        pointers = 128 + 120 * ds + 120 * rra + 16 + 112 * ds + 80 * ds * rra
        if pointers + 8 * rra > len(data):
            return data
        out = bytearray(data[:pointers]) + bytes(8 * rra)
        offset = pointers + 8 * rra
        for index in range(rra):
            rows = struct.unpack_from("<Q", data, 128 + 120 * ds + 120 * index + 24)[0]
            length = rows * ds * 8
            archive = data[offset:offset + length]
            current = struct.unpack_from("<Q", data, pointers + 8 * index)[0]
            if len(archive) != length:
                return data
            if rows:
                if current >= rows:
                    return data
                split = ((current + 1) % rows) * ds * 8
                out += archive[split:] + archive[:split]
            offset += length
        out += data[offset:]
        return fold_nan(bytes(out))
    except struct.error:
        return data


def run_case(program, case, work, pad):
    directory = tempfile.mkdtemp(dir=work)
    with open(os.path.join(directory, "in.xml"), "wb") as handle:
        handle.write(case.xml)
    pre = PRE_CONTENT[case.pre]
    if pre is not None:
        with open(os.path.join(directory, case.target), "wb") as handle:
            handle.write(pre)
    source = "in.xml" if case.mode == "file" else "-"
    env = dict(ENV, RONDI_VECTOR_PAD="x" * pad)
    try:
        proc = subprocess.run(
            [program, "restore", *case.opts, source, case.target], cwd=directory, env=env,
            input=case.xml if case.mode == "stdin" else b"", capture_output=True,
            timeout=TIMEOUT,
        )
        status = str(proc.returncode) if proc.returncode >= 0 else "signal"
        stdout, stderr = proc.stdout, proc.stderr
    except subprocess.TimeoutExpired:
        status, stdout, stderr = "hang", b"", b""
    if case.target == "-":
        produced, stdout = (stdout or None), b""
    else:
        path = os.path.join(directory, case.target)
        produced = None
        if os.path.isfile(path):
            with open(path, "rb") as handle:
                produced = handle.read()
    shutil.rmtree(directory, ignore_errors=True)
    if status in ("hang", "signal"):
        stdout = stderr = b""
    return {"status": status, "stdout": stdout, "stderr": stderr, "file": normal_form(produced)}


def digest(data):
    return "none" if data is None else hashlib.sha256(data).hexdigest()[:20]


# Cases where the reference corrupts memory (RS-64): three runs can agree
# on one machine and still differ on the next, so they are never pinned.
UNSTABLE = {"dup-database"}


def record(program, cases, runs=2):
    results = {}
    with tempfile.TemporaryDirectory() as work:
        for case in cases:
            seen = [run_case(program, case, work, pad) for pad in range(runs)]
            first = seen[0]
            flags = []
            if any(other != first for other in seen[1:]) or case.id.split("@")[0] in UNSTABLE:
                flags.append("nondeterministic")
            if first["status"] in ("hang", "signal"):
                flags.append(first["status"])
            results[case.id] = dict(first, flags=flags)
    return results


def frame(case_id, section, body):
    return f"@@ {case_id} {section} {len(body)}\n".encode() + body + b"\n"


def write_vectors(path, cases, results, outputs):
    with open(path, "wb") as handle:
        for case in cases:
            if case.id not in results:
                continue
            result = results[case.id]
            handle.write(frame(case.id, "status", result["status"].encode()))
            handle.write(frame(case.id, "stdout", result["stdout"]))
            handle.write(frame(case.id, "stderr", result["stderr"]))
            if "nondeterministic" in result["flags"]:
                handle.write(frame(case.id, "file", b"undefined"))
            else:
                handle.write(frame(case.id, "file", digest(result["file"]).encode()))
                if result["file"] is not None:
                    outputs[digest(result["file"])] = result["file"]
            handle.write(frame(case.id, "flags", " ".join(result["flags"]).encode()))


def parse_vectors(path):
    with open(path, "rb") as handle:
        data = handle.read()
    out = {}
    while data:
        newline = data.index(b"\n")
        _, case_id, section, length = data[:newline].decode().split(" ")
        length = int(length)
        out.setdefault(case_id, {})[section] = data[newline + 1:newline + 1 + length]
        data = data[newline + 1 + length + 1:]
    return out


def load_committed_cases():
    cases = []
    with open(os.path.join(DATA, "cases.tsv")) as handle:
        for line in handle:
            if not line.strip() or line.startswith("#"):
                continue
            fields = line.rstrip("\n").split("\t")
            case_id, input_name, mode, target, pre, expect = fields[:6]
            with open(os.path.join(DATA, "inputs", input_name + ".xml"), "rb") as handle:
                xml = handle.read()
            cases.append(Case(case_id, xml, mode=mode, opts=fields[6:], target=target, pre=pre,
                              expect=expect, input_name=input_name))
    return cases


def write_outputs(outputs):
    directory = os.path.join(DATA, "expected", "outputs")
    os.makedirs(directory, exist_ok=True)
    for name, data in outputs.items():
        path = os.path.join(directory, name + ".rrd")
        if not os.path.exists(path):
            with open(path, "wb") as handle:
                handle.write(data)


# --------------------------------------------------------------------------
# Expectations for Rondi that differ from "same". Kept here so that a
# rebuild of the table keeps them; docs/compatibility/restore.md lists why.

EXPECT = {
    # RS-70: COMPUTE data sources.
    "seed-compute": "unsupported",
    "type-compute": "unsupported",
    "walk-cdef-ok": "unsupported",
    "walk-cdef-bad": "unsupported",
    "walk-cdef-unknown-ds": "unsupported",
    # RS-82 to RS-87: documented divergences.
    "doc-utf16le-bom": "diverge",
    "doc-utf16be-bom": "diverge",
    "doc-latin1-name": "diverge",
    "doc-bad-utf8": "diverge",
    "value-internal-entity": "diverge",
    "dup-database": "diverge",
    "opt-force-longer-file": "truncates",
    "target-is-dir-force": "text-differs",
}


def apply_expectations(cases, results):
    """Assign each case its Rondi expectation.

    Inputs that libxml2 reports as not well formed (a "parser error"
    diagnostic for the named-file run) get "xml-syntax": their message text
    is library behavior (RS-40). Cases the reference crashes or hangs on
    get "diverge" unless listed above.
    """
    malformed = set()
    for case in cases:
        if case.mode == "file" and b": parser error :" in results[case.id]["stderr"]:
            malformed.add(case.input)
    for case in cases:
        base = case.id.split("@")[0]
        flags = results[case.id]["flags"]
        if base in EXPECT:
            case.expect = EXPECT[base]
        elif "hang" in flags or "signal" in flags:
            case.expect = "diverge"
        elif case.input in malformed and case.expect == "same":
            case.expect = "xml-syntax"


def committed_seeds():
    seeds = {}
    for name in SEEDS:
        for variant in [name] + [f"{name}-hdr-{header}" for header in HEADER_VARIANTS.get(name, [])]:
            with open(os.path.join(DATA, "inputs", f"seed-{variant}.xml"), "rb") as handle:
                seeds[variant] = handle.read()
    return seeds


def check_table():
    """Rebuild the case table from the committed seeds and compare it with
    cases.tsv and inputs/ without running any reference. Returns the number
    of differences."""
    built = all_cases(committed_seeds())
    committed = load_committed_cases()
    problems = []
    if [case.id for case in built] != [case.id for case in committed]:
        problems.append("case ids or order differ")
        built = []
    for new, old in zip(built, committed[: len(built)], strict=True):
        if (new.input, new.mode, new.opts, new.target, new.pre, new.xml) != (
                old.input, old.mode, old.opts, old.target, old.pre, old.xml):
            problems.append(f"{new.id}: definition or input differs")
    for problem in problems:
        print(problem)
    print(f"{len(built)} cases rebuilt, {len(problems)} differences")
    return len(problems)


def command_write(args):
    if args.check:
        if check_table():
            sys.exit(1)
        return
    with tempfile.TemporaryDirectory() as work:
        seeds = make_seeds(args.rrdtool, work)
    cases = all_cases(seeds)
    results = record(args.rrdtool, cases, runs=3)
    apply_expectations(cases, results)
    for sub_dir in ("inputs", "expected"):
        shutil.rmtree(os.path.join(DATA, sub_dir), ignore_errors=True)
    os.makedirs(os.path.join(DATA, "inputs"))
    os.makedirs(os.path.join(DATA, "expected"))
    written = set()
    for case in cases:
        if case.input in written:
            continue
        written.add(case.input)
        with open(os.path.join(DATA, "inputs", case.input + ".xml"), "wb") as handle:
            handle.write(case.xml)
    with open(os.path.join(DATA, "cases.tsv"), "w") as handle:
        handle.write("# id\tinput\tmode\ttarget\tpre\texpect\toptions...\n")
        for case in cases:
            handle.write("\t".join([case.id, case.input, case.mode, case.target, case.pre,
                                    case.expect, *case.opts]) + "\n")
    outputs = {}
    write_vectors(os.path.join(DATA, "expected", "base.vectors"), cases, results, outputs)
    write_outputs(outputs)
    print(f"{len(cases)} cases, {len(written)} inputs, {len(outputs)} distinct outputs")
    undefined = [c.id for c in cases if results[c.id]["flags"]]
    print("flagged:", " ".join(undefined) or "none")


def command_overrides(args):
    cases = load_committed_cases()
    base = parse_vectors(os.path.join(DATA, "expected", "base.vectors"))
    results = record(args.rrdtool, cases, runs=3)
    differing = []
    for case in cases:
        result = results[case.id]
        recorded = base[case.id]
        file_digest = b"undefined" if "nondeterministic" in result["flags"] else digest(result["file"]).encode()
        if (recorded["status"] != result["status"].encode() or recorded["stdout"] != result["stdout"]
                or recorded["stderr"] != result["stderr"] or recorded["file"] != file_digest
                or recorded["flags"] != " ".join(result["flags"]).encode()):
            differing.append(case)
    outputs = {}
    write_vectors(os.path.join(DATA, "expected", f"{args.platform}.vectors"), differing,
                  results, outputs)
    write_outputs(outputs)
    print(f"{len(differing)} of {len(cases)} cases differ on {args.platform}")


def command_compare(args):
    cases = load_committed_cases()
    vectors = parse_vectors(os.path.join(DATA, "expected", "base.vectors"))
    if args.platform:
        vectors.update(parse_vectors(os.path.join(DATA, "expected", f"{args.platform}.vectors")))
    results = record(args.program, cases, runs=1)
    for case in cases:
        result = results[case.id]
        recorded = vectors[case.id]
        diffs = []
        if recorded["status"] != result["status"].encode():
            diffs.append(f"status {result['status']} want {recorded['status'].decode()}")
        if recorded["stdout"] != result["stdout"]:
            diffs.append("stdout")
        if recorded["stderr"] != result["stderr"]:
            diffs.append(f"stderr {result['stderr'][-160:]!r} want {recorded['stderr'][-160:]!r}")
        if recorded["file"] not in (b"undefined", digest(result["file"]).encode()):
            diffs.append(f"file {digest(result['file'])} want {recorded['file'].decode()}")
        if diffs:
            print(case.id, case.expect, "; ".join(diffs))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    write = commands.add_parser("write")
    write.add_argument("--rrdtool", default="rrdtool")
    write.add_argument("--check", action="store_true",
                       help="rebuild the table from the committed seeds and compare; run nothing")
    overrides = commands.add_parser("overrides")
    overrides.add_argument("--platform", required=True, choices=["linux-x86_64", "linux-aarch64"])
    overrides.add_argument("--rrdtool", default="rrdtool")
    compare = commands.add_parser("compare")
    compare.add_argument("--program", required=True)
    compare.add_argument("--platform")
    args = parser.parse_args()
    {"write": command_write, "overrides": command_overrides, "compare": command_compare}[args.command](args)


if __name__ == "__main__":
    main()
