#![allow(dead_code)]
use crate::cli::SymlinkMode;
use crate::filter::FilterEngine;
use crate::integrity::ChecksumType;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub dry_run: bool,
    pub diff_mode: bool,
    pub delete: DeleteMode,
    #[allow(dead_code)]
    pub trash: bool,
    pub quiet: bool,
    pub max_concurrent: usize,
    pub max_errors: usize,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub filter_engine: FilterEngine,
    pub bwlimit: Option<u64>,
    pub resume: ResumeConfig,
    pub json: bool,
    pub verification: VerificationConfig,
    pub preserve: PreserveConfig,
    pub progress: bool,
    pub comparison: ComparisonConfig,
    pub cache: bool,
    pub clear_cache: bool,
    pub dest_is_remote: bool,
    pub perf: bool,
    // rsync-compat flags
    pub remove_source_files: bool,
    pub existing: bool,
    pub dirs: bool,
    pub backup: Option<String>,
    pub backup_dir: Option<PathBuf>,
    pub suffix: String,
    pub partial: Option<String>,
    pub partial_dir: Option<PathBuf>,
    pub timeout: Option<u64>,
    pub contimeout: Option<u64>,
    pub rsh: Option<String>,
    pub compress_level: Option<u8>,
    pub itemize_changes: bool,
    pub human_readable: bool,
    pub stats: bool,
}

impl SyncConfig {
    #[allow(dead_code)]
    pub fn test_default() -> Self {
        Self {
            dry_run: false,
            diff_mode: false,
            delete: DeleteMode::Disabled,
            trash: false,
            quiet: true,
            max_concurrent: 4,
            max_errors: 100,
            min_size: None,
            max_size: None,
            filter_engine: FilterEngine::new(),
            bwlimit: None,
            resume: ResumeConfig::disabled(),
            json: false,
            verification: VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
                checksum_db: false,
                clear_checksum_db: false,
                prune_checksum_db: false,
            },
            preserve: PreserveConfig::default(),
            progress: false,
            comparison: ComparisonConfig::default(),
            cache: false,
            clear_cache: false,
            dest_is_remote: false,
            perf: false,
            // rsync-compat flags
            remove_source_files: false,
            existing: false,
            dirs: false,
            backup: None,
            backup_dir: None,
            suffix: "~".to_string(),
            partial: None,
            partial_dir: None,
            timeout: None,
            contimeout: None,
            rsh: None,
            compress_level: None,
            itemize_changes: false,
            human_readable: false,
            stats: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DeleteMode {
    Disabled,
    Enabled { threshold: u8, force: bool },
}

impl DeleteMode {
    pub fn is_enabled(&self) -> bool {
        matches!(self, DeleteMode::Enabled { .. })
    }

    pub fn threshold(&self) -> u8 {
        match self {
            DeleteMode::Disabled => 0,
            DeleteMode::Enabled { threshold, .. } => *threshold,
        }
    }

    pub fn is_forced(&self) -> bool {
        match self {
            DeleteMode::Disabled => false,
            DeleteMode::Enabled { force, .. } => *force,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ComparisonConfig {
    pub ignore_times: bool,
    pub size_only: bool,
    pub checksum: bool,
    pub update_only: bool,
    pub ignore_existing: bool,
}

#[derive(Debug, Clone, Default)]
pub struct VerificationConfig {
    pub mode: ChecksumType,
    pub verify_on_write: bool,
    pub checksum_db: bool,
    pub clear_checksum_db: bool,
    pub prune_checksum_db: bool,
}

#[derive(Debug, Clone)]
pub struct PreserveConfig {
    pub xattrs: bool,
    pub hardlinks: bool,
    pub acls: bool,
    pub flags: bool,
    pub symlink_mode: SymlinkMode,
    pub permissions: bool,
}

impl Default for PreserveConfig {
    fn default() -> Self {
        Self {
            xattrs: false,
            hardlinks: false,
            acls: false,
            flags: false,
            symlink_mode: SymlinkMode::Preserve,
            permissions: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResumeConfig {
    pub enabled: bool,
    pub only: bool,
    pub checkpoint_files: usize,
    pub checkpoint_bytes: u64,
}

impl ResumeConfig {
    #[allow(dead_code)]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            only: false,
            checkpoint_files: 0,
            checkpoint_bytes: 0,
        }
    }
}
