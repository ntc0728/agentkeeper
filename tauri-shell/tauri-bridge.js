/*
 * Tauri 外壳适配层。
 *
 * 渲染层从这里拿到 `window.ak`，它把同一套接口铺在 `window.__TAURI__.core.invoke`
 * 之上。这里的名字、参数个数与参数形状都是与 `src/renderer/app.js` 的契约：差一点
 * 就会表现成 UI 缺陷，所以 `scripts/check.js bridge` 会把每次调用拿去和 Rust 命令
 * 的签名比对。
 *
 * CSP 说明：这个文件由同源的 <script> 标签加载，所以在 `script-src 'self'` 下是
 * 允许的。
 */
(function () {
  'use strict';

  var invoke = window.__TAURI__ && window.__TAURI__.core
    ? window.__TAURI__.core.invoke
    : null;

  if (!invoke) {
    // 要大声地失败。这里静默不做事，渲染出来的空面板会看起来像数据问题，
    // 而不是桥接坏了。
    document.body.innerHTML =
      '<pre style="color:#b00;font:13px monospace;padding:24px">' +
      'Tauri bridge unavailable: window.__TAURI__.core.invoke is missing.</pre>';
    return;
  }

  function call(cmd, payload) {
    return invoke(cmd, payload || {});
  }

  var ak = {
    bootstrap: function () { return call('bootstrap'); },
    scan: function () { return call('scan'); },
    readAsset: function (assetId) { return call('read_asset', { args: { assetId: assetId } }); },
    saveMarkdown: function (assetId, content) { return call('save_markdown', { args: { assetId: assetId, content: content } }); },
    diff: function (assetId, content) { return call('diff', { args: { assetId: assetId, content: content } }); },
    // Rust 那边这个命令的参数叫 `payload`，Tauri 是按名字在 invoke 的 body 里找它的
    // （否则就报 `missing required key`）。所以对象要挂在这个键下面 —— 直接把 payload
    // 本身递过去会让调用失败，而渲染层没有 catch，按钮看上去就像点不动。
    saveSkill: function (payload) { return call('save_skill', { payload: payload }); },
    upsertMcp: function (payload) { return call('upsert_mcp', { payload: payload }); },
    deleteMcp: function (assetId) { return call('delete_mcp', { args: { assetId: assetId } }); },
    deleteAsset: function (assetId) { return call('delete_asset', { args: { assetId: assetId } }); },
    distribute: function (assetId, targets) { return call('distribute', { args: { assetId: assetId, targets: targets } }); },
    pickProject: function () { return call('pick_project'); },
    removeProject: function (id) { return call('remove_project', { id: id }); },
    getSettings: function () { return call('get_settings'); },
    saveSettings: function (settings) { return call('save_settings', { settings: settings }); },

    // --- 软件更新 ---
    updateStatus: function () { return call('update_status'); },
    updateCheck: function () { return call('update_check'); },
    updateReleases: function (limit) { return call('update_releases', { limit: limit }); },
    updateSaveSettings: function (patch) { return call('update_save_settings', { patch: patch }); },
    updateSetToken: function (token) { return call('update_set_token', { token: token }); },
    updateDownload: function (url, name) { return call('update_download', { url: url, name: name }); },
    updateProgress: function () { return call('update_progress'); },
    updateCancel: function () { return call('update_cancel'); },
    updateInstall: function (path) { return call('update_install', { path: path }); },
    updateSkip: function (version) { return call('update_skip', { version: version }); },
    openReleasePage: function (url) { return call('open_release_page', { url: url }); },

    /**
     * 外壳会发 'update-event'（见 `lib.rs`）。返回一个退订函数，这样重新渲染
     * 就不会把监听器越叠越多。
     */
    onUpdateEvent: function (callback) {
      if (window.__TAURI__ && window.__TAURI__.event && typeof window.__TAURI__.event.listen === 'function') {
        var p = window.__TAURI__.event.listen('update-event', function (e) {
          try { callback(e.payload); } catch (_e) { /* renderer bug */ }
        });
        return function () { p.then(function (un) { un(); }); };
      }
      return function () {};
    },

    localStats: function () { return call('local_stats'); },
    baselineDiff: function () { return call('baseline_diff'); },
    setBaseline: function () { return call('set_baseline'); },
    exportBundle: function (assetIds) { return call('export_bundle', { assetIds: assetIds }); },
    importBundle: function () { return call('import_bundle'); },
    searchRegistry: function (opts) { return call('search_registry', { opts: opts }); },
    installSkill: function (payload) { return call('install_skill', { payload: payload }); },
    audit: function (opts) { return call('audit', { opts: opts }); },
    openPath: function (path) { return call('open_path', { path: path }); },
    openStore: function () { return call('open_store_dir'); },
    // 渲染层的崩溃上报通道。这个命令只接受一个 `payload` 参数，所以对象要挂在这个键
    // 下面（见 `saveSkill` 上面那段说明）：直接递裸对象会被 Tauri 以
    // `missing required key payload` 拒掉，而那个错误又会悄无声息地丢掉。
    uiError: function (payload) { return call('ui_error', { payload: payload }); },

    // 是数据属性而不是函数：app.js 把它当值读。
    platform: 'win32'
  };

  window.ak = ak;
  window.__AK_BRIDGE__ = 'tauri';
})();