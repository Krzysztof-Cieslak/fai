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

    def identity(self, environment=None, profile=compiler.PROFILE):
        return compiler.identity(self.root, {} if environment is None else environment, HOST, profile)

    def fake_bundle(self, expected=None):
        expected = expected or self.identity()
        directory = self.cache / expected["key"]
        directory.mkdir(parents=True)
        binary = directory / "fai"
        binary.write_bytes(b"fake compiler executable")
        manifest = {
            "identity": expected,
            "buildInfo": {"sourceId": expected["sourceId"], "target": HOST[0],
                          "profile": expected["profile"], "debugAssertions": True},
            "files": {"fai": {"size": binary.stat().st_size,
                              "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}},
        }
        (directory / "manifest.json").write_text(json.dumps(manifest))
        return binary

    def test_package_edits_do_not_change_compiler_key(self):
        before = self.identity()
        self.write("packages/json/Json.fai", "changed library")
        self.assertEqual(before, self.identity())

    def test_profiles_have_separate_bundle_identities(self):
        default = self.identity()
        ci = self.identity(profile="ci")
        self.assertEqual(default["sourceId"], ci["sourceId"])
        self.assertEqual(default["profile"], "package-dev")
        self.assertEqual(ci["profile"], "ci")
        self.assertNotEqual(default["key"], ci["key"])

    def test_unsupported_bundle_profile_is_rejected(self):
        with self.assertRaisesRegex(compiler.BundleError, "unsupported.*profile"):
            self.identity(profile="release")

    def test_package_integration_metadata_does_not_change_compiler_key(self):
        before = self.identity()
        self.write("packages/json/ci.json", json.dumps({"schemaVersion": 1, "name": "json", "dependencies": [],
                   "integrations": [{"name": "runtime", "entry": "examples/Runtime.fai"}]}))
        self.assertEqual(before, self.identity())

    def test_report_presentation_does_not_change_compiler_key(self):
        before = self.identity()
        self.write("scripts/performance_report/__init__.py", "new report wording")
        self.write("scripts/performance_report/tests/fixtures/normal.json", "{}")
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

    def test_cargo_output_directory_does_not_change_compiler_key(self):
        self.assertEqual(self.identity(), self.identity({"CARGO_TARGET_DIR": "/shared/build-output"}))

    def test_host_target_changes_compiler_key(self):
        other = compiler.identity(self.root, {}, ("aarch64-apple-darwin", "macos-aarch64-26.5"))
        self.assertEqual(self.identity()["sourceId"], other["sourceId"])
        self.assertNotEqual(self.identity()["key"], other["key"])

    def test_linux_compatibility_uses_running_libc_not_python_baseline(self):
        with patch.object(compiler.platform, "system", return_value="Linux"), patch.object(compiler.platform, "machine", return_value="x86_64"), patch.object(compiler.platform, "libc_ver", return_value=("glibc", "2.17")), patch.object(compiler.os, "confstr", return_value="glibc 2.39", create=True):
            self.assertEqual(compiler.host(), ("x86_64-unknown-linux-gnu", "linux-x86_64-glibc-2.39"))

    def test_no_build_does_not_invoke_cargo_on_a_miss(self):
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), patch.object(compiler, "build", side_effect=AssertionError("unexpected build")):
            with self.assertRaisesRegex(compiler.BundleError, "no compiler bundle"):
                compiler.ensure(self.root, self.cache, offline=True, no_build=True)

    def test_same_checkout_at_another_path_has_same_key(self):
        import shutil
        destination = Path(self.temporary.name) / "another"
        shutil.copytree(self.root, destination)
        self.assertEqual(self.identity(), compiler.identity(destination, {}, HOST))

    def test_cache_hit_never_invokes_cargo_or_any_subprocess(self):
        binary = self.fake_bundle()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), patch.object(subprocess, "run", side_effect=AssertionError("unexpected subprocess")):
            self.assertEqual(binary, compiler.ensure(self.root, self.cache, offline=True, no_build=True))

    def test_ci_cache_hit_never_invokes_cargo(self):
        binary = self.fake_bundle(self.identity(profile="ci"))
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run", side_effect=AssertionError("unexpected subprocess")):
            self.assertEqual(binary, compiler.ensure(self.root, self.cache, offline=True, no_build=True, profile="ci"))

    def test_ci_binary_cannot_be_relabelled_as_package_dev(self):
        binary = self.fake_bundle()
        manifest = json.loads((binary.parent / "manifest.json").read_text())
        manifest["buildInfo"]["profile"] = "ci"
        (binary.parent / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(compiler.BundleError, "provenance"):
            compiler.validate(binary.parent, self.identity())

    def test_unasserted_ci_binary_is_rejected(self):
        expected = self.identity(profile="ci")
        binary = self.fake_bundle(expected)
        manifest = json.loads((binary.parent / "manifest.json").read_text())
        manifest["buildInfo"]["debugAssertions"] = False
        (binary.parent / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(compiler.BundleError, "provenance"):
            compiler.validate(binary.parent, expected)

    def build_fixture(self, environment=None):
        environment = environment or {}
        expected = self.identity(environment, profile="ci")
        binary = compiler.built_binary(self.root, expected, environment)
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"compiler from the test build")
        info = {"sourceId": expected["sourceId"], "target": HOST[0], "profile": "ci", "debugAssertions": True}
        return expected, binary, info

    def test_unit_build_produces_a_bundle_without_a_second_cargo_build(self):
        expected, built, info = self.build_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run") as run, \
                patch.object(subprocess, "check_output", return_value=json.dumps(info)):
            binary = compiler.build_tests("unit", self.root, self.cache)
            reused = compiler.ensure(self.root, self.cache, offline=True, no_build=True, profile="ci")
        run.assert_called_once()
        self.assertEqual(run.call_args.args[0], ["cargo", "nextest", "run", "--workspace", "--locked",
                                               "--cargo-profile", "ci", "--profile", "unit", "--no-run"])
        self.assertEqual(run.call_args.kwargs["env"]["FAI_BUILD_PROFILE"], "ci")
        self.assertNotIn("FAI_TOOL_INPUT_ID", run.call_args.kwargs["env"])
        self.assertEqual(binary.read_bytes(), built.read_bytes())
        self.assertEqual(reused, binary)
        self.assertEqual(compiler.validate(binary.parent, expected), binary)

    def test_platform_build_uses_the_existing_cross_os_selection(self):
        _, _, info = self.build_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch("cross_os.cargo_args", return_value=["--package", "fai-cli", "--lib"]) as selection, \
                patch.object(subprocess, "run") as run, \
                patch.object(subprocess, "check_output", return_value=json.dumps(info)):
            compiler.build_tests("cross-os", self.root, self.cache)
        selection.assert_called_once()
        self.assertEqual(run.call_args.args[0][3:6], ["--package", "fai-cli", "--lib"])
        self.assertNotIn("--workspace", run.call_args.args[0])
        self.assertEqual(run.call_args.args[0][-3:], ["--profile", "cross-os", "--no-run"])

    def test_invalid_platform_selection_cannot_fall_back_to_skipped_tests(self):
        import cross_os
        self.build_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch("cross_os.cargo_args", side_effect=cross_os.SelectionError("unknown target")), \
                patch.object(subprocess, "run", side_effect=AssertionError("unexpected build")):
            with self.assertRaisesRegex(compiler.BundleError, "invalid cross-os build selection"):
                compiler.build_tests("cross-os", self.root, self.cache)

    def test_package_only_ci_miss_builds_only_the_cli_in_the_requested_profile(self):
        expected, _, info = self.build_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run") as run, \
                patch.object(subprocess, "check_output", return_value=json.dumps(info)):
            binary = compiler.ensure(self.root, self.cache, offline=True, profile="ci")
        self.assertEqual(run.call_args.args[0], ["cargo", "build", "--locked", "--profile", "ci", "-p", "fai-cli"])
        self.assertEqual(compiler.validate(binary.parent, expected), binary)

    def test_test_build_rejects_unknown_selection_before_running_cargo(self):
        with patch.object(subprocess, "run", side_effect=AssertionError("unexpected build")):
            with self.assertRaisesRegex(compiler.BundleError, "test-build selection"):
                compiler.build_tests("unknown", self.root, self.cache)

    def test_test_build_rejects_changed_inputs_before_packaging(self):
        self.build_fixture()
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run", side_effect=lambda *args, **kwargs: self.write("crates/tool/src/lib.rs", "changed")), \
                patch.object(subprocess, "check_output", side_effect=AssertionError("stale binary queried")):
            with self.assertRaisesRegex(compiler.BundleError, "inputs changed"):
                compiler.build_tests("unit", self.root, self.cache)

    def test_test_build_rejects_binary_from_another_profile(self):
        _, _, info = self.build_fixture()
        info["profile"] = "package-dev"
        with patch.dict("os.environ", {}, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run"), patch.object(subprocess, "check_output", return_value=json.dumps(info)):
            with self.assertRaisesRegex(compiler.BundleError, "provenance"):
                compiler.build_tests("unit", self.root, self.cache)

    def test_test_build_honors_custom_target_directory(self):
        environment = {"CARGO_TARGET_DIR": str(self.root / "custom-target")}
        expected, built, info = self.build_fixture(environment)
        with patch.dict("os.environ", environment, clear=True), patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run"), patch.object(subprocess, "check_output", return_value=json.dumps(info)) as query:
            binary = compiler.build_tests("unit", self.root, self.cache)
        self.assertEqual(query.call_args.args[0], [str(built), "build-info"])
        self.assertEqual(compiler.validate(binary.parent, expected), binary)

    def test_test_build_rejects_a_cross_compilation_target(self):
        with patch.dict("os.environ", {"CARGO_BUILD_TARGET": "aarch64-apple-darwin"}, clear=True), \
                patch.object(compiler, "host", return_value=HOST), \
                patch.object(subprocess, "run", side_effect=AssertionError("unexpected build")):
            with self.assertRaisesRegex(compiler.BundleError, "native target"):
                compiler.build_tests("unit", self.root, self.cache)

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

    def test_archive_cannot_cross_profile_boundaries(self):
        binary = self.fake_bundle(self.identity(profile="ci"))
        archive = Path(self.temporary.name) / "compiler.zip"
        compiler.pack(binary, archive)
        with self.assertRaisesRegex(compiler.BundleError, "identity"):
            compiler.restore(archive, Path(self.temporary.name) / "other-cache", self.identity())

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
