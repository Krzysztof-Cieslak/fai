"""JUnit renderer fixtures, including failure-safe malformed reports."""

from contextlib import redirect_stdout
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("junit_summary", Path(__file__).resolve().parents[1] / "junit-summary.py")
summary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(summary)


class JunitSummaryTests(unittest.TestCase):
    def rendered(self, text):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "report.xml"
            if text is not None:
                path.write_text(text)
            output = io.StringIO()
            with patch.object(sys, "argv", ["junit-summary.py", str(path)]), \
                    patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), redirect_stdout(output):
                self.assertEqual(summary.main(), 0)
            return output.getvalue()

    def test_passing_report_counts_cases_and_times(self):
        output = self.rendered('<testsuites><testsuite name="unit"><testcase name="a" time="0.25"/><testcase name="b" time="0.75"/></testsuite></testsuites>')
        self.assertIn("**2 passed**, **0 failed**", output)
        self.assertIn("| unit | 2 | 2 | 0 | 0 | 1.00 |", output)

    def test_failures_and_errors_are_reported(self):
        output = self.rendered('<testsuite name="unit"><testcase name="a"><failure/></testcase><testcase name="b"><error/></testcase></testsuite>')
        self.assertIn("**2 failed**", output)
        self.assertIn("`unit :: a`", output)
        self.assertIn("`unit :: b`", output)

    def test_skipped_cases_are_not_counted_as_passed(self):
        output = self.rendered('<testsuite name="unit"><testcase name="a"><skipped/></testcase></testsuite>')
        self.assertIn("**0 passed**, **0 failed**, 1 skipped", output)

    def test_missing_report_returns_a_note(self):
        self.assertIn("No JUnit report was produced", self.rendered(None))

    def test_malformed_xml_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered("<testsuite"))

    def test_invalid_duration_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered('<testsuite><testcase time="invalid"/></testsuite>'))

    def test_nonfinite_duration_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered('<testsuite><testcase time="NaN"/></testsuite>'))

    def test_negative_duration_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered('<testsuite><testcase time="-1"/></testsuite>'))

    def test_unrecognized_root_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered("<unrelated/>"))

    def test_summary_file_is_appended_without_overwriting(self):
        with tempfile.TemporaryDirectory() as temporary:
            report = Path(temporary) / "report.xml"
            report.write_text('<testsuite name="unit"><testcase/></testsuite>')
            output = Path(temporary) / "summary.md"
            output.write_text("existing summary\n")
            with patch.object(sys, "argv", ["junit-summary.py", str(report)]), \
                    patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(output)}):
                summary.main()
            self.assertTrue(output.read_text().startswith("existing summary\n"))
            self.assertIn("**1 passed**", output.read_text())
