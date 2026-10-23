# AgentKeeper MCP Server · Agent 接入说明书

> 让任意支持 MCP（Model Context Protocol）的 AI Agent 直接管理你机器上的 Agent 资产：
> 技能、MCP 服务、规则、记忆文件 —— 查询、创建、修改、分发，全程走 AgentKeeper
> 的校验 + 快照 + 事务写入，与桌面应用共享同一套安全边界。

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
| **分发** | 把一个资产复制到其他 Agent/项目 |
| **体检** | 校验问题 + 提示注入/凭据泄露等安全扫描 |
| **迁移** | Bundle 导出导入 |

它是**同一个 `core::` 的第二个入口**：与 GUI 共用扫描器、校验器、写入事务层，
不是另写一份写盘逻辑。因此 MCP 侧的每一次写入，在桌面应用的「操作记录」里都是同一笔事务。

**安全模型（重要）**：

- Agent **直接写真实 agent 目录**，与人操作走同一条路径：校验 → 内存快照 → 原子写 → 记日志。
- 写入前，被触及路径的当前状态读入内存快照；事务结果记入 `audit.jsonl`。写入中途失败会按
  快照整笔还原，还原不了的逐条报告（`store.restoreIncomplete`）。
- 校验与安全扫描不放松：`error` 级问题直接阻断，磁盘零写入。
- 桌面应用里人手动写入会弹二次确认框；MCP 侧没有交互界面，等价的安全网是「写前快照 +
  失败自动还原 + 审计日志」—— Agent 改了什么、改到哪，全在日志里。

---

## 2. 启动方式

二进制随桌面应用一起安装，与 shell 同级。它在独立的 `agentkeeper-core` crate 里（零 Tauri 依赖），
由 `tauri-shell/stage-core-bins.js`（`npm run tauri:core-bins`）暂存为 `bundle.externalBin` 的
sidecar，打包器把 `agentkeeper-mcp.exe` 放进安装目录。

用 `externalBin` 而不是让打包器自动收集，是因为两个二进制在**另一个** crate 里：打包器只扫描
它自己构建的那个 crate 的 `src/bin/`。而这条路的循环依赖已经消除 —— `tauri-build` 是 shell crate
的 build script，sidecar 属于 core crate（没有 build script），两者互不等待。

早先这里两个二进制是 Tauri crate 自己的 `[[bin]]`，那时 `externalBin` 既是多余的，也是循环的：
`tauri-build` 作为该 crate 的 build script 对每个 target 都会执行，干净的 `target/` 无法先产出
它自己要求的 sidecar。

```bash
# Windows 安装版
"%LOCALAPPDATA%\AgentKeeper\agentkeeper-mcp.exe"

# 开发构建（cargo build --release --bin agentkeeper-mcp 的产物）
tauri-shell/target/release/agentkeeper-mcp.exe
```

> 安装版路径为实测值：`tauri.conf.json` 的 `windows.nsis.installMode = "currentUser"`，
> NSIS 安装器（`/S` 静默、不传 `/D=`）默认落到 `%LOCALAPPDATA%\AgentKeeper`，
> **不需要管理员权限**；卸载用同目录的 `uninstall.exe /S`。
> 表里 `%APPDATA%\AgentKeeper\agentkeeper` 那份存储**不在**安装目录里，卸载不会清掉它。

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `AK_STORE_ROOT` | `%APPDATA%\AgentKeeper\agentkeeper` | MCP 与桌面应用共享同一存储（事务日志、设置都在这）；两边必须一致，否则 Agent 改的事务在 UI 的「操作记录」里看不到 |

启动后 **stderr** 打印一行诊断，stdout 只输出协议 JSON：

```
[agentkeeper-mcp] store: C:\Users\<你>\AppData\Roaming\AgentKeeper\agentkeeper
```

> stdout 是纯协议流：服务端保证每行一个紧凑 JSON-RPC 2.0 消息，任何日志都走 stderr。
> 这一条有进程级回归测试守着（`cargo test -p agentkeeper-core --test mcp`），因为它一旦被破坏，
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

重启 Claude Desktop，工具面板出现 `agentkeeper` 的 15 个工具即接入成功。

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
- `tools/list` → 15 个工具定义（含 JSON Schema 入参）
- `tools/call` → `{name, arguments}`，返回 `{content:[{type:"text", text:"<JSON 字符串>"}]}`；
  工具执行失败时信封上带 `result.isError: true`，payload 里是 `{ok:false, error}`
- 协议错误码：非法 JSON `-32700`（`id: null`）、未知方法 `-32601`、未知工具 `-32602`
- stdin 关闭即退出，退出码 `0`

> `isError` 的语义：业务失败（`{ok:false}`）同样标记 `result.isError: true`，按 MCP 规范
> 与协议错误一致处理。消费者是第三方客户端，只有统一标记才不会被当成成功。

---

## 4. 工具 API（15 个）

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
仪表盘数字：`{storeRoot, total, byKind, byOrigin, issues, shadowed, estTokens, agents, projects}`。
`shadowed` 是被项目作用域遮蔽的资产数（与写入无关）。
`agents` 除 17 个适配器外，还会包含 `shared` 伪条目（`~/.agents/skills` 存在时）。

#### `health_report`
`{items:[…], findings:n}` —— 全部校验问题 + 安全发现（含证据片段）。安全发现即 MCP 投毒扫描结果：
`tauri-shell/core/src/security.rs` 内置 13 条模式，含 `curl|sh`、PowerShell 编码执行、base64 解码执行、
反向 shell、凭据路径、环境变量窃取、`ignore previous instructions` 类注入短语等。
`includeCached` 默认 false（厂商缓存插件不参与体检，否则别人的错会被算到你头上）。

#### `baseline_diff`
指纹基线差异（谁被静默改了）。

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
→ `{ok:true, txId:"tx-…", files:["<真实目标路径>"], note:"…"}`。
`files` 里的路径**已经写好了** —— MCP 与人操作走同一条事务：先把目标路径的当前状态读入
内存快照，再原子写、向 `audit.jsonl` 追加一行。
校验不过（如 name 非法）返回 `blocked:true` + `issues`，不会写任何文件。
`agentId` 省略时取第一个已安装的 agent。

#### `update_asset`
`{"assetId":"rule:ts-strict:codex:global","content":"<完整新内容>"}` —— 校验后整文件替换。

#### `delete_asset`
先快照再删除；写入中途失败的整笔操作会按快照还原。

#### `distribute_asset`
```json
{
  "assetId": "skill:code-review:claude-code:global",
  "targets": [ {"agentId":"codex","scope":"global"}, {"agentId":"workbuddy","scope":"project","projectId":"p-xxx"} ]
}
```
同名冲突/同位置会被跳过并在返回中说明。

### 事务类

写入类工具都直接落盘，然后返回本次事务号。写入中途失败时，store 已按写前快照整笔还原，
并在返回里说明还原结果 —— Agent 侧不需要、也没有「再撤销一次」的接口。
「操作记录」里的日志是事后核查用的记录，不是撤销入口。

### 维护类

#### `set_baseline` / `export_bundle` / `import_bundle`
基线锚定 / Bundle 导出导入（`dir` 为绝对路径）。

### 获取类

#### `search_registry`
`{"q":"代码审查","limit":30}` → 查 agentskills.codes 公开注册表（免鉴权，6 s 超时，
网络失败返回明确错误不抛异常、不挂住进程）。

#### `install_skill`
`{"repo":"owner/repo","agentId":"claude-code"}` → `git clone --depth 1`（走用户本机 git/代理）→
定位 SKILL.md → 逐个校验 + 安全扫描（error 项跳过）→ 走 store 事务直接写入真实目录。
来源 repo 记入审计日志。
未知 `agentId` 在拉起 git 之前就报错。

---

## 5. 典型对话示例

接入后你可以直接对 Agent 说：

> 「看看我所有 Agent 里有多少技能，哪些 Agent 缺 code-review 这个技能」

Agent 会调 `stats` + `list_assets`，然后告诉你矩阵缺口（桌面应用 Agents 页有同款视图）。

> 「给 codex 也装上 code-review」

Agent 调 `distribute_asset`，直接写进 codex 的 skills 目录并返回 `txId`；
要撤销就让人把那个文件改回去或删掉 —— MCP 侧没有「按事务撤销」这个接口。

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
| 写入没按预期出现 | 到「操作记录」按 `txId` 看这笔事务；`blocked` 表示校验拦下了，磁盘零写入 |
| 写入中途失败 | 返回里会说明哪些路径没能还原（`store.restoreIncomplete`），按列出的路径手工核对 |
| 与桌面应用数据不同步 | 确认两者 `AK_STORE_ROOT` 一致；Agent 侧改完可调 `scan` 强制刷新 |

---

## 7. 与桌面应用的职责边界

| | 桌面应用 | Agent（经 MCP） |
|---|---|---|
| 查询/体检 | ✓ | ✓ |
| 直接写入真实目录 | ✓ 人操作，写前弹二次确认 | ✓ 无界面，靠写前快照 + 日志 |
| 写前内存快照 + 事务日志 | ✓ | ✓ |
| 写入失败自动还原 | ✓ | ✓ |
| 语言/主题等偏好 | ✓ | — |
