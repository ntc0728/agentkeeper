//! The mutating service.
//!
//! Every CRUD action lands here, so validation, audit and the write path's
//! failure-undo are impossible to bypass from the UI, the MCP server or a
//! future CLI. Each write goes through
//! [`super::store::Store::transaction`]; **nothing in this module may touch the
//! filesystem to change a file**. That single rule is what makes "a failed
//! write leaves the tree exactly as it was" a true statement.
//!
//! ## Resolution order
//!
//! An asset id resolved from the last scan decides *which file* to mutate; the
//! adapter table decides *where* a new one goes. Two consequences worth stating:
//!
//! * An id the last scan never produced is `asset not found` — never a guessed
//!   path. On a cold cache one scan is run first.
//! * A whole-file read never throws; a missing file reads as `""`. The only place that
//!   treats "cannot read" as fatal is a config that exists but does not parse:
//!   `upsert_mcp` / `delete_mcp` refuse to overwrite it, because proceeding
//!   would silently drop every sibling server in the file.
//!
//! ## yaml / toml
//!
//! YAML and TOML MCP configs are rewritten field-by-field, not byte-spliced:
//! `yaml` re-emits the whole document (comments are lost — the parser drops
//! them), `toml` re-emits only the one section and
//! splices it back so comments *outside* the server survive.

use serde_json::{json, Map, Value};
use std::path::Path;

use super::adapters::{by_id, family, resolve_home, Agent};
use super::assets;
use super::frontmatter;
use super::model::{basename, js_string, js_truthy, slug};
use super::registry;
use super::scan;
use super::settings;
use super::store::{self, Op, Store};
use super::toml_splice;
use super::validate;
use super::yaml;

/* ------------------------------------------------------------------ */
/* small coercion helpers                                              */
/* ------------------------------------------------------------------ */

fn prop(v: &Value, key: &str) -> String {
    v.get(key)
        .map(js_string)
        .unwrap_or_else(|| "undefined".to_string())
}

fn truthy(v: Option<&Value>) -> bool {
    v.map(js_truthy).unwrap_or(false)
}

fn read_all(path: &str) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn exists_file(p: &str) -> bool {
    Path::new(p).is_file()
}

fn exists_dir(p: &str) -> bool {
    Path::new(p).is_dir()
}

/// Join `base` with `parts`.
fn join(base: &str, parts: &[&str]) -> String {
    let mut p = std::path::PathBuf::from(base);
    for part in parts {
        p.push(part);
    }
    store::native_str(&p)
}

/// Absolute join with `.`/`..` collapsed.
fn resolve(base: &str, rel: &str) -> String {
    if Path::new(rel).is_absolute() {
        return store::normalize_path(rel);
    }
    store::normalize_path(&store::native_str(
        &std::path::PathBuf::from(base).join(rel),
    ))
}

/// Object spread `{ ...v }` for an object; index keys for an array or string.
fn spread_map(v: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                out.insert(k.clone(), val.clone());
            }
        }
        Value::Array(a) => {
            for (i, val) in a.iter().enumerate() {
                out.insert(i.to_string(), val.clone());
            }
        }
        Value::String(s) => {
            for (i, c) in s.chars().enumerate() {
                out.insert(i.to_string(), Value::String(c.to_string()));
            }
        }
        _ => {}
    }
    out
}

/// Shallow merge of `patch` over `base`; absent base means an empty object.
fn overlay(base: Option<&Value>, patch: &Value) -> Map<String, Value> {
    let mut out = base.map(spread_map).unwrap_or_default();
    for (k, v) in spread_map(patch) {
        out.insert(k, v);
    }
    out
}

/// The bucket a server list lives in; absent base means an empty map.
fn map_of(base: Option<&Value>) -> Map<String, Value> {
    base.map(spread_map).unwrap_or_default()
}

/// Set one key in a copy of `base`; absent base means an empty object.
fn with_entry(base: Option<&Value>, key: &str, value: Value) -> Map<String, Value> {
    let mut out = map_of(base);
    out.insert(key.to_string(), value);
    out
}

/// The one error an unparseable config may produce. Refusing is the point: a
/// write that proceeded would drop every sibling server in the file.
fn corrupt(file: &str, issues: Option<&Value>) -> Value {
    let error = format!("config file is corrupt, refusing to overwrite: {}", file);
    match issues {
        Some(i) => json!({ "ok": false, "error": error, "issues": i }),
        None => json!({ "ok": false, "error": error }),
    }
}

/// Recursive copy for the bundle exporter — deliberately *without* the
/// store's skip list: an exported skill must be complete.
fn copy_tree(src: &str, dest: &str) {
    if std::fs::create_dir_all(dest).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let from = join(src, &[&name]);
        let to = join(dest, &[&name]);
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => copy_tree(&from, &to),
            _ => {
                let _ = std::fs::copy(&from, &to);
            }
        }
    }
}

fn is_toml_path(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".toml")
}

fn is_yaml_path(file: &str) -> bool {
    let l = file.to_ascii_lowercase();
    l.ends_with(".yaml") || l.ends_with(".yml")
}

fn is_json_path(file: &str) -> bool {
    let l = file.to_ascii_lowercase();
    l.ends_with(".json") || l.ends_with(".jsonc")
}

/* ------------------------------------------------------------------ */
/* Keeper                                                              */
/* ------------------------------------------------------------------ */

pub struct Keeper {
    pub store: Store,
    pub user_home: String,
}

impl Keeper {
    pub fn new(store: Store, user_home: &str) -> Keeper {
        Keeper {
            store,
            user_home: user_home.to_string(),
        }
    }

    /// `~` resolved against the *current* user, not baked in.
    fn home_of(&self, agent: &Agent) -> String {
        resolve_home(agent.home, &self.user_home).unwrap_or_else(|| agent.home.to_string())
    }

    /// Drop the scan cache: it must not outlive a write.
    pub fn invalidate(&self) {
        assets::invalidate();
    }

    /// Cached lookup, with the one-scan cold-cache fallback.
    ///
    /// The fallback scan is deliberately the UNCACHED variant: it answers this
    /// one lookup and must not publish to the global read cache. Publishing
    /// made an id lookup clobber the index every other caller was using, which
    /// is what made `cached_index_operations` pass when run alone and fail
    /// under a full `cargo test` depending on interleaving.
    fn index_get(&self, id: &str) -> Option<Value> {
        if let Some(a) = assets::get(id) {
            return Some(a);
        }
        let projects = scan::read_projects(self.store.root_str());
        let (payload, _) = scan::scan_payload_uncached(&self.user_home, &projects);
        if let Some(list) = payload.get("effective").and_then(|v| v.as_array()) {
            if let Some(a) = list
                .iter()
                .find(|a| a.get("id").map(js_string).as_deref() == Some(id))
            {
                return Some(a.clone());
            }
        }
        // Still fall back to the cache: another thread may have published the
        // id while this scan ran.
        assets::get(id)
    }

    /* -------------------------------------------------------------- */
    /* render                                                          */
    /* -------------------------------------------------------------- */

    /// Skill rendering: frontmatter in the author's field order, then the body.
    pub fn render_skill(&self, data: &Value, body: &str, order: Option<&[&str]>) -> String {
        const DEFAULT_ORDER: [&str; 7] = [
            "name",
            "description",
            "version",
            "author",
            "license",
            "allowed-tools",
            "tags",
        ];
        frontmatter::serialize(data, body, order.unwrap_or(&DEFAULT_ORDER))
    }

    /* -------------------------------------------------------------- */
    /* skills                                                          */
    /* -------------------------------------------------------------- */

    /// Create or update a skill — one write per target, one transaction total.
    ///
    /// `input`: `{ data, body, targets: [{agentId, scope, projectId}], skillId }`
    pub fn save_skill(&self, input: &Value) -> Value {
        let data = input.get("data").cloned().unwrap_or_else(|| json!({}));
        let body = match input.get("body") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => js_string(other),
        };
        let targets: Vec<Value> = input
            .get("targets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let skill_id = input
            .get("skillId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut payload = Map::new();
        payload.insert("data".to_string(), data.clone());
        if let Some(b) = input.get("body") {
            payload.insert("body".to_string(), b.clone());
        }
        let issues = validate::validate_by_kind("skill", &Value::Object(payload));
        if validate::is_blocking(&issues) {
            return json!({ "ok": false, "blocked": true, "issues": issues });
        }

        // `slug` coerces like `String(name || 'unnamed')`: an
        // absent, null, empty or 0 name all become the literal "unnamed".
        let raw_name = data
            .get("name")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "unnamed".to_string());
        let name_slug = slug(&raw_name);
        let rendered = self.render_skill(&data, &body, None);

        let mut results: Vec<Value> = Vec::new();
        let mut ops: Vec<Op> = Vec::new();

        for t in &targets {
            let Some(dir) = self.skill_dir_for(t) else {
                results.push(json!({
                    "target": t,
                    "ok": false,
                    "error": format!(
                        "cannot resolve skill directory for {}/{}",
                        prop(t, "agentId"),
                        prop(t, "scope")
                    ),
                }));
                continue;
            };
            let folder = join(&dir, &[&name_slug]);
            let file = join(&folder, &["SKILL.md"]);
            let global = t.get("scope").and_then(|v| v.as_str()) == Some("global");
            if global && exists_dir(&folder) {
                // Update in place so extra bundled files beside SKILL.md stay.
                ops.push(Op::write(&file, &rendered));
            } else {
                ops.push(Op::mkdir(&folder));
                ops.push(Op::write(&file, &rendered));
            }
            results.push(json!({ "target": t, "ok": true, "file": file }));
        }

        if ops.is_empty() {
            let error = results
                .iter()
                .find(|r| r.get("ok").and_then(|v| v.as_bool()) != Some(true))
                .and_then(|r| r.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("no valid target")
                .to_string();
            return json!({ "ok": false, "issues": issues, "error": error });
        }

        let first = targets.first().cloned().unwrap_or_else(|| json!({}));
        let agent_id = first.get("agentId").map(js_string).unwrap_or_default();
        let mut meta = Map::new();
        meta.insert("kind".to_string(), json!("skill"));
        meta.insert(
            "action".to_string(),
            json!(if skill_id.is_some() {
                "update"
            } else {
                "create"
            }),
        );
        meta.insert(
            "agentId".to_string(),
            first.get("agentId").cloned().unwrap_or(Value::Null),
        );
        meta.insert(
            "agentLabel".to_string(),
            by_id(&agent_id)
                .map(|a| json!(a.label))
                .unwrap_or(Value::Null),
        );
        meta.insert(
            "projectId".to_string(),
            first.get("projectId").cloned().unwrap_or(Value::Null),
        );
        meta.insert(
            "projectName".to_string(),
            first
                .get("projectId")
                .and_then(|v| v.as_str())
                .and_then(registry::get)
                .and_then(|p| p.get("name").cloned())
                .unwrap_or(Value::Null),
        );
        meta.insert(
            "assetName".to_string(),
            data.get("name").cloned().unwrap_or(Value::Null),
        );
        meta.insert(
            "assetId".to_string(),
            skill_id.clone().map(Value::String).unwrap_or(Value::Null),
        );

        let mut tx = self.store.transaction(&ops, &Value::Object(meta));
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        let written: Vec<Value> = results
            .iter()
            .filter(|r| r.get("ok").and_then(|v| v.as_bool()) == Some(true))
            .filter_map(|r| r.get("file").cloned())
            .collect();
        if let Value::Object(m) = &mut tx {
            m.insert("issues".to_string(), json!(issues));
            m.insert("written".to_string(), json!(written));
        }
        tx
    }

    /// The directory a skill folder lives in, for one target.
    pub fn skill_dir_for(&self, t: &Value) -> Option<String> {
        let agent = by_id(&prop(t, "agentId"))?;
        if t.get("scope").and_then(|v| v.as_str()) == Some("global") {
            let rel = *agent.globals.skills.first()?;
            if rel.is_empty() {
                return None;
            }
            return Some(resolve(&self.home_of(agent), rel));
        }
        let proj = registry::get(t.get("projectId").and_then(|v| v.as_str())?)?;
        let dir = proj.get("dir").and_then(|v| v.as_str())?.to_string();
        let rel = *agent.projects.skills.first()?;
        if rel.is_empty() {
            return None;
        }
        Some(resolve(&dir, rel))
    }

    /// Update an existing md asset in place (rule / memory / skill file).
    pub fn save_markdown(&self, asset_id: &str, content: &str) -> Value {
        let Some(a) = self.index_get(asset_id) else {
            return json!({ "ok": false, "error": "asset not found" });
        };
        if a.get("writable").and_then(|v| v.as_bool()) == Some(false) {
            return json!({
                "ok": false,
                "error": "this asset is host-owned (plugin cache / shared dir) and cannot be edited here",
            });
        }

        let kind = a.get("kind").map(js_string).unwrap_or_default();
        if kind == "skill" {
            let fm = frontmatter::parse(content);
            let dir_name = a.get("dir").map(js_string).unwrap_or_default();
            let dir_name = if dir_name.is_empty() {
                String::new()
            } else {
                basename(&dir_name)
            };
            let issues = validate::validate_by_kind(
                "skill",
                &json!({ "data": fm.data, "body": fm.body, "dirName": dir_name }),
            );
            if validate::is_blocking(&issues) {
                return json!({ "ok": false, "blocked": true, "issues": issues });
            }
            // Renaming means moving the directory; refuse rather than write a
            // SKILL.md whose folder no longer matches its name.
            let new_name = fm.data.get("name").filter(|v| js_truthy(v)).map(js_string);
            if let Some(name) = new_name {
                if slug(&name) != dir_name {
                    return json!({
                        "ok": false,
                        "blocked": true,
                        "issues": [{
                            "level": "error",
                            "field": "name",
                            "message": format!(
                                "renaming a skill requires a move (directory is \"{}\")",
                                dir_name
                            ),
                        }],
                    });
                }
            }
        }

        let file = a.get("file").map(js_string).unwrap_or_default();
        let mut meta = Map::new();
        meta.insert(
            "kind".to_string(),
            a.get("kind").cloned().unwrap_or(Value::Null),
        );
        meta.insert("action".to_string(), json!("update"));
        for key in ["agentId", "agentLabel", "projectId", "projectName"] {
            meta.insert(key.to_string(), a.get(key).cloned().unwrap_or(Value::Null));
        }
        meta.insert("assetId".to_string(), json!(asset_id));
        meta.insert(
            "assetName".to_string(),
            a.get("name").cloned().unwrap_or(Value::Null),
        );

        let tx = self
            .store
            .transaction(&[Op::write(&file, content)], &Value::Object(meta));
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        tx
    }

    /* -------------------------------------------------------------- */
    /* MCP                                                             */
    /* -------------------------------------------------------------- */

    /// Which file an MCP server belongs in.
    ///
    /// The global list is searched, not just indexed: `workbuddy` declares
    /// `mcp.json, mcp-approvals.json, mcp-tool-list.json`, and writing a server
    /// into an approvals file would produce a config the agent reads as a
    /// decision record. The filter below is exactly that.
    pub fn mcp_target_for(
        &self,
        agent_id: &str,
        scope: &str,
        project_id: Option<&str>,
    ) -> Option<String> {
        let agent = by_id(agent_id)?;
        if scope == "global" {
            let rel = agent.globals.mcp.iter().copied().find(|r| {
                let is_cfg = r.ends_with(".json")
                    || r.ends_with(".jsonc")
                    || r.ends_with(".toml")
                    || r.ends_with(".yaml");
                let lower = r.to_ascii_lowercase();
                let is_bookkeeping = lower.contains("approvals") || lower.contains("tool-list");
                is_cfg && !is_bookkeeping
            })?;
            return Some(resolve(&self.home_of(agent), rel));
        }
        let proj = registry::get(project_id?)?;
        let dir = proj.get("dir").and_then(|v| v.as_str())?.to_string();
        let rel = *agent.projects.mcp.first()?;
        Some(resolve(&dir, rel))
    }

    /// The MCP bucket: copilot nests its servers under `servers`.
    pub fn mcp_bucket_for(&self, agent_id: &str) -> &'static str {
        match family(agent_id) {
            "copilot" => "servers",
            _ => "mcpServers",
        }
    }

    /// Create or update one MCP server in whatever format the agent reads.
    pub fn upsert_mcp(&self, input: &Value) -> Value {
        let name = input.get("name").map(js_string).unwrap_or_default();
        let config = input.get("config").cloned().unwrap_or_else(|| json!({}));
        let agent_id = input.get("agentId").map(js_string).unwrap_or_default();
        let scope = input
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global")
            .to_string();
        let project_id = input.get("projectId").and_then(|v| v.as_str());
        let asset_id = input
            .get("assetId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let issues = validate::validate_by_kind(
            "mcp",
            &json!({ "name": name.clone(), "config": config.clone() }),
        );
        if validate::is_blocking(&issues) {
            return json!({ "ok": false, "blocked": true, "issues": issues });
        }

        let Some(file) = self.mcp_target_for(&agent_id, &scope, project_id) else {
            return json!({
                "ok": false,
                "error": format!("no MCP config file declared for {} ({})", agent_id, scope),
            });
        };

        let mut meta = Map::new();
        meta.insert("kind".to_string(), json!("mcp"));
        meta.insert(
            "action".to_string(),
            json!(if asset_id.is_some() {
                "update"
            } else {
                "create"
            }),
        );
        meta.insert("agentId".to_string(), json!(agent_id));
        meta.insert(
            "projectId".to_string(),
            project_id.map(|p| json!(p)).unwrap_or(Value::Null),
        );
        meta.insert("assetName".to_string(), json!(name.clone()));
        meta.insert(
            "assetId".to_string(),
            asset_id.clone().map(Value::String).unwrap_or(Value::Null),
        );
        let meta = Value::Object(meta);

        let raw = read_all(&file);

        if is_yaml_path(&file) {
            let Ok(mut doc) = yaml::parse(&raw) else {
                return corrupt(&file, Some(&json!(issues)));
            };
            if !doc.is_object() {
                return corrupt(&file, Some(&json!(issues)));
            }
            let bucket = ["mcpServers", "servers", "mcp_servers"]
                .iter()
                .copied()
                .find(|b| truthy(doc.get(*b)))
                .unwrap_or("mcpServers")
                .to_string();
            let prev = doc.get(&bucket).and_then(|b| b.get(&name));
            let servers = with_entry(
                doc.get(&bucket),
                &name,
                Value::Object(overlay(prev, &config)),
            );
            if let Value::Object(m) = &mut doc {
                m.insert(bucket, Value::Object(servers));
            }
            // This emitter does not add a trailing newline.
            let mut body = yaml::stringify(&doc);
            if !body.ends_with('\n') {
                body.push('\n');
            }
            let mut tx = self.store.transaction(&[Op::write(&file, &body)], &meta);
            if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                self.invalidate();
            }
            if let Value::Object(m) = &mut tx {
                m.insert("issues".to_string(), json!(issues));
                m.insert("written".to_string(), json!([file]));
            }
            return tx;
        }

        if is_toml_path(&file) {
            let Ok(mut doc) = toml_splice::parse(&raw) else {
                return corrupt(&file, Some(&json!(issues)));
            };
            if !doc.is_object() {
                doc = json!({});
            }
            let prev = doc.get("mcp_servers").and_then(|s| s.get(&name));
            let server = Value::Object(overlay(prev, &config));
            let mut servers = Map::new();
            servers.insert(name.clone(), server);
            let mut root = Map::new();
            root.insert("mcp_servers".to_string(), Value::Object(servers));
            let section = match toml_splice::stringify(&Value::Object(root)) {
                Ok(s) => s,
                Err(_) => return corrupt(&file, Some(&json!(issues))),
            };
            let body = toml_splice::splice_server(&raw, &name, &section);
            let mut tx = self.store.transaction(&[Op::write(&file, &body)], &meta);
            if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                self.invalidate();
            }
            if let Value::Object(m) = &mut tx {
                m.insert("issues".to_string(), json!(issues));
                m.insert("written".to_string(), json!([file]));
            }
            return tx;
        }

        let bucket = self.mcp_bucket_for(&agent_id).to_string();
        let key = name.clone();
        let cfg = config.clone();
        let mut tx = self.store.transaction(
            &[Op::json_mutate(&file, move |cur| {
                let mut next = spread_map(&cur);
                let prev = next.get(&bucket).and_then(|b| b.get(&key));
                let servers =
                    with_entry(next.get(&bucket), &key, Value::Object(overlay(prev, &cfg)));
                next.insert(bucket.clone(), Value::Object(servers));
                Some(Value::Object(next))
            })],
            &meta,
        );
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        if let Value::Object(m) = &mut tx {
            m.insert("issues".to_string(), json!(issues));
            m.insert("written".to_string(), json!([file]));
        }
        tx
    }

    /// Delete one MCP server, leaving its siblings alone.
    pub fn delete_mcp(&self, asset_id: &str) -> Value {
        let Some(a) = self.index_get(asset_id) else {
            return json!({ "ok": false, "error": "asset not found" });
        };
        let name = a
            .get("meta")
            .and_then(|m| m.get("configKey"))
            .map(js_string)
            .unwrap_or_default();
        let file = a.get("file").map(js_string).unwrap_or_default();

        let mut meta = Map::new();
        meta.insert("kind".to_string(), json!("mcp"));
        meta.insert("action".to_string(), json!("delete"));
        meta.insert(
            "agentId".to_string(),
            a.get("agentId").cloned().unwrap_or(Value::Null),
        );
        meta.insert(
            "projectId".to_string(),
            a.get("projectId").cloned().unwrap_or(Value::Null),
        );
        meta.insert("assetName".to_string(), json!(name));
        meta.insert("assetId".to_string(), json!(asset_id));
        let meta = Value::Object(meta);
        let raw = read_all(&file);

        if is_toml_path(&file) {
            let Ok(doc) = toml_splice::parse(&raw) else {
                return corrupt(&file, None);
            };
            let present = doc.get("mcp_servers").and_then(|s| s.get(&name)).is_some();
            if !present {
                return json!({ "ok": false, "error": "asset not found" });
            }
            let body = toml_splice::remove_server(&raw, &name);
            let tx = self.store.transaction(&[Op::write(&file, &body)], &meta);
            if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                self.invalidate();
            }
            return tx;
        }

        if is_yaml_path(&file) {
            let Ok(mut doc) = yaml::parse(&raw) else {
                return corrupt(&file, None);
            };
            let bucket = ["mcpServers", "servers", "mcp_servers"]
                .iter()
                .copied()
                .find(|b| truthy(doc.get(*b).and_then(|m| m.get(&name))))
                .map(|b| b.to_string());
            let Some(bucket) = bucket else {
                return json!({ "ok": false, "error": "asset not found" });
            };
            if let Value::Object(m) = &mut doc {
                if let Some(Value::Object(servers)) = m.get_mut(&bucket) {
                    servers.remove(&name);
                }
            }
            let mut body = yaml::stringify(&doc);
            if !body.ends_with('\n') {
                body.push('\n');
            }
            let tx = self.store.transaction(&[Op::write(&file, &body)], &meta);
            if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                self.invalidate();
            }
            return tx;
        }

        if !is_json_path(&file) {
            return json!({
                "ok": false,
                "error": "only JSON-based MCP config files can be safely edited in place",
            });
        }

        let key = name.clone();
        let tx = self.store.transaction(
            &[Op::json_mutate(&file, move |cur| {
                let mut next = spread_map(&cur);
                for bucket in ["mcpServers", "servers"] {
                    if truthy(next.get(bucket).and_then(|b| b.get(&key))) {
                        let mut servers = next.get(bucket).map(spread_map).unwrap_or_default();
                        servers.remove(&key);
                        next.insert(bucket.to_string(), Value::Object(servers));
                    }
                }
                Some(Value::Object(next))
            })],
            &meta,
        );
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        tx
    }

    /* -------------------------------------------------------------- */
    /* delete / distribute                                             */
    /* -------------------------------------------------------------- */

    pub fn delete_asset(&self, asset_id: &str) -> Value {
        let Some(a) = self.index_get(asset_id) else {
            return json!({ "ok": false, "error": "asset not found" });
        };
        if a.get("writable").and_then(|v| v.as_bool()) == Some(false) {
            return json!({
                "ok": false,
                "error": "host-owned asset — delete it from the owning agent UI instead",
            });
        }
        let kind = a.get("kind").map(js_string).unwrap_or_default();
        // Defence in depth: an MCP asset's `file` is the *shared* config — a
        // plain delete would wipe every server that agent has. Callers may
        // forget to dispatch (the MCP tool did); the core never may.
        if kind == "mcp" {
            return self.delete_mcp(asset_id);
        }

        let target = if kind == "skill" {
            a.get("dir").map(js_string).unwrap_or_default()
        } else {
            a.get("file").map(js_string).unwrap_or_default()
        };
        let mut meta = Map::new();
        meta.insert("kind".to_string(), json!(kind));
        meta.insert("action".to_string(), json!("delete"));
        for key in ["agentId", "agentLabel", "projectId", "projectName"] {
            meta.insert(key.to_string(), a.get(key).cloned().unwrap_or(Value::Null));
        }
        meta.insert("assetId".to_string(), json!(asset_id));
        meta.insert(
            "assetName".to_string(),
            a.get("name").cloned().unwrap_or(Value::Null),
        );

        let tx = self
            .store
            .transaction(&[Op::delete(&target)], &Value::Object(meta));
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        tx
    }

    /// Copy an asset to other agents/scopes.
    ///
    /// Skill and markdown copies are batched into one transaction at the end;
    /// MCP copies run one transaction each, because they merge into a config
    /// file whose current content is only known at write time.
    pub fn distribute(&self, asset_id: &str, targets: &[Value]) -> Value {
        let Some(a) = self.index_get(asset_id) else {
            return json!({ "ok": false, "error": "asset not found" });
        };
        let kind = a.get("kind").map(js_string).unwrap_or_default();
        let name = a.get("name").map(js_string).unwrap_or_default();
        let name_slug = slug(&name);
        let mut ops: Vec<Op> = Vec::new();
        let mut written: Vec<Value> = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();

        for t in targets {
            let same_agent = prop(t, "agentId") == prop(&a, "agentId");
            let same_scope = prop(t, "scope") == prop(&a, "scope");
            let no_project =
                !truthy(t.get("projectId")) || prop(t, "projectId") == prop(&a, "projectId");
            if same_agent && same_scope && no_project {
                skipped.push(json!({ "target": t, "reason": "same location" }));
                continue;
            }

            match kind.as_str() {
                "skill" => {
                    let Some(dir) = self.skill_dir_for(t) else {
                        skipped
                            .push(json!({ "target": t, "reason": "agent has no skill directory" }));
                        continue;
                    };
                    let dest = join(&dir, &[&name_slug]);
                    if exists_dir(&dest) {
                        skipped.push(json!({ "target": t, "reason": "already exists" }));
                        continue;
                    }
                    let src = a.get("dir").map(js_string).unwrap_or_default();
                    ops.push(Op::copy_dir(&dest, &src));
                    written.push(json!(dest));
                }
                "rule" | "memory" => {
                    let agent_id = prop(t, "agentId");
                    let Some(agent) = by_id(&agent_id) else {
                        skipped.push(json!({ "target": t, "reason": "unresolved base dir" }));
                        continue;
                    };
                    let project = if prop(t, "scope") == "project" {
                        registry::get(
                            t.get("projectId")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default(),
                        )
                    } else {
                        None
                    };
                    let base = if prop(t, "scope") == "project" {
                        project.and_then(|p| {
                            p.get("dir").and_then(|v| v.as_str()).map(|s| s.to_string())
                        })
                    } else {
                        Some(self.home_of(agent))
                    };
                    let Some(base) = base else {
                        skipped.push(json!({ "target": t, "reason": "unresolved base dir" }));
                        continue;
                    };
                    let rels = if prop(t, "scope") == "project" {
                        match kind.as_str() {
                            "rule" => agent.projects.rules.to_vec(),
                            _ => agent.projects.memory.to_vec(),
                        }
                    } else {
                        match kind.as_str() {
                            "rule" => agent.globals.rules.to_vec(),
                            _ => agent.globals.memory.to_vec(),
                        }
                    };
                    let rel = rels
                        .first()
                        .copied()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("{}s", kind));
                    let src = a.get("file").map(js_string).unwrap_or_default();
                    let dest = join(&base, &[&rel, &basename(&src)]);
                    if let Some(parent) = Path::new(&dest).parent() {
                        ops.push(Op::mkdir(&store::native_str(parent)));
                    }
                    ops.push(Op::write(&dest, &read_all(&src)));
                    written.push(json!(dest));
                }
                "mcp" => {
                    let Some(cfg_file) = self.mcp_target_for(
                        &prop(t, "agentId"),
                        &prop(t, "scope"),
                        t.get("projectId").and_then(|v| v.as_str()),
                    ) else {
                        skipped.push(json!({
                            "target": t,
                            "reason": "op[0] (json) is missing a \"path\"",
                        }));
                        continue;
                    };
                    let cfg = a
                        .get("meta")
                        .and_then(|m| m.get("config"))
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let config_key = a
                        .get("meta")
                        .and_then(|m| m.get("configKey"))
                        .map(js_string)
                        .unwrap_or_else(|| name.clone());
                    let bucket = self.mcp_bucket_for(&prop(t, "agentId")).to_string();
                    let r = self.store.transaction(
                        &[Op::json_mutate(&cfg_file, move |cur| {
                            let mut next = spread_map(&cur);
                            let servers = with_entry(next.get(&bucket), &config_key, cfg.clone());
                            next.insert(bucket.clone(), Value::Object(servers));
                            Some(Value::Object(next))
                        })],
                        &json!({
                            "kind": "mcp",
                            "action": "distribute",
                            "agentId": t.get("agentId").cloned().unwrap_or(Value::Null),
                            "projectId": t.get("projectId").cloned().unwrap_or(Value::Null),
                            "assetName": name,
                            "assetId": asset_id,
                        }),
                    );
                    if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        self.invalidate();
                        if let Some(arr) = r.get("targets").and_then(|v| v.as_array()) {
                            written.extend(arr.iter().cloned());
                        }
                    } else {
                        let reason = r
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("write failed")
                            .to_string();
                        skipped.push(json!({ "target": t, "reason": reason }));
                    }
                }
                _ => {
                    skipped.push(json!({ "target": t, "reason": "unsupported asset kind" }));
                }
            }
        }

        if ops.is_empty() {
            return json!({ "ok": false, "error": "nothing to write", "skipped": skipped });
        }
        let first = targets.first().cloned().unwrap_or_else(|| json!({}));
        let mut tx = self.store.transaction(
            &ops,
            &json!({
                "kind": kind,
                "action": "distribute",
                "agentId": first.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": first.get("projectId").cloned().unwrap_or(Value::Null),
                "assetName": name,
                "assetId": asset_id,
            }),
        );
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        if let Value::Object(m) = &mut tx {
            m.insert("written".to_string(), json!(written));
            m.insert("skipped".to_string(), json!(skipped));
        }
        tx
    }

    /// Line diff for the preview panel.
    pub fn diff(&self, asset_id: &str, new_content: &str) -> Value {
        let Some(a) = self.index_get(asset_id) else {
            return json!({ "ok": false, "error": "asset not found" });
        };
        let file = a.get("file").map(js_string).unwrap_or_default();
        let old_text = read_all(&file);
        json!({
            "ok": true,
            "file": file,
            "oldText": old_text,
            "newText": new_content,
            "lines": toml_splice::line_diff(&old_text, new_content),
        })
    }

    /* -------------------------------------------------------------- */
    /* bundles                                                         */
    /* -------------------------------------------------------------- */

    /// Export assets into a portable folder (no zip dependency):
    ///
    /// ```text
    /// agentkeeper-bundle/
    ///   manifest.json   { schema, exportedAt, app, assets[] }
    ///   files/<n>/...   exact file/dir copies
    /// ```
    ///
    /// Only user-writable assets export; a vendor cache is not the user's to
    /// hand on.
    pub fn export_bundle(&self, asset_ids: &[Value], dir: &str) -> Value {
        if asset_ids.is_empty() {
            return json!({ "ok": false, "error": "no assets selected" });
        }
        if dir.is_empty() || !exists_dir(dir) {
            return json!({ "ok": false, "error": "target directory does not exist" });
        }
        let out = join(dir, &["agentkeeper-bundle"]);
        let files_dir = join(&out, &["files"]);
        let mut entries: Vec<Value> = Vec::new();
        let mut used: Vec<String> = Vec::new();

        for id in asset_ids {
            let Some(a) = self.index_get(&id.as_str().unwrap_or_default()) else {
                continue;
            };
            let writable = a.get("writable").and_then(|v| v.as_bool()).unwrap_or(true);
            let origin = a.get("origin").map(js_string).unwrap_or_default();
            if !writable && origin == "cached" {
                continue;
            }
            let name = a.get("name").map(js_string).unwrap_or_default();
            let agent_id = a.get("agentId").map(js_string).unwrap_or_default();
            let mut base = slug(&format!("{}-{}", name, agent_id));
            let mut n = 2;
            while used.contains(&base) {
                base = format!("{}-{}-{}", slug(&name), agent_id, n);
                n += 1;
            }
            used.push(base.clone());

            let kind = a.get("kind").map(js_string).unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("name".to_string(), json!(name));
            entry.insert("kind".to_string(), json!(kind));
            entry.insert("agentId".to_string(), json!(agent_id));
            entry.insert(
                "scope".to_string(),
                a.get("scope").cloned().unwrap_or(Value::Null),
            );
            entry.insert("dir".to_string(), json!(format!("files/{}", base)));

            let asset_dir = a.get("dir").map(js_string).unwrap_or_default();
            let asset_file = a.get("file").map(js_string).unwrap_or_default();
            if kind == "skill" && exists_dir(&asset_dir) {
                copy_tree(&asset_dir, &join(&files_dir, &[&base]));
            } else if !asset_file.is_empty() && exists_file(&asset_file) {
                let _ = std::fs::create_dir_all(join(&files_dir, &[&base]));
                let _ = std::fs::copy(
                    &asset_file,
                    join(&files_dir, &[&base, &basename(&asset_file)]),
                );
                entry.insert("fileName".to_string(), json!(basename(&asset_file)));
            } else {
                continue;
            }

            if kind == "mcp" {
                if let Some(meta) = a.get("meta") {
                    for key in ["config", "configKey", "transport"] {
                        if let Some(v) = meta.get(key) {
                            entry.insert(key.to_string(), v.clone());
                        }
                    }
                }
            }
            entries.push(Value::Object(entry));
        }

        if entries.is_empty() {
            return json!({ "ok": false, "error": "nothing exportable in selection" });
        }

        let manifest = json!({
            "schema": 1,
            "exportedAt": super::model::iso_from_millis(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            ),
            "app": "agentkeeper",
            "assets": entries,
        });
        if std::fs::create_dir_all(&files_dir).is_err() {
            return json!({ "ok": false, "error": "write failed: cannot create bundle directory" });
        }
        let body = serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_string());
        if let Err(e) = std::fs::write(join(&out, &["manifest.json"]), body) {
            return json!({ "ok": false, "error": format!("write failed: {}", e) });
        }

        let count = entries.len();
        settings::audit(
            self.store.root_str(),
            &json!({
                "kind": "bundle",
                "action": "export",
                "status": "ok",
                "assetName": format!("{} -> {}", count, out),
            }),
        );
        json!({ "ok": true, "out": out, "count": count })
    }

    /// Import a bundle folder back into the real agent directories — one
    /// transaction for the whole import, so a half-import cannot happen.
    pub fn import_bundle(&self, dir: &str) -> Value {
        let manifest_path = join(dir, &["agentkeeper-bundle", "manifest.json"]);
        if !exists_file(&manifest_path) {
            return json!({
                "ok": false,
                "error": "not an agentkeeper bundle (manifest.json missing)",
            });
        }
        let Ok(manifest) = serde_json::from_str::<Value>(&read_all(&manifest_path)) else {
            return json!({ "ok": false, "error": "manifest unreadable" });
        };
        if manifest.get("schema").and_then(|v| v.as_i64()) != Some(1) {
            return json!({
                "ok": false,
                "error": format!("unsupported bundle schema: {}", prop(&manifest, "schema")),
            });
        }
        let bundle_root = manifest_path
            .rsplit_once(['\\', '/'])
            .map(|(head, _)| head.to_string())
            .unwrap_or_else(|| dir.to_string());

        let mut ops: Vec<Op> = Vec::new();
        let mut names: Vec<Value> = Vec::new();
        let assets = manifest
            .get("assets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for e in &assets {
            let agent_id = prop(e, "agentId");
            let Some(agent) = by_id(&agent_id) else {
                continue;
            };
            let src = join(
                &bundle_root,
                &[e.get("dir").and_then(|v| v.as_str()).unwrap_or("")],
            );
            if !exists_dir(&src) {
                continue;
            }
            names.push(e.get("name").cloned().unwrap_or(Value::Null));
            let kind = prop(e, "kind");
            let name = prop(e, "name");

            match kind.as_str() {
                "skill" => {
                    let Some(dir2) = self.skill_dir_for(&json!({
                        "agentId": agent_id,
                        "scope": "global",
                    })) else {
                        continue;
                    };
                    let dest = join(&dir2, &[&slug(&name)]);
                    if exists_dir(&dest) {
                        continue;
                    }
                    ops.push(Op::copy_dir(&dest, &src));
                }
                "rule" | "memory" => {
                    let rels = if kind == "rule" {
                        agent.globals.rules
                    } else {
                        agent.globals.memory
                    };
                    let rel = rels
                        .first()
                        .copied()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("{}s", kind));
                    let file_name = e
                        .get("fileName")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("{}.md", slug(&name)));
                    let dest = join(&self.home_of(agent), &[&rel, &file_name]);
                    if let Some(parent) = Path::new(&dest).parent() {
                        ops.push(Op::mkdir(&store::native_str(parent)));
                    }
                    ops.push(Op::write(&dest, &read_all(&join(&src, &[&file_name]))));
                }
                "mcp" => {
                    let Some(cfg) = e.get("config").filter(|v| js_truthy(v)).cloned() else {
                        continue;
                    };
                    let Some(file) = self.mcp_target_for(&agent_id, "global", None) else {
                        continue;
                    };
                    let bucket = self.mcp_bucket_for(&agent_id).to_string();
                    let key = e
                        .get("configKey")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| name.clone());
                    ops.push(Op::json_mutate(&file, move |cur| {
                        let mut next = spread_map(&cur);
                        let servers = with_entry(next.get(&bucket), &key, cfg.clone());
                        next.insert(bucket.clone(), Value::Object(servers));
                        Some(Value::Object(next))
                    }));
                }
                _ => {}
            }
        }

        if ops.is_empty() {
            return json!({ "ok": false, "error": "nothing importable found" });
        }
        let mut tx = self.store.transaction(
            &ops,
            &json!({
                "kind": "bundle",
                "action": "import",
                "assetName": format!("{} assets", names.len()),
            }),
        );
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
        }
        if let Value::Object(m) = &mut tx {
            m.insert("imported".to_string(), json!(names));
        }
        tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ak-keeper-{}-{}-{}",
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

    #[test]
    fn mcp_bucket_puts_copilot_under_servers() {
        let _guard = assets::lock_for_test();
        let root = temp("bucket");
        let home = temp("bucket-home");
        let k = keeper_at(&root, &home);
        assert_eq!(k.mcp_bucket_for("copilot"), "servers");
        assert_eq!(k.mcp_bucket_for("claude-code"), "mcpServers");
        assert_eq!(k.mcp_bucket_for("continue"), "mcpServers");
        assert_eq!(k.mcp_bucket_for("nope"), "mcpServers");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn mcp_target_skips_bookkeeping_files() {
        let _guard = assets::lock_for_test();
        let root = temp("target");
        let home = temp("target-home");
        let k = keeper_at(&root, &home);

        // workbuddy declares mcp.json, mcp-approvals.json, mcp-tool-list.json —
        // only the first is a server list.
        let t = k.mcp_target_for("workbuddy", "global", None).unwrap();
        assert!(t.ends_with("mcp.json"), "{}", t);
        assert!(!t.contains("approvals") && !t.contains("tool-list"));

        // codex's first MCP entry is a .toml file, so it wins over mcp.json.
        let t = k.mcp_target_for("codex", "global", None).unwrap();
        assert!(t.ends_with("config.toml"), "{}", t);

        assert!(k.mcp_target_for("not-an-agent", "global", None).is_none());
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_skill_writes_the_rendered_file() {
        let _guard = assets::lock_for_test();
        let root = temp("save-skill");
        let home = temp("save-skill-home");
        let k = keeper_at(&root, &home);

        // `validate_skill` checks the *raw* name against
        // `^[a-z0-9][a-z0-9-]{0,63}$`, so "PDF Tools" is blocked before `slug`
        // ever runs — a name only survives if it is already a slug.
        let data = json!({ "name": "pdf-tools", "description": "Extract text from PDFs" });
        let r = k.save_skill(&json!({
            "data": data,
            "body": "Use it well.\n",
            "targets": [{ "agentId": "claude-code", "scope": "global" }],
        }));
        assert_eq!(r["ok"], json!(true), "{}", r);
        assert_eq!(r["issues"], json!([]));

        // `skill_dir_for` resolves `~/.claude/skills` against the user home.
        let file = home
            .join(".claude")
            .join("skills")
            .join("pdf-tools")
            .join("SKILL.md");
        assert_eq!(r["written"], json!([file.to_string_lossy()]));
        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            written,
            k.render_skill(&data, "Use it well.\n", None),
            "the file must be exactly what render_skill produces"
        );
        assert!(written.starts_with("---\n"));
        assert!(written.ends_with("---\nUse it well.\n"));
        assert!(r["txId"].as_str().unwrap().starts_with("tx-"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_skill_reports_an_unresolvable_target() {
        let _guard = assets::lock_for_test();
        let root = temp("save-skill-bad");
        let home = temp("save-skill-bad-home");
        let k = keeper_at(&root, &home);

        // A non-empty body is required: `validate_skill` blocks an empty one
        // first, and the blocked return has no `error` key.
        let r = k.save_skill(&json!({
            "data": { "name": "x", "description": "y" },
            "body": "body\n",
            "targets": [{ "agentId": "ghost", "scope": "global" }],
        }));
        assert_eq!(r["ok"], json!(false));
        assert_eq!(
            r["error"],
            json!("cannot resolve skill directory for ghost/global")
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_skill_is_blocked_by_validation_before_touching_disk() {
        let _guard = assets::lock_for_test();
        let root = temp("save-skill-invalid");
        let home = temp("save-skill-invalid-home");
        let k = keeper_at(&root, &home);

        // No name: validate_skill treats that as a blocking error.
        let r = k.save_skill(&json!({
            "data": { "description": "no name" },
            "body": "",
            "targets": [{ "agentId": "claude-code", "scope": "global" }],
        }));
        assert_eq!(r["blocked"], json!(true), "{}", r);
        assert!(!home.join(".claude").join("skills").exists());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn upsert_mcp_json_merges_and_keeps_siblings() {
        let _guard = assets::lock_for_test();
        let root = temp("upsert");
        let home = temp("upsert-home");
        // claude-code's home is `~/.claude`, so its global MCP config is
        // `<home>/.claude/.mcp.json` — not `<home>/.mcp.json`.
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join(".mcp.json"),
            r#"{"mcpServers":{"keep":{"command":"a"}}}"#,
        )
        .unwrap();
        let k = keeper_at(&root, &home);

        let r = k.upsert_mcp(&json!({
            "name": "added",
            "config": { "command": "b" },
            "agentId": "claude-code",
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(true), "{}", r);
        let after: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".claude").join(".mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(after["mcpServers"]["keep"]["command"], json!("a"));
        assert_eq!(after["mcpServers"]["added"]["command"], json!("b"));
        assert_eq!(
            r["written"],
            json!([home.join(".claude").join(".mcp.json").to_string_lossy()])
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn upsert_mcp_refuses_a_corrupt_config() {
        let _guard = assets::lock_for_test();
        let root = temp("upsert-corrupt");
        let home = temp("upsert-corrupt-home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude").join(".mcp.json"), "{ not json ").unwrap();
        let k = keeper_at(&root, &home);

        let r = k.upsert_mcp(&json!({
            "name": "x",
            "config": { "command": "b" },
            "agentId": "claude-code",
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(false));
        assert!(
            r["error"].as_str().unwrap().contains("corrupt"),
            "{}",
            r["error"]
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".claude").join(".mcp.json")).unwrap(),
            "{ not json "
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn import_bundle_lands_in_the_agent_home() {
        let _guard = assets::lock_for_test();
        let root = temp("import");
        let home = temp("import-home");
        let bundle = temp("import-bundle");
        let files = bundle
            .join("agentkeeper-bundle")
            .join("files")
            .join("pdf-x");
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("SKILL.md"), "---\nname: pdf\n---\nbody\n").unwrap();
        std::fs::write(
            bundle.join("agentkeeper-bundle").join("manifest.json"),
            serde_json::to_string(&json!({
                "schema": 1,
                "app": "agentkeeper",
                "assets": [{
                    "name": "pdf",
                    "kind": "skill",
                    "agentId": "claude-code",
                    "scope": "global",
                    "dir": "files/pdf-x",
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let k = keeper_at(&root, &home);
        let r = k.import_bundle(&bundle.to_string_lossy());
        assert_eq!(r["ok"], json!(true), "{}", r);
        assert_eq!(r["imported"], json!(["pdf"]));
        // A skill lands in `skill_dir_for({scope:'global'})` = `~/.claude/skills`.
        assert!(home
            .join(".claude")
            .join("skills")
            .join("pdf")
            .join("SKILL.md")
            .is_file());

        // A second import is a no-op: the folder already exists.
        let again = k.import_bundle(&bundle.to_string_lossy());
        assert_eq!(again["ok"], json!(false));
        assert_eq!(again["error"], json!("nothing importable found"));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&bundle).ok();
    }

    #[test]
    fn import_bundle_rejects_a_foreign_folder() {
        let _guard = assets::lock_for_test();
        let root = temp("import-foreign");
        let home = temp("import-foreign-home");
        let bundle = temp("import-foreign-bundle");
        let k = keeper_at(&root, &home);
        let r = k.import_bundle(&bundle.to_string_lossy());
        assert_eq!(
            r["error"],
            json!("not an agentkeeper bundle (manifest.json missing)")
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&bundle).ok();
    }

    /// Everything below resolves an id through the process-global scan cache, so
    /// it must hold the cache lock (see `assets::lock_for_test`).
    #[test]
    fn cached_index_operations() {
        let _guard = assets::lock_for_test();
        let root = temp("cached");
        let home = temp("cached-home");
        let k = keeper_at(&root, &home);

        // --- delete_mcp removes exactly one server -----------------------
        let cfg = home.join(".mcp.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"keep":{"command":"a"},"drop":{"command":"b"}}}"#,
        )
        .unwrap();
        let mcp_asset = json!({
            "id": "mcp:drop",
            "kind": "mcp",
            "name": "drop",
            "agentId": "claude-code",
            "agentLabel": "Claude Code",
            "scope": "global",
            "projectId": Value::Null,
            "dir": home.to_string_lossy(),
            "file": cfg.to_string_lossy(),
            "writable": true,
            "meta": { "configKey": "drop" },
        });
        assets::remember(
            std::slice::from_ref(&mcp_asset),
            std::slice::from_ref(&mcp_asset),
        );
        let r = k.delete_mcp("mcp:drop");
        assert_eq!(r["ok"], json!(true), "{}", r);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(after["mcpServers"].get("drop").is_none());
        assert_eq!(after["mcpServers"]["keep"]["command"], json!("a"));

        // --- delete_asset dispatches mcp to delete_mcp -------------------
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"keep":{"command":"a"},"drop":{"command":"b"}}}"#,
        )
        .unwrap();
        assets::remember(
            std::slice::from_ref(&mcp_asset),
            std::slice::from_ref(&mcp_asset),
        );
        let r = k.delete_asset("mcp:drop");
        assert_eq!(r["ok"], json!(true), "{}", r);
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("keep"));

        // --- distribute skips the asset's own location -------------------
        std::fs::create_dir_all(home.join("skills").join("pdf")).unwrap();
        std::fs::write(home.join("skills").join("pdf").join("SKILL.md"), "x").unwrap();
        let skill = json!({
            "id": "skill:pdf",
            "kind": "skill",
            "name": "pdf",
            "agentId": "claude-code",
            "agentLabel": "Claude Code",
            "scope": "global",
            "projectId": Value::Null,
            "dir": home.join("skills").join("pdf").to_string_lossy(),
            "file": home.join("skills").join("pdf").join("SKILL.md").to_string_lossy(),
            "writable": true,
            "meta": {},
        });
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let r = k.distribute(
            "skill:pdf",
            &[json!({ "agentId": "claude-code", "scope": "global" })],
        );
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"], json!("nothing to write"));
        assert_eq!(r["skipped"][0]["reason"], json!("same location"));

        // --- save_markdown refuses a rename ------------------------------
        // The rename guard only runs once `validate_skill` passes, and that
        // needs a description; without one the missing-description error (and
        // the dirName warning) is what comes back instead.
        let r = k.save_markdown("skill:pdf", "---\nname: other\ndescription: d\n---\nbody\n");
        assert_eq!(r["blocked"], json!(true), "{}", r);
        assert_eq!(
            r["issues"][0]["message"],
            json!("renaming a skill requires a move (directory is \"pdf\")")
        );

        // --- export_bundle writes a manifest and copies the folder -------
        // Re-seed the index first: `distribute` and `save_markdown` above both
        // end in `invalidate()`, which empties the process-global cache. On a
        // cold cache `index_get` falls back to a full `scan_payload` of
        // `self.user_home` — the REAL home, not this test's temp tree — so
        // whether `skill:pdf` resolves here depends on what the machine happens
        // to have installed. Without this line the test passed when run alone
        // and failed ~2 runs in 3 under `cargo test` (other tests holding the
        // cache lock change the interleaving). The re-seed makes the test
        // deterministic instead of load-dependent.
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let out = temp("cached-export");
        let r = k.export_bundle(&[json!("skill:pdf")], &out.to_string_lossy());
        assert_eq!(r["ok"], json!(true), "{}", r);
        assert_eq!(r["count"], json!(1));
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(out.join("agentkeeper-bundle").join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["schema"], json!(1));
        assert_eq!(manifest["app"], json!("agentkeeper"));
        assert_eq!(manifest["assets"][0]["name"], json!("pdf"));
        assert_eq!(manifest["assets"][0]["dir"], json!("files/pdf-claude-code"));
        assert!(out
            .join("agentkeeper-bundle")
            .join("files")
            .join("pdf-claude-code")
            .join("SKILL.md")
            .is_file());

        // --- diff reports the changed lines ------------------------------
        // Re-seed again: `export_bundle` itself does not touch the cache, but
        // the test suite runs in parallel and any other test that completes a
        // write calls `invalidate()`. The cache lock serialises the writes, not
        // the reads, so asserting on `diff` without re-seeding makes this test
        // depend on scheduling order.
        assets::remember(std::slice::from_ref(&skill), std::slice::from_ref(&skill));
        let r = k.diff("skill:pdf", "line one\nline two\n");
        assert_eq!(r["ok"], json!(true));
        assert_eq!(
            r["file"],
            json!(home
                .join("skills")
                .join("pdf")
                .join("SKILL.md")
                .to_string_lossy())
        );
        assert!(!r["lines"].as_array().unwrap().is_empty());

        assets::invalidate();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&out).ok();
    }
}
