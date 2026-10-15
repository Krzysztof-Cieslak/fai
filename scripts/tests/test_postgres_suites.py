"""Provisioning boundaries and inputs for the named PostgreSQL suite runner."""

import contextlib
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

import postgres_suites


class PostgresSuiteTests(unittest.TestCase):
    def test_requires_an_explicit_database(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(sys, "argv", ["postgres_suites.py"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as failure:
                postgres_suites.main()
        self.assertEqual(failure.exception.code, 2)

    def run_suite(self, tls):
        with tempfile.TemporaryDirectory() as directory:
            environment = {"FAI_PG_URL": "postgres://fixture/database", "RUNNER_TEMP": directory}
            if tls:
                environment["FAI_PG_ROOT"] = "fixture-root.pem"
            commands = []

            def execute(command, **options):
                commands.append(command)
                self.assertEqual(command[:4], ["fixture-fai", "test", "--no-daemon", "--deny-focused"])
                project = Path(command[command.index("-C") + 1])
                self.assertTrue((project / "fai.toml").is_file())
                self.assertTrue((project / "tests/Basic.fai").is_file())
                source = (project / "tests/Integration.fai").read_text(encoding="utf-8")
                self.assertIn("\ntests : Test.Suite", source)
                self.assertNotIn("\npublic tests :", source)
                wire = (project / "tests/WireChecks.fai").read_text(encoding="utf-8")
                self.assertIn("PostgreSQL wire protocol", wire)
                self.assertEqual(options, {"check": True, "timeout": 300})

            with mock.patch.dict(os.environ, environment, clear=True), mock.patch.object(sys, "argv", ["postgres_suites.py", "--fai", "fixture-fai"]), mock.patch.object(postgres_suites.subprocess, "run", side_effect=execute):
                postgres_suites.main()
            self.assertEqual(len(commands), 1)
            project = Path(commands[0][commands[0].index("-C") + 1])
            self.assertFalse(project.exists())
            return Path(commands[0][-1]).name

    def test_plain_suites_have_their_own_report(self):
        self.assertEqual(self.run_suite(False), "postgres-plain.xml")

    def test_tls_suites_do_not_overwrite_the_plain_report(self):
        self.assertEqual(self.run_suite(True), "postgres-tls.xml")


if __name__ == "__main__":
    unittest.main()
