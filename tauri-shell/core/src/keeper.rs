//! 变更服务。
//!
//! 每一个 CRUD 动作都落在这里，所以校验、审计和写入路径的失败撤销都无法从 UI、
//! MCP 服务器或将来的 CLI 绕过去。每次写入都走
//! [`super::store::Store::transaction`]；**本模块中任何东西都不得为了改动文件
//! 而去碰文件系统**。正是这一条规则让"一次失败的写入让目录树原封不动"成为
//! 一句真话。
//!
//! ## 解析顺序
//!
//! 从上一次扫描中解析出的资产 id 决定改动*哪个*文件；适配器表决定一个新文件
//! 去*哪里*。有两个值得说明的结果：
//!
//! * 上一次扫描从未产出过的 id 就是 `asset not found` —— 绝不猜一条路径。
//!   缓存冷的时候会先跑一次扫描。
//! * 整文件读取绝不抛出；缺失的文件读作 `""`。唯一把"读不了"当作致命错误的
//!   地方，是一个存在但解析不了的配置：`upsert_mcp` / `delete_mcp` 拒绝覆盖它，
//!   因为继续下去会悄悄丢掉文件里所有兄弟服务器。
//!
//! ## yaml / toml（YAML 与 TOML）
//!
//! YAML 和 TOML 的 MCP 配置是逐字段重写的，而不是按字节拼接：`yaml` 重新输出
//! 整个文档（注释会丢失 —— 解析器把它们丢掉了），`toml` 只重新输出那一个
//! 段落再拼回去，所以*在*该服务器*之外*的注释得以存活。

use serde_json::{json, Map, Value};
use std::path::Path;

use super::adapters::{by_id, family, resolve_home, Agent};
use super::assets;
use super::frontmatter;
use super::library;
use super::linkfs::{self, LinkMethod};
use super::model::{basename, js_string, js_truthy, slug};
use super::registry;
use super::scan;
use super::settings;
use super::sharing;
use super::store::{self, Op, Store};
use super::toml_splice;
use super::validate;
use super::yaml;

/* ------------------------------------------------------------------ */
/* 小型的强制转换辅助函数                                                    */
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

/// 把 `base` 与 `parts` 连接起来。
fn join(base: &str, parts: &[&str]) -> String {
    let mut p = std::path::PathBuf::from(base);
    for part in parts {
        p.push(part);
    }
    store::native_str(&p)
}

/// 绝对路径连接，`.`/`..` 被折叠。
fn resolve(base: &str, rel: &str) -> String {
    if Path::new(rel).is_absolute() {
        return store::normalize_path(rel);
    }
    store::normalize_path(&store::native_str(
        &std::path::PathBuf::from(base).join(rel),
    ))
}

/// 对象展开 `{ ...v }`；数组或字符串则用下标键。
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

/// 把 `patch` 浅合并到 `base` 之上；base 缺失即为空对象。
fn overlay(base: Option<&Value>, patch: &Value) -> Map<String, Value> {
    let mut out = base.map(spread_map).unwrap_or_default();
    for (k, v) in spread_map(patch) {
        out.insert(k, v);
    }
    out
}

/// 服务器列表所在的桶；base 缺失即为空映射。
fn map_of(base: Option<&Value>) -> Map<String, Value> {
    base.map(spread_map).unwrap_or_default()
}

/// 在 `base` 的一份副本里设置一个键；base 缺失即为空对象。
fn with_entry(base: Option<&Value>, key: &str, value: Value) -> Map<String, Value> {
    let mut out = map_of(base);
    out.insert(key.to_string(), value);
    out
}

/// 解析不了的配置可能产生的唯一一个错误。拒绝正是要点：一个继续下去的写入会
/// 丢掉文件里所有兄弟服务器。
fn corrupt(file: &str, issues: Option<&Value>) -> Value {
    let error = format!("config file is corrupt, refusing to overwrite: {}", file);
    match issues {
        Some(i) => json!({ "ok": false, "error": error, "issues": i }),
        None => json!({ "ok": false, "error": error }),
    }
}

/// 供 bundle 导出器使用的递归拷贝 —— 刻意*不*带 store 的跳过列表：导出的
/// skill 必须是完整的。
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
/* Keeper（保管器）                                                     */
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

    /// `~` 按*当前*用户解析，而不是写死在代码里。
    fn home_of(&self, agent: &Agent) -> String {
        resolve_home(agent.home, &self.user_home).unwrap_or_else(|| agent.home.to_string())
    }

    /// 丢掉扫描缓存：它的寿命不能超过一次写入。
    pub fn invalidate(&self) {
        assets::invalidate();
    }

    /// 按 slug 现扫一次，找出某个 agent 侧那份**真实副本**（不是链接）。
    ///
    /// `agent_id` 为空时取第一个匹配到的 agent。命中要求三件事同时成立：
    /// `kind == "skill"`、名字的 slug 相同、`meta.link` 为空（已经是链接的副本
    /// 没有可搬的内容，把它当来源会让 `copyDir` 顺着链接再拷一份）。
    ///
    /// 与 [`Self::index_get`] 一样用**不缓存**的扫描：这次查找只为当前调用服务，
    /// 发布到全局缓存会让别的调用方看到一份它没要求的索引。
    fn scan_for_skill(&self, slug_name: &str, agent_id: &str) -> Option<(String, String)> {
        let projects = scan::read_projects(self.store.root_str());
        let (payload, _) = scan::scan_payload_uncached(&self.user_home, &projects);
        // 库内主副本不能当来源：默认库根同时是伪 agent `shared` 的 skills 目录，
        // 让它命中会把库内目录拷到「库内同名位置」——那是自我覆盖。真实路径上的
        // 判断（`already in the library`）有第二道闸，这里在选来源时就排除掉。
        let lib = self.library();
        payload
            .get("assets")
            .and_then(Value::as_array)?
            .iter()
            .filter(|a| prop(a, "kind") == "skill")
            .filter(|a| slug(&prop(a, "name")) == slug_name)
            .filter(|a| agent_id.is_empty() || prop(a, "agentId") == agent_id)
            .filter(|a| prop(a, "agentId") != "shared")
            .filter(|a| {
                a.get("meta")
                    .and_then(|m| m.get("link"))
                    .and_then(Value::as_str)
                    .map(str::is_empty)
                    .unwrap_or(true)
            })
            .find_map(|a| {
                let dir = a.get("dir").and_then(Value::as_str)?;
                if dir.is_empty()
                    || library::is_under(Path::new(dir), lib.root())
                {
                    return None;
                }
                Some((
                    store::native_str(Path::new(dir)),
                    prop(a, "agentId"),
                ))
            })
    }

    /// 带一次扫描冷缓存兜底的缓存查找。
    ///
    /// 兜底那次扫描刻意用**不缓存**的变体：它只回答这一次查找，绝不能发布到
    /// 全局读缓存。发布会让一次 id 查找覆盖掉其它每个调用方正在使用的索引，而这
    /// 正是让 `cached_index_operations` 单独跑时通过、在完整 `cargo test` 下却
    /// 取决于交错顺序而失败的原因。
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
        // 仍然退回缓存：这次扫描运行期间，另一个线程可能已经发布了这个 id。
        assets::get(id)
    }

    /* -------------------------------------------------------------- */
    /* 渲染                                                             */
    /* -------------------------------------------------------------- */

    /// skill 渲染：按作者给出的字段顺序写 frontmatter，然后是正文。
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
    /* skills（技能）                                                    */
    /* -------------------------------------------------------------- */

    /// 创建或更新一个 skill —— 每个目标一次写入，总共一个事务。
    ///
    /// `input` 的字段：`{ data, body, targets: [{agentId, scope, projectId}], skillId }`。
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

        // `slug` 的强制转换与 `String(name || 'unnamed')` 一样：缺失、null、空
        // 或 0 的 name 都变成字面量 "unnamed"。
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
                // 就地更新，这样 SKILL.md 旁边额外打包的文件能保留。
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

    /// 某个目标的 skill 文件夹所在的目录。
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

    /// 就地更新一个已有的 md 资产（rule / memory / skill 文件）。
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
            // 改名意味着要移动目录；与其写出一个文件夹不再与名字匹配的
            // SKILL.md，不如拒绝。
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
    /* MCP（MCP 服务器）                                                 */
    /* -------------------------------------------------------------- */

    /// 某个 MCP 服务器该属于哪个文件。
    ///
    /// 这里搜索的是全局列表，而不只是取第一项：`workbuddy` 声明了
    /// `mcp.json, mcp-approvals.json, mcp-tool-list.json`，而把一个服务器写进
    /// approvals 文件会产出一份 agent 会当成决策记录来读的 配置。下面的过滤器
    /// 正是为此。
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

    /// MCP 所在的桶：copilot 把它的服务器嵌在 `servers` 之下。
    pub fn mcp_bucket_for(&self, agent_id: &str) -> &'static str {
        match family(agent_id) {
            "copilot" => "servers",
            _ => "mcpServers",
        }
    }

    /// 以 agent 所读的那种格式创建或更新一个 MCP 服务器。
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
            // 这个输出器不会追加结尾的换行。
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

    /// 删除一个 MCP 服务器，不动它的兄弟们。
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
    /* 删除 / 分发                                                         */
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
        // 纵深防御：一个 MCP 资产的 `file` 是那个*共享*配置 —— 一次普通删除会抹掉
        // 那个 agent 的所有服务器。调用方可能会忘记分发（MCP 工具就曾如此）；
        // 但核心层绝不允许。
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

    /// 把一个资产拷贝到其它 agent / scope。
    ///
    /// skill 和 markdown 的拷贝在最后被批成一个事务；MCP 的拷贝各自一个事务，
    /// 因为它们要合并进一个配置文件，而那个文件的当前内容只有在写入时才知道。
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

    /// 供预览面板使用的逐行差异。
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
    /* bundles（打包导出导入）                                            */
    /* -------------------------------------------------------------- */

    /// 把资产导出到一个可移植的文件夹里（不依赖 zip）：
    ///
    /// ```text
    /// agentkeeper-bundle/
    ///   manifest.json   清单 { schema, exportedAt, app, assets[] }
    ///   files/<n>/...   文件与目录的精确副本
    /// ```
    ///
    /// 只有用户可写的资产会被导出；厂商缓存不是用户可以转手的东西。
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

    /// 把一个 bundle 文件夹导入回真实的 agent 目录 —— 整个导入是一个事务，
    /// 所以半导入不可能发生。
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

    /* -------------------------------------------------------------- */
    /* 共享（M2）：启用到库 / 停用 / 从库里删                             */
    /* -------------------------------------------------------------- */

    /// 当前的库句柄。每次现算：库位置可以被设置切换，缓存进字段就会在切换后指向旧根。
    pub fn library(&self) -> library::Library {
        library::Library::resolve(&self.user_home, &self.settings())
    }

    fn settings(&self) -> Value {
        settings::read(self.store.root_str())
    }

    /// 设置里的链接方式偏好（`linkMethod`，`auto` = 交给能力顺序）。
    ///
    /// `pub(crate)`：`shared_ops` 的重链 / 导入动作要按同一份偏好建链接，
    /// 不能让新建的链接与用户设置里选的方式不一致。
    pub(crate) fn link_prefer(&self) -> Option<LinkMethod> {
        let raw = self
            .settings()
            .get("linkMethod")
            .and_then(Value::as_str)
            .unwrap_or("auto")
            .to_string();
        store::parse_link_method(&raw)
    }

    /// 库内某个 `<kind>/<slug>` 主副本是否已经存在（只有 skill 有目录形态）。
    fn library_asset_dir(&self, kind: &str, slug: &str) -> Option<String> {
        let lib = self.library();
        let dir = match kind {
            "skill" => lib.skill_dir(slug),
            "rule" => lib.dir_for("rule").join(format!("{}.md", slug)),
            "memory" => lib.dir_for("memory").join(format!("{}.md", slug)),
            _ => return None,
        };
        dir.exists().then(|| store::native_str(&dir))
    }

    /// 把库内资产启用到某个 agent / scope —— **删 + 建链接在同一个事务里**。
    ///
    /// `input`：`{ assetId?, name?, kind?, slug?, agentId, scope, projectId? }`。
    /// `assetId` 给定时从扫描结果里取来源路径（最常见的调用方式）；否则要求
    /// `name` + `kind` 指向一份**已经在库里**的主副本（用于「补启用」，例如库内
    /// 已有的 skill 还没挂到某个 agent 上）。
    ///
    /// op 顺序不能变：目标已存在时 `linkDir` 会直接报「目标已被占用」，所以真实
    /// 目录必须先删；而删除与建链接只有在同一个事务里才可能一起回滚
    /// （`docs/SHARED-ASSETS-IMPLEMENTATION.md` §11.3）。
    ///
    /// **`kind` 为 `rule` / `memory` 时是另一套编排**：目标是单个文件（`link` 或
    /// `compose`，见 [`Self::enable_rule`]），不走这里的目录收敛逻辑。
    ///
    /// **`kind` 为 `mcp` 时也是另一套编排**（M6）：MCP 的事实源是库内规范形态
    /// `<root>/mcp/servers/<slug>.json`，写盘目标是 agent 的 JSON/TOML 配置里的
    /// **一条**服务器。§17.9 明确允许「扩这个分支」，只要每种 kind 走自己的编排 ——
    /// 这里的 `mcp` 分支只是把 `sharing::materialize_plan` 算出的 op 送进事务，
    /// 与目录收敛那条路没有任何共用语义（详见 [`Self::enable_mcp`]）。
    pub fn enable_asset(&self, input: &Value) -> Value {
        let lib = self.library();
        // 用 `non_empty` 而不是 `prop`：`prop` 对缺失字段返回字符串 "undefined"
        // （它模仿 JS 的 `String(undefined)`），在这里会把「没给 kind」读成
        // 「给了个不认识的 kind」。
        let kind = non_empty(input, "kind").unwrap_or_else(|| "skill".to_string());
        if kind == "rule" || kind == "memory" {
            return self.enable_rule(&lib, &kind, input);
        }
        if kind == "mcp" {
            return self.enable_mcp(&lib, input);
        }
        if kind != "skill" {
            return unsatisfied("share.unsupportedKind");
        }
        // 来源与名字：给了 assetId 就以扫描结果为准，否则用 name/kind 在库里找。
        let (name, from, from_agent) = match non_empty(input, "assetId") {
            Some(id) => {
                let Some(a) = self.index_get(&id) else {
                    return unsatisfied("share.unknownAsset");
                };
                (
                    prop(&a, "name"),
                    // 扫描报出来的 `dir` 可能混着 `/`（agent 的 home token 用 `/` 拼），
                    // 而 `Op::copy_dir` 的前置校验用的是 `Path::is_dir()`，在 Windows 上
                    // 它对 `/` 分隔的路径并不总是成立。统一成原生分隔符再进事务。
                    a.get("dir")
                        .and_then(|v| v.as_str())
                        .map(|d| store::native_str(Path::new(d)))
                        .unwrap_or_default(),
                    prop(&a, "agentId"),
                )
            }
            None => (prop(input, "name"), String::new(), String::new()),
        };
        if name.is_empty() {
            return unsatisfied("share.unknownAsset");
        }
        let slug_name = slug(&name);

        // 只给了名字（最常见的调用方式，尤其是经 MCP 调进来的 agent）：来源目录
        // 得自己找。这里做**一次现扫**并按 (kind=skill, slug, agentId) 匹配，
        // 而不是只依赖 `assets` 全局缓存 —— 那个缓存只由渲染层发起过的扫描填充，
        // MCP 进程里它可能一直是空的。缺了这一步，`enable_asset {name, agentId}`
        // 会在「库里还没有这份资产」时直接报 `share.unknownAsset`，而盘上明明有
        // 一个真实目录可以收进库。
        let (from, from_agent) = if from.is_empty() {
            let hit = self.scan_for_skill(&slug_name, &prop(input, "agentId"));
            match hit {
                Some((dir, agent_id)) => (dir, agent_id),
                None => (String::new(), String::new()),
            }
        } else {
            (from, from_agent)
        };

        // 库内主副本：优先用已有的；没有就把来源目录当作主副本并记下来。
        let existing = self.library_asset_dir("skill", &slug_name);
        let store_keeper = existing.is_none();
        let keeper = match &existing {
            Some(p) => p.clone(),
            None => {
                if from.is_empty() {
                    return unsatisfied("share.unknownAsset");
                }
                store::native_str(&lib.skill_dir(&slug_name))
            }
        };

        let agent_id = prop(input, "agentId");
        let scope = {
            let s = prop(input, "scope");
            if s.is_empty() {
                "global".to_string()
            } else {
                s
            }
        };
        let target = json!({
            "agentId": agent_id,
            "scope": scope,
            "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
        });
        let Some(dir) = self.skill_dir_for(&target) else {
            return skipped("agent has no skill directory", &target);
        };
        let dst = join(&dir, &[&slug_name]);
        let dst_path = Path::new(&dst);

        // 已经是指向**这一份资产**的库内主副本的链接：什么都不做，而不是删了重建。
        //
        // 「指向库内」不算数：一条指着别的资产的链接、或库迁移后仍指着旧库根的链接，
        // 都必须走下面「删 + 重指」的同一事务。早先这里只判 `owns_target` 就返回
        // `already linked`，调用方（尤其是经 MCP 进来的 agent，它没有「重指」这个
        // 入口）拿到的是成功，而盘上那条永远指错的链接没人修。
        if linkfs::is_link(dst_path)
            && lib.owns_target(dst_path)
            && linkfs::read_target(dst_path)
                .is_some_and(|t| linkfs::same_target(&t, Path::new(&keeper)))
        {
            return json!({
                "ok": true,
                "written": [],
                "skipped": [{ "target": target, "reason": "already linked" }],
                "keeper": keeper,
            });
        }
        // 位置本身就是库内主副本（伪 agent `shared`）：没有可建的链接。
        if existing.as_deref() == Some(dst.as_str()) {
            return json!({
                "ok": true,
                "written": [],
                "skipped": [{ "target": target, "reason": "already in the library" }],
                "keeper": keeper,
            });
        }

        let mut ops: Vec<Op> = Vec::new();
        let mut written: Vec<Value> = Vec::new();
        // mkdir 的是 skills 目录本身（链接 dst 的父目录），不是它的上一级 ——
        // 上一级是 agent 的家目录，把它放进事务意味着整棵家目录树都要进快照
        //（包括全部 skills），既慢又占用撤销预算；而 `linkDir` 的执行分支自己
        // 会 `create_dir_all` 父链，这里只需保证 skills 这一层在快照里有位置。
        ops.push(Op::mkdir(&store::native_str(Path::new(&dir))));
        // 顺序：**先把内容搬进库，再删旧位置，最后建链接**。
        //
        // 不能先删：`copyDir` 的来源就是 agent 侧那个目录 —— 先删掉它，拷贝就没源
        // 可读了（`copy source missing`）。也不能先建链接：目标已存在时 `linkDir`
        // 直接拒绝（§11.3）。三条都在同一个事务里，任一条失败其余全部回滚。
        if store_keeper {
            // 来源目录整体成为库内主副本。用 `copyDir` 而不是 `write`：skill 是整棵
            // 目录（SKILL.md + 资源），`copyDir` 是事务里唯一认得「一棵树」的 op，
            // 它自己的撤销就是删掉拷贝出来的那份。
            ops.push(Op::copy_dir(&keeper, &from));
            written.push(json!(keeper));
        }
        // 旧位置：要么是真实目录（内容已在库里），要么是一条指向别处的链接。
        if dst_path.exists() || linkfs::is_link(dst_path) {
            if store_keeper && from.is_empty() {
                return unsatisfied("share.unknownAsset");
            }
            ops.push(Op::delete(&dst));
            written.push(json!(dst));
        }
        ops.push(Op::link_dir_with(&dst, &keeper, self.link_prefer()));
        written.push(json!(dst));

        let mut tx = self.store.transaction(
            &ops,
            &json!({
                "kind": "skill",
                "action": if store_keeper { "converge" } else { "enable" },
                "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                "assetName": name,
                "library": keeper,
                "fromAgent": from_agent,
            }),
        );
        if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return tx;
        }
        // 台账只在事务成功之后写：失败时不动它，是「台账不是真相」的前提之一。
        // 实际用掉的方式要从盘上读回（事务的日志只写进 `audit.jsonl`，应答里没有）：
        // 猜一个会让台账记错方式，doctor 随后就会报一条假的 `link.ledgerDrift`。
        let method = linkfs::link_kind(Path::new(&dst)).unwrap_or(LinkMethod::Symlink);
        if let Err(e) = self.record_link(&lib, "skill", &slug_name, &keeper, &dst, method) {
            tx["ok"] = json!(false);
            tx["error"] = json!(e);
        }
        self.invalidate();
        if let Value::Object(m) = &mut tx {
            m.insert("written".to_string(), json!(written));
            m.insert("skipped".to_string(), json!([]));
            m.insert("keeper".to_string(), json!(keeper));
            m.insert("storedInLibrary".to_string(), json!(store_keeper));
        }
        tx
    }

    /// 启用一个**规则或记忆**资产：`link` 或 `compose` 两种模式（M4，§6.2）。
    ///
    /// 与 skill 的区别不在于「写什么」，而在于**目标是文件不是目录**，而且它
    /// 很可能是用户手写的东西。所以：
    ///
    /// | 模式 | op | 目标文件 |
    /// |---|---|---|
    /// | `link` | 库内主副本 + `linkFile` | 原文件被换成指向库内的链接 |
    /// | `compose` | 一次 `write` | 原地多一个带标记的区块，外面一个字节不动 |
    ///
    /// 模式由 `mode` 显式给（`link` / `compose`），不给时按 `sharing::rule_plan`
    /// 的缺省（能 compose 就 compose）。判定与 op 规划全部在 `sharing` 里做，
    /// 本函数只负责：解析目标、校验、把 op 送进事务、成功之后记台账。
    ///
    /// `input`：`{ assetId?, name?, kind, agentId, scope, projectId?, mode? }`。
    fn enable_rule(&self, lib: &library::Library, kind: &str, input: &Value) -> Value {
        let agent_id = prop(input, "agentId");
        let scope = {
            let s = prop(input, "scope");
            if s.is_empty() {
                "global".to_string()
            } else {
                s
            }
        };
        let project_id = input.get("projectId").and_then(|v| v.as_str());

        // 目标文件：`assetId` 给定时以扫描结果的 `file` 为准（矩阵就是这么调的）；
        // 否则用 `file` 直接路径，再否则用 agent/scope 基准目录 + 表里声明的 rel。
        let mut target = None;
        if let Some(id) = non_empty(input, "assetId") {
            let Some(a) = self.index_get(&id) else {
                return unsatisfied("share.unknownAsset");
            };
            // 资产种类必须对得上：拿一个 skill 的 id 来「共享规则」是无意义的，
            // 而静默按它做点什么会写错文件。
            if prop(&a, "kind") != kind {
                return unsatisfied_with(
                    "share.unsupportedKind",
                    json!({ "want": kind, "got": prop(&a, "kind") }),
                );
            }
            target = Some(sharing::RuleTarget {
                base: String::new(),
                rel: None,
                file: a
                    .get("file")
                    .and_then(|v| v.as_str())
                    .map(|f| store::native_str(Path::new(f))),
            });
        } else if let Some(file) = non_empty(input, "file") {
            target = Some(sharing::RuleTarget {
                base: String::new(),
                rel: None,
                file: Some(store::native_str(Path::new(&file))),
            });
        } else if let Some(rel) = non_empty(input, "rel") {
            let Some(base) = sharing::scope_base(&self.user_home, &agent_id, &scope, project_id)
            else {
                return skipped("agent or project could not be resolved", input);
            };
            target = Some(sharing::RuleTarget {
                base,
                rel: Some(rel),
                file: None,
            });
        }
        let Some(target) = target else {
            return unsatisfied("rule.noTarget");
        };

        let mode = non_empty(input, "mode")
            .as_deref()
            .and_then(sharing::RuleMode::parse);
        // `mode` 给了却认不出来：报错而不是猜。猜错的后果是「把用户的文件换成
        // 链接」和「只插一个区块」之间选错，那是两种完全不同的写作方式。
        if mode.is_none() && non_empty(input, "mode").is_some() {
            return unsatisfied_with(
                "share.unsupportedKind",
                json!({ "field": "mode", "got": prop(input, "mode") }),
            );
        }

        let plan = sharing::rule_plan(lib, &target, mode);
        if let Some(code) = &plan.code {
            let mut out = unsatisfied_with(code, plan.detail.clone());
            out["file"] = json!(plan.file);
            out["mode"] = json!(plan.mode.as_str());
            return out;
        }
        // 零写入（已经共享过、或 compose 的区块内容本来就一致）：不产生事务，
        // 也就不会留下一条空的撤销快照。
        if plan.ops.is_empty() {
            return json!({
                "ok": true,
                "written": [],
                "skipped": [{ "target": input, "reason": "already shared" }],
                "file": plan.file,
                "mode": plan.mode.as_str(),
                "detail": plan.detail,
            });
        }

        let mut tx = self.store.transaction(
            &plan.ops,
            &json!({
                "kind": kind,
                "action": if plan.mode == sharing::RuleMode::Link { "link" } else { "compose" },
                "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                "assetName": non_empty(input, "name").unwrap_or_default(),
                "file": plan.file,
                "slug": plan.detail.get("slug").cloned().unwrap_or(Value::Null),
            }),
        );
        if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return tx;
        }
        // 台账：只有 link 模式有「链接」可记。方式从盘上读回，不猜。
        let mut ledger_err = None;
        if plan.mode == sharing::RuleMode::Link {
            let keeper = plan
                .detail
                .get("keeper")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let slug_name = plan
                .detail
                .get("slug")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let method = linkfs::link_kind(Path::new(&plan.file)).unwrap_or(LinkMethod::Symlink);
            if let Err(e) = self.record_link(lib, kind, &slug_name, &keeper, &plan.file, method) {
                ledger_err = Some(e);
            }
        }
        self.invalidate();
        if let Value::Object(m) = &mut tx {
            m.insert("written".to_string(), json!([plan.file]));
            m.insert("skipped".to_string(), json!([]));
            m.insert("file".to_string(), json!(plan.file));
            m.insert("mode".to_string(), json!(plan.mode.as_str()));
            m.insert("detail".to_string(), plan.detail);
            if let Some(e) = ledger_err {
                m.insert("ledgerWarning".to_string(), json!(e));
            }
        }
        tx
    }

    /// 停用一条规则 / 记忆的共享（M4）。两种模式的收尾方式不同：
    ///
    /// | 盘上是什么 | 动作 | 库内主副本 |
    /// |---|---|---|
    /// | 指向库内的链接 | 删链接（`Op::delete`，只删链接本身） | 不动 |
    /// | 含我们区块的文件 | 只删区块（`uncompose_plan` 算出整份新文本） | 不动 |
    ///
    /// `compose` 模式下**没有区块**就是零写入：说明它本来就没被共享过，不该把
    /// 用户的文件重写一遍（重写会改掉 mtime，diff 也会动）。
    fn disable_rule(&self, lib: &library::Library, kind: &str, input: &Value) -> Value {
        let Some(file) = non_empty(input, "file").or_else(|| {
            non_empty(input, "assetId").and_then(|id| {
                self.index_get(&id)
                    .and_then(|a| a.get("file").and_then(|v| v.as_str()).map(|s| s.to_string()))
            })
        }) else {
            return unsatisfied("rule.noTarget");
        };
        let file = store::native_str(Path::new(&file));
        let file_path = Path::new(&file);

        if linkfs::is_link(file_path) {
            // 与 skill 同一条安全闸：指向库外就拒绝，删它会动到用户自己的文件。
            // 硬链接的归属由台账作证（见 `ledger_backed_link` 的说明）。
            if !lib.owns_target(file_path) && !self.ledger_backed_link(lib, &file) {
                return unsatisfied_with("share.foreignLink", json!({ "path": file }));
            }
            let mut tx = self.store.transaction(
                &[Op::delete(&file)],
                &json!({
                    "kind": kind,
                    "action": "disable",
                    "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                    "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                    "file": file,
                }),
            );
            if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                return tx;
            }
            let mut ledger_err = None;
            if let Err(e) = lib.update_ledger(self.store.root_str(), |ledger| {
                ledger.remove(&file);
                Ok(())
            }) {
                ledger_err = Some(e);
            }
            self.invalidate();
            if let Value::Object(m) = &mut tx {
                m.insert("written".to_string(), json!([file]));
                m.insert("mode".to_string(), json!("link"));
                if let Some(e) = ledger_err {
                    m.insert("ledgerWarning".to_string(), json!(e));
                }
            }
            return tx;
        }

        // compose：slug 与 enable 时同一套算法（目标文件名归一）。
        let stem = Path::new(&file)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let slug_name = slug(&stem);
        let plan = sharing::uncompose_plan(&file, &slug_name);
        if plan.ops.is_empty() {
            return json!({
                "ok": true,
                "written": [],
                "skipped": [{ "target": input, "reason": "no managed block" }],
                "file": file,
                "mode": "compose",
            });
        }
        let mut tx = self.store.transaction(
            &plan.ops,
            &json!({
                "kind": kind,
                "action": "disable",
                "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                "file": file,
                "slug": slug_name,
            }),
        );
        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.invalidate();
            if let Value::Object(m) = &mut tx {
                m.insert("written".to_string(), json!([file]));
                m.insert("mode".to_string(), json!("compose"));
            }
        }
        tx
    }

    /// 启用一个 **MCP 服务器**（M6）：把库内规范形态物化到某个 agent / scope 的配置里。
    ///
    /// `input`：`{ name, agentId, scope?, projectId?, file? }`。`file` 可选 —— 矩阵的
    /// `mcp.cells[].file` 会带上解析好的目标配置；不给时按适配器声明顺序挑第一份
    /// 能「只动一条」的配置（[`sharing::mcp_config_candidates`]）。
    ///
    /// 编排全在 [`sharing::materialize_plan`] 里：它读库内记录、算出 open_ops、
    /// 决定 `DisableStyle`；本函数只负责把 op 送进事务并补上审计元数据。**不写台账**
    /// —— 台账记的是「链接」，MCP 物化没有目录链接这回事。
    fn enable_mcp(&self, lib: &library::Library, input: &Value) -> Value {
        self.operate_mcp(lib, input, true)
    }

    /// 停用一个 **MCP 服务器**（M6）：按 `DisableStyle` 删掉 agent 配置里的那一条，
    /// 或把它置为 `enabled:false`；库内记录与其它 agent 的配置一个字节不动。
    fn disable_mcp(&self, lib: &library::Library, input: &Value) -> Value {
        self.operate_mcp(lib, input, false)
    }

    /// 开 / 关一次 MCP 物化的共用主体：差别只有 `on` 与应答里的 reason / action。
    fn operate_mcp(&self, lib: &library::Library, input: &Value, on: bool) -> Value {
        // 名字是 MCP 的唯一索引（库内记录、配置里的键都由它决定）。没给 name 时
        // 从 assetId 反查 —— 与 `disable_asset` 的 skill 分支同一套兜底，避免
        // 调用方必须同时知道两种 id 形态。
        let name = match non_empty(input, "name") {
            Some(n) => n,
            None => {
                let resolved = non_empty(input, "assetId")
                    .and_then(|id| self.index_get(&id))
                    .map(|a| prop(&a, "name"))
                    .unwrap_or_default();
                if resolved.is_empty() {
                    return unsatisfied("mcp.noRecordInLibrary");
                }
                resolved
            }
        };
        let scope = non_empty(input, "scope").unwrap_or_else(|| "global".to_string());
        let project_dir = self.project_dir_of(input);
        let mut req = json!({
            "name": name,
            "agentId": prop(input, "agentId"),
            "scope": scope,
            "on": on,
        });
        if let Some(file) = non_empty(input, "file") {
            req["file"] = json!(store::native_str(Path::new(&file)));
        }
        // 显式记录只用于「刚导入、还没落库」的一次性物化（与 materialize_mcp 同口径）。
        if let Some(record) = input.get("record").filter(|v| v.is_object()) {
            req["record"] = record.clone();
        }
        // `config`（MCP 工具表的承诺，§9.1）：库里还没有这条记录时，先把它写成
        // 库内规范形态，再继续物化 —— 这正是「一份资产写一次，所有 agent 看到同一
        // 份」的入口。只收规范字段（command/args/env/url/headers/cwd/transport），
        // agent 私有调优不跟着走（与 `canonical_record` 的口径一致）。
        //
        // 为什么不放进 materialize_plan：落库是一次**写**（要走事务、进审计），
        // materialize 只管「算 op」；把写混进规划函数会破坏它「纯规划」的契约。
        if on
            && input.get("record").is_none()
            && !lib
                .mcp_records()
                .iter()
                .any(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
        {
            if let Some(config) = input.get("config").filter(|v| v.is_object()) {
                let mut record = json!({
                    "schema": sharing::MCP_SCHEMA,
                    "name": name,
                    "transport": config
                        .get("url")
                        .and_then(Value::as_str)
                        .map(|_| "http")
                        .unwrap_or("stdio"),
                });
                if let Value::Object(m) = &mut record {
                    for key in ["command", "args", "env", "cwd", "url", "headers"] {
                        if let Some(v) = config.get(key) {
                            m.insert(key.to_string(), v.clone());
                        }
                    }
                }
                let path = store::native_str(&lib.mcp_record_path(&slug(&name)));
                let body = format!(
                    "{}\n",
                    serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".to_string())
                );
                let tx = self.store.transaction(
                    &[Op::write(&path, &body)],
                    &json!({
                        "kind": "mcp",
                        "action": "library-record",
                        "assetName": name,
                    }),
                );
                if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return tx;
                }
            }
        }
        let plan = match sharing::materialize_plan(
            lib,
            &self.user_home,
            project_dir.as_deref(),
            &req,
        ) {
            Ok(Ok(Some(plan))) => plan,
            Ok(Ok(None)) => {
                // 已经是一模一样的状态：零写入是一次成功，不是失败。
                return json!({
                    "ok": true,
                    "written": [],
                    "skipped": [{
                        "target": input.clone(),
                        "reason": if on { "already open" } else { "already closed" },
                    }],
                });
            }
            Ok(Err(code)) => return json!({ "ok": false, "code": code }),
            Err(code) => return json!({ "ok": false, "code": code }),
        };
        let mut tx = self.store.transaction(
            &plan.ops,
            &json!({
                "kind": "mcp",
                "action": if on { "enable" } else { "disable" },
                "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                "assetName": name,
                "server": plan.server,
                "file": plan.file,
            }),
        );
        if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return tx;
        }
        self.invalidate();
        if let Value::Object(m) = &mut tx {
            m.insert("written".to_string(), json!([plan.file]));
            m.insert("skipped".to_string(), json!([]));
            m.insert("file".to_string(), json!(plan.file));
            m.insert("server".to_string(), json!(plan.server));
            m.insert("style".to_string(), json!(plan.style.as_str()));
            m.insert("on".to_string(), json!(on));
        }
        tx
    }

    /// 项目作用域的基准目录（`projects.json` 里登记的 `dir`）。解析不出来返回 `None`。
    fn project_dir_of(&self, input: &Value) -> Option<String> {
        registry::get(input.get("projectId").and_then(Value::as_str)?)
            .and_then(|p| p.get("dir").and_then(Value::as_str).map(|s| s.to_string()))
    }

    /// 往台账里登记（或更新）一条链接。台账读不出来时**不覆盖**，原样报错。
    fn record_link(
        &self,
        lib: &library::Library,
        kind: &str,
        slug_name: &str,
        src: &str,
        dst: &str,
        method: LinkMethod,
    ) -> Result<(), String> {
        // 读-改-写必须在同一把跨进程锁里完成，否则并发的两个进程会互相盖掉
        // 对方刚登记的条目（`library::update_ledger` 的文档讲了那个时序）。
        lib.update_ledger(self.store.root_str(), |ledger| {
            lib.record(
                ledger,
                library::LinkEntry::new(kind, slug_name, src, dst, method),
            );
            Ok(())
        })
    }

    /// 停用某个 agent 上的库内链接：**只删链接，库内主副本一个字节都不动**。
    ///
    /// 三道闸缺一不可（§4.2）：不是链接就拒绝（`share.notLinked`）；指向库外就拒绝
    /// （`share.foreignLink`，删它会动到用户自己的目录）；库内主副本永不删除 ——
    /// 本函数里没有任何一条 op 指向库内路径。
    ///
    /// ## 硬链接的归属凭什么认定
    ///
    /// `owns_target` 靠 `read_target` 读指向，而**硬链接没有可读的目标**（它不是
    /// 重解析点，只是同一份内容的另一个名字），于是指向库内主副本的硬链接会被
    /// 误判成 `share.foreignLink` —— 用户没法停用它。归属在台账里写得很清楚
    /// （`owner: "library"`、`src` 在库内），所以第二条闸对硬链接改由台账作证：
    /// 删掉那一个目录项只少一个名字，主副本的字节一个都不会动，这是安全的。
    pub fn ledger_backed_link(&self, lib: &library::Library, dst: &str) -> bool {
        match lib.load_ledger() {
            Ok(l) => l.find(dst).is_some_and(|e| {
                e.owner == "library" && library::is_under(Path::new(&e.src), lib.root())
            }),
            Err(_) => false,
        }
    }

    pub fn disable_asset(&self, input: &Value) -> Value {
        let lib = self.library();
        let kind = non_empty(input, "kind").unwrap_or_else(|| "skill".to_string());
        // 规则 / 记忆走另一套：目标可能是链接（撤链接），也可能是一个 compose
        // 出来的区块（只把我们放进去的那一截拿走）。两者都不是目录，所以不能
        // 掉进下面按 slug 拼 skill 目录的那条路 —— 那条路会算出一个不存在的
        // 路径，然后把一次**成功的**停用报成 `share.unknownAsset`。
        if kind == "rule" || kind == "memory" {
            return self.disable_rule(&lib, &kind, input);
        }
        // MCP（M6，§17.9 允许扩分支）：停用 = 在 agent 的配置里删掉那一条 / 或
        // 置 `enabled:false`（由 `DisableStyle` 决定），库内记录与其它 agent 不动。
        if kind == "mcp" {
            return self.disable_mcp(&lib, input);
        }
        let mut name = non_empty(input, "name").unwrap_or_default();
        if name.is_empty() {
            if let Some(id) = non_empty(input, "assetId") {
                name = self
                    .index_get(&id)
                    .map(|a| prop(&a, "name"))
                    .unwrap_or_default();
            }
        }
        if name.is_empty() {
            return unsatisfied("share.unknownAsset");
        }
        let slug_name = slug(&name);
        let agent_id = prop(input, "agentId");
        let scope = {
            let s = prop(input, "scope");
            if s.is_empty() {
                "global".to_string()
            } else {
                s
            }
        };
        let Some(dir) = self.skill_dir_for(&json!({
            "agentId": agent_id,
            "scope": scope,
            "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
        })) else {
            return skipped("agent has no skill directory", input);
        };
        let dst = join(&dir, &[&slug_name]);
        let dst_path = Path::new(&dst);

        if !linkfs::is_link(dst_path) {
            return unsatisfied_with("share.notLinked", json!({ "path": dst }));
        }
        if !lib.owns_target(dst_path) && !self.ledger_backed_link(&lib, &dst) {
            return unsatisfied_with("share.foreignLink", json!({ "path": dst }));
        }

        let mut tx = self.store.transaction(
            &[Op::delete(&dst)],
            &json!({
                "kind": kind,
                "action": "disable",
                "agentId": input.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": input.get("projectId").cloned().unwrap_or(Value::Null),
                "assetName": name,
            }),
        );
        if !tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return tx;
        }
        let mut ledger_err = None;
        if let Err(e) = lib.update_ledger(self.store.root_str(), |ledger| {
            ledger.remove(&dst);
            Ok(())
        }) {
            ledger_err = Some(e);
        }
        self.invalidate();
        if let Value::Object(m) = &mut tx {
            m.insert("written".to_string(), json!([dst]));
            m.insert("skipped".to_string(), json!([]));
            if let Some(e) = ledger_err {
                m.insert("ledgerWarning".to_string(), json!(e));
            }
        }
        tx
    }

    /// 从库里删除一个资产：先把它移进 `<store>/.trash/<kind>/<slug>-<ts>/`，
    /// 再删掉所有指向它的链接，最后清台账。
    ///
    /// 顺序的意义：先让主副本离开原位，再拆链接 —— 否则中途失败会留下一批悬空
    /// 链接，而用户看不到「内容还在回收站里」这个事实。每一步都进 `audit.jsonl`，
    /// 因为这是一次删除，而回收站会在一段时间后自己清空：审计线是它的唯一书面记录。
    pub fn remove_from_library(&self, kind: &str, name: &str) -> Value {
        let lib = self.library();
        let slug_name = slug(name);
        let Some(src) = self.library_asset_dir(kind, &slug_name) else {
            return unsatisfied_with("share.unknownAsset", json!({ "kind": kind, "name": name }));
        };
        let mut ledger = match lib.load_ledger() {
            Ok(l) => l,
            Err(e) => {
                // 台账读不出来就不能确认「哪些链接指向它」，宁可什么都不做也不留悬空链接。
                return json!({ "ok": false, "code": "link.ledgerDrift", "error": e });
            }
        };
        // 先读链接清单，**再**搬主副本：台账按 `src` 路径索引，而 `normalize` 先做
        // canonicalize —— 主副本一旦移进 `.trash`，原路径上的 `links_to` 就一条也
        // 找不到了（它变成一条读不出来的路径），链接清单会静默为空。
        let links: Vec<String> = ledger
            .links_to(&src)
            .iter()
            .map(|e| e.dst.clone())
            .collect();
        let trash = self
            .store
            .root()
            .join(".trash")
            .join(lib_dir_name(kind))
            .join(format!("{}-{}", slug_name, trash_stamp()));
        if let Some(parent) = trash.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let trash_str = store::native_str(&trash);
        match std::fs::rename(Path::new(&src), &trash) {
            Ok(()) => {}
            Err(_) => {
                // 跨卷（库在某些机器上会在另一个盘上）：先拷过去再删原处。
                //
                // **拷贝没成功就绝不删原处**：`copy_dir` 没有返回值，部分失败时
                // 目标里是一份不完整的副本，此时 `rmrf(源)` 等于把用户的主副本
                // 删掉而回收站里只有一半 —— 那是比「删除失败」严重得多的结果。
                // 所以拷完必须核对两边，对不上就放弃整次删除，磁盘保持原样。
                store::copy_dir(Path::new(&src), &trash);
                if !same_tree(Path::new(&src), &trash) {
                    let _ = std::fs::remove_dir_all(&trash);
                    return json!({
                        "ok": false,
                        "code": "library.trashFailed",
                        "detail": { "src": src, "trash": trash_str }
                    });
                }
                store::rmrf(Path::new(&src));
            }
        }
        // 两边内容一致的判据：用 `sharing::digest_dir` —— 它就是收敛与 doctor 用来
        // 回答「这两棵树是不是同一份内容」的那一个函数，逐文件流式 SHA-256，
        // 相对路径与种类都参与摘要。`unreadable > 0` 一律算不一致（读不出来就不能
        // 说「一样」，宁可放弃这次删除）。
        fn same_tree(a: &Path, b: &Path) -> bool {
            let (da, db) = (sharing::digest_dir(a), sharing::digest_dir(b));
            da.unreadable == 0 && db.unreadable == 0 && da.digest == db.digest
        }
        settings::audit(
            self.store.root_str(),
            &json!({
                "action": "library.trash",
                "kind": kind,
                "name": slug_name,
                "from": src,
                "to": trash_str,
            }),
        );

        let mut removed: Vec<Value> = Vec::new();
        let mut failed: Vec<Value> = Vec::new();
        for dst in &links {
            let p = Path::new(dst);
            let result = if !linkfs::is_link(p) {
                // 已经换成真实目录/文件：这不是我们的链接，绝不删。
                Err("this path is no longer a link".to_string())
            } else if !lib.owns_target(p) && !self.ledger_backed_link(&lib, dst) {
                // 硬链接没有可读目标，`owns_target` 认不出它；台账里那条
                // owner=library 且 src 在库内的记录就是归属证明（见
                // `ledger_backed_link`）。台账没记的照旧拒绝。
                Err("this link points outside the library".to_string())
            } else {
                linkfs::unlink(p).map_err(|e| e.to_string())
            };
            match result {
                Ok(()) => {
                    ledger.remove(dst);
                    settings::audit(
                        self.store.root_str(),
                        &json!({ "action": "library.unlink", "kind": kind, "name": slug_name, "path": dst }),
                    );
                    removed.push(json!(dst));
                }
                Err(reason) => failed.push(json!({ "path": dst, "reason": reason })),
            }
        }
        // 收尾的台账更新走同一把跨进程锁，并且**重新读一遍**：
        // 上面那段读盘只为拿链接清单，期间别的进程可能已经登记了新条目；
        // 拿旧快照整份写回会把它盖掉。
        let removed_paths: Vec<String> = removed
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        let ledger_err = lib
            .update_ledger(self.store.root_str(), |fresh| {
                for dst in &removed_paths {
                    fresh.remove(dst);
                }
                Ok(())
            })
            .err();
        self.invalidate();
        json!({
            "ok": true,
            "trashed": trash_str,
            "removed": removed,
            "failed": failed,
            "ledgerWarning": ledger_err,
        })
    }
}

/// `.trash/<kind>/` 里的目录名：与库内 `dir_for` 同一套命名。
fn lib_dir_name(kind: &str) -> &'static str {
    match kind {
        "rule" => "rules",
        "memory" => "memory",
        "mcp" => "mcp",
        _ => "skills",
    }
}

/// 回收站条目名里的时间戳（UTC，`YYYYMMDDTHHMMSS`）。排序即时间序。
fn trash_stamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let iso = super::model::iso_from_millis(ms);
    iso.chars()
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .take(15)
        .collect()
}

/// `input[key]` 的非空字符串。
fn non_empty(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 「这一步做不了」的统一形状：一个可读的 code，不给散文让渲染层去猜。
fn unsatisfied(code: &str) -> Value {
    unsatisfied_with(code, Value::Null)
}

fn unsatisfied_with(code: &str, detail: Value) -> Value {
    json!({ "ok": false, "code": code, "detail": detail })
}

/// 「没有可写的东西」：沿用 `distribute` 的 `skipped[]` 形状。
fn skipped(reason: &str, target: &Value) -> Value {
    json!({
        "ok": false,
        "written": [],
        "skipped": [{ "target": target, "reason": reason }],
    })
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

        // workbuddy 声明了 mcp.json、mcp-approvals.json、mcp-tool-list.json ——
        // 只有第一个才是服务器列表。
        let t = k.mcp_target_for("workbuddy", "global", None).unwrap();
        assert!(t.ends_with("mcp.json"), "{}", t);
        assert!(!t.contains("approvals") && !t.contains("tool-list"));

        // codex 的第一个 MCP 条目是一个 .toml 文件，所以它胜过 mcp.json。
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

        // `validate_skill` 会拿*原始*名字去比对
        // `^[a-z0-9][a-z0-9-]{0,63}$`，所以 "PDF Tools" 在 `slug`
        // 根本没运行之前就被拦下了 —— 一个名字只有本来就已经是 slug 才能存活。
        let data = json!({ "name": "pdf-tools", "description": "Extract text from PDFs" });
        let r = k.save_skill(&json!({
            "data": data,
            "body": "Use it well.\n",
            "targets": [{ "agentId": "claude-code", "scope": "global" }],
        }));
        assert_eq!(r["ok"], json!(true), "{}", r);
        assert_eq!(r["issues"], json!([]));

        // `skill_dir_for` 会针对用户主目录解析 `~/.claude/skills`。
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

        // 要求有非空的正文：`validate_skill` 会先拦下空的，而被拦下的返回
        // 没有 `error` 键。
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

        // 没有 name：validate_skill 把它当成一个阻断性错误。
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
        // claude-code 的 home 是 `~/.claude`，所以它的全局 MCP 配置是
        // `<home>/.claude/.mcp.json` —— 不是 `<home>/.mcp.json`。
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
        // 一个 skill 落在 `skill_dir_for({scope:'global'})` = `~/.claude/skills`。
        assert!(home
            .join(".claude")
            .join("skills")
            .join("pdf")
            .join("SKILL.md")
            .is_file());

        // 第二次导入什么也不做：文件夹已经存在。
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

    /// 下面的一切都通过进程全局的扫描缓存来解析 id，所以它必须持有那把缓存锁
    /// （见 `assets::lock_for_test`）。
    #[test]
    fn cached_index_operations() {
        let _guard = assets::lock_for_test();
        let root = temp("cached");
        let home = temp("cached-home");
        let k = keeper_at(&root, &home);

        // --- delete_mcp 只删掉一个服务器 -------------------------------
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

        // --- delete_asset 把 mcp 分发给 delete_mcp -----------------------
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

        // --- distribute 跳过资产自身所在的位置 -------------------------
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

        // --- save_markdown 拒绝改名 -------------------------------------
        // 改名守卫只有在 `validate_skill` 通过之后才运行，而那需要一个
        // description；没有它时回来的会是缺失 description 的错误（以及
        // dirName 警告）。
        let r = k.save_markdown("skill:pdf", "---\nname: other\ndescription: d\n---\nbody\n");
        assert_eq!(r["blocked"], json!(true), "{}", r);
        assert_eq!(
            r["issues"][0]["message"],
            json!("renaming a skill requires a move (directory is \"pdf\")")
        );

        // --- export_bundle 写出 manifest 并拷贝文件夹 --------------------
        // 先重新播种索引：上面的 `distribute` 和 `save_markdown` 都以
        // `invalidate()` 结尾，它会清空进程全局缓存。在冷缓存上 `index_get`
        // 会退回到对 `self.user_home` 的一次完整 `scan_payload` —— 是真实的
        // home，不是这个测试的临时目录 —— 所以 `skill:pdf` 在这里能否解析，
        // 取决于这台机器恰好装了什么。没有这一行时，测试单独跑会通过，而在
        // `cargo test` 下大约 3 次里有 2 次失败（持有缓存锁的其它测试会改变
        // 交错顺序）。这次重新播种让测试变得确定，而不是取决于负载。
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

        // --- diff 报告发生变化的行 --------------------------------------
        // 再重新播种一次：`export_bundle` 本身不碰缓存，但测试套件是并行运行的，
        // 而任何完成了写入的其它测试都会调用 `invalidate()`。缓存锁串行化的是
        // 写入而不是读取，所以在不重新播种的情况下断言 `diff` 会让这个测试
        // 依赖调度顺序。
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
