use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
    }

    Ok(())
}
