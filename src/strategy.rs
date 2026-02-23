use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;

use crate::pool::Pool;
use crate::ratelimit::Limiter;

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("no IPs available")]
    NoIPs,
    #[error("all IPs exhausted")]
    AllIPsExhausted,
    #[error("rate limit exceeded")]
    RateLimited,
    #[error("upstream error: {0}")]
    Upstream(#[from] reqwest::Error),
}

impl ProxyError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            ProxyError::NoIPs | ProxyError::AllIPsExhausted | ProxyError::RateLimited => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

pub struct ProxyStrategy {
    pool: Arc<Pool>,
    cooldown: Duration,
    limiter: Option<(Arc<Limiter>, Duration)>,
}

impl ProxyStrategy {
    pub fn new(
        pool: Arc<Pool>,
        cooldown: Duration,
        rate_limit: u32,
        rate_timeout: Duration,
    ) -> Self {
        let limiter = if rate_limit > 0 {
            Some((Arc::new(Limiter::new(rate_limit)), rate_timeout))
        } else {
            None
        };

        Self {
            pool,
            cooldown,
            limiter,
        }
    }

    pub async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, ProxyError> {
        // Apply rate limit if configured
        if let Some((ref limiter, timeout)) = self.limiter {
            limiter.wait(timeout).await?;
        }

        // Rotate through IPs
        for attempt in 0..self.pool.len() {
            let slot = self.pool.acquire().ok_or(ProxyError::NoIPs)?;

            tracing::info!(
                method = %req.method(),
                url = %req.url(),
                ip = ?slot.ip,
                attempt = attempt + 1,
                "upstream request"
            );

            let cloned = req
                .try_clone()
                .expect("request body must be cloneable for retries");

            match slot.client.execute(cloned).await {
                Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS => {
                    tracing::warn!(ip = ?slot.ip, cooldown = ?self.cooldown, "429, cooling down");
                    self.pool.cooldown(slot, self.cooldown);
                    continue;
                }
                Ok(resp) => {
                    self.pool.release(slot);
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::warn!(ip = ?slot.ip, error = %e, "error, cooling down");
                    self.pool.cooldown(slot, self.cooldown);
                    continue;
                }
            }
        }

        Err(ProxyError::AllIPsExhausted)
    }
}
