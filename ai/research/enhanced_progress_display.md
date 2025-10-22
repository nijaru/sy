# Enhanced Progress Display Research

**Date**: 2025-10-22
**Status**: Design Phase
**Version**: v0.0.38 (planned)

## Current State

**Progress Bar Template**:
```rust
"{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}"
```

**Displays**:
- Spinner (activity indicator)
- Progress bar (40 chars wide)
- File count (5/100 files)
- ETA based on file count
- Message (action + filename)

**Missing**:
- Bytes transferred vs total bytes
- Real-time transfer speed
- Dedicated current file display
- Bandwidth utilization percentage

## Research Findings

### Indicatif Capabilities (2025)

**Byte-Based Templates**:
- `{bytes}` - Bytes transferred so far
- `{total_bytes}` - Total bytes to transfer
- `{bytes_per_sec}` - Current transfer speed
- `{eta}` / `{eta_precise}` - Time remaining (auto-calculated from bytes)
- `{elapsed_precise}` - Elapsed time

**Example Template**:
```rust
"{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})"
```

**Multi-Line Support**:
- Use `\n` in template for multi-line display
- `{msg}` can show current file on separate line
- `{wide_bar}` adapts to terminal width

## Design Decision

### Enhanced Template (Proposed)

**Two-line display**:
```
Transferring: /path/to/current/file.txt
[=====>     ] 2.4 GB/10.5 GB (245 MB/s, 33s remaining)
```

**Template**:
```rust
"{msg}\n{spinner:.green} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})"
```

**With bandwidth limit**:
```
Transferring: /path/to/current/file.txt (Bandwidth: 87% of 100MB/s limit)
[=====>     ] 2.4 GB/10.5 GB (87 MB/s, 1m 35s)
```

### Implementation Strategy

**Step 1: Calculate Total Bytes**
```rust
// During planning phase, sum up all file sizes
let total_bytes: u64 = tasks.iter()
    .filter(|t| !matches!(t.action, SyncAction::Skip | SyncAction::Delete))
    .map(|t| t.source_file.as_ref().map(|f| f.size).unwrap_or(0))
    .sum();

// Create progress bar with byte-based length
let pb = ProgressBar::new(total_bytes);
```

**Step 2: Enhanced Template**
```rust
pb.set_style(
    ProgressStyle::default_bar()
        .template("{msg}\n{spinner:.green} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})")
        .unwrap()
        .progress_chars("#>-"),
);
```

**Step 3: Update Progress with Bytes**
```rust
// After each file transfer
pb.inc(bytes_written);
pb.set_message(format!("Transferring: {}", current_file));

// With bandwidth utilization (if bwlimit set)
if let Some(limit) = bwlimit {
    let current_speed = ...; // Calculate from pb.per_sec()
    let utilization = (current_speed / limit as f64) * 100.0;
    pb.set_message(format!(
        "Transferring: {} (Bandwidth: {:.0}% of {} limit)",
        current_file,
        utilization,
        format_speed(limit)
    ));
}
```

**Step 4: Handle Edge Cases**
- Zero-byte files (don't affect total)
- Directories (skip in total calculation)
- Skipped files (don't affect progress)
- Delta sync (only count bytes actually transferred)
- Compression (count compressed bytes, not uncompressed)

## Benefits

**User Experience**:
1. **Better ETA**: Based on bytes, not file count (more accurate for mixed sizes)
2. **Transfer Speed**: See real-time MB/s or GB/s
3. **Current File**: Know exactly what's being transferred
4. **Bandwidth Awareness**: See utilization when rate limit is set
5. **Terminal-Adaptive**: Wide bar uses full terminal width

**Technical**:
1. **No Breaking Changes**: Only affects progress display
2. **Indicatif Built-In**: Uses native byte formatting
3. **Accurate Progress**: Reflects actual data transferred
4. **Multi-Line**: Cleaner separation of info

## Alternative Approaches Considered

### Option 1: Keep File Count, Add Bytes Below
```
Files: [====>  ] 45/100 (45%)
Bytes: [=====> ] 2.4 GB/10.5 GB (245 MB/s, 33s)
```
- ❌ Too verbose (3 lines)
- ❌ Redundant ETAs (confusing)
- ✅ Shows both metrics

### Option 2: Hybrid (Files + Bytes in One Line)
```
[=====> ] 45/100 files | 2.4/10.5 GB (245 MB/s, 33s)
```
- ❌ Cramped display
- ❌ Hard to read on narrow terminals
- ✅ Single line

### Option 3: Bytes Only (CHOSEN)
```
Transferring: /path/to/file.txt
[=====> ] 2.4 GB/10.5 GB (245 MB/s, 33s)
```
- ✅ Clean, focused display
- ✅ Most accurate ETA
- ✅ Industry standard (rsync, wget, curl use bytes)
- ⚠️ Loses file count (acceptable - can add to final summary)

## Implementation Plan

### Phase 1: Basic Bytes Display
1. Calculate `total_bytes` from tasks
2. Create `ProgressBar::new(total_bytes)`
3. Update template to use `{bytes}`, `{total_bytes}`, `{bytes_per_sec}`
4. Update `pb.inc()` calls to use bytes instead of file count
5. Test with various file sizes

### Phase 2: Current File Display
1. Update message to show current file path
2. Truncate long paths if needed (e.g., "...long/path/file.txt")
3. Handle concurrent transfers (show most recent?)

### Phase 3: Bandwidth Utilization
1. Check if `bwlimit` is set
2. Calculate utilization: (current_speed / limit) * 100
3. Add to message: "Bandwidth: 87% of 100MB/s limit"
4. Color-code: green <80%, yellow 80-95%, red >95%

### Phase 4: Polish
1. Add bytes transferred to final summary
2. Update JSON output to include speed metrics
3. Handle quiet mode (no progress bar, but still track stats)
4. Update tests to verify bytes tracking

## Performance Impact

**Overhead**: Negligible
- Total bytes calculated once during planning
- `pb.inc()` called same number of times (just different values)
- Indicatif handles formatting efficiently

**Benefits**:
- Better UX with no performance cost
- More accurate progress tracking

## Testing Plan

**Test Cases**:
1. Single large file (10GB) - verify speed display
2. Many small files (1000 x 1KB) - verify ETA accuracy
3. Mixed sizes (1GB + 1000 x 1KB) - verify bytes matter more than count
4. With bwlimit (100MB/s) - verify utilization display
5. Narrow terminal (80 cols) - verify wide_bar adapts
6. Very long filenames - verify truncation
7. Zero-byte files - verify total_bytes excludes them
8. Delta sync - verify only transferred bytes counted

## References

1. Indicatif docs: https://docs.rs/indicatif/latest/indicatif/
2. Progress bar examples: https://github.com/console-rs/indicatif/tree/main/examples
3. rsync progress format: `--progress` and `--info=progress2`
4. wget progress: Uses bytes, shows current file, displays speed
5. curl progress: Similar byte-based approach
