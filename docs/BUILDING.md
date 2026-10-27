# 构建与测试执行说明书

本项目有 20 个 npm 脚本、2 个 Rust crate、5 个集成测试套件。**跑哪个、什么时候跑**，本文说了算。

核心原则：**改一处，只跑受影响的测试**。全量 `cargo test --workspace` 要 ~7 s / 333 个测试；
在迭代中它既慢又稀释注意力——改一个校验器却重跑更新子系统的 39 个集成测试，会让人误以为那里出了问题。

**另一条原则：构建不吃旧产物。** `tauri-shell/dist/` 不入库（它是 `src/renderer` 的生成物），
`tauri.conf.json` 把 `copy-frontend.js` 配成 `beforeBuildCommand` / `beforeDevCommand`，
所以每次 `cargo tauri build` / `cargo tauri dev` 都会重新生成它。同理，
`npm run tauri:bundle` 依次跑 `tauri:sync` → `tauri:core-bins` → `cargo tauri build` → 归档，
每一步要么重新生成、要么重新编译，不会复用上一次留下的文件。

---

## 1. 选脚本：先看你改了什么

| 你改了什么 | 跑什么 | 耗时 |
|---|---|---|
| `tauri-shell/core/src/validate.rs`、`security.rs`、`yaml.rs` | `npm test` | ~3 s |
| `tauri-shell/core/src/store.rs` | `npm test` | ~5 s |
| `tauri-shell/core/src/update/**`、`http.rs` | `npm test` | ~4 s |
| `tauri-shell/core/src/keeper.rs`、`assets.rs` | `npm test` | ~4 s |
| `tauri-shell/core/src/scan.rs`、`discover.rs`、`adapters.rs` | `npm test` | ~4 s |
| `tauri-shell/src-tauri/src/**`（shell 的 command 层） | `npm test` | ~6 s |
| `src/renderer/**`（界面） | `npm run check` | ~0.5 s |
| `Cargo.toml`、`Cargo.lock`、`build.rs`、`tauri.conf.json` | `npm run test:all` | ~7 s |
| 文档（`docs/**`、`README*`） | 不用跑 Rust 测试 | 0 |
| `scripts/**`（构建工具本身） | `npm run check` | ~0.5 s |

`npm test` 自己判断：它读 `git status`（或 `--changed <ref>` 的 diff），把改动映射到受影响的测试目标。
不确定时先看它要跑什么：

```bash
npm test -- --list     # 打印计划，不执行
```

**`npm test` 没有匹配到任何区域时什么都不跑**——这是有意的：改文档不该触发编译。

---

## 2. 脚本清单

### 测试

| 脚本 | 作用 | 何时用 |
|---|---|---|
| `npm test` | 只跑改动影响的测试 | **默认**。迭代中唯一该用的 |
| `npm run test:all` | 全部 333 个测试 | 提交前、CI、改动跨越多个区域时 |
| `npm run check` | 8 道静态闸门 | 改了前端、脚本、文档时 |

### 前端

| 脚本 | 作用 | 何时用 |
|---|---|---|
| `npm run tauri:sync` | `src/renderer` → `tauri-shell/dist` | 改了 `src/renderer/**` 之后。**改前端必跑**，否则 exe 里编的是旧界面 |
| `npm run tauri:check` | 只校验 dist 与 src 一致 | 想知道有没有漏 sync 时 |

### 打包

| 脚本 | 作用 | 何时用 |
|---|---|---|
| `npm run tauri:bundle` | 完整出包：安装版 + 免安装 zip → `release/` | 要出包时 |
| `npm run tauri:portable` | 只出免安装 zip | 只想要免安装版时 |
| `npm run tauri:core-bins` | 编译并暂存 core 的两个 bin | `tauri:bundle` 会自动做；单独跑是为了排查 sidecar 问题 |

### 维护

| 脚本 | 作用 | 何时用 |
|---|---|---|
| `npm run tauri:clean` | 清**孤儿 build 目录**（默认）；`--pdbs` / `--all` 才动 pdb 与增量缓存 | `tree` 闸门报出孤儿目录时 |
| `npm run tauri:adapters` | 从 `scripts/adapter-table.js` 重新生成 adapter 快照 | 改了 agent 列表后 |
| `npm run tauri:i18n` | Rust 引擎文案的中文覆盖检查 | 改了 Rust 里的英文句子后 |

---

## 3. Rust 侧的两个 profile

两个 profile 各司其职，**不要用同一个 profile 做两件事**。

| | `[profile.release]` | `[profile.dev]` |
|---|---|---|
| 用途 | **出包**（`npm run tauri:bundle`） | **迭代**（`cargo tauri dev`、`cargo test`） |
| LTO | 关（`lto = false`） | 关 |
| codegen-units | 16 | 16（dev 默认） |
| 改一行 shell 的重编 | 77 s | ~8 s |
| 产物大小 | shell 12.5 MiB | 不关心 |

**为什么 release 也关了 LTO**：fat LTO 会对 workspace 里**每一个**二进制重做一次全量跨 crate 优化。
实测改一行 shell 代码后全量重编：

| 配置 | 耗时 |
|---|---|
| `lto = true, codegen-units = 1` | 212 s |
| `lto = false, codegen-units = 16` | **68–77 s** |

代价是产物大 20%（shell 10.4 → 12.5 MiB，安装包 3.63 → ~4.0 MiB）。这个交换划算，因为 release
profile 现在只用来**出包**，而迭代走 dev profile。

### 冷启动出包的成本构成

`target/` 为空时，出包要编译整个依赖图：

| 阶段 | 耗时 |
|---|---|
| core 的依赖图（118 crate） | 37 s |
| 补上 tauri 依赖（336 crate） | 157 s |
| 打包（makensis + zip） | ~15 s |

这是一次性成本。`target/` 热起来之后 `cargo tauri dev` 约 8 s，日常迭代不受影响。

### 增量编译

`scripts/package-release.js` 给包路径设了 `CARGO_INCREMENTAL=1`，用编译缓存换时间：

| 场景 | 耗时 |
|---|---|
| 全量出包（首次 / 清缓存后） | ~100 s |
| 热出包（只改 shell 源码后 `npm run tauri:bundle`） | **~18 s**（含打包） |

需要注意的边界：

* release 树会常驻一份 **incremental 缓存（约 0.8 GB）**。它是产物不是残留，`npm run tauri:clean`
  默认**不删它** —— 删了热出包就退回 77 s。
* `npm run check` 的 `tree` 闸门守的是**总体积上限（6.5 GiB）**，不是「某类文件必须消失」。
  pdb、rlib、rmeta、incremental 都算产物，全部保留并计入体积。
* 该开关只在 `package-release.js` 的 env 里生效，手动 `cargo` 命令与 dev 构建不受影响
  （env 层优先级高于 `.cargo/config.toml` 的 `[build] incremental = false`）。

### 编译时间主要花在哪

改一行 shell 代码的热重编约 77 s，其中绝大部分在 codegen。主要来源是 `#[tauri::command]`
宏：每个命令都会实例化一遍 tauri 的泛型运行时（`CommandArg` / `Invoke` / `Resolver`）。

| 对照实验 | rlib 大小 | 增量编译 |
|---|---|---|
| 只用 tauri 类型，不用任何宏 | 0.01 MB | 0 s |
| 36 个普通函数 + serde 派生 | 0.03 MB | 1 s |
| 36 个 `#[tauri::command]` | 82 MB | 77 s |

**当前状态**：36 个命令全部注册在 `lib.rs` 的 `generate_handler!` 里，一次改动会重编整个
shell crate。要压缩这个成本，可行方向是把同域命令合并成带 `op` 参数的 dispatch —— 代价是
`tauri-shell/tauri-bridge.js` 的调用形状要跟着改，属于破坏性变更，必须同步改前端并跑全量测试。

> 迁移 command 到别的 crate 不会降低这个成本：宏在哪个 crate 展开都要实例化一遍泛型运行时。

---

## 4. 执行策略

| 阶段 | 跑什么 | 为什么 |
|---|---|---|
| 改 `core/src/validate.rs` | `cargo test -p agentkeeper-core --lib` | 校验器只被 core 单测和 write_path 覆盖 |
| 改 `core/src/adapters.rs`（含改名测试） | `cargo test -p agentkeeper-core --lib` | 改名只影响 core |
| 删/并集成测试 | `cargo test -p agentkeeper-shell --test <suite>` | 只动那一个套件 |
| 改源码注释 | `cargo test --workspace` | 注释不该影响行为，但要确认没改到代码 |
| 改 `[profile.release]` | `npm run tauri:bundle` + 全量测试 | profile 影响所有产物，必须全量验 |
| 加/改测试脚本本身 | `npm test -- --list` + 一次真跑 | 工具本身要验证映射正确 |
| 单文件小改 | 该文件对应的单一验证命令 | 范围明确时不必扩大 |

**规则**：改一个文件 → 跑该文件对应的测试目标 → 全部改完 → 跑一次 `npm run test:all` + `npm run check`。
中间不跑全量。

### 为什么不能每次都跑全量

`npm run test:all` 是 333 个测试、~7 s。全量跑当然不慢，但**它稀释注意力**：改一个校验器却重跑
更新子系统的 39 个集成测试，出问题时你会误以为那里坏了。窄路径下改一个校验器只跑 3 个目标。

更重要的是**出包不依赖全量测试**。出包只需要「代码能编译」——`cargo tauri build` 自己就会编译。
测试是给人看信心的，不是构建的前提。所以：

- 迭代：改 → 窄测试 → 继续
- 提交前：全量测试 + 闸门
- 出包：随时可以出，不需要先跑全量测试

### 已知会踩的坑

| 现象 | 原因 | 处置 |
|---|---|---|
| `tree` 闸门报 `target/` 超过 6.5 GiB | 孤儿 build 目录或异常增量缓存堆积 | 按闸门打印的顺序回收：先 `npm run tauri:clean`（孤儿目录），再考虑 `--all` |
| `dist` 闸门报 `dist/index.html` 过期 | 改了 `src/renderer/` 但没 sync | `npm run tauri:sync`；`npm test` 在检测到 renderer 变更时会自动先 sync |
| `npm test` 跑了全量而不是窄路径 | 改动了 `Cargo.toml` / `build.rs` / `tauri.conf.json` | 这是有意的：这些文件影响所有产物 |

---

## 5. 出包前的检查清单

```bash
npm run test:all      # 333 个测试
npm run check         # 8 道静态闸门
npm run tauri:bundle  # 出包（会先 sync + stage sidecar）
```

`npm run tauri:bundle` 之后 `npm run check` 若报 `tree` 闸门失败，只有一种原因：`target/` 超过了 6.5 GiB
上限。按闸门打印的顺序回收（孤儿目录优先），不要直接删 rlib/rmeta——那会让下次构建全量重编。

---

## 6. 产物放在哪

```
release/
  AgentKeeper_<ver>_x64-setup.exe        安装版（NSIS，currentUser，无需管理员）
  AgentKeeper_<ver>_x64-portable.zip     免安装版（解压即用）
```

`release/` 已被 `.gitignore` 忽略，不入库。免安装版内含 `agentkeeper-shell.exe`、
`agentkeeper-mcp.exe`、`ak-scan.exe` 和一份 `README-PORTABLE.txt`；两种形态共用
`%APPDATA%\AgentKeeper` 存设置，要隔离就设 `AK_STORE_ROOT`。
