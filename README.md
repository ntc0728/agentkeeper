# AgentKeeper

[**简体中文**](README.md) · [English](README.en.md)

![license](https://img.shields.io/badge/license-MIT-blue.svg)
![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D4.svg)

一个本地控制台，统一管理 AI 编码 Agent 的 **技能 / MCP 服务 / 插件 / 规则 / 记忆文件**。
5 类资产 × 17 种 Agent × 全局与项目两级作用域，一个窗口看完。

桌面壳是 Tauri 2，业务逻辑在 Rust（`tauri-shell/core`），界面是零依赖原生 DOM（`src/renderer`），没有前端构建步骤。

## 功能

- **发现** — 扫描 17 种 Agent 与共享技能目录，区分全局与项目作用域
- **看清生效状态** — 同名资产哪一份真正生效，被哪一层遮蔽
- **校验** — frontmatter / JSON / TOML / YAML 检查，MCP 安全扫描（提示注入、凭据外泄）
- **安全写入** — 写前快照 → 原子写 → 中途失败整笔还原；每次写入先弹确认框并列出真实路径
- **分发与审计** — 把资产复制到其他 Agent 或项目；追加式操作日志 + 基线指纹，报告静默改动
- **Agent 自管理** — 同一内核另出一个 15 工具的 stdio MCP 服务

## 下载安装

到 [Releases](https://github.com/ntc0728/agentkeeper/releases) 下载：

| 文件 | 说明 |
|---|---|
| `AgentKeeper_<version>_x64-setup.exe` | 安装版，装到当前用户，不需要管理员权限 |
| `AgentKeeper_<version>_x64-portable.zip` | 免安装，解压即用 |

需要 Windows 10/11 与 WebView2 运行时（Windows 11 自带）。

## 快速上手

1. 打开应用，自动扫描本机的 Agent 与项目
2. 在 **资产** 页搜索、筛选、校验、编辑
3. 每次写入前确认；失败会自动还原，每笔改动都记在 **操作记录** 里

## MCP 服务

安装目录里的 `agentkeeper-mcp.exe` 是同一内核的 stdio MCP 服务，接给任意 MCP 客户端：

```json
{
  "mcpServers": {
    "agentkeeper": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\AgentKeeper\\agentkeeper-mcp.exe"
    }
  }
}
```

15 个工具覆盖查询、校验、写入、分发与 Bundle 迁移，与图形界面共用同一套校验和事务写入。
详见 [docs/MCP-SERVER.md](docs/MCP-SERVER.md)。

## 从源码构建

需要 Rust stable（`x86_64-pc-windows-msvc` + MSVC 工具链）、Node.js（仅构建脚本用）、WebView2。

```bat
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
npm run tauri:sync
cd tauri-shell\src-tauri
cargo tauri dev
```

打包产物（安装包 + 便携包，落在 `release\`）：

```bash
npm run tauri:bundle
```

## 数据位置

`%APPDATA%\AgentKeeper\agentkeeper` —— 设置、项目清单、审计日志都在这里；
可用环境变量 `AK_STORE_ROOT` 覆盖。程序安装位置与数据目录互不影响。

## 检查与测试

```bash
npm run check        # 静态关卡：残留、桥接契约、渲染层规则、i18n、构建产物一致性
npm run test         # 跑受本次改动影响的测试
npm run test:all     # 全部测试
```

## 文档

| 文档 | 内容 |
|---|---|
| [docs/DESIGN.md](docs/DESIGN.md) | 设计：资产模型、扫描器、写入事务、界面结构 |
| [docs/MCP-SERVER.md](docs/MCP-SERVER.md) | MCP 服务接入 |
| [docs/UPDATE.md](docs/UPDATE.md) | 软件内更新（检查、下载、安装、令牌） |
| [docs/CHANGELOG.md](docs/CHANGELOG.md) | 发版说明与仓库内改动记录 |
| [docs/BUILDING.md](docs/BUILDING.md) | 构建与发布流水线 |
| [docs/RUST-ENV.md](docs/RUST-ENV.md) | Rust 环境与组件 |

## 许可证

[MIT](LICENSE)
