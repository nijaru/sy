use proptest::prelude::*;
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

proptest! {
    #[test]
    fn prop_all_files_synced(file_count in 1usize..20) {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create files with deterministic names
        for i in 0..file_count {
            let content = format!("content_{}", i);
            fs::write(source.path().join(format!("file_{}.txt", i)), content).unwrap();
        }

        // Run sync
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        prop_assert!(output.status.success());

        // Verify all files exist in destination
        for i in 0..file_count {
            let dest_file = dest.path().join(format!("file_{}.txt", i));
            prop_assert!(dest_file.exists());

            let expected = format!("content_{}", i);
            let actual = fs::read_to_string(&dest_file).unwrap();
            prop_assert_eq!(actual, expected);
        }
    }

    #[test]
    fn prop_sync_idempotent(file_count in 1usize..10) {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create files
        for i in 0..file_count {
            fs::write(source.path().join(format!("file_{}.txt", i)), format!("content_{}", i)).unwrap();
        }

        // First sync
        let output1 = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        prop_assert!(output1.status.success());

        // Second sync
        let output2 = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        prop_assert!(output2.status.success());

        let stdout = String::from_utf8_lossy(&output2.stdout);
        // Second sync should skip all files
        let expected_skip_line = format!("Files skipped:     {}", file_count);
        prop_assert!(stdout.contains(&expected_skip_line));
    }

    #[test]
    fn prop_delete_removes_extras(
        source_count in 1usize..10,
        dest_extra_count in 1usize..10,
    ) {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create source files
        for i in 0..source_count {
            fs::write(source.path().join(format!("keep_{}.txt", i)), "keep").unwrap();
        }

        // Create dest files (with extras)
        for i in 0..source_count {
            fs::write(dest.path().join(format!("keep_{}.txt", i)), "keep").unwrap();
        }
        for i in 0..dest_extra_count {
            fs::write(dest.path().join(format!("extra_{}.txt", i)), "extra").unwrap();
        }

        // Sync with --delete and --force-delete to bypass safety thresholds
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
                "--delete",
                "--force-delete",
            ])
            .output()
            .unwrap();

        prop_assert!(output.status.success());

        // Verify kept files exist
        for i in 0..source_count {
            let keep_file = dest.path().join(format!("keep_{}.txt", i));
            prop_assert!(keep_file.exists());
        }

        // Verify extra files removed
        for i in 0..dest_extra_count {
            let extra_file = dest.path().join(format!("extra_{}.txt", i));
            prop_assert!(!extra_file.exists());
        }
    }

    #[test]
    fn prop_nested_dirs_preserved(depth in 1usize..5, files_per_level in 1usize..5) {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create nested structure
        let mut current_path = source.path().to_path_buf();
        for d in 0..depth {
            current_path = current_path.join(format!("level_{}", d));
            fs::create_dir_all(&current_path).unwrap();

            for f in 0..files_per_level {
                fs::write(
                    current_path.join(format!("file_{}.txt", f)),
                    format!("depth_{}_file_{}", d, f),
                ).unwrap();
            }
        }

        // Sync
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();

        prop_assert!(output.status.success());

        // Verify structure preserved
        let mut current_path = dest.path().to_path_buf();
        for d in 0..depth {
            current_path = current_path.join(format!("level_{}", d));
            prop_assert!(current_path.exists());

            for f in 0..files_per_level {
                let file_path = current_path.join(format!("file_{}.txt", f));
                prop_assert!(file_path.exists());

                let content = fs::read_to_string(&file_path).unwrap();
                prop_assert_eq!(content, format!("depth_{}_file_{}", d, f));
            }
        }
    }

    #[test]
    fn prop_dry_run_makes_no_changes(file_count in 1usize..20) {
        let source = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        setup_git_repo(&source);

        // Create files
        for i in 0..file_count {
            fs::write(source.path().join(format!("file_{}.txt", i)), format!("content_{}", i)).unwrap();
        }

        // Dry run
        let output = Command::new(sy_bin())
            .args([
                &format!("{}/", source.path().display()),
                dest.path().to_str().unwrap(),
                "--dry-run",
            ])
            .output()
            .unwrap();

        prop_assert!(output.status.success());

        // Verify no files created
        for i in 0..file_count {
            let file_path = dest.path().join(format!("file_{}.txt", i));
            prop_assert!(!file_path.exists());
        }
    }
}
