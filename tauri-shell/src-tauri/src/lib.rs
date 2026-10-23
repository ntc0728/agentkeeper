//! AgentKeeper — Tauri shell.
//!
//! The asset scanner lives in Rust (`agentkeeper-core`): `bootstrap` / `scan`
//! return real data to the renderer in `src/renderer/app.js`, and every command
//! answers with the response *shape* the renderer reads — a command that
//! returned `{}` would prove nothing.
//!
//! The project list reaches the UI through `scan`; projects are added through
//! `pick_project`, the only registry mutation path the renderer has.

// The core is a separate crate so that it cannot reach a Tauri type. It is
// re-exported under the name `core`, so every path below — and the integration
// tests' `agentkeeper_shell_lib::core::…` — reads exactly as it did when the
// modules lived in this crate.
pub use agentkeeper_core as core;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use core::adapters::{resolve_home, Agent, AGENTS, SHARED_SKILLS_DIR};

/// Payload shapes match what the renderer passes in.
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
}

fn user_home() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

fn store_root() -> String {
    // `AK_STORE_ROOT` wins, then the platform default directory.
    //
    // It is NOT `<APPDATA>\agentkeeper`: the user-data directory resolves to
    // `<APPDATA>\<productName>` — `productName` ("AgentKeeper") wins over the
    // package `name` — and an `agentkeeper` subdirectory is appended. Getting
    // this wrong does not fail loudly, it silently
    // starts from an empty store beside the real one: no settings, no
    // projects, no transaction journal. Verified against the directory that
    // actually holds `settings.json` / `projects.json` on this machine.
    if let Ok(explicit) = std::env::var("AK_STORE_ROOT") {
        return explicit;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = match std::env::var("APPDATA") {
        Ok(appdata) => appdata,
        Err(_) => match std::env::var("HOME") {
            // The fallback anchor is `~/.config`.
            Ok(home) => format!("{}{}.config", home, sep),
            Err(_) => ".".to_string(),
        },
    };
    format!("{0}{1}AgentKeeper{1}agentkeeper", base, sep)
}

/// The store for this call. Every command that touches the store builds it
/// through here, so the store root can never disagree between them.
fn open_store() -> core::store::Store {
    core::store::Store::new(&store_root())
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
    // `projects.plugins` is omitted entirely when the adapter declares none,
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
        // Presence is enough: any entry counts, not just a directory.
        "installed": std::path::Path::new(home).exists(),
        "globals": paths_json(&agent.globals),
        "projects": project_paths_json(&agent.projects)
    })
}

/// Startup tracing. A packaged GUI process cannot be watched through stdout,
/// so the few milestones that matter are appended to a file instead. Disabled
/// unless `AK_TRACE` names a writable path — a release build never writes one.
fn trace(msg: &str) {
    use std::io::Write;
    let Ok(path) = std::env::var("AK_TRACE") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            f,
            "[{}] {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            msg
        );
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
                    if e.path().join("msedgewebview2.exe").exists() {
                        Some(name)
                    } else {
                        None
                    }
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
//     - walks a directory tree (`scan`, `distribute`'s copy per target),
//     - reads or writes a file whose size the store does not bound
//       (`audit.jsonl` grows with use; `audit`/`local_stats` parse the whole
//       journal on every call), or
//     - opens a socket (`search_registry`, `update_check`, `update_download`)
//       or spawns a process (`install_skill`, `export/import_bundle`,
//       `update_install`).
// * light → stays a synchronous `fn` when its entire IO is one small JSON
//   document the schema bounds (`get_settings`, `save_settings`,
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
    // Creating the store tree here means the shell works against a fresh
    // profile on first launch instead of failing on a missing meta.json or
    // audit.jsonl.
    let _ = core::settings::ensure_store(&root);
    seed_first_run(&root);
    let home = user_home();
    let agents: Vec<Value> = AGENTS
        .iter()
        .filter_map(|a| resolve_home(a.home, &home).map(|h| agent_json(a, &h)))
        .collect();
    json!({
        "storeRoot": root,
        "agents": agents,
        "sharedSkillsDir": resolve_home(SHARED_SKILLS_DIR, &home).unwrap_or_default(),
        "kinds": kinds_meta(),
        "version": env!("CARGO_PKG_VERSION")
    })
}

/// First-run seeding: only when the registry is empty and
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
    let Some(dir) = demo_project_root() else {
        return;
    };
    core::registry::seed(store_root, &dir);
}

/// Locate the bundled demo-project seed directory.
///
/// `AK_DEMO_PROJECTS` wins; otherwise a sibling directory next to the
/// executable (packaged) or a few levels above it (dev build).
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
/// main thread on a file walk is exactly the failure this command prevents.
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

/// Settings defaults and reads live in `core::settings`, next
/// to the atomic write they belong with — the command layer only adapts shapes.
#[tauri::command]
fn get_settings() -> Value {
    core::settings::read(&store_root())
}

/// Saves settings. Returns the `{ ok, settings }` envelope; a failed
/// write reports `ok: false` rather than a success the disk never saw.
#[tauri::command]
fn save_settings(settings: Value) -> Value {
    core::settings::save(&store_root(), &settings)
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

/// `read_asset` — the asset drawer's only data source.
///
/// Async + `spawn_blocking` because a cold cache triggers a scan; doing that on
/// the UI thread would stall the whole app for the length of the scan.
#[tauri::command]
async fn read_asset(args: AssetArgs) -> Result<Value, String> {
    let Some(asset_id) = args.asset_id else {
        return Ok(json!({ "ok": false, "error": "assetId is required" }));
    };
    let home = user_home();
    let root = store_root();
    tauri::async_runtime::spawn_blocking(move || core::assets::read_asset(&home, &root, &asset_id))
        .await
        .map_err(|e| format!("read_asset task failed: {}", e))
}

// --- write path: store/keeper transaction layer ---------------------------
//
// Every mutation goes through `Store::transaction` (validate → in-memory undo
// snapshot → atomic rename → audit), so a failed write always leaves the tree as
// it was. A write that answers `{ ok: true }` without touching a file is a fake
// success — worse than a refusal, because silently-wrong data looks exactly like
// correct data.

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
/// IO by the rule above: distributing to
/// five agents would freeze the window for the whole copy.
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
        .unwrap_or_else(
            |e| json!({ "ok": false, "error": format!("distribute task failed: {}", e) }),
        )
}

/// Seed the asset cache before a baseline read/write.
///
/// Both baseline commands are only meaningful over a scan, and the cache starts
/// empty after launch, so a cold Health page would otherwise run the *whole*
/// scan on the IPC thread. Shared here so the two commands cannot drift apart
/// on when they scan.
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

/// `ak:audit` → the store's audit reader. Returns a **plain array**: the renderer
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

/// `ak:local-stats` → `{ ok, counts: { scans, writes, failed } }`.
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
        let (mut scans, mut writes, mut failed) = (0u64, 0u64, 0u64);
        for line in raw.split('\n').filter(|l| !l.is_empty()) {
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match rec.get("action").and_then(|v| v.as_str()) {
                Some("scan") => scans += 1,
                Some("apply") => writes += 1,
                _ => {}
            }
            if rec.get("status").and_then(|v| v.as_str()) == Some("failed") {
                failed += 1;
            }
        }
        serde_json::json!({
            "ok": true,
            "counts": { "scans": scans, "writes": writes, "failed": failed }
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

/// `search_registry` — real network, 6 s deadline, never throws. Off the UI
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
/// The clone + `store.transaction` half is not wired: it fails loudly rather
/// than reporting an install that never happened.
#[tauri::command]
async fn install_skill(payload: Value) -> Value {
    let agent_id = payload
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
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

/// `ak:update-releases` — the changelog list for the About panel.
///
/// A **plain array** of `{ version, date, url, notes, prerelease }`, newest
/// first, because the renderer maps over it directly; a failure answers
/// `{ ok: false, error }` with the same convention as every other command
/// (`if (!r.ok) toast(r.error)`).
///
/// Heavy by the rule above — it opens a socket — so it goes on the blocking
/// pool.
#[tauri::command]
async fn update_releases(limit: Option<usize>) -> Value {
    tauri::async_runtime::spawn_blocking(move || update_releases_from(&store_root(), limit))
        .await
        // A panic in the blocking task is an update-task failure like any other,
        // and reusing `update task failed` keeps it inside the sentence the
        // renderer dictionary already translates — a new phrasing here would
        // reach an English-only UI as raw text.
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("update task failed: {}", e) }))
}

/// How many releases `ak:update-releases` returns when the caller names no
/// limit, and the ceiling it clamps to.
///
/// The ceiling is GitHub's own `per_page` maximum: asking for more silently
/// returns one page, so a larger `limit` would be a promise the request cannot
/// keep. 20 matches what the panel shows without scrolling.
const RELEASES_DEFAULT: usize = 20;
const RELEASES_MAX: usize = 100;

/// The pure half of `ak:update-releases`, with the store root as a parameter.
///
/// Split out so a test can point it at a temp profile and a loopback GitHub
/// mock without touching the process-global `AK_STORE_ROOT` — same reason the
/// update integration tests build their own `UpdateManager` instead of using
/// `ensure_manager`.
///
/// `pub` for that integration test only; the IPC surface is [`update_releases`].
///
/// Everything below is borrowed from the existing update pipeline rather than
/// reimplemented: the settings keys, [`normalize_repo`]'s validation rule, the
/// token store, and the transport (with its GitHub header set, redirect budget
/// and 4 MiB body cap). The one thing that is deliberately *not* reused is
/// `pick_listed_release`, which collapses a release list to the single highest
/// version — the opposite of what a changelog needs.
pub fn update_releases_from(root: &str, limit: Option<usize>) -> Value {
    let wanted = limit.unwrap_or(RELEASES_DEFAULT).clamp(1, RELEASES_MAX);

    let s = core::settings::read(root);
    let text = |key: &str| s.get(key).and_then(|v| v.as_str()).unwrap_or("");
    // Same assembly as `UpdateManager::settings_cfg`: an unusable `updateRepo`
    // falls back to the built-in one rather than disabling the feature, and the
    // API base is stored without a trailing slash.
    let configured = core::update::manager::normalize_repo(text("updateRepo"));
    let repo = if configured.is_empty() {
        core::update::manager::BUILT_IN_REPO.to_string()
    } else {
        configured
    };
    let api_base = core::ecma_trim(text("updateApiBase"));
    let api_base = api_base.trim_end_matches('/');
    let api_root = if api_base.is_empty() {
        core::update::version::GITHUB_API
    } else {
        api_base
    };

    // `normalize_repo` already restricts the charset, so the value needs no
    // percent-encoding.
    let url = format!("{}/repos/{}/releases?per_page={}", api_root, repo, wanted);
    // Same credential source `UpdateManager` uses: a UI-saved token wins over
    // the environment, and the transport adds no header for an absent one.
    let token = core::update::token::TokenStore::new(root).effective();

    let payload = match core::update::http::fetch_release_json(
        &url,
        token.as_deref(),
        core::update::manager::NETWORK_TIMEOUT_MS,
    ) {
        Ok(payload) => payload,
        // `fetch_release_json` already phrases every failure the way the update
        // panel shows it (transport, rate limit, HTTP status), and the
        // dictionary covers those strings, so the error is passed through as-is
        // rather than reworded into a second, untranslated sentence.
        Err(error) => {
            return json!({ "ok": false, "error": error.to_string() });
        }
    };

    let Some(items) = payload.as_array() else {
        return json!([]);
    };

    let rows: Vec<Value> = items
        .iter()
        // A draft is an unfinished note with no publish date; listing it in a
        // public changelog would show a version nobody can install. This is the
        // same rule `pick_listed_release` applies before it picks.
        .filter(|item| item.get("draft") != Some(&Value::Bool(true)))
        .map(|item| {
            // `release_from_api` is what caps `notes` at 8000 bytes and
            // synthesises a `pageUrl` when GitHub omits one; re-deriving either
            // here would give the changelog different limits than the panel.
            let release = core::update::version::release_from_api(
                item,
                core::update::version::ReleaseOptions {
                    platform: core::update::manager::node_platform(),
                    arch: core::update::manager::node_arch(),
                    api_base,
                    repo: &repo,
                },
            );
            let published = release
                .get("publishedAt")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            json!({
                // `release_from_api` strips the `v` prefix, so the panel shows
                // `0.2.1` rather than `v0.2.1` — same spelling as the update
                // banner.
                "version": release.get("tag").cloned().unwrap_or(Value::Null),
                // `YYYY-MM-DD` out of the RFC 3339 stamp; an unparseable or
                // missing one stays empty rather than becoming "Invalid Date".
                "date": published.get(..10).unwrap_or(""),
                "url": release.get("pageUrl").cloned().unwrap_or(Value::Null),
                "notes": release.get("notes").cloned().unwrap_or(Value::Null),
                "prerelease": release.get("prerelease").cloned().unwrap_or(json!(false)),
            })
        })
        .take(wanted)
        .collect();
    Value::Array(rows)
}

/// `ak:update-settings`.
///
/// The handler validates the patch before the manager sees it, and so does
/// this one. The guard is not in [`UpdateManager::save_settings`] because
/// `clampMinutes` has to stay reachable: `save_settings` clamps `0` → `1`, and
/// the IPC layer rejects the `0` before it gets there. See
/// [`UpdateManager::save_settings_checked`], which is what keeps that ordering
/// under test.
#[tauri::command]
fn update_save_settings(patch: serde_json::Value) -> serde_json::Value {
    upd(update_manager().save_settings_checked(&patch))
}

/// `ak:update-token`.
///
/// `null` clears the token (`set_token(None)`); a string replaces it; a body
/// without a `token` key is an error, never a silent delete.
///
/// The parameter is the **raw request body**, not `token: Option<Value>`, and
/// there is a concrete reason. The renderer's "clear" button sends `null`
/// (`app.js:1802`, the renderer's clear button), which a `String` parameter cannot
/// deserialize — the invoke was rejected and the click reported
/// an error instead of clearing. `Option<Value>` deserializes, but cannot
/// finish the job: Tauri's `CommandItem::deserialize_option`
/// (`tauri-2.12.4/src/ipc/command.rs:146-157`) calls `visit_none()` both for
/// a missing key and for `Value::Null`, so `{}` and `{token:null}` are the same
/// `None` by the time this body runs. Those two cases are separable only in the
/// raw body, so that is what has to reach [`core::update::manager::token_arg`].
#[tauri::command]
fn update_set_token(request: tauri::ipc::Request<'_>) -> serde_json::Value {
    match request.body() {
        tauri::ipc::InvokeBody::Json(payload) => upd(update_manager().set_token_checked(payload)),
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
// A 30 s timer calls `UpdateManager::tick()` and forwards the result to the
// renderer as `update-event`, which is what `onUpdateEvent` listens for. The
// `updateCheck` setting drives it.

/// Wake-up interval. Deliberately a fixed tick rather than
/// `interval * 60_000`: the configured interval can be 24 hours, and a long
/// sleep drifts across suspend/resume (a laptop that wakes hours late would
/// check hours late, or never). Each tick re-reads the persisted deadline, so a
/// suspended machine checks once after waking.
const UPDATE_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// First pass 4 s after boot. Not a
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
        // the renderer reads from `update-event` (`app.js:225` assigns it to
        // `S.update` and reads `hasUpdate`, `skipped`, `latest`, `current`,
        // `notes`).
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

/// `ak:update-open-release`. Only an http(s) URL reaches the OS
/// handler; anything else is `ipc.invalidUrl`.
#[tauri::command]
fn open_release_page(app: tauri::AppHandle, url: Option<String>) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    let target = match url {
        Some(ref text) if text.starts_with("http://") || text.starts_with("https://") => {
            text.clone()
        }
        _ => return json!({ "ok": false, "error": "invalid url" }),
    };
    match app.opener().open_url(&target, None::<&str>) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "error": format!("{}", e) }),
    }
}

/// `ak:open-path` — reveal a path in the file manager.
#[tauri::command]
fn open_path(app: tauri::AppHandle, path: String) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    if path.is_empty() {
        return json!({ "ok": false, "error": "path required" });
    }
    if !std::path::Path::new(&path).exists() {
        return json!({ "ok": false, "error": "path not found" });
    }
    match app.opener().reveal_item_in_dir(&path) {
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
    // This host cannot initialise the Chromium OS sandbox (render-process-gone
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
        // unconditionally, reintroducing the unpainted flash that
        // `show: false` + `ready-to-show` avoids.
        .setup(|app| {
            use tauri::webview::PageLoadEvent;
            let handle = app.handle().clone();

            // AK_TEST_URL lets the harness swap the page without a rebuild, so
            // "WebView2 is broken" and "our assets are broken" can be told
            // apart without guessing. about:blank needs no asset serving at
            // all, so a Finished event there proves the runtime works.
            let url = match std::env::var("AK_TEST_URL") {
                Ok(u) if u == "about:blank" => {
                    tauri::WebviewUrl::External("about:blank".parse().unwrap())
                }
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
                    // Same intent as `ready-to-show`: paint first, then reveal, so
                    // a dark-theme user never sees a white frame.
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
                                q.chars().all(|c| {
                                    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' '
                                })
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
            get_settings,
            save_settings,
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
            local_stats,
            search_registry,
            install_skill,
            export_bundle,
            import_bundle,
            update_status,
            update_check,
            update_releases,
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
