#!/usr/bin/env python3
"""Install and exercise packaged release bytes through a local GitHub-shaped mirror."""

import argparse
from functools import partial
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release
from smoke_compiler import daemon_pid


class Handler(http.server.SimpleHTTPRequestHandler):
    requests = 0

    def do_GET(self):
        type(self).requests += 1
        super().do_GET()

    def log_message(self, *args):
        pass


def put(directory, name, value):
    path = directory / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value), encoding="utf-8")


def previous_release(mirror, target, current):
    base = "https://github.com/Krzysztof-Cieslak/fai/releases"
    try:
        with urllib.request.urlopen(f"{base}/latest/download/release.json", timeout=60) as response:
            previous = json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            print("First-release qualification: no previous stable compiler is published.")
            return None
        raise
    release.validate_manifest(previous, complete=False)
    if previous["version"] == current or target not in previous["targets"]:
        return None
    asset = previous["targets"][target]["compiler"]
    directory = mirror / "download" / previous["tag"]
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / asset["name"]
    with urllib.request.urlopen(f"{base}/download/{previous['tag']}/{asset['name']}", timeout=120) as response, archive.open("wb") as stream:
        shutil.copyfileobj(response, stream)
    if release.sha(archive) != asset["sha256"] or archive.stat().st_size != asset["size"]:
        raise RuntimeError("previous stable compiler failed integrity verification")
    put(directory, "release.json", previous)
    return previous["version"]


def qualify(directory, target, fetch_previous=False):
    report = json.loads((directory / f"target-{target}.json").read_text(encoding="utf-8"))
    version = report["version"]
    manifest = {"schemaVersion": 1, "kind": "fai-compiler", "version": version, "tag": f"v{version}",
                "commit": report["commit"], "minimumInstaller": release.config()["minimumInstaller"],
                "targets": {target: {key: report[key] for key in ("minimumSystem", "buildInfo", "compiler", "installer")}}}
    base = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else "/tmp" if sys.platform == "darwin" else None
    with tempfile.TemporaryDirectory(prefix="fai-release-", dir=base) as temporary:
        root = Path(temporary).resolve()
        mirror = root / "mirror"
        tagged = mirror / "download" / f"v{version}"
        tagged.mkdir(parents=True)
        for kind in ("compiler", "installer"):
            shutil.copy2(directory / report[kind]["name"], tagged / report[kind]["name"])
        put(tagged, "release.json", manifest)
        put(mirror, "latest/download/release.json", manifest)
        put(mirror, "index.json", [{"tag_name": f"v{version}", "draft": False}])
        old = previous_release(mirror, target, version) if fetch_previous else None
        for name in ("install.sh", "install.ps1"):
            template = (release.ROOT / "scripts/install" / name).read_text(encoding="utf-8")
            (tagged / name).write_text(template.replace("@VERSION@", version), encoding="utf-8")
        (tagged / "SHA256SUMS").write_text("".join(f"{release.sha(path)}  {path.name}\n" for path in sorted(tagged.iterdir())), encoding="utf-8")
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(mirror)))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        home = root / "home space ü"
        home.mkdir()
        tools = root / "toolchains"
        environment = {**os.environ, "FAI_HOME": str(home), "FAI_TOOLCHAIN_DIR": str(tools),
                       "HOME": str(root / "user"), "USERPROFILE": str(root / "user"),
                       "FAI_RELEASE_BASE_URL": f"http://127.0.0.1:{server.server_port}",
                       "FAI_CACHE_DIR": str(root / "cache"), "FAI_RUNTIME_DIR": str(root / "runtime"),
                       "CARGO_HOME": str(root / "empty-cargo"), "RUSTUP_HOME": str(root / "empty-rustup")}
        environment.pop("FAI_TOOLCHAIN", None)
        environment.pop("FAI_TOOLCHAIN_PROBE", None)
        environment["PATH"] = os.pathsep.join(path for path in environment.get("PATH", "").split(os.pathsep)
                                              if ".cargo" not in path and ".rustup" not in path)
        Path(environment["HOME"]).mkdir()
        executable = "fai.exe" if os.name == "nt" else "fai"
        launcher = home / "bin" / executable
        project = root / "project"
        project.mkdir()
        text = f'manifest-version = 1\n[package]\nname = "release-smoke"\nversion = "0.1.0"\nfai-version = "{version}"\n[toolchain]\nversion = "{version}"\n'
        (project / "fai.toml").write_text(text, encoding="utf-8")
        (project / "src").mkdir()
        (project / "src/Main.fai").write_text('module Main\nexample: 40 + 2 = 42\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine "release works"\n', encoding="utf-8")

        def invoke(*args, success=True, env=None, cwd=project):
            result = subprocess.run([str(launcher), *args], cwd=cwd, env=env or environment,
                                    capture_output=True, text=True, encoding="utf-8", timeout=180)
            if success and result.returncode:
                raise AssertionError(f"{args}: {result.stdout}\n{result.stderr}")
            if not success and not result.returncode:
                raise AssertionError(f"expected {args} to fail")
            return result.stdout

        try:
            if os.name == "nt":
                for shell in ("powershell.exe", "pwsh.exe"):
                    if not shutil.which(shell):
                        raise AssertionError(f"release qualification requires {shell}")
                    subprocess.run([shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File",
                                    str(tagged / "install.ps1"), "-Version", version, "-NoModifyPath"],
                                   cwd=root, env=environment, check=True, timeout=180)
            else:
                subprocess.run(["sh", str(tagged / "install.sh"), "--version", version, "--no-modify-path"],
                               cwd=root, env=environment, check=True, timeout=180)
            assert version in invoke("--version")
            info = json.loads(invoke("build-info"))
            assert info == report["buildInfo"]
            invoke("toolchain", "install", version)
            concurrent = {**environment, "FAI_HOME": str(root / "concurrent-home"), "FAI_TOOLCHAIN_DIR": str(root / "concurrent-tools")}
            processes = [subprocess.Popen([str(launcher), "toolchain", "install", version], cwd=root, env=concurrent,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) for _ in range(2)]
            for process in processes:
                stdout, stderr = process.communicate(timeout=180)
                assert process.returncode == 0, (stdout, stderr)
            inventory = json.loads((root / "concurrent-tools/installed.json").read_text(encoding="utf-8"))
            assert len(inventory["compilers"]) == 1
            assert not (root / "concurrent-tools/transaction.json").exists()
            if old:
                invoke("toolchain", "install", old)
                invoke("toolchain", "default", old)
                assert old in invoke("--version", cwd=root)
                assert version in invoke("--version")
                invoke("toolchain", "default", version)
            selected = json.loads(invoke("toolchain", "which", "--message-format=json"))
            assert selected["result"]["reason"] == "project"
            invoke("check", "--no-examples")
            before = daemon_pid(invoke("daemon", "status"))
            invoke("check", "--no-examples")
            assert daemon_pid(invoke("daemon", "status")) == before
            invoke("test")
            assert invoke("run") == "release works\n"
            invoke("build", "--out", str(root / "program"))
            program = root / ("program.exe" if os.name == "nt" else "program")
            assert subprocess.check_output([str(program)], env=environment, text=True) == "release works\n"
            lsp = subprocess.Popen([str(launcher), "lsp", "-C", str(project)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=environment)
            messages = [
                {"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"processId":None,"rootUri":project.as_uri(),"capabilities":{}}},
                {"jsonrpc":"2.0", "method":"initialized", "params":{}},
                {"jsonrpc":"2.0", "id":2, "method":"shutdown", "params":None},
                {"jsonrpc":"2.0", "method":"exit", "params":None},
            ]
            wire = b""
            for message in messages:
                body = json.dumps(message).encode()
                wire += f"Content-Length: {len(body)}\r\n\r\n".encode() + body
            output, error = lsp.communicate(wire, timeout=120)
            assert lsp.returncode == 0 and b'"capabilities"' in output, error
            before = Handler.requests
            (project / "fai.toml").write_text(text.replace(f'version = "{version}"', 'version = "99.99.99"'), encoding="utf-8")
            invoke("check", success=False)
            assert Handler.requests == before, "a compiler invocation tried to download a missing pin"
            (project / "fai.toml").write_text(text, encoding="utf-8")
            if "-" not in version:
                invoke("upgrade")
            assert (project / "fai.toml").read_text(encoding="utf-8") == text
            # A corrupt transport must never publish a new inventory or default.
            archive = tagged / report["compiler"]["name"]
            with archive.open("ab") as stream:
                stream.write(b"corruption")
            failed = {**environment, "FAI_HOME": str(root / "failed-home"), "FAI_TOOLCHAIN_DIR": str(root / "failed-tools")}
            invoke("toolchain", "install", version, success=False, env=failed)
            assert not (root / "failed-tools/installed.json").exists()
            if "-" not in version:
                invoke("installer", "upgrade")
                for _ in range(60):
                    if not (home / "installer.pending.json").exists():
                        break
                    time.sleep(0.25)
            assert not (home / "installer.pending.json").exists(), "installer replacement remained pending"
            assert json.loads(invoke("installer", "--version", "--message-format=json"))["version"] == version
            if old:
                invoke("toolchain", "uninstall", old)
        finally:
            payload = tools / f"{version}-{target}" / "bin" / executable
            if payload.exists():
                subprocess.run([str(payload), "daemon", "stop", "-C", str(project)], env=environment, capture_output=True, timeout=30)
                # The stop acknowledgement precedes final executable-handle release.
                deadline = time.monotonic() + 30
                while True:
                    try:
                        payload.unlink()
                        break
                    except PermissionError:
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(0.1)
            server.shutdown()
            server.server_close()
            thread.join()
    print(f"Release qualification passed: {target}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--previous-stable", action="store_true")
    args = parser.parse_args()
    qualify(args.directory.resolve(), args.target, args.previous_stable)


if __name__ == "__main__":
    main()
