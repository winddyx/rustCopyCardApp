# rustCopyCardApp

相机存储卡复制工具，可执行文件名为 `rcca`。复制阶段交给内嵌的 [sy](https://github.com/nijaru/sy) v0.4.1 完成（镜像同步），遍历与 XXH64 双端校验由本工具完成。

## 构建

```bash
make dist               # 发行构建：先删除 dist/，再构建两个平台并生成校验和
cargo build --release   # 只构建当前平台，产物 target/release/rcca
```

sy 以源码形式内嵌在 `vendor/sy`（v0.4.1，含 3 处平台补丁，见 `vendor/sy/PATCHES.md`），构建不需要联网，也不需要另装 sy。

`make dist` 的产物：

- `dist/rcca-macos-arm64`：macOS arm64 可执行文件。
- `dist/rcca-windows-x64.exe`：Windows x64 可执行文件（PE32+，仅依赖系统 DLL）。
- `dist/SHA256SUMS`：两个产物的 SHA-256，校验用 `cd dist && shasum -a 256 -c SHA256SUMS`。

交叉编译 Windows 需要 `rustup target add x86_64-pc-windows-gnu` 与 `brew install mingw-w64`；`make dist` 内部调用 `cargo build --release --target`，先删除 `dist/` 再完整重建，不保留旧产物。

其他入口：`make test` 运行全部测试，`make lint` 执行 clippy 与格式检查，`make clean` 清理 `target/` 与 `dist/`。

## 用法

```bash
rcca <source> <target> [OPTIONS]
```

源目录内容复制到目标根；`--keep-source-name` 改为 `dst/<源目录名>/...`。

复制采用镜像语义：目标中源里不存在的文件会被删除（等价于 `sy <source>/ <target> --delete`）。与 sy 的默认值一致，当需要删除的文件超过目标文件数的 50% 时，sy 会拒绝执行并让本次运行以退出码 1 结束，此时目标保持原样。

## 选项

- `--keep-source-name`：目标布局保留源目录名。
- `--skip-unreadable`：跳过不可读的源文件，而不是预检失败。
- `--force`：目标空间不足时仍然继续。
- `--fail-fast`：首个失败即中止（映射为 sy 的 `max_errors = 1`）。
- `--quiet`：不绘制终端界面，抑制提示与 sy 的输出，只输出汇总。
- `--color auto|always|never`：进度行的颜色策略，默认 `auto`。
- `--verbose`：输出完整失败清单。
- `--keep-bad`：校验失败时把目标文件隔离为 `.<name>.badhash` 而不是删除。
- `--verify-no-cache`：校验时绕过页缓存（macOS `F_NOCACHE`、Linux `posix_fadvise`）。
- `--clean-stale`：开始前清理目标卷上遗留的 `.sy-*.tmp` 暂存文件。

## 终端界面

运行期屏幕上只有一行文字，原地刷新，不画进度条，也不显示速度与 ETA：

```
SCAN   128 个文件
COPY     23%    2.6/11.0 GiB  A001_06140008_C008.mov
VERIFY   75%    8.2/11.0 GiB  C7418.MP4
VERIFY  100%   11.0/11.0 GiB  C7418.MP4
```

- 阶段标签 `SCAN` / `COPY` / `VERIFY` 都补到 6 列，文件名从第 8 列起，切阶段不左右跳。
- 百分比按全程计算：分母是源总字节的两倍，复制占前一半、校验占后一半。复制阶段每处理完一个文件（含被跳过的）分子增长，校验阶段按实际读过的字节增长（两侧各读一遍，除以二）。全部成功时分子正好等于分母。
- 字节数分子与分母共用分母的单位，位数固定，重绘时不跳动。
- 文件名只显示文件名，超宽时从头截断保留尾部；终端过窄时依次丢掉文件名、字节数，最后只留标签与百分比。
- 标签用暗色，其余用默认色；`--color auto|always|never`，默认 `auto`（看 `NO_COLOR`、`TERM=dumb` 与 stderr 是否 TTY）。

进度写 stderr，stdout 只放汇总，所以 `rcca 源 目标 > 记录.txt` 得到的是干净文本。stderr 不是 TTY 或使用 `--quiet` 时不绘制这一行、不开线程，stderr 完全为空。

sy 自身的扫描提示、总进度条与逐文件进度条全部关闭，文件名与字节数通过 vendor 内新增的两个回调交给界面（见 `vendor/sy/PATCHES.md`）。复制固定串行（一次一个文件，对应 sy 的 `-j 1`），因此这一行始终是唯一正在处理的文件。

## 退出码

- 0：复制与校验全部成功。
- 1：复制失败（含 sy 报错）或校验失败。
- 2：用法错误（源与目标相同或互相包含、未知选项等）。
- 3：预检失败（源不可用、空间不足、FAT 系列 4 GiB 上限、源文件不可读、路径规则冲突、目标不可写）。
- 130：校验阶段被 Ctrl-C 中断。

## 行为要点

- 复制由 sy 完成：它自己负责扫描比对、传输、暂存与一次性重命名，因此目标目录里不会出现"名字是最终名、内容是半截"的文件。传输固定串行（`max_concurrent = 1`），顺序读对 U 盘与机械盘更友好，也让"当前文件"保持唯一。
- 复制阶段不拦截 Ctrl-C：信号按系统默认行为终止进程，可能残留 `.sy-xxxxxxxx.tmp`，用 `--clean-stale` 清理。校验阶段才接管 Ctrl-C，中断后退出码 130。
- 校验按 rcca 自己的清单逐个文件比对源与目标的 XXH64；失败默认删除目标文件，`--keep-bad` 改为隔离，保证"最终文件名等同于已校验"。
- 符号链接由 sy 按链接复制，但不进入校验清单（校验只覆盖普通文件）。
- 元数据：sy 保留文件的修改时间与权限位。
- 非 tty 或 `--quiet` 时不绘制进度行，只输出汇总，stderr 为空。

## 测试

```bash
cargo test                                        # 60 个测试（40 单元 + 20 集成）
cargo clippy --all-targets -- -D warnings
```

集成测试覆盖：完整复制与校验、覆盖已有文件、镜像删除、`--keep-source-name` 布局、安静模式、二次运行跳过未变文件、mtime 与权限保留、符号链接、`--clean-stale`、幂等、目标嵌套（退出码 2）、源不可读（退出码 3 与 `--skip-unreadable`）、删除阈值拒删（退出码 1）、中断后不留不完整的最终文件。

## 实测（macOS arm64，APFS，120 个 3 MiB 文件）

- 首次运行：复制 120 个文件并全部校验通过，二次运行不再传输数据（sy 报 `跳过`），校验仍然全量执行。
- 符号链接按链接复制；源文件的 mtime 与权限位在目标上保持一致。
- Windows 产物在 `x86_64-pc-windows-gnu` 下交叉编译，只依赖系统 DLL。
