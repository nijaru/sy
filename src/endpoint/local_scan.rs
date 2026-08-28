use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, ScanOptions};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[cfg(target_os = "macos")]
use std::os::darwin::fs::MetadataExt as DarwinMetadataExt;

/// Ordered local scan used by the v0.5 merge reconciler.
///
/// Unlike the legacy scanner, this path intentionally avoids xattr and ACL
/// reads. Those are preservation work, not reconciliation metadata, and are
/// fetched only when a transfer actually needs them.
pub(crate) struct OrderedLocalScanner {
    root: PathBuf,
    walker: ignore::Walk,
}

impl OrderedLocalScanner {
    pub(crate) fn new(root: PathBuf, options: ScanOptions) -> Self {
        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(false)
            .git_ignore(options.respect_gitignore)
            .git_global(options.respect_gitignore)
            .git_exclude(options.respect_gitignore)
            .follow_links(false)
            // `ignore` only applies sorting to the sequential walker. The
            // reconciler depends on this total ordering for a merge join.
            .sort_by_file_path(|left, right| left.cmp(right));

        if !options.include_git_dir {
            builder.filter_entry(|entry| entry.file_name() != ".git");
        }

        if options.respect_gitignore {
            let gitignore = root.join(".gitignore");
            if gitignore.exists() {
                builder.add_ignore(&gitignore);
            }
        }

        if options.dirs_only {
            builder.max_depth(Some(1));
        }

        Self {
            root,
            walker: builder.build(),
        }
    }
}

impl Iterator for OrderedLocalScanner {
    type Item = Result<FileEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let result = self.walker.next()?;
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    return Some(Err(SyncError::Io(std::io::Error::other(
                        error.to_string(),
                    ))))
                }
            };

            if entry.path() == self.root {
                continue;
            }

            return Some(file_entry(&self.root, entry));
        }
    }
}

fn file_entry(root: &Path, entry: ignore::DirEntry) -> Result<FileEntry> {
    let path = entry.path().to_path_buf();
    let metadata = std::fs::symlink_metadata(&path).map_err(|source| SyncError::ReadDirError {
        path: path.clone(),
        source,
    })?;
    let relative_path = path
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| SyncError::InvalidPath { path: path.clone() })?;

    let is_symlink = metadata.is_symlink();
    let symlink_target = if is_symlink {
        std::fs::read_link(&path).ok().map(Arc::new)
    } else {
        None
    };

    #[cfg(unix)]
    let (is_sparse, allocated_size, inode, nlink, mode) = {
        let allocated_size = metadata.blocks() * 512;
        let size = metadata.len();
        let is_sparse = !metadata.is_dir()
            && !is_symlink
            && size > 4096
            && allocated_size < size.saturating_sub(4096);
        (
            is_sparse,
            allocated_size,
            Some(metadata.ino()),
            metadata.nlink(),
            metadata.permissions().mode(),
        )
    };

    #[cfg(not(unix))]
    let (is_sparse, allocated_size, inode, nlink, mode) = (
        false,
        metadata.len(),
        None,
        1,
        if metadata.is_dir() { 0o755 } else { 0o644 },
    );

    #[cfg(target_os = "macos")]
    let bsd_flags = Some(metadata.st_flags());
    #[cfg(not(target_os = "macos"))]
    let bsd_flags = None;

    let modified = metadata.modified().map_err(|source| SyncError::ReadDirError {
        path: path.clone(),
        source,
    })?;

    Ok(FileEntry {
        path: Arc::new(path),
        relative_path: Arc::new(relative_path),
        size: metadata.len(),
        modified,
        mode,
        is_dir: metadata.is_dir(),
        is_symlink,
        symlink_target,
        is_sparse,
        allocated_size,
        // Reconciliation never needs these expensive metadata classes.
        xattrs: None,
        inode,
        nlink,
        acls: None,
        bsd_flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn emits_paths_in_total_order() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("z"), b"z").unwrap();
        std::fs::write(dir.path().join("a").join("z"), b"az").unwrap();
        std::fs::write(dir.path().join("a").join("a"), b"aa").unwrap();

        let entries: Vec<_> = OrderedLocalScanner::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
        )
        .collect::<Result<Vec<_>>>()
        .unwrap();
        let paths: Vec<_> = entries
            .into_iter()
            .map(|entry| (*entry.relative_path).clone())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn reconciliation_scan_skips_expensive_metadata() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file"), b"content").unwrap();
        let entry = OrderedLocalScanner::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
        )
        .next()
        .expect("one entry")
        .unwrap();

        assert!(entry.xattrs.is_none());
        assert!(entry.acls.is_none());
    }
}
