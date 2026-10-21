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

其余改动与编译无关，都是让宿主知道复制进展：

4. `src/sync/progress.rs` 增加两个观察者：文件开始复制的 `set_file_observer` / `notify_file_started`，
   以及字节数的 `set_bytes_observer` / `notify_bytes_accounted`。
   `src/sync/transfer.rs` 的 `copy_file` 在开始复制时通知前一个；`src/sync/mod.rs` 在每个任务处理完时
   上报该文件字节数（新建、更新与跳过都算）。
   上游没有任何逐文件回调，宿主只能靠进度条猜进展；有了这两个回调，rcca 可以关掉 sy 的全部进度输出
   （`quiet=true`、`progress=false`），自己用一行文字显示阶段、百分比、字节与当前文件。
   跳过也算进"已处理字节"，否则目标里已存在大半文件时总进度永远到不了 50%。

升级 sy 时需要重新执行上述四处改动，或确认上游已修复后删除对应补丁。
