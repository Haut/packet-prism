BINARY  := packet-prism

.PHONY: build run fmt clippy clean docker install

build:
	cargo build --release

run:
	cargo run --release

fmt:
	cargo fmt

clippy:
	cargo clippy -- -D warnings

clean:
	cargo clean

docker:
	docker compose up --build

install:
	deploy/install.sh $(VERSION)
