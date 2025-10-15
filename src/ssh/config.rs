use crate::error::{Result, SyncError};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// SSH configuration for a specific host
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // Will be used in upcoming SSH transport implementation
pub struct SshConfig {
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub identity_file: Vec<PathBuf>,
    pub proxy_jump: Option<String>,
    pub control_master: bool,
    pub control_path: Option<PathBuf>,
    pub control_persist: Option<Duration>,
    pub compression: bool,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            port: 22,
            user: whoami::username(),
            identity_file: Vec::new(),
            proxy_jump: None,
            control_master: false,
            control_path: None,
            control_persist: None,
            compression: false,
        }
    }
}

impl SshConfig {
    /// Create a new SSH config with defaults
    pub fn new(host: &str) -> Self {
        Self {
            hostname: host.to_string(),
            port: 22,
            user: whoami::username(),
            identity_file: Vec::new(),
            proxy_jump: None,
            control_master: false,
            control_path: None,
            control_persist: None,
            compression: false,
        }
    }

    /// Expand ~ and environment variables in paths
    fn expand_path(path: &str) -> PathBuf {
        if let Some(home) = dirs::home_dir() {
            PathBuf::from(path.replace('~', &home.display().to_string()))
        } else {
            PathBuf::from(path)
        }
    }
}

/// Parse SSH config file and return configuration for a specific host
///
/// This function parses ~/.ssh/config and applies pattern matching to find
/// the most specific configuration for the given host.
#[allow(dead_code)] // Will be used in SSH transport implementation
pub fn parse_ssh_config(host: &str) -> Result<SshConfig> {
    let config_path = dirs::home_dir()
        .ok_or_else(|| SyncError::Io(std::io::Error::other("Cannot find home directory")))?
        .join(".ssh/config");

    if !config_path.exists() {
        // No config file, return defaults
        return Ok(SshConfig::new(host));
    }

    let content = fs::read_to_string(&config_path).map_err(|e| SyncError::ReadDirError {
        path: config_path,
        source: e,
    })?;

    parse_ssh_config_from_str(host, &content)
}

/// Parse SSH config from a string (for testing)
pub fn parse_ssh_config_from_str(host: &str, content: &str) -> Result<SshConfig> {
    let mut config = SshConfig::new(host);
    let mut in_matching_host = false;

    for line in content.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on whitespace
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let keyword = parts[0].to_lowercase();

        match keyword.as_str() {
            "host" => {
                // Start of new host block
                let host_patterns: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
                in_matching_host = host_patterns
                    .iter()
                    .any(|pattern| host_matches(host, pattern));
            }
            _ if !in_matching_host => {
                // Skip directives not in matching host block
                continue;
            }
            "hostname" => {
                if let Some(value) = parts.get(1) {
                    config.hostname = value.to_string();
                }
            }
            "port" => {
                if let Some(value) = parts.get(1) {
                    if let Ok(port) = value.parse::<u16>() {
                        config.port = port;
                    }
                }
            }
            "user" => {
                if let Some(value) = parts.get(1) {
                    config.user = value.to_string();
                }
            }
            "identityfile" => {
                if let Some(value) = parts.get(1) {
                    config.identity_file.push(SshConfig::expand_path(value));
                }
            }
            "proxyjump" => {
                if let Some(value) = parts.get(1) {
                    config.proxy_jump = Some(value.to_string());
                }
            }
            "controlmaster" => {
                if let Some(value) = parts.get(1) {
                    config.control_master = matches!(value.to_lowercase().as_str(), "yes" | "auto");
                }
            }
            "controlpath" => {
                if let Some(value) = parts.get(1) {
                    config.control_path = Some(SshConfig::expand_path(value));
                }
            }
            "controlpersist" => {
                if let Some(value) = parts.get(1) {
                    config.control_persist = parse_duration(value);
                }
            }
            "compression" => {
                if let Some(value) = parts.get(1) {
                    config.compression = value.to_lowercase() == "yes";
                }
            }
            _ => {
                // Ignore unknown directives
            }
        }
    }

    Ok(config)
}

/// Check if a hostname matches an SSH config pattern
///
/// Supports wildcards (* and ?) and negation (!)
fn host_matches(host: &str, pattern: &str) -> bool {
    // Handle negation
    if let Some(negated_pattern) = pattern.strip_prefix('!') {
        return !host_matches(host, negated_pattern);
    }

    // Convert SSH pattern to regex-like matching
    let pattern = pattern.replace('.', r"\.");
    let pattern = pattern.replace('*', ".*");
    let pattern = pattern.replace('?', ".");

    regex::Regex::new(&format!("^{}$", pattern))
        .map(|re| re.is_match(host))
        .unwrap_or(false)
}

/// Parse duration strings like "10m", "1h", "30s"
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();

    if value == "yes" {
        // "yes" means persist indefinitely, use 1 year as proxy
        return Some(Duration::from_secs(365 * 24 * 60 * 60));
    }

    if value == "no" {
        return None;
    }

    // Parse number + unit
    let (num_str, unit) = if let Some(num) = value.strip_suffix('s') {
        (num, "s")
    } else if let Some(num) = value.strip_suffix('m') {
        (num, "m")
    } else if let Some(num) = value.strip_suffix('h') {
        (num, "h")
    } else if let Some(num) = value.strip_suffix('d') {
        (num, "d")
    } else {
        (value, "s") // default to seconds
    };

    let num: u64 = num_str.parse().ok()?;

    let seconds = match unit {
        "s" => num,
        "m" => num * 60,
        "h" => num * 60 * 60,
        "d" => num * 24 * 60 * 60,
        _ => return None,
    };

    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_config_defaults() {
        let config = SshConfig::new("example.com");
        assert_eq!(config.hostname, "example.com");
        assert_eq!(config.port, 22);
        assert_eq!(config.user, whoami::username());
        assert!(config.identity_file.is_empty());
        assert!(!config.control_master);
    }

    #[test]
    fn test_parse_simple_config() {
        let content = r#"
Host example
    HostName example.com
    Port 2222
    User admin
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        assert_eq!(config.hostname, "example.com");
        assert_eq!(config.port, 2222);
        assert_eq!(config.user, "admin");
    }

    #[test]
    fn test_parse_wildcard_host() {
        let content = r#"
Host *.example.com
    User admin
    Port 2222

Host specific.example.com
    Port 3333
"#;

        let config = parse_ssh_config_from_str("test.example.com", content).unwrap();
        assert_eq!(config.user, "admin");
        assert_eq!(config.port, 2222);

        let config2 = parse_ssh_config_from_str("specific.example.com", content).unwrap();
        assert_eq!(config2.port, 3333);
    }

    #[test]
    fn test_parse_identity_file() {
        let content = r#"
Host example
    IdentityFile ~/.ssh/id_rsa
    IdentityFile ~/.ssh/id_ed25519
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        assert_eq!(config.identity_file.len(), 2);
    }

    #[test]
    fn test_parse_proxy_jump() {
        let content = r#"
Host internal
    ProxyJump bastion.example.com
"#;

        let config = parse_ssh_config_from_str("internal", content).unwrap();
        assert_eq!(config.proxy_jump, Some("bastion.example.com".to_string()));
    }

    #[test]
    fn test_parse_control_master() {
        let content = r#"
Host example
    ControlMaster auto
    ControlPath ~/.ssh/control-%r@%h:%p
    ControlPersist 10m
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        assert!(config.control_master);
        assert!(config.control_path.is_some());
        assert_eq!(config.control_persist, Some(Duration::from_secs(600)));
    }

    #[test]
    fn test_parse_compression() {
        let content = r#"
Host example
    Compression yes
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        assert!(config.compression);
    }

    #[test]
    fn test_host_matching() {
        assert!(host_matches("example.com", "example.com"));
        assert!(host_matches("test.example.com", "*.example.com"));
        assert!(host_matches("example.com", "*.com"));
        assert!(!host_matches("example.org", "*.com"));
        assert!(host_matches("test", "?est"));
        assert!(!host_matches("example.com", "!example.com"));
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_duration("yes"),
            Some(Duration::from_secs(365 * 24 * 60 * 60))
        );
        assert_eq!(parse_duration("no"), None);
    }

    #[test]
    fn test_non_matching_host() {
        let content = r#"
Host other
    Port 2222
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        // Should use defaults since host doesn't match
        assert_eq!(config.port, 22);
    }

    #[test]
    fn test_comments_and_empty_lines() {
        let content = r#"
# This is a comment
Host example

    # Another comment
    Port 2222

    User admin
"#;

        let config = parse_ssh_config_from_str("example", content).unwrap();
        assert_eq!(config.port, 2222);
        assert_eq!(config.user, "admin");
    }
}
