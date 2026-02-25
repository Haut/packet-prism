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

#[derive(Debug)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(target: &str, ips: Option<&str>) -> Config {
        Config {
            target: target.to_string(),
            listen: "127.0.0.1:0".to_string(),
            user_agent: None,
            ips: ips.map(|s| s.to_string()),
            cooldown: 60,
            rate_limit: 0,
            rate_timeout: 5000,
        }
    }

    #[test]
    fn test_valid_http() {
        let v = config("http://example.com", None).validate().unwrap();
        assert_eq!(v.target_url.scheme(), "http");
        assert!(v.parsed_ips.is_empty());
    }

    #[test]
    fn test_valid_https_with_path() {
        let v = config("https://api.example.com/v1", None)
            .validate()
            .unwrap();
        assert_eq!(v.target_url.scheme(), "https");
        assert_eq!(v.target_url.path(), "/v1");
    }

    #[test]
    fn test_valid_with_port() {
        let v = config("https://example.com:8443", None).validate().unwrap();
        assert_eq!(v.target_url.port(), Some(8443));
    }

    #[test]
    fn test_valid_with_ips() {
        let v = config("https://example.com", Some("10.0.0.1,10.0.0.2"))
            .validate()
            .unwrap();
        assert_eq!(v.parsed_ips.len(), 2);
        assert_eq!(v.parsed_ips[0], "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(v.parsed_ips[1], "10.0.0.2".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn test_valid_ipv6() {
        let v = config("https://example.com", Some("::1,2001:db8::1"))
            .validate()
            .unwrap();
        assert_eq!(v.parsed_ips.len(), 2);
        assert!(v.parsed_ips[0].is_loopback());
    }

    #[test]
    fn test_invalid_no_scheme() {
        let err = config("example.com", None).validate().unwrap_err();
        assert!(matches!(err, ConfigError::InvalidTarget(_)));
    }

    #[test]
    fn test_invalid_no_host() {
        let err = config("http://", None).validate().unwrap_err();
        assert!(matches!(err, ConfigError::InvalidTarget(_)));
    }

    #[test]
    fn test_invalid_garbage() {
        let err = config("not a url at all", None).validate().unwrap_err();
        assert!(matches!(err, ConfigError::InvalidTarget(_)));
    }

    #[test]
    fn test_invalid_ip() {
        let err = config("https://example.com", Some("10.0.0.1,not_an_ip"))
            .validate()
            .unwrap_err();
        match err {
            ConfigError::InvalidIp(s) => assert_eq!(s, "not_an_ip"),
            other => panic!("expected InvalidIp, got: {other}"),
        }
    }

    #[test]
    fn test_ips_whitespace_and_empty() {
        let v = config("https://example.com", Some(" 10.0.0.1 , , 10.0.0.2 , "))
            .validate()
            .unwrap();
        assert_eq!(v.parsed_ips.len(), 2);
    }

    #[test]
    fn test_defaults_preserved() {
        let c = Config {
            target: "https://example.com".to_string(),
            listen: "0.0.0.0:9090".to_string(),
            user_agent: Some("TestBot/1.0".to_string()),
            ips: None,
            cooldown: 120,
            rate_limit: 50,
            rate_timeout: 3000,
        };
        let v = c.validate().unwrap();
        assert_eq!(v.listen, "0.0.0.0:9090");
        assert_eq!(v.user_agent.as_deref(), Some("TestBot/1.0"));
        assert_eq!(v.cooldown_secs, 120);
        assert_eq!(v.rate_limit, 50);
        assert_eq!(v.rate_timeout_ms, 3000);
    }
}
