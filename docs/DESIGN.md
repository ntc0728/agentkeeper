# AgentKeeper — 设计方案

> 面向 AI 编码 Agent 的资产控制平面。统一发现、校验、编辑、分发与审计
> 全局与项目级的 **skills / MCP / 插件 / 规则 / 记忆库**。
>
> 桌面壳：**Tauri 2**。业务逻辑：**Rust**（`tauri-shell/src-tauri/src/core/`）。
> 前端：与早期 Electron 版**同一份**零依赖原生 DOM（`src/renderer/`）。

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

调研了三个同类成熟项目（截至 2026-10）：

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
     AgentKeeper   │ 写入事务层：validate→backup→atomic→journal │
     独有          │ 影子层：staging + Apply/Discard           │
                    │ 结构化回滚：按目录树 byte-for-byte 还原    │
                    └──────────────────────────────────────────┘
```

**一句话差异**：其余三家解决"看见"和"编辑"，AgentKeeper 解决**"编辑之后还能安全地活下来"**。

---

## 2. 系统架构

```
┌─────────────────────────────────────────────────────────────────┐
│  WebView2（Tauri 2，无 Node 集成，CSP 见 tauri.conf.json）        │
│  index.html · styles.css · app.js（零依赖，原生 DOM，未 fork）     │
└───────────────────────────┬─────────────────────────────────────┘
                            │ window.ak.*  ← tauri-bridge.js
                            │ （保留 Electron preload 的函数形状）
┌───────────────────────────┴─────────────────────────────────────┐
│  Tauri command 层 —— src/lib.rs                                  │
│  40 个 #[tauri::command]，逐个校验入参，只做形状适配              │
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
│  store.rs      ★ 影子层 · 备份 · 原子写 · 事务 · 回滚 · 审计       │
│  keeper.rs     变更服务层（CRUD/分发/diff，永不绕过校验）          │
│  registry.rs   项目注册表                                         │
│  discover.rs   注册表搜索 + 从 GitHub 安装                         │
│  update/       软件更新内核：version · http · token · manager      │
│  mcp.rs        stdio MCP server（20 个工具，第二个入口）           │
└─────────────────────────────────────────────────────────────────┘
```

**关键约束**：`core::` 不认识 Tauri 类型。因此同一套逻辑有三个入口：

| 入口 | 二进制 | 用途 |
|---|---|---|
| GUI | `agentkeeper-shell` | 桌面应用 |
| MCP | `agentkeeper-mcp` | Agent 通过 stdio 调用 |
| 扫描 | `ak-scan` | 打印 `ak:scan` 的完整 payload（迁移期曾用于 parity diff，见 §6.1） |

扫描入口最初也是为跨运行时对齐造的：Rust 单测只能证明自洽，要证明「和迁移前的实现等价」，
得把 GUI 里的逻辑搬到命令行上，跟旧 Node 实现的输出逐字段比对。那套比对已随旧 `core/`
一并移除。`ak-scan` 因为能单独跑一次
无头扫描而保留；写入侧的 `ak-store` 除对齐没有别的用途，已于 2026-10-20 随 harness 一起
删除（从 tag `pre-electron-removal` 可取回）。

### 2.1 错误文案与中文翻译

这里有两层，必须分清，否则很容易写出"看起来一致、实际不一致"的结论：

| 层 | 现状 | 位置 |
|---|---|---|
| **JS 侧（迁移前基准，已随 `core/` 移除）** | 81 条稳定 code：`code → [英文模板, 占位符数]`，渲染层按 code 取中文模板 | 旧 `core/errors.js`；`code → 中文` 的词典仍保留在 `src/renderer/engine-zh.js` |
| **Rust 侧（当前发布）** | `#[tauri::command]` 返回 `{ok:false, error:"<英文句子>"}`，**不带 code**；只有 MCP 的 `tools/call` 会补 code（英文→code 表） | `tauri-shell/src-tauri/src/lib.rs` · `tauri-shell/src-tauri/src/core/keeper.rs` · `tauri-shell/src-tauri/src/core/store.rs` · `tauri-shell/src-tauri/src/core/mcp.rs` |

渲染层的 `zhError(payload)` 先试 `code → 中文模板`，取不到再退回**句子词典** `zhText()`。
所以 Rust 侧能不能显示中文，完全取决于它吐出的句子在不在句子词典里——
**没有 code 兜底，漏一条就露一句英文**。

#### 覆盖情况：由审计脚本持续守着

迁移前那道闸（`selftest.js` §18）只审计 JS 引擎文件。引擎换成 Rust 之后它就失效了：
JS 侧不再产出消息，而 Rust 侧每条消息都直达渲染层，中间没有任何拦路的东西。
Rust 侧的审计补上这一半：刮 `src-tauri/src/**/*.rs` 里的**全部字符串字面量**，用结构性规则
（panic / 日志 / 输入匹配器等）排掉不上屏的那些，剩下的逐条查词典，漏一条就 exit 1。

> **注**：迁移期一度出现了两个口径不同的审计脚本——`scripts/tauri-i18n.js` 与
> `scripts/i18n-engine-audit.js`（后者抽全部字面量再用 `looksSentence` 过滤，是前者的
> 严格超集）。**保留 `tauri-i18n.js` 并删掉另一个**（team-lead 裁决），功能并入前者，
> 所以现在只有一个脚本、一个口径。

`scripts/tauri-i18n.js` 当前结果：

| 指标 | 当前值 | 说明 |
|---|---|---|
| MISS | 0 | 退出码 0 |
| 覆盖 | 117 EXACT + 66 PATTERNS | 句子词典的规模 |
| 硬编码中文残留 | 0 | 对外发出的中文已清零；`src-tauri/src/` 里剩下的中文只在测试夹具与注释里 |
| 日志面豁免 | 3 | 只进日志，不上屏 |
| CLI 白名单 / CLI 文件 | 2 / 6 | `ak-store` 等终端提示，不进 WebView |
| MCP 协议面（按文件豁免） | 74 | 协议字段，不走渲染层 |
| CODE-MISS / CJK-KEY | 0 / 0 | 见下节 |
| AMBIGUOUS-MULTI（PATTERN×PATTERN） | 0 | 两个正则同时命中 |
| EXACT SHADOWING（EXACT 遮蔽 PATTERNS） | 1 | 预期内 |

> 上表的数字会随 Rust 侧文案增删而变，**以 `node scripts/tauri-i18n.js` 的实际输出为准**；
> 这里列出的是登记当时的读数，用来说明闸门在拦什么，不是锁死的指标。

建这个审计时它一次报出 **14 条**漏翻（`assetId is required`、`targets is required`、
`registry not initialised`；6 条 `spawn_blocking` 失败前缀 `xxx task failed: …`；
`timed out after Nms`；store 的 3 条；以及 `core/mcp.rs` 的
`const NO_INSTALLED_AGENT`），已全部补进 `src/renderer/engine-zh.js` 并同步 `dist/`。
**此前人工扫 5 个文件报出的数字远小于此**——抽样在这个规模上不可靠，
缺口只能靠脚本守，不能靠扫几眼。

其中 `const` 那一类最容易被漏：它既不在 `"error":` 旁边也不是 `Err(...)`，
一开始审计完全看不见它，是给脚本补了常量抽取才冒出来的。

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
> 少了这段因果，后人很容易把那个脚本当成多余的仪式感给删掉。

换个说法定性：**Rust 侧没有 code 体系是健壮性降级，不是功能缺口**——界面照样出中文，
只是"漏翻"这种失误没了编译期兜底，只能靠脚本兜。

#### 维护提醒：这道闸门认的是旧路径

`scripts/tauri-i18n.js` 里 `DICT` 常量（`const DICT = path.join(ROOT, 'src', 'renderer',
'engine-zh.js')`）**硬编码的是迁移前的路径**，全文共 6 处引用该文件。

前端整体搬到 `src/renderer/` 之后，这道闸门会直接找不到字典而 exit 2（报
`could not run`，而不是"有漏翻"）——**表现为"构建失败"，但根因是路径**。

所以搬前端时，**同一批改动里必须同步改这 6 处**，否则搬完第一次跑 CI，看到的是一条
误导性的失败信息。

---

## 3. 适配器层设计

每个 agent 是一条**纯数据声明**（`tauri-shell/src-tauri/src/core/adapters.rs`），
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

**一致性怎么守**：迁移期的 JS 适配器表曾在假 home 上求值、冻结成两个 Rust 常量
（`JS_TABLE_JSON` / `JS_RESOLVED_JSON`）固化在 `adapters.rs` 里；
`adapters.rs` 的测试再拿 `AGENTS` 与 `resolve_home()` 的每个结果逐字段比对。
`AGENTS` 一改，两边不一致就红——这就是它存在的意义。

#### 新增 agent 的维护规程

重新冻结 `JS_TABLE_JSON` / `JS_RESOLVED_JSON` 的路径**是通的**，但只剩一条：
`scripts/adapter-table.js`（旧 `core/adapters/index.js` 的残留件，2026-10-20 随 JS 内核一起
删除时抽出）→ `npm run tauri:adapters` 重算常量。旧 `core/` 没了，所以改表只能改这一个文件；
改了它就必须同一次跑生成器，别指望 `adapters.rs` 自己变。

1. 在 `tauri-shell/src-tauri/src/core/adapters.rs` 的 `AGENTS` 表里加一条纯数据声明；
2. 在 `scripts/adapter-table.js` 的 `AGENTS` 里加同一条（这是生成器的输入）；
3. `npm run tauri:adapters`，让 `JS_TABLE_JSON`（原始声明）与 `JS_RESOLVED_JSON`（假 home 上
   求值后的路径）一起更新；
4. 在常量附近注明该 agent 的字段来源（官方文档 / 实测路径），让下一个改的人有据可查。

> **测试红了怎么办：先去重算常量，不是删测试。**
>
> 这条测试断言 `js.len() == AGENTS.len()`，加第 18 个 agent 而没同步补常量时**必然红**。
> 这个红是**它唯一的价值所在**——旧 JS 真值没了之后，它是唯一还能证明
> 「这 17 个适配器的字段没被抄错」的东西。
>
> 把它删掉，CI 立刻转绿，但也从此再无任何东西能回答"Rust 侧这条声明和原始实现一致吗"。
> **为了 CI 转绿而删掉一条正在正确报错的测试，是最坏的处置方式。**
> 正确动作是回到第 2 步，把常量补齐后一起提交。

`detected` 函数**没有**搬到 Rust：JS 侧 `detectAgents()` 被 `ipc.js` import 但从未调用，
是一段死数据；两边统一用 `installed = existsDir(home)`。

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

### 4.1 六步管线

```
validate ──► backup ──► stage(sandbox) ──► atomic write ──► journal ──► [Apply]
   │            │                                                  │
   └─阻断       └─backups/<txId>/ 完整目录树快照                  └─回滚 = 再还原快照
```

1. **validate** — `validate.rs`。`error` 级直接阻断，**磁盘零写入**。
2. **backup** — 每个待触碰路径快照到 `backups/<txId>/<映射路径>`；记录 `existed`，因此**删除也可还原**。
3. **stage** — sandbox 开启时写 `sandbox/drive/<盘符>/…`，镜像完整目录结构；真实目录零改动。
4. **atomic write** — 写 `<path>.agentkeeper-tmp-<pid>-ts>` 再 `rename`。
   Windows 上 rename 遇占用会失败，降级为直写但仍有 tmp 残留可追。
5. **journal** — `audit.jsonl` 追加一行：`{ts, txId, status, action, kind, agentId, projectId, assetName, targets[]}`。
6. **Apply / Rollback** — 见下。

### 4.2 影子层（Shadow Layer）

```
真实路径  F:\repo\.workbuddy\mcp.json
             │
             │ sandbox ON 时 store.real() 重定向
             ▼
影子路径  <store>\sandbox\drive\F\repo\.workbuddy\mcp.json
             │
             ├─ pending.json 记录 {op, real, shadow, bytes, at}
             ▼
        [Apply] ──► 以**一个事务**批量重放到真实路径
        [Discard] ─► 删除影子文件，真实目录自始至终未被触碰
```

设计要点：

- **Apply 是单事务**。N 个待应用变更要么全部生效、要么全部回滚，不存在半应用状态。
- **`json` op 在 sandbox 下以真实文件为基底**。影子树初始是空的，若直接读会得到 `{}`，
  写出的 `mcp.json` 会丢掉全部兄弟 server。实现里显式做了 base 回退。
- **Rollback 会 unshadow 路径**。事务日志记的是"写入时的路径"（sandbox 时为影子路径），
  回滚时若不映射回影子树就会打到错误位置；同时清理对应 `pending` 条目。
- **删除时向上清理空目录**。回滚一个新建 skill 时，`skills/<name>/` 会留下空壳——
  部分 agent 会把空目录也枚举出来。
- **Apply 时对 json 做三方合并**（种子快照 base / 影子结果 ours / 真实文件 theirs）——
  未冲突的外部修改在 Apply 后保留。Apply 前有备份，可整体回滚。

**实测证据**（安装版二进制，`AK_STORE_ROOT` 指向临时 store）：
MCP 调 `create_skill` 返回 `staged:true`，真实路径
`C:\Users\<你>\.workbuddy\skills\<name>` **不存在**，影子副本落在
`<store>\sandbox\drive\C\Users\<你>\.workbuddy\skills\<name>\SKILL.md`，
`pending.json` 里是一条 `{op:"write", real, shadow, bytes, at}`。

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

前端自迁移以来**没有 fork**：`src/renderer/` 是唯一真源，`tauri-shell/copy-frontend.js`
生成 `tauri-shell/dist` 时只注入两处差异（`<html data-shell="tauri">` 与 `tauri-bridge.js`），
`npm run tauri:check` 保证不漂移。

### 5.1 信息架构

| 视图 | 回答的问题 |
|---|---|
| **概览** | 我这台机器上一共有多少 agent 资产？谁在吃 context？有几处待应用？ |
| **资产** | 某一个资产具体是什么？归谁所有？可编辑吗？ |
| **Agent** | 装了哪些 harness，各自认哪些目录？ |
| **项目** | 哪些仓库带 agent 文件？各贡献了多少？ |
| **健康检查** | 哪些文件 agent 会静默忽略或读错？ |
| **操作记录** | 谁在什么时候改了什么？能撤销吗？ |

### 5.2 关键交互

- **默认过滤掉缓存噪声**。插件里绝大多数是 host-owned 缓存。资产页默认显示
  "Curated + managed"，切到 "Everything incl. caches" 才全量列出。健康检查永久排除缓存，
  否则厂商的错会被误报成用户的错。
- **待应用面板置顶**。有 staged 变更时概览顶部出现 STAGED 条，列出 op / 目标路径 / 大小 / 暂存时间。
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
  - 原因：动态样式全部走 CSSOM（`applyStyle()` 用 `element.style.setProperty`；
    Electron 时代为此把内联样式全部改写成 CSSOM + 类名，见 §6.5），而 CSSOM 写入不受
    `style-src` 约束；受约束的是内联 `style="…"` 属性与脚本插入的 `<style>`。
  - CDP 探针在两份构装上逐字段相同，佐证 `'unsafe-inline'` 从未真正放开过东西：
    `element.style.setProperty('width','123px')` → 123px（生效）、
    `setAttribute('style','width:77px')` → 仍是 123px（被忽略）、脚本插入的 `<style>` → 不生效。
  - 局限（如实记）：两次构建之间 `core/update/manager.rs` 有并行改动（同仓库多 agent 协作），
    但含"更新分区"的设置页逐字节相同，该改动对结论无影响。
  - 截图与比对脚本是临时产物（`.tmp-csp-shots/`、`.tmp-csp-cmp.ps1`），未入库。

---

## 6. 验证

### 6.1 测试套件

```bash
cd tauri-shell/src-tauri && cargo test     # 单元 + 集成，断言数随覆盖增长
```

| 套件 | 覆盖 |
|---|---|
| `core::*` 单测 | 格式层往返、校验器、安全模式、影子层、事务回滚、适配器快照对齐、MCP dispatch |
| `tests/contract.rs` | IPC / 数据契约、资产读写、注册表、格式解析（用真实 keeper 跑临时 store） |
| `tests/write_path.rs` | 事务、备份、journal、回滚、校验与安全扫描 |
| `tests/mcp.rs` | MCP 工具 dispatch + 真实子进程协议（原 `mcp_stdio.rs`） |
| `tests/update.rs` | 更新链路：真实 `TcpListener` 起 HTTP 服务、参数守卫、JS 字面量漂移（见 `docs/UPDATE.md` §9） |
| `npm run tauri:i18n` | Rust 引擎错误文案的中文覆盖：漏一条 exit 1（见 §2.1） |

前端静态校验（纯 Node，不启浏览器，全量 ≈0.5 s）：

```bash
npm run check                # 残留扫描 · 桥接↔Rust 命令契约 · renderer 规则 · UI 审计 · i18n · dist 一致性
node scripts/check.js bridge # 单独跑某一道：residue / bridge / renderer / ui / i18n / dist
```

迁移期抓出的 parity bug（`~` 展开后残留正斜杠、UTF-16 切片、对象型 tags、
`preserve_order`、重复 early return）当年全部只有跨运行时字节级比对抓得到，单测抓不到——
那正是当时保留一份 Node 基准当参照物的理由。这套 harness 在旧架构删除前跑完了最后一次
全量；工具本身已于 2026-10-20 随旧 `core/` 一并移除。

### 6.2 MCP 协议回归

`cargo test --test mcp` 起真实进程，按客户端顺序走完：
`initialize` → `notifications/initialized`（必须静默）→ `tools/list`（20 个，名字顺序钉死）→
`list_assets` / `read_asset` / `stats` / `health_report` → `create_skill`（暂存且真实路径不落地）→
`list_pending` → `apply_pending`（沙盒拒绝）→ `-32601` / `-32602` / `-32700`（`id:null`）→
无 id 请求与空行静默 → **stdout 行数必须等于应答复的请求数** → 关 stdin 退出码 0。

### 6.3 打包验收（本机实测）

| 项 | 结果 |
|---|---|
| 安装包 | 3.7 MiB（Electron 版 69.5 MiB） |
| 静默安装 | `/S /D=<dir>` → exit 0，无需提权（`currentUser`） |
| 安装后 | 15.9 MiB / 17 文件；无 `dist/`（前端已内嵌） |
| `demo-projects` | **不再随包发布**：原 `bundle.resources` 条目已移除，安装目录中没有该目录 |
| 冷启动 → 窗口可见 | 0.76 / 0.78 / 0.83 / 0.85 / 0.93 s |
| 内存 | shell 69 MB（private 46 MB）；整棵进程树含 WebView2 ≈ 480 MB |
| 关闭 | WM_CLOSE → 50 ms 内 exit 0；WebView2 子进程 8–22 s 内自行退出（运行时回收，非泄漏） |
| 首启扫描 | 990 个资产 / 519–558 ms |

四张安装版截图见 README 的「验证截图」。

### 6.4 本机两个"看起来像应用的 bug、其实是工具"的坑

**坑一：WebView2 需要 `--no-sandbox --disable-gpu-sandbox`**

这台机器无法初始化 Chromium OS 沙盒。WebView2 不加这两个开关时渲染进程直接
`STATUS_BREAKPOINT`（`0x80000003`），现象是**窗口永远不出现**、连
`msedgewebview2.exe` 子进程都没有。`lib.rs::run()` 在创建 webview 之前无条件设置
`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`（release 同样生效；若用户已显式设置则不覆盖）。

历史教训保留在这里：Electron 时代曾把这个和 GPU 崩溃当成两个问题，据此把
`disable-gpu` / `in-process-gpu` / 关硬件加速一起固化，导致打包版整体卡顿。
后来 probe 矩阵证明**崩溃根因只有 OS 沙盒一条**，GPU 无辜，已全部移除。
（`scripts/probe.js` / `probe-matrix.js` 是那次排查留下的 Electron 工具，已于 2026-10-20
随 Electron 测试链一起删除；结论留在这里，工具不再需要。）

**坑二：截图工具 DPI 虚拟化 → 截图被裁掉三分之一**

`shot.ps1` 用 `PrintWindow(h, hdc, 2)` 抓图（不抢前台，前台被全屏程序占据时也能抓）。
但 PowerShell 进程默认 **DPI-unaware**：本机真实缩放 150%（144 DPI，3840×2160），
`GetWindowRect` 返回**虚拟化后**的 1455×977，而 `PrintWindow` 按真实 2160×1410 物理表面绘制 ——
位图只装得下左上 67%，右侧和底部被切掉。
症状极具误导性：概览页看起来"右列卡片被裁、布局溢出"，差点被当成 CSS 栅格 bug。

修法：脚本开头 `[AkWin]::SetProcessDPIAware()`，位图随之变成 2182×1466。
判据很简单——`.app` 栅格是 `248px 1fr`，实测侧栏若为 372 device px 就是 ×1.5，
说明位图分配与绘制比例不一致。

### 6.5 截图验证抓出的真实 bug（Electron 时代，仍适用于同一份前端）

| Bug | 症状 | 根因 | 修法 |
|---|---|---|---|
| **IPC payload 不匹配** | 点任意资产行弹 "asset not found" | preload 发 `{ assetId }`，handler 按字符串读，收到的是 `"[object Object]"` | handler 改读对象字段；并新增逐通道真实调用测试 |
| **CSP 阻断内联样式** | 104 条 `Refused to apply inline style`，进度条宽度/agent 颜色全丢 | `style-src 'self'` 但 `el()` 用 `style="…"` 属性 | `el()` 改走 CSSOM `element.style.setProperty()`；固定样式迁成 CSS 类 |
| **async 视图崩溃** | `appendChild ... not of type 'Node'` | async 视图返回 Promise 被直接 appendChild | `render()` 识别 Promise，先渲染占位再 swap |
| **沙盒下 distribute 失效** | 跨 agent 复制报 `copy source missing` | source 被 `store.real()` 重定向到影子树 | op 增加 `fromReal` 标记，源始终从真实树读 |
| **Health 卡片被挤成一条缝** | 长列表撑破 flex | issue 卡片缺 `min-width: 0` | 补 `min-width: 0` |

> 教训：截图 + console 错误收集是必需环节，不是装饰。上面 5 个 bug 里没有一个会让
> 进程崩溃或测试变红。

---

## 7. 已知限制

| 限制 | 影响 | 后续 |
|---|---|---|
| 写入后 agent 需重启/重扫才生效 | 面板不显示"已生效"实时态 | 接入各 agent 的 reload 信号 |
| 沙盒模式默认开启 | 新用户第一次 Apply 才落盘 | 首启引导页说明 |
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

## 8. 路线图

**v0.2 — 让它有牙（已发）**
- Rust core 全量替换 Node 实现，扫描 990 资产 0.5 s
- 安装包 3.7 MiB / currentUser / 无管理员权限
- 20 个工具的 stdio MCP server 随包分发

**v0.3 — CLI 通道**
- `ak scan --json`、`ak apply`、`ak doctor`（`ak-scan` / `ak-store` 已是雏形）
- 错误码体系在 Rust 侧补齐

**v0.4 — 团队**
- 团队共享 bundle + 版本 pin
- 安全扫描：MCP tool poisoning 检测（读 `tools/list` 描述里的可疑指令）

---

## 9. 目录索引

```
agentkeeper/
├── src/renderer/                 ★ 前端源码（零依赖原生 DOM），唯一真源
├── tauri-shell/
│   ├── tauri-bridge.js           window.ak ← invoke() 的 shim
│   ├── src-tauri/
│   │   ├── src/lib.rs           40 个 Tauri 命令 · 窗口装配 · 更新调度线程 · WebView2 环境
│   │   ├── src/main.rs          入口（release: windows_subsystem="windows"）
│   │   ├── src/core/            ★ 全部业务逻辑（见 §2）
│   │   ├── src/bin/             agentkeeper-mcp · ak-scan
│   │   ├── tests/               集成测试（contract · write_path · mcp · update）
│   │   ├── binaries/            externalBin 暂存目录（stage-mcp-bin.js 产出，不入库）
│   │   ├── tauri.conf.json      CSP · NSIS currentUser · externalBin
│   │   └── icons/
│   ├── stage-mcp-bin.js         MCP 二进制 → src-tauri/binaries/<name>-<triple>.exe
│   ├── copy-frontend.js         src/renderer → dist（唯一受控差异）
│   ├── shot.ps1                 截图（PrintWindow，DPI 感知）
│   ├── dist/                    生成物，被编进 exe
│   └── shots/                   安装版截图
├── scripts/                     check · lint-renderer · ui-audit · tauri-i18n
│                                · adapter-table · gen-adapters-snapshots · rust-env.ps1/sh
└── docs/{DESIGN.md,UPDATE.md,MCP-SERVER.md,RUST-ENV.md}
```

Electron 时代的 `core/`、`src/main/`、`src/mcp/`、`scripts/fixtures/` 已于 2026-10-20 整棵删除。

运行：

```bash
source scripts/rust-env.sh        # 新终端需要先接线 cargo / MSVC 环境
npm run tauri:sync                # 把前端同步进 dist
cd tauri-shell/src-tauri
cargo tauri dev                   # 开发
cargo tauri build                 # 安装包
cargo test                        # 单元 + 集成
AGENTKEEPER_SANDBOX=0 cargo tauri dev   # 启动即真实写入模式
```
