//! stdio MCP 服务器。
//!
//! agent 启动 `agentkeeper-mcp.exe` 并通过 stdin/stdout 讲 MCP：换行分隔的
//! JSON-RPC 2.0。它暴露与参考构建相同的工具，好让 agent 现有的工具列表继续
//! 可用，而每一个会改动内容的工具都走 [`super::keeper::Keeper`] —— 与 UI 使用的
//! 同一层事务，具备相同的校验、审计和失败撤销保证。
//!
//! ## agent 所依赖的契约
//!
//! * stdout 每行携带**恰好**一个紧凑的 JSON 对象；任何别的东西都会破坏这个流。
//!   诊断信息只走 stderr。
//! * `initialize` / `ping` / `tools/list` / `tools/call`，以及对
//!   `notifications/*` 保持沉默，并使用规格所命名的 JSON-RPC 错误码
//!   （`-32601` 未知方法、`-32602` 未知工具、`-32603` 内部错误）。
//! * handler 内部的 panic 会变成一个 `-32603` 响应，绝不是进程退出：
//!   [`dispatch`] 跑在 `catch_unwind` 之后，以一种永远不会以这种方式死掉的方式
//!   镜像了包住 `handle()` 的 `try/catch`。它在 release 构建下同样有效 —— 见
//!   [`dispatch`] 关于 `panic = "unwind"` 的说明。
//! * 工具结果是 `{ content: [{ type: "text", text }] }`，其中 `text` 是
//!   pretty-print 过的 JSON —— 所以里面的字符串会被其他 agent 的 prompt 匹配，
//!   绝不能改写措辞。
//!
//! ## 与规格最朴素读法的三处刻意差异
//!
//! * 格式错误的行以 `-32700 parse error` 作答，而不是被悄悄丢弃：
//!   `handle_line` 绝不能对 agent 确实发来的那一行返回 `None`，否则 agent 会
//!   永远等待一个永远不会到来的回复。空行仍然不产生任何东西（它们只是行尾噪声）。
//! * 带 `ok: false` 的工具结果会带上 `isError: true`（见 [`is_tool_error`]）：
//!   没有它，遵循规格的客户端会把"未找到资产"报告成一次成功应答。内部没有任何
//!   东西会基于这个形状分支；MCP 客户端是外部的，而规格就是契约。

use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock};

use super::assets;
use super::discover;
use super::keeper::Keeper;
use super::model::{js_slice, js_string, js_truthy};
use super::scan;
use super::settings;
use super::store::Store;

/// `initialize` 的应答内容，也是参考构建逐字给出的内容。
const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "agentkeeper";

/// `agentkeeper` 的版本号，与参考构建中 `package.json` 报告的一致。
fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `discover.noInstalledAgent` —— 由 `create_skill` 和 `install_skill` 共用。
const NO_INSTALLED_AGENT: &str = "no installed agent found; pass agentId explicitly";

/// `read_asset` 会把文件正文截断到 60 000 个字符。
const READ_CONTENT_LIMIT: usize = 60_000;

/* ------------------------------------------------------------------ */
/* 工具定义                                                             */
/* ------------------------------------------------------------------ */

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn n(desc: &str) -> Value {
    json!({ "type": "number", "description": desc })
}

fn b(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

/// 声明的工具列表，也就是 `tools/list` 返回的内容。
///
/// 每一个条目都镜像下面的工具表 —— 相同的名字、相同的描述、相同的 `inputSchema`。
/// agent 会匹配这些字符串，所以在这里改名就是对一个已发布 API 的破坏性变更。
pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "list_assets",
            "description": "List agent assets (skills, MCP servers, plugins, rules, memory files) across all detected agents and projects. Supports filtering.",
            "inputSchema": { "type": "object", "properties": {
                "kind": { "type": "string", "enum": ["skill", "mcp", "plugin", "rule", "memory"], "description": "filter by asset kind" },
                "agentId": s("filter by agent id (e.g. claude-code, codex, cursor)"),
                "scope": { "type": "string", "enum": ["global", "project"], "description": "filter by scope" },
                "search": s("case-insensitive substring match on name/description"),
                "limit": n("max rows to return (default 100, max 500)")
            } }
        }),
        json!({
            "name": "read_asset",
            "description": "Read one asset in full: file content, metadata, validation issues and security findings.",
            "inputSchema": { "type": "object", "properties": { "assetId": s("asset id from list_assets") }, "required": ["assetId"] }
        }),
        json!({
            "name": "scan",
            "description": "Re-scan every agent home and project, then return summary counts. Call after any external file change.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "stats",
            "description": "Dashboard summary: totals per kind, per agent, estimated context tokens, issue and shadow counts.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "health_report",
            "description": "Every validation issue and security finding (prompt-injection patterns, credential paths, encoded execution) with evidence.",
            "inputSchema": { "type": "object", "properties": { "includeCached": b("include vendor-cache plugins (default false)") } }
        }),
        json!({
            "name": "create_skill",
            "description": "Create a new skill for an agent (SKILL.md with YAML frontmatter). Validated and security-scanned first, then written in one reversible transaction.",
            "inputSchema": { "type": "object", "properties": {
                "name": s("skill name, lowercase-with-hyphens, must match the folder name"),
                "description": s("what the skill does and when the agent should use it"),
                "body": s("markdown instructions below the frontmatter"),
                "agentId": s("target agent (default: first detected agent)")
            }, "required": ["name", "description", "body"] }
        }),
        json!({
            "name": "update_asset",
            "description": "Replace the file content of a skill/rule/memory asset. Validation runs first; blocked writes never touch disk.",
            "inputSchema": { "type": "object", "properties": {
                "assetId": s("asset id"),
                "content": s("full new file content (frontmatter + body for SKILL.md)")
            }, "required": ["assetId", "content"] }
        }),
        json!({
            "name": "delete_asset",
            "description": "Delete an asset. MCP configs use delete_mcp instead.",
            "inputSchema": { "type": "object", "properties": { "assetId": s("asset id") }, "required": ["assetId"] }
        }),
        json!({
            "name": "distribute_asset",
            "description": "Copy an asset to other agents/projects (one write-through transaction). Only writable kinds.",
            "inputSchema": { "type": "object", "properties": {
                "assetId": s("asset id"),
                "targets": { "type": "array", "items": { "type": "object", "properties": {
                    "agentId": s("target agent"), "scope": { "type": "string", "enum": ["global", "project"] }, "projectId": s("required when scope=project")
                }, "required": ["agentId", "scope"] } }
            }, "required": ["assetId", "targets"] }
        }),
        json!({
            "name": "baseline_diff",
            "description": "Which assets changed/appeared/vanished since the last anchored fingerprint baseline — catches silent edits.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "set_baseline",
            "description": "Anchor the fingerprint baseline to the current scan.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "export_bundle",
            "description": "Export selected assets to an agentkeeper-bundle folder (manifest.json + files/). Absolute path required.",
            "inputSchema": { "type": "object", "properties": {
                "assetIds": { "type": "array", "items": s("asset id") },
                "dir": s("absolute destination directory")
            }, "required": ["assetIds", "dir"] }
        }),
        json!({
            "name": "import_bundle",
            "description": "Import a folder exported by AgentKeeper. One reversible transaction.",
            "inputSchema": { "type": "object", "properties": { "dir": s("directory that contains agentkeeper-bundle/") }, "required": ["dir"] }
        }),
        json!({
            "name": "search_registry",
            "description": "Search the public Agent Skills registry (agentskills.codes) for installable skills. Returns name, author, description, install count and the source GitHub repo.",
            "inputSchema": { "type": "object", "properties": {
                "q": s("search text (matches name, description, author, category)"),
                "limit": n("max rows to return (default 20, max 100)"),
                "offset": n("pagination offset (default 0)")
            } }
        }),
        json!({
            "name": "install_skill",
            "description": "Install skills from a GitHub repo (owner/repo) into an agent's global skills directory. Every skill is validated and security-scanned first, then written in one reversible transaction.",
            "inputSchema": { "type": "object", "properties": {
                "repo": s("GitHub repo as owner/repo or a github.com URL"),
                "agentId": s("target agent (default: first installed agent)")
            }, "required": ["repo"] }
        }),
    ]
}

/* ------------------------------------------------------------------ */
/* JSON 形状的辅助函数                                                   */
/* ------------------------------------------------------------------ */

/// `String(args.x || '')` —— 参考实现中每个 handler 都做的强制转换。
fn arg_str(args: &Value, key: &str) -> String {
    match args.get(key) {
        Some(v) if js_truthy(v) => js_string(v),
        _ => String::new(),
    }
}

/// `Number(v)`，falsy-zero 口径：`Number(x) || default` 在 `0` 上同样会退回
/// 默认值，而这正是 `Math.max(1, Number(args.limit) || 100)` 所依赖的。
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(x)) => x.as_f64().unwrap_or(f64::NAN),
        Some(Value::Bool(x)) => {
            if *x {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::String(x)) => x.trim().parse::<f64>().unwrap_or(f64::NAN),
        Some(Value::Null) | None => 0.0,
        Some(_) => f64::NAN,
    }
}

/// 对应 `Math.min(Math.max(1, Number(args[key]) || default), max)`：`list_assets`
/// 和 `search_registry` 共用的钳位。
fn clamp_count(args: &Value, key: &str, fallback: f64, max: f64) -> usize {
    let raw = js_number(args.get(key));
    let v = if raw.is_nan() || raw == 0.0 {
        fallback
    } else {
        raw
    };
    v.max(1.0).min(max) as usize
}

/// `payload[key]` 以切片形式返回 —— 不去克隆一个可能装着约 1000 行的数组。
fn arr_of<'a>(payload: &'a Value, key: &str) -> &'a [Value] {
    payload
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).map(js_string).unwrap_or_default()
}

/// 只有当该值还不是 `undefined` 时才插入：`JSON.stringify` 会丢弃 `undefined`
/// 值，于是那个键就整个消失了。
fn put(m: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        if !v.is_null() {
            m.insert(key.to_string(), v.clone());
        }
    }
}

/// `x || undefined` —— falsy 值同样会让这个键消失。
fn put_truthy(m: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        if js_truthy(v) {
            m.insert(key.to_string(), v.clone());
        }
    }
}

/* ------------------------------------------------------------------ */
/* MCP 内容信封                                                          */
/* ------------------------------------------------------------------ */

/// 把工具结果包装成 MCP 文本内容。
///
/// `errors.attach` 在这里运行，所以每一个抵达 MCP 客户端的工具失败都带着一个
/// 稳定的 `code`，与英文消息并列 —— agent 可以基于 `asset.notFound` 分支，而不必
/// 去匹配散文。
fn text(result: Value) -> Value {
    // 在 `result` 被消费之前读取：这个标志属于*包装层*（JSON-RPC 响应里的
    // `result.isError`），而 `ok` 位于会被序列化进 `content[0].text` 的负载上。
    let failed = is_tool_error(&result);
    let body = match result {
        Value::String(s) => s,
        other => serde_json::to_string_pretty(&attach(other)).unwrap_or_else(|_| "{}".to_string()),
    };
    let mut out = json!({ "content": [{ "type": "text", "text": body }] });
    if failed {
        out.as_object_mut()
            .unwrap()
            .insert("isError".to_string(), json!(true));
    }
    out
}

/// 把一条英文消息反查回一个 code。
///
/// 这里只索引 MCP 表面可能产生的消息（完整表格还覆盖了 IPC 层）。两条查找规则，
/// 与参考实现相同：先精确匹配，然后是最长的参数化前缀，且其骨架至少 12 个字符 ——
/// 更短的骨架会匹配到仅仅开头相同的散文。
fn attach(result: Value) -> Value {
    let Value::Object(mut m) = result else {
        return result;
    };
    if m.contains_key("code") {
        return Value::Object(m);
    }
    let Some(error) = m
        .get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Value::Object(m);
    };
    if let Some((code, params)) = code_of(&error) {
        m.insert("code".to_string(), Value::String(code.to_string()));
        if !params.is_empty() {
            m.insert("params".to_string(), json!(params));
        }
    }
    Value::Object(m)
}

const EXACT_CODES: [(&str, &str); 16] = [
    ("asset not found", "asset.notFound"),
    (
        "this asset is host-owned (plugin cache / shared dir) and cannot be edited here",
        "asset.hostOwnedEdit",
    ),
    (
        "host-owned asset — delete it from the owning agent UI instead",
        "asset.hostOwnedDelete",
    ),
    ("nothing to write", "write.nothingToWrite"),
    ("target directory does not exist", "write.targetDirMissing"),
    ("corrupt transaction record", "store.corruptTransaction"),
    (
        "only JSON-based MCP config files can be safely edited in place",
        "mcp.jsonOnly",
    ),
    ("config must be an object", "mcp.configNotObject"),
    ("no assets selected", "bundle.noSelection"),
    (
        "nothing exportable in selection",
        "bundle.nothingExportable",
    ),
    ("nothing importable found", "bundle.nothingImportable"),
    ("manifest unreadable", "bundle.manifestUnreadable"),
    (
        "not an agentkeeper bundle (manifest.json missing)",
        "bundle.notBundle",
    ),
    (
        "repo must be owner/repo or a github.com URL",
        "discover.repoFormat",
    ),
    ("nothing installable", "discover.nothingInstallable"),
    (NO_INSTALLED_AGENT, "discover.noInstalledAgent"),
];

/// (前缀, code) —— 最长前缀胜出，所以这个列表按长度排序。
const PREFIX_CODES: [(&str, &str); 12] = [
    (
        "config file is corrupt, refusing to overwrite",
        "store.configCorrupt",
    ),
    ("cannot resolve skill directory for", "skill.dirUnresolved"),
    ("no MCP config file declared for", "mcp.noConfigFile"),
    ("json mutate aborted for", "store.jsonMutateAborted"),
    ("copy source missing", "store.copySourceMissing"),
    ("unsupported bundle schema", "bundle.badSchema"),
    ("no SKILL.md found in", "discover.noSkillMd"),
    ("git clone failed", "discover.cloneFailed"),
    ("install failed", "discover.installFailed"),
    ("registry http", "discover.registryHttp"),
    ("write failed", "write.failed"),
    ("asset not found", "asset.notFoundId"),
];

fn code_of(message: &str) -> Option<(&'static str, Vec<String>)> {
    for (text, code) in EXACT_CODES {
        if message == text {
            return Some((code, Vec::new()));
        }
    }
    for (prefix, code) in PREFIX_CODES {
        if !message.starts_with(prefix) {
            continue;
        }
        // `text.indexOf(':', prefix.length - 1)` —— 参数就是骨架处或之后第一个
        // 冒号之后的全部内容。
        let from = prefix.len().saturating_sub(1);
        let tail = match message[from..].find(':') {
            Some(i) => message[from + i + 1..].trim().to_string(),
            None => message[prefix.len()..]
                .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '—' | '-'))
                .to_string(),
        };
        return Some((code, vec![tail]));
    }
    None
}

/// `list_assets` 和 `read_asset` 为单个资产返回的行形状。
fn brief_asset(a: &Value) -> Value {
    let mut m = Map::new();
    put(&mut m, "id", a.get("id"));
    put(&mut m, "name", a.get("name"));
    put(&mut m, "kind", a.get("kind"));
    put(&mut m, "agentId", a.get("agentId"));
    put(&mut m, "agent", a.get("agentLabel"));
    put(&mut m, "scope", a.get("scope"));
    put_truthy(&mut m, "project", a.get("projectName"));
    put(&mut m, "size", a.get("size"));
    // `a.meta?.estTokens` —— 0 会保留，缺失的 `meta` 会让这个键消失。
    let tokens = a.get("meta").and_then(|meta| meta.get("estTokens"));
    put(&mut m, "tokens", tokens);
    put(&mut m, "writable", a.get("writable"));
    m.insert(
        "issues".to_string(),
        json!(a
            .get("issues")
            .and_then(|v| v.as_array())
            .map(|l| l.len())
            .unwrap_or(0)),
    );
    m.insert(
        "security".to_string(),
        json!(a
            .get("security")
            .and_then(|v| v.as_array())
            .map(|l| l.len())
            .unwrap_or(0)),
    );
    let shadowed = a
        .get("effective")
        .and_then(|e| e.get("live"))
        .and_then(|l| l.as_bool())
        .map(|live| !live)
        .unwrap_or(false);
    m.insert("shadowed".to_string(), Value::Bool(shadowed));
    put(&mut m, "file", a.get("file"));
    Value::Object(m)
}

/* ------------------------------------------------------------------ */
/* 上下文                                                               */
/* ------------------------------------------------------------------ */

/// 存储根目录 —— 与 GUI 相同的解析方式（先看 `AK_STORE_ROOT`）。
///
/// 与 `src/lib.rs` 完全一致：`AK_STORE_ROOT`，否则
/// `<APPDATA>\AgentKeeper\agentkeeper`。MCP 服务器和 GUI 必须落在同一个目录，
/// 否则 agent 的写入对阅读事务日志的人来说就是不可见的。
fn store_root() -> String {
    if let Ok(explicit) = std::env::var("AK_STORE_ROOT") {
        return explicit;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let base = match std::env::var("APPDATA") {
        Ok(appdata) => appdata,
        Err(_) => match std::env::var("HOME") {
            Ok(home) => format!("{}{}.config", home, sep),
            Err(_) => ".".to_string(),
        },
    };
    format!("{0}{1}AgentKeeper{1}agentkeeper", base, sep)
}

fn user_home() -> String {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default()
}

/// `buildContext()` —— 进程生命周期内只有一个 keeper，与在 `main()` 里构建一次
/// 完全相同。
fn context() -> &'static Keeper {
    static CTX: OnceLock<Keeper> = OnceLock::new();
    CTX.get_or_init(|| Keeper::new(Store::new(&store_root()), &user_home()))
}

/// `keeper.lastScan` —— 扫描缓存，用 `Arc` 持有，所以只读它的工具不花任何代价。
/// 天真的版本会在**每一次**工具调用时重新扫描约 1000 个资产（再加上安全检查）
/// —— 对一段 agent 对话来说每次调用 3–8 秒。
fn last_scan() -> &'static Mutex<Option<Arc<Value>>> {
    static LAST: OnceLock<Mutex<Option<Arc<Value>>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(None))
}

fn scan_now(keeper: &Keeper) -> Arc<Value> {
    let projects = scan::read_projects(keeper.store.root_str());
    let (payload, _) = scan::scan_payload(&keeper.user_home, &projects);
    let payload = Arc::new(payload);
    *last_scan().lock().unwrap_or_else(|e| e.into_inner()) = Some(payload.clone());
    payload
}

/// `freshScan()` —— 扫一次，然后复用直到某次改动让它失效。
fn fresh_scan(keeper: &Keeper) -> Arc<Value> {
    if let Some(cached) = last_scan()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return cached;
    }
    scan_now(keeper)
}

/// `invalidate()` —— 扫描结果的寿命不能超过一次写入。
fn invalidate(keeper: &Keeper) {
    *last_scan().lock().unwrap_or_else(|e| e.into_inner()) = None;
    keeper.invalidate();
}

/// 有效资产优先：`keeper.scan().effective || keeper.scan().assets`
fn effective_or_assets(payload: &Value) -> &[Value] {
    let effective = arr_of(payload, "effective");
    if effective.is_empty() {
        arr_of(payload, "assets")
    } else {
        effective
    }
}

/// 扫描 `assets` 上的 `(id, fingerprint)` —— `baselineDiff` / `setBaseline`
/// 所接受的输入。
fn fingerprints(payload: &Value) -> Vec<(String, String)> {
    arr_of(payload, "assets")
        .iter()
        .filter_map(|a| {
            let id = a.get("id").map(js_string)?;
            Some((id, str_of(a, "fingerprint")))
        })
        .collect()
}

/// 第一个已安装的 agent，或 `""` —— `create_skill` 和 `install_skill` 的默认
/// 目标。
fn first_installed_agent(keeper: &Keeper) -> String {
    let payload = fresh_scan(keeper);
    arr_of(&payload, "agents")
        .iter()
        .find(|a| {
            a.get("installed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .map(|a| str_of(a, "id"))
        .unwrap_or_default()
}

/* ------------------------------------------------------------------ */
/* 工具实现                                                             */
/* ------------------------------------------------------------------ */

fn list_assets(keeper: &Keeper, args: &Value) -> Value {
    let payload = fresh_scan(keeper);
    let kind = args.get("kind").and_then(|v| v.as_str());
    let agent_id = args.get("agentId").and_then(|v| v.as_str());
    let scope = args.get("scope").and_then(|v| v.as_str());
    let search = arg_str(args, "search").to_lowercase();
    let limit = clamp_count(args, "limit", 100.0, 500.0);

    let mut rows: Vec<Value> = Vec::new();
    for a in arr_of(&payload, "assets") {
        if let Some(k) = kind {
            if str_of(a, "kind") != k {
                continue;
            }
        }
        if let Some(id) = agent_id {
            if str_of(a, "agentId") != id {
                continue;
            }
        }
        if let Some(sc) = scope {
            if str_of(a, "scope") != sc {
                continue;
            }
        }
        if !search.is_empty() {
            let haystack = format!(
                "{} {}",
                str_of(a, "name").to_lowercase(),
                str_of(a, "description").to_lowercase()
            );
            if !haystack.contains(&search) {
                continue;
            }
        }
        rows.push(brief_asset(a));
    }

    let total = rows.len();
    rows.truncate(limit);
    text(json!({
        "total": total,
        "showing": rows.len(),
        "assets": rows
    }))
}

fn read_asset(keeper: &Keeper, args: &Value) -> Value {
    let id = arg_str(args, "assetId");
    let r = assets::read_asset(&keeper.user_home, keeper.store.root_str(), &id);
    if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return text(json!({ "ok": false, "error": r.get("error").cloned() }));
    }
    let a = r.get("asset").cloned().unwrap_or(Value::Null);
    let mut metadata = Map::new();
    if let Some(meta) = a.get("meta") {
        put(&mut metadata, "transport", meta.get("transport"));
        put(&mut metadata, "version", meta.get("version"));
        put(&mut metadata, "relLabel", meta.get("relLabel"));
    }
    let content = r.get("content").map(js_string).unwrap_or_default();
    text(json!({
        "ok": true,
        "asset": brief_asset(&a),
        "metadata": Value::Object(metadata),
        "issues": a.get("issues").cloned().unwrap_or_else(|| json!([])),
        "security": a.get("security").cloned().unwrap_or_else(|| json!([])),
        "content": js_slice(&content, READ_CONTENT_LIMIT)
    }))
}

fn rescan(keeper: &Keeper) -> Value {
    invalidate(keeper);
    let payload = scan_now(keeper);
    let assets = arr_of(&payload, "assets");
    text(json!({
        "ok": true,
        "total": assets.len(),
        "summary": scan::summarize(effective_or_assets(&payload))
    }))
}

fn stats(keeper: &Keeper, _args: &Value) -> Value {
    let payload = fresh_scan(keeper);
    let summary = scan::summarize(effective_or_assets(&payload));
    let agents: Vec<Value> = arr_of(&payload, "agents")
        .iter()
        .map(|a| {
            json!({
                "id": a.get("id").cloned().unwrap_or(Value::Null),
                "label": a.get("label").cloned().unwrap_or(Value::Null),
                "installed": a.get("installed").and_then(|v| v.as_bool()).unwrap_or(false)
            })
        })
        .collect();
    text(json!({
        "storeRoot": keeper.store.root_str(),
        "total": summary.get("total").cloned().unwrap_or(json!(0)),
        "byKind": summary.get("by").cloned().unwrap_or_else(|| json!({})),
        "byOrigin": summary.get("byOrigin").cloned().unwrap_or_else(|| json!({})),
        "issues": summary.get("issues").cloned().unwrap_or(json!(0)),
        "shadowed": summary.get("shadowed").cloned().unwrap_or(json!(0)),
        "estTokens": summary.get("estTokens").cloned().unwrap_or(json!(0)),
        "agents": agents,
        "projects": arr_of(&payload, "projects").len()
    }))
}

fn health_report(keeper: &Keeper, args: &Value) -> Value {
    let include_cached = js_truthy(args.get("includeCached").unwrap_or(&Value::Null));
    let payload = fresh_scan(keeper);
    let mut out: Vec<Value> = Vec::new();
    for a in arr_of(&payload, "assets") {
        if !include_cached && str_of(a, "origin") == "cached" {
            continue;
        }
        let name = a.get("name").cloned().unwrap_or(Value::Null);
        let id = a.get("id").cloned().unwrap_or(Value::Null);
        let kind = a.get("kind").cloned().unwrap_or(Value::Null);
        for issue in arr_of(a, "issues") {
            out.push(json!({
                "asset": name, "id": id, "kind": kind,
                "level": issue.get("level").cloned().unwrap_or(Value::Null),
                "field": issue.get("field").cloned().unwrap_or(Value::Null),
                "message": issue.get("message").cloned().unwrap_or(Value::Null)
            }));
        }
        for finding in arr_of(a, "security") {
            out.push(json!({
                "asset": name, "id": id, "kind": kind,
                "level": finding.get("severity").cloned().unwrap_or(Value::Null),
                "field": finding.get("field").cloned().unwrap_or(Value::Null),
                "rule": finding.get("rule").cloned().unwrap_or(Value::Null),
                "message": finding.get("reason").cloned().unwrap_or(Value::Null),
                "evidence": finding.get("match").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    let count = out.len();
    out.truncate(200);
    text(json!({ "findings": count, "items": out }))
}

fn create_skill(keeper: &Keeper, args: &Value) -> Value {
    let agent_id = match arg_str(args, "agentId") {
        given if !given.is_empty() => given,
        _ => first_installed_agent(keeper),
    };
    if agent_id.is_empty() {
        return text(json!({ "ok": false, "error": NO_INSTALLED_AGENT }));
    }
    let r = keeper.save_skill(&json!({
        "data": {
            "name": arg_str(args, "name"),
            "description": arg_str(args, "description")
        },
        "body": arg_str(args, "body"),
        "targets": [{ "agentId": agent_id, "scope": "global" }]
    }));
    if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        let mut m = Map::new();
        m.insert("ok".to_string(), Value::Bool(false));
        put(&mut m, "error", r.get("error"));
        m.insert(
            "blocked".to_string(),
            Value::Bool(r.get("blocked").map(js_truthy).unwrap_or(false)),
        );
        put(&mut m, "issues", r.get("issues"));
        return text(Value::Object(m));
    }
    invalidate(keeper); // 新文件必须出现在下一次读取里
    text(json!({
        "ok": true,
        "txId": r.get("txId").cloned().unwrap_or(Value::Null),
        "files": r.get("written").cloned().unwrap_or_else(|| json!([])),
        "note": "written directly"
    }))
}

/// `create_skill` 和 `update_asset` 在写入被拒绝时返回的
/// `{ ok: false, error, blocked, issues }` 信封。
fn refused(r: &Value) -> Value {
    let mut m = Map::new();
    m.insert("ok".to_string(), Value::Bool(false));
    put(&mut m, "error", r.get("error"));
    m.insert(
        "blocked".to_string(),
        Value::Bool(r.get("blocked").map(js_truthy).unwrap_or(false)),
    );
    put(&mut m, "issues", r.get("issues"));
    text(Value::Object(m))
}

fn update_asset(keeper: &Keeper, args: &Value) -> Value {
    let r = keeper.save_markdown(&arg_str(args, "assetId"), &arg_str(args, "content"));
    if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return refused(&r);
    }
    invalidate(keeper);
    // `files` 镜像 `r.written`：当 keeper 没有报告它写了什么时，`undefined` 会被
    // `JSON.stringify` 丢弃、这个键就不存在。在这里发出 `files: []` 会告诉 agent
    // "什么都没写"，而事实是"未报告"。
    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    out.insert(
        "txId".to_string(),
        r.get("txId").cloned().unwrap_or(Value::Null),
    );
    if let Some(written) = r.get("written") {
        out.insert("files".to_string(), written.clone());
    }
    text(Value::Object(out))
}

fn delete_asset(keeper: &Keeper, args: &Value) -> Value {
    // keeper.delete_asset 会在内部把 MCP 配置转交给 delete_mcp —— 在这里传一个
    // mcp 资产只会删除配置条目，绝不会删除那个共享文件。
    let r = keeper.delete_asset(&arg_str(args, "assetId"));
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper);
    }
    text(r)
}

fn distribute_asset(keeper: &Keeper, args: &Value) -> Value {
    let targets = args
        .get("targets")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let r = keeper.distribute(&arg_str(args, "assetId"), &targets);
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper);
    }
    text(r)
}

fn baseline_diff(keeper: &Keeper, _args: &Value) -> Value {
    let payload = fresh_scan(keeper);
    let d = settings::baseline_diff(keeper.store.root_str(), &fingerprints(&payload));
    let mut m = Map::new();
    m.insert("ok".to_string(), Value::Bool(true));
    if let Value::Object(fields) = d {
        for (k, v) in fields {
            m.insert(k, v);
        }
    }
    text(Value::Object(m))
}

fn set_baseline(keeper: &Keeper, _args: &Value) -> Value {
    let payload = scan_now(keeper);
    text(settings::set_baseline(
        keeper.store.root_str(),
        &fingerprints(&payload),
    ))
}

fn export_bundle(keeper: &Keeper, args: &Value) -> Value {
    let ids = args
        .get("assetIds")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    text(keeper.export_bundle(&ids, &arg_str(args, "dir")))
}

fn import_bundle(keeper: &Keeper, args: &Value) -> Value {
    let r = keeper.import_bundle(&arg_str(args, "dir"));
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper);
    }
    text(r)
}

fn search_registry(_keeper: &Keeper, args: &Value) -> Value {
    text(discover::search_registry(&json!({
        "q": arg_str(args, "q"),
        "limit": clamp_count(args, "limit", 20.0, 100.0),
        "offset": js_number(args.get("offset"))
    })))
}

fn install_skill(keeper: &Keeper, args: &Value) -> Value {
    let repo = arg_str(args, "repo");
    let agent_id = match arg_str(args, "agentId") {
        given if !given.is_empty() => given,
        _ => first_installed_agent(keeper),
    };
    if agent_id.is_empty() {
        return text(json!({ "ok": false, "error": NO_INSTALLED_AGENT }));
    }
    let r = discover::install_from_repo(keeper, &repo, &agent_id);
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper); // 这次写入必须出现在下一次读取里
    }
    text(r)
}

/* ------------------------------------------------------------------ */
/* JSON-RPC 管道                                                         */
/* ------------------------------------------------------------------ */

fn respond(id: Option<Value>, result: Value) -> Value {
    // `JSON.stringify({ id: undefined, … })` 会丢弃 `id`；我们也这么做。
    let mut m = Map::new();
    m.insert("jsonrpc".to_string(), json!("2.0"));
    if let Some(id) = id {
        m.insert("id".to_string(), id);
    }
    m.insert("result".to_string(), result);
    Value::Object(m)
}

/// `fail(code, message)` —— 通知的错误会被丢弃。
fn fail(id: Option<Value>, code: i32, message: String) -> Value {
    let Some(id) = id else {
        return Value::Null;
    };
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn call_tool(keeper: &Keeper, req: &Value) -> Value {
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let name = arg_str(&params, "name");
    let args = match params.get("arguments") {
        Some(v) if !v.is_null() => v.clone(),
        _ => json!({}),
    };
    let result = match name.as_str() {
        "list_assets" => list_assets(keeper, &args),
        "read_asset" => read_asset(keeper, &args),
        "scan" => rescan(keeper),
        "stats" => stats(keeper, &args),
        "health_report" => health_report(keeper, &args),
        "create_skill" => create_skill(keeper, &args),
        "update_asset" => update_asset(keeper, &args),
        "delete_asset" => delete_asset(keeper, &args),
        "distribute_asset" => distribute_asset(keeper, &args),
        "baseline_diff" => baseline_diff(keeper, &args),
        "set_baseline" => set_baseline(keeper, &args),
        "export_bundle" => export_bundle(keeper, &args),
        "import_bundle" => import_bundle(keeper, &args),
        "search_registry" => search_registry(keeper, &args),
        "install_skill" => install_skill(keeper, &args),
        _ => {
            return fail(
                req.get("id").cloned(),
                -32602,
                format!("unknown tool: {}", name),
            )
        }
    };
    let failed = is_tool_error(&result);
    let mut out = respond(req.get("id").cloned(), result);
    if failed {
        if let Some(result) = out.get_mut("result").and_then(|r| r.as_object_mut()) {
            result.insert("isError".to_string(), json!(true));
        }
    }
    out
}

/// 工具结果上的 `ok: false`，依据 MCP 的 `tools/call` 契约。
///
/// 业务失败（`{ ok: false, error }`）在这里被标记 `isError: true`。如果只标记
/// *抛出*的 handler，就会让遵循规格的客户端把"未找到资产"渲染成一次成功应答。
///
/// 业务失败不是一个值得保留的行为：消费者是第三方 MCP 客户端，而规格规定工具
/// 执行失败必须被标记。
/// 见模块头部。
fn is_tool_error(result: &Value) -> bool {
    result.is_object() && result.get("ok").and_then(|v| v.as_bool()) == Some(false)
}

/// 纯粹的请求处理器。
///
/// 返回 JSON-RPC 响应对象，或在请求根本不该被应答时返回 `Value::Null`（一条
/// `notifications/*` 消息，或一个没有携带 `id` 的请求上的错误）。`handle_line`
/// 把 `Null` 变成 `None`。
///
/// 函数体跑在 [`guarded`] 之内 —— 它等价于包住 `handle()` 的 `try { … } catch`。
/// 没有它，一次 `unwrap()` 或切片越界就会在流中途杀死进程，而 agent 看到的是
/// 一个被丢弃的传输，而不是它可以报告的错误。
///
/// **为什么 `[profile.release]` 不再写 `panic = "abort"`：** `catch_unwind` 只能
/// 捕获*展开*的 panic，所以在 `abort` 下这个守卫恰好在最要紧的地方 —— 在发布的
/// 二进制里 —— 什么也做不了。`Cargo.toml` 现在刻意为 release profile 保留展开；
/// 那几百 KB 的展开表换来的是一个会回答 `-32603` 而不是中途死掉的服务器。展开
/// 仍然是最后手段，而不是一种设计：一个 panic 的 handler 其实已经让这个请求
/// 失败了。
pub fn dispatch(keeper: &super::keeper::Keeper, req: &Value) -> Value {
    let id = req.get("id").cloned();
    guarded(id, || dispatch_inner(keeper, req))
}

/// 运行 `f`，把一次 panic 变成一个 `-32603 internal error` 响应。
///
/// 响应保留请求的 `id`，所以客户端仍然能匹配上；没有 `id` 的请求回答 `Null`，
/// 与这里其他每一种失败一样。
fn guarded(id: Option<Value>, f: impl FnOnce() -> Value) -> Value {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(resp) => resp,
        Err(payload) => fail(id, -32603, panic_text(payload.as_ref())),
    }
}

/// panic 的负载转成文本。`panic!("literal")` 携带一个 `&str`，
/// `panic!("{}", x)` 携带一个 `String`；其它一切都退回这个实现所应答的固定
/// 消息。
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "internal error".to_string()
}

fn dispatch_inner(keeper: &super::keeper::Keeper, req: &Value) -> Value {
    let id = req.get("id").cloned();
    let method = match req.get("method") {
        None => "undefined".to_string(),
        Some(Value::String(v)) => v.clone(),
        Some(other) => js_string(other),
    };
    match method.as_str() {
        "initialize" => respond(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": server_version() }
            }),
        ),
        "ping" => respond(id, json!({})),
        m if m.starts_with("notifications/") => Value::Null,
        "tools/list" => respond(id, json!({ "tools": tools() })),
        "tools/call" => call_tool(keeper, req),
        _ => fail(id, -32601, format!("method not found: {}", method)),
    }
}

/// 读取一条 JSON-RPC 请求并返回一个响应，或在这一行没有需要应答的内容时返回
/// `None`（空行，或一条通知）。
///
/// 每行一次调用，不做批量：参考服务器行为相同，而一个会流水线化请求的 agent
/// 客户端无论哪种实现都会出问题。
pub fn handle_line(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
        eprintln!("[agentkeeper-mcp] bad json line ignored");
        // 参考实现会悄悄丢弃这一行；发出它的 agent 正在等待一个永远不会到来的
        // 回复，所以这里改用一条解析错误作答。
        return Some(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": { "code": -32700, "message": "parse error" }
        }));
    };
    let resp = dispatch(context(), &msg);
    if resp.is_null() {
        None
    } else {
        Some(resp)
    }
}

/// 运行循环直到 stdin 关闭。返回进程退出码。
pub fn serve() -> i32 {
    let keeper = context();
    eprintln!("[agentkeeper-mcp] store: {}", keeper.store.root_str());

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Some(resp) = handle_line(&line) else {
            continue;
        };
        // 一行紧凑内容，立即刷新：agent 把 stdout 当作流来读，而在这里
        // pretty-print 会破坏它的行解析器。
        let mut payload = serde_json::to_string(&resp).unwrap_or_default();
        payload.push('\n');
        if out.write_all(payload.as_bytes()).is_err() {
            eprintln!("[agentkeeper-mcp] stdout write failed");
            return 1;
        }
        if out.flush().is_err() {
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ak-mcp-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keeper_at(root: &std::path::Path, home: &std::path::Path) -> Keeper {
        Keeper::new(Store::new(&root.to_string_lossy()), &home.to_string_lossy())
    }

    /// 扫描负载缓存是进程全局的，就像 `lastScan`；把它丢掉，这样一个测试绝不会
    /// 读到另一个测试的扫描结果。
    fn clean_slate() {
        *last_scan().lock().unwrap_or_else(|e| e.into_inner()) = None;
        assets::invalidate();
    }

    fn call(keeper: &Keeper, id: u64, name: &str, args: Value) -> Value {
        dispatch(
            keeper,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": args }
            }),
        )
    }

    /// `JSON.parse(result.content[0].text)` —— MCP 客户端所做的事。
    fn payload_of(resp: &Value) -> Value {
        let raw = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("{}");
        serde_json::from_str(raw).unwrap_or(Value::Null)
    }

    #[test]
    fn initialize_reports_the_protocol_and_server_name() {
        let root = temp("init");
        let home = temp("init-home");
        let k = keeper_at(&root, &home);

        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
        );
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], json!("2024-11-05"));
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("agentkeeper"));
        assert!(resp["result"]["serverInfo"]["version"].is_string());
        assert_eq!(
            resp["result"]["capabilities"]["tools"]["listChanged"],
            json!(false)
        );

        // ping 应答一个空结果；通知则什么都不应答。
        let ping = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
        assert_eq!(ping["result"], json!({}));
        assert!(dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_null());
        assert!(
            handle_line("   ").is_none(),
            "a blank line is not a request"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn tools_list_declares_every_tool() {
        let declared = tools();
        let names: Vec<&str> = declared
            .iter()
            .map(|t| t["name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec![
                "list_assets",
                "read_asset",
                "scan",
                "stats",
                "health_report",
                "create_skill",
                "update_asset",
                "delete_asset",
                "distribute_asset",
                "baseline_diff",
                "set_baseline",
                "export_bundle",
                "import_bundle",
                "search_registry",
                "install_skill",
            ]
        );
        for t in tools() {
            assert!(t["description"].as_str().unwrap_or("").len() > 10, "{}", t);
            assert_eq!(t["inputSchema"]["type"], json!("object"), "{}", t);
        }
    }

    #[test]
    fn tools_list_is_wrapped_as_a_json_rpc_result() {
        let root = temp("list");
        let home = temp("list-home");
        let k = keeper_at(&root, &home);
        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }),
        );
        assert_eq!(resp["result"]["tools"][0]["name"], json!("list_assets"));
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 15);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_and_write_tools_round_trip_against_a_real_tree() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("rw");
        let home = temp("rw-home");
        let k = keeper_at(&root, &home);

        let dir = home.join("skills").join("pdf");
        let file = dir.join("SKILL.md");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, "---\nname: pdf\n---\nbody\n").unwrap();
        let asset = json!({
            "id": "skill:pdf",
            "kind": "skill",
            "name": "pdf",
            "agentId": "claude-code",
            "agentLabel": "Claude Code",
            "scope": "global",
            "projectId": Value::Null,
            "dir": dir.to_string_lossy(),
            "file": file.to_string_lossy(),
            "writable": true,
            "size": 12,
            "meta": { "estTokens": 3 },
            "issues": [],
            "security": [],
            "effective": { "live": true }
        });
        assets::remember(std::slice::from_ref(&asset), std::slice::from_ref(&asset));

        // --- 读取工具 --------------------------------------------------
        let out = payload_of(&call(
            &k,
            1,
            "read_asset",
            json!({ "assetId": "skill:pdf" }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["asset"]["name"], json!("pdf"));
        assert_eq!(out["asset"]["agent"], json!("Claude Code"));
        assert_eq!(out["asset"]["tokens"], json!(3));
        // `a.projectName || undefined` —— 这个键是不存在，而不是 null。
        assert!(out["asset"].get("project").is_none(), "{}", out["asset"]);
        assert_eq!(out["content"], json!("---\nname: pdf\n---\nbody\n"));

        // --- 写入工具：落在真实路径上，一个事务 --------
        let out = payload_of(&call(
            &k,
            2,
            "create_skill",
            json!({
                "name": "new-skill",
                "description": "does a thing",
                "body": "body\n",
                "agentId": "claude-code"
            }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        // 这条说明绝不能承诺一次撤销：写入路径只为*失败*情形保留内存快照，所以
        // "reversible" 是一个这个工具已经无法兑现的说法。
        assert_eq!(out["note"], json!("written directly"));
        // claude-code 把全局 skills 放在 `~/.claude/skills`。
        let target = home.join(".claude").join("skills").join("new-skill");
        assert!(
            target.join("SKILL.md").is_file(),
            "the write must reach the real agent tree: {:?}",
            target
        );

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unknown_tool_and_malformed_lines_are_well_formed_errors() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("err");
        let home = temp("err-home");
        let k = keeper_at(&root, &home);

        // 未知工具 → -32602
        let resp = call(&k, 1, "no_such_tool", json!({}));
        assert_eq!(resp["error"]["code"], json!(-32602));
        assert_eq!(
            resp["error"]["message"],
            json!("unknown tool: no_such_tool")
        );

        // 未知方法 → -32601
        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/nope" }),
        );
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert_eq!(
            resp["error"]["message"],
            json!("method not found: tools/nope")
        );

        // 工具失败是一个*结果*，而不是协议错误，并且带着 `errors.attach` 会加上
        // 的那个稳定 code。
        let out = payload_of(&call(&k, 3, "read_asset", json!({ "assetId": "ghost:1" })));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("asset not found: ghost:1"));
        assert_eq!(out["code"], json!("asset.notFoundId"));
        assert_eq!(out["params"], json!(["ghost:1"]));

        // 格式错误的行 → 一个 JSON-RPC 错误，绝不是 panic，也绝不是沉默。
        let bad = handle_line("not json").expect("a malformed line still answers");
        assert_eq!(bad["jsonrpc"], json!("2.0"));
        assert_eq!(bad["error"]["code"], json!(-32700));
        assert!(handle_line("").is_none(), "an empty line answers nothing");

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn a_panic_becomes_an_internal_error_response_instead_of_killing_the_loop() {
        // `guarded` 正是 `dispatch` 运行 handler 时的护栏。没有它，一次 panic 会
        // 展开逃出 `serve()`，agent 的传输就只是被关掉了。
        let resp = guarded(Some(json!(7)), || panic!("index out of bounds: 3"));
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(7), "the client must still match the id");
        assert_eq!(resp["error"]["code"], json!(-32603));
        assert_eq!(resp["error"]["message"], json!("index out of bounds: 3"));

        // `panic!("literal")` 携带的是一个 &str 负载，不是 String。
        let resp = guarded(Some(json!(8)), || panic!("plain payload"));
        assert_eq!(resp["error"]["message"], json!("plain payload"));

        // 非字符串负载退回固定文本。
        let resp = guarded(Some(json!(9)), || panic::panic_any(42u32));
        assert_eq!(resp["error"]["message"], json!("internal error"));

        // 没有 id 的请求上的错误会被丢弃，与其它任何失败一样。
        assert!(guarded(None, || panic!("no id")).is_null());

        // 正常路径原样透传。
        assert_eq!(
            guarded(Some(json!(10)), || json!({ "ok": true })),
            json!({ "ok": true })
        );
    }

    #[test]
    fn dispatch_answers_normally_behind_the_panic_guard() {
        let root = temp("guard");
        let home = temp("guard-home");
        let k = keeper_at(&root, &home);

        // tools/list 和一个未知方法原封不动地穿过这层包装。
        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 15);
        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/nope" }),
        );
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert!(dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_null());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// 一个解析出 `{ ok: false }` 的工具仍然必须被标记为一次失败执行：否则遵循
    /// 规格的客户端会把"未找到资产"渲染成一次正常应答。这是刻意的 —— 见
    /// [`is_tool_error`] 的说明。
    ///
    /// 一个*成功*的结果绝不能带上这个标志：快乐路径上的 `isError: true` 比缺了
    /// 它更糟，因为客户端会把那段文字当作错误呈现出来。
    #[test]
    fn failed_tool_results_are_flagged_with_is_error() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("iserr");
        let home = temp("iserr-home");
        let k = keeper_at(&root, &home);

        let resp = call(&k, 1, "read_asset", json!({ "assetId": "ghost:1" }));
        assert_eq!(resp["result"]["content"][0]["text"].is_string(), true);
        assert_eq!(
            resp["result"]["isError"],
            json!(true),
            "a refused read is a failed tool execution"
        );
        assert!(resp.get("error").is_none(), "but not a protocol error");

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn stdout_stays_one_compact_line_per_response() {
        let root = temp("compact");
        let home = temp("compact-home");
        let k = keeper_at(&root, &home);
        let resp = dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        );
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains('\n'), "stdout would be split mid-object");
        assert!(line.starts_with("{\"jsonrpc\":\"2.0\""));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// 磁盘上的一个 skill 资产加上它的负载形式，供读取缓存使用。调用方通过
    /// `assets::remember` 播种缓存 —— 每个测试一次调用，因为 `remember` 会
    /// *替换*缓存，而一次成功的写入会让它失效。
    fn seed_skill(home: &std::path::Path, body: &str) -> (std::path::PathBuf, Value) {
        let dir = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        let asset = json!({
            "id": "skill:pdf", "kind": "skill", "name": "pdf",
            "agentId": "claude-code", "agentLabel": "Claude Code", "scope": "global",
            "dir": dir.to_string_lossy(),
            "file": dir.join("SKILL.md").to_string_lossy(),
            "writable": true, "size": 12, "issues": [], "security": [],
            "effective": { "live": true }
        });
        (dir, asset)
    }

    #[test]
    fn delete_asset_removes_the_skill_and_refuses_host_owned() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("del");
        let home = temp("del-home");
        let k = keeper_at(&root, &home);
        let (dir, skill) = seed_skill(&home, "---\nname: pdf\n---\nbody\n");
        let plugin = json!({
            "id": "plugin:x", "kind": "plugin", "name": "x",
            "agentId": "claude-code", "agentLabel": "Claude Code", "scope": "global",
            "file": "C:/vendor/plugin-x/plugin.json",
            "writable": false, "issues": [], "security": [],
        });
        assets::remember(
            &[skill.clone(), plugin.clone()],
            &[skill.clone(), plugin.clone()],
        );

        // 删除落在真实路径上，位于一次可撤销的事务之内。
        let out = payload_of(&call(
            &k,
            1,
            "delete_asset",
            json!({ "assetId": "skill:pdf" }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(
            out["txId"].as_str().unwrap_or("").starts_with("tx-"),
            "{}",
            out
        );
        assert!(!dir.exists(), "the delete must land on disk");

        // 宿主所有：刻意的拒绝，带着它那个稳定的 code。（调用 1 成功后已经让缓存
        // 失效，所以两个资产都要重新播种。）
        assets::remember(
            &[skill.clone(), plugin.clone()],
            &[skill.clone(), plugin.clone()],
        );
        let out = payload_of(&call(
            &k,
            2,
            "delete_asset",
            json!({ "assetId": "plugin:x" }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(
            out["error"],
            json!("host-owned asset — delete it from the owning agent UI instead")
        );
        assert_eq!(out["code"], json!("asset.hostOwnedDelete"));

        // 未知的 id：精确的 `asset not found`。
        let out = payload_of(&call(
            &k,
            3,
            "delete_asset",
            json!({ "assetId": "ghost:1" }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("asset not found"));
        assert_eq!(out["code"], json!("asset.notFound"));

        // 重新创建文件夹证明第二次调用是一次真正的删除，而不是第一次调用状态的
        // 重放。
        let (dir2, skill2) = seed_skill(
            &home,
            "---
name: pdf
---
body
",
        );
        assets::remember(std::slice::from_ref(&skill2), std::slice::from_ref(&skill2));
        let out = payload_of(&call(
            &k,
            4,
            "delete_asset",
            json!({ "assetId": "skill:pdf" }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(
            !dir2.exists(),
            "write-through: the delete must land on disk"
        );

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn distribute_asset_copies_to_another_agent() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("dist");
        let home = temp("dist-home");
        let k = keeper_at(&root, &home);
        let (_, skill) = seed_skill(&home, "---\nname: pdf\n---\nbody\n");
        // 一次成功的写入会让缓存失效，所以每次调用前都要重新播种。
        let reseed =
            || assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        reseed();

        let out = payload_of(&call(
            &k,
            1,
            "distribute_asset",
            json!({
                "assetId": "skill:pdf",
                "targets": [{ "agentId": "cursor", "scope": "global" }]
            }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        let written = out["written"].as_array().cloned().unwrap_or_default();
        assert_eq!(written.len(), 1, "{}", out);
        let dest = written[0].as_str().unwrap_or_default().to_string();
        assert!(dest.contains(".cursor"), "{}", dest);
        assert!(dest.ends_with("pdf"), "{}", dest);
        assert!(
            std::path::Path::new(&dest).join("SKILL.md").is_file(),
            "the copy reaches the real agent tree: {}",
            dest
        );
        assert_eq!(out["skipped"], json!([]), "{}", out);

        // 相同位置会被跳过，不会被拷贝。
        reseed();
        let out = payload_of(&call(
            &k,
            2,
            "distribute_asset",
            json!({
                "assetId": "skill:pdf",
                "targets": [{ "agentId": "claude-code", "scope": "global" }]
            }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("nothing to write"));
        assert_eq!(out["code"], json!("write.nothingToWrite"));
        assert_eq!(out["skipped"][0]["reason"], json!("same location"));

        // 未知的资产。
        let out = payload_of(&call(
            &k,
            3,
            "distribute_asset",
            json!({
                "assetId": "ghost:1",
                "targets": [{ "agentId": "cursor", "scope": "global" }]
            }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("asset not found"));

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn export_and_import_bundle_round_trip() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("exp");
        let home = temp("exp-home");
        let k = keeper_at(&root, &home);
        let (_, skill) = seed_skill(&home, "---\nname: pdf\n---\nbody\n");
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let out_dir = temp("exp-out");

        // 错误分支，每一个都带着参考实现的 `errors.attach` 会加上的那个稳定 code。
        let out = payload_of(&call(
            &k,
            1,
            "export_bundle",
            json!({ "assetIds": [], "dir": out_dir.to_string_lossy() }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("no assets selected"));
        assert_eq!(out["code"], json!("bundle.noSelection"));

        let out = payload_of(&call(
            &k,
            2,
            "export_bundle",
            json!({ "assetIds": ["skill:pdf"], "dir": "Z:/definitely/not/here" }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("target directory does not exist"));
        assert_eq!(out["code"], json!("write.targetDirMissing"));

        // 快乐路径：`<dir>/agentkeeper-bundle` 下的 manifest.json + files/。
        let out = payload_of(&call(
            &k,
            3,
            "export_bundle",
            json!({ "assetIds": ["skill:pdf"], "dir": out_dir.to_string_lossy() }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["count"], json!(1));
        let bundle = out_dir.join("agentkeeper-bundle");
        assert_eq!(out["out"], json!(bundle.to_string_lossy()), "{}", out);
        let manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(bundle.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["schema"], json!(1));
        assert_eq!(manifest["app"], json!("agentkeeper"));
        assert_eq!(manifest["assets"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["assets"][0]["name"], json!("pdf"));
        assert_eq!(manifest["assets"][0]["kind"], json!("skill"));
        assert_eq!(manifest["assets"][0]["agentId"], json!("claude-code"));
        assert!(
            manifest["assets"][0]["dir"]
                .as_str()
                .unwrap_or("")
                .starts_with("files/"),
            "{}",
            manifest
        );

        // 导入到一个*全新的* home —— 同一个 slug 在那里会被跳过。
        let root2 = temp("imp");
        let home2 = temp("imp-home");
        let k2 = keeper_at(&root2, &home2);
        let out = payload_of(&call(
            &k2,
            4,
            "import_bundle",
            json!({ "dir": out_dir.to_string_lossy() }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(
            out["txId"].as_str().unwrap_or("").starts_with("tx-"),
            "{}",
            out
        );
        assert_eq!(out["imported"], json!(["pdf"]), "{}", out);
        let target = home2.join(".claude").join("skills").join("pdf");
        assert!(
            target.join("SKILL.md").is_file(),
            "the import reaches the real agent tree: {:?}",
            target
        );

        // 没有 manifest 的文件夹会被拒绝，而不是靠猜。
        let junk = temp("imp-junk");
        let out = payload_of(&call(
            &k2,
            5,
            "import_bundle",
            json!({ "dir": junk.to_string_lossy() }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(
            out["error"],
            json!("not an agentkeeper bundle (manifest.json missing)")
        );
        assert_eq!(out["code"], json!("bundle.notBundle"));

        assets::invalidate();
        for d in [&root, &home, &out_dir, &root2, &home2, &junk] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn write_through_updates_land_and_leave_only_an_audit_line() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("tx");
        let home = temp("tx-home");
        let k = keeper_at(&root, &home);

        let file = home.join("rule-a.md");
        std::fs::write(&file, "old body\n").unwrap();
        let rule = json!({
            "id": "rule:a", "kind": "rule", "name": "a",
            "agentId": "claude-code", "agentLabel": "Claude Code", "scope": "global",
            "file": file.to_string_lossy(), "writable": true, "issues": [], "security": [],
        });
        let locked = json!({
            "id": "rule:locked", "kind": "rule", "name": "locked",
            "agentId": "claude-code", "agentLabel": "Claude Code", "scope": "global",
            "file": file.to_string_lossy(), "writable": false, "issues": [], "security": [],
        });
        assets::remember(&[rule.clone(), locked.clone()], &[rule, locked]);

        // 宿主所有：只读拒绝，带着它那个稳定的 code。
        let out = payload_of(&call(
            &k,
            1,
            "update_asset",
            json!({ "assetId": "rule:locked", "content": "x" }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(
            out["error"],
            json!("this asset is host-owned (plugin cache / shared dir) and cannot be edited here")
        );
        assert_eq!(out["code"], json!("asset.hostOwnedEdit"));

        // 一次直写更新：工具应答的是新状态，并不存在可供应答的事务工具。
        let out = payload_of(&call(
            &k,
            2,
            "update_asset",
            json!({ "assetId": "rule:a", "content": "new body\n" }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        // `txId` 仍然只是审计行的关联 id，别无其它。
        let tx_id = out["txId"].as_str().unwrap_or_default().to_string();
        assert!(tx_id.starts_with("tx-"), "{}", out);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new body\n");

        let audit =
            std::fs::read_to_string(std::path::Path::new(&root).join("audit.jsonl")).unwrap();
        assert!(
            audit.contains(&tx_id),
            "the write must be journalled in audit.jsonl: {audit}"
        );
        // 退役的工具是从界面上消失的，而不只是被注销：一个 agent 仍能调用的
        // 名字就是一项仍然存在的能力。
        for gone in ["rollback_transaction", "list_transactions"] {
            let resp = call(&k, 3, gone, json!({ "txId": tx_id, "limit": 50 }));
            assert_eq!(
                resp["error"]["code"],
                json!(-32602),
                "{} must not resolve: {}",
                gone,
                resp
            );
            assert!(
                resp.get("result").is_none(),
                "{} must not answer a payload: {}",
                gone,
                resp
            );
        }

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// 针对一个临时 home 的*真实*扫描来测读取工具 —— 不用播种的缓存 ——
    /// 这样负载形状（`stats`、`list_assets`、那一对 baseline）是在扫描器真正产出的
    /// 东西上被检查的。
    #[test]
    fn scan_stats_list_and_baseline_tools_read_a_real_home() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("scan");
        let home = temp("scan-home");
        let k = keeper_at(&root, &home);

        let mk = |home: &std::path::Path, name: &str, body: &str| {
            let dir = home.join(".claude").join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), body).unwrap();
        };
        mk(
            &home,
            "pdf",
            "---\nname: pdf\ndescription: make pdfs\n---\nbody\n",
        );
        mk(
            &home,
            "excel",
            "---\nname: excel\ndescription: spreadsheet wrangling\n---\nbody\n",
        );

        let out = payload_of(&call(&k, 1, "scan", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);

        let out = payload_of(&call(&k, 2, "stats", json!({})));
        assert_eq!(out["storeRoot"], json!(root.to_string_lossy()), "{}", out);
        assert!(out["byKind"]["skill"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let agents = out["agents"].as_array().cloned().unwrap_or_default();
        let claude = agents
            .iter()
            .find(|a| a["id"] == json!("claude-code"))
            .expect("claude-code in stats.agents");
        assert_eq!(claude["installed"], json!(true), "{}", claude);

        // 过滤器、大小写不敏感的搜索，以及 limit 的钳位。
        let out = payload_of(&call(&k, 3, "list_assets", json!({ "kind": "skill" })));
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let out = payload_of(&call(&k, 4, "list_assets", json!({ "search": "PDF" })));
        assert_eq!(out["total"], json!(1), "{}", out);
        assert_eq!(out["assets"][0]["name"], json!("pdf"));
        let pdf_id = out["assets"][0]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let out = payload_of(&call(&k, 5, "list_assets", json!({ "agentId": "cursor" })));
        assert_eq!(out["total"], json!(0), "{}", out);
        let out = payload_of(&call(&k, 6, "list_assets", json!({ "limit": 1 })));
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);
        assert_eq!(out["showing"], json!(1), "{}", out);
        assert_eq!(out["assets"].as_array().unwrap().len(), 1);

        // baseline：先锚定、看到没有漂移，然后改动 / 新增 / 移除。
        let out = payload_of(&call(&k, 7, "set_baseline", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["count"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let out = payload_of(&call(&k, 8, "baseline_diff", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["changed"], json!([]), "{}", out);
        assert_eq!(out["added"], json!([]), "{}", out);
        assert_eq!(out["gone"], json!([]), "{}", out);
        assert_eq!(out["hasBaseline"], json!(true));

        mk(
            &home,
            "pdf",
            "---\nname: pdf\ndescription: make pdfs\n---\nbody v2\n",
        );
        mk(
            &home,
            "extra",
            "---\nname: extra\ndescription: extra\n---\nbody\n",
        );
        std::fs::remove_dir_all(home.join(".claude").join("skills").join("excel")).unwrap();
        let _ = payload_of(&call(&k, 9, "scan", json!({}))); // 重新扫描，好让差异能看见它
        let out = payload_of(&call(&k, 10, "baseline_diff", json!({})));
        let ids = |key: &str| {
            out[key]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect::<Vec<String>>()
        };
        assert!(
            ids("changed").contains(&pdf_id),
            "changed must list the edited skill: {}",
            out
        );
        assert!(
            ids("added").iter().any(|id| id.contains("extra")),
            "{}",
            out
        );
        assert!(ids("gone").iter().any(|id| id.contains("excel")), "{}", out);

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}
