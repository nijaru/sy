use crate::endpoint::{
    BoxReader, Capabilities, Endpoint, EndpointType, FileMetadata, ScanOptions, StagedWriter,
};
use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, Scanner};
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub struct LocalEndpoint {
    root: PathBuf,
    capabilities: Capabilities,
    scan_options: ScanOptions,
}

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

/// Local staged write backed by a temporary file in the destination directory.
///
/// Keeping staging and final paths in the same directory preserves same-filesystem
/// rename semantics. TempFileGuard provides best-effort cleanup if the writer is
/// dropped before commit.
struct LocalStagedWriter {
    file: Option<tokio::fs::File>,
    temp_path: PathBuf,
    final_path: PathBuf,
    guard: Option<crate::temp_file::TempFileGuard>,
}

impl LocalStagedWriter {
    async fn new(final_path: PathBuf) -> Result<Self> {
        if let Some(parent) = final_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let temp_path = crate::temp_file::TempFileGuard::temp_path_for(&final_path);
        let guard = crate::temp_file::TempFileGuard::new(&temp_path);
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await?;

        Ok(Self {
            file: Some(file),
            temp_path,
            final_path,
            guard: Some(guard),
        })
    }

    fn file_mut(&mut self) -> Result<&mut tokio::fs::File> {
        self.file.as_mut().ok_or_else(|| {
            SyncError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "staged writer is already closed",
            ))
        })
    }
}

#[async_trait]
impl StagedWriter for LocalStagedWriter {
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file_mut()?.write_all(data).await?;
        Ok(())
    }

    async fn set_metadata(&mut self, metadata: &FileMetadata) -> Result<()> {
        self.file_mut()?.flush().await?;

        filetime::set_file_mtime(
            &self.temp_path,
            filetime::FileTime::from_system_time(metadata.modified),
        )?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(metadata.mode);
            tokio::fs::set_permissions(&self.temp_path, perms).await?;
        }

        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
            drop(file);
        }

        tokio::fs::rename(&self.temp_path, &self.final_path).await?;
        if let Some(guard) = self.guard.take() {
            guard.defuse();
        }
        Ok(())
    }

    async fn abort(mut self: Box<Self>) -> Result<()> {
        self.file.take();
        match tokio::fs::remove_file(&self.temp_path).await {
            Ok(()) => {
                if let Some(guard) = self.guard.take() {
                    guard.defuse();
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(guard) = self.guard.take() {
                    guard.defuse();
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
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
        .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))?
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

    async fn open_reader(&self, path: &Path) -> Result<BoxReader> {
        let full_path = self.resolve(path);
        let file = tokio::fs::File::open(&full_path).await.map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to open file {}: {}", full_path.display(), e),
            ))
        })?;
        Ok(Box::pin(file))
    }

    async fn begin_write(&self, path: &Path) -> Result<Box<dyn StagedWriter>> {
        let full_path = self.resolve(path);
        Ok(Box::new(LocalStagedWriter::new(full_path).await?))
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let full_path = self.resolve(path);
        tokio::fs::read(&full_path).await.map_err(|e| {
            SyncError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read file {}: {}", full_path.display(), e),
            ))
        })
    }

    async fn write_file(&self, path: &Path, data: &[u8], meta: &FileMetadata) -> Result<()> {
        // Compatibility shim during the v0.5 migration. All local writes now
        // share the staged-writer transaction path even if the caller still
        // provides a whole in-memory buffer.
        let mut writer = self.begin_write(path).await?;
        writer.write(data).await?;
        writer.set_metadata(meta).await?;
        writer.commit().await
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
    async fn test_staged_write_abort_preserves_destination() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());
        fs::write(dir.path().join("test.txt"), b"old").unwrap();

        let mut writer = ep.begin_write(Path::new("test.txt")).await.unwrap();
        writer.write(b"new").await.unwrap();
        writer.abort().await.unwrap();

        assert_eq!(fs::read(dir.path().join("test.txt")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn test_staged_write_commit_replaces_destination() {
        let dir = TempDir::new().unwrap();
        let ep = LocalEndpoint::new(dir.path().to_path_buf());
        fs::write(dir.path().join("test.txt"), b"old").unwrap();

        let mut writer = ep.begin_write(Path::new("test.txt")).await.unwrap();
        writer.write(b"content").await.unwrap();
        writer.set_metadata(&make_meta()).await.unwrap();
        writer.commit().await.unwrap();

        assert_eq!(fs::read(dir.path().join("test.txt")).unwrap(), b"content");
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
        assert!(ep.capabilities().atomic_rename);
        assert!(ep.capabilities().streaming_read);
        assert!(ep.capabilities().staged_write);
        assert!(ep.capabilities().reflink);
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

        assert!(dir
            .path()
            .join("link.txt")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(dir.path().join("link.txt")).unwrap(),
            Path::new("target.txt")
        );
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

        let bytes = ep
            .copy_file(Path::new("source.txt"), Path::new("dest.txt"))
            .await
            .unwrap();

        assert_eq!(bytes, 7);
        assert_eq!(
            ep.read_file(Path::new("dest.txt")).await.unwrap(),
            b"content"
        );
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
