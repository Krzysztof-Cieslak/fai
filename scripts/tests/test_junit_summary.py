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

    def test_suite_duration_is_used_when_cases_have_no_timings(self):
        output = self.rendered('<testsuite name="fai test" time="1.25"><testcase name="example"/></testsuite>')
        self.assertIn("| fai test | 1 | 1 | 0 | 0 | 1.25 |", output)

    def test_case_duration_takes_precedence_over_suite_duration(self):
        output = self.rendered('<testsuite name="unit" time="10"><testcase name="a" time="1"/></testsuite>')
        self.assertIn("| unit | 1 | 1 | 0 | 0 | 1.00 |", output)

    def test_invalid_suite_duration_returns_a_note(self):
        self.assertIn("Could not parse", self.rendered('<testsuite time="NaN"><testcase/></testsuite>'))

    def test_directory_reports_are_named_and_aggregated(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "alpha.xml").write_text('<testsuites><testsuite name="fai test" time="0.5"><testcase name="ok"/></testsuite></testsuites>')
            (directory / "beta.xml").write_text('<testsuite name="fai test"><testcase name="bad"><error/></testcase></testsuite>')
            output = io.StringIO()
            with patch.object(sys, "argv", ["junit-summary.py", str(directory)]), \
                    patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), redirect_stdout(output):
                self.assertEqual(summary.main(), 0)
            text = output.getvalue()
            self.assertIn("**1 passed**, **1 failed**", text)
            self.assertIn("alpha: fai test", text)
            self.assertIn("beta: fai test", text)

    def test_empty_report_directory_returns_a_note(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = io.StringIO()
            with patch.object(sys, "argv", ["junit-summary.py", temporary]), \
                    patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), redirect_stdout(output):
                self.assertEqual(summary.main(), 0)
            self.assertIn("No JUnit report was produced", output.getvalue())

    def test_package_ci_requests_reports_outside_restored_caches(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml").read_text()
        commands = [line.strip() for line in workflow.splitlines() if line.strip().startswith("fai test --locked")]
        self.assertEqual(len(commands), 3)
        self.assertTrue(all("--junit" in command and "RUNNER_TEMP/fai-junit/" in command for command in commands))

    def test_package_ci_publishes_reports_on_failure(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/ci.yml").read_text()
        self.assertEqual(workflow.count("- name: Fai test summary\n        if: always()"), 2)
        blocks = workflow.split("- uses: actions/upload-artifact@v4")
        reports = [block for block in blocks if "name: junit-report-fai-" in block.split("- name:", 1)[0]]
        self.assertEqual(len(reports), 2)
        self.assertTrue(all("if: always()" in block and "${{ runner.temp }}/fai-junit/*.xml" in block for block in reports))
