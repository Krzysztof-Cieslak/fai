#!/usr/bin/env python3
"""Compiler negative fixtures for typed UI attributes and pure widget callbacks."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai")
    args = parser.parse_args()
    candidate = Path(args.fai)
    compiler = str(candidate.resolve()) if candidate.exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("fai executable not found")
    source_root = Path(__file__).resolve().parents[2] / "packages" / "tui" / "src"
    with tempfile.TemporaryDirectory(prefix="fai-tui-types-") as temporary:
        root = Path(temporary)
        shutil.copy2(source_root / "Ui.fai", root)
        shutil.copy2(source_root / "TuiStyle.fai", root)

        def check(source, expected):
            (root / "Main.fai").write_text(source, encoding="utf-8")
            result = subprocess.run([compiler, "check", "--no-daemon", "--no-examples",
                                     "--message-format=json", "-C", str(root), "Main.fai"],
                                    capture_output=True, text=True, encoding="utf-8", timeout=60)
            payload = json.loads(result.stdout)
            if expected is None:
                assert result.returncode == 0 and payload["ok"], payload
            else:
                assert result.returncode != 0 and not payload["ok"], payload
                errors = [error for error in payload["diagnostics"] if error["code"] == expected]
                assert errors, payload
                assert all(error["primary"]["start"]["line"] == 3 for error in errors), errors
                assert all(0 <= error["primary"]["byteStart"] < error["primary"]["byteEnd"] <= len(source.encode("utf-8")) for error in errors), errors

        check('module Main\n// é\nlet view = Ui.column [Ui.padding 1] [Ui.text [] "hello"]\n', None)
        check('module Main\n// é\nlet view = Ui.column [Ui.placeholder "wrong kind"] []\n', "FAI3001")
        check('module Main\nview : Console -> Ui.Element Unit\nlet view console = Ui.input (Ui.id "name") [] { label = "Name", value = "", onChange = fun text -> console.writeLine text }\n', "FAI3001")
    print("TUI attribute and callback compiler checks passed")


if __name__ == "__main__":
    main()
