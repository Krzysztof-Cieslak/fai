#!/usr/bin/env python3
"""Verify native paths and filesystem scopes through a relocated JIT/AOT compiler."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", type=Path, required=True)
    args = parser.parse_args()
    compiler = args.fai.resolve()
    if not compiler.exists() and compiler.with_suffix(".exe").exists():
        compiler = compiler.with_suffix(".exe")
    base = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="fai-filesystem-", dir=base) as temporary:
        root = Path(temporary)
        relocated = root / compiler.name
        shutil.copy2(compiler, relocated)
        shutil.copy2(Path(__file__).parent / "fixtures" / "FilesystemChecks.fai", root / "Main.fai")
        environment = {**os.environ, "CARGO_HOME": str(root / "cargo"), "RUSTUP_HOME": str(root / "rustup")}

        def invoke(*arguments):
            result = subprocess.run([str(relocated), *arguments, "-C", str(root)], env=environment,
                                    capture_output=True, timeout=180)
            assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result.stdout

        invoke("fmt", "--check", "Main.fai", "--no-daemon")
        invoke("check", "Main.fai", "--no-examples", "--no-daemon")
        invoke("test", "Main.fai", "--count", "128", "--seed", "42", "--no-daemon")
        assert invoke("run", "Main.fai", "--no-daemon") == b"filesystem checks passed\n"
        binary = root / "filesystem.exe"
        invoke("build", "Main.fai", "--out", str(binary), "--no-daemon")
        result = subprocess.run([str(binary)], env=environment, capture_output=True, timeout=30)
        assert result.returncode == 0 and result.stdout == b"filesystem checks passed\n", result
    print("Filesystem native paths, scopes, cancellation and relocated JIT/AOT passed")


if __name__ == "__main__":
    main()
