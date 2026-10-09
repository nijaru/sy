#!/usr/bin/env python3
"""Profile an explicit sy artifact against freshly reset, hash-checked trees.

Build an optimized artifact first. For symbolized captures, retain debug info:
  CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --locked --release --bin sy
  python3 scripts/profile.py --sy-binary target/release/sy --scenario large_file \
      --phase changed --recorder samply

--root selects the fixture filesystem (for example Btrfs versus tmpfs).
Samples use warm filesystem caches; this is not a cold-storage benchmark.
"""

import argparse
import json
import os
import platform
import random
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

from benchmark import (
    QUICK_SCENARIOS,
    SCENARIOS,
    generate_test_data,
    get_system_info,
    get_version_info,
    modify_files,
    resolve_sy_binary,
    sha256_file,
    tree_manifest,
)


def checked(command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def randomize_payloads(source):
    """Deterministic, distinct blocks without a file-sized allocation."""
    rng = random.Random(0)
    for path in sorted(source.rglob("*")):
        if not path.is_file():
            continue
        remaining = path.stat().st_size
        with path.open("wb") as file:
            while remaining:
                size = min(remaining, 1024 * 1024)
                file.write(rng.randbytes(size))
                remaining -= size


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sy-binary", type=Path, required=True)
    parser.add_argument("--scenario", choices=sorted(SCENARIOS), default="mixed")
    parser.add_argument("--quick", action="store_true", help="small fixture for harness checks")
    parser.add_argument("--phase", choices=["new", "unchanged", "changed", "scan"], default="changed")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--root", type=Path, help="existing directory on the fixture filesystem")
    parser.add_argument("--output", type=Path, help="new directory for results and optional capture")
    parser.add_argument("--recorder", choices=["none", "perf", "samply"], default="none")
    parser.add_argument("--entropy", choices=["random", "repetitive"], default="random")
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    if args.root is not None and not args.root.is_dir():
        parser.error("--root must be an existing directory")
    if args.recorder == "perf" and platform.system() != "Linux":
        parser.error("perf captures require Linux")
    if args.recorder != "none" and shutil.which(args.recorder) is None:
        parser.error(f"{args.recorder} is not installed")
    sy = resolve_sy_binary(args.sy_binary)
    versions = get_version_info(sy)
    artifact = {**versions["sy_binary"], "version": versions["sy"]}
    output = args.output or Path("target/profiling") / time.strftime("%Y%m%d-%H%M%S")
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    config = next(iter(QUICK_SCENARIOS.values())) if args.quick else SCENARIOS[args.scenario]
    timings = []

    with tempfile.TemporaryDirectory(prefix="sy-profile-", dir=args.root) as fixture:
        fixture = Path(fixture)
        source, basis, destination = (fixture / name for name in ["source", "basis", "destination"])
        source.mkdir()
        count, payload_bytes = generate_test_data(source, config)
        if args.entropy == "random":
            randomize_payloads(source)
        common = [str(sy), str(source) + os.sep, str(destination), "--preserve-times", "--quiet"]
        # Baseline creation, mutation, reset, and validation are all untimed.
        if args.phase in {"unchanged", "changed"}:
            checked([str(sy), str(source) + os.sep, str(basis), "--preserve-times", "--quiet"])
            if tree_manifest(str(basis)) != tree_manifest(str(source)):
                raise RuntimeError("baseline payload/tree mismatch")
        modified = modify_files(source) if args.phase == "changed" else 0
        expected = tree_manifest(str(source))
        command = common + (["--dry-run"] if args.phase == "scan" else [])

        def reset():
            if destination.exists():
                shutil.rmtree(destination)
            if args.phase in {"unchanged", "changed"}:
                # Portable ordinary copy, not a reflink reset. This seeds an
                # existing destination; sy's own COW capability is still native.
                shutil.copytree(basis, destination)

        def validate():
            if args.phase == "scan":
                if destination.exists():
                    raise RuntimeError("dry-run created a destination")
            elif tree_manifest(str(destination)) != expected:
                raise RuntimeError("sample payload/tree mismatch")
            if tree_manifest(str(source)) != expected:
                raise RuntimeError("sample mutated its source")
            if sha256_file(sy) != artifact["sha256"]:
                raise RuntimeError("artifact changed during profiling")

        reset()
        checked(command, stdout=subprocess.DEVNULL)
        validate()
        time_flag = "-l" if platform.system() == "Darwin" else "-v"
        for sample in range(args.samples):
            reset()
            # Preserve raw per-process resource diagnostics instead of treating
            # cumulative RUSAGE_CHILDREN maxima as this sample's peak RSS.
            with (output / f"resources-{sample}.txt").open("wb") as diagnostics:
                started = time.perf_counter()
                checked(["/usr/bin/time", time_flag, *command], stdout=subprocess.DEVNULL, stderr=diagnostics)
                timings.append(time.perf_counter() - started)
            validate()

        if args.recorder != "none":
            reset()  # The capture must exercise the same phase, not a later skip.
            if args.recorder == "perf":
                capture = ["perf", "record", "--call-graph", "dwarf", "-o", str(output / "perf.data"), "--"]
            else:
                capture = ["samply", "record", "--save-only", "--output", str(output / "samply.json"), "--"]
            checked(capture + command)
            validate()
        report = {
            "artifact": artifact,
            "system": get_system_info(),
            "fixture_parent": str(fixture.parent),
            "scenario": "quick" if args.quick else args.scenario,
            "config": config,
            "phase": args.phase,
            "entropy": args.entropy,
            "files": count,
            "logical_bytes": payload_bytes,
            "modified_files": modified,
            "command": command,
            "warmup_runs": 1,
            "seconds": timings,
            "median_seconds": statistics.median(timings),
            "reset": "portable ordinary copy; setup and hash checks excluded from timing",
            "cache": "warm; no page-cache eviction",
            "timing_scope": "sync process plus /usr/bin/time launch; reset and validation excluded",
            "recorder": args.recorder,
        }
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(output / "result.json")


if __name__ == "__main__":
    main()
