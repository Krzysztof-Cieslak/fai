#!/usr/bin/env python3
"""Compiler execution checks using the web package's native HTTP fixtures."""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai", help="existing compiler executable")
    args = parser.parse_args()
    candidate = Path(args.fai)
    if not candidate.exists() and candidate.with_suffix(".exe").exists():
        candidate = candidate.with_suffix(".exe")
    compiler = str(candidate.resolve()) if candidate.exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("fai was not found; pass --fai /path/to/fai")
    package = Path(__file__).resolve().parents[2] / "packages" / "web"
    root = package.parent
    if not (root / "json" / "src" / "Json.fai").exists():
        parser.error("place the json package beside the web package")

    def fai(*arguments, **kwargs):
        return subprocess.run(
            [compiler, *arguments, "--no-daemon", "-C", str(root)],
            check=True, text=True, encoding="utf-8", timeout=600, **kwargs,
        )

    result = fai("run", "web/examples/JsonWebExample.fai", capture_output=True)
    expected = result.stdout
    if os.name == "posix":
        import resource

        def small_main_stack():
            _, hard = resource.getrlimit(resource.RLIMIT_STACK)
            resource.setrlimit(resource.RLIMIT_STACK, (1024 * 1024, hard))

        # Model Windows' small initial stack while compiling the same program.
        result = fai("run", "web/examples/JsonWebExample.fai", capture_output=True,
                     preexec_fn=small_main_stack)
        assert result.stdout == expected, result
    effects = fai("run", "web/test/JsonEffects.fai", capture_output=True)
    assert effects.stdout == "read\nhandled\n200\n", effects
    backend_cases = [("JsonEffects", effects.stdout)]
    for name in ("HeadersOnWire", "RedirectSafety"):
        result = fai("run", f"web/test/{name}.fai", capture_output=True)
        backend_cases.append((name, result.stdout))
    with tempfile.TemporaryDirectory(prefix="fai-web-") as temp:
        executable = Path(temp) / "web-json"
        fai("build", "web/examples/JsonWebExample.fai", "--out", str(executable))
        if executable.with_suffix(".exe").exists():
            executable = executable.with_suffix(".exe")
        for workers in (1, 2, 4):
            try:
                result = subprocess.run(
                    [str(executable)], check=True, capture_output=True,
                    text=True, encoding="utf-8", timeout=120,
                    env={**os.environ, "FAI_WORKERS": str(workers)},
                )
            except subprocess.TimeoutExpired as error:
                raise AssertionError(
                    f"native HTTP shutdown timed out with {workers} workers; "
                    f"stdout={error.stdout!r}, stderr={error.stderr!r}"
                ) from error
            assert result.stdout == expected, (workers, result)
        for name, baseline in backend_cases:
            binary = Path(temp) / f"{name}.exe"
            fai("build", f"web/test/{name}.fai", "--out", str(binary))
            result = subprocess.run([str(binary)], check=True, capture_output=True,
                                    text=True, encoding="utf-8", timeout=120)
            assert result.stdout == baseline, (name, result)
    print("Web compiler fixtures: effect forwarding and JIT/AOT HTTP checks passed")


if __name__ == "__main__":
    main()
