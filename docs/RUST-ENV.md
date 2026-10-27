# Rust / Tauri 构建环境

本文件说明如何在一台干净的 Windows 机器上把 **Rust + MSVC + WebView2** 接起来构建
AgentKeeper。脚本只做环境变量接线，不重复安装已存在的组件。

## 前置组件

| 组件 | 要求 |
|---|---|
| Rust | stable，target `x86_64-pc-windows-msvc`（`rustup` 安装） |
| tauri-cli | `cargo install tauri-cli --version "^2"` |
| MSVC Build Tools | Visual Studio 生成工具 2022，工作负载「使用 C++ 的桌面开发」 |
| Windows SDK | 随 Build Tools 安装即可（10.0.22621 或更新） |
| WebView2 Runtime | Windows 11 自带；Windows 10 需单独安装 Evergreen Runtime |

安装完成后的典型占用：Build Tools ≈ 3.4 GB，Windows SDK ≈ 3.4 GB。

## 关键前提：PATH 与 MSVC 变量

`rustup-init -y` 若带 `--no-modify-path`（或安装发生在非交互会话里），**用户 PATH 里不会有
`.cargo\bin`**，新终端直接敲 `cargo` 会报「找不到命令」。

同时 **rustc 的 msvc target 不会自动搜索 VS 安装目录** —— 不设 `INCLUDE` / `LIB` 就会报
`linker link.exe not found`。

因此每次开新终端先接线：

```bash
source scripts/rust-env.sh        # Git Bash / bash
.\scripts\rust-env.ps1            # PowerShell
```

CMD 不能执行 `.ps1`：`call .\scripts\rust-env.ps1` 退出码为 0，但 `cargo` 仍不在 `PATH` 上
（`where cargo` 会返回 1）。CMD 用户请先开 PowerShell，跑完接线脚本再在同一窗口继续。

之后 `cargo build` / `cargo test` / `cargo tauri info` 均可直接使用。

验证：

```bash
cargo tauri info
```

应同时识别 WebView2 / MSVC / rustc 三项。

## cargo 镜像（国内网络）

`%USERPROFILE%\.cargo\config.toml`（**单层路径，不是 `.cargo\.cargo\config.toml`**）指向
rsproxy.cn 稀疏索引。这是**本机用户级配置，不在仓库里**——`tauri-shell/.cargo/config.toml`
只钉 `[build] incremental = false`，没有 `source` 替换。换一台机器要自己配，否则 Tauri 依赖的
数百个 crate 会走官方源，构建长时间无进展。

判断镜像是否真的生效要看**冷启动拉一个未缓存 crate**：用已缓存的依赖构建成功，不能说明镜像可用。
需要绕过镜像时：

```bash
CARGO_SOURCE_CRATES_IO_REPLACE_WITH="" cargo build
```

## 常见问题

| 现象 | 原因与处置 |
|---|---|
| `cargo: command not found` | PATH 没有 `.cargo\bin`，先跑上面的接线脚本 |
| `linker 'link.exe' not found` | MSVC 环境变量未接上，同上；确认装了「使用 C++ 的桌面开发」 |
| `Get-PSDrive` 报「驱动器不存在」 | 某些精简环境缺少驱动器枚举，判断路径改用 `Test-Path` |
| PowerShell 5.1 报 `ParserError` | 5.1 不支持 `$(try{...}catch{...})` 内联，把逻辑写进 `.ps1` 文件执行 |
| `Directory.Delete(path, true)` 删不掉文件 | 它只删目录；删文件用 `File.Delete` |
| VS 引导程序返回 3010 | 表示「成功，建议重启」，不是失败；无需重启即可继续 |
| VS 引导程序要求提权 | 它会自提权，从非管理员会话启动也能装成功，不必强行 `runas` |
| `cargo install tauri-cli` 很慢 | 首次从源码编译（16 核约 5 分钟），后续增量构建快得多 |

## 卸载

```powershell
# 1. MSVC Build Tools：通过 VS Installer 卸载
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe"

# 2. Rust
& "$env:USERPROFILE\.cargo\bin\rustup.exe" self uninstall

# 3. 清理镜像配置
Remove-Item "$env:USERPROFILE\.cargo\config.toml" -Force
```
