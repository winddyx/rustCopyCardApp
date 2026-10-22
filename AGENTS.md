# rustCopyCardApp project conventions

## Identity

- Package `rustcopycardapp`; library and binary are both `rcca` (`[lib]` / `[[bin]]` in
  `Cargo.toml`).
- Working branch `main` (copies go through the embedded sy). The hand-written engine is
  kept for reference only, reachable through the tag `v1.0.2`.
- Remote `fork` is `git@github.com:winddyx/rustCopyCardApp.git`, the user's own public
  repository; its `main` carries this line of work (`git push fork main`). Upstream
  nijaru/sy is never touched (no PRs, no issues, no feature requests).
- Behaviour changes bump `Cargo.toml` and add `git tag -a vX.Y.Z`; tags so far are
  `v2.1.0` … `v2.10.0`.
- License: MIT (`LICENSE`, Copyright (c) 2026 winddyx). `vendor/sy` keeps its own MIT
  license and copyright at `vendor/sy/LICENSE`; both notices must stay.

## Who does what

- `src/engine.rs` drives the embedded sy through
  `SyncEngine::with_config(LocalTransport::new(), config).sync(source, target)`. Do not
  reimplement copying and do not print sy's own progress (see the interface rules below).
- The source is passed by content (`<source>/`): the target receives the contents of the
  source directory, not an extra level named after it.
- Mirror semantics are fixed: `--delete` plus sy's default `max_delete 50%` guard. No
  switch exposes anything else. sy runs on every invocation, including a source with no
  regular files: an empty source still has to delete the target's extra files (and is
  refused by the guard when that exceeds 50%), a symlink-only source still has to copy
  the links, and a tree of empty directories still has to be created. Never short-circuit
  the copy because the manifest is empty.
- rcca owns the preflight (path nesting, writable target, free space, 4 GiB cap on FAT,
  unreadable sources, special files), the manifest from `src/scan.rs`, XXH64 verification
  in `src/verify.rs`, the single progress line in `src/ui.rs`, the summary in
  `src/report.rs` and the exit codes.
- The target is resolved without creating it (`resolve_target`), so a layout that is
  rejected with exit 2 never leaves a directory behind. It is created only after the
  nesting check, then canonicalised and checked once more.
- Special files (fifo, socket, device) are a preflight failure with exit 3, naming the
  first one. sy handles only regular files and symlinks: it would read a fifo as a normal
  file and block on it forever, and a socket fails with ENOTSUP. Do not turn this back
  into a note — the old note promised "not copied" while sy still attempted the copy.

## vendor/sy

- Keep only `src/`, `Cargo.toml` and `LICENSE`. Four patches, all documented in
  `vendor/sy/PATCHES.md`: three Windows compile fixes (`Win32_Foundation` in
  `windows-sys`, `dest_path` in `ssh.rs`, `symlink_file`/`symlink_dir`) and one callback
  patch (`set_file_observer` / `set_bytes_observer`, wired in `transfer.rs` and
  `mod.rs`) that lets rcca draw its own progress. The callback's `transferred_file` flag
  is true only for a regular file, so symlinks never reach the `Transfer` count.
- Any vendor change updates `PATCHES.md` in the same commit. Upgrade steps live there.

## Commands

- `make dist`: wipes `dist/`, then builds five targets: `aarch64-apple-darwin`,
  `x86_64-pc-windows-gnu`, `x86_64-unknown-linux-musl`,
  `armv7-unknown-linux-musleabihf`, `aarch64-unknown-linux-musl`. Artifacts are
  `dist/rcca-macos-arm64`, `dist/rcca-windows-x64.exe`, `dist/rcca-linux-x64`,
  `dist/rcca-linux-armv7`, `dist/rcca-linux-armv8`, their UPX-packed twins
  `dist/rcca-windows-x64-upx.exe`, `dist/rcca-linux-x64-upx`,
  `dist/rcca-linux-armv7-upx`, `dist/rcca-linux-armv8-upx`, plus `SHA256SUMS`.
  macOS stays unpacked (UPX rejects Mach-O arm64 without `--force-macos`); the packed
  copies sit beside the originals rather than replacing them. UPX is required, its flags
  are `UPX_FLAGS` (`-9 --lzma --no-progress`).
- Cross compiling needs the rustup targets above, Homebrew `mingw-w64` for Windows,
  `zig` for the musl Linux targets and Homebrew `upx` for the packed artifacts.
  `scripts/zig-cc.sh` / `scripts/zig-ar.sh` wrap zig as
  the C compiler, `rust-lld` links, and OpenSSL is vendored for Linux via a
  target-specific feature in `Cargo.toml` (`sy`'s `ssh` feature pulls openssl-sys).
  armv7 additionally links an empty `libatomic.a` (ARMv7 has native 64-bit atomics) that
  the recipe creates under `target/armv7-support`.
- `make test` (`cargo test`): 58 unit tests in `src/`, 33 integration tests in `tests/`
  (`cli_flow.rs`, `failure_paths.rs`).
- `make lint`: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.

## Invariants

- Exit codes: 0 success; 1 copy failure (including sy errors) or verification failure;
  2 usage error (nesting, unknown option, `--jobs 0`); 3 preflight failure; 130 Ctrl-C
  during verification.
- Ctrl-C during the copy is not intercepted (the default signal handling ends the
  process and may leave `.sy-xxxxxxxx.tmp`, cleaned by `--clean-stale`). Only the
  verification phase installs a handler: the first signal asks for a graceful stop
  (finish the current file, print the summary, exit 130), a second one exits immediately
  with 130 without printing the summary. Verification only checks the flag between
  files, so the second signal is the way out of a long single-file hash.
- Per-file copy errors are not capped unless `--fail-fast` is given: `engine::Options`
  maps `None` to sy's `0` ("unlimited"), which must be assigned explicitly because sy's
  `SyncConfig::test_default()` carries `max_errors: 100`. A capped run aborts mid-copy
  and prints only `rcca: copy failed: sy: …`, without a summary.
- When more than 50% of the target files would be deleted, sy refuses, the run exits 1
  and the target stays untouched. That upstream guard is intentional.
- Verification deletes a mismatching target file by default; `--keep-bad` quarantines it
  as `.<name>.badhash`. A final file name therefore means "verified".
- `--skip-unreadable` filters rcca's manifest only; sy still scans and attempts those
  files, so its error for the same destination is dropped (the preflight note stands)
  and one unreadable file counts as exactly one problem. A file removed this way is not
  verified, so "a final name means verified" has this one exception.

## Terminal output (easiest place to break things)

- Exactly one line, redrawn in place on stderr, no bars, no speed, no ETA:
  `label  percentage  done/total  current file`.
- Percentage denominator is twice the total source bytes: copy fills the first half,
  verification the second. The copy numerator counts every finished file including
  skipped ones; the verify numerator is bytes read, halved because both sides are read.
  When the denominator is 0 (every source file is empty) the percentage column shows
  `-`, because a byte-based percentage is undefined there.
- Labels `SCAN` / `COPY` / `VERIFY` are padded to 6 columns, the file name starts at
  column 8, over-long names are truncated from the start so the extension survives.
- stdout carries the summary only, so `rcca source target > log` is clean text. Without a
  TTY on stderr or with `--quiet`, nothing is drawn, no thread starts, stderr is empty.
- The failure count in the result line comes from `Stats::failure_count()`, not from the
  length of the example list: the store is bounded (32 examples, 64 KiB), the count is
  not. `--verbose` raises both bounds so its list is complete. Failure text is flattened
  to one line before printing, sy wraps its errors.
- The summary's `Transfer` row counts regular files only and pairs them with sy's
  `bytes_transferred`; the `Verify` row's byte figure covers only the files whose source
  side was actually hashed (`checked_bytes`), so a missing target contributes 0.
- Symlinks are counted in `Manifest::skipped_symlinks` and reported as a stderr note,
  never silently dropped. A source that holds only symlinks has an empty manifest but is
  still copied (the note is accurate only because sy runs regardless of the manifest).
  Special files never reach the copy: they fail the preflight (`skipped_special` and
  `first_special` only build that message).
- `-j/--jobs` (default 1) applies to both phases: sy's `max_concurrent` for the copy, a
  `std::thread::scope` pool with one buffer per worker for verification. Verification
  collects one outcome per file and reports in manifest order, so results never depend on
  worker order. Keep the default serial; above 1 the "current file" jumps.
- The label is dim, everything else default colour. `--color auto|always|never`; `NO_COLOR`
  or `TERM=dumb` disables colour.

## Definition of done

- `make test` and `make lint` green; `make dist` rebuilt when the change is user visible,
  with `cd dist && shasum -a 256 -c SHA256SUMS` passing.
- Interface changes update the `src/ui.rs` unit tests (column alignment, degradation
  order, width bound) and the sample in `README.md`.
- Changes that touch sy semantics (mirror delete, skip, threshold, jobs) run
  `tests/cli_flow.rs` and `tests/failure_paths.rs` and keep the exit codes above intact.

## Documentation

- `README.md` is the user-facing document: build, options, exit codes, sample output,
  interface rules. There are no design documents; the code is the specification, so a
  behaviour change updates README and code together.
