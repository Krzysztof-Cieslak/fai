#!/usr/bin/env python3
"""Run the self-contained JSON package against an existing Fai compiler."""

import argparse
from decimal import Decimal
import json
import math
from pathlib import Path
import random
import shutil
import struct
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fai", default="fai", help="existing compiler executable")
    args = parser.parse_args()
    candidate = Path(args.fai)
    if not candidate.exists() and candidate.with_suffix(".exe").exists():
        candidate = candidate.with_suffix(".exe")
    compiler = str(candidate.resolve()) if candidate.exists() else shutil.which(args.fai)
    if compiler is None:
        parser.error("fai was not found; pass --fai /path/to/fai")
    root = Path(__file__).resolve().parents[1]

    def fai(*arguments, **kwargs):
        return subprocess.run(
            [compiler, *arguments, "--no-daemon", "-C", str(root)],
            check=True, text=True, encoding="utf-8", timeout=600, **kwargs,
        )

    fai("fmt", "--check")
    fai("check", "--no-examples")
    fai("test", "--seed", "42", "--count", "128")
    example = fai("run", "examples/JsonExample.fai", capture_output=True)
    assert json.loads(example.stdout) == {
        "message": "Hello, JSON!", "large": 123456789012345678901234567890,
    }, example.stdout

    with tempfile.TemporaryDirectory(prefix="fai-json-") as temp:
        executable = Path(temp) / "oracle"
        fai("build", "test/Oracle.fai", "--out", str(executable))
        if executable.with_suffix(".exe").exists():
            executable = executable.with_suffix(".exe")

        def run(mode, lines):
            result = subprocess.run(
                [str(executable), mode], input="\n".join(lines) + "\n",
                check=True, capture_output=True, text=True, encoding="utf-8", timeout=300,
            )
            output = result.stdout.removesuffix("\n").split("\n")
            assert len(output) == len(lines), result
            return output

        rng = random.Random(42)
        numbers = [
            "0.1", "-0", "1e99999", "-1e-99999", "1.7976931348623158e308",
            "1.7976931348623159e308", "2.4703282292062327e-324",
            "2.4703282292062328e-324",
            "1.00000000000000011102230246251565404236316680908203125",
            "1.000000000000000111022302462515654042363166809082031250001",
        ]
        numbers += [f"{rng.randrange(-(1 << 63), 1 << 63)}e{rng.randrange(-350, 350)}" for _ in range(128)]
        for _ in range(128):
            value = struct.unpack(">d", rng.getrandbits(64).to_bytes(8, "big"))[0]
            if math.isfinite(value):
                numbers.append(repr(value))
        for source, actual in zip(numbers, run("float", numbers)):
            value = float(source)
            expected = str(struct.unpack(">q", struct.pack(">d", value))[0]) if math.isfinite(value) else "error"
            assert actual == expected, (source, actual, expected)

        alphabet = ["a", "é", "😀", "\x00", "\n", '"', "\\", "\u2028"]

        def document(depth):
            choice = rng.randrange(6 if depth else 4)
            if choice == 0:
                return None
            if choice == 1:
                return bool(rng.randrange(2))
            if choice == 2:
                return rng.randrange(-(1 << 63), 1 << 63)
            if choice == 3:
                return "".join(rng.choice(alphabet) for _ in range(rng.randrange(16)))
            if choice == 4:
                return [document(depth - 1) for _ in range(rng.randrange(6))]
            return {str(i): document(depth - 1) for i in range(rng.randrange(6))}

        documents = [json.dumps(document(5), ensure_ascii=False) for _ in range(128)]
        documents += ['{"a":1,"a":2}', '[1e9999,-0.000e-99,18446744073709551616]']

        def exact(text):
            return json.loads(text, parse_int=Decimal, parse_float=Decimal, object_pairs_hook=tuple)

        for source, actual in zip(documents, run("json", documents)):
            assert exact(source) == exact(actual), (source, actual)
    print("JSON package: contracts, JIT, AOT, and seeded conformance checks passed")


if __name__ == "__main__":
    main()
