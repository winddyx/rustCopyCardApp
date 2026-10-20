# rustCopyCardApp

相机存储卡复制工具，可执行文件名为 `rcca`。四个阶段严格串行：遍历清单、复制到暂存文件、复查源并原子重命名、XXH64 双端校验。

## 构建

```bash
cargo build --release        # 产物：target/release/rcca（Windows 为 rcca.exe）
```

## 用法

```bash
rcca <source> <target> [OPTIONS]
```

源目录内容平铺到目标根（等价于 `cp -R src/. dst/`）；`--keep-source-name` 改为 `dst/<源目录名>/...`。

## 选项

- `--buffer <SIZE>`：读写流水线的缓冲预算，默认 `128M`，允许 8M 至 1024M。
- `--chunk <SIZE>`：数据块大小，默认 `1M`，允许 1M 至 8M；机械盘建议 `4M`。
- `--serial-io`：关闭读写重叠，读一块写一块；同盘机械盘适用。
- `--no-fsync`：不做 fsync，性能优先，但断电后"校验通过"不再可信。
- `--keep-source-name`：目标布局保留源目录名。
- `--skip-unreadable`：跳过不可读的源文件，而不是预检失败。
- `--force`：目标空间不足时仍然继续。
- `--fail-fast`：首个失败即中止。
- `--quiet`：不绘制进度条，逐文件输出 `COPY <rel>` 与汇总。
- `--verbose`：逐文件细节与完整失败清单。
- `--keep-bad`：校验失败时把目标文件隔离为 `.<name>.badhash` 而不是删除。
- `--verify-no-cache`：校验时绕过页缓存（macOS `F_NOCACHE`、Linux `posix_fadvise`）。
- `--clean-stale`：开始前清理目标卷上遗留的 `.rcptmp`。

## 退出码

- 0：全部文件已提交且哈希一致。
- 1：校验失败，或有文件复制/提交失败。
- 2：用法或参数错误（路径不合法、目标嵌套、未知选项）。
- 3：预检失败（空间不足、FAT 系列 4 GiB 上限、源不可读、路径超长、目标不可写）。
- 130：被 SIGINT/SIGTERM 中断。

## 行为要点

- 最终文件名是承诺：文件先写同目录的 `.<name>.rcptmp`，阶段 3 校验长度与 dev/ino 后才原子重命名。任何时刻看到最终文件名，它都是完整文件。
- 中断只会留下 `.rcptmp`，不会留下"名字是最终名、内容是半截"的文件；重跑时覆盖清理。
- 校验失败默认删除目标文件，`--keep-bad` 改为隔离，保证"最终文件名等同于已校验"。
- 读取与写入各是一条串行流水线，两线程之间用字节预算封顶的队列解耦，内存峰值等于 `--buffer`，与文件数和文件大小无关。
- 目录权限在复制期间临时放宽（便于写入），阶段 4 结束后恢复为源目录权限。
- 非 tty 或 `--quiet` 时不绘制进度条，逐文件即时输出。

## 测试

```bash
cargo test                                        # 56 个测试（37 单元 + 19 集成）
cargo clippy --all-targets -- -D warnings
```

集成测试覆盖：正常复制与校验、覆盖已有文件、CJK 文件名、空文件与空目录、mtime 与权限保留、符号链接跳过、串行模式等价、`--clean-stale`、重复运行幂等、目标嵌套（退出码 2）、不可读源文件（退出码 3 与 `--skip-unreadable`）、提交冲突（退出码 1）、SIGINT 中断（退出码 130 且无最终文件）。

## 实测（macOS arm64，APFS，780 MiB / 202 个文件）

- `cp -R`：0.18 s（不校验、不 fsync）。
- `rcca --no-fsync`：0.31 s（复制 + 校验，约 4 倍 I/O 量）。
- `rcca` 默认（逐文件 fsync）：1.36 s，平均吞吐约 575 MiB/s。

与 `cp -R` 直接比较总耗时不具参考价值：`cp` 既不校验也不落盘。`rcca` 的复制阶段吞吐与 `cp` 同一量级，差额来自校验读取与 fsync。

小文件场景由逐文件 fsync 主导：2001 个 4 KiB 文件，默认 9.3 s，加 `--no-fsync` 为 0.34 s（`cp -R` 为 0.25 s）。存储卡本身读速较低时该开销会被掩盖；对以大量小文件为主的目标，可用 `--no-fsync` 换取速度，代价是断电后校验结论不再可信。
