use crate::endpoint::{Capabilities, Endpoint, EndpointType, FileMetadata, ScanOptions};
use crate::error::Result;
use crate::sync::scanner::{FileEntry, Scanner};
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

// Dead-code suppressed until Phase 3 (SyncSession) wires these in.
#[allow(dead_code)]
pub struct LocalEndpoint {
    root: PathBuf,
    capabilities: Capabilities,
    scan_options: ScanOptions,
}

#[allow(dead_code)] // Wired in by Phase 3 (SyncSession)
impl LocalEndpoint {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            capabilities: Capabilities::local(),
            scan_options: ScanOptions::default(),
        }
    }

    pub fn with_scan_options(mut self, options: ScanOptions) -> Self {
        self.scan_options = options;
        self
    }

    fn resolve(&self, relative: &Path) -> PathBuf {
        if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            self.root.join(relative)
        }
    }
}

#[allow(dead_code)] // Wired in by Phase 3 (SyncSession)
fn file_metadata_from_fs(meta: &fs::Metadata) -> FileMetadata {
    FileMetadata {
        size: meta.len(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        is_dir: meta.is_dir(),
        is_symlink: meta.is_symlink(),
        #[cfg(unix)]
        mode: meta.mode(),
        #[cfg(unix)]
        uid: meta.uid(),
        #[cfg(unix)]
        gid: meta.gid(),
        #[cfg(unix)]
        nlink: meta.nlink(),
    }
}

#[async_trait]
impl Endpoint for LocalEndpoint {
    fn endpoint_type(&self) -> EndpointType {
        EndpointType::Local
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn root(&self) -> &Path {
        &self.root
    }

    async fn scan(&self, opts: ScanOptions) -> Result<Vec<FileEntry>> {
        let path = self.root.clone();
        let options = opts;
        tokio::task::spawn_blocking(move || {
            let scanner = Scanner::new(&path).with_options(options);
            scanner.scan()
        })
        .await
        .map_err(|e| crate::error::SyncError::Io(std::io::Error::other(e.to_string())))?
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        let full_path = self.resolve(path);
        Ok(tokio::fs::try_exists(&full_path).await.unwrap_or(false))
    }

    async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
        let full_path = self.resolve(path);
        let meta = tokio::fs::metadata(&full_path).await?;
        Ok(file_metadata_from_fs(&meta))
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let full_path = self.resolve(path);
        tokio::fs::read(&full_path)
            .await
            .map_err(|e| crate::error::SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read file {}: {}", full_path.display(), e),
            )))
    }

    async fn write_file(&self, path: &Path, data: &[u8], meta: &FileMetadata) -> Result<()> {
        let full_path = self.resolve(path);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Atomic write: temp file + rename (same directory = same filesystem)
        let temp_path = crate::temp_file::TempFileGuard::temp_path_for(&full_path);
        let guard = crate::temp_file::TempFileGuard::new(&temp_path);

        tokio::fs::write(&temp_path, data).await?;
        filetime::set_file_mtime(
            &temp_path,
            filetime::FileTime::from_system_time(meta.modified),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(meta.mode);
            tokio::fs::set_permissions(&temp_path, perms).await?;
        }

        // Atomic rename to final path
        tokio::fs::rename(&temp_path, &full_path).await?;
        guard.defuse();

        Ok(())
    }

    async fn remove(&self, path: &Path, recursive: bool) -> Result<()> {
        let full_path = self.resolve(path);
        let meta = tokio::fs::symlink_metadata(&full_path).await?;
        if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&full_path).await?;
            } else {
                tokio::fs::remove_dir(&full_path).await?;
            }
        } else {
            tokio::fs::remove_file(&full_path).await?;
        }
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        let full_path = self.resolve(path);
        tokio::fs::create_dir_all(&full_path).await?;
        Ok(())
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        let full_dest = self.resolve(dest);
        if let Some(parent) = full_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::symlink(target, &full_dest).await?;
        Ok(())
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        let full_source = self.resolve(source);
        let full_dest = self.resolve(dest);
        if let Some(parent) = full_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::hard_link(&full_source, &full_dest).await?;
        Ok(())
    }

    async fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()> {
        let full_path = self.resolve(path);
        filetime::set_file_mtime(&full_path, filetime::FileTime::from_system_time(mtime))?;
        Ok(())
    }

    async fn set_permissions(&self, path: &Path, mode: u32) -> Result<()> {
        let full_path = self.resolve(path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&full_path, fs::Permissions::from_mode(mode)).await?;
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_meta() -> FileMetadata {
        FileMetadata {
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            #[cfg(unix)]
            mode: 0o644,
            #[cfg(unix)]
            uid: 0,
            #[cfg(unix)]
            gid: 0,
            #[cfg(unix)]
            nlink: 1,
        }
    }

    #[tokio::test]
    async fn test_scan() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        let ep = LocalEndpoint::new(dir.path().to_path_buf());
        let entries = ep.scan(ScanOptions::default()).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn test_read_write_roundtrip() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        ep.write_file(Path::new("test.txt"), b"content", &make_meta())
            .await
            .unwrap();

        let data = ep.read_file(Path::new("test.txt")).await.unwrap();
        assert_eq!(data, b"content");
    }

    #[tokio::test]
    async fn test_exists() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        assert!(!ep.exists(Path::new("missing.txt")).await.unwrap());

        fs::write(dir.path().join("exists.txt"), "data").unwrap();
        assert!(ep.exists(Path::new("exists.txt")).await.unwrap());
    }

    #[tokio::test]
    async fn test_capabilities() {
        let ep = LocalEndpoint::new(PathBuf::from("/tmp"));
        assert_eq!(ep.endpoint_type(), EndpointType::Local);
        assert!(ep.capabilities().cow_writes);
        assert!(ep.capabilities().delta_sync);
    }

    #[tokio::test]
    async fn test_remove_file() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        fs::write(dir.path().join("file.txt"), "data").unwrap();
        assert!(ep.exists(Path::new("file.txt")).await.unwrap());

        ep.remove(Path::new("file.txt"), false).await.unwrap();
        assert!(!ep.exists(Path::new("file.txt")).await.unwrap());
    }

    #[tokio::test]
    async fn test_remove_dir_recursive() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("subdir/file.txt"), "data").unwrap();

        ep.remove(Path::new("subdir"), true).await.unwrap();
        assert!(!ep.exists(Path::new("subdir")).await.unwrap());
    }

    #[tokio::test]
    async fn test_create_dir_all() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        ep.create_dir_all(Path::new("a/b/c")).await.unwrap();
        assert!(dir.path().join("a/b/c").is_dir());
    }

    #[tokio::test]
    async fn test_create_symlink() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        fs::write(dir.path().join("target.txt"), "data").unwrap();
        ep.create_symlink(Path::new("target.txt"), Path::new("link.txt"))
            .await
            .unwrap();

        assert!(dir.path().join("link.txt").symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(dir.path().join("link.txt")).unwrap(), Path::new("target.txt"));
    }

    #[tokio::test]
    async fn test_create_hardlink() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        fs::write(dir.path().join("original.txt"), "data").unwrap();
        ep.create_hardlink(Path::new("original.txt"), Path::new("hardlink.txt"))
            .await
            .unwrap();

        assert!(dir.path().join("hardlink.txt").exists());
        assert_eq!(fs::read(dir.path().join("hardlink.txt")).unwrap(), b"data");
    }

    #[tokio::test]
    async fn test_copy_file() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        ep.write_file(Path::new("source.txt"), b"content", &make_meta())
            .await
            .unwrap();

        let bytes = ep.copy_file(Path::new("source.txt"), Path::new("dest.txt"))
            .await
            .unwrap();

        assert_eq!(bytes, 7);
        assert_eq!(ep.read_file(Path::new("dest.txt")).await.unwrap(), b"content");
    }

    #[tokio::test]
    async fn test_metadata() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());

        ep.write_file(Path::new("file.txt"), b"content", &make_meta())
            .await
            .unwrap();

        let meta = ep.metadata(Path::new("file.txt")).await.unwrap();
        assert_eq!(meta.size, 7);
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
    }
}
