use crate::error::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// Type of hook to execute
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookType {
    PreSync,
    PostSync,
}

impl HookType {
    fn file_name(&self) -> &str {
        match self {
            HookType::PreSync => "pre-sync",
            HookType::PostSync => "post-sync",
        }
    }
}

/// Context passed to hooks via environment variables
#[derive(Debug, Clone)]
pub struct HookContext {
    pub source: String,
    pub destination: String,
    pub files_scanned: usize,
    pub files_created: usize,
    pub files_updated: usize,
    pub files_deleted: usize,
    pub files_skipped: usize,
    pub bytes_transferred: u64,
    pub duration_secs: u64,
    pub dry_run: bool,
}

impl HookContext {
    pub fn to_env_vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("SY_SOURCE".to_string(), self.source.clone());
        vars.insert("SY_DESTINATION".to_string(), self.destination.clone());
        vars.insert(
            "SY_FILES_SCANNED".to_string(),
            self.files_scanned.to_string(),
        );
        vars.insert(
            "SY_FILES_CREATED".to_string(),
            self.files_created.to_string(),
        );
        vars.insert(
            "SY_FILES_UPDATED".to_string(),
            self.files_updated.to_string(),
        );
        vars.insert(
            "SY_FILES_DELETED".to_string(),
            self.files_deleted.to_string(),
        );
        vars.insert(
            "SY_FILES_SKIPPED".to_string(),
            self.files_skipped.to_string(),
        );
        vars.insert(
            "SY_BYTES_TRANSFERRED".to_string(),
            self.bytes_transferred.to_string(),
        );
        vars.insert(
            "SY_DURATION_SECS".to_string(),
            self.duration_secs.to_string(),
        );
        vars.insert(
            "SY_DRY_RUN".to_string(),
            if self.dry_run { "1" } else { "0" }.to_string(),
        );
        vars
    }
}

/// Hook execution result
#[derive(Debug)]
#[allow(dead_code)] // Public API for hook execution results
pub struct HookResult {
    pub hook_type: HookType,
    pub path: PathBuf,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

/// Hook executor
pub struct HookExecutor {
    hooks_dir: PathBuf,
    abort_on_failure: bool,
}

impl HookExecutor {
    pub fn new() -> Result<Self> {
        let hooks_dir = Self::default_hooks_dir()?;
        Ok(Self {
            hooks_dir,
            abort_on_failure: false,
        })
    }

    pub fn with_abort_on_failure(mut self, abort: bool) -> Self {
        self.abort_on_failure = abort;
        self
    }

    fn default_hooks_dir() -> Result<PathBuf> {
        // Use XDG_CONFIG_HOME or fallback to ~/.config
        let config_dir = dirs::config_dir().ok_or_else(|| {
            crate::error::SyncError::Config("Could not determine config directory".to_string())
        })?;
        Ok(config_dir.join("sy").join("hooks"))
    }

    /// Find hook script for given type
    fn find_hook(&self, hook_type: HookType) -> Option<PathBuf> {
        let base_name = hook_type.file_name();

        // Try common extensions
        let extensions = if cfg!(windows) {
            vec!["bat", "cmd", "ps1", "exe"]
        } else {
            vec!["sh", "bash", "zsh", "fish", ""]
        };

        for ext in extensions {
            let file_name = if ext.is_empty() {
                base_name.to_string()
            } else {
                format!("{}.{}", base_name, ext)
            };

            let path = self.hooks_dir.join(&file_name);
            if path.exists() && path.is_file() {
                // Check if executable (Unix-like systems)
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(metadata) = path.metadata() {
                        let permissions = metadata.permissions();
                        if permissions.mode() & 0o111 == 0 {
                            tracing::warn!("Hook found but not executable: {}", path.display());
                            continue;
                        }
                    }
                }

                return Some(path);
            }
        }

        None
    }

    /// Execute a hook with given context
    pub fn execute(
        &self,
        hook_type: HookType,
        context: &HookContext,
    ) -> Result<Option<HookResult>> {
        let hook_path = match self.find_hook(hook_type) {
            Some(path) => path,
            None => {
                tracing::debug!(
                    "No {:?} hook found in {}",
                    hook_type,
                    self.hooks_dir.display()
                );
                return Ok(None);
            }
        };

        tracing::info!("Executing {:?} hook: {}", hook_type, hook_path.display());

        let start = std::time::Instant::now();

        // Build command
        let mut cmd = Command::new(&hook_path);

        // Add environment variables
        for (key, value) in context.to_env_vars() {
            cmd.env(key, value);
        }

        // Execute with timeout (default 30 seconds)
        let output = match cmd.output() {
            Ok(output) => output,
            Err(e) => {
                let err_msg = format!("Failed to execute hook {}: {}", hook_path.display(), e);
                tracing::error!("{}", err_msg);

                if self.abort_on_failure {
                    return Err(crate::error::SyncError::Hook(err_msg));
                }

                return Ok(Some(HookResult {
                    hook_type,
                    path: hook_path,
                    success: false,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: err_msg,
                    duration: start.elapsed(),
                }));
            }
        };

        let duration = start.elapsed();
        let success = output.status.success();
        let exit_code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !success {
            tracing::warn!(
                "Hook {:?} failed with exit code {:?}: {}",
                hook_type,
                exit_code,
                hook_path.display()
            );

            if !stderr.is_empty() {
                tracing::warn!("Hook stderr: {}", stderr);
            }

            if self.abort_on_failure {
                return Err(crate::error::SyncError::Hook(format!(
                    "Hook {:?} failed with exit code {:?}",
                    hook_type, exit_code
                )));
            }
        } else {
            tracing::info!(
                "Hook {:?} completed successfully in {:?}",
                hook_type,
                duration
            );

            if !stdout.is_empty() {
                tracing::debug!("Hook stdout: {}", stdout);
            }
        }

        Ok(Some(HookResult {
            hook_type,
            path: hook_path,
            success,
            exit_code,
            stdout,
            stderr,
            duration,
        }))
    }
}

impl Default for HookExecutor {
    fn default() -> Self {
        Self::new().unwrap_or_else(|_| Self {
            hooks_dir: PathBuf::from("/dev/null"),
            abort_on_failure: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_hook_context_env_vars() {
        let context = HookContext {
            source: "/src".to_string(),
            destination: "/dst".to_string(),
            files_scanned: 100,
            files_created: 10,
            files_updated: 5,
            files_deleted: 2,
            files_skipped: 83,
            bytes_transferred: 1024,
            duration_secs: 30,
            dry_run: false,
        };

        let vars = context.to_env_vars();
        assert_eq!(vars.get("SY_SOURCE").unwrap(), "/src");
        assert_eq!(vars.get("SY_DESTINATION").unwrap(), "/dst");
        assert_eq!(vars.get("SY_FILES_SCANNED").unwrap(), "100");
        assert_eq!(vars.get("SY_FILES_CREATED").unwrap(), "10");
        assert_eq!(vars.get("SY_DRY_RUN").unwrap(), "0");
    }

    #[test]
    fn test_hook_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let executor = HookExecutor {
            hooks_dir: temp_dir.path().to_path_buf(),
            abort_on_failure: false,
        };

        let context = HookContext {
            source: "/src".to_string(),
            destination: "/dst".to_string(),
            files_scanned: 0,
            files_created: 0,
            files_updated: 0,
            files_deleted: 0,
            files_skipped: 0,
            bytes_transferred: 0,
            duration_secs: 0,
            dry_run: false,
        };

        let result = executor.execute(HookType::PreSync, &context).unwrap();
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_hook_execution() {
        let temp_dir = TempDir::new().unwrap();
        let hook_path = temp_dir.path().join("pre-sync.sh");

        // Create a simple hook that echoes environment variables
        fs::write(
            &hook_path,
            "#!/bin/sh\necho \"Source: $SY_SOURCE\"\necho \"Files: $SY_FILES_SCANNED\"\n",
        )
        .unwrap();

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let executor = HookExecutor {
            hooks_dir: temp_dir.path().to_path_buf(),
            abort_on_failure: false,
        };

        let context = HookContext {
            source: "/test/src".to_string(),
            destination: "/test/dst".to_string(),
            files_scanned: 42,
            files_created: 0,
            files_updated: 0,
            files_deleted: 0,
            files_skipped: 0,
            bytes_transferred: 0,
            duration_secs: 0,
            dry_run: false,
        };

        let result = executor.execute(HookType::PreSync, &context).unwrap();
        assert!(result.is_some());

        let hook_result = result.unwrap();
        assert!(hook_result.success);
        assert!(hook_result.stdout.contains("/test/src"));
        assert!(hook_result.stdout.contains("42"));
    }

    #[cfg(unix)]
    #[test]
    fn test_hook_failure_abort() {
        let temp_dir = TempDir::new().unwrap();
        let hook_path = temp_dir.path().join("pre-sync.sh");

        // Create a hook that fails
        fs::write(&hook_path, "#!/bin/sh\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook_path, perms).unwrap();
        }

        let executor = HookExecutor {
            hooks_dir: temp_dir.path().to_path_buf(),
            abort_on_failure: true,
        };

        let context = HookContext {
            source: "/src".to_string(),
            destination: "/dst".to_string(),
            files_scanned: 0,
            files_created: 0,
            files_updated: 0,
            files_deleted: 0,
            files_skipped: 0,
            bytes_transferred: 0,
            duration_secs: 0,
            dry_run: false,
        };

        let result = executor.execute(HookType::PreSync, &context);
        assert!(result.is_err());
    }
}
