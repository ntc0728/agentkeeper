/*
 * Tauri shell adapter (stage 1 prototype).
 *
 * The Electron build gets `window.ak` from a contextBridge preload script.
 * Tauri has no equivalent injection point, so this file reproduces the same
 * surface on top of `window.__TAURI__.core.invoke`.
 *
 * Purpose is validation, not architecture: it must expose exactly the same
 * function names, arities and argument shapes as the (now deleted) Electron
 * preload script `src/main/preload.js` did, so the unmodified
 * src/renderer/app.js can run. Anything that differs here would
 * show up as a UI bug rather than a migration bug, which is the thing stage 1
 * is trying to avoid.
 *
 * CSP note: this file is loaded by a <script> tag from the same origin, so it
 * is allowed under `script-src 'self'`.
 */
(function () {
  'use strict';

  var invoke = window.__TAURI__ && window.__TAURI__.core
    ? window.__TAURI__.core.invoke
    : null;

  if (!invoke) {
    // Fail loudly. A silent no-op here would render an empty dashboard that
    // looks like a data problem rather than a broken bridge.
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
    // The command argument is named `payload` on the Rust side, and Tauri looks
    // it up by name in the invoke body (`missing required key` otherwise). Pass
    // the object under that key — handing the bare payload fails the call, and
    // the renderer has no catch, so the button just looks dead.
    saveSkill: function (payload) { return call('save_skill', { payload: payload }); },
    upsertMcp: function (payload) { return call('upsert_mcp', { payload: payload }); },
    deleteMcp: function (assetId) { return call('delete_mcp', { args: { assetId: assetId } }); },
    deleteAsset: function (assetId) { return call('delete_asset', { args: { assetId: assetId } }); },
    distribute: function (assetId, targets) { return call('distribute', { args: { assetId: assetId, targets: targets } }); },
    pickProject: function () { return call('pick_project'); },
    removeProject: function (id) { return call('remove_project', { id: id }); },
    setSandbox: function (on) { return call('set_sandbox', { on: on }); },
    getSettings: function () { return call('get_settings'); },
    saveSettings: function (settings) { return call('save_settings', { settings: settings }); },

    // --- software update (inert in stage 1) ---
    updateStatus: function () { return call('update_status'); },
    updateCheck: function () { return call('update_check'); },
    updateSaveSettings: function (patch) { return call('update_save_settings', { patch: patch }); },
    updateSetToken: function (token) { return call('update_set_token', { token: token }); },
    updateDownload: function (url, name) { return call('update_download', { url: url, name: name }); },
    updateProgress: function () { return call('update_progress'); },
    updateCancel: function () { return call('update_cancel'); },
    updateInstall: function (path) { return call('update_install', { path: path }); },
    updateSkip: function (version) { return call('update_skip', { version: version }); },
    openReleasePage: function (url) { return call('open_release_page', { url: url }); },

    /**
     * Electron pushes 'ak:update-event' from the main process. Tauri stages
     * emit 'update-event'. Returns an unsubscribe function in both worlds so
     * a re-render cannot stack listeners.
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
    pending: function () { return call('pending'); },
    applyPending: function () { return call('apply_pending'); },
    discardPending: function () { return call('discard_pending'); },
    audit: function (opts) { return call('audit', { opts: opts }); },
    transactions: function (limit) { return call('transactions', { limit: limit }); },
    rollback: function (txId) { return call('rollback', { txId: txId }); },
    openPath: function (path) { return call('open_path', { path: path }); },
    openStore: function () { return call('open_store_dir'); },
    // The renderer's crash channel. The command takes a single `payload`
    // argument, so the object goes in under that key (see the note above
    // `saveSkill`): handing over the bare payload is rejected by Tauri with
    // `missing required key payload`, and the error would then be lost silently.
    uiError: function (payload) { return call('ui_error', { payload: payload }); },

    // Data property in the Electron preload too — app.js reads it as a value.
    platform: 'win32'
  };

  window.ak = ak;
  window.__AK_BRIDGE__ = 'tauri';
})();