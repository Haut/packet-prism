use std::net::IpAddr;

use clap::Parser;
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid --target: {0}")]
    InvalidTarget(String),
    #[error("invalid IP: {0}")]
    InvalidIp(String),
}

#[derive(Debug, Parser)]
#[command(
    name = "packet-prism",
    about = "Outbound proxy with IP rotation and optional rate limiting",
    version
)]
pub struct Config {
    /// Upstream URL (scheme + host)
    #[arg(long, env = "TARGET_URL")]
    pub target: String,

    /// Bind address
    #[arg(long, env = "LISTEN_ADDR", default_value = "0.0.0.0:8080")]
    pub listen: String,

    /// Custom User-Agent header
    #[arg(long, env = "USER_AGENT")]
    pub user_agent: Option<String>,

    /// Comma-separated source IPs (omit to use default)
    #[arg(long, env = "IPS")]
    pub ips: Option<String>,

    /// Per-IP cooldown in seconds after error or 429
    #[arg(long, env = "COOLDOWN_SECONDS", default_value_t = 60)]
    pub cooldown: u64,

    /// Max requests per second (0 = unlimited)
    #[arg(long, env = "RATE_LIMIT", default_value_t = 0)]
    pub rate_limit: u32,

    /// Max wait time in ms when rate limited
    #[arg(long, env = "RATE_TIMEOUT_MS", default_value_t = 5000)]
    pub rate_timeout: u64,
}

pub struct ValidatedConfig {
    pub listen: String,
    pub target_url: Url,
    pub user_agent: Option<String>,
    pub parsed_ips: Vec<IpAddr>,
    pub cooldown_secs: u64,
    pub rate_limit: u32,
    pub rate_timeout_ms: u64,
}

impl Config {
    pub fn validate(self) -> Result<ValidatedConfig, ConfigError> {
        let url =
            Url::parse(&self.target).map_err(|e| ConfigError::InvalidTarget(e.to_string()))?;
        if url.scheme().is_empty() || url.host().is_none() {
            return Err(ConfigError::InvalidTarget(
                "must include scheme and host".into(),
            ));
        }

        let mut parsed_ips = Vec::new();
        if let Some(ref ips_str) = self.ips {
            for raw in ips_str.split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                let ip: IpAddr = raw
                    .parse()
                    .map_err(|_| ConfigError::InvalidIp(raw.to_string()))?;
                parsed_ips.push(ip);
            }
        }

        Ok(ValidatedConfig {
            listen: self.listen,
            target_url: url,
            user_agent: self.user_agent,
            parsed_ips,
            cooldown_secs: self.cooldown,
            rate_limit: self.rate_limit,
            rate_timeout_ms: self.rate_timeout,
        })
    }
}
