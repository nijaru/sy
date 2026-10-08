"""Focused harness checks: python3 -m unittest discover -s scripts -p 'test_benchmark.py'."""

import contextlib
import hashlib
import io
import json
import os
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import benchmark

# Real subprocess boundary, with a quick-check copier and a log of bytes that
# differed BEFORE each invocation. It catches an unchanged second delta sample.
TOOL_SCRIPT = """#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
if '--version' in sys.argv:
    print('sy harness-test-artifact')
    sys.exit(0)
args = [arg for arg in sys.argv[1:] if arg != '-a']
source, dest = map(pathlib.Path, (args[0], args[1].split(':')[-1]))
log = pathlib.Path(os.environ['HARNESS_LOG'])
calls = len(log.read_text().splitlines()) if log.exists() else 0
changed = []
for file in sorted(source.rglob('*')):
    if file.is_file():
        target = dest / file.relative_to(source)
        if not target.exists() or file.read_bytes() != target.read_bytes():
            changed.append(str(file.relative_to(source)))
with log.open('a') as out:
    out.write(json.dumps({'tool': pathlib.Path(sys.argv[0]).name, 'changed': changed,
                          'exists': dest.exists()}) + '\\n')
if calls + 1 == int(os.environ.get('HARNESS_FAIL_AT', '0')):
    print('injected sync failure', file=sys.stderr)
    sys.exit(7)
dest.mkdir(parents=True, exist_ok=True)
for file in sorted(source.rglob('*')):
    target = dest / file.relative_to(source)
    if file.is_dir():
        target.mkdir(parents=True, exist_ok=True)
    elif (not target.exists() or file.stat().st_size != target.stat().st_size
          or int(file.stat().st_mtime) != int(target.stat().st_mtime)):
        shutil.copy2(file, target)
if os.environ.get('HARNESS_CORRUPT'):
    file = next(file for file in dest.rglob('*') if file.is_file())
    with file.open('r+b') as out:
        out.write(b'!')
"""


class BenchmarkHarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "chosen sy"
        self.binary.write_text(
            TOOL_SCRIPT.replace("#!/usr/bin/env python3", f"#!{sys.executable}")
        )
        self.binary.chmod(0o755)
        self.rsync = self.root / "rsync"
        self.rsync.write_text(self.binary.read_text())
        self.rsync.chmod(0o755)
        self.log = self.root / "calls.jsonl"
        self.env = patch.dict(
            os.environ,
            {
                "HARNESS_LOG": str(self.log),
                "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
            },
        )
        self.env.start()
        self.addCleanup(self.env.stop)
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)

    def scenario(self, **kwargs):
        return benchmark.benchmark_scenario(
            "probe",
            {
                "files": 10,
                "size_kb": 1,
                "dirs": 2,
                "large_files": 1,
                "large_size_kb": 3,
            },
            self.binary,
            iterations=3,
            **kwargs,
        )

    def test_every_changed_sample_starts_from_basis_local_and_ssh(self):
        for transport in ("local", "ssh"):
            with self.subTest(transport=transport):
                self.log.unlink(missing_ok=True)
                scratch = []

                def ssh_on_local_peer(target, args, scratch=scratch):
                    self.assertEqual(target, "test-peer")
                    result = subprocess.run(
                        args, capture_output=True, text=True, check=True
                    )
                    if args[0] == "mktemp":
                        scratch.append(Path(result.stdout.strip()))
                    return result

                # The SSH process is isolated; remote cp/reset/hash/cleanup run
                # as real commands on a local peer, including shell quoting.
                with patch.object(
                    benchmark, "ssh_command", side_effect=ssh_on_local_peer
                ):
                    results = self.scenario(
                        transport=transport,
                        ssh_target="test-peer" if transport == "ssh" else None,
                    )
                self.assertEqual(len(results), 6)
                self.assertTrue(all(result.error is None for result in results))
                calls = [json.loads(line) for line in self.log.read_text().splitlines()]
                for tool in (self.binary.name, "rsync"):
                    samples = [call for call in calls if call["tool"] == tool]
                    self.assertEqual(len(samples), 9)
                    self.assertEqual(
                        [len(call["changed"]) for call in samples],
                        [11] * 3 + [0] * 3 + [1] * 3,
                    )
                    self.assertEqual(
                        [call["exists"] for call in samples], [False] * 3 + [True] * 6
                    )
                self.assertTrue(all(not path.exists() for path in scratch))

    def test_failed_later_sample_is_not_a_successful_median(self):
        # Each tool has 3 initial samples; sy's second unchanged sample fails.
        with patch.dict(os.environ, {"HARNESS_FAIL_AT": "8"}):
            results = self.scenario()
        failed = [result for result in results if result.error]
        self.assertEqual(len(failed), 1)
        self.assertEqual((failed[0].tool, failed[0].operation), ("sy", "incremental"))
        self.assertIn("sample 2/3: exit 7: injected sync failure", failed[0].error)
        self.assertGreater(failed[0].duration_ms, 0)
        self.assertFalse(
            any(
                result.tool == "sy" and result.operation == "delta"
                for result in results
            )
        )

    def test_same_size_corruption_is_rejected(self):
        with (
            patch.dict(os.environ, {"HARNESS_CORRUPT": "1"}),
            self.assertRaisesRegex(RuntimeError, "content mismatch"),
        ):
            self.scenario()

    def test_cli_artifact_path_metadata_and_failure_status(self):
        history = self.root / "history.jsonl"
        argv = [
            "benchmark.py",
            "--quick",
            "--sy-binary",
            str(self.binary),
            "--iterations",
            "1",
        ]
        with (
            patch.object(sys, "argv", argv),
            patch.object(benchmark, "HISTORY_FILE", history),
        ):
            benchmark.main()
        run = json.loads(history.read_text())
        artifact = run["ver"]["sy_binary"]
        self.assertEqual(artifact["path"], str(self.binary.resolve()))
        self.assertEqual(
            artifact["sha256"], hashlib.sha256(self.binary.read_bytes()).hexdigest()
        )
        self.assertEqual(artifact["version"], "sy harness-test-artifact")
        self.assertTrue(all(result["err"] is None for result in run["results"]))
        self.log.unlink()
        with (
            patch.dict(os.environ, {"HARNESS_FAIL_AT": "1"}),
            patch.object(sys, "argv", argv),
            patch.object(benchmark, "HISTORY_FILE", history),
            self.assertRaises(SystemExit) as error,
        ):
            benchmark.main()
        self.assertEqual(error.exception.code, 1)
        failed_run = json.loads(history.read_text().splitlines()[-1])
        self.assertIn("exit 7", failed_run["results"][0]["err"])

    def test_missing_default_artifact_never_falls_back_to_path(self):
        with (
            patch.object(benchmark, "DEFAULT_SY_BINARY", self.root / "missing"),
            patch.object(sys, "argv", ["benchmark.py", "--quick"]),
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(SystemExit) as error,
        ):
            benchmark.main()
        self.assertEqual(error.exception.code, 2)
        self.assertFalse(self.log.exists())

    def test_large_generation_and_patch_are_bounded_and_preserve_size(self):
        source = self.root / "source"
        source.mkdir()
        # Reject whole-file helpers, even though this fixture itself is small.
        with (
            patch.object(
                Path, "read_bytes", side_effect=AssertionError("whole-file read")
            ),
            patch.object(
                Path, "write_bytes", side_effect=AssertionError("whole-file write")
            ),
        ):
            count, size = benchmark.generate_test_data(
                source, {"files": 0, "dirs": 0, "large_files": 1, "large_size_kb": 2051}
            )
            file = source / "large_0.bin"
            before = file.stat()
            digest = benchmark.sha256_file(file)
            self.assertEqual(benchmark.modify_files(source), 1)
            self.assertEqual(file.stat().st_size, before.st_size)
            self.assertGreaterEqual(
                file.stat().st_mtime_ns - before.st_mtime_ns, 2_000_000_000
            )
            self.assertNotEqual(benchmark.sha256_file(file), digest)
        self.assertEqual((count, size), (1, 2051 * 1024))

    def test_delta_byte_accounting_uses_actual_changed_file_sizes(self):
        results = benchmark.benchmark_scenario(
            "large-only",
            {"files": 0, "dirs": 0, "large_files": 1, "large_size_kb": 3},
            self.binary,
            iterations=2,
        )
        delta = [result for result in results if result.operation == "delta"]
        self.assertEqual(len(delta), 2)
        self.assertTrue(
            all(
                result.files_count == 1 and result.bytes_total == 3072
                for result in delta
            )
        )

    def test_ssh_shell_arguments_are_quoted(self):
        with patch.object(subprocess, "run") as run:
            benchmark.ssh_command("test-peer", ["cp", "a b", "'$(false)'"])
        argv = run.call_args.args[0]
        self.assertEqual(argv[:3], ["ssh", "--", "test-peer"])
        self.assertEqual(shlex.split(argv[3]), ["cp", "a b", "'$(false)'"])


if __name__ == "__main__":
    unittest.main()
