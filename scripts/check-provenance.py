#!/usr/bin/env python3
"""Check Rust comments against the provenance rules in AGENTS.md.

Usage:
  scripts/check-provenance.py [PATH ...]

With no arguments, every tracked .rs file under crates/ and fuzz/ is checked.
Only comment text (lines starting with //, /// or //!) is examined; string
literals are left alone because some carry RRDtool's own output verbatim.

Rules:
  c-file      A comment names a C source or header file (foo.c, foo.h).
  c-line      A comment cites a C line number (foo.c:123, "line 123 of").
  provenance  A "Provenance:" comment does not name an accepted source:
              black-box ..., reference-source-inspection ..., specification
              ..., or a manual page such as rrdcached(1).
  see-spec    A reference-source-inspection comment does not point at a
              document under docs/ within the same comment block.

Files listed in LEGACY are pre-policy modules that docs/provenance-audit.md
already flags for replacement. The c-file and c-line rules are reported for
them as warnings, not errors. A LEGACY entry that no longer has any finding,
or no longer exists, is an error so the list only shrinks.
"""

from __future__ import annotations

import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Pre-policy files flagged High or Review in docs/provenance-audit.md that
# still name C files or lines. Remove an entry when its module is replaced.
LEGACY: set[str] = set()

C_FILE = re.compile(r"\b[A-Za-z0-9_]+\.[ch]\b(?![A-Za-z0-9_])")
C_LINE = re.compile(
    r"\.[ch]:\d+|\blines? \d+(?:\s*(?:-|to)\s*\d+)? (?:of|in)\b", re.IGNORECASE
)
PROVENANCE = re.compile(r"\bProvenance:\s*(.*)")
ACCEPTED_SOURCE = re.compile(
    r"(black-box|reference-source-inspection|specification\b|[a-z][a-z0-9_.-]*\(\d\))"
)
COMMENT = re.compile(r"^\s*//[/!]?\s?(.*)$")
# How many comment lines after "Provenance:" may still carry its "see docs/...".
SEE_WINDOW = 3


@dataclass(frozen=True)
class Finding:
    path: str
    line: int
    rule: str
    text: str

    def __str__(self) -> str:
        return f"{self.path}:{self.line}: {self.rule}: {self.text.strip()}"


def comment_lines(source: str) -> list[tuple[int, str | None]]:
    """Return (line number, comment text or None) for every line."""
    result = []
    for number, line in enumerate(source.splitlines(), start=1):
        match = COMMENT.match(line)
        result.append((number, match.group(1) if match else None))
    return result


def check_source(path: str, source: str) -> list[Finding]:
    findings = []
    lines = comment_lines(source)
    for index, (number, text) in enumerate(lines):
        if text is None:
            continue
        if C_FILE.search(text):
            findings.append(Finding(path, number, "c-file", text))
        if C_LINE.search(text):
            findings.append(Finding(path, number, "c-line", text))
        match = PROVENANCE.search(text)
        if not match:
            continue
        source_text = match.group(1)
        # "Provenance:" may end a wrapped line, with the source on the next.
        if not source_text.strip() and index + 1 < len(lines):
            source_text = lines[index + 1][1] or ""
        if not ACCEPTED_SOURCE.match(source_text):
            findings.append(Finding(path, number, "provenance", text))
            continue
        if "reference-source-inspection" in source_text:
            block = [source_text]
            for _, following in lines[index + 1 : index + 1 + SEE_WINDOW]:
                if following is None:
                    break
                block.append(following)
            if "docs/" not in " ".join(block):
                findings.append(Finding(path, number, "see-spec", text))
    return findings


def tracked_rust_files(root: Path) -> list[str]:
    output = subprocess.run(
        ["git", "ls-files", "--", "crates/*.rs", "fuzz/*.rs"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return sorted(line for line in output.splitlines() if line)


def main(argv: list[str]) -> int:
    root = Path(__file__).resolve().parent.parent
    paths = argv or tracked_rust_files(root)
    errors: list[str] = []
    warnings: list[str] = []
    seen_legacy = set()
    for path in paths:
        source = (root / path).read_text(encoding="utf-8", errors="replace")
        for finding in check_source(path, source):
            legacy = path in LEGACY and finding.rule in {"c-file", "c-line"}
            if legacy:
                seen_legacy.add(path)
                warnings.append(
                    f"warning: {finding} (legacy, see docs/provenance-audit.md)"
                )
            else:
                errors.append(f"error: {finding}")
    if not argv:
        for path in sorted(LEGACY - seen_legacy):
            errors.append(
                f"error: {path}: stale LEGACY entry (no findings or file removed); "
                "remove it from scripts/check-provenance.py"
            )
    for message in warnings + errors:
        print(message)
    if errors:
        print(f"{len(errors)} provenance error(s); see AGENTS.md, section Provenance.")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
