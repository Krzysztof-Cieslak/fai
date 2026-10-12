#!/usr/bin/env python3
"""Classify changes against the actual PR base and validate required CI gates."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

import packages


def git(root, *arguments):
    return subprocess.check_output(["git", *arguments], cwd=root, stderr=subprocess.DEVNULL)


def diff_paths(data):
    fields = data.decode("utf-8").split("\0")
    paths = set()
    i = 0
    while i < len(fields) and fields[i]:
        status = fields[i]
        count = 2 if status.startswith(("R", "C")) else 1
        if i + count >= len(fields):
            raise ValueError("incomplete git name-status output")
        paths.update(fields[i + 1:i + count + 1])
        i += count + 1
    return sorted(paths)


def catalog_at(root, revision):
    catalog = {}
    documents = {}
    files = git(root, "ls-tree", "-r", "--name-only", "-z", revision, "--", "packages").decode().split("\0")
    for path in files:
        parts = path.split("/")
        if len(parts) >= 3 and parts[-1] in {packages.MANIFEST, packages.LEGACY_MANIFEST}:
            text = git(root, "show", f"{revision}:{path}").decode()
            documents.setdefault(root / Path(path).parent, {})[parts[-1]] = text
    for directory, contents in documents.items():
        manifest = contents.get(packages.MANIFEST)
        if manifest is not None and "package" not in tomllib.loads(manifest):
            continue
        package = packages.from_documents(directory, manifest, contents.get(packages.LEGACY_MANIFEST))
        catalog[package.name] = package
    return packages.bind(catalog, root)


def documentation(path):
    return (path in {"AGENTS.md", "README.md", "llms.txt", "LICENSE", "CODE_OF_CONDUCT.md"}
            or (path.startswith("docs/") and path.endswith((".md", ".txt"))))


def classify(paths, current, previous, root=packages.ROOT):
    full = False
    reporting = False
    selected = set()
    reasons = []
    for path in paths:
        owners = set()
        for catalog in (current, previous):
            candidates = []
            for name, package in catalog.items():
                relative = package.directory.relative_to(root) if package.directory.is_absolute() else package.directory
                if Path(path).is_relative_to(relative):
                    candidates.append((len(relative.parts), name))
            if candidates:
                depth = max(depth for depth, _ in candidates)
                owners.update(name for size, name in candidates if size == depth)
        if owners:
            selected.update(owners)
            reasons.extend(f"package: {name}" for name in owners)
        elif path.startswith("scripts/performance_report/"):
            reporting = True
            reasons.append("report presentation")
        elif documentation(path):
            continue
        else:
            full = True
            reasons.append(f"compiler/infrastructure: {path}")
    package_change = bool(selected)
    if full:
        selected = set(current)
    else:
        while True:
            expanded = selected | {name for catalog in (current, previous) for name, package in catalog.items()
                                   if selected.intersection(package.all_dependencies)}
            if expanded == selected:
                break
            selected = expanded
    return {
        "compiler": full,
        "reporting": reporting or full,
        "packages": [name for name in packages.validate(current) if name in selected],
        "mode": "compiler" if full else "packages" if package_change else "reporting" if reporting else "docs",
        "reasons": sorted(set(reasons)),
    }


def scope(root, base, head="HEAD"):
    current = packages.discover(root)
    try:
        if not base or not base.strip("0"):
            raise ValueError("no comparison base")
        baseline = git(root, "merge-base", base, head).decode().strip()
        previous = catalog_at(root, baseline)
        paths = diff_paths(git(root, "diff", "--name-status", "-z", "--find-renames", baseline, head))
    except (subprocess.CalledProcessError, ValueError) as error:
        return {"compiler": True, "reporting": True, "packages": packages.validate(current), "mode": "compiler",
                "reasons": [f"conservative full check: {type(error).__name__}"]}
    result = classify(paths, current, previous, root)
    result["base"] = baseline
    return result


def summary(text):
    print(text)
    if "GITHUB_STEP_SUMMARY" in os.environ:
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(text + "\n")


def lane_summary(title, mode, compiler_checks, names):
    rust = "enabled" if compiler_checks else "skipped"
    return (f"### {title}: {mode}\n"
            f"Rust compiler checks: **{rust}**\n\n"
            f"Affected packages: {', '.join(names) or '(none)'}\n"
            "Package checks and declared integrations use the prepared compiler bundle.")


def require(result, mode, compiler_flag, reporting_flag="false"):
    if result != "success":
        raise ValueError(f"change classification did not succeed: {result}")
    if mode not in {"compiler", "packages", "reporting", "docs"} or compiler_flag != str(mode == "compiler").lower():
        raise ValueError("missing or inconsistent change classification outputs")
    if (reporting_flag not in {"true", "false"}
            or (mode in {"compiler", "reporting"} and reporting_flag != "true")
            or (mode == "docs" and reporting_flag != "false")):
        raise ValueError("missing or inconsistent reporting selection")


def selected_packages(encoded, names):
    selected = json.loads(encoded)
    if not isinstance(selected, list) or not all(isinstance(name, str) for name in selected):
        raise ValueError("missing package selection output")
    if selected != names.split() or len(selected) != len(set(selected)):
        raise ValueError("inconsistent package selection outputs")
    return selected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    classify_parser = sub.add_parser("scope")
    classify_parser.add_argument("--base", default=os.environ.get("CHANGE_BASE"))
    classify_parser.add_argument("--head", default="HEAD")
    classify_parser.add_argument("--github-output", action="store_true")
    gate = sub.add_parser("require")
    gate.add_argument("--result", default=os.environ.get("SCOPE_RESULT", "missing"))
    args = parser.parse_args()
    try:
        if args.action == "scope":
            result = scope(packages.ROOT, args.base, args.head)
            print(json.dumps(result, sort_keys=True))
            if args.github_output:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                    print(f"compiler={str(result['compiler']).lower()}", file=stream)
                    print(f"reporting={str(result['reporting']).lower()}", file=stream)
                    print(f"packages={json.dumps(result['packages'], separators=(',', ':'))}", file=stream)
                    print(f"package_names={' '.join(result['packages'])}", file=stream)
                    print(f"mode={result['mode']}", file=stream)
                summary(lane_summary("Selected CI lane", result["mode"], result["compiler"], result["packages"]))
            return
        require(args.result, os.environ.get("CI_MODE"), os.environ.get("CI_COMPILER"), os.environ.get("CI_REPORTING"))
        selected = selected_packages(os.environ.get("CI_PACKAGES", "null"), os.environ.get("PACKAGE_NAMES", ""))
        packages.affected(packages.discover(), selected, False)
        summary(lane_summary("CI lane", os.environ["CI_MODE"], os.environ["CI_COMPILER"] == "true", selected))
    except (packages.PackageError, ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"CI: {error}\n")


if __name__ == "__main__":
    main()
