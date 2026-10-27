// Tauri 构建脚本。
//
// `tauri_build::build()` 会把 `bundle.externalBin` 中声明的 sidecar
// 复制到编译产物旁边，一旦其中有缺失就会直接构建失败，报的是 Tauri 那句原文
//（引号里是找不到的那个资源路径，意为"资源路径不存在"）：
//
//   resource path `binaries\agentkeeper-mcp-x86_64-pc-windows-msvc.exe` doesn't exist
//
// 两个 sidecar 分别是 MCP 服务器和无头扫描器。它们位于
// `agentkeeper-core` crate ——刻意放在另一个 crate 里，这样它们就不会
// 继承 WebView 的依赖图——由 `tauri-shell/stage-core-bins.js`
// 构建并暂存。
//
// 因此这是一个先后顺序要求，而不是循环依赖：
//
//   cargo test / cargo build（shell）   →  需要 sidecar 已经暂存
//   stage-core-bins.js                 →  只构建 core crate
//
// core crate 没有构建脚本，所以暂存它永远不需要 shell
// 已经编译完成。克隆之后运行一次 `npm run tauri:core-bins`，之后 shell
// 就能正常编译。
//
// 下面的检查把 tauri 简略的错误信息变成可据以行动的提示。它
// 不是绕过任何东西：如果某个 sidecar 真的缺失，构建仍然必须
// 失败。

use std::path::Path;

/// 用户需要执行什么，以及原因。
const HINT: &str = "\
error: the MCP server / headless scanner sidecars are not staged.

They are built from the `agentkeeper-core` crate by:

    npm run tauri:core-bins

(that script builds only the core crate — it does not need the shell compiled,
so there is no circular dependency). `npm run tauri:bundle` does this for you.

To see the original failure, run cargo again with the sidecars present.";

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let binaries = Path::new(&manifest_dir).join("binaries");

    // 只检查本 crate 配置里真正声明过的 sidecar，这样
    // 提示信息绝不会指向一个并不需要的文件。
    let declared = ["agentkeeper-mcp", "ak-scan"];
    let missing: Vec<String> = declared
        .iter()
        .map(|name| {
            let file = binaries.join(format!("{name}-x86_64-pc-windows-msvc.exe"));
            if file.exists() {
                None
            } else {
                Some(format!("  missing: {}", file.display()))
            }
        })
        .flatten()
        .collect();

    if !missing.is_empty() {
        for line in &missing {
            println!("cargo:warning={line}");
        }
        println!("{HINT}");
        // 以一条人能据以行动的提示失败，而不是日志更下方
        // tauri 那句 `resource path ... doesn't exist`。
        std::process::exit(1);
    }

    tauri_build::build()
}
