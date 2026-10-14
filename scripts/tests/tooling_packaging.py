#!/usr/bin/env python3
"""Round-trip an existing compiler through release archives without relabelling its build."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release


def contents(path):
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            entries = archive.infolist()
            if len(entries) != len({entry.filename for entry in entries}) or any(
                    entry.is_dir() or (entry.external_attr >> 16) & 0o170000 != 0o100000 for entry in entries):
                raise ValueError("release ZIP must contain distinct regular files")
            return {entry.filename: archive.read(entry) for entry in entries}
    with tarfile.open(path) as archive:
        entries = archive.getmembers()
        if len(entries) != len({entry.name for entry in entries}) or any(not entry.isfile() for entry in entries):
            raise ValueError("release tarball must contain distinct regular files")
        return {entry.name: archive.extractfile(entry).read() for entry in entries}


def run(binary, root=release.ROOT):
    binary = Path(binary).resolve()
    info = json.loads(subprocess.check_output([str(binary), "build-info"], text=True))
    name = "fai.exe" if os.name == "nt" else "fai"
    files = {f"bin/{name}": binary.read_bytes(), "LICENSE": (root / "LICENSE").read_bytes(),
             "metadata.json": json.dumps({"kind": "ci-tooling-smoke", "buildInfo": info}, sort_keys=True).encode()}
    parent = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="fai-tooling-package-", dir=parent) as directory:
        temporary = Path(directory)
        restored = None
        for extension in ("zip", "tar.gz"):
            archive = temporary / f"compiler.{extension}"
            release.make_archive(archive, files)
            payload = contents(archive)
            if payload != files:
                raise ValueError(f"{extension} packaging changed compiler payloads")
            if extension == ("zip" if os.name == "nt" else "tar.gz"):
                restored = temporary / name
                restored.write_bytes(payload[f"bin/{name}"])
                restored.chmod(0o755)
        subprocess.run([sys.executable, str(root / "scripts/tests/smoke_compiler.py"), "--fai", str(restored)], check=True)
    print("Release archive round trips and relocated compiler checks passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai", help="existing compiler; never built by this fixture")
    args = parser.parse_args()
    resolved = shutil.which(args.fai)
    if resolved is None:
        parser.error(f"compiler executable not found: {args.fai}")
    run(resolved)


if __name__ == "__main__":
    main()
