"""Narrow Cargo target selection and exact cross-platform coverage equivalence."""

import copy
from contextlib import redirect_stdout
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import cross_os


POLICY = "(package(phase) | binary(native)) - (test(/proptest/) | binary(properties))"


def target(name, kind="test", enabled=True):
    return {"name": name, "kind": [kind], "test": enabled}


def metadata():
    packages = [
        {"id": "phase-id", "name": "phase", "targets": [target("phase", "lib"), target("tool", "bin"),
         target("phase-integration"), target("collision")]},
        {"id": "shared-id", "name": "shared", "targets": [target("shared", "lib"), target("native"),
         target("collision"), target("unrelated")]},
        {"id": "other-id", "name": "other", "targets": [target("other", "lib"), target("unrelated")]},
        {"id": "external-id", "name": "external", "targets": [target("native")]},
    ]
    return {"workspace_members": ["phase-id", "shared-id", "other-id"], "packages": packages}


def inventory(*cases):
    result = {"rust-suites": {}}
    for binary, name, status in cases:
        result["rust-suites"].setdefault(binary, {"testcases": {}})["testcases"][name] = {
            "filter-match": {"status": status}}
    return result


class BuildSelectionTests(unittest.TestCase):
    def setUp(self):
        self.metadata = metadata()

    def test_only_required_packages_and_target_names_are_requested(self):
        self.assertEqual(cross_os.plan(self.metadata, POLICY), [
            "--package", "phase", "--package", "shared", "--lib", "--bin", "tool",
            "--test", "collision", "--test", "native", "--test", "phase-integration"])

    def test_new_tests_in_selected_packages_are_included_automatically(self):
        self.metadata["packages"][0]["targets"].append(target("new-regression"))
        self.assertIn("new-regression", cross_os.plan(self.metadata, POLICY))

    def test_unselected_integration_targets_stay_out_of_the_build(self):
        self.assertNotIn("unrelated", cross_os.plan(self.metadata, POLICY))

    def test_binary_ownership_comes_from_cargo_not_a_hardcoded_package(self):
        self.metadata["packages"][1]["name"] = "renamed-library"
        self.assertIn("renamed-library", cross_os.plan(self.metadata, POLICY))

    def test_external_package_targets_do_not_become_workspace_roots(self):
        self.assertNotIn("external", cross_os.plan(self.metadata, POLICY))

    def test_runtime_static_and_rust_libraries_use_the_library_selector(self):
        self.metadata["packages"][0]["targets"] = [{"name": "phase", "kind": ["staticlib", "rlib"], "test": True}]
        self.assertIn("--lib", cross_os.plan(self.metadata, POLICY))

    def test_explicit_test_examples_keep_their_own_selector(self):
        self.metadata["packages"][0]["targets"].append(target("example", "example"))
        self.assertIn("--example", cross_os.plan(self.metadata, POLICY))

    def test_disabled_targets_are_not_explicitly_requested(self):
        self.metadata["packages"][0]["targets"].append(target("disabled", enabled=False))
        self.assertNotIn("disabled", cross_os.plan(self.metadata, POLICY))

    def test_excluded_property_binaries_do_not_need_compiling(self):
        self.metadata["packages"][0]["targets"].append(target("properties"))
        self.assertNotIn("properties", cross_os.plan(self.metadata, POLICY))

    def test_benchmarks_do_not_enter_the_default_test_build(self):
        self.metadata["packages"][0]["targets"].append(target("benchmark", "bench"))
        self.assertNotIn("benchmark", cross_os.plan(self.metadata, POLICY))

    def test_unknown_package_fails_closed(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unknown cross-os packages"):
            cross_os.plan(self.metadata, "(package(missing)) - (test(/proptest/))")

    def test_unknown_binary_fails_closed(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unknown cross-os binaries"):
            cross_os.plan(self.metadata, "(binary(missing)) - (test(/proptest/))")

    def test_dependency_only_binary_is_not_a_valid_workspace_selection(self):
        self.metadata["packages"][1]["targets"] = [target("shared", "lib")]
        with self.assertRaisesRegex(cross_os.SelectionError, "unknown cross-os binaries"):
            cross_os.plan(self.metadata, POLICY)

    def test_unrecognized_target_kind_fails_closed(self):
        self.metadata["packages"][0]["targets"].append(target("future", "future-kind"))
        with self.assertRaisesRegex(cross_os.SelectionError, "unsupported Cargo target"):
            cross_os.plan(self.metadata, POLICY)

    def test_regex_package_selectors_require_an_explicit_planner_update(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unsupported cross-os filter"):
            cross_os.plan(self.metadata, "(package(/phase/)) - (test(/proptest/))")

    def test_conjunctions_cannot_be_mistaken_for_a_union(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unsupported cross-os filter"):
            cross_os.plan(self.metadata, "(package(phase) & binary(native)) - (test(/proptest/))")

    def test_additional_union_after_subtraction_cannot_hide_a_selected_package(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unsupported cross-os subtraction"):
            cross_os.plan(self.metadata, "(package(phase)) - (test(/proptest/)) | package(shared)")

    def test_unrecognized_subtraction_fails_closed(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "unsupported cross-os subtraction"):
            cross_os.plan(self.metadata, "(package(phase)) - (package(shared))")

    def test_unparenthesized_policy_is_rejected(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "parenthesized"):
            cross_os.plan(self.metadata, "package(phase)")

    def test_empty_policy_is_rejected(self):
        with self.assertRaises(cross_os.SelectionError):
            cross_os.plan(self.metadata, "() - (test(/proptest/))")

    def test_empty_test_build_is_rejected(self):
        self.metadata["packages"][0]["targets"] = []
        with self.assertRaisesRegex(cross_os.SelectionError, "no Cargo test targets"):
            cross_os.plan(self.metadata, "(package(phase)) - (test(/proptest/))")

    def test_selection_is_deterministic_when_cargo_metadata_is_reordered(self):
        other = copy.deepcopy(self.metadata)
        other["packages"].reverse()
        for package in other["packages"]:
            package["targets"].reverse()
        self.assertEqual(cross_os.plan(self.metadata, POLICY), cross_os.plan(other, POLICY))

    def test_run_keeps_the_same_optimized_and_cross_os_profiles(self):
        self.assertEqual(cross_os.command("run", ["--package", "phase"]), [
            "cargo", "nextest", "run", "--package", "phase", "--locked",
            "--cargo-profile", "ci", "--profile", "cross-os"])


class CoverageTests(unittest.TestCase):
    def test_same_tests_in_a_different_order_preserve_coverage(self):
        full = inventory(("phase", "a", "matches"), ("shared::native", "b", "matches"))
        narrow = inventory(("shared::native", "b", "matches"), ("phase", "a", "matches"))
        self.assertEqual(cross_os.compare(full, narrow), {"tests": 2, "binaries": 2})

    def test_omitting_an_entire_selected_binary_fails(self):
        full = inventory(("phase", "a", "matches"), ("shared::native", "b", "matches"))
        with self.assertRaisesRegex(cross_os.SelectionError, "missing"):
            cross_os.compare(full, inventory(("phase", "a", "matches")))

    def test_omitting_one_test_in_a_retained_binary_fails(self):
        full = inventory(("phase", "a", "matches"), ("phase", "b", "matches"))
        with self.assertRaisesRegex(cross_os.SelectionError, "missing"):
            cross_os.compare(full, inventory(("phase", "a", "matches")))

    def test_equal_counts_with_different_test_identities_fail(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "coverage changed"):
            cross_os.compare(inventory(("phase", "a", "matches")), inventory(("phase", "b", "matches")))

    def test_extra_tests_fail_the_equivalence_guard(self):
        full = inventory(("phase", "a", "matches"))
        narrow = inventory(("phase", "a", "matches"), ("phase", "b", "matches"))
        with self.assertRaisesRegex(cross_os.SelectionError, "extra"):
            cross_os.compare(full, narrow)

    def test_filtered_properties_do_not_count_as_lost_tests(self):
        full = inventory(("phase", "a", "matches"), ("phase", "proptests::b", "mismatch"))
        self.assertEqual(cross_os.compare(full, inventory(("phase", "a", "matches")))["tests"], 1)

    def test_two_empty_inventories_cannot_pass_the_guard(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "selected no tests"):
            cross_os.compare(inventory(), inventory())

    def test_unknown_nextest_filter_status_fails_closed(self):
        with self.assertRaisesRegex(cross_os.SelectionError, "filter status"):
            cross_os.matched_tests(inventory(("phase", "a", "unknown")))


class DriverTests(unittest.TestCase):
    def test_planning_reads_cargo_metadata_without_building_tests(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".config").mkdir()
            (root / ".config/nextest.toml").write_text(f"[profile.cross-os]\ndefault-filter = '{POLICY}'\n")
            with patch.object(cross_os.subprocess, "check_output", return_value=json.dumps(metadata())) as command:
                selection = cross_os.cargo_args(root, {"FAI_BUILD_PROFILE": "ci"})
            self.assertEqual(selection, cross_os.plan(metadata(), POLICY))
            command.assert_called_once_with(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
                                            cwd=root, env={"FAI_BUILD_PROFILE": "ci"}, text=True)

    def test_verification_compares_full_and_narrowed_actual_test_inventories(self):
        tests = inventory(("phase", "a", "matches"))
        with patch.object(cross_os, "cargo_args", return_value=["--package", "phase"]), \
                patch.object(cross_os.subprocess, "check_output", side_effect=[json.dumps(tests), json.dumps(tests)]) as calls, \
                redirect_stdout(io.StringIO()):
            self.assertEqual(cross_os.verify(Path("workspace")), {"tests": 1, "binaries": 1})
        self.assertIn("--workspace", calls.call_args_list[0].args[0])
        self.assertNotIn("--workspace", calls.call_args_list[1].args[0])
        self.assertIn("--package", calls.call_args_list[1].args[0])
