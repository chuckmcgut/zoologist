.PHONY: check fmt lint test pure run

## everything CI runs
check: fmt lint test pure
	cargo deny check

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test

## no C/C++ libraries in the dependency tree
pure:
	scripts/check-pure-rust.sh

## run with the local config file
run:
	cargo run --release -p zoologist-server -- run --config config/zoologist.toml
