//! Local sync correctness across wide trees, deep trees, and multi-buffer files.
//! Speed belongs in benches/sync_bench.rs and scripts/benchmark.py; scanner and
//! scheduler unit tests exercise actual resource budgets rather than elapsed time.

use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn sync(source: &Path, dest: &Path, options: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_sy"))
        .arg(format!("{}/", source.display()))
        .arg(dest)
        .args(["--json", "--preserve-times"])
        .args(options)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "sync failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summaries: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|event: &serde_json::Value| event["type"] == "summary")
        .collect();
    assert_eq!(summaries.len(), 1);
    summaries.into_iter().next().unwrap()
}

fn assert_no_transfers(summary: &serde_json::Value, files: usize) {
    assert_eq!(summary["files_created"], 0);
    assert_eq!(summary["files_updated"], 0);
    assert_eq!(summary["files_deleted"], 0);
    assert_eq!(summary["bytes_transferred"], 0);
    assert_eq!(summary["files_skipped"], files);
}

fn assert_not_replaced(before: &Metadata, path: &Path) {
    let after = fs::metadata(path).unwrap();
    assert_eq!(after.len(), before.len(), "{}", path.display());
    assert_eq!(
        after.modified().unwrap(),
        before.modified().unwrap(),
        "{}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            (after.dev(), after.ino()),
            (before.dev(), before.ino()),
            "unchanged file was replaced: {}",
            path.display()
        );
    }
}

#[test]
fn wide_tree_copies_contents_and_leaves_unchanged_files_in_place() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let count = 1000;
    for index in (0..count).rev() {
        fs::write(
            source.path().join(format!("file_{index:04}.txt")),
            format!("content {index}\n"),
        )
        .unwrap();
    }

    let first = sync(source.path(), dest.path(), &[]);
    assert_eq!(first["files_created"], count);
    let before: Vec<_> = (0..count)
        .map(|index| {
            let path = dest.path().join(format!("file_{index:04}.txt"));
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                format!("content {index}\n")
            );
            fs::metadata(path).unwrap()
        })
        .collect();
    // Keep an inode alive even if a faulty second run unlinks its visible name.
    let held = File::open(dest.path().join("file_0000.txt")).unwrap();

    let second = sync(source.path(), dest.path(), &[]);
    assert_no_transfers(&second, count);
    assert_eq!(
        fs::read_dir(dest.path())
            .unwrap()
            .map(Result::unwrap)
            .count(),
        count
    );
    for (index, metadata) in before.iter().enumerate() {
        let name = format!("file_{index:04}.txt");
        let path = dest.path().join(&name);
        assert_not_replaced(metadata, &path);
        let expected = format!("content {index}\n");
        assert_eq!(fs::read_to_string(path).unwrap(), expected);
        assert_eq!(
            fs::read_to_string(source.path().join(name)).unwrap(),
            expected
        );
    }
    assert_not_replaced(
        &held.metadata().unwrap(),
        &dest.path().join("file_0000.txt"),
    );
}

#[test]
fn deep_tree_copies_every_level_and_empty_directory() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let mut relative = std::path::PathBuf::new();
    for level in 0..50 {
        relative.push(format!("level_{level}"));
        let path = source.path().join(&relative);
        fs::create_dir_all(path.join("empty")).unwrap();
        fs::write(path.join("file.txt"), format!("level {level}\n")).unwrap();
    }

    sync(source.path(), dest.path(), &[]);
    relative.clear();
    for level in 0..50 {
        relative.push(format!("level_{level}"));
        let path = dest.path().join(&relative);
        assert_eq!(
            fs::read_to_string(path.join("file.txt")).unwrap(),
            format!("level {level}\n")
        );
        assert_eq!(
            fs::read_dir(path.join("empty"))
                .unwrap()
                .map(Result::unwrap)
                .count(),
            0
        );
    }
}

#[test]
fn gitignore_filters_names_without_losing_included_contents() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let git = Command::new("git")
        .arg("init")
        .current_dir(source.path())
        .output()
        .unwrap();
    assert!(
        git.status.success(),
        "{}",
        String::from_utf8_lossy(&git.stderr)
    );
    let rules = "*.ignored\n";
    fs::write(source.path().join(".gitignore"), rules).unwrap();
    for index in 0..50 {
        fs::write(
            source.path().join(format!("file_{index}.txt")),
            format!("included {index}\n"),
        )
        .unwrap();
        fs::write(
            source.path().join(format!("file_{index}.ignored")),
            format!("ignored {index}\n"),
        )
        .unwrap();
    }

    sync(
        source.path(),
        dest.path(),
        &["--gitignore", "--exclude-vcs"],
    );
    assert_eq!(
        fs::read_to_string(dest.path().join(".gitignore")).unwrap(),
        rules
    );
    let mut actual: Vec<_> = fs::read_dir(dest.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let mut expected: Vec<std::ffi::OsString> = vec![".gitignore".into()];
    for index in 0..50 {
        let name = format!("file_{index}.txt");
        assert_eq!(
            fs::read_to_string(dest.path().join(&name)).unwrap(),
            format!("included {index}\n")
        );
        assert_eq!(
            fs::read_to_string(source.path().join(format!("file_{index}.ignored"))).unwrap(),
            format!("ignored {index}\n")
        );
        expected.push(name.into());
    }
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

const BLOCK_BYTES: usize = 64 * 1024;
// Cross many transfer buffers with distinct blocks and a partial tail.
// Neither fixture creation nor verification allocates a whole file.
const LARGE_BYTES: usize = 32 * 1024 * 1024 + 17;

fn fill_block(block: &mut [u8; BLOCK_BYTES], index: u64) {
    for (offset, byte) in block.iter_mut().enumerate() {
        *byte = ((offset as u64 + index) % 251) as u8;
    }
    block[..8].copy_from_slice(&index.to_le_bytes());
}

fn assert_large_contents(path: &Path) {
    let mut file = File::open(path).unwrap();
    assert_eq!(file.metadata().unwrap().len(), LARGE_BYTES as u64);
    let mut expected = [0; BLOCK_BYTES];
    let mut actual = [0; BLOCK_BYTES];
    for (index, start) in (0..LARGE_BYTES).step_by(BLOCK_BYTES).enumerate() {
        fill_block(&mut expected, index as u64);
        let length = BLOCK_BYTES.min(LARGE_BYTES - start);
        file.read_exact(&mut actual[..length]).unwrap();
        assert!(
            actual[..length] == expected[..length],
            "{}: corrupt block {index}",
            path.display()
        );
    }
    assert_eq!(file.read(&mut actual[..1]).unwrap(), 0);
}

#[test]
fn large_file_copies_all_bytes_and_is_not_replaced_on_repeat() {
    let source = TempDir::new().unwrap();
    let dest = TempDir::new().unwrap();
    let source_file = source.path().join("large.bin");
    let dest_file = dest.path().join("large.bin");
    let mut file = File::create(&source_file).unwrap();
    let mut block = [0; BLOCK_BYTES];
    for (index, start) in (0..LARGE_BYTES).step_by(BLOCK_BYTES).enumerate() {
        fill_block(&mut block, index as u64);
        file.write_all(&block[..BLOCK_BYTES.min(LARGE_BYTES - start)])
            .unwrap();
    }
    drop(file);

    let first = sync(source.path(), dest.path(), &["--verify=after"]);
    assert_eq!(first["files_created"], 1);
    assert_eq!(first["files_verified"], 1);
    assert_eq!(first["verification_failures"], 0);
    assert_large_contents(&dest_file);
    let held = File::open(&dest_file).unwrap();
    let before = held.metadata().unwrap();

    let second = sync(source.path(), dest.path(), &[]);
    assert_no_transfers(&second, 1);
    assert_not_replaced(&before, &dest_file);
    assert_large_contents(&dest_file);
    assert_large_contents(&source_file);
    assert_eq!(
        fs::read_dir(dest.path())
            .unwrap()
            .map(Result::unwrap)
            .count(),
        1
    );
}
