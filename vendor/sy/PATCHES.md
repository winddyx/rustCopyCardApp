# vendor/sy notes

Upstream: https://github.com/nijaru/sy
Version: v0.4.1 (commit ca6f0d87)
License: MIT, see LICENSE in this directory.

Only `src/` is kept (no `src/bin/`, tests, benches or docs). `Cargo.toml` drops the
`[[bin]]`, `[[test]]`, `[[bench]]` and `[dev-dependencies]` sections that pointed at them.

Four minimal changes follow: three so both platforms compile, one so the host can see
copy progress.

The three compile fixes:

1. `Cargo.toml`: add the `Win32_Foundation` feature to `windows-sys`.
   Upstream only declares `Win32_Storage_FileSystem`, but `GetDiskFreeSpaceExW` in
   `src/resource.rs` needs types from `Win32_Foundation`, so Windows builds fail.
2. `src/transport/ssh.rs` (Windows branch): use the existing `temp_path` instead of the
   undefined `dest_path`. Upstream references an undeclared variable in the
   `#[cfg(not(unix))]` branch, so Windows builds fail.
3. `src/endpoint/local.rs`: `create_symlink` uses `tokio::fs::symlink_file` /
   `symlink_dir` on Windows. Upstream calls `tokio::fs::symlink`, which is Unix only,
   so Windows builds fail.

The remaining change is about copy progress, not compilation:

4. `src/sync/progress.rs` gains two observers: `set_file_observer` /
   `notify_file_started` for "a file started copying", and `set_bytes_observer` /
   `notify_bytes_accounted` for byte counts. `copy_file` in `src/sync/transfer.rs`
   fires the first, and so does `update()` because updates go through
   `sync_file_with_delta` and never reach `copy_file`; `src/sync/mod.rs` reports the size
   of every finished task (create, update and skip all count).
   Upstream has no per-file callback at all, so a host can only guess progress from the
   bars. With these two callbacks rcca turns off every sy progress output
   (`quiet=true`, `progress=false`) and draws one line with phase, percentage, bytes and
   the current file. Skips count as handled bytes, otherwise the overall figure could
   never reach 50% when most of the target already exists.

Re-apply these four changes when upgrading sy, or drop a patch once upstream fixes it.
