# AgentKeeper

[English](README.md) | **简体中文**

> 一个本地控制平面，统一管理 AI 编码 Agent 的资产 —— 发现、校验、编辑、分发与审计
> **skills / MCP 服务 / 插件 / 规则 / 记忆库**，覆盖全局与项目两级作用域。

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![platform: Windows](https://img.shields.io/badge/platform-Windows-0078D4.svg)](#环境要求)
[![shell: Tauri 2](https://img.shields.io/badge/shell-Tauri%202-24C8DB.svg)](https://tauri.app/)
[![core: Rust](https://img.shields.io/badge/core-Rust-B7410E.svg)](tauri-shell/src-tauri)

5 类资产 × 17 个 Agent × 2 级作用域，一个窗口管完。桌面壳是 **Tauri 2**，
业务逻辑是 **Rust**（`tauri-shell/core/src/`），界面是零依赖原生 DOM
（`src/renderer/`），没有构建步骤。

---

## 为什么要它

Agent 的行为不只来自模型，更来自它启动前静默读入的文件：

| 资产 | 载体 | 失效方式 |
|---|---|---|
| Skill | `SKILL.md` + YAML frontmatter | 缺 `description` → 永不触发；目录名 ≠ `name` → 缓存键错位 |
| MCP 服务 | `mcp.json` / `.mcp.json` / `config.toml` | 缺 `command`/`url` → 连接失败；改写时吞掉兄弟 server |
| 插件 | `.codebuddy-plugin/plugin.json` 等 | 版本目录错位；缓存被误当作 marketplace |
| 规则 | `AGENTS.md` / `CLAUDE.md` / `.mdc` | 空文件照样烧 token；被两个 agent 重复加载 |
| 记忆 | `MEMORY.md` + `memory/YYYY-MM-DD.md` | 陈旧、冲突、无审计 |

这些文件散落在 `~/.claude`、`~/.codex`、`~/.cursor`、`~/.workbuddy` 以及每个仓库的隐藏目录里。
AgentKeeper 把它们变成**可见、可编辑、可校验、可追溯**的一个视图。

## 功能

- **发现** —— 扫描 17 个 Agent 加上共享的 `~/.agents/skills` 目录；全局与项目两级作用域。
- **看清生效态** —— 哪一份真正生效、项目作用域遮蔽了哪一份。
- **校验** —— frontmatter、JSON/JSONC、TOML、YAML 检查；对 MCP 条目做安全检查。
- **安全编辑** —— 写入直接落到真实 agent 目录，走事务：先把被触及路径的当前内容读入内存，再原子写、
  记日志。中途失败的写入会按这份快照还原；连快照都读不全的写入，会在**动第一个字节之前**被拒绝。
- **写入前二次确认** —— 新增 / 修改 / 删除都会先弹确认框，并列出即将写入的真实路径。
  设置页除外：设置写的是应用自己的 store，不碰 agent 目录。
- **分发** —— 把 skill / rule / memory / MCP 条目复制到其他 Agent 或已注册项目。
- **审计** —— 只追加的操作日志，加上报告静默改动的基线指纹。日志是「改了什么」的记录，
  不是撤销菜单：本版本不提供按事务回滚。
- **可无头运行** —— 同一套核心同时是一个 15 工具的 stdio MCP server（`agentkeeper-mcp`），
  Agent 走同一条「校验 → 快照 → 写入 → 日志」路径管理自己的资产。

## 环境要求

| 依赖 | 说明 |
|---|---|
| Rust stable（`x86_64-pc-windows-msvc`） | 编译 `tauri-shell/src-tauri`；需要 MSVC 生成工具 + Windows SDK |
| WebView2 Runtime | Windows 11 自带 |
| Node.js | 仅用于 `fs`/`path` 辅助脚本（前端同步、lint、i18n 闸门）。运行应用本身不需要 Node |

`rustup` 常以 `--no-modify-path` 安装，`.cargo\bin` 不在用户级也不在机器级 `PATH` 上，新终端里
根本找不到 `cargo`。按你的终端选一行：

```bat
:: CMD —— 每个窗口跑一次（想一次性永久配置见下）
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
```

```bash
source scripts/rust-env.sh        # Git Bash / bash
.\scripts\rust-env.ps1            # PowerShell
```

CMD 执行不了 `.ps1`，用 `for /f` 之类包一层也只会抄走它的输出、不会导出任何变量；真正让 `cargo`
可用的就是上面那句 `set`。而且 CMD 只需要这一句：`cargo build` / `cargo tauri` 会通过注册表自行
找到 MSVC 链接器，不需要 `INCLUDE` / `LIB`（已用只加这一句的完整 `--release` 链接验证过）。

想一劳永逸，在 **PowerShell**（不是 CMD）里跑一次：

```powershell
$bin = Join-Path $env:USERPROFILE '.cargo\bin'
$old = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($old -notlike "*$bin*") {
  [Environment]::SetEnvironmentVariable('PATH', "$old;$bin", 'User')
}
```

之后开新窗口生效。别用 `setx PATH ...`：它会把现有值里的 `%USERPROFILE%` 展开写死（本机注册表里
是 `REG_EXPAND_SZ` 且确实含它），且超过 1024 字符会静默截断。

`cargo tauri info` 应能识别 rustc、MSVC、WebView2 三项。已装组件与磁盘占用见
[`docs/RUST-ENV.md`](docs/RUST-ENV.md)。

## 从源码构建

日常开发：

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:sync
cd tauri-shell\src-tauri
cargo tauri dev
```

出包 —— **一条命令、两个产物、一个显眼的目录**：

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:bundle
```

```
release/
  AgentKeeper_<ver>_x64-setup.exe        安装版（NSIS）
  AgentKeeper_<ver>_x64-portable.zip     免安装版
```

`npm run tauri:bundle` 依次执行 `tauri:sync` → `tauri:core-bins` → `cargo tauri build` → 归档，把两个产物复制到
仓库根目录的 `release/`，并逐字节校验副本；任一产物缺失即非零退出。`--no-build` 只归档
`target/release` 里已有的产物，`--portable-only` 跳过安装版。

免安装包内含 `agentkeeper-shell.exe`、`agentkeeper-mcp.exe`、`ak-scan.exe` 与一份简短的
`README-PORTABLE.txt`；解压即用，不写注册表、不建快捷方式、无卸载项（应用本身完全不碰
Windows 注册表）。两种形态共用 `%APPDATA%\AgentKeeper` 存设置，想让两份互不干扰就设
`AK_STORE_ROOT`。

MCP server 与无头扫描器在独立的 crate `tauri-shell/core` 里，**零 Tauri 依赖** —— 因此它们只链接
114 个 crate 而不是 311 个，绝不会把用不到的 WebView 栈拖进来。`cargo tauri build` 只看得到它
自己那个 crate 的二进制，所以这两个由 `npm run tauri:core-bins` 暂存为 `bundle.externalBin` 的
sidecar（`tauri:bundle` 会自动执行这一步）。

也正因为如此，sidecar 这条路才走得通。当年这两个二进制是 Tauri crate 自己的 `[[bin]]`，
于是形成循环依赖：`tauri-build` 作为该 crate 的 build script 对**每个** target 都会执行，
干净的 `target/` 永远无法先产出它自己要求的 sidecar。core crate 没有 build script，
sidecar 由一个不关心这件事的东西产出。

没有 npm 依赖需要安装 —— `package.json` 只声明辅助脚本。

`tauri:sync` 不能跳过：Tauri 把 `tauri-shell/dist` 编进可执行文件，而 `dist` 由
`src/renderer` 生成，中间只有两处受控差异（`<html data-shell="tauri">` 与额外的
`tauri-bridge.js`）。`npm run tauri:check` 会在两者不一致时报错。

`dist/` **不入库**——它是构建产物，入库的副本可能悄悄比 `src/renderer` 旧（构建不校验新鲜度，
只把 `dist/` 里现有的东西编进去）。因此 `tauri.conf.json` 把 `copy-frontend.js` 配成
`beforeBuildCommand` / `beforeDevCommand`：`cargo tauri build` 和 `cargo tauri dev` 每次都会
重新生成它。全新 clone 没有 `dist/` 也能正常构建——这条命令会把它建出来。

只想校验不想写盘，用 `npm run tauri:check`（或 `npm run tauri:sync -- --check`）。裸写
`npm run tauri:sync --check` 等于不带参数跑 `tauri:sync` —— npm 只转发 `--` 之后的参数，
`--check` 会被丢掉，`dist` 照样被重写。

### 发布产物

| | 安装版 | 免安装版 |
|---|---|---|
| 文件 | `release/AgentKeeper_<ver>_x64-setup.exe` | `release/AgentKeeper_<ver>_x64-portable.zip` |
| 体积 | 3.86 MiB | 7.81 MiB（解压 21.3 MiB） |
| 安装方式 | `currentUser` → `%LOCALAPPDATA%\AgentKeeper`，无需管理员 | 不解压安装，解到任意目录跑 `agentkeeper-shell.exe` |
| 占用 | 安装后 21.4 MiB | 目录 21.3 MiB |
| 卸载 | `uninstall.exe /S` | 删目录 |
| 前端资源 | 编进可执行文件（磁盘上没有 `dist/`） | 同 |
| 冷启动到窗口可见 | 45–180 ms | 171–178 ms |

静默安装：`AgentKeeper_<ver>_x64-setup.exe /S /D=<绝对路径>`（`/D=` 放最后，不加引号）。

两个产物来自同一次构建；把 zip 一起发布不会干扰自动更新 —— `core/update/version.rs` 会用
两条独立规则丢弃它（`-?portable\b` 与 `.zip` 归档规则），只有 NSIS 安装包会被当作更新。

## 架构

```
src/renderer/            原生 DOM 界面（index.html · app.js · styles.css · i18n.js · engine-zh.js）
tauri-shell/dist/        src/renderer 的生成副本，被编进可执行文件（不入库）
tauri-shell/             Rust workspace
  core/                  agentkeeper-core —— 零 Tauri 依赖
    src/                 scan · assets · registry · adapters · store · keeper · mcp · security · update
    src/bin/             agentkeeper-mcp（stdio MCP server）· ak-scan（无头扫描）
  src-tauri/             agentkeeper-shell —— Tauri 应用
    src/lib.rs           #[tauri::command] 面 + 窗口装配
scripts/                 check · lint-renderer · ui-audit · tauri-i18n · adapter-table · package-release
release/                 出包目录：安装版 + 免安装 zip（已 git-ignore）
docs/                    DESIGN.md · UPDATE.md · MCP-SERVER.md · RUST-ENV.md
```

`core::` 不认识 Tauri 类型，因此同一套逻辑有三个入口：GUI、MCP server 与 `ak-scan`。

### 安全模型

- 前端没有 Node 集成，只能调用 `tauri-bridge.js` 暴露的具名函数。
- 两层 CSP（webview 配置 + 更严的文档级 meta）；文档层 `connect-src 'none'`。
- 路径一律从注册表反查，渲染层不传裸路径（唯一例外是「导入项目」，必须是已存在目录）。
- 写入直接作用于真实 agent 目录，走事务管线：校验 → 内存快照 → 原子写 → 日志。中途失败的
  写入按快照还原，快照不保留在磁盘上。
- 界面显示什么就写什么：不存在把写入偷偷重定向到别处的开关，也不存在「先放着、稍后落盘」
  的中间态 —— 要么被校验拦下，要么直接写进真实目录。
- 新增 / 修改 / 删除一律二次确认，确认框列出真实目标路径。设置页除外 —— 设置只写应用自己的 store。
- Tauri 插件只授予 `dialog`（选目录）与 `opener`（在资源管理器中显示）。

## 测试

```bash
npm test                  # 只跑改动能影响的测试 —— 迭代时的默认
npm run test:all          # 全部测试（333 个），提交前跑
npm test -- --list        # 打印将要跑什么，不执行
```

`npm test` 读 `git status`，挑出改动可能打破的测试目标；`npm run test:all` 跑全部。改什么该跑哪个
脚本、各脚本的作用，见 [`docs/BUILDING.md`](docs/BUILDING.md)。

直接跑全量：

```bash
cd tauri-shell
cargo test --workspace             # 单元 + 集成，两个 crate 都跑
cargo test -p agentkeeper-core --test mcp   # 真实 stdio MCP 协议回归
```

前端与桥接的静态闸门（纯 Node，不启浏览器，≈0.5 s）：

```bash
npm run check                    # residue · bridge · tree · renderer · ui · i18n · dist（8 道闸）
npm run tauri:i18n               # Rust 引擎文案的中文覆盖（CI 在构建前跑）
node scripts/check.js bridge     # 只跑一道：residue / bridge / tree / renderer / ui / i18n / dist
```

行为正确性由 Rust 测试负责。视觉与键盘验收需要真实 WebView2 和交互式桌面，因此在 CI 之外执行。

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | 问题陈述、架构、写入管线、适配器表、UI 模型 |
| [`docs/UPDATE.md`](docs/UPDATE.md) | 更新流水线：release 布局、签名、代理、安装器 |
| [`docs/MCP-SERVER.md`](docs/MCP-SERVER.md) | 15 个 MCP 工具及 schema |
| [`docs/RUST-ENV.md`](docs/RUST-ENV.md) | 本机工具链、组件、磁盘占用 |
| [`docs/BUILDING.md`](docs/BUILDING.md) | 改什么跑哪个脚本；两个 build profile 的分工与依据 |

## 故障排查

| 现象 | 处置 |
|---|---|
| `cargo: command not found` | `source scripts/rust-env.sh`（bash）或 `.\scripts\rust-env.ps1`（PowerShell） |
| `resource path binaries\agentkeeper-mcp-x86_64-pc-windows-msvc.exe doesn't exist` | sidecar 没暂存 —— `cargo tauri build` 前先跑 `npm run tauri:core-bins` |
| `npm run tauri:sync --check` 还是重写了 `dist/` | npm 会丢弃 `--` 之前的参数；改用 `npm run tauri:check` |
| `linker 'link.exe' not found` | MSVC 环境未接上，同上；确认装了「使用 C++ 的桌面开发」 |
| 窗口不出现 | 宿主无法初始化 Chromium OS 沙盒；`src/lib.rs` 在创建 webview 前设置 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`（`--no-sandbox --disable-gpu-sandbox`） |
| 改了前端但界面没变 | 忘了 `npm run tauri:sync`；`npm run tauri:check` 会报告 `dist` 过期 |
| 依赖下载卡住 | 镜像配在 `%USERPROFILE%\.cargo\config.toml`，不在仓库里，见 `docs/RUST-ENV.md` |
| 写入「没生效」 | 到「操作记录」看这笔事务：`error` 行表示被校验拦下，磁盘上什么都没写 |
| MCP 与 GUI 对不上 | 两侧必须指向同一个 `AK_STORE_ROOT` |

## 参与贡献

- 欢迎 issue 与 PR，一个 PR 只做一件事。
- 前端只改 `src/renderer/` —— `tauri-shell/dist/` 是生成物（不入库），每次 `cargo tauri build` / `cargo tauri dev` 都会重建。
- 新增文案需要中英两版：`src/renderer/i18n.js`（界面）与 `src/renderer/engine-zh.js`
  （Rust 引擎句子），否则 `npm run check` 会失败。
- 新增 Agent 要改两处：`core/adapters.rs` 与 `scripts/adapter-table.js`，然后跑
  `npm run tauri:adapters`。
- 仓库只收产品文件：不放审计/计划类文档、本地绝对路径、调试产物。

## 许可证

MIT —— 见 [`LICENSE`](LICENSE)。

Copyright (c) 2026 AgentKeeper.
