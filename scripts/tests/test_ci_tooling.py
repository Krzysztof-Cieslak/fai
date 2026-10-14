"""Explicit tooling lanes and their required portable validation."""

from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci
import ci_tooling
import packages

ROOT = Path(__file__).resolve().parents[2]


class ToolingSelectionTests(unittest.TestCase):
    def selected(self, paths):
        return ci.classify(paths, {}, {})

    def test_release_script_selects_release_checks_without_rust(self):
        result = self.selected(["scripts/release.py"])
        self.assertEqual((result["mode"], result["compiler"], result["tooling"], result["packages"]),
                         ("tooling", False, ["release"], []))

    def test_installer_templates_select_portable_release_validation(self):
        result = self.selected(["scripts/install/install.sh", "scripts/install/install.ps1"])
        self.assertEqual(result["tooling"], ["release"])
        self.assertFalse(result["compiler"])

    def test_performance_publication_selects_python_and_renderer_checks(self):
        result = self.selected(["scripts/performance.py", "scripts/performance_publish.py"])
        self.assertEqual((result["mode"], result["tooling"], result["reporting"], result["compiler"]),
                         ("tooling", ["performance"], True, False))

    def test_junit_renderer_needs_neither_compiler_nor_package_checks(self):
        result = self.selected(["scripts/junit-summary.py", "scripts/tests/test_junit_summary.py"])
        self.assertEqual((result["mode"], result["tooling"], result["vscode"], result["packages"]),
                         ("tooling", ["junit"], False, []))

    def test_mixed_tooling_groups_are_combined(self):
        result = self.selected(["scripts/release.py", "scripts/performance.py"])
        self.assertEqual(result["tooling"], ["performance", "release"])
        self.assertTrue(result["reporting"])

    def test_mixed_package_changes_keep_the_package_lane(self):
        catalog = {"json": packages.Package("json", Path("packages/json"), ())}
        result = ci.classify(["scripts/release.py", "packages/json/Json.fai"], catalog, catalog)
        self.assertEqual((result["mode"], result["tooling"], result["package_checks"]),
                         ("packages", ["release"], ["json"]))

    def test_compiler_bootstrap_still_selects_full_checks(self):
        result = self.selected(["scripts/compiler.py", "scripts/release.py"])
        self.assertTrue(result["compiler"])
        self.assertEqual(result["tooling"], sorted(ci_tooling.TESTS))

    def test_shared_tooling_runner_still_selects_full_checks(self):
        self.assertTrue(self.selected(["scripts/ci_tooling.py"])["compiler"])

    def test_release_matrix_remains_a_full_build_input(self):
        self.assertTrue(self.selected(["scripts/release-targets.json"])["compiler"])

    def test_unknown_python_file_does_not_inherit_a_lightweight_lane(self):
        self.assertTrue(self.selected(["scripts/new_release_helper.py"])["compiler"])

    def test_native_packaging_fixture_changes_remain_full(self):
        self.assertTrue(self.selected(["scripts/tests/tooling_packaging.py"])["compiler"])

    def test_renaming_a_tool_outside_the_allowlist_remains_full(self):
        paths = ci.diff_paths(b"R100\0scripts/release.py\0scripts/other-release.py\0")
        self.assertTrue(self.selected(paths)["compiler"])

    def test_doc_changes_do_not_select_any_tooling(self):
        result = self.selected(["README.md", "AGENTS.md", "llms.txt"])
        self.assertEqual((result["mode"], result["tooling"], result["compiler"]), ("docs", [], False))


class ToolingGateTests(unittest.TestCase):
    def test_tooling_lane_requires_a_nonempty_selection(self):
        with self.assertRaisesRegex(ValueError, "tooling selection"):
            ci.require("success", "tooling", "false", "false", "false", "[]")

    def test_full_lane_cannot_drop_tooling_checks(self):
        with self.assertRaisesRegex(ValueError, "tooling selection"):
            ci.require("success", "compiler", "true", "true", "true", '["junit"]')

    def test_performance_lane_cannot_drop_presentation_checks(self):
        with self.assertRaisesRegex(ValueError, "presentation checks"):
            ci.require("success", "tooling", "false", "false", "false", '["performance"]')

    def test_release_tooling_flags_are_valid(self):
        ci.require("success", "tooling", "false", "false", "false", '["release"]')

    def test_missing_tooling_selection_fails_closed(self):
        with self.assertRaises(ValueError):
            ci_tooling.selection(None)

    def test_unknown_group_fails_closed(self):
        with self.assertRaises(ValueError):
            ci_tooling.selection('["arbitrary"]')

    def test_duplicate_groups_fail_closed(self):
        with self.assertRaises(ValueError):
            ci_tooling.selection('["release","release"]')

    def test_tooling_lane_cannot_hide_package_work(self):
        with self.assertRaisesRegex(ValueError, "non-package"):
            ci.selected_checks("[]", "", ["json"], "tooling")


class ToolingRunnerTests(unittest.TestCase):
    def test_junit_checks_invoke_only_their_python_tests(self):
        with patch.object(ci_tooling.subprocess, "run") as run:
            ci_tooling.run(["junit"], ROOT)
        run.assert_called_once_with([sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests",
                                    "-p", "test_junit_summary.py", "-v"], cwd=ROOT, check=True)

    def test_release_checks_validate_posix_script_syntax(self):
        with patch.object(ci_tooling.subprocess, "run") as run:
            ci_tooling.run(["release"], ROOT, windows=False)
        self.assertEqual(run.call_args_list[-1].args[0], ["sh", "-n", "scripts/install/install.sh"])

    def test_missing_test_file_cannot_become_an_empty_successful_suite(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci_tooling.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "missing required tooling tests"):
                ci_tooling.run(["junit"], Path(temporary))
        run.assert_not_called()

    def test_windows_release_checks_require_both_shells(self):
        with patch.object(ci_tooling.shutil, "which", return_value=None), patch.object(ci_tooling.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "require pwsh"):
                ci_tooling.run(["release"], ROOT, windows=True)
        run.assert_not_called()

    def test_windows_powershell_checks_cannot_be_silently_skipped(self):
        with patch.object(ci_tooling.shutil, "which", side_effect=lambda value: "pwsh" if value == "pwsh" else None), \
                patch.object(ci_tooling.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "require powershell.exe"):
                ci_tooling.run(["release"], ROOT, windows=True)
        run.assert_not_called()

    def test_docs_and_editor_wrappers_use_ubuntu(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("(needs.changes.outputs.mode == 'vscode' || needs.changes.outputs.mode == 'docs')", workflow)
        self.assertIn("&& 'ubuntu-latest' || matrix.os", workflow)

    def test_tooling_selection_reaches_all_required_jobs(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertEqual(workflow.count("CI_TOOLING: ${{ needs.changes.outputs.tooling }}"), 3)
        self.assertIn("tooling: ${{ steps.scope.outputs.tooling }}", workflow)

    def test_release_smoke_runs_on_linux_and_native_platforms(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertEqual(workflow.count("name: Release tooling packaging smoke"), 2)
        self.assertIn("scripts/tests/tooling_packaging.py --fai", workflow)
