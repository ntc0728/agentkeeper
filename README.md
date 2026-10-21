# AgentKeeper

统一管理 AI 编码 Agent 资产的本地控制平面：**发现 → 校验 → 编辑 → 分发 → 审计 → 回滚**。

覆盖 5 类资产 × 17 个 Agent × 全局/项目两级作用域。桌面壳是 **Tauri 2**，
业务逻辑全部在 **Rust**（`tauri-shell/src-tauri/src/core/`），前端是同一份零依赖原生 DOM。

---

## 环境要求

| 依赖 | 说明 |
|---|---|
| Rust stable（`x86_64-pc-windows-msvc`） | 编译 `tauri-shell/src-tauri`。Windows 上还需要 **MSVC 生成工具 + Windows SDK** —— rustup 本身不装这两个 |
| WebView2 Runtime | Windows 11 自带。没装的话窗口不会渲染，见下方故障排查 |
| Node.js | **只**用来跑几个纯 `fs`/`path` 的辅助脚本（前端同步、lint、i18n 报告、parity 对齐）。**运行应用本身不需要 Node** |

rustup 装的时候带了 `--no-modify-path`，所以**新开的终端里 `cargo` 常常不在 PATH**。
先接线环境变量：

```bash
source scripts/rust-env.sh        # Git Bash / bash
.\scripts\rust-env.ps1            # PowerShell / CMD
```

`cargo tauri info` 应能同时识别 rustc / MSVC / WebView2 三项。已装组件与磁盘占用见
[`docs/RUST-ENV.md`](docs/RUST-ENV.md)。

## 快速开始

```bash
npm install                 # 只给 Node 侧的校验/对齐脚本用

npm run tauri:sync          # src/renderer → tauri-shell/dist（幂等，可先 --check）
cd tauri-shell/src-tauri
cargo tauri dev             # 开发运行（等价的裸命令是 `cargo run`，default-run 已锁定 GUI）
cargo tauri build           # 出安装包 → target/release/bundle/nsis/
```

`tauri:sync` 不可跳过：Tauri 把 `tauri-shell/dist` 编进 exe，而 `dist` 是从
`src/renderer` 生成的，中间只有两处受控差异（`<html data-shell="tauri">` 与
`tauri-bridge.js`）。`npm run tauri:check` 会在两者不一致时报错。

### 安装包

| | |
|---|---|
| 产物 | `tauri-shell/src-tauri/target/release/bundle/nsis/AgentKeeper_<ver>_x64-setup.exe` |
| 体积 | 3.7 MiB（Electron 版同名安装包 69.5 MiB） |
| 安装模式 | `currentUser` —— 装到 `%LOCALAPPDATA%\AgentKeeper`，**不需要管理员权限** |
| 安装后占用 | 15.9 MiB：shell + `agentkeeper-mcp.exe` + `ak-scan.exe` + `uninstall.exe`（`ak-store.exe` 已于 2026-10-20 随迁移期 parity harness 移除） |
| 前端资源 | 编进 exe（安装目录里没有 `dist/`） |
| 示例项目 | **不再随包发布**（原 `demo-projects/` 已移出 `bundle.resources`）；示例内容见 `scripts/fixtures/`，仅供测试 |
| 本机实测 | 冷启动到窗口可见 0.76–0.93 s；shell 常驻 69 MB，整棵进程树（含 WebView2）约 480 MB |

静默安装：`AgentKeeper_<ver>_x64-setup.exe /S /D=<绝对路径>`（`/D=` 放最后、不加引号）。

### 本机的 WebView2 硬性约束

这台机器无法初始化 Chromium OS 沙盒，WebView2 若不加 `--no-sandbox --disable-gpu-sandbox`
就**不渲染**（`render-process-gone` / `STATUS_BREAKPOINT`）。`src/lib.rs` 的 `run()` 在
webview 创建前无条件设置 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`，release 包同样生效
（若用户已显式设置该变量则不覆盖）。
安全性不受影响：前端无 Node 集成、CSP 锁定、插件只授予 dialog / opener。

### 故障排查

| 现象 | 处置 |
|---|---|
| `cargo: command not found` | 先 `source scripts/rust-env.sh`（或 PowerShell 跑 `.\scripts\rust-env.ps1`） |
| `linker 'link.exe' not found` | MSVC 环境变量没接上，同上；确认装了"使用 C++ 的桌面开发"或 VS 生成工具 |
| 窗口一片空白 / 进程起了但没界面 | 本机 WebView2 沙盒起不来，确认 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 已带 `--no-sandbox --disable-gpu-sandbox`（见上一节）；或确认装了 WebView2 Runtime |
| 改了前端但界面没变 | 忘了 `npm run tauri:sync`；`npm run tauri:check` 会告诉你 `dist` 是否过期 |
| 依赖下载卡住 | 已配 rsproxy.cn 稀疏索引（见 `docs/RUST-ENV.md`），若换机器需重配 |
| 写入"没生效" | 沙盒默认开启，写入只进影子层，需在界面点「应用」；或用 `AGENTKEEPER_SANDBOX=0` 启动 |
| store 与桌面应用对不上 | MCP 侧与 GUI 侧必须指向同一个 `AK_STORE_ROOT` |

---

## 验证

```bash
cd tauri-shell/src-tauri
cargo test                       # 单元 + 集成；断言数随覆盖增长，以实际输出为准
cargo test --test mcp            # 真实进程级 MCP 协议回归
```

前端与桥接的静态门禁（纯 Node，不启浏览器，全量 ≈0.5 s）：

```bash
npm run check                    # 残留扫描 · 桥接↔Rust 命令契约 · renderer 规则 · UI 审计 · i18n · dist 一致性
npm run tauri:i18n               # 只跑 Rust 引擎文案的中文覆盖门禁（CI 在构建前跑这条）
node scripts/check.js bridge     # 或者单独跑某一道：residue / bridge / renderer / ui / i18n / dist
```

> 门禁只做静态一致性检查：残留扫描、桥接↔Rust 命令签名对账、渲染层 i18n、UI 规则、dist 一致性。
> 行为正确性由 Rust 测试负责（`cargo test`），界面质量不进入 CI —— 桌面端的真机验证需要
> WebView2 与交互式桌面，跑不进 Actions，因此维护者的真机核验工具不进本仓库。


