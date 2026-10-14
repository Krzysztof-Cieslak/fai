#!/usr/bin/env python3
"""Run explicit Python-tooling checks without invoking the Rust workspace suite."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TESTS = {
    "release": ("test_release.py", "test_install_ps1.py", "test_tooling_packaging.py"),
    "performance": ("test_performance.py",),
    "junit": ("test_junit_summary.py",),
}
PATHS = {
    "release": {
        "scripts/release.py", "scripts/install/install.sh", "scripts/install/install.ps1",
        "scripts/tests/test_release.py", "scripts/tests/test_install_ps1.py",
        "scripts/tests/assert_installer_archive.ps1",
        "scripts/tests/test_tooling_packaging.py",
    },
    "performance": {"scripts/performance.py", "scripts/performance_publish.py", "scripts/tests/test_performance.py"},
    "junit": {"scripts/junit-summary.py", "scripts/tests/test_junit_summary.py"},
}


def selection(encoded):
    if not isinstance(encoded, str):
        raise ValueError("missing Python-tooling selection")
    groups = json.loads(encoded)
    if (not isinstance(groups, list) or not all(isinstance(group, str) for group in groups)
            or len(groups) != len(set(groups)) or set(groups) - TESTS.keys()):
        raise ValueError("missing or invalid Python-tooling selection")
    return groups


def run(groups, root=ROOT, windows=None):
    groups = selection(json.dumps(groups))
    windows = os.name == "nt" if windows is None else windows
    if "release" in groups and windows:
        for executable in ("pwsh", "powershell.exe"):
            if not shutil.which(executable):
                raise ValueError(f"release checks require {executable}; cannot skip installer validation")
    for group in groups:
        for pattern in TESTS[group]:
            if not (root / "scripts/tests" / pattern).is_file():
                raise ValueError(f"missing required tooling tests: {pattern}")
            subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-p", pattern, "-v"],
                           cwd=root, check=True)
    if "release" in groups and not windows:
        subprocess.run(["sh", "-n", "scripts/install/install.sh"], cwd=root, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--groups", default=os.environ.get("CI_TOOLING", "null"), help="JSON array of selected tooling groups")
    args = parser.parse_args()
    try:
        run(selection(args.groups))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Python tooling: {error}\n")


if __name__ == "__main__":
    main()
