# vendor/sy 说明

上游：https://github.com/nijaru/sy
版本：v0.4.1（commit ca6f0d87）
许可：MIT，见同目录 LICENSE

只保留 `src/`（去掉 `src/bin/` 与测试、基准、文档），`Cargo.toml` 去掉了指向这些目录的
`[[bin]]`、`[[test]]`、`[[bench]]` 与 `[dev-dependencies]` 段。

为了在 macOS arm64 与 x86_64-pc-windows-gnu 上编译，做了以下三处最小改动：

1. `Cargo.toml`：`windows-sys` 增加 `Win32_Foundation` feature。
   上游只声明了 `Win32_Storage_FileSystem`，而 `src/resource.rs` 用到的
   `GetDiskFreeSpaceExW` 还依赖 `Win32_Foundation` 中的类型，windows 目标下编译失败。
2. `src/transport/ssh.rs`（Windows 分支）：把不存在的 `dest_path` 改为 `temp_path`。
   上游在 `#[cfg(not(unix))]` 分支里引用了未定义的变量，windows 目标下编译失败。
3. `src/endpoint/local.rs`：`create_symlink` 在 Windows 上改用
   `tokio::fs::symlink_file` / `symlink_dir`。上游直接调用仅在 unix 可用的
   `tokio::fs::symlink`，windows 目标下编译失败。

升级 sy 时需要重新执行上述三处改动，或确认上游已修复后删除对应补丁。
