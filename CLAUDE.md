# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Build & Run

```bash
make build                      # build release binary
make clippy                     # run clippy
make fmt                        # run cargo fmt

# or without make:
cargo build --release
cargo clippy -- -D warnings
cargo fmt

# run locally
cp .env.example .env            # edit with your values
make run
```

### Docker

```bash
make docker                     # or: docker compose up --build
```

Rust 1.91+, edition 2024.

There are no tests yet.

## Architecture

**packet-prism** is an outbound proxy with IP rotation and optional rate limiting. Each instance handles one target URL. Multiple instances can be run via Docker Compose, each with its own `.env` file and port.

### How it works

Requests are proxied through a pool of 1+ source IPs (round-robin). An optional token-bucket rate limiter throttles outbound request rate. If no IPs are specified, the OS default is used with a single pool slot.

- **1 IP + rate limit** — rate-limited single-IP proxy
- **N IPs + no rate limit** — IP rotation proxy
- **N IPs + rate limit** — rate-limited rotation proxy

### Core components

- **src/main.rs** — Entry point. Uses clap for CLI with env var fallback via `#[arg(env = "...")]`. Runs the HTTP server with graceful shutdown (SIGINT/SIGTERM).
- **src/config.rs** — `Config` struct (clap derive) and `ValidatedConfig`. `validate()` parses and validates fields.
- **src/pool.rs** — Lock-free IP pool. Each `Slot` has states: idle → busy → cooling. Round-robin acquisition with atomic counter. Builds a `reqwest::Client` per IP with `local_address` binding.
- **src/proxy.rs** — `ProxyHandler` implements the full request lifecycle:
  - `handle()` — buffers body, builds outgoing request, executes via `execute()`, streams response
  - `execute()` — optional rate limit wait, then rotate through pool IPs with cooldown on 429/error
  - `build_outgoing()` — path passthrough, copy headers, strip hop-by-hop + client-identifying headers
  - `ProxyError` enum with status code mapping (503 for rate limit/exhausted IPs, 502 for upstream errors)
- **src/ratelimit.rs** — Token bucket rate limiter with async `wait(timeout)`. Uses `tokio::sync::Mutex`.

### Request flow

1. Incoming request → body buffered (10MB max)
2. Build outgoing request (path passthrough, copy headers, strip hop-by-hop + client-identifying headers)
3. Optional rate limit wait
4. Rotate through pool IPs — execute request, cooldown on 429/error, retry next IP
5. On error → 503 (rate limited / all IPs exhausted) or 502 (upstream error)
6. On success → stream response back

### Configuration

Configured via CLI flags (with env var fallback for docker-compose/systemd). Flags take priority. Run `packet-prism --help` for full usage.
