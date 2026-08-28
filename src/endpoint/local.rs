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

/// Local filesystem endpoint.
///
/// Scan options are intentionally passed per scan operation; the endpoint itself
/// only owns stable endpoint state and capabilities.
pub struct LocalEndpoint {
    root: PathBuf,
    capabilities: Capabilities,
}

impl LocalEndpoint {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            capabilities: Capabilities::local(),
        }
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

/// Transactional local write backed by a same-directory temporary file.
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
            tokio::fs::set_permissions(
                &self.temp_path,
                std::fs::Permissions::from_mode(metadata.mode),
            )
            .await?;
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(guard) = self.guard.take() {
                    guard.defuse();
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
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
        tokio::task::spawn_blocking(move || Scanner::new(&path).with_options(opts).scan())
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.resolve(path)).await.unwrap_or(false))
    }

    async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
        let meta = tokio::fs::metadata(self.resolve(path)).await?;
        Ok(file_metadata_from_fs(&meta))
    }

    async fn open_reader(&self, path: &Path) -> Result<BoxReader> {
        let full_path = self.resolve(path);
        let file = tokio::fs::File::open(&full_path).await.map_err(|error| {
            SyncError::Io(std::io::Error::new(
                error.kind(),
                format!("Failed to open file {}: {}", full_path.display(), error),
            ))
        })?;
        Ok(Box::pin(file))
    }

    async fn begin_write(&self, path: &Path) -> Result<Box<dyn StagedWriter>> {
        Ok(Box::new(LocalStagedWriter::new(self.resolve(path)).await?))
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let full_path = self.resolve(path);
        tokio::fs::read(&full_path).await.map_err(|error| {
            SyncError::Io(std::io::Error::new(
                error.kind(),
                format!("Failed to read file {}: {}", full_path.display(), error),
            ))
        })
    }

    async fn write_file(&self, path: &Path, data: &[u8], meta: &FileMetadata) -> Result<()> {
        // Compatibility shim for callers not yet migrated to streaming I/O.
        let mut writer = self.begin_write(path).await?;
        writer.write(data).await?;
        writer.set_metadata(meta).await?;
        writer.commit().await
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<u64> {
        let result = crate::endpoint::io::copy_file_streaming(self, source, self, dest).await?;
        Ok(result.bytes_written)
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
        tokio::fs::create_dir_all(self.resolve(path)).await?;
        Ok(())
    }

    async fn create_symlink(&self, target: &Path, dest: &Path) -> Result<()> {
        let full_dest = self.resolve(dest);
        if let Some(parent) = full_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        #[cfg(unix)]
        {
            tokio::fs::symlink(target, &full_dest).await?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            Err(SyncError::Config(
                "symlink creation is not implemented for this platform".to_string(),
            ))
        }
    }

    async fn create_hardlink(&self, source: &Path, dest: &Path) -> Result<()> {
        let full_source = self.resolve(source);
        let full_dest = self.resolve(dest);
        if let Some(parent) = full_dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::hard_link(full_source, full_dest).await?;
        Ok(())
    }

    async fn set_mtime(&self, path: &Path, mtime: SystemTime) -> Result<()> {
        filetime::set_file_mtime(
            self.resolve(path),
            filetime::FileTime::from_system_time(mtime),
        )?;
        Ok(())
    }

    async fn set_permissions(&self, path: &Path, mode: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(
                self.resolve(path),
                std::fs::Permissions::from_mode(mode),
            )
            .await?;
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
    async fn scan_uses_per_call_options() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.txt"), "content").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let entries = endpoint.scan(ScanOptions::default()).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn staged_abort_preserves_destination() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());

        let mut writer = endpoint.begin_write(Path::new("file")).await.unwrap();
        writer.write(b"new").await.unwrap();
        writer.abort().await.unwrap();

        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn staged_commit_replaces_destination() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file"), b"old").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());

        let mut writer = endpoint.begin_write(Path::new("file")).await.unwrap();
        writer.write(b"content").await.unwrap();
        writer.set_metadata(&make_meta()).await.unwrap();
        writer.commit().await.unwrap();

        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"content");
    }

    #[tokio::test]
    async fn endpoint_copy_is_streaming_and_staged() {
        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let data = vec![7_u8; 2 * 1024 * 1024 + 3];
        fs::write(dir.path().join("source"), &data).unwrap();

        let bytes = endpoint
            .copy_file(Path::new("source"), Path::new("dest"))
            .await
            .unwrap();
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(fs::read(dir.path().join("dest")).unwrap(), data);
    }
}
