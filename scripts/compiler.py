#!/usr/bin/env python3
"""Content-addressed, relocatable Fai compiler bundles (Python 3.11+)."""

from contextlib import contextmanager
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PROFILE = "package-dev"
SCHEMA = 1


class BundleError(RuntimeError):
    """A compiler bundle is absent, incompatible, or incomplete."""


def source_files(root=ROOT):
    spec = json.loads((root / "scripts/compiler-inputs.json").read_text())
    if spec["schemaVersion"] != SCHEMA:
        raise BundleError("unsupported compiler input manifest")
    paths = {root / name for name in spec["files"]}
    for name in spec["trees"]:
        tree = root / name
        if not tree.exists():
            continue
        for directory, dirs, files in os.walk(tree):
            dirs[:] = sorted(d for d in dirs if d not in spec["ignoredDirectories"])
            if any((Path(directory) / name).is_symlink() for name in dirs):
                raise BundleError("compiler source directories must not be symlinks")
            paths.update(
                Path(directory) / name for name in files
                if not any(name.endswith(suffix) for suffix in spec["ignoredSuffixes"])
            )
    return sorted(paths, key=lambda path: path.relative_to(root).as_posix())


def source_id(root=ROOT):
    digest = hashlib.sha256(b"fai-tool-sources-v1\0")
    for path in source_files(root):
        if path.is_symlink():
            raise BundleError("compiler source files must not be symlinks")
        name = path.relative_to(root).as_posix().encode("utf-8")
        content = path.read_bytes()
        digest.update(struct.pack("<Q", len(name)))
        digest.update(name)
        digest.update(struct.pack("<Q", len(content)))
        digest.update(content)
    return digest.hexdigest()


def host():
    machine = platform.machine().lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    system = platform.system()
    if system == "Linux":
        libc, version = platform.libc_ver()
        if libc != "glibc":
            raise BundleError("automatic compiler bundles currently require glibc on Linux")
        return f"{arch}-unknown-linux-gnu", f"linux-{arch}-{libc}-{version}"
    if system == "Darwin":
        return f"{arch}-apple-darwin", f"macos-{arch}-{platform.mac_ver()[0]}"
    if system == "Windows":
        return f"{arch}-pc-windows-msvc", f"windows-{arch}-msvc"
    raise BundleError(f"unsupported compiler host: {system}/{arch}")


def build_environment(environ=None):
    environ = os.environ if environ is None else environ
    exact = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTUP_TOOLCHAIN", "RUSTC", "RUSTC_WRAPPER",
             "RUSTC_WORKSPACE_WRAPPER", "RUSTC_BOOTSTRAP", "CC", "CFLAGS", "AR", "CXX", "CXXFLAGS",
             "LDFLAGS", "MACOSX_DEPLOYMENT_TARGET", "SDKROOT", "DEVELOPER_DIR", "CARGO_BUILD_TARGET",
             "CARGO_BUILD_RUSTFLAGS"}
    prefixes = ("CARGO_PROFILE_", "CARGO_TARGET_", "CC_", "CFLAGS_", "AR_", "CXX_", "CXXFLAGS_")
    return {key: value for key, value in sorted(environ.items())
            if key in exact or key.startswith(prefixes)}


def identity(root=ROOT, environ=None, host_info=None):
    target, compatibility = host() if host_info is None else host_info
    result = {"schemaVersion": SCHEMA, "sourceId": source_id(root), "target": target,
              "compatibility": compatibility, "profile": PROFILE,
              "environment": build_environment(environ)}
    result["key"] = hashlib.sha256(json.dumps(result, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return result


def cache_root():
    if "FAI_COMPILER_CACHE" in os.environ:
        return Path(os.environ["FAI_COMPILER_CACHE"]).expanduser().resolve()
    base = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    if os.name == "nt":
        base = Path(os.environ.get("LOCALAPPDATA", base))
    return base / "fai" / "compilers"


def executable_name(expected):
    return "fai.exe" if "windows" in expected["target"] else "fai"


def file_digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate(directory, expected, checksums=True):
    try:
        manifest = json.loads((directory / "manifest.json").read_text())
        if manifest["identity"] != expected:
            raise BundleError("compiler bundle identity does not match this checkout and host")
        name = executable_name(expected)
        if set(manifest["files"]) != {name}:
            raise BundleError("unexpected files in compiler bundle manifest")
        binary = directory / name
        info = manifest["buildInfo"]
        if info["sourceId"] != expected["sourceId"] or info["target"] != expected["target"] or info["profile"] != PROFILE or info["debugAssertions"] is not True:
            raise BundleError("compiler provenance does not match the requested bundle")
        entry = manifest["files"][name]
        if binary.is_symlink() or binary.stat().st_size != entry["size"]:
            raise BundleError("compiler bundle is incomplete")
        if checksums and file_digest(binary) != entry["sha256"]:
            raise BundleError("compiler bundle checksum mismatch")
        return binary
    except (OSError, KeyError, TypeError, ValueError) as error:
        raise BundleError(f"invalid compiler bundle at {directory}: {error}") from error


@contextmanager
def bundle_lock(cache, key):
    cache.mkdir(parents=True, exist_ok=True)
    with (cache / f".{key}.lock").open("a+b") as stream:
        if os.name == "nt":
            import msvcrt
            stream.seek(0, os.SEEK_END)
            if stream.tell() == 0:
                stream.write(b"\0")
                stream.flush()
            stream.seek(0)
            while True:
                try:
                    msvcrt.locking(stream.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    time.sleep(0.1)
        else:
            import fcntl
            fcntl.flock(stream, fcntl.LOCK_EX)
        try:
            yield
        finally:
            if os.name == "nt":
                stream.seek(0)
                msvcrt.locking(stream.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(stream, fcntl.LOCK_UN)


def install(directory, cache, expected):
    validate(directory, expected)
    destination = cache / expected["key"]
    if destination.exists():
        try:
            return validate(destination, expected)
        except BundleError:
            pass
        shutil.rmtree(destination)
    directory.rename(destination)
    return destination / executable_name(expected)


def restore(archive, cache, expected):
    cache.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".restore-", dir=cache) as temporary:
        directory = Path(temporary) / "bundle"
        directory.mkdir()
        with zipfile.ZipFile(archive) as zipped:
            names = zipped.namelist()
            allowed = {"manifest.json", executable_name(expected)}
            if set(names) != allowed or len(names) != len(allowed):
                raise BundleError("unexpected or unsafe compiler archive members")
            for name in names:
                with zipped.open(name) as source, (directory / name).open("wb") as destination:
                    shutil.copyfileobj(source, destination)
        binary = directory / executable_name(expected)
        binary.chmod(0o755)
        return install(directory, cache, expected)


def build(cache, expected, root=ROOT):
    environment = {**os.environ, "FAI_BUILD_PROFILE": PROFILE,
                   "FAI_TOOL_INPUT_ID": expected["sourceId"]}
    if environment.get("CARGO_BUILD_TARGET", expected["target"]) != expected["target"]:
        raise BundleError("compiler bootstrap requires a native target")
    print(f"Building Fai compiler {expected['key'][:12]} ({PROFILE})", file=sys.stderr)
    subprocess.run(["cargo", "build", "--locked", "--profile", PROFILE, "-p", "fai-cli"],
                   cwd=root, env=environment, stdout=sys.stderr, check=True)
    if identity(root) != expected:
        raise BundleError("compiler inputs changed during the build; retry with the new inputs")
    target = Path(environment.get("CARGO_TARGET_DIR", root / "target"))
    if not target.is_absolute():
        target = root / target
    if "CARGO_BUILD_TARGET" in environment:
        target /= environment["CARGO_BUILD_TARGET"]
    binary = target / PROFILE / executable_name(expected)
    info = json.loads(subprocess.check_output([str(binary), "build-info"], cwd=root, text=True))
    manifest = {"identity": expected, "buildInfo": info,
                "files": {binary.name: {"size": binary.stat().st_size, "sha256": file_digest(binary)}}}
    with tempfile.TemporaryDirectory(prefix=".build-", dir=cache) as temporary:
        directory = Path(temporary) / "bundle"
        directory.mkdir()
        shutil.copy2(binary, directory / binary.name)
        (directory / "manifest.json").write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
        return install(directory, cache, expected)


def gh_json(path):
    return json.loads(subprocess.check_output(["gh", "api", path], text=True, stderr=subprocess.DEVNULL))


def acceptable_producer(run, metadata, root, event=None):
    if run["conclusion"] != "success" or run["path"] != ".github/workflows/ci.yml":
        return False
    if run["event"] == "push" and run["head_branch"] == metadata["default_branch"]:
        return True
    if event == "push" or run["event"] != "pull_request" or run["head_repository"]["id"] != metadata["id"]:
        return False
    return subprocess.run(["git", "merge-base", "--is-ancestor", run["head_sha"], "HEAD"],
                          cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def download(cache, expected, root=ROOT):
    """Reuse successful default-branch or ancestral same-repository PR bundles."""
    if not shutil.which("gh"):
        return None
    try:
        repo = subprocess.check_output(["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"],
                                       cwd=root, text=True, stderr=subprocess.DEVNULL).strip()
        metadata = gh_json(f"repos/{repo}")
        artifacts = gh_json(f"repos/{repo}/actions/artifacts?name=fai-compiler-{expected['key']}&per_page=100")
        for artifact in artifacts["artifacts"]:
            if artifact["expired"]:
                continue
            run = gh_json(f"repos/{repo}/actions/runs/{artifact['workflow_run']['id']}")
            if not acceptable_producer(run, metadata, root, os.environ.get("GITHUB_EVENT_NAME")):
                continue
            with tempfile.TemporaryDirectory(prefix=".download-", dir=cache) as temporary:
                archive = Path(temporary) / "artifact.zip"
                with archive.open("wb") as output:
                    subprocess.run(["gh", "api", "--allow-escape-sequences", f"repos/{repo}/actions/artifacts/{artifact['id']}/zip"],
                                   stdout=output, stderr=subprocess.DEVNULL, check=True)
                # upload-artifact stores the portable zip as its one payload file.
                with zipfile.ZipFile(archive) as outer:
                    if outer.namelist() != ["compiler.zip"]:
                        continue
                    payload = Path(temporary) / "compiler.zip"
                    payload.write_bytes(outer.read("compiler.zip"))
                binary = restore(payload, cache, expected)
                print(f"Restored compiler bundle from run {run['id']}", file=sys.stderr)
                return binary
    except (subprocess.CalledProcessError, OSError, KeyError, ValueError, zipfile.BadZipFile, BundleError) as error:
        print(f"No reusable remote compiler bundle ({type(error).__name__})", file=sys.stderr)
    return None


def ensure(root=ROOT, cache=None, offline=False, no_build=False):
    cache = cache_root() if cache is None else cache
    expected = identity(root)
    try:
        return validate(cache / expected["key"], expected)
    except BundleError:
        pass
    with bundle_lock(cache, expected["key"]):
        try:
            return validate(cache / expected["key"], expected)
        except BundleError:
            pass
        if not offline:
            binary = download(cache, expected, root)
            if binary:
                return binary
        if no_build:
            raise BundleError(f"no compiler bundle for {expected['key']}")
        return build(cache, expected, root)


def pack(binary, output):
    directory = binary.parent
    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as zipped:
        zipped.write(directory / "manifest.json", "manifest.json")
        zipped.write(binary, binary.name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("key", "ensure", "pack", "restore"))
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args()
    cache = args.cache_dir.resolve() if args.cache_dir else cache_root()
    expected = identity()
    try:
        if args.action == "key":
            values = {**expected, "cacheDir": str(cache / expected["key"]),
                      "binary": str(cache / expected["key"] / executable_name(expected)),
                      "artifactName": f"fai-compiler-{expected['key']}"}
            if args.github_output:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                    for name in ("key", "cacheDir", "binary", "artifactName"):
                        print(f"{name}={values[name]}", file=stream)
            else:
                print(json.dumps(values, sort_keys=True))
            return
        if args.action == "restore":
            if not args.archive:
                parser.error("restore requires --archive")
            with bundle_lock(cache, expected["key"]):
                binary = restore(args.archive, cache, expected)
        else:
            binary = ensure(cache=cache, offline=args.offline, no_build=args.no_build)
            if args.action == "pack":
                if not args.output:
                    parser.error("pack requires --output")
                pack(binary, args.output)
        print(binary)
    except (BundleError, OSError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
        print(f"compiler bundle: {error}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
