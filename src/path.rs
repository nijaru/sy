use std::path::{Path, PathBuf};

/// Represents a sync path that can be either local, remote (SSH), or S3
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPath {
    Local {
        path: PathBuf,
        has_trailing_slash: bool,
    },
    Remote {
        host: String,
        user: Option<String>,
        path: PathBuf,
        has_trailing_slash: bool,
    },
    S3 {
        bucket: String,
        key: String,
        region: Option<String>,
        endpoint: Option<String>,
        has_trailing_slash: bool,
    },
}

impl SyncPath {
    /// Parse a path string into a SyncPath
    ///
    /// Supported formats:
    /// - Local: `/path/to/dir`, `./relative/path`, `relative/path`
    /// - Remote: `user@host:/path`, `host:/path`
    /// - S3: `s3://bucket/key/path`, `s3://bucket/key?region=us-west-2`, `s3://bucket/key?endpoint=https://...`
    ///
    /// Trailing slash semantics (rsync-compatible):
    /// - `/path/to/dir` (no slash): Copy directory itself to destination
    /// - `/path/to/dir/` (with slash): Copy directory contents to destination
    pub fn parse(s: &str) -> Self {
        // Detect trailing slash (before parsing)
        // For S3 paths with query parameters, check the path portion before '?'
        let has_trailing_slash = if s.starts_with("s3://") {
            if let Some(q_pos) = s.find('?') {
                s[..q_pos].ends_with('/')
            } else {
                s.ends_with('/')
            }
        } else {
            s.ends_with('/') || s.ends_with('\\')
        };
        // Check for S3 URL format
        if let Some(remainder) = s.strip_prefix("s3://") {
            // Split on ? to separate path from query params
            let (path_part, query_part) = if let Some(q_pos) = remainder.find('?') {
                (&remainder[..q_pos], Some(&remainder[q_pos + 1..]))
            } else {
                (remainder, None)
            };

            // Split path into bucket and key
            if let Some(slash_pos) = path_part.find('/') {
                let bucket = path_part[..slash_pos].to_string();
                let key = path_part[slash_pos + 1..].to_string();

                // Parse query parameters (region, endpoint)
                let mut region = None;
                let mut endpoint = None;

                if let Some(query) = query_part {
                    for param in query.split('&') {
                        if let Some((k, v)) = param.split_once('=') {
                            match k {
                                "region" => region = Some(v.to_string()),
                                "endpoint" => endpoint = Some(v.to_string()),
                                _ => {} // Ignore unknown params
                            }
                        }
                    }
                }

                return SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
                    has_trailing_slash,
                };
            } else {
                // Just bucket, no key (treat as root)
                return SyncPath::S3 {
                    bucket: path_part.to_string(),
                    key: String::new(),
                    region: None,
                    endpoint: None,
                    has_trailing_slash,
                };
            }
        }

        // Check for remote path format (contains : before any /)
        if let Some(colon_pos) = s.find(':') {
            // Check if this is a remote path (no / before the :)
            let before_colon = &s[..colon_pos];

            // Check if this is a Windows drive letter (single letter followed by :)
            if before_colon.len() == 1 && before_colon.chars().next().unwrap().is_ascii_alphabetic()
            {
                // Windows drive letter, treat as local
                return SyncPath::Local {
                    path: PathBuf::from(s),
                    has_trailing_slash,
                };
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
                        has_trailing_slash,
                    };
                } else {
                    return SyncPath::Remote {
                        host: before_colon.to_string(),
                        user: None,
                        path: PathBuf::from(path_part),
                        has_trailing_slash,
                    };
                }
            }
        }

        // Otherwise it's a local path
        SyncPath::Local {
            path: PathBuf::from(s),
            has_trailing_slash,
        }
    }

    /// Get the path component
    pub fn path(&self) -> &Path {
        match self {
            SyncPath::Local { path, .. } => path,
            SyncPath::Remote { path, .. } => path,
            SyncPath::S3 { key, .. } => Path::new(key),
        }
    }

    /// Check if the original path string had a trailing slash
    ///
    /// This is used for rsync-compatible directory behavior:
    /// - No trailing slash: copy the directory itself
    /// - Trailing slash: copy only the directory contents
    pub fn has_trailing_slash(&self) -> bool {
        match self {
            SyncPath::Local { has_trailing_slash, .. } => *has_trailing_slash,
            SyncPath::Remote { has_trailing_slash, .. } => *has_trailing_slash,
            SyncPath::S3 { has_trailing_slash, .. } => *has_trailing_slash,
        }
    }

    /// Check if this is a remote SSH path
    #[allow(dead_code)] // Used in tests
    pub fn is_remote(&self) -> bool {
        matches!(self, SyncPath::Remote { .. })
    }

    /// Check if this is a local path
    pub fn is_local(&self) -> bool {
        matches!(self, SyncPath::Local { .. })
    }

    /// Check if this is an S3 path
    #[allow(dead_code)] // Public API for S3 path detection
    pub fn is_s3(&self) -> bool {
        matches!(self, SyncPath::S3 { .. })
    }
}

impl std::fmt::Display for SyncPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncPath::Local { path, .. } => write!(f, "{}", path.display()),
            SyncPath::Remote { host, user, path, .. } => {
                if let Some(u) = user {
                    write!(f, "{}@{}:{}", u, host, path.display())
                } else {
                    write!(f, "{}:{}", host, path.display())
                }
            }
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                write!(f, "s3://{}/{}", bucket, key)?;
                let mut query_params = Vec::new();
                if let Some(r) = region {
                    query_params.push(format!("region={}", r));
                }
                if let Some(e) = endpoint {
                    query_params.push(format!("endpoint={}", e));
                }
                if !query_params.is_empty() {
                    write!(f, "?{}", query_params.join("&"))?;
                }
                Ok(())
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
    fn test_parse_windows_drive_letter_backslash() {
        // C:\path with backslashes
        let path = SyncPath::parse("C:\\Users\\nick");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("C:\\Users\\nick"));
    }

    #[test]
    fn test_parse_windows_lowercase_drive() {
        // Lowercase drive letter
        let path = SyncPath::parse("d:/projects");
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("d:/projects"));
    }

    #[test]
    fn test_parse_windows_unc_path() {
        // UNC path \\server\share\file
        let path = SyncPath::parse("\\\\server\\share\\file.txt");
        assert!(path.is_local());
        // UNC paths should be treated as local Windows paths
    }

    #[test]
    fn test_windows_reserved_names() {
        // Windows reserved names should still parse as local
        let path = SyncPath::parse("C:/Users/nick/CON");
        assert!(path.is_local());

        let path = SyncPath::parse("D:/temp/NUL.txt");
        assert!(path.is_local());

        let path = SyncPath::parse("C:/PRN");
        assert!(path.is_local());
    }

    #[test]
    fn test_display_local() {
        let path = SyncPath::Local { path: PathBuf::from("/home/user/docs"), has_trailing_slash: false };
        assert_eq!(path.to_string(), "/home/user/docs");
    }

    #[test]
    fn test_display_remote_with_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: Some("nick".to_string()),
            path: PathBuf::from("/home/nick/docs"),
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "nick@server:/home/nick/docs");
    }

    #[test]
    fn test_display_remote_without_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: None,
            path: PathBuf::from("/home/nick/docs"),
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "server:/home/nick/docs");
    }

    #[test]
    fn test_parse_s3_basic() {
        let path = SyncPath::parse("s3://my-bucket/path/to/file.txt");
        assert!(path.is_s3());
        assert_eq!(path.path(), Path::new("path/to/file.txt"));
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "path/to/file.txt");
                assert_eq!(region, None);
                assert_eq!(endpoint, None);
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_with_region() {
        let path = SyncPath::parse("s3://my-bucket/file.txt?region=us-west-2");
        assert!(path.is_s3());
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "file.txt");
                assert_eq!(region, Some("us-west-2".to_string()));
                assert_eq!(endpoint, None);
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_with_endpoint() {
        let path = SyncPath::parse("s3://my-bucket/file.txt?endpoint=https://s3.example.com");
        assert!(path.is_s3());
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "file.txt");
                assert_eq!(region, None);
                assert_eq!(endpoint, Some("https://s3.example.com".to_string()));
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_bucket_only() {
        let path = SyncPath::parse("s3://my-bucket");
        assert!(path.is_s3());
        match path {
            SyncPath::S3 { bucket, key, .. } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "");
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_display_s3() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "path/to/file.txt".to_string(),
            region: None,
            endpoint: None,
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "s3://my-bucket/path/to/file.txt");
    }

    #[test]
    fn test_display_s3_with_region() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "file.txt".to_string(),
            region: Some("us-west-2".to_string()),
            endpoint: None,
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "s3://my-bucket/file.txt?region=us-west-2");
    }

    #[test]
    fn test_display_s3_with_endpoint() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "file.txt".to_string(),
            region: None,
            endpoint: Some("https://s3.example.com".to_string()),
            has_trailing_slash: false,
        };
        assert_eq!(
            path.to_string(),
            "s3://my-bucket/file.txt?endpoint=https://s3.example.com"
        );
    }
}
