//! AgentKeeper 的 Tauri 外壳。
//!
//! 资产扫描器住在 Rust 里（`agentkeeper-core`）：`bootstrap` / `scan`
//! 把真实数据返回给 `src/renderer/app.js` 中的渲染层，而每个命令都以
//! 渲染层实际读取的响应*形状*作答——一个返回 `{}` 的命令什么也证明不了。
//!
//! 项目列表经由 `scan` 到达 UI；项目的添加只走 `pick_project`，
//! 这是渲染层唯一能改动注册表的路径。

// core 是独立的 crate，这样它就不可能碰到任何 Tauri 类型。它在这里以
// `core` 这个名字重新导出，于是下面每一条路径——以及集成测试里的
// `agentkeeper_shell_lib::core::…`——读起来都跟这些模块还住在本 crate
// 里时一模一样。
pub use agentkeeper_core as core;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use core::adapters::{resolve_home, Agent, AGENTS, SHARED_SKILLS_DIR};

/// 载荷形状与渲染层传进来的保持一致。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetArgs {
    pub asset_id: Option<String>,
    pub content: Option<String>,
    /// `distribute` 发的是 `[{agentId, scope, projectId}]`——是对象，不是 id。
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
    // `AK_STORE_ROOT` 优先，其次才是平台默认目录。
    //
    // 它**不是** `<APPDATA>\agentkeeper`：用户数据目录解析成
    // `<APPDATA>\<productName>`——`productName`（"AgentKeeper"）压过包里的
    // `name`——然后再接上一个 `agentkeeper` 子目录。这里搞错不会响亮地
    // 失败，而是悄无声息地从真实存储旁边的空存储起步：没有设置、没有
    // 项目、没有事务日志。已对着本机上真正装着 `settings.json` /
    // `projects.json` 的那个目录核对过。
    if let Ok(explicit) = std::env::var("AK_STORE_ROOT") {
        return explicit;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = match std::env::var("APPDATA") {
        Ok(appdata) => appdata,
        Err(_) => match std::env::var("HOME") {
            // 回退时的锚点是 `~/.config`。
            Ok(home) => format!("{}{}.config", home, sep),
            Err(_) => ".".to_string(),
        },
    };
    format!("{0}{1}AgentKeeper{1}agentkeeper", base, sep)
}

/// 本次调用用的存储。每一个要碰存储的命令都从这里构造它，
/// 于是各命令之间不可能出现存储根不一致。
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
    // 适配器没声明插件时 `projects.plugins` 整个被省掉，
    // 而扫描器对“缺席”和“空”是区别对待的。
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
        // 只要存在就算：任何条目都作数，不只限于目录。
        "installed": std::path::Path::new(home).exists(),
        "globals": paths_json(&agent.globals),
        "projects": project_paths_json(&agent.projects)
    })
}

/// 启动期追踪。打包后的 GUI 进程没法通过 stdout 观察，所以那几个要紧的
/// 里程碑改为追加进一个文件。除非 `AK_TRACE` 指向一个可写路径，否则它是
/// 关闭的——发布构建永远不会写这种文件。
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

/// WebView2 运行时的存在情况，在窗口建起来之前记一笔。如果加载器找不到
/// 这个运行时，webview 就永远加载不完，窗口也就永远隐藏着——没有这一行，
/// 它与一个卡死的应用毫无区别。
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
    // 运行时没有在全系统注册时，加载器只认
    // WEBVIEW2_BROWSER_EXECUTABLE_FOLDER 这个环境变量。
    match std::env::var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") {
        Ok(v) => trace(&format!("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER={}", v)),
        Err(_) => trace("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER unset"),
    }
}

// --- IPC 线程预算 ---------------------------------------------------------
//
// 一个命令怎么声明是性能决策，不是风格问题：同步的 `fn` 跑在 **IPC 线程**上，
// 于是它阻塞的一切都是窗口不能重绘、不能弹出“处理中……”提示、不能处理
// 下一次点击的时间。下面这条规则决定了什么时候用 `async` + `spawn_blocking`
// （也就是 `scan` 立下的那个模式）：
//
// * 重 → 放进阻塞池，当命令
//     - 遍历目录树（`scan`、`distribute` 对每个目标的拷贝），
//     - 读写存储不限制大小的文件（`audit.jsonl` 随使用增长；`audit` /
//       `local_stats` 每次调用都要解析整本日志），或
//     - 打开套接字（`search_registry`、`update_check`、`update_download`）
//       或拉起进程（`install_skill`、`export/import_bundle`、
//       `update_install` 这类命令）。
// * 轻 → 保持同步 `fn`，当它的全部 IO 是一份 schema 约束得住的小 JSON
//   文档（`get_settings`、`save_settings`、`update_status`、
//   `update_*_settings`/`token`/`skip`/`progress`/`cancel`）或编辑器早已
//   握在内存里的单个资产文件（`save_markdown`、`diff`、`save_skill`、
//   `upsert_mcp`、那几个删除）。一次线程跳转外加一步序列化什么也换不来，
//   而 `bootstrap` 另外还*必须*是同步的：它在窗口绘制之前运行，
//   所以没有别的命令能抢在它前面答复。
//
// 无论走哪条路，返回形状都一模一样——这里每个命令都以 JSON `Value`（或
// `Result<Value, String>`）作答，渲染层也只会 await 一个 promise。

#[tauri::command]
fn bootstrap() -> Value {
    let root = store_root();
    // 在这里就把存储目录树建出来，意味着首次启动时外壳能对着一个全新的
    // 用户配置工作，而不是因为缺 meta.json 或 audit.jsonl 而失败。
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

/// 首轮播种：只在注册表为空、且随包附带的演示项目确实解析得出来时才做。
///
/// 把它挂在 `bootstrap` 上而不是做成一次性的启动钩子是安全的，因为
/// `registry.add` 会拒绝重复项——所以第二次加载窗口是个空操作，
/// 而不是把每个演示项目再复制一遍。
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

/// 定位随包附带的演示项目种子目录。
///
/// `AK_DEMO_PROJECTS` 优先；否则依次是可执行文件旁边的同级目录（打包版）
/// 或往上几层的目录（开发构建）。
fn demo_project_root() -> Option<String> {
    if let Ok(explicit) = std::env::var("AK_DEMO_PROJECTS") {
        let p = std::path::PathBuf::from(&explicit);
        if p.is_dir() {
            return Some(explicit);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    // 开发构建: target/<profile>/ -> src-tauri -> tauri-shell -> <repo>
    // 打包版本: <install>/         -> <install>
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

/// 真正的扫描器。不在 UI 线程上跑：即便只要约 200ms，把主线程卡在一次
/// 文件遍历上也正是这个命令要避免的那种故障。
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

/// 设置的默认值与读取都住在 `core::settings` 里，紧挨着它们所属的那次
/// 原子写入——命令层只负责适配形状。
#[tauri::command]
fn get_settings() -> Value {
    core::settings::read(&store_root())
}

/// 保存设置。返回 `{ ok, settings }` 信封；写入失败时报告 `ok: false`，
/// 而不是报一个磁盘从未见过的成功。
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

/// `ak:pick-project` —— 多选文件夹选择器，然后对每个文件夹调用一次
/// `registry.add`。取消时必须报告 `{ok:false, canceled:true}`：渲染层会先
/// 检查 `r.canceled`，再决定要不要碰 `r.added`。
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

/// `read_asset` —— 资产抽屉唯一的数据来源。
///
/// 用 async + `spawn_blocking`，因为缓存是冷的时候会触发一次扫描；
/// 在 UI 线程上做这件事会让整个应用卡住，卡的时长等于一次扫描的时长。
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

// --- 写路径：store/keeper 事务层 -------------------------------------------
//
// 每一次变更都走 `Store::transaction`（校验 → 内存内撤销快照 → 原子改名 →
// 审计），所以一次失败的写入总是把目录树留在原样。一次没碰任何文件却答复
// `{ ok: true }` 的写入是假成功——比拒绝更糟，因为悄悄出错的数据
// 看上去与正确的数据一模一样。

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

/// 磁盘上的文件与编辑器缓冲区之间的逐行 diff。
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

/// `ak:distribute` —— 把一个资产拷贝到其他 agent / 项目目录。
///
/// 每个目标一次目录拷贝外加一次 `store.transaction`，所以按上面的规则这属于
/// 重 IO：分发到五个 agent 会让窗口冻结整个拷贝过程。
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

/// 在一次基线读/写之前把资产缓存喂饱。
///
/// 两个基线命令都只在一次扫描的结果之上才有意义，而缓存启动后是空的，
/// 所以冷启动下的健康页否则会在 IPC 线程上跑*整场*扫描。在这里共享出来，
/// 是为了让这两个命令不会在“什么时候扫描”上各走各的。
fn ensure_scan_cache(root: &str) {
    if core::assets::cached_count() > 0 {
        return;
    }
    let home = user_home();
    let projects = core::scan::read_projects(root);
    let _ = core::scan::scan_payload(&home, &projects);
}

/// `ak:baseline-diff` —— 把当前扫描结果与 `baseline.json` 比较。
/// 只读，且是健康页唯一会阻塞等待的调用——正因如此它绝不能阻塞 IPC 线程：
/// 缓存冷的时候它会先跑一整场扫描。
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

/// `ak:audit` → 存储的审计读取器。返回一个**纯数组**：渲染层直接在它上面
/// 调用 `audit.filter(...)`。
///
/// 放在阻塞池上算重活：`audit.jsonl` 是只追加且无上限的，所以每次调用都要
/// 读取并解析整本日志（它只是在解析之后、到了 `limit` 才停止往结果里
/// 添加）。
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

/// `ak:local-stats` → `{ ok, counts: { scans, writes, failed } }` 这样的回应。
///
/// 每次调用都从 `audit.jsonl` 现数，不做存储：日志本身就是唯一事实来源，
/// 再放一个计数器就可能与它不一致。什么都不会离开这台机器——它只喂设置页
/// 那一行。整本日志的解析正是它跑在阻塞池上的原因。
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

/// `ak:ui-error` —— 渲染层的崩溃通道。
///
/// 每行一个 JSON 对象追加到 `<store_root>/ui-errors.jsonl`，并裁剪到最新的
/// 200 行（读取-裁剪-写回，最旧的先丢），这样卡在报错循环里的渲染层就无法
/// 让文件无限增长。`message` 与 `stack` 各自在 UTF-8 字符边界上截到 4096
/// 字节。绝不 panic：写入失败答复 `{"ok":false,"error":...}` 并写 stderr；
/// 成功答复 `{"ok":true,"count":<保留的行数>}`。
///
/// 故意做成同步：文件有界、载荷有界，而且它是坏掉的渲染层做的*最后*一件事
/// ——把它绕去阻塞池，只会让它排在崩溃本身正在抱怨的那堆工作后面。
#[tauri::command]
fn ui_error(payload: Value) -> Value {
    core::settings::ui_error(&store_root(), &payload)
}

/// `search_registry` —— 真网络，6 秒截止，绝不抛错。不在 UI 线程上跑，因为
/// 冷解析器上的 DNS 很容易就是这里最慢的环节。
#[tauri::command]
async fn search_registry(opts: Value) -> Value {
    tauri::async_runtime::spawn_blocking(move || core::discover::search_registry(&opts))
        .await
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("registry task failed: {}", e) }))
}

/// `ak:install-skill` —— agent 校验排在任何事情之前，与 IPC 处理器里完全
/// 一样，这样一个未知 id 会直接失败，不会去拉起 git。
///
/// 克隆 + `store.transaction` 那半边还没接上：它会响亮地失败，而不是报告一次
/// 从未发生过的安装。
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

/// `ak:export-bundle` —— 选一个目标文件夹，然后把 bundle 写到那里。
/// 取消时报告 `{ok:false, canceled:true}`，这样渲染层就不会为用户故意做出的
/// 取消弹一条失败提示。
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

/// `ak:import-bundle` —— 选那个*包含* `agentkeeper-bundle/` 的文件夹。
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

// --- 更新通道 -------------------------------------------------------------
//
// 状态机住在 `core::update::manager` 里；这些命令是渲染层
// `window.ak.update*` 表面所期望的那层薄适配器。一次失败的更新操作返回
// `{ok:false,error}` 而不是抛错，因为渲染层的 promise 链只有一条
// `if (!r.ok) toast(r.error)` 路径。

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

/// `ak:update-releases` —— 关于面板用的更新日志列表。
///
/// 一个 `{ version, date, url, notes, prerelease }` 的**纯数组**，最新的在
/// 前，因为渲染层直接对它做 map；失败时按与其他每个命令相同的约定
/// 答复 `{ ok: false, error }`（`if (!r.ok) toast(r.error)`）。
///
/// 按上面的规则算重活——它会打开套接字——所以放到阻塞池上。
#[tauri::command]
async fn update_releases(limit: Option<usize>) -> Value {
    tauri::async_runtime::spawn_blocking(move || update_releases_from(&store_root(), limit))
        .await
        // 阻塞任务里发生 panic 与其他任何更新任务失败并无二致，而沿用
        // `update task failed` 能让它留在渲染层词典已经翻译过的那句话里
        // ——这里换一种新说法，只会以原始文本的形式落到一个纯英文的 UI 上。
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("update task failed: {}", e) }))
}

/// 调用方没指定上限时 `ak:update-releases` 返回多少条，以及它钳制到的
/// 天花板。
///
/// 这个天花板就是 GitHub 自己的 `per_page` 最大值：要得更多只会悄悄退回
/// 一页，所以一个更大的 `limit` 会是一个这个请求兑现不了的承诺。20 与面板
/// 不用滚动就能显示的数量一致。
const RELEASES_DEFAULT: usize = 20;
const RELEASES_MAX: usize = 100;

/// `ak:update-releases` 的纯逻辑那一半，存储根作为参数传进来。
///
/// 单独拆出来，是为了让测试能把它指向一个临时 profile 和一个回环的 GitHub
/// mock，而不必碰进程全局的 `AK_STORE_ROOT`——与更新集成测试自建
/// `UpdateManager` 而不用 `ensure_manager` 是同一个理由。
///
/// `pub` 只为那个集成测试；IPC 表面是 [`update_releases`]。
///
/// 下面的一切都是从既有的更新流水线上借来的，而不是重写一遍：设置键、
/// [`normalize_repo`] 的校验规则、令牌存储，以及传输层（连同它那套 GitHub
/// 请求头、重定向预算和 4 MiB 响应体上限）。唯一刻意*不*复用的是
/// `pick_listed_release`，它把一份版本列表塌缩成最高的那一个版本——恰好与
/// 更新日志需要的相反。
pub fn update_releases_from(root: &str, limit: Option<usize>) -> Value {
    let wanted = limit.unwrap_or(RELEASES_DEFAULT).clamp(1, RELEASES_MAX);

    let s = core::settings::read(root);
    let text = |key: &str| s.get(key).and_then(|v| v.as_str()).unwrap_or("");
    // 与 `UpdateManager::settings_cfg` 相同的组装方式：不可用的 `updateRepo`
    // 回退到内置那一个，而不是把功能关掉；API 基地址存的时候不带尾斜杠。
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

    // `normalize_repo` 已经把字符集限制住了，所以这个值不需要再做
    // 百分号编码。
    let url = format!("{}/repos/{}/releases?per_page={}", api_root, repo, wanted);
    // 与 `UpdateManager` 相同的凭据来源：UI 保存的令牌压过环境变量，
    // 而令牌缺席时传输层不会加任何请求头。
    let token = core::update::token::TokenStore::new(root).effective();

    let payload = match core::update::http::fetch_release_json(
        &url,
        token.as_deref(),
        core::update::manager::NETWORK_TIMEOUT_MS,
    ) {
        Ok(payload) => payload,
        // `fetch_release_json` 已经把每种失败都措辞成更新面板展示它的样子
        // （传输、限流、HTTP 状态码），而词典覆盖了那些句子，所以这个错误
        // 原样透传，而不是改写成一个第二句尚未翻译的话。
        Err(error) => {
            return json!({ "ok": false, "error": error.to_string() });
        }
    };

    let Some(items) = payload.as_array() else {
        return json!([]);
    };

    let rows: Vec<Value> = items
        .iter()
        // 草稿是一条没有发布日期的未完成公告；把它列进公开更新日志，等于
        // 展示一个谁都装不上的版本。这与 `pick_listed_release` 在挑之前
        // 所用的规则是同一条。
        .filter(|item| item.get("draft") != Some(&Value::Bool(true)))
        .map(|item| {
            // 把 `notes` 截到 8000 字节、以及在 GitHub 省略时合成
            // `pageUrl` 的正是 `release_from_api`；在这里重新推导任何一项，
            // 都会让更新日志与面板的界限不一致。
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
                // `release_from_api` 剥掉了 `v` 前缀，所以面板显示的是
                // `0.2.1` 而不是 `v0.2.1`——与更新横幅的写法一致。
                "version": release.get("tag").cloned().unwrap_or(Value::Null),
                // 从 RFC 3339 时间戳里切出 `YYYY-MM-DD`；解析不了或缺失的
                // 保持为空，而不是变成 "Invalid Date"。
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

/// `ak:update-settings` 命令。
///
/// IPC 处理器会在 manager 看到补丁之前先校验它，这一个也一样。这道守卫不在
/// [`UpdateManager::save_settings`] 里，因为 `clampMinutes` 必须保持可达：
/// `save_settings` 把 `0` 钳成 `1`，而 IPC 层会在这个 `0` 走到那里之前就
/// 拒掉它。见 [`UpdateManager::save_settings_checked`]，正是它让这个先后
/// 顺序处在测试之下。
#[tauri::command]
fn update_save_settings(patch: serde_json::Value) -> serde_json::Value {
    upd(update_manager().save_settings_checked(&patch))
}

/// `ak:update-token` 命令。
///
/// `null` 清除令牌（`set_token(None)`）；字符串替换它；一个没有 `token` 键的
/// 请求体是错误，绝不悄悄删掉。
///
/// 参数是**原始请求体**，而不是 `token: Option<Value>`，这有具体的原因。
/// 渲染层的“清除”按钮发的是 `null`（`app.js:1802`，渲染层的清除按钮），
/// 这是 `String` 参数反序列化不了的——那次 invoke 被拒，点击结果报了一个
/// 错误，而不是把令牌清掉。`Option<Value>` 能反序列化，却完不成这件事：
/// Tauri 的 `CommandItem::deserialize_option`
/// （`tauri-2.12.4/src/ipc/command.rs:146-157`）对缺失的键和 `Value::Null`
/// 都调用 `visit_none()`，所以到这个函数体运行时，`{}` 和 `{token:null}`
/// 已经是同一个 `None`。这两种情况只在原始请求体里才分得开，所以必须让
/// 原始请求体到达 [`core::update::manager::token_arg`]。
#[tauri::command]
fn update_set_token(request: tauri::ipc::Request<'_>) -> serde_json::Value {
    match request.body() {
        tauri::ipc::InvokeBody::Json(payload) => upd(update_manager().set_token_checked(payload)),
        // `asObject(payload) || {}` —— 非对象的请求体没有 `token` 字段。
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

/// 启动安装程序。拉起进程是一次对 OS 的阻塞等待（shell-Execute 加上子进程的
/// 第一秒），所以它跑在阻塞池上——安装程序转起来的过程中，用户看到的是窗口
/// 依旧响应。
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

// --- 自动检查 -------------------------------------------------------------
//
// 一个 30 秒的定时器调用 `UpdateManager::tick()`，并把结果作为
// `update-event` 转给渲染层，`onUpdateEvent` 监听的就是它。由
// `updateCheck` 设置驱动。

/// 唤醒间隔。刻意用一个固定的 tick，而不是 `interval * 60_000`：配置的间隔
/// 可能是 24 小时，而长时间休眠会跨过挂起/恢复漂移（一台晚醒几小时的笔记本
/// 会晚几小时检查，或者干脆不检查）。每个 tick 都重读持久化的截止时间，
/// 所以一台被挂起的机器在唤醒后只检查一次。
const UPDATE_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// 启动后 4 秒的第一轮。它本身不是一次请求——除非确实该检查了，
/// [`core::update::manager::UpdateManager::tick`] 都答 `None`，所以重启应用
/// 不会烧掉匿名 API 配额。
const UPDATE_FIRST_TICK: std::time::Duration = std::time::Duration::from_secs(4);

/// 一次调度等待的粒度，这样关停请求能在这个时长内被察觉，
/// 而不是等满一个完整间隔。
const SCHEDULER_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// 应用退出时被置起；调度器在各切片之间轮询它。
static UPDATE_SCHEDULER_STOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 最多睡 `total`，每 [`SCHEDULER_SLICE`] 醒一次去看 `stop`。
///
/// `false` = 这次等待被一个停止请求截断，`true` = 整个延迟走完。从线程体里
/// 拆出来，是为了在没有窗口、也不用为一个本不该花 30 秒的用例真等 30 秒的
/// 情况下测关停行为。
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

/// 调度线程体。
///
/// `tick()` 就是全部策略：它每次调用都从 `settings.json` 重读 `updateCheck` /
/// `updateCheckMinutes`，并在自动检查关着、一次检查已经在飞行中、或还没到
/// 持久化的截止时间时返回 `None`。于是“自动检查关闭”花费零次网络请求，
/// 也没有任何调度规则在这里被复制一遍——线程只决定*什么时候问*。
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
        // 一次失败的抓取是写进日志的事件，绝不 panic：`tick()` 已经把
        // `lastError` 存好了，下一个间隔会重试。
        if payload.get("ok") == Some(&Value::Bool(false)) {
            let message = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            eprintln!("[update] automatic check failed: {}", message);
        }
        // 这个载荷就是 `updateStatus` 返回的那个对象，也是渲染层从
        // `update-event` 里读的东西（`app.js:225` 把它赋给 `S.update`，并读
        // `hasUpdate`、`skipped`、`latest`、`current` 与 `notes` 这些字段）。
        if let Err(e) = app.emit("update-event", payload) {
            eprintln!("[update] could not deliver update-event: {}", e);
        }
    }
    trace("[update] scheduler stopped");
}

/// 从 `setup` 启动一次调度器。线程起不来就记日志，而不是把它变成一次启动
/// 失败：自动更新不值得为它拒绝打开应用。
fn start_update_scheduler(app: &tauri::AppHandle) {
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("ak-update-scheduler".into())
        .spawn(move || update_scheduler_loop(handle));
    if let Err(e) = spawned {
        eprintln!("[update] scheduler thread could not start: {}", e);
    }
}

/// `ak:update-open-release`。只有 http(s) URL 才会被交给 OS 的处理器；
/// 其他一律是 `ipc.invalidUrl`。
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

/// `ak:open-path` —— 在文件管理器里显示一个路径。
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

/// `ak:open-store` —— 在文件管理器里打开存储目录。
///
/// 名字里带 `..._dir`，是因为上面那个普通的 `open_store()` 已经为 `Store`
/// 构造器占住了这个标识符；否则 Tauri 会把两个命令注册在同一个名字下。
#[tauri::command]
fn open_store_dir(app: tauri::AppHandle) -> serde_json::Value {
    use tauri_plugin_opener::OpenerExt;
    let root = store_root();
    let _ = app.opener().open_path(&root, None::<&str>);
    json!({ "ok": true, "storeRoot": root })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 这台机器初始化不了 Chromium 的 OS 沙箱（不加 --no-sandbox 就是
    // render-process-gone 0x80000003）。WebView2 继承同一个沙箱，缺了这一段
    // 运行时进程根本起不来：没有 msedgewebview2.exe 子进程，没有 page_load
    // 事件，窗口永远不显示。
    //
    // 先试过环境变量的形式，那是可行的，但对一个出货构建没法依赖它，所以在
    // 运行时加载之前就地设置。安全性不受影响：等同于 contextIsolation（没有
    // node 集成）、一条严格的 CSP，以及没有任何授予 fs/网络权限的插件。
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
        // 窗口在这里建，而不是在 tauri.conf.json 里声明：`on_page_load` 是个
        // 消耗 Builder 的钩子，所以只有正在构造窗口时才能挂上去。在配置里声明
        // 窗口也能用，但那样窗口就必须无条件显示，重新引入
        // `show: false` + `ready-to-show` 本来避开的那个未绘制闪烁。
        .setup(|app| {
            use tauri::webview::PageLoadEvent;
            let handle = app.handle().clone();

            // AK_TEST_URL 让测试装置不用重新构建就能换掉页面，这样“WebView2 坏了”
            // 和“我们的产物坏了”不必靠猜就能区分开。about:blank 完全不需要
            // 供给任何资产，所以在那里出现一个 Finished 事件就能证明运行时是好的。
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
                    // 与 `ready-to-show` 同样的意图：先绘制，再显示，这样
                    // 一个深色主题的用户永远不会看到一帧白屏。
                    trace(&format!("page_load {:?}", payload.event()));
                    if matches!(payload.event(), PageLoadEvent::Finished) {
                        trace("window shown on Finished");
                        let _ = w.show();
                        // AK_NAV=<view>（可选 `view:query`）在加载后点击一个
                        // 侧栏条目。
                        //
                        // 渲染层没有 hash 路由，所以没有别的办法去截一张不是
                        // 仪表盘的页面——而健康 / 活动 / 设置这几页恰恰是
                        // 载荷形状回归会藏身的地方。有环境变量开关，所以正常
                        // 启动永远不会走到这里。
                        //
                        // `:query` 后缀是为 Discover 准备的，它在有人输入之前
                        // 一直显示空状态：没有它，这个页面只能被拍成空白，那
                        // 对证明注册表调用是否工作毫无意义。
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
                                // 这次点击在页面内部重试：`Finished` 在渲染层的
                                // 异步 bootstrap 把侧栏接好之前就触发了，所以
                                // 单次即时点击可能落在一个没有处理器的节点上
                                // 而什么都不做（第一次尝试时确实如此，而且无声
                                // 无息）。
                                let drive = match &query {
                                    None => String::new(),
                                    // 第二个轮询器，而不是一段内联写死的代码块：输入框
                                    // 要等视图渲染出来才存在，而那发生在点击处理器
                                    // 返回之后。
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
            // 更新调度器是一个线程，而不是渲染层的定时器：无论用户是不是正
            // 盯着设置页，检查都必须跑，而且它必须活过页面重载。
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
        // 用 `build` + `run(callback)` 而不是 `run(context)`，只为让退出事件
        // 能置起调度器的停止标志：`run(context)` 永远不会把事件循环交回来。
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

    /// 要关闭的应用绝不能等满一次 30 秒的睡眠：等待被切成片，停止标志在片与
    /// 片之间被读取。（线程本身是分离的；这是它关停过程中唯一能被测试钉住的
    /// 部分。）
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

    /// 没有停止请求时延迟会被完整走完——4 秒的第一轮和 30 秒的间隔都依赖
    /// 这次等待不提前返回。
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
