// 避免 release 构建在 Windows 上额外弹出一个控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    agentkeeper_shell_lib::run()
}
