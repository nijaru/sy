use super::{Adler32, BlockChecksum};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// A single delta operation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaOp {
    /// Copy block from existing file at given offset
    Copy { offset: u64, size: usize },
    /// Insert literal data
    Data(Vec<u8>),
}

/// Delta instructions for reconstructing a file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delta {
    pub ops: Vec<DeltaOp>,
    #[allow(dead_code)]
    pub source_size: u64,
    #[allow(dead_code)]
    pub block_size: usize,
}

impl Delta {
    /// Calculate compression ratio
    #[allow(dead_code)]
    pub fn compression_ratio(&self) -> f64 {
        let literal_bytes: usize = self
            .ops
            .iter()
            .filter_map(|op| match op {
                DeltaOp::Data(data) => Some(data.len()),
                _ => None,
            })
            .sum();

        let copy_bytes: usize = self
            .ops
            .iter()
            .filter_map(|op| match op {
                DeltaOp::Copy { size, .. } => Some(*size),
                _ => None,
            })
            .sum();

        let total_bytes = literal_bytes + copy_bytes;
        if total_bytes == 0 {
            return 1.0;
        }

        literal_bytes as f64 / total_bytes as f64
    }
}

/// Generate delta operations with streaming (memory-efficient)
///
/// This implements the rsync algorithm with constant memory usage:
/// 1. Build hash table of destination block checksums
/// 2. Read source in chunks (256KB at a time)
/// 3. Slide window through data using rolling hash
/// 4. Generate Copy ops for matches, Data ops for literals
///
/// Memory usage: ~512KB regardless of file size
pub fn generate_delta_streaming(
    source_path: &Path,
    dest_checksums: &[BlockChecksum],
    block_size: usize,
) -> io::Result<Delta> {
    const CHUNK_SIZE: usize = 256 * 1024; // 256KB chunks
    const MAX_LITERAL_SIZE: usize = 1024 * 1024; // Flush literals at 1MB to bound memory

    // Build hash map for O(1) lookup
    let mut checksum_map: HashMap<u32, Vec<&BlockChecksum>> = HashMap::new();
    for checksum in dest_checksums {
        checksum_map
            .entry(checksum.weak)
            .or_default()
            .push(checksum);
    }

    let mut source_file = File::open(source_path)?;
    let source_size = source_file.metadata()?.len();

    if source_size == 0 {
        return Ok(Delta {
            ops: vec![],
            source_size: 0,
            block_size,
        });
    }

    let mut ops = Vec::new();
    let mut literal_buffer = Vec::new();

    // Sliding window buffer: large enough for rolling hash + read ahead
    let mut window = Vec::with_capacity(block_size + CHUNK_SIZE);
    let mut chunk_buf = vec![0u8; CHUNK_SIZE];

    // Read initial chunk
    let mut bytes_read = source_file.read(&mut chunk_buf)?;
    if bytes_read > 0 {
        window.extend_from_slice(&chunk_buf[..bytes_read]);
    }

    // Initialize rolling hash
    let mut rolling = Adler32::new(block_size);
    if window.len() >= block_size {
        rolling.update_block(&window[0..block_size]);
    }

    let mut window_pos = 0; // Position within window
    let mut _file_pos = 0u64; // Absolute position in file (for debugging)

    while window_pos < window.len() {
        let remaining = window.len() - window_pos;
        let mut found_match = false;

        // Try to match full blocks
        if remaining >= block_size {
            let weak = rolling.digest();

            if let Some(candidates) = checksum_map.get(&weak) {
                let block = &window[window_pos..window_pos + block_size];

                // Verify with strong hash
                let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                hasher.update(block);
                let strong = hasher.digest();

                for checksum in candidates {
                    if checksum.strong == strong {
                        // Match found! Flush literals and add Copy
                        if !literal_buffer.is_empty() {
                            ops.push(DeltaOp::Data(std::mem::take(&mut literal_buffer)));
                        }

                        ops.push(DeltaOp::Copy {
                            offset: checksum.offset,
                            size: checksum.size,
                        });

                        window_pos += block_size;
                        _file_pos += block_size as u64;
                        found_match = true;

                        // Re-initialize rolling hash at new position
                        if window_pos + block_size <= window.len() {
                            rolling.update_block(&window[window_pos..window_pos + block_size]);
                        }
                        break;
                    }
                }
            }
        } else if remaining > 0 {
            // Partial block at end
            let partial = &window[window_pos..];
            let weak = Adler32::hash(partial);

            if let Some(candidates) = checksum_map.get(&weak) {
                let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                hasher.update(partial);
                let strong = hasher.digest();

                for checksum in candidates {
                    if checksum.size == partial.len() && checksum.strong == strong {
                        if !literal_buffer.is_empty() {
                            ops.push(DeltaOp::Data(std::mem::take(&mut literal_buffer)));
                        }

                        ops.push(DeltaOp::Copy {
                            offset: checksum.offset,
                            size: checksum.size,
                        });

                        window_pos += partial.len();
                        _file_pos += partial.len() as u64;
                        found_match = true;
                        break;
                    }
                }
            }
        }

        if !found_match && window_pos < window.len() {
            // No match - add byte to literal buffer
            literal_buffer.push(window[window_pos]);

            // Periodic flush to bound memory usage for files with few matches
            if literal_buffer.len() >= MAX_LITERAL_SIZE {
                ops.push(DeltaOp::Data(std::mem::take(&mut literal_buffer)));
            }

            // Update rolling hash for next position
            if window_pos + block_size < window.len() {
                rolling.roll(window[window_pos], window[window_pos + block_size]);
            }

            window_pos += 1;
            _file_pos += 1;
        }

        // Refill window when needed
        if window_pos >= block_size && bytes_read > 0 && window.len() - window_pos < block_size {
            // Shift window: remove processed bytes
            window.drain(0..window_pos);
            window_pos = 0;

            // Read more data
            bytes_read = source_file.read(&mut chunk_buf)?;
            if bytes_read > 0 {
                window.extend_from_slice(&chunk_buf[..bytes_read]);

                // Re-initialize rolling hash if we have enough data
                if window.len() >= block_size {
                    rolling.update_block(&window[0..block_size]);
                }
            }
        }
    }

    // Flush remaining literals
    if !literal_buffer.is_empty() {
        ops.push(DeltaOp::Data(literal_buffer));
    }

    Ok(Delta {
        ops,
        source_size,
        block_size,
    })
}

/// Generate delta operations by comparing source file against destination checksums
/// (legacy non-streaming version - loads entire file into memory)
///
/// This implements the rsync algorithm:
/// 1. Build hash table of destination block checksums
/// 2. Slide window through source file using rolling hash
/// 3. When weak hash matches, verify with strong hash
/// 4. Generate Copy ops for matches, Data ops for literals
///
/// Note: This loads entire source file into memory. For large files, use
/// `generate_delta_streaming` instead.
#[allow(dead_code)]
pub fn generate_delta(
    source_path: &Path,
    dest_checksums: &[BlockChecksum],
    block_size: usize,
) -> io::Result<Delta> {
    // Build hash map for O(1) lookup of weak checksums
    let mut checksum_map: HashMap<u32, Vec<&BlockChecksum>> = HashMap::new();
    for checksum in dest_checksums {
        checksum_map
            .entry(checksum.weak)
            .or_default()
            .push(checksum);
    }

    // Read source file
    let mut source_file = File::open(source_path)?;
    let mut source_data = Vec::new();
    source_file.read_to_end(&mut source_data)?;
    let source_size = source_data.len() as u64;

    if source_data.is_empty() {
        return Ok(Delta {
            ops: vec![],
            source_size: 0,
            block_size,
        });
    }

    let mut ops = Vec::new();
    let mut literal_buffer = Vec::new();
    let mut pos = 0;

    // Initialize rolling hash with first block
    let mut rolling = Adler32::new(block_size);
    if source_data.len() >= block_size {
        rolling.update_block(&source_data[0..block_size]);
    }

    while pos < source_data.len() {
        let mut found_match = false;
        let remaining = source_data.len() - pos;

        // Try to match full blocks first
        if remaining >= block_size {
            let weak = rolling.digest();

            // Check if weak hash matches any destination blocks
            if let Some(candidates) = checksum_map.get(&weak) {
                let block = &source_data[pos..pos + block_size];

                // Verify with strong hash (xxHash3)
                let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                hasher.update(block);
                let strong = hasher.digest();

                // Find exact match
                for checksum in candidates {
                    if checksum.strong == strong {
                        // Found a match!
                        // First, flush any accumulated literal data
                        if !literal_buffer.is_empty() {
                            ops.push(DeltaOp::Data(literal_buffer.clone()));
                            literal_buffer.clear();
                        }

                        // Add copy operation
                        ops.push(DeltaOp::Copy {
                            offset: checksum.offset,
                            size: checksum.size,
                        });

                        pos += block_size;
                        found_match = true;

                        // Update rolling hash for next position (if there is one)
                        if pos + block_size <= source_data.len() {
                            rolling.update_block(&source_data[pos..pos + block_size]);
                        }
                        break;
                    }
                }
            }
        } else {
            // Partial block at end - try to match against partial blocks in dest
            let partial_block = &source_data[pos..];
            let weak = Adler32::hash(partial_block);

            if let Some(candidates) = checksum_map.get(&weak) {
                let mut hasher = xxhash_rust::xxh3::Xxh3::new();
                hasher.update(partial_block);
                let strong = hasher.digest();

                for checksum in candidates {
                    if checksum.size == partial_block.len() && checksum.strong == strong {
                        // Found matching partial block!
                        if !literal_buffer.is_empty() {
                            ops.push(DeltaOp::Data(literal_buffer.clone()));
                            literal_buffer.clear();
                        }

                        ops.push(DeltaOp::Copy {
                            offset: checksum.offset,
                            size: checksum.size,
                        });

                        pos += partial_block.len();
                        found_match = true;
                        break;
                    }
                }
            }
        }

        if !found_match {
            // No match found - add byte to literal buffer
            literal_buffer.push(source_data[pos]);
            pos += 1;

            // Update rolling hash
            if pos > 0 && pos + block_size - 1 < source_data.len() {
                let old_byte = source_data[pos - 1];
                let new_byte = source_data[pos + block_size - 1];
                rolling.roll(old_byte, new_byte);
            }
        }
    }

    // Flush remaining literal data
    if !literal_buffer.is_empty() {
        ops.push(DeltaOp::Data(literal_buffer));
    }

    Ok(Delta {
        ops,
        source_size,
        block_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::compute_checksums;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_delta_identical_files() {
        // Create two identical files
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        let data = b"Hello, World! This is a test.";
        source.write_all(data).unwrap();
        dest.write_all(data).unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        // Compute checksums and delta
        let checksums = compute_checksums(dest.path(), 8).unwrap();
        let delta = generate_delta(source.path(), &checksums, 8).unwrap();

        // Should have only Copy operations
        assert!(delta
            .ops
            .iter()
            .all(|op| matches!(op, DeltaOp::Copy { .. })));

        // Compression ratio should be 0 (no literal data)
        assert_eq!(delta.compression_ratio(), 0.0);
    }

    #[test]
    fn test_delta_completely_different() {
        // Create two completely different files
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        source.write_all(b"AAAAAAAA").unwrap();
        dest.write_all(b"BBBBBBBB").unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        // Compute checksums and delta
        let checksums = compute_checksums(dest.path(), 4).unwrap();
        let delta = generate_delta(source.path(), &checksums, 4).unwrap();

        // Should have only Data operations
        assert!(delta.ops.iter().all(|op| matches!(op, DeltaOp::Data(_))));

        // Compression ratio should be 1.0 (all literal data)
        assert_eq!(delta.compression_ratio(), 1.0);
    }

    #[test]
    fn test_delta_partial_match() {
        // Source: "AAAABBBBCCCC" (12 bytes)
        // Dest:   "AAAA____CCCC" (12 bytes, middle is different)
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        source.write_all(b"AAAABBBBCCCC").unwrap();
        dest.write_all(b"AAAADDDDCCCC").unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        // Compute checksums and delta
        let block_size = 4;
        let checksums = compute_checksums(dest.path(), block_size).unwrap();
        let delta = generate_delta(source.path(), &checksums, block_size).unwrap();

        // Should have mix of Copy and Data
        let has_copy = delta
            .ops
            .iter()
            .any(|op| matches!(op, DeltaOp::Copy { .. }));
        let has_data = delta.ops.iter().any(|op| matches!(op, DeltaOp::Data(_)));
        assert!(has_copy && has_data);

        // Compression ratio should be between 0 and 1
        let ratio = delta.compression_ratio();
        assert!(ratio > 0.0 && ratio < 1.0);
    }

    #[test]
    fn test_delta_empty_source() {
        let source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        dest.write_all(b"some data").unwrap();
        dest.flush().unwrap();

        let checksums = compute_checksums(dest.path(), 4).unwrap();
        let delta = generate_delta(source.path(), &checksums, 4).unwrap();

        assert_eq!(delta.ops.len(), 0);
        assert_eq!(delta.source_size, 0);
    }

    #[test]
    fn test_delta_empty_dest() {
        let mut source = NamedTempFile::new().unwrap();
        source.write_all(b"some data").unwrap();
        source.flush().unwrap();

        let delta = generate_delta(source.path(), &[], 4).unwrap();

        // Should be all literal data
        assert_eq!(delta.ops.len(), 1);
        assert!(matches!(delta.ops[0], DeltaOp::Data(_)));
        assert_eq!(delta.compression_ratio(), 1.0);
    }

    // Tests for streaming version
    #[test]
    fn test_streaming_identical_files() {
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        let data = b"Hello, World! This is a test.";
        source.write_all(data).unwrap();
        dest.write_all(data).unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        let checksums = compute_checksums(dest.path(), 8).unwrap();
        let delta = generate_delta_streaming(source.path(), &checksums, 8).unwrap();

        // Should have only Copy operations
        assert!(delta
            .ops
            .iter()
            .all(|op| matches!(op, DeltaOp::Copy { .. })));
        assert_eq!(delta.compression_ratio(), 0.0);
    }

    #[test]
    fn test_streaming_large_file() {
        // Test with file larger than CHUNK_SIZE (128KB)
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();

        // Create 256KB of data
        let data = vec![0xAB; 256 * 1024];
        source.write_all(&data).unwrap();
        dest.write_all(&data).unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        let checksums = compute_checksums(dest.path(), 4096).unwrap();
        let delta = generate_delta_streaming(source.path(), &checksums, 4096).unwrap();

        // Should be all Copy operations
        assert!(delta
            .ops
            .iter()
            .all(|op| matches!(op, DeltaOp::Copy { .. })));
        assert_eq!(delta.source_size, 256 * 1024);
    }

    #[test]
    fn test_streaming_vs_nonstreaming_identical() {
        // Verify streaming produces same result as non-streaming
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();

        // Create test data with mix of matches and mismatches
        let source_data = b"AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJ";
        let dest_data = b"AAAABBBBXXXXDDDDEEEEYYYYGGGGHHHHZZZZJJJJ";
        source.write_all(source_data).unwrap();
        dest.write_all(dest_data).unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        let block_size = 4;
        let checksums = compute_checksums(dest.path(), block_size).unwrap();

        let delta1 = generate_delta(source.path(), &checksums, block_size).unwrap();
        let delta2 = generate_delta_streaming(source.path(), &checksums, block_size).unwrap();

        // Both should produce same operations
        assert_eq!(delta1.ops.len(), delta2.ops.len());
        assert_eq!(delta1.source_size, delta2.source_size);
        assert_eq!(delta1.ops, delta2.ops);
    }

    #[test]
    fn test_streaming_window_refill() {
        // Test that window refilling works correctly
        // Create file larger than 2*CHUNK_SIZE to force multiple refills
        let mut source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();

        // Create 512KB file (4 * 128KB chunks)
        let mut data = Vec::with_capacity(512 * 1024);
        for i in 0..512 {
            data.extend_from_slice(&[(i % 256) as u8; 1024]);
        }
        source.write_all(&data).unwrap();
        dest.write_all(&data).unwrap();
        source.flush().unwrap();
        dest.flush().unwrap();

        let block_size = 8192;
        let checksums = compute_checksums(dest.path(), block_size).unwrap();
        let delta = generate_delta_streaming(source.path(), &checksums, block_size).unwrap();

        // Should be all Copy operations
        assert!(delta
            .ops
            .iter()
            .all(|op| matches!(op, DeltaOp::Copy { .. })));
        assert_eq!(delta.source_size, 512 * 1024);
    }

    #[test]
    fn test_streaming_empty_file() {
        let source = NamedTempFile::new().unwrap();
        let mut dest = NamedTempFile::new().unwrap();
        dest.write_all(b"some data").unwrap();
        dest.flush().unwrap();

        let checksums = compute_checksums(dest.path(), 4).unwrap();
        let delta = generate_delta_streaming(source.path(), &checksums, 4).unwrap();

        assert_eq!(delta.ops.len(), 0);
        assert_eq!(delta.source_size, 0);
    }
}
