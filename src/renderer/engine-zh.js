'use strict';
/**
 * Engine-layer (core/ + src/main/ipc.js) English → Chinese translation.
 *
 * Policy (user mandate, 2026-10-18): when the UI language is Chinese there is
 * NO English degradation — every engine-produced sentence that reaches the
 * screen must render in Chinese, except proper nouns / protocol words.
 *
 * How it works:
 *   - EXACT     full-string replacements for engine messages.
 *   - PATTERNS  [pattern, zh] pairs; §1..§n are placeholders replaced by
 *               (.+) capture groups at runtime. The SAME pattern list is
 *               used by selftest §18 to audit engine source files: every
 *               sentence-like literal must hit EXACT, PATTERNS or INTERNAL.
 *               A miss fails the build — that is the no-degradation guarantee.
 *   - INTERNAL  audit allowlist for engine strings that never reach the UI
 *               (audit-log action names, protocol metadata, external tool
 *               output passthrough).
 *   - FIELD     validation issue field names as shown as chips.
 *
 * Applied at renderer choke points (toast, issue panels, findings, activity,
 * health detail) and in the main process for native dialogs.
 */

(function () {
const EXACT = {
  /* Electron native-menu labels (`src/main/main.js` buildMenu → `zhText`).
     These were briefly removed on 2026-10-20 as "dead entries" because
     `Menu|MenuItem|Submenu` has zero hits under `src-tauri/src` — but the
     Electron shell is still the shipped entry point (`package.json` `main`),
     and `buildMenu()` translates every label through `zhText`. Removing them
     meant the menu bar rendered in English under a Chinese UI. */
  'File': '文件',
  'Reload': '重新加载',
  'Open Store Folder': '打开存储目录',
  'Exit': '退出',
  'View': '视图',
  'Reload UI': '重新加载界面',
  'Toggle Developer Tools': '开发者工具',
  'Actual Size': '实际大小',
  'Zoom In': '放大',
  'Zoom Out': '缩小',
  'Toggle Full Screen': '切换全屏',

  /* store.js */
  'no pending changes': '没有待应用的变更',
  'corrupt transaction record': '事务记录已损坏',
  'at least one valid target is required': '至少需要一个有效目标',
  'invalid transaction id': '无效的事务 ID',
  'Import project': '导入项目',

  /* keeper.js */
  'no valid target': '没有有效目标',
  'asset not found': '未找到该资产',
  'this asset is host-owned (plugin cache / shared dir) and cannot be edited here':
    '此资产由宿主应用管理（插件缓存/共享目录），无法在此编辑',
  'only JSON-based MCP config files can be safely edited in place':
    '只有 JSON 格式的 MCP 配置文件才能安全地就地编辑',
  'host-owned asset — delete it from the owning agent UI instead':
    '宿主管理的资产——请改在所属 agent 的界面中删除',
  'same location': '目标位置与来源相同',
  'agent has no skill directory': '该 agent 没有技能目录',
  'already exists': '同名资产已存在',
  'unresolved base dir': '基础目录未能解析',
  'nothing to write': '没有需要写入的内容',
  'no assets selected': '未选择任何资产',
  'target directory does not exist': '目标目录不存在',
  'nothing exportable in selection': '所选内容中没有可导出的资产',
  'manifest unreadable': '清单文件无法读取',
  'nothing importable found': '没有发现可导入的内容',

  /* scanner.js */
  'project already registered': '项目已注册',
  'missing YAML frontmatter — the agent cannot load this skill':
    '缺少 YAML frontmatter——agent 无法加载此技能',
  'frontmatter.description missing — skill will never auto-trigger':
    'frontmatter 缺少 description——技能将永远不会被自动触发',
  'unparsable config': '配置无法解析',

  /* discover.js */
  'registry timeout': '注册表请求超时',
  'repo must be owner/repo or a github.com URL': '仓库地址必须是 owner/repo 或 GitHub 链接',
  'nothing installable': '仓库中没有可安装的技能',

  /* src/main/ipc.js — software update */
  'invalid url': '无效的链接',
  'staged in the shadow layer — apply in the AgentKeeper UI': '已暂存到影子层——请在 AgentKeeper 界面中应用',
  'written directly (sandbox off)': '已直接写入（沙盒已关闭）',

  /* ipc.js (also shown in native dialogs from the main process) */
  'config must be an object': '配置必须是对象',
  'Select one or more project folders to import': '请选择一个或多个要导入的项目文件夹',
  'not a directory': '所选路径不是文件夹',
  'directory not found': '文件夹不存在',
  'all writes are routed to the sandbox tree': '所有写入均已路由至沙盒目录树',
  'writes now hit real agent directories': '写入现已直接作用于真实 agent 目录',
  'Export bundle': '导出 Bundle',
  'Import bundle': '导入 Bundle',
  'unknown agent': '未知的 agent',
  'path not found (in sandbox the real path may not exist yet)':
    '路径不存在（沙盒模式下真实路径可能尚未生成）',
  'path required': '缺少路径参数',

  /* validate.js — skill */
  'name is required': '名称必填',
  'name must be lowercase letters/digits/hyphen, max 64 chars':
    '名称只能包含小写字母、数字和连字符，最长 64 个字符',
  'description is required — it is how the agent decides to load the skill':
    '描述必填——agent 依据描述决定何时加载该技能',
  'description over 1024 chars may be truncated by some agents':
    '描述超过 1024 字符，部分 agent 可能会截断它',
  'SKILL.md body is empty': 'SKILL.md 正文为空',
  'version should look like 1.0.0': '版本号应符合 1.0.0 格式',
  'allowed-tools must be a string or array': 'allowed-tools 必须是字符串或数组',
  'compatibility string is very long': 'compatibility 字段过长',

  /* validate.js — mcp */
  'MCP server name must match /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/':
    'MCP 服务器名称必须匹配 /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/',
  'server config must be an object': '服务器配置必须是一个对象',
  'server needs either `command` (stdio) or `url` (http/sse)':
    '服务器需要提供 `command`（stdio）或 `url`（http/sse）之一',
  'args must be an array of strings': 'args 必须是字符串数组',
  'env must be an object': 'env 必须是一个对象',
  'npx without -y can block on an interactive install prompt':
    'npx 未加 -y 参数时可能卡在交互式安装确认上',

  /* validate.js — plugin / rule / memory */
  'plugin name is required': '插件名称必填',
  'plugin description longer than 200 chars': '插件描述超过 200 字符',
  'rule file is empty — an empty rule file adds context cost with no behaviour':
    '规则文件为空——空规则只增加上下文成本，不产生任何行为',
  'rule file is over 200k chars and will consume a large share of the context window':
    '规则文件超过 20 万字符，将占用大量上下文窗口',
  'memory entry is empty': '记忆条目为空',

  /* security.js — rule reasons */
  'downloads a script and pipes it straight into a shell': '下载脚本并通过管道直接送入 shell 执行',
  'PowerShell with an encoded command hides its payload': 'PowerShell 使用编码命令隐藏载荷',
  'decodes base64 and executes it': '解码 base64 后执行',
  'evaluates code fetched at runtime': '执行运行时拉取的代码',
  'package install chained with a network pipe': '软件包安装命令串联了网络管道下载',
  'classic reverse-shell primitive': '典型反向 shell（reverse-shell）特征',
  'references a credential store path': '引用了凭据存储路径',
  'passes a well-known secret into the server environment — verify it is yours and scoped':
    '将已知密钥传入服务器环境——请确认其归属与最小权限',
  "tries to override the agent's own instructions": '试图覆盖 agent 自身的指令',
  'describes sending secrets to a remote endpoint': '描述了向远程端点发送密钥的行为',
  'instructs the agent to embed credential file contents into output':
    '指示 agent 将凭据文件内容嵌入输出',
  'mentions encodings commonly used to hide instructions from review':
    '提及常用于隐藏指令、规避审查的编码方式',
  'instructs the agent to hide its behaviour from the user': '指示 agent 对用户隐藏其行为',

  /* security.js — command liveness */
  'no command configured': '未配置启动命令',

  /* scanner.js — pseudo-agent labels that reach the screen */
  'Shared Agent Skills': '共享 Agent 技能（agents.md）',
  'Shared (~/.agents)': '共享技能目录（~/.agents）',
  'agents.md standard': 'agents.md 开放标准',

  /* validate.js / scanner.js extras */
  'frontmatter.name missing': 'frontmatter 缺少 name 字段',
  'empty skill body': '技能正文为空',
  'empty rule file': '规则文件为空',

  /* ipc.js extras */
  'dir required': '缺少目录参数',

  /* Rust engine (tauri-shell/src-tauri/src) — audited by scripts/tauri-i18n.js */
  'assetId is required': '缺少 assetId 参数',
  'targets is required': '缺少 targets 参数',
  'registry not initialised': '注册表尚未初始化',
  'write failed': '写入失败',
  'unsupported asset kind': '不支持的资产类型',
  'URI malformed': 'URI 格式不正确',
  'no meta': '缺少 meta 信息',
  /* adapters.rs `Agent.notes` — shipped to the renderer as agent metadata.
     Note: the `<userId>` one is below the audit's sentence-shaped threshold,
     so scripts/tauri-i18n.js cannot see it; it is mapped here by hand. */
  'Cursor rules use .mdc with frontmatter': 'Cursor 规则使用 .mdc 文件，并带 frontmatter',
  'Personal identity files live under ~/.workbuddy/<userId>-personal/':
    '个人身份文件位于 ~/.workbuddy/<userId>-personal/ 下',
  /* core/mcp.rs — NO_INSTALLED_AGENT */
  'no installed agent found; pass agentId explicitly':
    '本机没有检测到已安装的 agent；请显式指定 agentId',
  /*
   * core/update/manager.rs ERR_* consts. The author deliberately wrote these
   * in English (the JS `core/errors.js` templates held Chinese, so an English
   * UI used to print Chinese). Translations are taken verbatim from the
   * matching CODE_ZH entries so one rule keeps one rendering.
   */
  'check interval must be between 1 and 1440 minutes': '检查间隔必须是 1 ~ 1440 分钟',
  'update source must be an http(s) URL': '更新源必须是 http(s) 地址',
  'proxy must be auto, direct or an http(s) URL': '代理必须是 auto、direct 或 http(s) 地址',
  'token must be a string or null': 'token 必须是字符串或 null',
  'request is missing the token field (pass null to clear)': '请求缺少 token 字段（清除请传 null）',
  /* core/update/token.rs — DPAPI guards (defensive; never reached on a
     working build, but they are `Err(...)` strings, so the audit sees them. */
  'DPAPI refuses empty input': 'DPAPI 不接受空数据',
  'DPAPI refuses empty cipher text': 'DPAPI 数据为空',
  'secure storage is unavailable (no DPAPI on this platform)':
    '本机安全存储不可用（当前平台没有 DPAPI）',

  /*
   * core/update/*.rs + core/http.rs — messages the JS engine emitted in
   * Chinese (core/update/{http,manager,token,version}.js). The Rust port now
   * emits English; every Chinese below is copied verbatim from the JS source
   * so the Chinese UI reads exactly as it did before the migration.
   */
  /* core/http.rs — `describeError` / `describe_io_error` */
  'DNS lookup failed': '域名解析失败',
  'connection timed out': '连接超时',
  'connection refused (is the proxy running?)': '连接被拒绝（代理未运行？）',
  'connection was reset': '连接被重置',
  'unknown error': '未知错误',
  'direct connection': '直连',
  'download was aborted': '下载已中止',
  'installer exceeds the size cap; download aborted': '安装包超过体积上限，已中止',
  /* core/update/http.rs */
  'GitHub API rate limit reached; checks are paused and the last result kept. Add a GitHub token in update settings to raise the limit':
    'GitHub 接口访问受限，已暂停检查并保留上次结果；可在更新设置里填写 GitHub 令牌提高限额',
  /* core/update/manager.rs */
  'installer path is empty': '安装包路径为空',
  'installer path must be absolute': '安装包路径必须是绝对路径',
  'installer is missing or is not a file': '安装包不存在或不是文件',
  'installer is outside the managed download directory; execution refused':
    '安装包不在受控的下载目录内，已拒绝执行',
  'the installer is still downloading; wait for the download to finish before installing':
    '安装包还在下载中，请等下载完成后再安装',
  'failed to save settings': '设置保存失败',
  /*
   * Deliberately shadows `PATTERNS` `^cannot reach the server: §1$`.
   * `zhError()` checks EXACT first, so this entry wins and translates
   * `connection timed out` too; the PATTERNS rule would only paste the raw
   * English into §1 ("无法连接服务器：connection timed out").
   * Deleting this line does NOT make the sentence fail the audit — it makes
   * it render differently and still green. Do not remove it.
   */
  'cannot reach the server: connection timed out': '无法连接服务器：连接超时',
  /* core/update/token.rs */
  'token cannot be empty (pass null to clear it)': '令牌不能为空（要清除请传 null）',
  'token may only contain letters, digits, underscores and hyphens':
    '令牌只能包含字母、数字、下划线与连字符',
  'secure storage on this machine is unavailable (the saved token cannot be decrypted)':
    '本机安全存储不可用（无法解密已保存的令牌）',
  'secure storage on this machine is unavailable; refusing to save the token in plain text':
    '本机安全存储不可用，已拒绝以明文保存令牌',
  'cannot decrypt': '无法解密',
  /* core/update/version.rs */
  'the download address is not a valid URL': '下载地址不是合法 URL',
  'the download address must be https': '下载地址必须是 https'

  /* The `[update]` diagnostics used to be mapped here. They are `self.log(...)`
     lines from core/update/manager.rs, whose logger is the no-op `|_| {}` —
     they reach neither the WebView nor a terminal, so the audit classifies
     them as logSurface and never required a translation. Removed rather than
     carried as unreachable entries; re-add them together with a real logger. */
};

/*
 * Patterns for messages with interpolated values. §n = capture n.
 * Audited against engine sources with `${...}` normalized to §n.
 */
const PATTERNS = [
  ['^op[§1] is missing an "op" field$', 'op[§1] 缺少 "op" 字段'],
  ['^unknown op §1$', '未知操作：§1'],
  ['^scan payload is not cloneable: §1$', '扫描结果无法克隆：§1'],
  ['^"§1" is a reserved name on Windows$', '「§1」是 Windows 的保留名称'],
  ['^directory is "§1" but name is "§2" — some agents key the cache by directory$',
    '目录名为「§1」，但名称为「§2」——部分 agent 以目录名作为缓存键'],
  ['^unrecognized key "§1" — agent may ignore it$', '无法识别的键「§1」——agent 可能会忽略它'],
  ['^unknown kind §1$', '未知的资产类型：§1'],
  ['^git clone failed: §1$', '仓库拉取失败：§1'],
  ['^registry http §1$', '注册表返回 HTTP 错误：§1'],
  ['^registry unreachable: §1$', '注册表无法访问：§1'],
  ['^agent "§1" has no global skills directory$', 'agent「§1」没有全局技能目录'],
  ['^no SKILL.md found in §1$', '在 §1 中没有找到 SKILL.md'],
  ['^install failed: §1$', '安装失败：§1'],
  ['^asset not found: §1$', '未找到该资产：§1'],
  ['^cannot resolve skill directory for §1/§2$', '无法解析技能目录：§1/§2'],
  ['^renaming a skill requires a move (directory is "§1")$', '重命名技能需要移动目录（当前目录为「§1」）'],
  ['^no MCP config file declared for §1 (§2)$', '没有为 §1 声明 MCP 配置文件（§2）'],
  ['^write failed: §1$', '写入失败：§1'],
  ['^settings.json write failed: §1$', 'settings.json 写入失败：§1'],
  ['^baseline.json write failed: §1$', 'baseline.json 写入失败：§1'],
  ['^not an agentkeeper bundle (manifest.json missing)$', '不是 agentkeeper Bundle（缺少 manifest.json）'],
  ['^unsupported bundle schema: §1$', '不支持的 Bundle 格式版本：§1'],
  ['^unknown asset kind: §1$', '未知的资产类型：§1'],
  ['^directory not found: §1$', '文件夹不存在：§1'],
  ['^command not found on PATH: §1$', '在 PATH 中找不到命令：§1'],
  ['^op[§1] (§2) is missing a "path"$', '第 §1 个操作（§2）缺少 "path" 字段'],
  ['^copy source missing: §1$', '复制的来源文件不存在：§1'],
  ['^json mutate aborted for §1$', 'JSON 变更已中止：§1'],
  ['^transaction §1 not found$', '未找到事务：§1'],
  ['^config file is corrupt, refusing to overwrite: §1$', '配置文件已损坏，拒绝覆盖写入：§1'],
  /*
   * Rust engine — audited by scripts/tauri-i18n.js. The Rust side
   * returns `spawn_blocking` failures by prefix rather than by error code, so
   * these are matched on the sentence.
   */
  ['^timed out after §1ms$', '操作超时（§1 毫秒）'],
  ['^scan task failed: §1$', '扫描任务失败：§1'],
  ['^read_asset task failed: §1$', '读取资产任务失败：§1'],
  ['^bad json: §1$', '响应不是合法 JSON：§1'],
  ['^write source is a directory, not a file: §1$', '写入来源是目录而非文件：§1'],
  ['^registry task failed: §1$', '注册表任务失败：§1'],
  ['^install task failed: §1$', '安装任务失败：§1'],
  ['^export task failed: §1$', '导出任务失败：§1'],
  ['^import task failed: §1$', '导入任务失败：§1'],
  ['^update task failed: §1$', '更新任务失败：§1'],
  ['^download task failed: §1$', '下载任务失败：§1'],
  // lib.rs 2026-10-20: the remaining spawn_blocking wrappers were brought in line
  // with the scan/install ones. One entry per command name, matching the prefix
  // the Rust side formats, so the gate can prove each has a translation.
  ['^distribute task failed: §1$', '分发任务失败：§1'],
  ['^baseline task failed: §1$', '基线任务失败：§1'],
  ['^rollback task failed: §1$', '回滚任务失败：§1'],
  ['^apply task failed: §1$', '应用任务失败：§1'],
  ['^discard task failed: §1$', '放弃暂存失败：§1'],
  ['^audit task failed: §1$', '操作记录读取失败：§1'],
  ['^transactions task failed: §1$', '事务记录读取失败：§1'],
  ['^stats task failed: §1$', '本地统计读取失败：§1'],
  ['^op[§1] (§2) is missing a "from"$', '第 §1 个操作（§2）缺少 "from" 字段'],
  ['^cannot read write source §1: §2$', '无法读取写入源 §1：§2'],
  ['^write failed for §1: §2$', '写入 §1 失败：§2'],

  /*
   * core/http.rs + core/update/*.rs — see the EXACT block above: each Chinese
   * is copied verbatim from the JS engine (core/update/{http,manager,token,
   * version}.js) so the Chinese UI does not change.
   */
  ['^TLS failed (§1)$', 'TLS 失败（§1）'],
  ['^cannot resolve the redirect target: §1$', '无法解析重定向地址: §1'],
  ['^cannot reach the server: §1$', '无法连接服务器：§1'],
  ['^failed to write the installer: §1$', '写入安装包失败: §1'],
  ['^download interrupted: §1$', '下载中断: §1'],
  ['^installer size looks wrong (§1 MB); download aborted$', '安装包体积异常（§1 MB），已中止'],
  ['^response body exceeds the limit (§1 bytes)$', '响应体超过上限（§1 字节）'],
  ['^failed to parse the response: §1$', '解析响应失败: §1'],
  ['^GitHub returned HTTP §1$', 'GitHub 返回 HTTP §1'],
  ['^only §1 installers may be run$', '只允许运行 §1 安装包'],
  ['^installer filename does not contain "§1"; it looks like another project, so execution was refused$',
    '安装包文件名不含「§1」，疑似来自其他项目，已拒绝执行'],
  ['^installer is incomplete (expected §1 bytes, got §2 bytes)$',
    '安装包不完整（期望 §1 字节，实际 §2 字节）'],
  ['^download failed: the server returned HTTP §1$', '下载失败：服务器返回 HTTP §1'],
  ['^failed to launch the installer: §1$', '启动安装程序失败: §1'],
  ['^the silent install did not start (§1). You can run §2 from the download directory manually$',
    '静默安装未能启动（§1）。可手动运行下载目录里的 §2'],
  ['^token is too long (at most §1 characters)$', '令牌过长（最多 §1 字符）'],
  ['^the saved token could not be decrypted (§1)$', '已保存的令牌无法解密（§1）'],
  ['^failed to encrypt the token: §1$', '令牌加密失败：§1'],
  ['^DPAPI encryption failed (§1)$', 'DPAPI 加密失败（§1）'],
  ['^DPAPI decryption failed (§1)$', 'DPAPI 解密失败（§1）'],
  ['^download address host is not on the allow list: §1$', '下载地址域名不在允许列表内: §1']

  /* The `[update]` log-surface patterns were removed with the EXACT note above:
     same unreachable diagnostics, same reason. */
];

/* validation-issue field chips: rendered next to messages */
/**
 * Error code → Chinese. Keys are exactly `core/errors.js` CODES (selftest
 * §18 fails if the two tables ever drift), so the renderer never has to
 * match an English sentence to decide what to display.
 */
const CODE_ZH = {
  'asset.notFound': '未找到该资产',
  'asset.notFoundId': '未找到该资产：§1',
  'asset.hostOwnedEdit': '此资产由宿主应用管理（插件缓存/共享目录），无法在此编辑',
  'asset.hostOwnedDelete': '宿主管理的资产——请改在所属 agent 的界面中删除',
  'model.unknownKind': '未知的资产类型：§1',
  'write.noValidTarget': '至少需要一个有效目标',
  'write.nothingToWrite': '没有需要写入的内容',
  'write.targetDirMissing': '目标目录不存在',
  'write.failed': '写入失败：§1',
  'store.corruptTransaction': '事务记录已损坏',
  'store.opFieldMissing': '第 §1 项操作缺少 op 字段',
  'store.opPathMissing': '第 §1 项操作（§2）缺少 path 字段',
  'store.opFromMissing': '第 §1 项操作（§2）缺少 from 字段',
  'store.unknownOp': '未知操作类型：§1',
  'store.copySourceMissing': '复制的源路径不存在：§1',
  'store.configCorrupt': '配置文件已损坏，拒绝覆盖：§1',
  'store.jsonMutateAborted': 'JSON 变更已中止：§1',
  'store.txNotFound': '找不到事务 §1',
  'ipc.invalidTx': '无效的事务 ID',
  'mcp.jsonOnly': '只有 JSON 格式的 MCP 配置文件才能安全地就地编辑',
  'mcp.configNotObject': '配置必须是对象',
  'mcp.noConfigFile': '§1（§2）没有声明 MCP 配置文件',
  'mcp.sandboxGate': '沙盒已开启——暂存的变更需在 AgentKeeper 桌面端由人工审批。要让 agent 直接写入，请先关闭沙盒。',
  'project.duplicate': '项目已注册',
  'project.notDirectory': '所选路径不是文件夹',
  'project.directoryNotFound': '文件夹不存在',
  'project.dirRequired': '缺少目录参数',
  'ipc.pathRequired': '缺少路径参数',
  'ipc.pathNotFound': '路径不存在（沙盒模式下真实路径可能尚未生成）',
  'scan.dirNotFound': '目录不存在：§1',
  'skill.dirUnresolved': '无法解析 §1/§2 的技能目录',
  'ipc.unknownAgent': '未知的 agent',
  'bundle.noSelection': '未选择任何资产',
  'bundle.nothingExportable': '所选内容中没有可导出的资产',
  'bundle.nothingImportable': '没有发现可导入的内容',
  'bundle.manifestUnreadable': '清单文件无法读取',
  'bundle.badSchema': '不支持的 bundle schema：§1',
  'bundle.notBundle': '不是 AgentKeeper bundle（缺少 manifest.json）',
  'discover.repoFormat': '仓库地址必须是 owner/repo 或 GitHub 链接',
  'discover.nothingInstallable': '仓库中没有可安装的技能',
  'discover.noInstalledAgent': '本机没有检测到已安装的 agent；请显式指定 agentId',
  'discover.agentNoSkillsDir': 'agent「§1」没有全局技能目录',
  'discover.registryHttp': '注册表返回 HTTP §1',
  'discover.cloneFailed': 'git clone 失败：§1',
  'discover.noSkillMd': '仓库 §1 中没有 SKILL.md',
  'discover.installFailed': '安装失败：§1',
  'update.settingsSaveFailed': '设置保存失败',
  'update.internalNoStore': '内部错误：更新管理器缺少存储',
  'ipc.repoFormat': '仓库必须是 owner/repo 形式',
  'ipc.apiBaseFormat': '更新源必须是 http(s) 地址',
  'ipc.proxyFormat': '代理必须是 auto、direct 或 http(s) 地址',
  'ipc.intervalRange': '检查间隔必须是 1 ~ 1440 分钟',
  'ipc.invalidUrl': '无效的链接',
  'update.downloadUrlInvalid': '下载地址不是合法 URL',
  'update.downloadUrlScheme': '下载地址必须是 https',
  'update.downloadHost': '下载地址域名不在允许列表内: §1',
  'update.downloadTooLarge': '安装包体积异常（§1 MB），已中止',
  'update.downloadOverCap': '安装包超过体积上限，已中止',
  'update.downloadIncomplete': '安装包不完整（期望 §1 字节，实际 §2 字节）',
  'update.downloadInterrupted': '下载中断：§1',
  'update.writeFailed': '写入安装包失败：§1',
  'update.installerPathEmpty': '安装包路径为空',
  'update.installerPathRelative': '安装包路径必须是绝对路径',
  'update.installerExtension': '只允许运行 §1 安装包',
  'update.installerMissing': '安装包不存在或不是文件',
  'update.installerOutsideDir': '安装包不在受控的下载目录内，已拒绝执行',
  'update.installerForeign': '安装包文件名不含「§1」，疑似来自其他项目，已拒绝执行',
  'update.installerBusy': '安装包还在下载中，请等下载完成后再安装',
  'update.launchFailed': '启动安装程序失败：§1',
  'update.tokenFieldRequired': '请求缺少 token 字段（清除请传 null）',
  'update.tokenType': 'token 必须是字符串或 null',
  'update.tokenEmpty': '令牌不能为空（要清除请传 null）',
  'update.tokenTooLong': '令牌过长（最多 §1 字符）',
  'update.tokenCharset': '令牌只能包含字母、数字、下划线与连字符',
  'update.tokenNoCrypto': '本机安全存储不可用，已拒绝以明文保存令牌',
  'update.tokenNoCryptoRead': '本机安全存储不可用（无法解密已保存的令牌）',
  'update.tokenDecryptFailed': '已保存的令牌无法解密（§1）',
  'update.httpsOnly': 'GitHub 接口访问受限，已暂停检查并保留上次结果；可在更新设置里填写 GitHub 令牌提高限额',
  'update.httpStatus': 'GitHub 返回 HTTP §1',
  'update.parseFailed': '解析响应失败：§1',
  'update.unreachable': '无法连接服务器：§1'
};

/**
 * Render an engine error for the user.
 *
 * `zhCode` is the path: IPC results carry `code` + `params`. `zhText` stays
 * as the fallback for text that has no code yet (audit log lines, MCP
 * payloads, git clone stderr) and for pre-existing strings.
 */
function zhCode(code, params) {
  if (!isZh()) return null;
  const template = CODE_ZH[code];
  if (!template) return null;
  const list = Array.isArray(params) ? params : (params === undefined ? [] : [params]);
  return String(template).replace(/§(\d+)/g, (m, n) => {
    const v = list[Number(n) - 1];
    return v === undefined ? m : String(v);
  });
}

/**
 * Best-effort Chinese for anything the engine produced: a coded error first,
 * then the legacy sentence dictionary, then the original text.
 */
function zhError(payload) {
  if (payload && typeof payload === 'object') {
    const byCode = zhCode(payload.code, payload.params);
    if (byCode) return byCode;
    return zhText(String(payload.error || payload.message || ''));
  }
  return zhText(String(payload == null ? '' : payload));
}
const FIELD = {
  name: '名称',
  description: '描述',
  body: '正文',
  version: '版本',
  config: '配置',
  kind: '类型',
  command: 'command',
  url: 'url',
  args: 'args',
  env: 'env',
  headers: 'headers',
  timeout: 'timeout',
  cwd: 'cwd',
  transport: 'transport',
  'allowed-tools': 'allowed-tools',
  compatibility: 'compatibility'
};

/*
 * Audit allowlist: engine strings that never surface in the UI (internal
 * audit-log labels, protocol metadata). Kept minimal — the audit test fails
 * on anything not listed here, so new user-facing English fails the build.
 */
const INTERNAL = new Set([
  'use strict'
]);

function patternToRegExp(src) {
  // Escape regex metacharacters but keep ^ $ as anchors and § as placeholder.
  const escaped = src.replace(/[.*+?'"{}()|[\]\\]/g, '\\$&');
  return new RegExp(escaped.replace(/§(\d)/g, '(.+)'), 's');
}

const compiled = PATTERNS.map(([p, zh]) => ({ re: patternToRegExp(p), zh }));

/**
 * Is the UI currently Chinese?
 *
 * The engine emits English sentences (its `zh` dictionary is what makes them
 * Chinese), so these translators must only run when the UI language is zh.
 * They used to run unconditionally, which is how an English UI ended up with
 * "Shared (~/.agents)" rendered as 共享技能目录（~/.agents） in the "By agent"
 * table — 18 call sites, one cause. Outside a browser (the i18n gate and the
 * Node tests require this file) there is no UI language, and zh is the safe
 * answer: the gate is auditing the dictionary itself.
 */
function isZh() {
  try {
    if (typeof window === 'undefined') return true;
    const i18n = window.__akI18n;
    return !i18n || !i18n.lang || i18n.lang === 'zh';
  } catch (_e) {
    return true;
  }
}

/** Translate one engine string. Passes through anything already Chinese. */
function zhText(s) {
  if (typeof s !== 'string' || !s) return s;
  if (!isZh()) return s;
  if (/[\u4e00-\u9fff]/.test(s)) return s;
  if (Object.prototype.hasOwnProperty.call(EXACT, s)) return EXACT[s];
  for (const { re, zh } of compiled) {
    const m = s.match(re);
    if (m) {
      return zh.replace(/§(\d)/g, (_, d) => (m[Number(d)] != null ? m[Number(d)] : ''));
    }
  }
  return s;
}

/** Translate a validation-issue field name for display. */
function zhField(f) {
  if (!isZh()) return f;
  return (f && Object.prototype.hasOwnProperty.call(FIELD, f)) ? FIELD[f] : f;
}

/** Audit helper: is a (normalized) engine string covered? */
function isCovered(s) {
  if (Object.prototype.hasOwnProperty.call(EXACT, s)) return true;
  if (INTERNAL.has(s)) return true;
  return compiled.some(({ re }) => re.test(s));
}

  const api = { EXACT, PATTERNS, FIELD, INTERNAL, CODE_ZH, zhText, zhField, zhCode, zhError, isCovered, patternToRegExp };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  if (typeof window !== 'undefined') window.__akEngineZh = api;
})();
