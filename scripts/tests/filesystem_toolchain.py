#!/usr/bin/env python3
"""Verify native paths and filesystem scopes through a relocated JIT/AOT compiler."""

import argparse
import errno
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

        # Path must preserve all native units, but the mounted filesystem may
        # reject some names. Compare Fai with an independent native create probe.
        probe = str(root / "\ud800") if os.name == "nt" else os.fsencode(root) + b"/\xff"
        native_error = 0
        try:
            descriptor = os.open(probe, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except OSError as error:
            if error.errno not in (errno.EILSEQ, errno.EINVAL) and getattr(error, "winerror", None) != 123:
                raise
            native_error = getattr(error, "winerror", None) or error.errno
        else:
            os.close(descriptor)
            os.unlink(probe)

        def invoke(*arguments):
            result = subprocess.run([str(relocated), "-C", str(root), *arguments], env=environment,
                                    capture_output=True, timeout=180)
            assert result.returncode == 0, (arguments, result.stdout, result.stderr)
            return result.stdout

        invoke("fmt", "--check", "Main.fai", "--no-daemon")
        invoke("check", "Main.fai", "--no-examples", "--no-daemon")
        invoke("test", "Main.fai", "--count", "128", "--seed", "42", "--no-daemon")
        output = invoke("run", "Main.fai", "--no-daemon", "--", str(root), str(native_error))
        assert output == b"filesystem checks passed\n", output
        binary = root / "filesystem.exe"
        invoke("build", "Main.fai", "--out", str(binary), "--no-daemon")
        result = subprocess.run([str(binary), str(root), str(native_error)], env=environment, capture_output=True, timeout=30)
        assert result.returncode == 0 and result.stdout == b"filesystem checks passed\n", result
    print("Filesystem native paths, scopes, cancellation and relocated JIT/AOT passed")


if __name__ == "__main__":
    main()
