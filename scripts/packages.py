"""Source-package test metadata and dependency-aware selection."""

from dataclasses import dataclass
import json
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = "ci.json"
PLATFORMS = ("linux", "macos", "windows")
MODES = ("jit", "aot")
NAME = re.compile(r"[a-z][a-z0-9_-]*")
TEMPLATE = re.compile(r"\$\{([^}]+)\}")


class PackageError(ValueError):
    """Invalid package catalog or selection."""


@dataclass(frozen=True)
class Fixture:
    name: str
    repository: str
    revision: str


@dataclass(frozen=True)
class Integration:
    name: str
    entry: str
    args: tuple[str, ...] = ()
    env: tuple[tuple[str, str], ...] = ()
    modes: tuple[str, ...] = MODES
    platforms: tuple[str, ...] = PLATFORMS
    timeout_seconds: int = 180
    stdout: str | None = None
    stdout_lines: tuple[str, ...] = ()


@dataclass(frozen=True)
class Package:
    name: str
    directory: Path
    dependencies: tuple[str, ...]
    integrations: tuple[Integration, ...] = ()
    fixtures: tuple[Fixture, ...] = ()


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def fields(data, allowed, context):
    if not isinstance(data, dict):
        raise ValueError(f"{context} must be an object")
    unknown = data.keys() - allowed
    if unknown:
        raise ValueError(f"unknown {context} fields: {', '.join(sorted(unknown))}")


def name_value(value, context):
    if not isinstance(value, str) or not NAME.fullmatch(value):
        raise ValueError(f"invalid {context} name")
    return value


def strings(value, context):
    if not isinstance(value, list) or not all(isinstance(item, str) and "\0" not in item for item in value):
        raise ValueError(f"{context} must be an array of strings without NULs")
    for item in value:
        item.encode("utf-8")
    return tuple(value)


def choices(value, allowed, context):
    selected = strings(value, context)
    if not selected or len(set(selected)) != len(selected) or set(selected) - set(allowed):
        raise ValueError(f"{context} must contain distinct values from {', '.join(allowed)}")
    return selected


def entry_path(value):
    if not isinstance(value, str) or not value or "\0" in value or "\\" in value:
        raise ValueError("integration entry must be a relative .fai path with forward slashes")
    value.encode("utf-8")
    path = PurePosixPath(value)
    if (path.is_absolute() or PureWindowsPath(value).drive or ".." in path.parts
            or path.as_posix() != value or path.suffix != ".fai"):
        raise ValueError("integration entry must be a relative .fai path inside its package")
    return value


def fixture_value(data):
    fields(data, {"name", "repository", "revision"}, "fixture")
    name = name_value(data["name"], "fixture")
    repository = data["repository"]
    if not isinstance(repository, str) or any(char.isspace() for char in repository) or "\0" in repository:
        raise ValueError("fixture repository must be an HTTPS URL")
    repository.encode("utf-8")
    url = urlsplit(repository)
    if (url.scheme != "https" or not url.hostname or not url.path
            or url.username is not None or url.password is not None or url.query or url.fragment):
        raise ValueError("fixture repository must be an HTTPS URL without credentials, query or fragment")
    revision = data["revision"]
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", revision):
        raise ValueError("fixture revision must be a full Git commit hash")
    return Fixture(name, repository, revision.lower())


def template_tokens(value):
    tokens = TEMPLATE.findall(value)
    if "${" in TEMPLATE.sub("", value):
        raise ValueError("unterminated integration template")
    return tokens


def integration_value(data, fixtures):
    fields(data, {"name", "entry", "args", "env", "modes", "platforms", "timeoutSeconds", "stdout", "stdoutLines"}, "integration")
    name = name_value(data["name"], "integration")
    entry = entry_path(data["entry"])
    args = strings(data.get("args", []), "integration args")
    env = data.get("env", {})
    if not isinstance(env, dict) or not all(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key)
                                           and isinstance(value, str) and "\0" not in value
                                           for key, value in env.items()):
        raise ValueError("integration env must map environment names to strings without NULs")
    if len({key.casefold() for key in env}) != len(env):
        raise ValueError("integration env names must be distinct ignoring case")
    for value in env.values():
        value.encode("utf-8")
    for value in (*args, *env.values()):
        for token in template_tokens(value):
            if token in {"package", "workspace", "temp"}:
                continue
            if token.startswith("fixture:") and token[8:] in fixtures:
                continue
            raise ValueError(f"unknown integration template: ${{{token}}}")
    modes = choices(data.get("modes", list(MODES)), MODES, "integration modes")
    platforms = choices(data.get("platforms", list(PLATFORMS)), PLATFORMS, "integration platforms")
    timeout = data.get("timeoutSeconds", 180)
    if type(timeout) is not int or not 1 <= timeout <= 2147483647:
        raise ValueError("integration timeoutSeconds must be a positive 32-bit integer")
    stdout = data.get("stdout")
    if "stdout" in data and not isinstance(stdout, str):
        raise ValueError("integration stdout must be a string")
    if stdout is not None:
        stdout.encode("utf-8")
    lines = strings(data.get("stdoutLines", []), "integration stdoutLines")
    if ("stdoutLines" in data and not lines) or any(not line or "\n" in line or "\r" in line for line in lines):
        raise ValueError("integration stdoutLines must contain nonempty individual lines")
    if "stdout" in data and "stdoutLines" in data:
        raise ValueError("use stdout or stdoutLines, not both")
    return Integration(name, entry, args, tuple(sorted(env.items())), modes, platforms, timeout, stdout, lines)


def named_items(data, key, parse_item):
    items = data.get(key, [])
    if not isinstance(items, list):
        raise ValueError(f"{key} must be an array")
    parsed = tuple(parse_item(item) for item in items)
    if len({item.name for item in parsed}) != len(parsed):
        raise ValueError(f"duplicate {key} names")
    return parsed


def parse(directory, text):
    try:
        data = json.loads(text, object_pairs_hook=unique_object)
        fields(data, {"schemaVersion", "name", "dependencies", "integrations", "fixtures"}, "package")
        name = name_value(data["name"], "package")
        dependencies = data["dependencies"]
        if type(data["schemaVersion"]) is not int or data["schemaVersion"] != 1 or name != directory.name:
            raise ValueError("invalid name or schema")
        if not isinstance(dependencies, list) or not all(isinstance(name, str) and NAME.fullmatch(name) for name in dependencies) or len(set(dependencies)) != len(dependencies):
            raise ValueError("dependencies must be distinct package names")
        fixtures = named_items(data, "fixtures", fixture_value)
        integrations = named_items(data, "integrations", lambda item: integration_value(item, {fixture.name for fixture in fixtures}))
        return Package(name, directory, tuple(dependencies), integrations, fixtures)
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
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeError as error:
            raise PackageError(f"invalid UTF-8 in {path}") from error
        package = parse(directory, text)
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
