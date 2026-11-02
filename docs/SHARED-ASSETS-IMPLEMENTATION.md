# 共享资产实施手册（Shared Assets Implementation Handbook）

**本文是实施文档，不是计划文档。** 计划与决策依据见 `SHARED-ASSETS-PLAN.md`；
本文只回答一个问题：**现在打开一个空窗口，第 1 步敲什么、第 N 步验收什么。**

> **第 0 节起的所有数字都来自对本仓库 0.2.4（commit `24b32f4`）源码与门禁脚本的逐行核对，
> 以及在本机跑出的基线。** 凡与 `SHARED-ASSETS-PLAN.md` 或本轮之前口径不同的地方，以本文为准；
> §12 列了 7 处冲突。标注「待 P0 实测」的，是必须用 `link-probe` 在本机定论、不能从文档外推的。

| 项 | 值 |
|---|---|
| 仓库 | `F:\demo\tools\agentkeeper` |
| 目标版本 | v0.3 |
| 平台 | Windows 10/11 x64（仅此范围，不做 macOS/Linux） |
| 基线 | 0.2.4，commit `24b32f4` |
| 工期 | 14–17 个工作日（P0→P6，含测试；见 §11 的改动幅度） |
| 当前状态 | **M1（P0+P1）、M2（P2）、M3（P3）、M4（P4）、M5（P5 的 Rust 侧）已完成并验收**（2026-11-02）；M6 起未开始。验收记录见 §16、§17、§18、§19 与 §20 |
| 环境 | rustc/cargo 1.99.1 · tauri-cli 2.12.4 · MSVC 14.44 · SDK 10.0.26100 · Node 22.22.1 |

> **本轮之后开工的人先读 §16–§20**：§16 是 M1 的实际交付清单与实测结论（与本手册初稿
> 不同的地方已就地改写），§17 是 M2 的交付清单、三条验收的落点、以及在实施中修掉的五个
> 既有缺陷 —— 其中三个会直接毁数据，务必先看一遍；§18 是 M3（MCP 单一事实源）
> 的交付清单、四条验收的落点，以及实施中修掉的一处 TOML 换行混写缺陷；§19 是 M4
> （规则与记忆共享）的交付清单、四条验收的落点，以及实施中修掉的三个缺陷（其中两个
> 会静默写坏内容）；§20 是 M5（扫描与健康度）的交付清单、七类 finding 的判据与三条
> 验收的落点，以及实施中清零的一处既有缺陷 —— 扫描去重会把「同一个文件的规则与记忆」
> 折成一条，记忆计数因此少一份。

---

## 0. 开工前的 3 分钟清单

新窗口第一件事，按顺序执行，不要跳：

```powershell
# 1. 接线工具链（每个新终端都要；PATH 里没有 cargo 是设计如此，详见 docs/RUST-ENV.md）
cd F:\demo\tools\agentkeeper
. .\scripts\rust-env.ps1
#    期望输出五行：cargo / rustc / cargo-tauri / MSVC 14.44 / Win SDK 10.0.26100

# 2. cargo 必须在工作区根上跑（仓库根没有 Cargo.toml，在仓库根跑 cargo 会报错）
cd tauri-shell

# 3. 基线自检（4 项全绿才开始改代码）
cargo test --workspace              # 期望 333 passed, 0 failed
cd ..; node scripts/check.js        # 期望 all 8 gates passed
node scripts/test-affected.js       # 期望 exit 0
node scripts/ui-audit.js            # 期望 total findings: 0（它永远 exit 0，看数字）
```

**实测基线（2026-11-02 本机，干净工作树）**：

| 命令 | 实测结果 | 备注 |
|---|---|---|
| `cargo test --workspace` | **333 passed / 0 failed** | 分布：core 单测 197、core `mcp` 7、shell 单测 2 + 10、`contract` 52、`update` 46、`boundary` 0、`write_path` 19 |
| `node scripts/check.js` | **all 8 gates passed** | 关口：`residue` `bridge` `tree` `renderer` `ui` `i18n` `dist` `untracked` |
| `node scripts/ui-audit.js` | **total findings: 0**（I18N/INTERACTION/THEME/UX 全 0，ACCEPTED 2） | 发现项非 0 时它**仍然 exit 0**（§11.1），不能只靠 `check.js` 的 ui 关口 |
| `node scripts/test-affected.js` | exit 0（只报「哪些测试受改动影响」） | 没有改动时输出 `no Rust test covers the changed files` |
| `node scripts/tauri-i18n.js` | MISS 0 / CODE-MISS 0 / AMBIGUOUS 0；EXACT 109 + PATTERNS 73 | `CODE-INTERNAL` 4 条不计失败（不可达渲染层） |

**已知基线噪声（不是你的 bug，别去修，除非任务点名）**：
`cargo test -p agentkeeper-core` 里 `update::manager::tests` 两个并发测试
（`concurrent_checks_share_one_request`、`status_reports_checking_while_a_check_is_in_flight`）
在某些调度下会失败，压测失败率分别约 1.5% / 0%，复跑即过。根因已定位（见 §12.2），
属既有缺陷，与本功能无关。本轮全量跑一次未复现。

### 0.1 每次提交前的门禁（缺一不可）

```powershell
cd tauri-shell; cargo test --workspace; cd ..
node scripts/check.js            # residue / bridge / tree / renderer / ui / i18n / dist / untracked
node scripts/ui-audit.js         # 看 total findings 是否仍为 0（它不看退出码）
node scripts/test-affected.js    # 只跑受影响的
npm run changelog:add -- feat "一句话说清用户能感知的变化"
```

提交标题必须用 `新增: ` / `修复: ` / `优化: ` 前缀（`.githooks/commit-msg` 会拒绝其他形式）。
装钩子：`npm run hooks:install`。

> 实测：`node scripts/changelog.js add feat "…"` 与
> `npm run changelog:add -- feat "…"` 等价，两者都会把条目追加到
> `changelog/next.json`（当前 24 条）。**开新阶段前先确认记录是干净的**
> （`npm run changelog:check`），别把本轮探测写的测试条目带上。

---

## 1. 硬约束（写代码前先记住，违反其一就返工）

| # | 约束 | 违反的后果 | 怎么被抓到 |
|---|---|---|---|
| C1 | **`core::` 不许碰 Tauri 类型** | 编译期直接断（这是设计） | 拆 crate 保证 |
| C2 | **所有写入必须走 `store.transaction()`** | 快照/还原/journal 全部失效 | `src-tauri/tests/write_path.rs` |
| C3 | **Rust 侧只发英文 sentence 或 code，中文一律渲染层翻** | `tauri-i18n.js` exit 1 | `check.js` 的 i18n 关口 |
| C4 | **新 code 必须同时进 `engine-zh.js` 的 `CODE_ZH`** | CODE-MISS 红灯 | `tauri-i18n.js`（扫 `core/src` 与 `src-tauri/src` 全部 `.rs`） |
| C5 | **每个新 `#[tauri::command]` 必须在 `tauri-bridge.js` 有形状匹配的 `call(...)`** | bridge 关口红灯 | `check.js`（比对参数形状，`Option<T>` 算可选） |
| C6 | **不许破坏 TOML 注释**（只许 `toml_splice` 按区段手术） | 用户配置被抹 | 人工评审 + 测试夹具 |
| C7 | **不引入镜像树/暂存层**，写入直落真实路径 | UI 显示与实际不一致 | 代码评审 |
| C8 | **`adapters.rs::AGENTS` 与 `scripts/adapter-table.js` 必须同步**，改完跑 `npm run tauri:adapters` | 适配器快照测试红 | `adapters.rs` 内置快照（17 个 agent / 187 条路径模板） |
| C9 | **禁止把 `tauri-shell/dist/` 手改**，只由 `node tauri-shell/copy-frontend.js` 生成 | dist 一致性红灯 | `check.js` 的 dist 关口（`--check`） |
| C10 | 注释用中文；新代码零英文注释（`//!`、`///` 模块文档也算） | `npm run comments:lang` 进度倒退 | 人工 |
| C11 | **新增前端脚本文件要在 7 处登记**（§11.1），否则某些检查会静默漏掉它 | 打包后空白页，本地却全绿 | 只有 `checkGlobalCollisions` 会间接抓到一部分 |

---

## 2. 交付物总览

### 2.1 新增文件

| 文件 | 行数估计 | 模块 | 职责 |
|---|---|---|---|
| `tauri-shell/core/src/linkfs.rs` | ~420 | M1 | 链接原语 + 能力探测（junction 需 FSCTL） |
| `tauri-shell/core/src/library.rs` | ~700 | M1 | 库根、`links.json` 台账、add/remove/retarget/resync |
| `tauri-shell/core/src/compose.rs` | ~260 | M4 | 规则区块标记的插入/替换/剥离（纯函数） |
| `tauri-shell/core/src/sharing.rs` | ~900 | M2/M3/M4 | 收敛探测、MCP 物化、规则共享的事务编排 |
| `tauri-shell/core/src/doctor.rs` | ~450 | M5 | 7 类 finding 的检测（只读） |
| `tauri-shell/src-tauri/tests/sharing.rs` | ~600 | 测 | **集成测试必须放这里**（理由见 §3.7）；同目录已有 `common/mod.rs` 可直接 `mod common;` |
| `src/renderer/view-shared.js` | ~1100 | M6 | 共享页（矩阵 / 收敛向导 / 预览） |
| `scripts/adapter-table.js` 的字段 | — | M3 | 每条 agent 加 `mcpDisable`（C8） |

> `src/renderer/` 现在是 6 个文件：`theme-init.js` `engine-zh.js` `i18n.js` `app.js`
> `styles.css` `index.html`。`app.js` 实测 **4287 行 / 204,826 字节**（不是 4300 行那么
> 精确），共享页单独成文件后约 5400 行的顾虑仍然成立，**建议拆**。
> 拆的代价是 §11.1 的 7 处登记，漏一处就会出现「本地全绿、打包空白」。

### 2.2 改动文件

| 文件 | 改动性质 |
|---|---|
| `tauri-shell/core/src/lib.rs` | `pub mod` 增加 5 个模块（**按字母序插入**：`compose` 在 `assets` 之后、`discover` 之前；`doctor`/`library`/`linkfs`/`sharing` 同理） |
| `tauri-shell/core/Cargo.toml` | `windows-sys` 的 features **必须补** `Win32_Storage_FileSystem`、`Win32_System_IO`、`Win32_System_Ioctl`、`Win32_Security`（见 §3.3） |
| `tauri-shell/core/src/store.rs` | `OpKind` +2、`Op` +2 构造器、`DirSnap::Link` 加 `kind`、`Snapshot::Link` 加 `kind`、`restore()`/`restore_dir()` 按类型还原、`STORE_LINK_FAILED` |
| `tauri-shell/core/src/scan.rs` | 资产打 `meta.link` 标；`compute_effective` 增加 `sameInode` |
| `tauri-shell/core/src/keeper.rs` | 新增 `enable_asset`/`disable_asset`/`converge_plan`/`library_*`；`distribute` 保留 copy 分支并加 link 分支 |
| `tauri-shell/core/src/settings.rs` | `defaults()` 增 `linkMethod`、`libraryRoot`、`trashDays`；`ensure_store` 增 `.trash` 清理 |
| `tauri-shell/core/src/adapters.rs` + `scripts/adapter-table.js` | `Agent` 增 `mcp_disable: DisableStyle`（改完 `npm run tauri:adapters`） |
| `tauri-shell/core/src/mcp.rs` | `tools()` +5、`call_tool` 分发 +5 |
| `tauri-shell/core/src/bin/ak-scan.rs` | `--shared` 参数 |
| `tauri-shell/core/tests/mcp.rs` | `const TOOLS: [&str; 15]` → `[&str; 20]`（追加、不重排） |
| `tauri-shell/src-tauri/src/lib.rs` | +8 个命令 + `generate_handler!` 列表 |
| `tauri-shell/tauri-bridge.js` | +8 个 `window.ak.*` |
| `src/renderer/index.html` | 导航项 + script 标签 |
| `src/renderer/app.js` | `VIEWS` 注册 `shared`、`applyLanguage` 的导航标签、`render()` 的 `acts` |
| `src/renderer/i18n.js` | 中英各约 60 条新文案（两侧各 462 条 → 各约 522 条） |
| `src/renderer/engine-zh.js` | `CODE_ZH` 新增约 14 个 code（现有 84 条） |
| `src/renderer/styles.css` | 矩阵/横幅/向导样式（**不许出现裸 hex 色值**，`ui-audit` 会报 hardcoded color） |
| `scripts/copy-frontend.js` | `PLAIN` 白名单加 `view-shared.js` |
| `scripts/lint-renderer.js` | 4 处硬编码 `app.js` 改成「全部 renderer 脚本」（§11.1 第 3/4 行） |
| `scripts/ui-audit.js` | 改成读全部 renderer 脚本；并核对「批量/分页」探针（§11.1 第 5/6 行） |
| `scripts/test-affected.js` | 新增 `shared` 区域映射（否则改了新文件一条测试都不跑） |
| `scripts/ui-baseline.js` | （可选）`VIEWS` 加 `shared` 并重新 capture |
| `docs/DESIGN.md` | 新增「共享层」章节，并入本文的可长期保留部分 |
| `docs/MCP-SERVER.md` | 20 个工具表（现有 15 个，`MCP-SERVER.md:136` 的标题「工具 API（15 个）」要改） |
| `README.md` / `README.en.md` | 「15 工具」→「20 工具」（各有 2 处） |
| `changelog/next.json` | 每阶段一条 `feat` |

---

## 3. M1 · 链接内核（一切的基础，先做完再碰别的）

### 3.1 先做实测，不要先写抽象

**在动手写 `linkfs.rs` 之前**，先建一个临时 bin 把这些问题测出来，结果写进 `linkfs.rs` 的模块注释：

```rust
// tauri-shell/core/src/bin/link-probe.rs（临时，测完删除，结论写进 linkfs.rs 注释）
// 要回答的问题：
// 1) std::os::windows::fs::symlink_dir 在【当前会话】是否成功？is_symlink/read_link 行为？
// 2) FSCTL 建出来的 junction，std::fs::read_link 能否读回？symlink_metadata().file_type().is_symlink() 是什么？
// 3) 用 junction 替换一个已存在的目录后，std::fs::remove_dir 是否只删链接不删内容？
// 4) 通过 junction 打开文件句柄时，不带 FILE_FLAG_OPEN_REPARSE_POINT 的
//    GetFinalPathNameByHandleW 返回库内路径（前缀 \\?\）还是链接路径？← 决定 §3.5 的同一性判定
// 5) std::fs::hard_link 在同一卷/跨卷分别返回什么？
```

**已知部分结论**（上一轮已实测，可直接用，但要在目标机复测）：
- Rust 的 `symlink_dir` 在**普通用户会话**下创建**成功**（`is_symlink=true`、`read_link` 可读、可透过链接读文件）。
- PowerShell 的 `New-Item -ItemType SymbolicLink` 在同一会话被拒。
- **结论：不能从 PowerShell 的结果外推 Rust 的能力。** 降级链以 `linkfs` 自己的实测为准。
- 本次核对到的一个**与直觉相反**的事实：仓库里**没有任何 `linkfs` / 链接能力探测的现成代码**，
  `store.rs` 只有在快照/还原里用 `std::fs::symlink_dir/symlink_file`（`store.rs:455`）。
  所以 P0 是**从零**开始，不是「接上已有实现」。

### 3.2 `linkfs.rs` 对外 API（照抄签名）

```rust
//! 链接原语与能力探测。
//!
//! ## 为什么不用 std 建 junction
//! std 没有 junction API，只有 symlink。junction 必须自己发 FSCTL_SET_REPARSE_POINT，
//! 否则「symlink 被拒时的回退」这一层根本不存在。

use std::path::{Path, PathBuf};

/// 一条链接实际是用什么建的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkMethod {
    Symlink,   // std::os::windows::fs::symlink_dir / symlink_file
    Junction,  // FSCTL_SET_REPARSE_POINT + IO_REPARSE_TAG_MOUNT_POINT
    Hardlink,  // std::fs::hard_link，仅文件
    Copy,      // 不是链接，是受管副本（"降级到复制"）
}

impl LinkMethod {
    /// 写进 links.json 与审计日志的字符串，**改它就是改数据格式**。
    pub fn as_str(self) -> &'static str { /* "symlink" | "junction" | "hardlink" | "copy" */ }
    pub fn parse(s: &str) -> Option<LinkMethod> { /* 严格匹配，未知返回 None */ }
}

/// 本机支持哪些方式。
pub struct LinkCap {
    pub symlink_dir: bool,
    pub symlink_file: bool,
    pub junction: bool,
    pub hardlink: bool,
    /// 顺序已排好：directories 给目录用，files 给单文件用。
    pub dir_order: Vec<LinkMethod>,
    pub file_order: Vec<LinkMethod>,
    /// 探测用的临时目录（用于在 UI 上解释「为什么是 copy 模式」）。
    pub probed_at: String,
}

pub fn capability(probe_root: &Path, cache: Option<&Path>) -> LinkCap;

pub fn create_dir_link(src: &Path, dst: &Path, prefer: Option<LinkMethod>) -> Result<LinkMethod, String>;
pub fn create_file_link(src: &Path, dst: &Path, prefer: Option<LinkMethod>) -> Result<LinkMethod, String>;

/// 读回链接目标。**junction 与 symlink 都要能读**。
pub fn read_target(p: &Path) -> Option<PathBuf>;

/// 这条路径是不是链接（symlink 或 junction）。
/// 注意：junction 的 is_symlink() 在部分 Rust 版本是 false，所以本函数不能只用它。
pub fn is_link(p: &Path) -> bool;

/// 判定链接类型，用于快照/还原时保持原类型。
pub fn link_kind(p: &Path) -> Option<LinkMethod>;

/// 解除链接本身，不跟随（junction 用 remove_dir，symlink 用 remove_file）。
pub fn unlink(p: &Path) -> std::io::Result<()>;

/// 同一物理对象？（realpath 归一后比较）
pub fn same_target(a: &Path, b: &Path) -> bool;

/// **新增（本轮补）**：目录里某一条目的链接类型。Windows 上**必须**用它，
/// 因为 `DirEntry::file_type()` 对 junction 返回的不是 `is_symlink()`（见 §3.5）。
pub fn entry_link_kind(p: &Path) -> Option<LinkMethod>;
```

### 3.3 junction 的实现要点（最容易踩坑的一段）

`std` 没有 junction，必须直接调 Win32。**本仓库的 `windows-sys` 特征清单目前是不够的**：

```toml
# tauri-shell/core/Cargo.toml:60 —— 现状（只为 DPAPI 开的两项）
windows-sys = { version = "0.52", features = ["Win32_Foundation", "Win32_Security_Cryptography"] }

# 本功能需要追加（已对本机 registry 里的 windows-sys 0.52.0 核对，以下符号都存在）：
#   Win32_Storage_FileSystem  → CreateFileW / CreateSymbolicLinkW / GetFinalPathNameByHandleW
#                               FILE_FLAG_OPEN_REPARSE_POINT / FILE_FLAG_BACKUP_SEMANTICS
#                               FILE_ATTRIBUTE_REPARSE_POINT / FILE_WRITE_EA
#   Win32_System_IO           → DeviceIoControl
#   Win32_System_Ioctl        → FSCTL_SET_REPARSE_POINT / FSCTL_GET_REPARSE_POINT
#                               IO_REPARSE_TAG_MOUNT_POINT（**注意：常数在 Ioctl 模块，不在 FileSystem**）
#   Win32_Security            → CreateFileW 的文档要求（以及 §3.5 的 TOKEN_QUERY / GetTokenInformation）
windows-sys = { version = "0.52", features = [
  "Win32_Foundation", "Win32_Security_Cryptography", "Win32_Security",
  "Win32_Storage_FileSystem", "Win32_System_IO", "Win32_System_Ioctl",
] }
```

> 漏 `Win32_System_Ioctl` 的表现是 `FSCTL_SET_REPARSE_POINT` 未定义（编译期），
> 漏 `Win32_Security` 的表现是 `CreateFileW` 未定义 —— 两者都会在 P0 第一小时撞上。

```rust
// 伪代码，只写关键调用顺序；错误分支全部返回 String（调用方转成 TxError）
fn create_junction(target: &Path, link: &Path) -> std::io::Result<()> {
    // 0. target 必须是**绝对路径**，并去掉 `\\?\` 前缀；相对路径建出来的 junction 会解析错
    // 1. CreateDirectoryW(link, null) —— junction 的宿主必须是一个已存在的空目录
    // 2. CreateFileW(link, GENERIC_WRITE, 0, null, OPEN_EXISTING,
    //                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS, null)
    // 3. 组 REPARSE_DATA_BUFFER（结构体按定义手写 #[repr(C)]）：
    //      ReparseTag        = IO_REPARSE_TAG_MOUNT_POINT (0xA0000003)
    //      ReparseDataLength = 8 + 路径缓冲区长度
    //      Reserve           = 0
    //      SubstituteNameOffset = 0
    //      SubstituteNameLength = "\\??\\" + target 的**字节数，不含结尾 NUL**
    //      PrintNameOffset      = SubstituteNameLength + 2   （+2 是结尾 NUL）
    //      PrintNameLength      = target 的字节数，不含结尾 NUL
    //      PathBuffer = L"\\??\\" + target + L"\0" + target + L"\0"
    //    （偏移全部是**相对于 PathBuffer 起点的字节数**，这是最容易写错的地方；
    //      长度字段不含结尾 NUL，而第 4 步传入的总长度 = 8 + ReparseDataLength）
    // 4. DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, &buffer, buffer_len, null, 0, &mut ret, null)
    // 5. 失败时清理第 1 步建的空目录，避免留下垃圾
}
```

**读取 junction** 同理：`CreateFileW(..., FILE_FLAG_OPEN_REPARSE_POINT)` +
`FSCTL_GET_REPARSE_POINT`，解析 `PathBuffer` 的第一个 NUL 结尾串（`\??\C:\...`），
去掉 `\??\` 前缀。**用 `std::fs::read_link` 读 junction 在部分情况下返回 Err**，
所以 `read_target` 必须自己实现这条路径，不能只包一层 `read_link`。

### 3.4 `store.rs` 的改动（逐处，行号为实测）

**① `OpKind` 加两个变体**（`store.rs:745`）：

```rust
pub enum OpKind {
    Write, CopyDir, Json, Delete, Mkdir,
    LinkDir,   // 新建 dst 为指向 from 的目录链接
    LinkFile,  // 新建 dst 为指向 from 的文件链接（symlink 优先）
}
// OpKind::name()（store.rs:753）里补 "linkDir" / "linkFile"（写进审计日志与 op 回显）
```

> **降级链统一（本轮定论，与计划文档相反）**：`create_dir_link` 的顺序是
> `symlink → junction → Err`；`create_file_link` 是 `symlink → hardlink → Err`。
> `copy` **不由 linkfs 产生**——Copy 是 `Op::copy_dir` 的语义，由调用方显式选择。
> 理由见 §12.5：P0 实测 symlink 可用时它就是最简单的方案；被测不可用时 junction 顶上。

**② `Op` 两个构造器**（`Op` 与构造器在 `store.rs:767`–`:825`）：

```rust
pub fn link_dir(dst: &str, src: &str) -> Op { /* kind=LinkDir, from=Some(src), path=dst */ }
pub fn link_file(dst: &str, src: &str) -> Op { /* kind=LinkFile, from=Some(src), path=dst */ }
```

**③ `transaction()` 前置校验**（`store.rs:994`–`:1001`）：目前只校验 `CopyDir` 缺 `from`，要扩成三个：

```rust
if matches!(op.kind, OpKind::CopyDir | OpKind::LinkDir | OpKind::LinkFile) && op.from.is_none() { ... }
```

**④ 执行分支**（`store.rs:1037` 的 `match op.kind`，`CopyDir` 分支在 `:1076`，新臂紧随其后）：

```rust
OpKind::LinkDir => {
    let src = op.from.clone().unwrap_or_default();
    if !Path::new(&src).is_dir() {
        return Err(TxError::plain(format!("link source is not a directory: {}", src)));
    }
    if Path::new(&real).exists() || linkfs::is_link(Path::new(&real)) {
        return Err(TxError::plain(format!("link target already exists: {}", real)));
    }
    if let Some(parent) = Path::new(&real).parent() { let _ = std::fs::create_dir_all(parent); }
    let method = linkfs::create_dir_link(Path::new(&src), Path::new(&real), None)
        .map_err(TxError::plain)?;
    log.push(json!({ "op": "linkDir", "path": real, "from": src, "method": method.as_str() }));
}
// LinkFile 同构，只是校验 is_file() 与 create_file_link
```

**⑤ 快照侧：必须记录链接的“种类”**（本次改动里最容易漏的地方）。
`store.rs` 里链接被捕获/还原的地方有**两处**，两处都要改：

| 位置 | 现状 | 改法 |
|---|---|---|
| `DirSnap` 枚举（`store.rs:833`–`:841`） | `Link(String, String, bool)` | 改成结构体变体 `Link { rel, target, is_dir, kind }` |
| `Snapshot` 枚举 + `Store::snapshot()`（`store.rs:941`–`:952`） | `Snapshot::Link { path, target, is_dir }` | 加 `kind` |
| `collect_dir_seen`（`store.rs:349`） | 只判 `ft.is_symlink()` | 改成 `linkfs::entry_link_kind(&from)`，**因此 junction 才能被捕获** |
| `restore_dir`（`store.rs:453`） | 一律 `symlink_dir/symlink_file` | 按 `kind` 分派：`Junction → create_junction`、`Hardlink → std::fs::hard_link`、其余 `symlink_*` |
| `Store::restore` 的 `Snapshot::Link` 臂（`store.rs:1287`） | 一律 `symlink_*` | 同上按 `kind` 分派 |
| `entry_name`（`store.rs:479`） | 模式匹配 `Link(name, _, _)` | 跟着结构体变体改 |

**为什么必须用 `entry_link_kind` 而不是 `file_type().is_symlink()`**：
`scan.rs` 自己已经为 junction 写过兜底（`scan.rs:264`–`:269`：`if !is_dir && e.is_symlink { is_dir = exists_dir(&dir) }`），
说明**作者已经观察到 junction 在 `DirEntry` 层不报 `is_symlink`**。老代码在 restore 时
依赖 `ft.is_symlink()` 才能认出链接；一个 junction 在快照时若被当成「目录」，
快照会**顺着链接把整个库内容当普通文件包一遍**，还原时把它写成真实副本 —— 静默、昂贵、不可察觉。

**⑥ 新增失败码**（`store.rs:616` 附近，与 `RESTORE_SYMLINK_FAILED` 并列）：

```rust
pub const STORE_LINK_FAILED: &str = "store.linkFailed";
```
并同步 `src/renderer/engine-zh.js` 的 `CODE_ZH`：
`'store.linkFailed': '创建链接失败：§1'`（`zhCode` 用 `§1` 占位符，见 `engine-zh.js:361` 起的现有条目）。

### 3.5 `library.rs` 对外 API

```rust
pub struct Library { /* root: PathBuf, store: String */ }

impl Library {
    /// 库根：设置里的 libraryRoot，未设置时默认 `~/.agents`（`~/.agents/skills` 的父级）。
    /// **不创建目录**，只解析。
    pub fn root(&self, user_home: &str, settings: &Value) -> PathBuf;

    /// skills / mcp / rules / memory 四类的主副本目录。
    pub fn dir_for(&self, kind: &str) -> PathBuf;

    /// links.json 台账读写。损坏时**不当作空**，返回 Err —— 与 store 对损坏 JSON 的立场一致。
    pub fn load_ledger(&self) -> Result<Ledger, String>;
    pub fn save_ledger(&self, l: &Ledger) -> Result<(), String>;

    /// 登记一条链接（只记账，不落盘）。真正的落盘由调用方组织成 Op 走 transaction。
    pub fn record(&self, l: &mut Ledger, entry: LinkEntry);

    /// **新增（本轮补）**：`p` 是否指向库内（`same_target(p, 库内主副本)` 或 realpath 落在库根下）。
    /// `disable_asset` 的安全闸与 `link.unmanaged` 判定共用这一个函数。
    pub fn owns_target(&self, cap: &LinkCap, p: &Path) -> bool;
}

#[derive(Clone)]
pub struct LinkEntry {
    pub kind: String,        // skill | rule | memory | mcp
    pub name: String,        // slug
    pub src: String,         // 库内主副本绝对路径
    pub dst: String,         // agent 侧路径
    pub method: LinkMethod,
    pub mode: String,        // "link" | "copy"
    pub owner: String,       // "library" | "external"
    pub created_at: String,
    pub last_sync_at: String,
}
```

**默认库根与既有约定一致**：扫描把 `~/.agents/skills` 当伪 agent `shared`
（`scan.rs:1806` 起），库根取它的父目录 `~/.agents`，`skills/` 子目录正好就是那个共享
目录 —— 收敛 5 份 skill 之后，`shared` 伪 agent 位置本身就是「库内主副本」，用户看到
的资产条数不会因为「多出一个库目录」而翻倍。

**ledger 的写入时机**：`save_ledger` 必须在 `transaction()` **成功之后**调用，
失败则不动台账。台账本身写坏不致命（`doctor` 会以磁盘为准重建），
但**不允许**用台账去覆盖磁盘。

**`same_target` 的正确实现**：`std::fs::canonicalize` 只做**词法 + 链接跟随**，
不做 UAC 别名归一（`C:\Program Files` 与 `C:\Program Files (x86)` 这类），也**不把盘符大小写统一**
（`d:\` 与 `D:\` 视为不同）。所以：

```
1. 先 try canonicalize；
2. 失败（悬空链接）时退回「父目录 canonicalize + 文件名」；
3. 最后统一 to_lowercase()（Windows 路径大小写不敏感）再比较。
```

§3.1 的第 4 个探测问题就是为这件事服务的：如果句柄路径返回的是库内路径，
`same_target` 可以直接用句柄，不必再猜。

### 3.6 M1 验收（4 条，全过才算完成）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core linkfs
cargo test -p agentkeeper-shell --test write_path
```

1. **非管理员会话**下 `create_dir_link` 返回 `Symlink` 或 `Junction`，**不是** `Err`。
2. 事务里「删除 agent 侧真实目录 + 建链接」，用一个必然失败的后续 op 触发还原 →
   目录原样回来，**内容逐字节相同**（用 `tests/common/mod.rs` 的 `dir_snapshot`）。
3. **junction 还原后仍是 junction**（`linkfs::link_kind` 断言为 `Junction`），不是被悄悄换成 symlink。
4. 台账被手工改坏（写个 `{`）→ `load_ledger` 返回 `Err`，**不返回空台账**，且不触碰磁盘。

### 3.7 测试放在哪个 crate（本轮实证：**必须是 `src-tauri`**）

`src-tauri/tests/contract.rs` 的开头写着原因：`Keeper` 与 `Store` 住在 `core`，
但集成测试一律 `use agentkeeper_shell_lib::core::…`，因为所有集成套件都是
**外壳 crate 的集成测试**。这与 cargo 的常规做法相反，但现状如此：

```powershell
cargo test -p agentkeeper-core --test write_path   # → error: no test target named `write_path` in package `agentkeeper-core`
cargo test -p agentkeeper-shell --test write_path  # → ok. 19 passed
```

所以：

| 测试 | 放哪里 | 怎么跑 |
|---|---|---|
| `linkfs` 能力探测 / `compose` 纯函数 / 台账读写 | `core/src/*.rs` 的 `mod tests` | `cargo test -p agentkeeper-core <名字>` |
| 链接事务：创建 / 还原 / 类型保持 / 台账损坏 | `tauri-shell/src-tauri/tests/write_path.rs`（**追加**，别新建） | `cargo test -p agentkeeper-shell --test write_path` |
| 收敛端到端、矩阵 payload 形状、doctor findings | `tauri-shell/src-tauri/tests/contract.rs` 或新建 `sharing.rs` 同目录 | `cargo test -p agentkeeper-shell --test sharing` |

> `core/tests/` 下**只有** `mcp.rs` 和 `common/mod.rs`（`common/mod.rs` 是一行文档注释，
> 没有任何代码）—— 不要以为 `tests/common/` 能直接复用，它只属于 `mcp.rs`。
> **`src-tauri/tests/` 下已经有一个完整的 `common/mod.rs`**（255 行：`temp_root` / `cleanup` /
> `serial` / `write` / `read` / `exists` / `dir_snapshot` / `join`），
> `write_path.rs:16` 的 `mod common;` + `:23` 的 `use common::{…}` 就是用法；新建的
> `sharing.rs` 只要写 `mod common;` 即可，**不要复制一份**。

---

## 4. M2 · Skills 共享

### 4.1 收敛探测（只读，M7 的 `converge_plan` 复用同一函数）

```rust
// sharing.rs
pub struct ConvergeGroup {
    pub name: String,              // slug
    pub verdict: Verdict,          // Identical | Diverged | SingleSource
    pub keeper: Option<String>,    // 保留哪一份（绝对路径）
    pub replace: Vec<ReplaceItem>, // 要替换成链接的路径
    pub diffs: Vec<FileDiff>,      // Diverged 时逐文件差异（只给文件名 + 状态，不给全文）
}

pub enum Verdict { Identical, Diverged, SingleSource }

pub struct FileDiff {
    pub rel: String,
    pub state: String,   // "only-a" | "only-b" | "different"
}

/// **纯只读**：不写盘、不建目录、不改台账。
pub fn plan_converge(assets: &[Value], library: &Library, cap: &LinkCap) -> Vec<ConvergeGroup>;
```

判定规则（写死，不要“聪明化”）：

| 情况 | 裁决 | 动作 |
|---|---|---|
| ≥2 份，逐文件 SHA-256 全等 | `Identical` | 保留一份入库存，其余替换为链接 |
| ≥2 份，任一文件内容不同 | `Diverged` | **不动**，只给 `FileDiff` 清单 |
| 只有 1 份 | `SingleSource` | 并入库，原位置替换为链接 |

> 哈希口径：先按 `store::list_files` 的相对路径排序，逐文件算 SHA-256，
> 再对「路径+哈希」列表整体算一次摘要。**必须排除** `SKIP_DIRS`（`store.rs:45` 的同一个常量，
> **它是私有 `const`，要改成 `pub(crate)` 或提供包装函数**，不要另抄一份）。

> **哈希要逐文件流式算，不能 `read` 整个文件**：`store::list_files` 会给到目录下**所有**文件，
> 而 skill 里常有几十 MB 的二进制。`sha2` 已在依赖里（`core/Cargo.toml:36`）。

> **口径漏洞（必须补的一条判定）**：`Identical` 建立在「条目集合 + 内容 + 链接目标字符串」上。
> 两个 skill 里各有一个**同相对路径、同目标字符串的链接**是等价的；但一个**文件**与一个
> **指向同内容的链接**在「逐文件读取内容」的口径下会被判成相同 —— 收敛成链接后语义变了。
> 所以：`FileDiff.state` 之外再加一个 `kind` 字段（`file` | `link`），**`kind` 不同一律算 `Diverged`**。

### 4.2 启用 / 停用（事务编排）

```rust
// keeper.rs
/// 把库内资产启用到某个 agent/scope。返回与 distribute 同形状的 { ok, written[], skipped[], ... }
pub fn enable_asset(&self, input: &Value) -> Value;   // { name, kind, agentId, scope, projectId }
pub fn disable_asset(&self, input: &Value) -> Value;  // 同上
```

`enable_asset` 的 op 序列（**顺序不能变**）：

```
1. Op::mkdir   <agent 侧 skills 父目录>
2. Op::delete  那个真实目录（仅当 agent 侧已存在真实目录且内容一致、且不是链接）
3. Op::link_dir <agent 侧 skills/<slug>>  ← from = 库内路径
```

> **必须先删后建**（目标已存在时 `link_dir` 会直接报 `link target already exists`），
> 而删除与建链接必须在**同一个事务**里，任一步失败两者都回滚。
> `Delete` 的路径要进 `written` 回显；`skipped[]` 沿用 `distribute` 的
> `{ target, reason }` 形状（`keeper.rs:843`–`:856` 的四种 reason 已有先例）。

`disable_asset` 的安全闸（三条，缺一不可）：

1. 目标必须是链接（`linkfs::is_link`）——是真实目录就拒绝，报 `share.notLinked`；
2. 链接指向的必须是**库内**路径（`Library::owns_target`）——指向库外就拒绝，报 `share.foreignLink`；
3. **绝不删除库内主副本**——代码里不允许出现 `rmrf(library_dir)`。

### 4.3 卸载语义与 `.trash`

```rust
/// 从库里删除资产：移入 <store>/.trash/<kind>/<slug>-<ts>/，然后删全部链接。
pub fn remove_from_library(&self, kind: &str, name: &str) -> Value;
```

- 移动用 `fs::rename`（同卷）→ 失败则 `copy_dir` + `rmrf`；
- 每次移动与每条链接删除**各记一行** `audit.jsonl`（用 `settings::audit(root, &rec)`，`settings.rs:259`）；
- `settings::ensure_store`（`settings.rs:87`）里把「启动即删 `backups/`」
  （`remove_legacy_backups`，`settings.rs:111`）扩成「删 `backups/`（遗留）+
  清理 `.trash/` 中超过 `trashDays`（默认 30）的条目」。两者的共同前提是
  **尽力而为、绝不阻塞启动**，新函数照抄这个语气。

### 4.4 M2 验收

在一块假 home 上造 5 个 agent × 同名 skill（内容全等）→ 收敛后：
- 库内 1 份；5 个链接；`audit.jsonl` 新增 6 行；磁盘总字节下降；
- 改库内 `SKILL.md` 正文，5 个 agent 侧同时读到新内容；
- 再造一组内容不同的：收敛入口置灰，`plan_converge` 返回 `Diverged` 且列出差异文件名。

---

## 5. M3 · MCP 单一事实源

### 5.1 适配器新增字段（C8：改完必须重新冻结快照）

`adapters.rs` 的 `Agent` 结构体（`adapters.rs:33`）加：

```rust
/// 「关闭」这个 agent 上的一个 server 时怎么做。
/// 多数 agent 是删条目；Pi 之类靠 enabled:false。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisableStyle { Remove, Flag }
```

`adapter-table.js` 每条 agent 加 `mcpDisable: 'remove' | 'flag'`（默认 `'remove'`），
改完跑 `npm run tauri:adapters`（实测输出 `froze 17 adapters and 187 resolved path templates`）。
**先只给确实需要 `Flag` 的 agent 标 `flag`，其余保持 `remove`** —— 不要凭猜给所有 agent 加字段。

### 5.2 库内规范形态

`<library>/mcp/servers/<slug>.json`：

```jsonc
{
  "schema": 1,
  "name": "fetch",
  "transport": "stdio",          // stdio | http | sse
  "command": "uvx",              // stdio 必填
  "args": ["mcp-server-fetch"],
  "env": {},                     // stdio
  "url": null,                   // http/sse 必填
  "headers": {},                 // http/sse
  "allOf": { "claude-code": true, "codex": true, "pi": false }
}
```

`settings`/`args` 之外**不存**任何 agent 特有封装。写盘时按 `mcp_bucket_for`
（`keeper.rs:521`，实测只有 `copilot` → `"servers"`，其余 `"mcpServers"`）决定桶名，
按 `mcp_target_for`（`keeper.rs:495`）决定落哪个文件（它已过滤 `approvals` / `tool-list` 这类记账文件）。

### 5.3 物化（开/关）

```rust
pub fn materialize_mcp(&self, name: &str, agent_id: &str, scope: &str, project_id: Option<&str>, on: bool) -> Value;
```

- **开**：JSON 目标 → `Op::json_mutate`，在 `bucket` 下插入/覆盖 `configKey` 那一条；
  TOML 目标 → 读出原文 → `toml_splice::splice_server(raw, server, section)` → `Op::write` 整份写回。
  ⚠️ `splice_server` 的签名是 `(&str, &str, &str)`（`toml_splice.rs:156`），**它内部只改目标小节**，
  不要自己序列化整份 TOML。
- **关**：`DisableStyle::Remove` → 从 bucket 删除该 key（JSON）/ `toml_splice::remove_server(raw, server)`（TOML，`toml_splice.rs:181`，**两参**）；
  `DisableStyle::Flag` → 设 `enabled: false`，**不删条目**。
- 关闭时必须**只动这一条**：JSON 里其余 key 与键序保持（`serde_json` 已开 `preserve_order`）；
  TOML 里其余注释逐字节保留。

### 5.4 漂移检测（`doctor.rs`）

```rust
pub struct McpDrift {
    pub name: String,
    pub agent_id: String,
    pub file: String,
    pub field: String,        // "command" | "args" | "env" | ...
    pub library_value: Value,
    pub agent_value: Value,
}
```

`doctor` 只**报告**，不写盘。界面给三个动作（M6）：
「以库为准覆盖」/「以该 agent 为准回写库」/「标记为独立（从 allOf 移除）」。

> **明确不做的**：库外、从未导入过的 server 一律不报告、不触碰。
> 同名不同参**不覆盖**——这是与 cc-switch 刻意相反的决策（覆盖 = 静默丢数据）。

### 5.5 M3 验收

1. Codex（TOML）与 Claude Code（JSON）同时开启同一个 server：
   - TOML 文件里**注释与小节顺序完全不变**（用夹具逐字节比对非目标部分）；
   - JSON 文件里兄弟 server 与键序保留。
2. 关闭后两个文件除该条外与开启前**逐字节相同**。
3. `~/.claude/mcp.json` 里手写一个库里没有的 server → `doctor` 零报告、零写入。
4. 同一 server 在两处参数不同 → `DoctorFinding { code: "mcp.drift" }`，且**没有**任何写入。

---

## 6. M4 · 规则与记忆共享（**已完成，见 §19**）

> 本节是实施前的规格；实际交付、验收落点与实施中修掉的三个缺陷见 **§19**。
> 与本节不同的地方（缺省模式、`.mdc` 的降级、区块不存在时不算外部改动）以 §19 为准。

### 6.1 `compose.rs`：区块标记（纯函数，最好测）

```rust
/// 把 body 作为一个区块放进 target：已有同名区块则替换，否则追加到末尾。
/// 标记之外的内容**逐字节不动**。
pub fn upsert_block(target: &str, slug: &str, body: &str) -> String;

/// 移除指定区块；区块不存在时原样返回。
pub fn remove_block(target: &str, slug: &str) -> String;

/// 解析出所有区块：slug → (起止字节位置, 内容, rev)
pub fn blocks(target: &str) -> Vec<Block>;

/// 判定盘上区块内容是否与「上次生成时」一致。不一致 = 被外部改过。
pub fn is_foreign_edit(target: &str, slug: &str, last_generated: &str) -> bool;
```

标记形态（**固定，不要在实现时改格式**）：

```markdown
<!-- agentkeeper:begin slug=team-conventions rev=9f2c1d -->
…内容…
<!-- agentkeeper:end slug=team-conventions -->
```

- `rev` = 内容 SHA-256 前 6 位（`model::hash16` 截断，`model.rs:15`），用于判断「盘上这份是不是我们上次写的那份」。
- **换行符**：保持目标文件原有风格（全 CRLF 或全 LF），写入时按目标文件的检测结果生成
  （`toml_splice.rs:157` 已有同样的检测：`if raw.contains("\r\n") { "\r\n" } else { "\n" }`，照抄这个判据）。
  这一点单独测：Windows 上用户文件常是 CRLF，混写会让 diff 整文件变动。
- `upsert_block` 必须是**幂等**的：同一 `(target, slug, body)` 跑两遍，第二遍结果逐字节等于第一遍。

### 6.2 规则共享的两种模式

| 模式 | 适用 | 实现 |
|---|---|---|
| `link` | 所有 agent 读同一份 | `Op::link_file`（symlink 优先，回退 hardlink）；agent 侧已是真实文件则先 `Delete` |
| `compose` | 几段规则拼一份 | 读目标现文 → `upsert_block` → `Op::write` |

**`compose` 的写回闸门**：写入前用 `is_foreign_edit` 判定；为真则**不写**，
返回 `{ ok:false, code:"rule.foreignEdit", detail:{ slug, file } }`，由 M6 弹三方 diff。

> ⚠️ hardlink 的已知缺陷：agent 用「写临时文件 + rename」保存时，hardlink 会被打断
> （rename 换掉的是目录项，另一端的 inode 不会更新）。因此**文件型共享优先 symlink**，
> hardlink 只在 symlink 不可用时使用，且 `doctor` 要把 hardlink 的链接单列为
> `link.fragile`（info 级），提示可能被 agent 打断。

### 6.3 本期不做

- `.mdc`（Cursor rules 有 `alwaysApply` / `globs` frontmatter）**只支持 `link`**，
  `compose` 的选项在 UI 上置灰并给出原因。
- 不做 frontmatter 合并。

### 6.4 M4 验收

1. `AGENTS.md` 被 4 个 agent 引用（link 模式），改库内版本 → 4 处同时生效。
2. `compose` 模式下在标记**外**追加一行 → 下次同步保留该行。
3. `compose` 模式下改标记**内**一行 → 返回 `rule.foreignEdit`，盘上文件未被改动。
4. CRLF 目标文件 compose 后仍是纯 CRLF（逐字节断言）。

---

## 7. M5 · 扫描与健康度（**已完成，见 §20**）

> 本节是实施前的规格；实际交付、七类 finding 的判据、三条验收的落点与实施中清零的
> 缺陷见 **§20**。与本节不同的地方（finding 的 `fix` 取值、`sameInode` 的判定口径、
> 以及 `asset.duplicated` 只覆盖 skill 目录）以 §20 为准。

### 7.1 扫描打标

在 `scan_skills`（`scan.rs:257`）建资产处（`make_asset` 的 `meta`，`scan.rs:332` 起）加：

```rust
"link": match linkfs::entry_link_kind(&dir) {
    None => Value::Null,
    Some(m) => json!({
        "method": m.as_str(),
        "src": linkfs::read_target(&dir).map(|p| store::native_str(&p)),
        "managed": /* 目标在库内 && 台账里有这条 */,
    }),
},
```

`link` 为 `null` 表示不是链接（绝大多数资产）。**不许因为这个改动改变现有 id**——
id 由 `model::asset_id`（`model.rs:185`）决定，其中 `dir` 参与折叠成 `hash16(dir)[..6]`，
**链接不参与**。也就是说：把真实目录换成链接后，`dir` 字符串不变 → **id 不变**。
这是本功能「收敛后资产条数不涨」的前提，验收时逐条比对 id。

### 7.2 `compute_effective` 增加 `sameInode`

`compute_effective`（`scan.rs:1383`）现在只判 `shadowedBy` / `overrides`。加一条：
同一 `kind:slug` 组里，若某两份 `canonicalize(dir)` 相同 → 给两者都打 `sameInode: true`，
且**不从 `shadowedBy` 里算它们**（同一份东西不存在“遮蔽”）。

> 现状提醒：`dedupe_by_physical_path`（`scan.rs:1303`）**先**按 canonicalize 折叠，
> 所以同 `kind:slug:physical` 的两条根本到不了 `compute_effective`。`sameInode` 真正
> 有用的是**跨 scope** 的情形（全局链接 + 项目链接指向同一个库内目录）。写测试时要
> 构造「两个不同 scope、同一个 phys」的夹具，否则断言永远为真而什么都没测。

### 7.3 7 类 finding

```rust
// doctor.rs
pub struct DoctorFinding {
    pub code: &'static str,
    pub severity: &'static str,  // "critical" | "warn" | "info"
    pub path: String,
    pub detail: Value,
    pub fixable: bool,
    pub fix: Option<&'static str>,  // "relink" | "adopt" | "remove" | "converge"
}
```

| code | 判定 | 级别 | fix |
|---|---|---|---|
| `link.dangling` | `read_target` 有值但目标 `!exists` | critical | `relink` |
| `link.unmanaged` | 是链接但指向库外 | info | `adopt`（导入库） |
| `link.hijacked` | 台账有记录但该路径已不是链接 | critical | `relink`（内容一致才允许） |
| `link.fragile` | `method == hardlink` | info | — |
| `asset.duplicated` | ≥2 份非链接、内容摘要相同 | info | `converge` |
| `mcp.drift` | 见 §5.4 | warn | 三选一 |
| `link.ledgerDrift` | 台账与磁盘不符 | warn | 以磁盘为准重建台账 |

`doctor` 是**只读**的：任何输入下零写入（M7 里有测试用只读路径断言）。

**新增 code 的翻译清单（M5/P1 分两次进 `CODE_ZH`）**：

```
store.linkFailed, share.notLinked, share.foreignLink, share.unknownAsset,
link.methodUnavailable, converge.diverged, converge.partial, rule.foreignEdit,
mcp.drift, link.ledgerDrift
```

> 为什么不能一次全加：`scripts/tauri-i18n.js` 只**扫源码里的 code 字面量**，多出来
> 的 `CODE_ZH` 条目不算错，但会在 `engine-zh.js` 里堆一段没人用的词条；
> 而**漏加**会直接 CODE-MISS 失败。所以按阶段加，每阶段跑一次 `node scripts/tauri-i18n.js`。
>
> **M5 实际结果**：P1/M2/M3 已经加掉了清单里适用的部分（`store.linkFailed`、
> `share.*`、`rule.foreignEdit`、`mcp.drift`、`link.ledgerDrift`）；七类 finding 的 code
> 不是 `zhCode` 错误码，而是给界面的**数据字段**，所以 `doctor.rs` 里用常量而不是
> 字符串字面量，`CODE-MISS` 保持 0，`engine-zh.js` 本期不动。`link.methodUnavailable`
> 与 `converge.diverged` / `converge.partial` 属于 M6 的写入路径（`apply_converge`
> 等命令），到那时再按这条规则加。

### 7.4 M5 验收

1. 手工把一个 junction 换成真实目录 → `link.hijacked` 出现，`fixable: true`。
2. 删除库内主副本 → 所有指向它的链接报 `link.dangling`。
3. 收敛 30 个 skill 前后，资产条数与「Curated + managed」计数按台账精确对上（且 30 个 id 不变）。

---

## 8. M6 · 界面

### 8.1 视图注册（三处必须同时改）

```js
// src/renderer/app.js:1258 的 VIEWS（现有 8 个视图，插在 discover 之后）
shared: { titleKey: 'nav.shared', subKey: 'shared.subtitle', render: viewShared }
```
```html
<!-- src/renderer/index.html:36 之后 —— Discover 之后、nav-group-operate 之前 -->
<button class="nav-item" data-view="shared"><span class="nav-ico">⇄</span><span class="nav-text" data-i18n="nav.shared">Shared</span></button>
```
```js
// src/renderer/i18n.js —— en 与 zh 各一条
'nav.shared': 'Shared' / '共享'
```

> 只改 `VIEWS` 不改 `index.html`，页签不会出现；只改 `index.html` 不改 `VIEWS`，
> 点击会掉回 `overview`（`app.js:1708` 的 `if (!VIEWS[view]) view = 'overview'`）。
> 点击绑定在 `app.js:1192`（`$$('.nav-item').forEach(b => b.addEventListener('click', () => go(b.dataset.view)))`），
> **不需要改**。但导航文字由 `applyLanguage()` 读取 `data-i18n`，所以新按钮必须带
> `data-i18n="nav.shared"`（`ui-audit.js` 的静态文本检查放行 `.nav-text`，但缺 `data-i18n`
> 会让语言切换后标签停在英文）。

### 8.2 命令与桥接（C5）

新增 8 个命令，`src-tauri/src/lib.rs` 与 `tauri-bridge.js` **成对**添加：

| 命令 | Rust 签名 | 桥接写法 | 返回 |
|---|---|---|---|
| `list_library` | `fn list_library() -> Value` | `call('list_library')` | `{ ok, root, cap, skills[], mcp[], rules[], memory[] }` |
| `plan_converge` | `fn plan_converge() -> Value` | `call('plan_converge')` | `{ ok, groups[], savedBytes }` |
| `apply_converge` | `fn apply_converge(payload: Value) -> Value` | `call('apply_converge', { payload: payload })` | 事务结果 + `written[]` |
| `enable_asset` | `fn enable_asset(payload: Value) -> Value` | `call('enable_asset', { payload: input })` | 事务结果 |
| `disable_asset` | `fn disable_asset(payload: Value) -> Value` | `call('disable_asset', { payload: input })` | 事务结果 |
| `shared_doctor` | `fn shared_doctor() -> Value` | `call('shared_doctor')` | `{ ok, findings[] }` |
| `apply_finding_fix` | `fn apply_finding_fix(payload: Value) -> Value` | `call('apply_finding_fix', { payload: input })` | 事务结果 |
| `library_migrate` | `fn library_migrate(payload: Value) -> Value` | `call('library_migrate', { payload: input })` | `{ ok, moved, total }` |

> 实测过的 `bridge` 关口规则（`scripts/check.js:170`–`:274`）：
> 1. 命令必须在 `generate_handler!` 里；
> 2. `call('x')` 的键必须是 Rust 形参名的 camelCase，**多余键也失败**；
> 3. `Option<T>` 形参算可选；
> 4. **只拦 `call('…', {…})` 这一种第二参写法**：传裸值、或先拼好对象再传变量
>    （`call('x', o)`）都会**绕过形状检查**。所以新命令一律写成内联对象字面量。
> 5. 另有基线习惯：`(args: Value)` 型命令在桥里写 `{ args: … }`（如 `read_asset`），
>    `(payload: Value)` 型写 `{ payload: … }`（如 `save_skill`）。两者都合法，**按参数名对应即可**。
>
> 另外：`list_library` / `plan_converge` / `shared_doctor` 会遍历库目录并做哈希，
> 属于**耗时命令**。既有先例是 `scan` / `read_asset` 用 `async fn` + `tauri::async_runtime::spawn_blocking`
> （`lib.rs:277`、`:363`），这几个新命令照抄，别在 UI 线程里同步跑。

### 8.3 矩阵交互

沿用 `SHARED-ASSETS-PLAN.md` §5.1 的形态。实现要点：

- 行数据来自 `list_library`，**一次请求拿全**，不要每行一次调用；
- 单元格点击 → `confirmWrite`（`app.js:713`，签名 `{ title, action, targets, detail, danger }`）
  → `enableAsset` / `disableAsset` → 成功后**局部重渲染该行**；
- 列头批量 → 只弹一次确认，列全部真实路径（`confirmWrite` 超过 10 条时自动折叠成 `confirm.moreTargets`）；
- 失败单元格 → 展示 `restoreErrors[]`（沿用 `reportWriteFailure`，`app.js:496`；
  **它只认 `store.*` 三个 code**，新 code 要么加进那张 `if` 链，要么在共享页自己判）；
- `cap.dir_order[0] === 'copy'` 时页面顶部常驻横幅 + 显示 `Resync`。

### 8.4 i18n 清单（约 60 条，务必成对）

前缀统一用 `shared.` / `matrix.` / `converge.` / `doctor.`：

```
nav.shared, shared.subtitle, shared.tabSkills, shared.tabMcp, shared.tabRules,
shared.bannerCopyMode, shared.resync, shared.empty, shared.importToLibrary,
matrix.agentColumn, matrix.enableAll, matrix.disableAll, matrix.cellFailed,
matrix.readOnlyCell, matrix.legendOn, matrix.legendOff, matrix.legendFail,
converge.title, converge.step1, converge.step2, converge.step3,
converge.identical, converge.diverged, converge.single, converge.keepAt,
converge.replaceWith, converge.diffFiles, converge.apply, converge.skipped,
doctor.title, doctor.critical, doctor.warn, doctor.info, doctor.fix,
doctor.dangling, doctor.unmanaged, doctor.hijacked, doctor.fragile,
doctor.duplicated, doctor.drift, doctor.ledgerDrift, doctor.zeroFindings,
```

**JS 侧必须成对**：`i18n.js` 的 `en`（`:17`）与 `zh`（`:497`）两个对象，实测各 462 条，
漏一边 `scripts/lint-renderer.js` 的 parity 会红；**多一条没人用的也会红**
（`checkI18nUnused`），所以别提前把用不上的词条写进去。

### 8.5 M6 验收

1. 新视图在 1440×900 与 2180×1466 不溢出（`node scripts/ui-audit.js`，且 `total findings: 0`）。
2. 矩阵键盘可达：Tab 走遍单元格、空格切换、列头有 `aria-label`。
3. `node scripts/check.js` 全绿。
4. 切到英文界面无中文残留，切回中文无英文句子（`node scripts/tauri-i18n.js` + 人工看一屏）。

---

## 9. M7 · CLI / MCP 工具 / 迁移

### 9.1 MCP 工具（+5，总数 20）

加到 `mcp.rs::tools()`（`mcp.rs:80`）与 `call_tool`（`mcp.rs:877`）的 `match name.as_str()`：

| 工具 | 参数 | 说明 |
|---|---|---|
| `list_library` | 无 | 库内资产 + 各自启用的 agent |
| `enable_asset` | `name, kind, agentId, scope?` | 走同一事务 |
| `disable_asset` | 同上 | 走同一事务 |
| `link_doctor` | 无 | findings |
| `converge_plan` | 无 | **只出方案，零写入** |

⚠️ `core/tests/mcp.rs:240` 有一处**钉死工具数量与顺序**的常量：

```rust
const TOOLS: [&str; 15] = [ /* … */ ];
```

加 5 个工具必须同一次把它改成 `[&str; 20]` 并在**末尾**追加名字
（`:238` 的注释写明「改名对已经记住该名字的 agent 是破坏性变更」，
所以**只能追加、不能重排、不能改名**）。另有两处会跟着红：

- `core/tests/mcp.rs:157` 起的 `tools_list_exposes_every_tool_with_a_schema`：每个工具必须带
  `name` / `description` / `inputSchema.type === "object"`；
- `core/tests/mcp.rs:486` 的 `assert_eq!(stats["agents"].len(), 17)`：**这是 agent 数，不是工具数**，不受影响，
  但同文件很容易误改，改前先读那一行的上下文。

### 9.2 `ak-scan --shared`

`core/src/bin/ak-scan.rs` 增加分支：打印 `{ library, ledger, findings }` 的完整 payload。
无参数时行为**完全不变**（CI 依赖现有输出）。

### 9.3 库位置迁移

在「`~/.agents`（共享约定）」与 `<store>`（私有）之间切换：

```
1. 台账里把每条标记 migrating（带 from/to）
2. 逐个：rename 主副本 → 重指全部链接（对每个 dst 做 unlink + link）
3. 全部完成后清掉 migrating 标记
4. 中断后重跑：从台账的 migrating 状态继续，不重复搬
```

**幂等验收**：200 个 skill，在第 100 个上强杀，重跑到底 → 最终状态与一次跑完一致。

### 9.4 首次启用

`bootstrap()`（`lib.rs:212`，**同步** `fn`）返回的 payload 增加
`shared: { enabled }`。收敛候选数（`duplicateGroups`）在打开共享页时由
`plan_converge` 现算，**不要**塞进启动路径（§9.4 下方的说明）。
前端在 `enabled && duplicateGroups > 0` 时提示一次，**默认不收敛**，点进去才进向导。

> `bootstrap`（`lib.rs:212`–`:228`）现在是**同步**命令，返回
> `{ storeRoot, agents, sharedSkillsDir, kinds, version }`，其中 `agents` 是 17 个适配器的
> 解析结果，**它自己不做扫描**（扫描由独立的 `scan` 命令在 `spawn_blocking` 里跑）。
> 所以 `duplicateGroups` 不能指望「已有扫描结果」：要么在打开共享页时才算，
> 要么给 `bootstrap` 的 `shared` 只放 `{ enabled }` 这种不需要遍历库的字段。
> **推荐后者** —— 它把首次启动的耗时增长按在零。

---

## 10. 分阶段执行顺序（每阶段独立可提交、可回滚）

> **规则：一个阶段做完 → 门禁全绿 → 提交 → 才开下一阶段。** 不要跨阶段并行改同一文件。

### P0 · 探测（0.5 天）— 改 3 个文件

1. `core/Cargo.toml` 补 `windows-sys` features（§3.3）；
2. 写 `linkfs.rs` 的能力探测 + 原语（`as_str/parse/is_link/read_target/link_kind/entry_link_kind/unlink/same_target`）；
3. `link-probe` 临时 bin 实测 §3.1 的 5 个问题，结论写进模块注释，**删掉 bin**；
4. `settings.rs::defaults()`（`settings.rs:25`）加三个键。

```powershell
cd tauri-shell; cargo test -p agentkeeper-core linkfs
```

**Gate**：普通会话下 `capability()` 给出的 `dir_order` 至少含一个非 copy 方式；探测在只读目录下不 panic。

### P1 · 事务与台账（2–3 天）

1. `store.rs`：`OpKind` +2（`:745`）、`Op` +2（`:779` 起）、`DirSnap::Link`/`Snapshot::Link` 加 `kind`
   （`:833`、`:948`）、`collect_dir_seen`（`:349`）、`restore_dir`（`:453`）、`restore` 的 Link 臂（`:1287`）、
   `STORE_LINK_FAILED`（`:616`）；
2. `library.rs`：库根、台账读写；
3. `engine-zh.js` 加 `store.linkFailed`；
4. `src-tauri/tests/write_path.rs` 追加 4 个用例（创建/还原/类型保持/台账损坏）。

**Gate**：§3.6 的 M1 四条验收全过。

### P2 · Skills（3–4 天）

1. `sharing.rs::plan_converge`（只读）+ `keeper.rs` 的 `enable_asset`/`disable_asset`/`remove_from_library`；
2. `scan.rs::scan_skills` 打 `meta.link`；
3. `settings.rs::ensure_store` 清理过期 `.trash`。

**Gate**：§4.4 三条验收；`--test sharing` 的收敛夹具测试绿。

### P3 · MCP（3–4 天，可与 P2 并行但**不要同改 keeper.rs**）

1. `adapters.rs` + `adapter-table.js` 加 `mcp_disable` → `npm run tauri:adapters`；
2. `sharing.rs::materialize_mcp`；
3. `doctor.rs` 的 `mcp.drift`。

**Gate**：§5.5 四条验收；TOML 逐字节夹具绿。

### P4 · 规则/记忆（2–3 天）

1. `compose.rs`（纯函数 + 单测先写）；
2. `sharing.rs` 的 rule/memory 编排；
3. `rule.foreignEdit`。

**Gate**：§6.4 四条验收。

### P5 · 健康 + UI（3–4 天）

1. `doctor.rs` 其余 6 类 finding；
2. `scan.rs::compute_effective` 的 `sameInode`；
3. 前端：`view-shared.js` + 7 处登记（§11.1 表）+ i18n + 桥接 + 8 个命令。

**Gate**：M5 + M6 全部验收；`node scripts/check.js` 全绿且 `ui-audit` 为 0。

### P6 · CLI / MCP / 迁移（2 天）

1. 5 个 MCP 工具 + `core/tests/mcp.rs` 断言同步；
2. `ak-scan --shared`；
3. `library_migrate`；
4. 文档：`DESIGN.md` 新章节、`MCP-SERVER.md`、`README*.md` 的 15→20、`changelog/next.json`。

**Gate**：§9 两条验收。

---

## 11. 已知坑（按踩到的概率排序）

### 11.1 新前端文件只登记一半（最隐蔽的一个）
新增 `view-shared.js` 要在**7 处**登记。少一处的后果各不相同，**其中 3 处完全不会报错**：

| # | 文件 | 位置（实测） | 不改的后果 |
|---|---|---|---|
| 1 | `src/renderer/index.html` | `:147`–`:149` 之间，**`engine-zh.js` 之后、`app.js` 之前** | 页面不加载 / 桥接注入报错 |
| 2 | `tauri-shell/copy-frontend.js` | `:27` 的 `PLAIN` 数组 | **打不进 dist，`check.js` 仍全绿**（dist 关口只比白名单内的文件） |
| 3 | `scripts/lint-renderer.js` | `:181`、`:213` 两处 `app.js` 硬编码 | i18n 覆盖率/未注册键检查看不到新文件（**不报错**） |
| 4 | `scripts/lint-renderer.js` | `:354` 的 `targets` 数组、`:377` 的 `endsWith("app.js")`、`:383` 的全局冲突清单 | 未翻译字符串、内联 style、**跨脚本全局冲突**都漏检 |
| 5 | `scripts/ui-audit.js` | `:12` 只读 `app.js` | 裸英文/死控件检查漏掉新文件（**不报错**） |
| 6 | `scripts/ui-audit.js` | `:121` 起的 `decisions` 探针（`S.checked` / `PER_PAGE\|S.page`）仍只匹配 `app.js` | 误报「no multi-select / unbounded rows」 |
| 7 | `scripts/test-affected.js` | `:92` 起的区域表 | `node scripts/test-affected.js` 对改动**一条测试都不跑** |

> 补充两条本轮核对到的事实：
> - `scripts/ui-baseline.js:55` 的 `VIEWS` 只截 `overview/assets/settings` 三页；
>   共享页要做像素基线才需要加，**它不是 `check.js` 的关口**。
> - `scripts/ui-audit.js` **在任何 findings 下都 exit 0**（`check.js` 的 ui 关口因此永远绿）。
>   判据是输出里的 `total findings: 0`，或 `check.js` 打印的
>   `ui: no dead controls / theme violations` 前面没有 `fail`。

**建议做法**：把 3/4/5 的硬编码改成「`fs.readdirSync(renderer).filter(f => f.endsWith('.js'))`」，
一次改完以后再也不会漏。第 2 处必须手加，且加完跑 `node tauri-shell/copy-frontend.js` 生成 dist。

### 11.2 `update::manager` 的两个既有并发测试
不是你的 bug。根因：
- `check()` 在 `manager.rs:1050` 置 `inflight.running = true`，但 `last_attempt_at`
  要到 `manager.rs:1056` 才写；中间窗口里 `status()` 能读到 `checking:true, lastAttemptAt:0`。
- 第二个调用者可能在第一个占槽前就走进 throttle/cooldown 分支，导致发两次请求。

修法（**若任务要求修**）：把 `mutate_state(last_attempt_at = started_at)` 挪到
`inflight` 锁内、`running = true` 的同一临界区。不要求就不要动。

### 11.3 删除顺序写反
「先建链接再删旧目录」必然失败（`link_dir` 的目标已存在会报错）。正确是 `Delete` → `LinkDir`，
且两者在**同一事务**里。

### 11.4 junction 的 reparse buffer 偏移算错
`SubstituteNameOffset` / `PrintNameOffset` 是**相对于 `PathBuffer` 起点的字节数**，不是相对结构体；
长度字段**不含**结尾 NUL。写错的表现是 `DeviceIoControl` 返回 `ERROR_INVALID_PARAMETER`。
**先写一个只建一个 junction 的最小测试**，跑通再往上叠。
另外：常数 `FSCTL_SET_REPARSE_POINT` / `IO_REPARSE_TAG_MOUNT_POINT` 在
`windows-sys` 的 `Win32_System_Ioctl` 模块，不在 `Win32_Storage_FileSystem`（§3.3）。

### 11.5 `file_type()` 认不出 junction
`store::collect_dir_seen`（`:349`）与 `scan::read_entries`（`:105`）都用
`DirEntry::file_type()`。`scan.rs:264`–`:269` 的注释已经记录了这个坑（junction 不报 `is_dir`），
原因是 Rust 0.52 系列只在 `symlink_metadata` 层判 `FILE_ATTRIBUTE_REPARSE_POINT`。
**凡是要判断「这是不是我们建的链接」的地方，一律走 `linkfs::entry_link_kind`。**

### 11.6 忘记重冻结适配器快照
改了 `adapters.rs::AGENTS` 或 `scripts/adapter-table.js` 而不跑 `npm run tauri:adapters`，
`adapters.rs` 内置快照测试会红（现在冻结了 17 个 agent / 187 条路径模板）。
**红了先重算常量，不要删测试。**

### 11.7 dist 陈旧
改了 `src/renderer/*` 后 `check.js` 报 dist 不一致。跑 `node tauri-shell/copy-frontend.js`
（或 `npm run tauri:sync`）。**永远不要手改 `tauri-shell/dist/`。**

### 11.8 `mcp.rs` 工具数量断言
`core/tests/mcp.rs:240` 是 `const TOOLS: [&str; 15]`，**长度写进了类型**，加工具必须同步改长度，
否则编译期就红（在这个具体点上，编译器替你守住了）。不要在别处另写一份工具名清单。

### 11.9 CRLF 混写
compose 写入规则文件时若把 CRLF 文件写成 LF，用户会看到「整个文件都变了」的 diff。
写入前检测目标文件换行风格并沿用（判据见 §6.1）。

### 11.10 `shared` 这个编号已被占用
`scan.rs:1806` 起把 `~/.agents/skills` 报成伪 agent `agentId: "shared"`。
新代码里避免再叫 `shared` 的东西（视图 key 叫 `shared` 没问题，是前端命名空间；
但 Rust 侧结构体/字段别复用这个词，容易与扫描结果混淆）。**更重要的是**：文件是否在
「库内」不能用 `agentId == "shared"` 来判断 —— 库根可被切换到 `<store>`（§9.3），
必须用 `Library::owns_target`。

### 11.11 `settings.save` 只接受 `defaults()` 里已有的键
`settings.rs:162`–`:169`：patch 里**未知键和类型不匹配的键会被静默丢弃**。
新增 `linkMethod` / `libraryRoot` / `trashDays` 必须先加进 `defaults()`（`settings.rs:25`），
否则设置页点了保存、界面回显成功，磁盘上却没有这个键 —— 而且失败是静默的。
`libraryRoot` 用 `String`（空串 = 跟随默认），`trashDays` 用 `Number`。

### 11.12 先写 `{` 手测台账损坏测试的隐患
§3.6 验收 4 要「手工写坏台账」。`common::write` 会 `create_dir_all` 父目录，
所以测试真的会建出 `links.json`；用完记得 `common::cleanup`，否则 `temp_root` 排在
`%TEMP%` 里会一直涨（`common/mod.rs` 的注释已经点名了这件事）。

---

## 12. 与计划文档的冲突（以本文为准）

实施时按右列写，并在 P0 顺手把 `SHARED-ASSETS-PLAN.md` 改成一致（别留两套说法）。

| # | `SHARED-ASSETS-PLAN.md` 的说法 | 实施口径（本文） |
|---|---|---|
| 1 | `read_link_target(p)`：「`fs::read_link` 对 junction 可用，但需用 `symlink_metadata().file_type().is_symlink()` 统一判定」 | **两句都不成立**：`read_link` 对 junction 在部分情况返回 Err，`is_symlink()` 也可能为 false。读出目标必须走 `FSCTL_GET_REPARSE_POINT`，判定类型必须走 `linkfs::entry_link_kind`（§3.3、§3.4⑤） |
| 2 | 「快照侧零改动，`DirSnap::Link` 已经能把链接原样捕获/还原」 | **必须改**：`store.rs:349` 用 `ft.is_symlink()`，认不出 junction；此时它会把链接当目录，**顺着链接把库内整棵树快照进内存**，还原时写成真实副本（§3.4 ⑤） |
| 3 | `create_file_link`：`hardlink → symlink` | **symlink → hardlink**。hardlink 会被 agent 的「写临时文件 + rename」打断（计划自己在 §6.2/§7 也承认），不当首选（§3.4 ①） |
| 4 | M1 验收「`create_dir_link` 返回 `method: "junction"`」 | 返回 `Symlink` **或** `Junction` 都算通过；首选项由 P0 实测定（§3.6） |
| 5 | M6 命令表「`apply_converge { groups }`」「`enable_asset` 返回事务结果」 | 全部改成 `payload: Value` 一个形参，桥接 `call('x', { payload: input })`（§8.2） |
| 6 | 测试放 `core/tests/sharing.rs` / `tests/write_path.rs` | 本仓库的集成测试在 **`tauri-shell/src-tauri/tests/`**，`core` 下只有 `mcp.rs`（§3.7） |
| 7 | M4「`AGENTS.md` ↔ `CLAUDE.md` ↔ `GEMINI.md` 同源映射」 | 适配器表里 `rules` 是**按 agent 各自声明的一串文件名**（`pi` 用 `AGENTS.md`、`workbuddy` 用 `SOUL.md`/`IDENTITY.md`），映射按 `adapter-table.js` 实际声明做，不要硬编码文件名对；`CLAUDE.md` / `GEMINI.md` 需要先确认它们在表里出现的位置，没有再谈「同源」 |

> 另有一处**本轮核对得出、计划里没写**、会直接影响 P5 的：`keeper::skill_dir_for`
> （`keeper.rs:393`–`:407`）只取 `agent.globals.skills.first()`（项目作用域取
> `agent.projects.skills.first()`），**单个值**；而 `adapters.rs` 里 `warp` 的
> `globals.skills` 是 `NONE`（`adapters.rs:377`）。所以矩阵**不是 17 列**：skills 列按
> **实际解析出来的目录**生成，解析不出就不给这一列并写明原因（`warp` 该列为空，不是缺陷）。

---

## 13. 终验清单（发布前逐条打勾）

```powershell
cd tauri-shell
cargo test --workspace                    # 0 failed（基线 333）
cargo clippy --workspace --all-targets    # 无新增 warning（实测输出：每个 target 的 `generated N warnings` 之和 = 64）
cd ..
node scripts/check.js                     # all 8 gates passed
node scripts/tauri-i18n.js                # MISS 0 / CODE-MISS 0 / AMBIGUOUS 0
node scripts/ui-audit.js                  # total findings: 0（注意它永远 exit 0）
node tauri-shell/copy-frontend.js --check
node scripts/changelog.js check
cargo tauri build                         # 安装包能出（可选，CI 跑）
```

> 旧口径里的「clippy 基线 49 个」与实测不符。本机 `cargo clippy --workspace --all-targets`
> 的 `generated N warnings` 逐条是：core lib **23**、core test `mcp` **2**、core lib-test
> **27（23 条与 lib 重复）**、shell `update` **5**、`boundary` **5（1 条重复）**、
> `contract` **2（1 条重复）**。**判据用「改动前后跑同一条命令，逐行 diff 不新增 warning」**，
> 不要拿固定数字对 —— toolchain 一升级这个数字就变。

**功能终验**（在真实机器上，不只用夹具）：

| # | 场景 | 期望 |
|---|---|---|
| 1 | 5 份同名 skill 收敛 | 1 库 + 5 链接，改一处五处生效，**30 个 id 不变** |
| 2 | 内容不一致的一组 | 不收敛、给出差异文件名与 `only-a/only-b/different` 状态 |
| 3 | 停用某 agent | 只删链接，库内主副本仍在，`audit.jsonl` 有记录 |
| 4 | 删库内资产 | 进 `.trash`，链接全删，审计有记录，30 天后被 `ensure_store` 清掉 |
| 5 | Codex + Claude 同开一个 MCP | TOML 注释保留；关闭后逐字节还原 |
| 6 | 手写库外 MCP | `doctor` 零报告、零写入 |
| 7 | compose 规则 | 标记外内容保留；CRLF 不变 |
| 8 | agent 把链接换成真实目录 | 报 `link.hijacked`，可修复 |
| 9 | 库位置迁移中断后重跑 | 最终状态一致 |
| 10 | 恢复普通会话（非管理员） | 全流程可用（**这是本功能存在的理由**） |
| 11 | 跑完 #1 后再手动删库内 `SKILL.md` | 5 个 agent 侧都报 `link.dangling`，无 agent 崩溃 |

---

## 14. 文档同步（P6 一并做，不要留到最后）

| 文档 | 改什么 |
|---|---|
| `docs/DESIGN.md` | 新增「§X 共享层」：linkfs 降级链、Op 扩两型、台账语义、`.trash`、矩阵交互（现有 9 章，插在 §8 路线图之前） |
| `docs/MCP-SERVER.md` | 15 → 20 个工具：`:100`、`:125`、`:136`（标题「工具 API（15 个）」）三处 |
| `docs/SHARED-ASSETS-PLAN.md` | 文首加一行：`> 已实施，见 docs/SHARED-ASSETS-DESIGN.md`；并按 §12 修正 7 处口径 |
| `README.md` / `README.en.md` | 「15 工具」→「20 工具」（各有 2 处：功能列表 + MCP 服务段） |
| `changelog/next.json` | 每阶段一条 `feat`（**M1 已记**：`共享资产 M1：链接内核…`） |
| `docs/RUST-ENV.md` | 若 P0 的探测结论与本文 §3.1 不符，以实测定论并更新 |

---

## 15. 交接说明（给接手的人）

- **先读**：本文 §1（硬约束）→ §3（M1）→ §16（M1 实况）→ 对应模块。
  `SHARED-ASSETS-PLAN.md` 是决策依据，遇到「为什么这么做」去那里找；
  遇到「具体怎么改、行号在哪」只看本文。
- **不要改的**：`store.rs` 的事务语义、`toml_splice.rs` 的按区段手术、`scan.rs` 的 id 生成规则。
  这三处是既有不变量，动了要单独评审。
- **可以改的**：`keeper.rs` 的 `distribute`（把 copy 换成 link 是本次的核心目的之一），
  但要保留旧行为作为 `mode: "copy"` 分支（`keeper.rs:860` 的 `Op::copy_dir`），不要直接删。
- **拿不准时**：宁可让操作失败并给出可读的 code，也不要「尽力而为」地写出半对的状态。
  这条与本项目 `store.rs` 的整体立场一致。

---

## 16. M1 实施记录（2026-11-02，已完成）

### 16.1 交付清单（都已落在 0.2.4 工作树上）

| 文件 | 改动 | 说明 |
|---|---|---|
| `tauri-shell/core/src/linkfs.rs` | **新增 1210 行** | `LinkMethod` / `LinkCap` / `capability` / `probe` / `create_dir_link` / `create_file_link` / `read_target` / `is_link` / `link_kind` / `entry_link_kind` / `unlink` / `same_target` / `replace_with_hardlink` / `restore_hardlink`；14 条单测 |
| `tauri-shell/core/src/library.rs` | **新增 570 行** | `Library::resolve/root/dir_for/skill_dir/mcp_record_path/ledger_path/load_ledger/save_ledger/record/owns_target/owns_skill_dir/capability`、`Ledger`、`LinkEntry`、`Migrating`；9 条单测 |
| `tauri-shell/core/src/store.rs` | +464 / −63 | `OpKind::LinkDir/LinkFile` + `Op::link_dir/link_file` + 事务执行分支 + `STORE_LINK_FAILED`；`DirSnap::Link` 与 `Snapshot::Link` 都带上 `kind`；`collect_dir_seen` / `Store::snapshot` / `restore_dir` / `Store::restore` 全部按种类分派；`rmrf` 与 `Op::delete` 改为对链接「只删目录项」 |
| `tauri-shell/core/src/settings.rs` | +64 | `defaults()` 增 `linkMethod` / `libraryRoot` / `trashDays`；`ensure_store` 增 `remove_expired_trash`（按 `trashDays` 清理 `.trash/`，尽力而为、不阻塞启动） |
| `tauri-shell/core/src/lib.rs` | +3 | `pub mod library;` `pub mod linkfs;`（字母序） |
| `tauri-shell/core/Cargo.toml` | +22 / −1 | `windows-sys` features 补 `Win32_Security` / `Win32_Storage_FileSystem` / `Win32_System_IO` / `Win32_System_Ioctl` |
| `src/renderer/engine-zh.js` | +50 | `CODE_ZH` 增 `store.linkFailed`；`PATTERNS` 增 20 条（链接原语、台账、还原三句） |
| `tauri-shell/src-tauri/tests/write_path.rs` | +193 | **4 条 M1 验收用例**（见 16.3） |
| `tauri-shell/src-tauri/tests/contract.rs` | +5 | `get_settings_is_a_flat_defaults_object` 的键清单加入三个新键 |
| `changelog/next.json` | +1 条 | `feat：共享资产 M1：链接内核…` |

临时产物（`src/bin/link-probe.rs`、`link-probe2.rs`、`Cargo.toml` 里的对应 `[[bin]]`）**已按计划删除**。

### 16.2 P0 实测结论（覆盖 §3.1 的待测清单）

Windows 11 x64、普通用户会话、`symlink_dir` 未被策略禁用：

| 问题 | 结果 |
|---|---|
| `symlink_dir` 普通会话 | **成功**，`is_symlink`=true、`read_link` 可读回、可透过链接读文件 |
| FSCTL junction | **成功**；`attributes = 0x410`（DIRECTORY \| REPARSE_POINT）；`symlink_metadata().is_symlink()` 为 **true**，但 `file_type().is_dir()` 为 **false** |
| junction 上的 `std::fs::remove_dir` | **会跟随进库内删内容**（踩到过一次，库副本被清空）。必须 `RemoveDirectoryW` |
| 目录符号链接上的 `remove_file` | **报 Access denied**（`symlink_metadata().is_dir()` 对目录 symlink 是 false）。`unlink` 必须两种都试 |
| 在已存在目录上建链接 | symlink 与 junction 都 `ERROR_ALREADY_EXISTS (183)`，必须先删后建 |
| 相对目标 | `read_link` 返回 `.\foo`。`create_*_link` 因此只接受绝对路径 |
| 无写权限目录 | 两种方式都 `ERROR_ACCESS_DENIED (5)` |
| `hard_link` 跨卷 | `ERROR_NOT_SAME_DEVICE (17)`，`kind = CrossesDevices` |
| **SYMLINK 重解析点布局** | **Windows 10 起多 4 字节 `Flags`**，`PathBuffer` 起点 = 头 16 字节 + 4。漏掉它会读回 `md\??\C:\…`（偏移对、长度对、起点错 4 字节） |
| MOUNT_POINT 布局 | 无 `Flags`，`PathBuffer` 起点 = 16 字节 |

### 16.3 M1 四条验收（逐条落地在哪、怎么跑）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core --lib linkfs      # 14 passed
cargo test -p agentkeeper-core --lib library     #  9 passed
cargo test -p agentkeeper-shell --test write_path # 23 passed（含下面 4 条）
```

| # | 验收 | 测试 | 结果 |
|---|---|---|---|
| 1 | 非管理员会话下 `create_dir_link` 成功（不是 `Err`） | `linkfs::tests::probe_reports_a_usable_dir_chain`（能力层）+ `write_path::a_link_op_creates_a_link_not_a_copy`（事务层） | ✅ 本机 `dir_order = [Symlink, Junction]` |
| 2 | 事务里「删真实目录 + 建链接」，后续 op 失败 → 目录逐字节回来 | `write_path::a_failed_transaction_restores_the_directory_and_its_links` | ✅ 真实文件逐字节一致 + 目录里那条链接仍是链接且内容可读 |
| 3 | junction 还原后仍是 junction | `write_path::a_junction_is_restored_as_a_junction` + `linkfs::tests::junction_round_trips_through_the_reparse_buffer` | ✅ `link_kind == Junction` |
| 4 | 台账被写坏 → `Err`、不返回空、不动磁盘 | `write_path::a_corrupt_link_ledger_is_an_error_not_an_empty_one` + `library::tests::a_missing_ledger_is_empty_but_a_corrupt_one_is_an_error` | ✅ 追加覆盖「缺 `entries`」「条目缺字段」「未知 method」三种损坏 |

### 16.4 全量门禁（本轮最后一次运行）

```text
cargo test --workspace                 361 passed / 0 failed   （基线 333 → +28）
cargo clippy --workspace --all-targets 无新增 warning（25 / 2 / 29 / 5 / 5 / 2，与基线逐 target 相同量级，新文件 0 条）
node scripts/check.js                  all 8 gates passed
node scripts/ui-audit.js               total findings: 0
node scripts/tauri-i18n.js             MISS 0 / CODE-MISS 0 / AMBIGUOUS-MULTI 0
node scripts/test-affected.js          exit 0
node scripts/comment-lang.js           英文注释 11 行（新文件 0 行），中文覆盖率 99.8%
```

### 16.5 实施中修掉的三个既有缺陷（都在 M1 的写入路径上）

这三处原来不在计划里，但因为链接一旦落盘就会踩到，属于**必须清零**的问题：

1. **`store::rmrf` 会顺着 junction 删掉库内容**。已在入口处加 `linkfs::is_link` 分支，
   链接一律走`unlink`（只删目录项）。这条是数据丢失级的，若做 M2 的「删库内资产」前
   没修，第一次删除就会清空 shared 目录。
2. **`Op::delete` 对悬空链接静默无效**（`Path::exists()` 跟随链接返回 false），
   且 `remove_file` 失败被吞。现在「存在或链接」都进删除分支，失败返回错误。
3. **`construct` 侧硬链接没有目标可读**，快照只记 `kind+空 target` 会让还原直接失败、
   条目消失。现在快照额外记一份内容，还原阶段按内容在同目录里现找对端
   （`linkfs::restore_hardlink`）；找不到就对端就保留普通文件——内容永远不丢。

### 16.6 M2 可以直接接手的接口

```rust
// 1) 库根与能力（已实现，直接用）
let lib = core::library::Library::resolve(&user_home, &settings_value);
let cap = lib.capability(&store_root);                 // 探测落在 <store>/probe，缓存 <store>/linkcap.txt
let mut ledger = lib.load_ledger()?;                   // 损坏 = Err（M1 验收 4）

// 2) 事务里建链接（已实现）
ops.push(Op::delete(&agent_side));                     // 先删（同一事务）
ops.push(Op::link_dir(&agent_side, &lib.skill_dir("pdf")));
store.transaction(&ops, &meta);                        // 日志里带 "method": "symlink" | "junction" | "hardlink"
lib.record(&mut ledger, library::LinkEntry::new("skill", "pdf", &src, &dst, method));
lib.save_ledger(&ledger)?;                             // 只在事务成功之后

// 3) 安全闸（已实现）
if !linkfs::is_link(path) { return share_not_linked(); }
if !lib.owns_target(path) { return share_foreign_link(); }
```

**M2 当时需要自己补的两件事**（都已在 §17 完成）：

- 链接方式偏好原来从环境变量 `AK_LINK_METHOD` 读 —— 现在由 `Op::prefer` 随 op 传入，
  `Keeper::enable_asset` 从设置里的 `linkMethod` 解析后落到 `Op::link_dir_with`（见 §17.3）。
- `.trash/` 的写入（`remove_from_library`）—— 已实现，见 §17.4。

### 16.7 给下一位的三条提醒

1. **`scan.rs:264` 的注释过时了**：它说 junction 不报 `is_dir()`，本机实测
   `symlink_metadata().file_type().is_dir()` 为 false 但 `metadata()` 为 true。
   `scan` 的兜底仍然有效，但新代码一律用 `linkfs::entry_link_kind`。
2. **`store::same_target` 与 `linkfs::same_target` 是同一份实现**（前者直接转调后者）。
   新增的「同一物理对象」判断不要再写第三份。
3. **不要用 `std::fs::remove_dir` 删任何可能是 junction 的路径**。`rmrf` 已经守住了这条，
   但测试脚手架（`common::cleanup`）走的就是 `rmrf`，所以新增夹具也别绕过它。

---

## 17. M2 实施记录（2026-11-02，已完成）

M2 = §4（Skills 共享）+ §10 的 P2。**只做 M2**：MCP / 规则 / UI / CLI 一概没碰。
下面每一步都能用命令复现。

### 17.1 交付清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `tauri-shell/core/src/sharing.rs` | **新增 ~670 行** | `Verdict` / `FileDiff` / `CopyNode` / `ConvergeGroup` / `ConvergePlan` / `TreeDigest`、`digest_dir`（逐文件流式 SHA-256）、`diff_trees`、`plan_converge`、`skill_dirs_of`；9 条单测 |
| `tauri-shell/core/src/keeper.rs` | +≈400 | `library()`、`enable_asset`、`disable_asset`、`remove_from_library`、`record_link`、`library_asset_dir`、`link_prefer` |
| `tauri-shell/core/src/scan.rs` | +≈35 | `scan_skills` 的 `meta.link`（`skill_link_meta`）；**id 不变** |
| `tauri-shell/core/src/library.rs` | +≈45 / −12 | `pub fn is_under`（「这条路径是否在库下」）；**`normalize` 改成纯词法**（见 §17.2 第 1 条）；`owns_target` 改为「看链接指向」 |
| `tauri-shell/core/src/store.rs` | +≈40 / −15 | `Op::prefer` + `Op::link_dir_with` / `Op::link_file_with`；`parse_link_method`（取代 `AK_LINK_METHOD`）；`Op::delete` 对链接走 `linkfs::unlink` |
| `tauri-shell/core/src/lib.rs` | +1 | `pub mod sharing;`（字母序） |
| `src/renderer/engine-zh.js` | +≈20 | `CODE_ZH` 增 `link.ledgerDrift`、`share.notLinked`、`share.foreignLink`、`share.unknownAsset`、`share.unsupportedKind`、`rule.shareUnsupported`；`PATTERNS` 增 4 条 |
| `tauri-shell/src-tauri/tests/sharing.rs` | **新增 ~600 行** | 9 条集成用例，覆盖 §4.4 三条验收 + 三道安全闸 + 事务原子性 |
| `tauri-shell/src-tauri/tests/write_path.rs` | +1 | `Op` 字面量补 `prefer: None`（结构体加了字段） |
| `tauri-shell/src-tauri/tests/common/mod.rs` | 1 行 | `MAIN_SEPARATOR_STR`（新测试目标把这个既有 clippy 警告暴露出来，顺手清零） |
| `changelog/next.json` | +1 条 | `feat：共享资产 M2：Skills 收敛…` |

**没有做的事**（留给后面的阶段，不要以为漏了）：`list_library` / `plan_converge` /
`apply_converge` 等 8 个 Tauri 命令与桥接（M6/P5）、`doctor` 的 7 类 finding（M5/P5）、
MCP 物化（M3）、`compose`（M4）、`ak-scan --shared` 与 5 个 MCP 工具（M7/P6）、
`library_migrate`（P6）、共享页视图（M6）。`enable_asset` / `disable_asset` /
`remove_from_library` **已经可以从外部调用**，M6 只需要把命令与桥接照 §8.2 加上去。

### 17.2 实施中修掉的五个既有缺陷（都在 M2 的写入路径上，属「必须清零」）

1. **`library::normalize` 跟随链接，把五条链接折叠成一条**（最隐蔽的一个）。
   它原来用 `fs::canonicalize` 归一；而 agent 侧每一条链接都指回库内主副本，于是
   五条 `dst` 全部归一成同一个键 —— `Ledger::upsert` 认为它们是同一条，最后一次
   登记覆盖前面所有，台账永远只剩一条。同一个原因还让 `links_to(src)` 把每一条
   链向那个主副本的链接都算成 `src` 命中。
   **修法**：`normalize` 改成纯词法（去掉长路径前缀、折叠 `.`/`..`、抹平重复与末尾
   分隔符、小写，UNC 前缀保留），**绝不 `canonicalize`**。回归测试：
   `library::tests::two_links_to_one_target_stay_two_entries`。
   ⚠️ **不要把它改回去**：链接的身份是它自己的路径，不是它现在指向哪里；
   「是不是同一个物理对象」是 `linkfs::same_target` 的事，两者正好相反。
2. **`Op::delete` 在目录符号链接上报 Access denied**。`Delete` 分支原本对「非目录」
   一律 `remove_file`，而**目录符号链接**上 `RemoveDirectoryW` 能成功、`remove_file`
   要求 `FILE_ATTRIBUTE_DIRECTORY`（§3.3 的实测表里有一条，但当时的 `Delete` 没照办）。
   于是一次停用会让整个事务失败。**修法**：链接一律走 `linkfs::unlink`
   （它对目录 symlink / 文件 symlink / junction 三种都试）。
3. **`enable_asset` 的 op 顺序**：正确顺序是 **搬进库 → 删旧位置 → 建链接**。
   §4.2 写的是「mkdir → delete → linkDir」，那是**主副本已经在库里**的情形；一旦来源
   就是 agent 侧那个目录，先 `delete` 会让随后的 `copyDir` 无源可读
   （`copy source missing`）。两条前置条件撞在一起时，只有「先拷贝」同时满足。
4. **`remove_from_library` 必须先读链接清单再搬主副本**。台账按 `src` 索引，而
   `normalize` 对不存在的路径原样保留 —— 主副本一旦移进 `.trash`，原路径上的
   `links_to` 就一条也找不到，函数会「成功」地把库内目录搬走却留下五条悬空链接。
5. **扫描报出来的 `dir` 混着 `/`**（agent 的 home token 用 `/` 拼，例如
   `…\home/.claude/skills/pdf`）。`Op::copy_dir` 的前置校验用 `Path::is_dir()`，在
   Windows 上对这类路径并不总成立。`enable_asset` 现在统一 `native_str` 之后再进事务。

### 17.3 链接方式偏好：从环境变量改成随 op 传

`store.transaction` 原来从 `AK_LINK_METHOD` 读「用户偏好」。那是进程级全局量：同一次
会话里两个不同的选择会互相覆盖，而且 `core` 不该去读环境变量。现在是 `Op::prefer`：

```rust
ops.push(Op::link_dir_with(&dst, &keeper, prefer));            // prefer 来自设置里的 linkMethod
let prefer = store::parse_link_method(&settings["linkMethod"]); // "auto"（及未知值）→ None
```

`auto` 仍然是 `None` = 按能力顺序自动挑；给了值就只试那一种，建不出来 →
`store.linkFailed`，**不会悄悄换一种**（语义变化是用户看不见的）。

### 17.4 `.trash` 与审计

`remove_from_library(kind, name)`：

1. 先从台账读出指向它的链接清单（见 §17.2 第 4 条）；
2. `fs::rename(主副本 → <store>/.trash/<kind>/<slug>-<YYYYMMDDTHHMMSS>/)`，
   跨卷失败则 `copy_dir + rmrf`；
3. 每条链接触发一次 `linkfs::unlink`，**先过三道闸**：不是链接 → 不动；指向库外 → 不动；
4. 每一步各记一行 `audit.jsonl`（`library.trash` / `library.unlink`）；
5. 最后落台账。返回 `{ ok, trashed, removed[], failed[], ledgerWarning }` ——
   `failed[]` 非空时该条链接**原样留着**，因为一个删不掉的链接比一条假的「已删」好查。

清理侧（`settings::remove_expired_trash` + `trashDays`）M1 就做好了，这里不重复。

### 17.5 收敛口径（`plan_converge`）的实现细节

1. **哈希逐文件流式**（64 KiB 一块），不 `read` 整份；摘要 = 排序后的
   「相对路径 + 种类 + 文件 sha256」逐行再 SHA-256 一次。
2. **排除构建树不另抄一份清单**：直接走 `store::list_files`，它已经跳过 `SKIP_DIRS`、
   按 canonicalize 去环。所以 `store.rs` 里不需要多一个 `pub(crate)` 访问器。
3. **`kind` 不同一律算 `Diverged`**（`FileDiff.state = "kind"`）：一个文件与一条指向
   同内容的链接，按内容比是「相同」，收敛成链接之后语义已经变了。
4. **有文件读不出来**（权限、占用、悬空链接）→ 整组按 `Diverged` 处理、差异清单留空
   ——「读不全」不等于「相同」，这里不能猜。
5. 库内已有的主副本会作为一组的第一份参与比较，所以「库里已有一份、agent 侧还有真实
   副本」也能收敛；已经是「一份真实 + 若干链接」的组**不再出现**在方案里
   （那是 `doctor` 的纳管问题，不是收敛问题）。
6. 同一个 slug 的副本按**物理路径**去重，所以全局链接 + 项目链接指向同一个库内目录时
   不会被算成「两份」。

### 17.6 M2 验收三条（落点与怎么跑）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core --lib sharing          # 9 passed
cargo test -p agentkeeper-core --lib library          # 10 passed（含台账回归）
cargo test -p agentkeeper-shell --test sharing        # 9 passed
```

| # | §4.4 验收 | 测试 | 结果 |
|---|---|---|---|
| 1 | 5 个 agent × 同名同内容 → 库内 1 份 + 5 条链接，`audit.jsonl` 有记录，改库内正文五处同时生效 | `five_identical_skills_converge_into_one_library_copy` | ✅ 五条 `dst` 都 `same_target(keeper)`；台账 5 条；改 `SKILL.md` 后五处都读到 `NEW BODY` |
| 2 | 内容不同的一组 → 置灰，`plan_converge` 返回 `Diverged` 并列出差异文件名 | `diverged_skills_are_reported_with_diff_file_names` | ✅ `actionable() == 0`，`FileDiff{rel:"SKILL.md", state:"different"}`，且**零写入**（库目录都没建） |
| 3 | 停用只删链接 / 删库入 `.trash` | `disabling_removes_only_the_link_and_keeps_the_library_copy`、`removing_from_the_library_moves_it_to_the_trash_and_unlinks_every_agent` | ✅ 主副本逐字节不动；`.trash\skills\pdf-<ts>\` 里有 `SKILL.md`；链接全删；审计含 `library.trash` / `library.unlink` |
| + | 三道安全闸（不是链接 / 指向库外） | `disabling_refuses_paths_that_are_not_managed_links` | ✅ `share.notLinked` / `share.foreignLink`，两处磁盘状态都不动 |
| + | 事务原子性：库内 `skills` 被占成文件 → 启用失败但来源原样 | `a_failed_enable_leaves_the_source_untouched` | ✅ 来源仍是真实目录，文件清单与事务前一致 |

### 17.7 一个必须知道的扫描器行为（收敛之后「资产条数」怎么数）

收敛之后**再扫**，`assets` 里那条 skill 只剩 **1 条**，不是 5 条。
原因是既有的 `scan::dedupe_by_physical_path`（`scan.rs:1347`）按 `canonicalize` 后的
物理路径去重 —— 收敛完成后五个位置就是同一个物理目录，其余四个 agent 记在
`meta.alsoSeen` 里。

这是**既有口径**，不是本次改动引入的，也不是 bug：同一份物理内容在资产表里本来就只
出现一次（去重之前就是这样，只是那时它们真的是 5 份拷贝）。§7.1 说的「收敛后资产条数
不涨」指的正是这一点。**P5 的 `sameInode` 只影响 `effective` 视图（跨 scope 的链接），
不要去动 `dedupe_by_physical_path`** —— 改了会连带改变所有现有资产表的计数。

### 17.8 全量门禁（本轮最后一次运行）

```text
cargo test --workspace                  380 passed / 0 failed   （基线 361 → +19：core +10、sharing +9）
cargo clippy --workspace --all-targets  无新增 warning（逐 target 均 ≤ 基线 25 / 2 / 29 / 5 / 5 / 2）
node scripts/check.js                   all 8 gates passed
node scripts/ui-audit.js                total findings: 0
node scripts/tauri-i18n.js              MISS 0 / CODE-MISS 0 / AMBIGUOUS 0
node scripts/test-affected.js           all selected targets passed
node scripts/comment-lang.js            英文注释 13 行（sharing.rs 0 行），中文覆盖率 99.8%
node scripts/changelog.js check         ok（26 entries）
```

### 17.9 M3 可以直接接手的接口

```rust
// 1) 已经在 M2 落地的三个写入口（M6/P6 只需要加命令与桥接，见 §8.2）
keeper.enable_asset(&json!({ "assetId", "agentId", "scope", "projectId" }));  // → 事务结果
keeper.disable_asset(&json!({ "name", "kind", "agentId", "scope" }));         // → 事务结果
keeper.remove_from_library("skill", "pdf");                                  // → { ok, trashed, … }

// 2) 收敛方案（只读，M7 的 converge_plan 工具直接复用）
let lib = keeper.library();                       // 现算，跟随 libraryRoot 设置
let cap = lib.capability(store_root);             // 探测落 <store>/probe
let plan = sharing::plan_converge(&assets, &lib, &cap);
plan.to_json();                                   // { groups[], savedBytes }
plan.actionable();                                // 可一键收敛的组数

// 3) 链接方式偏好（M3 写文件链接时同样用它）
store::Op::link_file_with(&dst, &src, Some(LinkMethod::Symlink));
```

**给 M3 的三条提醒**：

1. `enable_asset` 目前只认 **skill**：`rule` / `memory` 返回 `rule.shareUnsupported`，
   其它 kind 返回 `share.unsupportedKind`。M3 加 MCP 时要么扩这个分支，要么走
   `materialize_mcp`（§5.3），**不要在 `enable_asset` 里塞第二种语义**。
   > **M4 之后**：`rule` / `memory` 已改为分流到 `enable_rule`（§19.1），`rule.shareUnsupported`
   > 这个 code 与 `engine-zh.js` 里的对应条目一并删除。M3 这条提醒的**结论**仍然成立 ——
   > 每种 kind 走自己的编排入口，不要在一个分支里塞第二种语义。
2. `remove_from_library` 读不出台账时会**拒绝**（返回 `link.ledgerDrift`），因为它无法
   确认哪些链接指向这条主副本。别把它改成「尽力而为」，那会留下悬空链接。
3. `.trash` 的目录名是 `<slug>-<YYYYMMDDTHHMMSS>`，排序即时间序；`trashDays` 的清理
   在 `settings::ensure_store` 里，启动时尽力而为、不阻塞。

---

## 18. M3 实施记录（2026-11-02，已完成）

M3 = §5（MCP 单一事实源）+ §10 的 P3。**只做 M3**：规则/记忆共享（M4）、doctor 其余 6 类
finding（M5）、界面（M6）、CLI 与迁移（M7）一概没碰。下面每一步都能用命令复现。

### 18.1 交付清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `scripts/adapter-table.js` | +≈20 | 首个字段说明块（`mcpDisable` 的判据）；`opencode` 标 `'flag'`，其余 16 个保持缺省 `'remove'` |
| `scripts/gen-adapters-snapshots.js` | +≈14 | 快照 `agents[]` 增 `mcpDisable`（缺省写成 `'remove'`）；Rust 侧断言逐条比对它 |
| `tauri-shell/core/src/adapters.rs` | +≈90 / 机械 +17 | 新增 `enum DisableStyle { Remove, Flag }`（`as_str` / `parse` / `Default = Remove`）与 `mcp_disable(agent_id)`；`Agent` 增字段 `mcp_disable`；17 条字面量逐条补齐；快照常量重新冻结（**仍是 17 个 agent / 187 条路径模板**） |
| `tauri-shell/core/src/sharing.rs` | +≈700（含 21 条单测） | `McpMaterial` / `materialize_entry` / `mcp_config_candidates` / `config_format` / `open_ops` / `close_ops` / `read_entry` / `server_names` / `validate_record`、`McpPlan` / `materialize_mcp`、`McpDrift` / `plan_drift` / `drift_report`；`MCP_SCHEMA` |
| `tauri-shell/core/src/library.rs` | +≈40 | `Library::mcp_records()`（读 `<root>/mcp/servers/*.json`）与模块级 `mcp_records(&Library)` |
| `tauri-shell/core/src/toml_splice.rs` | +≈20 / −8 | `splice_server` 与 `remove_server` 的段落按目标文件换行风格重写（见 §18.2）；新增 `match_nl`；`crlf_is_preserved` 夹具改成断言正确行为 |
| `tauri-shell/src-tauri/tests/mcp_sharing.rs` | **新增 ~430 行** | 8 条集成用例，覆盖 §5.5 四条验收 + `Flag` 风格 + YAML 拒绝 + 记录形状校验 |
| `src/renderer/engine-zh.js` | +≈15 | `CODE_ZH` 增 `mcp.shareUnsupported` / `mcp.noRecordInLibrary` / `mcp.notMaterialisable` / `mcp.drift`；`PATTERNS` 增 `^already open$` / `^already closed$` |
| `tauri-shell/dist/engine-zh.js` | 重新生成 | 由 `node tauri-shell/copy-frontend.js` 生成，**没有手改** |
| `changelog/next.json` | +1 条 | `feat：共享资产 M3：MCP 单一事实源…`（26 → 27 条） |

**没有做的事**（留给后面的阶段，不要以为漏了）：这 8 条 `shared.*` / `list_library` 等 Tauri
命令与桥接（M6/P5）、`doctor` 的 7 类 finding 汇总（M5）——M3 只提供
`sharing::plan_drift` / `drift_report` 这两份只读数据，**没有**接入 `doctor.rs`（它还不存在）；
5 个 MCP 工具（M7/P6）、`ak-scan --shared`（P6）、`library_migrate`（P6）、共享页视图（M6）。

### 18.2 实施中发现并清零的一处既有缺陷：TOML 段落换行混写

`toml_splice::splice_server` 把 `stringify` 产出的**一段**拼回目标文件，而
`toml::to_string` 永远输出 LF。于是往一份 **CRLF** 配置里写一条服务器时，文件会变成
「前半 CRLF、后半 LF」——用户的 diff 显示整段都变了，正是 §11.9 点名的那个坑，只不过
它藏在移植过来的拼接函数里，`craft` 侧目前没有 CRLF 的 TOML 夹具，所以一直没暴露。

修法（不改变既有夹具断言的任何形状）：段落先归一成目标文件的 `eol`，再用 `eol` 切行 ——
拿 LF 切一份 CRLF 文本会在每行末尾留一个 `\r`，拼回去就成了 `\r\r\n`（凭空多一个空行）。
`keeper::upsert_mcp` 走的是同一个函数，所以这条修复顺带把它也修好了。

`crlf_is_preserved` 原来断言的是**缺陷本身**（结果里没有末尾换行、且允许混写），
现在断言正确行为：段落里每一行都是 CRLF、不出现裸 LF。

### 18.3 §5.5 四条验收（逐条落地在哪、怎么跑）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core --lib sharing          # 25 passed（M2 的 9 条 + M3 的 16 条）
cargo test -p agentkeeper-core --lib toml_splice      # 11 passed
cargo test -p agentkeeper-core --lib adapters         #  5 passed（含 mcpDisable 逐条比对）
cargo test -p agentkeeper-shell --test mcp_sharing    #  8 passed
```

| # | §5.5 验收 | 测试 | 结果 |
|---|---|---|---|
| 1 | Codex（TOML）+ Claude Code（JSON）同开一个 server：TOML 注释与小节顺序完全不变；JSON 兄弟 server 与键序保留 | `mcp_sharing::opening_and_closing_keeps_every_other_byte_in_both_formats`(+ `sharing::tests::open_ops_keeps_toml_comments_and_section_order`、`open_ops_inserts_one_server_and_keeps_the_siblings`) | ✅ 新段落之前的部分与原文逐字节相同、段落顺序 `alpha → other → fetch`；JSON 顶层键序 `mcpServers, zeta` 与桶内顺序 `keep, fetch` 保留 |
| 2 | 关闭后两个文件除该条外与开启前逐字节相同 | 同上（同一用例的后半段） | ✅ 两份文件都**整体**等于开启前，`assert_eq!(read(file), before)` |
| 3 | `~/.claude/.mcp.json` 里手写库里没有的 server → 零报告、零写入 | `mcp_sharing::a_server_that_is_not_in_the_library_is_reported_nowhere_and_touched_never`(+ `sharing::tests::drift_ignores_servers_that_are_not_in_the_library`) | ✅ `plan_drift` 空、文件逐字节不变；显式请求物化给 `mcp.noRecordInLibrary` 且不写 |
| 4 | 同一 server 两处参数不同 → `DoctorFinding { code: "mcp.drift" }`，无任何写入 | `mcp_sharing::a_diverged_server_is_reported_once_and_nothing_is_written`(+ `sharing::tests::drift_never_writes`，它对整棵树做 路径+字节数+内容 的前后比对) | ✅ 恰好 1 条 `McpDrift{field:"command"}`；两份配置与库内记录逐字节不变。**注意**：`doctor.rs` 到 M5 才存在，所以本期交付的是 `sharing::drift_report()` 里那条 `mcp.drift` 语义的数据，没有 `DoctorFinding` 结构体 |
| + | `opencode` 走 `enabled: false`，其余 agent 删条目 | `mcp_sharing::opencode_closes_by_flag_and_keeps_the_entry` | ✅ 条目留着、`enabled: false`、其余字段不动；`mcp_disable("claude-code") == Remove` |
| + | YAML 配置拒绝而不是尽力而为 | `mcp_sharing::a_yaml_config_is_refused_instead_of_rewritten` | ✅ `mcp.shareUnsupported`，文件不变；`continue` 的候选列表里根本没有 `.yaml` |
| + | 记录缺传输方式 / 名字不合法 → 写之前就拦住 | `mcp_sharing::a_record_without_a_transport_is_refused_with_a_code` | ✅ `mcp.notMaterialisable`（名字带 `]` 会把 TOML 表头破开） |

### 18.4 规范形态与物化口径（实现细节）

1. **规范化只做三件事**：按 `transport` 挑字段、去掉空的 `args`/`env`、把显式写过的
   `timeout`/`enabled` 原样带上。**不补默认值** —— 记录里没有的字段不会凭空出现在 agent
   的配置里。`allOf` 是记账，不进 agent 的配置。
2. **候选文件是列表不是单值**：`continue` 声明的是 `config.yaml`，「只动一条」在 YAML 上
   做不到（重新序列化会丢注释）。所以 `mcp_config_candidates` 一次给出全部**能只动一条**的
   候选（排除 `approvals` / `tool-list` 这类记账文件与 YAML），调用方取第一份。
3. **桶名与 `keeper::mcp_bucket_for` 同一份规则**：`copilot` → `servers`，其余 → `mcpServers`。
4. **幂等**：同一条记录物化两遍，第二遍返回 `Ok(None)` = 零写入、零撤销快照。这是
   「全部打开一次」可以反复点而不产生垃圾事务的前提。
5. **漂移的比较面**：只比 `transport / command / args / env / url / headers / enabled` 这七个
   字段，且把「缺省的 `transport`」与「显式 stdio」视为等价（`implied_transport`）。
   agent 自己多写的 `timeout` / `cwd` / `type` / `oauth` **不算漂移** —— 覆盖它等于删掉用户
   写的东西。`allOf` 里标 `false` 的 agent 直接跳过。
6. **`DisableStyle` 只给核实过的 agent 标 `flag`**。本期只有一个：`opencode`，依据是它的
   官方 MCP 文档写明「把 `enabled` 置为 `false` 即可停用，不必删条目」。17 个 agent 里
   其余 16 个都是 `remove`；`pi` 的条目里其实已有 `enabled` 字段，但**没有文档**证明它会
   被读，所以按 §5.1「不要凭猜给所有 agent 加字段」保持 `remove`。

### 18.5 全量门禁（本轮最后一次运行）

```text
cargo test --workspace                  404 passed / 0 failed   （基线 380 → +24：core +16、mcp_sharing +8）
cargo clippy --workspace --all-targets  无新增 warning（25 / 2 / 29 / 4 / 5，与基线逐 target 相同）
node scripts/check.js                   all 8 gates passed
node scripts/ui-audit.js                total findings: 0
node scripts/tauri-i18n.js              MISS 0 / CODE-MISS 0 / AMBIGUOUS 0
node scripts/test-affected.js           all selected targets passed
node scripts/comment-lang.js            英文注释 13 行（与基线相同，新文件 0 行），中文覆盖率 99.8%
node scripts/changelog.js check         ok（27 entries）
```

### 18.6 M4 可以直接接手的接口

```rust
// 1) 库内规范形态（只读）
for rec in core::library::mcp_records(&lib) { /* { name, transport, command, … } */ }

// 2) 物化一条（开 / 关）—— 得到 op 列表，由调用方放进事务
let plan = core::sharing::materialize_plan(&lib, &home, None, &json!({
    "name": "fetch", "agentId": "codex", "scope": "global", "on": true
}));   // Ok(Ok(Some(McpPlan { file, server, style, ops })))
store.transaction(&plan.ops, &json!({ "kind": "mcp" }));

// 3) 漂移（只读，零写入）
let drifts = core::sharing::plan_drift(&lib, &home, project_dir, "global");
core::sharing::drift_report(&lib, &home, None, "global");   // { ok, drift[], count }

// 4) 更低一层的原语（做「以库为准覆盖 / 以该 agent 为准回写」时用）
core::sharing::read_entry(&file, "claude-code", "fetch");        // agent 自己的写法
core::sharing::server_names(&file, "claude-code");               // 文件里的服务器名
core::sharing::open_ops(&file, "codex", &record);                // Option<Vec<Op>>
core::sharing::close_ops(&file, "opencode", "fetch", DisableStyle::Flag);
```

**给 M4 的三条提醒**：

1. `materialize_mcp` 的「关」只处理**已经在库里**的服务器；库外的（用户手写的）一律不报告、
   不触碰 —— `case 3` 的验收就是这条。
2. 「以库为准覆盖」= `open_ops`；「以该 agent 为准回写库」= 把 `read_entry` 的结果按规范形态
   归一后写进 `<root>/mcp/servers/<name>.json`（**那是 M6 的动作，本期没有实现**）；
   「标记为独立」= 在 `allOf` 里补一个 `false`。
3. `toml_splice::splice_server` 现在会按目标文件换行风格重写段落（§18.2）。规则共享（M4）
   若也要拼 TOML，走同一个函数，不要另写一份。

---

## 19. M4 实施记录（2026-11-02，已完成）

M4 = §6（规则与记忆共享）+ §10 的 P4。**只做 M4**：`doctor.rs` 与其余 6 类 finding（M5）、
共享页界面与 8 个 Tauri 命令（M6）、5 个 MCP 工具 / `ak-scan --shared` / `library_migrate`
（M7）一概没碰。下面每一步都能用命令复现。

### 19.1 交付清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `tauri-shell/core/src/compose.rs` | **新增 545 行**（含 28 条单测） | `rev_of` / `is_valid_slug` / `eol_of` / `blocks` / `find_block` / `upsert_block` / `remove_block` / `is_foreign_edit` + `Block` 结构。纯函数，不读写文件 |
| `tauri-shell/core/src/lib.rs` | +1 | `pub mod compose;`（按字母序插在 `assets` 之后） |
| `tauri-shell/core/src/sharing.rs` | +≈500（含 15 条单测） | `RuleMode`（`link` / `compose`）、`kind_patterns`、`scope_base`、`RuleTarget`、`RulePlan`、`compose_unsupported`、`rule_plan`、`uncompose_plan` |
| `tauri-shell/core/src/keeper.rs` | +≈300 | `enable_asset` 按 kind 分流；新增 `enable_rule` / `disable_rule`；`disable_asset` 按 kind 分流 |
| `tauri-shell/src-tauri/tests/rule_sharing.rs` | **新增 419 行** | 7 条集成用例：§6.4 四条验收 + `.mdc` 只 link + compose 停用 + assetId 路径 |
| `tauri-shell/src-tauri/tests/sharing.rs` | 改 1 条 | `enabling_a_non_skill_reports_a_code` 的断言已过期（M2 时代断言 `rule` → `rule.shareUnsupported`），改成真的不支持的 kind（`plugin` → `share.unsupportedKind`），改名 `enabling_an_unsupported_kind_reports_a_code` |
| `src/renderer/engine-zh.js` | +6 / −1 | `CODE_ZH` 删 `rule.shareUnsupported`（M4 起规则可共享，这个 code 已无来源），增 `rule.foreignEdit` / `rule.mdcLinkOnly` / `rule.noTarget`；`PATTERNS` 增 `^already shared$` / `^no managed block$`；句子表增 `agent or project could not be resolved` |
| `tauri-shell/dist/engine-zh.js` | 重新生成 | 由 `node tauri-shell/copy-frontend.js` 生成，**没有手改** |
| `changelog/next.json` | +1 条 | `feat：共享资产 M4 …`（27 → 28 条） |

**没有做的事**（留给后面的阶段，不要以为漏了）：`doctor.rs` 与 `link.fragile`（硬链接易被
agent 的「写临时文件 + rename」打断 —— §6.2 的注记仍然有效，但**没有**实现成 finding）、
`compute_effective` 的 `sameInode`（M5）、共享页与 8 个命令 / 桥接（M6）、5 个 MCP 工具
（M7）、`ak-scan --shared`（P6）、`library_migrate`（P6）、frontmatter 合并（§6.3 明确不做）。

### 19.2 实施中发现并清零的三个缺陷（都在 M4 的写入路径上）

**① `copy_dir` 不能搬单个文件 —— 会静默造出一个空目录。**
`store::copy_dir` 的第一步是 `std::fs::create_dir_all(dest)`，第二步是 `read_dir(src)`；
来源是一个**文件**时 `read_dir` 失败，函数**静默返回**。于是「把 agent 侧的规则文件搬进
库」这一步会留下一个空目录，随后 `linkFile` 报 `link source is not a file` —— 整个事务回滚，
但错误信息指向的是链接，不是真正的原因。改用 `Op::write(keeper, content)`。

**② `Op::write` 的 `from` 在**执行期**读来源，而同一个事务里 `linkFile` 会先把它换掉。**
这是本轮最隐蔽的一个，**实测写坏了数据**：`{ write(keeper, from=规则文件), delete(规则文件),
linkFile(规则文件 → keeper) }` 三条 op 按顺序执行，等执行 `write` 时第 3 条还没跑，但
`from` 这时**仍然**是规则文件本身 —— 一旦目标文件已经是链接（补启用、或事务重放），
`write` 读回的就是「链接自己」这个路径字符串，主副本里落的是一行 `C:\...\AGENTS.md`。
事务成功、界面全绿、内容已经没了。

修法：内容在**规划期**读好，用 `Op::write(path, &content)` 的字面形式随 op 传下去。
`rule_sharing::one_agents_md_shared_by_four_agents_takes_effect_everywhere_at_once` 里有一条
断言专门钉住它（「库内主副本必须是原文，不能是指向自己的路径」）。

**③ `is_foreign_edit` 把「区块不存在」也算外部改动 —— 会让首次 compose 永远开不了头。**
`compose::is_foreign_edit` 的三条判据（区块没了 / `rev` 与内容对不上 / `rev` 与上次生成的不符）
对「同步一份**已有**区块」完全正确，但**首次** compose 时区块本来就不存在，一律回 `true`。
所以 `rule_plan` 里把闸门拆成两步：**只有区块存在**时才调 `is_foreign_edit`。
区块整个不见 ≠ 内容被改：用户删掉标记想重新纳入，同步把内容放回去正是他的意思；
真正的数据风险在「区块还在、里面被改过」，那一种必须拒（§6.4 验收 3 就是它）。

### 19.3 §6.4 四条验收（逐条落地在哪、怎么跑）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core --lib compose        # 28 passed（纯函数：标记形态/幂等/换行/闸门）
cargo test -p agentkeeper-core --lib sharing        # 40 passed（M2 的 9 + M3 的 16 + M4 的 15）
cargo test -p agentkeeper-shell --test rule_sharing #  7 passed
```

| # | §6.4 验收 | 测试 | 结果 |
|---|---|---|---|
| 1 | `AGENTS.md` 被 4 个 agent 引用（link 模式），改库内版本 → 4 处同时生效 | `rule_sharing::one_agents_md_shared_by_four_agents_takes_effect_everywhere_at_once` | ✅ 用 `codex` / `opencode` / `amp` / `pi` 四个**真实声明了全局 `AGENTS.md`** 的 agent；四处都是指向 `<root>/rules/agents.md` 的链接；改库内一份后四处逐字节同新内容；再启用一遍零写入且台账不重复 |
| 2 | `compose` 模式下在标记**外**追加一行 → 下次同步保留该行 | `rule_sharing::a_line_appended_outside_the_block_survives_the_next_sync` | ✅ 追加行仍在，文件头前缀逐字节不变，区块内容仍是库内那一份 |
| 3 | `compose` 模式下改标记**内**一行 → 返回 `rule.foreignEdit`，盘上文件未被改动 | `rule_sharing::a_line_edited_inside_the_block_is_refused_and_the_disk_is_untouched` | ✅ `ok:false` + `rule.foreignEdit`；`detail.onDisk` / `detail.want` 就是三方 diff 要的两份；`assert_eq!(read(&file), edited)` |
| 4 | CRLF 目标文件 compose 后仍是纯 CRLF（逐字节断言） | `rule_sharing::a_crlf_rule_file_stays_pure_crlf_after_compose` | ✅ 每个 `\n` 前面都是 `\r`、没有 `\r\r\n`；库内主副本刻意用 LF 写，正是最容易混写的组合 |
| + | `.mdc` 只支持 link，compose 被拒且文件不动 | `rule_sharing::an_mdc_rule_refuses_compose_instead_of_rewriting_the_file` | ✅ `rule.mdcLinkOnly`，文件逐字节不变 |
| + | 停用 compose 只拿走我们的区块 | `rule_sharing::disabling_a_composed_rule_leaves_the_users_own_text` | ✅ 停用后文件回到用户原文 |
| + | 从扫描结果按 `assetId` 共享规则；种类对不上就拒绝 | `rule_sharing::a_scanned_rule_asset_can_be_shared_by_its_id` | ✅ 规则走 link，记忆/不匹配的 kind 回 `share.unsupportedKind` |

### 19.4 实现口径（细节，改之前先读）

1. **主副本按文件名归一**：`<root>/rules/<slug>.md`，`slug` 取自目标文件的文件名
   （`AGENTS.md` → `agents`）。所以不同 agent 的同名规则文件天然收敛成**一份**——这正是
   验收 1 成立的前提，也是 §12 第 7 条「按 `adapter-table.js` 的实际声明做映射，不要
   硬编码文件名对」的落地方式。**注意**：`CLAUDE.md` 与 `AGENTS.md` 目前是**两份**主副本；
   它们要不要合成一份，等 M6 给出「同源映射」的界面语义之后再定，本期不做隐式合并。
2. **模式不由代码猜**：`mode` 给 `link` / `compose`；不给时按「能 compose 就 compose」，`.mdc`
   退到 `link`。**要求**了 `.mdc` + compose 才回 `rule.mdcLinkOnly`（那是给 UI 置灰用的
   理由，不是缺省）。
3. **`compose` 的区块内容来源**：库内有主副本就用主副本，没有就用目标文件自己（首次
   compose = 把现有内容圈成受管区块，**一个字节都不改写**）。`compose` 从不创建主副本。
4. **幂等**：`link` 已指向库内 → 零 op；`compose` 算出来的文本与原文相同 → 零 op
    （零事务 = 零撤销快照），连点两下不产生垃圾事务。
5. **换行判据与 `toml_splice` 同一份**（`raw.contains("\r\n") → CRLF`）。区块内容先归一成
   目标文件的风格再拼接，两个方向都不混写。
6. **标记外逐字节不动**是字符串拼接层面的保证：`upsert_block` / `remove_block` 只替换
   `Block.start..Block.end` 这一段，其余原样 `push_str`。
7. **台账只记 `link`**：`compose` 没有「链接」可记，也就不往 `links.json` 里写。链接方式
   （symlink / hardlink）从盘上 `linkfs::link_kind` 读回，不猜。
8. **停用的三道闸沿用 M2**：不是链接且没有区块 → 零写入；链接指向库外 →
   `share.foreignLink`（删它会动到用户自己的文件）；库内主副本永不删除。

### 19.5 全量门禁（本轮最后一次运行）

```text
cargo test --workspace                  448 passed / 0 failed   （基线 404 → +44：core +37、rule_sharing +7；core 的 lib 单测 247 → 284）
cargo clippy --workspace --all-targets  无新增 warning（compose/sharing/rule_sharing 零条；
                                        keeper.rs 唯一一条在 export_bundle 的既有代码里）
node scripts/check.js                   all 8 gates passed
node scripts/ui-audit.js                total findings: 0
node scripts/tauri-i18n.js              MISS 0 / CODE-MISS 0 / AMBIGUOUS 0
node scripts/test-affected.js           all selected targets passed
node scripts/comment-lang.js            英文注释 14 行（基线 17 → 14，新文件 0 行），中文覆盖率 99.8%
node scripts/changelog.js check         ok（28 entries）
```

### 19.6 M5 / M6 可以直接接手的接口

```rust
// 1) 纯函数：区块读写（M6 的「预览」直接用它算差异，不要另写）
let next = core::compose::upsert_block(&raw, "agents", &body);
let same = core::compose::is_foreign_edit(&raw, "agents", &last_generated);
for b in core::compose::blocks(&raw) { /* slug / rev / body / start..end */ }

// 2) 一次共享的规划（只读，不落盘）—— M6 的矩阵拿它做预览
let plan = core::sharing::rule_plan(&lib, &core::sharing::RuleTarget {
    base: home_of_agent, rel: Some("AGENTS.md".into()), file: None,
}, Some(core::sharing::RuleMode::Compose));
// plan.code: None | "rule.foreignEdit" | "rule.mdcLinkOnly" | "rule.noTarget"
// plan.ops 由调用方送进 store.transaction

// 3) 路径解析（M6 画矩阵时要知道每个 agent / scope 声明了哪些规则路径）
core::sharing::kind_patterns("claude-code", "rule", "global");  // Some(["CLAUDE.md", ...])
core::sharing::scope_base(&home, "claude-code", "global", None); // Some("<home>/.claude")
```

**给 M5/M6 的三条提醒**：

1. `enable_asset` / `disable_asset` 现在按 `kind` 分流：`skill` 走目录收敛，`rule` / `memory`
   走文件级编排（`enable_rule` / `disable_rule`）。加新 kind 时**先**想清楚它属于哪一类，
   不要把规则塞进目录那条路 —— 那会算出一个不存在的路径。
2. `doctor` 要报 `link.fragile`（hardlink 可能被 agent 的「写临时文件 + rename」打断）时，
   判据是 `linkfs::link_kind(p) == Some(Hardlink)`，别用 `nlink`（它对普通文件也会命中）。
3. 规则区块的 slug 来自**文件名**（`AGENTS.md` → `agents`）。M6 若要支持「同一个文件里放多个
   来源的区块」，slug 得带上来源前缀；在那之前一个文件只有一个受管区块。

---

## 20. M5 实施记录（2026-11-02，已完成）

M5 = §7（扫描与健康度）+ §10 的 P5 的 **Rust 侧**。**只做 M5**：共享页界面、8 个 Tauri
命令与桥接（M6）、5 个 MCP 工具 / `ak-scan --shared` / `library_migrate`（M7）一概没碰。
本期 `src/renderer/` 一个字节都没动 —— `doctor` 只提供数据，文案与交互留给 M6。

### 20.1 交付清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `tauri-shell/core/src/doctor.rs` | **新增 787 行**（含 3 条单测） | 七类 finding 的只读检测：`DoctorFinding` / `DoctorReport` / `diagnose` / `check_ledger` / `check_scan_links` / `check_duplicated_skills` / `check_mcp_drift`、`same_content`、`canonical_key`；code / severity / fix 都是常量 |
| `tauri-shell/core/src/scan.rs` | +271 / −9 | `compute_effective` 增加 `sameInode`（跨 scope 指向同一物理对象的两条不算互相遮蔽）；`shrink_effective` 保留该字段（否则它到不了渲染层）；**去重键补类别**（§20.5 的既有缺陷）；4 条单测 |
| `tauri-shell/core/src/lib.rs` | +1 | `pub mod doctor;`（按字母序，`discover` 之后） |
| `tauri-shell/src-tauri/tests/doctor.rs` | **新增 382 行** | 4 条集成用例：§7.4 三条验收 + doctor 零写入 |
| `tauri-shell/src-tauri/tests/contract.rs` | 1 处断言 | `effective` 的键清单加 `sameInode`（M5 起是四个键） |
| `scripts/test-affected.js` | +12 | 新增 `doctor` 区域；`scan` 区域补跑 `--test doctor`；`ALL_TESTS` 补 `--test doctor` |
| `changelog/next.json` | +1 条 | `feat：共享资产 M5 …`（28 → 29 条） |

**没有做的事**（留给 M6/M7，不要以为漏了）：`shared_doctor` / `apply_finding_fix` 等 8 个
Tauri 命令与 `tauri-bridge.js`（M6）、共享页 `view-shared.js` 与 i18n（M6）、
`link.fragile` 的修复动作（只报告、不改盘）、5 个 MCP 工具（M7）、`ak-scan --shared`（P6）、
`library_migrate`（P6）、frontmatter 合并（§6.3 明确不做）。

### 20.2 七类 finding 的判定口径（实现按这张表）

| code | 级别 | 判据 | fix | 数据来源 |
|---|---|---|---|---|
| `link.dangling` | critical | 链接的 `read_target` 有值、但目标 `!exists` | `relink` | 台账 `dst` ∪ 扫描资产 `meta.link` |
| `link.unmanaged` | info | 链接目标存在、但不在库根下 | `adopt` | 扫描资产（skill 目录） |
| `link.hijacked` | critical | 台账有记录，路径存在但**已不是链接**（被换成真实目录/文件） | `relink`（仅 `contentMatches` 时 `fixable`） | 台账 |
| `link.fragile` | info | `linkfs::link_kind(p) == Some(Hardlink)` | —（只提示） | 台账 ∪ 扫描 |
| `asset.duplicated` | info | 同一个 slug 有 ≥2 份**真实目录**且逐文件摘要相同（库内主副本也参与比较） | `converge` | 扫描资产 + `<root>/skills/*` |
| `mcp.drift` | warn | `sharing::plan_drift` 的每一条（M3 已实现的只读口径） | 三选一，`fix: null` | 库内 MCP 记录 + agent 配置 |
| `link.ledgerDrift` | warn | 台账与磁盘的其余不符：台账损坏 / 目标读不出 / 指向别处 / 路径消失 / 库内链接未登记 | `rebuild`（以磁盘为准重建） | 台账 ∪ 扫描 |

几个容易写错的边界，实现里都单独处理了：

1. **「被顶掉」与「台账漂移」的边界**：路径上还是链接 → 归 `link.ledgerDrift`
   （`wrongTarget` / `unreadableTarget`）；路径上变成真实目录/文件 → `link.hijacked`；
   路径上什么都没有 → `link.ledgerDrift`（`missingLink`）。三种修法完全不同，不能合成一条。
2. **`copy` 条目直接跳过**：`mode == "copy"` / `method == Copy` 的受管副本本来就不是链接，
   报成「被顶掉」是误报。
3. **硬链接必须先接住**：硬链接没有可读目标，若先走 `read_target` 会变成
   `unreadableTarget`；所以 `link_kind == Hardlink` 的判定排在 `read_target` 之前
   （单测 `a_hardlinked_rule_is_reported_as_fragile_not_as_hijacked` 钉住）。
4. **去重**：`Collector` 按 `(code, 小写 path)` 去重。台账与扫描两边都看见同一条链接时
   只报一条，`code` 不同则各报各的。
5. **只读契约**：`diagnose` 没有 `&Store` 形参、不探测链接能力（探测会往
   `<store>/probe` 写文件）、不写审计/台账。单测
   `a_corrupt_ledger_is_a_finding_and_the_file_is_left_alone` 与集成
   `doctor_never_writes_anything`（整棵夹具树逐字节比对）钉住这条。
6. **`sameInode` 不进 `shadowedBy`**：同一个 `kind:slug` 组里 `canonicalize` 相同的一对，
   两边都打 `sameInode: true`，互相不算遮蔽/覆盖（§7.2）。`shrink_effective` 已把该字段
   加进发给渲染层的四个键里 —— 漏掉它 `effective` 视图就永远读不到。

### 20.3 §7.4 三条验收（逐条落地在哪、怎么跑）

```powershell
cd tauri-shell
cargo test -p agentkeeper-core --lib doctor      # 3 passed
cargo test -p agentkeeper-core --lib scan        # 4 passed（M5 新增 4 条）
cargo test -p agentkeeper-shell --test doctor    # 4 passed
```

| # | §7.4 验收 | 测试 | 结果 |
|---|---|---|---|
| 1 | 手工把一个 junction 换成真实目录 → `link.hijacked` 出现，`fixable: true` | `replacing_a_managed_link_with_a_real_directory_is_reported_as_hijacked` | ✅ `contentMatches: true`、`fixable: true`、`fix: relink`；检查前后台账逐字节不变 |
| 2 | 删除库内主副本 → 所有指向它的链接报 `link.dangling` | `deleting_the_library_copy_makes_every_link_dangle` | ✅ 3 条链接 → 3 条 critical dangling，`fix: relink`；`link.hijacked` 为 0（悬空链接没有被误判） |
| 3 | 收敛 30 个 skill 前后，资产条数与「Curated + managed」计数按台账精确对上（且 30 个 id 不变） | `thirty_converged_skills_keep_their_ids_and_match_the_ledger` | ✅ 60 份 → 30 条资产（§17.7 的既有口径）；台账 60 条 = 扫描里 60 个受管位置；Curated + managed = 30；30 个 id 与收敛前逐条相同；收敛干净后 doctor 零 finding |
| + | doctor 只读 | `doctor_never_writes_anything` | ✅ 审计文件与整棵夹具树逐字节不变 |
| + | 台账损坏 / 硬链接 / 库外链接 | `doctor::tests` 三条单测 | ✅ 坏台账只报一条 `corruptLedger` 且不动文件；硬链接报 fragile 不报 hijacked；库外链接报 unmanaged 且不动它 |

> 第 3 条里「台账精确对上」的口径：扫描资产是**去重后**的物理条数（同一份内容只出现一次，
> 其余位置进 `meta.alsoSeen`），而台账是**每个 agent 侧一条链接**。所以断言分两半：
> 受管位置（资产本身 + 非 `shared` 的 `alsoSeen`）= 台账条数；总位置 = 30 份库内主副本 +
> 60 条链接。不要用「资产条数 = 台账条数」——那会把库内主副本也算成链接。

### 20.4 全量门禁（本轮最后一次运行）

```text
cargo test --workspace                  459 passed / 0 failed   （基线 448 → +11：core +7、doctor +4）
cargo clippy --workspace --all-targets  无新增 warning（25 / 2 / 29 / 4 / 5，与 §19.5 的基线逐 target 相同；doctor 与新的测试目标 0 条）
node scripts/check.js                   all 8 gates passed
node scripts/ui-audit.js                total findings: 0
node scripts/tauri-i18n.js              MISS 0 / CODE-MISS 0 / AMBIGUOUS 0（finding 的 code 用常量，不进 zhCode）
node scripts/test-affected.js           all selected targets passed
node scripts/comment-lang.js            英文注释 14 行（与基线相同，新文件 0 行），中文覆盖率 99.8%
node scripts/changelog.js check         ok（29 entries）
```

### 20.5 实施中清零的既有缺陷：扫描去重把「同一文件的规则与记忆」折成一条

适配器表里有 **13 个 agent** 把同一个文件同时声明在 `rules` 与 `memory` 两个列表里
（`claude-code` 的 `CLAUDE.md`、`codex` 的 `AGENTS.md`、`cursor` / `continue` / `roo` 的
`rules` 目录等）。`scan_rules` 与 `scan_memory` 于是各产出一条 kind 不同的资产，指向
**同一个物理文件**。

`dedupe_by_physical_path` 的普通分支原来用 `"file:<物理路径>"` 拼键，**键里没有类别**。
后果（用假 home 实测，`.codex/AGENTS.md` 一份文件）：

```text
by {"skill":0,"mcp":0,"plugin":0,"rule":2,"memory":0}
rule   codex   …\.codex\AGENTS.md   alsoSeen= 1
```

两条规则资产、**记忆一条都没有**；被折掉的那一条只留下 `agentId` / `rel` 进了
`alsoSeen`，而 `alsoSeen` 不记类别 —— 界面上记忆面板里那一条直接消失，计数也少一份。
这不是「同一份内容在多个 agent 出现」，而是「同一份内容被当成两个类别读出来」。

修法：普通分支的键改为 `"<kind>:file:<物理路径>"`；`mcp`（带 `configKey`）与 `plugin`
（只可能是 plugin）两个分支本来就有类别信息，不动。**同类别、同物理路径仍然折叠**，
那是去重的本职；改变的只有「不同类别共享同一物理文件」这一种，而那一种本来就该是两条
资产。回归测试：`scan::tests::one_file_declared_as_both_rule_and_memory_stays_two_assets`。

> **不要改回去**：这条与 §17.7 的「不要去动 `dedupe_by_physical_path`」不冲突——
> §17.7 说的是「不要靠改折叠规则来让收敛后的资产条数变多/变少」，这里修的是键里缺类别
> 导致的**错折**。改完之后 `scan` 的资产表会多出本来就应该存在的记忆条目，这是修正，
> 不是口径漂移。

### 20.6 给 M6 的接口

```rust
// 1) 只读健康检查（扫描 + 检测；M6 的 shared_doctor 命令直接包一层）
let lib = keeper.library();                                  // 跟随 libraryRoot 设置
let projects = core::scan::read_projects(keeper.store.root_str());
let (payload, _timings) = core::scan::scan_payload(&keeper.user_home, &projects);
let assets = payload["assets"].as_array().cloned().unwrap_or_default();
let report = core::doctor::diagnose(&lib, &assets, &keeper.user_home, None); // None = 只看全局
report.to_json();          // { ok, findings[], counts{ total, critical, warn, info } }
report.by_code(core::doctor::LINK_DANGLING);
report.has_critical();

// 2) finding 的形状（序列化后的键名与字段同名）
// { code, severity, path, detail, fixable, fix? }
// fix = "relink" | "adopt" | "converge" | "rebuild"，缺席表示没有单一修法
```

**给 M6 的三条提醒**：

1. `shared_doctor` 会遍历库目录并做内容哈希，属于**耗时命令**：照 §8.2 的先例用
   `async fn` + `spawn_blocking`（`scan` / `read_asset` 就是这么做），别在 UI 线程里同步跑。
2. `doctor::diagnose` **不探测链接能力**（探测会写 `<store>/probe`）。M6 若要把
   `asset.duplicated` 的一键收敛按钮按「本机能不能建链接」置灰，自己调
   `lib.capability(store_root)` —— 那是一次真实的写盘，属于动作路径，不该混进只读检查。
3. `findings[].path` 一律是 `store::native_str` 的绝对路径；文案按 code 查 `i18n.js` 的
   `doctor.*`（§8.4），**不要**在 Rust 侧拼英文句子，否则 `tauri-i18n.js` 直接 MISS。
   `apply_finding_fix` 按 `code` 分派；`link.hijacked` 必须先看 `fixable` /
   `detail.contentMatches` —— 内容不一致时只能人工处理，不能自动 relink。

