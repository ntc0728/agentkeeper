# 软件更新（GitHub Releases）

AgentKeeper 的更新链路基于 GitHub Releases：检查 → 通道选择 → 流式下载 → 交给系统安装器。

代码在 `tauri-shell/core/src/update/{version,http,token,manager}.rs`，不依赖 Tauri，可单测，
也能被 `ak-scan` 等无界面入口复用。行为以代码与测试为准（`cd tauri-shell && cargo test`），
本文解释行为与取舍。

---

## 1. 检查：两个通道

| 通道 | 请求 | 取版规则 |
|---|---|---|
| 正式（默认） | `GET /repos/{repo}/releases/latest` | GitHub 自己排除草稿与预发布 |
| beta（开关打开） | `GET /repos/{repo}/releases?per_page=30` | 按**版本号**取最高，不用列表顺序（热修会让时间序与版本序不一致） |

beta 通道的关键分叉：**当前跑的是不是预发布版**。

- 当前是正式版 → 预发布一并参与比较，取最高。仓库没有 beta 时照旧收正式版更新（打开开关不该让人收不到更新）。
- 当前是 beta 版（`0.3.0-beta.1`）→ 只看预发布。同核心号的转正版（`0.3.0`）**不算更新**，因为「加入 beta」的语义是留在 beta 上；想回正式版由用户关开关。

不降级：只有 `compare(最新, 当前) > 0` 才叫有新版本。关掉 beta 后停在
`0.3.0-beta.1` 的用户，会等正式版号真正超上来（`0.4.0`）才收到。

版本比较按 semver 第 11 条比预发布段：`beta.2 < beta.10`（数字标识符按数值比）、
`1.0.0-beta < 1.0.0`。`1.0.0.1`、`1.0.0_beta` 这类非 semver 写法按「没有预发布段」处理。

## 2. 额度与冷却

- 匿名 60 次/小时，按**出口 IP** 计；填 GitHub 令牌后 5000 次/小时，按用户计。
- 检查状态（`update-state.json`）持久化：`lastAttemptAt` / `lastSuccessAt` / `nextCheckAt` /
  `retryAt` / `lastError` / `release` / `skippedVersion`。重启只补跑到期的那一轮，不会每次启动都打一次 GitHub。
- 手动检查也受两道闸：60 秒最短间隔（连点复用上次结果）、失败冷却（403/429 的
  `Retry-After` / `X-RateLimit-Reset`，重启不会提前解除）。
- **换线路 / 换令牌会清冷却**：冷却记的是某个配额桶的恢复时刻，换出口或换身份就换了桶。
  换通道（beta 开关）不清——桶没变。
- 403/429 的提示里带「可在更新设置里填写 GitHub 令牌」，这是不必干等的那条路。

## 3. 出网

| 设置值 | 候选顺序 |
|---|---|
| `auto`（默认） | 直连 → `127.0.0.1:7897`（Clash 混合端口） |
| `direct` | 只有直连 |
| 指定 URL | 只有它（**不暗中回落**，否则用户以为走了所选线路） |

只有**传输层**失败（DNS / 连接 / TLS / 超时）才换下一个候选；HTTP 状态码是答复，不是线路问题
（404 换代理重试只是白烧一次额度）。

实现是 `http`/`https` + CONNECT 隧道，不用 `fetch`：需要在无依赖的前提下支持显式代理、
流式下载、即时取消。

## 4. 下载

- 只接受 https（配置了自建源 `updateApiBase` 时，该源的 host 按配置的 scheme 放行）。
- 落盘 `{storeRoot}/updates/{safeFileName}`；文件名去路径、折叠 `\/:*?"<>|`。

**两个目录要分清** —— 安装包下载落点和数据目录是**两回事，互不影响**：

| | 位置 | 说明 |
|---|---|---|
| 数据目录（store 根） | `%APPDATA%\AgentKeeper\agentkeeper` | settings / projects / 审计日志都在这里；可用环境变量 `AK_STORE_ROOT` 覆盖 |
| 程序安装落点 | `%LOCALAPPDATA%\AgentKeeper` | 由 NSIS（`installMode = "currentUser"`）决定，不需要管理员权限。应用不读也不知道它的位置，只负责把安装包下载到 `updates/` 再交给系统安装器 |

> 静默安装（`/S`）的默认目标是 `%LOCALAPPDATA%\AgentKeeper`。这个落点由 NSIS 的
> `installMode` 决定：改成 per-machine 就会变，所以不要把它当成常量硬编码。

store 根的同一约定在 Rust 侧也有一份实现（`tauri-shell/src-tauri/src/lib.rs` 的
`store_root()`），与这里保持一致。
- 体积上限 600 MB，`content-length` 超限直接中止；写完后核对实际大小，避免「进度 100% 但文件不完整」。
- 失败或取消**删除半截文件**——留着会被误当成可运行的安装包；成功路径绝不删。
- 进度与取消都在 Rust 侧（`core/update/manager.rs`，不随页面切换中断），渲染层 900ms 轮询。

## 5. 安装：三道闸

`install({path})` 只接受同时满足以下条件的文件：

1. 绝对路径；
2. 扩展名属于本平台（Windows `.exe`）；
3. `realpath` 落在 `{storeRoot}/updates` 内；
4. 文件名包含 `updateAssetKeyword`（默认 `AgentKeeper`）。

第 4 条是本项目加的、上游没有的闸门：`updateRepo` 配错时，第四道会拦住
「把别人的安装包装到自己机器上」。真实例子里 `Agent2API_3.1.0_x64-setup.exe`
会被这条拒绝（文件名不含 AgentKeeper），而不会被执行。

启动方式是 Rust 侧 `Command::new(file).spawn()`；装机器上 NSIS 是 per-user，不需要提权。
启动成功后延迟 400ms 退出，让出被占用的文件。

下载完成**不自动安装**：会弹「安装包已就绪 → 安装并退出」，因为退出应用是用户的决定。
上游桌面端是下完即装，这里的差别是有意的。

## 6. 令牌

- 用 **Windows DPAPI** 加密，存 `enc1:<base64>`，即 `CryptProtectData(token_utf8)` 的
  base64。信封与 Windows `OSCrypt` / DPAPI 的既有格式字节兼容（user scope、无 entropy），
  机器上已存在的加密令牌可以直接读取。
- **DPAPI 绑的是 Windows 用户，不是应用**。换台机器、或换一个 Windows 用户登录，密文就
  解不开了——这是设计意图（密文离开本机用户账户即不可用），不是缺陷。
- UI 永远不回显：`status()` 只报 `filled` / `origin`（`stored` / `env`）。
- 解不开时**不当作未配置**——配置里明明有密文，静默按匿名跑会让人以为令牌还在生效。
  界面显示具体原因并提示重新保存（新密钥 + 新密文，自愈）。
- 安全存储不可用时**拒绝保存**，不回落到明文。
- 优先级：界面保存的 > 环境变量 `GITHUB_TOKEN`。

## 7. 调度

`UpdateManager::tick()` 是无状态心跳：每次被调用时重新读持久化的截止时间，到点才打一次
GitHub，而不是注册一个 `setInterval(minutes * 60000)`。间隔最长 24 小时——长定时器在
睡眠/唤醒后会漂移，醒来只补一次比永远不补合理。

壳侧由一个 30 秒 ticker 驱动 `tick()`（`lib.rs` 的 `spawn_update_ticker`），到点后把结果
`emit("update-event", payload)` 推给渲染层。`window.ak.onUpdateEvent` 据此：

- 在任意页面弹出「检测到更新」（设置页除外——面板就在眼前，不叠窗）；
- 同一版本本会话只弹一次（「稍后」不记忆）；「跳过此次版本」按版本号持久化，下个版本照常弹。

## 8. 配置

| settings 键 | 默认 | 说明 |
|---|---|---|
| `updateRepo` | `''` | `owner/repo`；**留空 = 用编译进包的 `BUILT_IN_REPO`**（开箱即查，不猜别人的仓库；想指到自己的 fork 才填）。代码在 `core/update/manager.rs:794-798`，用例 `manager.rs:2097` |
| `updateApiBase` | `''` | 空 = `api.github.com`；自建源需同形的 `/repos/{repo}/releases` |
| `updateCheck` | `false` | 自动检查开关 |
| `updateCheckMinutes` | `20` | 1 ~ 1440 |
| `updateBeta` | `false` | 加入 beta 通道 |
| `updateProxy` | `auto` | 见 §3 |
| `updateToken` | `''` | `enc1:` 信封，绝不存明文 |
| `updateAssetKeyword` | `AgentKeeper` | 安装包名必须包含它 |

## 9. 测试怎么覆盖的

更新链路的回归分两层，都在 Rust 里跑（`cd tauri-shell/src-tauri && cargo test`）：

**纯函数层** —— `tauri-shell/core/src/update/version.rs` 的单元测试，
覆盖版本比较：beta 排序（`beta.2 < beta.10`）、草稿排除、非 semver 写法按「无预发布段」处理。
挑选规则的边界也钉在这里（`tests/update.rs::update_installer_selection_and_file_name_rules`）。

**全链路层** —— `tauri-shell/src-tauri/tests/update.rs`。它起的不是打桩，
而是**真实的 `TcpListener`**：一个货真价实的 HTTP 服务，`/releases/latest`、`/releases`、`/dl/*`
都按真字节流写回去，包括 `Transfer-Encoding: chunked`（无 `Content-Length`）的响应。
所以下面这些覆盖点走的是真 socket、真解析：

版本比较（含 beta 排序、草稿、非 semver）→ 资产挑选（NSIS setup 优先）→ 内置仓库零配置 →
发现更新 → 60 秒节流 → 下载到 100% 且字节数一致 → 安装路径三道闸 → 跳过/取消跳过 →
令牌加解密与「解密失败不等於未配置」→ beta 通道两种身份 → 403 冷却且第二次不发请求 →
404 视为无发布 → 超限下载不留文件。

下面几个卡在边界上的用例值得单独点名，它们对应的是**只有真 HTTP 才复现得出**的失败：

| 用例 | 卡的是什么 |
|---|---|
| `download_streams_to_completion_and_reaches_100_percent` | 有 `Content-Length`，正常收满 |
| `an_oversized_installer_is_refused_before_anything_is_written` | `Content-Length` 声明超限，**一个字节都不写** |
| `a_stream_that_outgrows_the_ceiling_is_cut_off_and_unlinked` | chunked 没有 `Content-Length`，只能边收边判，超限时把已写的半截 `unlink` 掉 |
| `a_truncated_download_leaves_no_installer_behind` | 连接提前断，同样不留文件 |

**安装路径三道闸**（`UpdateManager::verify_installer()`，
`tauri-shell/core/src/update/manager.rs:1284`）：

1. **必须在下载目录内** —— `canonicalize` 后要求相对路径不出现 `..`；符号链接会被解引用，
   防止拿软链指到目录外面去。
2. **扩展名必须是 `.exe`** —— `version::allowed_installer_suffixes("win32")` 的返回值
   **只有 `["exe"]` 一项**，`.msi` 不在允许列表内。
3. **必须是本项目产物** —— 文件名必须包含 `updateAssetKeyword`（默认 `AgentKeeper`）。
   这条是本项目加的、上游没有的闸门：`updateRepo` 配错时，它拦住
   「把别人的安装包装到自己机器上」。真实例子里 `Agent2API_3.1.0_x64-setup.exe`
   会被这条拒绝，而不会被执行。

（另有两条前置输入校验：路径非空、必须是绝对路径。它们不计入三道闸。）

IPC 面的契约由 `tauri-shell/src-tauri/tests/update.rs` 守：status 形状、
参数校验（repo / 间隔 / 代理）、skip、令牌在无安全存储时被拒绝、install 拒绝非法路径；
并且断言**每条守卫消息都是英文、且在词典里有对应条目**（见 DESIGN §2.1 的契约）。

> **没验证到的部分**：真实 UAC 提权与 NSIS 覆盖安装无法自动化，只能人工点一次。

## 10. 发布：让通道有内容

更新器只认 **GitHub Release 资产**，仓库地址已内置（`BUILT_IN_REPO`），用户零配置。

发一版只需要一条命令：

```bash
git tag v0.3.0 && git push origin v0.3.0
```

想在本地先自测：

```bash
npm run tauri:bundle           # 前端 → dist，构建，归档到 release/
```

产物：`release/AgentKeeper_<ver>_x64-setup.exe` 与 `release/AgentKeeper_<ver>_x64-portable.zip`。

### `.github/workflows/release.yml` 的实际步骤

| # | 步骤 | 说明 |
|---|---|---|
| 1 | `actions/checkout@v4` | |
| 2 | `actions/setup-node@v4`（Node 20） | **没有 `npm ci`** —— 前端没有 npm 依赖，Node 只用来跑 `tauri:i18n` 与 `copy-frontend.js` |
| 3 | `npm run tauri:i18n` | i18n 闸，放在构建**之前**：漏翻译在这里被抓到只是重跑一次，发布后被看到就是中文界面里的一句英文 |
| 4 | `node tauri-shell/copy-frontend.js` | 重新生成 `tauri-shell/dist`。该目录**不入库**（`.gitignore` 排除——入库副本可能相对 `src/renderer` 悄悄过期），这一步保证可执行文件带的是本次提交的渲染层 |
| 5 | Stamp the release version | 用 tag 号回写 `tauri.conf.json` 与 `Cargo.toml` —— **见下方警告** |
| 6 | `dtolnay/rust-toolchain@stable` + `Swatinem/rust-cache@v2` | target `x86_64-pc-windows-msvc` |
| 7 | `cargo install tauri-cli --version "^2" --locked` | |
| 8 | `cargo tauri build` + `package-release.js --no-build` | 产物落在 `tauri-shell/target/release/bundle/nsis/` 与 `release/*.zip` |
| 9 | `softprops/action-gh-release@v2` | 上传**安装包 exe + 便携 zip**；tag 含 `beta` / `rc` / `alpha` 时自动标记 pre-release |

**两条构建通道**：

| 触发 | 结果 |
|---|---|
| push tag `vX.Y.Z` | 正式发布：版本号从 tag 戳进构建，安装包与便携包挂到该 tag 的 Release 上 |
| push 到 `main` | **只构建，不发布**：同一套产物作为该次运行的 Artifacts 上传（`agentkeeper-windows-x64`，保留 30 天），Releases 页不会多出任何条目 |

推 `main` 不发布是有意的：早先每次推 `main` 都会生成一条 `v<版本>-ci.<run>` 预发布，把真正的版本埋在 CI 噪音下面（应用内更新器本来就不看预发布，那些条目纯属杂物）。需要某个提交的产物时，从该次运行的 Artifacts 下载即可。

> **⚠️ 版本号必须回写 `Cargo.toml` + `tauri.conf.json`（第 5 步）——这是整条发布链最容易翻车的地方。**
>
> Rust 侧的 `UpdateManager::app_version` 是 `env!("CARGO_PKG_VERSION")`，**编译期常量**。
> 忘了回写，程序就会一直自报 `Cargo.toml` 里那个旧版本号，于是每次检查都判定「有更新」：
> **用户刚装完 v0.3.0，下次检查又被提示装 v0.3.0，反复循环。**
>
> 顺带：安装包文件名里的版本号也来自这里，所以漏了回写，连资产名都会带着旧版本。

### 要点

- **更新器只读 GitHub Release API 的 `assets[]`，从不解析 `latest.yml` / `.blockmap` /
  `latest.json`** —— 那三个属于其他更新器的格式，与本更新器无关。（Rust 源码里对这三者
  零引用，可以自己 grep 验证。）
- **资产挑选规则**：排除 `-portable` / 归档 / 其他平台 → 优先名字含 `setup|install`
  → 其次按架构 → 最后取第一个。所以**只上传安装包即可**；portable 传上去也不会被选中
  （它装不出快捷方式与卸载项）。
- **仓库没 tag** → 没 Release → `checked:true, latest:null, hasUpdate:null`
  → 界面显示「该仓库还没有发布版本」。这是诚实的「无可更新」，不是故障。
- **Release 里没有本平台资产** → 检查照样成功，只是 `downloadSupported:false`，
  界面不显示「安装并退出」按钮。
- **portable 版用户也能收到更新提示**，但它自己不能被 NSIS 覆盖安装 —— 这是便携版的固有约束，
  不在 updater 里掩盖。
- **装完落在哪由 NSIS 决定，不在 updater 的管辖内** —— 实测（`/S` 静默安装，EXIT=0）默认落到
  `%LOCALAPPDATA%\AgentKeeper`。发布说明里如果要写路径，写这个；别从 `installer.nsi` 推。

> **没验证到的部分**：真实 UAC 提权与 NSIS 覆盖安装无法自动化，只能人工点一次。
