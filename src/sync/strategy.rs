use super::scanner::FileEntry;
use crate::transport::{Transport, FileInfo};
use crate::error::Result;
use std::path::Path;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// Skip - file unchanged
    Skip,
    /// Create - new file or directory
    Create,
    /// Update - file exists but differs
    Update,
    /// Delete - file exists in destination but not source
    Delete,
}

#[derive(Debug)]
pub struct SyncTask {
    pub source: Option<FileEntry>,
    pub dest_path: std::path::PathBuf,
    pub action: SyncAction,
}

pub struct StrategyPlanner {
    /// mtime tolerance in seconds (to handle filesystem granularity)
    mtime_tolerance: u64,
    /// Ignore modification times, always compare checksums
    ignore_times: bool,
    /// Only compare file size, skip mtime checks
    size_only: bool,
    /// Always compare checksums instead of size+mtime
    checksum: bool,
}

impl StrategyPlanner {
    pub fn new() -> Self {
        Self {
            mtime_tolerance: 1, // 1 second tolerance for mtime comparison
            ignore_times: false,
            size_only: false,
            checksum: false,
        }
    }

    /// Create a new planner with custom comparison flags
    pub fn with_comparison_flags(ignore_times: bool, size_only: bool, checksum: bool) -> Self {
        Self {
            mtime_tolerance: 1,
            ignore_times,
            size_only,
            checksum,
        }
    }

    /// Determine sync action for a source file (async version using transport)
    pub async fn plan_file_async<T: Transport>(
        &self,
        source: &FileEntry,
        dest_root: &Path,
        transport: &T,
    ) -> Result<SyncTask> {
        let dest_path = dest_root.join(&source.relative_path);

        let action = if source.is_dir {
            // For directories, just check existence (no metadata needed)
            let exists = transport.exists(&dest_path).await.unwrap_or(false);
            if exists {
                SyncAction::Skip
            } else {
                SyncAction::Create
            }
        } else {
            // For files, check existence and file info
            match transport.file_info(&dest_path).await {
                Ok(dest_info) => {
                    let needs_update = self.needs_update(source, &dest_info);
                    if needs_update {
                        SyncAction::Update
                    } else {
                        SyncAction::Skip
                    }
                }
                Err(_) => SyncAction::Create,
            }
        };

        Ok(SyncTask {
            source: Some(source.clone()),
            dest_path,
            action,
        })
    }

    /// Determine sync action for a source file (sync version for local-only)
    #[allow(dead_code)]
    pub fn plan_file(&self, source: &FileEntry, dest_root: &Path) -> SyncTask {
        let dest_path = dest_root.join(&source.relative_path);

        let action = if source.is_dir {
            // For directories, just check existence (no metadata needed)
            if dest_path.exists() {
                SyncAction::Skip
            } else {
                SyncAction::Create
            }
        } else {
            // For files, check existence and metadata
            match std::fs::metadata(&dest_path) {
                Ok(dest_meta) => {
                    // Convert Metadata to FileInfo for comparison
                    let dest_info = FileInfo {
                        size: dest_meta.len(),
                        modified: dest_meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    };
                    let needs_update = self.needs_update(source, &dest_info);
                    if needs_update {
                        SyncAction::Update
                    } else {
                        SyncAction::Skip
                    }
                }
                Err(_) => SyncAction::Create,
            }
        };

        SyncTask {
            source: Some(source.clone()),
            dest_path,
            action,
        }
    }

    /// Check if file needs update based on size and mtime
    fn needs_update(&self, source: &FileEntry, dest_info: &FileInfo) -> bool {
        // Handle comparison flags

        // --checksum: Always return true to force checksum comparison
        // (actual checksum verification happens during transfer)
        if self.checksum {
            return true;
        }

        // --ignore-times: Skip mtime checks, only compare size
        // (if sizes match, still force transfer to compare checksums)
        if self.ignore_times {
            if source.size != dest_info.size {
                return true;  // Different size = definitely needs update
            }
            return true;  // Same size but ignore mtime = force checksum comparison
        }

        // --size-only: Only compare file size, skip mtime checks
        if self.size_only {
            return source.size != dest_info.size;
        }

        // Default behavior: compare size + mtime

        // Different size = needs update
        if source.size != dest_info.size {
            return true;
        }

        // Check mtime with tolerance
        if !self.mtime_matches(&source.modified, &dest_info.modified) {
            return true;
        }

        false
    }

    /// Check if mtimes match within tolerance
    fn mtime_matches(&self, source_mtime: &SystemTime, dest_mtime: &SystemTime) -> bool {
        match source_mtime.duration_since(*dest_mtime) {
            Ok(duration) => duration.as_secs() <= self.mtime_tolerance,
            Err(e) => e.duration().as_secs() <= self.mtime_tolerance,
        }
    }

    /// Find files to delete (in destination but not in source)
    pub fn plan_deletions(&self, source_files: &[FileEntry], dest_root: &Path) -> Vec<SyncTask> {
        let mut deletions = Vec::new();

        // Build set of source paths for quick lookup
        let source_paths: std::collections::HashSet<_> = source_files
            .iter()
            .map(|f| f.relative_path.clone())
            .collect();

        // Scan destination
        if let Ok(dest_scanner) = crate::sync::scanner::Scanner::new(dest_root).scan() {
            for dest_file in dest_scanner {
                if !source_paths.contains(&dest_file.relative_path) {
                    deletions.push(SyncTask {
                        source: None,
                        dest_path: dest_file.path,
                        action: SyncAction::Delete,
                    });
                }
            }
        }

        deletions
    }
}

impl Default for StrategyPlanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_plan_create() {
        let temp = TempDir::new().unwrap();
        let dest_root = temp.path();

        let source_file = FileEntry {
            path: PathBuf::from("/source/file.txt"),
            relative_path: PathBuf::from("file.txt"),
            size: 100,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 100,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let planner = StrategyPlanner::new();
        let task = planner.plan_file(&source_file, dest_root);

        assert_eq!(task.action, SyncAction::Create);
    }

    #[test]
    fn test_plan_skip_identical() {
        let temp = TempDir::new().unwrap();
        let dest_root = temp.path();

        // Create destination file
        fs::write(dest_root.join("file.txt"), "content").unwrap();

        let source_file = FileEntry {
            path: PathBuf::from("/source/file.txt"),
            relative_path: PathBuf::from("file.txt"),
            size: 7, // "content".len()
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 7,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let planner = StrategyPlanner::new();
        let task = planner.plan_file(&source_file, dest_root);

        assert_eq!(task.action, SyncAction::Skip);
    }

    #[test]
    fn test_plan_update_different_size() {
        let temp = TempDir::new().unwrap();
        let dest_root = temp.path();

        // Create destination file with different content
        fs::write(dest_root.join("file.txt"), "old").unwrap();

        let source_file = FileEntry {
            path: PathBuf::from("/source/file.txt"),
            relative_path: PathBuf::from("file.txt"),
            size: 100, // Different size
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 100,
            xattrs: None,
            inode: None,
            nlink: 1,
                acls: None,
        };

        let planner = StrategyPlanner::new();
        let task = planner.plan_file(&source_file, dest_root);

        assert_eq!(task.action, SyncAction::Update);
    }
}
