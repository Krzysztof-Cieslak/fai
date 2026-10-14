#!/usr/bin/env python3
"""Classify changes against the actual PR base and validate required CI gates."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib

import ci_workspace
import ci_tooling
import packages


PACKAGE_WORKSPACE_METADATA = {"packages/fai.toml", "packages/fai.lock"}
TOOLING_PATHS = {path: group for group, paths in ci_tooling.PATHS.items() for path in paths}


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
    path = path.lower()
    return (path in {"agents.md", "readme", "readme.md", "readme.txt", "llms.txt", "llm.txt", "license",
                     "license.md", "license.txt", "code_of_conduct.md", "contributing.md", "security.md", "changelog.md"}
            or (path.startswith("docs/") and path.endswith((".md", ".txt"))))


def package_change(path, package, root):
    directory = package.directory.relative_to(root) if package.directory.is_absolute() else package.directory
    relative = Path(path).relative_to(directory)
    name = relative.as_posix()
    if documentation(name):
        return "docs"
    if name == packages.LEGACY_MANIFEST and package.source_roots is not None:
        return "integrations"
    if relative.suffix != ".fai" or package.source_roots is None:
        return "production"
    if name in package.entrypoints:
        return "local"
    roots = [(Path(value), kind) for kind, values in (("production", package.source_roots), ("local", package.test_roots))
             for value in values if relative.is_relative_to(Path(value))]
    if not roots:
        if any(name == integration.entry for integration in package.integrations):
            return "local"
        return "production"
    depth = max(len(path.parts) for path, _ in roots)
    return "production" if any(len(path.parts) == depth and kind == "production" for path, kind in roots) else "local"


def affected_sources(selected, current, previous):
    """Normal edges propagate library changes; development edges affect only that root."""
    selected = set(selected)
    while True:
        expanded = selected | {name for catalog in (current, previous) for name, package in catalog.items()
                               if selected.intersection(package.dependencies)}
        if expanded == selected:
            break
        selected = expanded
    return selected | {name for catalog in (current, previous) for name, package in catalog.items()
                       if selected.intersection(package.development)}


def classify(paths, current, previous, root=packages.ROOT, workspace_impact=None):
    full = False
    reporting = False
    vscode = False
    tooling = set()
    workspace_changed = False
    production = set()
    local = set()
    integrations = set()
    reasons = []
    for path in paths:
        if path in PACKAGE_WORKSPACE_METADATA:
            workspace_changed = True
            production.update(current if workspace_impact is None else workspace_impact[0])
            reasons.append(f"package workspace: {path}")
            continue
        owners = []
        for catalog in (current, previous):
            candidates = []
            for name, package in catalog.items():
                relative = package.directory.relative_to(root) if package.directory.is_absolute() else package.directory
                if Path(path).is_relative_to(relative):
                    candidates.append((len(relative.parts), name, package))
            if candidates:
                depth = max(depth for depth, _, _ in candidates)
                owners.extend((name, package) for size, name, package in candidates if size == depth)
        if owners:
            for name, package in owners:
                kind = package_change(path, package, root)
                if kind == "production":
                    production.add(name)
                elif kind == "local":
                    local.add(name)
                elif kind == "integrations":
                    integrations.add(name)
                if kind != "docs":
                    reasons.append(f"package: {name}" if kind == "production" else f"package {kind}: {name}")
        elif path in TOOLING_PATHS:
            group = TOOLING_PATHS[path]
            tooling.add(group)
            reasons.append(f"Python tooling: {group}")
        elif path.startswith("editors/vscode/"):
            vscode = True
            reasons.append("VS Code extension")
        elif path.startswith("scripts/performance_report/"):
            reporting = True
            reasons.append("report presentation")
        elif documentation(path):
            continue
        else:
            full = True
            reasons.append(f"compiler/infrastructure: {path}")
    if workspace_changed and workspace_impact is not None:
        reasons.extend(workspace_impact[1])
    package_changed = workspace_changed or bool(production | local | integrations)
    if full:
        checks = selected = set(current)
    else:
        checks = affected_sources(production, current, previous) | local
        selected = checks | integrations
    ordered = packages.validate(current)
    return {
        "compiler": full,
        "reporting": reporting or full or "performance" in tooling,
        "vscode": vscode or full,
        "tooling": sorted(ci_tooling.TESTS if full else tooling),
        "packages": [name for name in ordered if name in selected],
        "package_checks": [name for name in ordered if name in checks],
        "mode": "compiler" if full else "packages" if package_changed else "tooling" if tooling else "reporting" if reporting else "vscode" if vscode else "docs",
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
        names = packages.validate(current)
        return {"compiler": True, "reporting": True, "vscode": True, "tooling": sorted(ci_tooling.TESTS),
                "packages": names, "package_checks": names, "mode": "compiler",
                "reasons": [f"conservative full check: {type(error).__name__}"]}
    impact = None
    if PACKAGE_WORKSPACE_METADATA.intersection(paths):
        def documents(revision):
            result = {}
            for path in sorted(PACKAGE_WORKSPACE_METADATA):
                exists = git(root, "ls-tree", "--name-only", revision, "--", path)
                result[path] = tomllib.loads(git(root, "show", f"{revision}:{path}").decode()) if exists else None
            return result
        impact = ci_workspace.affected(documents(baseline), documents(head), current, previous, root)
    result = classify(paths, current, previous, root, impact)
    result["base"] = baseline
    return result


def summary(text):
    print(text)
    if "GITHUB_STEP_SUMMARY" in os.environ:
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(text + "\n")


def lane_summary(title, mode, compiler_checks, names, checks=None, vscode=False, tooling=()):
    rust = "enabled" if compiler_checks else "skipped"
    return (f"### {title}: {mode}\n"
            f"Rust compiler checks: **{rust}**\n\n"
            f"VS Code checks: **{'enabled' if vscode else 'skipped'}**\n\n"
            f"Python tooling: {', '.join(tooling) or '(none)'}\n\n"
            f"Affected packages: {', '.join(names) or '(none)'}\n"
            f"Native package checks: {', '.join(names if checks is None else checks) or '(none)'}\n"
            "Package checks and declared integrations use the prepared compiler bundle.")


def validate_package_workspace(root=packages.ROOT, compiler="fai"):
    """Validate all declared root/platform graphs without repairing committed metadata."""
    workspace = root / "packages"
    if not (workspace / packages.MANIFEST).exists():
        if (workspace / "fai.lock").exists():
            raise packages.PackageError("package workspace lock requires a fai.toml manifest")
        return
    subprocess.run([compiler, "resolve", "--locked", "-C", str(workspace)], check=True)


def require(result, mode, compiler_flag, reporting_flag="false", vscode_flag="false", tooling_flag="[]"):
    if result != "success":
        raise ValueError(f"change classification did not succeed: {result}")
    if mode not in {"compiler", "packages", "tooling", "reporting", "vscode", "docs"} or compiler_flag != str(mode == "compiler").lower():
        raise ValueError("missing or inconsistent change classification outputs")
    if (reporting_flag not in {"true", "false"}
            or (mode in {"compiler", "reporting"} and reporting_flag != "true")
            or (mode in {"docs", "vscode"} and reporting_flag != "false")):
        raise ValueError("missing or inconsistent reporting selection")
    if (vscode_flag not in {"true", "false"}
            or (mode in {"compiler", "vscode"} and vscode_flag != "true")
            or (mode == "docs" and vscode_flag != "false")):
        raise ValueError("missing or inconsistent VS Code selection")
    tooling = ci_tooling.selection(tooling_flag)
    if ((mode == "tooling" and not tooling) or (mode in {"docs", "vscode", "reporting"} and tooling)
            or (mode == "compiler" and tooling != sorted(ci_tooling.TESTS))):
        raise ValueError("inconsistent Python-tooling selection")
    if "performance" in tooling and reporting_flag != "true":
        raise ValueError("performance tooling requires presentation checks")


def selected_packages(encoded, names):
    selected = json.loads(encoded)
    if not isinstance(selected, list) or not all(isinstance(name, str) for name in selected):
        raise ValueError("missing package selection output")
    if selected != names.split() or len(selected) != len(set(selected)):
        raise ValueError("inconsistent package selection outputs")
    return selected


def selected_checks(encoded, names, selected, mode):
    checks = selected_packages(encoded, names)
    if not set(checks) <= set(selected) or (mode == "compiler" and checks != selected):
        raise ValueError("inconsistent native package check selection")
    if mode in {"docs", "reporting", "vscode", "tooling"} and (checks or selected):
        raise ValueError("non-package lane cannot select packages")
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    classify_parser = sub.add_parser("scope")
    classify_parser.add_argument("--base", default=os.environ.get("CHANGE_BASE"))
    classify_parser.add_argument("--head", default="HEAD")
    classify_parser.add_argument("--github-output", action="store_true")
    classify_parser.add_argument("--force-full", action="store_true")
    validation = sub.add_parser("validate-packages")
    validation.add_argument("--fai", default="fai")
    gate = sub.add_parser("require")
    gate.add_argument("--result", default=os.environ.get("SCOPE_RESULT", "missing"))
    args = parser.parse_args()
    try:
        if args.action == "validate-packages":
            validate_package_workspace(compiler=args.fai)
            return
        if args.action == "scope":
            if args.force_full:
                names = packages.validate(packages.discover())
                result = {"compiler": True, "reporting": True, "vscode": True, "tooling": sorted(ci_tooling.TESTS),
                          "packages": names, "package_checks": names,
                          "mode": "compiler", "reasons": ["full release verification"]}
            else:
                result = scope(packages.ROOT, args.base, args.head)
            print(json.dumps(result, sort_keys=True))
            if args.github_output:
                with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
                    print(f"compiler={str(result['compiler']).lower()}", file=stream)
                    print(f"reporting={str(result['reporting']).lower()}", file=stream)
                    print(f"vscode={str(result['vscode']).lower()}", file=stream)
                    print(f"tooling={json.dumps(result['tooling'], separators=(',', ':'))}", file=stream)
                    print(f"packages={json.dumps(result['packages'], separators=(',', ':'))}", file=stream)
                    print(f"package_names={' '.join(result['packages'])}", file=stream)
                    print(f"package_checks={json.dumps(result['package_checks'], separators=(',', ':'))}", file=stream)
                    print(f"package_check_names={' '.join(result['package_checks'])}", file=stream)
                    print(f"mode={result['mode']}", file=stream)
                details = "\n\nSelection reasons:\n" + "\n".join(f"- {reason}" for reason in result["reasons"])
                summary(lane_summary("Selected CI lane", result["mode"], result["compiler"], result["packages"], result["package_checks"], result["vscode"], result["tooling"]) + details)
            return
        require(args.result, os.environ.get("CI_MODE"), os.environ.get("CI_COMPILER"), os.environ.get("CI_REPORTING"), os.environ.get("CI_VSCODE"), os.environ.get("CI_TOOLING"))
        selected = selected_packages(os.environ.get("CI_PACKAGES", "null"), os.environ.get("PACKAGE_NAMES", ""))
        checks = selected_checks(os.environ.get("CI_PACKAGE_CHECKS", "null"), os.environ.get("PACKAGE_CHECK_NAMES", ""),
                                 selected, os.environ["CI_MODE"])
        packages.affected(packages.discover(), selected, False)
        summary(lane_summary("CI lane", os.environ["CI_MODE"], os.environ["CI_COMPILER"] == "true", selected, checks, os.environ["CI_VSCODE"] == "true", ci_tooling.selection(os.environ["CI_TOOLING"])))
    except (packages.PackageError, ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"CI: {error}\n")


if __name__ == "__main__":
    main()
