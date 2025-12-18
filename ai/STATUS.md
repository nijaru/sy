# Status

## Current State

| Metric  | Value        | Updated    |
| ------- | ------------ | ---------- |
| Version | v0.1.2       | 2025-11-27 |
| Tests   | 620+ passing | 2025-11-27 |
| Build   | 🟢 PASSING   | 2025-11-27 |

## What Worked

- Bidirectional server mode (74f7c35): Push + pull over SSH using binary protocol
- Delta sync optimization: 2x faster than rsync for partial updates
- Protocol fix (66d05d5): Always send MKDIR_BATCH even if empty
- Adaptive block sizes: 2KB→64KB based on file size

## What Didn't Work

- Initial pull mode protocol: Client expected MKDIR_BATCH but server only sent it when directories existed → Fixed by always sending MKDIR_BATCH

## Active Work

**Benchmark infrastructure created** (2025-12-18): `scripts/benchmark.py` with JSONL history tracking.

```bash
python scripts/benchmark.py --quick      # Smoke test
python scripts/benchmark.py --ssh user@host  # SSH benchmark
python scripts/benchmark.py --history    # View results
```

**SSH bottlenecks identified**:
| Issue | Location | Fix |
|-------|----------|-----|
| Sequential delta | server_mode.rs:203-306 | Pipeline checksum requests |
| No stream compression | protocol.rs | Wrap channel in zstd |
| Full file in memory | server_mode.rs:153 | Streaming read/write |

**Community request**: [Issue #12](https://github.com/nijaru/sy/issues/12) - `--one-file-system`, SSH args, `--numeric-ids`

Tasks: `bd ready` or `bd list`

## Blockers

| Blocker                    | Impact                      |
| -------------------------- | --------------------------- |
| SSH benchmarks not yet run | Need data before optimizing |

## Recent Releases

### v0.1.2 (2025-11-27)

- Bidirectional server mode (push + pull)
- Delta sync 2x faster than rsync
- Removed ~300 lines dead bulk_transfer code

### v0.1.1 (2025-11-26)

- Batch destination scanning (~1000x fewer SSH round-trips)
- Planning phase: 90 min → 30 sec for 531K files

### v0.1.0 (2025-11-25)

- Breaking: rsync-compatible defaults
- New flags: `--gitignore`, `--exclude-vcs`, `-u/--update`

## Feature Flags

| Flag  | Default  | Notes             |
| ----- | -------- | ----------------- |
| SSH   | Enabled  | ssh2 (libssh2)    |
| Watch | Disabled | File watching     |
| ACL   | Disabled | Linux: libacl-dev |
| S3    | Disabled | Experimental      |
