use crate::error::SyncError;
use std::future::Future;
use std::time::Duration;

/// Configuration for retry logic with exponential backoff
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_attempts: u32,
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Backoff multiplier for exponential growth
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryConfig {
    /// Create a new RetryConfig with custom settings
    pub fn new(max_attempts: u32, initial_delay: Duration) -> Self {
        Self {
            max_attempts,
            initial_delay,
            ..Default::default()
        }
    }

    /// Set the maximum delay between retries
    #[allow(dead_code)] // Configuration builder - may be used for custom retry strategies
    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        self.max_delay = max_delay;
        self
    }

    /// Set the backoff multiplier
    #[allow(dead_code)] // Configuration builder - may be used for custom retry strategies
    pub fn with_backoff_multiplier(mut self, multiplier: f64) -> Self {
        self.backoff_multiplier = multiplier;
        self
    }

    /// Calculate delay for a given attempt number (0-indexed)
    fn calculate_delay(&self, attempt: u32) -> Duration {
        let delay_secs =
            self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi(attempt as i32);
        let delay = Duration::from_secs_f64(delay_secs);
        std::cmp::min(delay, self.max_delay)
    }
}

/// Retry an async operation with exponential backoff
///
/// This function will retry the operation up to `config.max_attempts` times
/// if it fails with a retryable error. The delay between retries increases
/// exponentially according to the backoff configuration.
///
/// # Example
///
/// ```ignore
/// let config = RetryConfig::default();
/// let result = retry_with_backoff(&config, || async {
///     // Your async operation here
///     Ok(())
/// }).await?;
/// ```
pub async fn retry_with_backoff<F, Fut, T>(
    config: &RetryConfig,
    mut operation: F,
) -> Result<T, SyncError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SyncError>>,
{
    let mut attempt = 0;

    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if attempt >= config.max_attempts => {
                return Err(e);
            }
            Err(e) if e.is_retryable() => {
                let delay = config.calculate_delay(attempt);

                eprintln!(
                    "Attempt {}/{} failed: {}. Retrying in {:?}...",
                    attempt + 1,
                    config.max_attempts,
                    e,
                    delay
                );

                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => {
                // Non-retryable error, fail immediately
                return Err(e);
            }
        }
    }
}

/// Convenience wrapper for retrying with default config
#[allow(dead_code)] // Helper function - may be used for simple retry scenarios
pub async fn retry_default<F, Fut, T>(operation: F) -> Result<T, SyncError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, SyncError>>,
{
    retry_with_backoff(&RetryConfig::default(), operation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.initial_delay, Duration::from_secs(1));
        assert_eq!(config.max_delay, Duration::from_secs(30));
        assert_eq!(config.backoff_multiplier, 2.0);
    }

    #[test]
    fn test_retry_config_custom() {
        let config = RetryConfig::new(5, Duration::from_millis(500))
            .with_max_delay(Duration::from_secs(60))
            .with_backoff_multiplier(3.0);

        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.initial_delay, Duration::from_millis(500));
        assert_eq!(config.max_delay, Duration::from_secs(60));
        assert_eq!(config.backoff_multiplier, 3.0);
    }

    #[test]
    fn test_calculate_delay_exponential() {
        let config = RetryConfig::default();

        // First retry: 1s * 2^0 = 1s
        assert_eq!(config.calculate_delay(0), Duration::from_secs(1));

        // Second retry: 1s * 2^1 = 2s
        assert_eq!(config.calculate_delay(1), Duration::from_secs(2));

        // Third retry: 1s * 2^2 = 4s
        assert_eq!(config.calculate_delay(2), Duration::from_secs(4));

        // Fourth retry: 1s * 2^3 = 8s
        assert_eq!(config.calculate_delay(3), Duration::from_secs(8));
    }

    #[test]
    fn test_calculate_delay_capped_at_max() {
        let config = RetryConfig {
            initial_delay: Duration::from_secs(10),
            max_delay: Duration::from_secs(15),
            backoff_multiplier: 2.0,
            max_attempts: 5,
        };

        // First retry: 10s * 2^0 = 10s (under max)
        assert_eq!(config.calculate_delay(0), Duration::from_secs(10));

        // Second retry: 10s * 2^1 = 20s, capped at 15s
        assert_eq!(config.calculate_delay(1), Duration::from_secs(15));

        // Third retry: 10s * 2^2 = 40s, capped at 15s
        assert_eq!(config.calculate_delay(2), Duration::from_secs(15));
    }

    #[tokio::test]
    async fn test_retry_success_first_attempt() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, SyncError>(42)
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_success_after_retries() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = RetryConfig::new(3, Duration::from_millis(10));
        let result = retry_with_backoff(&config, || {
            let c = counter_clone.clone();
            async move {
                let count = c.fetch_add(1, Ordering::SeqCst);
                if count < 2 {
                    // Fail first 2 attempts with retryable error
                    Err(SyncError::NetworkTimeout {
                        duration: Duration::from_secs(1),
                    })
                } else {
                    Ok(42)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(counter.load(Ordering::SeqCst), 3); // 2 failures + 1 success
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = RetryConfig::new(2, Duration::from_millis(10));
        let result = retry_with_backoff(&config, || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(SyncError::NetworkTimeout {
                    duration: Duration::from_secs(1),
                })
            }
        })
        .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SyncError::NetworkTimeout { .. }
        ));
        // Initial attempt + 2 retries = 3 total
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_non_retryable_error() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>(SyncError::NetworkFatal {
                    message: "Fatal error".to_string(),
                })
            }
        })
        .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SyncError::NetworkFatal { .. }
        ));
        // Should fail immediately without retries
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_default_wrapper() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = retry_default(|| {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok::<_, SyncError>(100)
            }
        })
        .await;

        assert_eq!(result.unwrap(), 100);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
