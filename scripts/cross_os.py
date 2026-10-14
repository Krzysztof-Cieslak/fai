#!/usr/bin/env python3
"""Build only Cargo targets needed by the existing cross-platform nextest policy."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
LIB_KINDS = {"lib", "rlib", "staticlib", "dylib", "cdylib", "proc-macro"}


class SelectionError(ValueError):
    """A policy or test inventory cannot safely be represented by Cargo selectors."""


def selectors(expression):
    # Only the positive union determines which binaries may be needed. The
    # subtraction stays with nextest, including its per-test property exclusions.
    parts = re.split(r"\s+-\s+", expression, maxsplit=1)
    positive = parts[0].strip()
    if not positive.startswith("(") or not positive.endswith(")"):
        raise SelectionError("cross-os filter must start with a parenthesized package/binary union")
    packages, binaries = set(), set()
    for atom in positive[1:-1].split("|"):
        match = re.fullmatch(r"\s*(package|binary)\(([A-Za-z0-9_-]+)\)\s*", atom)
        if match is None:
            raise SelectionError("unsupported cross-os filter; update the Cargo build planner")
        (packages if match[1] == "package" else binaries).add(match[2])
    if not packages and not binaries:
        raise SelectionError("empty cross-os build selection")
    excluded = set()
    if len(parts) == 2:
        negative = parts[1].strip()
        if not negative.startswith("(") or not negative.endswith(")"):
            raise SelectionError("unsupported cross-os subtraction")
        for atom in negative[1:-1].split("|"):
            match = re.fullmatch(r"\s*binary\(([A-Za-z0-9_-]+)\)\s*", atom)
            if match:
                excluded.add(match[1])
            elif not re.fullmatch(r"\s*test\(/[^()]+/\)\s*", atom):
                raise SelectionError("unsupported cross-os subtraction; update the Cargo build planner")
    return packages, binaries, excluded


def target_kind(target):
    kinds = set(target["kind"])
    if kinds & LIB_KINDS:
        return "lib"
    if len(kinds) == 1 and kinds <= {"bin", "test", "example", "bench", "custom-build"}:
        return next(iter(kinds))
    raise SelectionError(f"unsupported Cargo target kind: {target['kind']}")


def plan(metadata, expression):
    selected_packages, selected_binaries, excluded = selectors(expression)
    members = set(metadata["workspace_members"])
    packages = [package for package in metadata["packages"] if package["id"] in members]
    names = {package["name"] for package in packages}
    if selected_packages - names:
        raise SelectionError(f"unknown cross-os packages: {sorted(selected_packages - names)}")
    owners = set(selected_packages)
    targets = {"lib": set(), "bin": set(), "test": set(), "example": set()}
    found_binaries = set()
    for package in packages:
        for target in package["targets"]:
            if not target["test"]:
                continue
            kind = target_kind(target)
            if kind in {"bench", "custom-build"}:
                continue
            name = target["name"]
            if name in excluded:
                continue
            if name in selected_binaries:
                found_binaries.add(name)
            if package["name"] in selected_packages or name in selected_binaries:
                owners.add(package["name"])
                targets[kind].add(name)
    if selected_binaries - found_binaries:
        raise SelectionError(f"unknown cross-os binaries: {sorted(selected_binaries - found_binaries)}")
    if not any(targets.values()):
        raise SelectionError("cross-os policy selects no Cargo test targets")
    args = [part for name in sorted(owners) for part in ("--package", name)]
    if targets["lib"]:
        args.append("--lib")
    for kind in ("bin", "test", "example"):
        args.extend(part for name in sorted(targets[kind]) for part in (f"--{kind}", name))
    return args


def cargo_args(root=ROOT, environment=None):
    with (root / ".config/nextest.toml").open("rb") as stream:
        expression = tomllib.load(stream)["profile"]["cross-os"]["default-filter"]
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root, env=environment, text=True))
    args = plan(metadata, expression)
    print(f"Cross-platform build: {args.count('--package')} packages, "
          f"{args.count('--test')} integration target names", file=sys.stderr)
    return args


def command(action, selection):
    return ["cargo", "nextest", action, *selection, "--locked", "--cargo-profile", "ci", "--profile", "cross-os"]


def matched_tests(inventory):
    result = set()
    for binary, suite in inventory["rust-suites"].items():
        for name, test in suite["testcases"].items():
            status = test["filter-match"]["status"]
            if status == "matches":
                result.add((binary, name))
            elif status != "mismatch":
                raise SelectionError(f"unknown nextest filter status: {status}")
    return result


def compare(full, narrowed):
    expected = matched_tests(full)
    actual = matched_tests(narrowed)
    if not expected:
        raise SelectionError("full cross-os policy selected no tests")
    missing, extra = expected - actual, actual - expected
    if missing or extra:
        raise SelectionError(f"cross-os coverage changed: missing={sorted(missing)}, extra={sorted(extra)}")
    return {"tests": len(actual), "binaries": len({binary for binary, _ in actual})}


def verify(root=ROOT):
    selection = cargo_args(root)
    def listed(arguments):
        return json.loads(subprocess.check_output(
            [*command("list", arguments), "--message-format", "json"], cwd=root, text=True))
    result = compare(listed(["--workspace"]), listed(selection))
    print(json.dumps({"crossOsCoverage": "unchanged", **result}, sort_keys=True))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("plan", "run", "verify"))
    args = parser.parse_args()
    try:
        if args.action == "verify":
            verify()
        elif args.action == "plan":
            print(json.dumps(cargo_args()))
        else:
            subprocess.run(command("run", cargo_args()), cwd=ROOT, check=True)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"cross-os tests: {error}\n")


if __name__ == "__main__":
    main()
