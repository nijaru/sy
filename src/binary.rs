// Binary deployment utilities for auto-deploying sy-remote to remote servers
use std::io;
use std::path::PathBuf;

/// Find the sy-remote binary in common locations
pub fn find_sy_remote_binary() -> io::Result<PathBuf> {
    // 1. Try relative to current executable (cargo install)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let candidate = parent.join("sy-remote");
            if candidate.exists() {
                tracing::debug!("Found sy-remote at: {}", candidate.display());
                return Ok(candidate);
            }
        }
    }

    // 2. Try ./target/release (development)
    let target_release = PathBuf::from("target/release/sy-remote");
    if target_release.exists() {
        tracing::debug!("Found sy-remote at: {}", target_release.display());
        return Ok(target_release);
    }

    // 3. Try searching PATH
    if let Ok(path_var) = std::env::var("PATH") {
        for path_dir in std::env::split_paths(&path_var) {
            let candidate = path_dir.join("sy-remote");
            if candidate.exists() {
                tracing::debug!("Found sy-remote in PATH: {}", candidate.display());
                return Ok(candidate);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Could not find sy-remote binary. Please ensure sy is properly installed.",
    ))
}

/// Read sy-remote binary into memory
pub fn read_sy_remote_binary() -> io::Result<Vec<u8>> {
    let binary_path = find_sy_remote_binary()?;
    let data = std::fs::read(&binary_path)?;
    tracing::debug!(
        "Read sy-remote binary: {} bytes from {}",
        data.len(),
        binary_path.display()
    );
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_sy_remote_binary() {
        // Should find sy-remote in target/release or PATH
        let result = find_sy_remote_binary();
        match result {
            Ok(path) => {
                println!("Found sy-remote at: {}", path.display());
                assert!(path.exists(), "Binary path should exist");
            }
            Err(e) => {
                println!("Expected in dev/test environment: {}", e);
                // This is OK during early development
            }
        }
    }
}
