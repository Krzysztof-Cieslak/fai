"""Semantic workspace metadata selection, including shared lock records."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci
import ci_workspace
import packages


TARGETS = ("aarch64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu")
MANIFEST = "packages/fai.toml"
LOCK = "packages/fai.lock"


def descriptor(name):
    return {"id": name, "version": "0.1.0", "logical": {"path": name},
            "source": {"path": name}, "manifest-fingerprint": "original"}


def graph(name, target, dependencies=()):
    return {"id": f"{name}-{target}", "root": name, "target": target, "root-package": name,
            "input-fingerprint": "original", "resolved-with": "0.0.1",
            "packages": [name, *dependencies], "branches": [],
            "edges": [{"from": name, "to": dependency, "alias": dependency, "kind": "normal"}
                      for dependency in dependencies]}


def override(name, **updates):
    return {"replace": {"git": "https://github.com/Krzysztof-Cieslak/fai.git", "subdir": f"packages/{name}"},
            "with": {"path": name}, **updates}


class WorkspaceImpactTests(unittest.TestCase):
    def setUp(self):
        self.previous = {
            "json": packages.Package("json", Path("packages/json"), ()),
            "web": packages.Package("web", Path("packages/web"), ("json",)),
            "cli": packages.Package("cli", Path("packages/cli"), ()),
        }
        self.current = dict(self.previous)
        self.before = {
            MANIFEST: {"manifest-version": 1, "workspace": {"members": ["cli", "json", "web"]},
                       "overrides": [override("json"), override("cli")]},
            LOCK: {"lock-version": 1, "resolver-version": 1, "context": "shared",
                   "packages": [descriptor(name) for name in self.previous],
                   "graphs": [graph(name, target, package.dependencies)
                              for name, package in self.previous.items() for target in TARGETS]},
        }
        self.after = copy.deepcopy(self.before)

    def impact(self):
        return ci_workspace.affected(self.before, self.after, self.current, self.previous)

    def selected(self):
        result = ci.classify([MANIFEST, LOCK], self.current, self.previous, workspace_impact=self.impact())
        self.assertEqual((result["mode"], result["compiler"]), ("packages", False))
        return result["packages"]

    def web_graph(self):
        return next(item for item in self.after[LOCK]["graphs"] if item["root"] == "web")

    def test_new_yaml_package_and_platform_graphs_select_only_yaml(self):
        self.current["yaml"] = packages.Package("yaml", Path("packages/yaml"), ("json",))
        self.after[MANIFEST]["workspace"]["members"].append("yaml")
        self.after[MANIFEST]["overrides"].append(override("yaml"))
        self.after[LOCK]["packages"].append(descriptor("yaml"))
        self.after[LOCK]["graphs"].extend(graph("yaml", target, ("json",)) for target in TARGETS)
        self.assertEqual(self.selected(), ["yaml"])
        self.assertIn("workspace member: yaml", self.impact()[1])

    def test_membership_removal_retains_previous_dependents(self):
        self.after[MANIFEST]["workspace"]["members"].remove("json")
        del self.current["json"]
        self.current["web"] = packages.Package("web", Path("packages/web"), ())
        self.assertEqual(self.selected(), ["web"])

    def test_member_reordering_needs_only_workspace_validation(self):
        self.after[MANIFEST]["workspace"]["members"].reverse()
        self.assertEqual(self.selected(), [])

    def test_override_reordering_needs_only_workspace_validation(self):
        self.after[MANIFEST]["overrides"].reverse()
        self.assertEqual(self.selected(), [])

    def test_lock_record_reordering_needs_only_workspace_validation(self):
        self.after[LOCK]["packages"].reverse()
        self.after[LOCK]["graphs"].reverse()
        self.web_graph()["packages"].reverse()
        self.assertEqual(self.selected(), [])

    def test_changed_graph_selects_root_not_unchanged_dependencies(self):
        self.web_graph()["input-fingerprint"] = "changed"
        self.assertEqual(self.selected(), ["web"])

    def test_non_host_graph_changes_select_the_root(self):
        item = next(item for item in self.after[LOCK]["graphs"]
                    if item["root"] == "web" and item["target"] == "x86_64-pc-windows-msvc")
        item["input-fingerprint"] = "windows changed"
        self.assertEqual(self.selected(), ["web"])

    def test_removed_graph_selects_the_root(self):
        self.after[LOCK]["graphs"].remove(self.web_graph())
        self.assertEqual(self.selected(), ["web"])

    def test_new_target_graph_selects_the_root(self):
        self.after[LOCK]["graphs"].append(graph("web", "aarch64-unknown-linux-gnu", ("json",)))
        self.assertEqual(self.selected(), ["web"])

    def test_graph_alias_change_selects_the_root(self):
        self.web_graph()["edges"][0]["alias"] = "data"
        self.assertEqual(self.selected(), ["web"])

    def test_shared_descriptor_change_selects_every_referencing_root(self):
        self.after[LOCK]["packages"][0]["manifest-fingerprint"] = "changed"
        self.assertEqual(self.selected(), ["json", "web"])

    def test_external_source_change_selects_consumers_without_catalog_edges(self):
        external = {**descriptor("external"), "logical": {"git": "https://example.org/repo", "subdir": "lib"},
                    "source": {"git": "https://example.org/repo", "subdir": "lib", "rev": "a" * 40}}
        for documents in (self.before, self.after):
            documents[LOCK]["packages"].append(copy.deepcopy(external))
            for item in documents[LOCK]["graphs"]:
                if item["root"] in {"cli", "web"}:
                    item["packages"].append("external")
                    item["edges"].append({"from": item["root"], "to": "external", "alias": "ext", "kind": "normal"})
        self.after[LOCK]["packages"][-1]["source"]["rev"] = "b" * 40
        self.assertEqual(self.selected(), ["cli", "web"])

    def test_informational_compiler_release_does_not_select_tests(self):
        self.web_graph()["resolved-with"] = "0.0.2"
        self.assertEqual(self.selected(), [])

    def test_development_dependency_change_selects_root(self):
        self.web_graph()["edges"][0].update({"from": "$root", "kind": "development"})
        self.assertEqual(self.selected(), ["web"])

    def test_branch_binding_change_selects_root(self):
        self.web_graph()["branches"].append({"logical-package": "json", "branch": "main", "rev": "a" * 40})
        self.assertEqual(self.selected(), ["web"])

    def test_in_tree_override_change_selects_dependents(self):
        self.after[MANIFEST]["overrides"][0]["with"] = {"path": "other-json"}
        self.assertEqual(self.selected(), ["json", "web"])

    def test_override_removal_selects_previous_consumers(self):
        self.after[MANIFEST]["overrides"].pop(0)
        self.assertEqual(self.selected(), ["json", "web"])

    def test_scoped_external_override_selects_only_its_member(self):
        self.after[MANIFEST]["overrides"].append({
            "replace": {"git": "https://example.org/repo", "subdir": "lib"},
            "with": {"path": "replacement"}, "members": ["web"],
        })
        self.assertEqual(self.selected(), ["web"])

    def test_unknown_external_override_selects_all_packages(self):
        self.after[MANIFEST]["overrides"].append({
            "replace": {"git": "https://example.org/repo", "subdir": "lib"}, "with": {"path": "json"}})
        self.assertEqual(self.selected(), ["cli", "json", "web"])
        self.assertIn("unscoped external override", self.impact()[1][0])

    def test_lock_target_configuration_change_selects_all_packages(self):
        self.after[MANIFEST]["lock-targets"] = [TARGETS[0]]
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_compiler_pin_change_selects_all_packages(self):
        self.after[MANIFEST]["toolchain"] = {"version": "0.0.2"}
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_global_configuration_selects_all_packages(self):
        self.after[MANIFEST]["future-setting"] = True
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_lock_schema_selects_all_packages(self):
        self.after[LOCK]["lock-version"] = 2
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_graph_field_selects_all_packages(self):
        self.web_graph()["future-setting"] = True
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_package_field_selects_all_packages(self):
        self.after[LOCK]["packages"][0]["future-setting"] = True
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_missing_metadata_selects_all_packages(self):
        self.before[LOCK] = None
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_member_glob_selects_all_packages(self):
        self.after[MANIFEST]["workspace"]["members"] = ["*"]
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_graph_root_selects_all_packages(self):
        self.web_graph()["root"] = "unknown"
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_unknown_member_path_selects_all_packages(self):
        self.after[MANIFEST]["workspace"]["members"].append("unknown")
        self.assertEqual(self.selected(), ["cli", "json", "web"])

    def test_nested_paths_resolve_to_package_names(self):
        self.current["web-examples"] = packages.Package("web-examples", Path("packages/web/examples"), ("web",))
        self.after[MANIFEST]["workspace"]["members"].append("web/examples")
        self.assertEqual(self.selected(), ["web-examples"])

    def test_malformed_members_fail_selection(self):
        self.after[MANIFEST]["workspace"]["members"] = "web"
        with self.assertRaisesRegex(packages.PackageError, "array of strings"):
            self.impact()

    def test_duplicate_members_fail_selection(self):
        self.after[MANIFEST]["workspace"]["members"].append("web")
        with self.assertRaisesRegex(packages.PackageError, "duplicate workspace members"):
            self.impact()

    def test_duplicate_package_ids_fail_selection(self):
        self.after[LOCK]["packages"].append(descriptor("json"))
        with self.assertRaisesRegex(packages.PackageError, "duplicate lock package"):
            self.impact()

    def test_duplicate_graph_keys_fail_selection(self):
        self.after[LOCK]["graphs"].append({**self.web_graph(), "id": "another-id"})
        with self.assertRaisesRegex(packages.PackageError, "duplicate lock graph"):
            self.impact()

    def test_duplicate_graph_ids_fail_selection(self):
        self.web_graph()["id"] = self.after[LOCK]["graphs"][0]["id"]
        with self.assertRaisesRegex(packages.PackageError, "duplicate lock graph"):
            self.impact()

    def test_missing_package_record_fails_selection(self):
        self.web_graph()["packages"].append("absent")
        with self.assertRaisesRegex(packages.PackageError, "missing package records"):
            self.impact()

    def test_missing_root_record_fails_selection(self):
        self.web_graph()["root-package"] = "cli"
        with self.assertRaisesRegex(packages.PackageError, "missing package records"):
            self.impact()

    def test_duplicate_graph_members_fail_selection(self):
        self.web_graph()["packages"].append("web")
        with self.assertRaisesRegex(packages.PackageError, "duplicate lock graph packages"):
            self.impact()

    def test_missing_edge_target_fails_selection(self):
        self.web_graph()["edges"][0]["to"] = "cli"
        with self.assertRaisesRegex(packages.PackageError, "invalid lock graph edge"):
            self.impact()

    def test_malformed_edge_kind_fails_selection(self):
        self.web_graph()["edges"][0]["kind"] = {}
        with self.assertRaisesRegex(packages.PackageError, "lock edge kind"):
            self.impact()

    def test_missing_branch_package_fails_selection(self):
        self.web_graph()["branches"].append({"logical-package": "cli", "branch": "main", "rev": "a" * 40})
        with self.assertRaisesRegex(packages.PackageError, "missing package record"):
            self.impact()
