#!/usr/bin/env python3
"""Measure a frozen main revision, validate it, and publish three current reports."""

import argparse
from datetime import datetime, timezone
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys

import performance_report

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "scripts/performance-trackers.json"
ENVIRONMENT = ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "OCAMLPARAM", "OCAMLRUNPARAM",
               "GLIBC_TUNABLES", "MALLOC_ARENA_MAX", "FAI_WORKERS", "RUSTC", "RUSTC_WRAPPER", "RUSTUP_TOOLCHAIN")
ROLES = ("perceus", "memory", "runtime")


def utc():
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def output(command, cwd=ROOT, env=None):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True).strip()


def configuration(path):
    value = read_json(path)
    if value.get("schemaVersion") != 1 or set(value.get("trackers", {})) != set(ROLES):
        raise ValueError("invalid performance tracker configuration")
    if not re.fullmatch(r"[A-Za-z0-9_-]+/[A-Za-z0-9_-]+", value.get("repository", "")):
        raise ValueError("invalid repository")
    reports = dict(zip(ROLES, ["perceus-memory.md", "language-memory.md", "language-runtime.md"]))
    for role, tracker in value["trackers"].items():
        tracker.setdefault("title", performance_report.TITLES[role])
        if not isinstance(tracker.get("title"), str) or not tracker["title"] or tracker.get("report") != reports[role]:
            raise ValueError(f"invalid tracker: {role}")
        if tracker.get("issue") is not None and (type(tracker["issue"]) is not int or tracker["issue"] <= 0):
            raise ValueError("tracker issue numbers must be positive integers")
    superseded = value.get("supersededIssues", [])
    if len(superseded) != len(set(superseded)) or any(type(n) is not int or n <= 0 for n in superseded):
        raise ValueError("invalid superseded issue list")
    ids = [tracker["issue"] for tracker in value["trackers"].values() if tracker.get("issue") is not None]
    if len(ids) != len(set(ids)) or set(ids).intersection(superseded):
        raise ValueError("tracker IDs overlap each other or superseded issues")
    return value


def environment(manifest, source):
    env = dict(os.environ)
    for key, value in manifest["environment"].items():
        if value is None:
            env.pop(key, None)
        else:
            env[key] = value
    for key in list(env):
        if key.startswith("DIVAN_") or key in {"FAI_REPORT_RSS", "FAI_REPORT_ALLOCATIONS"}:
            env.pop(key)
    env.update(CARGO_INCREMENTAL="0", CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0",
               CARGO_BUILD_JOBS=env.get("CARGO_BUILD_JOBS", "1"),
               GITHUB_SHA=manifest["measuredRevision"], GITHUB_REPOSITORY=manifest["repository"],
               FAI_BENCH_OCAMLOPT=manifest["toolchains"]["ocamlExecutable"])
    if output(["git", "rev-parse", "HEAD"], source) != manifest["measuredRevision"]:
        raise ValueError("measurement worktree moved from the pinned revision")
    if output(["git", "status", "--porcelain", "--untracked-files=no"], source):
        raise ValueError("measurement source has tracked modifications")
    return env


def host():
    cpu = platform.processor() or platform.machine()
    if Path("/proc/cpuinfo").exists():
        match = re.search(r"^model name\s*:\s*(.+)$", Path("/proc/cpuinfo").read_text(), re.M)
        if match:
            cpu = match[1]
    return {"system": platform.system(), "machine": platform.machine(), "kernel": platform.release(), "cpu": cpu}


def executable(path):
    with path.open("rb") as stream:
        magic = stream.read(4)
    return magic == b"\x7fELF" or magic[:2] == b"MZ" or magic in {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"}


def compress_artifacts(run, folder, state):
    """Compress only completed executable artifacts, preserving exact bytes."""
    for path in sorted(folder.rglob("*")):
        if not path.is_file() or not executable(path):
            continue
        relative = path.relative_to(run).as_posix()
        if relative == "bin/performance-report":
            continue
        target = path.with_name(path.name + ".gz")
        temporary = target.with_name(target.name + ".tmp")
        expected = digest(path)
        with path.open("rb") as source, temporary.open("wb") as destination:
            with gzip.GzipFile(filename="", mode="wb", fileobj=destination, mtime=0) as compressed:
                shutil.copyfileobj(source, compressed)
        with gzip.open(temporary, "rb") as stream:
            if hashlib.file_digest(stream, "sha256").hexdigest() != expected:
                raise ValueError(f"compression verification failed: {relative}")
        temporary.replace(target)
        blobs = state.setdefault("compressedBlobs", {})
        compressed_hash = digest(target)
        previous = run / blobs.get(compressed_hash, target.relative_to(run).as_posix())
        if previous != target and previous.is_file() and digest(previous) == compressed_hash:
            linked = target.with_name(target.name + ".link")
            try:
                os.link(previous, linked)
                linked.replace(target)
            except OSError:
                linked.unlink(missing_ok=True)
        else:
            blobs[compressed_hash] = target.relative_to(run).as_posix()
        state["compressed"][relative] = target.relative_to(run).as_posix()
        write_json(run / "state.json", state)
        path.unlink()


def preserve_attempt(run, name, state):
    suffix = utc().replace(":", "-")
    destination = run / "attempts" / f"{name}-{suffix}"
    for relative in [f"raw/{name}.stdout", f"raw/{name}.stderr", f"artifacts/{name}"]:
        path = run / relative
        if path.exists():
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            path.rename(target)
            for logical, stored in list(state["compressed"].items()):
                if logical == relative or logical.startswith(relative + "/"):
                    prefix = destination.relative_to(run).as_posix() + "/"
                    del state["compressed"][logical]
                    state["compressed"][prefix + logical] = prefix + stored
    if destination.exists():
        compress_artifacts(run, destination, state)
    write_json(run / "state.json", state)


def host_observation(cpu):
    observation = {}
    for name in ["loadavg", "meminfo", "stat"]:
        path = Path("/proc") / name
        if path.exists():
            text = path.read_text()
            if name == "stat":
                text = "\n".join(line for line in text.splitlines() if line.startswith(("cpu ", f"cpu{cpu} ")))
            observation[name] = text
    frequency = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
    for name in ["scaling_governor", "scaling_cur_freq", "scaling_min_freq", "scaling_max_freq"]:
        path = frequency / name
        if path.exists():
            observation[name] = path.read_text().strip()
    return observation


def build(run, manifest, state, role, env):
    known = state["binaries"].get(role)
    if known:
        for item in known.values():
            path = run / item["path"]
            if not path.exists() and item["path"] in state["compressed"]:
                with gzip.open(run / state["compressed"][item["path"]], "rb") as source, path.open("wb") as target:
                    shutil.copyfileobj(source, target)
                path.chmod(0o755)
            if not path.exists() or digest(path) != item["sha256"]:
                raise ValueError(f"missing or changed retained executable: {path}")
        return {name: run / item["path"] for name, item in known.items()}
    source = run / "source"
    target = run / "build"
    prefix = ["cargo"]
    if role == "timing":
        commands = [prefix + ["bench", "-p", "fai-tests", "--bench", "algorithms_aot_warm", "--bench", "algorithms_aot", "--bench", "algorithms_mem", "--no-run", "--locked", "--message-format=json", "--target-dir", str(target)],
                    prefix + ["build", "--release", "-p", "fai-tests", "--bin", "performance-report", "--locked", "--message-format=json", "--target-dir", str(target)]]
        wanted = {"algorithms_aot_warm", "algorithms_aot", "algorithms_mem", "performance-report"}
    elif role == "allocation":
        commands = [prefix + ["bench", "-p", "fai-tests", "--features", "allocation-stats", "--bench", "allocation_design", "--no-run", "--locked", "--message-format=json", "--target-dir", str(target)]]
        wanted = {"allocation_design"}
    else:
        commands = [prefix + ["test", "-p", "fai-tests", "--test", "perceus_space", "--test", "perceus_storage", "--no-run", "--locked", "--message-format=json", "--target-dir", str(target)]]
        wanted = {"perceus_space", "perceus_storage"}
    binaries = {}
    for index, command in enumerate(commands):
        log = run / "build-records" / f"{role}-{index}.jsonl"
        if log.exists():
            log = log.with_name(f"{role}-{index}-{len(list(log.parent.glob(role + '-*')))}.jsonl")
        error = log.with_suffix(".stderr")
        log.parent.mkdir(parents=True, exist_ok=True)
        with log.open("w") as stdout, error.open("w") as stderr:
            result = subprocess.run(command, cwd=source, env=env, stdout=stdout, stderr=stderr)
        if result.returncode:
            raise RuntimeError(f"build failed; see {error}")
        for line in log.read_text().splitlines():
            row = json.loads(line)
            if row.get("reason") != "compiler-artifact" or not row.get("executable") or row["target"]["name"] not in wanted:
                continue
            profile = row["profile"]
            if profile["debug_assertions"] != (role == "guards") or (role != "guards" and profile["opt_level"] != "3"):
                raise ValueError("unexpected benchmark build profile")
            name = row["target"]["name"]
            destination = run / "bin" / role / name
            if name == "performance-report":
                destination = run / "bin/performance-report"
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(row["executable"], destination)
            binaries[name] = destination
    if set(binaries) != wanted:
        raise ValueError(f"missing built executables: {wanted - binaries.keys()}")
    manifest["builds"] = [item for item in manifest["builds"] if item["name"] != role]
    manifest["builds"].append({"name": role, "revision": manifest["measuredRevision"], "commands": commands,
                               "optimization": "0" if role == "guards" else "3", "debugAssertions": role == "guards", "instrumented": role != "timing"})
    state["binaries"][role] = {name: {"path": str(path.relative_to(run)), "sha256": digest(path)} for name, path in binaries.items()}
    write_json(run / "state.json", state)
    write_json(run / "snapshot.json", manifest)
    if role == "timing":
        write_json(run / "catalog.json", json.loads(output([str(binaries["performance-report"]), "catalog"], source, env)))
    return binaries


def step(run, manifest, state, name, command, role, env):
    prior = next((item for item in manifest["steps"] if item["name"] == name), None)
    if prior and prior["exitCode"] == 0 and (run / prior["stdout"]).is_file() and (run / prior["stderr"]).is_file() and (run / prior["artifacts"]).is_dir():
        compress_artifacts(run, run / prior["artifacts"], state)
        shutil.rmtree(run / "tmp" / name, ignore_errors=True)
        return
    preserve_attempt(run, name, state)
    directory = run / "artifacts" / name
    directory.mkdir(parents=True)
    stdout = run / "raw" / f"{name}.stdout"
    stderr = stdout.with_suffix(".stderr")
    stdout.parent.mkdir(parents=True, exist_ok=True)
    temporary = run / "tmp" / name
    temporary.mkdir(parents=True, exist_ok=True)
    local = dict(env, TMPDIR=str(temporary), FAI_BENCH_ARTIFACT_DIR=str(directory))
    started = utc()
    before = host_observation(manifest["machine"]["benchmarkCpu"])
    print(f"Running {name}", flush=True)
    measured = ["taskset", "-c", manifest["machine"]["benchmarkCpu"], *map(str, command)]
    with stdout.open("w") as out, stderr.open("w") as err:
        result = subprocess.run(measured, cwd=run / "source", env=local, stdout=out, stderr=err)
    write_json(directory / "host.json", {"before": before, "after": host_observation(manifest["machine"]["benchmarkCpu"])})
    passed = None
    if role == "guards" and result.returncode == 0:
        matches = re.findall(r"test result: ok\. (\d+) passed; 0 failed;", stdout.read_text())
        if len(matches) != 1 or int(matches[0]) == 0:
            raise ValueError(f"guard did not execute tests: {name}")
        passed = int(matches[0])
    record = {"name": name, "command": measured, "exitCode": result.returncode,
              "stdout": stdout.relative_to(run).as_posix(), "stderr": stderr.relative_to(run).as_posix(),
              "artifacts": directory.relative_to(run).as_posix(), "build": role,
              "startedAt": started, "finishedAt": utc(), "passed": passed}
    manifest["steps"] = [item for item in manifest["steps"] if item["name"] != name] + [record]
    write_json(run / "snapshot.json", manifest)
    if result.returncode:
        raise RuntimeError(f"{name} failed; retained output: {stdout}, {stderr}")
    compress_artifacts(run, directory, state)
    shutil.rmtree(temporary)
    print(f"Completed {name}", flush=True)


def index_artifacts(run, manifest, state):
    renderer = run / "bin/performance-report"
    paths = []
    for folder in ["raw", "artifacts", "bin", "build-records", "attempts"]:
        paths.extend(path for path in (run / folder).rglob("*") if path.is_file())
    paths.extend(run / name for name in ["catalog.json", "source.tar.gz"])
    inverse = {stored: logical for logical, stored in state["compressed"].items()}
    files = []
    for path in sorted(paths):
        stored = path.relative_to(run).as_posix()
        compressed = stored in inverse
        command = [str(renderer), "fingerprint", str(path), *(["--gzip"] if compressed else [])]
        row = json.loads(output(command, run))
        row.update(path=inverse.get(stored, stored), storedPath=stored)
        files.append(row)
    manifest["files"] = files


def configure_validator(run, executable, revision):
    """Retain a newer exporter for a completed historical snapshot."""
    manifest = read_json(run / "snapshot.json")
    if manifest["status"] != "complete" or not revision or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("a validator override needs a complete snapshot and its full source revision")
    executable = Path(executable).resolve()
    identity = digest(executable)
    directory = run / "presentation"
    directory.mkdir(exist_ok=True)
    name = "validator-" + identity[:16] + (".exe" if executable.suffix == ".exe" else "")
    destination = directory / name
    if not destination.exists():
        shutil.copy2(executable, destination)
    write_json(directory / "validator.json", {
        "file": name, "sha256": identity, "revision": revision,
        "snapshotSha256": digest(run / "snapshot.json"),
    })


def validator(run, manifest):
    override = run / "presentation/validator.json"
    if override.exists():
        info = read_json(override)
        if Path(info["file"]).name != info["file"] or info["snapshotSha256"] != digest(run / "snapshot.json"):
            raise ValueError("invalid historical validator identity")
        path = run / "presentation" / info["file"]
        expected = info["sha256"]
        revision = info["revision"]
    else:
        path = run / "bin/performance-report"
        recorded = next((row for row in manifest["files"] if row["path"] == "bin/performance-report"), None)
        if not recorded:
            raise ValueError("retained report-data validator is missing")
        expected = recorded["sha256"]
        revision = manifest["reporterRevision"]
    if digest(path) != expected:
        raise ValueError("retained report-data validator is missing or changed")
    return path, {"sha256": expected, "revision": revision}


def render(run, evidence=""):
    manifest = read_json(run / "snapshot.json")
    executable, validator_info = validator(run, manifest)
    subprocess.run([str(executable), "export", str(run), str(run / "reports")], check=True)
    data_path = run / "reports/report-data.json"
    data = read_json(data_path)
    snapshot_hash = digest(run / "snapshot.json")
    if data.get("snapshotSha256") != snapshot_hash:
        raise ValueError("exported report data belongs to a different snapshot")
    reports = performance_report.render(data, evidence)
    source = ROOT / "scripts/performance_report"
    source_files = {path.name: digest(path) for path in sorted(source.glob("*.py"))}
    renderer_id = hashlib.sha256(json.dumps(source_files, sort_keys=True).encode()).hexdigest()
    retained = run / "presentation" / renderer_id
    retained.mkdir(parents=True, exist_ok=True)
    for name in source_files:
        shutil.copy2(source / name, retained / name)
    for name, text in reports.items():
        (run / "reports" / name).write_text(text, encoding="utf-8", newline="\n")
    write_json(run / "reports/render-info.json", {
        "schemaVersion": 1, "snapshotSha256": snapshot_hash, "reportDataSha256": digest(data_path),
        "validator": validator_info, "rendererId": renderer_id, "rendererFiles": source_files,
        "rendererRevision": output(["git", "rev-parse", "HEAD"]), "pythonVersion": platform.python_version(),
    })


def release_timing_cache(run, state, env):
    """Retained bin/ images embed their runtime; Cargo's copies are disposable."""
    if state.get("timingCacheReleased") or not (run / "build/release").exists():
        return
    if shutil.disk_usage(run).free >= 2_000_000_000:
        return
    command = ["cargo", "clean", "--profile", "release", "-p", "fai-tests", "-p", "fai-driver", "--target-dir", str(run / "build")]
    output = subprocess.run(command, cwd=run / "source", env=env, check=True, capture_output=True, text=True)
    write_json(run / "build-records/timing-cache-release.json", {
        "command": command, "stdout": output.stdout, "stderr": output.stderr, "finishedAt": utc()})
    state["timingCacheReleased"] = True
    write_json(run / "state.json", state)


def execute(run):
    manifest = read_json(run / "snapshot.json")
    if manifest["status"] == "complete":
        render(run)
        return
    state = read_json(run / "state.json")
    if host() != {key: manifest["machine"][key] for key in host()}:
        raise ValueError("resumption needs the recorded host configuration")
    env = environment(manifest, run / "source")
    if output([env.get("RUSTC", "rustc"), "-vV"], run / "source", env) != manifest["toolchains"]["rustDetails"]:
        raise ValueError("Rust compiler changed since the snapshot started")
    if output([manifest["toolchains"]["ocamlExecutable"], "-config"], run / "source", env) != manifest["toolchains"]["ocamlConfiguration"]:
        raise ValueError("OCaml configuration changed since the snapshot started")
    settings = manifest["settings"]
    try:
        normal = build(run, manifest, state, "timing", env)
        catalog = json.loads(output([str(normal["performance-report"]), "catalog"], run / "source", env))
        write_json(run / "catalog.json", catalog)
        manifest["methodology"] = catalog["methodology"]
        write_json(run / "snapshot.json", manifest)
        release_timing_cache(run, state, env)
        step(run, manifest, state, "warm", [normal["algorithms_aot_warm"], "--samples", str(settings["warmSamples"]), "--passes", str(settings["warmPasses"])], "timing", env)
        step(run, manifest, state, "components", [normal["algorithms_aot_warm"], "--components", "--samples", str(settings["warmSamples"]), "--passes", str(settings["warmPasses"])], "timing", env)
        for n in range(1, settings["aotPasses"] + 1):
            release_timing_cache(run, state, env)
            step(run, manifest, state, f"aot-{n}", [normal["algorithms_aot"], "--bench", "--sample-count", str(settings["aotSamples"]), "--min-time", "0.25", "--max-time", "120", "--sortr" if n % 2 == 0 else "--sort", "name"], "timing", env)
        release_timing_cache(run, state, env)
        step(run, manifest, state, "memory", [normal["algorithms_mem"], "--size", "all", "--samples", str(settings["memorySamples"]), "--repeats", ",".join(map(str, settings["memoryRepeats"]))], "timing", env)
        release_timing_cache(run, state, env)
        instrumented = build(run, manifest, state, "allocation", env)
        step(run, manifest, state, "allocations", [instrumented["allocation_design"]], "allocation", env)
        if (run / "build/release").exists():
            shutil.rmtree(run / "build/release")
        guards = build(run, manifest, state, "guards", env)
        for name in ["perceus_space", "perceus_storage"]:
            step(run, manifest, state, name, [guards[name], "--test-threads=1"], "guards", env)
        compress_artifacts(run, run / "bin", state)
        index_artifacts(run, manifest, state)
        manifest.update(status="complete", completedAt=utc())
        write_json(run / "snapshot.json", manifest)
        render(run)
    except (Exception, KeyboardInterrupt):
        manifest["status"] = "incomplete"
        write_json(run / "snapshot.json", manifest)
        raise
    if (run / "build").exists():
        shutil.rmtree(run / "build")
    subprocess.run(["git", "worktree", "remove", str(run / "source")], cwd=ROOT, check=True)
    print(f"Complete validated snapshot: {run}", flush=True)


def start(args):
    if platform.system() != "Linux" or not shutil.which("taskset"):
        raise ValueError("complete footprint checkpoints require Linux and taskset")
    config = configuration(args.config)
    compiler = args.ocaml or os.environ.get("FAI_BENCH_OCAMLOPT", "ocamlopt")
    compiler = shutil.which(compiler) or compiler
    ocaml = output([compiler, "-config"])
    version = re.search(r"^version:\s*(.+)$", ocaml, re.M)
    if not version or version[1] != config["ocamlVersion"] or "flambda: true" not in ocaml:
        raise ValueError("selected OCaml must match the configured version with Flambda enabled")
    subprocess.run(["git", "fetch", "--no-tags", args.remote, "main"], cwd=ROOT, check=True)
    revision = output(["git", "rev-parse", "FETCH_HEAD"])
    run_id = utc().replace(":", "").replace("-", "") + "-" + revision[:12]
    run = (args.output or ROOT / "target/performance" / run_id).resolve()
    run.mkdir(parents=True, exist_ok=False)
    subprocess.run(["git", "worktree", "add", "--detach", str(run / "source"), revision], cwd=ROOT, check=True)
    source = run / "source"
    rust = output([os.environ.get("RUSTC", "rustc"), "-vV"], source)
    machine = host()
    allowed = os.sched_getaffinity(0)
    cpu = args.cpu if args.cpu is not None else 1 if 1 in allowed else min(allowed)
    if cpu not in allowed:
        raise ValueError("benchmark CPU is outside the allowed affinity")
    machine["benchmarkCpu"] = str(cpu)
    env = {key: os.environ.get(key) for key in ENVIRONMENT}
    manifest = {"schemaVersion": 2, "runId": run_id, "status": "incomplete", "repository": config["repository"],
                "measuredRevision": revision, "reporterRevision": revision, "startedAt": utc(), "completedAt": "",
                "methodology": "", "machine": machine, "environment": env,
                "toolchains": {"rustVersion": rust.splitlines()[0], "rustDetails": rust, "ocamlVersion": version[1], "ocamlConfiguration": ocaml, "ocamlExecutable": str(Path(compiler).resolve())},
                "settings": config["settings"], "notes": config.get("notes", {}), "builds": [], "steps": [], "files": []}
    write_json(run / "snapshot.json", manifest)
    write_json(run / "state.json", {"compressed": {}, "binaries": {}})
    with (run / "source.tar.gz").open("wb") as archive:
        subprocess.run(["git", "archive", "--format=tar.gz", revision], cwd=ROOT, stdout=archive, check=True)
    execute(run)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="fetch and pin main, then measure a complete checkpoint")
    run.add_argument("--remote", default="origin")
    run.add_argument("--output", type=Path)
    run.add_argument("--ocaml")
    run.add_argument("--cpu", type=int)
    run.add_argument("--config", type=Path, default=CONFIG)
    resume = sub.add_parser("resume", help="resume missing/failed steps of the same pinned run")
    resume.add_argument("snapshot", type=Path)
    report = sub.add_parser("render", help="strictly validate and regenerate all three reports")
    report.add_argument("snapshot", type=Path)
    report.add_argument("--evidence-url", default="")
    report.add_argument("--validator", type=Path, help="retain a current exporter for an older snapshot")
    report.add_argument("--validator-revision", help="full source revision of the selected exporter")
    publish = sub.add_parser("publish", help="publish a validated snapshot to configured trackers")
    publish.add_argument("snapshot", type=Path)
    publish.add_argument("--config", type=Path, default=CONFIG)
    publish.add_argument("--create", action="store_true")
    publish.add_argument("--migrate", action="store_true")
    publish.add_argument("--record-ids", action="store_true")
    publish.add_argument("--validator", type=Path, help="retain a current exporter for an older snapshot")
    publish.add_argument("--validator-revision", help="full source revision of the selected exporter")
    args = parser.parse_args()
    if getattr(args, "validator_revision", None) and not getattr(args, "validator", None):
        raise ValueError("--validator-revision requires --validator")
    if getattr(args, "validator", None):
        configure_validator(args.snapshot.resolve(), args.validator, args.validator_revision)
    if args.command == "run":
        start(args)
    elif args.command == "resume":
        execute(args.snapshot.resolve())
    elif args.command == "render":
        render(args.snapshot.resolve(), args.evidence_url)
    else:
        import performance_publish
        performance_publish.publish(args.snapshot.resolve(), configuration(args.config), args, render)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, subprocess.CalledProcessError, OSError) as error:
        print(f"performance: {error}", file=sys.stderr)
        sys.exit(1)
