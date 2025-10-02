use crate::error::Result;
use crate::sync::scanner::FileEntry;
use crate::transport::{Transport, TransferResult};
use std::path::Path;

pub struct Transferrer<'a, T: Transport> {
    transport: &'a T,
    dry_run: bool,
}

impl<'a, T: Transport> Transferrer<'a, T> {
    pub fn new(transport: &'a T, dry_run: bool) -> Self {
        Self { transport, dry_run }
    }

    /// Create a new file or directory
    /// Returns Some(TransferResult) for files, None for directories
    pub async fn create(&self, source: &FileEntry, dest_path: &Path) -> Result<Option<TransferResult>> {
        if self.dry_run {
            tracing::info!("Would create: {}", dest_path.display());
            return Ok(None);
        }

        if source.is_dir {
            self.create_directory(dest_path).await?;
            Ok(None)
        } else {
            let result = self.copy_file(&source.path, dest_path).await?;
            Ok(Some(result))
        }
    }

    /// Update an existing file
    /// Returns Some(TransferResult) for files, None for directories
    pub async fn update(&self, source: &FileEntry, dest_path: &Path) -> Result<Option<TransferResult>> {
        if self.dry_run {
            tracing::info!("Would update: {}", dest_path.display());
            return Ok(None);
        }

        if !source.is_dir {
            // Use delta sync for updates
            let result = self.transport.sync_file_with_delta(&source.path, dest_path).await?;
            tracing::info!("Updated: {} -> {}", source.path.display(), dest_path.display());
            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    /// Delete a file or directory
    pub async fn delete(&self, dest_path: &Path, is_dir: bool) -> Result<()> {
        if self.dry_run {
            tracing::info!("Would delete: {}", dest_path.display());
            return Ok(());
        }

        self.transport.remove(dest_path, is_dir).await?;
        tracing::info!("Deleted: {}", dest_path.display());
        Ok(())
    }

    async fn create_directory(&self, path: &Path) -> Result<()> {
        self.transport.create_dir_all(path).await?;
        tracing::debug!("Created directory: {}", path.display());
        Ok(())
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            self.transport.create_dir_all(parent).await?;
        }

        // Copy file using transport
        let result = self.transport.copy_file(source, dest).await?;

        tracing::debug!("Copied: {} -> {}", source.display(), dest.display());
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::local::LocalTransport;
    use std::fs;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_copy_file() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false);
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());
        assert_eq!(fs::read_to_string(&dest_path).unwrap(), "test content");
    }

    #[tokio::test]
    async fn test_dry_run() {
        let source_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();

        let source_file = source_dir.path().join("test.txt");
        fs::write(&source_file, "test content").unwrap();

        let file_entry = FileEntry {
            path: source_file.clone(),
            relative_path: PathBuf::from("test.txt"),
            size: 12,
            modified: SystemTime::now(),
            is_dir: false,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, true); // dry_run = true
        let dest_path = dest_dir.path().join("test.txt");
        transferrer.create(&file_entry, &dest_path).await.unwrap();

        // File should NOT exist in dry-run mode
        assert!(!dest_path.exists());
    }

    #[tokio::test]
    async fn test_create_directory() {
        let dest_dir = TempDir::new().unwrap();

        let dir_entry = FileEntry {
            path: PathBuf::from("/source/subdir"),
            relative_path: PathBuf::from("subdir"),
            size: 0,
            modified: SystemTime::now(),
            is_dir: true,
        };

        let transport = LocalTransport::new();
        let transferrer = Transferrer::new(&transport, false);
        let dest_path = dest_dir.path().join("subdir");
        transferrer.create(&dir_entry, &dest_path).await.unwrap();

        assert!(dest_path.exists());
        assert!(dest_path.is_dir());
    }
}
