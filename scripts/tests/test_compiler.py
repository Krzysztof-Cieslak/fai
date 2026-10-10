"""Compiler cache invariants; these tests need neither Rust nor a Fai binary."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compiler

HOST = ("x86_64-unknown-linux-gnu", "test-host")


class CompilerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "source"
        (self.root / "scripts").mkdir(parents=True)
        (self.root / "crates/tool/src").mkdir(parents=True)
        (self.root / "packages/json").mkdir(parents=True)
        self.write("Cargo.toml", "workspace")
        self.write("crates/tool/src/lib.rs", "compiler")
        self.write("packages/json/Json.fai", "module Json")
        self.write("scripts/compiler.py", "bootstrap")
        self.write("scripts/compiler-inputs.json", json.dumps({
            "schemaVersion": 1,
            "files": ["Cargo.toml", "scripts/compiler-inputs.json", "scripts/compiler.py"],
            "trees": ["crates", "std", ".cargo"],
            "ignoredDirectories": ["target", "__pycache__"],
            "ignoredSuffixes": [".snap.new", ".pyc"],
        }))
        self.cache = Path(self.temporary.name) / "cache"

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)

    def identity(self, environment=None):
        return compiler.identity(self.root, {} if environment is None else environment, HOST)

    def fake_bundle(self, expected=None):
        expected = expected or self.identity()
        directory = self.cache / expected["key"]
        directory.mkdir(parents=True)
        binary = directory / "fai"
        binary.write_bytes(b"fake compiler executable")
        manifest = {
            "identity": expected,
            "buildInfo": {"sourceId": expected["sourceId"], "target": HOST[0],
                          "profile": compiler.PROFILE, "debugAssertions": True},
            "files": {"fai": {"size": binary.stat().st_size,
                              "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}},
        }
        (directory / "manifest.json").write_text(json.dumps(manifest))
        return binary

    def test_package_edits_do_not_change_compiler_key(self):
        before = self.identity()
        self.write("packages/json/Json.fai", "changed library")
        self.assertEqual(before, self.identity())

    def test_tooling_edits_change_compiler_key(self):
        before = self.identity()
        self.write("crates/tool/src/lib.rs", "changed compiler")
        self.assertNotEqual(before["key"], self.identity()["key"])

    def test_new_source_changes_compiler_key(self):
        before = self.identity()
        self.write("crates/tool/src/new.rs", "new module")
        self.assertNotEqual(before["key"], self.identity()["key"])

    def test_removed_source_changes_compiler_key(self):
        before = self.identity()
        (self.root / "crates/tool/src/lib.rs").unlink()
        self.assertNotEqual(before["key"], self.identity()["key"])

    def test_embedded_standard_library_changes_compiler_key(self):
        before = self.identity()
        self.write("std/Prelude.fai", "module Prelude")
        self.assertNotEqual(before["key"], self.identity()["key"])

    def test_cargo_configuration_changes_compiler_key(self):
        before = self.identity()
        self.write(".cargo/config.toml", "[build]\nrustflags = []")
        self.assertNotEqual(before["key"], self.identity()["key"])

    def test_build_outputs_do_not_change_compiler_key(self):
        before = self.identity()
        self.write("crates/tool/target/generated.rs", "generated")
        self.write("crates/tool/src/failure.snap.new", "pending snapshot")
        self.assertEqual(before, self.identity())

    def test_build_flags_change_compiler_key(self):
        self.assertNotEqual(self.identity()["key"], self.identity({"RUSTFLAGS": "-C target-cpu=native"})["key"])

    def test_cache_location_does_not_change_compiler_key(self):
        self.assertEqual(self.identity(), self.identity({"FAI_COMPILER_CACHE": "/elsewhere"}))

    def test_same_checkout_at_another_path_has_same_key(self):
        import shutil
        destination = Path(self.temporary.name) / "another"
        shutil.copytree(self.root, destination)
        self.assertEqual(self.identity(), compiler.identity(destination, {}, HOST))

    def test_cache_hit_never_invokes_cargo_or_any_subprocess(self):
        binary = self.fake_bundle()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), patch.object(subprocess, "run", side_effect=AssertionError("unexpected subprocess")):
            self.assertEqual(binary, compiler.ensure(self.root, self.cache, offline=True, no_build=True))

    def test_corrupt_executable_is_rejected(self):
        binary = self.fake_bundle()
        binary.write_bytes(b"x" * binary.stat().st_size)
        with self.assertRaisesRegex(compiler.BundleError, "checksum"):
            compiler.validate(binary.parent, self.identity())

    def test_missing_executable_is_rejected(self):
        binary = self.fake_bundle()
        binary.unlink()
        with self.assertRaises(compiler.BundleError):
            compiler.validate(binary.parent, self.identity())

    def test_archive_round_trip_is_relocatable(self):
        binary = self.fake_bundle()
        archive = Path(self.temporary.name) / "compiler.zip"
        compiler.pack(binary, archive)
        destination = Path(self.temporary.name) / "other-cache"
        restored = compiler.restore(archive, destination, self.identity())
        self.assertEqual(binary.read_bytes(), restored.read_bytes())
        self.assertEqual(restored, compiler.validate(restored.parent, self.identity()))

    def test_archive_path_traversal_is_rejected(self):
        archive = Path(self.temporary.name) / "bad.zip"
        with zipfile.ZipFile(archive, "w") as zipped:
            zipped.writestr("../escaped", "bad")
        with self.assertRaisesRegex(compiler.BundleError, "unsafe"):
            compiler.restore(archive, self.cache, self.identity())
        self.assertFalse((self.cache.parent / "escaped").exists())

    def test_wrong_build_identity_is_rejected(self):
        binary = self.fake_bundle()
        self.write("crates/tool/src/lib.rs", "changed compiler")
        with self.assertRaisesRegex(compiler.BundleError, "identity"):
            compiler.validate(binary.parent, self.identity())

    def producer(self, **changes):
        return {"conclusion": "success", "path": ".github/workflows/ci.yml", "event": "push",
                "head_branch": "main", "head_sha": "abc", "head_repository": {"id": 1}, **changes}

    def test_successful_default_branch_producer_is_accepted(self):
        self.assertTrue(compiler.acceptable_producer(self.producer(), {"id": 1, "default_branch": "main"}, self.root))

    def test_failed_producer_is_rejected(self):
        self.assertFalse(compiler.acceptable_producer(self.producer(conclusion="failure"), {"id": 1, "default_branch": "main"}, self.root))

    def test_unrelated_pr_producer_is_rejected(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 1)):
            self.assertFalse(compiler.acceptable_producer(self.producer(event="pull_request"), {"id": 1, "default_branch": "main"}, self.root))

    def test_stack_ancestor_producer_is_accepted(self):
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0)):
            self.assertTrue(compiler.acceptable_producer(self.producer(event="pull_request"), {"id": 1, "default_branch": "main"}, self.root))

    def test_main_push_does_not_consume_pr_producers(self):
        with patch.object(subprocess, "run", side_effect=AssertionError("should not inspect PR ancestry")):
            self.assertFalse(compiler.acceptable_producer(self.producer(event="pull_request"), {"id": 1, "default_branch": "main"}, self.root, event="push"))


if __name__ == "__main__":
    unittest.main()
