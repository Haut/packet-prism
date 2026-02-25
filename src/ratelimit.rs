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
    /// Refill rate in microtokens per nanosecond (pre-calculated).
    rate_micro_per_ns: u64,
    /// Maximum tokens in microtokens.
    max_micro: u64,
    epoch: Instant,
}

impl Limiter {
    pub fn new(rate_per_second: u32) -> Self {
        let rate = u64::from(rate_per_second);
        let max_micro = rate * MICRO;
        // rate_per_second tokens/sec = rate * MICRO microtokens / 1_000_000_000 ns
        // To avoid losing precision, we store the rate and compute inline.
        Limiter {
            tokens: AtomicU64::new(max_micro),
            last_refill_ns: AtomicU64::new(0),
            rate_micro_per_ns: rate, // actual rate = rate * MICRO / 1e9; computed inline
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
                // microtokens to add = elapsed_ns * rate_per_sec * MICRO / 1_000_000_000
                let add = elapsed_ns.saturating_mul(self.rate_micro_per_ns) / 1_000; // (MICRO / 1e9 = 1/1000)

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

            // Sleep for estimated refill time of one token.
            // 1 token = MICRO microtokens; rate = rate_micro_per_ns * MICRO / 1e9 microtokens/ns
            // time_for_one_token_ns = 1e9 / rate_per_sec = 1_000_000_000 / rate_micro_per_ns
            let sleep_ns = 1_000_000_000u64
                .checked_div(self.rate_micro_per_ns)
                .unwrap_or(1_000_000);

            let remaining = deadline_ns.saturating_sub(now_ns);
            let sleep_ns = sleep_ns.min(remaining);

            tokio::time::sleep(Duration::from_nanos(sleep_ns)).await;
        }
    }
}
