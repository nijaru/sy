//! Integration tests for archive mode (-a) and related flags
//!
//! These tests verify actual sync behavior, not just flag state.
//! Issue #11 revealed that we tested flags but not behavior.

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

// =============================================================================
// Archive Mode (-a) Tests
// =============================================================================

#[test]
fn test_archive_includes_git_directory() {
    // Issue #11: -a should include .git/ directories
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create a git repo with some content
    setup_git_repo(&source);
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Ensure .git exists and has content
    assert!(source.path().join(".git").exists());
    assert!(source.path().join(".git/config").exists());

    // Run with -a (archive mode)
    let output = Command::new(sy_bin())
        .args([
            "-a",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy -a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Archive mode should include .git
    assert!(
        dest.path().join(".git").exists(),
        ".git directory should be included with -a flag"
    );
    assert!(
        dest.path().join(".git/config").exists(),
        ".git/config should be included with -a flag"
    );
    assert!(dest.path().join("file.txt").exists());
}

#[test]
fn test_archive_includes_hidden_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create hidden files
    fs::write(source.path().join(".hidden"), "hidden content").unwrap();
    fs::write(source.path().join(".config"), "config content").unwrap();
    fs::write(source.path().join("visible.txt"), "visible").unwrap();

    // Run with -a
    let output = Command::new(sy_bin())
        .args([
            "-a",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy -a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // All files should be synced
    assert!(
        dest.path().join(".hidden").exists(),
        ".hidden should be included with -a"
    );
    assert!(
        dest.path().join(".config").exists(),
        ".config should be included with -a"
    );
    assert!(dest.path().join("visible.txt").exists());
}

#[test]
#[cfg(unix)]
fn test_archive_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create file with specific permissions
    let file_path = source.path().join("script.sh");
    fs::write(&file_path, "#!/bin/bash\necho hello").unwrap();

    // Set executable permission (755)
    let mut perms = fs::metadata(&file_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&file_path, perms).unwrap();

    // Run with -a
    let output = Command::new(sy_bin())
        .args([
            "-a",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy -a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check permissions are preserved
    let dest_perms = fs::metadata(dest.path().join("script.sh"))
        .unwrap()
        .permissions();
    assert_eq!(
        dest_perms.mode() & 0o777,
        0o755,
        "Permissions should be preserved with -a"
    );
}

#[test]
fn test_archive_syncs_gitignored_files() {
    // -a implies --no-gitignore, so ignored files should be synced
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);

    // Create .gitignore that ignores .log files
    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("app.log"), "log content").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run with -a (should ignore .gitignore rules)
    let output = Command::new(sy_bin())
        .args([
            "-a",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy -a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Even .log files should be synced with -a
    assert!(
        dest.path().join("app.log").exists(),
        "gitignored files should be synced with -a"
    );
    assert!(dest.path().join("file.txt").exists());
    assert!(dest.path().join(".gitignore").exists());
}

// =============================================================================
// --include-vcs Tests
// =============================================================================

#[test]
fn test_include_vcs_includes_git() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run with --include-vcs (but not full archive mode)
    let output = Command::new(sy_bin())
        .args([
            "--include-vcs",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --include-vcs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // .git should be included
    assert!(
        dest.path().join(".git").exists(),
        ".git should be included with --include-vcs"
    );
}

#[test]
fn test_default_excludes_git() {
    // By default (without -a or --include-vcs), .git should be excluded
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run without any special flags
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // .git should NOT be included by default
    assert!(
        !dest.path().join(".git").exists(),
        ".git should be excluded by default"
    );
    assert!(dest.path().join("file.txt").exists());
}

// =============================================================================
// --no-gitignore Tests
// =============================================================================

#[test]
fn test_no_gitignore_syncs_ignored_files() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);

    // Create .gitignore
    fs::write(source.path().join(".gitignore"), "*.log\nbuild/\n").unwrap();

    // Create files that would be ignored
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::create_dir(source.path().join("build")).unwrap();
    fs::write(source.path().join("build/output.txt"), "output").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run with --no-gitignore
    let output = Command::new(sy_bin())
        .args([
            "--no-gitignore",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --no-gitignore failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Ignored files should be synced
    assert!(
        dest.path().join("debug.log").exists(),
        "*.log should be synced with --no-gitignore"
    );
    assert!(
        dest.path().join("build").exists(),
        "build/ should be synced with --no-gitignore"
    );
    assert!(dest.path().join("build/output.txt").exists());
}

#[test]
fn test_default_respects_gitignore() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);

    // Create .gitignore
    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Run without --no-gitignore (default behavior)
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // .log files should NOT be synced by default
    assert!(
        !dest.path().join("debug.log").exists(),
        "*.log should be excluded by default (gitignore)"
    );
    assert!(dest.path().join("file.txt").exists());
}

// =============================================================================
// Combined Flag Tests
// =============================================================================

#[test]
fn test_archive_is_complete_backup() {
    // -a should produce a complete backup including VCS and ignored files
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);

    // Create various files
    fs::write(source.path().join(".gitignore"), "*.tmp\n").unwrap();
    fs::write(source.path().join(".hidden"), "hidden").unwrap();
    fs::write(source.path().join("temp.tmp"), "temp").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Create nested structure
    fs::create_dir_all(source.path().join("src")).unwrap();
    fs::write(source.path().join("src/main.rs"), "fn main() {}").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "-a",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy -a failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Everything should be present
    assert!(dest.path().join(".git").exists(), ".git with -a");
    assert!(
        dest.path().join(".gitignore").exists(),
        ".gitignore with -a"
    );
    assert!(dest.path().join(".hidden").exists(), ".hidden with -a");
    assert!(
        dest.path().join("temp.tmp").exists(),
        "ignored file with -a"
    );
    assert!(dest.path().join("file.txt").exists());
    assert!(dest.path().join("src/main.rs").exists());
}

#[test]
fn test_include_vcs_without_no_gitignore() {
    // --include-vcs alone should include .git but still respect .gitignore
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    setup_git_repo(&source);

    fs::write(source.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--include-vcs",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --include-vcs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // .git should be included
    assert!(dest.path().join(".git").exists(), ".git with --include-vcs");

    // But .gitignore rules should still apply
    assert!(
        !dest.path().join("debug.log").exists(),
        "gitignore should still apply with --include-vcs alone"
    );
}
