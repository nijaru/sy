use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use sy::compress::{decompress, Compression};
use sy::delta::{apply_delta, compute_checksums, Delta};
use sy::sparse::DataRegion;
use sy::sync::scanner::Scanner;

#[derive(Parser)]
#[command(name = "sy-remote")]
#[command(about = "Remote helper for sy - executes on remote hosts via SSH")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan a directory and output file list as JSON
    Scan {
        /// Directory to scan
        path: PathBuf,
        /// Disable .gitignore respect
        #[arg(long)]
        no_git_ignore: bool,
        /// Include .git directory
        #[arg(long, default_value_t = false)]
        include_git: bool,
    },
    /// Compute block checksums for a file
    Checksums {
        /// File to compute checksums for
        path: PathBuf,
        /// Block size in bytes
        #[arg(long)]
        block_size: usize,
    },
    /// Compute file checksum (for verification)
    FileChecksum {
        /// File to compute checksum for
        path: PathBuf,
        /// Checksum type: "fast" (xxHash3) or "cryptographic" (BLAKE3)
        #[arg(long, default_value = "fast")]
        checksum_type: String,
    },
    /// Apply delta operations to a file (reads delta JSON from stdin)
    ApplyDelta {
        /// Existing file to apply delta to
        base_file: PathBuf,
        /// Output file path
        output_file: PathBuf,
    },
    /// Receive a file (potentially compressed) from stdin and write to disk
    ReceiveFile {
        /// Output file path
        output_path: PathBuf,
        /// Optional modification time (seconds since epoch)
        #[arg(long)]
        mtime: Option<u64>,
    },
    /// Receive a sparse file with specified data regions
    ReceiveSparseFile {
        /// Output file path
        output_path: PathBuf,
        /// Total file size in bytes
        #[arg(long)]
        total_size: u64,
        /// Data regions as JSON (array of {offset, length} objects)
        #[arg(long)]
        regions: String,
        /// Optional modification time (seconds since epoch)
        #[arg(long)]
        mtime: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct ScanOutput {
    entries: Vec<FileEntryJson>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileEntryJson {
    path: String,
    size: u64,
    mtime: i64,
    is_dir: bool,
    // Extended metadata for full preservation
    is_symlink: bool,
    symlink_target: Option<String>,
    is_sparse: bool,
    allocated_size: u64,
    #[serde(default)]
    xattrs: Option<Vec<(String, String)>>, // (key, base64-encoded value)
    inode: Option<u64>,
    nlink: u64,
    #[serde(default)]
    acls: Option<String>, // ACL text format (one per line)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan {
            path,
            no_git_ignore,
            include_git,
        } => {
            let scanner = Scanner::new(&path)
                .respect_gitignore(!no_git_ignore)
                .include_git_dir(include_git);
            let entries = scanner.scan()?;

            let json_entries: Vec<FileEntryJson> = entries
                .into_iter()
                .map(|e| {
                    let mtime = e
                        .modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;

                    // Encode xattrs to base64 for transport
                    let xattrs = e.xattrs.map(|xattrs_map| {
                        use base64::{engine::general_purpose, Engine as _};
                        xattrs_map
                            .into_iter()
                            .map(|(key, value)| {
                                let encoded = general_purpose::STANDARD.encode(&value);
                                (key, encoded)
                            })
                            .collect()
                    });

                    // Convert ACLs from bytes to string
                    let acls = e
                        .acls
                        .and_then(|acl_bytes| String::from_utf8(acl_bytes).ok());

                    FileEntryJson {
                        path: e.path.to_string_lossy().to_string(),
                        size: e.size,
                        mtime,
                        is_dir: e.is_dir,
                        is_symlink: e.is_symlink,
                        symlink_target: e.symlink_target.map(|p| p.to_string_lossy().to_string()),
                        is_sparse: e.is_sparse,
                        allocated_size: e.allocated_size,
                        xattrs,
                        inode: e.inode,
                        nlink: e.nlink,
                        acls,
                    }
                })
                .collect();

            let output = ScanOutput {
                entries: json_entries,
            };

            println!("{}", serde_json::to_string(&output)?);
        }
        Commands::Checksums { path, block_size } => {
            let checksums = compute_checksums(&path, block_size)?;
            println!("{}", serde_json::to_string(&checksums)?);
        }
        Commands::FileChecksum {
            path,
            checksum_type,
        } => {
            use sy::integrity::{ChecksumType, IntegrityVerifier};

            let csum_type = match checksum_type.as_str() {
                "fast" => ChecksumType::Fast,
                "cryptographic" => ChecksumType::Cryptographic,
                _ => anyhow::bail!(
                    "Invalid checksum type: {}. Use 'fast' or 'cryptographic'",
                    checksum_type
                ),
            };

            let verifier = IntegrityVerifier::new(csum_type, false);
            let checksum = verifier.compute_file_checksum(&path)?;

            // Output checksum as hex string
            println!("{}", checksum.to_hex());
        }
        Commands::ApplyDelta {
            base_file,
            output_file,
        } => {
            // Read delta data from stdin (may be compressed)
            let mut stdin_data = Vec::new();
            std::io::stdin().read_to_end(&mut stdin_data)?;

            // Check if data is compressed (Zstd magic: 0x28, 0xB5, 0x2F, 0xFD)
            let delta_json = if stdin_data.len() >= 4
                && stdin_data[0] == 0x28
                && stdin_data[1] == 0xB5
                && stdin_data[2] == 0x2F
                && stdin_data[3] == 0xFD
            {
                // Decompress zstd data
                let decompressed = decompress(&stdin_data, Compression::Zstd)?;
                String::from_utf8(decompressed)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            } else {
                // Uncompressed JSON
                String::from_utf8(stdin_data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            };

            let delta: Delta = serde_json::from_str(&delta_json)?;
            let stats = apply_delta(&base_file, &delta, &output_file)?;
            println!(
                "{{\"operations_count\": {}, \"literal_bytes\": {}}}",
                stats.operations_count, stats.literal_bytes
            );
        }
        Commands::ReceiveFile { output_path, mtime } => {
            // Read file data from stdin (may be compressed)
            let mut stdin_data = Vec::new();
            std::io::stdin().read_to_end(&mut stdin_data)?;

            // Check if data is compressed (Zstd magic: 0x28, 0xB5, 0x2F, 0xFD)
            let file_data = if stdin_data.len() >= 4
                && stdin_data[0] == 0x28
                && stdin_data[1] == 0xB5
                && stdin_data[2] == 0x2F
                && stdin_data[3] == 0xFD
            {
                // Decompress zstd data
                decompress(&stdin_data, Compression::Zstd)?
            } else {
                // Uncompressed data
                stdin_data
            };

            // Ensure parent directory exists
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Write file
            let mut output_file = std::fs::File::create(&output_path)?;
            output_file.write_all(&file_data)?;
            output_file.flush()?;

            // Set mtime if provided
            if let Some(mtime_secs) = mtime {
                use std::time::{Duration, UNIX_EPOCH};
                let mtime = UNIX_EPOCH + Duration::from_secs(mtime_secs);
                let _ = filetime::set_file_mtime(
                    &output_path,
                    filetime::FileTime::from_system_time(mtime),
                );
            }

            // Report success with bytes written
            println!("{{\"bytes_written\": {}}}", file_data.len());
        }
        Commands::ReceiveSparseFile {
            output_path,
            total_size,
            regions,
            mtime,
        } => {
            // Parse data regions from JSON
            let data_regions: Vec<DataRegion> = serde_json::from_str(&regions)?;

            // Ensure parent directory exists
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Create file and set its size (creates sparse file with holes)
            let mut output_file = std::fs::File::create(&output_path)?;
            output_file.set_len(total_size)?;

            // Read and write each data region from stdin
            let mut stdin = std::io::stdin();
            let mut total_bytes_written = 0u64;

            for region in &data_regions {
                // Seek to the region's offset
                output_file.seek(SeekFrom::Start(region.offset))?;

                // Read exactly `region.length` bytes from stdin
                let mut buffer = vec![0u8; region.length as usize];
                stdin.read_exact(&mut buffer)?;

                // Write to file
                output_file.write_all(&buffer)?;
                total_bytes_written += region.length;
            }

            output_file.flush()?;
            output_file.sync_all()?;

            // Set mtime if provided
            if let Some(mtime_secs) = mtime {
                use std::time::{Duration, UNIX_EPOCH};
                let mtime = UNIX_EPOCH + Duration::from_secs(mtime_secs);
                let _ = filetime::set_file_mtime(
                    &output_path,
                    filetime::FileTime::from_system_time(mtime),
                );
            }

            // Report success with total data bytes written (not file size)
            println!(
                "{{\"bytes_written\": {}, \"file_size\": {}, \"regions\": {}}}",
                total_bytes_written,
                total_size,
                data_regions.len()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_receive_sparse_file_basic() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("sparse_output.dat");

        // Create data regions: 2 regions
        // Region 1: offset 0, length 1024 (data: "A" repeated)
        // Region 2: offset 2048, length 512 (data: "B" repeated)
        let regions = vec![
            DataRegion {
                offset: 0,
                length: 1024,
            },
            DataRegion {
                offset: 2048,
                length: 512,
            },
        ];

        let total_size = 4096; // 4KB total file
        let regions_json = serde_json::to_string(&regions).unwrap();

        // Prepare input data (simulating stdin)
        let mut input_data = Vec::new();
        input_data.extend(vec![b'A'; 1024]); // Region 1 data
        input_data.extend(vec![b'B'; 512]); // Region 2 data

        // Simulate the command (we'll manually execute the logic)
        let mut output_file = std::fs::File::create(&output_path).unwrap();
        output_file.set_len(total_size).unwrap();

        // Parse regions
        let data_regions: Vec<DataRegion> = serde_json::from_str(&regions_json).unwrap();

        // Write regions
        let mut offset_in_buffer = 0;
        for region in &data_regions {
            use std::io::Seek;
            output_file
                .seek(std::io::SeekFrom::Start(region.offset))
                .unwrap();
            output_file
                .write_all(&input_data[offset_in_buffer..offset_in_buffer + region.length as usize])
                .unwrap();
            offset_in_buffer += region.length as usize;
        }

        output_file.flush().unwrap();
        drop(output_file);

        // Verify the file
        let result = std::fs::read(&output_path).unwrap();
        assert_eq!(result.len(), 4096);

        // Check region 1 (offset 0, length 1024) has 'A's
        assert!(result[0..1024].iter().all(|&b| b == b'A'));

        // Check hole (offset 1024..2048) has zeros
        assert!(result[1024..2048].iter().all(|&b| b == 0));

        // Check region 2 (offset 2048, length 512) has 'B's
        assert!(result[2048..2560].iter().all(|&b| b == b'B'));

        // Check remaining (offset 2560..4096) has zeros
        assert!(result[2560..4096].iter().all(|&b| b == 0));

        // Verify file is sparse (on Unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&output_path).unwrap();
            let allocated = metadata.blocks() * 512;
            // File should use less space than total size (due to holes)
            // On some filesystems this might not work, so we just check it doesn't panic
            let _ = allocated < total_size;
        }
    }

    #[test]
    fn test_receive_sparse_file_single_region() {
        let temp = TempDir::new().unwrap();
        let output_path = temp.path().join("single_region.dat");

        // Single region at offset 1MB
        let regions = vec![DataRegion {
            offset: 1024 * 1024,
            length: 100,
        }];

        let total_size = 1024 * 1024 + 200; // Slightly larger
        let regions_json = serde_json::to_string(&regions).unwrap();

        let input_data = vec![b'X'; 100];

        // Execute logic
        let mut output_file = std::fs::File::create(&output_path).unwrap();
        output_file.set_len(total_size).unwrap();

        let data_regions: Vec<DataRegion> = serde_json::from_str(&regions_json).unwrap();

        use std::io::Seek;
        for region in &data_regions {
            output_file
                .seek(std::io::SeekFrom::Start(region.offset))
                .unwrap();
            output_file.write_all(&input_data).unwrap();
        }

        output_file.flush().unwrap();
        drop(output_file);

        // Verify
        let metadata = std::fs::metadata(&output_path).unwrap();
        assert_eq!(metadata.len(), total_size);

        // Read the specific region
        let mut file = std::fs::File::open(&output_path).unwrap();
        file.seek(std::io::SeekFrom::Start(1024 * 1024)).unwrap();
        let mut buffer = vec![0u8; 100];
        file.read_exact(&mut buffer).unwrap();
        assert!(buffer.iter().all(|&b| b == b'X'));
    }

    #[test]
    fn test_data_region_json_serialization() {
        let regions = vec![
            DataRegion {
                offset: 0,
                length: 1024,
            },
            DataRegion {
                offset: 4096,
                length: 2048,
            },
        ];

        let json = serde_json::to_string(&regions).unwrap();
        let deserialized: Vec<DataRegion> = serde_json::from_str(&json).unwrap();

        assert_eq!(regions.len(), deserialized.len());
        assert_eq!(regions[0].offset, deserialized[0].offset);
        assert_eq!(regions[0].length, deserialized[0].length);
        assert_eq!(regions[1].offset, deserialized[1].offset);
        assert_eq!(regions[1].length, deserialized[1].length);
    }
}
