use crate::path::SyncPath;
use clap::{Parser, ValueEnum};

// Import integrity types for verification modes
use crate::integrity::ChecksumType;

// Import compression types for detection modes
use crate::compress::CompressionDetection;

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

    Ok((num * multiplier as f64) as u64)
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SymlinkMode {
    /// Preserve symlinks as symlinks (default)
    Preserve,

    /// Follow symlinks and copy targets
    Follow,

    /// Skip all symlinks
    Skip,
}

impl Default for SymlinkMode {
    fn default() -> Self {
        Self::Preserve
    }
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

    # Verification modes
    sy /source /destination --verify            # BLAKE3 cryptographic verification
    sy /source /destination --mode paranoid     # Maximum reliability

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
    #[arg(long)]
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
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    pub resume: bool,

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

    /// Preserve BSD file flags (macOS: hidden, immutable, nodump, etc.)
    #[cfg(target_os = "macos")]
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

    /// Archive mode (equivalent to -rlptgoD: recursive, links, perms, times, group, owner, devices)
    /// Note: Does NOT include -X (xattrs), -A (ACLs), or -H (hardlinks) - use those flags separately
    #[arg(short = 'a', long)]
    pub archive: bool,

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
            source: Some(SyncPath::Local(temp.path().to_path_buf())),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_validate_source_not_exists() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/nonexistent/path"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
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
            source: Some(SyncPath::Local(file_path.clone())),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
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
            source: Some(SyncPath::Remote {
                host: "server".to_string(),
                user: Some("user".to_string()),
                path: PathBuf::from("/remote/path"),
            }),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert!(cli.validate().is_ok());
    }

    #[test]
    fn test_log_level_quiet() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: true,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::ERROR);
    }

    #[test]
    fn test_log_level_default() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::INFO);
    }

    #[test]
    fn test_log_level_verbose() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 1,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.log_level(), tracing::Level::DEBUG);
    }

    #[test]
    fn test_log_level_very_verbose() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 2,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
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
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: Some(1024 * 1024), // 1MB
            max_size: Some(500 * 1024),  // 500KB (smaller than min)
        };

        let result = cli.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min-size"));
    }

    #[test]
    fn test_verification_mode_default() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.verification_mode(), VerificationMode::Standard);
    }

    #[test]
    fn test_verification_mode_verify_flag_override() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
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
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Preserve);
    }

    #[test]
    fn test_symlink_mode_copy_links_override() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Skip, // Should be overridden
            copy_links: true,         // Override to Follow
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Follow);
    }

    #[test]
    fn test_symlink_mode_skip() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Skip,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };
        assert_eq!(cli.symlink_mode(), SymlinkMode::Skip);
    }

    #[test]
    fn test_archive_mode_enables_all_flags() {
        let cli = Cli {
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: true, // Archive mode enabled
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
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
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: true, // Only permissions enabled
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
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
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: true, // Explicit flag also enabled
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: true, // Archive mode also enabled
            ignore_times: false,
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
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
            source: Some(SyncPath::Local(PathBuf::from("/tmp/src"))),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: true, // Both enabled - should fail
            size_only: true,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
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
            source: Some(SyncPath::Local(temp.path().to_path_buf())),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: true, // Only this flag enabled
            size_only: false,
            checksum: false,
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };

        // Should be valid - only one comparison flag
        assert!(cli.validate().is_ok());
        assert!(cli.ignore_times);
    }

    #[test]
    fn test_checksum_flag_alone() {
        let temp = TempDir::new().unwrap();
        let cli = Cli {
            source: Some(SyncPath::Local(temp.path().to_path_buf())),
            destination: Some(SyncPath::Local(PathBuf::from("/tmp/dest"))),
            dry_run: false,
            diff: false,
            delete: false,
            delete_threshold: 50,
            trash: false,
            force_delete: false,
            verbose: 0,
            quiet: false,
            perf: false,
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
            resume: true,
            checkpoint_files: 10,
            checkpoint_bytes: 104857600,
            clean_state: false,
            links: SymlinkMode::Preserve,
            copy_links: false,
            preserve_xattrs: false,
            preserve_hardlinks: false,
            preserve_acls: false,
            #[cfg(target_os = "macos")]
            preserve_flags: false,
            preserve_permissions: false,
            preserve_times: false,
            preserve_group: false,
            preserve_owner: false,
            preserve_devices: false,
            archive: false,
            ignore_times: false,
            size_only: false,
            checksum: true, // Only this flag enabled
            verify_only: false,
            json: false,
            watch: false,
            no_hooks: false,
            abort_on_hook_failure: false,
            profile: None,
            list_profiles: false,
            show_profile: None,
            use_cache: false,
            clear_cache: false,
            checksum_db: false,
            clear_checksum_db: false,
            prune_checksum_db: false,
            min_size: None,
            max_size: None,
        };

        // Should be valid - only one comparison flag
        assert!(cli.validate().is_ok());
        assert!(cli.checksum);
    }
}
