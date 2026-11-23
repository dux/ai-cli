.PHONY: build

# Installs the release binary as ~/.cargo/bin/ai, the one on PATH.
build:
	cargo install --path .
