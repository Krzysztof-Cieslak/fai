"""Change classification, stack-base selection, and required-gate behavior."""

from pathlib import Path
import json
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

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

    def test_package_integration_metadata_stays_in_the_package_lane(self):
        result = ci.classify(["packages/json/ci.json", "packages/json/examples/Runtime.fai"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["json", "web"]))

    def test_package_integration_runner_changes_run_full_checks(self):
        self.assertTrue(ci.classify(["scripts/package_integrations.py"], catalog(), catalog())["compiler"])

    def test_unknown_paths_run_full_checks(self):
        self.assertTrue(ci.classify(["new-build-input.txt"], catalog(), catalog())["compiler"])

    def test_docs_changes_need_no_compiler(self):
        result = ci.classify(["docs/DEVELOPMENT.md", "README.md"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]), ("docs", False, []))

    def test_report_presentation_selects_only_python_checks(self):
        result = ci.classify(["scripts/performance_report/__init__.py", "docs/PERFORMANCE_REPORTS.md"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["packages"]),
                         ("reporting", False, True, []))

    def test_report_and_package_edits_select_both_without_rust(self):
        result = ci.classify(["scripts/performance_report/tests/fixtures/normal.json", "packages/json/src/Json.fai"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["packages"]),
                         ("packages", False, True, ["json", "web"]))

    def test_report_data_edits_still_run_compiler_checks(self):
        result = ci.classify(["crates/fai-tests/src/bench_summary/tracking/data.rs"], catalog(), catalog())
        self.assertTrue(result["compiler"] and result["reporting"])

    def test_reporting_mode_cannot_skip_its_checks(self):
        with self.assertRaisesRegex(ValueError, "reporting selection"):
            ci.require("success", "reporting", "false", "false")
        ci.require("success", "reporting", "false", "true")

    def test_removed_dependency_edges_still_select_previous_dependents(self):
        result = ci.classify(["packages/json/src/Json.fai"], catalog(()), catalog())
        self.assertEqual(result["packages"], ["json", "web"])

    def test_nested_example_edit_uses_its_own_package_and_development_dependents(self):
        nested = catalog()
        nested["web-examples"] = packages.Package("web-examples", Path("packages/web/examples"), ("web",))
        nested["web"] = packages.Package("web", Path("packages/web"), ("json",), development=("web-examples",))
        result = ci.classify(["packages/web/examples/App.fai"], nested, nested)
        self.assertEqual(result["packages"], ["web", "web-examples"])
        self.assertFalse(result["compiler"])

    def test_workspace_manifest_without_snapshots_selects_all_packages_without_rust(self):
        result = ci.classify(["packages/fai.toml"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["packages"]),
                         ("packages", False, False, ["json", "web"]))
        self.assertEqual(result["reasons"], ["package workspace: packages/fai.toml"])

    def test_workspace_lock_without_snapshots_selects_all_packages_without_rust(self):
        result = ci.classify(["packages/fai.lock"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["json", "web"]))

    def test_new_package_without_snapshots_conservatively_keeps_the_package_lane(self):
        current = {**catalog(), "encoding": packages.Package("encoding", Path("packages/encoding"), ())}
        result = ci.classify(["packages/encoding/fai.toml", "packages/encoding/src/Hex.fai",
                              "packages/fai.toml", "packages/fai.lock"], current, catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["encoding", "json", "web"]))

    def test_workspace_metadata_with_rust_changes_still_runs_full_checks(self):
        result = ci.classify(["packages/fai.lock", "crates/fai-cli/src/lib.rs"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("compiler", True, ["json", "web"]))

    def test_workspace_metadata_with_presentation_changes_runs_both_lanes(self):
        result = ci.classify(["packages/fai.toml", "scripts/performance_report/__init__.py"], catalog(), catalog())
        self.assertEqual((result["mode"], result["compiler"], result["reporting"], result["packages"]),
                         ("packages", False, True, ["json", "web"]))

    def test_workspace_removal_selects_only_surviving_packages(self):
        current = {"web": catalog(())["web"]}
        result = ci.classify(["packages/fai.toml", "packages/json/fai.toml"], current, catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["web"]))

    def test_empty_workspace_metadata_still_requires_package_validation(self):
        result = ci.classify(["packages/fai.toml", "packages/fai.lock"], {}, catalog())
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, []))

    def test_cargo_lock_changes_still_run_full_checks(self):
        self.assertTrue(ci.classify(["Cargo.lock"], catalog(), catalog())["compiler"])

    def test_unrecognized_workspace_files_still_run_full_checks(self):
        self.assertTrue(ci.classify(["packages/build.py"], catalog(), catalog())["compiler"])

    def test_classifier_changes_still_run_full_checks(self):
        self.assertTrue(ci.classify(["scripts/ci.py", "packages/fai.toml"], catalog(), catalog())["compiler"])

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
                dependencies = "\n".join(f'{dependency} = {{path = "../{dependency}"}}' for dependency in package.dependencies)
                (directory / packages.MANIFEST).write_text(f'manifest-version = 1\n[package]\nname = "{name}"\nversion = "0.1.0"\n[dependencies]\n{dependencies}\n')
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

    def test_committed_integration_manifest_edit_uses_package_lane(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, stderr=subprocess.DEVNULL).decode().strip()

            git("init", "-b", "main")
            git("config", "user.email", "test@example.invalid")
            git("config", "user.name", "Test")
            directory = root / "packages/example"
            directory.mkdir(parents=True)
            manifest = {"schemaVersion": 1}
            (directory / packages.MANIFEST).write_text('manifest-version = 1\n[package]\nname = "example"\nversion = "0.1.0"\n')
            (directory / packages.LEGACY_MANIFEST).write_text(json.dumps(manifest))
            git("add", ".")
            git("commit", "-m", "Add package")
            base = git("rev-parse", "HEAD")
            (directory / "Run.fai").write_text("module Run\n")
            manifest["integrations"] = [{"name": "runtime", "entry": "Run.fai", "stdoutLines": ["passed"]}]
            (directory / packages.LEGACY_MANIFEST).write_text(json.dumps(manifest))
            git("add", ".")
            git("commit", "-m", "Add integration entrypoint")
            result = ci.scope(root, base)
            self.assertEqual((result["mode"], result["compiler"], result["packages"], result["reasons"]),
                             ("packages", False, ["example"], ["package: example"]))


class WorkspaceMetadataScopeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "-b", "main")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "Test")
        for name in ("json", "web"):
            directory = self.root / "packages" / name
            directory.mkdir(parents=True)
            (directory / "fai.toml").write_text(
                f'manifest-version = 1\n[package]\nname = "{name}"\nversion = "0.1.0"\n')
        self.workspace = self.root / "packages/fai.toml"
        self.workspace.write_text('manifest-version = 1\n[workspace]\nmembers = ["json", "web"]\n')
        self.lock = self.root / "packages/fai.lock"
        self.lock.write_text('lock-version = 1\nresolver-version = 1\ncontext = "shared"\n' + "".join(
            f'\n[[packages]]\nid = "{name}"\nversion = "0.1.0"\nmanifest-fingerprint = "original"\n'
            f'[packages.logical]\npath = "{name}"\n[packages.source]\npath = "{name}"\n'
            for name in ("json", "web")) + "".join(
            f'\n[[graphs]]\nid = "{name}-linux"\nroot = "{name}"\ntarget = "x86_64-unknown-linux-gnu"\n'
            f'root-package = "{name}"\ninput-fingerprint = "original"\npackages = ["{name}"]\n'
            'resolved-with = "0.0.1"\n' for name in ("json", "web")))
        self.git("add", ".")
        self.git("commit", "-m", "Add workspace")
        self.base = self.git("rev-parse", "HEAD")

    def git(self, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=self.root,
                                       stderr=subprocess.DEVNULL).decode().strip()

    def classify(self):
        self.git("add", "-A")
        self.git("commit", "-m", "Change workspace metadata")
        return ci.scope(self.root, self.base)

    def test_committed_lock_comment_needs_only_workspace_validation(self):
        with self.lock.open("a") as output:
            output.write("# Updated graph records\n")
        result = self.classify()
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, []))

    def test_committed_lock_graph_edit_selects_only_its_package(self):
        prefix, web = self.lock.read_text().split('id = "web-linux"', 1)
        self.lock.write_text(prefix + 'id = "web-linux"' + web.replace('input-fingerprint = "original"',
                                                                     'input-fingerprint = "changed"'))
        result = self.classify()
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["web"]))

    def test_new_package_registration_does_not_retest_existing_packages(self):
        directory = self.root / "packages/yaml"
        directory.mkdir()
        (directory / "fai.toml").write_text(
            'manifest-version = 1\n[package]\nname = "yaml"\nversion = "0.1.0"\n'
            '[dependencies]\njson = {path = "../json"}\n')
        self.workspace.write_text(
            self.workspace.read_text().replace('["json", "web"]', '["json", "web", "yaml"]')
            + '\n[[overrides]]\nreplace = {git = "https://github.com/Krzysztof-Cieslak/fai.git", subdir = "packages/yaml"}\n'
            'with = {path = "yaml"}\n')
        with self.lock.open("a") as output:
            output.write('\n[[packages]]\nid = "yaml"\nversion = "0.1.0"\nmanifest-fingerprint = "new"\n'
                         '[packages.logical]\npath = "yaml"\n[packages.source]\npath = "yaml"\n'
                         '[[graphs]]\nid = "yaml-linux"\nroot = "yaml"\ntarget = "x86_64-unknown-linux-gnu"\n'
                         'root-package = "yaml"\ninput-fingerprint = "new"\npackages = ["yaml", "json"]\n'
                         'resolved-with = "0.0.1"\n[[graphs.edges]]\nfrom = "yaml"\nto = "json"\nalias = "json"\nkind = "normal"\n')
        result = self.classify()
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["yaml"]))

    def test_workspace_override_edit_selects_every_package(self):
        with self.workspace.open("a") as output:
            output.write('\n[[overrides]]\nreplace = {git = "https://example.invalid/repo", subdir = "lib"}\nwith = {path = "json"}\n')
        result = self.classify()
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["json", "web"]))

    def test_deleting_workspace_metadata_keeps_package_checks(self):
        self.workspace.unlink()
        self.lock.unlink()
        result = self.classify()
        self.assertEqual((result["mode"], result["compiler"], result["packages"]),
                         ("packages", False, ["json", "web"]))

    def test_invalid_workspace_toml_fails_selection(self):
        self.workspace.write_text('manifest-version = [\n')
        with self.assertRaises(ValueError):
            self.classify()

    def test_invalid_lock_toml_fails_selection(self):
        self.lock.write_text('lock-version = [\n')
        with self.assertRaises(ValueError):
            self.classify()


class PackageWorkspaceValidationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.workspace = self.root / "packages"
        self.workspace.mkdir()

    def test_versioned_workspace_uses_prepared_compiler_and_locked_resolution(self):
        (self.workspace / "fai.toml").write_text('manifest-version = 1\n[workspace]\nmembers = []\n')
        with mock.patch.object(ci.subprocess, "run") as run:
            ci.validate_package_workspace(self.root, "prepared-fai")
        run.assert_called_once_with(["prepared-fai", "resolve", "--locked", "-C", str(self.workspace)], check=True)

    def test_compiler_rejecting_metadata_fails_validation(self):
        (self.workspace / "fai.toml").write_text('manifest-version = 1\n[workspace]\nmembers = []\n')
        with mock.patch.object(ci.subprocess, "run", side_effect=subprocess.CalledProcessError(1, ["fai"])):
            with self.assertRaises(subprocess.CalledProcessError):
                ci.validate_package_workspace(self.root, "fai")

    def test_workspace_lock_without_manifest_fails_validation(self):
        (self.workspace / "fai.lock").write_text('lock-version = 1\n')
        with mock.patch.object(ci.subprocess, "run") as run:
            with self.assertRaisesRegex(packages.PackageError, "lock.*manifest"):
                ci.validate_package_workspace(self.root, "fai")
        run.assert_not_called()

    def test_legacy_package_tree_needs_no_workspace_lock(self):
        with mock.patch.object(ci.subprocess, "run") as run:
            ci.validate_package_workspace(self.root, "fai")
        run.assert_not_called()
