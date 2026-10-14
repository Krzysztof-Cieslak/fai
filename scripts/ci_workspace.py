"""Scope workspace metadata edits without replacing the compiler's lock validation."""

import json
from pathlib import PurePosixPath, PureWindowsPath

import packages


class Unscoped(ValueError):
    """Metadata whose impact requires conservative workspace-wide checks."""


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def table(value, context, allowed=None):
    if not isinstance(value, dict):
        raise packages.PackageError(f"{context} must be a table")
    if allowed is not None and value.keys() - allowed:
        raise Unscoped(f"unrecognized {context} fields")
    return value


def records(value, context):
    if not isinstance(value, list) or not all(isinstance(item, dict) for item in value):
        raise packages.PackageError(f"{context} must be an array of tables")
    return value


def text(value, context):
    if not isinstance(value, str) or not value or "\0" in value:
        raise packages.PackageError(f"{context} must be a nonempty string")
    return value


def strings(value, context):
    if not isinstance(value, list):
        raise packages.PackageError(f"{context} must be an array of strings")
    result = [text(item, context) for item in value]
    if len(result) != len(set(result)):
        raise packages.PackageError(f"duplicate {context}")
    return result


def literal_path(value):
    text(value, "workspace path")
    path = PurePosixPath(value)
    if (path.is_absolute() or PureWindowsPath(value).drive or ".." in path.parts
            or path.as_posix() != value or any(char in value for char in "\\*?[]{}")):
        raise Unscoped("nonliteral workspace path")
    return value


def directory_names(catalog, root):
    result = {}
    for name, package in catalog.items():
        directory = package.directory
        if directory.is_absolute():
            directory = directory.relative_to(root)
        path = directory.relative_to("packages").as_posix()
        result[path] = name
    return result


def named_paths(paths, catalogs):
    result = set()
    for path in paths:
        path = literal_path(path)
        names = {catalog[path] for catalog in catalogs if path in catalog}
        if not names:
            raise Unscoped(f"workspace path has no package: {path}")
        result.update(names)
    return result


def manifest_members(document):
    table(document, "workspace manifest")
    if type(document.get("manifest-version")) is not int or document["manifest-version"] != 1:
        raise Unscoped("unsupported workspace manifest version")
    workspace = table(document.get("workspace"), "workspace", {"members"})
    return {literal_path(path) for path in strings(workspace.get("members", []), "workspace members")}


def overrides(document):
    result = {}
    for entry in records(document.get("overrides", []), "workspace overrides"):
        table(entry, "workspace override", {"replace", "with", "members"})
        replacement = table(entry.get("replace"), "override identity", {"git", "subdir"})
        text(replacement.get("git"), "override repository")
        literal_path(replacement.get("subdir", "."))
        table(entry.get("with"), "override source", {"git", "subdir", "version", "rev", "branch", "path"})
        if "members" in entry:
            members = strings(entry["members"], "override members")
            entry = {**entry, "members": sorted(literal_path(path) for path in members)}
        key = canonical(entry)
        if key in result:
            raise packages.PackageError("duplicate workspace override")
        result[key] = entry
    return result


def manifest_impact(before, after, catalogs):
    old_members, new_members = manifest_members(before), manifest_members(after)
    old_global = {key: value for key, value in before.items() if key not in {"workspace", "overrides"}}
    new_global = {key: value for key, value in after.items() if key not in {"workspace", "overrides"}}
    if old_global != new_global:
        raise Unscoped("global workspace configuration changed")
    selected = named_paths(old_members ^ new_members, catalogs)
    reasons = [f"workspace member: {path}" for path in sorted(old_members ^ new_members)]
    old_overrides, new_overrides = overrides(before), overrides(after)
    for key in sorted(old_overrides.keys() ^ new_overrides.keys()):
        entry = old_overrides.get(key, new_overrides.get(key))
        if "members" in entry:
            names = named_paths(entry["members"], catalogs)
        else:
            identity = entry["replace"]
            if not packages.in_tree_repository(identity["git"]):
                raise Unscoped("unscoped external override changed")
            path = PurePosixPath(identity.get("subdir", "."))
            if not path.is_relative_to("packages"):
                raise Unscoped("override outside source packages changed")
            names = named_paths([path.relative_to("packages").as_posix()], catalogs)
        selected.update(names)
        reasons.extend(f"workspace override: {name}" for name in sorted(names))
    return selected, reasons


def lock_graphs(document):
    table(document, "workspace lock", {"lock-version", "resolver-version", "context", "packages", "graphs"})
    if (type(document.get("lock-version")) is not int or document["lock-version"] != 1
            or type(document.get("resolver-version")) is not int or document["resolver-version"] != 1
            or document.get("context") != "shared"):
        raise Unscoped("unsupported workspace lock format")
    descriptors = {}
    for record in records(document.get("packages", []), "lock packages"):
        table(record, "lock package", {"id", "logical", "version", "source", "manifest-fingerprint", "checksum", "released"})
        identifier = text(record.get("id"), "lock package id")
        if identifier in descriptors:
            raise packages.PackageError("duplicate lock package id")
        table(record.get("logical"), "lock logical source", {"path", "git", "subdir"})
        table(record.get("source"), "lock source", {"path", "git", "subdir", "rev"})
        descriptors[identifier] = canonical(record)
    result = {}
    identifiers = set()
    for graph in records(document.get("graphs", []), "lock graphs"):
        table(graph, "lock graph", {"id", "root", "target", "root-package", "input-fingerprint",
                                     "packages", "edges", "branches", "resolved-with"})
        identifier = text(graph.get("id"), "lock graph id")
        key = (literal_path(graph.get("root")), text(graph.get("target"), "lock graph target"))
        if key in result or identifier in identifiers:
            raise packages.PackageError("duplicate lock graph")
        identifiers.add(identifier)
        members = strings(graph.get("packages"), "lock graph packages")
        if graph.get("root-package") not in members or set(members) - descriptors.keys():
            raise packages.PackageError("lock graph has missing package records")
        edges = records(graph.get("edges", []), "lock graph edges")
        for edge in edges:
            table(edge, "lock edge", {"from", "to", "alias", "kind"})
            text(edge.get("alias"), "lock edge alias")
            kind = text(edge.get("kind"), "lock edge kind")
            source = text(edge.get("from"), "lock edge source")
            target = text(edge.get("to"), "lock edge target")
            if (kind not in {"normal", "development"} or target not in members
                    or (source != "$root" if kind == "development" else source not in members)):
                raise packages.PackageError("invalid lock graph edge")
        branches = records(graph.get("branches", []), "lock branches")
        for branch in branches:
            table(branch, "lock branch", {"logical-package", "branch", "rev"})
            if branch.get("logical-package") not in members:
                raise packages.PackageError("lock branch has missing package record")
        result[key] = canonical({
            "root-package": graph["root-package"],
            "input-fingerprint": graph.get("input-fingerprint"),
            "packages": sorted(descriptors[member] for member in members),
            "edges": sorted(canonical(edge) for edge in edges),
            "branches": sorted(canonical(branch) for branch in branches),
        })
    return result


def affected(before, after, current, previous, root=packages.ROOT):
    """Return package seeds and reasons; uncertain metadata selects the entire workspace.

    This comparison is deliberately not a resolver or a complete schema validator.
    Every package lane still validates the entire workspace with `resolve --locked`.
    """
    try:
        if any(document is None for document in (*before.values(), *after.values())):
            raise Unscoped("workspace metadata added or removed")
        catalogs = (directory_names(current, root), directory_names(previous, root))
        selected, reasons = manifest_impact(before["packages/fai.toml"], after["packages/fai.toml"], catalogs)
        old_graphs = lock_graphs(before["packages/fai.lock"])
        new_graphs = lock_graphs(after["packages/fai.lock"])
        for key in sorted(old_graphs.keys() | new_graphs.keys()):
            if old_graphs.get(key) != new_graphs.get(key):
                path, target = key
                selected.update(named_paths([path], catalogs))
                reasons.append(f"workspace graph: {path} ({target})")
        return selected, reasons
    except Unscoped as error:
        return set(current) | set(previous), [f"conservative workspace checks: {error}"]
