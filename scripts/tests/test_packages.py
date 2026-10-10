"""Package selection and fast-loop command contracts."""

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import packages


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.add_package("json", [])
        self.add_package("web", ["json"])
        self.add_package("app", ["web"])

    def add_package(self, name, dependencies):
        directory = self.root / "packages" / name
        (directory / "test").mkdir(parents=True, exist_ok=True)
        (directory / packages.MANIFEST).write_text(json.dumps({"schemaVersion": 1, "name": name,
                                                              "dependencies": dependencies}))

    def test_json_selects_transitive_dependents(self):
        self.assertEqual(packages.affected(packages.discover(self.root), ["json"]), ["json", "web", "app"])

    def test_web_does_not_select_its_dependency(self):
        self.assertEqual(packages.affected(packages.discover(self.root), ["web"]), ["web", "app"])

    def test_focused_selection_can_exclude_dependents(self):
        self.assertEqual(packages.affected(packages.discover(self.root), ["json"], False), ["json"])

    def test_missing_dependency_is_rejected(self):
        self.add_package("web", ["absent"])
        with self.assertRaisesRegex(packages.PackageError, "unknown package"):
            packages.discover(self.root)

    def test_cycle_is_rejected(self):
        self.add_package("json", ["app"])
        with self.assertRaisesRegex(packages.PackageError, "cycle"):
            packages.discover(self.root)

    def test_unregistered_source_package_is_rejected(self):
        directory = self.root / "packages/new"
        directory.mkdir()
        (directory / "New.fai").write_text("module New\n")
        with self.assertRaisesRegex(packages.PackageError, "missing"):
            packages.discover(self.root)

    def test_invalid_dependency_type_is_rejected(self):
        self.add_package("json", [False])
        with self.assertRaisesRegex(packages.PackageError, "dependencies"):
            packages.discover(self.root)

    def test_unknown_selection_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unknown packages"):
            packages.affected(packages.discover(self.root), ["unknown"])


if __name__ == "__main__":
    unittest.main()
