"""Release archive smoke tests retain the compiler's original build identity."""

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import warnings
import zipfile

import tooling_packaging


class ToolingPackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / ("fai.exe" if os.name == "nt" else "fai")
        self.binary.write_bytes(b"existing compiler bytes")
        (self.root / "LICENSE").write_bytes(b"license")
        self.info = {"profile": "ci", "debugAssertions": True, "sourceId": "retained"}

    def test_archives_round_trip_before_the_existing_compiler_is_exercised(self):
        observed = []
        make_archive = tooling_packaging.release.make_archive
        def archive(path, files):
            observed.append(json.loads(files["metadata.json"]))
            make_archive(path, files)
        def smoke(arguments, **kwargs):
            self.assertEqual(Path(arguments[-1]).read_bytes(), self.binary.read_bytes())
            self.assertIn("smoke_compiler.py", arguments[1])
        with patch.object(tooling_packaging.subprocess, "check_output", return_value=json.dumps(self.info)), \
                patch.object(tooling_packaging.release, "make_archive", side_effect=archive), \
                patch.object(tooling_packaging.subprocess, "run", side_effect=smoke) as run:
            tooling_packaging.run(self.binary, self.root)
        run.assert_called_once()
        self.assertEqual(observed, [{"kind": "ci-tooling-smoke", "buildInfo": self.info}] * 2)

    def test_changed_archive_payload_fails_before_native_execution(self):
        make_archive = tooling_packaging.release.make_archive
        def corrupt(path, files):
            make_archive(path, {name: value for name, value in files.items() if name != "LICENSE"})
        with patch.object(tooling_packaging.subprocess, "check_output", return_value=json.dumps(self.info)), \
                patch.object(tooling_packaging.release, "make_archive", side_effect=corrupt), \
                patch.object(tooling_packaging.subprocess, "run") as run:
            with self.assertRaisesRegex(ValueError, "changed compiler payloads"):
                tooling_packaging.run(self.binary, self.root)
        run.assert_not_called()

    def test_duplicate_archive_members_are_rejected(self):
        path = self.root / "bad.zip"
        with zipfile.ZipFile(path, "w") as archive, warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            archive.writestr("same", b"one")
            archive.writestr("same", b"two")
        with self.assertRaisesRegex(ValueError, "distinct regular files"):
            tooling_packaging.contents(path)

    def test_archive_symlinks_are_rejected(self):
        path = self.root / "bad.zip"
        with zipfile.ZipFile(path, "w") as archive:
            info = zipfile.ZipInfo("link")
            info.create_system = 3
            info.external_attr = 0o120777 << 16
            archive.writestr(info, b"target")
        with self.assertRaisesRegex(ValueError, "distinct regular files"):
            tooling_packaging.contents(path)
