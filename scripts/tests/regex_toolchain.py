#!/usr/bin/env python3
"""Complement native regex suites with relocated AOT and compiler-negative checks."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from package_fixture import prepare

ROOT = Path(__file__).resolve().parents[2]


def run(command, environment=None):
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True, timeout=180)
    assert result.returncode == 0, (command, result.stdout, result.stderr)
    return result.stdout


def negative_checks(compiler, root):
    prepare(root, ["regex"])
    cases = [
        ("FAI3001", "public bad : Regex.Regex -> Bytes -> Result Bool RegexTypes.Error\nlet bad regex bytes = RegexBytes.isMatch regex bytes\n"),
        ("FAI2018", "let bad = RegexEngine.ProgramCell 0\n"),
        ("FAI5001", "step : Console -> Regex.Captures -> Result String String / { Console }\nlet step console captures =\n  let logged = console.writeLine \"called\"\n  Ok \"x\"\npublic bad : Console -> Regex.Regex -> Result String (RegexTypes.ReplaceError String)\nlet bad console regex = Regex.replaceWith regex (step console) \"abc\"\n"),
        ("FAI5001", "step : Console -> Int -> Regex.Match -> Result (Int * RegexTypes.Decision) String / { Console }\nlet step console count found =\n  let logged = console.writeLine \"called\"\n  Ok (count + 1, RegexTypes.Continue)\npublic bad : Console -> Regex.Regex -> RegexTypes.FoldResult Int String\nlet bad console regex = Regex.fold regex \"abc\" 0 (step console)\n"),
    ]
    for expected, body in cases:
        source = "module Negative\nimport regex.Regex\nimport regex.RegexBytes\nimport regex.RegexTypes\n// é\n" + body
        file = root / "Negative.fai"
        file.write_bytes(source.encode("utf-8"))
        result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples", "--message-format=json", "-C", str(root), str(file)], cwd=ROOT, capture_output=True, timeout=90)
        diagnostics = json.loads(result.stdout)["diagnostics"]
        matching = [d for d in diagnostics if d["code"] == expected]
        assert result.returncode != 0 and matching, (expected, diagnostics, result.stderr)
        assert all(0 <= d["primary"]["byteStart"] < d["primary"]["byteEnd"] <= len(source.encode("utf-8")) for d in matching), matching


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    compiler = str(Path(args.fai).resolve()) if Path(args.fai).exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("compiler not found")
    temporary_base = Path("/tmp/opencode") if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="fai-regex-", dir=temporary_base) as temporary:
        root = Path(temporary)
        relocated = root / Path(compiler).name
        shutil.copy2(compiler, relocated)
        environment = {**os.environ, "CARGO_HOME": str(root / "cargo"), "RUSTUP_HOME": str(root / "rustup")}
        cases = [
            ("NativeChecks", b"regex native checks passed\n"),
            ("CallbackChecks", b"visit:a\nvisit:b\nregex callback checks passed\n"),
            ("StressChecks", b"regex stress checks passed\n"),
            ("UnicodeScan", "héllo, 世界\n".encode()),
        ]
        for name, expected in cases:
            source = f"regex/examples/{name}.fai"
            binary = root / f"{name}.exe"
            run([str(relocated), "build", "--no-daemon", "-C", "packages", source, "--out", str(binary)], environment)
            output = run([str(binary)], environment)
            assert output == expected, (name, output)
        negatives = root / "negative"
        negatives.mkdir()
        negative_checks(compiler, negatives)
    print("Regex relocated AOT and compiler type/effect checks passed")


if __name__ == "__main__":
    main()
