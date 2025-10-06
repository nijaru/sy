use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use sy::compress::{decompress, Compression};
use sy::delta::{apply_delta, compute_checksums, Delta};
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
    },
    /// Compute block checksums for a file
    Checksums {
        /// File to compute checksums for
        path: PathBuf,
        /// Block size in bytes
        #[arg(long)]
        block_size: usize,
    },
    /// Apply delta operations to a file (reads delta JSON from stdin)
    ApplyDelta {
        /// Existing file to apply delta to
        base_file: PathBuf,
        /// Output file path
        output_file: PathBuf,
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Scan { path } => {
            let scanner = Scanner::new(&path);
            let entries = scanner.scan()?;

            let json_entries: Vec<FileEntryJson> = entries
                .into_iter()
                .map(|e| {
                    let mtime = e
                        .modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    FileEntryJson {
                        path: e.path.to_string_lossy().to_string(),
                        size: e.size,
                        mtime,
                        is_dir: e.is_dir,
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
        Commands::ApplyDelta {
            base_file,
            output_file,
        } => {
            // Read delta data from stdin (may be compressed)
            use std::io::Read;
            let mut stdin_data = Vec::new();
            std::io::stdin().read_to_end(&mut stdin_data)?;

            // Check if data is compressed (Zstd magic: 0x28, 0xB5, 0x2F, 0xFD)
            let delta_json = if stdin_data.len() >= 4 &&
                stdin_data[0] == 0x28 && stdin_data[1] == 0xB5 &&
                stdin_data[2] == 0x2F && stdin_data[3] == 0xFD {
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
    }

    Ok(())
}
