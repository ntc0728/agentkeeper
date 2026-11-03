'use strict';
/**
 * 引擎层的英文 → 中文翻译表。
 *
 * 策略：当界面语言是中文时，绝不允许出现英文降级——
 * 凡是到达屏幕上的引擎产出的句子都必须渲染为
 * 中文，专有名词 / 协议词除外。
 *
 * 工作方式：
 *   - EXACT     引擎消息的整串替换。
 *   - PATTERNS  [pattern, zh] 对；§1..§n 是占位符，运行时由
 *               (.+) 捕获组替换。同一份模式列表也被
 *               `scripts/tauri-i18n.js` 使用：引擎里每一个像句子的字面量
 *               都必须命中 EXACT、PATTERNS 或 INTERNAL。
 *               漏掉一个就会让构建失败——这就是不降级的保证。
 *   - INTERNAL  审计白名单，收录永远不会进入 UI 的引擎字符串
 *               （审计日志的动作名、协议元数据、外部工具
 *               输出的透传内容）。
 *   - FIELD     以标签形式展示的校验问题字段名。
 *
 * 应用位置：渲染进程的各个收口处（toast、问题面板、发现、活动、
 * 健康详情），以及主进程里的原生对话框。
 */

(function () {
const EXACT = {
  /* store：待处理变更、事务 */
  'corrupt transaction record': '事务记录已损坏',
  'at least one valid target is required': '至少需要一个有效目标',
  'Import project': '导入项目',

  /* keeper：读写、分发 */
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
  'agent or project could not be resolved': '无法解析出这个 agent 或项目的目录',
  'nothing to write': '没有需要写入的内容',
  'no assets selected': '未选择任何资产',
  'target directory does not exist': '目标目录不存在',
  'nothing exportable in selection': '所选内容中没有可导出的资产',
  'manifest unreadable': '清单文件无法读取',
  'nothing importable found': '没有发现可导入的内容',

  /* scanner：注册、frontmatter */
  'project already registered': '项目已注册',
  'missing YAML frontmatter — the agent cannot load this skill':
    '缺少 YAML frontmatter——agent 无法加载此技能',
  'frontmatter.description missing — skill will never auto-trigger':
    'frontmatter 缺少 description——技能将永远不会被自动触发',
  'unparsable config': '配置无法解析',

  /* discover：注册表搜索、安装 */
  'registry timeout': '注册表请求超时',
  'repo must be owner/repo or a github.com URL': '仓库地址必须是 owner/repo 或 GitHub 链接',
  'nothing installable': '仓库中没有可安装的技能',

  /* update：IPC 接口面 */
  'invalid url': '无效的链接',
  'written directly': '已直接写入',

  /* IPC 输入校验（原生对话框也复用） */
  'config must be an object': '配置必须是对象',
  'Select one or more project folders to import': '请选择一个或多个要导入的项目文件夹',
  'not a directory': '所选路径不是文件夹',
  'directory not found': '文件夹不存在',
  'Export bundle': '导出 Bundle',
  'Import bundle': '导入 Bundle',
  'unknown agent': '未知的 agent',
  'path not found': '路径不存在',
  'path required': '缺少路径参数',

  /* 校验：技能 */
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

  /* 校验：MCP */
  'MCP server name must match /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/':
    'MCP 服务器名称必须匹配 /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/',
  'server config must be an object': '服务器配置必须是一个对象',
  'server needs either `command` (stdio) or `url` (http/sse)':
    '服务器需要提供 `command`（stdio）或 `url`（http/sse）之一',
  'args must be an array of strings': 'args 必须是字符串数组',
  'env must be an object': 'env 必须是一个对象',
  'npx without -y can block on an interactive install prompt':
    'npx 未加 -y 参数时可能卡在交互式安装确认上',

  /* 校验：插件 / 规则 / 记忆 */
  'plugin name is required': '插件名称必填',
  'plugin description longer than 200 chars': '插件描述超过 200 字符',
  'rule file is empty — an empty rule file adds context cost with no behaviour':
    '规则文件为空——空规则只增加上下文成本，不产生任何行为',
  'rule file is over 200k chars and will consume a large share of the context window':
    '规则文件超过 20 万字符，将占用大量上下文窗口',
  'memory entry is empty': '记忆条目为空',

  /* security.js —— 规则命中原因 */
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

  /* security.js —— 命令存活性 */
  'no command configured': '未配置启动命令',

  /* scanner：会出现在屏幕上的伪 agent 标签 */
  'Shared Agent Skills': '共享 Agent 技能（agents.md）',
  'Shared (~/.agents)': '共享技能目录（~/.agents）',
  'agents.md standard': 'agents.md 开放标准',

  /* 校验 / scanner 的补充项 */
  'frontmatter.name missing': 'frontmatter 缺少 name 字段',
  'empty skill body': '技能正文为空',
  'empty rule file': '规则文件为空',

  /* IPC 的补充项 */
  'dir required': '缺少目录参数',

  /* Rust 引擎（tauri-shell/src-tauri/src）——由 scripts/tauri-i18n.js 审计 */
  'assetId is required': '缺少 assetId 参数',
  'targets is required': '缺少 targets 参数',
  'registry not initialised': '注册表尚未初始化',
  'write failed': '写入失败',
  'unsupported asset kind': '不支持的资产类型',
  'URI malformed': 'URI 格式不正确',
  'no meta': '缺少 meta 信息',
  /* adapters.rs 的 `Agent.notes`——以 agent 元数据的形式送往渲染进程。
     注意：`<userId>` 那一条低于审计的"像句子"阈值，
     所以 scripts/tauri-i18n.js 看不到它；这里由手工映射。 */
  'Cursor rules use .mdc with frontmatter': 'Cursor 规则使用 .mdc 文件，并带 frontmatter',
  'Personal identity files live under ~/.workbuddy/<userId>-personal/':
    '个人身份文件位于 ~/.workbuddy/<userId>-personal/ 下',
  /* core/mcp.rs 的 NO_INSTALLED_AGENT 常量 */
  'no installed agent found; pass agentId explicitly':
    '本机没有检测到已安装的 agent；请显式指定 agentId',
  /*
   * core/update/manager.rs 的 ERR_* 常量。它们本来就是英文：中文
   * 界面在这里翻译。译文与 CODE_ZH 中的条目逐字一致，
   * 因此一条规则只对应一种呈现。
   */
  'check interval must be between 1 and 1440 minutes': '检查间隔必须是 1 ~ 1440 分钟',
  'update source must be an http(s) URL': '更新源必须是 http(s) 地址',
  'proxy must be auto, direct or an http(s) URL': '代理必须是 auto、direct 或 http(s) 地址',
  'token must be a string or null': 'token 必须是字符串或 null',
  'request is missing the token field (pass null to clear)': '请求缺少 token 字段（清除请传 null）',
  /* core/update/token.rs —— DPAPI 防护（防御性；在正常构建下
     永远不会触发，但它们是 `Err(...)` 字符串，所以审计能看见。 */
  'DPAPI refuses empty input': 'DPAPI 不接受空数据',
  'DPAPI refuses empty cipher text': 'DPAPI 数据为空',
  'secure storage is unavailable (no DPAPI on this platform)':
    '本机安全存储不可用（当前平台没有 DPAPI）',

  /*
   * core/update/*.rs + core/http.rs —— 引擎输出的是英文；下面这些
   * 中文正是本字典一贯采用的措辞，因此同一条消息在中文界面里
   * 始终只有一种呈现。
   */
  /* core/http.rs 的 `describeError` / `describe_io_error` 函数 */
  'DNS lookup failed': '域名解析失败',
  'connection timed out': '连接超时',
  'connection refused (is the proxy running?)': '连接被拒绝（代理未运行？）',
  'connection was reset': '连接被重置',
  'unknown error': '未知错误',
  'direct connection': '直连',
  'download was aborted': '下载已中止',
  'installer exceeds the size cap; download aborted': '安装包超过体积上限，已中止',
  /* core/update/http.rs —— 下载与 GitHub 接口相关错误 */
  'GitHub API rate limit reached; checks are paused and the last result kept. Add a GitHub token in update settings to raise the limit':
    'GitHub 接口访问受限，已暂停检查并保留上次结果；可在更新设置里填写 GitHub 令牌提高限额',
  /* core/update/manager.rs —— 更新管理器的错误文案 */
  'installer path is empty': '安装包路径为空',
  'installer path must be absolute': '安装包路径必须是绝对路径',
  'installer is missing or is not a file': '安装包不存在或不是文件',
  'installer is outside the managed download directory; execution refused':
    '安装包不在受控的下载目录内，已拒绝执行',
  'the installer is still downloading; wait for the download to finish before installing':
    '安装包还在下载中，请等下载完成后再安装',
  'failed to save settings': '设置保存失败',
  /*
   * 故意遮蔽 `PATTERNS` 里的 `^cannot reach the server: §1$`。
   * `zhError()` 先查 EXACT，所以这一条胜出，也会顺带翻译
   * `connection timed out`；而 PATTERNS 规则只会把原始
   * 英文粘进 §1（"无法连接服务器：connection timed out"）。
   * 删除这一行不会让该句子在审计中失败——只会让
   * 它的呈现不同，而且仍然是绿的。不要删它。
   */
  'cannot reach the server: connection timed out': '无法连接服务器：连接超时',
  /* core/update/token.rs —— 令牌存取错误 */
  'token cannot be empty (pass null to clear it)': '令牌不能为空（要清除请传 null）',
  'token may only contain letters, digits, underscores and hyphens':
    '令牌只能包含字母、数字、下划线与连字符',
  'secure storage on this machine is unavailable (the saved token cannot be decrypted)':
    '本机安全存储不可用（无法解密已保存的令牌）',
  'secure storage on this machine is unavailable; refusing to save the token in plain text':
    '本机安全存储不可用，已拒绝以明文保存令牌',
  'cannot decrypt': '无法解密',
  /* core/update/version.rs —— 版本号解析错误 */
  'the download address is not a valid URL': '下载地址不是合法 URL',
  'the download address must be https': '下载地址必须是 https'

  /* `[update]` 的诊断信息是 core/update/manager.rs 里的 `self.log(...)`
     行；它们的 logger 是空实现的 `|_| {}`，所以既不会到达
     WebView 也不会到达终端，因此不需要字典条目。 */
};

/*
 * 带插值消息的模式表。§n = 第 n 个捕获组。
 * 以 `${...}` 归一化为 §n 后，与引擎源码逐条比对。
 */
const PATTERNS = [
  // 共享层（M2）：启用/停用时「这次什么都没做」的两种正常状态，以及两条逐条失败原因。
  ['^already linked$', '该位置已是指向共享库的链接'],
  ['^already in the library$', '该位置就是共享库内的主副本，无需建立链接'],
  // 共享层（M3）：MCP 已经是目标状态时的两种「什么都没做」——两次点击之间
  // 状态没变，是成功，不是失败。
  ['^already open$', '该 agent 的配置里这条 MCP 服务器已经是开启状态'],
  ['^already closed$', '该 agent 的配置里这条 MCP 服务器已经是关闭状态'],
  // 共享层（M4）：规则/记忆共享里「这次本来就没事可做」的两种正常状态。
  // 它们不是失败 —— 用户连点两下也会看到它们，文案要读得出「已经对了」。
  ['^already shared$', '这条规则已经是共享状态，无需再次写入'],
  ['^no managed block$', '这个文件里没有由共享库管理的区块，无需改动'],
  ['^this path is no longer a link$', '该位置已不再是链接'],
  ['^this link points outside the library$', '该链接指向共享库之外'],
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
  ['^config file is corrupt, refusing to overwrite: §1$', '配置文件已损坏，拒绝覆盖写入：§1'],
  /*
   * Rust 引擎——由 scripts/tauri-i18n.js 审计。Rust 侧
   * 是按前缀（而不是按错误码）返回 `spawn_blocking` 失败的，所以
   * 这里按整句匹配。
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
  // 每个命令名一条，匹配 Rust 侧所格式化出的
  // 前缀，这样关卡才能证明每条命令都有译文。
  ['^distribute task failed: §1$', '分发任务失败：§1'],
  ['^baseline task failed: §1$', '基线任务失败：§1'],
  ['^apply task failed: §1$', '应用任务失败：§1'],
  ['^discard task failed: §1$', '放弃暂存失败：§1'],
  ['^audit task failed: §1$', '操作记录读取失败：§1'],
  ['^stats task failed: §1$', '本地统计读取失败：§1'],
  // M6 的 6 个耗时命令各有自己的前缀（命令名与 Rust 侧 `format!` 逐字对应），
  // 这样关卡能证明每条命令的失败都有译文，而不是都落到一句泛泛的「任务失败」。
  ['^list_library task failed: §1$', '读取共享库失败：§1'],
  ['^plan_converge task failed: §1$', '计算收敛方案失败：§1'],
  ['^apply_converge task failed: §1$', '应用收敛方案失败：§1'],
  ['^shared_doctor task failed: §1$', '共享健康检查失败：§1'],
  ['^apply_finding_fix task failed: §1$', '修复健康问题失败：§1'],
  ['^library_migrate task failed: §1$', '迁移共享库失败：§1'],
  ['^op[§1] (§2) is missing a "from"$', '第 §1 个操作（§2）缺少 "from" 字段'],
  ['^cannot read write source §1: §2$', '无法读取写入源 §1：§2'],
  ['^write failed for §1: §2$', '写入 §1 失败：§2'],
  // 来自 core/src/store.rs 的四句撤销捕获文案。
  //
  // 它们刻意不嵌套在 `^write failed for §1: §2$` 之下：那条
  // 模式只翻译前缀、把 §2 原样透传，于是携带"用户必须采取的
  // 动作"（"needs Administrator"、"was refused rather than performed"）的
  // 句子会以"写入 X 失败：<整段英文建议>"的形式
  // 到达中文读者面前——这正是本分支要消除的半翻译
  // 缺陷。所以每句都有自己的句干。
  ['^the file could not be read into the undo snapshot \(§1\)$', '文件无法读入撤销快照（§1）'],
  ['^the directory could not be recreated \(§1\)$', '目录无法重建（§1）'],
  ['^the directory is not writable, so the captured tree was left in place \(§1\)$', '目录不可写，已保留原有目录树（§1）'],
  ['^the file could not be read into the undo snapshot, so the write was refused rather than performed with no way back \(§1\)$', '文件无法读入撤销快照，因此已拒绝写入（而不是在没有退路的情况下继续写入）（§1）'],
  // 撤销快照的体积上限，由 store.rs 以人类可读的单位上报。那条
  // 建议（拆分写入 / 去掉不属于该资产的文件）本身就是
  // 句子的一部分，出于与上面相同的原因，它需要自己的句干。
  ['^undo snapshot limit reached: .*$', '撤销快照已达上限：所需数据超过单次写入可持有的额度（8 MB），因此未写入任何内容。请将该资产拆分为多次写入，或移除其中不必要的文件（依赖目录与构建产物会自动跳过）'],
  // 链接还原的三句（store.rs 的 `recreate_link`）。它们都陈述了
  // 用户必须去做什么，所以整句翻译，而不是把英文的 §2 留在中国文前缀后面。
  ['^the §1 link was not recreated \(§2\); the other files in this directory are back, but this one is missing — recreate it by hand$',
    '未能重建 §1 链接（§2）；该目录内其余文件均已还原，仅缺这一个——请手工重建该链接'],
  ['^the link was recreated as §1 instead of §2$', '重建出来的链接是 §1，而不是原来的 §2'],
  ['^the link target could not be read when this write started, so there is nothing to point at; recreate the link by hand$',
    '写入开始时读不到这条链接的目标，因此没有可指向的位置；请手工重建该链接'],
  ['^the snapshot recorded a managed copy where a link was expected$', '撤销快照在该位置记录的是一份副本，而不是链接'],

  /*
   * linkfs.rs —— 链接原语。这些串既会经由 `store.linkFailed` 的 detail 进界面，
   * 也会被 `library.rs` 的台账错误直接显示，所以全都整句翻译。
   */
  ['^link source is not a directory: §1$', '链接来源不是目录：§1'],
  ['^link source is not a file: §1$', '链接来源不是文件：§1'],
  ['^link target already exists: §1$', '链接目标已存在：§1'],
  ['^the parent directory could not be created \(§1\): §2$', '无法创建父目录（§1）：§2'],
  ['^no link could be created at §1 \(§2\)$', '无法在 §1 建立链接（§2）'],
  ['^links need absolute paths, got src §1 and dst §2$', '链接需要绝对路径，实际来源为 §1、目标为 §2'],
  ['^links need absolute paths, got dst §1$', '链接需要绝对路径，实际目标为 §1'],
  ['^delete failed for §1: §2$', '删除失败（§1）：§2'],
  ['^a hard link must be a file: §1$', '硬链接只能用于文件：§1'],
  ['^not a link: §1$', '不是链接：§1'],
  ['^a junction needs an absolute target$', '目录联接（junction）需要绝对目标路径'],
  ['^an unknown kind$', '未知的链接类型'],

  /*
   * library.rs —— 台账（links.json）。台账损坏是一条必须解释清楚的拒绝：
   * 它的修法是「以磁盘为准重建」，而不是「重试」。
   */
  ['^the link ledger could not be read \(§1\): §2$', '无法读取链接台账（§1）：§2'],
  ['^the link ledger is not valid JSON, so it was left untouched \(§1\): §2$', '链接台账不是合法 JSON，已原样保留未做改动（§1）：§2'],
  ['^the link ledger has no "entries" array, so it was left untouched \(§1\)$', '链接台账缺少 entries 数组，已原样保留未做改动（§1）'],
  ['^the link ledger entry #§1 is malformed, so it was left untouched \(§2\)$', '链接台账第 §1 条记录格式损坏，已原样保留未做改动（§2）'],
  ['^the library directory could not be created \(§1\): §2$', '无法创建库目录（§1）：§2'],
  ['^the link ledger could not be serialised: §1$', '无法序列化链接台账：§1'],
  ['^the link ledger could not be written \(§1\): §2$', '无法写入链接台账（§1）：§2'],

  /*
   * core/http.rs + core/update/*.rs —— 见上面的 EXACT 块：每条中文
   * 都是从 Rust 源码（core/update/{http,manager,token,
   * version}.rs）逐字抄来的，因此中文界面的呈现不变。
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

  /* `[update]` 的日志面模式已随上面的 EXACT 说明一并移除：
     同样是不可达的诊断信息，同样的理由。 */
];

/* 校验问题的字段标签：与消息并排渲染 */
/**
 * 错误码 → 中文。键与 `scripts/tauri-i18n.js` 中的引擎码表保持镜像，
 * 两者一旦走偏该脚本就会失败，因此渲染进程永远不需要
 * 靠匹配英文句子来决定显示什么。
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
  // Store::transaction（store.rs）产生的错误码。三种不同情形，
  // 过去会坍缩成同一句"写入失败"：
  //   snapshotTooLarge     —— 在任何写入发生之前就被拒绝；磁盘上什么都没变
  //   restoreIncomplete   —— 写入已执行，但撤销没能把它全部还原
  //   symlinkRestoreFailed —— 上述情形内部某一种具体的还原失败
  'store.snapshotTooLarge': '该资产超过单次写入的撤销上限（约 8 MB），未写入任何内容。请拆分后重试，或移除其中不必要的文件（依赖目录与构建产物已自动跳过）',
  'store.restoreIncomplete': '写入中断，已自动还原大部分内容，但部分路径未能还原，请检查下方列出的路径',
  'store.symlinkRestoreFailed': '文件已还原，但符号链接未能重建。请以管理员身份运行，或开启开发者模式后手工重建该链接',
  'store.restorePathFailed': '该路径未能还原',
  // 建链接失败（linkDir / linkFile）。单列一个 code 的理由：这条失败最常见的成因
  // 不是权限，而是目标位置已被占用（agent 自己写过一份副本）或来源已被删 ——
  // 前者要用户先停用/删掉那份副本，后者要先恢复库内主副本，两者都不该被说成
  // 「路径写不进去」。§1 由 `zhCode` 的第二个参数（路径）填充。
  'store.linkFailed': '未能建立链接（§1）。目标位置可能已被真实文件或目录占用，或来源已被删除；请先停用该位置上的副本后重试',
  // 共享层（M2）：把库内资产启用到 agent、或把 agent 侧的位置停用时的四种拒绝。
  // 每一种的修法都不一样，所以不能合成一句「操作失败」：
  //   notLinked       —— 那个位置是用户自己的真实目录，不是我们建的链接
  //   foreignLink     —— 是链接，但指向库外，删它会动到别人的目录
  //   unknownAsset    —— 名字/assetId 在扫描结果与库里都找不到
  //   unsupportedKind —— 本期只做 skill 共享
  'share.notLinked': '「§1」不是由共享库建立的链接，而是真实目录或文件；为安全起见不会自动删除',
  'share.foreignLink': '「§1」指向共享库之外，删除它会动到你自己的目录，已拒绝',
  'share.unknownAsset': '在共享库与扫描结果里都没有找到这个资产',
  'share.unsupportedKind': '当前版本不支持共享这一类资产',
  'rule.foreignEdit': '「§1」里的共享区块被你改过了；为避免覆盖你的编辑，本次没有写入。请先处理这处差异再同步',
  'rule.mdcLinkOnly': '「§1」是带 frontmatter 的 .mdc 规则文件，只能以链接方式共享，不能拼接区块',
  'rule.noTarget': '没有找到可共享的规则文件：这个 agent 或项目下没有声明对应的规则路径',
  'rule.targetUnreadable': '「§1」读不出来（权限或被占用）；为避免当成空文件覆盖你的内容，本次没有写入。请先解决占用再同步',
  'link.ledgerDrift': '链接台账与磁盘上的实际情况不一致；请让健康检查以磁盘为准重建台账',
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
  // 共享层（M3）：MCP 物化与漂移。四种拒绝各有一句单独的话，因为修法不同：
  //   shareUnsupported  —— 这份配置是 YAML，整份重写会丢注释，宁可不做
  //   noRecordInLibrary —— 库里没有这份规范形态，无从物化
  //   notMaterialisable —— 记录缺必要字段（既无 command 也无 url）
  //   drift             —— 是同一条服务器，但两边的参数不一样；没有自动改动
  'mcp.shareUnsupported': '这个 agent 的 MCP 配置是 YAML，整份重写会丢掉你写的注释；为免毁掉它们，这次操作没有执行',
  'mcp.noRecordInLibrary': '共享库里没有这条 MCP 服务器的规范形态，无法写入 agent（可能只有名字，或缺了必要字段）',
  'mcp.notMaterialisable': '这条 MCP 服务器缺少必要字段（命令行服务器要有 command，远程服务器要有 url），已拒绝写入',
  'mcp.drift': '共享库与这个 agent 的配置里，这条 MCP 服务器的参数不一致；没有自动改动任何一方',
  // 共享层（M6）：收敛、健康修复与库迁移的拒绝。
  // 每一种修法都不同，不能合并成一句「操作失败」：
  //   converge.diverged        —— 同名副本内容不一致，收敛会覆盖其中一方的改动
  //   converge.notConvergeable —— 组里没有可动的副本
  //   doctor.notFixable        —— 这类问题本来就没有单一修法（如硬链接）
  //   doctor.sourceMissing     —— 重新链接时源位置（库内主副本）已不存在
  //   doctor.contentChanged    —— 被顶掉的副本与库内不一致，自动重建会覆盖改动
  //   doctor.unsupportedFix    —— 这个 code 没有对应的修复动作
  'converge.diverged': '这组同名副本的内容不一致，收敛会覆盖其中一方的改动，请先手工对齐',
  'converge.notConvergeable': '这组里没有可以收敛的副本',
  'doctor.notFixable': '这类问题没有可自动执行的修复动作，需要手工处理',
  'doctor.sourceMissing': '重新链接时源位置已不存在（库内主副本可能被删除了），请先恢复源文件',
  'doctor.contentChanged': '被替换的副本内容与库内主副本不一致，自动重建会覆盖你的改动，本次没有写入',
  'doctor.unsupportedFix': '当前版本没有这种问题的修复动作',
  // 库根迁移（§9.3）。目标不可用要在搬任何东西之前拒绝；失败则保留
  // `migrating` 标记，重跑从断点继续 —— 两种结局的后续动作不同，所以分两条。
  'library.migrateTarget': '迁移目标不可用：需要绝对路径，且不能与当前库根互相嵌套；若有另一条未完成的迁移也不会被覆盖',
  'library.migrateFailed': '共享库迁移中断了：已搬完的部分不会重复搬，修复问题后重跑同一目标即可从断点继续',
  // 删库内资产时主副本进回收站没成功（跨卷拷贝不完整）：整次删除放弃，磁盘原样。
  'library.trashFailed': '移入回收站失败：内容没有完整拷进回收站，所以原位置一个字没动',
  // 跨进程写锁（flock.rs）：另一个进程（桌面端 / agentkeeper-mcp）正握着共享状态。
  'store.busy': '另一个进程正在修改共享数据，请稍等几秒再试一次',
  'store.lockFailed': '共享数据锁不可用（权限或只读介质），本次写入没有执行',
  'project.duplicate': '项目已注册',
  'project.notDirectory': '所选路径不是文件夹',
  'project.directoryNotFound': '文件夹不存在',
  'project.dirRequired': '缺少目录参数',
  'ipc.pathRequired': '缺少路径参数',
  'ipc.pathNotFound': '路径不存在',
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
 * 为用户渲染引擎错误。
 *
 * `zhCode` 是首选路径：IPC 结果带有 `code` + `params`。`zhText` 仍然
 * 作为还没有错误码的文本的兜底（审计日志行、MCP
 * 载荷、git clone 的 stderr）以及既有字符串的兜底。
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
 * 对引擎产出的任何内容尽力给出中文：先看错误码，
 * 再查句子字典，最后退回原文。
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
 * 审计白名单：永远不会出现在 UI 上的引擎字符串（内部的
 * 审计日志标签、协议元数据）。保持最小——审计测试会对
 * 这里没列出的任何内容失败，所以新出现的面向用户的英文会让构建失败。
 */
const INTERNAL = new Set([
  'use strict'
]);

function patternToRegExp(src) {
  // 转义正则元字符，但保留 ^ $ 作为锚点、§ 作为占位符。
  const escaped = src.replace(/[.*+?'"{}()|[\]\\]/g, '\\$&');
  return new RegExp(escaped.replace(/§(\d)/g, '(.+)'), 's');
}

const compiled = PATTERNS.map(([p, zh]) => ({ re: patternToRegExp(p), zh }));

/**
 * 当前界面是否是中文？
 *
 * 引擎输出的是英文句子（它的 `zh` 字典负责把它们变成
 * 中文），所以这些翻译函数只在界面语言为 zh 时才运行。
 * 它们必须只在 zh 界面语言下运行：无条件运行会让
 * 英文的 "By agent" 表格里的 "Shared (~/.agents)" 变成 共享技能目录（~/.agents）。
 * 在浏览器之外（i18n 关卡和 Node 测试
 * 需要引入本文件）没有界面语言；那里 zh 是安全的默认值，
 * 因为关卡审计的就是这个字典本身。
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

/** 翻译一条引擎字符串。已经是中文的内容原样透传。 */
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

/** 翻译校验问题的字段名以便展示。 */
function zhField(f) {
  if (!isZh()) return f;
  return (f && Object.prototype.hasOwnProperty.call(FIELD, f)) ? FIELD[f] : f;
}

/** 审计辅助函数：这条（已归一化的）引擎字符串是否被覆盖？ */
function isCovered(s) {
  if (Object.prototype.hasOwnProperty.call(EXACT, s)) return true;
  if (INTERNAL.has(s)) return true;
  return compiled.some(({ re }) => re.test(s));
}

  const api = { EXACT, PATTERNS, FIELD, INTERNAL, CODE_ZH, zhText, zhField, zhCode, zhError, isCovered, patternToRegExp };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  if (typeof window !== 'undefined') window.__akEngineZh = api;
})();
