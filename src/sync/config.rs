#![allow(dead_code)]
use crate::cli::SymlinkMode;
use crate::compress::CompressionDetection;
use crate::filter::FilterEngine;
use crate::integrity::ChecksumType;
use std::path::PathBuf;
pub use sy::engine::delete_plan::DeleteLimit;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub dry_run: bool,
    pub diff_mode: bool,
    pub delete: DeleteMode,
    #[allow(dead_code)]
    pub quiet: bool,
    pub max_concurrent: usize,
    pub max_errors: usize,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub filter_engine: FilterEngine,
    pub bwlimit: Option<u64>,
    pub json: bool,
    pub verification: VerificationConfig,
    pub preserve: PreserveConfig,
    pub progress: bool,
    pub comparison: ComparisonConfig,
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
    pub compress_level: Option<u8>,
    pub compression_detection: CompressionDetection,
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
            quiet: true,
            max_concurrent: 4,
            max_errors: 100,
            min_size: None,
            max_size: None,
            filter_engine: FilterEngine::new(),
            bwlimit: None,
            json: false,
            verification: VerificationConfig {
                mode: ChecksumType::Fast,
                verify_on_write: false,
            },
            preserve: PreserveConfig::default(),
            progress: false,
            comparison: ComparisonConfig::default(),
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
            compress_level: None,
            compression_detection: CompressionDetection::Never,
            itemize_changes: false,
            human_readable: false,
            stats: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DeleteMode {
    Disabled,
    Enabled { limit: DeleteLimit, force: bool },
}

impl DeleteMode {
    pub fn is_enabled(&self) -> bool {
        matches!(self, DeleteMode::Enabled { .. })
    }

    pub fn limit(&self) -> Option<DeleteLimit> {
        match self {
            DeleteMode::Disabled => None,
            DeleteMode::Enabled { limit, .. } => Some(*limit),
        }
    }

    pub fn is_forced(&self) -> bool {
        match self {
            DeleteMode::Disabled => false,
            DeleteMode::Enabled { force, .. } => *force,
        }
    }
}

pub fn parse_delete_limit(value: &str) -> std::result::Result<DeleteLimit, String> {
    if let Some(percent) = value.strip_suffix('%') {
        let percentage = percent
            .parse::<u8>()
            .map_err(|_| format!("--max-delete must be a number or percentage (got: '{value}')"))?;
        if percentage > 100 {
            return Err(format!(
                "--max-delete percentage must be between 0% and 100% (got: '{value}')"
            ));
        }
        return Ok(DeleteLimit::Percentage(percentage));
    }

    let count = value
        .parse::<u64>()
        .map_err(|_| format!("--max-delete must be a number or percentage (got: '{value}')"))?;
    if count == 0 {
        Ok(DeleteLimit::Unlimited)
    } else {
        Ok(DeleteLimit::Count(count))
    }
}

pub fn format_delete_limit(limit: DeleteLimit) -> String {
    match limit {
        DeleteLimit::Unlimited => "0".to_string(),
        DeleteLimit::Percentage(value) => format!("{value}%"),
        DeleteLimit::Count(value) => value.to_string(),
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
}

#[derive(Debug, Clone)]
pub struct PreserveConfig {
    pub xattrs: bool,
    pub hardlinks: bool,
    pub acls: bool,
    pub flags: bool,
    pub symlink_mode: SymlinkMode,
    pub permissions: bool,
    pub times: bool,
    pub group: bool,
    pub owner: bool,
    pub devices: bool,
    pub keep_dirlinks: bool,
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
            times: false,
            group: false,
            owner: false,
            devices: false,
            keep_dirlinks: false,
        }
    }
}

#[cfg(test)]
mod delete_limit_tests {
    use super::*;

    #[test]
    fn parses_typed_delete_limits_without_ambiguous_zero() {
        assert_eq!(parse_delete_limit("0").unwrap(), DeleteLimit::Unlimited);
        assert_eq!(format_delete_limit(DeleteLimit::Unlimited), "0");
        assert_eq!(format_delete_limit(DeleteLimit::Percentage(50)), "50%");
        assert_eq!(format_delete_limit(DeleteLimit::Count(1000)), "1000");
        assert_eq!(
            parse_delete_limit("0%").unwrap(),
            DeleteLimit::Percentage(0)
        );
        assert_eq!(
            parse_delete_limit("50%").unwrap(),
            DeleteLimit::Percentage(50)
        );
        assert_eq!(
            parse_delete_limit("1000").unwrap(),
            DeleteLimit::Count(1000)
        );
    }

    #[test]
    fn rejects_invalid_delete_limits() {
        assert!(parse_delete_limit("101%").is_err());
        assert!(parse_delete_limit("%").is_err());
        assert!(parse_delete_limit("many").is_err());
    }
}
