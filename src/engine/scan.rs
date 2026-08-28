/// Metadata requested during the cheap ordered reconciliation scan.
///
/// Keep this list small. Anything that requires additional content I/O or
/// expensive metadata enumeration belongs to demand-driven planning instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMetadataRequest {
    /// Include Unix permission bits when available.
    pub unix_mode: bool,
    /// Resolve symlink targets for symlink comparison/preservation.
    pub symlink_target: bool,
    /// Include a stable-enough metadata identity token for TOCTOU validation.
    pub identity: bool,
    /// Include hardlink grouping when the endpoint can identify link topology.
    pub hardlink_group: bool,
}

impl Default for EntryMetadataRequest {
    fn default() -> Self {
        Self {
            unix_mode: false,
            symlink_target: true,
            identity: true,
            hardlink_group: false,
        }
    }
}

/// Endpoint-independent request for an ordered tree scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanRequest {
    pub respect_gitignore: bool,
    pub include_git_dir: bool,
    /// Maximum walk depth relative to the endpoint root. `None` means recursive.
    pub max_depth: Option<usize>,
    pub metadata: EntryMetadataRequest,
}

impl Default for ScanRequest {
    fn default() -> Self {
        Self {
            respect_gitignore: false,
            include_git_dir: true,
            max_depth: None,
            metadata: EntryMetadataRequest::default(),
        }
    }
}

impl ScanRequest {
    pub const fn shallow(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_scan_is_lean_but_identity_safe() {
        let request = ScanRequest::default();
        assert!(!request.metadata.unix_mode);
        assert!(request.metadata.symlink_target);
        assert!(request.metadata.identity);
        assert!(!request.metadata.hardlink_group);
        assert!(request.include_git_dir);
        assert_eq!(request.max_depth, None);
    }
}
