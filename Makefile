MAKEFILE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
MANIFEST_PATH := $(MAKEFILE_DIR)Cargo.toml

build:
	mkdir -p "$(MAKEFILE_DIR)debian/tmp_files/.cargo"
	CARGO_HOME="$(MAKEFILE_DIR)debian/tmp_files/.cargo" cargo build --release --manifest-path "$(MANIFEST_PATH)"

clean:
	rm -rf "$(MAKEFILE_DIR)debian/tmp_files/.cargo"
	cargo clean --manifest-path "$(MANIFEST_PATH)"
