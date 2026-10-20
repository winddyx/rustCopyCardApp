# rustCopyCardApp / rcca 构建入口
#
# make dist   发行构建：先删除 dist/，再重建 macOS arm64 与 Windows x64，并生成 SHA256SUMS
# make test   运行全部测试
# make lint   clippy 与格式检查

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

# 每次先删 dist/ 再完整重建，dist 只反映当前源码。
dist:
	rm -rf $(DIST)
	@echo "== rcca $(VERSION) =="
	$(MAKE) --no-print-directory macos-arm64
	$(MAKE) --no-print-directory windows-x64
	$(MAKE) --no-print-directory checksums
	@echo "== $(DIST) =="
	@ls -lh $(DIST)
	@cat $(DIST)/SHA256SUMS

# 兼容旧写法
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
