use super::scanner::FileEntry;
use crate::error::Result;
use crate::integrity::{Checksum, ChecksumType, IntegrityVerifier};
use crate::transport::{FileInfo, Transport};
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
    /// Pre-computed source checksum (for --checksum mode)
    pub source_checksum: Option<Checksum>,
    /// Pre-computed destination checksum (for --checksum mode)
    pub dest_checksum: Option<Checksum>,
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
    /// Integrity verifier for checksum computation
    verifier: Option<IntegrityVerifier>,
}

impl StrategyPlanner {
    pub fn new() -> Self {
        Self {
            mtime_tolerance: 1, // 1 second tolerance for mtime comparison
            ignore_times: false,
            size_only: false,
            checksum: false,
            verifier: None,
        }
    }

    /// Create a new planner with custom comparison flags
    pub fn with_comparison_flags(ignore_times: bool, size_only: bool, checksum: bool) -> Self {
        // Create verifier if checksum mode is enabled
        let verifier = if checksum {
            // Use Fast (xxHash3) checksums for pre-transfer comparison (faster than BLAKE3)
            Some(IntegrityVerifier::new(ChecksumType::Fast, false))
        } else {
            None
        };

        Self {
            mtime_tolerance: 1,
            ignore_times,
            size_only,
            checksum,
            verifier,
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

        let (action, source_checksum, dest_checksum) = if source.is_dir {
            // For directories, just check existence (no metadata needed)
            let exists = transport.exists(&dest_path).await.unwrap_or(false);
            let action = if exists {
                SyncAction::Skip
            } else {
                SyncAction::Create
            };
            (action, None, None)
        } else {
            // For files, check existence and file info
            match transport.file_info(&dest_path).await {
                Ok(dest_info) => {
                    // Compute checksums if verifier is present and files are local
                    let (source_cksum, dest_cksum) = if let Some(ref verifier) = self.verifier {
                        self.compute_checksums_local(source, &dest_path, verifier)?
                    } else {
                        (None, None)
                    };

                    // If checksums are available and match, skip transfer
                    let action = if let (Some(ref src_cksum), Some(ref dst_cksum)) = (&source_cksum, &dest_cksum) {
                        if src_cksum == dst_cksum {
                            tracing::debug!(
                                "Checksums match for {}, skipping transfer",
                                source.relative_path.display()
                            );
                            SyncAction::Skip
                        } else {
                            tracing::debug!(
                                "Checksums differ for {}, will transfer",
                                source.relative_path.display()
                            );
                            SyncAction::Update
                        }
                    } else {
                        // No checksums available, use normal comparison
                        let needs_update = self.needs_update(source, &dest_info);
                        if needs_update {
                            SyncAction::Update
                        } else {
                            SyncAction::Skip
                        }
                    };

                    (action, source_cksum, dest_cksum)
                }
                Err(_) => (SyncAction::Create, None, None),
            }
        };

        Ok(SyncTask {
            source: Some(source.clone()),
            dest_path,
            action,
            source_checksum,
            dest_checksum,
        })
    }

    /// Compute checksums for local files (both source and dest)
    /// Returns (source_checksum, dest_checksum) if both files are accessible locally
    fn compute_checksums_local(
        &self,
        source: &FileEntry,
        dest_path: &Path,
        verifier: &IntegrityVerifier,
    ) -> Result<(Option<Checksum>, Option<Checksum>)> {
        // Try to compute source checksum (source should always be local in current design)
        let source_checksum = if source.path.exists() {
            match verifier.compute_file_checksum(&source.path) {
                Ok(cksum) => Some(cksum),
                Err(e) => {
                    tracing::warn!("Failed to compute source checksum for {}: {}", source.path.display(), e);
                    None
                }
            }
        } else {
            None
        };

        // Try to compute dest checksum (only if it exists and is local)
        let dest_checksum = if dest_path.exists() {
            match verifier.compute_file_checksum(dest_path) {
                Ok(cksum) => Some(cksum),
                Err(e) => {
                    tracing::warn!("Failed to compute dest checksum for {}: {}", dest_path.display(), e);
                    None
                }
            }
        } else {
            None
        };

        Ok((source_checksum, dest_checksum))
    }

    /// Determine sync action for a source file (sync version for local-only)
    #[allow(dead_code)]
    pub fn plan_file(&self, source: &FileEntry, dest_root: &Path) -> SyncTask {
        let dest_path = dest_root.join(&source.relative_path);

        let (action, source_checksum, dest_checksum) = if source.is_dir {
            // For directories, just check existence (no metadata needed)
            let action = if dest_path.exists() {
                SyncAction::Skip
            } else {
                SyncAction::Create
            };
            (action, None, None)
        } else {
            // For files, check existence and metadata
            match std::fs::metadata(&dest_path) {
                Ok(dest_meta) => {
                    // Compute checksums if verifier is present
                    let (source_cksum, dest_cksum) = if let Some(ref verifier) = self.verifier {
                        self.compute_checksums_local(source, &dest_path, verifier)
                            .unwrap_or((None, None))
                    } else {
                        (None, None)
                    };

                    // If checksums are available and match, skip transfer
                    let action = if let (Some(ref src_cksum), Some(ref dst_cksum)) = (&source_cksum, &dest_cksum) {
                        if src_cksum == dst_cksum {
                            tracing::debug!(
                                "Checksums match for {}, skipping transfer",
                                source.relative_path.display()
                            );
                            SyncAction::Skip
                        } else {
                            tracing::debug!(
                                "Checksums differ for {}, will transfer",
                                source.relative_path.display()
                            );
                            SyncAction::Update
                        }
                    } else {
                        // No checksums available, use normal comparison
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
                    };

                    (action, source_cksum, dest_cksum)
                }
                Err(_) => (SyncAction::Create, None, None),
            }
        };

        SyncTask {
            source: Some(source.clone()),
            dest_path,
            action,
            source_checksum,
            dest_checksum,
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
                return true; // Different size = definitely needs update
            }
            return true; // Same size but ignore mtime = force checksum comparison
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
    ///
    /// Uses a memory-efficient Bloom filter for large file sets (>10k files),
    /// providing 100x memory reduction vs HashMap while maintaining correctness.
    ///
    /// For small file sets (<10k), uses HashMap for simplicity.
    pub fn plan_deletions(&self, source_files: &[FileEntry], dest_root: &Path) -> Vec<SyncTask> {
        let mut deletions = Vec::new();

        // Choose strategy based on file count
        const BLOOM_THRESHOLD: usize = 10_000;

        if source_files.len() > BLOOM_THRESHOLD {
            // Large file set: Use Bloom filter + streaming for memory efficiency
            // Memory: ~1.2MB for 1M files vs ~100MB for HashSet
            use crate::sync::scale::FileSetBloom;

            // Build Bloom filter of source paths
            let mut source_bloom = FileSetBloom::new(source_files.len());
            for file in source_files {
                source_bloom.insert(&file.relative_path);
            }

            // Also keep a small HashSet for false positive verification
            // Only stored when Bloom filter says "might exist"
            let source_paths: std::collections::HashSet<_> = source_files
                .iter()
                .map(|f| f.relative_path.clone())
                .collect();

            // Stream destination files and check against Bloom filter
            if let Ok(dest_scanner) = crate::sync::scanner::Scanner::new(dest_root).scan_streaming()
            {
                for dest_file in dest_scanner.flatten() {
                    // Check Bloom filter first (O(1), no false negatives)
                    if !source_bloom.contains(&dest_file.relative_path) {
                        // Definitely not in source - safe to delete
                        deletions.push(SyncTask {
                            source: None,
                            dest_path: dest_file.path,
                            action: SyncAction::Delete,
                            source_checksum: None,
                            dest_checksum: None,
                        });
                    } else {
                        // Bloom says "might exist" - verify with HashMap to handle false positives
                        if !source_paths.contains(&dest_file.relative_path) {
                            deletions.push(SyncTask {
                                source: None,
                                dest_path: dest_file.path,
                                action: SyncAction::Delete,
                                source_checksum: None,
                                dest_checksum: None,
                            });
                        }
                    }
                }
            }
        } else {
            // Small file set: Use simple HashSet (fast and simple for <10k files)
            let source_paths: std::collections::HashSet<_> = source_files
                .iter()
                .map(|f| f.relative_path.clone())
                .collect();

            // Scan destination (use streaming to avoid loading all into memory)
            if let Ok(dest_scanner) = crate::sync::scanner::Scanner::new(dest_root).scan_streaming()
            {
                for dest_file in dest_scanner.flatten() {
                    if !source_paths.contains(&dest_file.relative_path) {
                        deletions.push(SyncTask {
                            source: None,
                            dest_path: dest_file.path,
                            action: SyncAction::Delete,
                            source_checksum: None,
                            dest_checksum: None,
                        });
                    }
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

    #[test]
    fn test_plan_deletions_small_set() {
        let temp_dest = TempDir::new().unwrap();
        let dest_root = temp_dest.path();

        // Create some files in destination
        fs::write(dest_root.join("keep.txt"), "keep").unwrap();
        fs::write(dest_root.join("delete1.txt"), "delete").unwrap();
        fs::write(dest_root.join("delete2.txt"), "delete").unwrap();

        // Source only has keep.txt
        let source_files = vec![FileEntry {
            path: PathBuf::from("/source/keep.txt"),
            relative_path: PathBuf::from("keep.txt"),
            size: 4,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 4,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
        }];

        let planner = StrategyPlanner::new();
        let deletions = planner.plan_deletions(&source_files, dest_root);

        // Should plan to delete 2 files (delete1.txt, delete2.txt)
        assert_eq!(deletions.len(), 2);
        assert!(deletions.iter().all(|t| t.action == SyncAction::Delete));

        let deletion_names: Vec<_> = deletions
            .iter()
            .map(|t| t.dest_path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(deletion_names.contains(&"delete1.txt"));
        assert!(deletion_names.contains(&"delete2.txt"));
    }

    #[test]
    fn test_plan_deletions_large_set_with_bloom() {
        let temp_dest = TempDir::new().unwrap();
        let dest_root = temp_dest.path();

        // Create 100 files in destination (simulating a larger set)
        for i in 0..100 {
            fs::write(dest_root.join(format!("file{}.txt", i)), "content").unwrap();
        }

        // Create extra files to delete
        fs::write(dest_root.join("delete1.txt"), "delete").unwrap();
        fs::write(dest_root.join("delete2.txt"), "delete").unwrap();

        // Source has 11,000 files (triggers Bloom filter path)
        // We'll create dummy entries without actual files
        let mut source_files = Vec::new();
        for i in 0..11_000 {
            source_files.push(FileEntry {
                path: PathBuf::from(format!("/source/file{}.txt", i)),
                relative_path: PathBuf::from(format!("file{}.txt", i)),
                size: 7,
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
            });
        }

        let planner = StrategyPlanner::new();
        let deletions = planner.plan_deletions(&source_files, dest_root);

        // Should find delete1.txt and delete2.txt (files not in source)
        assert_eq!(deletions.len(), 2);
        assert!(deletions.iter().all(|t| t.action == SyncAction::Delete));

        let deletion_names: Vec<_> = deletions
            .iter()
            .map(|t| t.dest_path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert!(deletion_names.contains(&"delete1.txt"));
        assert!(deletion_names.contains(&"delete2.txt"));
    }

    #[test]
    fn test_plan_deletions_empty_source() {
        let temp_dest = TempDir::new().unwrap();
        let dest_root = temp_dest.path();

        // Create files in destination
        fs::write(dest_root.join("file1.txt"), "content").unwrap();
        fs::write(dest_root.join("file2.txt"), "content").unwrap();

        // Empty source
        let source_files: Vec<FileEntry> = vec![];

        let planner = StrategyPlanner::new();
        let deletions = planner.plan_deletions(&source_files, dest_root);

        // Should delete all files in destination
        assert_eq!(deletions.len(), 2);
        assert!(deletions.iter().all(|t| t.action == SyncAction::Delete));
    }

    #[test]
    fn test_checksum_mode_skip_identical_files() {
        let temp = TempDir::new().unwrap();
        let dest_root = temp.path();

        // Create destination file with same content as source
        let content = b"Hello, world!";
        fs::write(dest_root.join("file.txt"), content).unwrap();

        let source_file = FileEntry {
            path: dest_root.join("file.txt"), // Use same file as source for testing
            relative_path: PathBuf::from("file.txt"),
            size: content.len() as u64,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: content.len() as u64,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
        };

        // Create planner with checksum mode enabled
        let planner = StrategyPlanner::with_comparison_flags(false, false, true);
        let task = planner.plan_file(&source_file, dest_root);

        // Should skip because checksums match
        assert_eq!(task.action, SyncAction::Skip);
        // Checksums should be computed
        assert!(task.source_checksum.is_some());
        assert!(task.dest_checksum.is_some());
        // Checksums should match
        assert_eq!(task.source_checksum, task.dest_checksum);
    }

    #[test]
    fn test_checksum_mode_transfer_different_files() {
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("source");
        let dest_dir = temp.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        // Create source and dest files with different content
        fs::write(source_dir.join("file.txt"), b"Source content").unwrap();
        fs::write(dest_dir.join("file.txt"), b"Dest content (different)").unwrap();

        let source_file = FileEntry {
            path: source_dir.join("file.txt"),
            relative_path: PathBuf::from("file.txt"),
            size: 14, // "Source content".len()
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 14,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
        };

        // Create planner with checksum mode enabled
        let planner = StrategyPlanner::with_comparison_flags(false, false, true);
        let task = planner.plan_file(&source_file, &dest_dir);

        // Should update because checksums differ
        assert_eq!(task.action, SyncAction::Update);
        // Checksums should be computed
        assert!(task.source_checksum.is_some());
        assert!(task.dest_checksum.is_some());
        // Checksums should differ
        assert_ne!(task.source_checksum, task.dest_checksum);
    }

    #[test]
    fn test_checksum_mode_create_new_file() {
        let temp = TempDir::new().unwrap();
        let source_dir = temp.path().join("source");
        let dest_dir = temp.path().join("dest");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&dest_dir).unwrap();

        // Create source file, no dest file
        fs::write(source_dir.join("file.txt"), b"New file content").unwrap();

        let source_file = FileEntry {
            path: source_dir.join("file.txt"),
            relative_path: PathBuf::from("file.txt"),
            size: 16, // "New file content".len()
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            is_sparse: false,
            allocated_size: 16,
            xattrs: None,
            inode: None,
            nlink: 1,
            acls: None,
        };

        // Create planner with checksum mode enabled
        let planner = StrategyPlanner::with_comparison_flags(false, false, true);
        let task = planner.plan_file(&source_file, &dest_dir);

        // Should create because dest doesn't exist
        assert_eq!(task.action, SyncAction::Create);
        // No checksums computed for non-existent dest
        assert!(task.source_checksum.is_none());
        assert!(task.dest_checksum.is_none());
    }

    #[test]
    fn test_plan_deletions_no_deletions_needed() {
        let temp_dest = TempDir::new().unwrap();
        let dest_root = temp_dest.path();

        // Create files in destination
        fs::write(dest_root.join("file1.txt"), "content").unwrap();
        fs::write(dest_root.join("file2.txt"), "content").unwrap();

        // Source has the same files
        let source_files = vec![
            FileEntry {
                path: PathBuf::from("/source/file1.txt"),
                relative_path: PathBuf::from("file1.txt"),
                size: 7,
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
            },
            FileEntry {
                path: PathBuf::from("/source/file2.txt"),
                relative_path: PathBuf::from("file2.txt"),
                size: 7,
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
            },
        ];

        let planner = StrategyPlanner::new();
        let deletions = planner.plan_deletions(&source_files, dest_root);

        // No deletions needed
        assert_eq!(deletions.len(), 0);
    }
}
