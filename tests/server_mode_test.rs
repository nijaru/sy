#[cfg(test)]
mod tests {
    use std::fs;
    use sy::path::SyncPath;
    use sy::sync::server_mode::{sync_pull_server_mode, sync_server_mode};
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_server_mode_local_to_local() -> anyhow::Result<()> {
        // Setup
        let temp = TempDir::new()?;
        let source = temp.path().join("src");
        let dest = temp.path().join("dest");

        fs::create_dir(&source)?;
        fs::create_dir(&dest)?;

        // Create source files
        fs::write(source.join("file1.txt"), "Hello World")?;
        fs::write(source.join("file2.txt"), "Another file")?;
        fs::create_dir(source.join("subdir"))?;
        fs::write(source.join("subdir/file3.txt"), "Nested file")?;

        // Run sync
        let dest_sync_path = SyncPath::Local {
            path: dest.clone(),
            has_trailing_slash: false,
        };

        // Note: We need to ensure 'sy' binary is available for the server process
        // This test assumes 'sy' is built and in target/debug or PATH.
        // cargo test builds the binary but doesn't put it in PATH.
        // We can use current_exe() to find the test binary, but we need the main 'sy' binary.
        // Workaround: Point to the raw 'sy' binary path if possible, or skip if not found.
        // For CI, we usually install it.
        // Let's check if we can find it relative to CWD.

        let sy_bin = std::env::current_exe()?
            .parent()
            .unwrap()
            .parent()
            .unwrap() // deps
            .parent()
            .unwrap() // debug
            .join("sy");

        if !sy_bin.exists() {
            eprintln!("Skipping test: sy binary not found at {}", sy_bin.display());
            return Ok(());
        }

        // Update PATH to include 'sy' dir
        let path_env = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", sy_bin.parent().unwrap().display(), path_env);
        std::env::set_var("PATH", new_path);

        sync_server_mode(&source, &dest_sync_path).await?;

        // Verify
        assert!(dest.join("file1.txt").exists());
        assert_eq!(fs::read_to_string(dest.join("file1.txt"))?, "Hello World");
        assert!(dest.join("file2.txt").exists());
        assert!(dest.join("subdir/file3.txt").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_server_mode_pull_local_to_local() -> anyhow::Result<()> {
        // Setup
        let temp = TempDir::new()?;
        let source = temp.path().join("src");
        let dest = temp.path().join("dest");

        fs::create_dir(&source)?;
        fs::create_dir(&dest)?;

        // Create source files (simulating remote)
        fs::write(source.join("file1.txt"), "Pull test file 1")?;
        fs::write(source.join("file2.txt"), "Pull test file 2")?;
        fs::create_dir(source.join("subdir"))?;
        fs::write(source.join("subdir/file3.txt"), "Pull nested file")?;

        // Find sy binary
        let sy_bin = std::env::current_exe()?
            .parent()
            .unwrap()
            .parent()
            .unwrap() // deps
            .parent()
            .unwrap() // debug
            .join("sy");

        if !sy_bin.exists() {
            eprintln!("Skipping test: sy binary not found at {}", sy_bin.display());
            return Ok(());
        }

        // Update PATH to include 'sy' dir
        let path_env = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", sy_bin.parent().unwrap().display(), path_env);
        std::env::set_var("PATH", new_path);

        // Run pull sync (source is "remote", dest is local)
        let source_sync_path = SyncPath::Local {
            path: source.clone(),
            has_trailing_slash: false,
        };

        sync_pull_server_mode(&source_sync_path, &dest).await?;

        // Verify
        assert!(dest.join("file1.txt").exists());
        assert_eq!(
            fs::read_to_string(dest.join("file1.txt"))?,
            "Pull test file 1"
        );
        assert!(dest.join("file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dest.join("file2.txt"))?,
            "Pull test file 2"
        );
        assert!(dest.join("subdir/file3.txt").exists());
        assert_eq!(
            fs::read_to_string(dest.join("subdir/file3.txt"))?,
            "Pull nested file"
        );

        Ok(())
    }
}
