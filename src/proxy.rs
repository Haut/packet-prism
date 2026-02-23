use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use reqwest::StatusCode as ReqwestStatus;
use url::Url;

use crate::pool::Pool;
use crate::ratelimit::Limiter;

const MAX_BODY_SIZE: usize = 10 << 20; // 10 MB

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

const CLIENT_IDENTIFYING_HEADERS: &[&str] = &[
    "x-forwarded-for",
    "x-real-ip",
    "forwarded",
    "via",
    "x-client-ip",
    "x-originating-ip",
];

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
    fn status_code(&self) -> StatusCode {
        match self {
            ProxyError::NoIPs | ProxyError::AllIPsExhausted | ProxyError::RateLimited => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

pub struct ProxyHandler {
    pool: Arc<Pool>,
    cooldown: Duration,
    limiter: Option<(Arc<Limiter>, Duration)>,
    target_url: Url,
    user_agent: Option<String>,
}

impl ProxyHandler {
    pub fn new(
        pool: Arc<Pool>,
        cooldown: Duration,
        rate_limit: u32,
        rate_timeout: Duration,
        target_url: Url,
        user_agent: Option<String>,
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
            target_url,
            user_agent,
        }
    }

    pub async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let headers = req.headers().clone();

        tracing::info!(method = %method, uri = %uri, "incoming request");

        let body_bytes = match read_body(req.into_body()).await {
            Ok(b) => b,
            Err(_) => {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large",
                ));
            }
        };

        let out_req = match self.build_outgoing(&method, &uri, &headers, &body_bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "build request error");
                return Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"));
            }
        };

        match self.execute(out_req).await {
            Ok(resp) => {
                tracing::info!(
                    method = %method,
                    uri = %uri,
                    status = resp.status().as_u16(),
                    "upstream response"
                );
                Ok(convert_response(resp).await)
            }
            Err(e) => {
                let status = e.status_code();
                tracing::error!(method = %method, uri = %uri, error = %e, "proxy error");
                Ok(error_response(
                    status,
                    status.canonical_reason().unwrap_or("error"),
                ))
            }
        }
    }

    async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, ProxyError> {
        if let Some((ref limiter, timeout)) = self.limiter {
            limiter.wait(timeout).await?;
        }

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
                Ok(resp) if resp.status() == ReqwestStatus::TOO_MANY_REQUESTS => {
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

    fn build_outgoing(
        &self,
        method: &hyper::Method,
        uri: &hyper::Uri,
        headers: &hyper::HeaderMap,
        body: &Bytes,
    ) -> Result<reqwest::Request, ProxyError> {
        let mut target = self.target_url.clone();
        target.set_path(&single_joining_slash(self.target_url.path(), uri.path()));
        target.set_query(uri.query());

        let method =
            reqwest::Method::from_bytes(method.as_str().as_bytes()).expect("valid HTTP method");

        // Bare client just for building the request — each slot's client executes it.
        let mut builder = reqwest::Client::new()
            .request(method, target.as_str())
            .body(body.clone());

        for (name, value) in headers.iter() {
            let lower = name.as_str().to_lowercase();
            if HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if CLIENT_IDENTIFYING_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            builder = builder.header(name.as_str(), value.as_bytes());
        }

        if let Some(ref ua) = self.user_agent {
            builder = builder.header("user-agent", ua.as_str());
        }

        if let Some(host) = self.target_url.host_str() {
            let host_val = match self.target_url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            };
            builder = builder.header("host", &host_val);
        }

        builder.build().map_err(ProxyError::Upstream)
    }
}

async fn read_body(body: Incoming) -> Result<Bytes, ()> {
    let mut collected = Vec::new();
    let mut remaining = MAX_BODY_SIZE;
    let mut stream = body;

    loop {
        match stream.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if data.len() > remaining {
                        return Err(());
                    }
                    remaining -= data.len();
                    collected.extend_from_slice(&data);
                }
            }
            Some(Err(_)) => return Err(()),
            None => break,
        }
    }

    Ok(Bytes::from(collected))
}

async fn convert_response(resp: reqwest::Response) -> Response<Full<Bytes>> {
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let body = resp.bytes().await.unwrap_or_default();

    let mut response = Response::new(Full::new(body));
    *response.status_mut() =
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    for (name, value) in resp_headers.iter() {
        let lower = name.as_str().to_lowercase();
        if HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }

    response
}

fn error_response(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(format!("{msg}\n"))));
    *resp.status_mut() = status;
    resp
}

fn single_joining_slash(base: &str, path: &str) -> String {
    let a = base.ends_with('/');
    let b = path.starts_with('/');
    match (a, b) {
        (true, true) => format!("{}{}", base, &path[1..]),
        (false, false) => format!("{base}/{path}"),
        _ => format!("{base}{path}"),
    }
}
