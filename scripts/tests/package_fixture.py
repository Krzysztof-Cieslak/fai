"""Temporary consumers of real in-tree packages for compiler integration fixtures."""

import json
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[2]


def prepare(directory, dependencies, *, portable=False):
    """Match transitive Git identities when fixtures exchange package-owned types."""
    directory.mkdir(parents=True, exist_ok=True)
    workspace = tomllib.loads((ROOT / "packages/fai.toml").read_text(encoding="utf-8"))
    lines = [
        "manifest-version = 1", "[package]", 'name = "compiler-fixture"',
        'version = "0.1.0"', 'source-dir = "."', "auto-main = false", "[dependencies]",
    ]
    for name in dependencies:
        path = ROOT / "packages" / name
        alias = name.replace("-", "_")
        if portable:
            source = next(rule["replace"] for rule in workspace["overrides"] if rule["with"].get("path") == name)
            version = tomllib.loads((path / "fai.toml").read_text(encoding="utf-8"))["package"]["version"]
            lines.append(f'{alias} = {{git = {json.dumps(source["git"])}, subdir = {json.dumps(source["subdir"])}, version = {json.dumps("^" + version)}}}')
        else:
            lines.append(f'{alias} = {{path = {json.dumps(str(path))}}}')
    for rule in workspace["overrides"]:
        lines.extend([
            "[[overrides]]",
            "replace = {git = " + json.dumps(rule["replace"]["git"]) + ", subdir = " + json.dumps(rule["replace"]["subdir"]) + "}",
            "with = {path = " + json.dumps(str(ROOT / "packages" / rule["with"]["path"])) + "}",
        ])
    (directory / "fai.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")
