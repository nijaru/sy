use crate::path::SyncPath;
use clap::Parser;

fn parse_sync_path(s: &str) -> Result<SyncPath, String> {
    Ok(SyncPath::parse(s))
}

#[derive(Parser, Debug)]
#[command(name = "sy")]
#[command(about = "Modern file synchronization tool", long_about = None)]
#[command(version)]
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
}

impl Cli {
    pub fn validate(&self) -> anyhow::Result<()> {
        // Only validate local source paths (remote paths are validated during connection)
        if self.source.is_local() {
            let path = self.source.path();
            if !path.exists() {
                anyhow::bail!("Source path does not exist: {}", self.source);
            }

            if !path.is_dir() {
                anyhow::bail!("Source must be a directory: {}", self.source);
            }
        }

        Ok(())
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
            source: SyncPath::Local(file_path),
            destination: SyncPath::Local(PathBuf::from("/tmp/dest")),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
            parallel: 10,
        };
        let result = cli.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must be a directory"));
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
        };
        assert_eq!(cli.log_level(), tracing::Level::TRACE);
    }
}
