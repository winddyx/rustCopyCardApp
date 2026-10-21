# rustCopyCardApp 项目约定

## 项目身份

- 包名 `rustcopycardapp`，库与二进制名都是 `rcca`（见 `Cargo.toml` 的 `[lib]`、`[[bin]]`）。
- 版本号与 git tag 一起走：行为变化就升版本并 `git tag -a vX.Y.Z`。当前 `v2.0.3`。
- 主线分支 `sy-wrapper`（复制交给内嵌 sy）。`main` 停在 `v1.0.2` 的自研引擎实现，只作对照，不再演进。
- 仓库尚未配置远端；按全局规则只推自己的 fork，绝不推上游仓库。

## 架构边界

- 复制不自己实现：`src/engine.rs` 调用内嵌的 sy（上游 `nijaru/sy` v0.4.1，path 依赖 `vendor/sy`）。
- 调用入口必须是 `SyncEngine::with_config(LocalTransport::new(), config).sync(source, target)`。
  另一条 `SyncSession`（内部走 `TaskExecutor`）在本地复制时不输出任何进度，终端会整段空白。
- 源路径按“复制内容”语义传递（`<source>/`）：目标目录里是源目录的内容，不多套一层源目录名。
- 镜像语义固定为 `--delete` + `max_delete 50%`（上游默认保护），不再暴露额外开关。
- rcca 自己负责：预检（路径嵌套、目标可写、可用空间、FAT 系列 4 GiB、源文件不可读）、
  遍历清单、XXH64 校验、三行终端界面、汇总与退出码。

## vendor/sy

- 只保留 `src/`、`Cargo.toml`、`LICENSE`，补丁与升级步骤写在 `vendor/sy/PATCHES.md`。
- 三处 Windows 补丁（`windows-sys` 的 `Win32_Foundation`、`ssh.rs` 的 `dest_path`、`symlink_file`/`symlink_dir`）
  是 `x86_64-pc-windows-gnu` 能编译的前提；另有回调补丁（文件开始复制时通知宿主、任务处理完
  上报字节数），让 rcca 关掉 sy 的全部进度输出、自己绘制那一行。改动 vendor 必须同步 `PATCHES.md`。
- 不向上游提交 PR 或 issue；需要的行为差异就在 `vendor/sy` 内改并记录。

## 命令

- `make dist`：先删 `dist/`，再构建 `aarch64-apple-darwin` 与 `x86_64-pc-windows-gnu`，
  产出 `dist/rcca-macos-arm64`、`dist/rcca-windows-x64.exe`、`dist/SHA256SUMS`。
- `make test`（即 `cargo test`）：37 个单元测试 + 19 个集成测试。
- `make lint`：`cargo clippy --all-targets -- -D warnings` 加 `cargo fmt --check`。
- Windows 交叉编译依赖 `rustup target add x86_64-pc-windows-gnu` 与 Homebrew 的 `mingw-w64`。

## 回头要守住的行为

- 退出码：0 全部成功；1 复制失败（含 sy 报错）或校验失败；2 用法错误；3 预检失败；130 校验阶段被 Ctrl-C。
- 复制阶段不拦截 Ctrl-C（信号按系统默认终止），可能残留 `.sy-xxxxxxxx.tmp`，由 `--clean-stale` 清理；
  校验阶段才安装处理，中断后退出码 130。
- 目标里需要删除的文件超过 50% 时 sy 拒绝执行，整次运行以 1 退出、目标保持原样，这是刻意保留的上游保护。

## 终端界面（最容易改错的地方）

- 不画进度条，也不显示速度与 ETA。运行期只有一行文字，原地刷新：`标签  百分比  已完成/总量  当前文件`。
- 百分比分母固定为源总字节的两倍，复制占前一半、校验占后一半；复制阶段分子含被跳过的文件，校验阶段按读过的字节除以二。
- 标签 `SCAN` / `COPY` / `VERIFY` 都补到 6 列，文件名固定在第八列起；文件名超宽时从头截断，保留扩展名。
- 进度写 stderr，stdout 只放汇总；stderr 不是 TTY 或 `--quiet` 时不绘制、不开线程，stderr 为空。
- sy 自身的所有进度输出保持关闭（`quiet=true`、`progress=false`），文件名与字节数靠 vendor 内的两个回调。
- 复制串行（`max_concurrent = 1`），不要为了吞吐改回并发，否则"当前文件"会在多个文件之间跳。
- 标签暗色、其余默认色；`--color auto|always|never`，`NO_COLOR` 与 `TERM=dumb` 时不上色。

## 完成标准

- 改界面、校验或统计后：`make test` 与 `make lint` 全绿，再 `make dist` 确认两个平台产物与 `SHA256SUMS`。
- 界面改动同步更新 `src/ui.rs` 的单元测试（列位对齐、降级顺序、宽度上限）与 README 里的界面样例。
- 涉及 sy 语义的改动（镜像删除、跳过、阈值）跑 `tests/cli_flow.rs` 与 `tests/failure_paths.rs`，
  并确认上面列出的退出码没有变化。

## 文档

- `README.md` 面向使用：构建、选项、退出码、界面样例。不另写设计方案文档，行为以代码为准。
- 行为变化时 README 与代码一起改，不要只在提交信息里说明。
