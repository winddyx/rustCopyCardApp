//! rustCopyCardApp 核心库：四阶段复制、XXH64 校验、tmp 原子提交。
//!
//! 二进制入口见 `src/main.rs`，二进制名为 `rcca`。

pub mod cli;
pub mod commit;
pub mod copy;
pub mod error;
pub mod paths;
pub mod pipeline;
pub mod report;
pub mod scan;
pub mod stats;
pub mod ui;
pub mod verify;
