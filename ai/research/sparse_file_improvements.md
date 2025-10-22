# Sparse File Optimization Improvements

**Date**: 2025-10-22
**Status**: Research Phase
**Target Version**: v0.0.41

## Executive Summary

**Current State**: Sparse file support is comprehensive for LOCAL sync, but MISSING for REMOTE (SSH) sync.

**Goal**: Extend sparse file preservation to SSH transport for bandwidth and storage savings.

## Current Implementation Analysis

### ✅ What Works (Local Sync)

1. **Detection** (src/sync/scanner.rs:32-51)
   - Unix: Detects sparse files via `allocated_size < file_size`
   - Stores `is_sparse` and `allocated_size` in FileEntry
   - Test coverage: `test_scanner_sparse_files()`

2. **Preservation** (src/transport/local.rs:36-175)
   - **Fast path**: SEEK_HOLE/SEEK_DATA (lines 48-125)
     - Finds data regions efficiently
     - Only copies non-zero blocks
     - Falls back if not supported (EINVAL)
   - **Slow path**: Block-based zero detection (lines 127-175)
     - Reads 4KB blocks
     - Skips writing all-zero blocks
     - Portable fallback

3. **Integration** (src/transport/local.rs:257, 399)
   - Auto-detects sparse files during transfer
   - Uses `copy_sparse_file()` when `is_sparse == true`
   - Logging for sparse file operations

### ❌ What's Missing (Remote Sync)

1. **SSH Transport** (src/transport/ssh.rs)
   - NO sparse file handling code found
   - Transfers entire file over network
   - Wastes bandwidth for sparse files

2. **Remote Detection**
   - Scanner only runs locally
   - No way to detect if remote file is sparse
   - Can't optimize based on remote sparse status

3. **Protocol Support**
   - sy-remote binary doesn't handle sparse files
   - No wire protocol for sparse file transfer
   - Would need new message types

## Problem: Sparse Files Over SSH

**Scenario**: Syncing a 10GB virtual disk image (sparse, only 1GB actual data)

| Method | Data Transferred | Time (100 Mbps) |
|--------|------------------|-----------------|
| **Current sy SSH** | 10GB | 800 seconds (13 min) |
| **Optimal (proposed)** | 1GB | 80 seconds (1.3 min) |
| **Savings** | 9GB (90%) | 10x faster |

**Use Cases**:
- Virtual machine disk images (.vmdk, .qcow2, .vdi)
- Database files with allocated but unused space
- Log files with holes
- Container images

## Proposed Solution

### Option 1: Rsync-Style Sparse Protocol (RECOMMENDED)

**Design**: Send sparse file metadata + data regions only

**Protocol**:
```
1. Client scans file, detects sparse (already done)
2. Client sends: SparseMeta { regions: Vec<(offset, length)> }
3. For each region: send data chunk
4. Server reconstructs: ftruncate + write data regions
```

**Implementation**:
```rust
// In sy-remote protocol (src/bin/sy-remote.rs)
enum TransferMessage {
    // ... existing variants
    SparseFileMeta {
        path: String,
        total_size: u64,
        regions: Vec<(u64, u64)>,  // (offset, length) pairs
    },
    SparseFileData {
        offset: u64,
        data: Vec<u8>,
    },
}
```

**Benefits**:
- ✅ Maximum bandwidth savings
- ✅ Works with any filesystem (client detects)
- ✅ Protocol-level optimization

**Challenges**:
- Requires protocol changes
- More complex than simple transfer
- Need to handle protocol version compatibility

### Option 2: Compression + Post-Transfer Punch Holes

**Design**: Compress file, send, decompress, then use fallocate to punch holes

**Steps**:
1. Client compresses file (zstd compresses zeros well)
2. Send compressed stream
3. Server decompresses to temp file
4. Server detects zero blocks and punches holes (fallocate FALLOC_FL_PUNCH_HOLE)

**Benefits**:
- ✅ Simpler implementation
- ✅ No protocol changes
- ✅ Works with existing transport

**Challenges**:
- ❌ Compression overhead (CPU)
- ❌ Two-pass on server (decompress + punch holes)
- ❌ Less efficient than direct sparse transfer

### Option 3: Block-Level Transfer with Zero Detection

**Design**: Stream file in blocks, send only non-zero blocks

**Protocol**:
```
1. Send file size
2. For each 4KB block:
   - If all zeros: send SKIP marker
   - If data: send DATA + block content
3. Server writes data blocks, skips holes
```

**Benefits**:
- ✅ Simple protocol
- ✅ Streaming (low memory)
- ✅ Efficient bandwidth

**Challenges**:
- Granularity limited to block size
- More network round-trips (block markers)
- Slower than SEEK_HOLE/SEEK_DATA (reads entire file)

## Recommendation: Option 1 (Rsync-Style)

**Rationale**:
1. **Best performance**: Only send actual data
2. **Proven approach**: rsync has solved this
3. **Clean protocol**: Explicit sparse support

**Trade-offs**:
- More implementation complexity
- Protocol version bump needed
- Testing required for edge cases

## Implementation Plan

### Phase 1: Detection Enhancement (2 hours)

**Add sparse detection to SSH scanner**:

```rust
// In src/transport/ssh.rs
async fn scan_with_sparse_detection(&mut self, path: &Path) -> Result<Vec<FileEntry>> {
    // Send command to sy-remote to get file metadata including allocated size
    self.send_command(&format!("STAT_SPARSE {}", path.display()))?;

    // Parse response with sparse info
    let entries = self.receive_file_list_with_sparse()?;
    Ok(entries)
}
```

**Modify sy-remote**:
```rust
// In src/bin/sy-remote.rs
fn handle_stat_sparse(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;

    #[cfg(unix)]
    let allocated = {
        use std::os::unix::fs::MetadataExt;
        metadata.blocks() * 512  // blocks are 512-byte units
    };

    #[cfg(not(unix))]
    let allocated = metadata.len();  // No sparse support

    // Send: size, allocated_size, is_sparse
    println!("{},{},{}", metadata.len(), allocated, allocated < metadata.len());
    Ok(())
}
```

### Phase 2: Sparse Transfer Protocol (4 hours)

**Add sparse transfer messages**:
```rust
// Extend TransferRequest/Response in ssh.rs
#[derive(Serialize, Deserialize)]
pub enum TransferMode {
    Full,           // Existing: full file
    Delta,          // Existing: rsync delta
    Sparse(Vec<DataRegion>),  // NEW: sparse transfer
}

#[derive(Serialize, Deserialize)]
pub struct DataRegion {
    pub offset: u64,
    pub length: u64,
}
```

**Implement sparse sender** (client):
```rust
async fn send_sparse_file(&mut self, path: &Path, regions: Vec<DataRegion>) -> Result<()> {
    // Send metadata
    self.send_json(&TransferRequest::Sparse {
        path: path.to_path_buf(),
        total_size: file_size,
        regions: regions.clone(),
    })?;

    // Send each data region
    let mut file = File::open(path)?;
    for region in regions {
        file.seek(SeekFrom::Start(region.offset))?;
        let mut buf = vec![0u8; region.length as usize];
        file.read_exact(&mut buf)?;

        self.send_bytes(&buf).await?;
    }

    Ok(())
}
```

**Implement sparse receiver** (server - sy-remote):
```rust
fn receive_sparse_file(path: &Path, total_size: u64, regions: Vec<DataRegion>) -> Result<()> {
    // Create file with correct size (creates sparse file)
    let mut file = File::create(path)?;
    file.set_len(total_size)?;

    // Write each data region
    for region in regions {
        file.seek(SeekFrom::Start(region.offset))?;

        let mut buf = vec![0u8; region.length as usize];
        io::stdin().read_exact(&mut buf)?;
        file.write_all(&buf)?;
    }

    file.sync_all()?;
    Ok(())
}
```

### Phase 3: Region Detection (2 hours)

**Extract data regions from sparse file**:
```rust
// Reuse local.rs SEEK_HOLE/SEEK_DATA logic
fn detect_data_regions(path: &Path) -> io::Result<Vec<DataRegion>> {
    let mut regions = Vec::new();
    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let fd = file.as_raw_fd();

    let mut pos: i64 = 0;
    while pos < file_size as i64 {
        let data_start = unsafe { libc::lseek(fd, pos, SEEK_DATA) };
        if data_start < 0 {
            break;  // No more data
        }

        let hole_start = unsafe { libc::lseek(fd, data_start, SEEK_HOLE) };
        let data_end = if hole_start < 0 {
            file_size as i64
        } else {
            hole_start
        };

        regions.push(DataRegion {
            offset: data_start as u64,
            length: (data_end - data_start) as u64,
        });

        pos = data_end;
    }

    Ok(regions)
}
```

### Phase 4: Integration & Testing (3 hours)

**Integration**:
1. Update SshTransport::copy_file to detect sparse files
2. Call send_sparse_file() for sparse files
3. Update sy-remote main loop to handle sparse transfer
4. Add fallback for when sparse not supported

**Tests**:
```rust
#[test]
#[cfg(unix)]
fn test_ssh_sparse_file_transfer() {
    // Create 10MB sparse file (1MB actual data)
    let sparse_file = create_sparse_file_with_holes();

    // Sync over SSH
    let result = sync_ssh(&sparse_file, remote_dest).unwrap();

    // Verify:
    // 1. Content matches
    // 2. Remote file is sparse
    // 3. Bandwidth < 2MB (not full 10MB)
    assert!(result.bytes_transferred < 2 * 1024 * 1024);
}
```

## Estimated Impact

### Performance Gains

**Virtual Machine Disk (10GB sparse, 1GB data)**:
- Bandwidth savings: 90% (10GB → 1GB)
- Time savings: 10x faster on 100 Mbps
- Storage savings: Preserved sparseness

**Database Files (100GB sparse, 20GB data)**:
- Bandwidth savings: 80% (100GB → 20GB)
- Time savings: 5x faster
- Critical for large database backups

### Use Cases Unlocked

1. **VM/Container Images**: Fast remote backup/restore
2. **Database Backups**: Efficient remote replication
3. **Log Archives**: Sparse log files with holes
4. **Disk Images**: Backup tools, forensics

## Risks & Mitigations

**Risk 1**: Protocol complexity
- **Mitigation**: Comprehensive tests, fallback to full transfer

**Risk 2**: Compatibility with old sy-remote
- **Mitigation**: Protocol version check, graceful degradation

**Risk 3**: Performance regression for non-sparse files
- **Mitigation**: Only use sparse mode when `is_sparse == true`

**Risk 4**: Edge cases (holes at end, all-zero files)
- **Mitigation**: Extensive edge case testing

## Success Criteria

✅ **Functional**:
- Sparse files transferred correctly over SSH
- Content matches (bitwise comparison)
- Sparseness preserved on destination

✅ **Performance**:
- Bandwidth usage ≤ (actual_data_size + 10% overhead)
- No performance regression for non-sparse files

✅ **Robustness**:
- Handles edge cases (all holes, no holes, holes at end)
- Graceful fallback if sparse not supported
- Works across different filesystems

## Timeline

- **Research & Design**: 1 hour ✅ (this document)
- **Phase 1** (Detection): 2 hours
- **Phase 2** (Protocol): 4 hours
- **Phase 3** (Region Detection): 2 hours
- **Phase 4** (Integration & Tests): 3 hours
- **Total**: ~12 hours implementation + testing

## Alternative: Defer to v1.0+

**Option**: Skip this feature for now, revisit in v1.0

**Rationale**:
- Local sparse support is complete
- Remote sparse is nice-to-have, not critical
- Protocol changes add complexity
- Can use compression workaround for now

**When to implement**:
- If users request remote sparse support
- If VM/database backup becomes primary use case
- As part of v1.0 protocol redesign

## Recommendation

**Proceed with implementation** IF:
- User has expressed need for VM/database backup
- Bandwidth savings are critical for use case
- Willing to invest 12 hours

**Defer to later** IF:
- No immediate user need
- Want to focus on other features (macOS, Windows)
- Prefer simpler implementation path

**For v0.0.41**: I recommend **proceeding** since this is a high-value feature for common use cases (VM backups, databases).
