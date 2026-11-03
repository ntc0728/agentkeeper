'use strict';
/**
 * 共享资产页（M6 · 界面）。
 *
 * 这一页是 8 个共享命令的渲染层（契约见 `.dsh-ops/M6-CONTRACT.md`）：
 * 数据只来自 `window.ak.listLibrary()`（矩阵、代理列、库根、链接能力横幅的唯一
 * 数据源），写入只走 `enableAsset` / `disableAsset`；收敛与健康检查各自懒加载
 * （`planConverge` / `sharedDoctor`），进入页面时不跑耗时命令。
 *
 * 结构：copyOnly 横幅 → tab 栏（Skills / MCP / 规则 / 记忆 / 健康）→ 矩阵或健康
 * 面板。矩阵的单元格是真正的 <button>（Tab 可达、空格/回车触发），列头是整列的
 * 批量入口。所有可见文案走 t()，路径和 code 原样显示。
 *
 * 本文件用 IIFE 包住，只向外暴露 window.__akShared 上的 viewShared：渲染层是
 * 传统 <script>，顶层再声明一个同名标识符就会让整扇窗白屏。
 */
(function () {
  /* ---------------------------------------------------------------- */
  /* 模块状态                                                          */
  /* ---------------------------------------------------------------- */

  const S = {
    // listLibrary 的结果（含 root/cap/agents/skills/mcp/rules/memory）。
    data: null,
    loading: false,
    error: null,
    // 当前挂载的视图根节点；异步回来时要往「最新的」根上重画。
    root: null,
    tab: 'skills',
    // Skills 搜索框（本地过滤，不重新拉数据）。
    q: '',
    // 健康 tab：懒加载，只有第一次点进去才调 sharedDoctor()。
    doctor: null,
    doctorLoading: false,
    doctorError: null,
    // 正在修那一条 finding（key = code|path）；用于禁用修复按钮。
    fixing: '',
    // 「收敛重复」正在探测。
    convergeBusy: false,
    // 首次启用提示（§9.4）：bootstrap 说 `shared.enabled` 且第一次探测发现
    // 可收敛的组时，在页顶挂一条横幅。**默认不收敛** —— 点进向导才会写盘。
    hintGroups: 0,
    hintDismissed: false,
    // 探测只在每次打开共享页时跑一次（bootstrap 刻意不把候选数算进启动路径）。
    hintProbed: false,
    // 库位置迁移正在跑（按钮禁用，避免连点两次）。
    migrateBusy: false
  };

  const TABS = ['skills', 'mcp', 'rules', 'memory', 'health'];
  const TAB_KEY = {
    skills: 'shared.tabSkills',
    mcp: 'shared.tabMcp',
    rules: 'shared.tabRules',
    memory: 'shared.tabMemory',
    health: 'shared.tabHealth'
  };
  const TAB_FIELD = { skills: 'skills', mcp: 'mcp', rules: 'rules', memory: 'memory' };
  const KIND_BY_TAB = { skills: 'skill', mcp: 'mcp', rules: 'rule', memory: 'memory' };

  // 数据形状里的单元格状态（契约 §2.1）。顺序同时决定图例顺序。
  const STATE_ORDER = ['on', 'off', 'copy', 'foreign', 'drift', 'unsupported', 'error'];
  const STATE_SYMBOL = ['●', '○', '◐', '↗', '⚠', '—', '!'];
  const STATE_KEY = {
    on: 'matrix.stateOn',
    off: 'matrix.stateOff',
    copy: 'matrix.stateCopy',
    foreign: 'matrix.stateForeign',
    drift: 'matrix.stateDrift',
    unsupported: 'matrix.stateUnsupported',
    error: 'matrix.stateError'
  };
  // unsupported 的 reason 是稳定枚举（契约 §2.1），翻译在 i18n 里；未知值退回通用文案。
  const REASON_KEY = {
    noSkillsDir: 'matrix.reasonNoSkillsDir',
    noRulePath: 'matrix.reasonNoRulePath',
    noMcpConfig: 'matrix.reasonNoMcpConfig',
    noStyle: 'matrix.reasonNoStyle',
    noSource: 'matrix.reasonNoSource'
  };

  // 健康检查的 severity / code 顺序（与 docs 的 finding 表一致）。
  const SEV_ORDER = ['critical', 'warn', 'info'];
  const SEV_KEY = { critical: 'doctor.critical', warn: 'doctor.warn', info: 'doctor.info' };
  const CODE_ORDER = [
    'link.dangling',
    'link.unmanaged',
    'link.hijacked',
    'link.fragile',
    'asset.duplicated',
    'mcp.drift',
    'link.ledgerDrift'
  ];
  const CODE_KEY = {
    'link.dangling': 'doctor.dangling',
    'link.unmanaged': 'doctor.unmanaged',
    'link.hijacked': 'doctor.hijacked',
    'link.fragile': 'doctor.fragile',
    'asset.duplicated': 'doctor.duplicated',
    'mcp.drift': 'doctor.drift',
    'link.ledgerDrift': 'doctor.ledgerDrift'
  };
  const CHOICE_KEY = {
    library: 'doctor.choiceLibrary',
    agent: 'doctor.choiceAgent',
    independent: 'doctor.choiceIndependent'
  };

  // 收敛分组的裁决与文件差异状态。
  const VERDICT_KEY = {
    identical: 'converge.identical',
    diverged: 'converge.diverged',
    single: 'converge.single'
  };
  const DIFF_KEY = {
    different: 'converge.diffDifferent',
    'only-a': 'converge.diffOnlyA',
    'only-b': 'converge.diffOnlyB',
    kind: 'converge.diffKind'
  };

  /* ---------------------------------------------------------------- */
  /* 小工具                                                            */
  /* ---------------------------------------------------------------- */

  /** 统一从 window.ak 调命令；桥接还没接线时给出可识别、可本地化的错误。 */
  async function call(name) {
    const ak = window.ak || {};
    if (typeof ak[name] !== 'function') {
      const e = new Error('window.ak.' + name + '() is missing');
      e.akBridgeMissing = true;
      e.akExpected = true;
      throw e;
    }
    return ak[name].apply(ak, Array.prototype.slice.call(arguments, 1));
  }

  function isBridgeMissing(e) {
    return !!(e && e.akBridgeMissing);
  }

  /**
   * 把一次失败写成一句话：先查稳定 code 的中文，再退回引擎句子，最后退回兜底键。
   * 共享页的 code 不在 reportWriteFailure 的 store.* 白名单里，所以自己收口。
   */
  function failText(r, key) {
    const code = r && (r.code || r.errorCode);
    const byCode = code ? zhCode(code) : null;
    if (byCode) return byCode;
    const raw = r && (r.error || r.code || '');
    const msg = zhError(typeof raw === 'string' ? raw : (raw && raw.message) || '');
    if (msg) return t(key, { msg: msg });
    return t(key, { msg: code || t('shared.unknownError') });
  }

  function joinPath(base, name) {
    const a = String(base || '').replace(/[\\/]+$/, '');
    const b = String(name || '').replace(/^[\\/]+/, '');
    if (!a) return b;
    return a + '\\' + b;
  }

  function labelOf(row) {
    return String((row && (row.label || row.name || row.slug)) || '');
  }

  function slugOf(row) {
    return String((row && (row.slug || row.name)) || '');
  }

  function cellOf(row, agentId) {
    const cells = (row && row.cells) || {};
    return cells[agentId] || null;
  }

  function installedAgents(d) {
    const list = (d && Array.isArray(d.agents)) ? d.agents : [];
    return list.filter((a) => a && a.installed);
  }

  function stateSymbol(st) {
    const i = STATE_ORDER.indexOf(st);
    return i < 0 ? '—' : STATE_SYMBOL[i];
  }

  function sevChipClass(sev) {
    if (sev === 'critical') return 'chip err';
    if (sev === 'warn') return 'chip warn';
    return 'chip';
  }

  /**
   * 一个单元格的完整描述（state + 原始 cell + 失效原因）。
   *
   * 契约 §2.1：rules/memory 的行按**目标文件名 slug** 分组，固定文件名声明
   * （AGENTS.md / CLAUDE.md …）只出现在同名 slug 行上，所以多数 agent 在多数
   * 行上根本没有 `cells[agent]`。缺席不是「目标为空、可以点」——它意味着这个
   * agent 对本行没有目标文件、库里也没有可用的主副本：没有可共享的内容。
   * 这种格子渲染成 unsupported(noSource)，绝不能落进 enableAsset（那会因为没有
   * `file` 被服务端以 `rule.noTarget` 拒绝，也就是真实运行时冒烟抓到的那条）。
   */
  function cellInfo(tab, row, agentId) {
    const cell = cellOf(row, agentId);
    if (cell) {
      const state = STATE_KEY[cell.state] ? cell.state : 'unsupported';
      return { cell: cell, state: state, reason: cell.reason || '' };
    }
    // 外部 skill 行：缺席的格子没有「导入」入口，同样只能提示。
    if (row && row.external) return { cell: null, state: 'unsupported', reason: '' };
    // rules/memory 的缺席格：没有来源（文件不存在、库里也没有主副本）。
    if (tab === 'rules' || tab === 'memory') return { cell: null, state: 'unsupported', reason: 'noSource' };
    // skills/mcp 的缺席格按「目标为空、可启用」处理（服务端能按 name 解析）。
    return { cell: null, state: 'off', reason: '' };
  }

  function unsupportedText(cell, reasonOverride) {
    const reason = reasonOverride || (cell && cell.reason);
    const key = REASON_KEY[reason];
    return key ? t(key) : t('matrix.stateUnsupported');
  }

  /** 单元格的可访问名字 / 悬停说明：资产 · agent: 状态（状态里带原因）。 */
  function cellStateText(st, cell, reason) {
    if (st === 'unsupported') return unsupportedText(cell, reason);
    if (st === 'error') return t('matrix.stateError', { msg: zhError((cell && cell.reason) || '') });
    if (st === 'drift' && cell && cell.drift) {
      return t('matrix.stateDrift') + ' · ' + driftText(cell.drift);
    }
    return t(STATE_KEY[st] || 'matrix.stateUnsupported');
  }

  /** mcp.drift 的 detail：field / libraryValue / agentValue。纯数据拼接，不走 i18n。 */
  function driftText(drift) {
    const field = zhField(drift && drift.field) || '';
    return field + ': ' + valueText(drift && drift.libraryValue) + ' → ' + valueText(drift && drift.agentValue);
  }

  function valueText(v) {
    if (v === null || v === undefined) return '—';
    if (typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return String(v);
    try {
      const s = JSON.stringify(v);
      return String(s).length > 120 ? String(s).slice(0, 117) + '…' : String(s);
    } catch (_e) {
      return String(v);
    }
  }

  /* ---------------------------------------------------------------- */
  /* 数据加载                                                          */
  /* ---------------------------------------------------------------- */

  /** 首次进入：listLibrary 拿全量数据；失败给可重试错误面板。 */
  function ensureData() {
    if (S.data || S.loading) return;
    S.loading = true;
    S.error = null;
    paint();
    call('listLibrary').then((r) => {
      if (r && r.ok === false) S.error = failText(r, 'shared.loadFailed');
      else if (r && typeof r === 'object') S.data = r;
      else S.error = t('shared.loadFailed', { msg: t('shared.unknownError') });
    }).catch((e) => {
      if (isBridgeMissing(e)) S.error = t('shared.bridgeMissing');
      else {
        reportUiError('error', e);
        S.error = t('shared.loadFailed', { msg: zhError(e) });
      }
    }).finally(() => {
      S.loading = false;
      paint();
    });
  }

  /** 写入之后重新拉一次 listLibrary，只重画本页，不整页刷新。 */
  async function reload() {
    try {
      const r = await call('listLibrary');
      if (r && r.ok === false) {
        toast(failText(r, 'shared.loadFailed'), 'err');
        return;
      }
      S.data = r;
      S.error = null;
    } catch (e) {
      if (isBridgeMissing(e)) {
        toast(t('shared.bridgeMissing'), 'err');
        return;
      }
      reportUiError('error', e);
      toast(t('shared.loadFailed', { msg: zhError(e) }), 'err');
      return;
    }
    paint();
  }

  function retryLoad() {
    S.error = null;
    S.data = null;
    S.hintProbed = false;
    paint();
    ensureData();
  }

  /**
   * 首次启用提示的探测（§9.4）。
   *
   * `bootstrap` 只给 `shared.enabled`，候选数必须在这里现算：把 200 个目录的
   * 遍历塞进窗口绘制之前那条同步命令里，等于给每次启动加一次全盘扫描。
   * 探测只读（`planConverge` 零写入），失败静默 —— 一条「提示没出来」不该变成
   * 一个报错弹窗。
   */
  function probeHint() {
    if (S.hintProbed || S.hintDismissed) return;
    const enabled = window.__akBoot && window.__akBoot.shared
      ? window.__akBoot.shared.enabled !== false
      : true;
    if (!enabled) return;
    S.hintProbed = true;
    call('planConverge').then((r) => {
      const n = r && r.ok !== false && Number(r.actionable)
        ? Number(r.actionable)
        : 0;
      if (n <= 0) return;
      S.hintGroups = n;
      paint();
    }).catch(() => { /* 提示是锦上添花：探测失败就当好，不打扰用户 */ });
  }

  function dismissHint() {
    S.hintDismissed = true;
    S.hintGroups = 0;
    paint();
  }

  /** 健康检查是耗时命令：只在点进健康 tab 时跑，修完再跑一次。 */
  async function loadDoctor() {
    if (S.doctorLoading) return;
    S.doctorLoading = true;
    S.doctorError = null;
    paint();
    try {
      const r = await call('sharedDoctor');
      if (r && r.ok === false) S.doctorError = failText(r, 'doctor.error');
      else S.doctor = r;
    } catch (e) {
      if (isBridgeMissing(e)) S.doctorError = t('shared.bridgeMissing');
      else {
        reportUiError('error', e);
        S.doctorError = t('doctor.error', { msg: zhError(e) });
      }
    } finally {
      S.doctorLoading = false;
      paint();
    }
  }

  /* ---------------------------------------------------------------- */
  /* 单元格参数                                                        */
  /* ---------------------------------------------------------------- */

  /**
   * 启用/停用一个位置时发给 `enableAsset` / `disableAsset` 的载荷。
   *
   * 契约 §2.1 规定：rules/memory 即使 state=off，`cells[agent].path` 也必须是该
   * agent 声明的第一个目标文件 —— 服务端只认 `file`，不读 `name`；mcp 同理带
   * `file`（候选配置文件），且 mcp 的 `name` 必须用库内记录名 `row.name`（不是
   * 目录 slug）。skills 也只用 name（服务端按扫描结果/库内主副本解析）。
   */
  function togglePayload(kind, row, agentId, cell, enabling) {
    // rules/memory 的 name 是规则文件 slug；skills/mcp 用库内记录名（可能带空格与大小写）。
    const name = (kind === 'rule' || kind === 'memory')
      ? (row.slug || row.name || labelOf(row))
      : (row.name || labelOf(row));
    const payload = {
      name: name,
      kind: kind,
      agentId: agentId,
      scope: 'global'
    };
    if (kind === 'mcp') {
      const file = (cell && cell.file) || '';
      if (file) payload.file = file;
    } else if (kind === 'rule' || kind === 'memory') {
      const file = (cell && cell.path) || '';
      if (file) payload.file = file;
      const mode = (cell && cell.mode) || row.mode;
      // 只有启用需要选写作方式；停用按盘上实际形态收尾。
      if (enabling && mode) payload.mode = mode;
    }
    return payload;
  }

  /** 这个位置写入后会落在哪个路径（写前确认要把它原样列出来）。 */
  function targetPath(tab, row, agent, cell) {
    if (cell && cell.path) return cell.path;
    if (tab === 'mcp') {
      if (cell && cell.file) return cell.file;
      const files = (agent && agent.mcpFiles) || [];
      return files.length ? files[0] : '';
    }
    if (tab === 'skills') {
      return agent && agent.skillsDir ? joinPath(agent.skillsDir, slugOf(row)) : '';
    }
    const dirs = tab === 'rules' ? ((agent && agent.ruleDirs) || []) : ((agent && agent.memoryDirs) || []);
    if (!dirs.length) return '';
    const fileName = row.label || (slugOf(row) + '.md');
    return joinPath(dirs[0], fileName);
  }

  /* ---------------------------------------------------------------- */
  /* 矩阵                                                              */
  /* ---------------------------------------------------------------- */

  function filteredRows(tab) {
    const d = S.data;
    if (!d) return [];
    const list = Array.isArray(d[TAB_FIELD[tab]]) ? d[TAB_FIELD[tab]] : [];
    if (tab !== 'skills' || !S.q) return list;
    const q = S.q.toLowerCase();
    return list.filter((row) => (labelOf(row) + ' ' + slugOf(row)).toLowerCase().indexOf(q) >= 0);
  }

  function libraryEmpty(d) {
    return ['skills', 'mcp', 'rules', 'memory'].every((k) => !(d && Array.isArray(d[k]) && d[k].length));
  }

  function tabCount(tab) {
    if (tab === 'health') {
      const c = S.doctor && S.doctor.counts;
      return c && Number.isFinite(Number(c.total)) ? Number(c.total) : null;
    }
    const d = S.data;
    const list = d && Array.isArray(d[TAB_FIELD[tab]]) ? d[TAB_FIELD[tab]] : [];
    return list.length;
  }

  function reloadButton() {
    return el('button', { class: 'btn sm', type: 'button', onclick: () => void reload() }, t('shared.refresh'));
  }

  function resyncButton() {
    return el('button', { class: 'btn sm', type: 'button', onclick: () => void doResync() }, t('shared.resync'));
  }

  /** copyOnly 横幅上的「重新扫描」：先跑既有的 scan，再刷新本页数据。 */
  async function doResync() {
    try {
      if (typeof rescan === 'function') await rescan(true);
      await reload();
    } catch (e) {
      reportUiError('error', e);
      toast(t('shared.loadFailed', { msg: zhError(e) }), 'err');
    }
  }

  function skillsTools() {
    const input = el('input', {
      id: 'shared-q',
      type: 'search',
      class: 'mx-search',
      value: S.q,
      autocomplete: 'off',
      'aria-label': t('shared.searchLabel'),
      placeholder: t('shared.searchPlaceholder'),
      oninput: (e) => {
        S.q = String(e.target.value || '');
        paint('shared-q');
      }
    });
    const converge = el('button', {
      class: 'btn sm' + (S.convergeBusy ? ' loading' : ''),
      type: 'button',
      disabled: S.convergeBusy,
      onclick: () => void openConverge()
    }, t('converge.button'));
    return el('div', { class: 'shared-tools' }, input, converge);
  }

  /** 列头的整列批量入口：只弹一次 confirmWrite，然后逐个位置串行写。 */
  function agentHeadButton(tab, agent) {
    const rows = filteredRows(tab);
    const states = rows.map((row) => cellInfo(tab, row, agent.id).state).filter((st) => st === 'on' || st === 'off');
    const enabling = states.indexOf('off') >= 0 || !states.length;
    const label = enabling ? t('matrix.enableAll', { agent: agent.label }) : t('matrix.disableAll', { agent: agent.label });
    return el('button', {
      class: 'mx-col',
      type: 'button',
      'aria-label': label,
      title: label,
      'data-focus-key': 'shared-col|' + tab + '|' + agent.id,
      onclick: () => void batchColumn(tab, agent, rows)
    }, agent.label);
  }

  function metaChips(tab, row, external) {
    const chips = [];
    if (tab === 'skills') {
      if (row.size) chips.push(el('span', { class: 'muted fs-11' }, bytes(row.size)));
      if (external) chips.push(el('span', { class: 'chip ghost' }, t('shared.external')));
    } else if (tab === 'rules' || tab === 'memory') {
      // 行级 mode 只是提示；契约 §2.1 之后行可能没有 mode（模式在 cell 上），
      // 所以只在它确实是已知取值时才显示，避免把「未知」画成「合并区块」。
      if (row.mode === 'compose' || row.mode === 'link') {
        chips.push(el('span', { class: 'chip' }, row.mode === 'link' ? t('shared.modeLink') : t('shared.modeCompose')));
      }
    }
    return chips.length ? el('span', { class: 'mx-chips' }, ...chips) : null;
  }

  function importButton(row) {
    return el('button', {
      class: 'btn sm mt-6',
      type: 'button',
      'data-focus-key': 'shared-import|' + slugOf(row),
      onclick: () => void importExternal(row)
    }, t('shared.importToLibrary'));
  }

  /**
   * rules/memory 的 foreign 是否允许「收进库」。
   *
   * 只有两种情形是安全的：
   *   · 库里**已经有本行主副本**（`row.libraryFile`）—— compose 有内容可拼；
   *   · 该行是 **link 模式** —— link 会先把目标内容写进库内主副本，再从目标建链。
   * 库内无主副本 + compose 会命中 M4 的 keeper-absent 分支（区块内容 = 目标现文），
   * 把整份内容既留在区块外又复制进区块（重复），而且不会在库里创建主副本 ——
   * 这条路径没有验收夹具覆盖，UI 必须挡住。
   */
  function canAdoptForeign(tab, row, cell) {
    if (tab !== 'rules' && tab !== 'memory') return false;
    if (row && row.libraryFile) return true;
    return !!(cell && cell.mode === 'link');
  }

  function cellButton(tab, kind, row, agent) {
    const info = cellInfo(tab, row, agent.id);
    const cell = info.cell;
    const st = info.state;
    // rules/memory 的 foreign = 盘上已有同名文件/外链，但还没纳管 —— 这正是 M4
    // 「把已存在的规则/记忆文件收进库」的入口；但只有「库内已有主副本」或
    // 「link 模式」时才安全（见 canAdoptForeign）。其余 foreign 只读并说明原因。
    // skills/mcp 的 foreign 仍然只是信息（那两种没有「采纳现有文件」这条路径）。
    const foreignRule = st === 'foreign' && (tab === 'rules' || tab === 'memory');
    const adoptable = foreignRule && canAdoptForeign(tab, row, cell);
    const blockedForeign = foreignRule && !adoptable;
    const togglable = st === 'on' || st === 'off';
    const actionable = togglable || adoptable;
    const text = (blockedForeign ? t('matrix.foreignNoSource') : cellStateText(st, cell, info.reason))
      + (adoptable ? ' · ' + t('matrix.enable') : '');
    const label = labelOf(row) + ' · ' + String(agent.label || agent.id) + ': ' + text;
    return el('button', {
      class: 'mx-cell',
      type: 'button',
      'data-state': st,
      'aria-pressed': st === 'on' ? 'true' : 'false',
      'aria-disabled': actionable ? 'false' : 'true',
      'aria-label': label,
      title: label,
      'data-focus-key': 'shared-cell|' + tab + '|' + slugOf(row) + '|' + agent.id,
      onclick: () => void onCell(tab, kind, row, agent, cell, st, info.reason)
    }, stateSymbol(st));
  }

  function onCell(tab, kind, row, agent, cell, st, reason) {
    if (st === 'on' || st === 'off') {
      void toggleCell(tab, kind, row, agent, cell, st);
      return;
    }
    if (st === 'foreign' && (tab === 'rules' || tab === 'memory')) {
      if (canAdoptForeign(tab, row, cell)) void adoptForeign(kind, row, agent, cell);
      else toast(t('matrix.foreignNoSource'), 'warn');
      return;
    }
    // 只读状态不是死按钮：点它给出原因（写盘入口只在 on/off 与 rules/memory 的
    // 可采纳 foreign 上）。
    toast(cellStateText(st, cell, reason), 'warn');
  }

  /**
   * 把一份已存在的规则/记忆文件收进库（M4 的核心流程）。
   *
   * 走 `enableAsset`：`mode` 取盘上该文件的既有形态（`.mdc` 等只支持 link，其余
   * compose）。compose 会把原内容圈成受管区块（一个字节都不改写）；link 会先把
   * 内容复制进库再替换成链接。两种情况都**不丢内容**。如果盘上区块被外部改过，
   * 服务端会以 `rule.foreignEdit` 拒绝（零写入），这里用 failText 原样展示。
   */
  async function adoptForeign(kind, row, agent, cell) {
    const assetName = labelOf(row);
    const file = (cell && cell.path) || '';
    if (!file) {
      toast(cellStateText('foreign', cell), 'warn');
      return;
    }
    const mode = (cell && cell.mode) || 'compose';
    const ok = await confirmWrite({
      title: t('matrix.confirmEnableTitle', { asset: assetName, agent: agent.label }),
      action: t('matrix.enable'),
      targets: [file],
      detail: t('matrix.enableDetail')
    });
    if (!ok) return;
    try {
      const r = await call('enableAsset', {
        name: slugOf(row),
        kind: kind,
        agentId: agent.id,
        scope: 'global',
        file: file,
        mode: mode
      });
      if (r && r.ok === false) {
        toast(failText(r, 'matrix.cellFailed'), 'err');
        return;
      }
      toast(t('matrix.enabled', { asset: assetName, agent: agent.label }), 'ok');
      S.doctor = null;
      await reload();
    } catch (e) {
      if (isBridgeMissing(e)) {
        toast(t('shared.bridgeMissing'), 'err');
        return;
      }
      reportUiError('error', e);
      toast(t('matrix.cellFailed', { msg: zhError(e) }), 'err');
    }
  }

  async function toggleCell(tab, kind, row, agent, cell, st) {
    const enabling = st === 'off';
    const assetName = labelOf(row);
    const target = targetPath(tab, row, agent, cell);
    const ok = await confirmWrite({
      title: enabling
        ? t('matrix.confirmEnableTitle', { asset: assetName, agent: agent.label })
        : t('matrix.confirmDisableTitle', { asset: assetName, agent: agent.label }),
      action: enabling ? t('matrix.enable') : t('matrix.disable'),
      targets: target ? [target] : [],
      detail: enabling ? t('matrix.enableDetail') : t('matrix.disableDetail'),
      danger: !enabling
    });
    if (!ok) return;
    try {
      const payload = togglePayload(kind, row, agent.id, cell, enabling);
      const r = enabling ? await call('enableAsset', payload) : await call('disableAsset', payload);
      if (r && r.ok === false) {
        toast(failText(r, 'matrix.cellFailed'), 'err');
        return;
      }
      toast(enabling
        ? t('matrix.enabled', { asset: assetName, agent: agent.label })
        : t('matrix.disabled', { asset: assetName, agent: agent.label }), 'ok');
      S.doctor = null;
      await reload();
    } catch (e) {
      if (isBridgeMissing(e)) {
        toast(t('shared.bridgeMissing'), 'err');
        return;
      }
      reportUiError('error', e);
      toast(t('matrix.cellFailed', { msg: zhError(e) }), 'err');
    }
  }

  async function batchColumn(tab, agent, rows) {
    const kind = KIND_BY_TAB[tab];
    const writable = rows.map((row) => {
      const info = cellInfo(tab, row, agent.id);
      return { row: row, cell: info.cell, st: info.state };
    }).filter((x) => x.st === 'on' || x.st === 'off');
    if (!writable.length) {
      toast(t('matrix.batchNone', { agent: agent.label }), 'warn');
      return;
    }
    const enabling = writable.some((x) => x.st === 'off');
    const targets = writable.map((x) => targetPath(tab, x.row, agent, x.cell)).filter(Boolean);
    const ok = await confirmWrite({
      title: t('matrix.batchTitle', { agent: agent.label }),
      action: enabling ? t('matrix.enable') : t('matrix.disable'),
      targets: targets,
      detail: t('matrix.batchDetail'),
      danger: !enabling
    });
    if (!ok) return;
    let done = 0;
    let failed = 0;
    // 串行写：一次一个事务，失败不阻断后面的位置（与 apply_converge 同口径）。
    for (const x of writable) {
      try {
        const payload = togglePayload(kind, x.row, agent.id, x.cell, enabling);
        const r = enabling ? await call('enableAsset', payload) : await call('disableAsset', payload);
        if (r && r.ok === false) failed++;
        else done++;
      } catch (e) {
        reportUiError('error', e);
        failed++;
      }
    }
    const parts = [t('matrix.batchOk', { n: done })];
    if (failed) parts.push(t('matrix.batchFailed', { n: failed }));
    toast(parts.join(' · '), failed ? 'warn' : 'ok');
    S.doctor = null;
    await reload();
  }

  async function importExternal(row) {
    const ext = Array.isArray(row.external) && row.external.length ? row.external[0] : null;
    if (!ext || !ext.assetId) {
      toast(t('shared.importNoAsset'), 'warn');
      return;
    }
    const ok = await confirmWrite({
      title: t('shared.importTitle', { name: labelOf(row) }),
      action: t('shared.importToLibrary'),
      targets: ext.path ? [ext.path] : [],
      detail: t('shared.importDetail')
    });
    if (!ok) return;
    try {
      const r = await call('enableAsset', {
        name: row.name || labelOf(row),
        kind: 'skill',
        agentId: ext.agentId,
        scope: ext.scope || 'global',
        assetId: ext.assetId
      });
      if (r && r.ok === false) {
        toast(failText(r, 'matrix.cellFailed'), 'err');
        return;
      }
      toast(t('shared.imported', { name: labelOf(row) }), 'ok');
      S.doctor = null;
      await reload();
    } catch (e) {
      if (isBridgeMissing(e)) {
        toast(t('shared.bridgeMissing'), 'err');
        return;
      }
      reportUiError('error', e);
      toast(t('matrix.cellFailed', { msg: zhError(e) }), 'err');
    }
  }

  function legendBar() {
    const entries = [
      ['on', 'matrix.legendOn'],
      ['off', 'matrix.legendOff'],
      ['copy', 'matrix.legendCopy'],
      ['drift', 'matrix.legendWarn'],
      ['unsupported', 'matrix.legendNone']
    ];
    return el('div', { class: 'mx-legend' }, ...entries.map(([st, key]) =>
      el('span', { class: 'mx-legend-item', 'data-state': st },
        el('span', { class: 'mx-legend-dot', 'aria-hidden': 'true' }, stateSymbol(st)),
        t(key))));
  }

  function rowNode(tab, kind, row, agents) {
    const external = tab === 'skills' && (row.dir === null || row.dir === undefined) && Array.isArray(row.external) && row.external.length > 0;
    const nameCell = el('td', { class: 'mx-name' },
      el('div', { class: 'mx-name-line' },
        el('span', { class: 'mx-name-text' }, labelOf(row)),
        metaChips(tab, row, external)),
      external ? importButton(row) : null);
    // 数据行本身不可点（控件在单元格里）；表格挂 tbl static，行游标保持默认。
    return el('tr', { class: 'mx-row' },
      nameCell,
      ...agents.map((agent) => el('td', { class: 'mx-c' }, cellButton(tab, kind, row, agent))));
  }

  function matrixTable(tab, kind, agents, rows) {
    return el('table', { class: 'tbl matrix static mx-table' },
      el('thead', {}, el('tr', {},
        el('th', { scope: 'col', class: 'mx-th' }, t('shared.assetColumn')),
        ...agents.map((agent) => el('th', { scope: 'col', class: 'mx-th mx-th-agent' }, agentHeadButton(tab, agent))))),
      el('tbody', {}, ...rows.map((row) => rowNode(tab, kind, row, agents))));
  }

  function renderMatrix(tab) {
    const kind = KIND_BY_TAB[tab];
    const agents = installedAgents(S.data);
    const rows = filteredRows(tab);
    const card = el('div', { class: 'card mx-card' },
      el('div', { class: 'card-head' },
        el('h2', {}, t(TAB_KEY[tab])),
        el('span', { class: 'sub spacer' }, t('shared.matrixNote')),
        tab === 'skills' ? skillsTools() : null));
    const body = el('div', { class: 'card-body mx-wrap' });
    if (!agents.length) {
      body.appendChild(el('div', { class: 'empty' },
        el('div', { class: 'mb-12' }, t('shared.noAgents')),
        reloadButton()));
    } else if (!rows.length) {
      body.appendChild(el('div', { class: 'empty' },
        el('div', { class: 'mb-12' }, t(tab === 'skills' && S.q ? 'shared.noMatches' : 'shared.tabEmpty')),
        tab === 'skills' && S.q ? null : reloadButton()));
    } else {
      body.appendChild(matrixTable(tab, kind, agents, rows));
      body.appendChild(legendBar());
    }
    card.appendChild(body);
    return card;
  }

  /* ---------------------------------------------------------------- */
  /* 健康 tab                                                          */
  /* ---------------------------------------------------------------- */

  function recheckButton() {
    return el('button', { class: 'btn sm', type: 'button', onclick: () => void loadDoctor() }, t('doctor.recheck'));
  }

  function codeText(code) {
    return t(CODE_KEY[code] || 'doctor.unknownCode');
  }

  function findingKey(f) {
    return String((f && f.code) || '') + '|' + String((f && f.path) || '');
  }

  /** mcp.drift 的 detail 行：字段名 + 库内值 → agent 值（纯数据）。 */
  function driftLine(f) {
    if (!f || f.code !== 'mcp.drift' || !f.detail) return null;
    return el('div', { class: 'doc-drift fs-11 mono' }, driftText(f.detail));
  }

  function findingActions(f, busy) {
    if (f.code === 'mcp.drift') {
      return ['library', 'agent', 'independent'].map((choice) => el('button', {
        class: 'btn sm',
        type: 'button',
        disabled: busy,
        onclick: () => void applyFix(f, choice)
      }, t(CHOICE_KEY[choice])));
    }
    if (f.fixable && f.fix) {
      return [el('button', {
        class: 'btn sm',
        type: 'button',
        disabled: busy,
        onclick: () => void applyFix(f, null)
      }, t('doctor.fix'))];
    }
    return [];
  }

  function findingNode(f) {
    const busy = !!S.fixing;
    const actions = findingActions(f, busy);
    return el('div', { class: 'doc-finding', 'data-code': f.code || '' },
      el('div', { class: 'doc-path mono fs-11' }, String(f.path || '—')),
      driftLine(f),
      actions.length ? el('div', { class: 'doc-actions' }, ...actions) : null);
  }

  /** 同一 severity 内按 code 固定顺序分组；未知 code 排最后。 */
  function groupByCode(list) {
    const out = [];
    for (const code of CODE_ORDER) {
      const items = list.filter((f) => f.code === code);
      if (items.length) out.push([code, items]);
    }
    const rest = list.filter((f) => CODE_ORDER.indexOf(f.code) < 0);
    if (rest.length) out.push([String((rest[0] && rest[0].code) || ''), rest]);
    return out;
  }

  function findingsList(findings) {
    const counts = { critical: 0, warn: 0, info: 0 };
    for (const f of findings) counts[f.severity] = (counts[f.severity] || 0) + 1;
    const summary = el('div', { class: 'doc-counts' },
      ...SEV_ORDER.filter((sev) => counts[sev]).map((sev) =>
        el('span', { class: sevChipClass(sev) }, t(SEV_KEY[sev]) + ' ' + counts[sev])));
    const wrap = el('div', { class: 'doc-list' }, summary);
    for (const sev of SEV_ORDER) {
      const group = findings.filter((f) => f.severity === sev);
      if (!group.length) continue;
      wrap.appendChild(el('div', { class: 'doc-section', 'data-severity': sev },
        el('div', { class: 'doc-section-head' }, t(SEV_KEY[sev])),
        ...groupByCode(group).map(([code, items]) =>
          el('div', { class: 'doc-code-group' },
            el('div', { class: 'doc-code' }, codeText(code)),
            ...items.map((f) => findingNode(f))))));
    }
    return wrap;
  }

  async function applyFix(f, choice) {
    if (S.fixing) return;
    S.fixing = findingKey(f);
    paint();
    try {
      const payload = { code: f.code, path: f.path, detail: f.detail || {}, fix: f.fix || null };
      if (choice) payload.choice = choice;
      const r = await call('applyFindingFix', payload);
      if (r && r.ok === false) toast(failText(r, 'doctor.fixFailed'), 'err');
      else toast(t('doctor.fixApplied'), 'ok');
    } catch (e) {
      if (isBridgeMissing(e)) toast(t('shared.bridgeMissing'), 'err');
      else {
        reportUiError('error', e);
        toast(t('doctor.fixFailed', { msg: zhError(e) }), 'err');
      }
    } finally {
      S.fixing = '';
      S.doctor = null;
      await loadDoctor();
    }
  }

  function renderHealth() {
    const card = el('div', { class: 'card doc-card' },
      el('div', { class: 'card-head' },
        el('h2', {}, t('doctor.title')),
        el('span', { class: 'sub spacer' }, t('doctor.note')),
        recheckButton()));
    const body = el('div', { class: 'card-body' });
    if (S.doctorLoading) {
      body.appendChild(el('div', { class: 'empty' }, t('common.loading')));
    } else if (S.doctorError) {
      body.appendChild(errorPanel(S.doctorError, loadDoctor));
    } else {
      const findings = S.doctor && Array.isArray(S.doctor.findings) ? S.doctor.findings : null;
      if (!findings) {
        body.appendChild(el('div', { class: 'empty' }, t('common.loading')));
      } else if (!findings.length) {
        body.appendChild(el('div', { class: 'empty' },
          el('div', { class: 'fs-26 mb-8' }, '✓'),
          el('div', { class: 'mb-12' }, t('doctor.zeroFindings')),
          recheckButton()));
      } else {
        body.appendChild(findingsList(findings));
      }
    }
    card.appendChild(body);
    return card;
  }

  /* ---------------------------------------------------------------- */
  /* 收敛向导                                                          */
  /* ---------------------------------------------------------------- */

  /** 替换 #modal 的内容（向导翻页用；首次打开仍走 app.js 的 openModal）。 */
  function setModalContent(title, body, foot) {
    // 用户可能在异步动作期间按 Esc 关掉了对话框：不要把它重新「填」出来。
    const scrim = document.getElementById('modal-scrim');
    if (!scrim || !scrim.classList.contains('on')) return;
    const titleEl = document.getElementById('modal-title');
    if (titleEl) titleEl.textContent = title;
    const bodyEl = document.getElementById('modal-body');
    if (bodyEl) {
      bodyEl.textContent = '';
      bodyEl.appendChild(body);
    }
    const footEl = document.getElementById('modal-foot');
    if (footEl) {
      footEl.textContent = '';
      for (const n of foot) footEl.appendChild(n);
    }
    const modal = document.getElementById('modal');
    if (modal) {
      if (typeof focusInto === 'function') focusInto(modal);
      else modal.focus();
    }
  }

  function verdictChip(verdict) {
    const key = VERDICT_KEY[verdict] || 'converge.unknown';
    return el('span', { class: 'chip ' + (verdict === 'identical' ? 'ok' : verdict === 'diverged' ? 'warn' : '') }, t(key));
  }

  function convergeGroupError(item) {
    if (item && item.code) return zhCode(item.code) || String(item.code);
    return zhError((item && item.error) || '');
  }

  async function openConverge() {
    if (S.convergeBusy) return;
    S.convergeBusy = true;
    paint();
    try {
      const r = await call('planConverge');
      if (!r || r.ok === false) {
        toast(failText(r, 'converge.loadFailed'), 'err');
        return;
      }
      const groups = Array.isArray(r.groups) ? r.groups : [];
      if (!groups.length) {
        toast(t('converge.none'), 'warn');
        return;
      }
      showConvergeWizard(groups);
    } catch (e) {
      if (isBridgeMissing(e)) toast(t('shared.bridgeMissing'), 'err');
      else {
        reportUiError('error', e);
        toast(t('converge.loadFailed', { msg: zhError(e) }), 'err');
      }
    } finally {
      S.convergeBusy = false;
      paint();
    }
  }

  function showConvergeWizard(groups) {
    // identical 默认勾选；diverged 置灰；single 只在用户显式点选时才动。
    const chosen = new Set(groups.filter((g) => g.verdict === 'identical').map((g) => g.name));
    let opened = false;
    let nextBtn = null;
    let countEl = null;

    function show(body, foot) {
      if (!opened) {
        opened = true;
        openModal(t('converge.title'), body, foot, { width: 'form' });
      } else {
        setModalContent(t('converge.title'), body, foot);
      }
    }

    function picked() {
      return groups.filter((g) => chosen.has(g.name));
    }

    function savedBytesOf(list) {
      return list.reduce((n, g) => n + (Number(g.savedBytes) || 0), 0);
    }

    function groupPick(g) {
      const diverged = g.verdict === 'diverged';
      const check = el('input', {
        type: 'checkbox',
        class: 'cv-check',
        checked: chosen.has(g.name),
        disabled: diverged,
        onchange: (e) => {
          if (e.target.checked) chosen.add(g.name);
          else chosen.delete(g.name);
          if (countEl) countEl.textContent = t('converge.selected', { n: chosen.size });
          if (nextBtn) nextBtn.disabled = chosen.size === 0;
        }
      });
      const replacePaths = (g.replace || []).map((c) => el('div', { class: 'mono fs-11 cv-subpath' }, String((c && c.path) || '')));
      const diffs = (g.diffs || []).length
        ? el('div', { class: 'cv-diffs' },
          el('div', { class: 'muted fs-11' }, t('converge.diffFiles')),
          ...(g.diffs || []).map((d) => el('div', { class: 'mono fs-11' }, String((d && d.rel) || '') + ' · ' + t(DIFF_KEY[d.state] || 'converge.diffDifferent'))))
        : null;
      return el('div', { class: 'cv-group' + (diverged ? ' is-diverged' : ''), 'data-verdict': g.verdict || '' },
        el('label', { class: 'cv-group-head' }, check,
          el('span', { class: 'cv-name' }, String(g.label || g.name || '')),
          verdictChip(g.verdict)),
        el('div', { class: 'cv-paths' },
          el('div', { class: 'cv-path' },
            el('span', { class: 'muted fs-11' }, t('converge.keepAt')), ' ',
            el('span', { class: 'mono fs-11' }, String(g.keeper || '—'))),
          el('div', { class: 'cv-path' }, el('span', { class: 'muted fs-11' }, t('converge.replaceWith'))),
          ...replacePaths),
        diffs);
    }

    function stepOne() {
      countEl = el('span', { class: 'cv-count muted fs-12' }, t('converge.selected', { n: chosen.size }));
      nextBtn = el('button', {
        class: 'btn primary',
        type: 'button',
        disabled: chosen.size === 0,
        onclick: () => { if (chosen.size) stepTwo(); }
      }, t('converge.next'));
      const body = el('div', { class: 'cv-step' },
        el('div', { class: 'cv-step-label' }, t('converge.step1')),
        el('div', { class: 'muted fs-12 mb-8' }, t('converge.singleNote')),
        el('div', { class: 'muted fs-12 mb-12' }, t('converge.divergedNote')),
        el('div', { class: 'cv-list' }, ...groups.map((g) => groupPick(g))),
        countEl);
      const foot = [
        el('button', { class: 'btn', type: 'button', onclick: closeModal }, t('common.cancel')),
        nextBtn
      ];
      show(body, foot);
    }

    function previewGroup(g) {
      const replacePaths = (g.replace || []).map((c) => el('div', { class: 'mono fs-11 cv-subpath' }, String((c && c.path) || '')));
      return el('div', { class: 'cv-group', 'data-verdict': g.verdict || '' },
        el('div', { class: 'cv-group-head' },
          el('span', { class: 'cv-name' }, String(g.label || g.name || '')),
          verdictChip(g.verdict)),
        el('div', { class: 'cv-paths' },
          el('div', { class: 'cv-path' },
            el('span', { class: 'muted fs-11' }, t('converge.keepAt')), ' ',
            el('span', { class: 'mono fs-11' }, String(g.keeper || '—'))),
          el('div', { class: 'cv-path' }, el('span', { class: 'muted fs-11' }, t('converge.replaceWith'))),
          ...replacePaths));
    }

    function stepTwo() {
      const list = picked();
      const body = el('div', { class: 'cv-step' },
        el('div', { class: 'cv-step-label' }, t('converge.step2')),
        el('div', { class: 'cv-saved' }, t('converge.saved', { size: bytes(savedBytesOf(list)) })),
        el('div', { class: 'cv-list' }, ...list.map((g) => previewGroup(g))));
      const foot = [
        el('button', { class: 'btn', type: 'button', onclick: stepOne }, t('converge.back')),
        el('button', { class: 'btn primary', type: 'button', onclick: stepThree }, t('converge.next'))
      ];
      show(body, foot);
    }

    function stepThree() {
      const list = picked();
      const applyBtn = el('button', {
        class: 'btn primary',
        type: 'button',
        onclick: () => {
          applyBtn.disabled = true;
          applyBtn.classList.add('loading');
          applyBtn.textContent = t('common.loading');
          void applyConverge(list);
        }
      }, t('converge.apply'));
      const body = el('div', { class: 'cv-step' },
        el('div', { class: 'cv-step-label' }, t('converge.step3')),
        el('div', { class: 'alert warn mb-12' }, t('converge.confirmLine')),
        el('div', { class: 'cv-saved' }, t('converge.saved', { size: bytes(savedBytesOf(list)) })),
        el('div', { class: 'cv-list' }, ...list.map((g) =>
          el('div', { class: 'cv-group' }, el('div', { class: 'cv-group-head' },
            el('span', { class: 'cv-name' }, String(g.label || g.name || '')), verdictChip(g.verdict))))));
      const foot = [
        el('button', { class: 'btn', type: 'button', onclick: stepTwo }, t('converge.back')),
        applyBtn
      ];
      show(body, foot);
    }

    async function applyConverge(list) {
      let r;
      try {
        r = await call('applyConverge', { groups: list.map((g) => g.name) });
      } catch (e) {
        if (isBridgeMissing(e)) {
          showResult({ ok: false, groups: [], error: t('shared.bridgeMissing') }, list);
          return;
        }
        reportUiError('error', e);
        showResult({ ok: false, groups: [], error: zhError(e) }, list);
        return;
      }
      showResult(r || {}, list);
    }

    function showResult(r, list) {
      const resultGroups = Array.isArray(r.groups) ? r.groups : [];
      const rows = list.map((g) => {
        // R2 的 apply_converge 返回 `groups[].name` 是 **slug**（与 plan_converge
        // 的分组名同口径）；显示名才用 label。组名匹配只用 slug，绝不拿 label 去对。
        const got = resultGroups.find((x) => x && x.name === g.name);
        return {
          name: String(g.label || g.name || ''),
          ok: got ? got.ok !== false : r.ok !== false,
          item: got || null
        };
      });
      const failed = rows.filter((x) => !x.ok).length;
      const body = el('div', { class: 'cv-step' },
        el('div', { class: 'cv-step-label' }, t('converge.resultTitle')),
        el('div', { class: 'cv-list' }, ...rows.map((x) =>
          el('div', { class: 'cv-result' + (x.ok ? ' ok' : ' err') },
            el('span', { class: 'cv-result-name' }, x.name),
            el('span', { class: 'chip ' + (x.ok ? 'ok' : 'err') }, x.ok ? t('common.done') : t('converge.skipped')),
            x.ok ? null : el('div', { class: 'muted fs-11' }, convergeGroupError(x.item) || t('converge.skipped'))))));
      const foot = [el('button', { class: 'btn', type: 'button', onclick: closeModal }, t('common.close'))];
      show(body, foot);
      if (!failed) toast(t('converge.applied', { n: rows.length }), 'ok');
      else toast(t('converge.failed', { n: failed }), 'err');
      S.doctor = null;
      void reload();
    }

    stepOne();
  }

  /**
   * 库位置迁移对话框（§9.3）。
   *
   * 两个目标都是后端认识的枚举值（`default` = `~/.agents`，`store` = 应用私有
   * 目录），自定义绝对路径留给设置文件 —— 一个没有目录选择器的输入框只会让
   * 用户手打路径打错。迁移是**幂等**的：选到当前位置时后端回 `moved: 0`，
   * 不是错误，所以这里不预判「你已经在那个位置」。
   *
   * 迁移会改 `settings.libraryRoot`，所以成功后要重新扫描：矩阵、台账与
   * 各 agent 的链接都换了根，缓存里的那一份立刻就是陈的。
   */
  async function openMigrate() {
    if (S.migrateBusy) return;
    const root = (S.data && S.data.root) || '—';
    const pick = async (to) => {
      // `openModal` 没有返回值，关闭走 app.js 的全局 `closeModal`
      //（它同时把 return-focus 还回去，Esc/遮罩路径也走同一条）。
      if (typeof closeModal === 'function') closeModal();
      S.migrateBusy = true;
      paint();
      try {
        const r = await call('libraryMigrate', { to: to });
        if (!r || r.ok === false) {
          toast(r && r.code ? (zhCode(r.code) || String(r.code)) : failText(r, 'shared.migrateFailed'), 'err');
          return;
        }
        toast(t('shared.migrateDone', { to: String(r.to || to), n: r.links || 0 }), 'ok');
        await reload();
        if (typeof rescan === 'function') await rescan(true);
      } catch (e) {
        reportUiError('error', e);
        toast(t('shared.migrateFailed', { msg: zhError(e) }), 'err');
      } finally {
        S.migrateBusy = false;
        paint();
      }
    };
    const body = el('div', { class: 'stack-12' },
      el('div', { class: 'muted fs-12' }, t('shared.migrateBody', { root: String(root) })),
      el('div', { class: 'muted fs-11' }, t('shared.migrateHint')));
    const foot = [
      el('button', { class: 'btn', type: 'button', onclick: () => { if (typeof closeModal === 'function') closeModal(); } }, t('common.cancel')),
      el('button', { class: 'btn', type: 'button', onclick: () => void pick('store') }, t('shared.migrateStore')),
      el('button', { class: 'btn primary', type: 'button', onclick: () => void pick('default') }, t('shared.migrateDefault'))
    ];
    openModal(t('shared.migrateTitle'), body, foot, { width: 'form' });
  }

  /* ---------------------------------------------------------------- */
  /* 视图骨架                                                          */
  /* ---------------------------------------------------------------- */

  function renderTop() {
    const box = el('div', { class: 'stack-14' });
    if (S.error && !S.data) {
      box.appendChild(el('div', { class: 'card' },
        el('div', { class: 'card-body' }, errorPanel(S.error, retryLoad))));
      return box;
    }
    const d = S.data;
    if (!d) return box;
    if (d.cap && d.cap.copyOnly) {
      box.appendChild(el('div', { class: 'shared-banner' },
        el('span', { class: 'shared-banner-ico', 'aria-hidden': 'true' }, '!'),
        el('span', { class: 'shared-banner-text' }, t('shared.bannerCopy')),
        resyncButton()));
    }
    // 首次发现重复技能（§9.4）：只提示一次、只说明、不动盘。按钮进的是同一个
    // 收敛向导，用户在向导里确认之后才会写。
    if (S.hintGroups > 0 && !S.hintDismissed) {
      box.appendChild(el('div', { class: 'shared-banner' },
        el('span', { class: 'shared-banner-ico', 'aria-hidden': 'true' }, 'i'),
        el('span', { class: 'shared-banner-text' }, t('shared.hintGroups', { n: S.hintGroups })),
        el('button', {
          class: 'btn sm',
          type: 'button',
          onclick: () => void openConverge()
        }, t('shared.hintReview')),
        el('button', {
          class: 'btn sm',
          type: 'button',
          'data-focus-key': 'shared-hint-dismiss',
          onclick: dismissHint
        }, t('shared.hintDismiss'))));
    }
    box.appendChild(el('div', { class: 'shared-meta' },
      el('span', { class: 'muted fs-11' }, t('shared.root')),
      el('span', { class: 'mono fs-11 shared-root' }, String(d.root || '—')),
      // 库位置迁移（§9.3 / §21.7 点名给 P6 的界面入口）。命令一直在、桥接也
      // 一直在，缺的就是这个按钮：不给入口时用户只能手工改 settings.json。
      el('button', {
        class: 'btn sm' + (S.migrateBusy ? ' loading' : ''),
        type: 'button',
        disabled: S.migrateBusy,
        'data-focus-key': 'shared-migrate',
        onclick: () => void openMigrate()
      }, t('shared.migrate'))));
    if (libraryEmpty(d)) {
      box.appendChild(el('div', { class: 'card' },
        el('div', { class: 'empty' },
          el('div', { class: 'fs-26 mb-8' }, '⌂'),
          el('div', { class: 'mb-12' }, t('shared.emptyTitle')),
          el('div', { class: 'muted fs-12 mb-12' }, t('shared.emptyHint')),
          reloadButton())));
    }
    return box;
  }

  function renderTabsBar() {
    const bar = el('div', { class: 'shared-tabs', role: 'tablist' });
    for (const tab of TABS) {
      const count = tabCount(tab);
      bar.appendChild(el('button', {
        class: 'shared-tab',
        type: 'button',
        role: 'tab',
        id: 'shared-tab-' + tab,
        'aria-selected': S.tab === tab ? 'true' : 'false',
        'aria-controls': 'shared-panel',
        'data-focus-key': 'shared-tab-' + tab,
        onclick: () => setTab(tab)
      }, t(TAB_KEY[tab]) + (count === null ? '' : ' · ' + count)));
    }
    return bar;
  }

  function setTab(tab) {
    if (S.tab === tab) return;
    S.tab = tab;
    if (tab === 'health' && !S.doctor && !S.doctorLoading && !S.doctorError) void loadDoctor();
    paint();
  }

  function renderContent() {
    if (S.tab === 'health') return renderHealth();
    return renderMatrix(S.tab);
  }

  /** 重画整页。`focusId` 用于搜索框：输入时重建 DOM 之后把焦点和光标还回去。 */
  function paint(focusId) {
    const root = S.root;
    if (!root) return;
    // 重画会换掉所有节点；先把焦点所在的 data-focus-key 记下来，画完再放回去
    // （单元格切换 / 切 tab 之后焦点不能掉到 body 上）。
    const prevKey = typeof focusKeyNow === 'function' ? focusKeyNow() : null;
    root.textContent = '';
    root.appendChild(renderTop());
    if (S.data) {
      root.appendChild(renderTabsBar());
      root.appendChild(el('div', {
        id: 'shared-panel',
        class: 'shared-panel',
        role: 'tabpanel',
        'aria-labelledby': 'shared-tab-' + S.tab
      }, renderContent()));
    }
    if (focusId) {
      const node = document.getElementById(focusId);
      if (node && typeof node.focus === 'function') {
        node.focus({ preventScroll: true });
        if (typeof node.setSelectionRange === 'function' && typeof node.value === 'string') {
          try { node.setSelectionRange(node.value.length, node.value.length); } catch (_e) { /* type=search */ }
        }
      }
    } else if (prevKey && typeof restoreFocusKey === 'function') {
      restoreFocusKey(root, prevKey);
    }
  }

  /** VIEWS.shared.render 的入口：先同步画出骨架，数据在异步里补上。 */
  function viewShared() {
    S.root = el('div', { class: 'shared-page' });
    paint();
    ensureData();
    // 首次提示的探测排在数据之后：矩阵先画出来，横幅晚一步出现也不会挡住人。
    probeHint();
    return S.root;
  }

  window.__akShared = { viewShared: viewShared };
})();
