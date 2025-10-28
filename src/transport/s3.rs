use super::{FileInfo, TransferResult, Transport};
use crate::error::{Result, SyncError};
use crate::sync::scanner::FileEntry;
use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_smithy_types::byte_stream::ByteStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// S3 transport for cloud storage operations
///
/// Supports AWS S3 and S3-compatible services (Cloudflare R2, Backblaze B2, Wasabi)
pub struct S3Transport {
    client: Client,
    bucket: String,
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
        // Load AWS config
        let config = if let Some(r) = region {
            aws_config::from_env()
                .region(aws_sdk_s3::config::Region::new(r))
                .load()
                .await
        } else {
            aws_config::load_from_env().await
        };

        // Build S3 client with optional custom endpoint
        let s3_config_builder = aws_sdk_s3::config::Builder::from(&config);

        let s3_config = if let Some(ep) = endpoint {
            s3_config_builder
                .endpoint_url(ep)
                .force_path_style(true) // Required for non-AWS S3 (R2, B2, etc.)
                .build()
        } else {
            s3_config_builder.build()
        };

        let client = Client::from_conf(s3_config);

        Ok(Self {
            client,
            bucket,
            prefix,
        })
    }

    /// Convert a local path to an S3 key
    fn path_to_key(&self, path: &Path) -> String {
        let path_str = path.to_string_lossy();
        let path_str = path_str.trim_start_matches('/');

        if self.prefix.is_empty() {
            path_str.to_string()
        } else {
            format!("{}/{}", self.prefix.trim_end_matches('/'), path_str)
        }
    }

    /// Convert an S3 key to a local path
    fn key_to_path(&self, key: &str) -> PathBuf {
        let key = if !self.prefix.is_empty() {
            key.strip_prefix(&self.prefix)
                .unwrap_or(key)
                .trim_start_matches('/')
        } else {
            key
        };
        PathBuf::from(key)
    }

    /// Upload a large file using multipart upload
    ///
    /// This is required for files >5GB and recommended for files >100MB
    async fn copy_file_multipart(
        &self,
        source: &Path,
        dest: &Path,
        total_size: u64,
    ) -> Result<TransferResult> {
        use tokio::io::AsyncReadExt;

        let key = self.path_to_key(dest);

        // Start multipart upload
        let multipart_upload = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to create multipart upload: {}",
                    e
                )))
            })?;

        let upload_id = multipart_upload
            .upload_id()
            .ok_or_else(|| SyncError::Io(std::io::Error::other("No upload ID returned")))?;

        // Upload parts (5MB chunks, S3 minimum)
        const PART_SIZE: usize = 5 * 1024 * 1024; // 5 MB
        let mut file = tokio::fs::File::open(source).await?;
        let mut part_number = 1;
        let mut parts = Vec::new();
        let mut buffer = vec![0u8; PART_SIZE];

        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break; // EOF
            }

            let part_data = &buffer[..bytes_read];

            // Upload this part
            let upload_part_response = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(part_data.to_vec()))
                .send()
                .await
                .map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to upload part {}: {}",
                        part_number, e
                    )))
                })?;

            // Store part info
            let e_tag = upload_part_response
                .e_tag()
                .ok_or_else(|| SyncError::Io(std::io::Error::other("No ETag returned for part")))?;

            parts.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );

            part_number += 1;
        }

        // Complete multipart upload
        let completed_upload = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(&key)
            .upload_id(upload_id)
            .multipart_upload(completed_upload)
            .send()
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to complete multipart upload: {}",
                    e
                )))
            })?;

        Ok(TransferResult::new(total_size))
    }
}

#[async_trait]
impl Transport for S3Transport {
    async fn scan(&self, _path: &Path) -> Result<Vec<FileEntry>> {
        // List all objects in the bucket with the given prefix
        let mut continuation_token: Option<String> = None;
        let mut entries = Vec::new();

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&self.prefix);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request.send().await.map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to list S3 objects: {}",
                    e
                )))
            })?;

            // Process objects
            for obj in response.contents() {
                let key = obj
                    .key()
                    .ok_or_else(|| SyncError::Io(std::io::Error::other("Object missing key")))?;

                let size = obj.size().unwrap_or(0) as u64;
                let modified = obj
                    .last_modified()
                    .map(|dt| {
                        // Convert aws_smithy_types::DateTime to SystemTime
                        let secs = dt.secs();
                        let nanos = dt.subsec_nanos();
                        SystemTime::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
                    })
                    .unwrap_or(SystemTime::UNIX_EPOCH);

                // Check if this is a directory marker (ends with /)
                let is_dir = key.ends_with('/');

                entries.push(FileEntry {
                    path: Arc::new(PathBuf::from(key)),
                    relative_path: Arc::new(self.key_to_path(key)),
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

            // Check if there are more pages
            if response.is_truncated().unwrap_or(false) {
                continuation_token = response.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(entries)
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        let key = self.path_to_key(path);

        let result = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await;

        Ok(result.is_ok())
    }

    async fn metadata(&self, _path: &Path) -> Result<std::fs::Metadata> {
        // S3 doesn't have std::fs::Metadata, this method shouldn't be used
        Err(SyncError::Io(std::io::Error::other(
            "metadata() not supported for S3, use file_info() instead",
        )))
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo> {
        let key = self.path_to_key(path);

        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to get S3 object metadata: {}",
                    e
                )))
            })?;

        let size = response.content_length().unwrap_or(0) as u64;
        let modified = response
            .last_modified()
            .map(|dt| {
                let secs = dt.secs();
                let nanos = dt.subsec_nanos();
                SystemTime::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
            })
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Ok(FileInfo { size, modified })
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        // S3 doesn't have directories in the traditional sense
        // We can create a directory marker object (key ending with /)
        let key = format!("{}/", self.path_to_key(path));

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from_static(b""))
            .send()
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

        // Use multipart upload for large files (>100MB)
        const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024; // 100 MB

        if size > MULTIPART_THRESHOLD {
            self.copy_file_multipart(source, dest, size).await
        } else {
            // Small file: use simple upload
            let data = tokio::fs::read(source).await?;
            let key = self.path_to_key(dest);

            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(data))
                .send()
                .await
                .map_err(|e| {
                    SyncError::Io(std::io::Error::other(format!(
                        "Failed to upload to S3: {}",
                        e
                    )))
                })?;

            Ok(TransferResult::new(size))
        }
    }

    async fn remove(&self, path: &Path, _is_dir: bool) -> Result<()> {
        let key = self.path_to_key(path);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| {
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
        let key = self.path_to_key(path);

        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| {
                SyncError::Io(std::io::Error::other(format!(
                    "Failed to download from S3: {}",
                    e
                )))
            })?;

        let data = response.body.collect().await.map_err(|e| {
            SyncError::Io(std::io::Error::other(format!(
                "Failed to read S3 object body: {}",
                e
            )))
        })?;

        Ok(data.into_bytes().to_vec())
    }

    async fn write_file(&self, path: &Path, data: &[u8], _mtime: SystemTime) -> Result<()> {
        let key = self.path_to_key(path);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from(data.to_vec()))
            .send()
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
