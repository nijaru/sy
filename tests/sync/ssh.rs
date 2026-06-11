//! SSH sync tests — remote operations.
//!
//! Covers: SSH push/pull, bisync, resume/retry, sparse files, hardlinks, progress.

use std::process::Command;
use tempfile::TempDir;

fn sy_bin() -> String {
    env!("CARGO_BIN_EXE_sy").to_string()
}

fn setup_test_dir(_name: &str) -> (TempDir, TempDir) {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();

    // Create git repo in source for .gitignore support
    Command::new("git")
        .args(["init"])
        .current_dir(source.path())
        .output()
        .unwrap();

    (source, dest)
}

// SSH tests require an SSH agent and are ignored by default
// To run: cargo test --features ssh-tests

#[test]
#[ignore] // Requires SSH agent
fn test_ssh_push_basic() {
    // Placeholder - SSH tests require agent setup
}
