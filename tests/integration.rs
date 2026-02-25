use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use packet_prism::pool::Pool;
use packet_prism::proxy::ProxyHandler;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestProxy {
    addr: SocketAddr,
    _shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl TestProxy {
    async fn start(
        target_url: Url,
        cooldown: Duration,
        rate_limit: u32,
        rate_timeout: Duration,
        user_agent: Option<String>,
    ) -> Self {
        let pool = Arc::new(Pool::new(&[]));
        let handler = Arc::new(ProxyHandler::new(
            pool,
            cooldown,
            rate_limit,
            rate_timeout,
            target_url,
            user_agent,
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((stream, _)) = result else { continue };
                        let handler = handler.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let svc = service_fn(move |req| {
                                let handler = handler.clone();
                                async move { handler.handle(req).await }
                            });
                            http1::Builder::new()
                                .keep_alive(true)
                                .serve_connection(io, svc)
                                .await
                                .ok();
                        });
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        TestProxy {
            addr,
            _shutdown_tx: shutdown_tx,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.addr.port(), path)
    }
}

async fn simple_proxy(mock: &MockServer) -> TestProxy {
    TestProxy::start(
        Url::parse(&mock.uri()).unwrap(),
        Duration::from_secs(60),
        0,
        Duration::from_secs(5),
        None,
    )
    .await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

// ---------------------------------------------------------------------------
// Proxy pass-through
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_passthrough() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/data"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client().get(proxy.url("/api/data")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn test_post_with_body() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .respond_with(ResponseTemplate::new(201).set_body_string("created"))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client()
        .post(proxy.url("/items"))
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.text().await.unwrap(), "created");
}

#[tokio::test]
async fn test_status_code_passthrough() {
    let mock = MockServer::start().await;
    for status in [404, 201, 500] {
        Mock::given(method("GET"))
            .and(path(format!("/status/{status}")))
            .respond_with(ResponseTemplate::new(status))
            .mount(&mock)
            .await;
    }

    let proxy = simple_proxy(&mock).await;
    for status in [404, 201, 500] {
        let resp = client()
            .get(proxy.url(&format!("/status/{status}")))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), status, "expected {status}");
    }
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_simple_path() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/foo"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client().get(proxy.url("/foo")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_base_path_concat() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = TestProxy::start(
        Url::parse(&format!("{}/api/v1", mock.uri())).unwrap(),
        Duration::from_secs(60),
        0,
        Duration::from_secs(5),
        None,
    )
    .await;

    let resp = client().get(proxy.url("/users")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_double_slash_normalized() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/data"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    // Target URL with trailing slash
    let proxy = TestProxy::start(
        Url::parse(&format!("{}/api/", mock.uri())).unwrap(),
        Duration::from_secs(60),
        0,
        Duration::from_secs(5),
        None,
    )
    .await;

    // Request path with leading slash — should produce /api/data, not /api//data
    let resp = client().get(proxy.url("/data")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_query_string_forwarded() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "test"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client()
        .get(proxy.url("/search?q=test&page=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Header handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hop_by_hop_stripped_outgoing() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/headers"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let _resp = client()
        .get(proxy.url("/headers"))
        .header("connection", "keep-alive")
        .header("transfer-encoding", "chunked")
        .header("proxy-authorization", "Basic xyz")
        .send()
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let headers = &received[0].headers;
    assert!(
        headers.get("connection").is_none(),
        "connection header should be stripped"
    );
    assert!(
        headers.get("transfer-encoding").is_none(),
        "transfer-encoding header should be stripped"
    );
    assert!(
        headers.get("proxy-authorization").is_none(),
        "proxy-authorization header should be stripped"
    );
}

#[tokio::test]
async fn test_client_identifying_stripped() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/id"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let _resp = client()
        .get(proxy.url("/id"))
        .header("x-forwarded-for", "1.2.3.4")
        .header("x-real-ip", "5.6.7.8")
        .header("forwarded", "for=1.2.3.4")
        .send()
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    let headers = &received[0].headers;
    assert!(headers.get("x-forwarded-for").is_none());
    assert!(headers.get("x-real-ip").is_none());
    assert!(headers.get("forwarded").is_none());
}

#[tokio::test]
async fn test_custom_headers_forwarded() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/custom"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let _resp = client()
        .get(proxy.url("/custom"))
        .header("authorization", "Bearer token123")
        .header("x-custom", "value123")
        .send()
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    let headers = &received[0].headers;
    assert_eq!(
        headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer token123"
    );
    assert_eq!(
        headers.get("x-custom").unwrap().to_str().unwrap(),
        "value123"
    );
}

#[tokio::test]
async fn test_host_header_set() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/host"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let _resp = client().get(proxy.url("/host")).send().await.unwrap();

    let received = mock.received_requests().await.unwrap();
    let host = received[0].headers.get("host").unwrap().to_str().unwrap();
    // Host should be the mock server's address, not the proxy's
    let mock_addr = Url::parse(&mock.uri()).unwrap();
    let expected_host = format!(
        "{}:{}",
        mock_addr.host_str().unwrap(),
        mock_addr.port().unwrap()
    );
    assert_eq!(host, expected_host);
}

#[tokio::test]
async fn test_custom_user_agent() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ua"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = TestProxy::start(
        Url::parse(&mock.uri()).unwrap(),
        Duration::from_secs(60),
        0,
        Duration::from_secs(5),
        Some("MyBot/1.0".to_string()),
    )
    .await;

    let _resp = client()
        .get(proxy.url("/ua"))
        .header("user-agent", "original")
        .send()
        .await
        .unwrap();

    let received = mock.received_requests().await.unwrap();
    let ua = received[0]
        .headers
        .get("user-agent")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(ua, "MyBot/1.0");
}

#[tokio::test]
async fn test_response_headers_stripped() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/resp"))
        .respond_with(
            ResponseTemplate::new(200)
                .append_header("x-custom", "yes")
                .append_header("connection", "close")
                .append_header("content-encoding", "gzip")
                .append_header("transfer-encoding", "chunked"),
        )
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client().get(proxy.url("/resp")).send().await.unwrap();

    assert_eq!(resp.headers().get("x-custom").unwrap(), "yes");
    assert!(resp.headers().get("connection").is_none());
    assert!(resp.headers().get("content-encoding").is_none());
    // Note: transfer-encoding is stripped from upstream response headers, but hyper
    // re-adds "transfer-encoding: chunked" for streaming responses without content-length.
    // This is correct HTTP/1.1 behavior, so we don't assert its absence.
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_429_exhausts_pool_returns_503() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&mock)
        .await;

    // 1 slot (default pool) + upstream always 429 → all IPs exhausted → 503
    let proxy = simple_proxy(&mock).await;
    let resp = client().get(proxy.url("/limited")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn test_upstream_error_returns_502() {
    // Point at a port that's almost certainly not listening
    let proxy = TestProxy::start(
        Url::parse("http://127.0.0.1:1").unwrap(),
        Duration::from_secs(60),
        0,
        Duration::from_secs(5),
        None,
    )
    .await;

    let resp = client().get(proxy.url("/anything")).send().await.unwrap();
    // Connection refused → upstream error → all attempts fail → 503
    // (each failed attempt cools down the slot, and with 1 slot, pool is exhausted)
    assert!(
        resp.status() == 502 || resp.status() == 503,
        "expected 502 or 503, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_body_too_large_returns_413() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0) // upstream should never be called
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let big_body = vec![0u8; 10 * 1024 * 1024 + 1]; // 10MB + 1 byte
    let resp = client()
        .post(proxy.url("/upload"))
        .body(big_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

#[tokio::test]
async fn test_rate_limit_timeout_returns_503() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/rated"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;

    let proxy = TestProxy::start(
        Url::parse(&mock.uri()).unwrap(),
        Duration::from_secs(60),
        1,                         // 1 req/sec
        Duration::from_millis(50), // 50ms timeout
        None,
    )
    .await;

    // First request consumes the one token — should succeed
    let resp = client().get(proxy.url("/rated")).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    // Second request immediately — rate limited, times out after 50ms → 503
    let resp = client().get(proxy.url("/rated")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

// ---------------------------------------------------------------------------
// Response integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_large_response_body_intact() {
    let mock = MockServer::start().await;
    let body: String = "abcdefghij".repeat(10_000); // 100KB
    Mock::given(method("GET"))
        .and(path("/large"))
        .respond_with(ResponseTemplate::new(200).set_body_string(&body))
        .mount(&mock)
        .await;

    let proxy = simple_proxy(&mock).await;
    let resp = client().get(proxy.url("/large")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let received = resp.text().await.unwrap();
    assert_eq!(received.len(), body.len());
    assert_eq!(received, body);
}
