# rustCopyCardApp / rcca build entry points
#
# make dist   release build: wipe dist/, rebuild macOS arm64 and Windows x64, write SHA256SUMS
# make test   run all tests
# make lint   clippy plus formatting check

SHELL := /bin/bash

BIN     := rcca
DIST    := dist
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)

MAC_TARGET := aarch64-apple-darwin
WIN_TARGET := x86_64-pc-windows-gnu

MAC_ARTIFACT := $(DIST)/$(BIN)-macos-arm64
WIN_ARTIFACT := $(DIST)/$(BIN)-windows-x64.exe

.PHONY: all dist makedist macos-arm64 windows-x64 checksums test lint clean

all: dist

# Every run wipes dist/ and rebuilds, so dist/ only ever reflects the current source.
dist:
	rm -rf $(DIST)
	@echo "== rcca $(VERSION) =="
	$(MAKE) --no-print-directory macos-arm64
	$(MAKE) --no-print-directory windows-x64
	$(MAKE) --no-print-directory checksums
	@echo "== $(DIST) =="
	@ls -lh $(DIST)
	@cat $(DIST)/SHA256SUMS

# kept for the old spelling
makedist: dist

macos-arm64:
	cargo build --release --target $(MAC_TARGET)
	@mkdir -p $(DIST)
	install -m 0755 target/$(MAC_TARGET)/release/$(BIN) $(MAC_ARTIFACT)
	@echo "built $(MAC_ARTIFACT)"

windows-x64:
	cargo build --release --target $(WIN_TARGET)
	@mkdir -p $(DIST)
	install -m 0755 target/$(WIN_TARGET)/release/$(BIN).exe $(WIN_ARTIFACT)
	@echo "built $(WIN_ARTIFACT)"

checksums:
	@cd $(DIST) && shasum -a 256 $(notdir $(MAC_ARTIFACT)) $(notdir $(WIN_ARTIFACT)) > SHA256SUMS

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

clean:
	cargo clean
	rm -rf $(DIST)
