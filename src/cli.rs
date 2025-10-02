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
