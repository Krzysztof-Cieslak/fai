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


class IntegrationMetadataTests(unittest.TestCase):
    def manifest(self, **updates):
        data = {"schemaVersion": 1, "name": "example", "dependencies": [],
                "integrations": [{"name": "runtime", "entry": "examples/Runtime.fai"}]}
        data.update(updates)
        return packages.parse(Path("packages/example"), json.dumps(data))

    def integration(self, **updates):
        entry = {"name": "runtime", "entry": "examples/Runtime.fai", **updates}
        return self.manifest(integrations=[entry]).integrations[0]

    def test_legacy_catalog_has_no_integrations(self):
        package = packages.parse(Path("packages/example"), '{"schemaVersion":1,"name":"example","dependencies":[]}')
        self.assertEqual((package.integrations, package.fixtures), ((), ()))

    def test_defaults_run_jit_and_aot_on_every_platform(self):
        integration = self.integration()
        self.assertEqual((integration.modes, integration.platforms, integration.timeout_seconds),
                         (("jit", "aot"), ("linux", "macos", "windows"), 180))

    def test_manifest_keeps_arguments_environment_and_expectations(self):
        integration = self.integration(args=["two words", "é🙂"], env={"ANSWER": "42"},
                                       stdoutLines=["passed"], timeoutSeconds=17,
                                       modes=["aot"], platforms=["windows"])
        self.assertEqual(integration, packages.Integration("runtime", "examples/Runtime.fai", ("two words", "é🙂"),
                         (("ANSWER", "42"),), ("aot",), ("windows",), 17, None, ("passed",)))

    def test_pinned_fixture_and_templates_are_retained(self):
        fixture = {"name": "corpus", "repository": "https://example.org/corpus.git", "revision": "A" * 40}
        package = self.manifest(fixtures=[fixture], integrations=[{
            "name": "corpus", "entry": "examples/Corpus.fai", "args": ["${fixture:corpus}/tests"],
            "env": {"BASE": "${workspace}", "SOURCE": "${package}", "OUTPUT": "${temp}"}}])
        self.assertEqual(package.fixtures, (packages.Fixture("corpus", fixture["repository"], "a" * 40),))

    def test_catalog_parsing_does_not_require_historical_entry_files(self):
        self.assertEqual(self.integration(entry="removed/Old.fai").entry, "removed/Old.fai")

    def test_unknown_manifest_field_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unknown package fields"):
            self.manifest(integration=[])

    def test_unknown_integration_field_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unknown integration fields"):
            self.integration(timeouts=15)

    def test_duplicate_json_keys_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "duplicate JSON field"):
            packages.parse(Path("packages/example"), '{"name":"example","name":"other"}')

    def test_boolean_schema_version_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "invalid name or schema"):
            self.manifest(schemaVersion=True)

    def test_duplicate_integration_names_are_rejected(self):
        entry = {"name": "runtime", "entry": "examples/Runtime.fai"}
        with self.assertRaisesRegex(packages.PackageError, "duplicate integrations"):
            self.manifest(integrations=[entry, entry])

    def test_parent_entry_path_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "relative .fai path"):
            self.integration(entry="../other/Run.fai")

    def test_absolute_entry_path_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "relative .fai path"):
            self.integration(entry="/tmp/Run.fai")

    def test_windows_drive_entry_is_rejected_on_every_host(self):
        with self.assertRaisesRegex(packages.PackageError, "relative .fai path"):
            self.integration(entry="C:/Run.fai")

    def test_backslash_entry_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "forward slashes"):
            self.integration(entry="examples\\Run.fai")

    def test_non_fai_entry_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "relative .fai path"):
            self.integration(entry="run.sh")

    def test_shell_string_arguments_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "args must be an array"):
            self.integration(args="--port 42")

    def test_nul_arguments_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "without NULs"):
            self.integration(args=["a\0b"])

    def test_invalid_unicode_argument_is_rejected(self):
        with self.assertRaises(packages.PackageError):
            self.integration(args=["\ud800"])

    def test_invalid_unicode_entry_is_rejected(self):
        with self.assertRaises(packages.PackageError):
            self.integration(entry="examples/\ud800.fai")

    def test_nonstring_environment_values_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "env must map"):
            self.integration(env={"PORT": 42})

    def test_environment_names_cannot_collide_on_windows(self):
        with self.assertRaisesRegex(packages.PackageError, "ignoring case"):
            self.integration(env={"PATH": "a", "Path": "b"})

    def test_empty_modes_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "integration modes"):
            self.integration(modes=[])

    def test_unknown_platform_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "integration platforms"):
            self.integration(platforms=["darwin"])

    def test_zero_timeout_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "timeoutSeconds"):
            self.integration(timeoutSeconds=0)

    def test_boolean_timeout_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "timeoutSeconds"):
            self.integration(timeoutSeconds=True)

    def test_two_output_policies_are_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "not both"):
            self.integration(stdout="ok\n", stdoutLines=["ok"])

    def test_multiline_line_expectation_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "individual lines"):
            self.integration(stdoutLines=["first\nsecond"])

    def test_fixture_revision_must_be_a_commit_hash(self):
        with self.assertRaisesRegex(packages.PackageError, "full Git commit hash"):
            self.manifest(fixtures=[{"name": "corpus", "repository": "https://example.org/data.git", "revision": "main"}])

    def test_fixture_url_cannot_embed_credentials(self):
        with self.assertRaisesRegex(packages.PackageError, "without credentials"):
            self.manifest(fixtures=[{"name": "corpus", "repository": "https://token@example.org/data.git", "revision": "a" * 40}])

    def test_duplicate_fixture_names_are_rejected(self):
        fixture = {"name": "corpus", "repository": "https://example.org/data.git", "revision": "a" * 40}
        with self.assertRaisesRegex(packages.PackageError, "duplicate fixtures"):
            self.manifest(fixtures=[fixture, fixture])

    def test_undeclared_fixture_template_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unknown integration template"):
            self.integration(args=["${fixture:missing}/tests"])

    def test_unterminated_template_is_rejected(self):
        with self.assertRaisesRegex(packages.PackageError, "unterminated integration template"):
            self.integration(args=["${fixture:missing"])


if __name__ == "__main__":
    unittest.main()
