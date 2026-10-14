"""Isolated VS Code checks, mixed changes, and required editor-selection outputs."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci
import packages


ROOT = Path(__file__).resolve().parents[2]


def catalog():
    return {"json": packages.Package("json", Path("packages/json"), (), source_roots=("src",), test_roots=("test",))}


class EditorSelectionTests(unittest.TestCase):
    def selected(self, paths):
        return ci.classify(paths, catalog(), catalog())

    def editor_only(self, path):
        result = self.selected([path])
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["vscode"],
                          result["packages"], result["package_checks"]),
                         ("vscode", False, False, True, [], []))

    def test_extension_source_uses_only_editor_checks(self):
        self.editor_only("editors/vscode/src/extension.ts")

    def test_textmate_grammar_uses_only_editor_checks(self):
        self.editor_only("editors/vscode/syntaxes/fai.tmLanguage.json")

    def test_npm_manifest_uses_only_editor_checks(self):
        self.editor_only("editors/vscode/package.json")

    def test_npm_lock_uses_only_editor_checks(self):
        self.editor_only("editors/vscode/package-lock.json")

    def test_packaged_readme_uses_editor_packaging_not_rust(self):
        self.editor_only("editors/vscode/README.md")

    def test_root_documentation_still_needs_no_build(self):
        result = self.selected(["README.md", "AGENTS.md", "llms.txt", "llm.txt"])
        self.assertEqual((result["mode"], result["compiler"], result["vscode"]), ("docs", False, False))

    def test_editor_and_root_documentation_use_only_editor_checks(self):
        result = self.selected(["README.md", "editors/vscode/src/extension.ts"])
        self.assertEqual((result["mode"], result["compiler"], result["vscode"]), ("vscode", False, True))

    def test_editor_and_package_changes_select_both_without_rust(self):
        result = self.selected(["packages/json/src/Json.fai", "editors/vscode/src/extension.ts"])
        self.assertEqual((result["mode"], result["compiler"], result["vscode"], result["packages"]),
                         ("packages", False, True, ["json"]))

    def test_editor_and_reporting_changes_select_both_without_rust(self):
        result = self.selected(["scripts/performance_report/render.py", "editors/vscode/src/extension.ts"])
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["vscode"]),
                         ("reporting", False, True, True))

    def test_compiler_changes_retain_editor_grammar_validation(self):
        result = self.selected(["crates/fai-syntax/src/lexer.rs"])
        self.assertEqual((result["mode"], result["compiler"], result["vscode"]), ("compiler", True, True))

    def test_sample_changes_retain_editor_grammar_validation(self):
        result = self.selected(["samples/NewSyntax.fai"])
        self.assertTrue(result["compiler"] and result["vscode"])

    def test_editor_and_workflow_changes_retain_full_checks(self):
        result = self.selected([".github/workflows/ci.yml", "editors/vscode/src/extension.ts"])
        self.assertTrue(result["compiler"] and result["vscode"])

    def test_similar_directory_name_does_not_get_editor_only_treatment(self):
        self.assertTrue(self.selected(["editors/vscode-tools/check.py"])["compiler"])

    def test_rename_out_of_editor_tree_retains_destination_checks(self):
        paths = ci.diff_paths(b"R100\0editors/vscode/src/old.ts\0scripts/new.ts\0")
        self.assertTrue(self.selected(paths)["compiler"])

    def test_committed_editor_diff_uses_the_editor_lane(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.DEVNULL).decode().strip()
            git("init", "-b", "main")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "Test")
            source = root / "editors/vscode/src/extension.ts"
            source.parent.mkdir(parents=True)
            source.write_text("export const version = 1;\n")
            git("add", ".")
            git("commit", "-m", "Add extension")
            base = git("rev-parse", "HEAD")
            source.write_text("export const version = 2;\n")
            git("commit", "-am", "Update extension")
            result = ci.scope(root, base)
        self.assertEqual((result["mode"], result["compiler"], result["vscode"], result["packages"]),
                         ("vscode", False, True, []))


class EditorGateTests(unittest.TestCase):
    def test_editor_lane_requires_its_checks(self):
        with self.assertRaisesRegex(ValueError, "VS Code selection"):
            ci.require("success", "vscode", "false", "false", "false")

    def test_editor_lane_can_pass_with_explicit_selection(self):
        ci.require("success", "vscode", "false", "false", "true")

    def test_full_lane_cannot_drop_editor_checks(self):
        with self.assertRaisesRegex(ValueError, "VS Code selection"):
            ci.require("success", "compiler", "true", "true", "false")

    def test_missing_editor_flag_fails_required_gates(self):
        with self.assertRaisesRegex(ValueError, "VS Code selection"):
            ci.require("success", "docs", "false", "false", None)

    def test_docs_lane_keeps_all_builds_disabled(self):
        with self.assertRaisesRegex(ValueError, "VS Code selection"):
            ci.require("success", "docs", "false", "false", "true")

    def test_editor_lane_cannot_hide_reporting_work(self):
        with self.assertRaisesRegex(ValueError, "reporting selection"):
            ci.require("success", "vscode", "false", "true", "true")

    def test_editor_lane_cannot_hide_package_work(self):
        with self.assertRaisesRegex(ValueError, "non-package lane"):
            ci.selected_checks("[]", "", ["json"], "vscode")

    def test_mixed_package_editor_flags_are_valid(self):
        ci.require("success", "packages", "false", "false", "true")


class EditorWorkflowTests(unittest.TestCase):
    def test_required_editor_checks_run_once_under_the_existing_lint_gate(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        lint = workflow.split("  lint:", 1)[1].split("  unit:", 1)[0]
        self.assertEqual(workflow.count("uses: ./.github/actions/vscode"), 1)
        self.assertIn("uses: ./.github/actions/vscode", lint)
        self.assertIn("if: needs.changes.outputs.vscode == 'true'", lint)

    def test_editor_selection_reaches_every_required_job(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertEqual(workflow.count("CI_VSCODE: ${{ needs.changes.outputs.vscode }}"), 3)
        self.assertIn("vscode: ${{ steps.scope.outputs.vscode }}", workflow)

    def test_isolated_editor_wrappers_do_not_require_native_platform_runners(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("runs-on: ${{ (needs.changes.outputs.mode == 'vscode' || needs.changes.outputs.mode == 'docs') && 'ubuntu-latest' || matrix.os }}", workflow)

    def test_manual_workflow_reuses_the_same_editor_checks(self):
        workflow = (ROOT / ".github/workflows/editors.yml").read_text()
        self.assertIn("uses: ./.github/actions/vscode", workflow)
        self.assertNotIn("run: npm", workflow)

    def test_editor_action_keeps_all_checks_and_the_packaged_artifact(self):
        action = (ROOT / ".github/actions/vscode/action.yml").read_text()
        self.assertIn("run: npm ci", action)
        self.assertIn("run: npm run check-types", action)
        self.assertIn("run: npm test", action)
        self.assertIn("run: npm run package", action)
        self.assertIn("path: editors/vscode/fai.vsix", action)
        self.assertIn("if-no-files-found: error", action)
