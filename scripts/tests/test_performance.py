"""Snapshot retention and transactional publication boundaries without network I/O."""

import copy
import gzip
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import performance
import performance_publish as publishing


class FakeGithub:
    def __init__(self):
        self.issues = {10: {"number": 10, "state": "open", "title": "Previous tracker", "body": "history"}}
        self.comments = {}
        self.calls = []
        self.fail_update = False
        self.gist = None

    def request(self, path, payload=None, method=None):
        self.calls.append((path, copy.deepcopy(payload), method))
        if path == "gists":
            self.gist = {"id": "archive", "html_url": "https://gist.github.com/archive", "history": [{"version": "version"}], "files": payload["files"]}
            return copy.deepcopy(self.gist)
        if path == "gists/archive":
            if payload:
                self.gist["files"].update(payload["files"])
                self.gist["history"].insert(0, {"version": "updated"})
            return copy.deepcopy(self.gist)
        if path.endswith("/comments"):
            number = int(path.split("/")[-2])
            self.comments.setdefault(number, []).append(payload)
            return copy.deepcopy(payload)
        if path.endswith("/issues"):
            number = max(self.issues) + 1
            self.issues[number] = {"number": number, "state": "open", **payload}
            return copy.deepcopy(self.issues[number])
        number = int(path.rsplit("/", 1)[1])
        if payload:
            if self.fail_update and number != 10:
                self.fail_update = False
                raise RuntimeError("interrupted publication")
            self.issues[number].update(payload)
        return copy.deepcopy(self.issues[number])

    def pages(self, path):
        if path.endswith("/comments"):
            return iter(copy.deepcopy(self.comments.get(int(path.split("/")[-2]), [])))
        return iter(copy.deepcopy([row for row in self.issues.values() if row["state"] == "open"]))

    def verify_archive(self, identity, files):
        assert all(self.gist["files"][name] == value for name, value in files.items())


class GithubRequestTests(unittest.TestCase):
    def test_error_response_retains_the_github_validation_details(self):
        failure = subprocess.CalledProcessError(1, ["gh", "api"], output='{"field":"files","code":"missing_field"}')
        with patch("performance_publish.subprocess.check_output", side_effect=failure):
            with self.assertRaisesRegex(RuntimeError, '"code":"missing_field"'):
                publishing.Github().request("gists", {"files": {}})


class PerformanceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.config = {"schemaVersion": 1, "repository": "example/fai", "labels": ["performance"],
                       "trackers": {role: {"issue": None, "title": role, "report": name} for role, name in zip(publishing.ROLES, ["perceus-memory.md", "language-memory.md", "language-runtime.md"])},
                       "supersededIssues": [10]}
        self.config_path = self.root / "config.json"
        performance.write_json(self.config_path, self.config)
        performance.write_json(self.root / "snapshot.json", {"status": "complete", "repository": "example/fai", "runId": "run-one", "measuredRevision": "a" * 40})
        (self.root / "reports").mkdir()
        for role, value in self.config["trackers"].items():
            (self.root / "reports" / value["report"]).write_text(publishing.issue_marker(role) + "\nSnapshot run-one\n")
        (self.root / "reports/results.json").write_text("{}")
        self.args = SimpleNamespace(create=True, migrate=True, record_ids=False, config=self.config_path)
        self.github = FakeGithub()
        self.addCleanup(patch.stopall)
        patch("performance_publish.Github", return_value=self.github).start()

    def publish(self, render=lambda *_: None):
        publishing.publish(self.root, self.config, self.args, render)

    def test_failed_validation_performs_no_github_operations(self):
        def reject(*_):
            raise ValueError("missing observation")
        with self.assertRaises(ValueError):
            self.publish(reject)
        self.assertEqual(self.github.calls, [])

    def test_old_tracker_closes_only_after_three_new_reports_are_verified(self):
        self.publish()
        self.assertEqual(self.github.issues[10]["state"], "closed")
        closure = next(i for i, (_, payload, _) in enumerate(self.github.calls) if payload and payload.get("state") == "closed")
        for number in [11, 12, 13]:
            reads = [i for i, (path, payload, _) in enumerate(self.github.calls) if path.endswith(f"/issues/{number}") and payload is None]
            self.assertTrue(reads and max(reads) < closure)
            self.assertIn("Related trackers", self.github.issues[number]["body"])

    def test_retry_does_not_create_duplicate_trackers_or_refresh_comments(self):
        self.publish()
        self.publish()
        self.assertEqual(set(self.github.issues), {10, 11, 12, 13})
        self.assertEqual([len(self.github.comments[n]) for n in [10, 11, 12, 13]], [1, 1, 1, 1])

    def test_partial_publication_keeps_previous_tracker_open(self):
        self.github.fail_update = True
        with self.assertRaises(RuntimeError):
            self.publish()
        self.assertEqual(self.github.issues[10]["state"], "open")
        self.publish()
        self.assertEqual(set(self.github.issues), {10, 11, 12, 13})
        self.assertEqual(self.github.issues[10]["state"], "closed")

    def test_published_snapshot_cannot_be_changed_in_place(self):
        self.publish()
        value = performance.read_json(self.root / "snapshot.json")
        value["measuredRevision"] = "b" * 40
        performance.write_json(self.root / "snapshot.json", value)
        with self.assertRaisesRegex(ValueError, "immutable"):
            self.publish()

    def test_archive_verification_failure_resumes_the_same_archive(self):
        with patch.object(self.github, "verify_archive", side_effect=RuntimeError("interrupted read")):
            with self.assertRaises(RuntimeError):
                self.publish()
        self.publish()
        self.assertEqual(sum(path == "gists" for path, _, _ in self.github.calls), 1)
        self.assertEqual(self.github.issues[10]["state"], "closed")

    def test_later_migration_links_the_revision_containing_old_issue_bodies(self):
        self.args.migrate = False
        self.publish()
        self.assertEqual(self.github.issues[10]["state"], "open")
        self.args.migrate = True
        self.publish()
        receipt = performance.read_json(self.root / "publication.json")
        self.assertEqual(receipt["archiveUrl"], "https://gist.github.com/archive/updated")
        self.assertIn(receipt["archiveUrl"], self.github.comments[10][0]["body"])

    def test_blank_logs_are_archived_exactly_without_invalid_gist_files(self):
        (self.root / "raw").mkdir()
        (self.root / "raw/empty.stderr").write_text("")
        (self.root / "raw/whitespace.stdout").write_text("\n\t ")
        self.publish()
        files = self.github.gist["files"]
        self.assertTrue(all(value["content"].strip() for value in files.values()))
        self.assertEqual(json.loads(files["blank-files.json"]["content"]), {
            "raw-empty.stderr": "", "raw-whitespace.stdout": "\n\t "})

    def test_executable_compression_is_lossless_and_keeps_identity_mapping(self):
        folder = self.root / "artifacts"
        folder.mkdir()
        data = b"\x7fELF" + bytes(range(256)) * 20
        path = folder / "worker"
        path.write_bytes(data)
        state = {"compressed": {}}
        performance.compress_artifacts(self.root, folder, state)
        self.assertFalse(path.exists())
        self.assertEqual(gzip.decompress((folder / "worker.gz").read_bytes()), data)
        self.assertEqual(state["compressed"], {"artifacts/worker": "artifacts/worker.gz"})

    def test_changed_source_revision_cannot_resume(self):
        manifest = {"environment": {}, "measuredRevision": "a" * 40, "repository": "example/fai", "toolchains": {"ocamlExecutable": "ocamlopt"}}
        with patch("performance.output", return_value="b" * 40):
            with self.assertRaisesRegex(ValueError, "pinned revision"):
                performance.environment(manifest, self.root)

    def test_identical_executable_archives_share_storage_without_changing_contents(self):
        folder = self.root / "artifacts"
        folder.mkdir()
        data = b"\x7fELF" + b"retained executable" * 100
        (folder / "first").write_bytes(data)
        (folder / "second").write_bytes(data)
        state = {"compressed": {}}
        performance.compress_artifacts(self.root, folder, state)
        first, second = folder / "first.gz", folder / "second.gz"
        self.assertEqual(gzip.decompress(first.read_bytes()), data)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        self.assertEqual(first.stat().st_ino, second.stat().st_ino)
        self.assertEqual(len(state["compressed"]), 2)


if __name__ == "__main__":
    unittest.main()
