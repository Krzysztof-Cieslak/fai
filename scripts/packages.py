"""Source-package test metadata and dependency-aware selection."""

from dataclasses import dataclass
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = "ci.json"


class PackageError(ValueError):
    """Invalid package catalog or selection."""


@dataclass(frozen=True)
class Package:
    name: str
    directory: Path
    dependencies: tuple[str, ...]


def parse(directory, text):
    try:
        data = json.loads(text)
        name = data["name"]
        dependencies = data["dependencies"]
        if data["schemaVersion"] != 1 or name != directory.name or not re.fullmatch(r"[a-z][a-z0-9_-]*", name):
            raise ValueError("invalid name or schema")
        if not isinstance(dependencies, list) or not all(isinstance(name, str) for name in dependencies) or len(set(dependencies)) != len(dependencies):
            raise ValueError("dependencies must be distinct package names")
        return Package(name, directory, tuple(dependencies))
    except (KeyError, TypeError, ValueError) as error:
        raise PackageError(f"invalid {directory / MANIFEST}: {error}") from error


def validate(catalog):
    done, active = set(), set()
    ordered = []

    def visit(name):
        if name in active:
            raise PackageError(f"package dependency cycle at {name}")
        if name in done:
            return
        if name not in catalog:
            raise PackageError(f"unknown package dependency: {name}")
        active.add(name)
        for dependency in catalog[name].dependencies:
            visit(dependency)
        active.remove(name)
        done.add(name)
        ordered.append(name)

    for name in sorted(catalog):
        visit(name)
    return ordered


def discover(root=ROOT):
    catalog = {}
    workspace = root / "packages"
    if not workspace.exists():
        return catalog
    for directory in sorted(workspace.iterdir()):
        if not directory.is_dir() or directory.name.startswith("."):
            continue
        path = directory / MANIFEST
        if not path.exists():
            if any(directory.rglob("*.fai")):
                raise PackageError(f"source package is missing {path}")
            continue
        package = parse(directory, path.read_text())
        catalog[package.name] = package
    validate(catalog)
    return catalog


def affected(catalog, names, dependents=True):
    selected = set(names)
    missing = selected - catalog.keys()
    if missing:
        raise PackageError(f"unknown packages: {', '.join(sorted(missing))}")
    if dependents:
        while True:
            expanded = selected | {name for name, package in catalog.items()
                                   if selected.intersection(package.dependencies)}
            if expanded == selected:
                break
            selected = expanded
    return [name for name in validate(catalog) if name in selected]
