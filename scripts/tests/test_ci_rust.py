"""Rust cache health reporting and assertion-enabled CI build configuration."""

from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import ci_rust


ROOT = Path(__file__).resolve().parents[2]


class CacheInventoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.target = Path(self.temporary.name) / "target"

    def write(self, name, content):
        path = self.target / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path

    def test_missing_cache_has_no_compiled_artifacts(self):
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    def test_metadata_only_cache_does_not_count_as_compiled_reuse(self):
        self.write(".rustc_info.json", b"{}")
        self.write("debug/.fingerprint/dependency/lib-dependency", b"marker")
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    def test_libraries_and_check_metadata_are_counted_across_profiles(self):
        self.write("ci/deps/libdependency.rlib", b"123")
        self.write("debug/deps/libother.rmeta", b"45")
        self.write("x86_64-pc-windows-msvc/release/deps/libnative.rlib", b"6")
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 3, "bytes": 6})

    def test_empty_artifact_is_not_usable(self):
        self.write("ci/deps/libdependency.rlib", b"")
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    def test_libraries_outside_dependency_directories_are_not_counted(self):
        self.write("ci/libdependency.rlib", b"123")
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    def test_directories_with_library_suffixes_are_not_counted(self):
        (self.target / "ci/deps/not-a-library.rlib").mkdir(parents=True)
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    @unittest.skipIf(os.name == "nt", "Windows symlinks need developer privileges")
    def test_symlinks_do_not_count_external_artifacts(self):
        external = Path(self.temporary.name) / "outside.rlib"
        external.write_bytes(b"external")
        link = self.target / "ci/deps/libexternal.rlib"
        link.parent.mkdir(parents=True)
        link.symlink_to(external)
        self.assertEqual(ci_rust.inventory(self.target), {"artifacts": 0, "bytes": 0})

    def report(self, stage, hit):
        output = io.StringIO()
        with redirect_stdout(output), mock.patch.object(ci_rust.ci, "summary") as summary:
            result = ci_rust.report(self.target, stage, hit)
        return result, output.getvalue(), summary.call_args.args[0]

    def test_empty_exact_hit_warns_but_does_not_block_rebuilding(self):
        result, output, summary = self.report("restored", "true")
        self.assertEqual(result, {"artifacts": 0, "bytes": 0})
        self.assertIn("::warning::", output)
        self.assertIn("Exact cache hit: **yes**", summary)
        self.assertEqual(json.loads(output.splitlines()[0])["stage"], "restored")

    def test_empty_cache_miss_is_normal(self):
        _, output, summary = self.report("restored", "false")
        self.assertNotIn("::warning::", output)
        self.assertIn("Exact cache hit: **no**", summary)

    def test_populated_exact_hit_reports_usable_bytes(self):
        self.write("ci/deps/libdependency.rlib", b"123")
        result, output, summary = self.report("restored", "true")
        self.assertEqual(result, {"artifacts": 1, "bytes": 3})
        self.assertNotIn("::warning::", output)
        self.assertIn("Compiled libraries: **1**", summary)

    def test_post_build_inventory_does_not_claim_a_restored_hit(self):
        self.write("ci/deps/libdependency.rlib", b"123")
        _, output, summary = self.report("built", "false")
        self.assertIn("Rust artifacts: built", summary)
        self.assertNotIn("Exact cache hit", summary)
        self.assertIsNone(json.loads(output.splitlines()[0])["cacheHit"])


class CiBuildConfigurationTests(unittest.TestCase):
    def test_optimized_profile_keeps_assertions_and_overflow_checks(self):
        profile = tomllib.loads((ROOT / "Cargo.toml").read_text())["profile"]["ci"]
        self.assertEqual(profile["inherits"], "test")
        self.assertGreater(profile["opt-level"], 0)
        self.assertIs(profile["debug-assertions"], True)
        self.assertIs(profile["overflow-checks"], True)

    def test_cache_namespace_cannot_restore_legacy_empty_entries(self):
        action = (ROOT / ".github/actions/rust-cache/action.yml").read_text()
        self.assertIn("prefix-key: fai-rust-v1", action)
        self.assertIn("cache-bin: false", action)

    def test_required_workflow_and_compiler_misses_use_shared_cache_action(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        compiler = (ROOT / ".github/actions/compiler/action.yml").read_text()
        self.assertNotIn("uses: Swatinem/rust-cache", workflow + compiler)
        self.assertIn("uses: ./.github/actions/rust-cache", workflow)
        self.assertIn("uses: ./.github/actions/rust-cache", compiler)

    def test_nextest_build_and_run_commands_use_the_same_cargo_profile(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        commands = [line.strip() for line in workflow.splitlines() if "run: cargo nextest run" in line]
        self.assertEqual(len(commands), 5)
        self.assertTrue(all("--cargo-profile ci" in command for command in commands))

    def test_doctests_and_partition_reuse_the_ci_build(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("cargo test --workspace --locked --profile ci --doc", workflow)
        self.assertIn("./scripts/check-nextest-partition.sh ci", workflow)
