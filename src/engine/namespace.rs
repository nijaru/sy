use crate::engine::domain::RelativePath;
use crate::protocol::PlatformOs;
use std::collections::HashMap;
use std::fmt;
use unicode_normalization::UnicodeNormalization;

/// Namespace case and normalization policy for a destination endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaseSensitivity {
    /// Case-sensitive, byte-exact components (e.g. Linux ext4/btrfs default).
    #[default]
    Sensitive,
    /// Case-insensitive with Unicode canonical decomposition (NFD) normalization
    /// (e.g. macOS APFS, Windows NTFS).
    Insensitive,
}

impl CaseSensitivity {
    pub fn for_platform(os: PlatformOs) -> Self {
        match os {
            PlatformOs::Macos | PlatformOs::Windows => Self::Insensitive,
            _ => Self::Sensitive,
        }
    }
}

/// Normalized representation of a relative path for collision preflight.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NormalizedKey(Vec<u8>);

impl fmt::Debug for NormalizedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.0))
    }
}

impl NormalizedKey {
    pub fn from_path(path: &RelativePath, sensitivity: CaseSensitivity) -> Self {
        match sensitivity {
            CaseSensitivity::Sensitive => {
                Self(path.as_path().as_os_str().as_encoded_bytes().to_vec())
            }
            CaseSensitivity::Insensitive => {
                let mut bytes = Vec::new();
                for (i, component) in path.as_path().components().enumerate() {
                    if i > 0 {
                        bytes.push(0); // delimiter between path components
                    }
                    let comp_bytes = component.as_os_str().as_encoded_bytes();
                    if let Ok(s) = std::str::from_utf8(comp_bytes) {
                        for ch in s.to_lowercase().nfd() {
                            for lower_ch in ch.to_lowercase() {
                                let mut buf = [0u8; 4];
                                bytes.extend_from_slice(lower_ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                    } else {
                        for &b in comp_bytes {
                            bytes.push(b.to_ascii_lowercase());
                        }
                    }
                }
                Self(bytes)
            }
        }
    }
}

/// Destination namespace collision detected during preflight before mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "destination namespace collision: '{colliding}' collides with '{existing}' under case-insensitive/normalizing semantics"
)]
pub struct NamespaceCollision {
    pub existing: RelativePath,
    pub colliding: RelativePath,
}

/// Preflight collision detector for case-insensitive or normalizing targets.
#[derive(Debug, Default)]
pub struct NamespaceCollisionDetector {
    sensitivity: CaseSensitivity,
    seen: HashMap<NormalizedKey, RelativePath>,
}

impl NamespaceCollisionDetector {
    pub fn new(sensitivity: CaseSensitivity) -> Self {
        Self {
            sensitivity,
            seen: HashMap::new(),
        }
    }

    pub fn sensitivity(&self) -> CaseSensitivity {
        self.sensitivity
    }

    /// Check a path and all its ancestor directory prefixes.
    ///
    /// If an entry with the same normalized key but different exact path was
    /// already recorded, returns `Err(NamespaceCollision)`.
    pub fn check_and_record(&mut self, path: &RelativePath) -> Result<(), NamespaceCollision> {
        if self.sensitivity == CaseSensitivity::Sensitive {
            return Ok(());
        }

        self.check_single(path)?;

        let mut current = path.parent();
        while let Some(parent) = current {
            self.check_single(&parent)?;
            current = parent.parent();
        }

        Ok(())
    }

    fn check_single(&mut self, path: &RelativePath) -> Result<(), NamespaceCollision> {
        let key = NormalizedKey::from_path(path, self.sensitivity);
        if let Some(existing) = self.seen.get(&key) {
            if existing != path {
                return Err(NamespaceCollision {
                    existing: existing.clone(),
                    colliding: path.clone(),
                });
            }
        } else {
            self.seen.insert(key, path.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> RelativePath {
        RelativePath::new(path).unwrap()
    }

    #[test]
    fn case_folding_detects_collision() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        detector.check_and_record(&rel("foo/bar.txt")).unwrap();
        let err = detector.check_and_record(&rel("FOO/BAR.TXT")).unwrap_err();
        assert_eq!(err.existing, rel("foo/bar.txt"));
        assert_eq!(err.colliding, rel("FOO/BAR.TXT"));
    }

    #[test]
    fn case_sensitive_mode_permits_case_differences() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Sensitive);
        detector.check_and_record(&rel("foo.txt")).unwrap();
        detector.check_and_record(&rel("FOO.TXT")).unwrap();
        assert_eq!(detector.seen.len(), 0); // Sensitive mode bypasses tracking
    }

    #[test]
    fn unicode_normalization_detects_collision() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        let nfc = rel("caf\u{e9}.txt");
        let nfd = rel("cafe\u{301}.txt");
        assert_ne!(nfc, nfd);
        detector.check_and_record(&nfc).unwrap();
        let err = detector.check_and_record(&nfd).unwrap_err();
        assert_eq!(err.existing, nfc);
        assert_eq!(err.colliding, nfd);
    }

    #[test]
    fn distinct_paths_do_not_collide() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        detector.check_and_record(&rel("a/b.txt")).unwrap();
        detector.check_and_record(&rel("a/c.txt")).unwrap();
        detector.check_and_record(&rel("b/b.txt")).unwrap();
    }

    #[test]
    fn exact_matches_do_not_collide() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        detector.check_and_record(&rel("foo/bar.txt")).unwrap();
        detector.check_and_record(&rel("foo/bar.txt")).unwrap();
    }

    #[test]
    fn parent_directory_case_collision_detected() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        detector.check_and_record(&rel("Dir/file1.txt")).unwrap();
        let err = detector
            .check_and_record(&rel("dir/file2.txt"))
            .unwrap_err();
        assert_eq!(err.existing, rel("Dir"));
        assert_eq!(err.colliding, rel("dir"));
    }

    #[test]
    fn component_delimiters_prevent_concatenation_collision() {
        let mut detector = NamespaceCollisionDetector::new(CaseSensitivity::Insensitive);
        detector.check_and_record(&rel("a/bc")).unwrap();
        detector.check_and_record(&rel("ab/c")).unwrap();
    }
}
