//! Filter tests — include, exclude, filter rules, gitignore, size filters.
//!
//! Consolidates: filter_cli_test.rs, size_filter_test.rs, gitignore tests.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir() -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();
    (source, dest)
}

fn sync_args<'a>(source: &'a TempDir, dest: &'a TempDir, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        format!("{}/", source.path().display()),
        dest.path().to_str().unwrap().to_string(),
        "--exclude-vcs".to_string(),
    ];
    for e in extra {
        args.push(e.to_string());
    }
    args
}

#[test]
fn test_exclude_flag_basic() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--exclude", "*.log"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_exclude_flag_multiple() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();
    fs::write(source.path().join("file.tmp"), "tmp").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--exclude", "*.log", "--exclude", "*.tmp"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("file.log").exists());
    assert!(!dest.path().join("file.tmp").exists());
}

#[test]
fn test_exclude_directory() {
    let (source, dest) = setup_test_dir();

    fs::create_dir(source.path().join("node_modules")).unwrap();
    fs::write(source.path().join("node_modules/package"), "dep").unwrap();
    fs::write(source.path().join("file.txt"), "content").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--exclude", "node_modules/"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("node_modules").exists());
}

#[test]
fn test_include_flag_basic() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--include", "*.txt", "--exclude", "*"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_include_exclude_order_matters() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("file.txt"), "content").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--include", "*.txt", "--exclude", "*"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("file.txt").exists());
    assert!(!dest.path().join("file.log").exists());
}

#[test]
fn test_filter_flag_include_syntax() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--filter", "+ *.txt", "--filter", "- *"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_filter_flag_exclude_syntax() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--filter", "- *.log"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_exclude_from_file() {
    let (source, dest) = setup_test_dir();
    let filter_file = TempDir::new().unwrap().into_path().join("exclude.txt");

    fs::write(&filter_file, "*.log\n*.tmp").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("file.log"), "log").unwrap();
    fs::write(source.path().join("file.tmp"), "tmp").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--exclude-from"]))
        .arg(&filter_file)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(!dest.path().join("file.log").exists());
    assert!(!dest.path().join("file.tmp").exists());
}

#[test]
fn test_include_from_file() {
    let (source, dest) = setup_test_dir();
    let filter_file = TempDir::new().unwrap().into_path().join("include.txt");

    fs::write(&filter_file, "*.txt").unwrap();
    fs::write(source.path().join("include.txt"), "include").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--include-from"]))
        .arg(&filter_file)
        .args(["--exclude", "*"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("include.txt").exists());
    assert!(!dest.path().join("exclude.log").exists());
}

#[test]
fn test_nested_directory_exclusion() {
    let (source, dest) = setup_test_dir();

    fs::create_dir_all(source.path().join("a/b/c")).unwrap();
    fs::write(source.path().join("a/b/c/file.txt"), "nested").unwrap();
    fs::write(source.path().join("a/b/c/exclude.log"), "log").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--exclude", "*.log"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("a/b/c/file.txt").exists());
    assert!(!dest.path().join("a/b/c/exclude.log").exists());
}

#[test]
fn test_min_size_filter() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("small.txt"), "hi").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--min-size", "10"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("small.txt").exists());
    assert!(dest.path().join("large.txt").exists());
}

#[test]
fn test_max_size_filter() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("small.txt"), "hi").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--max-size", "5"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("small.txt").exists());
    assert!(!dest.path().join("large.txt").exists());
}

#[test]
fn test_min_max_size_filter_combined() {
    let (source, dest) = setup_test_dir();

    fs::write(source.path().join("tiny.txt"), "a").unwrap();
    fs::write(source.path().join("medium.txt"), "hello").unwrap();
    fs::write(source.path().join("large.txt"), "hello world!").unwrap();

    let output = Command::new(sy_bin())
        .args(sync_args(&source, &dest, &["--min-size", "3", "--max-size", "10"]))
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!dest.path().join("tiny.txt").exists());
    assert!(dest.path().join("medium.txt").exists());
    assert!(!dest.path().join("large.txt").exists());
}

#[test]
fn test_gitignore_basic() {
    let (source, dest) = setup_test_dir();

    // Initialize git repo for gitignore to work
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();

    fs::write(source.path().join(".gitignore"), "*.log\nbuild/").unwrap();
    fs::write(source.path().join("keep.txt"), "keep").unwrap();
    fs::write(source.path().join("exclude.log"), "log").unwrap();
    fs::create_dir(source.path().join("build")).unwrap();
    fs::write(source.path().join("build/output"), "output").unwrap();

    // Use --gitignore to respect .gitignore rules
    let output = Command::new(sy_bin())
        .args([
            &format!("{}/", source.path().display()),
            dest.path().to_str().unwrap(),
            "--gitignore",
        ])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(dest.path().join("keep.txt").exists());
    assert!(dest.path().join(".gitignore").exists());
    assert!(!dest.path().join("exclude.log").exists());
    assert!(!dest.path().join("build").exists());
}
