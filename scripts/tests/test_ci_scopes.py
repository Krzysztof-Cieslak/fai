"""Documentation, package test scopes, and non-transitive development effects."""

from dataclasses import replace
from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci
import packages


def package(name, dependencies=(), development=(), directory=None, **updates):
    return packages.Package(name, Path(directory or f"packages/{name}"), dependencies,
                            development=development, source_roots=("src",), test_roots=(".",), **updates)


def catalog():
    return {"json": package("json"), "web": package("web", ("json",)), "app": package("app", ("web",))}


class PackageScopeTests(unittest.TestCase):
    def selected(self, paths, checks, integrations=None, current=None, previous=None):
        current = catalog() if current is None else current
        result = ci.classify(paths, current, current if previous is None else previous)
        self.assertFalse(result["compiler"])
        self.assertEqual(result["package_checks"], checks)
        self.assertEqual(result["packages"], checks if integrations is None else integrations)
        return result

    def docs(self, path):
        result = self.selected([path], [])
        self.assertEqual(result["mode"], "docs")
        self.assertFalse(result["reporting"])

    def test_root_readme_needs_no_compiler(self):
        self.docs("README.md")

    def test_root_agent_guide_needs_no_compiler(self):
        self.docs("AGENTS.md")

    def test_root_llms_guide_needs_no_compiler(self):
        self.docs("llms.txt")

    def test_root_singular_llm_guide_needs_no_compiler(self):
        self.docs("llm.txt")

    def test_root_lowercase_readme_needs_no_compiler(self):
        self.docs("readme.md")

    def test_root_changelog_needs_no_compiler(self):
        self.docs("changelog.md")

    def test_package_readme_needs_no_compiler_or_consumers(self):
        self.docs("packages/json/README.md")

    def test_package_documentation_subtree_needs_no_compiler(self):
        self.docs("packages/json/docs/decoding.md")

    def test_package_agent_guide_needs_no_compiler(self):
        self.docs("packages/json/AGENTS.md")

    def test_root_docs_do_not_suppress_rust_changes(self):
        result = ci.classify(["README.md", "AGENTS.md", "crates/fai-runtime/src/lib.rs"], catalog(), catalog())
        self.assertTrue(result["compiler"])
        self.assertEqual(result["package_checks"], ["json", "web", "app"])

    def test_root_docs_do_not_suppress_package_source_changes(self):
        self.selected(["llms.txt", "packages/json/src/Json.fai"], ["json", "web", "app"])

    def test_package_docs_do_not_suppress_source_changes(self):
        self.selected(["packages/json/README.md", "packages/json/src/Json.fai"], ["json", "web", "app"])

    def test_unknown_root_text_files_stay_in_full_lane(self):
        self.assertTrue(ci.classify(["CMakeLists.txt"], catalog(), catalog())["compiler"])

    def test_test_only_change_does_not_retest_production_consumers(self):
        self.selected(["packages/json/test/CodecSpec.fai"], ["json"])

    def test_unexported_example_change_is_local_to_its_package(self):
        self.selected(["packages/json/examples/Example.fai"], ["json"])

    def test_integration_metadata_runs_only_owning_integrations(self):
        self.selected(["packages/json/ci.json"], [], ["json"])

    def test_integration_metadata_with_tests_retains_owning_checks(self):
        self.selected(["packages/json/ci.json", "packages/json/test/Spec.fai"], ["json"])

    def test_manifest_changes_remain_conservative(self):
        self.selected(["packages/json/fai.toml"], ["json", "web", "app"])

    def test_unknown_non_source_fixture_changes_remain_conservative(self):
        self.selected(["packages/json/fixtures/input.txt"], ["json", "web", "app"])

    def test_legacy_integration_manifest_can_change_dependencies(self):
        current = catalog()
        current["json"] = replace(current["json"], source_roots=None)
        self.selected(["packages/json/ci.json"], ["json", "web", "app"], current=current)

    def test_old_library_ownership_survives_a_changed_source_root(self):
        previous = catalog()
        current = catalog()
        current["json"] = replace(current["json"], source_roots=("library",))
        self.selected(["packages/json/src/Json.fai"], ["json", "web", "app"], current=current, previous=previous)

    def test_test_to_source_rename_selects_consumers(self):
        self.selected(["packages/json/test/Helper.fai", "packages/json/src/Helper.fai"], ["json", "web", "app"])

    def test_custom_test_root_overrides_a_broader_source_root(self):
        current = catalog()
        current["json"] = replace(current["json"], source_roots=(".",), test_roots=("specs",))
        self.selected(["packages/json/specs/Json.fai"], ["json"], current=current)

    def test_library_root_overrides_a_broader_test_root(self):
        self.selected(["packages/json/src/Json.fai"], ["json", "web", "app"])

    def test_equally_specific_roots_are_conservatively_production(self):
        current = catalog()
        current["json"] = replace(current["json"], source_roots=(".",))
        self.selected(["packages/json/Json.fai"], ["json", "web", "app"], current=current)

    def test_explicit_binary_entry_is_not_a_library_source(self):
        current = catalog()
        current["json"] = replace(current["json"], entrypoints=("src/Inspect.fai",))
        self.selected(["packages/json/src/Inspect.fai"], ["json"], current=current)

    def test_platform_specific_library_sources_select_consumers(self):
        current = catalog()
        current["json"] = replace(current["json"], source_roots=("src", "windows"))
        self.selected(["packages/json/windows/Json.fai"], ["json", "web", "app"], current=current)

    def test_platform_specific_test_sources_remain_local(self):
        current = catalog()
        current["json"] = replace(current["json"], test_roots=(".", "windows/tests"))
        self.selected(["packages/json/windows/tests/Json.fai"], ["json"], current=current)

    def test_development_dependency_effect_does_not_leak_to_library_consumers(self):
        current = {"helper": package("helper"), "web": package("web", development=("helper",)),
                   "app": package("app", ("web",))}
        self.selected(["packages/helper/src/Helper.fai"], ["web", "helper"], current=current)

    def test_normal_dependencies_of_a_development_helper_still_affect_its_user(self):
        current = {"json": package("json"), "helper": package("helper", ("json",)),
                   "web": package("web", development=("helper",)), "app": package("app", ("web",))}
        self.selected(["packages/json/src/Json.fai"], ["web", "json", "helper"], current=current)

    def test_development_helper_test_edit_does_not_affect_its_users(self):
        current = {"helper": package("helper"), "web": package("web", development=("helper",))}
        self.selected(["packages/helper/test/HelperSpec.fai"], ["helper"], current=current)

    def test_removed_development_edges_still_select_previous_test_users(self):
        previous = {"helper": package("helper"), "web": package("web", development=("helper",)),
                    "app": package("app", ("web",))}
        current = {**previous, "web": replace(previous["web"], development=())}
        self.selected(["packages/helper/src/Helper.fai"], ["web", "helper"], current=current, previous=previous)

    def test_nested_helper_source_retains_development_consumers(self):
        current = {"web": package("web", development=("web-examples",)),
                   "web-examples": package("web-examples", directory="packages/web/examples"),
                   "app": package("app", ("web",))}
        self.selected(["packages/web/examples/src/Helper.fai"], ["web", "web-examples"], current=current)

    def test_nested_package_test_is_not_owned_by_its_parent(self):
        current = {"web": package("web", development=("web-examples",)),
                   "web-examples": package("web-examples", directory="packages/web/examples"),
                   "app": package("app", ("web",))}
        self.selected(["packages/web/examples/test/Helper.fai"], ["web-examples"], current=current)


class CheckOutputTests(unittest.TestCase):
    def test_missing_check_selection_fails_the_required_gate(self):
        with self.assertRaises(ValueError):
            ci.selected_checks("null", "", [], "docs")

    def test_check_selection_must_be_a_subset_of_selected_packages(self):
        with self.assertRaisesRegex(ValueError, "check selection"):
            ci.selected_checks('["web"]', "web", ["json"], "packages")

    def test_full_compiler_lane_cannot_drop_native_package_checks(self):
        with self.assertRaisesRegex(ValueError, "check selection"):
            ci.selected_checks('[]', "", ["json"], "compiler")

    def test_docs_lane_cannot_accidentally_select_package_work(self):
        with self.assertRaisesRegex(ValueError, "non-package lane"):
            ci.selected_checks('[]', "", ["json"], "docs")

    def test_integration_only_selection_needs_no_contract_checks(self):
        self.assertEqual(ci.selected_checks('[]', "", ["json"], "packages"), [])

    def test_release_forcing_retains_every_native_package_check(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(Path(temporary) / "summary")}), \
                    patch.object(sys, "argv", ["ci.py", "scope", "--force-full", "--github-output"]), \
                    patch.object(packages, "discover", return_value=catalog()), redirect_stdout(io.StringIO()):
                ci.main()
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual(values["compiler"], "true")
        self.assertEqual(values["packages"], values["package_checks"])
        self.assertEqual(values["package_names"], values["package_check_names"])

    def test_documentation_outputs_cannot_enable_package_checks(self):
        result = ci.classify(["AGENTS.md", "README.md", "llms.txt"], catalog(), catalog())
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(Path(temporary) / "summary")}), \
                    patch.object(sys, "argv", ["ci.py", "scope", "--github-output"]), \
                    patch.object(ci, "scope", return_value=result), redirect_stdout(io.StringIO()):
                ci.main()
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual((values["mode"], values["compiler"], values["packages"], values["package_checks"]),
                         ("docs", "false", "[]", "[]"))
