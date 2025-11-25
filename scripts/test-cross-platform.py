#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["rich"]
# ///
"""
Cross-platform test suite for sy.

Tests sy across macOS and Fedora, covering:
- Local unit/integration tests
- Large-scale tests (10k+ files)
- SSH transfers between platforms
- Cross-filesystem metadata preservation
- Real-world scenarios
- Performance baselines

Usage:
    uv run scripts/test-cross-platform.py              # Standard run
    uv run scripts/test-cross-platform.py --large      # Include large/slow tests
    uv run scripts/test-cross-platform.py --skip-build # Skip cargo builds
    uv run scripts/test-cross-platform.py --local-only # Skip SSH tests
    uv run scripts/test-cross-platform.py --perf       # Run performance baselines
"""

import argparse
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

# Optional rich import for nicer output
try:
    from rich.console import Console
    from rich.table import Table
    from rich.progress import Progress, SpinnerColumn, TextColumn, TimeElapsedColumn
    RICH_AVAILABLE = True
except ImportError:
    RICH_AVAILABLE = False


# ============================================================================
# Configuration
# ============================================================================

FEDORA_HOST = "fedora"
FEDORA_USER = "nick"
FEDORA_REPO = "~/github/nijaru/sy"

PROJECT_ROOT = Path(__file__).parent.parent
SY_BIN = PROJECT_ROOT / "target/release/sy"


# ============================================================================
# Output helpers
# ============================================================================

if RICH_AVAILABLE:
    console = Console()

    def log_info(msg: str):
        console.print(f"[blue]ℹ[/blue] {msg}")

    def log_success(msg: str):
        console.print(f"[green]✓[/green] {msg}")

    def log_error(msg: str):
        console.print(f"[red]✗[/red] {msg}")

    def log_warn(msg: str):
        console.print(f"[yellow]⚠[/yellow] {msg}")

    def log_header(msg: str):
        console.print(f"\n[bold cyan]{msg}[/bold cyan]")
else:
    def log_info(msg: str):
        print(f"ℹ {msg}")

    def log_success(msg: str):
        print(f"✓ {msg}")

    def log_error(msg: str):
        print(f"✗ {msg}")

    def log_warn(msg: str):
        print(f"⚠ {msg}")

    def log_header(msg: str):
        print(f"\n=== {msg} ===")


# ============================================================================
# Test result tracking
# ============================================================================

@dataclass
class TestResult:
    name: str
    passed: bool
    duration: float
    error: Optional[str] = None


class TestRunner:
    def __init__(self, verbose: bool = False):
        self.verbose = verbose
        self.results: list[TestResult] = []

    def run(self, name: str, cmd: list[str], cwd: Optional[Path] = None,
            timeout: int = 300) -> bool:
        """Run a test command and track result."""
        start = time.time()

        def do_run():
            return subprocess.run(
                cmd,
                cwd=cwd or PROJECT_ROOT,
                capture_output=not self.verbose,
                text=True,
                timeout=timeout,
            )

        try:
            if self.verbose or not RICH_AVAILABLE:
                result = do_run()
            else:
                # Show spinner while running
                with console.status(f"[bold blue]{name}...", spinner="dots"):
                    result = do_run()

            duration = time.time() - start
            passed = result.returncode == 0

            error = None
            if not passed and not self.verbose:
                error = result.stderr or result.stdout

            self.results.append(TestResult(name, passed, duration, error))

            if passed:
                log_success(f"{name} ({duration:.1f}s)")
            else:
                log_error(f"{name} ({duration:.1f}s)")
                if error and not self.verbose:
                    # Show last few lines of error
                    lines = error.strip().split('\n')[-10:]
                    for line in lines:
                        print(f"    {line}")

            return passed
        except subprocess.TimeoutExpired:
            duration = time.time() - start
            self.results.append(TestResult(name, False, duration, "Timeout"))
            log_error(f"{name} (timeout after {timeout}s)")
            return False
        except Exception as e:
            duration = time.time() - start
            self.results.append(TestResult(name, False, duration, str(e)))
            log_error(f"{name}: {e}")
            return False

    def run_ssh(self, name: str, remote_cmd: str, timeout: int = 300) -> bool:
        """Run a command on Fedora via SSH."""
        cmd = ["ssh", f"{FEDORA_USER}@{FEDORA_HOST}", remote_cmd]
        return self.run(name, cmd, timeout=timeout)

    def summary(self) -> bool:
        """Print summary and return True if all passed."""
        passed = sum(1 for r in self.results if r.passed)
        failed = sum(1 for r in self.results if not r.passed)
        total_time = sum(r.duration for r in self.results)

        log_header("Test Summary")

        if RICH_AVAILABLE:
            table = Table()
            table.add_column("Test", style="cyan")
            table.add_column("Status")
            table.add_column("Duration", justify="right")

            for r in self.results:
                status = "[green]PASS[/green]" if r.passed else "[red]FAIL[/red]"
                table.add_row(r.name, status, f"{r.duration:.1f}s")

            console.print(table)
        else:
            for r in self.results:
                status = "PASS" if r.passed else "FAIL"
                print(f"  {status}: {r.name} ({r.duration:.1f}s)")

        print()
        if failed == 0:
            log_success(f"All {passed} tests passed in {total_time:.1f}s")
        else:
            log_error(f"{failed}/{passed + failed} tests failed")

        return failed == 0


# ============================================================================
# Test categories
# ============================================================================

def check_ssh_connection(runner: TestRunner) -> bool:
    """Verify SSH connection to Fedora."""
    log_info("Checking SSH connection...")
    return runner.run(
        "SSH connection",
        ["ssh", "-o", "ConnectTimeout=5", "-o", "BatchMode=yes",
         f"{FEDORA_USER}@{FEDORA_HOST}", "echo ok"],
        timeout=10
    )


def build_local(runner: TestRunner) -> bool:
    """Build sy locally."""
    log_header("Building locally")

    ok = runner.run("Build sy", ["cargo", "build", "--release", "--bin", "sy"])
    ok = runner.run("Build sy-remote", ["cargo", "build", "--release", "--bin", "sy-remote"]) and ok
    return ok


def build_fedora(runner: TestRunner) -> bool:
    """Build sy-remote on Fedora."""
    log_header("Building on Fedora")

    branch = subprocess.run(
        ["git", "branch", "--show-current"],
        capture_output=True, text=True, cwd=PROJECT_ROOT
    ).stdout.strip()

    log_info(f"Syncing branch: {branch}")

    # Sync repo on Fedora
    ok = runner.run_ssh(
        "Git sync",
        f"cd {FEDORA_REPO} && git fetch origin && git checkout {branch} && git pull origin {branch}"
    )
    if not ok:
        return False

    # Build sy-remote
    ok = runner.run_ssh(
        "Build sy-remote (Fedora)",
        f"cd {FEDORA_REPO} && cargo build --release --bin sy-remote"
    )
    if not ok:
        return False

    # Install to PATH
    ok = runner.run_ssh(
        "Install sy-remote",
        f"cd {FEDORA_REPO} && cargo install --path . --bin sy-remote --force"
    )
    return ok


def test_local_unit(runner: TestRunner) -> bool:
    """Run local unit and integration tests."""
    log_header("Local Tests")
    return runner.run("Unit/integration tests", ["cargo", "test"], timeout=180)


def test_local_large(runner: TestRunner) -> bool:
    """Run large-scale local tests."""
    log_header("Large-Scale Tests")

    ok = runner.run(
        "Massive directory tests",
        ["cargo", "test", "--test", "massive_directory_test", "--", "--ignored"],
        timeout=600
    )
    ok = runner.run(
        "Large file tests",
        ["cargo", "test", "--test", "large_file_test", "--", "--ignored"],
        timeout=300
    ) and ok
    return ok


def test_ssh_comprehensive(runner: TestRunner) -> bool:
    """Run SSH comprehensive tests."""
    log_header("SSH Comprehensive Tests")
    return runner.run(
        "SSH comprehensive",
        ["cargo", "test", "--test", "ssh_comprehensive_test", "--", "--ignored"],
        timeout=300
    )


def test_cross_filesystem(runner: TestRunner) -> bool:
    """Test cross-filesystem sync (APFS ↔ ext4)."""
    log_header("Cross-Filesystem Tests")

    import tempfile

    with tempfile.TemporaryDirectory() as tmpdir:
        local_src = Path(tmpdir) / "src"
        local_src.mkdir()

        # Create test files with various attributes
        (local_src / "regular.txt").write_text("hello world")
        (local_src / "empty.txt").write_text("")
        (local_src / "subdir").mkdir()
        (local_src / "subdir" / "nested.txt").write_text("nested content")

        # Binary file
        (local_src / "binary.bin").write_bytes(bytes(range(256)))

        # File with spaces and special chars
        (local_src / "file with spaces.txt").write_text("spaces")
        (local_src / "special!@#.txt").write_text("special")

        remote_dest = "/tmp/sy-cross-fs-test"
        local_roundtrip = Path(tmpdir) / "roundtrip"

        # Sync local → Fedora
        ok = runner.run(
            "Sync macOS → Fedora",
            [str(SY_BIN), str(local_src) + "/", f"{FEDORA_USER}@{FEDORA_HOST}:{remote_dest}"],
            timeout=60
        )
        if not ok:
            return False

        # Sync Fedora → local (roundtrip)
        ok = runner.run(
            "Sync Fedora → macOS",
            [str(SY_BIN), f"{FEDORA_USER}@{FEDORA_HOST}:{remote_dest}/", str(local_roundtrip)],
            timeout=60
        )
        if not ok:
            return False

        # Verify roundtrip integrity
        import filecmp
        import os

        match, mismatch, errors = filecmp.cmpfiles(
            local_src, local_roundtrip,
            [f.name for f in local_src.iterdir() if f.is_file()],
            shallow=False
        )

        if mismatch or errors:
            log_error(f"Roundtrip mismatch: {mismatch}, errors: {errors}")
            return False

        log_success(f"Roundtrip verified: {len(match)} files match")

        # Cleanup remote
        runner.run_ssh("Cleanup remote", f"rm -rf {remote_dest}")

        return True


def test_real_world_scenarios(runner: TestRunner) -> bool:
    """Test real-world sync scenarios."""
    log_header("Real-World Scenarios")

    import tempfile

    with tempfile.TemporaryDirectory() as tmpdir:
        src = Path(tmpdir) / "src"
        dst = Path(tmpdir) / "dst"
        src.mkdir()
        dst.mkdir()

        # Scenario 1: Git repository sync
        log_info("Scenario: Git repository")
        subprocess.run(["git", "init"], cwd=src, capture_output=True)
        (src / "README.md").write_text("# Test")
        (src / ".gitignore").write_text("*.log\n")
        (src / "debug.log").write_text("should be ignored with --gitignore")

        ok = runner.run(
            "Git repo (include all)",
            [str(SY_BIN), str(src) + "/", str(dst)],
            timeout=30
        )

        # Verify .git is included by default (new v0.1.0 behavior)
        if not (dst / ".git").exists():
            log_error("Expected .git to be synced by default")
            return False
        if not (dst / "debug.log").exists():
            log_error("Expected debug.log to be synced by default")
            return False

        # Scenario 2: With --gitignore and --exclude-vcs (developer workflow)
        dst2 = Path(tmpdir) / "dst2"
        dst2.mkdir()

        ok = runner.run(
            "Git repo (--gitignore --exclude-vcs)",
            [str(SY_BIN), str(src) + "/", str(dst2), "--gitignore", "--exclude-vcs"],
            timeout=30
        ) and ok

        # Verify .git excluded and .gitignore respected
        if (dst2 / ".git").exists():
            log_error("Expected .git to be excluded with --exclude-vcs")
            return False
        if (dst2 / "debug.log").exists():
            log_error("Expected debug.log to be excluded with --gitignore")
            return False

        log_success("Git repo scenarios passed")

        # Scenario 3: Idempotent sync
        log_info("Scenario: Idempotent sync")

        result = subprocess.run(
            [str(SY_BIN), str(src) + "/", str(dst), "--exclude-vcs"],
            capture_output=True, text=True, cwd=PROJECT_ROOT
        )
        if "Files skipped:" not in result.stdout:
            log_warn("Expected files to be skipped on re-sync")

        return ok


def test_performance_baselines(runner: TestRunner) -> bool:
    """Run performance baseline tests and report timings."""
    log_header("Performance Baselines")

    # Run the performance regression tests
    ok = runner.run(
        "Performance regression suite",
        ["cargo", "test", "--test", "performance_test", "--release"],
        timeout=120
    )

    return ok


def test_cli_flag_combinations(runner: TestRunner) -> bool:
    """Test various CLI flag combinations."""
    log_header("CLI Flag Combinations")

    import tempfile

    with tempfile.TemporaryDirectory() as tmpdir:
        src = Path(tmpdir) / "src"
        dst = Path(tmpdir) / "dst"
        src.mkdir()

        # Create test files
        (src / "file1.txt").write_text("content1")
        (src / "file2.txt").write_text("content2")

        flag_combos = [
            (["--dry-run"], "Dry run"),
            (["--quiet"], "Quiet mode"),
            (["-z"], "Compression"),
            (["--checksum"], "Checksum mode"),
            (["--size-only"], "Size only"),
            (["--ignore-times"], "Ignore times"),
        ]

        all_ok = True
        for flags, name in flag_combos:
            dst_path = Path(tmpdir) / f"dst_{name.replace(' ', '_')}"
            dst_path.mkdir(exist_ok=True)

            ok = runner.run(
                f"CLI: {name}",
                [str(SY_BIN), str(src) + "/", str(dst_path)] + flags,
                timeout=30
            )
            all_ok = all_ok and ok

        return all_ok


# ============================================================================
# Main
# ============================================================================

def main():
    parser = argparse.ArgumentParser(description="Cross-platform test suite for sy")
    parser.add_argument("-v", "--verbose", action="store_true", help="Verbose output")
    parser.add_argument("-l", "--large", action="store_true", help="Include large/slow tests")
    parser.add_argument("-s", "--skip-build", action="store_true", help="Skip cargo builds")
    parser.add_argument("--local-only", action="store_true", help="Skip SSH/remote tests")
    parser.add_argument("--perf", action="store_true", help="Run performance baselines")
    args = parser.parse_args()

    runner = TestRunner(verbose=args.verbose)

    log_header("sy Cross-Platform Test Suite")
    if args.large:
        log_info("Including large tests")
    if args.skip_build:
        log_info("Skipping builds")
    if args.local_only:
        log_info("Local only (no SSH)")

    # Build
    if not args.skip_build:
        if not build_local(runner):
            return 1

    # Local tests (always run)
    if not test_local_unit(runner):
        log_error("Local tests failed - fix before continuing")
        runner.summary()
        return 1

    # Large tests (optional)
    if args.large:
        test_local_large(runner)

    # Performance baselines (optional)
    if args.perf:
        test_performance_baselines(runner)

    # CLI flag combinations
    test_cli_flag_combinations(runner)

    # Real-world scenarios
    test_real_world_scenarios(runner)

    # SSH/remote tests
    if not args.local_only:
        log_header("Remote Tests")

        if not check_ssh_connection(runner):
            log_warn("SSH not available - skipping remote tests")
        else:
            if not args.skip_build:
                build_fedora(runner)

            test_ssh_comprehensive(runner)
            test_cross_filesystem(runner)

    # Summary
    print()
    success = runner.summary()

    if success:
        branch = subprocess.run(
            ["git", "branch", "--show-current"],
            capture_output=True, text=True, cwd=PROJECT_ROOT
        ).stdout.strip()
        print()
        log_info(f"Branch '{branch}' is ready for CI")

    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())
