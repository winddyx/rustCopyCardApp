# vendor/sy 说明

上游：https://github.com/nijaru/sy
版本：v0.4.1（commit ca6f0d87）
许可：MIT，见同目录 LICENSE

只保留 `src/`（去掉 `src/bin/` 与测试、基准、文档），`Cargo.toml` 去掉了指向这些目录的
`[[bin]]`、`[[test]]`、`[[bench]]` 与 `[dev-dependencies]` 段。

为了让两个平台都能编译、并让复制阶段的终端输出可用，做了以下五处最小改动。

先说明编译相关的三处：

1. `Cargo.toml`：`windows-sys` 增加 `Win32_Foundation` feature。
   上游只声明了 `Win32_Storage_FileSystem`，而 `src/resource.rs` 用到的
   `GetDiskFreeSpaceExW` 还依赖 `Win32_Foundation` 中的类型，windows 目标下编译失败。
2. `src/transport/ssh.rs`（Windows 分支）：把不存在的 `dest_path` 改为 `temp_path`。
   上游在 `#[cfg(not(unix))]` 分支里引用了未定义的变量，windows 目标下编译失败。
3. `src/endpoint/local.rs`：`create_symlink` 在 Windows 上改用
   `tokio::fs::symlink_file` / `symlink_dir`。上游直接调用仅在 unix 可用的
   `tokio::fs::symlink`，windows 目标下编译失败。

另外两处改动与编译无关，改的是复制阶段的进度输出：

4. `src/sync/mod.rs`（总体进度模板）：去掉模板开头的 `{msg}\n`，让进度只占一行。
   上游把当前文件名单独放一行，加上进度条后会超出终端宽度，换行后与进度条互相覆盖。
   条形本来就用 `{wide_bar}`，去掉消息行后整行恰好等于终端宽度。
5. `src/sync/progress.rs`：新增常量 `PER_FILE_PROGRESS = false`，逐文件进度条改为一律返回隐藏条。
   上游为每个大于 1 MiB 的文件单独建一条进度条，这些条与总体进度条各自重绘、互不清屏，
   在终端里表现为文件行与进度条互相覆盖。总体进度线已经提供字节、速率与 ETA，逐文件条没有额外信息。

升级 sy 时需要重新执行上述五处改动，或确认上游已修复后删除对应补丁。
