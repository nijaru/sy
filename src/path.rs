use std::path::{Path, PathBuf};

/// Represents a sync path that can be either local or remote
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPath {
    Local(PathBuf),
    Remote {
        host: String,
        user: Option<String>,
        path: PathBuf,
    },
}

impl SyncPath {
    /// Parse a path string into a SyncPath
    ///
    /// Supported formats:
    /// - Local: `/path/to/dir`, `./relative/path`, `relative/path`
    /// - Remote: `user@host:/path`, `host:/path`
    pub fn parse(s: &str) -> Self {
        // Check for remote path format (contains : before any /)
        if let Some(colon_pos) = s.find(':') {
            // Check if this is a remote path (no / before the :)
            let before_colon = &s[..colon_pos];

            // Check if this is a Windows drive letter (single letter followed by :)
            if before_colon.len() == 1 && before_colon.chars().next().unwrap().is_ascii_alphabetic() {
                // Windows drive letter, treat as local
                return SyncPath::Local(PathBuf::from(s));
            }

            if !before_colon.contains('/') && !before_colon.is_empty() {
                // This is a remote path
                let path_part = &s[colon_pos + 1..];

                // Parse user@host or just host
                if let Some(at_pos) = before_colon.find('@') {
                    let user = before_colon[..at_pos].to_string();
                    let host = before_colon[at_pos + 1..].to_string();
                    return SyncPath::Remote {
                        host,
                        user: Some(user),
                        path: PathBuf::from(path_part),
                    };
                } else {
                    return SyncPath::Remote {
                        host: before_colon.to_string(),
                        user: None,
                        path: PathBuf::from(path_part),
                    };
                }
            }
        }

        // Otherwise it's a local path
        SyncPath::Local(PathBuf::from(s))
    }

    /// Get the path component
    pub fn path(&self) -> &Path {
        match self {
            SyncPath::Local(path) => path,
            SyncPath::Remote { path, .. } => path,
        }
    }

    /// Check if this is a remote path
    #[allow(dead_code)] // Used in tests
    pub fn is_remote(&self) -> bool {
        matches!(self, SyncPath::Remote { .. })
    }

    /// Check if this is a local path
    pub fn is_local(&self) -> bool {
        matches!(self, SyncPath::Local(_))
    }
}

impl std::fmt::Display for SyncPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncPath::Local(path) => write!(f, "{}", path.display()),
            SyncPath::Remote { host, user, path } => {
                if let Some(u) = user {
                    write!(f, "{}@{}:{}", u, host, path.display())
                } else {
                    write!(f, "{}:{}", host, path.display())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_absolute() {
        let path = SyncPath::parse("/home/user/docs");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("/home/user/docs"));
    }

    #[test]
    fn test_parse_local_relative() {
        let path = SyncPath::parse("./docs");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("./docs"));
    }

    #[test]
    fn test_parse_local_relative_no_dot() {
        let path = SyncPath::parse("docs/subdir");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("docs/subdir"));
    }

    #[test]
    fn test_parse_remote_with_user() {
        let path = SyncPath::parse("nick@server:/home/nick/docs");
        assert!(path.is_remote());
        assert_eq!(path.path(), Path::new("/home/nick/docs"));
        match path {
            SyncPath::Remote { host, user, .. } => {
                assert_eq!(host, "server");
                assert_eq!(user, Some("nick".to_string()));
            }
            _ => panic!("Expected remote path"),
        }
    }

    #[test]
    fn test_parse_remote_without_user() {
        let path = SyncPath::parse("server:/home/nick/docs");
        assert!(path.is_remote());
        assert_eq!(path.path(), Path::new("/home/nick/docs"));
        match path {
            SyncPath::Remote { host, user, .. } => {
                assert_eq!(host, "server");
                assert_eq!(user, None);
            }
            _ => panic!("Expected remote path"),
        }
    }

    #[test]
    fn test_parse_windows_drive_letter() {
        // C:/path should be treated as local, not remote
        let path = SyncPath::parse("C:/Users/nick");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("C:/Users/nick"));
    }

    #[test]
    fn test_display_local() {
        let path = SyncPath::Local(PathBuf::from("/home/user/docs"));
        assert_eq!(path.to_string(), "/home/user/docs");
    }

    #[test]
    fn test_display_remote_with_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: Some("nick".to_string()),
            path: PathBuf::from("/home/nick/docs"),
        };
        assert_eq!(path.to_string(), "nick@server:/home/nick/docs");
    }

    #[test]
    fn test_display_remote_without_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: None,
            path: PathBuf::from("/home/nick/docs"),
        };
        assert_eq!(path.to_string(), "server:/home/nick/docs");
    }
}
