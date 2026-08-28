use crate::endpoint::{
    BoxReader, Capabilities, Endpoint, EndpointType, EntryStream, FileMetadata, ScanOptions,
    StagedWriter,
};
use crate::error::{Result, SyncError};
use crate::sync::scanner::{FileEntry, Scanner};
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// Local filesystem endpoint.
///
/// Scan options are passed per operation; the endpoint owns only stable root
/// and capability state.
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
    }
}

/// Transactional local regular-file write backed by a same-directory temp file.
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

    async fn staged_hash(&mut self) -> Result<Option<blake3::Hash>> {
        const BUFFER_SIZE: usize = 1024 * 1024;
        self.file_mut()?.flush().await?;

        let mut file = tokio::fs::File::open(&self.temp_path).await?;
        let mut buffer = vec![0_u8; BUFFER_SIZE];
        let mut hasher = blake3::Hasher::new();
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(Some(hasher.finalize()))
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

    fn native_path(&self, path: &Path) -> Option<PathBuf> {
        Some(self.resolve(path))
    }

    async fn scan(&self, opts: ScanOptions) -> Result<Vec<FileEntry>> {
        let path = self.root.clone();
        tokio::task::spawn_blocking(move || Scanner::new(&path).with_options(opts).scan())
            .await
            .map_err(|error| SyncError::Io(std::io::Error::other(error.to_string())))?
    }

    async fn scan_ordered(&self, opts: ScanOptions) -> Result<EntryStream> {
        const CHANNEL_CAPACITY: usize = 256;

        let root = self.root.clone();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let error_sender = sender.clone();

        tokio::spawn(async move {
            let scan = tokio::task::spawn_blocking(move || {
                for entry in crate::endpoint::local_scan::OrderedLocalScanner::new(root, opts) {
                    if sender.blocking_send(entry).is_err() {
                        break;
                    }
                }
            })
            .await;

            if let Err(error) = scan {
                let _ = error_sender
                    .send(Err(SyncError::Io(std::io::Error::other(error.to_string()))))
                    .await;
            }
        });

        Ok(Box::pin(futures::stream::unfold(
            receiver,
            |mut receiver| async move { receiver.recv().await.map(|entry| (entry, receiver)) },
        )))
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        match tokio::fs::symlink_metadata(self.resolve(path)).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn metadata(&self, path: &Path) -> Result<FileMetadata> {
        let meta = tokio::fs::symlink_metadata(self.resolve(path)).await?;
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
        // Compatibility shim while remaining v0.4 auxiliary paths migrate.
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
            // Stage the directory entry and atomically rename it over an existing
            // file/symlink. Type-changing directory replacements are rejected by
            // reconciliation before reaching this operation.
            let temp = crate::temp_file::TempFileGuard::temp_path_for(&full_dest);
            let guard = crate::temp_file::TempFileGuard::new(&temp);
            tokio::fs::symlink(target, &temp).await?;
            tokio::fs::rename(&temp, &full_dest).await?;
            guard.defuse();
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

        let temp = crate::temp_file::TempFileGuard::temp_path_for(&full_dest);
        let guard = crate::temp_file::TempFileGuard::new(&temp);
        tokio::fs::hard_link(&full_source, &temp).await?;
        tokio::fs::rename(&temp, &full_dest).await?;
        guard.defuse();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tempfile::TempDir;

    fn make_meta() -> FileMetadata {
        FileMetadata {
            size: 7,
            modified: SystemTime::now(),
            is_dir: false,
            is_symlink: false,
            #[cfg(unix)]
            mode: 0o644,
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
    async fn ordered_scan_stream_preserves_path_order() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("b")).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("z"), b"z").unwrap();
        fs::write(dir.path().join("a").join("file"), b"a").unwrap();

        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut stream = endpoint.scan_ordered(ScanOptions::default()).await.unwrap();
        let mut paths = Vec::new();
        while let Some(entry) = stream.next().await {
            paths.push((*entry.unwrap().relative_path).clone());
        }

        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
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
    async fn staged_hash_reads_uncommitted_bytes() {
        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        let mut writer = endpoint.begin_write(Path::new("file")).await.unwrap();
        writer.write(b"content").await.unwrap();
        let hash = writer.staged_hash().await.unwrap().unwrap();
        assert_eq!(hash, blake3::hash(b"content"));
        writer.abort().await.unwrap();
        assert!(!dir.path().join("file").exists());
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

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_update_is_atomic_at_endpoint_boundary() {
        let dir = TempDir::new().unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        endpoint
            .create_symlink(Path::new("first"), Path::new("link"))
            .await
            .unwrap();
        endpoint
            .create_symlink(Path::new("second"), Path::new("link"))
            .await
            .unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("link")).unwrap(),
            Path::new("second")
        );
    }

    #[tokio::test]
    async fn hardlink_update_is_atomic_at_endpoint_boundary() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("first"), b"one").unwrap();
        fs::write(dir.path().join("second"), b"two").unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        endpoint
            .create_hardlink(Path::new("first"), Path::new("link"))
            .await
            .unwrap();
        endpoint
            .create_hardlink(Path::new("second"), Path::new("link"))
            .await
            .unwrap();
        assert_eq!(fs::read(dir.path().join("link")).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exists_sees_dangling_symlink() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink("missing", dir.path().join("link")).unwrap();
        let endpoint = LocalEndpoint::new(dir.path().to_path_buf());
        assert!(endpoint.exists(Path::new("link")).await.unwrap());
        assert!(
            endpoint
                .metadata(Path::new("link"))
                .await
                .unwrap()
                .is_symlink
        );
    }

    #[test]
    fn exposes_native_path() {
        let endpoint = LocalEndpoint::new(PathBuf::from("/tmp/root"));
        assert_eq!(
            endpoint.native_path(Path::new("file")),
            Some(PathBuf::from("/tmp/root/file"))
        );
    }
}
