use crate::path::SyncPath;
use clap::{Parser, ValueEnum};

// Import integrity types for verification modes
use crate::integrity::ChecksumType;

// Import compression types for detection modes
use crate::compress::CompressionDetection;

use crate::sync::scanner::ScanOptions;

fn parse_sync_path(s: &str) -> Result<SyncPath, String> {
    Ok(SyncPath::parse(s))
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_uppercase();

    // Try to extract number and unit
    let (num_str, unit) = if let Some(pos) = s.find(|c: char| c.is_alphabetic()) {
        (&s[..pos], &s[pos..])
    } else {
        // No unit, assume bytes
        return s.parse::<u64>().map_err(|e| format!("Invalid size: {}", e));
    };

    let num: f64 = num_str
        .trim()
        .parse()
        .map_err(|e| format!("Invalid number '{}': {}", num_str, e))?;

    let multiplier: u64 = match unit.trim() {
        "B" => 1,
        "KB" | "K" => 1024,
        "MB" | "M" => 1024 * 1024,
        "GB" | "G" => 1024 * 1024 * 1024,
        "TB" | "T" => 1024 * 1024 * 1024 * 1024,
        _ => return Err(format!("Unknown unit '{}'. Use B, KB, MB, GB, or TB", unit)),
    };

    let result = num * multiplier as f64;
    if result < 0.0 || result > u64::MAX as f64 {
        return Err(format!("Size '{}' exceeds maximum (~16 exabytes)", s));
    }
    Ok(result as u64)
}

/// Verification mode for file integrity
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum VerificationMode {
    /// Size and mtime only (fastest, least reliable)
    Fast,

    /// Add xxHash3 checksums (default, good balance)
    Standard,

    /// BLAKE3 end-to-end verification (slower, cryptographic)
    Verify,

    /// BLAKE3 + verify every block during transfer (slowest, maximum reliability)
    Paranoid,
}

impl VerificationMode {
    /// Get the checksum type for this mode
    pub fn checksum_type(&self) -> ChecksumType {
        match self {
            Self::Fast => ChecksumType::None,
            Self::Standard => ChecksumType::Fast,
            Self::Verify | Self::Paranoid => ChecksumType::Cryptographic,
        }
    }

    /// Check if this mode requires block-level verification
    pub fn verify_blocks(&self) -> bool {
        matches!(self, Self::Paranoid)
    }
}

/// Symlink handling mode
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum SymlinkMode {
    /// Preserve symlinks as symlinks (default)
    #[default]
    Preserve,

    /// Follow symlinks and copy targets
    Follow,

    /// Skip all symlinks
    Skip,
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

    # S3 sync
    sy /local s3://bucket/path
    sy s3://bucket/path /local

    # Quiet mode (only errors)
    sy /source /destination --quiet

    # Bandwidth limiting
    sy /source /destination --bwlimit 1MB     # Limit to 1 MB/s
    sy /source user@host:/dest --bwlimit 500KB  # Limit to 500 KB/s

    # Verification modes
    sy /source /destination --verify            # BLAKE3 cryptographic verification
    sy /source /destination --mode paranoid     # Maximum reliability

    # Network retry options
    sy /source user@host:/dest --retry 5        # Retry up to 5 times on network errors
    sy /source user@host:/dest --retry-delay 2  # Start with 2s delay (2s, 4s, 8s, ...)

    # Resume interrupted transfers
    sy /source user@host:/dest --resume         # Auto-resume interrupted large files
    sy /source user@host:/dest --resume-only    # Only resume, don't start new transfers
    sy /source user@host:/dest --clear-resume-state  # Clear all resume state

For more information: https://github.com/nijaru/sy")]
pub struct Cli {
    /// Source path (local: /path or remote: user@host:/path)
    /// Optional when using --profile
    #[arg(value_parser = parse_sync_path)]
    pub source: Option<SyncPath>,

    /// Destination path (local: /path or remote: user@host:/path)
    /// Optional when using --profile
    #[arg(value_parser = parse_sync_path)]
    pub destination: Option<SyncPath>,

    /// Show changes without applying them (dry-run)
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Show detailed changes in dry-run mode (file sizes, byte changes)
    /// Requires --dry-run to be effective
    #[arg(long)]
    pub diff: bool,

    /// Delete files in destination not present in source
    #[arg(short, long)]
    pub delete: bool,

    /// Maximum percentage of files that can be deleted (0-100, default: 50)
    /// Prevents accidental mass deletion
    #[arg(long, default_value = "50")]
    pub delete_threshold: u8,

    /// Move deleted files to trash instead of permanent deletion
    #[arg(long)]
    pub trash: bool,

    /// Skip deletion safety checks (dangerous - use with caution)
    #[arg(long)]
    pub force_delete: bool,

    /// Verbosity level (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Quiet mode (only show errors)
    #[arg(short, long)]
    pub quiet: bool,

    /// Show detailed performance summary at the end
    #[arg(long)]
    pub perf: bool,

    /// Show progress bar for each large file (>= 1MB) being transferred
    /// Automatically hidden when output is piped or with --quiet
    #[arg(long)]
    pub per_file_progress: bool,

    /// Number of parallel file transfers (default: 10)
    #[arg(short = 'j', long, default_value = "10")]
    pub parallel: usize,

    /// Maximum number of errors before aborting (0 = unlimited, default: 100)
    #[arg(long, default_value = "100")]
    pub max_errors: usize,

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

    /// Include files matching pattern (can be repeated, processed in order with --exclude)
    /// Examples: "*.rs", "important.log"
    #[arg(long)]
    pub include: Vec<String>,

    /// Filter rules in rsync syntax: "+ pattern" (include) or "- pattern" (exclude)
    /// Can be repeated. Rules processed in order, first match wins.
    /// Examples: "+ *.rs", "- *.log", "- target/*"
    #[arg(long, allow_hyphen_values = true)]
    pub filter: Vec<String>,

    /// Read exclude patterns from file (one pattern per line)
    #[arg(long)]
    pub exclude_from: Option<std::path::PathBuf>,

    /// Read include patterns from file (one pattern per line)
    #[arg(long)]
    pub include_from: Option<std::path::PathBuf>,

    /// Apply ignore template from ~/.config/sy/templates/ (can be repeated)
    /// Examples: "rust", "node", "python"
    #[arg(long)]
    pub ignore_template: Vec<String>,

    /// Bandwidth limit in bytes per second (e.g., "1MB", "500KB")
    #[arg(long, value_parser = parse_size)]
    pub bwlimit: Option<u64>,

    /// Enable resume support (auto-resume if state file found, default: true)
    #[arg(long, overrides_with = "no_resume")]
    resume: bool,

    /// Disable resume support
    #[arg(long, overrides_with = "resume")]
    pub no_resume: bool,

    /// Only resume interrupted transfers, don't start new ones
    #[arg(long)]
    pub resume_only: bool,

    /// Clear all resume state before starting
    #[arg(long)]
    pub clear_resume_state: bool,

    /// Use streaming mode for massive directories (experimental)
    #[arg(long)]
    pub stream: bool,

    /// Checkpoint every N files (default: 10)
    #[arg(long, default_value = "10")]
    pub checkpoint_files: usize,

    /// Checkpoint every N bytes transferred (e.g., "100MB", default: 100MB)
    #[arg(long, value_parser = parse_size, default_value = "104857600")]
    pub checkpoint_bytes: u64,

    /// Delete any existing state files before starting (fresh sync)
    #[arg(long)]
    pub clean_state: bool,

    /// Use directory cache for faster re-syncs (default: false)
    /// The cache stores directory mtimes to skip unchanged directories
    #[arg(long, default_value = "false", action = clap::ArgAction::Set)]
    pub use_cache: bool,

    /// Delete any existing cache files before starting
    #[arg(long)]
    pub clear_cache: bool,

    /// Use checksum database for faster --checksum re-syncs (default: false)
    /// The database stores checksums to avoid recomputation for unchanged files
    #[arg(long, default_value = "false", action = clap::ArgAction::Set)]
    pub checksum_db: bool,

    /// Clear checksum database before starting
    #[arg(long)]
    pub clear_checksum_db: bool,

    /// Remove stale entries from checksum database (files no longer in source)
    #[arg(long)]
    pub prune_checksum_db: bool,

    /// Verification mode (fast, standard, verify, paranoid)
    #[arg(long, value_enum, default_value = "standard")]
    pub mode: VerificationMode,

    /// Enable BLAKE3 verification (shortcut for --mode verify)
    #[arg(long)]
    pub verify: bool,

    /// Enable compression for network transfers (auto-detects based on file type)
    #[arg(long)]
    pub compress: bool,

    /// Compression detection mode (auto, extension, always, never)
    /// - auto: Content-based detection with sampling (default)
    /// - extension: Extension-only detection (legacy)
    /// - always: Always compress (override detection)
    /// - never: Never compress (override detection)
    #[arg(long, value_enum, default_value = "auto")]
    pub compression_detection: CompressionDetection,

    /// Symlink handling mode (preserve, follow, skip)
    #[arg(long, value_enum, default_value = "preserve")]
    pub links: SymlinkMode,

    /// Follow symlinks and copy targets (shortcut for --links follow)
    #[arg(short = 'L', long)]
    pub copy_links: bool,

    /// Preserve extended attributes (xattrs)
    #[arg(short = 'X', long)]
    pub preserve_xattrs: bool,

    /// Preserve hard links (treat multiple links to the same file as one copy)
    #[arg(short = 'H', long)]
    pub preserve_hardlinks: bool,

    /// Preserve access control lists (ACLs)
    #[arg(short = 'A', long)]
    pub preserve_acls: bool,

    /// Preserve BSD file flags (macOS only: hidden, immutable, nodump, etc.; no-op on other platforms)
    #[arg(short = 'F', long)]
    pub preserve_flags: bool,

    /// Preserve permissions
    #[arg(short = 'p', long)]
    pub preserve_permissions: bool,

    /// Preserve modification times
    #[arg(short = 't', long)]
    pub preserve_times: bool,

    /// Preserve group (requires appropriate permissions)
    #[arg(short = 'g', long)]
    pub preserve_group: bool,

    /// Preserve owner (requires root)
    #[arg(short = 'o', long)]
    pub preserve_owner: bool,

    /// Preserve device files and special files (requires root)
    #[arg(short = 'D', long)]
    pub preserve_devices: bool,

    /// Archive mode: preserve all metadata (-rlptgoD) and copy everything
    ///
    /// Equivalent to rsync's -rlptgoD (recursive, links, perms, times, group, owner, devices).
    /// Also implies --no-gitignore and --include-vcs to ensure complete backups.
    ///
    /// Does NOT include: -X (xattrs), -A (ACLs), -H (hardlinks) - add those flags separately.
    #[arg(short = 'a', long)]
    pub archive: bool,

    /// Don't filter files based on .gitignore rules
    ///
    /// By default, sy respects .gitignore for developer-friendly syncs.
    /// Use this flag (or -a/--archive) to sync all files regardless of .gitignore.
    #[arg(long)]
    pub no_gitignore: bool,

    /// Include .git directories in the sync
    ///
    /// By default, sy excludes .git directories for faster syncs.
    /// Use this flag (or -a/--archive) to include version control directories.
    #[arg(long)]
    pub include_vcs: bool,

    /// Ignore modification times, always compare checksums (rsync --ignore-times)
    #[arg(long)]
    pub ignore_times: bool,

    /// Only compare file size, skip mtime checks (rsync --size-only)
    #[arg(long)]
    pub size_only: bool,

    /// Always compare checksums instead of size+mtime (slow but thorough, rsync --checksum)
    #[arg(short = 'c', long)]
    pub checksum: bool,

    /// Verify-only mode: audit file integrity without modifying anything
    /// Compares source and destination checksums and reports mismatches
    /// Returns exit code 0 if all match, 1 if mismatches found, 2 on error
    #[arg(long)]
    pub verify_only: bool,

    /// Output JSON (newline-delimited JSON for scripting)
    #[arg(long)]
    pub json: bool,

    /// Watch mode - continuously monitor source for changes
    #[arg(long)]
    pub watch: bool,

    /// Disable hook execution (skip pre-sync and post-sync hooks)
    #[arg(long)]
    pub no_hooks: bool,

    /// Abort sync if any hook fails (default: warn and continue)
    #[arg(long)]
    pub abort_on_hook_failure: bool,

    /// Use named profile from config file
    #[arg(long)]
    pub profile: Option<String>,

    /// List all available profiles
    #[arg(long)]
    pub list_profiles: bool,

    /// Show details of a specific profile
    #[arg(long)]
    pub show_profile: Option<String>,

    /// Bidirectional sync mode - sync changes in both directions
    /// Detects and resolves conflicts automatically based on --conflict-resolve strategy
    #[arg(short = 'b', long)]
    pub bidirectional: bool,

    /// Conflict resolution strategy for bidirectional sync
    /// Options: newer (default), larger, smaller, source, dest, rename
    #[arg(long, default_value = "newer")]
    pub conflict_resolve: String,

    /// Maximum percentage of files that can be deleted in bidirectional sync (0-100)
    /// Set to 0 for unlimited deletions (default: 50)
    #[arg(long, default_value = "50")]
    pub max_delete: u8,

    /// Clear bidirectional sync state before syncing
    /// Forces full comparison instead of using cached state
    #[arg(long)]
    pub clear_bisync_state: bool,

    /// Force resync by ignoring corrupt state (recovery mode)
    /// Use this when bisync state file is corrupted
    /// All differences will be treated as new changes on first sync
    #[arg(long)]
    pub force_resync: bool,

    /// Maximum retry attempts for network operations (default: 3, 0 = no retries)
    #[arg(long, default_value = "3")]
    pub retry: u32,

    /// Initial delay between retries in seconds (default: 1)
    /// Delay increases exponentially with each retry (1s, 2s, 4s, ...)
    #[arg(long, default_value = "1")]
    pub retry_delay: u64,
}

impl Cli {
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate size filters first (independent of source path)
        if let (Some(min), Some(max)) = (self.min_size, self.max_size) {
            if min > max {
                anyhow::bail!(
                    "--min-size ({}) cannot be greater than --max-size ({})",
                    min,
                    max
                );
            }
        }

        // Validate comparison flags (mutually exclusive)
        let comparison_flags = [self.ignore_times, self.size_only, self.checksum];
        let enabled_count = comparison_flags.iter().filter(|&&x| x).count();
        if enabled_count > 1 {
            anyhow::bail!("--ignore-times, --size-only, and --checksum are mutually exclusive");
        }

        // Validate deletion threshold (0-100)
        if self.delete_threshold > 100 {
            anyhow::bail!(
                "--delete-threshold must be between 0 and 100 (got: {})",
                self.delete_threshold
            );
        }

        // --verify-only conflicts with modification flags
        if self.verify_only {
            if self.delete {
                anyhow::bail!("--verify-only cannot be used with --delete (read-only mode)");
            }
            if self.watch {
                anyhow::bail!("--verify-only cannot be used with --watch (read-only mode)");
            }
            if self.dry_run {
                anyhow::bail!("--verify-only is already read-only, --dry-run is redundant");
            }
        }

        // Bidirectional sync validation
        if self.bidirectional {
            // Validate max_delete percentage
            if self.max_delete > 100 {
                anyhow::bail!(
                    "--max-delete must be between 0 and 100 (got: {})",
                    self.max_delete
                );
            }

            // Validate conflict resolution strategy
            let valid_strategies = ["newer", "larger", "smaller", "source", "dest", "rename"];
            if !valid_strategies.contains(&self.conflict_resolve.as_str()) {
                anyhow::bail!(
                    "Invalid --conflict-resolve strategy '{}'. Valid options: {}",
                    self.conflict_resolve,
                    valid_strategies.join(", ")
                );
            }

            // Bidirectional conflicts with certain flags
            if self.verify_only {
                anyhow::bail!(
                    "--bidirectional cannot be used with --verify-only (conflicts with sync logic)"
                );
            }
            if self.watch {
                anyhow::bail!("--bidirectional with --watch is not yet supported (deferred to future version)");
            }

            // Bidirectional sync doesn't support S3 paths
            let source_is_s3 = self.source.as_ref().is_some_and(|p| p.is_s3());
            let dest_is_s3 = self.destination.as_ref().is_some_and(|p| p.is_s3());
            if source_is_s3 || dest_is_s3 {
                anyhow::bail!(
                    "--bidirectional does not support S3 paths (use unidirectional sync instead)"
                );
            }
        }

        // --list-profiles and --show-profile don't need source/destination
        if self.list_profiles || self.show_profile.is_some() {
            return Ok(());
        }

        // If using --profile, source/destination come from profile (validated later)
        // Otherwise, source and destination must be provided
        if self.profile.is_none() && (self.source.is_none() || self.destination.is_none()) {
            anyhow::bail!("Source and destination are required (or use --profile)");
        }

        // Only validate local source paths (remote paths are validated during connection)
        if let Some(source) = &self.source {
            if source.is_local() {
                let path = source.path();
                if !path.exists() {
                    anyhow::bail!("Source path does not exist: {}", source);
                }
            }
        }

        Ok(())
    }

    /// Get the effective verification mode (applying --verify flag override)
    pub fn verification_mode(&self) -> VerificationMode {
        if self.verify {
            VerificationMode::Verify
        } else {
            self.mode
        }
    }

    /// Get the effective symlink mode (applying --copy-links flag override)
    pub fn symlink_mode(&self) -> SymlinkMode {
        if self.copy_links {
            SymlinkMode::Follow
        } else {
            self.links
        }
    }

    /// Get scan options based on CLI flags
    ///
    /// Priority (highest to lowest):
    /// 1. Explicit flags: --no-gitignore, --include-vcs
    /// 2. Archive mode (-a): implies both --no-gitignore and --include-vcs
    /// 3. Default: respect .gitignore, exclude .git (developer-friendly)
    pub fn scan_options(&self) -> ScanOptions {
        // Archive mode sets both, but explicit flags can override
        let archive_mode = self.archive;

        // respect_gitignore: false if --no-gitignore OR archive mode
        let respect_gitignore = !(self.no_gitignore || archive_mode);

        // include_git_dir: true if --include-vcs OR archive mode
        let include_git_dir = self.include_vcs || archive_mode;

        ScanOptions {
            respect_gitignore,
            include_git_dir,
        }
    }

    /// Get effective resume setting (default: true)
    ///
    /// Priority:
    /// 1. --no-resume: disables resume (returns false)
    /// 2. --resume: enables resume (returns true)
    /// 3. Default: enabled (returns true)
    pub fn resume(&self) -> bool {
        if self.no_resume {
            false
        } else {
            // Default is true, --resume flag also sets to true
            true
        }
    }

    /// Check if source is a file (not a directory)
    pub fn is_single_file(&self) -> bool {
        self.source
            .as_ref()
            .is_some_and(|s| s.is_local() && s.path().is_file())
    }

    pub fn log_level(&self) -> tracing::Level {
        if self.quiet || self.json {
            return tracing::Level::ERROR;
        }

        match self.verbose {
            0 => tracing::Level::INFO,
            1 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    }

    /// Check if permissions should be preserved (archive mode or explicit flag)
    #[allow(dead_code)] // Public API for permission preservation (planned feature)
    pub fn should_preserve_permissions(&self) -> bool {
        self.archive || self.preserve_permissions
    }

    /// Check if modification times should be preserved (archive mode or explicit flag)
    #[allow(dead_code)] // Public API for time preservation (planned feature)
    pub fn should_preserve_times(&self) -> bool {
        self.archive || self.preserve_times
    }

    /// Check if group should be preserved (archive mode or explicit flag)
    #[allow(dead_code)] // Public API for group preservation (planned feature)
    pub fn should_preserve_group(&self) -> bool {
        self.archive || self.preserve_group
    }

    /// Check if owner should be preserved (archive mode or explicit flag)
    #[allow(dead_code)] // Public API for owner preservation (planned feature)
    pub fn should_preserve_owner(&self) -> bool {
        self.archive || self.preserve_owner
    }

    /// Check if device files should be preserved (archive mode or explicit flag)
    #[allow(dead_code)] // Public API for device preservation (planned feature)
    pub fn should_preserve_devices(&self) -> bool {
        self.archive || self.preserve_devices
    }

    /// Check if symlinks should be preserved (archive mode enables by default)
    #[allow(dead_code)] // Public API for symlink preservation (planned feature)
    pub fn should_preserve_symlinks(&self) -> bool {
        // Archive mode implies -l (preserve symlinks)
        // Unless user explicitly set --links to something else or used -L
        if self.archive && !self.copy_links {
            true
        } else {
            self.symlink_mode() == SymlinkMode::Preserve
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
            source: Some(SyncPath::Local {
                path: temp.path().to_path_buf(),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            min_size: None,
            max_size: None,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_validate_source_not_exists() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/nonexistent/path"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            min_size: None,
            max_size: None,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
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
            source: Some(SyncPath::Local {
                path: file_path.clone(),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        // Single file sync is now supported
        assert!(cli.validate().is_ok());
        assert!(cli.is_single_file());
    }

    #[test]
    fn test_validate_remote_source() {
        // Remote sources should not be validated locally
        let cli = Cli {
            source: Some(SyncPath::Remote {
                host: "server".to_string(),
                user: Some("user".to_string()),
                path: PathBuf::from("/remote/path"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_log_level_quiet() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: true,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::ERROR);
    }

    #[test]
    fn test_log_level_default() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::INFO);
    }

    #[test]
    fn test_log_level_verbose() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 1,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.log_level(), tracing::Level::DEBUG);
    }

    #[test]
    fn test_log_level_very_verbose() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 2,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
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
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: Some(1024 * 1024), // 1MB
            max_size: Some(500 * 1024),  // 500KB (smaller than min)
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        let result = cli.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min-size"));
    }

    #[test]
    fn test_verification_mode_default() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.verification_mode(), VerificationMode::Standard);
    }

    #[test]
    fn test_verification_mode_verify_flag_override() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Fast, // Set to Fast
            verify: true,                 // But --verify flag should override
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        // verify flag should override mode to Verify
        assert_eq!(cli.verification_mode(), VerificationMode::Verify);
    }

    #[test]
    fn test_verification_mode_checksum_type_mapping() {
        assert_eq!(VerificationMode::Fast.checksum_type(), ChecksumType::None);
        assert_eq!(
            VerificationMode::Standard.checksum_type(),
            ChecksumType::Fast
        );
        assert_eq!(
            VerificationMode::Verify.checksum_type(),
            ChecksumType::Cryptographic
        );
        assert_eq!(
            VerificationMode::Paranoid.checksum_type(),
            ChecksumType::Cryptographic
        );
    }

    #[test]
    fn test_verification_mode_verify_blocks() {
        assert!(!VerificationMode::Fast.verify_blocks());
        assert!(!VerificationMode::Standard.verify_blocks());
        assert!(!VerificationMode::Verify.verify_blocks());
        assert!(VerificationMode::Paranoid.verify_blocks());
    }

    #[test]
    fn test_symlink_mode_default() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Preserve);
    }

    #[test]
    fn test_symlink_mode_copy_links_override() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Skip, // Should be overridden
            copy_links: true,         // Override to Follow
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Follow);
    }

    #[test]
    fn test_symlink_mode_skip() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Skip,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Skip);
    }

    #[test]
    fn test_archive_mode_enables_all_flags() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: true, // Archive mode enabled
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        // Archive mode should enable all these flags
        assert!(cli.should_preserve_permissions());
        assert!(cli.should_preserve_times());
        assert!(cli.should_preserve_group());
        assert!(cli.should_preserve_owner());
        assert!(cli.should_preserve_devices());
        assert!(cli.should_preserve_symlinks());
    }

    #[test]
    fn test_individual_preserve_flags() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: true, // Only permissions enabled
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        // Only permissions should be enabled
        assert!(cli.should_preserve_permissions());
        assert!(!cli.should_preserve_times());
        assert!(!cli.should_preserve_group());
        assert!(!cli.should_preserve_owner());
        assert!(!cli.should_preserve_devices());
    }

    #[test]
    fn test_explicit_flag_overrides_with_archive() {
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: true, // Explicit flag also enabled
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: true, // Archive mode also enabled
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        // All should be enabled (archive mode OR individual flags)
        assert!(cli.should_preserve_permissions());
        assert!(cli.should_preserve_times());
        assert!(cli.should_preserve_group());
        assert!(cli.should_preserve_owner());
        assert!(cli.should_preserve_devices());
    }

    #[test]
    fn test_comparison_flags_mutually_exclusive() {
        // Test that --ignore-times and --size-only are mutually exclusive
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: true, // Both enabled - should fail
            size_only: true,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        let result = cli.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));
    }

    #[test]
    fn test_ignore_times_flag_alone() {
        let temp = TempDir::new().unwrap();
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: temp.path().to_path_buf(),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: true, // Only this flag enabled
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        // Should be valid - only one comparison flag
        assert!(cli.validate().is_ok());
        assert!(cli.ignore_times);
    }

    #[test]
    fn test_checksum_flag_alone() {
        let temp = TempDir::new().unwrap();
        let cli = Cli {
            source: Some(SyncPath::Local {
                path: temp.path().to_path_buf(),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: true, // Only this flag enabled
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        };

        // Should be valid - only one comparison flag
        assert!(cli.validate().is_ok());
        assert!(cli.checksum);
    }

    #[test]
    fn test_scan_options_default() {
        // Default: respect gitignore, exclude .git
        let cli = create_test_cli();
        let options = cli.scan_options();
        assert!(options.respect_gitignore);
        assert!(!options.include_git_dir);
    }

    #[test]
    fn test_scan_options_archive_mode() {
        // Archive mode: don't respect gitignore, include .git
        let mut cli = create_test_cli();
        cli.archive = true;
        let options = cli.scan_options();
        assert!(!options.respect_gitignore);
        assert!(options.include_git_dir);
    }

    #[test]
    fn test_scan_options_explicit_flags() {
        // Explicit flags work independently
        let mut cli = create_test_cli();
        cli.no_gitignore = true;
        cli.include_vcs = false;
        let options = cli.scan_options();
        assert!(!options.respect_gitignore);
        assert!(!options.include_git_dir);

        let mut cli = create_test_cli();
        cli.no_gitignore = false;
        cli.include_vcs = true;
        let options = cli.scan_options();
        assert!(options.respect_gitignore);
        assert!(options.include_git_dir);
    }

    // Helper to create a minimal test CLI
    fn create_test_cli() -> Cli {
        Cli {
            source: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/src"),
                has_trailing_slash: false,
            }),
            destination: Some(SyncPath::Local {
                path: PathBuf::from("/tmp/dest"),
                has_trailing_slash: false,
            }),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
            per_file_progress: false,
            parallel: 10,
            max_errors: 100,
            exclude: vec![],
            include: vec![],
            filter: vec![],
            exclude_from: None,
            include_from: None,
            ignore_template: vec![],
            bwlimit: None,
            compress: false,
            compression_detection: CompressionDetection::Auto,
            mode: VerificationMode::Standard,
            verify: false,
            resume: false,
            no_resume: false,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            no_gitignore: false,
            include_vcs: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            stream: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            bidirectional: false,
            conflict_resolve: "newer".to_string(),
            max_delete: 50,
            clear_bisync_state: false,
            force_resync: false,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
            retry: 3,
            retry_delay: 1,
            resume_only: false,
            clear_resume_state: false,
        }
    }
}
