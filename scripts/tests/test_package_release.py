"""Prepared package metadata, native evidence, and atomic publication boundaries."""

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import package_release
import release


class PackageFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "source"
        self.root.mkdir()
        self.remote = Path(self.temporary.name) / "remote.git"
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Package Test")
        self.git("config", "user.email", "packages@example.invalid")
        subprocess.run(["git", "init", "--bare", "-q", str(self.remote)], check=True)
        self.git("remote", "add", "origin", str(self.remote))
        self.write("scripts/release-targets.json", json.dumps({"schemaVersion": 1, "targets": [],
                                                            "repository": "Krzysztof-Cieslak/fai"}))
        self.write("packages/fai.toml", 'manifest-version = 1\n[workspace]\nmembers = ["alpha"]\n')
        self.write("packages/alpha/fai.toml", 'manifest-version = 1\n[package]\nname = "alpha"\nversion = "0.1.0"\nfai-version = "^0.1.0"\n')
        self.write("packages/alpha/README.md", "# Alpha\n\nA library.\n")
        self.write("packages/alpha/changelog.md", "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-10-15\n\n- Initial release.\n")
        self.commit()
        self.git("push", "-q", "-u", "origin", "main")
        self.value = package_release.request("all", "0.1.0", root=self.root)

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def git(self, *arguments):
        return release.git(*arguments, root=self.root)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-q", "-m", "Prepare package source")

    def reports(self):
        result = []
        for target in package_release.TARGETS:
            plan = {"schemaVersion": 1, "ok": True, "status": "planned", "commit": self.value["commit"],
                    "tree": self.value["tree"], "validatedHost": target, "remote": "origin", "branch": "main",
                    "packages": [{key: item[key] for key in ("name", "path", "version", "tag")}
                                 for item in self.value["packages"]]}
            result.append({"schemaVersion": 1, "request": copy.deepcopy(self.value), "plan": plan,
                           "compiler": {"version": "0.1.0", "profile": "release", "debugAssertions": False,
                                        "sourceId": "a" * 64, "target": target}})
        return result


class PreparedMetadataTests(PackageFixture):
    def test_all_selects_prepared_versions_and_notes(self):
        self.assertEqual(self.value["packages"][0]["tag"], "packages/alpha/v0.1.0")
        self.assertEqual(self.value["packages"][0]["notes"], "- Initial release.\n")

    def test_explicit_exact_selection_matches_all(self):
        self.assertEqual(package_release.prepared_packages('{"alpha":"0.1.0"}', self.root), self.value["packages"])

    def test_unknown_package_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "unknown packages"):
            package_release.prepared_packages('{"other":"0.1.0"}', self.root)

    def test_duplicate_selection_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "duplicate"):
            package_release.prepared_packages('{"alpha":"0.1.0","alpha":"0.1.0"}', self.root)

    def test_empty_selection_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "nonempty"):
            package_release.prepared_packages('{}', self.root)

    def test_non_object_selection_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "JSON object"):
            package_release.prepared_packages('["alpha"]', self.root)

    def test_unprepared_version_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "pull request"):
            package_release.prepared_packages('{"alpha":"0.2.0"}', self.root)

    def test_bump_intent_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "exact compiler version"):
            package_release.prepared_packages('{"alpha":"minor"}', self.root)

    def test_missing_readme_is_rejected(self):
        (self.root / "packages/alpha/README.md").unlink()
        with self.assertRaisesRegex(release.ReleaseError, "README"):
            package_release.prepared_packages("all", self.root)

    def test_missing_changelog_is_rejected(self):
        (self.root / "packages/alpha/changelog.md").unlink()
        with self.assertRaisesRegex(release.ReleaseError, "changelog"):
            package_release.prepared_packages("all", self.root)

    def test_unreleased_entries_are_rejected(self):
        self.write("packages/alpha/changelog.md", "## [Unreleased]\n- Pending.\n\n## [0.1.0] - 2026-10-15\n- Initial.\n")
        with self.assertRaisesRegex(release.ReleaseError, "Unreleased"):
            package_release.prepared_packages("all", self.root)

    def test_empty_release_notes_are_rejected(self):
        self.write("packages/alpha/changelog.md", "## [Unreleased]\n\n## [0.1.0] - 2026-10-15\n")
        with self.assertRaisesRegex(release.ReleaseError, "nonempty release"):
            package_release.prepared_packages("all", self.root)

    def test_wildcard_compiler_requirement_is_rejected(self):
        path = self.root / "packages/alpha/fai.toml"
        path.write_text(path.read_text().replace('"^0.1.0"', '"*"'))
        with self.assertRaisesRegex(release.ReleaseError, "compiler requirement"):
            package_release.prepared_packages("all", self.root)

    def test_dirty_source_cannot_be_prepared(self):
        self.write("packages/alpha/README.md", "changed")
        with self.assertRaisesRegex(release.ReleaseError, "clean committed"):
            package_release.request("all", "0.1.0", root=self.root)

    def test_unmerged_source_is_rejected(self):
        self.write("packages/alpha/README.md", "a committed change")
        self.commit()
        with mock.patch.object(release, "require_repository"), self.assertRaisesRegex(release.ReleaseError, "already merged"):
            package_release.require_merged(self.git("rev-parse", "HEAD"), self.root)

    def test_modified_request_is_rejected(self):
        value = copy.deepcopy(self.value)
        value["packages"][0]["notes"] = "different notes"
        with self.assertRaisesRegex(release.ReleaseError, "no longer matches"):
            package_release.verify_request(value, self.root)


class QualificationTests(PackageFixture):
    def test_matching_three_host_evidence_is_accepted(self):
        package_release.verify_reports(self.value, self.reports())

    def test_missing_host_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "all three"):
            package_release.verify_reports(self.value, self.reports()[:-1])

    def test_duplicate_host_is_rejected(self):
        reports = self.reports()
        with self.assertRaisesRegex(release.ReleaseError, "duplicate"):
            package_release.verify_reports(self.value, reports + [reports[0]])

    def test_different_compiler_sources_are_rejected(self):
        reports = self.reports()
        reports[0]["compiler"]["sourceId"] = "b" * 64
        with self.assertRaisesRegex(release.ReleaseError, "same compiler"):
            package_release.verify_reports(self.value, reports)

    def test_wrong_compiler_version_is_rejected(self):
        reports = self.reports()
        reports[0]["compiler"]["version"] = "0.0.1"
        with self.assertRaisesRegex(release.ReleaseError, "requested optimized"):
            package_release.verify_reports(self.value, reports)

    def test_debug_compiler_is_rejected(self):
        reports = self.reports()
        reports[0]["compiler"]["debugAssertions"] = True
        with self.assertRaisesRegex(release.ReleaseError, "requested optimized"):
            package_release.verify_reports(self.value, reports)

    def test_report_from_another_request_is_rejected(self):
        reports = self.reports()
        reports[0]["request"]["compilerVersion"] = "0.2.0"
        with self.assertRaisesRegex(release.ReleaseError, "different release request"):
            package_release.verify_reports(self.value, reports)

    def test_plan_cannot_create_a_new_commit(self):
        reports = self.reports()
        reports[0]["plan"]["commit"] = "b" * 40
        with self.assertRaisesRegex(release.ReleaseError, "plan disagrees"):
            package_release.verify_reports(self.value, reports)

    def test_plan_cannot_change_destination_branch(self):
        reports = self.reports()
        reports[0]["plan"]["branch"] = "another"
        with self.assertRaisesRegex(release.ReleaseError, "plan disagrees"):
            package_release.verify_reports(self.value, reports)

    def test_failed_plan_is_rejected(self):
        reports = self.reports()
        reports[0]["plan"]["ok"] = False
        with self.assertRaisesRegex(release.ReleaseError, "plan disagrees"):
            package_release.verify_reports(self.value, reports)

    def test_plan_cannot_omit_a_package(self):
        reports = self.reports()
        reports[0]["plan"]["packages"] = []
        with self.assertRaisesRegex(release.ReleaseError, "plan disagrees"):
            package_release.verify_reports(self.value, reports)


class InstalledCompilerTests(PackageFixture):
    def setUp(self):
        super().setUp()
        self.info = {"version": "0.1.0", "profile": "release", "debugAssertions": False,
                     "sourceId": "a" * 64, "target": package_release.TARGETS[0]}
        self.actual = copy.deepcopy(self.info)
        self.real_run = release.run
        self.real_process_run = subprocess.run
        self.installations = []

    def installer(self, command, **kwargs):
        if command[0] in {"sh", "powershell.exe"}:
            self.installations.append(command)
            return subprocess.CompletedProcess(command, 0)
        return self.real_process_run(command, **kwargs)

    def command(self, *arguments, **kwargs):
        if arguments[:3] == ("gh", "release", "download"):
            destination = Path(arguments[arguments.index("--dir") + 1])
            package_release.write_json(destination / "release.json", {"targets": {self.info["target"]: {"buildInfo": self.info}}})
            return ""
        if arguments[-1] == "build-info":
            return json.dumps(self.actual)
        return self.real_run(*arguments, **kwargs)

    def test_installs_only_after_verifying_the_exact_published_release(self):
        with mock.patch.object(release, "run", side_effect=self.command), \
             mock.patch.object(release, "verify") as verify, \
             mock.patch.object(subprocess, "run", side_effect=self.installer):
            _, environment, info = package_release.install_compiler(self.value, self.root.parent / "compiler", self.root)
        self.assertEqual(verify.call_args.args[0], "0.1.0")
        self.assertEqual(info, self.info)
        self.assertEqual(Path(environment["FAI_HOME"]), self.root.parent / "compiler")
        self.assertIn("0.1.0", self.installations[0])

    def test_unverified_release_never_executes_installer(self):
        with mock.patch.object(release, "run", side_effect=self.command), \
             mock.patch.object(release, "verify", side_effect=release.ReleaseError("invalid published checksum")), \
             mock.patch.object(subprocess, "run", side_effect=self.installer), \
             self.assertRaisesRegex(release.ReleaseError, "checksum"):
            package_release.install_compiler(self.value, self.root.parent / "compiler", self.root)
        self.assertEqual(self.installations, [])

    def test_installed_build_must_match_published_build(self):
        self.actual["sourceId"] = "b" * 64
        with mock.patch.object(release, "run", side_effect=self.command), \
             mock.patch.object(release, "verify"), \
             mock.patch.object(subprocess, "run", side_effect=self.installer), \
             self.assertRaisesRegex(release.ReleaseError, "exact published build"):
            package_release.install_compiler(self.value, self.root.parent / "compiler", self.root)


class ValidationInvocationTests(PackageFixture):
    def setUp(self):
        super().setUp()
        self.plan = self.reports()[0]["plan"]
        self.info = self.reports()[0]["compiler"]
        self.output = self.root.parent / "qualification.json"
        self.real_process_run = subprocess.run
        self.status = 0
        self.change_source = False
        self.commands = []

    def execute(self, command, **kwargs):
        if command[0] == "published-fai":
            self.commands.append(command)
            if self.change_source:
                self.write("packages/alpha/README.md", "changed by a test")
            return subprocess.CompletedProcess(command, self.status, json.dumps(self.plan), "located validation error")
        return self.real_process_run(command, **kwargs)

    def validate(self):
        with mock.patch.object(package_release, "require_merged"), \
             mock.patch.object(package_release, "install_compiler", return_value=(Path("published-fai"), {}, self.info)), \
             mock.patch.object(subprocess, "run", side_effect=self.execute):
            return package_release.validate(self.value, self.root.parent / "compiler", self.output, self.root)

    def test_validation_uses_exact_compiler_and_read_only_publisher(self):
        result = self.validate()
        self.assertEqual(result["request"], self.value)
        self.assertEqual(self.commands[0][1], "+0.1.0")
        self.assertIn("--plan", self.commands[0])
        self.assertTrue(self.output.is_file())
        self.assertEqual(self.git("tag"), "")

    def test_failed_validation_preserves_compiler_diagnostics(self):
        self.status = 3
        with self.assertRaisesRegex(release.ReleaseError, "located validation error"):
            self.validate()
        self.assertFalse(self.output.exists())

    def test_validation_cannot_publish_evidence_after_a_source_edit(self):
        self.change_source = True
        with self.assertRaisesRegex(release.ReleaseError, "clean committed"):
            self.validate()
        self.assertFalse(self.output.exists())


class PublicationTests(PackageFixture):
    def setUp(self):
        super().setUp()
        self.github = {}
        self.commands = []
        self.output = Path(self.temporary.name) / "publication.json"
        self.real_run = release.run

    def command(self, *arguments, **kwargs):
        self.commands.append(arguments)
        if arguments[:3] == ("gh", "release", "create"):
            tag = arguments[3]
            self.github[tag] = {"draft": False, "immutable": True, "prerelease": "--prerelease" in arguments,
                                "body": Path(arguments[arguments.index("--notes-file") + 1]).read_text(),
                                "html_url": "https://example.invalid/" + tag}
            return ""
        return self.real_run(*arguments, **kwargs)

    def perform(self):
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main"}), \
             mock.patch.object(release, "require_repository"), \
             mock.patch.object(release, "run", side_effect=self.command), \
             mock.patch.object(release, "github_release", side_effect=lambda tag, *args, **kwargs: self.github.get(tag)):
            return package_release.publish(self.value, self.reports(), self.output, self.root)

    def test_publication_pushes_annotated_tags_without_moving_main(self):
        before = self.git("rev-parse", "origin/main")
        result = self.perform()
        self.assertEqual(self.git("rev-parse", "origin/main"), before)
        self.assertEqual(release.remote_tag("packages/alpha/v0.1.0", self.root), before)
        self.assertEqual(result["commit"], before)
        self.assertTrue(self.output.is_file())
        create = next(command for command in self.commands if command[:3] == ("gh", "release", "create"))
        self.assertIn("--latest=false", create)

    def test_retry_keeps_existing_release_and_tag(self):
        self.perform()
        before = self.git("rev-parse", "refs/tags/packages/alpha/v0.1.0")
        self.commands.clear()
        self.perform()
        self.assertEqual(self.git("rev-parse", "refs/tags/packages/alpha/v0.1.0"), before)
        self.assertFalse(any(command[:3] == ("gh", "release", "create") for command in self.commands))

    def test_conflicting_remote_tag_is_not_moved(self):
        self.git("tag", "-a", "packages/alpha/v0.1.0", "-m", "prior release")
        self.git("push", "-q", "origin", "refs/tags/packages/alpha/v0.1.0")
        previous = self.value["commit"]
        self.write("packages/alpha/README.md", "new source")
        self.commit()
        self.git("push", "-q", "origin", "main")
        self.value = package_release.request("all", "0.1.0", root=self.root)
        with self.assertRaisesRegex(release.ReleaseError, "different commit"):
            self.perform()
        self.assertEqual(release.remote_tag("packages/alpha/v0.1.0", self.root), previous)

    def test_unqualified_publication_is_rejected(self):
        with mock.patch.dict(os.environ, {}, clear=True), self.assertRaisesRegex(release.ReleaseError, "manually dispatched"):
            package_release.publish(self.value, self.reports(), self.output, self.root)

    def test_missing_host_prevents_any_tag_publication(self):
        with mock.patch.object(self, "reports", return_value=[]), \
             self.assertRaisesRegex(release.ReleaseError, "all three"):
            self.perform()
        self.assertEqual(self.git("tag"), "")
        self.assertEqual(self.github, {})

    def test_conflicting_release_notes_are_not_overwritten(self):
        self.perform()
        self.github["packages/alpha/v0.1.0"]["body"] = "different notes"
        with self.assertRaisesRegex(release.ReleaseError, "conflicts"):
            self.perform()


class RepositoryReleaseTests(unittest.TestCase):
    def test_every_source_package_has_prepared_documentation(self):
        selected = package_release.prepared_packages("all")
        self.assertEqual(len(selected), len(package_release.packages.discover()))
        self.assertTrue(all(item["version"] in (package_release.ROOT / item["path"] / "README.md").read_text()
                            for item in selected))

    def test_workflow_is_manual_and_requires_all_native_reports(self):
        workflow = (package_release.ROOT / ".github/workflows/package-release.yml").read_text()
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("  push:", workflow)
        self.assertNotIn("  schedule:", workflow)
        self.assertIn("needs: [prepare, validate]", workflow)
        self.assertIn("default: false", workflow)
        self.assertIn("steps.source.outputs.commit", workflow)


if __name__ == "__main__":
    unittest.main()
