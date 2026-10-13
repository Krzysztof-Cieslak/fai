#!/usr/bin/env python3
"""Exercise a relocated compiler with empty Rust homes and a warm daemon."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", type=Path, required=True)
    parser.add_argument("--other-fai", type=Path)
    parser.add_argument("--launcher", type=Path)
    args = parser.parse_args()
    binary = args.fai.resolve()
    if not binary.exists() and binary.with_suffix(".exe").exists():
        binary = binary.with_suffix(".exe")
    original = json.loads(subprocess.check_output([str(binary), "build-info"], text=True))
    with tempfile.TemporaryDirectory(prefix="fai-portable-") as temporary:
        root = Path(temporary)
        relocated = root / binary.name
        shutil.copy2(binary, relocated)
        (root / "Main.fai").write_text('module Main\nexample: 40 + 2 = 42\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine "portable"\n')
        environment = {**os.environ, "CARGO_HOME": str(root / "empty-cargo"),
                       "RUSTUP_HOME": str(root / "empty-rustup"),
                       "FAI_HOME": str(root / "home"), "FAI_TOOLCHAIN_DIR": str(root / "toolchains"),
                       "FAI_CACHE_DIR": str(root / "cache"), "FAI_RUNTIME_DIR": str(root / "runtime")}
        for name in ("FAI_TOOLCHAIN", "FAI_TOOLCHAIN_PROBE", "FAI_TOOLCHAIN_DISPATCHED"):
            environment.pop(name, None)
        environment["PATH"] = os.pathsep.join(
            entry for entry in environment.get("PATH", "").split(os.pathsep)
            if ".cargo" not in entry and ".rustup" not in entry
        )

        def invoke(executable, *arguments):
            try:
                return subprocess.run([str(executable), *arguments, "-C", str(root)],
                                      env=environment, capture_output=True, text=True,
                                      encoding="utf-8", check=True, timeout=120).stdout
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                print(f"Relocated compiler command failed: {arguments}", file=sys.stderr)
                for output in (error.stdout, error.stderr):
                    if output:
                        print(output.decode("utf-8", errors="replace") if isinstance(output, bytes) else output,
                              file=sys.stderr)
                if os.name == "nt" and isinstance(error, subprocess.TimeoutExpired):
                    print(f"Executable outputs: {[(path.name, path.stat().st_size) for path in root.glob('*.exe')]}", file=sys.stderr)
                    try:
                        processes = subprocess.run([
                            "pwsh.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
                            "Get-Process -Name fai,link,mspdbsrv -ErrorAction SilentlyContinue | "
                            "Select-Object Id,ProcessName,CPU,WorkingSet64,Path | ConvertTo-Json -Depth 3",
                        ], capture_output=True, timeout=15)
                        print(processes.stdout.decode("utf-8", errors="replace"), file=sys.stderr)
                    except (OSError, subprocess.TimeoutExpired) as diagnostic_error:
                        print(f"Process diagnostics unavailable: {diagnostic_error}", file=sys.stderr)
                    if arguments[0] == "build":
                        try:
                            stop = subprocess.run([str(executable), "daemon", "stop", "-C", str(root)],
                                                  env=environment, capture_output=True, timeout=15)
                            print(f"Diagnostic daemon stop: {stop.returncode}", file=sys.stderr)
                            direct_args = list(arguments)
                            if "--out" in direct_args:
                                direct_args[direct_args.index("--out") + 1] = str(root / "program-in-process")
                            direct = subprocess.run([str(executable), *direct_args, "--no-daemon", "-C", str(root)],
                                                    env={**environment, "FAI_CACHE_DIR": str(root / "diagnostic-cache")},
                                                    capture_output=True, timeout=120)
                            print(f"Diagnostic in-process build: {direct.returncode}", file=sys.stderr)
                            print(direct.stdout.decode("utf-8", errors="replace"), file=sys.stderr)
                            print(direct.stderr.decode("utf-8", errors="replace"), file=sys.stderr)
                        except (OSError, subprocess.TimeoutExpired) as diagnostic_error:
                            print(f"In-process diagnostic failed: {diagnostic_error}", file=sys.stderr)
                raise

        try:
            assert json.loads(invoke(relocated, "build-info")) == original
            invoke(relocated, "check", "--no-examples")
            before = invoke(relocated, "daemon", "status")
            invoke(relocated, "check", "--no-examples")
            after = invoke(relocated, "daemon", "status")
            assert re.search(r"pid (\d+)", before).group(1) == re.search(r"pid (\d+)", after).group(1)
            assert original["toolBuildId"] in after
            invoke(relocated, "test")
            assert invoke(relocated, "run", "Main.fai") == "portable\n"
            executable = root / "program"
            invoke(relocated, "build", "Main.fai", "--out", str(executable))
            if executable.with_suffix(".exe").exists():
                executable = executable.with_suffix(".exe")
            result = subprocess.run([str(executable)], env=environment, capture_output=True,
                                    text=True, encoding="utf-8", check=True, timeout=30)
            assert result.stdout == "portable\n", result
            if args.launcher:
                launcher = args.launcher.resolve()
                if not launcher.exists() and launcher.with_suffix(".exe").exists():
                    launcher = launcher.with_suffix(".exe")
                invoke(launcher, "toolchain", "register", str(relocated))
                invoke(launcher, "toolchain", "default", original["version"])
                assert json.loads(invoke(launcher, "build-info")) == original
                invoke(relocated, "daemon", "stop")
                # A cold forwarded command must release captured pipes while
                # its newly spawned daemon stays alive for the next command.
                invoke(launcher, "check", "--no-examples")
                forwarded_before = invoke(launcher, "daemon", "status")
                invoke(launcher, "check", "--no-examples")
                forwarded_after = invoke(launcher, "daemon", "status")
                assert re.search(r"pid (\d+)", forwarded_before).group(1) == re.search(r"pid (\d+)", forwarded_after).group(1)
                assert invoke(launcher, "run", "Main.fai") == "portable\n"
                print("Launcher: captured output and warm daemon forwarding passed")
            if args.other_fai:
                other = args.other_fai.resolve()
                other_info = json.loads(invoke(other, "build-info"))
                assert original["version"] == other_info["version"]
                assert original["toolBuildId"] != other_info["toolBuildId"]
                invoke(other, "check", "--no-examples")
                other_status = invoke(other, "daemon", "status")
                assert re.search(r"pid (\d+)", other_status).group(1) != re.search(r"pid (\d+)", after).group(1)
                assert original["toolBuildId"] in invoke(relocated, "daemon", "status")
                invoke(other, "daemon", "stop")
        finally:
            invoke(relocated, "daemon", "stop")
            # Shutdown acknowledgement can precede process exit. Windows keeps
            # the executable locked until exit; wait for release before rmtree.
            deadline = time.monotonic() + 30
            while True:
                try:
                    relocated.unlink()
                    break
                except PermissionError as error:
                    if getattr(error, "winerror", None) != 32 or time.monotonic() >= deadline:
                        raise
                    time.sleep(0.05)
    print("Relocated compiler: identity, warm daemon, contracts, JIT and AOT passed")


if __name__ == "__main__":
    main()
