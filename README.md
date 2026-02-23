# packet-prism

Outbound proxy with IP rotation and optional rate limiting. Proxies all requests to a single target URL, rotating through a pool of source IPs with cooldown on 429/error responses. An optional token-bucket rate limiter can throttle outbound request rate.

## Quick start

Download the latest binary from [releases](https://github.com/Haut/packet-prism/releases) and run:

```bash
curl -sL https://github.com/Haut/packet-prism/releases/latest/download/packet-prism_linux_x86_64.tar.gz | tar xz

# simplest — default IP, no rate limit
./packet-prism --target https://api.example.com

# rate limited
./packet-prism --target https://api.example.com --rate-limit 10

# rotate through IPs
./packet-prism --target https://api.example.com --ips 2001:db8::1,2001:db8::2,2001:db8::3

# both
./packet-prism --target https://api.example.com --ips 2001:db8::1,2001:db8::2 --rate-limit 5
```

Run `packet-prism --help` for all options.

## Configuration

All flags have corresponding environment variables as fallback:

```bash
# flags
packet-prism --target https://example.com --ips 2001:db8::1,2001:db8::2 --rate-limit 10

# env vars (for docker-compose, systemd, etc.)
TARGET_URL=https://example.com IPS=2001:db8::1,2001:db8::2 RATE_LIMIT=10 packet-prism
```

Flags take priority over env vars.

| Flag             | Env var            | Required | Default        | Description                     |
|------------------|--------------------|----------|----------------|---------------------------------|
| `--target`       | `TARGET_URL`       | yes      |                | Upstream URL (scheme + host)    |
| `--listen`       | `LISTEN_ADDR`      | no       | `0.0.0.0:8080` | Bind address                    |
| `--user-agent`   | `USER_AGENT`       | no       |                | Custom User-Agent header        |
| `--ips`          | `IPS`              | no       | OS default     | Comma-separated source IP list  |
| `--cooldown`     | `COOLDOWN_SECONDS` | no       | `60`           | Per-IP cooldown on 429/err (s)  |
| `--rate-limit`   | `RATE_LIMIT`       | no       | `0` (off)      | Max requests per second         |
| `--rate-timeout`  | `RATE_TIMEOUT_MS`  | no       | `5000`         | Max wait time in ms             |

## Deploy

### From source

```bash
git clone https://github.com/Haut/packet-prism.git
cd packet-prism
make build
./target/release/packet-prism --target https://api.example.com
```

### Docker

```bash
git clone https://github.com/Haut/packet-prism.git
cd packet-prism
cp .env.example .env        # edit with your values
docker compose up -d --build
```

### Systemd

The [install script](deploy/install.sh) downloads a release binary, copies it to `/usr/local/bin`, and sets up a systemd service:

```bash
git clone https://github.com/Haut/packet-prism.git
cd packet-prism/deploy
sudo ./install.sh v0.1.0
sudo vi /etc/packet-prism/.env
sudo systemctl enable --now packet-prism
```

## Make targets

| Target    | Description              |
|-----------|--------------------------|
| `build`   | Build release binary     |
| `run`     | Build and run            |
| `fmt`     | Run cargo fmt            |
| `clippy`  | Run clippy with warnings |
| `clean`   | Remove build artifacts   |
| `docker`  | Build and run via Docker |
| `install` | Install on server        |
