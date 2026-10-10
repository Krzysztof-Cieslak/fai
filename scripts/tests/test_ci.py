"""Change classification, stack-base selection, and required-gate behavior."""

from pathlib import Path
import json
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci
import packages


def catalog(web_dependencies=("json",)):
    return {"json": packages.Package("json", Path("packages/json"), ()),
            "web": packages.Package("web", Path("packages/web"), web_dependencies)}


class ClassificationTests(unittest.TestCase):
    def test_json_changes_select_native_tests_and_dependents(self):
        result = ci.classify(["packages/json/src/Json.fai"], catalog(), catalog())
        self.assertFalse(result["compiler"])
        self.assertEqual(result["packages"], ["json", "web"])

    def test_web_changes_select_only_web(self):
        result = ci.classify(["packages/web/test/WebSpec.fai"], catalog(), catalog())
        self.assertEqual(result["packages"], ["web"])
        self.assertEqual(result["mode"], "packages")

    def test_rust_changes_run_full_checks(self):
        result = ci.classify(["crates/fai-runtime/src/lib.rs"], catalog(), catalog())
        self.assertTrue(result["compiler"])
        self.assertEqual(result["packages"], ["json", "web"])

    def test_embedded_std_changes_are_compiler_changes(self):
        self.assertTrue(ci.classify(["std/core/Prelude.fai"], catalog(), catalog())["compiler"])

    def test_workflow_changes_run_full_checks(self):
        self.assertTrue(ci.classify([".github/workflows/ci.yml"], catalog(), catalog())["compiler"])

    def test_unknown_paths_run_full_checks(self):
        self.assertTrue(ci.classify(["new-build-input.txt"], catalog(), catalog())["compiler"])

    def test_docs_changes_need_no_compiler(self):
        result = ci.classify(["docs/DEVELOPMENT.md", "README.md"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]), ("docs", False, []))

    def test_removed_dependency_edges_still_select_previous_dependents(self):
        result = ci.classify(["packages/json/src/Json.fai"], catalog(()), catalog())
        self.assertEqual(result["packages"], ["json", "web"])

    def test_removed_package_selects_remaining_previous_dependents(self):
        current = {"web": catalog(())["web"]}
        result = ci.classify(["packages/json/src/Json.fai"], current, catalog())
        self.assertEqual(result["packages"], ["web"])

    def test_renames_include_both_paths(self):
        self.assertEqual(ci.diff_paths(b"R100\0packages/json/old.fai\0packages/web/new.fai\0"),
                         ["packages/json/old.fai", "packages/web/new.fai"])

    def test_classification_failure_cannot_pass_a_required_gate(self):
        with self.assertRaisesRegex(ValueError, "did not succeed"):
            ci.require("failure", "docs", "false")

    def test_skipped_classification_cannot_pass_a_required_gate(self):
        with self.assertRaises(ValueError):
            ci.require("skipped", "docs", "false")

    def test_successful_classification_passes_gate(self):
        ci.require("success", "docs", "false")

    def test_missing_outputs_cannot_pass_a_required_gate(self):
        with self.assertRaisesRegex(ValueError, "classification outputs"):
            ci.require("success", None, None)

    def test_inconsistent_outputs_cannot_pass_a_required_gate(self):
        with self.assertRaisesRegex(ValueError, "classification outputs"):
            ci.require("success", "compiler", "false")

    def test_missing_selection_cannot_pass_a_required_gate(self):
        with self.assertRaisesRegex(ValueError, "package selection"):
            ci.selected_packages("null", "")

    def test_selection_cannot_disagree_with_the_test_loop(self):
        with self.assertRaisesRegex(ValueError, "inconsistent"):
            ci.selected_packages('["json", "web"]', "")

    def test_selection_reaches_the_test_loop_unchanged(self):
        self.assertEqual(ci.selected_packages('["json", "web"]', "json web"), ["json", "web"])

    def test_stack_diff_uses_immediate_base_instead_of_main(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.DEVNULL).decode().strip()

            git("init", "-b", "base")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "Test")
            for name, package in catalog().items():
                directory = root / "packages" / name
                directory.mkdir(parents=True)
                (directory / packages.MANIFEST).write_text(json.dumps({"schemaVersion": 1, "name": name,
                                                                      "dependencies": package.dependencies}))
                (directory / "Code.fai").write_text("module Code\n")
            (root / "compiler.rs").write_text("old")
            git("add", ".")
            git("commit", "-m", "base")
            base = git("rev-parse", "HEAD")
            git("switch", "-c", "compiler-change")
            (root / "compiler.rs").write_text("new")
            git("commit", "-am", "compiler")
            immediate = git("rev-parse", "HEAD")
            git("switch", "-c", "package-change")
            (root / "packages/web/Code.fai").write_text("module Changed\n")
            git("commit", "-am", "package")
            result = ci.scope(root, immediate)
            self.assertEqual((result["mode"], result["packages"]), ("packages", ["web"]))
            self.assertTrue(ci.scope(root, base)["compiler"])
