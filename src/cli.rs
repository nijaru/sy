use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "sy")]
#[command(about = "Modern file synchronization tool", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Source path
    pub source: PathBuf,

    /// Destination path
    pub destination: PathBuf,

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
}

impl Cli {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.source.exists() {
            anyhow::bail!("Source path does not exist: {}", self.source.display());
        }

        if !self.source.is_dir() {
            anyhow::bail!("Source must be a directory: {}", self.source.display());
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
    use tempfile::TempDir;

    #[test]
    fn test_validate_source_exists() {
        let temp = TempDir::new().unwrap();
        let cli = Cli {
            source: temp.path().to_path_buf(),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_validate_source_not_exists() {
        let cli = Cli {
            source: PathBuf::from("/nonexistent/path"),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
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
            source: file_path,
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
        };
        let result = cli.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must be a directory"));
    }

    #[test]
    fn test_log_level_quiet() {
        let cli = Cli {
            source: PathBuf::from("/tmp/src"),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: true,
        };
        assert_eq!(cli.log_level(), tracing::Level::ERROR);
    }

    #[test]
    fn test_log_level_default() {
        let cli = Cli {
            source: PathBuf::from("/tmp/src"),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 0,
            quiet: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::INFO);
    }

    #[test]
    fn test_log_level_verbose() {
        let cli = Cli {
            source: PathBuf::from("/tmp/src"),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 1,
            quiet: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::DEBUG);
    }

    #[test]
    fn test_log_level_very_verbose() {
        let cli = Cli {
            source: PathBuf::from("/tmp/src"),
            destination: PathBuf::from("/tmp/dest"),
            dry_run: false,
            delete: false,
            verbose: 2,
            quiet: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::TRACE);
    }
}
