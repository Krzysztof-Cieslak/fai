#!/usr/bin/env python3
"""Validate and publish prepared source packages with a released Fai compiler."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

import packages
import release

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=packages.unique_object)


def write_json(path, value):
    release.atomic_write(Path(path), (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


def prepared_packages(selection, root=ROOT):
    catalog = packages.discover(root)
    metadata = {name: tomllib.loads((package.directory / "fai.toml").read_text(encoding="utf-8"))["package"]
                for name, package in catalog.items()}
    for name, item in metadata.items():
        if type(item.get("publish", True)) is not bool:
            raise release.ReleaseError(f"{name}: package.publish must be a boolean")
    if selection == "all":
        selected = {name: item["version"] for name, item in metadata.items() if item.get("publish", True)}
    else:
        selected = json.loads(selection, object_pairs_hook=packages.unique_object)
    if not isinstance(selected, dict) or not selected:
        raise release.ReleaseError("packages must be 'all' or a nonempty JSON object of package names and exact versions")
    unknown = selected.keys() - catalog.keys()
    if unknown:
        raise release.ReleaseError(f"unknown packages: {', '.join(sorted(unknown))}")
    result = []
    for name, version in sorted(selected.items()):
        if not metadata[name].get("publish", True):
            raise release.ReleaseError(f"{name}: private example/test projects cannot be released")
        private_dependencies = [dependency for dependency in catalog[name].all_dependencies
                                if not metadata[dependency].get("publish", True)]
        if private_dependencies:
            raise release.ReleaseError(f"{name}: publication depends on private projects: {', '.join(private_dependencies)}")
        if not isinstance(version, str):
            raise release.ReleaseError(f"{name}: version must be an exact version string")
        release.version_key(version)
        directory = catalog[name].directory
        item = metadata[name]
        if item["version"] != version:
            raise release.ReleaseError(f"{name}: prepare version {version} in a pull request before releasing")
        if not item.get("fai-version") or item["fai-version"] == "*":
            raise release.ReleaseError(f"{name}: an explicit compiler requirement is required")
        readme = directory / "README.md"
        changelog = directory / "changelog.md"
        if readme.is_symlink() or not readme.is_file() or not readme.read_text(encoding="utf-8").strip():
            raise release.ReleaseError(f"{name}: a nonempty README.md is required")
        if changelog.is_symlink() or not changelog.is_file():
            raise release.ReleaseError(f"{name}: changelog.md is required")
        text = changelog.read_text(encoding="utf-8")
        notes = release.notes(text, version)
        if release.has_entries(release.sections(text)["Unreleased"][0]):
            raise release.ReleaseError(f"{name}: move Unreleased entries into the prepared release section")
        path = directory.relative_to(root).as_posix()
        result.append({"name": name, "path": path, "version": version,
                       "tag": f"{path}/v{version}", "compilerRequirement": item["fai-version"],
                       "notes": notes})
    return result


def source_identity(source_ref, root=ROOT):
    release.clean(root)
    commit = release.git("rev-parse", "--verify", "--end-of-options", f"{source_ref}^{{commit}}", root=root)
    if commit != release.git("rev-parse", "HEAD", root=root):
        raise release.ReleaseError("check out the exact selected source commit")
    return commit, release.git("rev-parse", "HEAD^{tree}", root=root)


def require_merged(commit, root=ROOT):
    release.require_repository(root)
    result = subprocess.run(["git", "merge-base", "--is-ancestor", commit, "origin/main"], cwd=root)
    if result.returncode:
        raise release.ReleaseError("package releases must use a commit already merged into origin/main")


def request(selection, compiler_version, source_ref="HEAD", root=ROOT):
    release.version_key(compiler_version)
    commit, tree = source_identity(source_ref, root)
    return {"schemaVersion": 1, "commit": commit, "tree": tree, "compilerVersion": compiler_version,
            "targets": list(TARGETS), "packages": prepared_packages(selection, root)}


def verify_request(value, root=ROOT):
    if not isinstance(value, dict) or value.get("schemaVersion") != 1:
        raise release.ReleaseError("unsupported package release request")
    selection = json.dumps({item["name"]: item["version"] for item in value["packages"]})
    if request(selection, value["compilerVersion"], value["commit"], root) != value:
        raise release.ReleaseError("release request no longer matches the committed package metadata")


def prepare(selection, compiler_version, source_ref, output, root=ROOT):
    value = request(selection, compiler_version, source_ref, root)
    require_merged(value["commit"], root)
    write_json(output, value)
    return value


def install_compiler(value, destination, root=ROOT):
    """Verify published bootstrap bytes before installing an exact compiler."""
    verify_request(value, root)
    version = value["compilerVersion"]
    destination = Path(destination).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    windows = os.name == "nt"
    script = "install.ps1" if windows else "install.sh"
    with tempfile.TemporaryDirectory(prefix="fai-package-compiler-") as temporary:
        files = Path(temporary)
        release.run("gh", "release", "download", f"v{version}", "--repo", release.config(root)["repository"],
                    "--dir", str(files), "--pattern", "release.json", "--pattern", "SHA256SUMS",
                    "--pattern", "install.sh", "--pattern", "install.ps1", root=root)
        release.verify(version, files, root)
        manifest = read_json(files / "release.json")
        environment = {**os.environ, "FAI_HOME": str(destination),
                       "FAI_TOOLCHAIN_DIR": str(destination / "toolchains")}
        environment.pop("FAI_RELEASE_BASE_URL", None)
        if windows:
            command = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File",
                       str(files / script), "-Version", version, "-HomeDirectory", str(destination), "-NoModifyPath"]
        else:
            command = ["sh", str(files / script), "--version", version, "--home", str(destination), "--no-modify-path"]
        subprocess.run(command, cwd=root, env=environment, check=True, timeout=600)
        launcher = destination / "bin" / ("fai.exe" if windows else "fai")
        info = json.loads(release.run(str(launcher), f"+{version}", "build-info", root=root, env=environment))
        target = info.get("target")
        if target not in value["targets"] or info != manifest["targets"][target]["buildInfo"]:
            raise release.ReleaseError("installed compiler does not match the exact published build")
    write_json(destination / "release.json", manifest)
    return launcher, environment, info


def verify_plan(value, plan, target):
    expected = {(item["name"], item["path"], item["version"], item["tag"]) for item in value["packages"]}
    actual = [(item["name"], item["path"], item["version"], item["tag"]) for item in plan.get("packages", [])]
    if (plan.get("schemaVersion") != 1 or plan.get("ok") is not True or plan.get("status") != "planned"
            or plan.get("commit") != value["commit"] or plan.get("tree") != value["tree"]
            or plan.get("validatedHost") != target or plan.get("remote") != "origin" or plan.get("branch") != "main"
            or len(actual) != len(expected) or set(actual) != expected):
        raise release.ReleaseError("compiler release plan disagrees with the prepared source, packages, or validation host")


def validate(value, destination, output, root=ROOT):
    verify_request(value, root)
    require_merged(value["commit"], root)
    launcher, environment, info = install_compiler(value, destination, root)
    intents = [f"{item['name']}={item['version']}" for item in value["packages"]]
    # The publisher needs a branch even for a read-only plan. It never pushes here.
    release.git("switch", "-c", "fai-package-validation", root=root)
    command = [str(launcher), f"+{value['compilerVersion']}", "--no-daemon", "-C", "packages", "publish",
               *intents, "--plan", "--remote", "origin", "--branch", "main", "--message-format=json"]
    completed = subprocess.run(command, cwd=root, env=environment, capture_output=True, text=True, encoding="utf-8")
    if completed.returncode:
        raise release.ReleaseError(f"candidate validation failed:\n{completed.stdout}\n{completed.stderr}")
    plan = json.loads(completed.stdout)
    verify_plan(value, plan, info["target"])
    verify_request(value, root)
    report = {"schemaVersion": 1, "request": value, "compiler": info, "plan": plan}
    write_json(output, report)
    return report


def verify_reports(value, reports):
    found = {}
    source_ids = set()
    for report in reports:
        if report.get("schemaVersion") != 1 or report.get("request") != value:
            raise release.ReleaseError("qualification report belongs to a different release request")
        info = report["compiler"]
        target = info.get("target")
        if target not in value["targets"] or target in found:
            raise release.ReleaseError("duplicate or unexpected qualification host")
        if info.get("version") != value["compilerVersion"] or info.get("profile") != "release" or info.get("debugAssertions") is not False:
            raise release.ReleaseError("qualification must use the requested optimized published compiler")
        source_ids.add(info["sourceId"])
        verify_plan(value, report["plan"], target)
        found[target] = report
    if set(found) != set(value["targets"]) or len(source_ids) != 1:
        raise release.ReleaseError("all three native qualifications must use the same compiler source identity")


def release_body(item, compiler_version):
    return f"Validated with Fai {compiler_version}. Supported compiler range: `{item['compilerRequirement']}`.\n\n{item['notes']}"


def verify_github_release(item, published, compiler_version):
    if (published.get("draft") or published.get("immutable") is not True
            or published.get("prerelease") != ("-" in item["version"])
            or (published.get("body") or "").strip() != release_body(item, compiler_version).strip()):
        raise release.ReleaseError(f"published release {item['tag']} conflicts with the validated request")


def publish(value, reports, output, root=ROOT):
    if (os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or os.environ.get("GITHUB_REF") != "refs/heads/main"):
        raise release.ReleaseError("publish runs only in the manually dispatched package workflow on main")
    verify_request(value, root)
    require_merged(value["commit"], root)
    verify_reports(value, reports)
    repo = release.config(root)["repository"]
    existing = {}
    for item in value["packages"]:
        tagged = release.remote_tag(item["tag"], root)
        if tagged is not None and tagged != value["commit"]:
            raise release.ReleaseError(f"{item['tag']} already identifies a different commit")
        published = release.github_release(item["tag"], root, missing=True)
        if published is not None:
            verify_github_release(item, published, value["compilerVersion"])
            if tagged != value["commit"]:
                raise release.ReleaseError("published release has no matching source tag")
        existing[item["tag"]] = published
    references = []
    for item in value["packages"]:
        reference = f"refs/tags/{item['tag']}"
        local = subprocess.run(["git", "rev-parse", "--verify", f"{reference}^{{commit}}"], cwd=root,
                               capture_output=True, text=True)
        if local.returncode == 0:
            if local.stdout.strip() != value["commit"]:
                raise release.ReleaseError(f"local {item['tag']} identifies a different commit")
        else:
            release.git("-c", "user.name=github-actions[bot]", "-c",
                        "user.email=41898282+github-actions[bot]@users.noreply.github.com", "tag", "-a",
                        item["tag"], value["commit"], "-m", release_body(item, value["compilerVersion"]), root=root)
        references.append(f"{reference}:{reference}")
    # Only tags are pushed: prepared source already reached main through its PR.
    pushed = subprocess.run(["git", "push", "--atomic", "origin", *references], cwd=root)
    if pushed.returncode and any(release.remote_tag(item["tag"], root) != value["commit"] for item in value["packages"]):
        raise release.ReleaseError("atomic tag publication failed; retry the same source and exact versions")
    result = []
    for item in value["packages"]:
        if existing[item["tag"]] is None:
            with tempfile.TemporaryDirectory(prefix="fai-package-notes-") as temporary:
                notes = Path(temporary) / "notes.md"
                notes.write_text(release_body(item, value["compilerVersion"]), encoding="utf-8")
                command = ["gh", "release", "create", item["tag"], "--repo", repo, "--verify-tag", "--latest=false",
                           "--title", f"{item['name']} {item['version']}", "--notes-file", str(notes)]
                if "-" in item["version"]:
                    command.append("--prerelease")
                release.run(*command, root=root)
        published = release.github_release(item["tag"], root)
        verify_github_release(item, published, value["compilerVersion"])
        if release.remote_tag(item["tag"], root) != value["commit"]:
            raise release.ReleaseError("published package tag does not match its qualified source")
        result.append({"name": item["name"], "version": item["version"], "tag": item["tag"], "url": published["html_url"]})
    output_value = {"schemaVersion": 1, "commit": value["commit"], "packages": result}
    write_json(output, output_value)
    return output_value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="check prepared versions, READMEs, and changelogs locally")
    check.add_argument("--packages", default="all")
    prepare_command = commands.add_parser("prepare")
    prepare_command.add_argument("--packages", required=True)
    prepare_command.add_argument("--compiler-version", required=True)
    prepare_command.add_argument("--source-ref", required=True)
    prepare_command.add_argument("--output", required=True)
    prepare_command.add_argument("--github-output", action="store_true")
    validation = commands.add_parser("validate")
    validation.add_argument("--request", required=True)
    validation.add_argument("--compiler-home", required=True)
    validation.add_argument("--output", required=True)
    publication = commands.add_parser("publish")
    publication.add_argument("--request", required=True)
    publication.add_argument("--reports", required=True)
    publication.add_argument("--output", required=True)
    args = parser.parse_args()
    try:
        if args.command == "check":
            result = prepared_packages(args.packages)
        elif args.command == "prepare":
            result = prepare(args.packages, args.compiler_version, args.source_ref, args.output)
            if args.github_output:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                    print(f"commit={result['commit']}", file=stream)
        elif args.command == "validate":
            result = validate(read_json(args.request), args.compiler_home, args.output)
        else:
            reports = [read_json(path) for path in sorted(Path(args.reports).rglob("qualification.json"))]
            result = publish(read_json(args.request), reports, args.output)
        print(json.dumps(result, sort_keys=True))
    except (release.ReleaseError, packages.PackageError, OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Package release: {error}\n")


if __name__ == "__main__":
    main()
