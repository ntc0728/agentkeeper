//! `src/mcp/server.js` — the stdio MCP server, in Rust.
//!
//! An agent launches `agentkeeper-mcp.exe` and speaks MCP over stdin/stdout:
//! newline-delimited JSON-RPC 2.0. It exposes the same tools as the JS build so
//! an agent's existing tool list keeps working, and every mutating tool goes
//! through [`super::keeper::Keeper`] — the same transaction layer the UI uses,
//! including the shadow-layer gate (an agent may never apply pending changes).
//!
//! ## Contract an agent relies on
//!
//! * stdout carries **exactly** one compact JSON object per line; anything
//!   else corrupts the stream. Diagnostics go to stderr only.
//! * `initialize` / `ping` / `tools/list` / `tools/call` and silence for
//!   `notifications/*`, with the same JSON-RPC error codes as the JS
//!   (`-32601` unknown method, `-32602` unknown tool, `-32603` internal).
//! * A panic inside a handler becomes a `-32603` response, never a process
//!   exit: [`dispatch`] runs behind `catch_unwind`, mirroring the `try/catch`
//!   that wraps `handle()` in the JS (which can never die this way). It works
//!   in release builds too — see [`dispatch`] on `panic = "unwind"`.
//! * Tool results are `{ content: [{ type: "text", text }] }`, where `text` is
//!   pretty-printed JSON — so the strings inside it are matched by other
//!   agents' prompts and must not be reworded.
//!
//! ## Three deliberate differences from the JavaScript
//!
//! * A malformed line answers with `-32700 parse error` instead of being
//!   silently dropped: `handle_line` must never return `None` for a line an
//!   agent actually sent, or the agent waits forever on a reply that will not
//!   come. Blank lines still produce nothing (they are line-ending noise).
//! * [`Store::apply_pending`] needs `&mut self`, so the non-sandboxed branch
//!   builds a `Store` over the same root rather than mutating the keeper's.
//!   State lives on disk (`pending.json`), so the result is identical.
//! * A tool result with `ok: false` carries `isError: true` (see
//!   [`is_tool_error`]). The JS omits it, so spec-following clients reported
//!   "asset not found" as a successful answer. Nothing internal depends on
//!   the old shape; MCP clients are external and the spec is the contract.

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

/// What `initialize` answers, and what the JS answers verbatim.
const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "agentkeeper";

/// `agentkeeper`'s version, as `package.json` reports it in the JS build.
fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The shadow-layer gate.
///
/// Applying from here while the sandbox is on would let an agent bypass the
/// human-approval step the whole design rests on — so it is refused, with
/// exactly the text the JS sends (and that `scripts/selftest.js` §15b greps).
const SANDBOX_GATE: &str = "sandbox is ON — staged changes await human approval in the AgentKeeper desktop UI. To let agents apply directly, start this server with AGENTKEEPER_SANDBOX=0.";

/// `discover.noInstalledAgent` — shared by `create_skill` and `install_skill`.
const NO_INSTALLED_AGENT: &str = "no installed agent found; pass agentId explicitly";

/// `read_asset` truncates the file body at 60 000 characters, like the JS.
const READ_CONTENT_LIMIT: usize = 60_000;

/* ------------------------------------------------------------------ */
/* tool definitions                                                    */
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

/// The declared tool list, as `tools/list` returns it.
///
/// Every entry mirrors `TOOLS` in `src/mcp/server.js` — same names, same
/// descriptions, same `inputSchema`. An agent matches on those strings, so a
/// rename here is a breaking change to a published API.
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
            "description": "Create a new skill for an agent (SKILL.md with YAML frontmatter). With the sandbox on (default) the write is staged for human approval.",
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
            "description": "Delete an asset (backed up first, reversible via rollback_transaction). MCP configs use delete_mcp instead.",
            "inputSchema": { "type": "object", "properties": { "assetId": s("asset id") }, "required": ["assetId"] }
        }),
        json!({
            "name": "distribute_asset",
            "description": "Copy an asset to other agents/projects (one reversible transaction). Only writable kinds.",
            "inputSchema": { "type": "object", "properties": {
                "assetId": s("asset id"),
                "targets": { "type": "array", "items": { "type": "object", "properties": {
                    "agentId": s("target agent"), "scope": { "type": "string", "enum": ["global", "project"] }, "projectId": s("required when scope=project")
                }, "required": ["agentId", "scope"] } }
            }, "required": ["assetId", "targets"] }
        }),
        json!({
            "name": "list_pending",
            "description": "Staged (shadow-layer) changes waiting to be applied. Non-empty means the human still needs to press Apply in the desktop app.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "apply_pending",
            "description": "Apply every staged change to the real agent directories (backup + atomic rename + journal). Refused while the sandbox is on — apply from the AgentKeeper desktop UI instead.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "discard_pending",
            "description": "Drop all staged changes. Nothing was ever written to the real directories.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_transactions",
            "description": "Recent write transactions with ids, for rollback.",
            "inputSchema": { "type": "object", "properties": { "limit": n("default 20, max 200") } }
        }),
        json!({
            "name": "rollback_transaction",
            "description": "Restore every file touched by a transaction to its pre-change state.",
            "inputSchema": { "type": "object", "properties": { "txId": s("transaction id (tx-…)") }, "required": ["txId"] }
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
            "description": "Import an agentkeeper-bundle folder previously exported. One reversible transaction.",
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
            "description": "Install skills from a GitHub repo (owner/repo) into an agent's global skills directory. Every skill is validated and security-scanned first; with the sandbox on the result is STAGED for human approval, not written.",
            "inputSchema": { "type": "object", "properties": {
                "repo": s("GitHub repo as owner/repo or a github.com URL"),
                "agentId": s("target agent (default: first installed agent)")
            }, "required": ["repo"] }
        }),
    ]
}

/* ------------------------------------------------------------------ */
/* JSON-shaped helpers                                                 */
/* ------------------------------------------------------------------ */

/// `String(args.x || '')` — the coercion every handler in the JS applies.
fn arg_str(args: &Value, key: &str) -> String {
    match args.get(key) {
        Some(v) if js_truthy(v) => js_string(v),
        _ => String::new(),
    }
}

/// `Number(v)`, with JS's falsy-zero: `Number(x) || default` also falls back
/// on `0`, which is what `Math.max(1, Number(args.limit) || 100)` relies on.
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

/// `Math.min(Math.max(1, Number(args[key]) || default), max)` — the clamp
/// `list_assets`, `list_transactions` and `search_registry` all share.
fn clamp_count(args: &Value, key: &str, fallback: f64, max: f64) -> usize {
    let raw = js_number(args.get(key));
    let v = if raw.is_nan() || raw == 0.0 {
        fallback
    } else {
        raw
    };
    v.max(1.0).min(max) as usize
}

/// `payload[key]` as a slice — no clone of an array that can hold ~1000 rows.
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

/// Insert only when the JS would not have produced `undefined`:
/// `JSON.stringify` drops `undefined` values, so the key disappears entirely.
fn put(m: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        if !v.is_null() {
            m.insert(key.to_string(), v.clone());
        }
    }
}

/// `x || undefined` — falsy values drop the key too.
fn put_truthy(m: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        if js_truthy(v) {
            m.insert(key.to_string(), v.clone());
        }
    }
}

/* ------------------------------------------------------------------ */
/* MCP content envelope                                                */
/* ------------------------------------------------------------------ */

/// Wrap a tool result as MCP text content.
///
/// `errors.attach` runs here so every tool failure that reaches an MCP client
/// carries a stable `code` next to the English message — an agent can branch on
/// `asset.notFound` instead of matching prose.
fn text(result: Value) -> Value {
    // Read before `result` is consumed: the flag belongs on the *wrapper*
    // (`result.isError` in the JSON-RPC response), while `ok` lives on the
    // payload that gets serialised into `content[0].text`.
    let failed = is_tool_error(&result);
    let body = match result {
        Value::String(s) => s,
        other => {
            serde_json::to_string_pretty(&attach(other)).unwrap_or_else(|_| "{}".to_string())
        }
    };
    let mut out = json!({ "content": [{ "type": "text", "text": body }] });
    if failed {
        out.as_object_mut()
            .unwrap()
            .insert("isError".to_string(), json!(true));
    }
    out
}

/// `core/errors.js` `attach()` — resolve an English message back to a code.
///
/// Only the messages the MCP surface can produce are indexed (the full table
/// lives in `core/errors.js` and covers the IPC layer too). Two lookup rules,
/// same as the JS: an exact match first, then the longest parameterised prefix
/// whose stem is at least 12 characters — a shorter stem would match prose
/// that merely starts the same way.
fn attach(result: Value) -> Value {
    let Value::Object(mut m) = result else {
        return result;
    };
    if m.contains_key("code") {
        return Value::Object(m);
    }
    let Some(error) = m.get("error").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
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
    ("nothing exportable in selection", "bundle.nothingExportable"),
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

/// (prefix, code) — longest prefix wins, so this list is ordered by length.
const PREFIX_CODES: [(&str, &str); 12] = [
    ("config file is corrupt, refusing to overwrite", "store.configCorrupt"),
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
        // `text.indexOf(':', prefix.length - 1)` — the parameter is whatever
        // follows the first colon at or after the stem.
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

/// The row shape `list_assets` and `read_asset` return for one asset.
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
    // `a.meta?.estTokens` — 0 is kept, a missing `meta` drops the key.
    let tokens = a.get("meta").and_then(|meta| meta.get("estTokens"));
    put(&mut m, "tokens", tokens);
    put(&mut m, "writable", a.get("writable"));
    m.insert(
        "issues".to_string(),
        json!(a.get("issues").and_then(|v| v.as_array()).map(|l| l.len()).unwrap_or(0)),
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
/* context                                                             */
/* ------------------------------------------------------------------ */

/// The store root — `resolveStoreRoot()` in `src/mcp/server.js:35`.
///
/// Identical to `src/lib.rs`: `AK_STORE_ROOT`, else
/// `<APPDATA>\AgentKeeper\agentkeeper`. The MCP server and the GUI must land
/// on the same directory or an agent's staged writes are invisible to the
/// human who has to approve them.
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

/// `buildContext()` — one keeper for the life of the process, exactly as the
/// JS builds it once in `main()`.
fn context() -> &'static Keeper {
    static CTX: OnceLock<Keeper> = OnceLock::new();
    CTX.get_or_init(|| {
        let sandbox = settings::sandbox_from_env();
        Keeper::new(Store::new(&store_root(), sandbox), &user_home())
    })
}

/// `keeper.lastScan` — the scan cache, held in an `Arc` so a tool that only
/// reads it costs nothing. The naive version re-scanned ~1000 assets (plus the
/// security pass) on EVERY tool call — 3–8 s per call for an agent
/// conversation.
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

/// `freshScan()` — scan once, then reuse until a mutation invalidates it.
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

/// `invalidate()` — the scan must not outlive a write.
fn invalidate(keeper: &Keeper) {
    *last_scan().lock().unwrap_or_else(|e| e.into_inner()) = None;
    keeper.invalidate();
}

/// `keeper.scan().effective || keeper.scan().assets`
fn effective_or_assets(payload: &Value) -> &[Value] {
    let effective = arr_of(payload, "effective");
    if effective.is_empty() {
        arr_of(payload, "assets")
    } else {
        effective
    }
}

/// `(id, fingerprint)` over the scan's `assets` — the input `baselineDiff` /
/// `setBaseline` take in the JS.
fn fingerprints(payload: &Value) -> Vec<(String, String)> {
    arr_of(payload, "assets")
        .iter()
        .filter_map(|a| {
            let id = a.get("id").map(js_string)?;
            Some((id, str_of(a, "fingerprint")))
        })
        .collect()
}

/// The first installed agent, or `""` — the default target of `create_skill`
/// and `install_skill`.
fn first_installed_agent(keeper: &Keeper) -> String {
    let payload = fresh_scan(keeper);
    arr_of(&payload, "agents")
        .iter()
        .find(|a| a.get("installed").and_then(|v| v.as_bool()).unwrap_or(false))
        .map(|a| str_of(a, "id"))
        .unwrap_or_default()
}

/* ------------------------------------------------------------------ */
/* tool implementations                                                */
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
        "sandbox": keeper.store.sandbox(),
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
    invalidate(keeper); // staged content must show up in the next read
    text(json!({
        "ok": true,
        "staged": keeper.store.sandbox(),
        "txId": r.get("txId").cloned().unwrap_or(Value::Null),
        "files": r.get("written").cloned().unwrap_or_else(|| json!([])),
        "note": if keeper.store.sandbox() {
            "staged in the shadow layer — apply in the AgentKeeper UI"
        } else {
            "written directly"
        }
    }))
}

/// The `{ ok: false, error, blocked, issues }` envelope `create_skill` and
/// `update_asset` return on a refused write.
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
    // `files: r.written` in the JS (`src/mcp/server.js:304`): when the keeper
    // did not report what it wrote, `undefined` is dropped by `JSON.stringify`
    // and the key is absent. Emitting `files: []` here would tell an agent
    // "nothing was written" when the truth is "not reported".
    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    out.insert("staged".to_string(), json!(keeper.store.sandbox()));
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
    // keeper.delete_asset dispatches MCP configs to delete_mcp internally —
    // passing an mcp asset here deletes the config entry, never the shared file.
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

fn list_pending(keeper: &Keeper, _args: &Value) -> Value {
    // read_pending returns the changes array itself — earlier code assumed
    // {changes:[]} and always reported zero.
    let pending = keeper.store.read_pending();
    let changes: Vec<Value> = pending
        .iter()
        .take(50)
        .map(|c| {
            json!({
                "op": c.get("op").cloned().unwrap_or(Value::Null),
                "target": c.get("real").cloned().unwrap_or(Value::Null),
                "bytes": c.get("bytes").cloned().unwrap_or(json!(0)),
                "at": c.get("at").cloned().unwrap_or(Value::Null)
            })
        })
        .collect();
    text(json!({
        "count": pending.len(),
        "sandbox": keeper.store.sandbox(),
        "note": if pending.is_empty() {
            "nothing staged"
        } else {
            "staged changes await human approval in the AgentKeeper UI"
        },
        "changes": changes
    }))
}

fn apply_pending(keeper: &Keeper, _args: &Value) -> Value {
    // The shadow layer exists so an agent can stage, never silently
    // overwrite. Applying from here while the sandbox is on would let an AI
    // bypass the human-approval step the whole design rests on — so it is
    // refused, matching the documented security model.
    if keeper.store.sandbox() {
        return text(json!({ "ok": false, "error": SANDBOX_GATE }));
    }
    let mut store = Store::new(keeper.store.root_str(), keeper.store.sandbox());
    let r = store.apply_pending();
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper);
    }
    text(r)
}

fn discard_pending(keeper: &Keeper, _args: &Value) -> Value {
    let r = keeper.store.discard_pending();
    if r.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        invalidate(keeper);
    }
    text(r)
}

fn list_transactions(keeper: &Keeper, args: &Value) -> Value {
    let limit = clamp_count(args, "limit", 20.0, 200.0);
    text(Value::Array(settings::list_transactions(
        keeper.store.root_str(),
        limit,
    )))
}

fn rollback_transaction(keeper: &Keeper, args: &Value) -> Value {
    let r = keeper.store.rollback(&arg_str(args, "txId"));
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
        invalidate(keeper); // staged content must show up in the next read
    }
    text(r)
}

/* ------------------------------------------------------------------ */
/* JSON-RPC plumbing                                                   */
/* ------------------------------------------------------------------ */

fn respond(id: Option<Value>, result: Value) -> Value {
    // `JSON.stringify({ id: undefined, … })` drops `id`; so do we.
    let mut m = Map::new();
    m.insert("jsonrpc".to_string(), json!("2.0"));
    if let Some(id) = id {
        m.insert("id".to_string(), id);
    }
    m.insert("result".to_string(), result);
    Value::Object(m)
}

/// `fail(code, message)` — a notification's error is dropped, like the JS.
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
        "list_pending" => list_pending(keeper, &args),
        "apply_pending" => apply_pending(keeper, &args),
        "discard_pending" => discard_pending(keeper, &args),
        "list_transactions" => list_transactions(keeper, &args),
        "rollback_transaction" => rollback_transaction(keeper, &args),
        "baseline_diff" => baseline_diff(keeper, &args),
        "set_baseline" => set_baseline(keeper, &args),
        "export_bundle" => export_bundle(keeper, &args),
        "import_bundle" => import_bundle(keeper, &args),
        "search_registry" => search_registry(keeper, &args),
        "install_skill" => install_skill(keeper, &args),
        _ => return fail(req.get("id").cloned(), -32602, format!("unknown tool: {}", name)),
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

/// `ok: false` on a tool result, per the MCP `tools/call` contract.
///
/// `src/mcp/server.js` sets `isError` only in the `.catch()` of `tools/call`
/// (a *thrown* handler); a handler that resolves with `{ ok: false, error }`
/// comes back as an ordinary result with no `isError`. A client that follows
/// the spec therefore renders "asset not found" as a successful answer.
///
/// That is a defect in the JS, not a behaviour worth preserving — and unlike
/// the rest of this port there is no parity obligation once the JS build is
/// gone: the consumers are third-party MCP clients, and the spec says a tool
/// execution failure must be flagged. See the module header.
fn is_tool_error(result: &Value) -> bool {
    result.is_object() && result.get("ok").and_then(|v| v.as_bool()) == Some(false)
}

/// The pure request handler.
///
/// Returns the JSON-RPC response object, or `Value::Null` when the request
/// must not be answered at all (a `notifications/*` message, or an error on a
/// request that carried no `id`). `handle_line` turns `Null` into `None`.
///
/// The body runs inside [`guarded`] — the equivalent of the `try { … } catch`
/// around `handle()` in `src/mcp/server.js:453`. Without it a single `unwrap()`
/// or slice out of bounds kills the process mid-stream, and the agent sees a
/// dropped transport rather than an error it can report.
///
/// **Why `[profile.release]` no longer says `panic = "abort"`:** `catch_unwind`
/// can only catch a panic that *unwinds*, so under `abort` this guard would do
/// nothing exactly where it matters — in the shipped binary. `Cargo.toml` now
/// keeps unwinding for the release profile on purpose; the few hundred KB of
/// unwind tables buy a server that answers `-32603` instead of dying mid-
/// stream. Unwinding is still the last resort, not a design: a handler that
/// panics has already failed the request.
pub fn dispatch(keeper: &super::keeper::Keeper, req: &Value) -> Value {
    let id = req.get("id").cloned();
    guarded(id, || dispatch_inner(keeper, req))
}

/// Run `f`, turning a panic into a `-32603 internal error` response.
///
/// The response keeps the request's `id`, so the client can still match it; an
/// `id`-less request answers `Null`, like every other failure here.
fn guarded(id: Option<Value>, f: impl FnOnce() -> Value) -> Value {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(resp) => resp,
        Err(payload) => fail(id, -32603, panic_text(payload.as_ref())),
    }
}

/// The panic payload as text. `panic!("literal")` carries a `&str`,
/// `panic!("{}", x)` a `String`; anything else falls back to the fixed
/// message `src/mcp/server.js:455` answers with.
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

/// Read one JSON-RPC request and return one response, or `None` when the line
/// carries nothing to answer (blank, or a notification).
///
/// One call per line, no batching: the JS server behaves the same way and an
/// agent client that pipelined requests would break on it either way.
pub fn handle_line(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
        eprintln!("[agentkeeper-mcp] bad json line ignored");
        // The JS drops the line silently; an agent that sent it is waiting for
        // a reply that will never come, so answer with a parse error instead.
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

/// Run the loop until stdin closes. Returns the process exit code.
pub fn serve() -> i32 {
    let keeper = context();
    eprintln!(
        "[agentkeeper-mcp] store: {} · sandbox: {}",
        keeper.store.root_str(),
        keeper.store.sandbox()
    );

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Some(resp) = handle_line(&line) else {
            continue;
        };
        // One compact line, flushed immediately: an agent reads stdout as a
        // stream, and pretty-printing here would break its line parser.
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

    fn keeper_at(root: &std::path::Path, home: &std::path::Path, sandbox: bool) -> Keeper {
        Keeper::new(
            Store::new(&root.to_string_lossy(), sandbox),
            &home.to_string_lossy(),
        )
    }

    /// The scan payload cache is process-global, like the JS `lastScan`; drop
    /// it so one test never reads another test's scan.
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

    /// `JSON.parse(result.content[0].text)` — what an MCP client does.
    fn payload_of(resp: &Value) -> Value {
        let raw = resp["result"]["content"][0]["text"].as_str().unwrap_or("{}");
        serde_json::from_str(raw).unwrap_or(Value::Null)
    }

    #[test]
    fn initialize_reports_the_protocol_and_server_name() {
        let root = temp("init");
        let home = temp("init-home");
        let k = keeper_at(&root, &home, true);

        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }));
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], json!("2024-11-05"));
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("agentkeeper"));
        assert!(resp["result"]["serverInfo"]["version"].is_string());
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], json!(false));

        // ping answers an empty result; a notification answers nothing.
        let ping = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }));
        assert_eq!(ping["result"], json!({}));
        assert!(dispatch(
            &k,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_null());
        assert!(handle_line("   ").is_none(), "a blank line is not a request");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn tools_list_declares_every_tool_the_js_declares() {
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
                "list_pending",
                "apply_pending",
                "discard_pending",
                "list_transactions",
                "rollback_transaction",
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
        let k = keeper_at(&root, &home, true);
        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" }));
        assert_eq!(resp["result"]["tools"][0]["name"], json!("list_assets"));
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 20);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_and_write_tools_round_trip_through_the_shadow_tree() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("rw");
        let home = temp("rw-home");
        let k = keeper_at(&root, &home, true);

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

        // --- read tool --------------------------------------------------
        let out = payload_of(&call(&k, 1, "read_asset", json!({ "assetId": "skill:pdf" })));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["asset"]["name"], json!("pdf"));
        assert_eq!(out["asset"]["agent"], json!("Claude Code"));
        assert_eq!(out["asset"]["tokens"], json!(3));
        // `a.projectName || undefined` — the key is absent, not null.
        assert!(out["asset"].get("project").is_none(), "{}", out["asset"]);
        assert_eq!(out["content"], json!("---\nname: pdf\n---\nbody\n"));

        // --- write tool: staged, never written --------------------------
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
        assert_eq!(out["staged"], json!(true), "{}", out);
        assert_eq!(
            out["note"],
            json!("staged in the shadow layer — apply in the AgentKeeper UI")
        );
        // claude-code keeps global skills in `~/.claude/skills`.
        let target = home.join(".claude").join("skills").join("new-skill");
        assert!(
            !target.exists(),
            "sandbox: the real tree must be untouched"
        );
        let pending = k.store.read_pending();
        assert_eq!(pending.len(), 1, "{:?}", pending);
        assert_eq!(pending[0]["op"], json!("write"));
        assert_eq!(
            pending[0]["real"],
            json!(target.join("SKILL.md").to_string_lossy())
        );

        // --- list_pending reports it ------------------------------------
        let out = payload_of(&call(&k, 3, "list_pending", json!({})));
        assert_eq!(out["count"], json!(1), "{}", out);
        assert_eq!(out["sandbox"], json!(true));
        assert_eq!(
            out["note"],
            json!("staged changes await human approval in the AgentKeeper UI")
        );

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn apply_pending_is_refused_while_the_sandbox_is_on() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("gate");
        let home = temp("gate-home");
        let k = keeper_at(&root, &home, true);

        // Stage one change so the refusal is not a "nothing to do" no-op.
        let probe = home.join("gate.md");
        let p = probe.to_string_lossy().into_owned();
        let tx = k.store.transaction(
            &[crate::core::store::Op::write(&p, "gate probe\n")],
            &json!({ "kind": "rule", "action": "update" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert_eq!(k.store.read_pending().len(), 1);

        let out = payload_of(&call(&k, 1, "apply_pending", json!({})));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!(SANDBOX_GATE));
        assert_eq!(
            k.store.read_pending().len(),
            1,
            "a refused apply must leave the queue intact"
        );
        assert!(!probe.exists(), "a refused apply must not touch the real tree");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn apply_pending_works_once_the_sandbox_is_off() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("apply");
        let home = temp("apply-home");
        // sandbox off — the branch the gate does not cover.
        let k = keeper_at(&root, &home, false);
        let probe = home.join("gate.md");
        let p = probe.to_string_lossy().into_owned();
        let tx = k.store.transaction(
            &[crate::core::store::Op::write(&p, "gate probe\n")],
            &json!({ "kind": "rule", "action": "update" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);

        let out = payload_of(&call(&k, 1, "apply_pending", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(probe.exists(), true);
        assert!(k.store.read_pending().is_empty());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unknown_tool_and_malformed_lines_are_well_formed_errors() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("err");
        let home = temp("err-home");
        let k = keeper_at(&root, &home, true);

        // unknown tool → -32602
        let resp = call(&k, 1, "no_such_tool", json!({}));
        assert_eq!(resp["error"]["code"], json!(-32602));
        assert_eq!(resp["error"]["message"], json!("unknown tool: no_such_tool"));

        // unknown method → -32601
        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/nope" }));
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert_eq!(
            resp["error"]["message"],
            json!("method not found: tools/nope")
        );

        // a tool failure is a *result*, not a protocol error, and carries the
        // stable code the JS `errors.attach` would add.
        let out = payload_of(&call(&k, 3, "read_asset", json!({ "assetId": "ghost:1" })));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("asset not found: ghost:1"));
        assert_eq!(out["code"], json!("asset.notFoundId"));
        assert_eq!(out["params"], json!(["ghost:1"]));

        // malformed line → a JSON-RPC error, never a panic and never silence.
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
        // `guarded` is what `dispatch` runs the handler behind. Without it a
        // panic unwinds out of `serve()` and the agent's transport just closes.
        let resp = guarded(Some(json!(7)), || panic!("index out of bounds: 3"));
        assert_eq!(resp["jsonrpc"], json!("2.0"));
        assert_eq!(resp["id"], json!(7), "the client must still match the id");
        assert_eq!(resp["error"]["code"], json!(-32603));
        assert_eq!(resp["error"]["message"], json!("index out of bounds: 3"));

        // `panic!("literal")` carries a &str payload, not a String.
        let resp = guarded(Some(json!(8)), || panic!("plain payload"));
        assert_eq!(resp["error"]["message"], json!("plain payload"));

        // A non-string payload falls back to the fixed JS text (server.js:455).
        let resp = guarded(Some(json!(9)), || panic::panic_any(42u32));
        assert_eq!(resp["error"]["message"], json!("internal error"));

        // An error on an id-less request is dropped, like any other failure.
        assert!(guarded(None, || panic!("no id")).is_null());

        // The normal path is passed through untouched.
        assert_eq!(
            guarded(Some(json!(10)), || json!({ "ok": true })),
            json!({ "ok": true })
        );
    }

    #[test]
    fn dispatch_answers_normally_behind_the_panic_guard() {
        let root = temp("guard");
        let home = temp("guard-home");
        let k = keeper_at(&root, &home, true);

        // tools/list and an unknown method survive the wrapper unchanged.
        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        assert_eq!(resp["result"]["tools"].as_array().unwrap().len(), 20);
        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/nope" }));
        assert_eq!(resp["error"]["code"], json!(-32601));
        assert!(
            dispatch(&k, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_null()
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// A tool that resolves `{ ok: false }` must still be flagged as a failed
    /// execution. `src/mcp/server.js` does not do this (it only sets `isError`
    /// on a thrown handler), which is why an MCP client showed "asset not
    /// found" as a normal answer. Deliberate divergence — see
    /// [`is_tool_error`].
    ///
    /// A *successful* result must not carry the flag: `isError: true` on a
    /// happy path is worse than missing it, because clients surface the text
    /// as an error.
    #[test]
    fn failed_tool_results_are_flagged_with_is_error() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("iserr");
        let home = temp("iserr-home");
        let k = keeper_at(&root, &home, true);

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
        let k = keeper_at(&root, &home, true);
        let resp = dispatch(&k, &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        let line = serde_json::to_string(&resp).unwrap();
        assert!(!line.contains('\n'), "stdout would be split mid-object");
        assert!(line.starts_with("{\"jsonrpc\":\"2.0\""));
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// A skill asset on disk plus its payload form, for the read cache. The
    /// caller seeds the cache via `assets::remember` — one call per test, since
    /// `remember` *replaces* the cache and a successful write invalidates it.
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
    fn delete_asset_stages_in_sandbox_and_refuses_host_owned() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("del");
        let home = temp("del-home");
        let k = keeper_at(&root, &home, true);
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

        // sandboxed delete: the real tree is untouched. The staged queue stays
        // empty too — `_recordPending` skips a path with no shadow copy (a
        // delete only stages when the target was staged before), exactly like
        // the JS `store.js` line 224 guard.
        let out = payload_of(&call(&k, 1, "delete_asset", json!({ "assetId": "skill:pdf" })));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(
            out["txId"].as_str().unwrap_or("").starts_with("tx-"),
            "{}",
            out
        );
        assert!(dir.exists(), "sandbox: the real tree must be untouched");
        assert!(
            k.store.read_pending().is_empty(),
            "{:?}",
            k.store.read_pending()
        );

        // host-owned: the deliberate refusal, with its stable code. (Call 1
        // invalidated the cache on success, so re-seed both assets.)
        assets::remember(
            &[skill.clone(), plugin.clone()],
            &[skill.clone(), plugin.clone()],
        );
        let out = payload_of(&call(&k, 2, "delete_asset", json!({ "assetId": "plugin:x" })));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(
            out["error"],
            json!("host-owned asset — delete it from the owning agent UI instead")
        );
        assert_eq!(out["code"], json!("asset.hostOwnedDelete"));

        // unknown id: `asset not found` exactly, like the JS.
        let out = payload_of(&call(&k, 3, "delete_asset", json!({ "assetId": "ghost:1" })));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("asset not found"));
        assert_eq!(out["code"], json!("asset.notFound"));

        // with the sandbox off the same call deletes for real.
        let k2 = keeper_at(&root, &home, false);
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let out = payload_of(&call(&k2, 4, "delete_asset", json!({ "assetId": "skill:pdf" })));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(!dir.exists(), "write-through: the delete must land on disk");

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn distribute_asset_stages_a_copy_for_another_agent() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("dist");
        let home = temp("dist-home");
        let k = keeper_at(&root, &home, true);
        let (_, skill) = seed_skill(&home, "---\nname: pdf\n---\nbody\n");
        // A successful write invalidates the cache, so re-seed before each call.
        let reseed = || assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
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
            !std::path::Path::new(&dest).exists(),
            "sandbox: the copy is staged, not written"
        );
        assert_eq!(out["skipped"], json!([]), "{}", out);
        let pending = k.store.read_pending();
        assert!(
            pending.iter().any(|c| c["real"] == json!(dest)),
            "{:?}",
            pending
        );

        // same location is skipped, not copied.
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

        // unknown asset.
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
        let k = keeper_at(&root, &home, true);
        let (_, skill) = seed_skill(&home, "---\nname: pdf\n---\nbody\n");
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let out_dir = temp("exp-out");

        // error branches, each with the stable code the JS `errors.attach` adds.
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

        // happy path: manifest.json + files/ under <dir>/agentkeeper-bundle.
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
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(bundle.join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["schema"], json!(1));
        assert_eq!(manifest["app"], json!("agentkeeper"));
        assert_eq!(manifest["assets"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["assets"][0]["name"], json!("pdf"));
        assert_eq!(manifest["assets"][0]["kind"], json!("skill"));
        assert_eq!(manifest["assets"][0]["agentId"], json!("claude-code"));
        assert!(
            manifest["assets"][0]["dir"].as_str().unwrap_or("").starts_with("files/"),
            "{}",
            manifest
        );

        // import into a *fresh* home — the same slug would be skipped there.
        let root2 = temp("imp");
        let home2 = temp("imp-home");
        let k2 = keeper_at(&root2, &home2, true);
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
        assert!(!target.exists(), "sandbox: an import is staged, not written");
        let pending = k2.store.read_pending();
        assert!(
            pending.iter().any(|c| c["real"] == json!(target.to_string_lossy())),
            "{:?}",
            pending
        );

        // a folder without a manifest is refused, not guessed at.
        let junk = temp("imp-junk");
        let out = payload_of(&call(&k2, 5, "import_bundle", json!({ "dir": junk.to_string_lossy() })));
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
    fn transactions_are_listed_and_rolled_back() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("tx");
        let home = temp("tx-home");
        // sandbox off: the write lands on disk, so the tx and its backup are real.
        let k = keeper_at(&root, &home, false);

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

        // host-owned: the read-only refusal, with its stable code.
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

        // a write-through update, then the tx shows up in list_transactions.
        let out = payload_of(&call(
            &k,
            2,
            "update_asset",
            json!({ "assetId": "rule:a", "content": "new body\n" }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["staged"], json!(false), "{}", out);
        let tx_id = out["txId"].as_str().unwrap_or_default().to_string();
        assert!(tx_id.starts_with("tx-"), "{}", out);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new body\n");

        let out = payload_of(&call(&k, 3, "list_transactions", json!({ "limit": 50 })));
        let rows = out.as_array().cloned().unwrap_or_default();
        let row = rows
            .iter()
            .find(|r| r["txId"] == json!(tx_id))
            .expect("the write tx must be listed");
        assert_eq!(row["hasBackup"], json!(true), "{}", row);
        assert_eq!(row["action"], json!("update"), "{}", row);

        // rollback restores the pre-change content byte for byte.
        let out = payload_of(&call(
            &k,
            4,
            "rollback_transaction",
            json!({ "txId": tx_id }),
        ));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old body\n");

        let out = payload_of(&call(
            &k,
            5,
            "rollback_transaction",
            json!({ "txId": "tx-nope" }),
        ));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["error"], json!("transaction tx-nope not found"));

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// The read tools against a *real* scan of a temp home — no seeded cache —
    /// so the payload shapes (`stats`, `list_assets`, the baseline pair) are
    /// checked on what the scanner actually produces.
    #[test]
    fn scan_stats_list_and_baseline_tools_read_a_real_home() {
        let _guard = assets::lock_for_test();
        clean_slate();
        let root = temp("scan");
        let home = temp("scan-home");
        let k = keeper_at(&root, &home, true);

        let mk = |home: &std::path::Path, name: &str, body: &str| {
            let dir = home.join(".claude").join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), body).unwrap();
        };
        mk(&home, "pdf", "---\nname: pdf\ndescription: make pdfs\n---\nbody\n");
        mk(&home, "excel", "---\nname: excel\ndescription: spreadsheet wrangling\n---\nbody\n");

        let out = payload_of(&call(&k, 1, "scan", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);

        let out = payload_of(&call(&k, 2, "stats", json!({})));
        assert_eq!(out["storeRoot"], json!(root.to_string_lossy()), "{}", out);
        assert_eq!(out["sandbox"], json!(true));
        assert!(out["byKind"]["skill"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let agents = out["agents"].as_array().cloned().unwrap_or_default();
        let claude = agents
            .iter()
            .find(|a| a["id"] == json!("claude-code"))
            .expect("claude-code in stats.agents");
        assert_eq!(claude["installed"], json!(true), "{}", claude);

        // filters, the case-insensitive search, and the limit clamp.
        let out = payload_of(&call(&k, 3, "list_assets", json!({ "kind": "skill" })));
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let out = payload_of(&call(&k, 4, "list_assets", json!({ "search": "PDF" })));
        assert_eq!(out["total"], json!(1), "{}", out);
        assert_eq!(out["assets"][0]["name"], json!("pdf"));
        let pdf_id = out["assets"][0]["id"].as_str().unwrap_or_default().to_string();
        let out = payload_of(&call(&k, 5, "list_assets", json!({ "agentId": "cursor" })));
        assert_eq!(out["total"], json!(0), "{}", out);
        let out = payload_of(&call(&k, 6, "list_assets", json!({ "limit": 1 })));
        assert!(out["total"].as_u64().unwrap_or(0) >= 2, "{}", out);
        assert_eq!(out["showing"], json!(1), "{}", out);
        assert_eq!(out["assets"].as_array().unwrap().len(), 1);

        // baseline: anchor, see no drift, then change / add / remove.
        let out = payload_of(&call(&k, 7, "set_baseline", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["count"].as_u64().unwrap_or(0) >= 2, "{}", out);
        let out = payload_of(&call(&k, 8, "baseline_diff", json!({})));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["changed"], json!([]), "{}", out);
        assert_eq!(out["added"], json!([]), "{}", out);
        assert_eq!(out["gone"], json!([]), "{}", out);
        assert_eq!(out["hasBaseline"], json!(true));

        mk(&home, "pdf", "---\nname: pdf\ndescription: make pdfs\n---\nbody v2\n");
        mk(&home, "extra", "---\nname: extra\ndescription: extra\n---\nbody\n");
        std::fs::remove_dir_all(home.join(".claude").join("skills").join("excel")).unwrap();
        let _ = payload_of(&call(&k, 9, "scan", json!({}))); // rescan, so the diff sees it
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
        assert!(
            ids("gone").iter().any(|id| id.contains("excel")),
            "{}",
            out
        );

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}
