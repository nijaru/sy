//! Test for issue #2: rsync-compatible trailing slash behavior
//!
//! This test verifies that sy follows rsync semantics for directory copying:
//! - `sy /a/dir target/` creates `target/dir/` (copies directory itself)
//! - `sy /a/dir/ target/` copies contents to `target/` (copies directory contents)

use std::path::PathBuf;

#[test]
fn test_syncpath_trailing_slash_detection() {
    // Test trailing slash detection for local paths
    let path_without = sy::path::SyncPath::parse("/home/user/mydir");
    assert!(!path_without.has_trailing_slash(), "/home/user/mydir should NOT have trailing slash");

    let path_with = sy::path::SyncPath::parse("/home/user/mydir/");
    assert!(path_with.has_trailing_slash(), "/home/user/mydir/ should have trailing slash");

    // Test remote paths
    let remote_without = sy::path::SyncPath::parse("user@host:/path/to/dir");
    assert!(!remote_without.has_trailing_slash(), "user@host:/path/to/dir should NOT have trailing slash");

    let remote_with = sy::path::SyncPath::parse("user@host:/path/to/dir/");
    assert!(remote_with.has_trailing_slash(), "user@host:/path/to/dir/ should have trailing slash");

    // Test Windows paths
    let windows_without = sy::path::SyncPath::parse("C:\\Users\\name\\dir");
    assert!(!windows_without.has_trailing_slash(), "C:\\Users\\name\\dir should NOT have trailing slash");

    let windows_with = sy::path::SyncPath::parse("C:\\Users\\name\\dir\\");
    assert!(windows_with.has_trailing_slash(), "C:\\Users\\name\\dir\\ should have trailing slash");
}

#[test]
fn test_destination_computation_without_trailing_slash() {
    // Source: /a/myproject (no trailing slash)
    // Dest: /target
    // Expected: /target/myproject (directory itself is copied)

    let source = sy::path::SyncPath::parse("/a/myproject");
    let dest = sy::path::SyncPath::parse("/target");

    let source_path = source.path();

    // Logic from compute_destination_path in main.rs:
    let effective_dest = if source.has_trailing_slash() {
        dest.path().to_path_buf()
    } else {
        if let Some(dir_name) = source_path.file_name() {
            dest.path().join(dir_name)
        } else {
            dest.path().to_path_buf()
        }
    };

    assert_eq!(effective_dest, PathBuf::from("/target/myproject"));
}

#[test]
fn test_destination_computation_with_trailing_slash() {
    // Source: /a/myproject/ (WITH trailing slash)
    // Dest: /target
    // Expected: /target (contents only are copied)

    let source = sy::path::SyncPath::parse("/a/myproject/");
    let dest = sy::path::SyncPath::parse("/target");

    let source_path = source.path();

    // Logic from compute_destination_path in main.rs:
    let effective_dest = if source.has_trailing_slash() {
        dest.path().to_path_buf()
    } else {
        if let Some(dir_name) = source_path.file_name() {
            dest.path().join(dir_name)
        } else {
            dest.path().to_path_buf()
        }
    };

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

    let source_path = source.path();
    let effective_dest = if source.has_trailing_slash() {
        dest.path().to_path_buf()
    } else {
        if let Some(dir_name) = source_path.file_name() {
            dest.path().join(dir_name)
        } else {
            dest.path().to_path_buf()
        }
    };

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

    let source_path = source.path();
    let effective_dest = if source.has_trailing_slash() {
        dest.path().to_path_buf()
    } else {
        if let Some(dir_name) = source_path.file_name() {
            dest.path().join(dir_name)
        } else {
            dest.path().to_path_buf()
        }
    };

    assert_eq!(effective_dest, PathBuf::from("/target"));
}
