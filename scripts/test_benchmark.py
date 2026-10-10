"""Focused harness checks: python3 -m unittest discover -s scripts -p 'test_benchmark.py'."""

import contextlib
import hashlib
import io
import json
import os
import shlex
import shutil
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
args = [arg for arg in sys.argv[1:] if arg not in ('-a', '--json', '-ii')
        and not arg.startswith('--out-format=')]
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
records = [('cd+++++++', 0, './')]
bytes_transferred = 0
metadata_updates = 0
for file in sorted(source.rglob('*')):
    target = dest / file.relative_to(source)
    name = str(file.relative_to(source))
    if file.is_dir():
        target.mkdir(parents=True, exist_ok=True)
        records.append(('.d       ', 0, name + '/'))
    elif (not target.exists() or file.stat().st_size != target.stat().st_size
          or int(file.stat().st_mtime) != int(target.stat().st_mtime)
          or os.environ.get('HARNESS_FORCE_COPY')):
        shutil.copy2(file, target)
        bytes_transferred += file.stat().st_size
        records.append(('>f+++++++', file.stat().st_size + 40, name))
    else:
        if os.environ.get('HARNESS_METADATA'):
            target.chmod(target.stat().st_mode)  # ctime only, no file payload
            metadata_updates += 1
        records.append(('.f...p...', 0, name))
if not os.environ.get('HARNESS_NO_PROOF'):
    if '--json' in sys.argv:
        print(json.dumps({'type': 'summary', 'bytes_transferred': bytes_transferred,
                          'files_updated': metadata_updates}))
    else:
        for item, count, name in records:
            print(f'SYBENCH|{item}|{count}|{name}')
if os.environ.get('HARNESS_CORRUPT'):
    file = next(file for file in dest.rglob('*') if file.is_file())
    with file.open('r+b') as out:
        out.write(b'!')
"""


class RealBenchmarkHarnessTests(unittest.TestCase):
    """Controlled caller-owned fixtures; not general archive-policy equivalence.

    Build the default CLI with cargo build --bin sy first, or select an existing
    artifact with SY_BENCHMARK_TEST_BINARY. No SSH process or heavy dataset runs.
    """

    @classmethod
    def setUpClass(cls):
        cls.binary = Path(
            os.environ.get(
                "SY_BENCHMARK_TEST_BINARY",
                str(benchmark.REPO_ROOT / "target" / "debug" / "sy"),
            )
        ).resolve()
        if not cls.binary.is_file():
            raise unittest.SkipTest("build sy with cargo build --bin sy first")
        cls.tools = ["sy"] + (["rsync"] if shutil.which("rsync") else [])

    def setUp(self):
        output = contextlib.redirect_stdout(io.StringIO())
        output.__enter__()
        self.addCleanup(output.__exit__, None, None, None)

    def test_archive_contents_exact_fixture_and_each_delta_basis(self):
        # More directories than files exercises empty-directory verification.
        config = {"files": 2, "size_kb": 1, "dirs": 4, "depth": 1}
        generate = benchmark.generate_test_data
        run = benchmark.run_command
        observed = {"sy": [], "rsync": []}
        originals = []

        def inspect_fixture(root, config):
            counts = generate(root, config)
            tree = benchmark.tree_manifest(str(root))
            self.assertEqual(
                sum(value is not None for value in tree.values()), counts[0]
            )
            self.assertFalse(any(name.split("/")[0] == ".git" for name in tree))
            originals.append(tree)
            return counts

        def inspect_sample(args):
            tool = "sy" if args[0] == str(self.binary) else "rsync"
            dest = Path(args[-1])
            observed[tool].append(
                benchmark.tree_manifest(str(dest)) if dest.exists() else None
            )
            return run(args)

        # Inspect the real process boundary, not a replacement sync algorithm.
        with (
            patch.object(benchmark, "generate_test_data", side_effect=inspect_fixture),
            patch.object(benchmark, "run_command", side_effect=inspect_sample),
        ):
            results = benchmark.benchmark_scenario(
                "tiny", config, self.binary, iterations=2
            )
        self.assertEqual(len(results), 3 * len(self.tools))
        for tool in self.tools:
            initial = next(
                result
                for result in results
                if result.tool == tool and result.operation == "initial"
            )
            self.assertIsNone(initial.error)
            unchanged = next(
                result
                for result in results
                if result.tool == tool and result.operation == "incremental"
            )
            if tool == "rsync" and unchanged.error_kind == "proof_unavailable":
                # Unsupported public interface is not a semantic failure and must
                # not be reported as zero bytes. Other operations still run.
                self.assertIsNone(unchanged.bytes_total)
                self.assertEqual(observed[tool], [None, None] + [originals[0]] * 3)
            else:
                self.assertIsNone(unchanged.error)
                self.assertEqual(unchanged.bytes_total, 0)
                self.assertEqual(
                    [proof.status for proof in unchanged.transfer_evidence],
                    ["zero_file_payload"] * 2,
                )
                self.assertEqual(observed[tool], [None, None] + [originals[0]] * 4)
            delta = next(
                result
                for result in results
                if result.tool == tool and result.operation == "delta"
            )
            self.assertIsNone(delta.error)
            self.assertEqual((delta.files_count, delta.bytes_total), (1, 1024))

    def test_metadata_only_refresh_has_zero_payload_public_evidence(self):
        with tempfile.TemporaryDirectory() as scratch:
            source = Path(scratch) / "source"
            source.mkdir()
            (source / "file").write_bytes(b"payload")
            (source / "empty").mkdir()
            expected = benchmark.tree_manifest(str(source))
            for tool in self.tools:
                with self.subTest(tool=tool):
                    dest = Path(scratch) / tool
                    args = (
                        [str(self.binary), "-a", "--json"]
                        if tool == "sy"
                        else [
                            "rsync",
                            "-a",
                            "-ii",
                            f"--out-format={benchmark.RSYNC_FORMAT}",
                        ]
                    ) + [f"{source}{os.sep}", str(dest)]
                    self.assertTrue(benchmark.run_command(args)[1])
                    # A real metadata-only update: permissions differ, bytes and
                    # mtime agree. ctime changes but that does not violate zero payload.
                    (dest / "file").chmod(0o600)
                    before = benchmark.file_observations(str(dest))
                    _, success, diagnostic, stdout = benchmark.run_command(args)
                    self.assertTrue(success, diagnostic)
                    self.assertEqual(benchmark.tree_manifest(str(dest)), expected)
                    after = benchmark.file_observations(str(dest))
                    self.assertEqual(before["file"][:4], after["file"][:4])
                    self.assertNotEqual(before["file"][4], after["file"][4])
                    proof = benchmark.transfer_evidence(tool, stdout, expected)
                    self.assertEqual(proof.status, "zero_file_payload", proof)
                    if tool == "sy":
                        summary = json.loads(stdout.splitlines()[-1])
                        self.assertEqual(summary["files_updated"], 1)

    def test_original_bare_sy_and_basename_operands_are_rejected(self):
        run = benchmark.run_command
        for variant, diagnostic in (
            ("bare-sy", "file_transfer"),
            ("basename", "content mismatch"),
        ):
            with self.subTest(variant=variant):

                def original_command(args, variant=variant):
                    if args[0] == str(self.binary):
                        args = args.copy()
                        if variant == "bare-sy":
                            args.remove("-a")
                        else:
                            args[-2] = args[-2].rstrip(os.sep)
                    return run(args)

                with patch.object(
                    benchmark, "run_command", side_effect=original_command
                ):
                    if variant == "basename":
                        with self.assertRaisesRegex(RuntimeError, diagnostic):
                            benchmark.benchmark_scenario(
                                "original-defect",
                                {"files": 1, "size_kb": 1, "dirs": 0},
                                self.binary,
                                iterations=1,
                            )
                    else:
                        results = benchmark.benchmark_scenario(
                            "original-defect",
                            {"files": 1, "size_kb": 1, "dirs": 0},
                            self.binary,
                            iterations=1,
                        )
                        unchanged = next(
                            result
                            for result in results
                            if result.tool == "sy" and result.operation == "incremental"
                        )
                        self.assertIn(diagnostic, unchanged.error)
                        self.assertIsNone(unchanged.bytes_total)

    def test_exact_tree_checker_rejects_extra_git_wrong_content_and_missing_empty_dir(
        self,
    ):
        with tempfile.TemporaryDirectory() as scratch:
            source = Path(scratch) / "source"
            dest = Path(scratch) / "dest"
            source.mkdir()
            (source / "file").write_bytes(b"x")
            (source / "empty").mkdir()
            expected = benchmark.tree_manifest(str(source))
            subprocess.run(
                [str(self.binary), "-a", f"{source}{os.sep}", str(dest)],
                capture_output=True,
                check=True,
            )
            self.assertEqual(benchmark.tree_manifest(str(dest)), expected)
            (dest / ".git").mkdir()
            self.assertNotEqual(benchmark.tree_manifest(str(dest)), expected)
            (dest / ".git").rmdir()
            (dest / "empty").rmdir()
            self.assertNotEqual(benchmark.tree_manifest(str(dest)), expected)
            (dest / "empty").mkdir()
            (dest / "file").write_bytes(b"y")
            self.assertNotEqual(benchmark.tree_manifest(str(dest)), expected)


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

    def test_each_ssh_delta_sample_resets_basis_without_network(self):
        scratch = []

        def ssh_on_local_peer(target, args):
            self.assertEqual(target, "test-peer")
            result = subprocess.run(args, capture_output=True, text=True, check=True)
            if args[0] == "mktemp":
                scratch.append(Path(result.stdout.strip()))
            return result

        # Local semantics are covered with real sy/rsync above. Isolate SSH only:
        # peer cp/reset/hash/cleanup still run as real local commands, no network.
        with patch.object(benchmark, "ssh_command", side_effect=ssh_on_local_peer):
            results = self.scenario(transport="ssh", ssh_target="test-peer")
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

    def test_ctime_only_refresh_with_public_zero_proof_passes(self):
        with patch.dict(os.environ, {"HARNESS_METADATA": "1"}):
            results = self.scenario()
        unchanged = [r for r in results if r.operation == "incremental"]
        self.assertEqual(len(unchanged), 2)
        for result in unchanged:
            self.assertIsNone(result.error)
            self.assertEqual(result.bytes_total, 0)
            self.assertTrue(all(result.file_changes))
            for changes in result.file_changes:
                for change in changes.values():
                    self.assertEqual(change["before"][:4], change["after"][:4])
                    self.assertNotEqual(change["before"][4], change["after"][4])
            self.assertEqual(
                [p.status for p in result.transfer_evidence], ["zero_file_payload"] * 3
            )

    def test_identical_byte_recopy_is_not_mistaken_for_zero_payload(self):
        # Exact hashes still match: only evidence from the timed invocation
        # exposes this transfer. Protects against simply dropping ctime checks.
        with patch.dict(os.environ, {"HARNESS_FORCE_COPY": "1"}):
            results = self.scenario()
        for result in results:
            if result.operation == "incremental":
                self.assertEqual(result.error_kind, "file_transfer")
                self.assertIsNone(result.bytes_total)
                self.assertEqual(result.transfer_evidence[0].status, "file_transfer")
            else:
                self.assertIsNone(result.error)

    def test_unavailable_proof_is_not_zero_and_does_not_disable_delta(self):
        with patch.dict(os.environ, {"HARNESS_NO_PROOF": "1"}):
            results = self.scenario()
        self.assertEqual(len(results), 6)
        for result in results:
            if result.operation == "incremental":
                self.assertEqual(result.error_kind, "proof_unavailable")
                self.assertIsNone(result.bytes_total)
            else:
                self.assertIsNone(result.error)

    def test_transfer_interface_validation_is_not_incidental_text_parsing(self):
        expected = {"file": [1, "digest"], "empty": None}
        for stdout in (
            "",
            '{"type":"summary"}',
            '{"type":"summary","bytes_transferred":false}',
            '{"type":"summary","bytes_transferred":-1}',
            '{"type":"summary","bytes_transferred":0}\n{"type":"skip"}',
            (
                '{"type":"summary","bytes_transferred":0}\n'
                '{"type":"summary","bytes_transferred":0}'
            ),
            "Transferred: 0 bytes",
        ):
            with self.subTest(tool="sy", stdout=stdout):
                self.assertEqual(
                    benchmark.transfer_evidence("sy", stdout, expected).status,
                    "proof_unavailable",
                )
        valid = "SYBENCH|.d       |0|./\nSYBENCH|.d       |0|empty/\n"
        for record, status in (
            ("SYBENCH|.f...p...|0|file", "zero_file_payload"),
            ("SYBENCH|.f...p.....|0|file", "zero_file_payload"),
            ("SYBENCH|>f.......|51|file", "file_transfer"),
            ("SYBENCH|<f.........|0|file", "file_transfer"),
            ("SYBENCH|.f...p...|51|file", "zero_file_payload"),
            ("SYBENCH|.f%broken!|0|file", "proof_unavailable"),
            ("SYBENCH|%i|%b|file", "proof_unavailable"),
            ("", "proof_unavailable"),
        ):
            with self.subTest(tool="rsync", record=record):
                self.assertEqual(
                    benchmark.transfer_evidence(
                        "rsync", valid + record, expected
                    ).status,
                    status,
                )
        self.assertEqual(
            benchmark.transfer_evidence(
                "rsync",
                valid + "SYBENCH|.f       |0|file\nSYBENCH|.f       |0|file",
                expected,
            ).status,
            "proof_unavailable",
        )
        # Missing coverage or unrelated malformed output cannot erase a
        # validated positive transfer from this invocation.
        for stdout in (
            "SYBENCH|>f.......|51|file",
            "unsupported record\nSYBENCH|>f.......|51|file",
            "SYBENCH|>f.......|51|file\nunsupported record",
        ):
            with self.subTest(stdout=stdout):
                self.assertEqual(
                    benchmark.transfer_evidence("rsync", stdout, expected).status,
                    "file_transfer",
                )

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
        rsync_artifact = run["ver"]["rsync_binary"]
        self.assertEqual(rsync_artifact["path"], str(self.rsync.resolve()))
        self.assertEqual(
            rsync_artifact["sha256"], hashlib.sha256(self.rsync.read_bytes()).hexdigest()
        )
        self.assertEqual(rsync_artifact["version"], "sy harness-test-artifact")
        self.assertTrue(all(result["err"] is None for result in run["results"]))
        for result in run["results"]:
            if result["op"] == "incremental":
                self.assertEqual(result["bytes"], 0)
                self.assertIsNone(result["error_kind"])
                self.assertEqual(
                    result["transfer_evidence"][0]["status"], "zero_file_payload"
                )
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
