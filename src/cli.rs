use crate::path::SyncPath;
use clap::Parser;

fn parse_sync_path(s: &str) -> Result<SyncPath, String> {
    Ok(SyncPath::parse(s))
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_uppercase();

    // Try to extract number and unit
    let (num_str, unit) = if let Some(pos) = s.find(|c: char| c.is_alphabetic()) {
        (&s[..pos], &s[pos..])
    } else {
        // No unit, assume bytes
        return s.parse::<u64>().map_err(|e| format!("Invalid size: {}", e));
    };

    let num: f64 = num_str.trim().parse()
        .map_err(|e| format!("Invalid number '{}': {}", num_str, e))?;

    let multiplier: u64 = match unit.trim() {
        "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        "TB" | "T" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("Unknown unit '{}'. Use B, KB, MB, GB, or TB", unit)),
    };

    Ok((num * multiplier as f64) as u64)
}

#[derive(Parser, Debug)]
#[command(name = "sy")]
#[command(about = "Modern file synchronization tool", long_about = None)]
#[command(version)]
#[command(after_help = "EXAMPLES:
    # Basic sync
    sy /source /destination

    # Preview changes without applying
    sy /source /destination --dry-run

    # Mirror mode (delete extra files in destination)
    sy /source /destination --delete

    # Parallel transfers (20 workers)
    sy /source /destination -j 20

    # Sync single file
    sy /path/to/file.txt /dest/file.txt

    # Remote sync (SSH)
    sy /local user@host:/remote
    sy user@host:/remote /local

    # Quiet mode (only errors)
    sy /source /destination --quiet

    # Bandwidth limiting
    sy /source /destination --bwlimit 1MB     # Limit to 1 MB/s
    sy /source user@host:/dest --bwlimit 500KB  # Limit to 500 KB/s

For more information: https://github.com/nijaru/sy")]
pub struct Cli {
    /// Source path (local: /path or remote: user@host:/path)
    #[arg(value_parser = parse_sync_path)]
    pub source: SyncPath,

    /// Destination path (local: /path or remote: user@host:/path)
    #[arg(value_parser = parse_sync_path)]
    pub destination: SyncPath,

    /// Show changes without applying them (dry-run)
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Delete files in destination not present in source
    #[arg(short, long)]
    pub delete: bool,

    /// Verbosity level (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Quiet mode (only show errors)
    #[arg(short, long)]
    pub quiet: bool,

    /// Number of parallel file transfers (default: 10)
    #[arg(short = 'j', long, default_value = "10")]
    pub parallel: usize,

    /// Minimum file size to sync (e.g., "1MB", "500KB")
    #[arg(long, value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Maximum file size to sync (e.g., "100MB", "1GB")
    #[arg(long, value_parser = parse_size)]
    pub max_size: Option<u64>,

    /// Exclude files matching pattern (can be repeated)
    /// Examples: "*.log", "node_modules", "target/"
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Bandwidth limit in bytes per second (e.g., "1MB", "500KB")
    #[arg(long, value_parser = parse_size)]
    pub bwlimit: Option<u64>,

    /// Enable resume support (auto-resume if state file found, default: true)
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    pub resume: bool,

    /// Checkpoint every N files (default: 10)
    #[arg(long, default_value = "10")]
    pub checkpoint_files: usize,

    /// Checkpoint every N bytes transferred (e.g., "100MB", default: 100MB)
    #[arg(long, value_parser = parse_size, default_value = "104857600")]
    pub checkpoint_bytes: u64,

    /// Enable compression for network transfers (auto-detects based on file type)
    #[arg(long)]
    pub compress: bool,
}

impl Cli {
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate size filters first (independent of source path)
        if let (Some(min), Some(max)) = (self.min_size, self.max_size) {
            if min > max {
                anyhow::bail!("--min-size ({}) cannot be greater than --max-size ({})", min, max);
            }
        }

        // Only validate local source paths (remote paths are validated during connection)
        if self.source.is_local() {
            let path = self.source.path();
            if !path.exists() {
                anyhow::bail!("Source path does not exist: {}", self.source);
            }
        }

        Ok(())
    }

    /// Check if source is a file (not a directory)
    pub fn is_single_file(&self) -> bool {
        self.source.is_local() && self.source.path().is_file()
    }

    pub fn log_level(&self) -> tracing::Level {
        if self.quiet {
            return tracing::Level::ERROR;
        }

        match self.verbose {
            0 => tracing::Level::INFO,
            1 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_validate_source_exists() {
        let temp = TempDir::new().unwrap();
        let cli = Cli {
            source: SyncPath::Local(temp.path().to_path_buf()),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            min_size: None,
            max_size: None,
            exclude: vec![],
            bwlimit: None,
            compress: false,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_validate_source_not_exists() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/nonexistent/path")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            min_size: None,
            max_size: None,
            exclude: vec![],
            bwlimit: None,
            compress: false,
        };
        let result = cli.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_validate_source_is_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("file.txt");
        fs::write(&file_path, "content").unwrap();

        let cli = Cli {
            source: SyncPath::Local(file_path.clone()),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        // Single file sync is now supported
        assert!(cli.validate().is_ok());
        assert!(cli.is_single_file());
    }

    #[test]
    fn test_validate_remote_source() {
        // Remote sources should not be validated locally
        let cli = Cli {
            source: SyncPath::Remote {
                host: "server".to_string(),
                user: Some("user".to_string()),
                path: PathBuf::from("/remote/path"),
            },
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_log_level_quiet() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/tmp/src")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: true,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::ERROR);
    }

    #[test]
    fn test_log_level_default() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/tmp/src")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::INFO);
    }

    #[test]
    fn test_log_level_verbose() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/tmp/src")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 1,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::DEBUG);
    }

    #[test]
    fn test_log_level_very_verbose() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/tmp/src")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 2,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::TRACE);
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1KB").unwrap(), 1024);
        assert_eq!(parse_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1.5MB").unwrap(), (1.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_size("500KB").unwrap(), 500 * 1024);

        // Test case insensitivity
        assert_eq!(parse_size("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1Mb").unwrap(), 1024 * 1024);

        // Test short forms
        assert_eq!(parse_size("1K").unwrap(), 1024);
        assert_eq!(parse_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_size_filter_validation() {
        let cli = Cli {
            source: SyncPath::Local(PathBuf::from("/tmp/src")),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
            exclude: vec![],
            bwlimit: None,
            compress: false,
            min_size: Some(1024 * 1024),  // 1MB
            max_size: Some(500 * 1024),    // 500KB (smaller than min)
        };

        let result = cli.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min-size"));
    }
}
