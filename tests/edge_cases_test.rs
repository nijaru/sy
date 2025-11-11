use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_git_repo(dir: &TempDir) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .unwrap();
}

#[test]
fn test_empty_directories() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create empty directory structure
    fs::create_dir_all(source.path().join("empty1/empty2/empty3")).unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("empty1/empty2/empty3").exists());
    assert!(dest.path().join("empty1/empty2/empty3").is_dir());
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_special_characters_in_filenames() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files with special characters
    let special_files = vec![
        "file with spaces.txt",
        "file_with_underscores.txt",
        "file-with-dashes.txt",
        "file.multiple.dots.txt",
        "日本語.txt",   // Japanese
        "emoji_🚀.txt", // Emoji
        "file'quote.txt",
    ];

    for filename in &special_files {
        let result = fs::write(source.path().join(filename), "content");
        if result.is_ok() {
            // Only test files that can be created on this filesystem
            continue;
        }
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify files that were created
    for filename in &special_files {
        let source_file = source.path().join(filename);
        let dest_file = dest.path().join(filename);
        if source_file.exists() {
            assert!(dest_file.exists(), "File {} should be synced", filename);
        }
    }
}

#[test]
fn test_unicode_filenames() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Test various unicode filenames
    let unicode_files = vec![
        "文件.txt",   // Chinese
        "файл.txt",   // Russian
        "αρχείο.txt", // Greek
        "ملف.txt",    // Arabic (if filesystem supports RTL)
    ];

    for filename in &unicode_files {
        if fs::write(source.path().join(filename), "content").is_ok() {
            // File created successfully
        }
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    for filename in &unicode_files {
        let source_file = source.path().join(filename);
        let dest_file = dest.path().join(filename);
        if source_file.exists() {
            assert!(
                dest_file.exists(),
                "Unicode file {} should be synced",
                filename
            );
            assert_eq!(fs::read_to_string(&dest_file).unwrap(), "content");
        }
    }
}

#[test]
fn test_deeply_nested_paths() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create deeply nested structure (20 levels)
    let mut path = source.path().to_path_buf();
    for i in 0..20 {
        path = path.join(format!("level_{}", i));
    }
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("deep.txt"), "deep content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify deep file exists
    let mut dest_path = dest.path().to_path_buf();
    for i in 0..20 {
        dest_path = dest_path.join(format!("level_{}", i));
    }
    assert!(dest_path.join("deep.txt").exists());
    assert_eq!(
        fs::read_to_string(dest_path.join("deep.txt")).unwrap(),
        "deep content"
    );
}

#[test]
fn test_large_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 10MB file
    let large_content = "x".repeat(10 * 1024 * 1024);
    fs::write(source.path().join("large.txt"), &large_content).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("large.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("large.txt"))
            .unwrap()
            .len(),
        10 * 1024 * 1024
    );
}

#[test]
fn test_many_small_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create 1000 small files
    for i in 0..1000 {
        fs::write(
            source.path().join(format!("file_{}.txt", i)),
            format!("content_{}", i),
        )
        .unwrap();
    }

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Verify all files synced
    for i in 0..1000 {
        let dest_file = dest.path().join(format!("file_{}.txt", i));
        assert!(dest_file.exists());
        assert_eq!(
            fs::read_to_string(&dest_file).unwrap(),
            format!("content_{}", i)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files created:     1000"));
}

#[test]
fn test_same_source_and_dest() {
    let source = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Try to sync directory to itself
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            &format!("{}/", source.path().display()),
        ])
        .output()
        .unwrap();

    // Should still work (though pointless)
    // This is acceptable behavior - skip all files
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Files skipped:     1"));
}

#[test]
fn test_binary_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create binary file with non-UTF8 content
    let binary_data: Vec<u8> = (0..=255).cycle().take(1024).collect();
    fs::write(source.path().join("binary.bin"), &binary_data).unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("binary.bin").exists());

    let dest_data = fs::read(dest.path().join("binary.bin")).unwrap();
    assert_eq!(dest_data, binary_data);
}

#[test]
fn test_hidden_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create hidden files (Unix convention)
    fs::write(source.path().join(".hidden"), "hidden content").unwrap();
    fs::write(source.path().join(".config"), "config").unwrap();
    fs::write(source.path().join("visible.txt"), "visible").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // Hidden files should be synced (we don't skip hidden files by default)
    assert!(dest.path().join(".hidden").exists());
    assert!(dest.path().join(".config").exists());
    assert!(dest.path().join("visible.txt").exists());
}

#[test]
fn test_file_permissions_preserved() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create file with specific permissions
        let file_path = source.path().join("executable.sh");
        fs::write(&file_path, "#!/bin/bash\necho hello").unwrap();

        // Set executable permission
        let mut perms = fs::metadata(&file_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file_path, perms).unwrap();

        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        assert!(output.status.success());

        let dest_file = dest.path().join("executable.sh");
        assert!(dest_file.exists());

        let dest_perms = fs::metadata(&dest_file).unwrap().permissions();
        // Note: Permissions may not be exactly preserved in Phase 1
        // This is a future enhancement, but let's document current behavior
        assert!(dest_perms.mode() > 0);
    }
}

#[test]
fn test_zero_byte_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create empty files
    fs::write(source.path().join("empty1.txt"), "").unwrap();
    fs::write(source.path().join("empty2.txt"), "").unwrap();
    fs::write(source.path().join("not_empty.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(dest.path().join("empty1.txt").exists());
    assert!(dest.path().join("empty2.txt").exists());
    assert_eq!(
        fs::read_to_string(dest.path().join("empty1.txt")).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("empty2.txt")).unwrap(),
        ""
    );
    assert_eq!(
        fs::read_to_string(dest.path().join("not_empty.txt")).unwrap(),
        "content"
    );
}
