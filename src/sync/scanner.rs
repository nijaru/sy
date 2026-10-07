//! Scan options for file tree traversal.

/// Options controlling file traversal behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOptions {
    /// Whether to respect `.gitignore` rules during scanning.
    pub respect_gitignore: bool,
    /// Whether to include `.git` directories in traversal.
    pub include_git_dir: bool,
    /// Whether to only visit directory entries without recursing.
    pub dirs_only: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            respect_gitignore: false,
            include_git_dir: true,
            dirs_only: false,
        }
    }
}
