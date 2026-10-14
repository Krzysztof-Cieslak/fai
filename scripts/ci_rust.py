#!/usr/bin/env python3
"""Report usable Rust cache artifacts separately from cache-key hits."""

import argparse
import json
from pathlib import Path

import ci


def inventory(target):
    """Count compiled Rust libraries, not empty directories or metadata markers."""
    count = size = 0
    for artifact in target.rglob("*"):
        if artifact.parent.name != "deps" or artifact.suffix not in {".rlib", ".rmeta"}:
            continue
        if artifact.is_symlink():
            continue
        try:
            metadata = artifact.stat()
        except FileNotFoundError:
            continue
        if artifact.is_file() and metadata.st_size:
            count += 1
            size += metadata.st_size
    return {"artifacts": count, "bytes": size}


def report(target, stage, hit):
    observed = inventory(target)
    hit = hit == "true" if stage == "restored" else None
    print(json.dumps({"stage": stage, "cacheHit": hit, **observed}, sort_keys=True))
    cache_status = f"Exact cache hit: **{'yes' if hit else 'no'}**\n\n" if hit is not None else ""
    ci.summary(f"### Rust artifacts: {stage}\n\n"
               f"{cache_status}"
               f"Compiled libraries: **{observed['artifacts']:,}** "
               f"(**{observed['bytes'] / 1024 / 1024:.1f} MiB**). "
               "Counts include rlib/rmeta artifacts, not empty cache markers.")
    if stage == "restored" and hit and not observed["artifacts"]:
        print("::warning::Rust cache key matched but contained no compiled libraries; Cargo will rebuild them.")
    return observed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    cache = sub.add_parser("cache")
    cache.add_argument("--target", type=Path, default=Path("target"))
    cache.add_argument("--stage", choices=("restored", "built"), required=True)
    cache.add_argument("--hit", choices=("true", "false", ""), default="false")
    args = parser.parse_args()
    report(args.target, args.stage, args.hit)


if __name__ == "__main__":
    main()
