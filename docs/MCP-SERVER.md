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
| **共享库** | 列出共享库与各 Agent 的启用矩阵、开关单项、健康体检与逐个修复、只读收敛方案 |

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

> 安装版路径由 `tauri.conf.json` 的 `windows.nsis.installMode = "currentUser"` 决定：
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

重启 Claude Desktop，工具面板出现 `agentkeeper` 的 21 个工具即接入成功。

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
- `tools/list` → 21 个工具定义（含 JSON Schema 入参）
- `tools/call` → `{name, arguments}`，返回 `{content:[{type:"text", text:"<JSON 字符串>"}]}`；
  工具执行失败时信封上带 `result.isError: true`，payload 里是 `{ok:false, error}`
- 协议错误码：非法 JSON `-32700`（`id: null`）、未知方法 `-32601`、未知工具 `-32602`
- stdin 关闭即退出，退出码 `0`

> `isError` 的语义：业务失败（`{ok:false}`）同样标记 `result.isError: true`，按 MCP 规范
> 与协议错误一致处理。消费者是第三方客户端，只有统一标记才不会被当成成功。

---

## 4. 工具 API（21 个）

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

### 共享库类

这一组与桌面应用「共享」页**读的是同一批 core 函数**（`library_view::list_library`、
`shared_ops::converge_plan` / `doctor_report` / `apply_finding_fix`、
`Keeper::enable_asset` / `disable_asset`），
所以返回载荷与界面看到的完全一致 —— 不存在「MCP 版本」的第二套形状。
这六个工具追加在工具表末尾（改名对已经记住名字的 agent 是破坏性变更，只能追加）。

#### `list_library`
无参。→ `{ok, root, cap, agents[], skills[], mcp[], rules[], memory[]}`。
矩阵的唯一数据源：`agents` 是列，四类是行，每个单元格的 `state` 取
`on` / `off` / `copy` / `foreign`（被顶掉） / `drift`（MCP 漂移） / `unsupported`（附 `reason`） / `error` 之一
（词表以 `library_view.rs` 与 `view-shared.js` 的实现为准）。
**不缓存**：每次调用都按盘上的当前事实现算。

#### `enable_asset`
```json
{ "name": "code-review", "kind": "skill", "agentId": "codex", "scope": "global" }
```
* `kind` 取 `skill`（默认）/ `mcp` / `rule` / `memory`；
* `name` + `agentId` 必填；`assetId` 可代替 `name`，但**不是必填** ——
  只给名字时服务端会现扫一次、按 (kind, slug, agentId) 找到盘上那份真实副本
  （agent 手里通常没有 assetId，这是最常见的调用方式）；
* `kind: "mcp"` 时库里还没有这条记录，就再给 `config`（`command` / `args` / `env` / `url`），
  它会先被写进库、再物化到该 agent 的配置里；
* `scope: "project"` 时补 `projectId`（`mcp` 则给 `file`，即那个 agent 的项目配置文件）。

→ `{ok, written[], skipped[], keeper}`；已经是链接时是**空操作**（`ok:true` + `skipped: already linked`），
不是错误。一次事务，失败按快照整笔还原。

#### `disable_asset`
入参同上（不需要 `config`）→ `{ok, written[], skipped[]}`。
只删该 agent 侧的那一条：skill 删链接（库内主副本不动）、`mcp` 从配置里删条目
（或按 `mcpDisable: "flag"` 置 `enabled:false`）、`rule`/`memory` 只剥掉我们放进去的区块。
链接已经不在了返回 `ok:false` + `share.notLinked`（**不是**成功的空操作）。

#### `link_doctor`
无参。→ `{ok, findings[], counts}`。七类只读发现（`link.dangling` / `link.hijacked` /
`link.unmanaged` / `link.fragile` / `asset.duplicated` / `mcp.drift` / `link.ledgerDrift`），
每条带 `code` / `path` / `detail` / `fixable` / `fix`（`relink` / `adopt` / `rebuild` /
`converge`，或空）。**零写入**：要改盘用 `apply_finding_fix`。

#### `converge_plan`
无参。→ `{ok, groups[], savedBytes, actionable}`。**只出方案，零写入**；
`actionable` 是「能一键收敛」的组数（排除内容不一致的组）。**应用收敛只能在桌面应用**的
收敛向导里 —— 那一步需要人确认，见下面的 `apply_finding_fix`。

#### `apply_finding_fix`
修 `link_doctor` 报出的**一条**发现。参数直接抄 `link_doctor` 的返回值即可：

```json
{
  "code": "link.dangling",
  "path": "C:\\\\Users\\\\你\\\\.claude\\\\skills\\\\pdf",
  "fix": "relink",
  "detail": { "src": "…", "reason": "targetMissing" },
  "choice": "library"
}
```

* `code` 必填，其余按发现的类型给：`path` / `fix` / `detail` 原样透传；
* `fix` 可省略，省略时按 `code` 自己的修法执行；给了就必须对上，对不上回
  `doctor.unsupportedFix`；
* `choice` 只对 `mcp.drift` 有效，取 `library`（以库内定义覆盖 agent 配置）/
  `agent`（以该 agent 盘上的值为准回写库）/ `independent`（标记为独立，从库内启用清单移除）。

各 code 的修法与前提：

| code | 修法 | 前提 |
|---|---|---|
| `link.dangling`（`reason: targetMissing`） | 重连 | 库内主副本还在；`detail.src` 或台账能定位到主副本 |
| `link.hijacked` | 重连 | 只在该发现 `fixable: true`（agent 侧内容与库内一致）时允许 |
| `link.unmanaged` | 导入库（`adopt`） | 目标在库外且可读 |
| `link.ledgerDrift` | 以磁盘为准重建台账（`rebuild`） | 无需参数 |
| `mcp.drift` | 三选一，见 `choice` | 索引是 `detail.name` + `detail.agentId`；配置路径与条目从盘上重新解析，不信调用方给的路径 |
| `link.fragile` | — | 硬链接可能被 agent 打断，没有可自动执行的修法，回 `doctor.notFixable` |

**边界**：`asset.duplicated`（批量收敛，一次会动几十个位置）**显式拒绝**，返回
`{ok:false, code:"doctor.notFixable", detail.reason:"converge touches every copy at once; use the desktop wizard"}`。
要去桌面应用的「共享」页点收敛向导。同样不在这里做的还有批量修复：一次只修一条发现。

写入走与桌面端同一个 `apply_finding_fix` 命令：事务、快照、失败整笔还原、审计日志一致。
`mcp.drift` 里的「以库为准」等于用库内定义重写那一条配置条目，「以 agent 为准」是反过来
回写库 —— 两边都由调用方明确说出选哪边，工具不会自己猜。

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

> 「code-review 这个技能现在到底装在哪些 Agent 上？缺的那个帮我接上」

Agent 调 `list_library` 看矩阵单元格，再调 `enable_asset`
（`{"name":"code-review","kind":"skill","agentId":"cursor"}`）
把 cursor 那份换成指向库内主副本的链接 —— 只动这一个位置，库内主副本与其他 Agent 不受影响。

> 「有没有重复的技能白占磁盘？先别动我的东西」

Agent 调 `converge_plan`（零写入）报出组数、可释放字节数与 `actionable`；
要真收敛就请人到桌面应用的「共享」页点向导 —— MCP 侧刻意不提供一键应用，
因为那会把「确认」这一步整个省掉。

> 「那条悬空的链接帮我修好」

Agent 调 `link_doctor` 找到对应发现，再调 `apply_finding_fix`（`code` + `path` + `fix`
照抄发现里的值）把它接回库内主副本。一次只修一条；批量收敛会被明确拒绝。

> 「fetch 这个 MCP 服务在 codex 里的参数被我改过，以库里的为准」

Agent 调 `link_doctor` 取到 `mcp.drift` 的发现，再调 `apply_finding_fix`
（`code:"mcp.drift"`、`detail.name`、`detail.agentId`、`choice:"library"`）；
选 `agent` 则反过来以盘上的值为准回写库。

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
| 报 `store.busy` | 另一个进程（通常是桌面应用）正在写共享台账 / 设置，或正在迁移库位置。等它结束再试 |
| `apply_finding_fix` 回 `doctor.notFixable` | 这条发现没有可自动执行的修法（硬链接易断、批量收敛），或前提不成立（库内主副本已缺失）。照 `detail.reason` 的提示走桌面端 |
| `apply_finding_fix` 回 `doctor.unsupportedFix` | 传的 `fix` 与 `code` 对不上。省略 `fix` 即按该 `code` 的标准修法执行 |
| `enable_asset` 回 `share.notLinked` / `share.foreignLink` | 该 agent 侧不是链接，或链接指向库外。先看 `link_doctor` 有没有对应发现 |
| 迁移库位置期间调用共享工具报错 | 迁移整段持写锁。等迁移结束后再调用 |

---

## 7. 与桌面应用的职责边界

| | 桌面应用 | Agent（经 MCP） |
|---|---|---|
| 查询/体检 | ✓ | ✓ |
| 直接写入真实目录 | ✓ 人操作，写前弹二次确认 | ✓ 无界面，靠写前快照 + 日志 |
| 写前内存快照 + 事务日志 | ✓ | ✓ |
| 写入失败自动还原 | ✓ | ✓ |
| 共享库：开关、体检、逐个修复 | ✓ | ✓ |
| 收敛重复技能（一次动多个位置） | ✓ 收敛向导逐组确认 | ✗ 只出方案（`converge_plan`） |
| 库位置迁移 | ✓ | ✗ |
| 语言/主题等偏好 | ✓ | — |
