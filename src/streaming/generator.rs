//! Generator task for streaming sync.
//!
//! Scans source directory and streams file metadata to Sender.
//! Receives destination state during Initial Exchange.

use crate::filter::FilterEngine;
use crate::streaming::channel::{
    DeltaInfo, DestFileState, DestIndex, FileJob, FileJobSender, GeneratorMessage, DELTA_MIN_SIZE,
};
use crate::streaming::protocol::{DestFileEntry, DestFileFlags};
use crate::sync::scanner::{ScanOptions, Scanner};
use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Generator configuration
pub struct GeneratorConfig {
    /// Root path to scan
    pub root: PathBuf,
    /// Whether to include hidden files
    pub include_hidden: bool,
    /// Whether to follow symlinks
    pub follow_symlinks: bool,
    /// Whether --delete is enabled
    pub delete_enabled: bool,
    /// Whether --force-delete bypasses threshold
    pub force_delete: bool,
    /// Maximum deletions (absolute count or percentage string like "50%")
    pub max_delete: Option<String>,
    /// Filter engine for --exclude/--include/--filter
    pub filter: Option<FilterEngine>,
    /// Scanner options for gitignore, VCS directory, and dirs-only behavior
    pub scan_options: ScanOptions,
}

/// Generator state
pub struct Generator {
    config: GeneratorConfig,
    dest_index: DestIndex,
    seen_inodes: HashMap<u64, Arc<PathBuf>>, // For hard link detection
    excluded_dest_dirs: Vec<PathBuf>,
}

impl Generator {
    pub fn new(config: GeneratorConfig) -> Self {
        Self {
            config,
            dest_index: DestIndex::new(),
            seen_inodes: HashMap::new(),
            excluded_dest_dirs: Vec::new(),
        }
    }

    /// Process a DEST_FILE_ENTRY received during Initial Exchange.
    /// Call this for each entry before starting the scan.
    pub fn add_dest_entry(&mut self, entry: DestFileEntry) {
        // Apply filter to dest entries too (e.g., --exclude-vcs). If a directory
        // is excluded, exclude all children as rsync users expect (`--exclude .git`).
        let path = PathBuf::from(&entry.path);
        if self
            .excluded_dest_dirs
            .iter()
            .any(|dir| path.starts_with(dir))
        {
            return;
        }
        if let Some(ref filter) = self.config.filter {
            let is_dir = entry.flags.contains(DestFileFlags::DIR);
            if filter.should_exclude(&path, is_dir) {
                if is_dir {
                    self.excluded_dest_dirs.push(path);
                }
                return;
            }
        }

        let delta_info = if entry.flags.contains(DestFileFlags::HAS_CHECKSUMS) {
            Some(DeltaInfo {
                block_size: entry.block_size,
                file_size: entry.size,
                checksums: entry.checksums,
            })
        } else {
            None
        };

        self.dest_index.insert(
            entry.path,
            DestFileState {
                size: entry.size,
                mtime: entry.mtime,
                mode: entry.mode,
                is_dir: entry.flags.contains(DestFileFlags::DIR),
                delta_info,
            },
        );
    }

    /// Called after all DEST_FILE_ENTRY received (after DEST_FILE_END).
    pub fn dest_count(&self) -> usize {
        self.dest_index.len()
    }

    /// Run the generator, scanning source and sending to channel.
    /// Returns (total_files, total_bytes).
    pub async fn run(mut self, tx: FileJobSender) -> Result<(u64, u64)> {
        let scanner = Scanner::new(&self.config.root)
            .follow_links(self.config.follow_symlinks)
            .with_options(self.config.scan_options);

        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        // Snapshot total dest count before scan (for deletion threshold)
        let original_dest_count = self.dest_index.len() as u64;

        // Scanner::scan() is blocking, so we run it in spawn_blocking
        let entries = tokio::task::spawn_blocking(move || scanner.scan()).await??;

        let mut excluded_source_dirs: Vec<PathBuf> = Vec::new();

        for entry in entries {
            let rel_path = entry.relative_path.as_ref().to_path_buf();
            let rel_path_str = rel_path.to_string_lossy().to_string();

            // Skip root directory (empty relative path)
            if rel_path_str.is_empty() {
                continue;
            }

            // Apply filter engine (--exclude/--include/--filter). If a directory
            // is excluded, exclude all children as rsync users expect (`--exclude .git`).
            if excluded_source_dirs
                .iter()
                .any(|dir| rel_path.starts_with(dir))
            {
                continue;
            }
            if let Some(ref filter) = self.config.filter {
                if filter.should_exclude(&entry.relative_path, entry.is_dir) {
                    if entry.is_dir {
                        excluded_source_dirs.push(rel_path.clone());
                    }
                    continue;
                }
            }

            // Get destination state before removing from index
            let dest_state = self.dest_index.remove(&rel_path_str);

            let mtime = entry
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let mtime_nanos = mtime.as_nanos() as i64;

            let mode = entry.mode;

            // Skip unchanged files (matching size and mtime)
            if !entry.is_dir && !entry.is_symlink {
                if let Some(ref dest) = dest_state {
                    if dest.size == entry.size && dest.mtime == mtime_nanos {
                        // File unchanged, skip it
                        continue;
                    }
                }
            }

            let msg = if entry.is_dir {
                GeneratorMessage::Mkdir {
                    path: Arc::new(rel_path),
                    mode,
                }
            } else if entry.is_symlink {
                GeneratorMessage::Symlink {
                    path: Arc::new(rel_path),
                    target: entry
                        .symlink_target
                        .as_ref()
                        .map(|t| t.to_string_lossy().to_string())
                        .unwrap_or_default(),
                }
            } else {
                // Check for hard link
                let inode = entry.inode.unwrap_or(0);
                let _link_target = if entry.nlink > 1 {
                    if let Some(existing) = self.seen_inodes.get(&inode) {
                        Some(existing.clone())
                    } else {
                        self.seen_inodes.insert(inode, Arc::new(rel_path.clone()));
                        None
                    }
                } else {
                    None
                };

                // Determine if delta is needed
                let (need_delta, checksums) =
                    self.check_delta_for_state(dest_state.as_ref(), entry.size);

                total_files += 1;
                total_bytes += entry.size;

                GeneratorMessage::File(FileJob {
                    path: Arc::new(rel_path),
                    size: entry.size,
                    mtime: mtime_nanos,
                    mode,
                    inode,
                    need_delta,
                    checksums,
                })
            };

            tx.send(msg).await?;
        }

        // Send FILE_END
        tx.send(GeneratorMessage::FileEnd {
            total_files,
            total_bytes,
        })
        .await?;

        // Send deletes if enabled
        if self.config.delete_enabled {
            let remaining: Vec<_> = self
                .dest_index
                .remaining_paths()
                .map(|(path, state)| (path.clone(), state.is_dir))
                .collect();

            // Check deletion threshold (skip if force_delete)
            let delete_count = remaining.len() as u64;
            if !self.config.force_delete {
                if let Some(ref max_delete) = self.config.max_delete {
                    let threshold = parse_max_delete(max_delete, original_dest_count);
                    if delete_count > threshold {
                        anyhow::bail!(
                            "Deletion safety threshold exceeded: {} files to delete exceeds max-delete '{}' (threshold: {} of {} dest files)",
                            delete_count, max_delete, threshold, original_dest_count
                        );
                    }
                }
            }

            for (path, is_dir) in remaining {
                tx.send(GeneratorMessage::Delete {
                    path: Arc::new(PathBuf::from(path)),
                    is_dir,
                })
                .await?;
            }
            tx.send(GeneratorMessage::DeleteEnd {
                count: delete_count,
            })
            .await?;
        }

        Ok((total_files, total_bytes))
    }

    fn check_delta_for_state(
        &self,
        dest_state: Option<&DestFileState>,
        size: u64,
    ) -> (bool, Option<DeltaInfo>) {
        if size < DELTA_MIN_SIZE {
            return (false, None);
        }

        if let Some(state) = dest_state {
            if let Some(ref delta_info) = state.delta_info {
                return (true, Some(delta_info.clone()));
            }
        }

        (false, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_generator_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: false,
            max_delete: None,
            force_delete: false,
            filter: None,
            scan_options: ScanOptions::default(),
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let gen = Generator::new(config);

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        // Should receive FileEnd with 0 files
        match rx.recv().await {
            Some(GeneratorMessage::FileEnd { total_files, .. }) => {
                assert_eq!(total_files, 0);
            }
            other => panic!("Expected FileEnd, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_generator_with_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file1.txt"), "hello").unwrap();
        fs::write(tmp.path().join("file2.txt"), "world").unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: false,
            max_delete: None,
            force_delete: false,
            filter: None,
            scan_options: ScanOptions::default(),
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let gen = Generator::new(config);

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        let mut file_count = 0;
        let mut dir_count = 0;

        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::File(_) => file_count += 1,
                GeneratorMessage::Mkdir { .. } => dir_count += 1,
                GeneratorMessage::FileEnd { total_files, .. } => {
                    assert_eq!(total_files, 2);
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(file_count, 2);
        assert_eq!(dir_count, 1);
    }

    #[tokio::test]
    async fn test_generator_delete_detection() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("keep.txt"), "keep").unwrap();

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: true,
            max_delete: None,
            force_delete: false,
            filter: None,
            scan_options: ScanOptions::default(),
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let mut gen = Generator::new(config);

        // Simulate dest has extra file
        gen.add_dest_entry(DestFileEntry {
            path: "delete_me.txt".to_string(),
            size: 100,
            mtime: 0,
            mode: 0o644,
            flags: DestFileFlags::empty(),
            block_size: 0,
            checksums: vec![],
        });

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        let mut got_delete = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::Delete { path, .. } => {
                    if path.to_string_lossy() == "delete_me.txt" {
                        got_delete = true;
                    }
                }
                GeneratorMessage::DeleteEnd { .. } => break,
                _ => {}
            }
        }

        assert!(got_delete, "Should have received delete for delete_me.txt");
    }

    #[tokio::test]
    async fn test_generator_delete_threshold_exceeded() {
        let tmp = TempDir::new().unwrap();
        // No source files — all dest entries will be candidates for deletion

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: true,
            max_delete: Some("50%".to_string()),
            force_delete: false,
            filter: None,
            scan_options: ScanOptions::default(),
        };

        let (tx, _rx) = crate::streaming::channel::file_job_channel();
        let mut gen = Generator::new(config);

        // Add 10 dest entries — deleting all 10 exceeds 50% threshold
        for i in 0..10 {
            gen.add_dest_entry(DestFileEntry {
                path: format!("file_{}.txt", i),
                size: 100,
                mtime: 0,
                mode: 0o644,
                flags: DestFileFlags::empty(),
                block_size: 0,
                checksums: vec![],
            });
        }

        // Generator should fail with threshold exceeded
        let result = gen.run(tx).await;
        assert!(
            result.is_err(),
            "Should have failed with threshold exceeded"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("threshold"),
            "Error should mention threshold, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_generator_delete_force_bypasses_threshold() {
        let tmp = TempDir::new().unwrap();
        // No source files — all dest entries will be candidates for deletion

        let config = GeneratorConfig {
            root: tmp.path().to_path_buf(),
            include_hidden: false,
            follow_symlinks: false,
            delete_enabled: true,
            max_delete: Some("50%".to_string()),
            force_delete: true, // bypass threshold
            filter: None,
            scan_options: ScanOptions::default(),
        };

        let (tx, mut rx) = crate::streaming::channel::file_job_channel();
        let mut gen = Generator::new(config);

        // Add 10 dest entries
        for i in 0..10 {
            gen.add_dest_entry(DestFileEntry {
                path: format!("file_{}.txt", i),
                size: 100,
                mtime: 0,
                mode: 0o644,
                flags: DestFileFlags::empty(),
                block_size: 0,
                checksums: vec![],
            });
        }

        tokio::spawn(async move {
            gen.run(tx).await.unwrap();
        });

        // Should receive 10 deletes (threshold bypassed)
        let mut delete_count = 0;
        while let Some(msg) = rx.recv().await {
            match msg {
                GeneratorMessage::Delete { .. } => delete_count += 1,
                GeneratorMessage::DeleteEnd { .. } => break,
                _ => {}
            }
        }
        assert_eq!(delete_count, 10, "force_delete should bypass threshold");
    }
}

/// Parse max-delete value (absolute count or percentage)
/// Returns the maximum number of files that can be deleted
fn parse_max_delete(max_delete: &str, dest_count: u64) -> u64 {
    if max_delete.ends_with('%') {
        let percent: f64 = max_delete.trim_end_matches('%').parse().unwrap_or(50.0);
        (dest_count as f64 * percent / 100.0) as u64
    } else {
        max_delete.parse::<u64>().unwrap_or(dest_count)
    }
}

#[cfg(test)]
mod parse_tests {
    use super::parse_max_delete;

    #[test]
    fn test_percentage() {
        assert_eq!(parse_max_delete("50%", 100), 50);
        assert_eq!(parse_max_delete("10%", 200), 20);
        assert_eq!(parse_max_delete("100%", 100), 100);
        assert_eq!(parse_max_delete("0%", 100), 0);
    }

    #[test]
    fn test_absolute() {
        assert_eq!(parse_max_delete("1000", 100), 1000);
        assert_eq!(parse_max_delete("0", 100), 0);
        assert_eq!(parse_max_delete("1", 100), 1);
    }

    #[test]
    fn test_invalid_fallback() {
        assert_eq!(parse_max_delete("abc%", 100), 50);
        assert_eq!(parse_max_delete("abc", 100), 100);
    }

    #[test]
    fn test_integer_truncation() {
        assert_eq!(parse_max_delete("50%", 10), 5);
        assert_eq!(parse_max_delete("50%", 1), 0);
        assert_eq!(parse_max_delete("50%", 3), 1);
    }
}
