use super::{FileInfo, TransferResult, Transport};
use crate::error::{Result, SyncError};
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// S3 transport for cloud storage operations
///
/// Supports AWS S3 and S3-compatible services (Cloudflare R2, Backblaze B2, Wasabi)
pub struct S3Transport {
    store: Arc<dyn ObjectStore>,
    prefix: String, // Key prefix for all operations
}

impl S3Transport {
    /// Create a new S3 transport
    ///
    /// # Arguments
    /// * `bucket` - S3 bucket name
    /// * `prefix` - Key prefix (e.g., "backups/")
    /// * `region` - Optional AWS region (defaults to config/env)
    /// * `endpoint` - Optional custom endpoint (for R2, B2, etc.)
    pub async fn new(
        bucket: String,
        prefix: String,
        region: Option<String>,
        endpoint: Option<String>,
    ) -> Result<Self> {
        // Build S3 store with object_store
        let mut builder = AmazonS3Builder::new().with_bucket_name(&bucket);

        if let Some(r) = region {
            builder = builder.with_region(&r);
        }

        if let Some(ep) = endpoint {
            builder = builder.with_endpoint(&ep);
        }

        let store = Arc::new(builder.build().map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to create S3 client: {}",
                e
            )))
        })?);

        Ok(Self { store, prefix })
    }

    /// Convert a local path to an object store path
    fn path_to_object_path(&self, path: &Path) -> ObjectPath {
        let path_str = path.to_string_lossy();
        let path_str = path_str.trim_start_matches('/');

        let key = if self.prefix.is_empty() {
            path_str.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), path_str)
        };

        ObjectPath::from(key)
    }

    /// Convert an object store path to a local path
    fn object_path_to_path(&self, object_path: &ObjectPath) -> PathBuf {
        let key = object_path.as_ref();
        let key = if !self.prefix.is_empty() {
            key.strip_prefix(&self.prefix)
                .unwrap_or(key)
                .trim_start_matches('/')
        } else {
            key
        };
        PathBuf::from(key)
    }
}

#[async_trait]
impl Transport for S3Transport {
    async fn scan(&self, _path: &Path) -> Result<Vec<FileEntry>> {
        use futures::stream::StreamExt;

        let prefix = if self.prefix.is_empty() {
            None
        } else {
            Some(ObjectPath::from(self.prefix.clone()))
        };

        let mut entries = Vec::new();
        let mut list_stream = self.store.list(prefix.as_ref());

        while let Some(meta) = list_stream.next().await {
            let meta = meta.map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to retrieve object metadata: {}",
                    e
                )))
            })?;

            let key = meta.location.as_ref();
            let size = meta.size;
            let modified = meta.last_modified.into();

            // Check if this is a directory marker (ends with /)
            let is_dir = key.ends_with('/');

            entries.push(FileEntry {
                path: Arc::new(PathBuf::from(key)),
                relative_path: Arc::new(self.object_path_to_path(&meta.location)),
                size,
                modified,
                is_dir,
                is_symlink: false, // S3 doesn't have symlinks
                symlink_target: None,
                is_sparse: false,
                allocated_size: size,
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
                bsd_flags: None,
            });
        }

        Ok(entries)
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        let object_path = self.path_to_object_path(path);
        let result = self.store.head(&object_path).await;
        Ok(result.is_ok())
    }

    async fn metadata(&self, _path: &Path) -> Result<std::fs::Metadata> {
        // S3 doesn't have std::fs::Metadata, this method shouldn't be used
        Err(SyncError::Io(std::io::Error::other(
            "metadata() not supported for S3, use file_info() instead",
        )))
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo> {
        let object_path = self.path_to_object_path(path);

        let meta = self.store.head(&object_path).await.map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to get S3 object metadata: {}",
                e
            )))
        })?;

        Ok(FileInfo {
            size: meta.size,
            modified: meta.last_modified.into(),
        })
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        // S3 doesn't have directories in the traditional sense
        // We can create a directory marker object (key ending with /)
        let mut key_str = self.path_to_object_path(path).to_string();
        if !key_str.ends_with('/') {
            key_str.push('/');
        }
        let object_path = ObjectPath::from(key_str);

        self.store
            .put(&object_path, Bytes::new().into())
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to create S3 directory marker: {}",
                    e
                )))
            })?;

        Ok(())
    }

    async fn copy_file(&self, source: &Path, dest: &Path) -> Result<TransferResult> {
        let metadata = tokio::fs::metadata(source).await?;
        let size = metadata.len();

        // object_store handles multipart upload automatically
        let data = tokio::fs::read(source).await?;
        let object_path = self.path_to_object_path(dest);

        self.store
            .put(&object_path, Bytes::from(data).into())
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to upload to S3: {}",
                    e
                )))
            })?;

        Ok(TransferResult::new(size))
    }

    async fn remove(&self, path: &Path, _is_dir: bool) -> Result<()> {
        let object_path = self.path_to_object_path(path);

        self.store.delete(&object_path).await.map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to delete S3 object: {}",
                e
            )))
        })?;

        Ok(())
    }

    async fn create_hardlink(&self, _source: &Path, _dest: &Path) -> Result<()> {
        Err(SyncError::Io(std::io::Error::other(
            "Hardlinks not supported on S3",
        )))
    }

    async fn create_symlink(&self, _target: &Path, _dest: &Path) -> Result<()> {
        Err(SyncError::Io(std::io::Error::other(
            "Symlinks not supported on S3",
        )))
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let object_path = self.path_to_object_path(path);

        let result = self.store.get(&object_path).await.map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to download from S3: {}",
                e
            )))
        })?;

        let bytes = result.bytes().await.map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to read S3 object body: {}",
                e
            )))
        })?;

        Ok(bytes.to_vec())
    }

    async fn write_file(&self, path: &Path, data: &[u8], _mtime: SystemTime) -> Result<()> {
        let object_path = self.path_to_object_path(path);

        self.store
            .put(&object_path, Bytes::copy_from_slice(data).into())
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to upload to S3: {}",
                    e
                )))
            })?;

        Ok(())
    }

    async fn get_mtime(&self, path: &Path) -> Result<SystemTime> {
        let info = self.file_info(path).await?;
        Ok(info.modified)
    }
}
