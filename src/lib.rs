//! rustCopyCardApp 的核心库，crate 名为 `rcca`。
//!
//! 分工：真正搬运字节的工作交给内嵌的 sy 引擎（`vendor/sy`），本 crate 负责前置检查
//! （preflight）、目录遍历与清单、XXH64 校验、单行进度、结果汇总和退出码映射。
//!
//! 二进制入口在 `src/main.rs`，产物名同为 `rcca`。`Cargo.toml` 用 `[lib]` 与 `[[bin]]`
//! 两段分别声明库和可执行文件，两者同名不冲突，因为它们参与链接的方式不同。
//!
//! `main.rs` 里写的是 `use rcca::...`，也就是二进制把本库当成外部 crate 引用；`tests/`
//! 下的集成测试同样如此。因此凡是需要被 CLI 用到的逻辑都必须 `pub`，只在库内部
//! 使用的东西保持私有即可。
//!
//! 模块顺序与运行流程一致：解析参数、做前置检查、复制、校验、显示、汇总。

// 模块声明放在文件最外层。`pub mod cli;` 会让编译器去找 `src/cli.rs`（或 `src/cli/mod.rs`）。
// 若写成 `mod cli;`，模块只在 crate 内部可见，`main.rs` 就无法写 `rcca::cli::Cli`。
// 注意：模块声明处的可见性只决定"模块本身能否被外部访问"，模块里的条目还需各自 `pub`。
// 下面每个模块的 `///` 是文档注释，会出现在 `cargo doc` 生成的文档里。

/// 命令行参数解析。用 clap 的 derive 方式，结构体字段即选项。
pub mod cli;
/// 输出设备准备：Windows 控制台的 UTF-8 代码页切换与恢复。
pub mod console;
/// 复制阶段。把选项映射到内嵌 sy 引擎的配置，自己不动字节。
pub mod engine;
/// 错误类型与退出码映射。
pub mod error;
/// 路径规则：相对路径类型、临时文件名、嵌套与长度检查。
pub mod paths;
/// 运行结束后的结果汇总，输出到 stdout。
pub mod report;
/// 源目录遍历与清单生成，含复制后的稳定性复核。
pub mod scan;
/// 跨线程共享的运行状态与统计计数器。
pub mod stats;
/// stderr 上的单行进度。
pub mod ui;
/// 校验阶段：XXH64 计算与比较。
pub mod verify;
