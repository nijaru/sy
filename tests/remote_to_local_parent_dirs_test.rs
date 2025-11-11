//! Test for issue #3: Remote sync fails to create nested files
//!
//! This test verifies that when syncing from remote to local, parent directories
//! are created before files are copied into them.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use sy::error::Result;
use sy::sync::scanner::FileEntry;
use sy::transport::{TransferResult, Transport};
use tempfile::TempDir;

/// Mock remote transport that simulates a remote directory structure
struct MockRemoteTransport {
    files: Vec<FileEntry>,
}

impl MockRemoteTransport {
    fn new_with_nested_structure() -> Self {
        // Simulate remote structure: utimer/.gitignore, utimer/Cargo.toml, utimer/src/main.rs
        let files = vec![
            FileEntry {
                path: Arc::new(PathBuf::from("/remote/utimer/.gitignore")),
                relative_path: Arc::new(PathBuf::from(".gitignore")),
                size: 10,
                modified: SystemTime::now(),
                is_dir: false,
                is_symlink: false,
                symlink_target: None,
                is_sparse: false,
                allocated_size: 10,
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
                bsd_flags: None,
            },
            FileEntry {
                path: Arc::new(PathBuf::from("/remote/utimer/Cargo.toml")),
                relative_path: Arc::new(PathBuf::from("Cargo.toml")),
                size: 100,
                modified: SystemTime::now(),
                is_dir: false,
                is_symlink: false,
                symlink_target: None,
                is_sparse: false,
                allocated_size: 100,
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
                bsd_flags: None,
            },
            FileEntry {
                path: Arc::new(PathBuf::from("/remote/utimer/src/main.rs")),
                relative_path: Arc::new(PathBuf::from("src/main.rs")),
                size: 50,
                modified: SystemTime::now(),
                is_dir: false,
                is_symlink: false,
                symlink_target: None,
                is_sparse: false,
                allocated_size: 50,
                xattrs: None,
                inode: None,
                nlink: 1,
                acls: None,
                bsd_flags: None,
            },
        ];

        Self { files }
    }
}

#[async_trait]
impl Transport for MockRemoteTransport {
    async fn scan(&self, _path: &std::path::Path) -> Result<Vec<FileEntry>> {
        Ok(self.files.clone())
    }

    async fn exists(&self, _path: &std::path::Path) -> Result<bool> {
        Ok(true)
    }

    async fn metadata(&self, _path: &std::path::Path) -> Result<std::fs::Metadata> {
        Err(sy::error::SyncError::Io(std::io::Error::other(
            "metadata() not implemented in MockRemoteTransport test fixture",
        )))
    }

    async fn create_dir_all(&self, _path: &std::path::Path) -> Result<()> {
        // Mock: do nothing (can't create on remote in this scenario)
        Ok(())
    }

    async fn copy_file(
        &self,
        _source: &std::path::Path,
        _dest: &std::path::Path,
    ) -> Result<TransferResult> {
        Err(sy::error::SyncError::Io(std::io::Error::other(
            "copy_file() not implemented in MockRemoteTransport - DualTransport should use read_file/write_file instead",
        )))
    }

    async fn remove(&self, _path: &std::path::Path, _is_dir: bool) -> Result<()> {
        Ok(())
    }

    async fn create_hardlink(
        &self,
        _source: &std::path::Path,
        _dest: &std::path::Path,
    ) -> Result<()> {
        Ok(())
    }

    async fn create_symlink(
        &self,
        _target: &std::path::Path,
        _dest: &std::path::Path,
    ) -> Result<()> {
        Ok(())
    }

    async fn read_file(&self, path: &std::path::Path) -> Result<Vec<u8>> {
        // Simulate reading remote files
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        Ok(format!("Mock content of {}", filename).into_bytes())
    }

    async fn get_mtime(&self, _path: &std::path::Path) -> Result<SystemTime> {
        // Return current time for mock files
        Ok(SystemTime::now())
    }
}

#[tokio::test]
async fn test_remote_to_local_creates_parent_dirs() {
    let temp = TempDir::new().unwrap();
    let dest_root = temp.path().join("utimer");

    // Create dual transport (mock remote + local)
    let source_transport = Box::new(MockRemoteTransport::new_with_nested_structure());
    let dest_transport = Box::new(sy::transport::local::LocalTransport::new());
    let dual_transport = sy::transport::dual::DualTransport::new(source_transport, dest_transport);

    // Create destination root
    dual_transport.create_dir_all(&dest_root).await.unwrap();

    // Try to copy a file from remote to local (simulating what happens during sync)
    // Source path is remote: /remote/utimer/.gitignore
    // Dest path is local: {temp}/utimer/.gitignore
    let source_path = PathBuf::from("/remote/utimer/.gitignore");
    let dest_path = dest_root.join(".gitignore");

    // This should create parent directories automatically
    let result = dual_transport.copy_file(&source_path, &dest_path).await;

    // Verify the file was created
    assert!(
        result.is_ok(),
        "copy_file should succeed: {:?}",
        result.err()
    );
    assert!(dest_path.exists(), "Destination file should exist");
    assert!(dest_root.exists(), "Parent directory should exist");

    // Try a nested file
    let source_nested = PathBuf::from("/remote/utimer/src/main.rs");
    let dest_nested = dest_root.join("src/main.rs");

    let result = dual_transport.copy_file(&source_nested, &dest_nested).await;
    assert!(
        result.is_ok(),
        "Nested file copy should succeed: {:?}",
        result.err()
    );
    assert!(dest_nested.exists(), "Nested destination file should exist");
    assert!(
        dest_nested.parent().unwrap().exists(),
        "Nested parent directory should exist"
    );
}
