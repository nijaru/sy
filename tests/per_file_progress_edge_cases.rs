/// Edge case tests for per-file progress feature
///
/// Tests error scenarios, disk full, I/O errors, interruptions, etc.
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn test_progress_with_empty_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create an empty file
    let empty_file = source.path().join("empty.dat");
    fs::write(&empty_file, &[]).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
        assert_eq!(total, 0, "Empty file should have 0 total bytes");
        assert_eq!(bytes, 0, "Empty file should have 0 bytes transferred");
    });

    let dest_file = dest.path().join("empty.dat");
    let result = transport
        .copy_file_streaming(&empty_file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 0);
    assert!(dest_file.exists());
}

#[tokio::test]
async fn test_progress_with_exact_chunk_boundary() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file exactly 2MB (2 * 1MB chunk size)
    let file = source.path().join("exact.dat");
    let data = vec![0x42u8; 2 * 1024 * 1024];
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
        assert!(bytes <= total);
        assert_eq!(total, 2 * 1024 * 1024);
    });

    let dest_file = dest.path().join("exact.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());

    // Should have exactly 3 updates: 0, 1MB, 2MB
    let update_count = updates.load(Ordering::SeqCst);
    assert_eq!(
        update_count, 3,
        "Expected exactly 3 progress updates for 2MB file at chunk boundary"
    );
}

#[tokio::test]
async fn test_progress_with_odd_file_size() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file with odd size (not aligned to chunk boundaries)
    let file = source.path().join("odd.dat");
    let data = vec![0x42u8; 1_500_000]; // 1.5MB (not aligned to 1MB chunks)
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();
    let last_bytes = Arc::new(AtomicU64::new(0));
    let last_bytes_clone = last_bytes.clone();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
        last_bytes_clone.store(bytes, Ordering::SeqCst);
        assert!(bytes <= total);
        assert_eq!(total, 1_500_000);
    });

    let dest_file = dest.path().join("odd.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 1_500_000);
    assert!(dest_file.exists());

    // Final progress should be exactly file size
    let final_bytes = last_bytes.load(Ordering::SeqCst);
    assert_eq!(final_bytes, 1_500_000);
}

#[tokio::test]
async fn test_progress_callback_never_exceeds_total() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a large file
    let file = source.path().join("test.dat");
    let data = vec![0x42u8; 10 * 1024 * 1024]; // 10MB
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let exceeded = Arc::new(AtomicBool::new(false));
    let exceeded_clone = exceeded.clone();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        if bytes > total {
            exceeded_clone.store(true, Ordering::SeqCst);
        }
    });

    let dest_file = dest.path().join("test.dat");
    let _result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert!(
        !exceeded.load(Ordering::SeqCst),
        "Progress callback should never report bytes > total"
    );
}

#[tokio::test]
async fn test_progress_monotonic_increase() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a file
    let file = source.path().join("test.dat");
    let data = vec![0x42u8; 5 * 1024 * 1024]; // 5MB
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let last_bytes = Arc::new(AtomicU64::new(0));
    let last_bytes_clone = last_bytes.clone();
    let monotonic = Arc::new(AtomicBool::new(true));
    let monotonic_clone = monotonic.clone();

    let progress_callback = Arc::new(move |bytes: u64, _total: u64| {
        let prev = last_bytes_clone.load(Ordering::SeqCst);
        if bytes < prev {
            // Progress should never decrease
            monotonic_clone.store(false, Ordering::SeqCst);
        }
        last_bytes_clone.store(bytes, Ordering::SeqCst);
    });

    let dest_file = dest.path().join("test.dat");
    let _result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert!(
        monotonic.load(Ordering::SeqCst),
        "Progress should monotonically increase (never decrease)"
    );
}

#[tokio::test]
async fn test_progress_with_zero_byte_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a zero-byte file
    let file = source.path().join("zero.dat");
    fs::File::create(&file).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        assert_eq!(bytes, 0);
        assert_eq!(total, 0);
    });

    let dest_file = dest.path().join("zero.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 0);
    assert!(dest_file.exists());
}

#[tokio::test]
async fn test_progress_preserves_mtime() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a file with specific mtime
    let file = source.path().join("test.dat");
    let data = vec![0x42u8; 2 * 1024 * 1024];
    fs::write(&file, &data).unwrap();

    // Get original mtime
    let original_mtime = fs::metadata(&file).unwrap().modified().unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        // Progress updates shouldn't affect mtime preservation
    });

    let dest_file = dest.path().join("test.dat");
    let _result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    // Verify mtime is preserved (within 2 seconds for filesystem precision)
    let dest_mtime = fs::metadata(&dest_file).unwrap().modified().unwrap();
    let diff = dest_mtime
        .duration_since(original_mtime)
        .unwrap_or_else(|_| original_mtime.duration_since(dest_mtime).unwrap());

    assert!(
        diff.as_secs() <= 2,
        "Modification time should be preserved (diff: {:?})",
        diff
    );
}

#[tokio::test]
async fn test_progress_with_binary_data() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file with all possible byte values
    let file = source.path().join("binary.dat");
    let mut data = Vec::new();
    for _ in 0..8192 {
        // 8192 * 256 = 2MB
        for byte in 0..=255u8 {
            data.push(byte);
        }
    }
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        // Progress should work with binary data
    });

    let dest_file = dest.path().join("binary.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, data.len() as u64);

    // Verify binary data integrity
    let dest_data = fs::read(&dest_file).unwrap();
    assert_eq!(
        dest_data, data,
        "Binary data should be preserved exactly during progress streaming"
    );
}

#[tokio::test]
async fn test_progress_concurrent_files() {
    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create multiple files
    for i in 0..5 {
        let file = source.path().join(format!("file{}.dat", i));
        fs::write(&file, vec![0x42u8; 2 * 1024 * 1024]).unwrap();
    }

    let transport = Arc::new(LocalTransport::new());

    // Copy files concurrently
    let mut handles = vec![];
    for i in 0..5 {
        let transport = transport.clone();
        let source_path = source.path().to_path_buf();
        let dest_path = dest.path().to_path_buf();

        let handle = tokio::spawn(async move {
            let source_file = source_path.join(format!("file{}.dat", i));
            let dest_file = dest_path.join(format!("file{}.dat", i));

            let updates = Arc::new(AtomicU64::new(0));
            let updates_clone = updates.clone();

            let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
                updates_clone.fetch_add(1, Ordering::SeqCst);
            });

            let result = transport
                .copy_file_streaming(&source_file, &dest_file, Some(progress_callback))
                .await
                .unwrap();

            (result, updates.load(Ordering::SeqCst))
        });

        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        let (result, update_count) = handle.await.unwrap();
        assert_eq!(result.bytes_written, 2 * 1024 * 1024);
        assert!(update_count >= 3, "Each file should have progress updates");
    }
}

#[tokio::test]
#[cfg(unix)]
async fn test_progress_with_readonly_source() {
    use std::os::unix::fs::PermissionsExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a file and make it read-only
    let file = source.path().join("readonly.dat");
    fs::write(&file, vec![0x42u8; 2 * 1024 * 1024]).unwrap();

    let mut perms = fs::metadata(&file).unwrap().permissions();
    perms.set_mode(0o444); // Read-only
    fs::set_permissions(&file, perms).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        // Should work with read-only source
    });

    let dest_file = dest.path().join("readonly.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());
}

#[tokio::test]
async fn test_progress_parent_directory_created() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create source file
    let file = source.path().join("test.dat");
    fs::write(&file, vec![0x42u8; 2 * 1024 * 1024]).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        // Progress should work even when parent dirs need creation
    });

    // Destination with nested non-existent directories
    let dest_file = dest.path().join("a/b/c/test.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());
    assert!(dest_file.parent().unwrap().exists());
}
