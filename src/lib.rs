//! rustCopyCardApp 核心库：复制交给内嵌的 sy 引擎，遍历与 XXH64 校验由本工具完成。
//!
//! 二进制入口见 `src/main.rs`，二进制名为 `rcca`。

pub mod cli;
pub mod engine;
pub mod error;
pub mod paths;
pub mod report;
pub mod scan;
pub mod stats;
pub mod ui;
pub mod verify;
