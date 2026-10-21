//! rustCopyCardApp core library: copying is delegated to the embedded sy engine,
//! traversal and XXH64 verification are done here.
//!
//! The binary entry point is `src/main.rs`, the binary is named `rcca`.

pub mod cli;
pub mod engine;
pub mod error;
pub mod paths;
pub mod report;
pub mod scan;
pub mod stats;
pub mod ui;
pub mod verify;
