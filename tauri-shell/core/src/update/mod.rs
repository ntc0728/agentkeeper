//! `core/update/*` — 软件更新子系统。
//!
//! 从执行 IO 的模块中拆分出来，好让纯逻辑部分（版本比较、发布选择、
//! 下载加固）无需网络或文件系统即可测试。`version.rs` 除了标准库加
//! `serde_json` 之外没有别的依赖；`http.rs` 负责传输，
//! `manager.rs` 负责其上的状态机。
pub mod http;
pub mod manager;
pub mod token;
pub mod version;
