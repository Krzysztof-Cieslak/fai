#!/usr/bin/env python3
"""Prepare, qualify, and publish immutable Fai compiler releases (Python 3.11+)."""

import argparse
import datetime
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\Z")
HEADING = re.compile(r"^## \[([^\]]+)\](?: - (\d{4}-\d{2}-\d{2}))?\s*$", re.MULTILINE)


class ReleaseError(RuntimeError):
    """An invalid release or a failed publication precondition."""


def version_key(value):
    match = VERSION.fullmatch(value)
    if not match:
        raise ReleaseError(f"invalid exact compiler version: {value}")
    parts = match.group(4).split(".") if match.group(4) else []
    if any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in parts):
        raise ReleaseError(f"noncanonical prerelease version: {value}")
    return (*map(int, match.group(1, 2, 3)), not parts,
            tuple((0, int(part)) if part.isdigit() else (1, part) for part in parts))


def run(*args, root=ROOT, env=None):
    return subprocess.check_output(args, cwd=root, env=env, text=True, encoding="utf-8").strip()


def git(*args, root=ROOT):
    return run("git", *args, root=root)


def config(root=ROOT):
    data = json.loads((root / "scripts/release-targets.json").read_text(encoding="utf-8"))
    if data["schemaVersion"] != 1 or len(data["targets"]) != len({item["target"] for item in data["targets"]}):
        raise ReleaseError("invalid release target catalog")
    return data


def sections(text):
    matches = list(HEADING.finditer(text))
    if not matches or matches[0].group(1) != "Unreleased":
        raise ReleaseError("changelog must start with ## [Unreleased]")
    if len(re.findall(r"^## ", text, re.MULTILINE)) != len(matches):
        raise ReleaseError("invalid changelog version heading")
    result = {}
    previous = None
    for index, match in enumerate(matches):
        name, date = match.group(1, 2)
        if name in result:
            raise ReleaseError(f"duplicate changelog section: {name}")
        if name == "Unreleased":
            if date:
                raise ReleaseError("Unreleased must not have a release date")
        else:
            key = version_key(name)
            if date is None:
                raise ReleaseError(f"release {name} needs an ISO date")
            datetime.date.fromisoformat(date)
            if previous is not None and key >= previous:
                raise ReleaseError("changelog releases must be newest first")
            previous = key
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        result[name] = (text[match.end():end].strip(), match.start(), end)
    return result


def has_entries(body):
    return bool(re.search(r"^[-*] \S", body, re.MULTILINE))


def notes(text, version):
    version_key(version)
    parsed = sections(text)
    if version not in parsed or not has_entries(parsed[version][0]):
        raise ReleaseError(f"changelog has no nonempty release section for {version}")
    return parsed[version][0] + "\n"


def requires_changelog(paths):
    roots = {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rustfmt.toml", ".gitattributes", ".gitignore"}
    prefixes = ("crates/", "std/", "scripts/", "editors/", ".github/", ".cargo/", ".config/")
    return any(path in roots or path.startswith(prefixes) for path in paths if not path.startswith("packages/"))


def check_changelog(base, root=ROOT):
    sections((root / "changelog.md").read_text(encoding="utf-8"))
    if not base or not base.strip("0"):
        return {"ok": True, "comparison": "initial"}
    baseline = git("merge-base", base, "HEAD", root=root)
    paths = git("diff", "--name-only", "--no-renames", baseline, "HEAD", root=root).splitlines()
    if requires_changelog(paths) and "changelog.md" not in paths:
        raise ReleaseError("compiler, tooling, or std changes must update changelog.md under [Unreleased]")
    return {"ok": True, "base": baseline, "required": requires_changelog(paths)}


def workspace(root=ROOT):
    return tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]


def internal_packages(root=ROOT):
    return {tomllib.loads((root / member / "Cargo.toml").read_text(encoding="utf-8"))["package"]["name"]
            for member in workspace(root)["members"]}


def clean(root=ROOT):
    if git("status", "--porcelain", root=root):
        raise ReleaseError("release preparation/publication requires a clean committed checkout")


def validate(version, source_ref="HEAD", root=ROOT, ancestry=False):
    version_key(version)
    clean(root)
    commit = git("rev-parse", "--verify", "--end-of-options", f"{source_ref}^{{commit}}", root=root)
    if commit != git("rev-parse", "HEAD", root=root):
        raise ReleaseError("check out the selected release commit before validating")
    if workspace(root)["package"]["version"] != version:
        raise ReleaseError("requested release does not match Cargo.toml")
    lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
    names = internal_packages(root)
    locked = {item["name"]: item["version"] for item in lock["package"] if item["name"] in names and "source" not in item}
    if set(locked) != names or any(value != version for value in locked.values()):
        raise ReleaseError("Cargo.lock workspace versions do not match the release")
    changelog = (root / "changelog.md").read_text(encoding="utf-8")
    release_notes = notes(changelog, version)
    if has_entries(sections(changelog)["Unreleased"][0]):
        raise ReleaseError("move all Unreleased entries into the release section before publishing")
    tag = f"v{version}"
    existing = subprocess.run(["git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"], cwd=root, capture_output=True, text=True)
    if existing.returncode == 0 and existing.stdout.strip() != commit:
        raise ReleaseError("release tag already identifies a different commit")
    if ancestry:
        allowed = any(subprocess.run(["git", "merge-base", "--is-ancestor", commit, f"origin/{branch}"], cwd=root,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
                      for branch in config(root)["releaseBranches"])
        if not allowed:
            raise ReleaseError("release commit must belong to a configured protected release branch")
    return {"version": version, "tag": tag, "commit": commit, "notes": release_notes,
            "rust": tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"],
            "matrix": {"include": config(root)["targets"]}}


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".fai-release-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        if path.exists():
            os.chmod(temporary, path.stat().st_mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def prepare(version, root=ROOT):
    journal = Path(git("rev-parse", "--git-path", "fai-release-prepare.json", root=root))
    if not journal.is_absolute():
        journal = root / journal
    if journal.exists():
        pending = json.loads(journal.read_text(encoding="utf-8"))
        if pending["version"] != version:
            raise ReleaseError(f"resume preparation for {pending['version']} first")
    else:
        clean(root)
        if git("branch", "--show-current", root=root) in config(root)["releaseBranches"]:
            raise ReleaseError("prepare releases on a feature branch and merge through a pull request")
        current = workspace(root)["package"]["version"]
        if version in {"major", "minor", "patch"}:
            if "-" in current:
                raise ReleaseError("prerelease transitions require an exact version")
            core = list(map(int, current.split(".")))
            index = {"major": 0, "minor": 1, "patch": 2}[version]
            core[index] += 1
            for trailing in range(index + 1, 3):
                core[trailing] = 0
            version = ".".join(map(str, core))
        version_key(version)
        cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
        cargo, count = re.subn(r'(\[workspace\.package\]\s*\nversion\s*=\s*)"[^"]+"', rf'\g<1>"{version}"', cargo, count=1)
        if count != 1:
            raise ReleaseError("cannot locate workspace version")
        names = internal_packages(root)
        lock = (root / "Cargo.lock").read_text(encoding="utf-8")
        def replace_lock(match):
            block = match.group()
            data = tomllib.loads(block)["package"][0]
            if data["name"] in names and "source" not in data:
                block = re.sub(r'^version = "[^"]+"', f'version = "{version}"', block, count=1, flags=re.MULTILINE)
            return block
        lock = re.sub(r'\[\[package\]\][\s\S]*?(?=\[\[package\]\]|\Z)', replace_lock, lock)
        changelog = (root / "changelog.md").read_text(encoding="utf-8")
        parsed = sections(changelog)
        if version in parsed or not has_entries(parsed["Unreleased"][0]):
            raise ReleaseError("release needs new Unreleased entries and a new version heading")
        body, start, end = parsed["Unreleased"]
        date = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
        changelog = changelog[:start] + f"## [Unreleased]\n\n## [{version}] - {date}\n\n{body}\n\n" + changelog[end:]
        sections(changelog)
        pending = {"version": version, "files": {name: {"before": sha(root / name), "after": content}
                    for name, content in {"Cargo.toml": cargo, "Cargo.lock": lock, "changelog.md": changelog}.items()}}
        atomic_write(journal, json.dumps(pending).encode())
    for name, edit in pending["files"].items():
        if name not in {"Cargo.toml", "Cargo.lock", "changelog.md"}:
            raise ReleaseError("invalid release-preparation journal path")
        if sha(root / name) not in {edit["before"], hashlib.sha256(edit["after"].encode()).hexdigest()}:
            raise ReleaseError(f"{name} changed during preparation; journal retained")
    for name, edit in pending["files"].items():
        atomic_write(root / name, edit["after"].encode())
    journal.unlink()
    return {"version": pending["version"], "files": sorted(pending["files"]), "next": "review, commit, and merge a release-preparation PR"}


def make_archive(path, files):
    """Write only fixed regular-file members with normalized archive metadata."""
    if path.suffix == ".zip":
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, content in sorted(files.items()):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = (0o100755 if name.startswith("bin/") else 0o100644) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, content)
    else:
        with path.open("wb") as stream, gzip.GzipFile(filename="", fileobj=stream, mode="wb", mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, content in sorted(files.items()):
                    info = tarfile.TarInfo(name)
                    info.size = len(content)
                    info.mode = 0o755 if name.startswith("bin/") else 0o644
                    archive.addfile(info, io.BytesIO(content))


def pack(target, output, root=ROOT, build=True):
    version = workspace(root)["package"]["version"]
    version_key(version)
    spec = next((item for item in config(root)["targets"] if item["target"] == target), None)
    if spec is None:
        raise ReleaseError(f"unknown release target: {target}")
    rust_host = re.search(r"^host: (.+)$", run("rustc", "-vV", root=root), re.MULTILINE).group(1)
    if rust_host != target:
        raise ReleaseError(f"release build requires native {target}; rustc host is {rust_host}")
    environment = {**os.environ, "FAI_BUILD_PROFILE": "release"}
    if environment.get("CARGO_BUILD_TARGET", target) != target:
        raise ReleaseError("CARGO_BUILD_TARGET disagrees with release host")
    if spec["deploymentTarget"]:
        environment["MACOSX_DEPLOYMENT_TARGET"] = spec["deploymentTarget"]
    if "windows" in target:
        if "CARGO_ENCODED_RUSTFLAGS" in environment:
            environment["CARGO_ENCODED_RUSTFLAGS"] += "\x1f-C\x1ftarget-feature=+crt-static"
        else:
            environment["RUSTFLAGS"] = environment.get("RUSTFLAGS", "") + " -C target-feature=+crt-static"
    if build:
        subprocess.run(["cargo", "build", "--release", "--locked", "--target", target, "-p", "fai-cli", "-p", "fai-installer"],
                       cwd=root, env=environment, stdout=sys.stderr, check=True)
    target_dir = Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
    if not target_dir.is_absolute():
        target_dir = root / target_dir
    target_dir /= target
    binary_dir = target_dir / "release"
    suffix = ".exe" if "windows" in target else ""
    info = json.loads(run(str(binary_dir / f"fai{suffix}"), "build-info", root=root))
    import compiler
    if (info["version"] != version or info["target"] != target or info["profile"] != "release"
            or info["debugAssertions"] is not False or info["sourceId"] != compiler.source_id(root)):
        raise ReleaseError("compiler is not an optimized release build for the requested identity")
    output.mkdir(parents=True, exist_ok=True)
    report = {"version": version, "commit": git("rev-parse", "HEAD", root=root), "target": target,
              "dirty": bool(git("status", "--porcelain", root=root)),
              "minimumSystem": spec["minimumSystem"], "buildInfo": info}
    for kind, binary_name in (("compiler", "fai"), ("installer", "fai-installer")):
        binary = binary_dir / f"{binary_name}{suffix}"
        metadata = info if kind == "compiler" else json.loads(run(str(binary), "installer", "--version", "--message-format=json", root=root))
        if metadata["version"] != version or metadata["target"] != target:
            raise ReleaseError("installer identity does not match compiler")
        extension = "zip" if suffix else "tar.gz"
        archive = output / f"{binary_name}-{version}-{target}.{extension}"
        make_archive(archive, {f"bin/fai{suffix}": binary.read_bytes(), "LICENSE": (root / "LICENSE").read_bytes(),
                               "metadata.json": (json.dumps(metadata, sort_keys=True, indent=2) + "\n").encode()})
        report[kind] = {"name": archive.name, "size": archive.stat().st_size, "sha256": sha(archive), "executableSha256": sha(binary)}
    atomic_write(output / f"target-{target}.json", (json.dumps(report, sort_keys=True, indent=2) + "\n").encode())
    return report


def asset_names(manifest):
    return sorted(["release.json", "install.sh", "install.ps1", "SHA256SUMS"] +
                  [data[kind]["name"] for data in manifest["targets"].values() for kind in ("compiler", "installer")])


def validate_manifest(manifest, root=ROOT, complete=True):
    version = manifest["version"]
    version_key(version)
    version_key(manifest["minimumInstaller"])
    if (manifest["schemaVersion"] != 1 or manifest["kind"] != "fai-compiler"
            or manifest["tag"] != f"v{version}" or not re.fullmatch(r"[0-9a-f]{40}", manifest["commit"])):
        raise ReleaseError("invalid public release identity")
    expected = {item["target"] for item in config(root)["targets"]}
    if not manifest["targets"] or not set(manifest["targets"]) <= expected or (complete and set(manifest["targets"]) != expected):
        raise ReleaseError("incomplete or unexpected release target matrix")
    for target, entry in manifest["targets"].items():
        info = entry["buildInfo"]
        if (info["version"] != version or info["target"] != target or info["profile"] != "release"
                or info["debugAssertions"] is not False or not entry["minimumSystem"]):
            raise ReleaseError("invalid compiler build metadata")
        for field in ("sourceId", "toolBuildId", "backendBuildId"):
            if not re.fullmatch(r"[0-9a-f]{64}", info[field]):
                raise ReleaseError(f"invalid {field}")
        for kind, prefix in (("compiler", "fai"), ("installer", "fai-installer")):
            artifact = entry[kind]
            extension = "zip" if "windows" in target else "tar.gz"
            if (artifact["name"] != f"{prefix}-{version}-{target}.{extension}"
                    or type(artifact["size"]) is not int or not 0 < artifact["size"] < 2**31
                    or any(not re.fullmatch(r"[0-9a-f]{64}", artifact[key]) for key in ("sha256", "executableSha256"))):
                raise ReleaseError("invalid release artifact descriptor")


def assemble(version, source, output, root=ROOT):
    version_key(version)
    catalog = config(root)
    expected = {item["target"]: item for item in catalog["targets"]}
    targets = {}
    commit = git("rev-parse", "HEAD", root=root)
    output.mkdir(parents=True, exist_ok=True)
    for report_path in sorted(source.rglob("target-*.json")):
        report = json.loads(report_path.read_text(encoding="utf-8"))
        target = report["target"]
        if target not in expected or target in targets or report["version"] != version or report["commit"] != commit or report.get("dirty", True):
            raise ReleaseError("duplicate, unexpected, or mismatched release target")
        info = report["buildInfo"]
        if info["version"] != version or info["target"] != target or info["profile"] != "release" or info["debugAssertions"] is not False:
            raise ReleaseError("release target reports the wrong compiler identity")
        if report["minimumSystem"] != expected[target]["minimumSystem"]:
            raise ReleaseError("release target compatibility disagrees with catalog")
        for kind, prefix in (("compiler", "fai"), ("installer", "fai-installer")):
            artifact = report[kind]
            extension = "zip" if "windows" in target else "tar.gz"
            if artifact["name"] != f"{prefix}-{version}-{target}.{extension}":
                raise ReleaseError("invalid release asset filename")
            path = report_path.parent / artifact["name"]
            if path.is_symlink() or path.stat().st_size != artifact["size"] or sha(path) != artifact["sha256"]:
                raise ReleaseError(f"release artifact checksum mismatch: {path}")
            destination = output / path.name
            if path.resolve() != destination.resolve():
                shutil.copy2(path, destination)
        targets[target] = {key: report[key] for key in ("minimumSystem", "buildInfo", "compiler", "installer")}
    if set(targets) != set(expected):
        raise ReleaseError(f"incomplete release matrix; missing {sorted(set(expected) - set(targets))}")
    if len({item["buildInfo"]["sourceId"] for item in targets.values()}) != 1:
        raise ReleaseError("release targets were built from different source inputs")
    manifest = {"schemaVersion": 1, "kind": "fai-compiler", "version": version, "tag": f"v{version}",
                "commit": commit, "minimumInstaller": catalog["minimumInstaller"], "targets": targets}
    validate_manifest(manifest, root)
    atomic_write(output / "release.json", (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode())
    for name in ("install.sh", "install.ps1"):
        template = (root / "scripts/install" / name).read_text(encoding="utf-8")
        atomic_write(output / name, template.replace("@VERSION@", version).encode())
    sums = "".join(f"{sha(output / name)}  {name}\n" for name in asset_names(manifest) if name != "SHA256SUMS")
    atomic_write(output / "SHA256SUMS", sums.encode())
    return manifest


def gh_api(path, root=ROOT, missing=False):
    result = subprocess.run(["gh", "api", path], cwd=root, capture_output=True, text=True, encoding="utf-8")
    if result.returncode:
        if missing and "HTTP 404" in result.stderr:
            return None
        raise ReleaseError(result.stderr.strip())
    return json.loads(result.stdout)


def require_repository(root=ROOT):
    remote = git("remote", "get-url", "origin", root=root)
    prefixes = ("https://github.com/", "ssh://git@github.com/", "git@github.com:")
    repository = next((remote[len(prefix):].rstrip("/").removesuffix(".git") for prefix in prefixes if remote.startswith(prefix)), None)
    if repository is None or repository.casefold() != config(root)["repository"].casefold():
        raise ReleaseError("origin does not match the configured GitHub release repository")


def remote_tag(tag, root=ROOT):
    lines = git("ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}", root=root).splitlines()
    refs = dict(line.split(None, 1)[::-1] for line in lines)
    if f"refs/tags/{tag}" in refs and f"refs/tags/{tag}^{{}}" not in refs:
        raise ReleaseError("release tags must be annotated")
    return refs.get(f"refs/tags/{tag}^{{}}")


def verify_assets(release, manifest, directory):
    assets = {asset["name"]: asset for asset in release["assets"]}
    if set(assets) != set(asset_names(manifest)):
        raise ReleaseError("GitHub release asset set is incomplete or unexpected")
    for name, asset in assets.items():
        path = directory / name
        if path.exists():
            size, digest = path.stat().st_size, sha(path)
        else:
            declared = [target[kind] for target in manifest["targets"].values() for kind in ("compiler", "installer") if target[kind]["name"] == name]
            if len(declared) != 1:
                raise ReleaseError(f"missing metadata asset: {name}")
            size, digest = declared[0]["size"], declared[0]["sha256"]
        if asset["size"] != size or asset.get("digest") != f"sha256:{digest}":
            raise ReleaseError(f"GitHub release checksum mismatch: {name}")


def verify(version, directory=None, root=ROOT):
    version_key(version)
    require_repository(root)
    if directory is None:
        with tempfile.TemporaryDirectory(prefix="fai-release-verify-") as temporary:
            directory = Path(temporary)
            run("gh", "release", "download", f"v{version}", "--repo", config(root)["repository"], "--dir", str(directory),
                "--pattern", "release.json", "--pattern", "SHA256SUMS", "--pattern", "install.sh", "--pattern", "install.ps1", root=root)
            return verify(version, directory, root)
    manifest = json.loads((directory / "release.json").read_text(encoding="utf-8"))
    validate_manifest(manifest, root)
    if manifest["version"] != version:
        raise ReleaseError("release directory has a different version")
    repo = config(root)["repository"]
    release = gh_api(f"repos/{repo}/releases/tags/v{version}", root)
    if release["draft"] or remote_tag(f"v{version}", root) != manifest["commit"]:
        raise ReleaseError("release is not published at the expected commit")
    if release.get("immutable") is not True:
        raise ReleaseError("published release is not immutable; enable repository immutable releases before publication")
    verify_assets(release, manifest, directory)
    sums = dict(line.split("  ", 1)[::-1] for line in (directory / "SHA256SUMS").read_text().splitlines())
    if set(sums) != set(asset_names(manifest)) - {"SHA256SUMS"}:
        raise ReleaseError("published checksum file is incomplete")
    for name, digest in sums.items():
        asset = next(item for item in release["assets"] if item["name"] == name)
        if asset.get("digest") != f"sha256:{digest}":
            raise ReleaseError(f"published checksum file disagrees with {name}")
    if (release.get("body") or "").strip() != notes((root / "changelog.md").read_text(encoding="utf-8"), version).strip():
        raise ReleaseError("published release notes disagree with changelog")
    if release["prerelease"] != ("-" in version):
        raise ReleaseError("published prerelease status disagrees with version")
    return {"version": version, "commit": manifest["commit"], "url": release["html_url"], "assets": asset_names(manifest)}


def publish(version, directory, root=ROOT):
    if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch":
        raise ReleaseError("publication runs inside the manually dispatched Release workflow; use the dispatch command")
    require_repository(root)
    prepared = validate(version, root=root, ancestry=True)
    manifest = json.loads((directory / "release.json").read_text(encoding="utf-8"))
    validate_manifest(manifest, root)
    import compiler
    source_id = compiler.source_id(root)
    if any(target["buildInfo"]["sourceId"] != source_id for target in manifest["targets"].values()):
        raise ReleaseError("release artifacts do not match the selected compiler source inputs")
    if manifest["version"] != version or manifest["commit"] != prepared["commit"]:
        raise ReleaseError("release artifacts do not identify the tested source")
    if set(manifest["targets"]) != {item["target"] for item in config(root)["targets"]}:
        raise ReleaseError("release matrix is incomplete")
    sums = dict(line.split("  ", 1)[::-1] for line in (directory / "SHA256SUMS").read_text().splitlines())
    if set(sums) != set(asset_names(manifest)) - {"SHA256SUMS"} or any(sha(directory / name) != digest for name, digest in sums.items()):
        raise ReleaseError("release directory failed checksum verification")
    repo = config(root)["repository"]
    tag = prepared["tag"]
    remote = remote_tag(tag, root)
    if remote is not None and remote != prepared["commit"]:
        raise ReleaseError("remote tag already identifies a different commit")
    if remote is None:
        existing = subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/tags/{tag}"], cwd=root).returncode == 0
        if not existing:
            git("-c", "user.name=github-actions[bot]", "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
                "tag", "-a", tag, prepared["commit"], "-m", f"Fai {version}", root=root)
        git("push", "origin", f"refs/tags/{tag}", root=root)
        if remote_tag(tag, root) != prepared["commit"]:
            raise ReleaseError("remote tag verification failed")
    release = gh_api(f"repos/{repo}/releases/tags/{tag}", root, missing=True)
    if release and not release["draft"]:
        return verify(version, directory, root)
    with tempfile.TemporaryDirectory(prefix="fai-notes-") as temporary:
        note_file = Path(temporary) / "notes.md"
        note_file.write_text(prepared["notes"], encoding="utf-8")
        if not release:
            run("gh", "release", "create", tag, "--repo", repo, "--verify-tag", "--draft", "--title", f"Fai {version}", "--notes-file", str(note_file), root=root)
        else:
            run("gh", "release", "edit", tag, "--repo", repo, "--notes-file", str(note_file), root=root)
        run("gh", "release", "upload", tag, "--repo", repo, "--clobber", *[str(directory / name) for name in asset_names(manifest)], root=root)
    release = gh_api(f"repos/{repo}/releases/tags/{tag}", root)
    verify_assets(release, manifest, directory)
    latest = gh_api(f"repos/{repo}/releases/latest", root, missing=True)
    promote = "-" not in version
    if latest and promote:
        try:
            promote = version_key(version) > version_key(latest["tag_name"].removeprefix("v"))
        except ReleaseError:
            pass
    run("gh", "release", "edit", tag, "--repo", repo, "--draft=false",
        f"--prerelease={str('-' in version).lower()}", f"--latest={str(promote).lower()}", root=root)
    return verify(version, directory, root)


def dispatch(version, source_ref, root=ROOT):
    require_repository(root)
    prepared = validate(version, source_ref, root, ancestry=True)
    request = uuid.uuid4().hex
    repo = config(root)["repository"]
    run("gh", "workflow", "run", "release.yml", "--repo", repo, "--ref", "main", "-f", f"version={version}",
        "-f", f"source_ref={prepared['commit']}", "-f", f"request_id={request}", root=root)
    for _ in range(30):
        runs = json.loads(run("gh", "run", "list", "--repo", repo, "--workflow", "release.yml", "--event", "workflow_dispatch",
                              "--limit", "100", "--json", "databaseId,displayTitle,url", root=root))
        matches = [item for item in runs if request in item["displayTitle"]]
        if len(matches) == 1:
            return {"version": version, "commit": prepared["commit"], "runId": matches[0]["databaseId"], "url": matches[0]["url"]}
        time.sleep(2)
    raise ReleaseError(f"workflow dispatched with request ID {request}; its run is not visible yet; do not dispatch again blindly")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    preparation = commands.add_parser("prepare")
    preparation.add_argument("version")
    checks = commands.add_parser("check-changelog")
    checks.add_argument("--base", default=os.environ.get("CHANGE_BASE"))
    for name in ("validate", "dispatch"):
        child = commands.add_parser(name)
        child.add_argument("--version", required=True)
        child.add_argument("--source-ref", default="HEAD")
        if name == "validate":
            child.add_argument("--ancestry", action="store_true")
            child.add_argument("--github-output", action="store_true")
    packing = commands.add_parser("pack")
    packing.add_argument("--target", required=True)
    packing.add_argument("--output", type=Path, default=ROOT / "dist")
    packing.add_argument("--no-build", action="store_true")
    assembly = commands.add_parser("assemble")
    assembly.add_argument("--version", required=True)
    assembly.add_argument("--input", type=Path, required=True)
    assembly.add_argument("--output", type=Path, default=ROOT / "dist")
    for name in ("publish", "verify"):
        child = commands.add_parser(name)
        child.add_argument("--version", required=True)
        child.add_argument("--directory", type=Path, default=ROOT / "dist" if name == "publish" else None)
    for child in commands.choices.values():
        child.add_argument("--json", action="store_true", help="emit machine-readable output (also the default)")
    args = parser.parse_args()
    try:
        if args.command == "prepare": result = prepare(args.version)
        elif args.command == "check-changelog": result = check_changelog(args.base)
        elif args.command == "validate":
            result = validate(args.version, args.source_ref, ancestry=args.ancestry)
            if args.github_output:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                    for name in ("version", "tag", "commit", "rust", "matrix"):
                        print(f"{name}={json.dumps(result[name], separators=(',', ':')) if name == 'matrix' else result[name]}", file=stream)
        elif args.command == "dispatch": result = dispatch(args.version, args.source_ref)
        elif args.command == "pack": result = pack(args.target, args.output.resolve(), build=not args.no_build)
        elif args.command == "assemble": result = assemble(args.version, args.input.resolve(), args.output.resolve())
        elif args.command == "publish": result = publish(args.version, args.directory.resolve())
        else: result = verify(args.version, args.directory.resolve() if args.directory else None)
        print(json.dumps({"schemaVersion": 1, "ok": True, "result": result}, sort_keys=True))
    except (ReleaseError, OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(json.dumps({"schemaVersion": 1, "ok": False, "error": str(error)}))
        sys.exit(1)


if __name__ == "__main__":
    main()
