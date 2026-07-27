MAKEFILE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
MANIFEST_PATH := $(MAKEFILE_DIR)Cargo.toml

build:
	mkdir -p debian/tmp_files/.cargo
	CARGO_HOME=debian/tmp_files/.cargo cargo build --release --manifest-path "$(MANIFEST_PATH)"

clean:
	cargo clean --manifest-path "$(MANIFEST_PATH)"
