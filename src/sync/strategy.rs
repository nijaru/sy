use super::scanner::FileEntry;
use crate::transport::Transport;
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
}

impl StrategyPlanner {
    pub fn new() -> Self {
        Self {
            mtime_tolerance: 1, // 1 second tolerance for mtime comparison
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
            // For files, check existence and metadata
            match transport.metadata(&dest_path).await {
                Ok(dest_meta) => {
                    let needs_update = self.needs_update(source, &dest_meta);
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
                    let needs_update = self.needs_update(source, &dest_meta);
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
    fn needs_update(&self, source: &FileEntry, dest_meta: &std::fs::Metadata) -> bool {
        // Different size = needs update
        if source.size != dest_meta.len() {
            return true;
        }

        // Check mtime with tolerance
        if let Ok(dest_mtime) = dest_meta.modified() {
            if !self.mtime_matches(&source.modified, &dest_mtime) {
                return true;
            }
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
        };

        let planner = StrategyPlanner::new();
        let task = planner.plan_file(&source_file, dest_root);

        assert_eq!(task.action, SyncAction::Update);
    }
}
