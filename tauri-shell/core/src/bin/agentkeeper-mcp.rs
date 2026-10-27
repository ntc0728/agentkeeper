//! `agentkeeper-mcp`——走 stdio 的 MCP 服务器二进制。
//!
//! 刻意做得很薄：所有东西都在 `core::mcp` 里，这样服务器能被 `cargo test`
//! 直接跑到，而不必另起一个进程。

fn main() {
    std::process::exit(agentkeeper_core::mcp::serve());
}
