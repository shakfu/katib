.PHONY: build test check run

build:
	cargo build --workspace

test:
	cargo test --workspace

check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

# demo/ is a sample vault. Sync stays off there: it needs `sync = true` in
# demo/.katib/config.toml, which is not set.
VAULT ?= demo

run:
	cargo run -p katib -- "$(VAULT)"
