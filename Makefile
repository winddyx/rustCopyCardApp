# rustCopyCardApp / rcca build entry points
#
# make dist   release build: wipe dist/, rebuild every platform, upx pack the
#             Windows/Linux artifacts, write SHA256SUMS
# make test   run all tests
# make lint   clippy plus formatting check

SHELL := /bin/bash

BIN     := rcca
DIST    := dist
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)

# 校验和工具：macOS 上是 `shasum -a 256`，Windows 上 `mingw32-make win-dist
# SHA256=sha256sum` 覆盖。两者的清单都能被 `shasum -c` 与 `sha256sum -c` 读取，
# 只是 Windows 的 sha256sum 会在文件名前加上二进制模式标记 `*`。
SHA256 ?= shasum -a 256

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

# upx-packed copies. macOS is left alone: upx refuses Mach-O arm64 unless
# --force-macos is given, which upstream marks as unsupported. The unpacked
# binaries stay in dist/ as well, so the packed ones are the small alternative
# and not a replacement.
WIN_UPX_ARTIFACT   := $(DIST)/$(BIN)-windows-x64-upx.exe
X64_UPX_ARTIFACT   := $(DIST)/$(BIN)-linux-x64-upx
ARMV7_UPX_ARTIFACT := $(DIST)/$(BIN)-linux-armv7-upx
ARMV8_UPX_ARTIFACT := $(DIST)/$(BIN)-linux-armv8-upx
UPX_ARTIFACTS := $(WIN_UPX_ARTIFACT) $(X64_UPX_ARTIFACT) $(ARMV7_UPX_ARTIFACT) $(ARMV8_UPX_ARTIFACT)

ARTIFACTS := $(MAC_ARTIFACT) $(WIN_ARTIFACT) $(X64_ARTIFACT) $(ARMV7_ARTIFACT) $(ARMV8_ARTIFACT) $(UPX_ARTIFACTS)

# upx is a hard requirement for the packed artifacts; -9 --lzma is the smallest
# output (roughly a quarter of the input) at a slightly slower startup.
UPX       ?= upx
UPX_FLAGS ?= -9 --lzma --no-progress

# The Linux binaries are static musl builds cross compiled with zig, which ships a
# complete musl sysroot and libc. See scripts/zig-cc.sh for why the wrapper exists.
ZIG_CC := $(CURDIR)/scripts/zig-cc.sh
ZIG_AR := $(CURDIR)/scripts/zig-ar.sh
# rust-lld links the self-contained musl targets without a C linker driver.
RUST_LLD := $(shell rustc --print sysroot)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin/rust-lld
# armv7 needs -latomic on the link line; ARMv7 has native 64-bit atomics, so rustc's
# compiler_builtins already provide the symbols and an empty archive satisfies the flag.
ARMV7_SUPPORT := $(CURDIR)/target/armv7-support

.PHONY: all dist win-dist makedist macos-arm64 windows-x64 linux-x64 linux-armv7 linux-armv8 upx-check checksums win-checksums test lint clean

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

# Windows 主机上的发行构建：只产出本平台的产物。另外四个目标需要 macOS SDK 与 zig，
# 在 Windows 上不存在，所以不放进这里，也不假装它们成功。与 `dist` 相同，先清空 dist/。
win-dist:
	rm -rf $(DIST)
	@echo "== rcca $(VERSION) (windows only) =="
	$(MAKE) --no-print-directory windows-x64
	$(MAKE) --no-print-directory win-checksums
	@echo "== $(DIST) =="
	@ls -lh $(DIST)
	@cat $(DIST)/SHA256SUMS

# The packed targets need upx on PATH; missing tooling stops the run instead of
# silently shipping unpacked-only artifacts.
upx-check:
	@command -v $(UPX) >/dev/null 2>&1 || { echo "error: $(UPX) not found; install upx (brew install upx)" >&2; exit 1; }

macos-arm64:
	cargo build --release --target $(MAC_TARGET)
	@mkdir -p $(DIST)
	install -m 0755 target/$(MAC_TARGET)/release/$(BIN) $(MAC_ARTIFACT)
	@echo "built $(MAC_ARTIFACT) (not upx packed)"

windows-x64: upx-check
	cargo build --release --target $(WIN_TARGET)
	@mkdir -p $(DIST)
	install -m 0755 target/$(WIN_TARGET)/release/$(BIN).exe $(WIN_ARTIFACT)
	cp $(WIN_ARTIFACT) $(WIN_UPX_ARTIFACT)
	$(UPX) $(UPX_FLAGS) $(WIN_UPX_ARTIFACT)
	@echo "built $(WIN_ARTIFACT) and $(WIN_UPX_ARTIFACT)"

linux-x64: upx-check
	CC_x86_64_unknown_linux_musl=$(ZIG_CC) AR_x86_64_unknown_linux_musl=$(ZIG_AR) \
	RCCA_ZIG_TARGET=x86_64-linux-musl \
	CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_X64)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_X64)/release/$(BIN) $(X64_ARTIFACT)
	cp $(X64_ARTIFACT) $(X64_UPX_ARTIFACT)
	$(UPX) $(UPX_FLAGS) $(X64_UPX_ARTIFACT)
	@echo "built $(X64_ARTIFACT) and $(X64_UPX_ARTIFACT)"

linux-armv7: upx-check
	@mkdir -p $(ARMV7_SUPPORT)
	zig ar cr $(ARMV7_SUPPORT)/libatomic.a
	CC_armv7_unknown_linux_musleabihf=$(ZIG_CC) AR_armv7_unknown_linux_musleabihf=$(ZIG_AR) \
	RCCA_ZIG_TARGET=arm-linux-musleabihf \
	RUSTFLAGS="-L $(ARMV7_SUPPORT)" \
	CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_ARMV7)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_ARMV7)/release/$(BIN) $(ARMV7_ARTIFACT)
	cp $(ARMV7_ARTIFACT) $(ARMV7_UPX_ARTIFACT)
	$(UPX) $(UPX_FLAGS) $(ARMV7_UPX_ARTIFACT)
	@echo "built $(ARMV7_ARTIFACT) and $(ARMV7_UPX_ARTIFACT)"

linux-armv8: upx-check
	CC_aarch64_unknown_linux_musl=$(ZIG_CC) AR_aarch64_unknown_linux_musl=$(ZIG_AR) \
	RCCA_ZIG_TARGET=aarch64-linux-musl \
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$(RUST_LLD) \
	cargo build --release --target $(LNX_ARMV8)
	@mkdir -p $(DIST)
	install -m 0755 target/$(LNX_ARMV8)/release/$(BIN) $(ARMV8_ARTIFACT)
	cp $(ARMV8_ARTIFACT) $(ARMV8_UPX_ARTIFACT)
	$(UPX) $(UPX_FLAGS) $(ARMV8_UPX_ARTIFACT)
	@echo "built $(ARMV8_ARTIFACT) and $(ARMV8_UPX_ARTIFACT)"

checksums:
	@cd $(DIST) && $(SHA256) $(notdir $(ARTIFACTS)) > SHA256SUMS

# Windows 主机上只有两个产物，清单因此单独生成。
win-checksums:
	@cd $(DIST) && $(SHA256) $(notdir $(WIN_ARTIFACT) $(WIN_UPX_ARTIFACT)) > SHA256SUMS

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings
	cargo fmt --check

clean:
	cargo clean
	rm -rf $(DIST) $(ARMV7_SUPPORT)
