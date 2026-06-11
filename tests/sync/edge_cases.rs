//! Edge case tests — unicode, deep paths, special chars, empty dirs, trailing slash.
//!
//! Consolidates: edge_cases_test.rs, trailing_slash_behavior_test.rs, remote_to_local_parent_dirs_test.rs.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn sy() -> Command {
    Command::new(sy_bin())
}

#[test]
fn test_unicode_filenames() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("日本語.txt"), "japanese").unwrap();
    fs::write(source.path().join("émoji🎉.txt"), "emoji").unwrap();
    fs::write(source.path().join("über.txt"), "german").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("日本語.txt")).unwrap(), "japanese");
    assert_eq!(fs::read_to_string(dest.path().join("émoji🎉.txt")).unwrap(), "emoji");
    assert_eq!(fs::read_to_string(dest.path().join("über.txt")).unwrap(), "german");
}

#[test]
fn test_deep_directory_structure() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    let mut path = source.path().to_path_buf();
    for i in 0..10 {
        path = path.join(format!("level{}", i));
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.txt"), "deep content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());

    let mut dest_path = dest.path().to_path_buf();
    for i in 0..10 {
        dest_path = dest_path.join(format!("level{}", i));
    }
    assert_eq!(fs::read_to_string(dest_path.join("deep.txt")).unwrap(), "deep content");
}

#[test]
fn test_special_characters_in_filenames() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file with spaces.txt"), "spaces").unwrap();
    fs::write(source.path().join("file-with-dashes.txt"), "dashes").unwrap();
    fs::write(source.path().join("file.with.dots.txt"), "dots").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file with spaces.txt")).unwrap(), "spaces");
    assert_eq!(fs::read_to_string(dest.path().join("file-with-dashes.txt")).unwrap(), "dashes");
    assert_eq!(fs::read_to_string(dest.path().join("file.with.dots.txt")).unwrap(), "dots");
}

#[test]
fn test_empty_directories() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::create_dir(source.path().join("empty1")).unwrap();
    fs::create_dir_all(source.path().join("nested/empty2")).unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("empty1").is_dir());
    assert!(dest.path().join("nested/empty2").is_dir());
}

#[test]
fn test_trailing_slash_source() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy()
        .arg(format!("{}/", source.path().display()))
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "content");
}

#[test]
fn test_trailing_slash_dest() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy()
        .arg(source.path())
        .arg(format!("{}/", dest.path().display()))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "content");
}

#[test]
fn test_trailing_slash_both() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy()
        .arg(format!("{}/", source.path().display()))
        .arg(format!("{}/", dest.path().display()))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "content");
}

#[test]
fn test_no_trailing_slash_copies_directory() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    let source_dir = source.path().join("mydir");
    fs::create_dir(&source_dir).unwrap();
    fs::write(source_dir.join("file.txt"), "content").unwrap();

    let output = sy()
        .arg(&source_dir)
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "content");
}

#[test]
fn test_root_path_sync() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_large_number_of_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    for i in 0..100 {
        fs::write(source.path().join(format!("file{:03}.txt", i)), format!("content{}", i)).unwrap();
    }

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());

    for i in 0..100 {
        let path = dest.path().join(format!("file{:03}.txt", i));
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("content{}", i));
    }
}

#[test]
fn test_mixed_file_types() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("regular.txt"), "regular").unwrap();
    fs::create_dir(source.path().join("directory")).unwrap();
    fs::write(source.path().join("directory/file.txt"), "in dir").unwrap();

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("regular.txt", source.path().join("symlink.txt")).unwrap();
    }

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("regular.txt").exists());
    assert!(dest.path().join("directory/file.txt").exists());

    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(dest.path().join("symlink.txt")).unwrap();
        assert!(meta.is_symlink());
    }
}

#[test]
fn test_overwrite_existing_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "new content").unwrap();
    fs::write(dest.path().join("file.txt"), "old content").unwrap();

    let output = sy().arg(source.path()).arg(dest.path()).output().unwrap();

    assert!(output.status.success());
    assert_eq!(fs::read_to_string(dest.path().join("file.txt")).unwrap(), "new content");
}
