#!/usr/bin/env python3
"""Render a nextest JUnit report as a Markdown test summary.

Writes to the file named by ``$GITHUB_STEP_SUMMARY`` (so the result shows on the
GitHub Actions run page) or, when that variable is unset, to stdout (handy for
local inspection). Never fails the build: a missing or malformed report degrades
to a short note.

Usage: junit-summary.py [path-to-junit.xml]
"""

from __future__ import annotations

import os
import sys
import xml.etree.ElementTree as ET


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "target/nextest/ci/junit.xml"
    lines: list[str] = []

    if not os.path.exists(path):
        emit(["## Test results", "", "No JUnit report was produced (tests may not have run)."])
        return 0

    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as err:
        emit(["## Test results", "", f"Could not parse the JUnit report: `{err}`."])
        return 0

    total = failed = skipped = 0
    rows: list[tuple[str, int, int, int, int, float]] = []
    failures: list[str] = []

    # Aggregate from the testcases: nextest records timing and skip/failure state
    # there, not on the <testsuite> element (which only carries counts).
    for suite in root.iter("testsuite"):
        name = suite.get("name", "?")
        tests = fails = skips = 0
        seconds = 0.0
        for case in suite.iter("testcase"):
            tests += 1
            seconds += float(case.get("time", "0") or "0")
            if case.find("failure") is not None or case.find("error") is not None:
                fails += 1
                failures.append(f'{name} :: {case.get("name", "?")}')
            elif case.find("skipped") is not None:
                skips += 1
        total += tests
        failed += fails
        skipped += skips
        rows.append((name, tests, tests - fails - skips, fails, skips, seconds))

    passed = total - failed - skipped
    status = "all passed" if failed == 0 else f"{failed} FAILED"
    lines.append(f"## Test results — {status}")
    lines.append("")
    lines.append(f"**{passed} passed**, **{failed} failed**, {skipped} skipped of {total} tests")
    lines.append("")
    lines.append("| Suite | Tests | Passed | Failed | Skipped | Time (s) |")
    lines.append("| --- | --: | --: | --: | --: | --: |")
    for name, tests, ok, fails, skips, seconds in sorted(rows):
        lines.append(f"| {name} | {tests} | {ok} | {fails} | {skips} | {seconds:.2f} |")

    if failures:
        lines.append("")
        lines.append("### Failed tests")
        lines.append("")
        lines.extend(f"- `{name}`" for name in failures)

    emit(lines)
    return 0


def emit(lines: list[str]) -> None:
    text = "\n".join(lines) + "\n"
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(text)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    raise SystemExit(main())
