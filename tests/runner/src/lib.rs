//! test-runner 的库面——存在的唯一理由是让 `tests/*.rs` 集成测试能直接调用
//! `fixture`/`config` 等模块（不重复实现一遍拷贝/占位符替换逻辑）。
//! 真正的入口仍是 `src/main.rs` 的二进制。

pub mod api_runner;
pub mod cli_runner;
pub mod comparator;
pub mod config;
pub mod fixture;
pub mod mutations;
pub mod plugin_fixture;
pub mod reporter;
pub mod rerun;
pub mod script;
pub mod script_session;
pub mod scripted_model;
