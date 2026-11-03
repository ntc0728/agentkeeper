# AgentKeeper — 设计方案

> 面向 AI 编码 Agent 的资产控制平面。统一发现、校验、编辑、分发与审计
> 全局与项目级的 **skills / MCP / 插件 / 规则 / 记忆库**。
>
> 桌面壳：**Tauri 2**。业务逻辑：**Rust**（`tauri-shell/core/src/`）。
> 前端：零依赖原生 DOM（`src/renderer/`），无框架、无构建步骤。

---

## 0. 问题陈述

一个 Agent 的"行为"并不只来自模型，它来自模型启动前被静默预加载的文件：

| 资产 | 载体 | 失效方式 |
|---|---|---|
| Skill | `SKILL.md` + YAML frontmatter | 缺 `description` → 永不触发；目录名与 `name` 不符 → 缓存键错位 |
| MCP Server | `mcp.json` / `.mcp.json` / `config.toml` | 缺 `command`/`url` → 连接失败；改写时覆盖兄弟 server |
| Plugin | `.codebuddy-plugin/plugin.json` 等 | 版本目录错位；误当作 marketplace |
| Rule | `AGENTS.md` / `CLAUDE.md` / `.mdc` | 空文件白烧 token；多 agent 重复加载 |
| Memory | `MEMORY.md` + `memory/YYYY-MM-DD.md` | 陈旧、冲突、无审计 |

现状的三个硬伤：

1. **不可见** — 散落在 `~/.claude` `~/.codex` `~/.cursor` 和每个仓库的隐藏目录，没人说得清当前生效的是哪一份。
2. **易损坏** — 手改 JSON/YAML/frontmatter，一个逗号就能让整棵配置树失效。
3. **无追溯** — 改错了只能 `git checkout`，而 `~/.workbuddy` 往往不在任何仓库里。

---

## 1. 开源对标与差异化

与三个同类成熟项目做了对照（截至 2026 年 10 月）：

### 1.1 Claude Code Organizer（`@mcpware/claude-code-organizer`）

MIT，零依赖 npx 启动。核心能力：Scan → See Effective → Fix 三段循环，
含 **MCP 工具投毒扫描**、**每项 context token 预算**、**每项目 MCP 开关**。

**可借鉴**：Show Effective（回答"agent 到底加载了什么"）、安全扫描与 finding→跳转删除的闭环。
**不借鉴**：仅服务 Claude Code 一种 harness。

### 1.2 SkillHub Local（`skillhub-local`）

MIT，TypeScript。**最接近本方案**：自动探测 16–18 个 agent，统一视图
skills · memories · instructions · plugins · MCP servers，支持跨 agent copy/symlink。

**可借鉴**：适配器矩阵、按 agent 声明 native/global/shared skill 目录、矩阵视图。
**不借鉴**：强依赖外部 `skills` CLI 做安装动作；写入无事务/备份/回滚层。

### 1.3 AgentCorral

Tauri v2 + React + Rust。repo registry、visual editor、config bundle 导出/导入。
**可借鉴**：Global/Project scope toggle、effective merged view with source annotations、bundle 概念。
**不借鉴**：需要 Rust 工具链 + 三平台签名打包。

### 1.4 AgentKeeper 的定位

```
                    ┌──────────────────────────────────────────┐
                    │  三者共有：统一视图 · 适配器 · CRUD       │
                    ├──────────────────────────────────────────┤
  claude-code-org → │ Show Effective  ·  安全扫描  ·  预算视图  │  ← 借鉴
   skillhub-local → │ 18-agent 适配器  ·  跨 agent 矩阵/分发    │  ← 借鉴
    AgentCorral   → │ scope toggle  ·  bundle  ·  merged view  │  ← 借鉴
                    ├──────────────────────────────────────────┤
     AgentKeeper   │ 写入事务层：validate→capture→write→journal│
     独有          │ 直写真实目录 · 二次确认 · 失败自动还原     │
                    │ 内存快照：整棵目录树逐字节还原             │
                    └──────────────────────────────────────────┘
```

**一句话差异**：其余三家解决「看见」和「编辑」，AgentKeeper 解决**「编辑之后还能安全地活下来」**。

---

## 2. 系统架构

```
┌─────────────────────────────────────────────────────────────────┐
│  WebView2（Tauri 2，无 Node 集成，CSP 见 tauri.conf.json）        │
│  index.html · styles.css · app.js（零依赖，原生 DOM，未 fork）     │
└───────────────────────────┬─────────────────────────────────────┘
                            │ window.ak.*  ← tauri-bridge.js
                            │ （桥接层保持 window.ak 的函数形状）
┌───────────────────────────┴─────────────────────────────────────┐
│  Tauri command 层 —— src/lib.rs                                  │
│  45 个 #[tauri::command]，逐个校验入参，只做形状适配              │
│  窗口装配：visible(false) + on_page_load(Finished) → show()      │
└───────────────────────────┬─────────────────────────────────────┘
                            │ 直接调用，无进程边界、无序列化往返
┌───────────────────────────┴─────────────────────────────────────┐
│  core::（Rust）—— 不依赖 Tauri，可单测、可被 CLI 复用              │
│                                                                  │
│  adapters.rs   17 个 agent 的纯数据声明 + JS 快照对齐测试          │
│  scan.rs       发现 · 解析 · 去重 · effective                     │
│  model.rs / yaml.rs / jsonc.rs / toml_splice.rs  格式层（保序）   │
│  validate.rs   写入前校验（error 阻断 / warn 放行）                │
│  security.rs   13 条投毒/凭据/编码执行模式                         │
│  store.rs      ★ 内存快照 · 原子写 · 事务 · 失败还原 · 审计        │
│  keeper.rs     变更服务层（CRUD/分发/diff，永不绕过校验）          │
│  registry.rs   项目注册表                                         │
│  discover.rs   注册表搜索 + 从 GitHub 安装                         │
│  update/       软件更新内核：version · http · token · manager      │
│  mcp.rs        stdio MCP server（21 个工具，第二个入口）           │
└─────────────────────────────────────────────────────────────────┘
```

**关键约束**：`core::` 不认识 Tauri 类型。因此同一套逻辑有三个入口。

| 入口 | 二进制 | 用途 |
|---|---|---|
| GUI | `agentkeeper-shell` | 桌面应用 |
| MCP | `agentkeeper-mcp` | Agent 通过 stdio 调用 |
| 扫描 | `ak-scan` | 无头打印 `ak:scan` 的完整 payload，供脚本与 CI 取数 |

`ak-scan` 只做无头扫描（打印一次完整 payload），写入只走 GUI 与 MCP 两条通道。

### 2.1 错误文案与中文翻译

这里有两层，必须分清，否则很容易写出"看起来一致、实际不一致"的结论：

| 层 | 现状 | 位置 |
|---|---|---|
| **Rust 侧（当前发布）** | `#[tauri::command]` 返回 `{ok:false, error:"<英文句子>"}`，**不带 code**；只有 MCP 的 `tools/call` 会补 code（英文→code 表） | `tauri-shell/src-tauri/src/lib.rs` · `tauri-shell/core/src/keeper.rs` · `tauri-shell/core/src/store.rs` · `tauri-shell/core/src/mcp.rs` |

渲染层的 `zhError(payload)` 先试 `code → 中文模板`，取不到再退回**句子词典** `zhText()`。
所以 Rust 侧能不能显示中文，完全取决于它吐出的句子在不在句子词典里——
**没有 code 兜底，漏一条就露一句英文**。

#### 覆盖情况：由脚本守住

`scripts/tauri-i18n.js` 刮 `tauri-shell/{core,src-tauri}/src/**/*.rs` 里的**全部字符串字面量**，用结构性规则
（panic / 日志 / 输入匹配器等）排掉不上屏的那些，剩下的逐条查词典，漏一条就 exit 1。
它同时校验 `CODE_ZH`：Rust 侧产出的 code 必须在 `src/renderer/engine-zh.js` 里有中文。

`scripts/tauri-i18n.js` 当前结果：

| 指标 | 当前值 | 说明 |
|---|---|---|
| MISS | 0 | 退出码 0 |
| 覆盖 | 117 EXACT + 66 PATTERNS | 句子词典的规模 |
| 硬编码中文残留 | 0 | 对外发出的句子没有中文；`src-tauri/src/` 里剩下的中文只在测试夹具与注释里 |
| 日志面豁免 | 3 | 只进日志，不上屏 |
| CLI 白名单 / CLI 文件 | 2 / 6 | 终端提示，不进 WebView |
| MCP 协议面（按文件豁免） | 74 | 协议字段，不走渲染层 |
| CODE-MISS / CJK-KEY | 0 / 0 | 见下节 |
| AMBIGUOUS-MULTI（PATTERN×PATTERN） | 0 | 两个正则同时命中 |
| EXACT SHADOWING（EXACT 遮蔽 PATTERNS） | 1 | 预期内 |

> 上表的数字会随 Rust 侧文案增删而变，**以 `node scripts/tauri-i18n.js` 的实际输出为准**。

`const` 形式的错误文案既不在 `"error":` 旁边也不是 `Err(...)`，脚本因此单独抽取常量，
否则这类漏翻不会以 MISS 的形式出现。抽样在这个规模上不可靠：缺口只能靠脚本守。

#### 架构契约（i18n）

这三条是约束，不是现状描述：

1. **Rust 侧只发英文**，中文一律由渲染层翻译。Rust 源码里不该出现面向用户的中文。
2. **凡是发 `code` 的，必须有对应的 `CODE_ZH` 条目。** 由 `scripts/tauri-i18n.js`
   的 **CODE-MISS** 检查强制（已上线，缺条目即 exit 1）。
   判定只针对**渲染层能看见的 code 通道**（json 面 / code 表 / `Serialize` 过的结构体字段）；
   不上屏的（结构体有 `code:` 字段但没 derive `Serialize`）单独列出、不拦，并打印一行
   "if the struct ever derives Serialize this flips to CODE-MISS"。
   同区还有 **CJK-KEY**（英文 key 槽位里出现中文，比如 `EXACT` 的 key 写成中文），同样是红灯。
3. **不发 `code` 的错误走句子词典回退，这是设计内的**——不是缺陷。
   代价是它比 code 匹配**更脆**：英文措辞一改，`zhText()` 就匹配不上，中文会**静默**
   退回英文。所以英文措辞一旦变更，**必须**同步 `EXACT` / `PATTERNS`。

> 第 3 条就是 CI 里那道 i18n 检查存在的**全部理由**。它拦的不是"忘了翻译"这种
> 明显失误，而是"改了英文没改词典"——后者不报错、不红、只有用户看得见。

换个说法定性：**Rust 侧没有 code 体系是健壮性降级，不是功能缺口**——界面照样出中文，
只是"漏翻"这种失误没了编译期兜底，只能靠脚本兜。

#### 维护提醒：字典只有一个来源

`scripts/tauri-i18n.js` 的 `DICT` 指向 `src/renderer/engine-zh.js`（全文这样引用 6 次）。
`src/renderer/` 是唯一真源，移动该文件时必须同步改这 6 处，否则闸门会以 exit 2
（`could not run`）失败，而不是报告"有漏翻"。

---

## 3. 适配器层设计

每个 agent 是一条**纯数据声明**（`tauri-shell/core/src/adapters.rs`），
新增 agent 是改数据不是改代码：

```rust
Agent {
    id: "claude-code",
    home: "~/.claude",
    globals:  Paths { skills: &["skills"], plugins: &["plugins"], rules: &["CLAUDE.md"], … },
    projects: ProjectPaths { marker: ".claude", skills: &[".claude/skills"], rules: &["AGENTS.md"], … },
}
```

已覆盖 17 个：**WorkBuddy · Claude Code · Codex · Cursor · OpenCode · Gemini CLI ·
GitHub Copilot · Windsurf · Cline · Amp · Continue · Roo Code · Goose · Zed ·
Warp · Pi · DeepSeek Harness**，外加 `~/.agents/skills` 跨 agent 共享标准目录
（scan 里作为一个 `shared` 伪条目出现，不占 17 个名额）。

**一致性怎么守**：`scripts/adapter-table.js` 在假 home 上求值，冻结成两个 Rust 常量
（`JS_TABLE_JSON` / `JS_RESOLVED_JSON`）固化在 `adapters.rs` 里；
`adapters.rs` 的测试再拿 `AGENTS` 与 `resolve_home()` 的每个结果逐字段比对。
`AGENTS` 一改，两边不一致就红。

#### 新增 agent 的维护规程

重新冻结常量只有一条路径：`scripts/adapter-table.js` 是生成器的唯一输入，
`npm run tauri:adapters` 用它重算常量。改了表就必须同一次跑生成器，
`adapters.rs` 不会自己变。

1. 在 `tauri-shell/core/src/adapters.rs` 的 `AGENTS` 表里加一条纯数据声明；
2. 在 `scripts/adapter-table.js` 的 `AGENTS` 里加同一条（这是生成器的输入）；
3. `npm run tauri:adapters`，让 `JS_TABLE_JSON`（原始声明）与 `JS_RESOLVED_JSON`（假 home 上
   求值后的路径）一起更新；
4. 在常量附近注明该 agent 的字段来源（官方文档 / 实测路径），让下一个改的人有据可查。

> **测试红了先重算常量，不要删测试。**
>
> `js.len() == AGENTS.len()` 在加了第 18 个 agent 而没同步常量时必然红——这正是它要报的错。
> 正确动作是回到第 2 步补齐常量后一起提交。

`installed` 统一取 `existsDir(home)`，不做二次探测。

### 3.1 扫描时的三个非显然问题

**问题一：插件缓存是三层目录，且 marketplace 不是插件。**

```
plugins/cache/<marketplace>/<plugin>/<version>/{.codebuddy-plugin/plugin.json, skills/…}
```

早期实现把 `<marketplace>` 本身也当成插件，一台机器扫出 **953 条**、其中 148 条是
`all-agents` / `api-testing` 这类 marketplace 清单条目。

修法（`scan.rs::walk_plugin_root`）：
- 命中 `marketplace.json` → 判定为容器，**递归下钻且自身不入表**；
- 命中 5 种 manifest 之一（`.codebuddy-plugin` `.claude-plugin` `.workbuddy-plugin` `.gemini-extension` `plugin.json`）→ 收为资产，**停止下钻**；
- 最大深度 4，按 manifest realpath 去重。

结果：**974 条**，且 0 个 marketplace 泄漏。

**问题二：一份文件被 7 个 agent 声明，一份配置里有 N 个 server。**

`AGENTS.md` 同时是 WorkBuddy / Claude Code / Codex / OpenCode / Amp / Pi / DSH 的项目规则；
`mcp.json` 里有多个 server。按"目录"去重会吞掉兄弟 server，按"文件"去重会吞掉共享文件。

修法（`dedupe_by_physical_path`）按 kind 区分物理身份：

| kind | 去重键 | 理由 |
|---|---|---|
| `mcp` | `file + configKey` | 同文件多 server 必须都活下来 |
| `plugin` | `dir` | 同一缓存被两条 root 命中就是同一个插件 |
| 其他 | `file` | 共享文件收一条，`meta.alsoSeen` 记录共同读取者 |

**问题三：id 必须锚在物理位置上。**

早期 `id = kind:name:agent:scope` 让同名不同版本的插件撞 id，`effective` Map 被静默去重，
974 条变 850 条。改为 `…:<hash(dir)前6位>`。

> 这三个坑都是**先量出来、再改**的：一台真实机器的 953 / 974 / 850 是同一组数字的三种失败形态。

---

## 4. 写入事务层（核心）

> 这是"操作可作用 agent 且不出错"的落点。实现：`core/store.rs`。

### 4.1 写入管线

```
validate ──► capture ──► atomic write ──► journal ──► [auto-restore]
   │            │                                       │
   └─阻断       └─内存快照 + 体积预算                     └─失败 = 按快照还原
```

1. **validate** — `validate.rs`。`error` 级直接阻断，**磁盘零写入**。
2. **capture** — 每个待触碰路径的当前状态读入内存 `Snapshot`（`Absent` / `File` / `Dir` / `Link`），
   记录 `existed`，因此**删除也可还原**。同一路径被多个 op 触及只快照一次，预算只扣一次。
   全事务共享一个 `MAX_SNAPSHOT_BYTES`（8 MiB）预算；超出则在**写第一个字节之前**整笔拒绝
   （`store.snapshotTooLarge`）——半份快照会还原出半新半旧的目录，比直接失败更糟。
3. **atomic write** — 直接写真实路径：先写 `<path>.agentkeeper-tmp-<pid>-ts>` 再 `rename`。
   Windows 上 rename 遇占用会失败，降级为直写但仍有 tmp 残留可追。
4. **journal** — `audit.jsonl` 追加一行：
   `{ts, txId, status, action, kind, agentId, projectId, assetName, targets[]}`。
   事务不落目录，日志行是唯一的持久痕迹。
5. **auto-restore** — 任一 op 失败即按快照还原**全部**已写路径，不因某个路径还原失败而中途放弃；
   还原不掉的逐条汇总返回（`restoreErrors[]` + `store.restoreIncomplete`）。

### 4.2 直写 + 三层安全网

写入**没有中间层**：`store` 不做路径重定向，也没有单独的落盘步骤或开关。
界面显示的目标路径就是实际被写的路径 —— 一旦两者可能不一致，UI 本身就成了误导。

既然写入直接落盘，安全靠三层网兜住：

| 层 | 机制 | 落点 |
|---|---|---|
| ① 写前快照 | 每次写入前把被触及路径的当前状态读入内存（含 `existed` 标记） | 进程内存，不落盘 |
| ② 事务留痕 | 每笔写入向 `audit.jsonl` 追加一行（只追加日志） | `<store>/` |
| ③ 失败自动还原 | 任一 op 失败即整笔还原；还原不掉的逐条报告 | `store.snapshotTooLarge` / `store.restoreIncomplete` |

写入前另有**二次确认**：新增 / 修改 / 删除都会弹窗，列出即将写入的真实路径。
设置页除外 —— 设置写的是应用自己的 store，不碰 agent 目录。

设计要点：

- **快照先于写入**。任何一步失败都能回到写入前状态，不存在「改了一半又没快照」的窗口。
- **快照只在内存里**。没有 `backups/` 目录，也就没有「操作记录里点一下撤销」这条路径：
  日志是「改了什么」的记录，不是撤销菜单。
- **`json` op 以真实文件为基底**。因为直接读写真实路径，改写 `mcp.json` 时兄弟 server
  天然保留，不需要额外的三方合并逻辑。
- **删除时向上清理空目录**。还原一个新建 skill 时，`skills/<name>/` 会留下空壳——
  部分 agent 会把空目录也枚举出来。
- **损坏的 JSON 显式中止**，不当作空配置覆盖，避免静默丢掉兄弟条目。

**一次写入的预期结果**：
MCP 调 `create_skill` 之后，`~/.workbuddy/skills/<name>/SKILL.md` 文件存在，
`audit.jsonl` 中有一行对应这个 `txId`，且 store 根目录下不会出现 `backups/`。

### 4.3 校验规则（节选）

| 资产 | error（阻断） | warn（放行） |
|---|---|---|
| skill | 无 `name` / `name` 非 `^[a-z0-9][a-z0-9-]{0,63}$` / 缺 `description` / body 为空 / name 是 Windows 保留名 | name 与目录名不一致、description > 1024 字符 |
| mcp | 无 `command` 且无 `url` / `args` 非数组 / `env` 非对象 / name 非法 | 未知 key、`npx` 缺 `-y` |
| rule | 文件为空 | > 200 KB（context 预算警告） |
| plugin | 无 `name` | version 非 semver |

`name` 与目录不一致之所以只是 warn：改名等价于目录移动，需要 move 而非 write，
在编辑器里直接改会造成缓存键错位——实现上 `saveMarkdown` 对这种情况**硬阻断**并提示。

---

## 5. UI 设计

前端只有一份源码：`src/renderer/` 是唯一真源，`tauri-shell/copy-frontend.js`
生成 `tauri-shell/dist` 时只注入两处差异（`<html data-shell="tauri">` 与 `tauri-bridge.js`），
`npm run tauri:check` 保证不漂移。

### 5.1 信息架构

| 视图 | 回答的问题 |
|---|---|
| **概览** | 我这台机器上一共有多少 agent 资产？谁在吃 context？最近改了什么？ |
| **资产** | 某一个资产具体是什么？归谁所有？可编辑吗？ |
| **Agent** | 装了哪些 harness，各自认哪些目录？ |
| **项目** | 哪些仓库带 agent 文件？各贡献了多少？ |
| **健康检查** | 哪些文件 agent 会静默忽略或读错？ |
| **操作记录** | 谁在什么时候改了什么？ |

### 5.2 关键交互

- **默认过滤掉缓存噪声**。插件里绝大多数是 host-owned 缓存。资产页默认显示
  "Curated + managed"，切到 "Everything incl. caches" 才全量列出。健康检查永久排除缓存，
  否则厂商的错会被误报成用户的错。
- **写入前二次确认**。新增 / 修改 / 删除都在落盘前弹确认框，框内列出即将写入的真实路径。
  设置页不弹：它写的是应用自己的 store，与 agent 目录无关。这条与「直写」配套存在 ——
  写入直接落盘，人必须在写之前就看清楚落点。
- **Diff 先行**。Save 前有 "Preview changes" 生成行级 diff，`Ctrl+S` 直接保存。
- **只读资产显式声明**。插件缓存 `writable:false`，UI 置灰并显示 read-only chip。
- **快捷键**：`/` 聚焦搜索，`Esc` 关闭，`Ctrl+S` 保存。

### 5.3 安全基线

CSP 有两处声明，浏览器对两份**各自独立执行、取交集**：

| 位置 | 策略 |
|---|---|
| `tauri-shell/src-tauri/tauri.conf.json:13`（webview 层） | `default-src 'self'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; connect-src 'self' ipc: http://ipc.localhost` |
| 前端 `index.html:7` 的 `<meta http-equiv="Content-Security-Policy">`（文档层，更严） | `default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; img-src 'self' data:; connect-src 'none'` |

文档层更严，所以 `connect-src` 实际是 `'none'`，配置里那份 `'self' ipc: http://ipc.localhost`
并不放开任何网络；应用照常 IPC 是因为 Tauri 2 的 invoke 走宿主对象 `postMessage`，不走 `fetch` / `XHR`。

- 无 Node 集成：渲染层拿不到 `require` / `fs`，只有 `tauri-bridge.js` 暴露的具名函数。
- `withGlobalTauri: true` 是内部 IPC 的必要条件，不是一个 API 面。
- 路径一律从注册表反查，**不接受渲染层传入的裸路径**（唯一例外是"导入项目"，且必须是已存在目录）。
- 插件只授 `dialog`（选目录）与 `opener`（在资源管理器中揭示）；没有 fs、没有网络。
- **`style-src` 不带 `'unsafe-inline'`，已收口，勿加回。** 依据是实测不是推断：
  - 两个安装包、只翻转 `style-src` 一个字符串，各自静默装到独立目录，用同一份 store 快照
    各截 5 个视图逐像素比对（2182×1466，`SetProcessDPIAware` + `PrintWindow`）：
    **项目 / 设置 / 健康三页 0 / 3,198,812 像素不同，PNG 逐字节相同**；概览页 185 像素，
    是 "7m ago" → "8m ago" 这种相对时间；资产页 139,967 像素，是一行 `:hover` 底色
    随鼠标位置漂移——同构建的 A/A 对照是 140,213 像素，噪声本身就有这个量级。
  - 四个"需要点进去才看得到"的状态同样比过（用 CDP 在页面里点开／弹出，不改任何源码）：
    **模态框 0 / 3,198,812 像素，逐字节相同**；资产抽屉 709 像素，是窗口标题栏
    激活／非激活态的文字颜色；toast 192 像素，是底下那行相对时间；右键菜单 182,059 像素，
    同样是某一行 `:hover` 底色。**右键菜单这个 CSSOM 定位的浮层，两组坐标完全一致**
    （`left:760px` / `top:430px`，7 个菜单项），即"悬浮卡片定位"没有受策略影响。
  - 原因：动态样式全部走 CSSOM（`applyStyle()` 用 `element.style.setProperty`），
    静态样式写成类名；CSSOM 写入不受 `style-src` 约束，受约束的是内联 `style="…"` 属性
    与脚本插入的 `<style>`。
  - 在两份构装上用 CDP 逐字段比对，结果完全相同，佐证 `'unsafe-inline'` 从未真正放开过东西：
    `element.style.setProperty('width','123px')` → 123px（生效）、
    `setAttribute('style','width:77px')` → 仍是 123px（被忽略）、脚本插入的 `<style>` → 不生效。
  - 局限（如实记）：两次构建之间 `core/update/manager.rs` 有并行改动（同仓库多 agent 协作），
    但含"更新分区"的设置页逐字节相同，该改动对结论无影响。

---

## 6. 验证

### 6.1 测试套件

```bash
cd tauri-shell && cargo test --workspace   # 单元 + 集成，断言数随覆盖增长
```

| 套件 | 覆盖 |
|---|---|
| `core::*` 单测 | 格式层往返、校验器、安全模式、内存快照与原子写、失败还原、适配器快照对齐、MCP dispatch |
| `tests/contract.rs` | IPC / 数据契约、资产读写、注册表、格式解析（用真实 keeper 跑临时 store） |
| `tests/write_path.rs` | 事务、journal、失败还原、校验与安全扫描 |
| `tests/boundary.rs` | 并发与边界：多线程写、journal 完整性、路径与超时 |
| `core/tests/mcp.rs` | MCP 工具 dispatch + 真实子进程协议 |
| `tests/update.rs` | 更新链路：真实 `TcpListener` 起 HTTP 服务、参数守卫、版本比较与安装包挑选 |
| `npm run tauri:i18n` | Rust 引擎错误文案的中文覆盖：漏一条 exit 1（见 §2.1） |

**分层原则**：纯函数的边界值（参数守卫、版本比较、域名白名单、文件名清洗）在 `core` 的单元测试里，
集成测试不再重复——两边调用的是同一个函数。下载、调度、journal 这类有状态的端到端行为两层都保留：
core 侧用 mock 定位快，集成侧用真实 HTTP / 文件系统验证真实路径。

**不写指向已删除文档的引用**
「参考实现 §9」「IPC-contract section 5」这类指针在文档重组后极易失效：读者无法查阅，
编号也失去意义。注释只写**行为本身**（「保存设置会追加一条审计记录」），不写指向某个编号的指针。
JS 语言的**语义事实**（`String(1.0)` 是 `"1"`、`\s` 的空白集、`javascript:` 是 URL scheme）
要保留——那是 Rust 代码这么写的理由，与是否还有 JS 实现无关。

前端静态校验（纯 Node，不启浏览器，全量 ≈0.5 s）：

```bash
npm run check                # 残留扫描 · 桥接↔Rust 命令契约 · renderer 规则 · UI 审计 · i18n · dist 一致性
node scripts/check.js bridge # 单独跑某一道：residue / bridge / renderer / ui / i18n / dist
```

### 6.2 MCP 协议回归

`cargo test -p agentkeeper-core --test mcp` 起真实进程，按客户端顺序走完：
`initialize` → `notifications/initialized`（必须静默）→ `tools/list`（21 个，名字顺序钉死）→
`list_assets` / `read_asset` / `stats` / `health_report` → `create_skill`（直接写入真实路径，
只在 `audit.jsonl` 留一行）→ 已下线的 `rollback_transaction` / `list_transactions` 必须
返回 `-32601` → `-32602` / `-32700`（`id:null`）→
无 id 请求与空行静默 → **stdout 行数必须等于应答复的请求数** → 关 stdin 退出码 0。

### 6.3 打包验证

| 项 | 结果 |
|---|---|
| 安装包 | 3.86 MiB |
| 静默安装 | `/S /D=<dir>` → exit 0，无需提权（`currentUser`） |
| 安装后 | 21.4 MiB；无 `dist/`（前端已内嵌） |
| `demo-projects` | 不随包发布：`bundle.resources` 中没有该目录 |
| 冷启动 → 窗口可见 | 0.76 / 0.78 / 0.83 / 0.85 / 0.93 s |
| 内存 | shell 69 MB（private 46 MB）；整棵进程树含 WebView2 ≈ 480 MB |
| 关闭 | WM_CLOSE → 50 ms 内 exit 0；WebView2 子进程 8–22 s 内自行退出（运行时回收，非泄漏） |
| 首启扫描 | 990 个资产 / 519–558 ms |

安装版截图可在 Windows 桌面用 `PrintWindow` 方式生成（产物不入库）。

### 6.4 Windows 桌面的两个约束

**窗口渲染依赖 WebView2 启动参数。** 部分机器无法初始化 Chromium OS 沙盒，WebView2 渲染
进程会以 `STATUS_BREAKPOINT`（`0x80000003`）退出，现象是窗口不出现、也没有
`msedgewebview2.exe` 子进程。`lib.rs::run()` 在创建 webview 前无条件设置
`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`（`--no-sandbox --disable-gpu-sandbox`，release
同样生效；用户已显式设置时不覆盖）。GPU 开关不再额外追加：崩溃根因只有 OS 沙盒一条。

**截图工具必须声明 DPI 感知。**

`PrintWindow` 抓图（不抢前台，前台被全屏程序占据时也能抓）。
PowerShell 进程默认 **DPI-unaware**：Windows 常见 150% 缩放（144 DPI）下，
`GetWindowRect` 返回虚拟化尺寸（1455×977），而 `PrintWindow` 按真实物理表面（2160×1410）绘制，
位图只装得下左上 67%。脚本开头 `[AkWin]::SetProcessDPIAware()` 后位图变为 2182×1466。
判据：`.app` 栅格是 `248px 1fr`，侧栏实测 372 device px 即 ×1.5。

### 6.5 UI 回归

界面正确性由真机 + CDP 验证：布局溢出、逐元素对比度、`focus-visible`、Tab 序与焦点陷阱、
缩放三档。静态门禁只覆盖可离线判定的部分（i18n 对齐、桥接契约、dist 一致性、死交互），
不替代真机走查。

---

## 7. 已知限制

| 限制 | 影响 | 后续 |
|---|---|---|
| 写入后 agent 需重启/重扫才生效 | 面板不显示"已生效"实时态 | 接入各 agent 的 reload 信号 |
| 插件缓存不可编辑 | 只能看不能改 | 引导用户去对应 agent 的插件 UI |
| symlink 去重按字面路径 | 同一物理文件的两个链接算两条 | `realpath` 归一 |
| WebView2 运行时未随包分发自带 | 目标机需装 WebView2 Runtime（Win11 自带） | NSIS 增加 bootstrapper 选项 |

> 事务写入对损坏的 JSON 配置会显式中止（`config file is corrupt, refusing to overwrite`），
> 不会把损坏文件当作空配置覆盖，避免静默丢失兄弟条目。
>
> YAML/TOML 均为完整解析（`serde_yaml_ng` / `toml`，开 `preserve_order`）：SKILL.md frontmatter、
> YAML MCP 配置、TOML MCP 配置。**TOML 写入按区段手术**，只重写目标服务器的小节，
> 其余内容（含注释）逐字节保留。非法 YAML 回退为逐行标量提取，尽量不丢字段。

---

## 8. 共享层（v0.3）

同一份 skills / MCP / 规则 / 记忆不再逐 agent 复制，而是**库内一份主副本 + 各处指向它**。
本章是这一层的完整规范：先讲不变量与结构（§8.1–§8.5），再讲链接内核与事务（§8.6–§8.7），
然后按资产分述共享方式（§8.8–§8.10），接着是扫描与健康检查（§8.11）、界面契约（§8.12）、
对外接口（§8.13）、怎么用（§8.14）、边界与故障排查（§8.15–§8.16），最后是实现时的纪律、
验收方式与反复踩到的坑（§8.17–§8.19）。

### 8.1 硬约束

这一层写代码前要先记住的十一条。违反任何一条都会被某道关口打回，没有例外。

| # | 约束 | 违反的后果 | 怎么被抓到 |
|---|---|---|---|
| C1 | **`core::` 不许碰 Tauri 类型** | 编译期直接断（这是设计） | 拆 crate 保证 |
| C2 | **所有写入必须走 `store.transaction()`**（唯一的例外是 §8.17.3 的库位置迁移） | 快照 / 还原 / journal 全部失效 | `src-tauri/tests/write_path.rs` |
| C3 | **Rust 侧只发英文句子或 code，中文一律渲染层翻** | `tauri-i18n.js` exit 1 | `check.js` 的 i18n 关口 |
| C4 | **新 code 必须同时进 `engine-zh.js` 的 `CODE_ZH`** | CODE-MISS 红灯 | `tauri-i18n.js`（扫 `core/src` 与 `src-tauri/src` 全部 `.rs`） |
| C5 | **每个新 `#[tauri::command]` 必须在 `tauri-bridge.js` 有形状匹配的 `call(...)`** | bridge 关口红灯 | `check.js`（比对参数形状，`Option<T>` 算可选） |
| C6 | **不许破坏 TOML 注释**（只许 `toml_splice` 按区段手术） | 用户配置被抹 | 逐字节回归夹具 |
| C7 | **不引入镜像树 / 暂存层**，写入直落真实路径 | 界面显示与实际不一致 | `LIST_LIBRARY` 的 `file` 字段与写入回显同源 |
| C8 | **`adapters.rs::AGENTS` 与 `scripts/adapter-table.js` 必须同步**，改完跑 `npm run tauri:adapters` | 适配器快照测试红 | `adapters.rs` 内置快照（17 个 agent / 187 条路径模板） |
| C9 | **禁止手改 `tauri-shell/dist/`**，只由 `node tauri-shell/copy-frontend.js` 生成 | dist 一致性红灯 | `check.js` 的 dist 关口（`--check`） |
| C10 | 注释用中文；`//!`、`///` 也算 | `npm run comments:lang` 进度倒退 | 人工 |
| C11 | **新增前端脚本文件要在七处登记**（§8.19.1） | 打包后空白页，本地却全绿 | 只有 `checkGlobalCollisions` 会间接抓到一部分 |

### 8.2 四种共享方式

按资产形态分，不混在一个开关里。四类资产的失败模式完全不同，混在一起会让「部分成功」
无法解释。

| 资产 | 形态 | 共享方式 |
|---|---|---|
| Skill | 目录（`SKILL.md` + 附属文件） | **链接**：整目录指过去，agent 读的路径不变 |
| MCP Server | 配置文件里的一个**条目** | **托管物化**：库内一份规范形态，开关决定写入 / 删除这一条 |
| Rule | 单个 `.md`（`AGENTS.md` / `CLAUDE.md` / `.mdc`） | **链接**或**区块合成**（`<!-- agentkeeper:begin/end -->`） |
| Memory | 文件或目录（`MEMORY.md` + `memory/YYYY-MM-DD.md`） | 目录走链接；文件走链接或区块合成 |

**不做的**：不做「一个开关管四类资产」。

### 8.3 链接能力与降级链

`linkfs.rs` 在目标机上**实测**能力，而不是从 PowerShell 的结果外推
（`New-Item -ItemType SymbolicLink` 被拒的会话里，Rust 的 `symlink_dir` 照样成功——
两者走的是不同的 API 路径）。

| 用途 | 顺序 | 说明 |
|---|---|---|
| 目录 | `symlink → junction → Err` | junction 要自己发 `FSCTL_SET_REPARSE_POINT`（std 没有 junction API），本机普通用户会话可用，不需要开发者模式 |
| 单文件 | `symlink → hardlink → Err` | 硬链接只同卷，且 agent 用「写临时文件 + rename」保存时会被打断，所以不当首选 |

`copy` **不由 linkfs 产生**：它是 `Op::copy_dir` 的语义，由调用方显式选择。全链路只在
「本机连链接都建不出来」时退化成受管副本，界面上以副本模式横幅说明。

`file_type().is_symlink()` **认不出 junction**，所以捕获链接一律走 `linkfs::entry_link_kind`。
快照里若把 junction 当成普通目录，会顺着链接把整个库内容包一遍、还原时写成真实副本
—— 静默、昂贵、不可察觉。

`linkfs.rs` 的对外 API：`capability`（探测并缓存进 `<store>/linkcap.txt`）、
`create_dir_link` / `create_file_link`（返回实际用上的 `LinkMethod`）、`read_target`
（junction 与 symlink 都要能读，必须走 `FSCTL_GET_REPARSE_POINT`）、`is_link`、`link_kind`、
`unlink`（只删链接本身，不跟随）、`same_target`、`entry_link_kind`。

`same_target` 不能只信 `canonicalize`：它只做词法归一 + 链接跟随，不做 UAC 别名归一
（`C:\Program Files` 与 `C:\Program Files (x86)`），也不统一盘符大小写。所以是
「先 `canonicalize` → 失败（悬空链接）时退回父目录归一 + 文件名 → 最后统一小写再比」。

### 8.4 库结构

```
~/.agents/                    ← 默认库根（社区约定；也是共享技能目录 ~/.agents/skills 的父级）
<store>/library|skills…       ← 可切换为库根（%APPDATA%\AgentKeeper\agentkeeper）
├── skills/<slug>/            ← skill 主副本（目录名恒等于 slug）
├── mcp/servers/<slug>.json   ← MCP 单一事实源（规范形态 + allOf 启用映射）
├── rules/<slug>.md           ← 规则片段
├── memory/<slug>…            ← 记忆
├── links.json                ← 链接台账
└── .trash/                   ← 库内删除的回收站（保留 trashDays 天）
```

库根取 `settings.libraryRoot`，为空时默认 `~/.agents`。**解析库根不创建目录**。默认位置与
扫描里的伪 agent `shared`（`~/.agents/skills`）重合，所以收敛之后「库内主副本」就是那个
位置本身，资产条数不会因为多出一个库目录而翻倍。

MCP 记录形如：

```jsonc
{
  "schema": 1,
  "name": "fetch",
  "transport": "stdio",          // stdio | http | sse
  "command": "uvx",              // stdio 必填
  "args": ["mcp-server-fetch"],
  "env": {},
  "url": null,                   // http/sse 必填
  "headers": {},
  "allOf": { "claude-code": true, "codex": true, "pi": false }
}
```

不存任何 agent 特有的封装。写盘时按 `mcp_bucket_for` 决定桶名（只有 `copilot` 用
`servers`，其余 `mcpServers`），按 `mcp_target_for` 决定落哪个文件（它已过滤 `approvals`
这类记账文件），选定文件在写入预览里显示。

### 8.5 不变量

1. **库内主副本是唯一可编辑对象**；agent 侧链接只读。
2. **删除链接 ≠ 删除资产**：停用某 agent 只删那条链接；删资产必须先移入 `.trash` 并记审计。
3. **内容不一致绝不自动收敛**：只报告 + 给 diff，裁决权在用户。
4. **写入落点必须与界面显示一致**：预览里写的路径就是事务实际写的路径。
5. **库外的东西不碰**：未导入的 server、未纳管的链接、用户手写的规则，一律只读、只报告。
6. **每个链接都可解释**：点开任一行都能回答「指向哪、谁建的、什么时候、用哪种方式」。

### 8.6 链接内核与事务

`store::OpKind` 增加 `LinkDir` / `LinkFile`（`Op::link_dir` / `Op::link_file`），与既有的
写 / 拷贝 / 删除同走 `store.transaction()`：写前快照、失败整笔还原、行 `audit.jsonl`，
外加失败码 `store.linkFailed`。

- `transaction()` 的前置校验扩成三个：`CopyDir` / `LinkDir` / `LinkFile` 都要求 `from`。
- 快照侧记下链接的**种类**：`DirSnap::Link` / `Snapshot::Link` 都带 `kind`，还原时按种类
  分派（junction 还原成 junction、硬链接还原成硬链接），否则一次失败的事务会把用户的
  junction 悄悄换成 symlink。
- `collect_dir_seen` 必须用 `linkfs::entry_link_kind` 而不是 `ft.is_symlink()`，否则 junction
  根本进不了快照。
- 「卸链接」与「建链接」必须在**同一个事务**里：目标已存在时 `linkDir` 会直接拒绝，所以
  真实目录必须先删；只有同一事务才可能一起回滚。

### 8.7 台账与回收站

`<library>/links.json` 是链接的**索引**：每条记 `kind` / `name` / `src` / `dst` / `method` /
`mode`（`link` | `copy`）/ `owner`（`library` | `external`）/ 时间戳。真相永远是磁盘。

- 台账**损坏时不当作空**：`load_ledger` 返回 `Err`，调用方报 `link.ledgerDrift`——把损坏
  读成「没有链接」会让健康检查误以为库是干净的。
- `save_ledger` 必须在事务**成功之后**调用；台账可以坏，但**不允许**用台账覆盖磁盘。
- 写入前要拿 `<store>/.lock` 上的跨进程排他锁（桌面端与 MCP 是不同进程，同一份台账、
  `settings.json`、`projects.json` 都是读-改-写）。拿不到锁时返回 `store.busy`——宁可失败
  并给出可读的 code，也不要两边各写各的。
- 库内资产删除：移入 `<store>/.trash/<kind>/<slug>-<ts>/`，然后删全部链接，每次移动与每条
  链接删除各记一行审计。跨卷走 `copy + rm` 时，拷完要核对两边的目录摘要，不一致就放弃
  整次删除并报 `library.trashFailed`（半个回收站等于数据丢失）。
- `settings::ensure_store` 在启动时清掉超过 `trashDays`（默认 30 天）的条目。与「启动即删
  `backups/`」同一条规矩：尽力而为、绝不阻塞启动。

### 8.8 Skills 共享与收敛

**收敛探测**（`sharing::plan_converge`，纯只读：不写盘、不建目录、不改台账）把同名 skill
按逐文件 SHA-256 分成三种裁决：

| 情况 | 裁决 | 动作 |
|---|---|---|
| ≥2 份，逐文件摘要全等 | `Identical` | 保留一份入库存，其余替换为链接 |
| ≥2 份，任一文件不同 | `Diverged` | **不动**，只列差异文件名与状态 |
| 只有 1 份 | `SingleSource` | 并入库，原位置替换为链接 |

- 摘要口径：按相对路径排序、逐文件流式算 SHA-256（skill 里常有几十 MB 的二进制），再对
  「路径 + 摘要」列表整体算一次；排除 `SKIP_DIRS`。
- 比较时还要比**条目种类**：一个文件与一个指向同内容的链接，「逐文件读内容」会把它们判
  成相同，但收敛后语义变了 —— 种类不同一律算 `Diverged`。
- `Diverged` 的组在界面上不能应用；`converge.diverged` 拒绝整组，零写入。

**启用**（`Keeper::enable_asset`）的 op 序列顺序不能变：

```
1. Op::mkdir   <agent 侧 skills 父目录>
2. Op::delete  那个真实目录（仅当 agent 侧已存在真实目录、内容一致、且不是链接）
3. Op::link_dir <agent 侧 skills/<slug>>   ← from = 库内路径
```

agent 侧已经是**指向该主副本**的链接时是空操作（`ok:true` + `skipped: already linked`），
不算错误；指向别处的链接会被重指回来。缺 `assetId` 时服务端现扫一次、按
(kind, slug, agentId) 找到盘上那份真实副本——agent 手里通常只有名字。

**停用**（`Keeper::disable_asset`）的三道闸，缺一不可：

1. 目标必须是链接——是真实目录就拒绝，报 `share.notLinked`；
2. 链接必须指向**库内**（`Library::owns_target`，硬链接由台账作证）——指向库外就拒绝，
   报 `share.foreignLink`；
3. **绝不删除库内主副本**。

链接已经不在了返回 `ok:false` + `share.notLinked`，不是成功的空操作。

### 8.9 MCP 单一事实源

打开某 agent 的开关 = 事务里对该 agent 的配置文件做 `Op::json_mutate`（JSON）或
`toml_splice`（TOML）**插入 / 更新这一条**；关闭 = **只删这一条**（`mcp_disable: "flag"`
的 agent 改为置 `enabled: false`，不删条目）。

- 关闭时必须只动这一条：JSON 里其余键与键序保持（`preserve_order`），TOML 里其余注释
  逐字节保留。**不允许**为图快改用整份 TOML 序列化——那会把用户注释全部抹掉。
- `enabled: false` 与「不存在」是两种状态，按 agent 在适配器表里声明 `mcpDisable`。
- **漂移检测**：逐份配置文件把库内规范形态与盘上条目比对，不一致报 `mcp.drift`，给三种
  处理：以库为准覆盖 / 以该 agent 为准回写库 / 标记为独立（从 `allOf` 移除）。
  **同名不同参绝不覆盖**——覆盖是静默丢数据。
- 非 stdio 传输下不适用的字段（如 sse 的 `args` / `env`）不会被物化，且会出现在应答的
  `skipped` 里，不静默丢弃。
- 从未导入过的、库外的 server 一律不报告、不触碰。

### 8.10 规则与记忆：两种模式

| 模式 | 适用 | 实现 |
|---|---|---|
| `link` | 所有 agent 读同一份 | 文件链接（symlink 优先，回退 hardlink）；agent 侧已是真实文件则先删 |
| `compose` | 几段规则拼一份 | 读目标现文 → 插入 / 替换区块 → 写回 |

区块标记固定形态，不要改：

```markdown
<!-- agentkeeper:begin slug=team-conventions rev=9f2c1d -->
…内容…
<!-- agentkeeper:end slug=team-conventions -->
```

- `rev` 是内容摘要的前 6 位，用来判断「盘上这份是不是我们上次写的那份」。
- **标记之外的内容逐字节保留**：区块操作只替换 `Block.start..Block.end` 这一段。
- **换行不混写**：沿用目标文件第一次出现的换行风格（`\r\n` 或 `\n`），两个方向都不混。
- **幂等**：`link` 已指向库内、`compose` 算出来的文本与原文相同，都是零 op（零事务，
  连点两下不产生垃圾事务）。
- **区块被外部改过时**不静默覆盖：返回 `rule.foreignEdit`，界面弹两方对比（盘上版本 /
  库内版本），由人选择。选「以库内为准」是唯一绕过闸门的方式，且必须是人看过的对比。
- 首次 `compose` 会把库内主副本落进库（同一事务）：单一事实源对 `compose` 同样成立，
  否则每次同步都拿「盘上自己的内容」当库内版本，强制重建时区块会层层嵌套。
- 主副本按目标文件名归一（`AGENTS.md` → `<root>/rules/agents.md`），所以不同 agent 的同名
  规则文件天然收敛成一份；`CLAUDE.md` 与 `AGENTS.md` 是两份主副本，不做隐式合并。
- **目标文件读不出来时一律拒绝**并提示，绝不把读失败当成空文件——那等于清空用户内容。
- `.mdc`（Cursor 规则的 `alwaysApply` / `globs` frontmatter）**只支持 `link`**，`compose` 的
  选项置灰并给出原因；不做 frontmatter 合并。
- 台账只记 `link`：`compose` 没有「链接」可记，链接方式从盘上读回，不猜。

### 8.11 扫描与健康检查

**扫描打标**：资产的 `meta.link` 记 `{ method, src, managed }`，不是链接时为空。资产的 id 由
目录字符串决定、链接不参与，所以把真实目录换成链接后 **id 不变**——这是「收敛后资产条数
不涨」的前提。

**effective**：同一 `kind:slug` 组里 `canonicalize` 相同的两份互为 `sameInode`，**不判遮蔽**
（同一份东西不存在冲突），并且该字段要一路带到渲染层。

**健康检查**（`doctor::diagnose`）**只读**：没有 `&Store` 形参、不探测链接能力、不写审计与
台账、不碰磁盘。七类发现：

| code | 级别 | 判据 | 修复 |
|---|---|---|---|
| `link.dangling` | critical | 链接能读出目标但目标不存在（`targetMissing`，可重连）；或主副本目录在但没有 `SKILL.md`（`masterUnreadable`，修不了） | `relink`（仅前一种） |
| `link.unmanaged` | info | 目标存在但不在库根下 | `adopt`（导入库） |
| `link.hijacked` | critical | 台账有记录、路径存在但**已不是链接**（被 agent 换成了真实目录 / 文件） | `relink`（仅内容一致时） |
| `link.fragile` | info | 是硬链接（agent 保存时可能被打断） | — 只提示（桌面与 MCP 都回 `doctor.notFixable`） |
| `asset.duplicated` | info | 同一 slug 有 ≥2 份真实目录且逐文件摘要相同 | `converge` |
| `mcp.drift` | warn | 库内规范形态与盘上条目不一致（逐份配置文件） | 三选一 |
| `link.ledgerDrift` | warn | 台账与磁盘的其余不符：台账损坏 / 目标读不出 / 指向别处 / 路径消失 / 库内链接未登记 | `rebuild`（以磁盘为准重建） |

几条容易写错的边界：

1. **「被顶掉」与「台账漂移」要分开**：路径上还是链接 → `link.ledgerDrift`；变成真实
   目录 / 文件 → `link.hijacked`；什么都没有 → `link.ledgerDrift`。三种修法完全不同。
2. **`copy` 条目直接跳过**：受管副本本来就不是链接，报成「被顶掉」是误报。
3. **硬链接的判定要排在读目标之前**：硬链接没有可读目标，先读会变成 `unreadableTarget`。
4. 台账与扫描两边都看见同一条链接时只报一条（按 `(code, 小写路径)` 去重），code 不同则各报各的。

### 8.12 界面契约

**视图**：导航「共享」页，五个标签（技能 / MCP / 规则 / 记忆 / 健康）。注册要三处同时改，
只改一处会出现「页签不出现」或「点了掉回首页」：

```js
// src/renderer/app.js 的 VIEWS
shared: { titleKey: 'nav.shared', subKey: 'shared.subtitle', render: () => window.__akShared.viewShared() }
```

```html
<!-- src/renderer/index.html —— Discover 之后、nav-group-operate 之前 -->
<button class="nav-item" data-view="shared"><span class="nav-ico">⇄</span><span class="nav-text" data-i18n="nav.shared">Shared</span></button>
```

外加 `i18n.js` 的 `nav.shared`（中英各一条）；缺 `data-i18n` 会让语言切换后标签停在英文。

**矩阵**：行 = 资产，列 = Agent，单元格状态共七种：

| state | 含义 |
|---|---|
| `on` | 已启用（链接 / 已物化 / 区块已在） |
| `off` | 未启用 |
| `copy` | 受管副本模式（本机建不了链接） |
| `foreign` | 被顶掉：agent 侧换成了自己的真实副本 |
| `drift` | MCP 条目与库内不一致 |
| `unsupported` | 该 agent 没有可写位置，附 `reason`（`noSkillsDir` / `noRulePath` / `noMcpConfig` / `noStyle` / `noSource`） |
| `error` | 读盘失败等错误 |

- 行数据来自一次 `list_library`，不要每行一次调用。
- 点单元格 = 立即执行，但执行前仍走 `confirmWrite`；列头批量只弹一次确认并列出全部真实
  路径（超过 10 条自动折叠）；完成后局部重渲染那一行。
- 失败单元格直接给原因与重试，不弹新页面；`restoreErrors[]` 逐条列出。
- 副本模式（`cap.dir_order[0] === 'copy'`）时页面顶部常驻横幅 + 「重新扫描」按钮。

**收敛向导**：三步（探测 → 逐项裁决 → 确认执行），每步都可返回；第二步默认全不选，
逐条可取消。确认框列出保留哪一份（真实路径）、会替换哪几份、每条新链接的方式。

**写入前确认**沿用既有 `confirmWrite`，但列表里必须写出**将要删除的路径**与**将要创建的
链接**及其方式。

**i18n**：共享页新增文案统一前缀 `shared.` / `matrix.` / `converge.` / `doctor.`。`i18n.js`
的 `en` 与 `zh` 两个对象必须成对——漏一边 parity 会红，多一条没人用的也会红，所以别提前
把用不上的词条写进去。Rust 侧只发英文 code 或句子，新 code 同时进 `engine-zh.js` 的
`CODE_ZH`。

**命令与桥接**：八个命令，`src-tauri/src/lib.rs` 与 `tauri-bridge.js` 成对添加。

| 命令 | 桥接写法 | 返回 |
|---|---|---|
| `list_library` | `call('list_library')` | `{ ok, root, cap, agents[], skills[], mcp[], rules[], memory[] }` |
| `converge_plan` | `call('converge_plan')` | `{ ok, groups[], savedBytes, actionable }` |
| `apply_converge` | `call('apply_converge', { payload: input })` | 事务结果 + `written[]` |
| `enable_asset` | `call('enable_asset', { payload: input })` | 事务结果 |
| `disable_asset` | `call('disable_asset', { payload: input })` | 事务结果 |
| `shared_doctor` | `call('shared_doctor')` | `{ ok, findings[], counts }` |
| `apply_finding_fix` | `call('apply_finding_fix', { payload: input })` | 事务结果 |
| `library_migrate` | `call('library_migrate', { payload: input })` | `{ ok, moved, total, links, resumed }` |

`list_library` / `converge_plan` / `shared_doctor` 要遍历库目录并做哈希，属于耗时命令：
照 `scan` / `read_asset` 的先例用 `async fn` + `spawn_blocking`，不要在 UI 线程里同步跑。

桥接关口的形状规则：`call('x', {...})` 的键必须是 Rust 形参名的 camelCase，**多余键也
失败**；`Option<T>` 算可选；传裸值或先拼好对象再传变量会**绕过**形状检查，所以新命令一律
写成内联对象字面量。

### 8.13 对外接口：CLI 与 MCP

**MCP** 六个工具追加在工具表末尾（改名对已经记住名字的 agent 是破坏性变更，只能追加）：

| 工具 | 参数 | 说明 |
|---|---|---|
| `list_library` | 无 | 矩阵数据（库内资产 + 各 agent 是否启用） |
| `enable_asset` | `name, kind, agentId, scope?, projectId?, file?, config?, assetId?` | 走同一事务 |
| `disable_asset` | 同上（不需要 `config`） | 走同一事务 |
| `link_doctor` | 无 | 七类发现，只读 |
| `converge_plan` | 无 | 只出方案，零写入 |
| `apply_finding_fix` | `code, path?, fix?, detail?, choice?` | 修**一条**发现，走同一事务 |

`apply_finding_fix` 能修 `link.dangling`（重连）、`link.hijacked`（内容一致时重连）、
`link.unmanaged`（导入库）、`link.ledgerDrift`（以磁盘为准重建）、`mcp.drift`（`choice` 三选一：
`library` / `agent` / `independent`）；`link.fragile` 没有可自动执行的修法，`asset.duplicated`
**显式拒绝**并回 `doctor.notFixable`——批量收敛一次会动几十个位置，只留在桌面向导里。

`enable_asset` 在 `kind=mcp` 且库内还没有这条记录时先把它写进库（`config` 即定义），再物化。

**MCP 侧刻意不做的事**：一键收敛与批量修复。`converge_plan` 只出方案、`apply_finding_fix`
一次只修一条，应用收敛要在桌面端的收敛向导里点——那条路径需要「确认」这一步，而 MCP 没有
二次确认的界面。

**`ak-scan --shared`** 把 `{ library, ledger, findings }` 打成一份文档，供 shell 里逐字段
diff；纯只读，不建目录、不写台账。台账损坏时 `ledger` 是 `{ ok:false, error }` 而不是空
列表。无参数时输出一个字都没变（CI 依赖它）。

**`bootstrap`** 只回 `shared: { enabled, root }`：收敛候选数要遍历整棵库与全部 agent 目录，
而 `bootstrap` 是同步命令、跑在窗口绘制之前，塞进去等于给每次启动加一次全盘扫描。候选数
在共享页打开时由 `converge_plan` 现算，且**默认不收敛**。

### 8.14 使用方法

**把一份技能变成共享的**：资产页选中 → 分发到目标 agent 之前，到「共享」页看「技能」
标签。重复的会先在页面顶部提示一次（「发现有 N 组技能可以共享库内同一份副本」），点
「去查看」进收敛向导：第一步列出每一组、保留哪一份、替换哪几份；第二步逐组勾选（默认
全不选）；第三步确认执行。内容不一致的组不给应用，只列差异文件。

**让某个 agent 用/不用某一项**：在矩阵里点那个单元格；点列头是一整列。每次都会先弹确认
并列出真实路径。停用只断这个 agent 的那一条，库内主副本不动。

**规则与记忆**：在「规则」/「记忆」标签里选共享方式。`link` 适合所有 agent 读同一份；
`compose` 适合把库内几段拼进某个 agent 自己的规则文件——标记之外的原文逐字节保留，区块
被外部改过时弹两方对比由你决定保留哪一边。

**换库位置**：库根那一行有「迁移库…」，可选默认位置（`~/.agents`）或应用私有目录。迁移
逐条重指链接、可中断续跑，过程中显示第几类、已重指多少条链接；迁完自动重新扫描。

**从命令行看**：`ak-scan --shared` 打印库、台账与全部发现，适合 CI 或写脚本比对。

**交给 Agent 做**：接入 MCP 之后，agent 可以 `list_library` 看矩阵、`enable_asset` /
`disable_asset` 开关、`link_doctor` 体检、`converge_plan` 出方案、`apply_finding_fix` 逐条
修复。参考对话：

> 「code-review 这个技能现在装在哪些 agent 上？cursor 缺的那个帮我接上」
> → `list_library` 看单元格 → `enable_asset`（`{"name":"code-review","kind":"skill","agentId":"cursor"}`）

> 「有没有重复的技能白占磁盘？先别动我的东西」
> → `converge_plan`（零写入）报组数与可释放字节 → 要真收敛请人到桌面端点向导

> 「那条悬空的链接帮我修好」→ `link_doctor` 找到对应的发现，`apply_finding_fix` 照抄它的
> `code` / `path` / `fix` 把它接回库内主副本；一次只修一条。

### 8.15 边界与已知限制

| 限制 | 影响 | 说明 |
|---|---|---|
| 矩阵只覆盖全局作用域 | project 作用域的资产在「技能」标签的 `external[]` 行，规则 / 记忆不进矩阵 | 17 个 agent 全部声明了 projects，加列会让矩阵宽度翻倍，而项目资产已有正确的归属与 `effective` 视图 |
| 规则 / 记忆没有「导入到库」按钮 | 要让一份现有规则进库，只能走 `link` 模式（目标文件成为主副本），或手工放到 `<root>/rules/…` 或 `<root>/memory/…` | 与 `enable_asset` 的语义重叠 |
| `.mdc` 不支持 `compose` | Cursor 规则只能整份链接 | 不做 frontmatter 合并 |
| 硬链接会被 agent 打断 | 本机 symlink 与 junction 都不可用时，单文件只能用硬链接 | 健康检查报 `link.fragile` 提示 |
| 写入后 agent 需重启 / 重扫才生效 | 面板不显示「已生效」实时态 | — |
| 库根被重定向（OneDrive、U 盘） | 链接存的是绝对路径，可能整片悬空 | 迁移库位置可修；默认位置在用户主目录下，不随盘符变 |
| `ak-scan --shared` 不做增量 | 每次全量现扫，大库冷盘约 0.5–1 s | 与 `list_library` 同源 |
| MCP 无一键收敛、无批量修复 | 批量动作只在桌面端 | 需要「确认」这一步 |

### 8.16 故障排查

| 现象 | 原因 | 处置 |
|---|---|---|
| 共享页顶部常驻「副本模式」横幅 | 这台机器连链接都建不出来（非 NTFS、网络盘、权限） | 库的改动要重新扫描才到达 agent；换到本地 NTFS 卷后冲突自动消失 |
| 某个单元格一直是 `foreign`（被顶掉） | agent 把链接换成了自己的真实副本 | 健康页对它点修复；内容一致才允许重连，不一致时健康检查不给修复按钮 |
| 健康页报 `link.dangling` 且标 `masterUnreadable` | 库内技能的 `SKILL.md` 被删（目录还在） | 修复按钮不会出现：内容已缺失，重连只会制造第二条发现。从回收站或备份取回 `SKILL.md`，或重新收敛 |
| 改了库内文件，某个 agent 没反应 | 该 agent 的单元格不是 `on`；或是副本模式 | 看矩阵单元格；副本模式下点「重新扫描」 |
| 某个 agent 的一列是 `unsupported` | 该 agent 没有声明可写位置（例如 `warp` 没有全局技能目录） | 不是缺陷，按 `reason` 显示的原因处理 |
| 抽屉里改了 agent 侧的规则文件，之后同步失败 | 区块被外部改过（`rule.foreignEdit`） | 界面会弹两方对比，选保留盘上版本或改回库内版本 |
| 规则文件读不出来 | 权限或编码问题 | 启用 / 停用会明确拒绝并提示，磁盘不动 |
| 提示 `store.busy` | 另一个进程（通常是 MCP）正在写共享状态 | 等它结束再重试；迁移正在进行时也会是这个 |
| 迁移中途被打断 | 关机、强杀 | 直接再点一次「迁移库…」：journal 会从断点继续，不重复搬、不重复重指 |
| 健康页报 `link.ledgerDrift` | 台账被手工改坏、或与磁盘不符 | 对它点修复即「以磁盘为准重建」；台账从不反向覆盖磁盘 |
| MCP 报 `doctor.notFixable` | 该发现没有可自动执行的修法（硬链接易断、批量收敛） | 换桌面端处理 |
| 迁移后 agent 全都不认这项技能了 | 库根字符串不是合法绝对路径 | 迁移只接受后端认识的枚举（默认位置 / 应用私有目录）；自定义路径要写进设置文件 |

### 8.17 实现纪律

#### 8.17.1 新增与改动的模块

| 文件 | 职责 |
|---|---|
| `linkfs.rs` | 链接原语与能力探测（junction 需 FSCTL） |
| `library.rs` | 库根解析、`links.json` 台账（`resolve` / `dir_for` / `skill_dir` / `mcp_record_path` / `ledger_path` / `load_ledger` / `save_ledger` / `record` / `owns_target` / `capability`） |
| `compose.rs` | 规则区块标记的插入 / 替换 / 剥离（纯函数） |
| `sharing.rs` | 收敛探测、MCP 物化、规则共享的事务编排 |
| `doctor.rs` | 七类发现的检测（只读） |
| `library_view.rs` | `list_library` 矩阵数据视图（库根 / 列 / 四类行 / 单元格状态，只读） |
| `shared_ops.rs` | 收敛应用、健康发现逐个修复、库根迁移（写入编排） |
| `view-shared.js` | 共享页（矩阵 / 收敛向导 / 健康面板 / 迁移入口） |

改动的既有文件：`store.rs`（`OpKind` +2、快照记链接种类、`store.linkFailed`）、
`scan.rs`（`meta.link`、`sameInode`）、`keeper.rs`（`enable_asset` / `disable_asset` /
`converge_plan` / `library_*`）、`settings.rs`（`linkMethod` / `libraryRoot` / `trashDays`、
`.trash` 清理）、`adapters.rs` + `scripts/adapter-table.js`（`mcpDisable`）、`mcp.rs`（工具表
+6）、`ak-scan.rs`（`--shared`）、`src-tauri/src/lib.rs`（+8 个命令）、`tauri-bridge.js`
（+8 个 `window.ak.*`）、`index.html` / `app.js` / `i18n.js` / `engine-zh.js` / `styles.css`。

#### 8.17.2 实现顺序

新增文件按依赖顺序接入：链接内核 → Skills → MCP → 规则与记忆 → 扫描与健康检查 →
界面 → 命令行 / MCP 工具 / 迁移。前一步不做完，后一步没有可用的写入原语。

#### 8.17.3 库位置迁移（事务的唯一例外）

在 `~/.agents`（共享约定）与 `<store>`（私有）之间切换。迁移是**唯一**允许绕过
`store.transaction` 的共享写路径：它要搬的是整棵库，一次事务要么撑爆撤销预算，要么把几百
次重命名绑成一个做不完的原子操作。取而代之的是 journal：

1. 台账里写 `migrating { from, to, done, relinked }`；
2. 逐个搬顶层子项（`rename`，跨卷退回 `copy + rm`），每搬一项刷一次 `done`；
3. 逐条重指链接，**每重指一条就写一次 `relinked`**；
4. 清 journal，把台账写到新库根、删旧台账，最后改 `settings.libraryRoot`。

`relinked` 不是冗余：顶层子项搬没搬，磁盘自己会答（源侧不在了）；**链接指没指对，磁盘答
不了** —— 旧库根要等第 4 步才删，所以「还指着旧根」与「已经指对了」在盘上长得一样。少了
这一栏，中断后每次重跑都会把全部链接解开重建一遍，几百次无谓写入，而每一次都是一个悬空
窗口。

其余约束：整段迁移持 `<store>/.lock`；读不出台账就拒绝迁移（不知道有哪些链接要重指，硬迁
会留下一批悬空链接）；目标与源互相嵌套时拒绝（那会让「搬库」变成递归自我覆盖）；journal
的目标与本次不一致时拒绝续跑（覆盖它会让那批链接永久指错）。中途进度由 `migrate_progress`
轮询，界面上是「第几类 / 已重指多少条链接」。

#### 8.17.4 交接立场

- **不要改的**：`store.rs` 的事务语义、`toml_splice.rs` 的按区段手术、`scan.rs` 的 id 生成
  规则。这三处是既有不变量。
- **可以改的**：`keeper.rs` 的 `distribute`——把 copy 换成 link 是这一层的核心目的之一，
  但要保留旧行为作为 `mode: "copy"` 分支，不要直接删。
- **拿不准时**：宁可让操作失败并给出可读的 code，也不要「尽力而为」地写出半对的状态。

### 8.18 验收方式

### 8.19 实现时反复踩到的坑

```powershell
cd tauri-shell
cargo test --workspace                    # 0 failed
cargo clippy --workspace --all-targets    # 与改动前逐行 diff 不新增 warning
cd ..
node scripts/check.js                     # all 8 gates passed
node scripts/tauri-i18n.js                # MISS 0 / CODE-MISS 0
node scripts/ui-audit.js                  # total findings: 0（它永远 exit 0，看输出）
node tauri-shell/copy-frontend.js --check
node scripts/changelog.js check
```

clippy 不按固定数字判：toolchain 一升级数字就变，**判据是「改动前后跑同一条命令、逐行
diff 不新增 warning」**。

功能验收（在真实机器上，不只用夹具）：

| # | 场景 | 期望 |
|---|---|---|
| 1 | 5 份同名 skill 收敛 | 1 份主副本 + 5 条链接，改一处五处生效，**资产 id 不变** |
| 2 | 内容不一致的一组 | 不收敛，给出差异文件名与 `only-a / only-b / different` 状态 |
| 3 | 停用某 agent | 只删链接，库内主副本仍在，审计有记录 |
| 4 | 删库内资产 | 进 `.trash`，链接全删，审计有记录，`trashDays` 天后被清掉 |
| 5 | Codex（TOML）与 Claude Code（JSON）同开一个 MCP | TOML 注释保留；关闭后逐字节还原 |
| 6 | 手写一个库外 MCP server | 健康检查零报告、零写入 |
| 7 | `compose` 规则 | 标记外内容保留；CRLF 不变 |
| 8 | agent 把链接换成真实目录 | 报 `link.hijacked`，内容一致时可修复 |
| 9 | 库位置迁移中断后重跑 | 最终状态一致，不重复搬、不重复重指 |
| 10 | 普通用户会话（非管理员） | 全流程可用 —— 这是这一层存在的理由 |
| 11 | 收敛后手动删库内 `SKILL.md` | 各 agent 侧报 `link.dangling`，无 agent 崩溃 |

#### 8.19.1 新前端文件只登记一半

新增一个渲染层脚本要同时改七处，漏一处会出现「本地全绿、打包后空白页」：
`index.html` 的 `<script>` 标签、`tauri-shell/copy-frontend.js` 的 `PLAIN` 白名单、
`scripts/lint-renderer.js` 的脚本清单、`scripts/ui-audit.js` 的脚本清单、
`scripts/test-affected.js` 的区域映射、`src/renderer/app.js` 的引用、`scripts/ui-baseline.js`
的视图表（若新视图要纳入像素基线）。

#### 8.19.2 junction 认不出来

见 §8.3。凡是要判断「这是不是我们建的链接」的地方，一律走 `linkfs::entry_link_kind`；
`DirEntry::file_type()` 与 `symlink_metadata().file_type().is_symlink()` 在 junction 上都
可能给出否定答案。

#### 8.19.3 删除顺序写反

`linkDir` 在目标已存在时直接拒绝，所以「先删真实目录、再建链接」的顺序不能反，且两步
必须在同一个事务里。

#### 8.19.4 CRLF 混写

规则区块若把 CRLF 文件写成 LF，用户看到的是「整个文件都变了」的 diff。换行判据与
`toml_splice` 同一份：沿用文件第一次出现的换行。

#### 8.19.5 `shared` 这个编号已被占用

扫描把 `~/.agents/skills` 报成伪 agent `agentId: "shared"`。Rust 侧的结构体 / 字段别复用
这个词；**判断一个路径是否在库内不能用 `agentId == "shared"`**——库根可被切到别处，必须用
`Library::owns_target`。

#### 8.19.6 `settings.save` 只接受 `defaults()` 里已有的键

未知键与类型不匹配的键会被**静默丢弃**：设置页点了保存、界面回显成功，磁盘上却没有这个
键。新增 `linkMethod` / `libraryRoot` / `trashDays` 必须先加进 `defaults()`。

#### 8.19.7 忘了重冻结适配器快照

改了 `adapters.rs::AGENTS` 或 `scripts/adapter-table.js` 而不跑 `npm run tauri:adapters`，
快照测试会红（现在冻结了 17 个 agent / 187 条路径模板）。红了先重算常量，不要删测试。

#### 8.19.8 工具数量写进了类型

`core/tests/mcp.rs` 里 `const TOOLS: [&str; N]` 的长度写在类型上，加工具必须同步改长度，
否则编译期就红。只能在末尾追加，不要重排、不要改名，也不要在别处另写一份工具名清单。

#### 8.19.9 集成测试放在哪个 crate

`Keeper` 与 `Store` 住在 `core`，但集成测试一律是**外壳 crate** 的集成测试
（`use agentkeeper_shell_lib::core::…`）。所以：

```powershell
cargo test -p agentkeeper-core --test write_path   # → error: no test target named `write_path`
cargo test -p agentkeeper-shell --test write_path  # → ok
```

| 测试 | 放哪里 |
|---|---|
| `linkfs` 能力探测 / `compose` 纯函数 / 台账读写 | 各模块内的 `mod tests` |
| 链接事务：创建 / 还原 / 类型保持 / 台账损坏 | `src-tauri/tests/write_path.rs`（追加，别新建） |
| 收敛端到端、矩阵载荷形状、健康发现 | `src-tauri/tests/sharing.rs` / `rule_sharing.rs` / `mcp_sharing.rs` / `doctor.rs` |

`core/tests/` 下只有 `mcp.rs` 与 `common/mod.rs`；`src-tauri/tests/common/mod.rs` 才是那份
带 `temp_root` / `cleanup` / `dir_snapshot` 的完整工具集。

---

## 9. 路线图

**v0.2 — 当前版本**
- Rust core 承担全部业务逻辑，扫描 990 资产 0.5 s
- 安装包 3.86 MiB / currentUser / 无管理员权限
- 21 个工具的 stdio MCP server 随包分发（见 §8）

**共享层（v0.3 起，规范见 §8）**
- 同一份 skills / MCP / 规则 / 记忆不再逐 agent 复制：库内一份主副本 + 各处指向它
- 21 个工具的 stdio MCP server（5 个共享库工具 + 1 个健康修复工具）
- 共享页矩阵、收敛向导、七类健康检查与逐个修复
- `ak-scan --shared`、库位置迁移（可中断续跑）、跨进程写锁

**v0.4 — 团队**
- 团队共享 bundle + 版本 pin
- 安全扫描：MCP tool poisoning 检测（读 `tools/list` 描述里的可疑指令）

---

## 10. 目录索引

```
agentkeeper/
├── src/renderer/                 ★ 前端源码（零依赖原生 DOM），唯一真源
├── tauri-shell/
│   ├── Cargo.toml                  workspace 根：members = [core, src-tauri] · release profile
│   ├── .cargo/config.toml          [build] incremental = false（workspace 级）
│   ├── tauri-bridge.js             window.ak ← invoke() 的 shim
│   ├── core/                       ★ agentkeeper-core：业务核心，零 Tauri 依赖
│   │   ├── src/lib.rs              core 模块树 + JS trim 兼容层
│   │   ├── src/*.rs                scan · assets · registry · store · keeper · mcp · security
│   │   │                            · linkfs · library · compose · sharing · doctor · library_view · shared_ops
│   │   ├── src/bin/                agentkeeper-mcp（stdio MCP server）· ak-scan（无头扫描）
│   │   ├── tests/                  mcp 协议回归
│   │   └── Cargo.toml              只有非 Tauri 依赖（serde · toml · ureq · windows-sys …）
│   ├── src-tauri/                  agentkeeper-shell：Tauri 桌面壳
│   │   ├── src/lib.rs              Tauri 命令 · 窗口装配 · 更新调度线程 · WebView2 环境
│   │   ├── src/main.rs             入口（release: windows_subsystem="windows"）
│   │   ├── tests/                  集成测试（contract · write_path · update · boundary
│   │   │                            · sharing · rule_sharing · mcp_sharing · doctor · cli_shared）
│   │   ├── tauri.conf.json         CSP · NSIS currentUser · externalBin（core 的两个 bin）
│   │   └── icons/
│   ├── target/                     cargo 共享产物目录（两个 crate 共用）
│   ├── copy-frontend.js            src/renderer → dist（唯一受控差异）
│   ├── stage-core-bins.js          core 的两个 bin → src-tauri/binaries/<name>-<triple>.exe
│   ├── clean-build.js              清孤儿 build 目录（--pdbs/--all 才动 pdb 与缓存）
│   └── dist/                       生成物，被编进 exe
├── src/renderer/                零依赖原生 DOM，唯一真源（含 view-shared.js：共享页）
├── scripts/                     check · lint-renderer · ui-audit · tauri-i18n
│                                · adapter-table · gen-adapters-snapshots · rust-env.ps1/sh
└── docs/{DESIGN.md,UPDATE.md,MCP-SERVER.md,RUST-ENV.md,BUILDING.md,CHANGELOG.md}
```

**为什么拆成两个 crate**：`core/` 从不引用 Tauri 类型（实测 0 处），但同处一个 crate 时，
`agentkeeper-mcp` / `ak-scan` 这两个纯 CLI 会被拖进 336 个 crate 的依赖图（含 18 个
tauri/wry/webview），并且每个 bin 都要为 fat LTO 付一次全量跨 crate 优化。拆开后 core 的依赖树
降到 118 个 crate、零 Tauri，两个 CLI 的重链从 44/49 s 降到合计 38 s。「core 不认识 Tauri」
从约定变成编译期保证。

运行：

```bash
source scripts/rust-env.sh        # 新终端需要先接线 cargo / MSVC 环境
npm run tauri:sync                # 把前端同步进 dist
cd tauri-shell/src-tauri
cargo tauri dev                   # 开发
npm run tauri:bundle              # 出包：安装版 + 免安装 zip → release/
cd tauri-shell && cargo test --workspace   # 单元 + 集成（两个 crate）
```
