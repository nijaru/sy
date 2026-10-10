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
import hashlib
import json
import os
import platform
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal

# ============================================================================
# Configuration
# ============================================================================

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SY_BINARY = REPO_ROOT / "target" / "release" / "sy"
HISTORY_FILE = REPO_ROOT / "benchmarks" / "history.jsonl"
BUFFER_SIZE = 1024 * 1024

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
class TransferEvidence:
    """Public evidence from the measured process, not filesystem inference."""

    status: Literal["zero_file_payload", "file_transfer", "proof_unavailable"]
    interface: str
    diagnostic: str = ""
    bytes_transferred: int | None = None  # sy's literal-file-byte counter only
    regular_file_transfers: int | None = None
    rsync_log_bytes: int | None = None  # %b may include protocol, NOT payload bytes


@dataclass
class BenchmarkResult:
    """Result from a single benchmark run."""

    scenario: str
    tool: str
    operation: str  # initial, incremental, delta
    duration_ms: float
    files_count: int
    # Logical changed-file bytes, not wire bytes; None means unproven unchanged.
    bytes_total: int | None
    throughput_mbps: float = 0.0
    files_per_sec: float = 0.0
    error: str | None = None
    error_kind: Literal["execution", "file_transfer", "proof_unavailable"] | None = None
    transfer_evidence: list[TransferEvidence] = field(default_factory=list)
    file_changes: list[dict] = field(default_factory=list)

    def __post_init__(self):
        if self.duration_ms > 0 and not self.error and self.bytes_total is not None:
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
                check=True,
            )
            info["cpu"] = result.stdout.strip()
        except (OSError, subprocess.CalledProcessError):
            info["cpu"] = "unknown"
    elif platform.system() == "Linux":
        try:
            with open("/proc/cpuinfo") as f:
                for line in f:
                    if "model name" in line:
                        info["cpu"] = line.split(":")[1].strip()
                        break
        except OSError:
            info["cpu"] = "unknown"

    info["cores"] = os.cpu_count() or 0

    return info


def get_git_info() -> dict:
    """Get current harness repository commit info, not artifact provenance."""
    try:
        commit = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            capture_output=True,
            text=True,
            cwd=REPO_ROOT,
            check=True,
        ).stdout.strip()

        branch = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            cwd=REPO_ROOT,
            check=True,
        ).stdout.strip()

        dirty = (
            subprocess.run(
                ["git", "status", "--porcelain"],
                capture_output=True,
                text=True,
                cwd=REPO_ROOT,
                check=True,
            ).stdout.strip()
            != ""
        )

        return {"commit": commit, "branch": branch, "dirty": dirty}
    except (OSError, subprocess.CalledProcessError):
        return {"commit": "unknown", "branch": "unknown", "dirty": True}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        while block := file.read(BUFFER_SIZE):
            digest.update(block)
    return digest.hexdigest()


def resolve_sy_binary(path: Path) -> Path:
    binary = path.expanduser().resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError(f"sy binary is not an executable file: {binary}")
    return binary


def get_version_info(sy_binary: Path) -> dict:
    """Record the tested artifact; repository HEAD is only harness provenance."""
    result = subprocess.run(
        [str(sy_binary), "--version"], capture_output=True, text=True, check=True
    )
    version = result.stdout.strip()
    if not version:
        raise ValueError(f"sy binary returned an empty version: {sy_binary}")
    info = {
        "sy": version.removeprefix("sy "),
        "sy_binary": {
            "path": str(sy_binary),
            "sha256": sha256_file(sy_binary),
            "version": version,
        },
    }

    rsync = shutil.which("rsync")
    if rsync:
        artifact = Path(rsync).resolve(strict=True)
        result = subprocess.run(
            [str(artifact), "--version"], capture_output=True, text=True, check=True
        )
        version = result.stdout.strip()
        if not version:
            raise ValueError(f"rsync binary returned an empty version: {artifact}")
        info["rsync"] = version.splitlines()[0]
        info["rsync_binary"] = {
            "path": str(artifact),
            "sha256": sha256_file(artifact),
            "version": version,
        }
    else:
        info["rsync"] = "unavailable"

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

    # Memory use is independent of individual file size.
    for i in range(files_count):
        dir_idx = i % len(directories)
        file_path = directories[dir_idx] / f"file_{i}.txt"
        write_repeated(file_path, b"x", size_kb * 1024)
        total_bytes += size_kb * 1024
        actual_files += 1

    for i in range(large_files):
        file_path = base_dir / f"large_{i}.bin"
        write_repeated(file_path, b"L", large_size_kb * 1024)
        total_bytes += large_size_kb * 1024
        actual_files += 1

    return actual_files, total_bytes


def write_repeated(path: Path, byte: bytes, size: int):
    block = byte * BUFFER_SIZE
    with path.open("wb") as file:
        remaining = size
        while remaining:
            count = min(remaining, len(block))
            file.write(block[:count])
            remaining -= count


def modify_files(base_dir: Path, percent: float = 10) -> int:
    """Patch a percentage of files without loading their contents into memory."""
    all_files = sorted(base_dir.rglob("*.txt")) + sorted(base_dir.rglob("*.bin"))
    modify_count = min(len(all_files), max(1, int(len(all_files) * percent / 100)))

    for file_path in all_files[:modify_count]:
        stat = file_path.stat()
        with file_path.open("r+b") as file:
            file.seek(stat.st_size // 2)
            file.write(b"MODIFIED"[: min(8, stat.st_size - stat.st_size // 2)])
        # rsync's default quick check uses whole seconds. Same-size patches must
        # be distinguishable even when generation and mutation happen together.
        os.utime(file_path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 2_000_000_000))

    return modify_count


# ============================================================================
# Benchmark Execution
# ============================================================================


def run_command(args: list[str]) -> tuple[float, bool, str, str]:
    """Time only the sync process, never fixture setup or verification."""
    start = time.perf_counter()
    try:
        result = subprocess.run(args, capture_output=True, text=True, check=False)
    except OSError as error:
        return (time.perf_counter() - start) * 1000, False, str(error), ""
    duration_ms = (time.perf_counter() - start) * 1000
    if result.returncode != 0:
        error = result.stderr.strip() or result.stdout.strip() or "no diagnostic"
        return (
            duration_ms,
            False,
            f"exit {result.returncode}: {error[:200]}",
            result.stdout,
        )
    return duration_ms, True, "", result.stdout


# Repeating itemization requests unchanged entries too. Exact record coverage
# prevents an ignored/unsupported format or empty output from proving zero.
# %i distinguishes transfers from metadata; %b includes transfer protocol bytes,
# so it is never reported as file payload. Apple's OpenRSYNC log.c counts per-item
# protocol I/O; Samba's log.c gates %b on ITEM_TRANSFER. Protocol-29 has 9 columns;
# modern Samba rsync emits 11. See rsync(1) --itemize-changes/--out-format and
# rsyncd.conf(5) log format; both implementations' log.c use transfer flags.
RSYNC_FORMAT = "SYBENCH|%i|%b|%n"


def transfer_evidence(tool: str, stdout: str, expected: dict) -> TransferEvidence:
    interface = "sy --json summary" if tool == "sy" else "rsync -ii %i/%b/%n"

    def unavailable(reason):
        return TransferEvidence("proof_unavailable", interface, reason)

    if tool == "sy":
        # Current owner: SyncReporter.finish / SyncEvent::Summary in
        # src/sync/output.rs. files_updated includes SyncOp::Metadata.
        try:
            events = [json.loads(line) for line in stdout.splitlines()]
        except json.JSONDecodeError:
            return unavailable("invalid NDJSON")
        if (
            not events
            or any(not isinstance(event, dict) for event in events)
            or events[-1].get("type") != "summary"
            or sum(event.get("type") == "summary" for event in events) != 1
            or any(event.get("type") == "error" for event in events)
        ):
            return unavailable("missing unique terminal summary")
        count = events[-1].get("bytes_transferred")
        if type(count) is not int or count < 0 or count > 2**64 - 1:
            return unavailable("invalid summary bytes_transferred")
        return TransferEvidence(
            "zero_file_payload" if count == 0 else "file_transfer",
            interface,
            bytes_transferred=count,
        )

    records = {"./": "d"}
    records.update(
        {
            name if entry is not None else f"{name}/": "f" if entry is not None else "d"
            for name, entry in expected.items()
        }
    )
    seen = set()
    transfers = 0
    log_bytes = 0
    unavailable_reason = ""
    attribute_letters = ("c", "s", "tT", "p", "o", "g", "unb", "a", "x")
    for line in stdout.splitlines():
        fields = line.split("|", 3)
        if len(fields) != 4 or fields[0] != "SYBENCH":
            unavailable_reason = "missing or unsupported itemize/log format"
            continue
        _, item, count, name = fields
        if (
            len(item) not in (9, 11)
            or item[0] not in ".<>c"
            or item[1] != records.get(name)
            or any(
                char not in f". +?{letters}"
                for char, letters in zip(item[2:], attribute_letters)
            )
            or name in seen
            or not count.isascii()
            or not count.isdecimal()
            or int(count) > 2**64 - 1
        ):
            unavailable_reason = "invalid itemize/log record"
            continue
        seen.add(name)
        if item[1] == "f":
            log_bytes += int(count)
            if item[0] in "<>c":
                transfers += 1
            # A '.' regular-file update is metadata-only regardless of protocol
            # I/O in %b. Transfer markers still count even with zero literal data.
    # A validated positive transfer disproves zero even when other records are
    # missing or malformed. Complete coverage is required only to prove zero.
    if not transfers:
        if unavailable_reason:
            return unavailable(unavailable_reason)
        if seen != set(records):
            return unavailable("incomplete repeated-itemize records")
    return TransferEvidence(
        "file_transfer" if transfers else "zero_file_payload",
        interface,
        regular_file_transfers=transfers,
        rsync_log_bytes=log_bytes,
    )


# Shared local/remote checker. SSH benchmarks require python3 on the peer.
# Hash reads are bounded; exact paths and empty directories are checked too.
MANIFEST_SCRIPT = """
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
if not root.is_dir():
    raise RuntimeError(f'missing destination directory: {root}')
manifest = {}
observe_files = len(sys.argv) > 2 and sys.argv[2] == 'file-observations'
for path in sorted(root.rglob('*')):
    name = str(path.relative_to(root))
    if path.is_symlink():
        raise RuntimeError(f'unexpected symlink: {path}')
    if path.is_dir():
        if not observe_files:
            manifest[name] = None
    elif path.is_file():
        stat = path.stat()
        if observe_files:
            manifest[name] = [stat.st_dev, stat.st_ino, stat.st_size,
                              stat.st_mtime_ns, stat.st_ctime_ns]
        else:
            digest = hashlib.sha256()
            with path.open('rb') as file:
                while block := file.read(1024 * 1024):
                    digest.update(block)
            manifest[name] = [stat.st_size, digest.hexdigest()]
    else:
        raise RuntimeError(f'unexpected entry: {path}')
print(json.dumps(manifest))
"""


def ssh_command(target: str, args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["ssh", "--", target, shlex.join(args)],
        capture_output=True,
        text=True,
        check=True,
    )


def tree_manifest(
    root: str, ssh_target: str | None = None, *, observe_files: bool = False
) -> dict:
    args = ["-c", MANIFEST_SCRIPT, root]
    if observe_files:
        args.append("file-observations")
    if ssh_target:
        result = ssh_command(ssh_target, ["python3", *args])
    else:
        result = subprocess.run(
            [sys.executable, *args],
            capture_output=True,
            text=True,
            check=True,
        )
    return json.loads(result.stdout)


def file_observations(root: str, ssh_target: str | None = None) -> dict:
    """Corroborating identity/mtime/ctime diagnostics, NOT transfer proof.

    Metadata refresh may change ctime without transferring file payload. Even
    stable observations and hashes cannot substitute for public transfer evidence.
    """
    return tree_manifest(root, ssh_target, observe_files=True)


def remove_tree(path: str, ssh_target: str | None):
    if ssh_target:
        ssh_command(ssh_target, ["rm", "-rf", "--", path])
    elif Path(path).exists():
        shutil.rmtree(path)


def copy_tree(source: str, dest: str, ssh_target: str | None):
    if ssh_target:
        ssh_command(ssh_target, ["cp", "-a", "--", source, dest])
    else:
        shutil.copytree(source, dest)


def benchmark_scenario(
    scenario_name: str,
    config: dict,
    sy_binary: Path,
    transport: str = "local",
    ssh_target: str | None = None,
    iterations: int = 3,
) -> list[BenchmarkResult]:
    """Run initial, unchanged, and changed-file samples with untimed checks."""
    if iterations < 1:
        raise ValueError("iterations must be positive")
    if transport not in ("local", "ssh") or (transport == "ssh" and not ssh_target):
        raise ValueError(
            "SSH transport requires a target; only local and ssh are supported"
        )
    if transport == "local":
        ssh_target = None
    results = []
    tools = ["sy"] + (["rsync"] if shutil.which("rsync") else [])

    with tempfile.TemporaryDirectory() as tmpdir:
        source_dir = Path(tmpdir) / "source"
        source_dir.mkdir()
        files_count, bytes_total = generate_test_data(source_dir, config)
        print(f"  Generated {files_count} files ({bytes_total / 1_000_000:.1f} MB)")
        original = tree_manifest(str(source_dir))
        expected = original
        remote_base = None
        if ssh_target:
            remote_base = ssh_command(
                ssh_target, ["mktemp", "-d", "/tmp/sy_bench.XXXXXXXX"]
            ).stdout.strip()
            if not remote_base.startswith("/tmp/sy_bench.") or "/" in remote_base[5:]:
                raise ValueError(f"invalid remote scratch path: {remote_base!r}")
        base = remote_base or tmpdir

        try:
            active_tools = tools.copy()
            for operation in ("initial", "incremental", "delta"):
                print(f"  Testing {operation} sync...")
                sample_files = files_count
                sample_bytes = bytes_total
                if operation == "delta":
                    modify_files(source_dir)
                    expected = tree_manifest(str(source_dir))
                    changed = [
                        name for name in expected if expected[name] != original[name]
                    ]
                    if not changed:
                        raise ValueError("delta fixture contains no changed bytes")
                    sample_files = len(changed)
                    sample_bytes = sum(expected[name][0] for name in changed)

                for tool in active_tools.copy():
                    dest = f"{base}/{tool}"
                    basis = f"{base}/{tool}_basis"
                    dest_arg = f"{ssh_target}:{dest}" if ssh_target else dest
                    # Archive contents syntax for ordinary caller-owned files.
                    # sy -a is not generally rsync -a's owner/group/device policy.
                    # Filtering is opt-in in the current sy CLI.
                    args = (
                        [
                            str(sy_binary),
                            "-a",
                            "--json",
                            f"{source_dir}{os.sep}",
                            dest_arg,
                        ]
                        if tool == "sy"
                        else [
                            "rsync",
                            "-a",
                            "-ii",
                            f"--out-format={RSYNC_FORMAT}",
                            f"{source_dir}/",
                            dest_arg,
                        ]
                    )
                    durations = []
                    error = ""
                    error_kind = None
                    evidence = []
                    file_changes = []
                    for sample in range(iterations):
                        if operation == "initial":
                            remove_tree(dest, ssh_target)
                        elif operation == "delta":
                            # Every timed sample starts from the unchanged basis,
                            # including SSH. Reset and digest checks are untimed.
                            remove_tree(dest, ssh_target)
                            copy_tree(basis, dest, ssh_target)
                            if tree_manifest(dest, ssh_target) != original:
                                raise RuntimeError(f"{tool}: delta basis reset failed")

                        before = (
                            file_observations(dest, ssh_target)
                            if operation == "incremental"
                            else None
                        )
                        duration, success, diagnostic, stdout = run_command(args)
                        if not success:
                            error = f"sample {sample + 1}/{iterations}: {diagnostic}"
                            error_kind = "execution"
                            active_tools.remove(tool)
                            break
                        after = (
                            file_observations(dest, ssh_target)
                            if before is not None
                            else None
                        )
                        if tree_manifest(dest, ssh_target) != expected:
                            raise RuntimeError(
                                f"{tool}/{operation} sample {sample + 1}: content mismatch"
                            )
                        if before is not None:
                            file_changes.append(
                                {
                                    name: {
                                        "before": before.get(name),
                                        "after": after.get(name),
                                    }
                                    for name in before.keys() | after.keys()
                                    if before.get(name) != after.get(name)
                                }
                            )
                            proof = transfer_evidence(tool, stdout, expected)
                            evidence.append(proof)
                            if proof.status != "zero_file_payload":
                                error_kind = proof.status
                                error = (
                                    f"sample {sample + 1}/{iterations}: {proof.status}: "
                                    + (
                                        proof.diagnostic
                                        or "regular-file transfer reported"
                                    )
                                )
                                # Verified contents still permit independent delta work.
                                break
                        durations.append(duration)

                    results.append(
                        BenchmarkResult(
                            scenario=scenario_name,
                            tool=tool,
                            operation=operation,
                            duration_ms=(
                                duration
                                if error
                                else sorted(durations)[len(durations) // 2]
                            ),
                            files_count=sample_files,
                            bytes_total=(
                                (None if error else 0)
                                if operation == "incremental"
                                else sample_bytes
                            ),
                            error=error or None,
                            error_kind=error_kind,
                            transfer_evidence=evidence,
                            file_changes=file_changes,
                        )
                    )
                    if operation == "initial" and not error:
                        copy_tree(dest, basis, ssh_target)
        finally:
            if remote_base:
                remove_tree(remote_base, ssh_target)

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
                "error_kind": r.error_kind,
                "transfer_evidence": [asdict(proof) for proof in r.transfer_evidence],
                "file_changes": r.file_changes,
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
            sy_result = tools.get("sy", {})
            rsync_result = tools.get("rsync", {})
            sy_ms = sy_result.get("ms") if not sy_result.get("err") else None
            rsync_ms = rsync_result.get("ms") if not rsync_result.get("err") else None

            if sy_ms and rsync_ms:
                speedup = rsync_ms / sy_ms
                speedup_str = (
                    f"{speedup:.2f}x" if speedup >= 1 else f"{1 / speedup:.2f}x slower"
                )
            else:
                speedup_str = "N/A"

            sy_text = (
                "ERROR"
                if sy_result.get("err")
                else (f"{sy_ms:.1f}" if sy_ms is not None else "-")
            )
            rsync_text = (
                "ERROR"
                if rsync_result.get("err")
                else (f"{rsync_ms:.1f}" if rsync_ms is not None else "-")
            )
            print(
                f"{scenario:<15} {op:<12} {sy_text:<12} {rsync_text:<12} {speedup_str:<10}"
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
            if r.get("err") or run1_lookup[key].get("err"):
                print(
                    f"{r['scenario']:<15} {r['op']:<10} {r['tool']:<8} ERROR (not compared)"
                )
                continue
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
                        f"{scenario:<15} {op:<12} {tool_name:<8} "
                        f"{'UNAVAILABLE' if r.error_kind == 'proof_unavailable' else 'ERROR'}: {r.error}"
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

        if (
            sy_r
            and rsync_r
            and not sy_r.error
            and not rsync_r.error
            and sy_r.duration_ms > 0
        ):
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
        "--ssh",
        type=str,
        help="SSH target (user@host); requires sy and python3 on peer",
    )
    parser.add_argument("--iterations", type=int, default=3, help="Iterations per test")
    parser.add_argument(
        "--sy-binary",
        type=Path,
        default=DEFAULT_SY_BINARY,
        help="sy executable to measure (default: repository target/release/sy)",
    )
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

    if args.iterations < 1:
        parser.error("--iterations must be positive")
    try:
        sy_binary = resolve_sy_binary(args.sy_binary)
        version_info = get_version_info(sy_binary)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.error(f"cannot inspect sy artifact: {error}")

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
    git_info["scope"] = "harness repository, not artifact build provenance"

    print(f"System: {system_info.get('cpu', 'unknown')[:40]}")
    print(f"Git: {git_info['commit']} ({git_info['branch']})")
    print(
        f"Versions: sy={version_info.get('sy', '?')}, rsync={version_info.get('rsync', '?')}"
    )
    print(f"Artifact: {sy_binary} (sha256 {version_info['sy_binary']['sha256']})")
    print("MB/s counts logical file bytes, not measured wire traffic.")
    print()

    # Run benchmarks
    all_results = []

    for scenario_name, config in scenarios.items():
        print(f"\n--- Scenario: {scenario_name} ---")
        try:
            results = benchmark_scenario(
                scenario_name,
                config,
                sy_binary=sy_binary,
                transport=transport,
                ssh_target=args.ssh,
                iterations=args.iterations,
            )
        except (
            OSError,
            ValueError,
            RuntimeError,
            subprocess.CalledProcessError,
        ) as error:
            print(f"Benchmark failed: {error}", file=sys.stderr)
            sys.exit(1)
        all_results.extend(results)

    for tool in ("sy", "rsync"):
        artifact = version_info.get(f"{tool}_binary")
        if artifact and sha256_file(Path(artifact["path"])) != artifact["sha256"]:
            print(f"Benchmark failed: {tool} artifact changed during the run", file=sys.stderr)
            sys.exit(1)

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
    if any(result.error for result in all_results):
        sys.exit(1)


if __name__ == "__main__":
    main()
