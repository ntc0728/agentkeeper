# AgentKeeper MCP Server · Agent 接入说明书

> 让任意支持 MCP（Model Context Protocol）的 AI Agent 直接管理你机器上的 Agent 资产：
> 技能、MCP 服务、规则、记忆文件 —— 查询、创建、修改、分发、回滚，全程走 AgentKeeper
> 的校验 + 备份 + 影子层事务，与桌面应用共享同一套安全边界。

---

## 1. 这是什么

`agentkeeper-mcp` 是一个 **stdio 进程**，实现标准 MCP 协议（JSON-RPC 2.0，
`protocolVersion: 2024-11-05`）。任何能接 MCP 的 Agent（Claude Desktop、Cursor、Cline、
Windsurf、Zed、CodeBuddy 等）都可通过一份配置接入，之后 Agent 可以：

| 能力 | 说明 |
|---|---|
| **看** | 列出/搜索全部资产，跨 17 种 Agent 与全部项目 |
| **读** | 读任意资产内容 + 校验问题 + 安全发现 |
| **写** | 创建技能、更新规则/记忆（校验拦截非法写入） |
| **分发** | 把一个资产复制到其他 Agent/项目（单事务可撤销） |
| **回滚** | 按事务撤销历史写入 |
| **体检** | 校验问题 + 提示注入/凭据泄露等安全扫描 |
| **备份** | Bundle 导出导入 |

它是**同一个 `core::` 的第二个入口**：与 GUI 共用扫描器、校验器、写入事务层，
不是另写一份写盘逻辑。因此 MCP 侧暂存的变更，在桌面应用的「待应用」面板里原样可见。

**安全模型（重要）**：

- 沙盒模式开启时（默认开），Agent 的写入只进入**影子层**（staged），
  **必须由人在桌面应用里点「应用」才真正落盘**。Agent 能暂存，不能悄悄改文件。
- `apply_pending` 在沙盒开启时**由代码强制拒绝**（不是靠提示词约束）。
- 每次真实写入都先备份、后原子写、记审计日志，任何一步可撤销。

---

## 2. 启动方式

二进制随桌面应用一起安装，与 shell 同级。这条链路是有守卫的，不是靠 bundler 顺手带上：
`tauri.conf.json` 的 `bundle.externalBin: ["binaries/agentkeeper-mcp"]` 声明它，
`tauri-shell/stage-mcp-bin.js`（`npm run tauri:mcp-bin`）负责把 `cargo build --release --bin
agentkeeper-mcp` 的产物按 Tauri 命名规则落到 `src-tauri/binaries/agentkeeper-mcp-<triple>.exe`，
`.github/workflows/release.yml` 在 `cargo tauri build` **之前**跑这一步——缺文件时
`tauri-build` 会硬失败，而不是悄悄打出一个没有 MCP 的安装包。

```bash
# Windows 安装版
"%LOCALAPPDATA%\AgentKeeper\agentkeeper-mcp.exe"

# 开发构建（先跑 node tauri-shell/stage-mcp-bin.js，或直接用 cargo 产物）
tauri-shell/src-tauri/target/release/agentkeeper-mcp.exe
```

> 安装版路径为实测值：`tauri.conf.json` 的 `windows.nsis.installMode = "currentUser"`，
> NSIS 安装器（`/S` 静默、不传 `/D=`）默认落到 `%LOCALAPPDATA%\AgentKeeper`，
> **不需要管理员权限**；卸载用同目录的 `uninstall.exe /S`。
> 表里 `%APPDATA%\AgentKeeper\agentkeeper` 那份存储**不在**安装目录里，卸载不会清掉它。

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `AK_STORE_ROOT` | `%APPDATA%\AgentKeeper\agentkeeper` | MCP 与桌面应用共享同一存储；两边必须一致，否则 Agent 暂存的变更在 UI 里看不到 |
| `AGENTKEEPER_SANDBOX` | 开 | `0` 关闭影子层（Agent 直接写真实目录，**不建议**） |

启动后 **stderr** 打印一行诊断，stdout 只输出协议 JSON：

```
[agentkeeper-mcp] store: C:\Users\<你>\AppData\Roaming\AgentKeeper\agentkeeper · sandbox: true
```

> stdout 是纯协议流：服务端保证每行一个紧凑 JSON-RPC 2.0 消息，任何日志都走 stderr。
> 这一条有进程级回归测试守着（`cargo test --test mcp`），因为它一旦被破坏，
> 客户端会把整条连接判为损坏而不是报一个可读的错误。

---

## 3. 接入配置

### 3.1 Claude Desktop

编辑 `claude_desktop_config.json`（设置 → 开发者 → 编辑配置）：

```json
{
  "mcpServers": {
    "agentkeeper": {
      "command": "C:\\Users\\你\\AppData\\Local\\AgentKeeper\\agentkeeper-mcp.exe",
      "env": {
        "AK_STORE_ROOT": "C:\\Users\\你\\AppData\\Roaming\\AgentKeeper\\agentkeeper"
      }
    }
  }
}
```

重启 Claude Desktop，工具面板出现 `agentkeeper` 的 20 个工具即接入成功。

### 3.2 Cursor

设置 → MCP → Add MCP Server，或编辑 `~/.cursor/mcp.json`：

```json
{
  "mcpServers": {
    "agentkeeper": {
      "command": "C:\\Users\\你\\AppData\\Local\\AgentKeeper\\agentkeeper-mcp.exe"
    }
  }
}
```

### 3.3 Cline / 其他 VS Code 插件

同样格式的 `mcpServers` JSON（Cline 用数组形态的 `mcpServers` 键则按其文档调整）。

### 3.4 通用 MCP 客户端（stdio）

- 传输：stdio，每行一个 JSON-RPC 2.0 消息；**不支持 batch**
- `initialize` → `protocolVersion: "2024-11-05"`、`serverInfo.name: "agentkeeper"`、`capabilities.tools`
- 通知（`notifications/*`、无 `id` 的请求、空行）**一律不应答**
- `tools/list` → 20 个工具定义（含 JSON Schema 入参）
- `tools/call` → `{name, arguments}`，返回 `{content:[{type:"text", text:"<JSON 字符串>"}]}`；
  工具执行失败时信封上带 `result.isError: true`，payload 里是 `{ok:false, error}`
- 协议错误码：非法 JSON `-32700`（`id: null`）、未知方法 `-32601`、未知工具 `-32602`
- stdin 关闭即退出，退出码 `0`

> 与旧 Electron 版的一处**有意差异**：JS 版只在 handler 抛异常时设 `isError`，
> 业务失败（`{ok:false}`）返回的是普通结果，遵循规范的客户端会把它当成成功。
> Rust 版按 MCP 规范统一标记 `isError`。消费者是第三方客户端，这里没有兼容包袱。

---

## 4. 工具 API（20 个）

### 查询类

#### `list_assets`
列出/筛选资产。
```json
{ "kind": "skill", "agentId": "claude-code", "scope": "global", "search": "代码审查", "limit": 100 }
```
全部参数可选（`limit` 默认 100、上限 500）。返回 `{total, showing, assets:[{id,name,kind,agentId,scope,size,tokens,issues,security,shadowed,file}]}`。
过滤在服务端完成——客户端拿不到全量再自己筛。

#### `read_asset`
`{"assetId": "skill:code-review:claude-code:global:ab12cd"}` → 完整内容 + 元数据 + 校验问题 + 安全发现。

#### `scan`
触发全量重扫，返回汇总。外部改了文件后调用。
首次任何查询也会隐式扫描一次，之后命中进程内缓存（约 1000 个资产 ≈ 0.5 s）。

#### `stats`
仪表盘数字：`{storeRoot, sandbox, total, byKind, byOrigin, issues, shadowed, estTokens, agents, projects}`。
`agents` 除 17 个适配器外，还会包含 `shared` 伪条目（`~/.agents/skills` 存在时）。

#### `health_report`
`{items:[…], findings:n}` —— 全部校验问题 + 安全发现（含证据片段）。安全发现即 MCP 投毒扫描结果：
`tauri-shell/src-tauri/src/core/security.rs` 内置 13 条模式，含 `curl|sh`、PowerShell 编码执行、base64 解码执行、
反向 shell、凭据路径、环境变量窃取、`ignore previous instructions` 类注入短语等。
`includeCached` 默认 false（厂商缓存插件不参与体检，否则别人的错会被算到你头上）。

#### `list_transactions` / `baseline_diff`
事务历史（含 txId，用于回滚）/ 指纹基线差异（谁被静默改了）。

### 写入类

#### `create_skill`
```json
{
  "name": "code-review",
  "description": "对当前分支做代码审查，输出风险清单",
  "body": "# Steps\n1. git diff main...\n2. ...",
  "agentId": "claude-code"
}
```
→ `{ok:true, staged:true, txId:"tx-…", files:["<真实目标路径>"], note:"…"}`。
`staged:true` 表示进了影子层，等人在桌面应用点「应用」——此时 `files` 里的路径**还不存在**，
影子副本在 `<store>/sandbox/drive/<盘符>/…`。
校验不过（如 name 非法）返回 `blocked:true` + `issues`，不会写任何文件。
`agentId` 省略时取第一个已安装的 agent。

#### `update_asset`
`{"assetId":"rule:ts-strict:codex:global","content":"<完整新内容>"}` —— 校验后整文件替换。

#### `delete_asset`
备份后删除，可用 `rollback_transaction` 撤销。

#### `distribute_asset`
```json
{
  "assetId": "skill:code-review:claude-code:global",
  "targets": [ {"agentId":"codex","scope":"global"}, {"agentId":"workbuddy","scope":"project","projectId":"p-xxx"} ]
}
```
同名冲突/同位置会被跳过并在返回中说明。

### 事务类

#### `apply_pending` / `discard_pending` / `list_pending`
影子层操作。**沙盒门禁**：沙盒开启时 `apply_pending` 直接拒绝，返回

```
sandbox is ON — staged changes await human approval in the AgentKeeper desktop UI.
To let agents apply directly, start this server with AGENTKEEPER_SANDBOX=0.
```

沙盒关闭时才可由 Agent 落盘。
`list_pending` → `{count, sandbox, note, changes:[{op, target, bytes, at}]}`。

#### `rollback_transaction`
`{"txId":"tx-xxx"}` —— 还原该事务触及的所有文件。

### 维护类

#### `set_baseline` / `export_bundle` / `import_bundle`
基线锚定 / Bundle 导出导入（`dir` 为绝对路径）。

### 获取类

#### `search_registry`
`{"q":"代码审查","limit":30}` → 查 agentskills.codes 公开注册表（免鉴权，6 s 超时，
网络失败返回明确错误不抛异常、不挂住进程）。

#### `install_skill`
`{"repo":"owner/repo","agentId":"claude-code"}` → `git clone --depth 1`（走用户本机 git/代理）→
定位 SKILL.md → 逐个校验 + 安全扫描（error 项跳过）→ 走 store 事务写入
（沙盒开 = 只进影子层等人工审批；沙盒关 = 直接落盘）。来源 repo 记入审计日志。
未知 `agentId` 在拉起 git 之前就报错。

---

## 5. 典型对话示例

接入后你可以直接对 Agent 说：

> 「看看我所有 Agent 里有多少技能，哪些 Agent 缺 code-review 这个技能」

Agent 会调 `stats` + `list_assets`，然后告诉你矩阵缺口（桌面应用 Agents 页有同款视图）。

> 「给 codex 也装上 code-review，先用影子层」

Agent 调 `distribute_asset`（沙盒开时自动 staged），你在桌面应用「待应用」面板确认后应用。

> 「有没有谁在偷偷改我的记忆文件？」

Agent 调 `baseline_diff` —— 相对上次锚点的变更/新增/消失清单。

> 「这个叫 xxx 的 MCP 服务器安全吗？」

Agent 调 `read_asset`，安全发现里会列出匹配到的模式与证据片段。

---

## 6. 排障

| 现象 | 处置 |
|---|---|
| 客户端看不到工具 | 确认 `command` 指向存在的 `agentkeeper-mcp.exe`；手动在终端跑一次，stderr 必须出现 `[agentkeeper-mcp] store:` |
| 客户端报「连接损坏 / 无法解析」 | 不可能由本项目产生：stdout 只有 JSON-RPC。若真遇到，把 stdout 原文留下来做 bug |
| 写入报 blocked | 校验拦截，`issues` 里是具体原因（frontmatter 字段等） |
| 工具调用超时 | 首次调用含全量扫描（千级资产 ≈ 0.5 s，冷盘更慢），客户端超时建议 ≥ 30 s |
| 想直接写不暂存 | 设 `AGENTKEEPER_SANDBOX=0`（不推荐；影子层才是安全默认） |
| 与桌面应用数据不同步 | 确认两者 `AK_STORE_ROOT` 一致；Agent 侧改完可调 `scan` 强制刷新 |

---

## 7. 与桌面应用的职责边界

| | 桌面应用 | Agent（经 MCP） |
|---|---|---|
| 查询/体检 | ✓ | ✓ |
| 暂存写入 | ✓ | ✓ |
| **应用影子层** | ✓ 人点「应用」 | 沙盒开启时 `apply_pending` 被拒绝（代码强制）；关沙盒后可落盘 |
| 回滚 | ✓ | ✓ |
| 语言/主题等偏好 | ✓ | — |
