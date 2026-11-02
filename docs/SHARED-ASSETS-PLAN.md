# 共享资产整改计划 —— 同一份 skills / MCP / 规则 / 记忆，不再逐 agent 复制

> 状态：**已实施** —— M1–M7 全部完成并验收，见 `SHARED-ASSETS-IMPLEMENTATION.md` §16–§22；
> 长期有效的部分已并入 `DESIGN.md` §8「共享层」。本文保留为决策依据（「为什么这么做」）
> 目标版本：v0.3
> 平台：Windows 10/11（与现有发布范围一致）
> 本文只描述要做的事、为什么这样做、怎么验收。实现细节以代码与 `DESIGN.md` 为准。
> **与实现口径不一致的地方以 `SHARED-ASSETS-IMPLEMENTATION.md` §12 为准。**

---

## 0. 一句话

现在 AgentKeeper 能做的是「把资产**复制**到另一个 agent」；要的是「一份资产写一次，
所有 agent **看到的是同一份**」。前者产生 N 份副本（漂移、token 浪费、改一处漏一处），
后者产生 1 份主副本 + N 个链接（或托管写入）。

---

## 1. 现状：已经有的 / 缺的

### 1.1 已有的地基（不需要重做）

| 能力 | 位置 | 说明 |
|---|---|---|
| 17 agent 的声明式路径表 | `core/src/adapters.rs` + `scripts/adapter-table.js` | 新增 agent = 改数据，不改代码 |
| 扫描 · 去重 · effective 视图 | `core/src/scan.rs` | 已经能把「同一物理文件的多个读者」折叠成一条（`meta.alsoSeen`） |
| 写入事务 | `core/src/store.rs` | validate → 快照 → 原子写 → journal → 失败整笔还原 |
| 事务里已支持链接语义 | `store.rs::DirSnap::Link` | 快照能捕获并还原链接，只是**没有任何 Op 会创建链接** |
| MCP 分区改写 | `core/src/toml_splice.rs` + `jsonc.rs` | TOML 只重写目标小节、保留注释；JSON 保留兄弟 server |
| 跨 agent 分发 | `keeper.rs::distribute` | 反例：这条路径**拷贝**，正是要替换掉的行为 |
| 共享目录约定 | `SHARED_SKILLS_DIR = ~/.agents/skills` | 社区约定，已在扫描里作为伪 agent `shared` 出现 |

### 1.2 缺的（本计划要补的）

1. **没有「主副本 / 库」概念** —— 每个资产属于某个 agent 的某个目录，没有「这是我维护的那一份」。
2. **没有链接层** —— `Op` 只有 `write / copyDir / json / delete / mkdir`，没有 `link / unlink`。
3. **扫描不认链接** —— 链接进来的 skill 会被当成「另一个 agent 的独立资产」，`shadowedBy` 会把它判成遮蔽关系，而不是「同一份东西的第二个入口」。
4. **MCP 没有单一事实源** —— 每个 server 的配置散落各文件，`distribute` 是逐份合并写入。
5. **规则/记忆只支持「整文件」共享** —— 一个文件只能整份指过去，没有「跨 agent 汇总」能力。
6. **没有链接健康度** —— 悬空链接、指向别处的链接、被 agent 自己改写的链接，都不上报。

---

## 2. 对标：cc-switch 做对了什么，我们取哪一半

cc-switch（`farion1231/cc-switch`，Tauri + Rust，MIT，150k★）是同类里最成熟的实现。
它的做法与本项目的差异可以归成一张表。

| 维度 | cc-switch | 取 / 不取 |
|---|---|---|
| **元数据存放** | SQLite 数据库 + 内置「迁移导入」：把 `~/.claude/.claude.json`、`~/.codex/config.toml`、`~/.gemini/settings.json` 一次性扫进数据库 | **不取库、取迁移**。本项目已有 append-only `audit.jsonl` + 指纹基线，引入 SQLite 等于第二套真相；但「从现有 agent 目录导入成受管资产」这一步必须有，否则新用户打开就是空的 |
| **页面结构** | 全局页（MCP / Skills 不属于任何 app），**一行一个资产 × 一列一个 app 的矩阵**，单元格是开关 | **全取**。这是本功能最核心的交互，见 §5.1 |
| **Skills 的共享** | 主副本放 `~/.cc-switch/skills/` 或 **`~/.agents/skills/`**（社区共享约定），再 **symlink-first，失败回退 copy** 到各 app 目录 | **取策略、换主力**。symlink 在 Windows 要管理员；见 §3.2 的判定 |
| **MCP 的共享** | 条目存在库里，开关驱动「写到 / 从该 app 的配置文件删除这一条」；不改用户没导入过的 server | **全取**。这就是「按需物化」，见 §4 |
| **不认识的 server** | 只管理库里有的；用户手加的同名条目会被覆盖（**明确写出来**） | **取并加强**：同名不同参不覆盖，报 `mcp.drift` 冲突（§7.3） |
| **卸载/回滚** | 卸载前备份到 `~/.cc-switch/skill-backups/`，可从备份恢复 | **取“备份在删除之前”**，但本项目不建备份树（`DESIGN.md` §4.2 明确反对），改为「移入库的 `.trash/` 并保留 N 天」，见 §7.6 |
| **同步方式** | 自动 / 仅 symlink / 仅 copy，改动后需「Resync now」 | **全取**，并给每个链接标注它是哪种方式 |
| **平台代价** | 「Symlink only 需要 Windows 开发者模式」 | **采用 junction 规避**：目录联接不需要任何特权（本机实测，见 §3.2），这是本项目的差异化点 |

`DESIGN.md` §1 已经把 Claude Code Organizer / SkillHub Local / AgentCorral 三个项目比较过，
本节只补 cc-switch 这一块，因为它恰好覆盖了「同一份资产供多个 agent 使用」这一层——
前面三家都止步于「看见」和「复制」。

---

## 3. 总体设计

### 3.1 四种共享方式（按资产形态分，不混在一个开关里）

| 资产 | 形态 | 默认共享方式 | 理由 |
|---|---|---|---|
| **Skill** | 目录（`SKILL.md` + 附属文件） | **链接**（junction，回退到 symlink / hardlink / 托管副本） | 整目录指过去即可，agent 读的路径完全不变 |
| **MCP Server** | 配置文件里的一个**条目** | **托管物化**（由开关决定写/删） | 一份 JSON/TOML 里有 N 个 server，无法用链接共享单个条目 |
| **Rule** | 单个 `.md`（`AGENTS.md` / `CLAUDE.md` / `.mdc`） | **聚合生成**（`<!-- 区块标记 -->`）＋ 可整体指过去 | 多 agent 往往要的是「同一份规则换个文件名」或「几段规则拼一份」 |
| **Memory** | 文件或目录（`MEMORY.md` + `memory/YYYY-MM-DD.md`） | 目录 → **链接**；文件 → **聚合生成** | 与 skill / rule 同理，按它是目录还是文件分 |

> **不做的**：不做「一个开关管四类资产」。skill 的链接、MCP 的物化、规则的聚合，
> 失败模式完全不同，混在一个事务里会让「部分成功」无法解释。

### 3.2 链接策略：Windows 上的事实

本机实测（非管理员会话，`IsInRole(Administrator) == false`）：

| 原语 | 结果 | 说明 |
|---|---|---|
| `New-Item -ItemType Junction` | **成功** | 目录联接，NTFS 重解析点，**不需要开发者模式、不需要管理员** |
| `New-Item -ItemType SymbolicLink` | **失败**（`此操作需要管理员权限`） | 一般用户会话下不可用 |
| `New-Item -ItemType HardLink` | **成功** | 仅文件；跨卷不行 |
| 把已存在的目录替换成 junction | **成功** | 这是「收敛」动作的关键原语 |
| **Rust `std::os::windows::fs::symlink_dir`** | **成功** | `is_symlink=true`、`read_link` 可读、可透过链接读文件 |

结论：**junction 是 Windows 上的主路径，不是降级路径**。
cc-switch 把 symlink 当主路径、把「需要开发者模式」写进文档，而同样的效果用 junction
可以零特权拿到。这直接改变了 §7.1「收敛」能否默认开启。

> **⚠️ 实现前必读（已实测，修正了本节初稿的错误）**
>
> Rust 标准库**没有 junction API**：`std::os::windows::fs::symlink_dir` 创建的就是 **symlink**，
> 不是 junction。实测在普通用户会话下，它创建**成功**（`is_symlink=true`，`read_link` 可读回），
> 但 `New-Item -ItemType SymbolicLink` 在同一会话里被系统拒绝——两者走的是不同的 API 路径
> （Rust 侧用了 `CreateSymbolicLinkW` 的对应模式），说明**不能因为 PowerShell 建不了 symlink
> 就断定 Rust 也建不了**。
>
> 因此 M1 的 `linkfs` 必须：
> 1. **直接调用 `DeviceIoControl(FSCTL_SET_REPARSE_POINT)`** 写 `IO_REPARSE_TAG_MOUNT_POINT`
>    再解析重解析点数据来创建/读取 junction（`windows-sys` 已在 `core` 的依赖里），
>    不要指望 `std` 提供；
> 2. 把「Rust 的 `symlink_dir` 在本机普通会话可用」作为**独立能力项**实测，而不是从 PowerShell 结果推断；
> 3. `read_link` 对 junction 的判定单独测（junction 的 `is_symlink()` 在部分 Rust 版本为 `false`）。
>
> 本机 Rust 侧已实测：`symlink_dir` 成功、可回读目标、可透过链接读文件内容。
> 这改变了降级链的顺序：若目标机 symlink 也可用，**优先 symlink**（语义与工具链一致，
> 且 `std` 直接支持），junction 作为 symlink 被拒时的回退——与本节初稿写的顺序相反。
> 最终顺序以 P0 的实测定论，写进 `LinkCap` 的判定表。

> ⚠️ junction 只解决**目录**。单文件（`AGENTS.md`、`MEMORY.md`）要共享只有两条路：
> hardlink（同卷、agent 保存时可能用 `rename` 打断链接）或托管聚合生成。见 §3.3。

### 3.3 链接能力判定（`LinkCapability`）

启动时（或首次进入共享页时）在 `<store>/probe/` 下做一次探测，结果缓存进设置：

```
能力等级（逐级降级，每级都要在 UI 上显式标出；顺序以 P0 实测定论，见 §3.2）
  symlink   → std 直接支持，单文件也可外链（Rust 侧实测普通会话可用）
  junction  → symlink 被拒时的目录级回退，需 FSCTL 直接调用
  hardlink  → 仅单文件、同卷
  copy      → 只能托管副本，写时逐个同步（跨卷、非 NTFS、网络盘）
```

规则：
- 探测**只写 `<store>/probe/`**，绝不碰 agent 目录；完成即删。
- 探测失败不得阻塞启动（与 `settings::remove_legacy_backups` 语气一致：尽力而为）。
- `copy` 模式下，共享页顶部必须显示「当前为副本模式：改库后需同步」的横幅，且 `Resync` 必现。

### 3.4 数据结构

```
~/.agents/skills/            ← 默认库（社区约定目录，与 cc-switch 的 Shared folder 一致）
<store>/                     ← 可切换为库根（%APPDATA%\AgentKeeper\agentkeeper）
├── skills/<slug>/           ← 库内 skill 主副本
├── mcp/servers/<slug>.json  ← MCP 单一事实源（含 allOf：全部 app 的开关映射）
├── rules/<slug>.md          ← 规则片段库（可被聚合引用）
├── memory/<slug>/           ← 记忆目录（如需共享）
├── links.json               ← 链接台账：每条链接的来源、目标、方式、模式
└── audit.jsonl              ← 既有审计（每笔链接/物化都追加一行）
```

`links.json` 是**索引不是真相**：真相永远是磁盘（链接的实际指向）。
启动时 `doctor` 以磁盘为准修正台账，不允许台账与磁盘背离后仍被信任——
这同 `DESIGN.md` 里「写入直落真实路径、界面显示的就是实际路径」是同一条原则。

```jsonc
// links.json 一条
{
  "kind": "skill",                 // skill | rule | memory
  "name": "pdf",
  "src": "C:\\Users\\me\\.agents\\skills\\pdf",   // 库内主副本
  "dst": "C:\\Users\\me\\.claude\\skills\\pdf",   // agent 侧路径
  "method": "junction",            // junction | symlink | hardlink | copy
  "mode": "link",                  // link | copy（copy 模式下 dst 是受管副本）
  "owner": "library",              // library（受管）| external（迁移时发现的外部链接）
  "createdAt": "…", "lastSyncAt": "…"
}
```

---

## 4. 模块划分

七个模块，按依赖顺序排。M1 是地基，M2/M3 是主力，M4/M5 依赖它们，M6/M7 是配套。

```
M1 库与链接内核 ──┬─► M2 Skills 共享
                  ├─► M3 MCP 单一事实源
                  └─► M4 规则与记忆共享
M5 扫描 · effective · 健康度  ◄── 依赖 M1–M4 的台账
M6 界面（共享矩阵 · 收敛向导 · 预览）
M7 命令行 / MCP 工具 / 迁移兼容
```

---

### M1 · 库与链接内核（`core/src/library.rs` · `core/src/linkfs.rs` · `store.rs` 扩 Op）

**做什么**

1. 新增 `core/src/linkfs.rs`：链接原语与能力探测
   - `capability() -> LinkCap`：探测 junction / symlink / hardlink 可用性，缓存。
   - `create_dir_link(src, dst) -> Result<Method>`：~~junction → symlink → 失败~~
     **symlink → junction → 失败**（P0 实测定论，见实施手册 §12-4 / §16.2）。
   - `create_file_link(src, dst) -> Result<Method>`：~~hardlink → symlink → 失败~~
     **symlink → hardlink → 失败**（hardlink 会被 agent 的「写临时文件 + rename」
     打断，不当首选，实施手册 §12-3）。
   - `read_link_target(p) -> Option<PathBuf>`：~~`fs::read_link` 对 junction 可用~~
     **两句都不成立**：`read_link` 对 junction 在部分情况返回 Err，
     `symlink_metadata().file_type().is_symlink()` 也可能为 false。读目标必须走
     `FSCTL_GET_REPARSE_POINT`，判定类型必须走 `linkfs::entry_link_kind`
     （实施手册 §12-1 / §3.3）。
   - `is_link(p)` / `points_to(p, expected)` / `identity(p)`：用 `realpath` 做**身份**比较
     （现有 `dedupe_by_physical_path` 已做 canonicalize，复用同一份缓存）。
2. `store.rs` 扩两个 Op：`Op::link_dir(dst, src)`、`Op::link_file(dst, src)`。
   - ~~快照侧**零改动**~~ **必须改**：快照用 `ft.is_symlink()` 认不出 junction，会把
     链接当目录、顺着链接把库内整棵树快照进内存，还原时写成真实副本
     （实施手册 §12-2 / §3.4⑤）。`DirSnap::Link` 记录链接种类，还原按种类分派。
   - 失败还原时，链接还原继续走 `symlink_dir/symlink_file`；**junction 还原**要补一条：
     捕获时记录 `LinkKind::Junction`，还原用 reparse point（否则还原出的链接类型变了，
     下层的「零特权」前提就破了）。
3. 新增 `core/src/library.rs`：库根解析、`links.json` 台账读写、`add/remove/retarget/sync/resync`。

**验收**
- 在同一事务里「删除 agent 侧独立目录 + 建链接」，中途人为注入失败 → 目录原样回来（含 mtime 与链接类型）。
- 非管理员会话下，`create_dir_link` 返回 `Symlink` **或** `Junction` 都算通过——首选项由
  P0 实测定，本机实测 symlink 可用所以返回 `symlink`（实施手册 §12-4 / §16.2）。
- 台账与磁盘不一致时，`doctor` 报 `link.ledgerDrift`，且不改磁盘。

**不做**：网络盘、跨卷 hardlink、macOS/Linux 的差异处理（本期不验收）。

---

### M2 · Skills 共享

**做什么**

1. **库内新建 / 导入**：`New skill` 的落点从「选一个 agent 目录」改为「默认进库」，
   再由矩阵勾选要启用的 agent。保留「直接写到某个 agent 目录」作为高级选项（现状兼容）。
2. **收敛（每项预览 + 确认后执行）**：
   - 探测：同一 `slug(name)` 的 skill 出现在 ≥2 个 agent 目录时，比对**逐文件 SHA-256**；
   - 分三种裁决，全部列进确认框：
     - 完全一致 → 「保留一份到库，其余替换为链接」
     - 有差异 → 「**不动**，仅报告 `skill.diverged`，并给逐文件 diff 入口」
     - 单边存在 → 「并入库，原位置替换为链接」
   - 确认框必须列出：保留哪一份（真实路径）、会删除哪几份（真实路径）、每条新建链接的方法。
3. **矩阵视图**：行 = 库内 skill，列 = 已安装 agent，单元格 = 启用/未启用/失败；
   列头可整列启用/停用。
4. **卸载语义**：从某个 agent 停用 = 只删那条链接（**绝不删库内主副本**）；
   从库里删除 = 先移入 `<store>/.trash/skills/<slug>-<ts>/`，再删全部链接，`audit.jsonl` 各记一行。

**验收**
- 在一台有 5 份同名 skill 的机器上收敛后：库内 1 份、5 个 junction、`audit.jsonl` 6 行、盘上总字节数下降。
- 5 个 agent 里任一个改动 `SKILL.md` 的正文，其余 4 个立即看到（因为是同一 inode 的同一份）。
- 内容不一致的那组：收敛按钮置灰，报告里给出差异文件名清单。

**风险**：少数 agent 在扫描 skill 时对链接做 `realpath` 后校验「目录名 == frontmatter name」，
链接名与库内目录名不一致会触发告警。对策：库内目录名恒等于 slug，链接名也用同一 slug。

---

### M3 · MCP 单一事实源

**做什么**

1. 库内 `mcp/servers/<slug>.json` 存一份**规范形态**（name / transport / command / args / env / url / headers），
   不存任何 agent 特有的封装。
2. 每个 server 带 `allOf`：`{ "claude-code": true, "codex": true, "pi": false, … }`。
3. 打开某 agent 的开关 = 事务里对该 agent 的配置文件做 `Op::json_mutate`（JSON）
   或 `toml_splice`（TOML）**插入/更新这一条**；关闭 = **只删这一条**。
   - 只有 `command` 的最小条目，远端接受吗？按 agent 的现成桶名（`mcp_bucket_for`）套用；
   - **`enabled: false` 与「不存在」是两种状态**，必须区分：Pi / MiniMax 之类靠 `enabled` 字段而不是删除，
     这套差异按 agent 在 `adapters.rs` 里新增 `mcp_disable: DisableStyle::{Remove, Flag}`。
4. **漂移检测**：`doctor` 比对库内规范形态与各 agent 配置文件里的实际条目；
   不一致 → `mcp.drift`，给出「以库为准覆盖 / 以该 agent 为准回写库 / 忽略」三个动作。
5. **不侵占**：从未导入过的、用户手写的 server 一律不动；同名不同参**不覆盖**，报冲突（与 cc-switch 的
   「同名即覆盖」刻意相反——覆盖是静默丢数据）。

**验收**
- 在 Codex（TOML）与 Claude Code（JSON）同时开启一个 server：前者只重写该小节、保留其余注释；
  后者保留兄弟 server 与键序。
- 关闭后两个文件各自恢复原状（除该条）。
- 手写一个库外 server，跑 `doctor` → 无任何报告、无任何写入。

**风险**：`~/.claude.json` 与 `~/.claude/settings.json` 同时可能存在 MCP 配置，写入目标必须唯一。
对策：`mcp_target_for` 已经选「第一个存在且声明了 mcp 路径的文件」，本模块沿用并**在预览里显示选定文件**。

---

### M4 · 规则与记忆共享

**做什么**

1. **两类共享方式，用户选**：
   - `link`（整份指过去）：适合「所有 agent 读同一份 `AGENTS.md`」；
   - `compose`（聚合生成）：库内若干个规则片段，按选定顺序拼成目标文件，
     用首尾标记包裹：
     ```markdown
     <!-- agentkeeper:begin slug=team-conventions rev=<hash> -->
     …
     <!-- agentkeeper:end slug=team-conventions -->
     ```
     标记之外的内容（用户手写的部分）**逐字节保留**。
2. **`compose` 的写回策略**：目标文件若被 agent 或用户改过标记内内容 → `rule.foreignEdit`，
   不静默覆盖；确认框里给出三方 diff（库内版本 / 盘上版本 / 上次生成版本）。
3. **记忆**：`MEMORY.md` + `memory/` 目录整体走链接；若目标 agent 只认单文件，则退化为 `compose`
   （把库内 `MEMORY.md` 聚合进去）。
4. **规则改名映射**：~~`AGENTS.md` ↔ `CLAUDE.md` ↔ `GEMINI.md` 是「同一个内容、不同文件名」，
   矩阵里对这几列给出「同源」提示~~ **按适配器表实际声明做，不硬编码文件名对**：
   `rules` 是各 agent 各自声明的一串文件名（`pi` 用 `AGENTS.md`、`workbuddy` 用
   `SOUL.md`/`IDENTITY.md`），`CLAUDE.md`/`GEMINI.md` 是否在表里要先确认
   （实施手册 §12-7）。

**验收**
- `AGENTS.md` 被 4 个 agent 引用，改动库内版本 → 4 个位置同时生效（link 模式）。
- `compose` 模式下手工在标记外追加一行 → 下一次同步保留那一行。
- `compose` 模式下手工改标记内一行 → 报告 `rule.foreignEdit`，界面不自动覆盖。

**不做**：`.mdc` 的 frontmatter 合并（Cursor rules 有 `alwaysApply`/`globs` 等字段），
本期对 `.mdc` 只支持 `link`，`compose` 置灰并说明原因。

---

### M5 · 扫描 · effective · 健康度

**做什么**

1. **扫描器识别链接**：`scan_skills` 已跟随 junction（`scan.rs:266-269` 的 `exists_dir` 兜底），
   需要补的是**打标**：`asset.link = { method, src, managed: true|false }`。
2. **`dedupe_by_physical_path` 保持现状**（按 canonicalize 折叠）——同一份被 N 个 agent 引用仍然只有一条，
   `meta.alsoSeen` 列出 N 个读者。这是既有的正确行为，链接只是让它**变得更常见**，语义不需要改。
3. **`compute_effective` 需要新增一种关系**：现在只有 `shadowedBy` / `overrides`（project 遮蔽 global）；
   同一份经链接出现在两个 scope 时应标注 `sameInode: true`，**不判遮蔽**（否则会给用户「有冲突」的错误暗示）。
4. **健康检查新增 4 类 finding**：
   - `link.dangling`：链接指向的库内路径已不存在（critical，可直接修）；
   - `link.unmanaged`：指向库外位置的链接（info，可「纳管」）；
   - `link.hijacked`：agent 侧把链接替换成了真实目录/文件（critical，说明有 agent 在写它自己的副本）；
   - `asset.duplicated`：内容完全相同但不是链接的多份（info，建议收敛）。
5. **一个必须量出来的数字**：收敛后 `contextToken` 预估是否下降。
   现有概览页已在算 context 预算，收敛前后各跑一次，写进验收。

**验收**
- 一台机器上 990 条资产、收敛 30 个 skill 后，资产条数与「Curated + managed」计数按台账精确对上。
- 手工把某个 junction 换成真实目录 → 健康页出现 `link.hijacked`，且点修复能「重新链接（先确认内容一致）」。

---

### M6 · 界面

**做什么**

1. **新增「共享」页**（`VIEWS` 里加 `shared`，导航归入「操作」组，位置在「资产」之后），三个标签：
   - **Skills**：矩阵（行 = 库内 skill，列 = agent）
   - **MCP**：矩阵（行 = server，列 = agent）
   - **规则 / 记忆**：矩阵 + 「共享方式」列（link / compose / 未共享）
2. **收敛向导**：分三步（探测 → 逐项裁决 → 确认执行），每一步都可返回；
   第二步的每一条都能单独取消（默认全不选，避免误伤）。
3. **写入前二次确认沿用现有 `confirmWrite`**，但列表里要显式写出**将要删除的路径**与**将要创建的链接**，
   并标明链接方法（junction / symlink / hardlink / copy）。
4. **副本模式横幅**：`LinkCap = copy` 时页面顶部常驻「库改动不会自动生效，需 Resync」。
5. **失败逐项报告**：一个事务里 N 个目标，按现有 `reportWriteFailure` 的形态列 `restoreErrors[]`，
   不允许「整体失败」一句话吞掉细节。
6. **i18n**：所有新文案进 `src/renderer/i18n.js`（中英各一份），Rust 侧只发英文 code，
   遵守 `DESIGN.md` §2.1 的三条契约；新增 code 必须同时进 `engine-zh.js` 的 `CODE_ZH`。

**验收**
- 所有新视图在 1440×900 与 2180×1466 两档缩放下不溢出（沿用 `scripts/ui-audit.js`）。
- 矩阵的键盘可达：Tab 能走遍单元格、空格切换开关、列头有 `aria-label`。
- `npm run check` 全绿（含 i18n 漏翻与 dist 一致性）。

---

### M7 · 命令行 / MCP 工具 / 迁移

**做什么**

1. **MCP 工具**（现有 15 个之外新增 5 个，`docs/MCP-SERVER.md` 同步）：
   - `list_library` —— 库内资产与启用的 agent
   - `enable_asset` / `disable_asset` —— 对应矩阵开关（内部走同一事务）
   - `link_doctor` —— 跑 M1/M3/M4 的检测，返回 findings
   - `converge_plan` —— 只出方案、不写盘（给 Agent 用：先看再问人）
2. **`ak-scan`** 增加 `--shared`：打印库 + 台账 + findings 的完整 payload，供 CI 取数。
3. **迁移**：`Settings → 库位置` 可在「`~/.agents/skills`（共享约定）」与「`<store>/skills`（私有）」之间切换；
   切换动作是「逐个搬主副本 → 重指向全部链接」，中途不退出、允许中断后继续（在台账里记 `migrating` 状态）。
4. **首次启用本功能**：弹一次「发现 N 组重复技能，要收敛吗？」——**默认不收敛**，
   点进去就是 M6 的向导。

**验收**
- `converge_plan` 对任何输入都不写盘（用只读文件系统的测试夹具断言零写入）。
- 库位置切换在 200 个 skill 上中断后重跑，最终状态一致（幂等）。

---

## 5. 关键交互

### 5.1 共享矩阵（核心）

```
共享 · Skills                                    [+ 新建]  [⇩ 收敛重复]  [⋯]
────────────────────────────────────────────────────────────────────────────
搜索 …                          筛选: 全部 / 有更新 / 未启用 / 冲突
────────────────────────────────────────────────────────────────────────────
                      │Claude│Codex │Cursor│Gemini│Pi   │
────────────────────────────────────────────────────────────────────────────
pdf                   │  ●   │  ●   │  ○   │  ●   │ ⚠   │  库 · 2.1 MB
office-docx           │  ●   │  ●   │  ●   │  ●   │  ●   │  库 · 1.4 MB
team-wiki（外部）      │  ○   │  ○   │  ○   │  ○   │  ○   │  未纳管 ← 可「导入到库」
────────────────────────────────────────────────────────────────────────────
● 已启用（junction）  ○ 未启用  ⚠ 失败（点击查看原因）  ◐ 副本模式（需同步）
```

交互约定：
- **点单元格 = 立即执行**（与 cc-switch 一致），但执行前仍走 `confirmWrite`；
  批量（列头 / 多选行）**只弹一次确认**，列全部路径。
- **失败单元格**点击直接给原因 + 重试，不弹新页面。

### 5.2 收敛预览

```
收敛 5 组重复技能？                         危险操作 · 不可撤销（有 .trash 30 天）

第 1 组  pdf                      内容一致（12 个文件，逐字节相同）
  保留    C:\Users\me\.agents\skills\pdf            ← 库
  替换为链接
          C:\Users\me\.claude\skills\pdf   → junction
          C:\Users\me\.codex\skills\pdf    → junction
          C:\Users\me\.cursor\skills\pdf   → junction

第 2 组  repo-review              内容不一致，跳过
  差异    SKILL.md（Claude 版比 Codex 版多 3 行）、scripts/check.sh（不一致）

[取消]   [只收敛第 1 组]   [收敛全部一致组（1）]
```

---

## 6. 不变量（这几条不能被实现压过去）

1. **库内主副本是唯一可编辑对象**；agent 侧链接只读（编辑器打开链接时提示「改的是库内文件」）。
2. **删除链接 ≠ 删除资产**：停用某 agent 只能删链接；删资产必须先移入 `.trash` 并记审计。
3. **内容不一致绝不自动收敛**：只报告 + 给 diff，裁决权在用户。
4. **写入落点必须与 UI 显示一致**：预览里写的路径就是 `store.transaction` 实际写的路径
   （`DESIGN.md` §4.2 的「直写」原则在链接层同样成立——不引入「镜像树」）。
5. **库外的东西不碰**：未导入的 server、未纳管的链接、用户手写的规则，一律只读、只报告。
6. **每个链接都可解释**：任一行点开都能回答「它指向哪、谁建的、什么时候、用哪种方式」。

---

## 7. 风险与对策

### 7.1 收敛默认开启吗
`LinkCap` 的判定**必须来自 P0 的真实探测**，不能从 PowerShell 的结果外推（§3.2 的警示：
PowerShell 建不了 symlink，但 Rust 的 `symlink_dir` 可以）。实测结论是 symlink 与 junction
在普通用户会话都可用 → **默认开启**，但首次执行仍需显式确认（§5.2）。
若探测结果为 `copy`，默认**关闭**收敛，并在设置里说明原因。

### 7.2 agent 自己重写配置，把链接替换成真实文件
已列进健康检查 `link.hijacked`。机制上无法阻止（agent 有写权限），但可以：
- 每次扫描比对台账（成本已付，`dedupe` 本来就要 canonicalize）；
- 健康页给出「重新链接（内容一致才允许）」与「保留为独立副本」两个动作。

### 7.3 同一 MCP server 在不同 agent 里的参数不同
**不覆盖**（与 cc-switch 相反）。报告 `mcp.drift`，给三方动作：以库为准 / 以 agent 为准回写 / 标记为「独立」。

### 7.4 TOML 写入破坏注释
已有 `toml_splice` 按区段手术，前提是「只重写目标小节」。M3 复用同一条路径，**不允许**为图快改用整份
`toml` 序列化——那会把用户注释全部抹掉，是不可接受的回归。

### 7.5 盘符/路径变化（U 盘、OneDrive 重定向）
链接存的是绝对路径。库位置若落在会被重定向的目录（OneDrive），`doctor` 需能识别
`link.dangling` 并批量重指。**建议默认库位置就是 `~/.agents/skills`（用户主目录，不随盘符变）**。

### 7.6 删除安全
不建备份树（沿 `DESIGN.md` §4.2）。删除动作统一为：**移入 `<store>/.trash/<kind>/<slug>-<ts>/`**，
保留 30 天后由 `ensure_store` 清理（替换现在「启动即删 `backups/`」的那段逻辑，改为「清理过期 trash」）。

---

## 8. 分阶段与里程碑

| 阶段 | 内容 | 依赖 | 验收门槛 |
|---|---|---|---|
| **P0 探测**（0.5 天） | `linkfs::capability()` + 单元测试；`LinkCap` 进设置。**必须实测 symlink / junction / hardlink 三者在 Rust 侧的表现，不得从 PowerShell 结果外推** | — | 普通会话下给出真实的降级链 |
| **P1 内核**（2–3 天） | `Op::link_*`、junction 还原（需 FSCTL 读重解析点）、`library.rs`、`links.json`、`doctor` 骨架 | P0 | 事务失败还原含链接类型 |
| **P2 Skills**（3–4 天） | 库内新建/导入、收敛向导、矩阵、扫描打标 | P1 | 5 组重复 → 1 库 5 链；不一致组不动 |
| **P3 MCP**（3–4 天） | 单一事实源、开关物化、漂移检测、`DisableStyle` | P1 | TOML 注释保留；库外条目零触碰 |
| **P4 规则/记忆**（2–3 天） | `link` / `compose`、标记保留、三方 diff | P2 | 标记外内容逐字节保留 |
| **P5 健康 + UI 收口**（2 天） | 4 类 finding、i18n、UI 审计、`npm run check` | P2–P4 | 检查全绿 |
| **P6 CLI / MCP / 迁移**（2 天） | 5 个 MCP 工具、`ak-scan --shared`、库迁移 | P2–P4 | `converge_plan` 零写入 |

合计约 **15–18 个工作日**（含测试）。P2 与 P3 可以并行（都只依赖 P1 与内存台账）。

---

## 9. 测试与验收

### 9.1 分层（沿用 `DESIGN.md` §6.1 的原则）

| 层 | 内容 |
|---|---|
| `core` 单测 | `linkfs` 能力探测与降级链、`links.json` 读写、`compose` 标记插入/剥离（纯函数） |
| `tests/write_path.rs` | 链接事务：创建、还原、junction 类型保持、`.trash` 语义 |
| `tests/contract.rs` | 矩阵 payload 形状、`doctor` findings 形状、开关幂等 |
| `core/tests/mcp.rs` | 新增 5 个工具的协议顺序与「只读工具零写入」 |
| 真机夹具 | 用假 home 造 17 个 agent 目录 + 5 份同名 skill，跑收敛前后比对 |

### 9.2 必须量出来的数字

| 指标 | 采集方式 | 期望 |
|---|---|---|
| 收敛前后磁盘占用 | 扫描器 `bytes` 汇总 | 下降 |
| 收敛前后 `contextToken` 预估 | 概览页已有统计 | 不上升 |
| 首启扫描耗时 | `scan` payload 的 `timings` | 波动 < 15%（链接要多做 canonicalize 缓存） |
| 建 1 个链接耗时 | 事务 journal `durationMs` | < 20 ms |
| `npm run check` | CI | 0 失败 |

---

## 10. 与其他文档的关系

- `DESIGN.md` §1：开源对标。本计划把 cc-switch 作为第四个对标并说明「取哪一半」（§2）。
- `DESIGN.md` §3：适配器层。M3 会新增 `mcp_disable` 字段 → 必须跑 `npm run tauri:adapters` 重冻结快照。
- `DESIGN.md` §4：写入事务。M1 新增两种 Op，不改事务语义。
- `DESIGN.md` §8 路线图：本计划对应 v0.3，原 v0.3 的「CLI 通道」只保留 `ak-scan --shared`，`ak apply` 顺延。
- `docs/CHANGELOG.md`：实施时每个阶段在 `changelog/next.json` 记一条 `feat`，提交标题用同一套前缀。

---

## 附：与 cc-switch 的差异一览（给评审用）

| 问题 | cc-switch | 本计划 |
|---|---|---|
| Windows 上怎么建链接 | symlink，需开发者模式；失败回退 copy | **symlink 优先（Rust 实测普通会话可用）**，被拒时回退 junction；两者都不行才 copy |
| 元数据在哪 | SQLite | 磁盘 + `links.json` 索引 + 追加式审计（无库） |
| 同名 MCP 参数冲突 | 库覆盖 agent | **不覆盖，报 drift** |
| 内容不一致的重复 skill | 未明确 | **绝不自动收敛**，只报告 + diff |
| 删除安全 | 备份目录 | `.trash` 30 天（不做备份树，沿本项目既有立场） |
| 聚合型规则 | 无 | **有**（`compose` 标记区块，保留标记外内容） |
