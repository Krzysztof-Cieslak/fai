#!/usr/bin/env python3
"""Run named PostgreSQL suites against an explicitly provisioned service."""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile

from package_fixture import prepare

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    if not os.environ.get("FAI_PG_URL"):
        parser.error("FAI_PG_URL must name an explicitly provisioned database")
    base = os.environ.get("RUNNER_TEMP")
    if base is None and Path("/tmp/opencode").is_dir():
        base = "/tmp/opencode"
    with tempfile.TemporaryDirectory(prefix="fai-postgres-", dir=base) as temporary:
        directory = Path(temporary)
        prepare(directory, ["postgres", "sql", "json", "uuid"], portable=True)
        tests = directory / "tests"
        tests.mkdir()
        for name in ("Basic", "Integration", "WireChecks"):
            source = (ROOT / "packages/postgres/examples" / f"{name}.fai").read_text(encoding="utf-8")
            (tests / f"{name}.fai").write_text(source.replace("public tests :", "tests :"), encoding="utf-8")
        mode = "tls" if os.environ.get("FAI_PG_ROOT") else "plain"
        report = Path(os.environ.get("RUNNER_TEMP", temporary)) / "fai-junit" / f"postgres-{mode}.xml"
        subprocess.run([args.fai, "test", "--no-daemon", "--deny-focused", "-C", str(directory), "--junit", str(report)], check=True, timeout=300)


if __name__ == "__main__":
    main()
