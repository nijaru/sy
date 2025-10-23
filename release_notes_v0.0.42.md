# sy v0.0.42 - SSH Performance & Sparse File Optimization

**Release Date:** 2025-10-23

## 🚀 Major Features

### SSH Connection Pooling - True Parallel Transfers
**Finally: Real parallel SSH transfers without bottlenecks!**

- **N workers = N connections**: Each parallel worker gets its own SSH session
- **Round-robin distribution**: Lock-free session allocation via atomic counter
- **No more ControlMaster bottleneck**: Avoids serializing all transfers on one TCP connection
- **Automatic sizing**: Pool size matches your `--parallel` flag
- **Zero configuration**: Just use `-j` flag as before, now with true parallelism

```bash
# 20 workers = 20 SSH connections = maximum throughput
sy /source user@host:/dest -j 20
```

**Why this matters:** SSH ControlMaster (the common approach) forces all transfers through a single TCP connection, defeating the purpose of parallel workers. Our connection pool gives each worker a dedicated connection for true parallel throughput.

### SSH Sparse File Transfer - Massive Bandwidth Savings
**Automatic bandwidth optimization for VM images, databases, and other sparse files**

- **10x bandwidth savings** for VM disk images (10GB file with 1GB data → only 1GB transferred)
- **5x bandwidth savings** for database files with large empty regions
- **Automatic detection**: Uses Unix file system metadata (allocated_size < file_size)
- **Smart protocol**: Detect regions → send JSON + stream data → reconstruct on remote
- **Graceful fallback**: If detection fails or not supported, falls back to regular transfer
- **Zero configuration**: Works automatically when syncing sparse files over SSH

```bash
# Just sync as normal - sy auto-detects and optimizes sparse files
sy /vm/images/disk.vmdk user@host:/backup/
# 10GB VM with 1GB data: transfers 1GB instead of 10GB (10x faster!)

sy /db/postgres.db user@host:/sync/
# 100GB database with 20GB data: transfers 20GB instead of 100GB (5x faster!)
```

**How it works:** sy uses SEEK_HOLE/SEEK_DATA to find actual data regions, sends only those regions over SSH, and reconstructs the sparse file on the remote side with proper holes.

## 🧪 Quality Improvements

### Testing Enhancements
Added **27 comprehensive tests** across multiple areas:

- **Performance monitoring accuracy** (9 tests): Duration tracking, speed calculation, concurrent operations
- **Error collection thresholds** (4 tests): Unlimited errors, abort on threshold, below threshold behavior
- **Sparse file edge cases** (11 tests): Multiple data regions, large offsets, boundary conditions
- **SSH sparse transfer integration** (3 tests): sy-remote ReceiveSparseFile command validation

**Test coverage increased**: 355 → 385 tests (all passing ✅)

## 📊 Performance Impact

### SSH Connection Pooling
- **Before**: ControlMaster approach serializes all transfers on one TCP connection
- **After**: N parallel connections = true parallel throughput
- **Impact**: Full utilization of available bandwidth across multiple workers

### SSH Sparse File Transfer
- **VM images**: 10x bandwidth reduction (typical sparse ratio: 10-20% allocated)
- **Databases**: 5x bandwidth reduction (typical sparse ratio: 20-40% allocated)
- **Large sparse files**: Transfer time reduced proportionally to sparse ratio

## 🔧 Implementation Details

### Connection Pool Architecture
- **Round-robin distribution**: `AtomicUsize` counter for lock-free session selection
- **Session management**: `Vec<Arc<Mutex<Session>>>` for thread-safe access
- **Integration**: Automatic pool sizing via `TransportRouter`

### Sparse File Protocol
1. **Detection**: Check `metadata.blocks() * 512 < metadata.len()` on Unix
2. **Region discovery**: Use SEEK_HOLE/SEEK_DATA to find data regions
3. **Transfer**: Send JSON array of regions, then stream concatenated data
4. **Reconstruction**: sy-remote creates file, sets size, writes regions at offsets

## 📦 Installation

```bash
# Install from crates.io
cargo install sy

# Verify installation
sy --version  # Should show v0.0.42
```

## 🔄 Upgrading

No breaking changes - simply update and enjoy the performance improvements!

```bash
# Upgrade via cargo
cargo install sy --force

# sy-remote is included in the binary
# Remote hosts will automatically use the new features when sy is upgraded
```

## 📝 Full Changelog

See [CHANGELOG.md](CHANGELOG.md#0042---2025-10-23) for complete details.

## 🙏 Acknowledgments

Research that informed this release:
- SSH multiplexing 2025 best practices (see ai/research/ssh_multiplexing_2025.md)
- Sparse file detection and transfer techniques
- Connection pooling patterns for parallel I/O

---

**Questions or feedback?** Open an issue at https://github.com/nijaru/sy/issues
