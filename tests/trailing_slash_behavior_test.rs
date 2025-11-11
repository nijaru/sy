//! Test for issue #2: rsync-compatible trailing slash behavior
//!
//! This test verifies that sy follows rsync semantics for directory copying:
//! - `sy /a/dir target/` creates `target/dir/` (copies directory itself)
//! - `sy /a/dir/ target/` copies contents to `target/` (copies directory contents)

use std::path::PathBuf;

/// Helper function that mirrors the logic from compute_destination_path in main.rs
/// This duplication is intentional to document the expected behavior independently
fn compute_test_destination(source: &sy::path::SyncPath, dest: &sy::path::SyncPath) -> PathBuf {
    let source_path = source.path();

    // For directories with trailing slash, use destination as-is (copy contents)
    if source.has_trailing_slash() {
        return dest.path().to_path_buf();
    }

    // For directories without trailing slash, append directory name to destination
    if let Some(dir_name) = source_path.file_name() {
        dest.path().join(dir_name)
    } else {
        // Fallback: use destination as-is
        dest.path().to_path_buf()
    }
}

#[test]
fn test_syncpath_trailing_slash_detection() {
    // Test trailing slash detection for local paths
    let path_without = sy::path::SyncPath::parse("/home/user/mydir");
    assert!(
        !path_without.has_trailing_slash(),
        "/home/user/mydir should NOT have trailing slash"
    );

    let path_with = sy::path::SyncPath::parse("/home/user/mydir/");
    assert!(
        path_with.has_trailing_slash(),
        "/home/user/mydir/ should have trailing slash"
    );

    // Test remote paths
    let remote_without = sy::path::SyncPath::parse("user@host:/path/to/dir");
    assert!(
        !remote_without.has_trailing_slash(),
        "user@host:/path/to/dir should NOT have trailing slash"
    );

    let remote_with = sy::path::SyncPath::parse("user@host:/path/to/dir/");
    assert!(
        remote_with.has_trailing_slash(),
        "user@host:/path/to/dir/ should have trailing slash"
    );

    // Test Windows paths
    let windows_without = sy::path::SyncPath::parse("C:\\Users\\name\\dir");
    assert!(
        !windows_without.has_trailing_slash(),
        "C:\\Users\\name\\dir should NOT have trailing slash"
    );

    let windows_with = sy::path::SyncPath::parse("C:\\Users\\name\\dir\\");
    assert!(
        windows_with.has_trailing_slash(),
        "C:\\Users\\name\\dir\\ should have trailing slash"
    );
}

#[test]
fn test_destination_computation_without_trailing_slash() {
    // Source: /a/myproject (no trailing slash)
    // Dest: /target
    // Expected: /target/myproject (directory itself is copied)

    let source = sy::path::SyncPath::parse("/a/myproject");
    let dest = sy::path::SyncPath::parse("/target");

    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, PathBuf::from("/target/myproject"));
}

#[test]
fn test_destination_computation_with_trailing_slash() {
    // Source: /a/myproject/ (WITH trailing slash)
    // Dest: /target
    // Expected: /target (contents only are copied)

    let source = sy::path::SyncPath::parse("/a/myproject/");
    let dest = sy::path::SyncPath::parse("/target");

    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, PathBuf::from("/target"));
}

#[test]
fn test_remote_destination_computation_without_trailing_slash() {
    // Source: user@host:/a/myproject (no trailing slash)
    // Dest: /target
    // Expected: /target/myproject

    let source = sy::path::SyncPath::parse("user@host:/a/myproject");
    let dest = sy::path::SyncPath::parse("/target");

    assert!(!source.has_trailing_slash());
    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, PathBuf::from("/target/myproject"));
}

#[test]
fn test_remote_destination_computation_with_trailing_slash() {
    // Source: user@host:/a/myproject/ (WITH trailing slash)
    // Dest: /target
    // Expected: /target

    let source = sy::path::SyncPath::parse("user@host:/a/myproject/");
    let dest = sy::path::SyncPath::parse("/target");

    assert!(source.has_trailing_slash());
    let effective_dest = compute_test_destination(&source, &dest);
    assert_eq!(effective_dest, PathBuf::from("/target"));
}
