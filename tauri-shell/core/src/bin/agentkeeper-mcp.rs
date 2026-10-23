//! `agentkeeper-mcp` — the stdio MCP server binary.
//!
//! Deliberately thin: everything lives in `core::mcp` so the server can be
//! exercised by `cargo test` without spawning a process.

fn main() {
    std::process::exit(agentkeeper_core::mcp::serve());
}
