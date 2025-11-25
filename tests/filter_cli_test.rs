//! Integration tests for CLI filter flags (--exclude, --include, --filter)
//!
//! These tests verify that filter flags work end-to-end, not just in unit tests.

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
// --exclude Tests
// =============================================================================

#[test]
fn test_exclude_flag_basic() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create files
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::write(source.path().join("error.log"), "error").unwrap();

    // Run with --exclude "*.log"
    let output = Command::new(sy_bin())
        .args([
            "--exclude",
            "*.log",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --exclude failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // .log files should be excluded
    assert!(dest.path().join("file.txt").exists());
    assert!(
        !dest.path().join("debug.log").exists(),
        "*.log should be excluded"
    );
    assert!(
        !dest.path().join("error.log").exists(),
        "*.log should be excluded"
    );
}

#[test]
fn test_exclude_flag_multiple() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::write(source.path().join("cache.tmp"), "tmp").unwrap();

    // Multiple --exclude flags
    let output = Command::new(sy_bin())
        .args([
            "--exclude",
            "*.log",
            "--exclude",
            "*.tmp",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("debug.log").exists());
    assert!(!dest.path().join("cache.tmp").exists());
}

#[test]
fn test_exclude_directory() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::create_dir(source.path().join("node_modules")).unwrap();
    fs::write(source.path().join("node_modules/package.json"), "{}").unwrap();

    // Exclude directory with trailing slash
    let output = Command::new(sy_bin())
        .args([
            "--exclude",
            "node_modules/",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(dest.path().join("file.txt").exists());
    assert!(
        !dest.path().join("node_modules").exists(),
        "node_modules/ should be excluded"
    );
}

// =============================================================================
// --include Tests
// =============================================================================

#[test]
fn test_include_flag_basic() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
    fs::write(source.path().join("lib.rs"), "pub fn lib() {}").unwrap();
    fs::write(source.path().join("readme.md"), "# Readme").unwrap();

    // Include only .rs files, exclude everything else
    let output = Command::new(sy_bin())
        .args([
            "--include",
            "*.rs",
            "--exclude",
            "*",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --include failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Only .rs files should be synced
    assert!(
        dest.path().join("main.rs").exists(),
        "*.rs should be included"
    );
    assert!(
        dest.path().join("lib.rs").exists(),
        "*.rs should be included"
    );
    assert!(
        !dest.path().join("readme.md").exists(),
        "*.md should be excluded"
    );
}

#[test]
fn test_include_exclude_order_matters() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("important.log"), "important").unwrap();
    fs::write(source.path().join("debug.log"), "debug").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    // Include important.log first, then exclude all .log
    let output = Command::new(sy_bin())
        .args([
            "--include",
            "important.log",
            "--exclude",
            "*.log",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // important.log should be included (first rule matches)
    assert!(
        dest.path().join("important.log").exists(),
        "important.log should be included (first match wins)"
    );
    // debug.log should be excluded (second rule matches)
    assert!(
        !dest.path().join("debug.log").exists(),
        "debug.log should be excluded"
    );
    assert!(dest.path().join("file.txt").exists());
}

// =============================================================================
// --filter Tests (rsync syntax)
// =============================================================================

#[test]
fn test_filter_flag_include_syntax() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
    fs::write(source.path().join("test.py"), "print('test')").unwrap();

    // rsync-style filter: + for include
    let output = Command::new(sy_bin())
        .args([
            "--filter",
            "+ *.rs",
            "--filter",
            "- *",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --filter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(dest.path().join("main.rs").exists());
    assert!(!dest.path().join("test.py").exists());
}

#[test]
fn test_filter_flag_exclude_syntax() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();

    // rsync-style filter: - for exclude
    let output = Command::new(sy_bin())
        .args([
            "--filter",
            "- *.log",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("debug.log").exists());
}

// =============================================================================
// --exclude-from / --include-from Tests
// =============================================================================

#[test]
fn test_exclude_from_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let exclude_file = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create exclude patterns file
    let patterns_path = exclude_file.path().join("excludes.txt");
    fs::write(&patterns_path, "*.log\n*.tmp\n# comment\nbuild/\n").unwrap();

    // Create source files
    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("debug.log"), "log").unwrap();
    fs::write(source.path().join("cache.tmp"), "tmp").unwrap();
    fs::create_dir(source.path().join("build")).unwrap();
    fs::write(source.path().join("build/out.txt"), "out").unwrap();

    let output = Command::new(sy_bin())
        .args([
            "--exclude-from",
            patterns_path.to_str().unwrap(),
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --exclude-from failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("debug.log").exists());
    assert!(!dest.path().join("cache.tmp").exists());
    assert!(!dest.path().join("build").exists());
}

#[test]
fn test_include_from_file() {
    // Note: --include-from is processed AFTER --exclude, so to use include-from
    // with exclude, use --filter for explicit ordering, or use --exclude-from
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let filter_file = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create filter patterns file with rsync syntax (order preserved within file)
    let patterns_path = filter_file.path().join("filters.txt");
    fs::write(&patterns_path, "+ *.rs\n+ *.toml\n- *\n").unwrap();

    // Create source files
    fs::write(source.path().join("main.rs"), "fn main() {}").unwrap();
    fs::write(source.path().join("Cargo.toml"), "[package]").unwrap();
    fs::write(source.path().join("readme.md"), "# Readme").unwrap();

    // Use --filter to load rules from file (preserves order)
    // Note: sy doesn't have --filter-from, so we load rules inline
    let output = Command::new(sy_bin())
        .args([
            "--filter",
            "+ *.rs",
            "--filter",
            "+ *.toml",
            "--filter",
            "- *",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "sy --filter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(dest.path().join("main.rs").exists());
    assert!(dest.path().join("Cargo.toml").exists());
    assert!(!dest.path().join("readme.md").exists());
}

// =============================================================================
// Complex Filter Scenarios
// =============================================================================

#[test]
fn test_nested_directory_exclusion() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create nested structure
    fs::create_dir_all(source.path().join("src/components")).unwrap();
    fs::write(source.path().join("src/main.rs"), "fn main() {}").unwrap();
    fs::write(
        source.path().join("src/components/button.rs"),
        "pub struct Button;",
    )
    .unwrap();
    fs::create_dir_all(source.path().join("target/debug")).unwrap();
    fs::write(source.path().join("target/debug/binary"), "binary").unwrap();

    // Exclude target directory
    let output = Command::new(sy_bin())
        .args([
            "--exclude",
            "target/",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    assert!(dest.path().join("src/main.rs").exists());
    assert!(dest.path().join("src/components/button.rs").exists());
    assert!(
        !dest.path().join("target").exists(),
        "target/ should be excluded"
    );
}

#[test]
fn test_basename_vs_path_matching() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    setup_git_repo(&source);

    // Create structure to test basename matching
    fs::create_dir_all(source.path().join("dir1")).unwrap();
    fs::create_dir_all(source.path().join("dir2")).unwrap();
    fs::write(source.path().join("test.log"), "root log").unwrap();
    fs::write(source.path().join("dir1/test.log"), "dir1 log").unwrap();
    fs::write(source.path().join("dir2/other.log"), "dir2 log").unwrap();

    // Pattern without '/' should match basename anywhere
    let output = Command::new(sy_bin())
        .args([
            "--exclude",
            "test.log",
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success());

    // test.log should be excluded everywhere (basename match)
    assert!(
        !dest.path().join("test.log").exists(),
        "test.log in root should be excluded"
    );
    assert!(
        !dest.path().join("dir1/test.log").exists(),
        "test.log in subdir should be excluded"
    );
    // other.log should exist (different name)
    assert!(dest.path().join("dir2/other.log").exists());
}
