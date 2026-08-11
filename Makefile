VENDOR ?= 1
CARGO_VENDOR_FLAGS ?=
CARGO_VENDOR_COMMAND ?= cargo

CARGO_LOCKFILE_V4 := $(shell grep -Eq '^version = 4$$' Cargo.lock 2>/dev/null && echo 1)
ifeq ($(CARGO_LOCKFILE_V4),1)
CARGO_NIGHTLY_PATH := $(shell rustup which --toolchain nightly cargo 2>/dev/null)
ifneq ($(strip $(CARGO_NIGHTLY_PATH)),)
CARGO_VENDOR_COMMAND := $(CARGO_NIGHTLY_PATH)
ifeq ($(strip $(CARGO_VENDOR_FLAGS)),)
CARGO_VENDOR_FLAGS := -Znext-lockfile-bump
endif
endif
endif

.PHONY: build clean distclean vendor extract-vendor

build: extract-vendor
	mkdir -p debian/tmp_files/.cargo
	CARGO_HOME=debian/tmp_files/.cargo cargo build --release $(if $(filter 1,$(VENDOR)),--frozen --offline,)

clean:
	rm -rf vendor .cargo/config .cargo/config.toml debian/tmp_files/.cargo
	cargo clean

distclean: clean
	rm -rf .cargo vendor vendor.tar debian/tmp_files/.cargo

vendor:
	rm -rf vendor
	mkdir -p .cargo
	@set -eu; tmp=$$(mktemp); trap 'rm -f "$$tmp"' EXIT; $(CARGO_VENDOR_COMMAND) vendor --locked $(CARGO_VENDOR_FLAGS) > "$$tmp"; sed '$$d' "$$tmp" > .cargo/config; echo 'directory = "vendor"' >> .cargo/config; tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner -cf vendor.tar vendor .cargo/config; rm -rf vendor

extract-vendor:
ifeq ($(VENDOR),1)
	@if test -f vendor.tar; then rm -rf vendor; tar pxf vendor.tar; else $(MAKE) vendor; rm -rf vendor; tar pxf vendor.tar; fi
endif
