mod config;
mod pool;
mod proxy;
mod ratelimit;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyper::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use config::{Config, ValidatedConfig};
use pool::Pool;
use proxy::ProxyHandler;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    let Ok(validated) = cfg.validate().inspect_err(|e| eprintln!("error: {e}")) else {
        std::process::exit(1);
    };

    let ValidatedConfig {
        listen,
        target_url,
        user_agent,
        parsed_ips,
        cooldown_secs,
        rate_limit,
        rate_timeout_ms,
    } = validated;

    let pool = Arc::new(Pool::new(&parsed_ips));
    let cooldown = Duration::from_secs(cooldown_secs);
    let rate_timeout = Duration::from_millis(rate_timeout_ms);

    tracing::info!(
        ips = parsed_ips.len(),
        cooldown_secs,
        rate_limit,
        "starting"
    );

    let handler = Arc::new(ProxyHandler::new(
        pool,
        cooldown,
        rate_limit,
        rate_timeout,
        target_url.clone(),
        user_agent,
    ));

    let addr: SocketAddr = listen.parse().unwrap_or_else(|e| {
        eprintln!("invalid listen address '{listen}': {e}");
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });

    tracing::info!(listen = %addr, target = %target_url, "listening");

    // Graceful shutdown signal
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
        }
        tracing::info!("shutting down...");
    };

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((stream, _)) = result.inspect_err(|e| {
                    tracing::error!(error = %e, "accept error");
                }) else {
                    continue;
                };

                stream.set_nodelay(true).ok();

                let handler = handler.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        async move { handler.handle(req).await }
                    });

                    if let Err(e) = http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(io, svc)
                        .await
                    {
                        tracing::error!(error = %e, "connection error");
                    }
                });
            }
            _ = &mut shutdown => {
                tracing::info!("shutdown complete");
                break;
            }
        }
    }
}
