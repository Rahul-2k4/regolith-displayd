VENDOR ?= 1

.PHONY: build clean distclean vendor extract-vendor

build: extract-vendor
	mkdir -p debian/tmp_files/.cargo
	CARGO_HOME=debian/tmp_files/.cargo cargo build --release $(if $(filter 1,$(VENDOR)),--frozen --offline,)

clean:
	cargo clean

distclean: clean
	rm -rf .cargo vendor vendor.tar debian/tmp_files/.cargo

vendor:
	mkdir -p .cargo
	cargo vendor | head -n -1 > .cargo/config
	echo 'directory = "vendor"' >> .cargo/config
	tar pcf vendor.tar vendor
	rm -rf vendor

extract-vendor:
ifeq ($(VENDOR),1)
	rm -rf vendor
	tar pxf vendor.tar
endif
