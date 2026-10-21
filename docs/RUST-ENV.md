# AgentKeeper Rust/Tauri 构建环境

> 生成日期：2026-10-19
> 用途：在**新开的终端**里恢复 Rust + MSVC 构建环境。已安装组件不会重复安装，本脚本只做环境变量接线。

## 已安装组件

| 组件 | 版本 | 位置 |
|---|---|---|
| Rust | 1.99.1 (stable-x86_64-pc-windows-msvc) | `%USERPROFILE%\.cargo` / `%USERPROFILE%\.rustup` |
| rustup | 1.29.1 | `%USERPROFILE%\.cargo\bin\rustup.exe` |
| tauri-cli | 2.12.3 | `%USERPROFILE%\.cargo\bin\cargo-tauri.exe` |
| MSVC Build Tools | Visual Studio 生成工具 2022 | `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools` |
| MSVC 编译器 | 14.44.35207 | `...\VC\Tools\MSVC\14.44.35207` |
| Windows SDK | 10.0.26100.0（+ 10.0.22621.0） | `C:\Program Files (x86)\Windows Kits\10` |
| WebView2 Runtime | 155.0.4283.62 | `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\155.0.4283.62` |

**合计占用约 6.7GB**（BuildTools 3.35GB + Windows SDK 3.35GB）。

## 关键前提：PATH 未自动生效

`rustup-init -y` 带了 `--no-modify-path`，且安装发生在非交互会话里，**用户 PATH 里没有 `.cargo\bin`**。新开终端直接敲 `cargo` 会报"找不到命令"。

同时 **rustc 的 msvc target 不会自动搜索 VS 安装目录** —— 不设 `INCLUDE`/`LIB` 就会报 `linker link.exe not found`。

## 用法

**Git Bash：**
```bash
source scripts/rust-env.sh
```

**PowerShell / CMD：**
```powershell
.\scripts\rust-env.ps1
```

之后 `cargo tauri build` / `cargo test` / `cargo tauri info` 均可直接使用。

验证：
```bash
cargo tauri info
```
应输出 WebView2 / MSVC / rustc 三项均识别成功。

## cargo 国内镜像已配置

`%USERPROFILE%\.cargo\config.toml`（**单层，不带 `.cargo\.cargo`**）已指向 **rsproxy.cn 稀疏索引**。
没有它，Tauri 依赖的数百个 crate 会走官方源，构建会卡到无法完成。

实测：首次全量 `cargo build --release`（Tauri + serde/sha2/toml/yaml/rayon 等）**181.8s**，
构建无网络卡顿。

> ⚠️ **路径纠正（阶段 1 发现）**：本文件早期版本写的是 `~/.cargo/.cargo/config.toml`，**是错的**。
> cargo 只读 `~/.cargo/config.toml`。之前"镜像验证通过"其实用的是已缓存的 crate ——
> 需要新依赖时会立刻暴露 502。**验证镜像必须冷启动拉一个新 crate。**

若需临时绕过镜像（例如拉取 rsproxy 未同步的包）：
```bash
CARGO_SOURCE_CRATES_IO_REPLACE_WITH="" cargo build
```

## 卸载（回滚用）

```powershell
# 1. 卸载 MSVC Build Tools（通过 VS Installer）
#    "%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" 查路径，
#    或直接运行 C:\Program Files (x86)\Microsoft Visual Studio\Installer\setup.exe

# 2. 卸载 Rust
& "$env:USERPROFILE\.cargo\bin\rustup.exe" self uninstall

# 3. 清理 cargo 镜像配置
Remove-Item "$env:USERPROFILE\.cargo\config.toml" -Force
```

## 踩坑记录

1. **`Get-PSDrive` 在此环境报"驱动器不存在"** —— 判断路径请用 `Test-Path`。
2. **PowerShell 5.1 不支持 `$(try{...}catch{...})` 内联**（ParserError），复杂逻辑写成 `.ps1` 文件执行。
3. **`Directory.Delete(path, true)` 不能删文件** —— 删文件必须用 `File.Delete`。
4. **VS 引导程序返回 3010 不代表失败** —— 那是"成功，建议重启"。实测无需重启，`link.exe`/`cl.exe` 立即可用。
5. **VS 引导程序会自提权** —— 即使从非管理员会话启动也能装成功，不必强行 `runas`。
6. **`cargo install tauri-cli` 需从源码编译** —— 16 核约 5 分钟。后续增量快得多。