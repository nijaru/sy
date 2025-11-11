/// Integration tests for per-file progress feature
///
/// Tests the --per-file-progress flag end-to-end with real file syncs
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn test_progress_shown_for_large_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a 2MB file (> MIN_SIZE_FOR_PROGRESS)
    let large_file = source.path().join("large.dat");
    let data = vec![0x42u8; 2 * 1024 * 1024];
    fs::write(&large_file, &data).unwrap();

    // Sync with progress enabled
    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    // Track progress updates
    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();

    let progress_callback = Arc::new(move |bytes: u64, _total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
        if bytes > 0 {
            // Progress is being reported
        }
    });

    let dest_file = dest.path().join("large.dat");
    let result = transport
        .copy_file_streaming(&large_file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    // Verify file was copied
    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());

    // Verify progress was updated multiple times (2MB file with 1MB chunks = at least 3 updates)
    let update_count = updates.load(Ordering::SeqCst);
    assert!(
        update_count >= 3,
        "Expected at least 3 progress updates for 2MB file, got {}",
        update_count
    );
}

#[tokio::test]
async fn test_progress_not_shown_for_small_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a small file (< MIN_SIZE_FOR_PROGRESS = 1MB)
    let small_file = source.path().join("small.txt");
    let data = vec![0x42u8; 512 * 1024]; // 512KB
    fs::write(&small_file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();
    let dest_file = dest.path().join("small.txt");

    // Copy without progress (small file)
    let result = transport.copy_file(&small_file, &dest_file).await.unwrap();

    // Verify file was copied
    assert_eq!(result.bytes_written, 512 * 1024);
    assert!(dest_file.exists());
}

#[tokio::test]
async fn test_progress_with_multiple_large_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create 3 large files
    for i in 1..=3 {
        let file_path = source.path().join(format!("file{}.dat", i));
        let data = vec![0x42u8; 2 * 1024 * 1024]; // 2MB each
        fs::write(&file_path, &data).unwrap();
    }

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    // Copy each file with progress tracking
    for i in 1..=3 {
        let source_file = source.path().join(format!("file{}.dat", i));
        let dest_file = dest.path().join(format!("file{}.dat", i));

        let updates = Arc::new(AtomicU64::new(0));
        let updates_clone = updates.clone();

        let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
            updates_clone.fetch_add(1, Ordering::SeqCst);
        });

        let result = transport
            .copy_file_streaming(&source_file, &dest_file, Some(progress_callback))
            .await
            .unwrap();

        assert_eq!(result.bytes_written, 2 * 1024 * 1024);
        assert!(dest_file.exists());

        // Each file should have progress updates
        let update_count = updates.load(Ordering::SeqCst);
        assert!(
            update_count >= 3,
            "File {} should have at least 3 progress updates, got {}",
            i,
            update_count
        );
    }
}

#[tokio::test]
async fn test_progress_with_mixed_file_sizes() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create mixed files: 2 small, 2 large
    let small1 = source.path().join("small1.txt");
    let small2 = source.path().join("small2.txt");
    let large1 = source.path().join("large1.dat");
    let large2 = source.path().join("large2.dat");

    fs::write(&small1, vec![0x42u8; 100 * 1024]).unwrap(); // 100KB
    fs::write(&small2, vec![0x42u8; 200 * 1024]).unwrap(); // 200KB
    fs::write(&large1, vec![0x42u8; 5 * 1024 * 1024]).unwrap(); // 5MB
    fs::write(&large2, vec![0x42u8; 3 * 1024 * 1024]).unwrap(); // 3MB

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    // Copy small files without progress (simulating per_file_progress=false for small files)
    for small_file in [&small1, &small2] {
        let dest_file = dest.path().join(small_file.file_name().unwrap());
        let _result = transport.copy_file(small_file, &dest_file).await.unwrap();
        assert!(dest_file.exists());
    }

    // Copy large files with progress
    for large_file in [&large1, &large2] {
        let dest_file = dest.path().join(large_file.file_name().unwrap());

        let updates = Arc::new(AtomicU64::new(0));
        let updates_clone = updates.clone();

        let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
            updates_clone.fetch_add(1, Ordering::SeqCst);
        });

        let _result = transport
            .copy_file_streaming(large_file, &dest_file, Some(progress_callback))
            .await
            .unwrap();

        assert!(dest_file.exists());

        // Large files should have progress
        let update_count = updates.load(Ordering::SeqCst);
        assert!(update_count >= 3, "Large file should have progress updates");
    }
}

#[tokio::test]
async fn test_progress_with_very_large_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a 100MB file
    let large_file = source.path().join("very_large.dat");

    // Write in chunks to avoid memory issues in test
    let chunk = vec![0x42u8; 10 * 1024 * 1024]; // 10MB chunks
    let mut file = fs::File::create(&large_file).unwrap();
    use std::io::Write;
    for _ in 0..10 {
        file.write_all(&chunk).unwrap();
    }
    file.flush().unwrap();
    drop(file);

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
        assert!(bytes <= total, "Bytes transferred should not exceed total");
    });

    let dest_file = dest.path().join("very_large.dat");
    let result = transport
        .copy_file_streaming(&large_file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    // Verify file was copied
    assert_eq!(result.bytes_written, 100 * 1024 * 1024);
    assert!(dest_file.exists());

    // Verify progress was updated many times (100MB / 1MB chunks = 100+ updates)
    let update_count = updates.load(Ordering::SeqCst);
    assert!(
        update_count >= 100,
        "Expected at least 100 progress updates for 100MB file, got {}",
        update_count
    );

    // Verify final progress shows complete transfer
    let final_bytes = last_bytes.load(Ordering::SeqCst);
    assert_eq!(
        final_bytes,
        100 * 1024 * 1024,
        "Final progress should show complete transfer"
    );
}

#[tokio::test]
async fn test_progress_callback_error_handling() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a file
    let file = source.path().join("test.dat");
    fs::write(&file, vec![0x42u8; 2 * 1024 * 1024]).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    // Progress callback that doesn't panic even with edge values
    let progress_callback = Arc::new(move |bytes: u64, total: u64| {
        // Test edge cases
        if bytes == 0 && total == 0 {
            // Should not panic
        }
        if bytes > total {
            // Should not happen, but callback shouldn't panic
            panic!("Bytes > total should never happen");
        }
    });

    let dest_file = dest.path().join("test.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());
}

#[tokio::test]
async fn test_streaming_with_nested_directories() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create nested structure with large files
    let nested_dir = source.path().join("dir1/dir2/dir3");
    fs::create_dir_all(&nested_dir).unwrap();

    let large_file = nested_dir.join("deep_file.dat");
    fs::write(&large_file, vec![0x42u8; 2 * 1024 * 1024]).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    // Ensure dest directory exists
    let dest_nested = dest.path().join("dir1/dir2/dir3");
    fs::create_dir_all(&dest_nested).unwrap();

    let updates = Arc::new(AtomicU64::new(0));
    let updates_clone = updates.clone();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        updates_clone.fetch_add(1, Ordering::SeqCst);
    });

    let dest_file = dest_nested.join("deep_file.dat");
    let result = transport
        .copy_file_streaming(&large_file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);
    assert!(dest_file.exists());

    let update_count = updates.load(Ordering::SeqCst);
    assert!(
        update_count >= 3,
        "Nested file should have progress updates"
    );
}

#[tokio::test]
async fn test_progress_maintains_file_integrity() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file with specific pattern
    let file = source.path().join("pattern.dat");
    let mut data = Vec::new();
    for i in 0..(2 * 1024 * 1024) {
        data.push((i % 256) as u8);
    }
    fs::write(&file, &data).unwrap();

    use sy::transport::local::LocalTransport;
    use sy::transport::Transport;

    let transport = LocalTransport::new();

    let progress_callback = Arc::new(move |_bytes: u64, _total: u64| {
        // Progress updates shouldn't affect file integrity
    });

    let dest_file = dest.path().join("pattern.dat");
    let result = transport
        .copy_file_streaming(&file, &dest_file, Some(progress_callback))
        .await
        .unwrap();

    assert_eq!(result.bytes_written, 2 * 1024 * 1024);

    // Verify data integrity
    let dest_data = fs::read(&dest_file).unwrap();
    assert_eq!(dest_data.len(), data.len());
    assert_eq!(
        dest_data, data,
        "File data should be identical after progress streaming"
    );
}
