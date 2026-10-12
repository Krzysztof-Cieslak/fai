#!/usr/bin/env python3
"""Run package-declared Fai integration programs with an existing compiler."""

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile

import packages


class IntegrationError(RuntimeError):
    """An integration could not run or did not satisfy its declared outcome."""


def host_platform():
    name = {"Linux": "linux", "Darwin": "macos", "Windows": "windows"}.get(platform.system())
    if name is None:
        raise IntegrationError(f"unsupported integration host: {platform.system()}")
    return name


def selected(catalog, names, ci=False, dependents=True, environment=None):
    environment = os.environ if environment is None else environment
    if ci:
        if names:
            raise IntegrationError("--ci cannot be combined with package names")
        try:
            names = json.loads(environment.get("CI_PACKAGES", "null"))
        except ValueError as error:
            raise IntegrationError(f"invalid CI_PACKAGES: {error}") from error
        if (not isinstance(names, list) or not all(isinstance(name, str) for name in names)
                or len(set(names)) != len(names) or names != environment.get("PACKAGE_NAMES", "").split()):
            raise IntegrationError("missing or inconsistent CI package selection")
        dependents = False
    elif not names:
        raise IntegrationError("provide package names or --ci")
    return packages.affected(catalog, names, dependents)


def plan(catalog, names, host):
    if host not in packages.PLATFORMS:
        raise IntegrationError(f"unsupported integration host: {host}")
    tasks = []
    for name in names:
        package = catalog[name]
        for integration in package.integrations:
            if host not in integration.platforms:
                continue
            directory = package.directory.resolve()
            entry = (directory / integration.entry).resolve()
            if not entry.is_relative_to(directory) or not entry.is_file():
                raise IntegrationError(f"{name}/{integration.name}: missing or external entry {integration.entry}")
            tasks.append((package, integration, entry))
    return tasks


def display_output(result):
    if result.stdout:
        print(result.stdout.decode("utf-8", errors="replace"), end="" if result.stdout.endswith(b"\n") else "\n", flush=True)
    if result.stderr:
        print(result.stderr.decode("utf-8", errors="replace"), end="" if result.stderr.endswith(b"\n") else "\n", file=sys.stderr, flush=True)


def command(arguments, cwd, environment, timeout, label):
    try:
        result = subprocess.run([str(arg) for arg in arguments], cwd=cwd, env=environment,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, check=False)
    except subprocess.TimeoutExpired as error:
        display_output(error)
        raise IntegrationError(f"{label}: timed out after {timeout}s") from error
    except OSError as error:
        raise IntegrationError(f"{label}: {error}") from error
    display_output(result)
    if result.returncode != 0:
        raise IntegrationError(f"{label}: exited with status {result.returncode}")
    return result


def verify_output(integration, result, label):
    if integration.stdout is not None and result.stdout != integration.stdout.encode("utf-8"):
        raise IntegrationError(f"{label}: stdout did not match the declared output")
    lines = result.stdout.splitlines()
    missing = [line for line in integration.stdout_lines if line.encode("utf-8") not in lines]
    if missing:
        raise IntegrationError(f"{label}: missing stdout lines: {missing!r}")


def fixture_names(integration):
    return {token[8:] for value in (*integration.args, *(value for _, value in integration.env))
            for token in packages.template_tokens(value) if token.startswith("fixture:")}


def checkout_fixture(fixture, destination, environment):
    destination.mkdir(parents=True)
    label = f"fixture {fixture.name}"
    command(["git", "init", "--quiet", str(destination)], destination.parent, environment, 180, label)
    command(["git", "remote", "add", "origin", fixture.repository], destination, environment, 180, label)
    command(["git", "fetch", "--quiet", "--no-tags", "--depth=1", "origin", fixture.revision], destination, environment, 180, label)
    command(["git", "-c", "core.autocrlf=false", "checkout", "--quiet", "--detach", "FETCH_HEAD"], destination, environment, 180, label)
    result = command(["git", "rev-parse", "HEAD"], destination, environment, 180, label)
    if result.stdout.decode("ascii").strip().lower() != fixture.revision:
        raise IntegrationError(f"{label}: fetched revision differs from the declared commit")
    return destination.resolve()


def expand(value, package, workspace, temporary, fixtures):
    paths = {"package": package.directory.resolve(), "workspace": workspace, "temp": temporary,
             **{f"fixture:{name}": path for name, path in fixtures.items()}}

    def replace(match):
        try:
            return str(paths[match.group(1)])
        except KeyError as error:
            raise IntegrationError(f"unknown integration template: {match.group(0)}") from error

    return packages.TEMPLATE.sub(replace, value)


def run_task(compiler, workspace, package, integration, entry, temporary, fixtures, environment):
    values = lambda text: expand(text, package, workspace, temporary, fixtures)
    args = [values(arg) for arg in integration.args]
    overrides = {key: values(value) for key, value in integration.env}
    replaced = {key.casefold() for key in overrides} if os.name == "nt" else set(overrides)
    env = {key: value for key, value in environment.items()
           if (key.casefold() if os.name == "nt" else key) not in replaced}
    env.update(overrides)
    label = f"{package.name}/{integration.name}"
    compiler_root = workspace
    for directory in entry.parents:
        if not directory.is_relative_to(workspace):
            break
        if (directory / packages.MANIFEST).is_file():
            compiler_root = directory
            break
    for mode in integration.modes:
        print(f"Integration {label} [{mode}]", flush=True)
        if mode == "jit":
            arguments = [compiler, "run", "--no-daemon", "-C", compiler_root, entry, "--", *args]
        else:
            binary = temporary / "program.exe"
            command([compiler, "build", "--no-daemon", "-C", compiler_root, entry, "--out", binary],
                    workspace, env, integration.timeout_seconds, f"{label} [aot build]")
            arguments = [binary, *args]
        result = command(arguments, workspace, env, integration.timeout_seconds, f"{label} [{mode}]")
        verify_output(integration, result, f"{label} [{mode}]")


def run(catalog, names, compiler, root=packages.ROOT, host=None, environment=None, temporary_parent=None):
    environment = dict(os.environ if environment is None else environment)
    host = host or host_platform()
    tasks = plan(catalog, names, host)
    print(f"Package integrations: {', '.join(names) or '(none)'}; host={host}; programs={len(tasks)}", flush=True)
    if not tasks:
        return 0
    resolved = shutil.which(str(compiler), path=environment.get("PATH"))
    if resolved is None:
        raise IntegrationError(f"compiler executable not found: {compiler}")
    compiler = Path(resolved).resolve()
    workspace = (root / "packages").resolve()
    if temporary_parent is None:
        preferred = Path("/tmp/opencode")
        temporary_parent = preferred if preferred.is_dir() else environment.get("RUNNER_TEMP")
    with tempfile.TemporaryDirectory(prefix="fai-package-integrations-", dir=temporary_parent) as directory:
        temporary = Path(directory).resolve()
        restored = {}
        for package, integration, entry in tasks:
            fixtures = {}
            needed = fixture_names(integration)
            for fixture in package.fixtures:
                if fixture.name not in needed:
                    continue
                key = (package.name, fixture.name)
                if key not in restored:
                    restored[key] = checkout_fixture(fixture, temporary / package.name / "fixtures" / fixture.name, environment)
                fixtures[fixture.name] = restored[key]
            task_directory = temporary / package.name / "programs" / integration.name
            task_directory.mkdir(parents=True)
            run_task(compiler, workspace, package, integration, entry, task_directory, fixtures, environment)
    print(f"Passed {len(tasks)} package integration programs", flush=True)
    return len(tasks)


def main():
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="backslashreplace")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("packages", nargs="*")
    parser.add_argument("--fai", default="fai", help="existing compiler executable (default: fai on PATH)")
    parser.add_argument("--ci", action="store_true", help="use the exact CI_PACKAGES/PACKAGE_NAMES selection")
    parser.add_argument("--no-dependents", action="store_true", help="run only the named packages")
    parser.add_argument("--list", action="store_true", help="list selected programs without executing them")
    args = parser.parse_args()
    try:
        catalog = packages.discover()
        names = selected(catalog, args.packages, args.ci, not args.no_dependents)
        if args.list:
            for package, integration, _ in plan(catalog, names, host_platform()):
                print(f"{package.name}/{integration.name}: {integration.entry} [{', '.join(integration.modes)}]")
        else:
            run(catalog, names, args.fai)
    except (packages.PackageError, IntegrationError, OSError) as error:
        parser.exit(1, f"Package integrations: {error}\n")


if __name__ == "__main__":
    main()
