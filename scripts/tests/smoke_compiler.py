#!/usr/bin/env python3
"""Exercise a relocated compiler with empty Rust homes and a warm daemon."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", type=Path, required=True)
    parser.add_argument("--other-fai", type=Path)
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
                       "RUSTUP_HOME": str(root / "empty-rustup")}
        environment["PATH"] = os.pathsep.join(
            entry for entry in environment.get("PATH", "").split(os.pathsep)
            if ".cargo" not in entry and ".rustup" not in entry
        )

        def invoke(executable, *arguments):
            return subprocess.run([str(executable), *arguments, "-C", str(root)],
                                  env=environment, capture_output=True, text=True,
                                  encoding="utf-8", check=True, timeout=120).stdout

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
    print("Relocated compiler: identity, warm daemon, contracts, JIT and AOT passed")


if __name__ == "__main__":
    main()
