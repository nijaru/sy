//! Actual CLI protection for deferred local tree-root acquisition.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

fn sync(source: &Path, destination: &Path, flags: &[&str]) -> Output {
    let mut source = source.as_os_str().to_os_string();
    source.push("/");
    Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(source)
        .arg(destination)
        .args(flags)
        .args(["--quiet", "--no-hooks"])
        .output()
        .unwrap()
}

#[test]
fn local_tree_missing_parents_wait_for_a_chosen_create() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let missing = fixture.path().join("missing");
    let destination = missing.join("parents/destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("directory")).unwrap();
    std::fs::write(source.join("directory/file"), b"source content").unwrap();
    for flags in [vec!["--existing"], vec!["--dry-run"], vec!["--exclude=*"]] {
        let output = sync(&source, &destination, &flags);
        assert!(
            output.status.success(),
            "{flags:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!missing.exists(), "{flags:?} created missing parents");
    }
    let output = sync(&source, &destination, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(destination.join("directory/file")).unwrap(),
        b"source content"
    );
}

#[test]
fn local_tree_failed_complete_scan_leaves_every_missing_parent_absent() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    let missing = fixture.path().join("missing");
    std::fs::create_dir(&source).unwrap();
    // A valid earlier Create cannot admit root creation when a later scan
    // observation fails. A special file fails deterministically even as root.
    std::fs::write(source.join("a-file"), b"would copy").unwrap();
    let socket = std::os::unix::net::UnixListener::bind(source.join("z-socket")).unwrap();
    let output = sync(&source, &missing.join("parents/destination"), &[]);
    assert!(!output.status.success());
    assert!(!missing.exists(), "failed scan created destination parents");
    assert_eq!(std::fs::read(source.join("a-file")).unwrap(), b"would copy");
    drop(socket);
}
