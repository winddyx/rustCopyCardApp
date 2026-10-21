# rustCopyCardApp / rcca build entry points
#
# make dist   release build: wipe dist/, rebuild every platform, write SHA256SUMS
# make test   run all tests
# make lint   clippy plus formatting check

SHELL := /bin/bash

BIN     := rcca
DIST    := dist
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)

MAC_TARGET  := aarch64-apple-darwin
WIN_TARGET  := x86_64-pc-windows-gnu
LNX_X64     := x86_64-unknown-linux-musl
LNX_ARMV7   := armv7-unknown-linux-musleabihf
LNX_ARMV8   := aarch64-unknown-linux-musl

MAC_ARTIFACT    := $(DIST)/$(BIN)-macos-arm64
WIN_ARTIFACT    := $(DIST)/$(BIN)-windows-x64.exe
X64_ARTIFACT    := $(DIST)/$(BIN)-linux-x64
ARMV7_ARTIFACT  := $(DIST)/$(BIN)-linux-armv7
ARMV8_ARTIFACT  := $(DIST)/$(BIN)-linux-armv8

ARTIFACTS := $(MAC_ARTIFACT) $(WIN_ARTIFACT) $(X64_ARTIFACT) $(ARMV7_ARTIFACT) $(ARMV8_ARTIFACT)

# The Linux binaries are static musl builds cross compiled with zig, which ships a
# complete musl sysroot and libc. See scripts/zig-cc.sh for why the wrapper exists.
ZIG_CC := $(CURDIR)/scripts/zig-cc.sh
ZIG_AR := $(CURDIR)/scripts/zig-ar.sh
# rust-lld links the self-contained musl targets without a C linker driver.
RUST_LLD := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin/rust-lld
# armv7 needs -latomic on the link line; ARMv7 has native 64-bit atomics, so rustc's
# compiler_builtins already provide the symbols and an empty archive satisfies the flag.
ARMV7_SUPPORT := $(CURDIR)/target/armv7-support

.PHONY: all dist makedist macos-arm64 windows-x64 linux-x64 linux-armv7 linux-armv8 checksums test lint clean

all: dist

# Every run wipes dist/ and rebuilds, so dist/ only ever reflects the current source.
dist:
	rm -rf $(DIST)
	@echo "== rcca $(VERSION) =="
	$(MAKE) --no-print-directory macos-arm64
	$(MAKE) --no-print-directory windows-x64
	$(MAKE) --no-print-directory linux-x64
	$(MAKE) --no-print-directory linux-armv7
	$(MAKE) --no-print-directory linux-armv8
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

linux-x64:
	CC_x86_64_unknown_linux_musl=$(ZIG_CC) AR_x86_64_unknown_linux_musl=$(ZIG_AR) \
	RCCA_ZIG_TARGET=x86_64-linux-musl \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_X64)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_X64)/release/$(BIN) $(X64_ARTIFACT)
	@echo "built $(X64_ARTIFACT)"

linux-armv7:
	@mkdir -p $(ARMV7_SUPPORT)
	zig ar cr $(ARMV7_SUPPORT)/libatomic.a
	CC_armv7_unknown_linux_musleabihf=$(ZIG_CC) AR_armv7_unknown_linux_musleabihf=$(ZIG_AR) \
	RCCA_ZIG_TARGET=arm-linux-musleabihf \
	RUSTFLAGS="-L $(ARMV7_SUPPORT)" \
	CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_ARMV7)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_ARMV7)/release/$(BIN) $(ARMV7_ARTIFACT)
	@echo "built $(ARMV7_ARTIFACT)"

linux-armv8:
	CC_aarch64_unknown_linux_musl=$(ZIG_CC) AR_aarch64_unknown_linux_musl=$(ZIG_AR) \
	RCCA_ZIG_TARGET=aarch64-linux-musl \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_ARMV8)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_ARMV8)/release/$(BIN) $(ARMV8_ARTIFACT)
	@echo "built $(ARMV8_ARTIFACT)"

checksums:
	@cd $(DIST) && shasum -a 256 $(notdir $(ARTIFACTS)) > SHA256SUMS

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

clean:
	cargo clean
	rm -rf $(DIST) $(ARMV7_SUPPORT)
