//! Filter tests — include, exclude, filter rules, gitignore, size filters.
//!
//! Consolidates: filter_cli_test.rs, size_filter_test.rs, gitignore tests.

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
fn test_exclude_flag_basic() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--exclude")
        .arg("*.log")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_exclude_flag_multiple() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();
    fs::write(source.path().join("file.tmp"), "tmp").unwrap();

    let output = sy()
        .arg("--exclude")
        .arg("*.log")
        .arg("--exclude")
        .arg("*.tmp")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("file.log").exists());
    assert!(!dest.path().join("file.tmp").exists());
}

#[test]
fn test_exclude_directory() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::create_dir(source.path().join("node_modules")).unwrap();
    fs::write(source.path().join("node_modules/package"), "dep").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = sy()
        .arg("--exclude")
        .arg("node_modules")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("node_modules").exists());
}

#[test]
fn test_include_flag_basic() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--include")
        .arg("*.txt")
        .arg("--exclude")
        .arg("*")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_include_exclude_order_matters() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();

    let output = sy()
        .arg("--include")
        .arg("*.txt")
        .arg("--exclude")
        .arg("*")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("file.log").exists());
}

#[test]
fn test_filter_flag_include_syntax() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--filter")
        .arg("+ *.txt")
        .arg("--filter")
        .arg("- *")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_filter_flag_exclude_syntax() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--filter")
        .arg("- *.log")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_exclude_from_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let filter_file = TempDir::new().unwrap().into_path().join("exclude.txt");

    fs::write(&filter_file, "*.log\n*.tmp").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();
    fs::write(source.path().join("file.tmp"), "tmp").unwrap();

    let output = sy()
        .arg("--exclude-from")
        .arg(&filter_file)
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("file.log").exists());
    assert!(!dest.path().join("file.tmp").exists());
}

#[test]
fn test_include_from_file() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let filter_file = TempDir::new().unwrap().into_path().join("include.txt");

    fs::write(&filter_file, "*.txt").unwrap();
    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--include-from")
        .arg(&filter_file)
        .arg("--exclude")
        .arg("*")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_nested_directory_exclusion() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::create_dir_all(source.path().join("a/b/c")).unwrap();
    fs::write(source.path().join("a/b/c/file.txt"), "nested").unwrap();
    fs::write(source.path().join("a/b/c/exclude.log"), "log").unwrap();

    let output = sy()
        .arg("--exclude")
        .arg("*.log")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("a/b/c/file.txt").exists());
    assert!(!dest.path().join("a/b/c/exclude.log").exists());
}

#[test]
fn test_min_size_filter() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("small.txt"), "hi").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = sy()
        .arg("--min-size")
        .arg("10")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("small.txt").exists());
    assert!(dest.path().join("large.txt").exists());
}

#[test]
fn test_max_size_filter() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("small.txt"), "hi").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = sy()
        .arg("--max-size")
        .arg("5")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("small.txt").exists());
    assert!(!dest.path().join("large.txt").exists());
}

#[test]
fn test_min_max_size_filter_combined() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join("tiny.txt"), "a").unwrap();
    fs::write(source.path().join("medium.txt"), "hello").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = sy()
        .arg("--min-size")
        .arg("3")
        .arg("--max-size")
        .arg("10")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("tiny.txt").exists());
    assert!(dest.path().join("medium.txt").exists());
    assert!(!dest.path().join("large.txt").exists());
}

#[test]
fn test_gitignore_basic() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    fs::write(source.path().join(".gitignore"), "*.log\nbuild/").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();
    fs::create_dir(source.path().join("build")).unwrap();
    fs::write(source.path().join("build/output"), "output").unwrap();

    let output = sy()
        .arg("--gitignore")
        .arg(source.path())
        .arg(dest.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(dest.path().join(".gitignore").exists());
    assert!(!dest.path().join("exclude.log").exists());
    assert!(!dest.path().join("build").exists());
}
