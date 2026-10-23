/*
 * Tauri shell adapter.
 *
 * The renderer gets `window.ak` from this file, which puts the same surface on
 * top of `window.__TAURI__.core.invoke`. The names, arities and argument shapes
 * here are a contract with `src/renderer/app.js`: anything that differs shows up
 * as a UI bug, so `scripts/check.js bridge` diffs each call against the Rust
 * command signature.
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
    getSettings: function () { return call('get_settings'); },
    saveSettings: function (settings) { return call('save_settings', { settings: settings }); },

    // --- software update ---
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
     * The shell emits 'update-event' (see `lib.rs`). Returns an unsubscribe
     * function so a re-render cannot stack listeners.
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
    // The renderer's crash channel. The command takes a single `payload`
    // argument, so the object goes in under that key (see the note above
    // `saveSkill`): handing over the bare payload is rejected by Tauri with
    // `missing required key payload`, and the error would then be lost silently.
    uiError: function (payload) { return call('ui_error', { payload: payload }); },

    // Data property, not a function: app.js reads it as a value.
    platform: 'win32'
  };

  window.ak = ak;
  window.__AK_BRIDGE__ = 'tauri';
})();