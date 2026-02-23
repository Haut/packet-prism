use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use url::Url;

use crate::strategy::{ProxyError, ProxyStrategy};

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

pub struct ProxyHandler {
    strategy: Arc<ProxyStrategy>,
    target_url: Url,
    user_agent: Option<String>,
}

impl ProxyHandler {
    pub fn new(strategy: Arc<ProxyStrategy>, target_url: Url, user_agent: Option<String>) -> Self {
        Self {
            strategy,
            target_url,
            user_agent,
        }
    }

    pub async fn handle(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        // Extract parts before consuming the body
        let method = req.method().clone();
        let uri = req.uri().clone();
        let headers = req.headers().clone();

        tracing::info!(method = %method, uri = %uri, "incoming request");

        // Buffer body with size limit
        let body_bytes = match read_body(req.into_body()).await {
            Ok(b) => b,
            Err(_) => {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large",
                ));
            }
        };

        // Build outgoing request
        let out_req = match self.build_outgoing(&method, &uri, &headers, &body_bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "build request error");
                return Ok(error_response(StatusCode::BAD_GATEWAY, "bad gateway"));
            }
        };

        // Execute via strategy
        match self.strategy.execute(out_req).await {
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
                tracing::error!(method = %method, uri = %uri, error = %e, "strategy error");
                Ok(error_response(
                    status,
                    status.canonical_reason().unwrap_or("error"),
                ))
            }
        }
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

        let method = reqwest::Method::from_bytes(method.as_str().as_bytes())
            .expect("valid HTTP method");

        // Use a bare Client just for building the request — the strategy's
        // own client (with local_address binding etc.) will execute it.
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
