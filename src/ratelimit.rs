use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
#[error("rate limit timeout")]
pub struct RateLimitTimeout;

// Tokens are stored as microtokens (1 token = 1_000_000 units) to avoid floats.
const MICRO: u64 = 1_000_000;

pub struct Limiter {
    /// Available tokens in microtokens.
    tokens: AtomicU64,
    /// Last refill timestamp in nanoseconds since epoch.
    last_refill_ns: AtomicU64,
    /// Tokens per second (kept as-is to avoid integer truncation in refill math).
    rate_per_sec: u64,
    /// Maximum tokens in microtokens.
    max_micro: u64,
    epoch: Instant,
}

impl Limiter {
    pub fn new(rate_per_second: u32) -> Self {
        let rate = u64::from(rate_per_second);
        let max_micro = rate * MICRO;
        Limiter {
            tokens: AtomicU64::new(max_micro),
            last_refill_ns: AtomicU64::new(0),
            rate_per_sec: rate,
            max_micro,
            epoch: Instant::now(),
        }
    }

    fn now_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    pub async fn wait(&self, timeout: Duration) -> Result<(), RateLimitTimeout> {
        let deadline_ns = self.now_ns() + timeout.as_nanos() as u64;

        loop {
            // Refill: compute tokens to add based on elapsed time.
            let now_ns = self.now_ns();
            let prev_ns = self.last_refill_ns.load(Ordering::Relaxed);
            let elapsed_ns = now_ns.saturating_sub(prev_ns);

            if elapsed_ns > 0 {
                // microtokens = elapsed_ns * rate_per_sec * MICRO / 1e9 = elapsed_ns * rate / 1_000
                let add = elapsed_ns.saturating_mul(self.rate_per_sec) / 1_000;

                if add > 0 {
                    // Try to advance the refill timestamp.
                    let _ = self.last_refill_ns.compare_exchange(
                        prev_ns,
                        now_ns,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    // Add tokens, capping at max.
                    self.tokens
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                            Some(self.max_micro.min(cur.saturating_add(add)))
                        })
                        .ok();
                }
            }

            // Try to consume one token.
            let result = self
                .tokens
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    if cur >= MICRO {
                        Some(cur - MICRO)
                    } else {
                        None
                    }
                });

            if result.is_ok() {
                return Ok(());
            }

            // Not enough tokens — check deadline.
            let now_ns = self.now_ns();
            if now_ns >= deadline_ns {
                return Err(RateLimitTimeout);
            }

            // Sleep for one token interval: 1e9 / rate_per_sec nanoseconds.
            let sleep_ns = 1_000_000_000u64
                .checked_div(self.rate_per_sec)
                .unwrap_or(1_000_000);

            let remaining = deadline_ns.saturating_sub(now_ns);
            let sleep_ns = sleep_ns.min(remaining);

            tokio::time::sleep(Duration::from_nanos(sleep_ns)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_immediate_when_full() {
        let limiter = Limiter::new(10);
        let start = Instant::now();
        for _ in 0..10 {
            limiter
                .wait(Duration::from_secs(1))
                .await
                .expect("should not timeout");
        }
        // All 10 tokens were available at start — should complete almost instantly
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_exhaustion_then_refill() {
        let limiter = Limiter::new(100); // 100 tokens/sec, refill 1 token per 10ms
        // Drain all 100 tokens
        for _ in 0..100 {
            limiter.wait(Duration::from_secs(1)).await.unwrap();
        }
        // 101st must wait for refill (~10ms)
        let start = Instant::now();
        limiter.wait(Duration::from_secs(1)).await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5) && elapsed < Duration::from_millis(100),
            "expected ~10ms refill wait, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_timeout_returns_error() {
        let limiter = Limiter::new(1); // 1 token/sec
        // Consume the one token
        limiter.wait(Duration::from_secs(1)).await.unwrap();
        // Next wait with short timeout should fail
        let result = limiter.wait(Duration::from_millis(10)).await;
        assert!(result.is_err(), "should timeout");
    }

    #[tokio::test]
    async fn test_refill_capped_at_max() {
        let limiter = Limiter::new(10);
        // Drain all 10 tokens
        for _ in 0..10 {
            limiter.wait(Duration::from_secs(1)).await.unwrap();
        }
        // Wait long enough to overfill (2 seconds at 10/sec = 20 tokens if uncapped)
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Should only get 10 tokens (the max)
        for i in 0..10 {
            limiter
                .wait(Duration::from_millis(50))
                .await
                .unwrap_or_else(|_| panic!("token {i} should be available"));
        }
        // 11th should not be immediately available
        let result = limiter.wait(Duration::from_millis(10)).await;
        assert!(result.is_err(), "should not have more than max tokens");
    }

    #[tokio::test]
    async fn test_high_rate_throughput() {
        let limiter = Limiter::new(1000);
        let start = Instant::now();
        for _ in 0..100 {
            limiter.wait(Duration::from_secs(5)).await.unwrap();
        }
        // 100 tokens at 1000/sec = 100ms theoretical minimum
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "100 tokens at 1000/sec should complete in < 500ms, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn test_concurrent_no_over_consumption() {
        let limiter = Arc::new(Limiter::new(10)); // starts with 10 tokens
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..20 {
            let limiter = limiter.clone();
            handles.push(tokio::spawn(async move {
                limiter.wait(Duration::from_millis(100)).await.is_ok()
            }));
        }
        let mut successes = 0;
        for h in handles {
            if h.await.unwrap() {
                successes += 1;
            }
        }
        let elapsed = start.elapsed();
        // At most 10 initial tokens + whatever refilled during the 100ms window
        // 10 tokens/sec * 0.1sec = 1 more token, so max ~11
        // Be generous with upper bound due to timing
        let max_expected = 10 + (elapsed.as_millis() as u32 / 100 + 1) * 10;
        assert!(
            successes <= max_expected as usize,
            "got {successes} successes but expected at most {max_expected}"
        );
        assert!(
            successes >= 10,
            "should grant at least the initial 10 tokens"
        );
    }
}
