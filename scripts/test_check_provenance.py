"""Tests for scripts/check-provenance.py. Run: python3 -m unittest discover scripts"""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "check_provenance", Path(__file__).with_name("check-provenance.py")
)
check = importlib.util.module_from_spec(_SPEC)
# dataclasses resolves the defining module through sys.modules.
sys.modules[_SPEC.name] = check
_SPEC.loader.exec_module(check)


def rules(source: str) -> list[str]:
    return [finding.rule for finding in check.check_source("x.rs", source)]


class CheckSourceTest(unittest.TestCase):
    def test_accepted_forms_pass(self):
        source = """
// Compatibility: RRDtool treats an unknown DS as an update error.
// Provenance: black-box compatibility test.
/// Provenance: reference-source-inspection; see docs/compatibility/rpn.md.
//! Provenance: specification (RS-72 allows a bound).
// Provenance: rrdcached(1) and black-box compatibility test.
"""
        self.assertEqual(rules(source), [])

    def test_wrapped_source_and_see_are_read(self):
        source = """
    // Compatibility: a CREATE option may appear in any order. Provenance:
    // reference-source-inspection; see "Options" in
    // docs/compatibility/rrdcached.md.
"""
        self.assertEqual(rules(source), [])

    def test_unknown_source_is_rejected(self):
        self.assertEqual(
            rules("// Provenance: ported from upstream.\n"), ["provenance"]
        )

    def test_source_inspection_needs_a_spec_reference(self):
        source = "// Provenance: reference-source-inspection.\nfn f() {}\n"
        self.assertEqual(rules(source), ["see-spec"])

    def test_c_file_and_line_in_comments(self):
        self.assertEqual(rules("// matches rrd_update.c\n"), ["c-file"])
        self.assertEqual(rules("// see rrd_update.c:812\n"), ["c-file", "c-line"])
        self.assertEqual(rules("// as on line 812 of the update code\n"), ["c-line"])

    def test_string_literals_and_rust_paths_are_ignored(self):
        source = 'let help = "see syslog.h";\n// uses std::f64::consts and file.rs\n'
        self.assertEqual(rules(source), [])


if __name__ == "__main__":
    unittest.main()
