mod config;
mod pool;
mod proxy;
mod ratelimit;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use config::Config;
use pool::Pool;
use proxy::ProxyHandler;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    let validated = match cfg.validate() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let pool = Arc::new(Pool::new(&validated.parsed_ips));
    let cooldown = Duration::from_secs(validated.cooldown_secs);
    let rate_timeout = Duration::from_millis(validated.rate_timeout_ms);

    tracing::info!(
        ips = validated.parsed_ips.len(),
        cooldown_secs = validated.cooldown_secs,
        rate_limit = validated.rate_limit,
        "starting"
    );

    let handler = Arc::new(ProxyHandler::new(
        pool,
        cooldown,
        validated.rate_limit,
        rate_timeout,
        validated.target_url.clone(),
        validated.user_agent.clone(),
    ));

    let addr: SocketAddr = validated.listen.parse().unwrap_or_else(|e| {
        eprintln!("invalid listen address '{}': {e}", validated.listen);
        std::process::exit(1);
    });

    let listener = TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("failed to bind {addr}: {e}");
        std::process::exit(1);
    });

    tracing::info!(listen = %addr, target = %validated.target_url, "listening");

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
                let (stream, _remote) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "accept error");
                        continue;
                    }
                };

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
