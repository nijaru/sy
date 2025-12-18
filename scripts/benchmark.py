#!/usr/bin/env python3
"""
sy vs rsync benchmark runner with JSONL history tracking.

Usage:
    python scripts/benchmark.py                    # Run all benchmarks
    python scripts/benchmark.py --quick            # Quick smoke test
    python scripts/benchmark.py --ssh user@host    # Test over SSH
    python scripts/benchmark.py --history          # Show recent results
    python scripts/benchmark.py --compare          # Compare last 2 runs
"""

import argparse
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional


# ============================================================================
# Configuration
# ============================================================================

HISTORY_FILE = Path(__file__).parent.parent / "benchmarks" / "history.jsonl"

# Test scenarios
SCENARIOS = {
    "small_files": {"files": 1000, "size_kb": 1, "dirs": 10},
    "large_file": {"files": 1, "size_kb": 100_000, "dirs": 0},  # 100MB
    "mixed": {
        "files": 500,
        "size_kb": 10,
        "dirs": 50,
        "large_files": 5,
        "large_size_kb": 10_000,
    },
    "deep_dirs": {"files": 100, "size_kb": 1, "dirs": 100, "depth": 10},
    "source_code": {"files": 5000, "size_kb": 5, "dirs": 200},  # Simulates codebase
}

QUICK_SCENARIOS = {"small_files": {"files": 100, "size_kb": 1, "dirs": 5}}


@dataclass
class BenchmarkResult:
    """Result from a single benchmark run."""

    scenario: str
    tool: str
    operation: str  # initial, incremental, delta
    duration_ms: float
    files_count: int
    bytes_total: int
    throughput_mbps: float = 0.0
    files_per_sec: float = 0.0
    error: Optional[str] = None

    def __post_init__(self):
        if self.duration_ms > 0:
            self.throughput_mbps = (self.bytes_total / 1_000_000) / (
                self.duration_ms / 1000
            )
            self.files_per_sec = self.files_count / (self.duration_ms / 1000)


@dataclass
class BenchmarkRun:
    """A complete benchmark run with all scenarios."""

    timestamp: str
    system: dict
    git: dict
    version: dict
    transport: str  # local, ssh, ssh-simulated
    results: list[BenchmarkResult] = field(default_factory=list)
    notes: str = ""


# ============================================================================
# System Information
# ============================================================================


def get_system_info() -> dict:
    """Collect system information for reproducibility."""
    info = {
        "os": platform.system(),
        "os_version": platform.release(),
        "arch": platform.machine(),
        "host": socket.gethostname()[:16],
        "python": platform.python_version(),
    }

    # CPU info
    if platform.system() == "Darwin":
        try:
            result = subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                capture_output=True,
                text=True,
            )
            info["cpu"] = result.stdout.strip()
        except Exception:
            info["cpu"] = "unknown"
    elif platform.system() == "Linux":
        try:
            with open("/proc/cpuinfo") as f:
                for line in f:
                    if "model name" in line:
                        info["cpu"] = line.split(":")[1].strip()
                        break
        except Exception:
            info["cpu"] = "unknown"

    info["cores"] = os.cpu_count() or 0

    return info


def get_git_info() -> dict:
    """Get current git commit info."""
    try:
        commit = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            capture_output=True,
            text=True,
            cwd=Path(__file__).parent.parent,
        ).stdout.strip()

        branch = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            cwd=Path(__file__).parent.parent,
        ).stdout.strip()

        dirty = (
            subprocess.run(
                ["git", "status", "--porcelain"],
                capture_output=True,
                text=True,
                cwd=Path(__file__).parent.parent,
            ).stdout.strip()
            != ""
        )

        return {"commit": commit, "branch": branch, "dirty": dirty}
    except Exception:
        return {"commit": "unknown", "branch": "unknown", "dirty": True}


def get_version_info() -> dict:
    """Get tool versions."""
    info = {}

    # sy version
    try:
        result = subprocess.run(["sy", "--version"], capture_output=True, text=True)
        info["sy"] = result.stdout.strip().replace("sy ", "")
    except Exception:
        # Try cargo build version
        try:
            result = subprocess.run(
                ["cargo", "run", "--release", "--", "--version"],
                capture_output=True,
                text=True,
                cwd=Path(__file__).parent.parent,
            )
            info["sy"] = result.stdout.strip().replace("sy ", "")
        except Exception:
            info["sy"] = "unknown"

    # rsync version
    try:
        result = subprocess.run(["rsync", "--version"], capture_output=True, text=True)
        first_line = result.stdout.split("\n")[0]
        info["rsync"] = (
            first_line.split()[2] if len(first_line.split()) > 2 else "unknown"
        )
    except Exception:
        info["rsync"] = "not installed"

    return info


# ============================================================================
# Test Data Generation
# ============================================================================


def generate_test_data(base_dir: Path, config: dict) -> tuple[int, int]:
    """
    Generate test files and directories.
    Returns (file_count, total_bytes).
    """
    files_count = config.get("files", 100)
    size_kb = config.get("size_kb", 1)
    dirs_count = config.get("dirs", 10)
    depth = config.get("depth", 3)
    large_files = config.get("large_files", 0)
    large_size_kb = config.get("large_size_kb", 10_000)

    total_bytes = 0
    actual_files = 0

    # Create directory structure
    directories = []
    for i in range(dirs_count):
        if depth > 1:
            # Create nested directories
            parts = [f"d{j}" for j in range(i % depth + 1)]
            parts.append(f"dir_{i}")
            dir_path = base_dir / "/".join(parts)
        else:
            dir_path = base_dir / f"dir_{i}"
        dir_path.mkdir(parents=True, exist_ok=True)
        directories.append(dir_path)

    if not directories:
        directories = [base_dir]

    # Create regular files
    content_block = b"x" * 1024  # 1KB block
    for i in range(files_count):
        dir_idx = i % len(directories)
        file_path = directories[dir_idx] / f"file_{i}.txt"
        content = content_block * size_kb
        file_path.write_bytes(content)
        total_bytes += len(content)
        actual_files += 1

    # Create large files if specified
    large_content = b"L" * 1024 * large_size_kb
    for i in range(large_files):
        file_path = base_dir / f"large_{i}.bin"
        file_path.write_bytes(large_content)
        total_bytes += len(large_content)
        actual_files += 1

    return actual_files, total_bytes


def modify_files(base_dir: Path, percent: float = 10) -> int:
    """
    Modify a percentage of files for incremental/delta testing.
    Returns count of modified files.
    """
    all_files = list(base_dir.rglob("*.txt")) + list(base_dir.rglob("*.bin"))
    modify_count = max(1, int(len(all_files) * percent / 100))

    for i, file_path in enumerate(all_files[:modify_count]):
        content = file_path.read_bytes()
        # Modify middle of file (triggers delta sync)
        mid = len(content) // 2
        modified = content[:mid] + b"MODIFIED" + content[mid + 8 :]
        file_path.write_bytes(modified)

    return modify_count


# ============================================================================
# Benchmark Execution
# ============================================================================


def run_sy(
    source: str, dest: str, extra_args: list[str] = None
) -> tuple[float, bool, str]:
    """
    Run sy and return (duration_ms, success, error_msg).
    """
    args = ["sy", source, dest]
    if extra_args:
        args.extend(extra_args)

    start = time.perf_counter()
    result = subprocess.run(args, capture_output=True, text=True)
    duration_ms = (time.perf_counter() - start) * 1000

    if result.returncode != 0:
        return duration_ms, False, result.stderr[:200]

    return duration_ms, True, ""


def run_rsync(
    source: str, dest: str, extra_args: list[str] = None
) -> tuple[float, bool, str]:
    """
    Run rsync and return (duration_ms, success, error_msg).
    """
    args = ["rsync", "-a", f"{source}/", dest]
    if extra_args:
        args.extend(extra_args)

    start = time.perf_counter()
    result = subprocess.run(args, capture_output=True, text=True)
    duration_ms = (time.perf_counter() - start) * 1000

    if result.returncode != 0:
        return duration_ms, False, result.stderr[:200]

    return duration_ms, True, ""


def benchmark_scenario(
    scenario_name: str,
    config: dict,
    transport: str = "local",
    ssh_target: str = None,
    iterations: int = 3,
) -> list[BenchmarkResult]:
    """
    Run a complete benchmark scenario (initial + incremental + delta).
    """
    results = []

    with tempfile.TemporaryDirectory() as tmpdir:
        source_dir = Path(tmpdir) / "source"
        source_dir.mkdir()

        # Generate test data
        files_count, bytes_total = generate_test_data(source_dir, config)
        print(f"  Generated {files_count} files ({bytes_total / 1_000_000:.1f} MB)")

        # Determine source/dest paths based on transport
        if transport == "ssh" and ssh_target:
            # For SSH: sync to remote
            remote_base = f"/tmp/sy_bench_{os.getpid()}"
            source_path = str(source_dir)
            sy_dest = f"{ssh_target}:{remote_base}/sy"
            rsync_dest = f"{ssh_target}:{remote_base}/rsync"

            # Clean remote dirs
            subprocess.run(
                ["ssh", ssh_target, f"rm -rf {remote_base}"], capture_output=True
            )
            subprocess.run(
                ["ssh", ssh_target, f"mkdir -p {remote_base}"], capture_output=True
            )
        else:
            source_path = str(source_dir)
            sy_dest = str(Path(tmpdir) / "dest_sy")
            rsync_dest = str(Path(tmpdir) / "dest_rsync")

        # =========== INITIAL SYNC ===========
        print("  Testing initial sync...")

        # sy initial
        durations = []
        for i in range(iterations):
            if transport != "ssh":
                # Clear dest for each iteration
                shutil.rmtree(sy_dest, ignore_errors=True)
            else:
                subprocess.run(
                    ["ssh", ssh_target, f"rm -rf {remote_base}/sy"], capture_output=True
                )

            duration, success, error = run_sy(source_path, sy_dest)
            if success:
                durations.append(duration)
            elif i == 0:  # Only record error on first try
                results.append(
                    BenchmarkResult(
                        scenario=scenario_name,
                        tool="sy",
                        operation="initial",
                        duration_ms=duration,
                        files_count=files_count,
                        bytes_total=bytes_total,
                        error=error,
                    )
                )
                break

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="sy",
                    operation="initial",
                    duration_ms=median_duration,
                    files_count=files_count,
                    bytes_total=bytes_total,
                )
            )

        # rsync initial
        durations = []
        for i in range(iterations):
            if transport != "ssh":
                shutil.rmtree(rsync_dest, ignore_errors=True)
            else:
                subprocess.run(
                    ["ssh", ssh_target, f"rm -rf {remote_base}/rsync"],
                    capture_output=True,
                )

            duration, success, error = run_rsync(source_path, rsync_dest)
            if success:
                durations.append(duration)
            elif i == 0:
                results.append(
                    BenchmarkResult(
                        scenario=scenario_name,
                        tool="rsync",
                        operation="initial",
                        duration_ms=duration,
                        files_count=files_count,
                        bytes_total=bytes_total,
                        error=error,
                    )
                )
                break

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="rsync",
                    operation="initial",
                    duration_ms=median_duration,
                    files_count=files_count,
                    bytes_total=bytes_total,
                )
            )

        # =========== INCREMENTAL SYNC (no changes) ===========
        print("  Testing incremental sync (no changes)...")

        # sy incremental
        durations = []
        for _ in range(iterations):
            duration, success, _ = run_sy(source_path, sy_dest)
            if success:
                durations.append(duration)

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="sy",
                    operation="incremental",
                    duration_ms=median_duration,
                    files_count=files_count,
                    bytes_total=0,  # No bytes transferred
                )
            )

        # rsync incremental
        durations = []
        for _ in range(iterations):
            duration, success, _ = run_rsync(source_path, rsync_dest)
            if success:
                durations.append(duration)

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="rsync",
                    operation="incremental",
                    duration_ms=median_duration,
                    files_count=files_count,
                    bytes_total=0,
                )
            )

        # =========== DELTA SYNC (10% modified) ===========
        print("  Testing delta sync (10% modified)...")

        modified_count = modify_files(source_dir, percent=10)
        modified_bytes = modified_count * config.get("size_kb", 1) * 1024

        # sy delta
        durations = []
        for _ in range(iterations):
            duration, success, _ = run_sy(source_path, sy_dest)
            if success:
                durations.append(duration)

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="sy",
                    operation="delta",
                    duration_ms=median_duration,
                    files_count=modified_count,
                    bytes_total=modified_bytes,
                )
            )

        # rsync delta
        durations = []
        for _ in range(iterations):
            duration, success, _ = run_rsync(source_path, rsync_dest)
            if success:
                durations.append(duration)

        if durations:
            median_duration = sorted(durations)[len(durations) // 2]
            results.append(
                BenchmarkResult(
                    scenario=scenario_name,
                    tool="rsync",
                    operation="delta",
                    duration_ms=median_duration,
                    files_count=modified_count,
                    bytes_total=modified_bytes,
                )
            )

        # Cleanup SSH remote
        if transport == "ssh" and ssh_target:
            subprocess.run(
                ["ssh", ssh_target, f"rm -rf {remote_base}"], capture_output=True
            )

    return results


# ============================================================================
# History & Reporting
# ============================================================================


def save_run(run: BenchmarkRun):
    """Save benchmark run to JSONL history file."""
    HISTORY_FILE.parent.mkdir(parents=True, exist_ok=True)

    run_dict = {
        "ts": run.timestamp,
        "sys": run.system,
        "git": run.git,
        "ver": run.version,
        "transport": run.transport,
        "results": [
            {
                "scenario": r.scenario,
                "tool": r.tool,
                "op": r.operation,
                "ms": round(r.duration_ms, 1),
                "files": r.files_count,
                "bytes": r.bytes_total,
                "mbps": round(r.throughput_mbps, 2),
                "fps": round(r.files_per_sec, 1),
                "err": r.error,
            }
            for r in run.results
        ],
    }
    if run.notes:
        run_dict["notes"] = run.notes

    with open(HISTORY_FILE, "a") as f:
        f.write(json.dumps(run_dict) + "\n")

    print(f"\nResults saved to {HISTORY_FILE}")


def load_history(limit: int = 10) -> list[dict]:
    """Load recent benchmark history."""
    if not HISTORY_FILE.exists():
        return []

    runs = []
    with open(HISTORY_FILE) as f:
        for line in f:
            if line.strip():
                runs.append(json.loads(line))

    return runs[-limit:]


def show_history(limit: int = 10):
    """Display recent benchmark history."""
    runs = load_history(limit)

    if not runs:
        print("No benchmark history found.")
        return

    print(f"\n{'=' * 80}")
    print("Recent Benchmark History")
    print(f"{'=' * 80}\n")

    for run in runs:
        print(
            f"Date: {run['ts'][:19]} | Commit: {run['git']['commit']} | Transport: {run['transport']}"
        )
        print(
            f"System: {run['sys'].get('cpu', 'unknown')[:30]} ({run['sys']['cores']} cores)"
        )
        print()

        # Group by scenario
        by_scenario = {}
        for r in run["results"]:
            key = (r["scenario"], r["op"])
            if key not in by_scenario:
                by_scenario[key] = {}
            by_scenario[key][r["tool"]] = r

        print(
            f"{'Scenario':<15} {'Operation':<12} {'sy (ms)':<12} {'rsync (ms)':<12} {'Speedup':<10}"
        )
        print("-" * 65)

        for (scenario, op), tools in sorted(by_scenario.items()):
            sy_ms = tools.get("sy", {}).get("ms", 0)
            rsync_ms = tools.get("rsync", {}).get("ms", 0)

            if sy_ms and rsync_ms:
                speedup = rsync_ms / sy_ms
                speedup_str = (
                    f"{speedup:.2f}x" if speedup >= 1 else f"{1 / speedup:.2f}x slower"
                )
            else:
                speedup_str = "N/A"

            print(
                f"{scenario:<15} {op:<12} {sy_ms:<12.1f} {rsync_ms:<12.1f} {speedup_str:<10}"
            )

        print()


def compare_runs(run1: dict, run2: dict):
    """Compare two benchmark runs."""
    print(f"\nComparing: {run1['git']['commit']} -> {run2['git']['commit']}")
    print(f"  Before: {run1['ts'][:19]} ({run1['transport']})")
    print(f"  After:  {run2['ts'][:19]} ({run2['transport']})")
    print()

    # Build lookup for run1
    run1_lookup = {}
    for r in run1["results"]:
        key = (r["scenario"], r["op"], r["tool"])
        run1_lookup[key] = r

    print(
        f"{'Scenario':<15} {'Op':<10} {'Tool':<8} {'Before':<10} {'After':<10} {'Change':<10}"
    )
    print("-" * 70)

    for r in run2["results"]:
        key = (r["scenario"], r["op"], r["tool"])
        if key in run1_lookup:
            before = run1_lookup[key]["ms"]
            after = r["ms"]
            if before > 0:
                change = ((after / before) - 1) * 100
                change_str = f"{change:+.1f}%"
                if change < -5:
                    change_str = f"{change_str} (better)"
                elif change > 5:
                    change_str = f"{change_str} (worse)"
            else:
                change_str = "N/A"

            print(
                f"{r['scenario']:<15} {r['op']:<10} {r['tool']:<8} {before:<10.1f} {after:<10.1f} {change_str:<10}"
            )


def print_results(results: list[BenchmarkResult]):
    """Print benchmark results table."""
    print(f"\n{'=' * 80}")
    print("Benchmark Results")
    print(f"{'=' * 80}\n")

    # Group by scenario and operation
    by_scenario = {}
    for r in results:
        key = (r.scenario, r.operation)
        if key not in by_scenario:
            by_scenario[key] = {}
        by_scenario[key][r.tool] = r

    print(
        f"{'Scenario':<15} {'Operation':<12} {'Tool':<8} {'Time (ms)':<12} {'MB/s':<10} {'Files/s':<10}"
    )
    print("-" * 75)

    for (scenario, op), tools in sorted(by_scenario.items()):
        for tool_name in ["sy", "rsync"]:
            if tool_name in tools:
                r = tools[tool_name]
                if r.error:
                    print(
                        f"{scenario:<15} {op:<12} {tool_name:<8} ERROR: {r.error[:30]}"
                    )
                else:
                    print(
                        f"{scenario:<15} {op:<12} {tool_name:<8} {r.duration_ms:<12.1f} {r.throughput_mbps:<10.1f} {r.files_per_sec:<10.1f}"
                    )

    # Summary comparison
    print(f"\n{'=' * 80}")
    print("Summary: sy vs rsync")
    print(f"{'=' * 80}\n")

    for (scenario, op), tools in sorted(by_scenario.items()):
        sy_r = tools.get("sy")
        rsync_r = tools.get("rsync")

        if sy_r and rsync_r and not sy_r.error and not rsync_r.error:
            if sy_r.duration_ms > 0:
                speedup = rsync_r.duration_ms / sy_r.duration_ms
                if speedup >= 1:
                    print(f"{scenario}/{op}: sy is {speedup:.2f}x FASTER")
                else:
                    print(f"{scenario}/{op}: sy is {1 / speedup:.2f}x SLOWER")


# ============================================================================
# Main
# ============================================================================


def main():
    parser = argparse.ArgumentParser(description="sy vs rsync benchmark runner")
    parser.add_argument("--quick", action="store_true", help="Run quick smoke test")
    parser.add_argument(
        "--ssh", type=str, help="SSH target (user@host) for remote testing"
    )
    parser.add_argument("--iterations", type=int, default=3, help="Iterations per test")
    parser.add_argument("--history", action="store_true", help="Show benchmark history")
    parser.add_argument("--compare", action="store_true", help="Compare last 2 runs")
    parser.add_argument("--notes", type=str, default="", help="Notes for this run")
    parser.add_argument("--scenario", type=str, help="Run specific scenario only")
    args = parser.parse_args()

    # History commands
    if args.history:
        show_history()
        return

    if args.compare:
        runs = load_history(2)
        if len(runs) < 2:
            print("Need at least 2 runs to compare")
            return
        compare_runs(runs[0], runs[1])
        return

    # Check tools available
    if shutil.which("sy") is None:
        print("Error: 'sy' not found in PATH. Build with: cargo build --release")
        print("Then add to PATH or run: cargo install --path .")
        sys.exit(1)

    if shutil.which("rsync") is None:
        print("Warning: 'rsync' not found - will only benchmark sy")

    # Determine scenarios
    if args.quick:
        scenarios = QUICK_SCENARIOS
    elif args.scenario:
        if args.scenario not in SCENARIOS:
            print(f"Unknown scenario: {args.scenario}")
            print(f"Available: {', '.join(SCENARIOS.keys())}")
            sys.exit(1)
        scenarios = {args.scenario: SCENARIOS[args.scenario]}
    else:
        scenarios = SCENARIOS

    # Determine transport
    transport = "ssh" if args.ssh else "local"

    print(f"\n{'=' * 80}")
    print("sy vs rsync Benchmark")
    print(f"{'=' * 80}")
    print(f"Transport: {transport}")
    print(f"Scenarios: {', '.join(scenarios.keys())}")
    print(f"Iterations: {args.iterations}")
    if args.ssh:
        print(f"SSH Target: {args.ssh}")
    print()

    # Collect system info
    system_info = get_system_info()
    git_info = get_git_info()
    version_info = get_version_info()

    print(f"System: {system_info.get('cpu', 'unknown')[:40]}")
    print(f"Git: {git_info['commit']} ({git_info['branch']})")
    print(
        f"Versions: sy={version_info.get('sy', '?')}, rsync={version_info.get('rsync', '?')}"
    )
    print()

    # Run benchmarks
    all_results = []

    for scenario_name, config in scenarios.items():
        print(f"\n--- Scenario: {scenario_name} ---")
        results = benchmark_scenario(
            scenario_name,
            config,
            transport=transport,
            ssh_target=args.ssh,
            iterations=args.iterations,
        )
        all_results.extend(results)

    # Print results
    print_results(all_results)

    # Save to history
    run = BenchmarkRun(
        timestamp=datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S"),
        system=system_info,
        git=git_info,
        version=version_info,
        transport=transport,
        results=all_results,
        notes=args.notes,
    )
    save_run(run)


if __name__ == "__main__":
    main()
