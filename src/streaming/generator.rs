//! Generator task for streaming sync.
//!
//! Scans source directory and streams file metadata to Sender.
//! Receives destination state during Initial Exchange.

use crate::streaming::channel::{
    DeltaInfo, DestFileState, DestIndex, FileJob, FileJobSender, GeneratorMessage, DELTA_MIN_SIZE,
};
use crate::streaming::protocol::{DestFileEntry, DestFileFlags};
use crate::sync::scanner::Scanner;
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
}

/// Generator state
pub struct Generator {
    config: GeneratorConfig,
    dest_index: DestIndex,
    seen_inodes: HashMap<u64, Arc<PathBuf>>, // For hard link detection
}

impl Generator {
    pub fn new(config: GeneratorConfig) -> Self {
        Self {
            config,
            dest_index: DestIndex::new(),
            seen_inodes: HashMap::new(),
        }
    }

    /// Process a DEST_FILE_ENTRY received during Initial Exchange.
    /// Call this for each entry before starting the scan.
    pub fn add_dest_entry(&mut self, entry: DestFileEntry) {
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
        let mut scanner = Scanner::new(&self.config.root);
        scanner = scanner.follow_links(self.config.follow_symlinks);

        // ScanOptions in this codebase only has respect_gitignore and include_git_dir
        // We'll use defaults for now.

        let mut total_files = 0u64;
        let mut total_bytes = 0u64;

        // Scanner::scan() is blocking, so we run it in spawn_blocking
        let entries = tokio::task::spawn_blocking(move || scanner.scan()).await??;

        for entry in entries {
            let rel_path = entry.relative_path.as_ref().to_path_buf();
            let rel_path_str = rel_path.to_string_lossy().to_string();

            // Skip root directory (empty relative path)
            if rel_path_str.is_empty() {
                continue;
            }

            // Get destination state before removing from index
            let dest_state = self.dest_index.remove(&rel_path_str);

            let mtime = entry
                .modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            // TODO: Scanner should provide mode. For now use default.
            let mode = if entry.is_dir { 0o755 } else { 0o644 };

            // Skip unchanged files (matching size and mtime)
            if !entry.is_dir && !entry.is_symlink {
                if let Some(ref dest) = dest_state {
                    if dest.size == entry.size && dest.mtime == mtime {
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
                    mtime,
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
            let mut delete_count = 0u64;
            let remaining: Vec<_> = self
                .dest_index
                .remaining_paths()
                .map(|(path, state)| (path.clone(), state.is_dir))
                .collect();

            for (path, is_dir) in remaining {
                tx.send(GeneratorMessage::Delete {
                    path: Arc::new(PathBuf::from(path)),
                    is_dir,
                })
                .await?;
                delete_count += 1;
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
}
