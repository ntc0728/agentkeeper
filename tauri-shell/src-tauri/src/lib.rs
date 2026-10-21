//! AgentKeeper — Tauri shell.
//!
//! Stage 1 proved the shell can carry the existing renderer unchanged.
//! Stage 2 replaces the one genuinely slow part of the app — the asset
//! scanner — with a Rust implementation in `src/core/`, and wires
//! `bootstrap` / `scan` to real data instead of the stage-1 stub dataset.
//!
//! Everything else is still a stub: writes (save/distribute/apply), update
//! channels and the project registry mutation path arrive in later stages.
//! Those stubs return the real response *shape* so `src/renderer/app.js`
//! keeps rendering — a stub that returns `{}` would prove nothing.
//!
//! Stage-1 leftovers (`ak_agents`, `ak_kinds`, `shell_report`, `add_project`,
//! `list_projects`, `update_unskip`) were removed on 2026-10-20: the renderer
//! has no consumer for any of them, and the bridge listed them as reachable
//! surface. The project list reaches the UI through `scan`; projects are added
//! through `pick_project`.

pub mod core;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use core::adapters::{resolve_home, Agent, AGENTS, SHARED_SKILLS_DIR};

/// Payload shapes mirror the preload surface in `src/main/preload.js`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetArgs {
    pub asset_id: Option<String>,
    pub content: Option<String>,
    /// `distribute` sends `[{agentId, scope, projectId}]` — objects, not ids.
    pub targets: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
pub struct StoreRoot {
    pub store_root: String,
    pub sandbox: bool,
}

fn user_home() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

fn store_root() -> String {
    // `AK_STORE_ROOT` wins, then the directory the Electron build uses.
    //
    // It is NOT `<APPDATA>\agentkeeper`: Electron's `app.getPath('userData')`
    // resolves to `<APPDATA>\<productName>` — `productName` ("AgentKeeper")
    // wins over the package `name` — and `main.js:52` appends another
    // "agentkeeper". Getting this wrong does not fail loudly, it silently
    // starts from an empty store beside the real one: no settings, no
    // projects, no pending changes. Verified against the directory that
    // actually holds `settings.json` / `projects.json` on this machine.
    if let Ok(explicit) = std::env::var("AK_STORE_ROOT") {
        return explicit;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = match std::env::var("APPDATA") {
        Ok(appdata) => appdata,
        Err(_) => match std::env::var("HOME") {
            // `src/mcp/server.js` falls back to `~/.config/AgentKeeper`.
            Ok(home) => format!("{}{}.config", home, sep),
            Err(_) => ".".to_string(),
        },
    };
    format!("{0}{1}AgentKeeper{1}agentkeeper", base, sep)
}

/// The sandbox switch, for the life of the process.
///
/// It is session-scoped in both builds: Electron seeds it from
/// `AGENTKEEPER_SANDBOX` (default on) and mirrors later toggles into
/// `meta.json`, which nothing reads back at startup. Holding it in a
/// process-global is what makes `set_sandbox` outlive the command that set it —
/// without it every subsequent write would silently re-apply the env default and
/// a user who turned the sandbox *off* would watch their edits vanish into the
/// shadow tree.
static SANDBOX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SANDBOX_INIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sandbox_on() -> bool {
    use std::sync::atomic::Ordering;
    if !SANDBOX_INIT.swap(true, Ordering::Relaxed) {
        SANDBOX.store(core::settings::sandbox_from_env(), Ordering::Relaxed);
    }
    SANDBOX.load(Ordering::Relaxed)
}

fn remember_sandbox(on: bool) {
    use std::sync::atomic::Ordering;
    SANDBOX.store(on, Ordering::Relaxed);
    SANDBOX_INIT.store(true, Ordering::Relaxed);
}

/// The store for this call. Every command that touches the store builds it
/// through here so the sandbox flag can never disagree between them.
fn open_store() -> core::store::Store {
    core::store::Store::new(&store_root(), sandbox_on())
}

fn open_keeper() -> core::keeper::Keeper {
    core::keeper::Keeper::new(open_store(), &user_home())
}

fn kinds_meta() -> Value {
    json!({
        "skill":  { "label": "Skill",      "icon": "SK", "color": "#7c5cff" },
        "mcp":    { "label": "MCP Server", "icon": "MC", "color": "#2f6bff" },
        "plugin": { "label": "Plugin",     "icon": "PL", "color": "#0f9d58" },
        "rule":   { "label": "Rule",       "icon": "RU", "color": "#e07b39" },
        "memory": { "label": "Memory",     "icon": "ME", "color": "#0d9488" }
    })
}

fn paths_json(p: &core::adapters::Paths) -> Value {
    json!({
        "skills": p.skills,
        "plugins": p.plugins,
        "rules": p.rules,
        "memory": p.memory,
        "mcp": p.mcp
    })
}

fn project_paths_json(p: &core::adapters::ProjectPaths) -> Value {
    let mut m = Map::new();
    m.insert("marker".into(), json!(p.marker));
    m.insert("skills".into(), json!(p.skills));
    m.insert("rules".into(), json!(p.rules));
    m.insert("memory".into(), json!(p.memory));
    m.insert("mcp".into(), json!(p.mcp));
    // The JS adapter omits `projects.plugins` entirely when it declares none,
    // and the scanner treats "absent" and "empty" differently.
    if let Some(plugins) = p.plugins {
        m.insert("plugins".into(), json!(plugins));
    }
    Value::Object(m)
}

fn agent_json(agent: &Agent, home: &str) -> Value {
    json!({
        "id": agent.id,
        "label": agent.label,
        "vendor": agent.vendor,
        "accent": agent.accent,
        "home": home,
        // bootstrap uses existsSync (any) rather than isDirectory.
        "installed": std::path::Path::new(home).exists(),
        "globals": paths_json(&agent.globals),
        "projects": project_paths_json(&agent.projects)
    })
}

/// Stage-1 tracing. A packaged GUI process cannot be watched through stdout,
/// so the few milestones that matter are appended to a file instead — the
/// same lesson as the earlier Electron startup diagnosis.
fn trace(msg: &str) {
    use std::io::Write;
    let path = std::env::var("AK_TRACE").unwrap_or_else(|_| r"F:\demo\tools\.tmp-shell-trace.log".to_string());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] {}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0), msg);
    }
}

/// WebView2 runtime presence, recorded before the window is built. If the
/// loader cannot find the runtime the webview never finishes loading and the
/// window stays hidden forever — indistinguishable from a hung app without
/// this line.
fn trace_webview2() {
    let candidates = [
        r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application",
        r"C:\Program Files\Microsoft\EdgeWebView\Application",
    ];
    for dir in candidates {
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut vers: Vec<String> = rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if e.path().join("msedgewebview2.exe").exists() { Some(name) } else { None }
                })
                .collect();
            vers.sort();
            trace(&format!("webview2 dir={} versions={:?}", dir, vers));
        } else {
            trace(&format!("webview2 dir={} MISSING", dir));
        }
    }
    // WEBVIEW2_BROWSER_EXECUTABLE_FOLDER is what the loader honours when the
    // runtime is not registered system-wide.
    match std::env::var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") {
        Ok(v) => trace(&format!("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER={}", v)),
        Err(_) => trace("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER unset"),
    }
}

// --- IPC thread budget ----------------------------------------------------
//
// How a command is declared is a performance decision, not a style one: a
// synchronous `fn` runs **on the IPC thread**, so everything it blocks on is
// time the window cannot repaint, cannot toast "working…" and cannot process
// the next click. The rule below is what decides `async` + `spawn_blocking`
// (the pattern `scan` established):
//
// * heavy → move it to the blocking pool when the command
//     - walks a directory tree (`scan`, `transactions`, `rollback`,
//       `apply_pending`, `discard_pending`, `distribute`'s copy per target),
//     - reads or writes a file whose size the store does not bound
//       (`audit.jsonl` grows with use; `audit`/`local_stats` parse the whole
//       journal on every call), or
//     - opens a socket (`search_registry`, `update_check`, `update_download`)
//       or spawns a process (`install_skill`, `export/import_bundle`,
//       `update_install`).
// * light → stays a synchronous `fn` when its entire IO is one small JSON
//   document the schema bounds (`get_settings`, `save_settings`, `pending`,
//   `update_status`, `update_*_settings`/`token`/`skip`/`progress`/`cancel`) or
//   one asset file the editor already holds in memory (`save_markdown`, `diff`,
//   `save_skill`, `upsert_mcp`, the deletes). A thread hop plus a serialization
//   step would buy nothing, and `bootstrap` additionally *must* be synchronous:
//   it runs before the window paints, so nothing else can reply first.
//
// The return shapes are identical either way — every one of these commands
// answers a JSON `Value` (or `Result<Value, String>`), and the renderer only
// ever awaits a promise.

#[tauri::command]
fn bootstrap() -> Value {
    let root = store_root();
    // The Electron build's `Store` constructor does this on first launch; doing
    // it here means the shell works against a fresh profile instead of failing
    // on a missing sandbox/ or audit.jsonl.
    let sandbox = core::settings::sandbox_from_env();
    let _ = core::settings::ensure_store(&root, sandbox);
    seed_first_run(&root);
    let home = user_home();
    let agents: Vec<Value> = AGENTS
        .iter()
        .filter_map(|a| resolve_home(a.home, &home).map(|h| agent_json(a, &h)))
        .collect();
    json!({
        "storeRoot": root,
        "sandbox": sandbox,
        "agents": agents,
        "sharedSkillsDir": resolve_home(SHARED_SKILLS_DIR, &home).unwrap_or_default(),
        "kinds": kinds_meta(),
        "version": env!("CARGO_PKG_VERSION")
    })
}

/// First-run seeding, mirroring `main.js`: only when the registry is empty and
/// the bundled demos actually resolve.
///
/// Running it from `bootstrap` rather than a one-shot startup hook is safe
/// because `registry.add` rejects duplicates — so a second window load is a
/// no-op, not a second copy of every demo project.
fn seed_first_run(store_root: &str) {
    core::registry::ensure_loaded(store_root);
    if !core::registry::list().is_empty() {
        return;
    }
    let Some(dir) = demo_project_root() else { return };
    core::registry::seed(store_root, &dir);
}

/// Locate `demo-projects/`.
///
/// The JS version probes `process.resourcesPath` and `__dirname`. The running
/// executable is the equivalent anchor here: in a dev build it sits at
/// `tauri-shell/src-tauri/target/<profile>/`, and once packaged the demos are
/// copied next to it via `bundle.resources`.
fn demo_project_root() -> Option<String> {
    if let Ok(explicit) = std::env::var("AK_DEMO_PROJECTS") {
        let p = std::path::PathBuf::from(&explicit);
        if p.is_dir() {
            return Some(explicit);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // dev:      target/<profile>/ -> src-tauri -> tauri-shell -> <repo>
    // packaged: <install>/         -> <install>
    let candidates = [
        dir.join("demo-projects"),
        dir.join("../../../../demo-projects"),
        dir.join("../demo-projects"),
    ];
    candidates
        .iter()
        .find(|c| c.is_dir())
        .map(|c| c.to_string_lossy().to_string())
}

/// The real scanner. Runs off the UI thread: even at ~200ms, blocking the
/// main thread on a file walk is exactly the failure this stage removes.
#[tauri::command]
async fn scan() -> Result<Value, String> {
    let home = user_home();
    let root = store_root();
    tauri::async_runtime::spawn_blocking(move || {
        let projects = core::scan::read_projects(&root);
        let started = std::time::Instant::now();
        let (payload, timings) = core::scan::scan_payload(&home, &projects);
        let detail: Vec<String> = timings
            .iter()
            .map(|(k, ms)| format!("{}={}ms", k, ms))
            .collect();
        trace(&format!(
            "scan {} total={}ms assets={}",
            detail.join(" "),
            started.elapsed().as_millis(),
            payload
                .get("assets")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        ));
        payload
    })
    .await
    .map_err(|e| format!("scan task failed: {}", e))
}

/// `ak:pending` → `{ sandbox, changes: [{op, real, shadow, bytes, at}] }`.
///
/// The renderer reads `r.sandbox` to label the banner and `r.changes` to list
/// the staged writes — there is no `ok` field in the JS shape, so none here.
#[tauri::command]
fn pending() -> Value {
    let store = open_store();
    let changes: Vec<Value> = store
        .read_pending()
        .iter()
        .map(|c| {
            json!({
                "op": c.get("op").cloned().unwrap_or(Value::Null),
                "real": c.get("real").cloned().unwrap_or(Value::Null),
                "shadow": c.get("shadow").cloned().unwrap_or(Value::Null),
                "bytes": c.get("bytes").cloned().unwrap_or(json!(0)),
                "at": c.get("at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "sandbox": store.sandbox(), "changes": changes })
}

/// `Store.defaultSettings()` / `readSettings()` live in `core::settings`, next
/// to the atomic write they belong with — the command layer only adapts shapes.
#[tauri::command]
fn get_settings() -> Value {
    core::settings::read(&store_root())
}

/// `Store.saveSettings()`. Returns the JS envelope `{ ok, settings }`; a failed
/// write reports `ok: false` rather than a success the disk never saw.
#[tauri::command]
fn save_settings(settings: Value) -> Value {
    core::settings::save(&store_root(), &settings)
}

#[tauri::command]
fn set_sandbox(on: bool) -> serde_json::Value {
    let root = store_root();
    remember_sandbox(on);
    core::settings::set_sandbox_meta(&root, on);
    serde_json::json!({
        "ok": true,
        "sandbox": on,
        "note": if on {
            "all writes are routed to the sandbox tree"
        } else {
            "writes now hit real agent directories"
        }
    })
}

#[tauri::command]
fn remove_project(id: String) -> Value {
    let root = store_root();
    core::registry::ensure_loaded(&root);
    core::registry::remove(&id)
}

/// `ak:pick-project` — multi-select folder picker, then `registry.add` per
/// folder. Cancelling must report `{ok:false, canceled:true}`: the renderer
/// checks `r.canceled` before touching `r.added`.
#[tauri::command]
async fn pick_project(app: tauri::AppHandle) -> Value {
    use tauri_plugin_dialog::DialogExt;
    let picked = app
        .dialog()
        .file()
        .set_title("Select one or more project folders to import")
        .blocking_pick_folders();
    let Some(paths) = picked else {
        return json!({ "ok": false, "canceled": true });
    };
    let root = store_root();
    core::registry::ensure_loaded(&root);
    let mut added_items: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for p in paths {
        let dir = p.to_string();
        let res = core::registry::add(&dir, None);
        if res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            added_items.push(res.get("project").cloned().unwrap_or(Value::Null));
        } else {
            skipped.push(json!({
                "dir": dir,
                "reason": res.get("error").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    json!({ "ok": true, "added": added_items, "skipped": skipped })
}

/// `Keeper.readAsset()` — the asset drawer's only data source.
///
/// Async + `spawn_blocking` because a cold cache triggers a scan; doing that on
/// the UI thread is the exact stall this migration exists to remove.
#[tauri::command]
async fn read_asset(args: AssetArgs) -> Result<Value, String> {
    let Some(asset_id) = args.asset_id else {
        return Ok(json!({ "ok": false, "error": "assetId is required" }));
    };
    let home = user_home();
    let root = store_root();
    tauri::async_runtime::spawn_blocking(move || {
        core::assets::read_asset(&home, &root, &asset_id)
    })
    .await
    .map_err(|e| format!("read_asset task failed: {}", e))
}

// --- write path: store/keeper transaction layer ---------------------------
//
// Every mutation goes through `Store::transaction` (validate → backup → atomic
// rename → journal), so a rollback always has something to restore. These used
// to answer `{ ok: true }` without touching a file; a fake success is worse than
// a refusal, because silently-wrong data looks exactly like correct data.

fn missing_asset_id() -> Value {
    json!({ "ok": false, "error": "assetId is required" })
}

#[tauri::command]
fn save_markdown(args: AssetArgs) -> Value {
    let (Some(asset_id), Some(content)) = (args.asset_id, args.content) else {
        return missing_asset_id();
    };
    open_keeper().save_markdown(&asset_id, &content)
}

/// Line diff between the file on disk and the editor buffer.
#[tauri::command]
fn diff(args: AssetArgs) -> Value {
    let Some(asset_id) = args.asset_id else {
        return missing_asset_id();
    };
    open_keeper().diff(&asset_id, args.content.as_deref().unwrap_or(""))
}

#[tauri::command]
fn save_skill(payload: Value) -> Value {
    open_keeper().save_skill(&payload)
}

#[tauri::command]
fn upsert_mcp(payload: Value) -> Value {
    open_keeper().upsert_mcp(&payload)
}

#[tauri::command]
fn delete_mcp(args: AssetArgs) -> Value {
    let Some(asset_id) = args.asset_id else {
        return missing_asset_id();
    };
    open_keeper().delete_mcp(&asset_id)
}

#[tauri::command]
fn delete_asset(args: AssetArgs) -> Value {
    let Some(asset_id) = args.asset_id else {
        return missing_asset_id();
    };
    open_keeper().delete_asset(&asset_id)
}

/// `ak:distribute` — copy one asset to other agents' / projects' directories.
///
/// One directory copy plus one `store.transaction` per target, so this is heavy
/// IO by the rule above (it was a synchronous `fn`, which meant distributing to
/// five agents froze the window for the whole copy).
#[tauri::command]
async fn distribute(args: AssetArgs) -> Value {
    let Some(asset_id) = args.asset_id else {
        return missing_asset_id();
    };
    let targets = args.targets.unwrap_or_default();
    if targets.is_empty() {
        return json!({ "ok": false, "error": "targets is required" });
    }
    tauri::async_runtime::spawn_blocking(move || open_keeper().distribute(&asset_id, &targets))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("distribute task failed: {}", e) }))
}

/// Seed the asset cache before a baseline read/write.
///
/// Both baseline commands are only meaningful over a scan, and the cache starts
/// empty after launch, so a cold Health page used to run the *whole* scan on the
/// IPC thread. Shared here so the two commands cannot drift apart on when they
/// scan.
fn ensure_scan_cache(root: &str) {
    if core::assets::cached_count() > 0 {
        return;
    }
    let home = user_home();
    let projects = core::scan::read_projects(root);
    let _ = core::scan::scan_payload(&home, &projects);
}

/// `ak:baseline-diff` — compares the current scan against `baseline.json`.
/// Read-only, and the one call the Health page blocks on — which is why it must
/// not block the IPC thread: on a cold cache it runs a full scan first.
#[tauri::command]
async fn baseline_diff() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(|| {
        let root = store_root();
        ensure_scan_cache(&root);
        core::settings::baseline_diff(&root, &core::assets::fingerprints())
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("baseline task failed: {}", e) }))
}

#[tauri::command]
async fn set_baseline() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(|| {
        let root = store_root();
        ensure_scan_cache(&root);
        core::settings::set_baseline(&root, &core::assets::fingerprints())
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("baseline task failed: {}", e) }))
}

/// `ak:rollback` — restore one transaction's backups. Copies whole directories
/// back, so it belongs on the blocking pool.
#[tauri::command]
async fn rollback(tx_id: String) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || open_store().rollback(&tx_id))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("rollback task failed: {}", e) }))
}

/// Apply every staged change onto the real paths — one transaction, so a
/// partial apply cannot happen. The scan cache is dropped afterwards for the
/// same reason `keeper.lastScan = null` exists in the JS: the assets the UI
/// holds ids for no longer describe what is on disk.
#[tauri::command]
async fn apply_pending() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(|| {
        let mut store = open_store();
        let r = store.apply_pending();
        if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            core::assets::invalidate();
        }
        r
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("apply task failed: {}", e) }))
}

#[tauri::command]
async fn discard_pending() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(|| {
        let store = open_store();
        let r = store.discard_pending();
        if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            core::assets::invalidate();
        }
        r
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("discard task failed: {}", e) }))
}

/// `ak:audit` → `Store.readAudit()`. Returns a **plain array**: the renderer
/// calls `audit.filter(...)` on it directly.
///
/// Heavy on the blocking pool: `audit.jsonl` is append-only and unbounded, so
/// this reads and parses the whole journal on every call (it only stops adding
/// to the result at `limit`, after parsing).
#[tauri::command]
async fn audit(opts: serde_json::Value) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || {
        let requested = opts.get("limit").and_then(|v| v.as_u64()).unwrap_or(300) as usize;
        let limit = requested.clamp(1, 2000);
        let s = |key: &str| {
            opts.get(key)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
        };
        let (kind, agent_id, project_id) = (s("kind"), s("agentId"), s("projectId"));
        Value::Array(core::settings::read_audit(
            &store_root(),
            limit,
            kind.as_deref(),
            agent_id.as_deref(),
            project_id.as_deref(),
        ))
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("audit task failed: {}", e) }))
}

/// `ak:transactions` → `Store.listTransactions()`. Also a plain array.
///
/// One `read_dir` over `backups/` plus a `tx.json` parse per directory, i.e. a
/// directory walk on the blocking pool.
#[tauri::command]
async fn transactions(limit: Option<usize>) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || {
        Value::Array(core::settings::list_transactions(
            &store_root(),
            limit.unwrap_or(120),
        ))
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("transactions task failed: {}", e) }))
}

/// `ak:local-stats` → `{ ok, counts: { scans, writes, rollbacks, failed } }`.
///
/// Counted from `audit.jsonl` on every call, not stored: the journal is already
/// the source of truth and a second counter could disagree with it. Nothing
/// leaves the machine — this only feeds the Settings row. The full-journal parse
/// is why it runs on the blocking pool.
#[tauri::command]
async fn local_stats() -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(|| {
        let path = std::path::Path::new(&store_root()).join("audit.jsonl");
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let (mut scans, mut writes, mut rollbacks, mut failed) = (0u64, 0u64, 0u64, 0u64);
        for line in raw.split('\n').filter(|l| !l.is_empty()) {
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match rec.get("action").and_then(|v| v.as_str()) {
                Some("scan") => scans += 1,
                Some("rollback") => rollbacks += 1,
                Some("apply") => writes += 1,
                _ => {}
            }
            if rec.get("status").and_then(|v| v.as_str()) == Some("failed") {
                failed += 1;
            }
        }
        serde_json::json!({
            "ok": true,
            "counts": { "scans": scans, "writes": writes, "rollbacks": rollbacks, "failed": failed }
        })
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("stats task failed: {}", e) }))
}

/// `ak:ui-error` — the renderer's crash channel.
///
/// One JSON object per line appended to `<store_root>/ui-errors.jsonl`, trimmed
/// to the newest 200 lines (read-trim-write, oldest dropped first) so a renderer
/// stuck in an error loop cannot grow the file without bound. `message` and
/// `stack` are each cut to 4096 bytes on a UTF-8 char boundary. Never panics: a
/// failed write answers `{"ok":false,"error":...}` and goes to stderr; success
/// answers `{"ok":true,"count":<lines kept>}`.
///
/// Synchronous on purpose: bounded file, bounded payload, and it is the *last*
/// thing a broken renderer does — routing it through the blocking pool would
/// queue it behind whatever work the crash is complaining about.
#[tauri::command]
fn ui_error(payload: Value) -> Value {
    core::settings::ui_error(&store_root(), &payload)
}

/// `searchRegistry` — real network, 6 s deadline, never throws. Off the UI
/// thread because DNS on a cold resolver is easily the slowest thing here.
#[tauri::command]
async fn search_registry(opts: Value) -> Value {
    tauri::async_runtime::spawn_blocking(move || core::discover::search_registry(&opts))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("registry task failed: {}", e) }))
}

/// `ak:install-skill` — the agent check runs before anything else, exactly as
/// in the IPC handler, so an unknown id fails without spawning git.
///
/// The clone + `store.transaction` half needs stage 5. Until then it fails
/// loudly rather than reporting an install that never happened.
#[tauri::command]
async fn install_skill(payload: Value) -> Value {
    let agent_id = payload.get("agentId").and_then(|v| v.as_str()).unwrap_or("");
    let repo = payload.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    if !AGENTS.iter().any(|a| a.id == agent_id) {
        return json!({ "ok": false, "error": "unknown agent" });
    }
    let repo = repo.to_string();
    let agent_id = agent_id.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let keeper = open_keeper();
        core::discover::install_from_repo(&keeper, &repo, &agent_id)
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("install task failed: {}", e) }))
}

/// `ak:export-bundle` — pick a destination folder, then write the bundle there.
/// Cancelling reports `{ok:false, canceled:true}` so the renderer does not toast
/// a failure the user caused on purpose.
#[tauri::command]
async fn export_bundle(app: tauri::AppHandle, asset_ids: Vec<String>) -> Value {
    use tauri_plugin_dialog::DialogExt;
    let Some(dir) = app
        .dialog()
        .file()
        .set_title("Export bundle")
        .blocking_pick_folder()
    else {
        return json!({ "ok": false, "canceled": true });
    };
    let ids: Vec<Value> = asset_ids.into_iter().map(Value::String).collect();
    let dir = dir.to_string();
    tauri::async_runtime::spawn_blocking(move || open_keeper().export_bundle(&ids, &dir))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("export task failed: {}", e) }))
}

/// `ak:import-bundle` — pick the folder that *contains* `agentkeeper-bundle/`.
#[tauri::command]
async fn import_bundle(app: tauri::AppHandle) -> Value {
    use tauri_plugin_dialog::DialogExt;
    let Some(dir) = app
        .dialog()
        .file()
        .set_title("Import bundle")
        .blocking_pick_folder()
    else {
        return json!({ "ok": false, "canceled": true });
    };
    let dir = dir.to_string();
    tauri::async_runtime::spawn_blocking(move || open_keeper().import_bundle(&dir))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("import task failed: {}", e) }))
}

// --- update channels ------------------------------------------------------
//
// The state machine lives in `core::update::manager`; these commands are the
// thin adapter the renderer's `window.ak.update*` surface expects. A failed
// update action returns `{ok:false,error}` rather than throwing, because the
// renderer's promise chain has a single `if (!r.ok) toast(r.error)` path.

fn upd<T: std::fmt::Display>(r: Result<Value, T>) -> Value {
    r.unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }))
}

fn update_manager() -> &'static core::update::manager::UpdateManager {
    core::update::manager::ensure_manager(&store_root())
}

#[tauri::command]
fn update_status() -> serde_json::Value {
    update_manager().status()
}

#[tauri::command]
async fn update_check() -> Value {
    tauri::async_runtime::spawn_blocking(|| update_manager().check(true))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("update task failed: {}", e) }))
}

/// `ak:update-settings` — `ipc.js:259`.
///
/// The JS handler validates the patch before the manager sees it, and so does
/// this one. The guard is not in [`UpdateManager::save_settings`] because
/// `clampMinutes` has to stay reachable: `save_settings` clamps `0` → `1`, and
/// the IPC layer rejects the `0` before it gets there. See
/// [`UpdateManager::save_settings_checked`], which is what keeps that ordering
/// under test.
#[tauri::command]
fn update_save_settings(patch: serde_json::Value) -> serde_json::Value {
    upd(update_manager().save_settings_checked(&patch))
}

/// `ak:update-token` — `ipc.js:296`.
///
/// `null` clears the token (`set_token(None)`); a string replaces it; a body
/// without a `token` key is an error, never a silent delete.
///
/// The parameter is the **raw request body**, not `token: Option<Value>`, and
/// there is a concrete reason. The renderer's "clear" button sends `null`
/// (`app.js:1802` → `tauri-bridge.js:60`), which a `String` parameter cannot
/// deserialize — that was BUG-1: the invoke was rejected and the click reported
/// an error instead of clearing. `Option<Value>` deserializes, but cannot
/// finish the job: Tauri's `CommandItem::deserialize_option`
/// (`tauri-2.12.4/src/ipc/command.rs:146-157`) calls `visit_none()` both for
/// a missing key and for `Value::Null`, so `{}` and `{token:null}` are the same
/// `None` by the time this body runs. `ipc.js:299` separates exactly those two
/// cases, so the body is what has to reach [`core::update::manager::token_arg`].
#[tauri::command]
fn update_set_token(request: tauri::ipc::Request<'_>) -> serde_json::Value {
    match request.body() {
        tauri::ipc::InvokeBody::Json(payload) => {
            upd(update_manager().set_token_checked(payload))
        }
        // `asObject(payload) || {}` — a non-object body has no `token` field.
        _ => json!({
            "ok": false,
            "error": core::update::manager::ERR_TOKEN_FIELD_REQUIRED
        }),
    }
}

#[tauri::command]
async fn update_download(url: String, name: String) -> Value {
    tauri::async_runtime::spawn_blocking(move || {
        upd(update_manager().start_download(&url, Some(&name)))
    })
    .await
    .unwrap_or_else(|e| json!({ "ok": false, "error": format!("download task failed: {}", e) }))
}

#[tauri::command]
fn update_progress() -> serde_json::Value {
    update_manager().progress()
}

#[tauri::command]
fn update_cancel() -> serde_json::Value {
    update_manager().cancel()
}

/// Starts the installer. Spawning a process is a blocking wait on the OS
/// (shell-Execute plus the child's first second), so it runs on the blocking
/// pool — the user sees the window stay responsive while the installer spins up.
#[tauri::command]
async fn update_install(path: String) -> serde_json::Value {
    tauri::async_runtime::spawn_blocking(move || upd(update_manager().install(&path)))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("install task failed: {}", e) }))
}

#[tauri::command]
fn update_skip(version: String) -> serde_json::Value {
    update_manager().skip(Some(&version))
}

// --- automatic checks -----------------------------------------------------
//
// `main.js` owned this half of the update feature in the Electron build
// (`startUpdateScheduler`): a 30 s timer calling `update.tick()` and forwarding
// the result to the renderer as `ak:update-event`. The Tauri shell had neither
// the timer nor the emit, so `UpdateManager::tick` was reachable only from
// tests, the `updateCheck` setting was a switch wired to nothing, and
// `tauri-bridge.js`'s `onUpdateEvent` listened for an event nobody sent (B2).

/// Wake-up interval. Deliberately a fixed tick rather than
/// `interval * 60_000`: the configured interval can be 24 hours, and a long
/// sleep drifts across suspend/resume (a laptop that wakes hours late would
/// check hours late, or never). Each tick re-reads the persisted deadline, so a
/// suspended machine checks once after waking.
const UPDATE_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// First pass after boot (`main.js` used `setTimeout(tick, 4000)`). Not a
/// request in itself — [`core::update::manager::UpdateManager::tick`] answers
/// `None` unless a check is actually due, so restarting the app does not burn
/// the anonymous API quota.
const UPDATE_FIRST_TICK: std::time::Duration = std::time::Duration::from_secs(4);

/// Granularity of a scheduler wait, so a shutdown request is noticed within
/// this long instead of after a whole interval.
const SCHEDULER_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// Raised when the app is exiting; the scheduler polls it between slices.
static UPDATE_SCHEDULER_STOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Sleep up to `total`, waking every [`SCHEDULER_SLICE`] to look at `stop`.
///
/// `false` = the wait was cut short by a stop request, `true` = the whole delay
/// elapsed. Split out of the thread so the shutdown behaviour can be tested
/// without a window and without waiting 30 s for a case that must not take 30 s.
pub(crate) fn scheduler_wait(
    stop: &std::sync::atomic::AtomicBool,
    total: std::time::Duration,
) -> bool {
    use std::sync::atomic::Ordering;
    let mut left = total;
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        if left.is_zero() {
            return true;
        }
        let slice = if left < SCHEDULER_SLICE {
            left
        } else {
            SCHEDULER_SLICE
        };
        std::thread::sleep(slice);
        left -= slice;
    }
}

/// The scheduler thread body.
///
/// `tick()` is the entire policy: it re-reads `updateCheck` /
/// `updateCheckMinutes` from `settings.json` on every call and returns `None`
/// when auto-check is off, when a check is already in flight, or before the
/// persisted deadline. So "auto-check off" costs zero network requests and no
/// scheduling rule is duplicated here — the thread only decides *when to ask*.
fn update_scheduler_loop(app: tauri::AppHandle) {
    use tauri::Emitter;
    trace("[update] scheduler started");
    let mut delay = UPDATE_FIRST_TICK;
    loop {
        if !scheduler_wait(&UPDATE_SCHEDULER_STOP, delay) {
            break;
        }
        delay = UPDATE_TICK;
        let Some(payload) = update_manager().tick() else {
            continue;
        };
        // A failed fetch is an event to log, never a panic: `tick()` already
        // stored `lastError` and the next interval retries.
        if payload.get("ok") == Some(&Value::Bool(false)) {
            let message = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            eprintln!("[update] automatic check failed: {}", message);
        }
        // The payload is the same object `updateStatus` returns, which is what
        // the renderer consumed from Electron's `ak:update-event`
        // (`app.js:225` assigns it to `S.update` and reads `hasUpdate`,
        // `skipped`, `latest`, `current`, `notes`).
        if let Err(e) = app.emit("update-event", payload) {
            eprintln!("[update] could not deliver update-event: {}", e);
        }
    }
    trace("[update] scheduler stopped");
}

/// Start the scheduler once, from `setup`. A thread that cannot be spawned is
/// logged rather than turned into a startup failure: automatic updates are not
/// worth refusing to open the app over.
fn start_update_scheduler(app: &tauri::AppHandle) {
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("ak-update-scheduler".into())
        .spawn(move || update_scheduler_loop(handle));
    if let Err(e) = spawned {
        eprintln!("[update] scheduler thread could not start: {}", e);
    }
}

/// `ak:update-open-release` — `ipc.js:347`. Only an http(s) URL reaches the OS
/// handler; anything else is `ipc.invalidUrl`.
#[tauri::command]
fn open_release_page(app: tauri::AppHandle, url: Option<String>) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    let target = match url {
        Some(ref text)
            if text.starts_with("http://") || text.starts_with("https://") =>
        {
            text.clone()
        }
        _ => return json!({ "ok": false, "error": "invalid url" }),
    };
    match app.opener().open_url(&target, None::<&str>) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("{}", e) }),
    }
}

/// `ak:open-path` — reveal a *real* path in the file manager.
///
/// The path is resolved through `store.real()` first: in sandbox mode the path
/// the UI holds is the shadow one, and revealing a file that only exists in the
/// mirror would be a confusing "not found" from Explorer.
#[tauri::command]
fn open_path(app: tauri::AppHandle, path: String) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    if path.is_empty() {
        return json!({ "ok": false, "error": "path required" });
    }
    let real = open_store().real(&path);
    if !std::path::Path::new(&real).exists() {
        return json!({
            "ok": false,
            "error": "path not found (in sandbox the real path may not exist yet)",
        });
    }
    match app.opener().reveal_item_in_dir(&real) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("{}", e) }),
    }
}

/// `ak:open-store` — open the store directory in the file manager.
///
/// Named `..._dir` because the plain `open_store()` above already owns that
/// identifier for the `Store` constructor; Tauri would otherwise register two
/// commands under one name.
#[tauri::command]
fn open_store_dir(app: tauri::AppHandle) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    let root = store_root();
    let _ = app.opener().open_path(&root, None::<&str>);
    json!({ "ok": true, "storeRoot": root })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // This host cannot initialise the Chromium OS sandbox — the same failure
    // the Electron build documents in src/main/main.js (render-process-gone
    // 0x80000003 unless --no-sandbox). WebView2 inherits the same sandbox, and
    // without this the runtime process never even starts: no
    // msedgewebview2.exe child, no page_load event, window never shown.
    //
    // Env-var form was tried first and works, but it cannot be relied on for a
    // shipped build, so it is set in-process before the runtime loads.
    // Safety is unaffected: contextIsolation-equivalent (no node integration),
    // a restrictive CSP, and no plugin grants to fs/network.
    if std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_err() {
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            "--no-sandbox --disable-gpu-sandbox",
        );
        trace("set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--no-sandbox --disable-gpu-sandbox");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // The window is built here rather than declared in tauri.conf.json:
        // `on_page_load` is a consuming Builder hook, so it can only be
        // attached while the window is being constructed. Declaring the window
        // in config would work too, but then the window would have to be shown
        // unconditionally, reintroducing the unpainted flash that Electron
        // avoided with `show: false` + `ready-to-show`.
        .setup(|app| {
            use tauri::webview::PageLoadEvent;
            let handle = app.handle().clone();

            // AK_TEST_URL lets the harness swap the page without a rebuild, so
            // "WebView2 is broken" and "our assets are broken" can be told
            // apart without guessing. about:blank needs no asset serving at
            // all, so a Finished event there proves the runtime works.
            let url = match std::env::var("AK_TEST_URL") {
                Ok(u) if u == "about:blank" => tauri::WebviewUrl::External("about:blank".parse().unwrap()),
                Ok(u) => tauri::WebviewUrl::External(u.parse().unwrap()),
                Err(_) => tauri::WebviewUrl::App("index.html".into()),
            };

            trace_webview2();
            let win = tauri::WebviewWindowBuilder::new(&handle, "main", url)
            .title("AgentKeeper")
            .inner_size(1440.0, 940.0)
            .min_inner_size(1120.0, 720.0)
            .background_color(tauri::window::Color(244, 245, 247, 255))
            .visible(false)
            .on_page_load(move |w, payload| {
                // Same intent as Electron's `ready-to-show`: paint first,
                // then reveal, so a dark-theme user never sees a white frame.
                trace(&format!("page_load {:?}", payload.event()));
                if matches!(payload.event(), PageLoadEvent::Finished) {
                    trace("window shown on Finished");
                    let _ = w.show();
                    // AK_NAV=<view> (optionally `view:query`) clicks one
                    // sidebar entry after load.
                    //
                    // The renderer has no hash routing, so there is no other way
                    // to screenshot a page that is not the dashboard — and the
                    // Health / Activity / Settings pages are exactly where a
                    // payload-shape regression hides. Env-gated, so a normal
                    // launch never evaluates it.
                    //
                    // The `:query` suffix exists for Discover, which shows an
                    // empty state until someone types: without it the page can
                    // only be photographed blank, which proves nothing about
                    // whether the registry call works.
                    if let Ok(spec) = std::env::var("AK_NAV") {
                        let (view, query) = match spec.split_once(':') {
                            Some((v, q)) => (v.to_string(), Some(q.to_string())),
                            None => (spec.clone(), None),
                        };
                        let view_ok = !view.is_empty()
                            && view
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                        let query_ok = query.as_deref().map_or(true, |q| {
                            q.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ')
                        });
                        if view_ok && query_ok {
                            trace(&format!("nav -> {}{}", view, spec.len() - view.len()));
                            // The click is retried inside the page: `Finished`
                            // fires before the renderer's async bootstrap has
                            // wired the sidebar, so a single immediate click can
                            // land on a handler-less node and do nothing (it did,
                            // silently, on the first attempt).
                            let drive = match &query {
                                None => String::new(),
                                // A second poller, not an inline block: the input
                                // only exists once the view has rendered, which
                                // happens after the click handler returns.
                                Some(q) => format!(
                                    r#"
  var n2 = 0;
  var t2 = setInterval(function(){{
    var qi = document.querySelector('#disc-q'), gb = document.querySelector('#disc-go');
    if (qi && gb) {{ qi.value = "{q}"; gb.click(); clearInterval(t2); }}
    else if (++n2 > 60) {{ clearInterval(t2); }}
  }}, 100);"#,
                                    q = q
                                ),
                            };
                            let js = format!(
                                r#"(function(){{
  var v = "{view}", n = 0;
  var t = setInterval(function(){{
    var el = document.querySelector('[data-view="' + v + '"]');
    if (el) {{ el.click(); clearInterval(t);{drive} }}
    else if (++n > 40) {{ clearInterval(t); }}
  }}, 100);
}})();"#,
                                view = view,
                                drive = drive
                            );
                            let _ = w.eval(&js);
                        } else {
                            trace("nav ignored: invalid view name");
                        }
                    }
                }
            })
            .build()?;

            let _ = win;
            trace("window built");
            // The update scheduler is a thread, not a renderer timer: the check
            // has to run whether or not the user is looking at Settings, and it
            // must outlive a page reload.
            start_update_scheduler(&handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            scan,
            pending,
            get_settings,
            save_settings,
            set_sandbox,
            pick_project,
            remove_project,
            read_asset,
            save_markdown,
            diff,
            save_skill,
            upsert_mcp,
            delete_mcp,
            delete_asset,
            distribute,
            baseline_diff,
            set_baseline,
            audit,
            transactions,
            rollback,
            apply_pending,
            discard_pending,
            local_stats,
            search_registry,
            install_skill,
            export_bundle,
            import_bundle,
            update_status,
            update_check,
            update_save_settings,
            update_set_token,
            update_download,
            update_progress,
            update_cancel,
            update_install,
            update_skip,
            open_release_page,
            open_path,
            open_store_dir,
            ui_error,
        ])
        // `build` + `run(callback)` instead of `run(context)` only so the exit
        // event can raise the scheduler's stop flag: `run(context)` never hands
        // back the event loop.
        .build(tauri::generate_context!())
        .expect("error while building AgentKeeper shell")
        .run(|_handle, event| {
            use std::sync::atomic::Ordering;
            if matches!(
                event,
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
            ) {
                UPDATE_SCHEDULER_STOP.store(true, Ordering::Relaxed);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A closing app must not wait out a 30 s sleep: the wait is sliced and the
    /// stop flag is read between slices. (The thread itself is detached; this is
    /// the only part of its shutdown that can be pinned down by a test.)
    #[test]
    fn scheduler_wait_aborts_within_a_slice_of_a_stop_request() {
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                stop.store(true, Ordering::Relaxed);
            });
            assert!(
                !scheduler_wait(&stop, Duration::from_secs(30)),
                "a stop request must end the wait, not sleep through it"
            );
        });
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "the wait returned only after {:?} of a 30 s delay",
            waited
        );
        assert!(
            waited >= Duration::from_millis(50),
            "returned after {:?}, before the flag was set",
            waited
        );
    }

    /// Without a stop request the delay is honoured in full — the 4 s first pass
    /// and the 30 s interval both rely on the wait not returning early.
    #[test]
    fn scheduler_wait_honours_the_delay_and_a_zero_budget() {
        let stop = AtomicBool::new(false);
        assert!(
            scheduler_wait(&stop, Duration::ZERO),
            "a zero budget waits for nothing"
        );
        let started = Instant::now();
        assert!(
            scheduler_wait(&stop, Duration::from_millis(250)),
            "no stop request means the delay elapses"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "returned after {:?} of a 250 ms delay",
            started.elapsed()
        );
    }
}