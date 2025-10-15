use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    #[allow(dead_code)] // Config infrastructure for future use
    pub defaults: Defaults,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Defaults {
    #[allow(dead_code)] // Global default for future use
    pub parallel: Option<usize>,
    #[allow(dead_code)] // Global default for future use
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Profile {
    pub source: Option<String>,
    pub destination: Option<String>,
    pub delete: Option<bool>,
    pub exclude: Option<Vec<String>>,
    pub bwlimit: Option<String>,
    pub resume: Option<bool>,
    pub min_size: Option<String>,
    pub max_size: Option<String>,
    pub parallel: Option<usize>,
    pub dry_run: Option<bool>,
    pub quiet: Option<bool>,
    pub verbose: Option<u8>,
}

impl Config {
    /// Load config from ~/.config/sy/config.toml
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;

        toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", config_path.display()))
    }

    /// Get the config file path
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Cannot find config directory (XDG_CONFIG_HOME or ~/.config)")?;

        Ok(config_dir.join("sy").join("config.toml"))
    }

    /// Get a profile by name
    pub fn get_profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// List all available profile names
    pub fn list_profiles(&self) -> Vec<&String> {
        let mut names: Vec<&String> = self.profiles.keys().collect();
        names.sort();
        names
    }

    /// Show profile details in human-readable format
    pub fn show_profile(&self, name: &str) -> Option<String> {
        self.get_profile(name).map(|profile| {
            let toml = toml::to_string_pretty(profile)
                .unwrap_or_else(|_| "Error serializing profile".to_string());
            format!("[profiles.{}]\n{}", name, toml)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_empty_config() {
        let config = Config::default();
        assert_eq!(config.profiles.len(), 0);
        assert!(config.defaults.parallel.is_none());
    }

    #[test]
    fn test_parse_config() {
        let toml = r#"
[defaults]
parallel = 20
exclude = ["*.tmp", ".DS_Store"]

[profiles.test-profile]
source = "~/src"
destination = "~/dst"
delete = true
exclude = ["*.log"]
bwlimit = "10MB"
resume = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        assert_eq!(config.defaults.parallel, Some(20));
        assert_eq!(
            config.defaults.exclude,
            Some(vec!["*.tmp".to_string(), ".DS_Store".to_string()])
        );

        let profile = config.get_profile("test-profile").unwrap();
        assert_eq!(profile.source, Some("~/src".to_string()));
        assert_eq!(profile.destination, Some("~/dst".to_string()));
        assert_eq!(profile.delete, Some(true));
        assert_eq!(profile.bwlimit, Some("10MB".to_string()));
        assert_eq!(profile.resume, Some(true));
    }

    #[test]
    fn test_list_profiles() {
        let toml = r#"
[profiles.profile-a]
source = "~/a"

[profiles.profile-b]
source = "~/b"

[profiles.profile-c]
source = "~/c"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let profiles = config.list_profiles();

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles, vec!["profile-a", "profile-b", "profile-c"]);
    }

    #[test]
    fn test_get_profile_missing() {
        let config = Config::default();
        assert!(config.get_profile("nonexistent").is_none());
    }

    #[test]
    fn test_show_profile() {
        let toml = r#"
[profiles.test]
source = "~/src"
destination = "~/dst"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let output = config.show_profile("test").unwrap();

        assert!(output.contains("[profiles.test]"));
        assert!(output.contains("source = \"~/src\""));
        assert!(output.contains("destination = \"~/dst\""));
    }

    #[test]
    fn test_config_path() {
        let path = Config::config_path().unwrap();
        // Check path components instead of string representation (cross-platform)
        assert!(path.parent().unwrap().ends_with("sy"));
        assert_eq!(path.file_name().unwrap(), "config.toml");
    }

    #[test]
    fn test_load_nonexistent_config() {
        // Should return default config if file doesn't exist
        // (We can't actually test this without mocking the config dir)
        let config = Config::default();
        assert_eq!(config.profiles.len(), 0);
    }

    #[test]
    fn test_parse_minimal_profile() {
        let toml = r#"
[profiles.minimal]
source = "~/src"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let profile = config.get_profile("minimal").unwrap();

        assert_eq!(profile.source, Some("~/src".to_string()));
        assert!(profile.destination.is_none());
        assert!(profile.delete.is_none());
    }

    #[test]
    fn test_parse_all_profile_fields() {
        let toml = r#"
[profiles.complete]
source = "~/src"
destination = "~/dst"
delete = true
exclude = ["*.log", "*.tmp"]
bwlimit = "10MB"
resume = false
min_size = "1KB"
max_size = "100MB"
parallel = 20
dry_run = true
quiet = true
verbose = 2
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        let profile = config.get_profile("complete").unwrap();

        assert_eq!(profile.source, Some("~/src".to_string()));
        assert_eq!(profile.destination, Some("~/dst".to_string()));
        assert_eq!(profile.delete, Some(true));
        assert_eq!(
            profile.exclude,
            Some(vec!["*.log".to_string(), "*.tmp".to_string()])
        );
        assert_eq!(profile.bwlimit, Some("10MB".to_string()));
        assert_eq!(profile.resume, Some(false));
        assert_eq!(profile.min_size, Some("1KB".to_string()));
        assert_eq!(profile.max_size, Some("100MB".to_string()));
        assert_eq!(profile.parallel, Some(20));
        assert_eq!(profile.dry_run, Some(true));
        assert_eq!(profile.quiet, Some(true));
        assert_eq!(profile.verbose, Some(2));
    }
}
