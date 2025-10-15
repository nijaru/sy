use std::time::{Duration, Instant};

/// Simple token bucket rate limiter
pub struct RateLimiter {
    bytes_per_second: u64,
    last_refill: Instant,
    available_tokens: f64,
    max_tokens: f64,
}

impl RateLimiter {
    pub fn new(bytes_per_second: u64) -> Self {
        // Allow burst up to 1 second worth of tokens
        let max_tokens = bytes_per_second as f64;
        Self {
            bytes_per_second,
            last_refill: Instant::now(),
            available_tokens: max_tokens,
            max_tokens,
        }
    }

    /// Consume tokens for the given number of bytes
    /// Returns the duration to sleep to maintain rate limit
    pub fn consume(&mut self, bytes: u64) -> Duration {
        // Refill tokens based on elapsed time
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        let tokens_to_add =
            (elapsed.as_secs_f64() * self.bytes_per_second as f64).min(self.max_tokens);
        self.available_tokens = (self.available_tokens + tokens_to_add).min(self.max_tokens);
        self.last_refill = now;

        let bytes_f64 = bytes as f64;

        if self.available_tokens >= bytes_f64 {
            // We have enough tokens, consume them
            self.available_tokens -= bytes_f64;
            Duration::ZERO
        } else {
            // Not enough tokens, calculate sleep time
            let deficit = bytes_f64 - self.available_tokens;
            let sleep_secs = deficit / self.bytes_per_second as f64;
            self.available_tokens = 0.0;
            Duration::from_secs_f64(sleep_secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_rate_limiter_burst() {
        let mut limiter = RateLimiter::new(1024 * 1024); // 1 MB/s

        // First consume should be instant (burst allowed)
        let sleep = limiter.consume(1024 * 1024);
        assert_eq!(sleep, Duration::ZERO);

        // Second immediate consume should require sleep
        let sleep = limiter.consume(1024 * 1024);
        assert!(sleep > Duration::ZERO);
        assert!(sleep.as_secs_f64() > 0.9 && sleep.as_secs_f64() < 1.1);
    }

    #[test]
    fn test_rate_limiter_refill() {
        let mut limiter = RateLimiter::new(1024); // 1 KB/s

        // Consume all tokens
        limiter.consume(1024);

        // Wait for refill
        thread::sleep(Duration::from_millis(500));

        // Should have ~512 bytes available
        let sleep = limiter.consume(512);
        assert_eq!(sleep, Duration::ZERO);
    }

    #[test]
    fn test_rate_limiter_small_transfers() {
        let mut limiter = RateLimiter::new(1024 * 1024); // 1 MB/s

        // Small transfers should be instant
        for _ in 0..10 {
            let sleep = limiter.consume(1024);
            assert_eq!(sleep, Duration::ZERO);
        }
    }
}
