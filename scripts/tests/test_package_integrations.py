"""Package integration execution, selection and failure propagation."""

from dataclasses import replace
import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import package_integrations as integrations
import packages


class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.directory = self.root / "packages" / "example"
        (self.directory / "examples").mkdir(parents=True)
        self.entry = self.directory / "examples" / "Run.fai"
        self.entry.write_bytes(b"module Run\n")
        self.spec = packages.Integration("runtime", "examples/Run.fai", stdout_lines=("passed",))
        self.package = packages.Package("example", self.directory, (), (self.spec,))
        self.compiler = Path(sys.executable).resolve()
        self.output = subprocess.CompletedProcess([], 0, b"passed\n", b"")
        self.stdout = contextlib.redirect_stdout(io.StringIO())
        self.stdout.__enter__()
        self.addCleanup(self.stdout.__exit__, None, None, None)

    def run_package(self, package=None, **kwargs):
        package = package or self.package
        return integrations.run({package.name: package}, [package.name], self.compiler,
                                root=self.root, host="linux", temporary_parent=self.root, **kwargs)

    def test_jit_and_aot_use_the_same_argv_environment_and_workspace(self):
        spec = replace(self.spec, args=("--value", "é two words;$(false)"), env=(("ANSWER", "42"),), timeout_seconds=31)
        package = replace(self.package, integrations=(spec,))
        with patch.object(integrations, "command", return_value=self.output) as command:
            self.assertEqual(self.run_package(package, environment={"BASE": "kept"}), 1)
        jit, build, native = command.call_args_list
        self.assertEqual(jit.args[0], [self.compiler, "run", "--no-daemon", "-C", self.directory.parent.resolve(),
                                      self.entry.resolve(), "--", "--value", "é two words;$(false)"])
        self.assertEqual(build.args[0][1:4], ["build", "--no-daemon", "-C"])
        self.assertEqual(native.args[0][1:], ["--value", "é two words;$(false)"])
        self.assertEqual(jit.args[1:4], (self.directory.parent.resolve(), {"BASE": "kept", "ANSWER": "42"}, 31))
        self.assertEqual(native.args[1:4], jit.args[1:4])
        self.assertEqual(native.args[0][0], build.args[0][-1])

    def test_unselected_platform_needs_no_compiler_or_fixture(self):
        package = replace(self.package, integrations=(replace(self.spec, platforms=("windows",)),))
        with patch.object(integrations.shutil, "which", side_effect=AssertionError("unexpected compiler lookup")):
            self.assertEqual(self.run_package(package), 0)

    def test_nested_manifest_selects_compiler_scope_without_changing_working_directory(self):
        (self.entry.parent / packages.MANIFEST).write_text('manifest-version = 1\n[package]\nname = "example-programs"\nversion = "0.1.0"\n')
        with patch.object(integrations, "command", return_value=self.output) as command:
            self.run_package()
        jit, build, native = command.call_args_list
        self.assertEqual(jit.args[0][4], self.entry.parent.resolve())
        self.assertEqual(build.args[0][4], self.entry.parent.resolve())
        self.assertEqual(jit.args[1], self.directory.parent.resolve())
        self.assertEqual(native.args[1], self.directory.parent.resolve())

    def test_all_entries_are_validated_before_execution(self):
        package = replace(self.package, integrations=(self.spec, replace(self.spec, name="missing", entry="Missing.fai")))
        with patch.object(integrations, "command", side_effect=AssertionError("unexpected execution")):
            with self.assertRaisesRegex(integrations.IntegrationError, "missing or external entry"):
                self.run_package(package)

    def test_entry_symlink_cannot_escape_its_package(self):
        external = self.root / "Outside.fai"
        external.write_text("module Outside\n")
        self.entry.unlink()
        try:
            self.entry.symlink_to(external)
        except OSError as error:
            self.skipTest(f"symlinks unavailable: {error}")
        with self.assertRaisesRegex(integrations.IntegrationError, "external entry"):
            integrations.plan({"example": self.package}, ["example"], "linux")

    def test_failure_marker_stops_before_aot(self):
        with patch.object(integrations, "command", return_value=subprocess.CompletedProcess([], 0, b"FAILED\n", b"")) as command:
            with self.assertRaisesRegex(integrations.IntegrationError, "missing stdout lines"):
                self.run_package()
        self.assertEqual(command.call_count, 1)
        self.assertEqual(list(self.root.glob("fai-package-integrations-*")), [])

    def test_exact_output_detects_extra_output(self):
        spec = replace(self.spec, stdout="passed\n", stdout_lines=())
        with self.assertRaisesRegex(integrations.IntegrationError, "did not match"):
            integrations.verify_output(spec, subprocess.CompletedProcess([], 0, b"passed\nextra\n", b""), "example/runtime")

    def test_line_output_accepts_crlf(self):
        integrations.verify_output(self.spec, subprocess.CompletedProcess([], 0, b"detail\r\npassed\r\n", b""), "example/runtime")

    def test_line_output_requires_a_complete_line(self):
        with self.assertRaisesRegex(integrations.IntegrationError, "missing stdout lines"):
            integrations.verify_output(self.spec, subprocess.CompletedProcess([], 0, b"not passed\n", b""), "example/runtime")

    def test_nonzero_exit_retains_label_and_status(self):
        with patch.object(integrations.subprocess, "run", return_value=subprocess.CompletedProcess([], 7, b"detail\n", b"")):
            with self.assertRaisesRegex(integrations.IntegrationError, r"example/runtime \[jit\]: exited with status 7"):
                integrations.command(["fai", "run"], self.root, {}, 10, "example/runtime [jit]")

    def test_timeout_is_a_failure_with_partial_output(self):
        timeout = subprocess.TimeoutExpired(["fai"], 10, output=b"partial\n", stderr=b"stuck\n")
        with patch.object(integrations.subprocess, "run", side_effect=timeout), contextlib.redirect_stderr(io.StringIO()) as stderr:
            with self.assertRaisesRegex(integrations.IntegrationError, "timed out after 10s"):
                integrations.command(["fai"], self.root, {}, 10, "example/runtime")
        self.assertIn("stuck", stderr.getvalue())

    def test_missing_executable_is_a_named_failure(self):
        with patch.object(integrations.subprocess, "run", side_effect=FileNotFoundError("gone")):
            with self.assertRaisesRegex(integrations.IntegrationError, "example/runtime: gone"):
                integrations.command(["missing"], self.root, {}, 10, "example/runtime")

    def test_arguments_are_never_run_through_a_shell(self):
        with patch.object(integrations.subprocess, "run", return_value=self.output) as run:
            integrations.command(["fai", "two words;$(false)"], self.root, {"TEST": "1"}, 10, "run")
        self.assertEqual(run.call_args.args[0], ["fai", "two words;$(false)"])
        self.assertFalse(run.call_args.kwargs.get("shell", False))

    def test_environment_overrides_do_not_leak_to_next_program(self):
        first = replace(self.spec, env=(("VALUE", "changed"),), modes=("jit",))
        second = replace(self.spec, name="second", modes=("jit",))
        package = replace(self.package, integrations=(first, second))
        environment = {"VALUE": "original"}
        with patch.object(integrations, "command", return_value=self.output) as command:
            self.run_package(package, environment=environment)
        self.assertEqual([call.args[2]["VALUE"] for call in command.call_args_list], ["changed", "original"])
        self.assertEqual(environment, {"VALUE": "original"})

    def test_templates_expand_paths_without_rescanning_replacements(self):
        directory = self.root / "${temp} with spaces"
        package = replace(self.package, directory=directory)
        result = integrations.expand("${package}/é ${fixture:corpus}/tests ${workspace} ${temp}", package,
                                     self.root / "packages", self.root / "output", {"corpus": self.root / "data"})
        self.assertEqual(result, f"{directory.resolve()}/é {self.root / 'data'}/tests {self.root / 'packages'} {self.root / 'output'}")

    def test_only_referenced_fixtures_are_fetched_and_reused(self):
        fixture = packages.Fixture("corpus", "https://example.org/corpus.git", "a" * 40)
        unused = replace(fixture, name="unused")
        first = replace(self.spec, args=("${fixture:corpus}/tests",), modes=("jit",))
        second = replace(first, name="second")
        package = replace(self.package, integrations=(first, second), fixtures=(fixture, unused))
        with patch.object(integrations, "checkout_fixture", return_value=self.root / "corpus") as checkout, \
                patch.object(integrations, "command", return_value=self.output) as command:
            self.run_package(package)
        self.assertEqual(checkout.call_count, 1)
        self.assertEqual(checkout.call_args.args[0], fixture)
        self.assertEqual(command.call_args_list[0].args[0][-1], f"{self.root / 'corpus'}/tests")

    def test_fetched_fixture_must_match_the_pin(self):
        fixture = packages.Fixture("corpus", "https://example.org/corpus.git", "a" * 40)
        wrong = subprocess.CompletedProcess([], 0, b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n", b"")
        with patch.object(integrations, "command", return_value=wrong):
            with self.assertRaisesRegex(integrations.IntegrationError, "differs from the declared commit"):
                integrations.checkout_fixture(fixture, self.root / "checkout", {})

    def test_fixture_checkout_uses_the_pinned_commit_not_latest(self):
        repository = self.root / "repository"
        repository.mkdir()

        def git(*args):
            return subprocess.check_output(["git", *args], cwd=repository, stderr=subprocess.DEVNULL).decode().strip()

        git("init")
        git("config", "user.email", "test@example.invalid")
        git("config", "user.name", "Test")
        (repository / "data.txt").write_bytes(b"first\n")
        git("add", ".")
        git("commit", "-m", "first")
        revision = git("rev-parse", "HEAD")
        (repository / "data.txt").write_bytes(b"second\n")
        git("commit", "-am", "second")
        fixture = packages.Fixture("corpus", repository.as_uri(), revision)
        checked = integrations.checkout_fixture(fixture, self.root / "checkout", dict(os.environ))
        self.assertEqual((checked / "data.txt").read_bytes(), b"first\n")

    def test_ci_selection_does_not_expand_a_second_time(self):
        dependent = packages.Package("dependent", self.root / "packages/dependent", ("example",))
        catalog = {"example": self.package, "dependent": dependent}
        self.assertEqual(integrations.selected(catalog, [], ci=True,
                         environment={"CI_PACKAGES": '["example"]', "PACKAGE_NAMES": "example"}), ["example"])

    def test_local_selection_includes_dependents(self):
        dependent = packages.Package("dependent", self.root / "packages/dependent", ("example",))
        self.assertEqual(integrations.selected({"example": self.package, "dependent": dependent}, ["example"]),
                         ["example", "dependent"])

    def test_invalid_ci_selection_is_rejected(self):
        with self.assertRaisesRegex(integrations.IntegrationError, "inconsistent CI package selection"):
            integrations.selected({"example": self.package}, [], ci=True,
                                  environment={"CI_PACKAGES": '["example"]', "PACKAGE_NAMES": "other"})

    def test_unknown_selected_package_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unknown packages"):
            integrations.selected({"example": self.package}, ["absent"])


if __name__ == "__main__":
    unittest.main()
