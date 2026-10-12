#!/usr/bin/env python3
"""Check logging effects, record integrity, queue bounds and scoped shutdown."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
from package_fixture import prepare

ROOT = Path(__file__).resolve().parents[2]


def run(command):
    result = subprocess.run(command, cwd=ROOT, capture_output=True, timeout=180)
    assert result.returncode == 0, (command, result.stdout, result.stderr)
    return result


def negative_checks(compiler, directory):
    prepare(directory, ["logging"])
    cases = [
        ("FAI5001", "public bad : Log.Logger { Console } -> Result Log.Receipt Log.Error\nlet bad logger = Log.info logger \"hidden\" []\n"),
        ("FAI5001", "public bad : LogBestEffort.Logger { Console } -> Unit\nlet bad logger = LogBestEffort.info logger \"hidden\" []\n"),
        ("FAI5001", "public bad : Runtime -> LogSink.Output { FileSystem } -> Result (LogQueue.Completed Unit) (LogQueue.Error String) / { Clock, Concurrency }\nlet bad env output = LogQueue.withOutput env LogQueue.defaults output (fun queued -> Ok ())\n"),
        ("FAI3001", "build : Console -> Unit -> Log.Entry / { Console }\nlet build console unit =\n  let written = console.writeLine \"builder\"\n  Log.entry \"effectful\" []\npublic bad : Console -> Log.Logger { Console } -> Result Log.Receipt Log.Error / { Console }\nlet bad console logger = Log.infoLazy logger (build console)\n"),
    ]
    for code, body in cases:
        source = "module Negative\nimport logging.Log\nimport logging.LogBestEffort\nimport logging.LogQueue\nimport logging.LogSink\n// é\n" + body
        path = directory / "Negative.fai"
        path.write_bytes(source.encode("utf-8"))
        result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples", "--message-format=json", "-C", str(directory), str(path)], cwd=ROOT, capture_output=True, timeout=90)
        diagnostics = json.loads(result.stdout)["diagnostics"]
        matching = [d for d in diagnostics if d["code"] == code]
        assert result.returncode != 0 and matching, (code, diagnostics, result.stderr)
        assert all(0 <= d["primary"]["byteStart"] < d["primary"]["byteEnd"] <= len(source.encode("utf-8")) for d in matching), matching


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    compiler = str(Path(args.fai).resolve()) if Path(args.fai).exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("compiler not found")
    base = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="fai-logging-", dir=base) as temporary:
        root = Path(temporary)
        cases = [
            ("DirectChecks", b"direct logging checks passed\n"),
            ("QueueChecks", b"queued logging checks passed\n"),
            ("BackpressureChecks", b"logging backpressure checks passed\n"),
            ("CaptureChecks", b"logging capture checks passed\n"),
            ("FileQueueChecks", b"queued file logging checks passed\n"),
        ]
        for name, expected in cases:
            source = f"logging/examples/{name}.fai"
            assert run([compiler, "run", "--no-daemon", "-C", "packages", source]).stdout == expected, name
            binary = root / f"{name}.exe"
            run([compiler, "build", "--no-daemon", "-C", "packages", source, "--out", str(binary)])
            result = run([str(binary)])
            assert result.stdout == expected, (name, result.stdout, result.stderr)
        result = run([compiler, "run", "--no-daemon", "-C", "packages", "logging/examples/Example.fai"])
        assert result.stdout == b"", result.stdout
        record = json.loads(result.stderr)
        assert record["fields"] == {"request_id": "request-1", "status": 200}, record
        assert record["level"] == "info" and record["timestamp"].endswith("Z"), record
        negative_checks(compiler, root / "negative")
    print("Logging JIT/AOT, effects, file integrity, capture and queue lifecycle passed")


if __name__ == "__main__":
    main()
