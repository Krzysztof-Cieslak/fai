#!/usr/bin/env python3
"""Supply process interop fixtures to named tests and retain AOT/terminal checks."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from package_fixture import prepare

ROOT = Path(__file__).resolve().parents[2]


def run(command, **kwargs):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, timeout=300, **kwargs)
    assert result.returncode == 0, (command, result.returncode, result.stdout, result.stderr)
    return result.stdout


def negative_checks(compiler, directory):
    prepare(directory, ["process"])
    path = directory / "Check.fai"
    source = """module Check
public bad : ProcessHost.Process -> ProcessHost.Session -> Unit
let bad host child = host.stop child
"""
    path.write_bytes(source.encode("utf-8"))
    result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples",
                             "--message-format=json", "-C", str(directory), str(path)],
                            capture_output=True, timeout=60)
    payload = json.loads(result.stdout)
    assert result.returncode != 0, payload
    errors = [d for d in payload["diagnostics"] if d["code"] == "FAI5001"]
    assert errors and all(0 <= d["primary"]["byteStart"] < d["primary"]["byteEnd"] <= len(source) for d in errors), payload

    source = """module Check
import process.Process
log : Console -> Process.Chunk -> Result Process.Control Process.Error / { Console }
let log console chunk =
  let written = console.writeLine "chunk"
  Ok Process.Continue
public bad : { concurrency : Concurrency, console : Console, process : ProcessHost.Process } -> Process.Command -> Result Process.Output Process.Error / { Concurrency, ProcessHost.Process }
let bad env command = Process.withOutput env Process.defaults command (log env.console)
"""
    path.write_bytes(source.encode("utf-8"))
    result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples",
                             "--message-format=json", "-C", str(directory), str(path)],
                            capture_output=True, timeout=60)
    payload = json.loads(result.stdout)
    assert result.returncode != 0 and any(d["code"] == "FAI5001" for d in payload["diagnostics"]), payload
    source = source.replace("/ { Concurrency, ProcessHost.Process }", "/ { Concurrency, Console, ProcessHost.Process }")
    path.write_bytes(source.encode("utf-8"))
    run([compiler, "check", "--no-daemon", "--no-examples", "-C", str(directory), str(path)])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    candidate = Path(args.fai)
    compiler = str(candidate.resolve()) if candidate.exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("fai executable not found")
    fixture = str(ROOT / "scripts/tests/process_child.py")
    program = "process/examples/RuntimeChecks.fai"
    with tempfile.TemporaryDirectory(prefix="fai-process-") as temporary:
        directory = Path(temporary)
        # The interoperability fixture remains external, but individual process
        # assertions are ordinary Fai tests with per-case reporting.
        suite = directory / "suite"
        prepare(suite, ["process"])
        (suite / "tests").mkdir()
        source = (ROOT / "packages" / program).read_text(encoding="utf-8")
        source = source.replace("module ProcessRuntimeChecks\n", "module ProcessRuntimeChecks\n\nimport process.Process\n", 1)
        (suite / "tests/RuntimeChecks.fai").write_text(source, encoding="utf-8")
        reports = Path(os.environ.get("RUNNER_TEMP", temporary)) / "fai-junit"
        report = reports / "process-interop.xml"
        run([compiler, "test", "--no-daemon", "--deny-focused", "-C", str(suite),
             "--junit", str(report)],
             env={**os.environ, "FAI_TEST_PYTHON": sys.executable, "FAI_TEST_PROCESS_FIXTURE": fixture})
        cases = ET.parse(report).findall(".//testcase")
        assert len(cases) == 21 and all(
            json.loads(case.attrib["name"])[0] == "native process sessions"
            and not any(case.find(tag) is not None for tag in ("failure", "error", "skipped"))
            for case in cases
        ), ET.tostring(ET.parse(report).getroot())
        executable = directory / "process-checks.exe"
        run([compiler, "build", "--no-daemon", "-C", "packages", program,
             "--out", str(executable)])
        output = run([str(executable), sys.executable, fixture])
        assert b"process checks passed" in output and b"FAIL" not in output and b"inherited-marker" in output, output
        output = run([str(executable), sys.executable, fixture, "inherit-stdin"], input=bytes(range(256)))
        assert b"inherited input ok" in output, output
        negative_checks(compiler, directory)
        for example in ("Capture", "TaskRunner"):
            run([compiler, "build", "--no-daemon", "-C", "packages",
                 f"process/examples/{example}.fai", "--out", str(directory / f"{example}.exe")])
        if os.name != "nt":
            from tui_terminal import exercise
            exercise([str(directory / "TaskRunner.exe"), sys.executable, fixture,
                      "args", "process-ui-output"], b"Process task runner", b"\r",
                     b"\x03", expected=b"completed")
            exercise([str(directory / "TaskRunner.exe"), sys.executable, fixture,
                      "hold"], b"Process task runner", b"\r", b"\x03")
    print("Process named suites, AOT, byte-pipe, lifecycle and effect checks passed")


if __name__ == "__main__":
    main()
