//! Exercise the real session entrypoint: controller journals must not touch the
//! source before a scanner reports an unsafe temporary-directory configuration.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn snapshot(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    fn visit(path: &Path, entries: &mut Vec<(PathBuf, SystemTime)>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        entries.push((path.to_path_buf(), metadata.modified().unwrap()));
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, &mut entries);
    entries.sort();
    entries
}

#[test]
fn cli_refuses_source_side_scratch_before_session_journals() {
    let source = tempfile::TempDir::new().unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(source.path().join("nested")).unwrap();
    std::fs::write(source.path().join("file"), b"source").unwrap();
    std::fs::write(dest.path().join("extra"), b"must survive").unwrap();
    let before = snapshot(source.path());
    for scratch in [source.path().to_path_buf(), source.path().join("nested")] {
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .args(["--delete", "--max-delete", "100%"])
            .arg(format!("{}/", source.path().display()))
            .arg(dest.path())
            .env("TMPDIR", &scratch)
            .env("TMP", &scratch)
            .env("TEMP", &scratch)
            .output()
            .unwrap();
        assert_eq!(
            snapshot(source.path()),
            before,
            "source names/mtimes changed"
        );
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("temporary directory is inside scanned root"),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(source.path().join("file")).unwrap(),
            b"source"
        );
        assert_eq!(
            std::fs::read(dest.path().join("extra")).unwrap(),
            b"must survive"
        );
        assert!(!dest.path().join("file").exists());
    }
}

#[cfg(unix)]
#[test]
fn cli_resolves_scratch_and_source_aliases_but_accepts_temp_ancestor() {
    use std::os::unix::fs::symlink;
    let parent = tempfile::TempDir::new().unwrap();
    let source = parent.path().join("source");
    let scratch_alias = parent.path().join("scratch-alias");
    let source_alias = parent.path().join("source-alias");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"source").unwrap();
    symlink(&source, &scratch_alias).unwrap();
    symlink(&source, &source_alias).unwrap();
    let dest = tempfile::TempDir::new().unwrap();
    let before = snapshot(&source);
    let parent_path = parent.path().to_path_buf();
    for (root, scratch, succeeds) in [
        (&source, &scratch_alias, false),
        (&source_alias, &source, false),
        (&source, &parent_path, true),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sy"))
            .arg(format!("{}/", root.display()))
            .arg(dest.path())
            .env("TMPDIR", scratch)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            succeeds,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(snapshot(&source), before);
    }
    assert_eq!(std::fs::read(dest.path().join("file")).unwrap(), b"source");
}
