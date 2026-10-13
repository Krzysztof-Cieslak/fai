"""Release identity, changelog policy, preparation recovery, and matrix integrity."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release


class ChangelogTests(unittest.TestCase):
    def test_compiler_changes_require_an_entry(self):
        self.assertTrue(release.requires_changelog(["crates/fai-cli/src/lib.rs"]))

    def test_internal_tooling_changes_require_an_entry(self):
        self.assertTrue(release.requires_changelog(["scripts/performance_report/__init__.py"]))

    def test_embedded_std_changes_require_an_entry(self):
        self.assertTrue(release.requires_changelog(["std/core/Prelude.fai"]))

    def test_source_packages_have_independent_changelogs(self):
        self.assertFalse(release.requires_changelog(["packages/json/src/Json.fai", "packages/fai.toml"]))

    def test_mixed_changes_still_require_a_compiler_entry(self):
        self.assertTrue(release.requires_changelog(["packages/json/src/Json.fai", ".github/workflows/release.yml"]))

    def test_release_notes_are_taken_only_from_the_requested_section(self):
        text = "# Changelog\n\n## [Unreleased]\n\n## [1.1.0] - 2026-10-13\n\n### Added\n- New thing.\n\n## [1.0.0] - 2026-10-12\n\n- Old thing.\n"
        self.assertEqual(release.notes(text, "1.1.0"), "### Added\n- New thing.\n")

    def test_duplicate_sections_are_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "duplicate"):
            release.sections("## [Unreleased]\n## [1.0.0] - 2026-10-13\n- A\n## [1.0.0] - 2026-10-13\n- B\n")

    def test_invalid_release_date_is_rejected(self):
        with self.assertRaises(ValueError):
            release.sections("## [Unreleased]\n## [1.0.0] - 2026-02-31\n- A\n")

    def test_release_requires_actual_notes(self):
        with self.assertRaises(release.ReleaseError):
            release.notes("## [Unreleased]\n## [1.0.0] - 2026-10-13\n### Added\n", "1.0.0")

    def test_prerelease_versions_sort_by_semver(self):
        self.assertLess(release.version_key("1.0.0-rc.2"), release.version_key("1.0.0-rc.10"))
        self.assertLess(release.version_key("1.0.0-rc.10"), release.version_key("1.0.0"))

    def test_build_metadata_is_not_an_official_version(self):
        with self.assertRaises(release.ReleaseError):
            release.version_key("1.0.0+rebuilt")

    def test_oversized_version_components_are_rejected_before_preparation(self):
        with self.assertRaises(release.ReleaseError):
            release.version_key("18446744073709551616.0.0")

    def test_prerelease_numeric_identifiers_are_canonical(self):
        with self.assertRaises(release.ReleaseError):
            release.version_key("1.0.0-rc.01")


class ReleaseEnvironmentTests(unittest.TestCase):
    def test_python_windows_environment_keys_are_uppercase(self):
        with tempfile.TemporaryDirectory(prefix="MSVC tools ") as temporary:
            tools = Path(temporary)
            linker = tools / "bin/Hostx64/x64/link.exe"
            linker.parent.mkdir(parents=True)
            linker.touch()
            environment = release.release_environment("x86_64-pc-windows-msvc", environ={
                "VCTOOLSINSTALLDIR": str(tools), "VSCMD_ARG_HOST_ARCH": "x64", "PATH": "git/usr/bin",
            })
            self.assertEqual(environment["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"], str(linker))
            self.assertEqual(environment["PATH"].split(os.pathsep)[0], str(linker.parent))

    def test_msvc_linker_precedes_git_bash_for_host_and_target_builds(self):
        with tempfile.TemporaryDirectory(prefix="MSVC tools ") as temporary:
            tools = Path(temporary)
            linker = tools / "bin/Hostx64/x64/link.exe"
            linker.parent.mkdir(parents=True)
            linker.touch()
            original = {"VCToolsInstallDir": str(tools), "PATH": "git/usr/bin", "VSCMD_ARG_HOST_ARCH": "x64"}
            environment = release.release_environment("x86_64-pc-windows-msvc", environ=original)
            self.assertEqual(environment["PATH"].split(os.pathsep)[0], str(linker.parent))
            self.assertEqual(environment["CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"], str(linker))
            self.assertIn("target-feature=+crt-static", environment["RUSTFLAGS"])
            self.assertEqual(original["PATH"], "git/usr/bin")

    def test_arm64_releases_select_native_arm64_tools(self):
        with tempfile.TemporaryDirectory() as temporary:
            tools = Path(temporary)
            linker = tools / "bin/Hostarm64/arm64/link.exe"
            linker.parent.mkdir(parents=True)
            linker.touch()
            environment = release.release_environment("aarch64-pc-windows-msvc", environ={
                "VCToolsInstallDir": str(tools), "VSCMD_ARG_HOST_ARCH": "arm64", "PATH": "git/usr/bin",
            })
            self.assertEqual(environment["CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"], str(linker))

    def test_msvc_tools_are_required_before_starting_cargo(self):
        with self.assertRaisesRegex(release.ReleaseError, "developer environment"):
            release.release_environment("x86_64-pc-windows-msvc", environ={"PATH": "git/usr/bin"})

    def test_encoded_flags_are_preserved_when_selecting_static_crt(self):
        with tempfile.TemporaryDirectory() as temporary:
            tools = Path(temporary)
            linker = tools / "bin/Hostx64/x64/link.exe"
            linker.parent.mkdir(parents=True)
            linker.touch()
            environment = release.release_environment("x86_64-pc-windows-msvc", environ={
                "VCToolsInstallDir": str(tools), "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fcustom_build",
            })
            self.assertEqual(environment["CARGO_ENCODED_RUSTFLAGS"], "--cfg\x1fcustom_build\x1f-C\x1ftarget-feature=+crt-static")

    def test_macos_deployment_target_does_not_require_windows_tools(self):
        environment = release.release_environment("aarch64-apple-darwin", "15.0", {"PATH": "/usr/bin"})
        self.assertEqual(environment["PATH"], "/usr/bin")
        self.assertEqual(environment["MACOSX_DEPLOYMENT_TARGET"], "15.0")


class GitHubReleaseTests(unittest.TestCase):
    def test_published_release_uses_the_tag_endpoint(self):
        published = {"id": 42, "tag_name": "v0.0.1", "draft": False}
        with mock.patch.object(release, "gh_api", return_value=published) as api:
            self.assertEqual(release.github_release("v0.0.1"), published)
        self.assertEqual(api.call_count, 1)

    def test_authenticated_draft_is_found_when_tag_endpoint_returns_404(self):
        draft = {"id": 43, "tag_name": "v0.0.1", "draft": True}
        with mock.patch.object(release, "gh_api", side_effect=[None, [draft]]) as api:
            self.assertEqual(release.github_release("v0.0.1"), draft)
        self.assertIn("releases?per_page=100&page=1", api.call_args.args[0])

    def test_draft_lookup_traverses_full_pages(self):
        other = [{"tag_name": f"other-{index}"} for index in range(100)]
        draft = {"id": 44, "tag_name": "v0.0.1", "draft": True}
        with mock.patch.object(release, "gh_api", side_effect=[None, other, [draft]]) as api:
            self.assertEqual(release.github_release("v0.0.1"), draft)
        self.assertIn("page=2", api.call_args.args[0])

    def test_missing_optional_release_returns_none(self):
        with mock.patch.object(release, "gh_api", side_effect=[None, []]):
            self.assertIsNone(release.github_release("v0.0.1", missing=True))

    def test_missing_required_release_is_an_error(self):
        with mock.patch.object(release, "gh_api", side_effect=[None, []]):
            with self.assertRaisesRegex(release.ReleaseError, "was not found"):
                release.github_release("v0.0.1")

    def test_network_errors_do_not_become_missing_releases(self):
        with mock.patch.object(release, "gh_api", side_effect=release.ReleaseError("HTTP 502")):
            with self.assertRaisesRegex(release.ReleaseError, "HTTP 502"):
                release.github_release("v0.0.1", missing=True)


class ResumeQualificationTests(unittest.TestCase):
    def setUp(self):
        self.commit = "a" * 40
        self.repo = release.config()["repository"]
        self.source = {"event": "workflow_dispatch", "path": ".github/workflows/release.yml",
                       "status": "completed", "head_branch": "main", "head_sha": self.commit,
                       "repository": {"full_name": self.repo}, "head_repository": {"full_name": self.repo}}
        names = [f"Qualify {item['target']}" for item in release.config()["targets"]]
        names += ["Validate prepared release", "Full source verification / Select affected checks",
                  "Full source verification / fmt + clippy", "Full source verification / unit + e2e tests (linux)",
                  "Full source verification / platform tests (macos-latest)", "Full source verification / platform tests (windows-latest)"]
        self.checks = [{"name": name, "status": "completed", "conclusion": "success",
                        "details_url": f"https://github.com/{self.repo}/actions/runs/123/job/{index}"}
                       for index, name in enumerate(names)]
        self.api = self.enterContext(mock.patch.object(release, "gh_api", side_effect=self.response))
        self.enterContext(mock.patch.object(release, "require_repository"))
        self.tag = self.enterContext(mock.patch.object(release, "remote_tag", return_value=self.commit))

    def response(self, path, root):
        if "/actions/runs/" in path:
            return self.source
        return {"check_runs": self.checks}

    def test_fully_qualified_run_identifies_the_unchanged_tagged_source(self):
        self.assertEqual(release.qualified_run("0.0.1", 123), {"version": "0.0.1", "commit": self.commit, "runId": "123"})

    def test_invalid_run_id_is_rejected_before_network_access(self):
        with self.assertRaisesRegex(release.ReleaseError, "positive integer"):
            release.qualified_run("0.0.1", "../123")
        self.api.assert_not_called()

    def test_unfinished_run_cannot_be_resumed(self):
        self.source["status"] = "in_progress"
        with self.assertRaisesRegex(release.ReleaseError, "completed manual Release"):
            release.qualified_run("0.0.1", 123)

    def test_other_workflows_cannot_authorize_publication(self):
        self.source["path"] = ".github/workflows/ci.yml"
        with self.assertRaisesRegex(release.ReleaseError, "completed manual Release"):
            release.qualified_run("0.0.1", 123)

    def test_pull_request_runs_cannot_authorize_publication(self):
        self.source["event"] = "pull_request"
        with self.assertRaisesRegex(release.ReleaseError, "completed manual Release"):
            release.qualified_run("0.0.1", 123)

    def test_unprotected_branches_cannot_authorize_publication(self):
        self.source["head_branch"] = "experiment"
        with self.assertRaisesRegex(release.ReleaseError, "completed manual Release"):
            release.qualified_run("0.0.1", 123)

    def test_fork_runs_cannot_authorize_publication(self):
        self.source["head_repository"]["full_name"] = "other/fai"
        with self.assertRaisesRegex(release.ReleaseError, "completed manual Release"):
            release.qualified_run("0.0.1", 123)

    def test_existing_tag_cannot_move_to_the_workflow_commit(self):
        self.tag.return_value = "b" * 40
        with self.assertRaisesRegex(release.ReleaseError, "tag does not identify"):
            release.qualified_run("0.0.1", 123)

    def test_missing_native_qualification_blocks_resume(self):
        self.checks.pop(0)
        with self.assertRaisesRegex(release.ReleaseError, "all six native"):
            release.qualified_run("0.0.1", 123)

    def test_failed_native_qualification_blocks_resume(self):
        self.checks[0]["conclusion"] = "failure"
        with self.assertRaisesRegex(release.ReleaseError, "all six native"):
            release.qualified_run("0.0.1", 123)

    def test_skipped_full_source_check_blocks_resume(self):
        self.checks[-1]["conclusion"] = "skipped"
        with self.assertRaisesRegex(release.ReleaseError, "all six native"):
            release.qualified_run("0.0.1", 123)

    def test_other_runs_checks_do_not_authorize_resume(self):
        self.checks[0]["details_url"] = self.checks[0]["details_url"].replace("/123/", "/456/")
        with self.assertRaisesRegex(release.ReleaseError, "all six native"):
            release.qualified_run("0.0.1", 123)

    def test_duplicate_qualification_checks_are_rejected(self):
        self.checks.append(self.checks[0].copy())
        with self.assertRaisesRegex(release.ReleaseError, "ambiguous"):
            release.qualified_run("0.0.1", 123)

    def test_running_check_cannot_authorize_resume(self):
        self.checks[0]["status"] = "in_progress"
        with self.assertRaisesRegex(release.ReleaseError, "all six native"):
            release.qualified_run("0.0.1", 123)

    def test_resume_assembles_and_publishes_from_the_original_checkout(self):
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "workflow_dispatch"}), \
                mock.patch.object(release, "git") as git, mock.patch.object(release, "run") as run, \
                mock.patch.object(release, "assemble") as assemble, mock.patch.object(release, "publish", return_value={"ok": True}) as publish:
            self.assertEqual(release.resume("0.0.1", 123), {"ok": True})
        checkout = Path(git.call_args_list[0].args[3])
        self.assertEqual(git.call_args_list[0].args[:3], ("worktree", "add", "--detach"))
        self.assertEqual(git.call_args_list[0].args[4], self.commit)
        self.assertIn("123", run.call_args.args)
        self.assertEqual(assemble.call_args.args[-1], checkout)
        self.assertEqual(publish.call_args.args[-1], checkout)
        self.assertEqual(git.call_args.args, ("worktree", "remove", str(checkout)))

    def test_resume_cleans_up_the_source_checkout_after_verification_fails(self):
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "workflow_dispatch"}), \
                mock.patch.object(release, "git") as git, mock.patch.object(release, "run"), \
                mock.patch.object(release, "assemble", side_effect=release.ReleaseError("corrupt artifact")), \
                mock.patch.object(release, "publish") as publish:
            with self.assertRaisesRegex(release.ReleaseError, "corrupt artifact"):
                release.resume("0.0.1", 123)
        publish.assert_not_called()
        self.assertEqual(git.call_args.args[:2], ("worktree", "remove"))

    def test_local_resume_cannot_bypass_the_manual_workflow(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(release.ReleaseError, "manually dispatched"):
                release.resume("0.0.1", 123)
        self.api.assert_not_called()


class RepositoryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/fai-cli"]\n[workspace.package]\nversion = "0.1.0"\n')
        self.write("crates/fai-cli/Cargo.toml", '[package]\nname = "fai-cli"\nversion.workspace = true\n')
        self.write("Cargo.lock", 'version = 4\n\n[[package]]\nname = "fai-cli"\nversion = "0.1.0"\n\n[[package]]\nname = "third-party"\nversion = "0.1.0"\nsource = "registry+https://example.invalid"\n')
        self.write("rust-toolchain.toml", '[toolchain]\nchannel = "1.96.0"\n')
        self.write("changelog.md", "# Changelog\n\n## [Unreleased]\n\n### Added\n- A compiler feature.\n")
        self.write("scripts/release-targets.json", (release.ROOT / "scripts/release-targets.json").read_text())
        self.git("init", "-b", "main")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("add", ".")
        self.git("commit", "-m", "Initial sources")
        self.git("switch", "-c", "prepare-release")

    def git(self, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=self.root, text=True, stderr=subprocess.DEVNULL).strip()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def test_prepare_rolls_notes_and_updates_only_internal_lock_versions(self):
        result = release.prepare("0.2.0", self.root)
        self.assertEqual(result["version"], "0.2.0")
        lock = (self.root / "Cargo.lock").read_text()
        self.assertIn('name = "fai-cli"\nversion = "0.2.0"', lock)
        self.assertIn('name = "third-party"\nversion = "0.1.0"', lock)
        sections = release.sections((self.root / "changelog.md").read_text())
        self.assertFalse(release.has_entries(sections["Unreleased"][0]))
        self.assertIn("A compiler feature.", sections["0.2.0"][0])

    def test_prepare_requires_a_feature_branch(self):
        self.git("switch", "main")
        with self.assertRaisesRegex(release.ReleaseError, "feature branch"):
            release.prepare("0.2.0", self.root)

    def test_prepare_resumes_an_interrupted_multi_file_edit(self):
        original = release.atomic_write
        def fail_lock(path, content):
            if path.name == "Cargo.lock":
                raise OSError("interrupted")
            original(path, content)
        with mock.patch.object(release, "atomic_write", side_effect=fail_lock):
            with self.assertRaises(OSError):
                release.prepare("0.2.0", self.root)
        release.prepare("0.2.0", self.root)
        self.assertIn('version = "0.2.0"', (self.root / "Cargo.lock").read_text())

    def test_preparation_recovery_preserves_conflicting_user_edits(self):
        original = release.atomic_write
        def fail_lock(path, content):
            if path.name == "Cargo.lock":
                raise OSError("interrupted")
            original(path, content)
        with mock.patch.object(release, "atomic_write", side_effect=fail_lock):
            with self.assertRaises(OSError):
                release.prepare("0.2.0", self.root)
        self.write("Cargo.toml", "# user edit\n")
        with self.assertRaisesRegex(release.ReleaseError, "changed during preparation"):
            release.prepare("0.2.0", self.root)
        self.assertEqual((self.root / "Cargo.toml").read_text(), "# user edit\n")

    def test_validate_rejects_a_different_requested_version(self):
        release.prepare("0.2.0", self.root)
        self.git("add", ".")
        self.git("commit", "-m", "Prepare compiler release")
        with self.assertRaisesRegex(release.ReleaseError, "Cargo.toml"):
            release.validate("0.3.0", root=self.root)

    def test_validate_rejects_a_release_older_than_its_installer_floor(self):
        catalog = release.config(self.root)
        catalog["minimumInstaller"] = "0.2.0"
        self.write("scripts/release-targets.json", json.dumps(catalog))
        with self.assertRaisesRegex(release.ReleaseError, "minimum installer"):
            release.validate("0.0.1", root=self.root)

    def test_validate_rejects_an_existing_tag_on_another_commit(self):
        self.git("tag", "-a", "v0.2.0", "-m", "Other release")
        release.prepare("0.2.0", self.root)
        self.git("add", ".")
        self.git("commit", "-m", "Prepare compiler release")
        with self.assertRaisesRegex(release.ReleaseError, "different commit"):
            release.validate("0.2.0", root=self.root)

    def test_changelog_gate_uses_the_actual_diff(self):
        base = self.git("rev-parse", "HEAD")
        self.write("crates/fai-cli/src/lib.rs", "// changed\n")
        self.git("add", ".")
        self.git("commit", "-m", "Change compiler")
        with self.assertRaisesRegex(release.ReleaseError, "must update changelog"):
            release.check_changelog(base, self.root)

    def test_a_different_origin_cannot_receive_release_tags(self):
        self.git("remote", "add", "origin", "https://github.com/other/project.git")
        with self.assertRaisesRegex(release.ReleaseError, "origin does not match"):
            release.require_repository(self.root)

    def test_build_only_can_qualify_unreleased_source_on_one_target(self):
        result = release.validate("0.1.0", root=self.root, build_only=True, target="aarch64-pc-windows-msvc")
        self.assertTrue(result["buildOnly"])
        self.assertEqual([item["target"] for item in result["matrix"]["include"]], ["aarch64-pc-windows-msvc"])
        self.assertEqual(result["notes"], "")

    def test_publication_cannot_select_a_partial_target_matrix(self):
        with self.assertRaisesRegex(release.ReleaseError, "build-only"):
            release.validate("0.1.0", root=self.root, target="aarch64-pc-windows-msvc")

    def test_unknown_qualification_target_is_rejected(self):
        with self.assertRaisesRegex(release.ReleaseError, "unknown qualification target"):
            release.validate("0.1.0", root=self.root, build_only=True, target="not-a-host")


class ArtifactTests(unittest.TestCase):
    def test_local_publication_cannot_bypass_the_manual_workflow(self):
        with mock.patch.dict(os.environ, {}, clear=True), mock.patch.object(release, "require_repository") as repository:
            with self.assertRaisesRegex(release.ReleaseError, "manually dispatched"):
                release.publish("1.0.0", Path("unused"))
            repository.assert_not_called()

    def test_complete_matrix_produces_fixed_tag_installers_and_verified_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "inputs", root / "output"
            source.mkdir()
            (root / "scripts").mkdir()
            shutil.copy2(release.ROOT / "scripts/release-targets.json", root / "scripts/release-targets.json")
            shutil.copytree(release.ROOT / "scripts/install", root / "scripts/install")
            for spec in release.config(root)["targets"]:
                target = spec["target"]
                report = {"version": "0.2.0", "commit": "b" * 40, "target": target, "dirty": False,
                          "minimumSystem": spec["minimumSystem"],
                          "buildInfo": {"version": "0.2.0", "target": target, "profile": "release", "debugAssertions": False,
                                        "sourceId": "a" * 64, "toolBuildId": "a" * 64, "backendBuildId": "a" * 64}}
                for kind, prefix in (("compiler", "fai"), ("installer", "fai-installer")):
                    extension = "zip" if "windows" in target else "tar.gz"
                    path = source / f"{prefix}-0.2.0-{target}.{extension}"
                    path.write_bytes(b"verified archive bytes")
                    report[kind] = {"name": path.name, "size": path.stat().st_size, "sha256": release.sha(path),
                                    "executableSha256": "c" * 64}
                (source / f"target-{target}.json").write_text(json.dumps(report))
            with mock.patch.object(release, "git", return_value="b" * 40):
                manifest = release.assemble("0.2.0", source, output, root)
            self.assertEqual(len(manifest["targets"]), 6)
            self.assertNotIn("@VERSION@", (output / "install.sh").read_text())
            self.assertNotIn("@VERSION@", (output / "install.ps1").read_text())
            checksums = dict(line.split("  ", 1)[::-1] for line in (output / "SHA256SUMS").read_text().splitlines())
            self.assertEqual(checksums, {name: release.sha(output / name) for name in release.asset_names(manifest) if name != "SHA256SUMS"})

    def test_tar_archives_have_reproducible_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            first, second = Path(temporary) / "first.tar.gz", Path(temporary) / "second.tar.gz"
            files = {"bin/fai": b"compiler", "metadata.json": b"{}", "LICENSE": b"license"}
            release.make_archive(first, files)
            release.make_archive(second, files)
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_zip_archives_have_reproducible_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            first, second = Path(temporary) / "first.zip", Path(temporary) / "second.zip"
            files = {"bin/fai.exe": b"compiler", "metadata.json": b"{}", "LICENSE": b"license"}
            release.make_archive(first, files)
            release.make_archive(second, files)
            self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_incomplete_platform_matrix_cannot_be_assembled(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(release.ReleaseError, "incomplete release matrix"):
                release.assemble("0.1.0", root / "inputs", root / "out")

    def test_published_asset_digest_must_match_shipped_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            manifest = {"targets": {}}
            for name in release.asset_names(manifest):
                (directory / name).write_bytes(b"content")
            assets = [{"name": name, "size": 7, "digest": "sha256:" + hashlib.sha256(b"content").hexdigest()}
                      for name in release.asset_names(manifest)]
            assets[0]["digest"] = "sha256:" + "0" * 64
            with self.assertRaisesRegex(release.ReleaseError, "checksum mismatch"):
                release.verify_assets({"assets": assets}, manifest, directory)


if __name__ == "__main__":
    unittest.main()
