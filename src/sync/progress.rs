// Per-file progress display for large file transfers
//
// Shows real-time progress bars for files >= 1MB to provide better UX
// for long-running transfers without cluttering output for small files.
//
// TTY Detection:
// - indicatif automatically detects if stdout is a TTY
// - Progress bars are hidden when output is piped/redirected
// - Respects --quiet flag (handled in SyncEngine)
//
// Usage:
//   sy /source /dest --per-file-progress  # Show progress for large files
//   sy /source /dest --quiet               # Hide all progress

use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Minimum file size (in bytes) to show progress bar
/// Files smaller than this are transferred too quickly for meaningful progress display
pub const MIN_SIZE_FOR_PROGRESS: u64 = 1024 * 1024; // 1MB

/// Create a progress bar for a file transfer
///
/// Returns Arc-wrapped progress bar that can be shared with the callback
pub fn create_file_progress_bar(file_path: &Path, total_bytes: u64) -> Arc<ProgressBar> {
    let pb = ProgressBar::new(total_bytes);

    // Format: filename [=====>] 45.2 MB/100 MB (45%) 12 MB/s ETA: 4s
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}\n[{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%) {bytes_per_sec} ETA: {eta}"
        )
        .unwrap()
        .progress_chars("=>-")
    );

    // Set message to filename (truncate if too long)
    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let msg = if filename.len() > 60 {
        format!("{}...", &filename[..57])
    } else {
        filename.to_string()
    };

    pb.set_message(msg);

    // Update every 100ms for smooth progress
    pb.enable_steady_tick(Duration::from_millis(100));

    Arc::new(pb)
}

/// Create a progress callback for a file transfer
///
/// Returns a callback that can be passed to copy_file_streaming
pub fn create_progress_callback(
    file_path: &Path,
    total_bytes: u64,
) -> Arc<dyn Fn(u64, u64) + Send + Sync> {
    let pb = create_file_progress_bar(file_path, total_bytes);

    Arc::new(move |bytes_transferred: u64, _total: u64| {
        pb.set_position(bytes_transferred);
    })
}

/// Finish a progress bar
///
/// Call this after the transfer completes to finalize the display
#[allow(dead_code)]
pub fn finish_progress_bar(pb: Arc<ProgressBar>) {
    pb.finish_with_message("✓ Transfer complete");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_min_size_constant() {
        assert_eq!(MIN_SIZE_FOR_PROGRESS, 1024 * 1024);
    }

    #[test]
    fn test_create_progress_bar() {
        let path = PathBuf::from("/tmp/test.txt");
        let pb = create_file_progress_bar(&path, 1000);
        assert_eq!(pb.length(), Some(1000));
    }

    #[test]
    fn test_create_progress_bar_long_filename() {
        let long_name = "a".repeat(100);
        let path = PathBuf::from(format!("/tmp/{}", long_name));
        let pb = create_file_progress_bar(&path, 1000);

        // Message should be truncated
        let msg = pb.message();
        assert!(msg.len() <= 63); // 60 + "..."
    }

    #[test]
    fn test_create_progress_callback() {
        let path = PathBuf::from("/tmp/test.txt");
        let callback = create_progress_callback(&path, 1000);

        // Call callback to ensure it doesn't panic
        callback(500, 1000);
        callback(1000, 1000);
    }

    #[test]
    fn test_finish_progress_bar() {
        let path = PathBuf::from("/tmp/test.txt");
        let pb = create_file_progress_bar(&path, 1000);
        pb.set_position(1000);
        finish_progress_bar(pb);
        // Should not panic
    }
}
