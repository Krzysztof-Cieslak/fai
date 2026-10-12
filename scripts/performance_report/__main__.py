"""Render exported report data without the compiler or benchmark executables."""

import argparse
import json
from pathlib import Path
import sys

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from performance_report import render


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--evidence-url", default="")
    args = parser.parse_args()
    try:
        reports = render(json.loads(args.data.read_text(encoding="utf-8")), args.evidence_url)
        args.output.mkdir(parents=True, exist_ok=True)
        for name, text in reports.items():
            (args.output / name).write_text(text, encoding="utf-8", newline="\n")
    except (ValueError, KeyError, TypeError, OSError) as error:
        parser.exit(1, f"performance-report: {error}\n")


if __name__ == "__main__":
    main()
