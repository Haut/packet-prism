FROM rust:1.91-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM alpine:3.21
RUN apk add --no-cache ca-certificates \
    && addgroup -S prism && adduser -S prism -G prism
WORKDIR /app
COPY --from=builder /app/target/release/packet-prism .
USER prism
CMD ["./packet-prism"]
