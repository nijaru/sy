use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Filter rule action
#[derive(Debug, Clone, PartialEq)]
pub enum FilterAction {
    /// Include the file
    Include,
    /// Exclude the file
    Exclude,
}

/// A single filter rule
#[derive(Debug, Clone)]
pub struct FilterRule {
    /// Action to take if pattern matches
    pub action: FilterAction,
    /// Compiled glob pattern
    pub pattern: glob::Pattern,
    /// Original pattern string (for debugging)
    pub pattern_str: String,
}

impl FilterRule {
    /// Create a new filter rule from a pattern string
    pub fn new(action: FilterAction, pattern: &str) -> Result<Self> {
        let pattern_str = pattern.to_string();
        let pattern = glob::Pattern::new(pattern)
            .with_context(|| format!("Invalid filter pattern: {}", pattern))?;

        Ok(Self {
            action,
            pattern,
            pattern_str,
        })
    }

    /// Check if this rule matches the given path
    pub fn matches(&self, path: &Path) -> bool {
        // Convert path to string for pattern matching
        if let Some(path_str) = path.to_str() {
            self.pattern.matches(path_str)
        } else {
            false
        }
    }
}

/// Filter engine that processes include/exclude rules
#[derive(Debug, Clone)]
pub struct FilterEngine {
    /// Ordered list of filter rules (first match wins)
    rules: Vec<FilterRule>,
}

impl FilterEngine {
    /// Create a new empty filter engine
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a filter rule from rsync-style syntax
    ///
    /// Rules can be:
    /// - "+ pattern" - Include rule
    /// - "- pattern" - Exclude rule
    /// - "pattern" - Defaults to exclude
    pub fn add_rule(&mut self, rule: &str) -> Result<()> {
        let rule = rule.trim();

        if rule.is_empty() || rule.starts_with('#') {
            // Skip empty lines and comments
            return Ok(());
        }

        let (action, pattern) = if let Some(pattern) = rule.strip_prefix("+ ") {
            (FilterAction::Include, pattern.trim())
        } else if let Some(pattern) = rule.strip_prefix("+") {
            (FilterAction::Include, pattern.trim())
        } else if let Some(pattern) = rule.strip_prefix("- ") {
            (FilterAction::Exclude, pattern.trim())
        } else if let Some(pattern) = rule.strip_prefix("-") {
            (FilterAction::Exclude, pattern.trim())
        } else {
            // Default to exclude if no prefix
            (FilterAction::Exclude, rule)
        };

        if pattern.is_empty() {
            anyhow::bail!("Empty filter pattern");
        }

        let rule = FilterRule::new(action, pattern)?;
        self.rules.push(rule);
        Ok(())
    }

    /// Add an include rule
    pub fn add_include(&mut self, pattern: &str) -> Result<()> {
        let rule = FilterRule::new(FilterAction::Include, pattern)?;
        self.rules.push(rule);
        Ok(())
    }

    /// Add an exclude rule
    pub fn add_exclude(&mut self, pattern: &str) -> Result<()> {
        let rule = FilterRule::new(FilterAction::Exclude, pattern)?;
        self.rules.push(rule);
        Ok(())
    }

    /// Load filter rules from a file
    pub fn add_rules_from_file(&mut self, file_path: &Path) -> Result<()> {
        let file = File::open(file_path)
            .with_context(|| format!("Failed to open filter file: {}", file_path.display()))?;

        let reader = BufReader::new(file);

        for (line_num, line) in reader.lines().enumerate() {
            let line = line
                .with_context(|| format!("Failed to read line {} from {}", line_num + 1, file_path.display()))?;

            self.add_rule(&line)
                .with_context(|| format!("Invalid rule at line {} in {}", line_num + 1, file_path.display()))?;
        }

        Ok(())
    }

    /// Check if a path should be included (not excluded)
    ///
    /// Returns true if the file should be synced, false if it should be excluded.
    /// First matching rule wins. If no rules match, default is to include.
    pub fn should_include(&self, path: &Path) -> bool {
        if self.rules.is_empty() {
            // No rules = include everything
            return true;
        }

        // Find first matching rule
        for rule in &self.rules {
            if rule.matches(path) {
                return rule.action == FilterAction::Include;
            }
        }

        // No rules matched - default is to include
        true
    }

    /// Check if a path should be excluded
    pub fn should_exclude(&self, path: &Path) -> bool {
        !self.should_include(path)
    }

    /// Get number of rules
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Check if filter has any rules
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Default for FilterEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_filter_includes_all() {
        let filter = FilterEngine::new();
        assert!(filter.should_include(Path::new("foo.txt")));
        assert!(filter.should_include(Path::new("bar/baz.rs")));
    }

    #[test]
    fn test_exclude_pattern() {
        let mut filter = FilterEngine::new();
        filter.add_exclude("*.log").unwrap();

        assert!(!filter.should_include(Path::new("test.log")));
        assert!(filter.should_include(Path::new("test.txt")));
    }

    #[test]
    fn test_include_pattern() {
        let mut filter = FilterEngine::new();
        // Exclude all .log files
        filter.add_exclude("*.log").unwrap();
        // But include important.log
        filter.add_include("important.log").unwrap();

        // First rule matches (exclude)
        assert!(!filter.should_include(Path::new("test.log")));
        // No rules match (default include)
        assert!(filter.should_include(Path::new("test.txt")));
        // Second rule matches (include) - but first rule already matched!
        // This shows order matters
        assert!(!filter.should_include(Path::new("important.log")));
    }

    #[test]
    fn test_rule_order_matters() {
        let mut filter = FilterEngine::new();
        // Include important.log first
        filter.add_include("important.log").unwrap();
        // Then exclude all .log files
        filter.add_exclude("*.log").unwrap();

        // First rule matches (include)
        assert!(filter.should_include(Path::new("important.log")));
        // Second rule matches (exclude)
        assert!(!filter.should_include(Path::new("test.log")));
        // No rules match (default include)
        assert!(filter.should_include(Path::new("test.txt")));
    }

    #[test]
    fn test_rsync_style_syntax() {
        let mut filter = FilterEngine::new();
        // Test rsync-style + and - prefixes
        filter.add_rule("+ *.rs").unwrap();
        filter.add_rule("- *.log").unwrap();
        filter.add_rule("*.tmp").unwrap(); // No prefix = exclude

        assert!(filter.should_include(Path::new("foo.rs")));
        assert!(!filter.should_include(Path::new("bar.log")));
        assert!(!filter.should_include(Path::new("baz.tmp")));
        assert!(filter.should_include(Path::new("qux.txt")));
    }

    #[test]
    fn test_directory_patterns() {
        let mut filter = FilterEngine::new();
        filter.add_exclude("target/*").unwrap();
        filter.add_exclude("node_modules/*").unwrap();

        assert!(!filter.should_include(Path::new("target/debug")));
        assert!(!filter.should_include(Path::new("node_modules/foo")));
        assert!(filter.should_include(Path::new("src/main.rs")));
    }

    #[test]
    fn test_comments_and_empty_lines() {
        let mut filter = FilterEngine::new();
        filter.add_rule("# This is a comment").unwrap();
        filter.add_rule("").unwrap();
        filter.add_rule("   ").unwrap();
        filter.add_rule("*.log").unwrap();

        // Should only have one rule (the *.log pattern)
        assert_eq!(filter.rule_count(), 1);
        assert!(!filter.should_include(Path::new("test.log")));
    }

    #[test]
    fn test_invalid_pattern() {
        let mut filter = FilterEngine::new();
        // Invalid glob pattern with unbalanced brackets
        let result = filter.add_exclude("[invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_default_action() {
        let mut filter = FilterEngine::new();
        // Pattern without prefix defaults to exclude
        filter.add_rule("*.log").unwrap();

        assert!(!filter.should_include(Path::new("test.log")));
        assert!(filter.should_include(Path::new("test.txt")));
    }

    #[test]
    fn test_glob_wildcards() {
        let mut filter = FilterEngine::new();
        filter.add_exclude("**/*.log").unwrap();
        filter.add_exclude("temp/**").unwrap();

        assert!(!filter.should_include(Path::new("foo/bar/test.log")));
        assert!(!filter.should_include(Path::new("temp/foo/bar")));
        assert!(filter.should_include(Path::new("src/main.rs")));
    }
}
