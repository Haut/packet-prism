use std::time::{Duration, Instant};

use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
#[error("rate limit timeout")]
pub struct RateLimitTimeout;

struct Inner {
    rate: f64,
    max: f64,
    tokens: f64,
    last_time: Instant,
}

pub struct Limiter {
    inner: Mutex<Inner>,
}

impl Limiter {
    pub fn new(rate_per_second: u32) -> Self {
        let r = f64::from(rate_per_second);
        Limiter {
            inner: Mutex::new(Inner {
                rate: r,
                max: r,
                tokens: r,
                last_time: Instant::now(),
            }),
        }
    }

    pub async fn wait(&self, timeout: Duration) -> Result<(), RateLimitTimeout> {
        let deadline = Instant::now() + timeout;

        loop {
            {
                let mut inner = self.inner.lock().await;
                inner.refill();
                if inner.tokens >= 1.0 {
                    inner.tokens -= 1.0;
                    return Ok(());
                }
                let wait_secs = (1.0 - inner.tokens) / inner.rate;
                let wait = Duration::from_secs_f64(wait_secs);

                if Instant::now() + wait > deadline {
                    return Err(RateLimitTimeout);
                }
                // drop lock before sleeping
                drop(inner);
                tokio::time::sleep(wait).await;
            }
        }
    }
}

impl Inner {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_time).as_secs_f64();
        self.tokens += elapsed * self.rate;
        if self.tokens > self.max {
            self.tokens = self.max;
        }
        self.last_time = now;
    }
}
