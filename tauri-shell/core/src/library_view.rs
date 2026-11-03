//! 共享页（界面契约）的只读数据视图。
//!
//! `list_library` 是共享矩阵的唯一数据源：一次请求拿回库根、链接能力、矩阵列
//! （agent）与 skills / mcp / rules / memory 四类行。渲染层**不做**路径解析与
//! 状态判定 —— 那些规则（哪些路径由哪个 agent 声明、一个位置算「已启用」还是
//! 「副本」）全部在 Rust 侧按适配器表算好，界面只负责画。
//!
//! 载荷形状与单元格状态词表（`on` / `off` / `copy` / `foreign` / `drift` /
//! `unsupported` / `error`）见 `docs/DESIGN.md` §8.12 界面契约；这份词表是冻结的，
//! 前端只认这些值。
//!
//! ## 只读契约
//!
//! 本模块不写盘、不建目录、不改台账。唯一的写入是
//! `Library::capability` 在 `<store>/probe` 下的能力探测缓存 —— 那是既有行为
//! （不真的建一次链接就回答不了「这台机器能不能建」），不属于本模块的写入。
//!
//! ## 为什么用 uncached 扫描
//!
//! 资产列表来自一次 `scan_payload_uncached`：`scan_payload` 会把结果发布成进程
//! 全局读缓存，而共享页只是消费自己这一份。一次「打开共享页」覆盖掉其它视图正在
//! 用的快照，会让那些视图手上的资产 id 与列表对不上（`keeper` 的单测踩过这种
//! 交错顺序）。外部行的 `assetId` 本来就随这次扫描一起返回，不需要经过读缓存。

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::adapters::{self, resolve_home};
use crate::compose;
use crate::keeper::Keeper;
use crate::library::Library;
use crate::linkfs::{self, LinkMethod};
use crate::model;
use crate::scan;
use crate::sharing;
use crate::store;

/// 共享矩阵的唯一数据源。
///
/// 一次调用把四类行、矩阵列与链接能力全部算好；矩阵列顺序 = 适配器表顺序，
/// 行顺序是确定的（库内按路径 / 名字排序），同一份磁盘状态每次打开看到的顺序
/// 必须一样。
pub fn list_library(keeper: &Keeper) -> Value {
    let lib = keeper.library();
    // 探测结果会缓存在 `<store>/linkcap.txt`，见模块注释。
    let cap = lib.capability(keeper.store.root_str());
    let agents = resolve_agents(keeper);
    let assets = scan_assets(keeper);
    let drifts = sharing::plan_drift(&lib, &keeper.user_home, None, "global");

    let mut skills = library_skill_rows(&lib, &agents);
    skills.extend(external_skill_rows(&lib, &agents, &assets));
    skills.sort_by(|a, b| {
        let ka = a.get("slug").and_then(Value::as_str).unwrap_or_default();
        let kb = b.get("slug").and_then(Value::as_str).unwrap_or_default();
        ka.cmp(kb)
    });

    json!({
        "ok": true,
        "root": store::native_str(lib.root()),
        "cap": cap.to_json(),
        "agents": agents.iter().map(AgentView::to_json).collect::<Vec<Value>>(),
        "skills": skills,
        "mcp": mcp_rows(&lib, &agents, &drifts),
        "rules": md_rows(&lib, &agents, "rule", &keeper.user_home),
        "memory": md_rows(&lib, &agents, "memory", &keeper.user_home),
    })
}

/* ------------------------------------------------------------------ */
/* 扫描与列（agent）                                                     */
/* ------------------------------------------------------------------ */

/// 一次只读扫描，结果**不**发布到全局读缓存（原因见模块注释）。
fn scan_assets(keeper: &Keeper) -> Vec<Value> {
    let projects = scan::read_projects(keeper.store.root_str());
    let (payload, _timings) = scan::scan_payload_uncached(&keeper.user_home, &projects);
    payload
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// 矩阵的一列。`id` 是唯一键；路径只在 Rust 侧用来判定单元格。
struct AgentView {
    id: String,
    label: String,
    accent: String,
    installed: bool,
    skills_dir: Option<String>,
    rule_dirs: Vec<String>,
    memory_dirs: Vec<String>,
    mcp_files: Vec<String>,
    mcp_style: &'static str,
}

impl AgentView {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "accent": self.accent,
            "installed": self.installed,
            "skillsDir": self.skills_dir,
            "ruleDirs": self.rule_dirs,
            "memoryDirs": self.memory_dirs,
            "mcpFiles": self.mcp_files,
            "mcpStyle": self.mcp_style,
        })
    }
}

/// 解析出全部**有 home** 的 agent。`warp` 的 home 能解析，只是没有 skills 目录，
/// 所以它照样是一列（单元格给 `unsupported`），而不是悄悄消失 —— 列数靠
/// 「解析得出 home」决定，不写死 17。
fn resolve_agents(keeper: &Keeper) -> Vec<AgentView> {
    let mut out = Vec::with_capacity(adapters::AGENTS.len());
    for agent in adapters::AGENTS {
        let Some(home) = resolve_home(agent.home, &keeper.user_home) else {
            continue;
        };
        let home = store::native_str(Path::new(&home));
        out.push(AgentView {
            id: agent.id.to_string(),
            label: agent.label.to_string(),
            accent: agent.accent.to_string(),
            installed: Path::new(&home).is_dir(),
            skills_dir: keeper.skill_dir_for(&json!({
                "agentId": agent.id,
                "scope": "global",
            })),
            rule_dirs: declared_dirs(keeper, agent.id, "rule"),
            memory_dirs: declared_dirs(keeper, agent.id, "memory"),
            mcp_files: sharing::mcp_config_candidates(agent.id, "global", None, &keeper.user_home),
            mcp_style: adapters::mcp_disable(agent.id).as_str(),
        });
    }
    out
}

/// 某类（rule / memory）全局声明所在的基准目录，去重后给界面当提示。
///
/// 契约（见 `docs/DESIGN.md` §8.12 界面契约）的 `ruleDirs` / `memoryDirs` 是「基准目录」而不是逐文件路径：
/// 全局 scope 下同类声明都挂在同一个基准目录（agent 的 home）上，去重后只有
/// 一项。没有声明（如 `warp` 的 rules）就是空数组。
fn declared_dirs(keeper: &Keeper, agent_id: &str, kind: &str) -> Vec<String> {
    let Some(patterns) = sharing::kind_patterns(agent_id, kind, "global") else {
        return Vec::new();
    };
    if patterns.is_empty() {
        return Vec::new();
    }
    let Some(base) = sharing::scope_base(&keeper.user_home, agent_id, "global", None) else {
        return Vec::new();
    };
    vec![store::native_str(Path::new(&base))]
}

/* ------------------------------------------------------------------ */
/* skills（技能）                                                        */
/* ------------------------------------------------------------------ */

/// 库内主副本行：`<root>/skills/*` 每个目录一行。
fn library_skill_rows(lib: &Library, agents: &[AgentView]) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(lib.skill_dir("")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    let mut rows = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for dir in dirs {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let slug = model::slug(&name);
        if seen.contains(&slug) {
            continue;
        }
        seen.push(slug.clone());
        rows.push(json!({
            "name": name,
            "slug": slug,
            "label": name,
            "dir": store::native_str(&dir),
            "size": dir_size(&dir),
            "cells": skill_cells(lib, agents, &slug),
        }));
    }
    rows
}

/// 外部行：扫描到了某 slug 的真实副本，但库里还没有它。
///
/// 外部行是「可以导入到库」的候选项，所以只收**真实目录**：链接是「已启用」的
/// 形态，不是需要导入的副本；库根下的目录是主副本本身（`~/.agents/skills/*`），
/// 也不能被误当成外部副本。
fn external_skill_rows(lib: &Library, agents: &[AgentView], assets: &[Value]) -> Vec<Value> {
    struct ExternalGroup {
        slug: String,
        entries: Vec<Value>,
        bytes: u64,
        /// 全局 scope 的副本：矩阵列是全局视图，只有它们能填单元格。
        paths: Vec<(String, String)>,
    }

    let mut groups: Vec<ExternalGroup> = Vec::new();
    for asset in assets {
        if asset.get("kind").and_then(Value::as_str) != Some("skill") {
            continue;
        }
        let (Some(dir), Some(name)) = (
            asset.get("dir").and_then(Value::as_str),
            asset.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        let dir = store::native_str(Path::new(dir));
        let slug = model::slug(name);
        // 库内已有同 slug：这是主副本那一行的事，不另起外部行。
        if lib.skill_dir(&slug).is_dir() {
            continue;
        }
        // 库根下的资产就是主副本本身（伪 agent `shared`）。
        if lib.owns_target(Path::new(&dir)) {
            continue;
        }
        // 只收真实目录；链接不是「待导入的副本」。
        let linked = asset
            .get("meta")
            .and_then(|m| m.get("link"))
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if linked || !Path::new(&dir).is_dir() {
            continue;
        }
        // 单文件 skill 的资产 `dir` 是它所在的 skills 根目录，拿它当「可导入的
        // 副本」会把整个 skills 目录导入库。真正的目录型 skill 必有一份 SKILL.md
        // （扫描认的是它），这是两者之间最便宜也最可靠的区分。
        if !Path::new(&dir).join("SKILL.md").is_file() {
            continue;
        }
        let agent_id = asset
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let scope = asset
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let entry = json!({
            "assetId": asset.get("id").cloned().unwrap_or(Value::Null),
            "agentId": agent_id,
            "scope": scope,
            "path": dir,
        });
        let size = asset.get("size").and_then(Value::as_u64).unwrap_or(0);

        let slot = match groups.iter_mut().find(|g| g.slug == slug) {
            Some(g) => g,
            None => {
                groups.push(ExternalGroup {
                    slug: slug.clone(),
                    entries: Vec::new(),
                    bytes: 0,
                    paths: Vec::new(),
                });
                let last = groups.len() - 1;
                &mut groups[last]
            }
        };
        let dup = slot.entries.iter().any(|e| {
            e.get("agentId").and_then(Value::as_str) == Some(agent_id.as_str())
                && e.get("path").and_then(Value::as_str) == Some(dir.as_str())
        });
        if !dup {
            slot.entries.push(entry);
            slot.bytes += size;
        }
        if scope == "global" && !slot.paths.iter().any(|(a, _)| a == &agent_id) {
            slot.paths.push((agent_id, dir));
        }
    }

    groups.sort_by(|a, b| a.slug.cmp(&b.slug));
    let mut rows = Vec::new();
    for group in groups {
        let ExternalGroup {
            slug,
            entries,
            bytes,
            paths,
        } = group;
        let mut cells = match skill_cells(lib, agents, &slug) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        // 扫描结果比「按 slug 拼出来的路径」权威：目录名与 frontmatter 名字
        // 不一定一字不差。
        for (agent_id, path) in paths {
            cells.insert(agent_id, json!({ "state": "copy", "path": path }));
        }
        rows.push(json!({
            "name": slug,
            "slug": slug,
            "label": slug,
            "dir": Value::Null,
            "size": bytes,
            "external": entries,
            "cells": Value::Object(cells),
        }));
    }
    rows
}

/// 每个 agent 一格。没有 skills 目录的（`warp`）给 `unsupported`，不消失。
fn skill_cells(lib: &Library, agents: &[AgentView], slug: &str) -> Value {
    let mut cells = Map::new();
    for agent in agents {
        let cell = match &agent.skills_dir {
            Some(dir) => skill_cell(lib, &join_native(dir, slug)),
            None => unsupported("noSkillsDir"),
        };
        cells.insert(agent.id.clone(), cell);
    }
    Value::Object(cells)
}

/// 一个 skill 位置的判定：链接进库内 = `on`，真实目录 = `copy`，链接指向库外 =
/// `foreign`，什么都没有 = `off`，读盘失败 = `error`。
fn skill_cell(lib: &Library, target: &str) -> Value {
    let path = Path::new(target);
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({ "state": "off" });
        }
        Err(e) => return json!({ "state": "error", "reason": e.to_string() }),
    };
    if let Some(method) = linkfs::entry_link_kind(path) {
        if !lib.owns_target(path) {
            return json!({ "state": "foreign", "path": target, "method": method.as_str() });
        }
        // 链接指向库内但主副本的关键内容没了（skill 没有 SKILL.md）：
        // 不能显示 `on` —— 那会让用户以为共享层健康，而 agent 读内容确实会
        // 失败（§13 终验 #11 的字面场景）。按读盘失败的既有状态报 `error`。
        if master_unreadable(target) {
            return json!({
                "state": "error",
                "path": target,
                "method": method.as_str(),
                "reason": "masterUnreadable",
            });
        }
        return json!({ "state": "on", "path": target, "method": method.as_str() });
    }
    if meta.is_dir() {
        json!({ "state": "copy", "path": target })
    } else {
        // 位置被一个同名文件占着：不是 skill，也不是空位。
        json!({ "state": "foreign", "path": target })
    }
}

/// 主副本的关键内容还在吗（与 `doctor::master_unreadable` 同一口径）：
/// skill 的判定是「目录里有 SKILL.md」，rule / memory 的主副本是单文件，
/// 目录级判定已经足够。
fn master_unreadable(target: &str) -> bool {
    !Path::new(target).join("SKILL.md").is_file()
}

/// 目录总字节数。**不跟随链接**：库内副本里若有一条指回上级的链接，跟随会无限
/// 递归；链接本身的大小也不是用户想看的数字。
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if linkfs::entry_link_kind(&path).is_some() {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_size(&path);
        } else if file_type.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/* ------------------------------------------------------------------ */
/* MCP（配置）                                                          */
/* ------------------------------------------------------------------ */

/// 库内每条规范形态一行；单元格看各 agent 的配置文件里有没有这条服务器。
fn mcp_rows(lib: &Library, agents: &[AgentView], drifts: &[sharing::McpDrift]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for record in lib.mcp_records() {
        let Some(name) = record
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let slug = model::slug(&name);
        if seen.contains(&slug) {
            continue;
        }
        seen.push(slug.clone());
        let all_of = record.get("allOf").and_then(Value::as_object);
        let mut cells = Map::new();
        for agent in agents {
            cells.insert(agent.id.clone(), mcp_cell(agent, &name, all_of, drifts));
        }
        rows.push(json!({
            "name": name,
            "slug": slug,
            "record": store::native_str(&lib.mcp_record_path(&slug)),
            "cells": Value::Object(cells),
        }));
    }
    rows
}

/// 一条 MCP 服务器在某个 agent 上的状态。
///
/// 契约补充（见 `docs/DESIGN.md` §8.12 界面契约）：**每个** cell 都带 `file`，
/// 固定取该 agent 的第一个可物化配置候选（与 `agents[].mcpFiles[0]` 同源），
/// 前端 enable / disable 会把它原样传回。
fn mcp_cell(
    agent: &AgentView,
    name: &str,
    all_of: Option<&Map<String, Value>>,
    drifts: &[sharing::McpDrift],
) -> Value {
    if agent.mcp_files.is_empty() {
        return unsupported("noMcpConfig");
    }
    let file = agent.mcp_files[0].clone();
    // 与 plan_drift 同口径：按候选顺序找第一份真的读到了这条服务器的配置。
    let present = agent
        .mcp_files
        .iter()
        .any(|f| sharing::read_entry(f, &agent.id, name).is_some());
    if !present {
        return json!({
            "state": "off",
            "file": file,
            "configKey": name,
            "drift": Value::Null,
        });
    }
    // `allOf[agentId] == false` = 用户标记为「独立」，漂移不参与判定。
    let independent = all_of
        .and_then(|m| m.get(&agent.id))
        .map(|v| v == &Value::Bool(false))
        .unwrap_or(false);
    if !independent {
        if let Some(drift) = drifts
            .iter()
            .find(|d| d.name.as_str() == name && d.agent_id == agent.id)
        {
            return json!({
                "state": "drift",
                "file": file,
                "configKey": name,
                "drift": {
                    "field": drift.field,
                    "libraryValue": drift.library_value,
                    "agentValue": drift.agent_value,
                },
            });
        }
    }
    json!({
        "state": "on",
        "file": file,
        "configKey": name,
        "drift": Value::Null,
    })
}

/* ------------------------------------------------------------------ */
/* rules / memory（规则与记忆）                                          */
/* ------------------------------------------------------------------ */

/// 一行规则 / 记忆。行标识是**目标文件名主干**（slug），与
/// `sharing::rule_plan` 推库内主副本的算法一致（`CLAUDE.md` → `rules/claude.md`）。
/// 行标识若用「库内文件名」，点 claude 的单元格会去写另一个 slug 的库内文件，
/// 单元格立刻变 `foreign`（真实运行时已复现）。
struct MdRow {
    slug: String,
    label: String,
    library_file: Option<String>,
}

/// `rules` / `memory` 的矩阵行。
///
/// 行集合 = 库内 `<kind>/*.md` 的 slug ∪ 各 agent **固定文件名**声明的 slug。
/// 目录型声明（`rules`、`memories`）不产生固定 slug 行：它没有自己的文件名，
/// 只给每一行提供一个「目标 = `<dir>/<slug>.md`」的单元格。
fn md_rows(lib: &Library, agents: &[AgentView], kind: &str, user_home: &str) -> Vec<Value> {
    md_row_seeds(lib, kind, user_home)
        .iter()
        .map(|row| {
            let mut cells = Map::new();
            for agent in agents {
                if let Some(cell) = md_cell(lib, agent, kind, row, user_home) {
                    cells.insert(agent.id.clone(), cell);
                }
            }
            json!({
                "name": row.slug,
                "slug": row.slug,
                "label": row.label,
                "libraryFile": row.library_file,
                "mode": "compose",
                "cells": Value::Object(cells),
            })
        })
        .collect()
}

fn md_row_seeds(lib: &Library, kind: &str, user_home: &str) -> Vec<MdRow> {
    let mut rows: Vec<MdRow> = Vec::new();
    // 1) 库内主副本 `<root>/rules/*.md`（memory 同理）。label 用库内文件名：
    // 没有任何 agent 声明同名文件时，它是这一行唯一的显示名。
    if let Ok(entries) = std::fs::read_dir(lib.dir_for(kind)) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_md_file(p))
            .collect();
        files.sort();
        for file in files {
            let slug = slug_of_path(&file);
            if slug.is_empty() || rows.iter().any(|r| r.slug == slug) {
                continue;
            }
            let Some(name) = file.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            rows.push(MdRow {
                slug,
                label: name,
                library_file: Some(store::native_str(&file)),
            });
        }
    }
    // 2) 各 agent 的固定文件名声明（`AGENTS.md` / `CLAUDE.md` / …）。同 slug
    // 只取第一个出现的文件名当 label。
    for agent in adapters::AGENTS {
        if resolve_home(agent.home, user_home).is_none() {
            continue;
        }
        let Some(patterns) = sharing::kind_patterns(agent.id, kind, "global") else {
            continue;
        };
        for rel in &patterns {
            if !is_fixed_file_decl(rel) {
                continue;
            }
            let slug = slug_of_decl(rel);
            if slug.is_empty() || rows.iter().any(|r| r.slug == slug) {
                continue;
            }
            rows.push(MdRow {
                slug,
                label: decl_file_name(rel),
                // 库内 `<kind>/<slug>.md` 若存在，第 1 步已经把它列成一行了；
                // 合成行意味着库里没有这份主副本（没有内容可共享）。
                library_file: None,
            });
        }
    }
    rows.sort_by(|a, b| a.slug.cmp(&b.slug));
    rows
}

/// 一个 agent 在某一行上的一格。
///
/// 返回 `None` 表示这一行在该 agent 上既没有同名的固定文件名声明、也没有目录型
/// 声明 —— 这个位置根本不存在，单元格不出现（不是「未启用」）。
fn md_cell(
    lib: &Library,
    agent: &AgentView,
    kind: &str,
    row: &MdRow,
    user_home: &str,
) -> Option<Value> {
    let Some(patterns) = sharing::kind_patterns(&agent.id, kind, "global") else {
        return Some(unsupported("noRulePath"));
    };
    if patterns.is_empty() {
        return Some(unsupported("noRulePath"));
    }
    let Some(base) = sharing::scope_base(user_home, &agent.id, "global", None) else {
        return Some(unsupported("noRulePath"));
    };
    let mut fixed: Option<String> = None;
    let mut dirs: Vec<String> = Vec::new();
    for rel in &patterns {
        if is_fixed_file_decl(rel) {
            // 固定文件名声明只挂在同名 slug 行：`CLAUDE.md` 不挂到 `agents` 行。
            if slug_of_decl(rel) == row.slug {
                fixed = Some(join_native(&base, rel));
            }
        } else if is_directory_decl(rel) {
            dirs.push(join_native(&base, rel));
        }
    }
    let target = match fixed {
        Some(target) => target,
        None => {
            // 目录型声明没有固定文件名：矩阵这一格的目标就是 `<dir>/<slug>.md`。
            let dir = dirs.first()?;
            join_native(dir, &format!("{}.md", row.slug))
        }
    };
    Some(rule_file_cell(lib, kind, &target, row))
}

/// 目录型声明（`rules`、`memories`、`memory`）：没有扩展名即目录。
fn is_directory_decl(rel: &str) -> bool {
    Path::new(rel).extension().is_none()
}

/// 固定文件名声明：带 `.md` / `.mdc` 扩展名的才是规则文件。同列表里的
/// `settings.json` 不是规则目标，不能产生行。
fn is_fixed_file_decl(rel: &str) -> bool {
    !is_directory_decl(rel)
        && Path::new(rel)
            .extension()
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("mdc"))
            .unwrap_or(false)
}

fn is_md_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false)
}

/// 声明里文件的显示名（`rules/global_rules.md` → `global_rules.md`）。
fn decl_file_name(rel: &str) -> String {
    Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 声明里文件名的主干 slug，与 `sharing::rule_plan` 的 `slug(stem)` 同一算法。
fn slug_of_decl(rel: &str) -> String {
    slug_of_path(Path::new(rel))
}

/// 规则 / 记忆文件的判定（见 `docs/DESIGN.md` §8.12 界面契约）。
///
/// | 盘上是什么 | 状态 |
/// |---|---|
/// | 链接，目标就是本行库内主副本 | `on` + `mode: link` |
/// | 文件里有本行 slug 的受管区块、且没被外部改过 | `on` + `mode: compose` |
/// | 有区块但被外部改过 / 链接指向别处 / 文件存在但没有区块 | `foreign` |
/// | 文件不存在、库内有主副本 | `off`（可点） |
/// | 文件不存在、库内也没有主副本 | `unsupported` + `reason: noSource` |
/// | 读盘失败 | `error` |
///
/// `path` 在所有状态下都返回：前端 enable / disable 一律带 `file: path`。
fn rule_file_cell(lib: &Library, kind: &str, target: &str, row: &MdRow) -> Value {
    let path = Path::new(target);
    let mode = default_rule_mode(target);
    // 本行主副本的期望路径：库内已有就用它，没有就用「将来建了会落在哪」。链接
    // 的「on」必须指向本行这一份，否则一条指向 `rules/claude.md` 的链接会把
    // 所有行都点亮成 on。
    let expected = match row.library_file.as_deref() {
        Some(file) => PathBuf::from(file),
        None => lib.dir_for(kind).join(format!("{}.md", row.slug)),
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match &row.library_file {
                Some(_) => json!({ "state": "off", "path": target, "mode": mode }),
                None => json!({
                    "state": "unsupported",
                    "reason": "noSource",
                    "path": target,
                    "mode": mode,
                }),
            };
        }
        Err(e) => {
            return json!({
                "state": "error",
                "reason": e.to_string(),
                "path": target,
            });
        }
    }
    if let Some(method) = linkfs::entry_link_kind(path) {
        if method != LinkMethod::Hardlink {
            if let Some(link_target) = linkfs::read_target(path) {
                if linkfs::same_target(&link_target, &expected) {
                    return json!({
                        "state": "on",
                        "path": target,
                        "mode": "link",
                        "method": method.as_str(),
                    });
                }
                return json!({ "state": "foreign", "path": target, "mode": "link" });
            }
        }
    }
    // 真实文件（含硬链接）读内容找本行的受管区块。硬链接没有目标可读，只能靠
    // 内容判定；读不到就是外来的。
    let raw = store::read_text(path);
    if let Some(block) = compose::find_block(&raw, &row.slug) {
        // 「上次生成的内容」= 库内主副本；库里没有主副本时退回区块自身（只剩
        // 「区块 rev 是否自洽」这一条判据），与 rule_plan 首次 compose 的口径一致。
        let last_generated = match row.library_file.as_deref() {
            Some(file) => store::read_text(Path::new(file)),
            None => block.body.clone(),
        };
        if compose::is_foreign_edit(&raw, &row.slug, &last_generated) {
            return json!({ "state": "foreign", "path": target, "mode": mode });
        }
        return json!({ "state": "on", "path": target, "mode": "compose" });
    }
    json!({ "state": "foreign", "path": target, "mode": mode })
}

/// 一份规则文件的缺省共享模式：`.mdc` 只支持链接（§6.3），其余按 compose。
fn default_rule_mode(target: &str) -> &'static str {
    if target.to_ascii_lowercase().ends_with(".mdc") {
        "link"
    } else {
        "compose"
    }
}

/* ------------------------------------------------------------------ */
/* 小工具                                                              */
/* ------------------------------------------------------------------ */

fn unsupported(reason: &str) -> Value {
    json!({ "state": "unsupported", "reason": reason })
}

fn join_native(base: &str, rel: &str) -> String {
    store::native_str(&Path::new(base).join(rel))
}

fn slug_of_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| model::slug(s.to_string_lossy().as_ref()))
        .unwrap_or_default()
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linkfs::LinkCap;
    use crate::settings;
    use crate::store::Store;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        dir: PathBuf,
        home: String,
        store_dir: String,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "ak-libview-{}-{}-{}",
                tag,
                std::process::id(),
                nanos
            ));
            let home = dir.join("home");
            let store_dir = dir.join("store");
            std::fs::create_dir_all(&home).expect("home");
            Fixture {
                dir,
                home: store::native_str(&home),
                store_dir: store::native_str(&store_dir),
            }
        }

        fn keeper(&self) -> Keeper {
            Keeper::new(Store::new(&self.store_dir), &self.home)
        }

        fn payload(&self) -> Value {
            let keeper = self.keeper();
            list_library(&keeper)
        }

        fn library_root(&self) -> String {
            join_native(&self.home, ".agents")
        }

        fn library_skill(&self, slug: &str) -> String {
            join_native(&self.library_root(), &format!("skills\\{}", slug))
        }

        fn agent_skill(&self, home_dir: &str, slug: &str) -> String {
            join_native(&join_native(&self.home, home_dir), &format!("skills\\{}", slug))
        }

        fn write(&self, path: &str, content: &str) {
            write_file(path, content);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            store::rmrf(&self.dir);
        }
    }

    fn write_file(path: &str, content: &str) {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, content).expect("write file");
    }

    fn row<'a>(payload: &'a Value, kind: &str, slug: &str) -> &'a Value {
        payload[kind]
            .as_array()
            .unwrap_or_else(|| panic!("{} is not an array", kind))
            .iter()
            .find(|r| r.get("slug").and_then(Value::as_str) == Some(slug))
            .unwrap_or_else(|| panic!("no {} row for {}", kind, slug))
    }

    fn cell<'a>(payload: &'a Value, kind: &str, slug: &str, agent: &str) -> &'a Value {
        let c = &row(payload, kind, slug)["cells"][agent];
        assert!(c.is_object(), "no {} cell for {} / {}", kind, slug, agent);
        c
    }

    fn skill_fixture(f: &Fixture, slug: &str) {
        f.write(
            &join_native(&f.library_skill(slug), "SKILL.md"),
            &format!(
                "---\nname: {}\ndescription: fixture\n---\n\nbody\n",
                slug
            ),
        );
    }

    #[test]
    fn an_empty_library_still_returns_the_columns_and_an_ok_payload() {
        let f = Fixture::new("empty");
        let payload = f.payload();
        assert_eq!(payload["ok"], json!(true));
        assert_eq!(payload["root"], json!(f.library_root()));
        assert_eq!(payload["skills"], json!([]));
        assert_eq!(payload["mcp"], json!([]));
        // rules/memory 的行集合含各 agent 的固定文件名声明：库是空的也有行，
        // 但没有内容可共享 → 单元格是 unsupported(noSource)，不是可点的 off。
        assert!(!payload["rules"].as_array().expect("rules").is_empty());
        assert!(!payload["memory"].as_array().expect("memory").is_empty());
        let agents_row = row(&payload, "rules", "agents");
        assert_eq!(agents_row["libraryFile"], Value::Null);
        let no_source = cell(&payload, "rules", "agents", "codex");
        assert_eq!(no_source["state"], json!("unsupported"));
        assert_eq!(no_source["reason"], json!("noSource"));
        assert_eq!(
            no_source["path"],
            json!(join_native(
                &join_native(&f.home, ".codex"),
                "AGENTS.md"
            )),
            "even an unsupported cell carries its target path"
        );
        assert!(payload["cap"]["copyOnly"].is_boolean());

        // 17 个 agent 的 home 都能解析，所以 17 列；`warp` 只是没有 skills 目录。
        let agents = payload["agents"].as_array().expect("agents");
        assert_eq!(agents.len(), adapters::AGENTS.len());
        let claude = agents
            .iter()
            .find(|a| a["id"] == json!("claude-code"))
            .expect("claude-code column");
        assert_eq!(claude["installed"], json!(false));
        assert_eq!(
            claude["skillsDir"],
            json!(join_native(&f.home, ".claude\\skills"))
        );
        assert_eq!(claude["ruleDirs"], json!([join_native(&f.home, ".claude")]));
        assert_eq!(claude["mcpStyle"], json!("remove"));
        assert!(claude["mcpFiles"].as_array().map(|a| !a.is_empty()).unwrap_or(false));
    }

    #[test]
    fn a_skill_link_into_the_library_is_on() {
        let f = Fixture::new("on");
        let lib_dir = f.library_skill("pdf");
        skill_fixture(&f, "pdf");
        let link = f.agent_skill(".claude", "pdf");
        linkfs::create_dir_link(Path::new(&lib_dir), Path::new(&link), None).expect("dir link");

        let payload = f.payload();
        let c = cell(&payload, "skills", "pdf", "claude-code");
        assert_eq!(c["state"], json!("on"));
        assert_eq!(c["path"], json!(link));
        assert!(c["method"].is_string(), "a link cell must say how it is made");

        let r = row(&payload, "skills", "pdf");
        assert_eq!(r["dir"], json!(lib_dir));
        assert!(r["size"].as_u64().unwrap_or(0) > 0);
        // 库里有主副本时不应再出现同 slug 的外部行。
        assert_eq!(payload["skills"].as_array().map(|a| a.len()), Some(1));
    }

    #[test]
    fn a_real_skill_directory_next_to_the_library_is_copy() {
        let f = Fixture::new("copy");
        skill_fixture(&f, "pdf");
        let real = f.agent_skill(".codex", "pdf");
        f.write(&join_native(&real, "SKILL.md"), "---\nname: pdf\n---\n\nbody\n");

        let payload = f.payload();
        let c = cell(&payload, "skills", "pdf", "codex");
        assert_eq!(c["state"], json!("copy"));
        assert_eq!(c["path"], json!(real));
    }

    #[test]
    fn a_copy_outside_the_library_becomes_an_external_row() {
        let f = Fixture::new("external");
        let real = f.agent_skill(".codex", "pdf");
        f.write(&join_native(&real, "SKILL.md"), "---\nname: pdf\n---\n\nbody\n");

        let payload = f.payload();
        let r = row(&payload, "skills", "pdf");
        assert_eq!(r["dir"], Value::Null);
        let external = r["external"].as_array().expect("external entries");
        assert_eq!(external.len(), 1);
        assert_eq!(external[0]["agentId"], json!("codex"));
        assert_eq!(external[0]["scope"], json!("global"));
        assert_eq!(external[0]["path"], json!(real));
        assert!(external[0]["assetId"].is_string(), "import needs an assetId");

        assert_eq!(cell(&payload, "skills", "pdf", "codex")["state"], json!("copy"));
        assert_eq!(
            cell(&payload, "skills", "pdf", "claude-code")["state"],
            json!("off")
        );
    }

    #[test]
    fn warp_has_no_skills_column_and_says_why() {
        let f = Fixture::new("warp");
        skill_fixture(&f, "pdf");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "---\nname: agents\n---\n\nbody\n",
        );

        let payload = f.payload();
        let warp_skills = cell(&payload, "skills", "pdf", "warp");
        assert_eq!(warp_skills["state"], json!("unsupported"));
        assert_eq!(warp_skills["reason"], json!("noSkillsDir"));

        let warp_rules = cell(&payload, "rules", "agents", "warp");
        assert_eq!(warp_rules["state"], json!("unsupported"));
        assert_eq!(warp_rules["reason"], json!("noRulePath"));

        let warp = payload["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .find(|a| a["id"] == json!("warp"))
            .expect("warp column");
        assert_eq!(warp["skillsDir"], Value::Null);
    }

    #[test]
    fn an_mcp_server_present_in_the_agent_config_is_on() {
        let f = Fixture::new("mcp-on");
        let record = join_native(
            &join_native(&f.library_root(), "mcp"),
            "servers\\github.json",
        );
        f.write(
            &record,
            "{\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"npx\",\"args\":[\"github-mcp\"]}",
        );
        let agent_config = join_native(&f.home, ".claude\\.mcp.json");
        f.write(
            &agent_config,
            "{\"mcpServers\":{\"github\":{\"command\":\"npx\",\"args\":[\"github-mcp\"]}}}",
        );

        let payload = f.payload();
        let c = cell(&payload, "mcp", "github", "claude-code");
        assert_eq!(c["state"], json!("on"));
        assert_eq!(c["file"], json!(agent_config));
        assert_eq!(c["configKey"], json!("github"));
        assert_eq!(c["drift"], Value::Null);
        assert_eq!(row(&payload, "mcp", "github")["record"], json!(record));
    }

    #[test]
    fn a_diverged_mcp_server_is_drift_with_the_first_differing_field() {
        let f = Fixture::new("mcp-drift");
        let record = join_native(
            &join_native(&f.library_root(), "mcp"),
            "servers\\github.json",
        );
        f.write(
            &record,
            "{\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"npx\",\"args\":[\"github-mcp\"]}",
        );
        f.write(
            &join_native(&f.home, ".claude\\.mcp.json"),
            "{\"mcpServers\":{\"github\":{\"command\":\"different-binary\"}}}",
        );

        let payload = f.payload();
        let c = cell(&payload, "mcp", "github", "claude-code");
        assert_eq!(c["state"], json!("drift"));
        assert_eq!(c["drift"]["field"], json!("command"));
        assert_eq!(c["drift"]["libraryValue"], json!("npx"));
        assert_eq!(c["drift"]["agentValue"], json!("different-binary"));
        assert_eq!(c["file"], json!(join_native(&f.home, ".claude\\.mcp.json")));
    }

    #[test]
    fn an_agent_without_an_mcp_config_is_unsupported() {
        let f = Fixture::new("mcp-none");
        f.write(
            &join_native(
                &join_native(&f.library_root(), "mcp"),
                "servers\\github.json",
            ),
            "{\"name\":\"github\",\"transport\":\"stdio\",\"command\":\"npx\"}",
        );
        let payload = f.payload();
        let c = cell(&payload, "mcp", "github", "warp");
        assert_eq!(c["state"], json!("unsupported"));
        assert_eq!(c["reason"], json!("noMcpConfig"));
        // 其余 agent 有候选文件、配置里没有这条：off。
        assert_eq!(
            cell(&payload, "mcp", "github", "codex")["state"],
            json!("off")
        );
    }

    #[test]
    fn a_composed_rule_block_is_on() {
        let f = Fixture::new("rule-on");
        let library = join_native(&join_native(&f.library_root(), "rules"), "agents.md");
        // 库内主副本的内容就是区块的内容：compose 之后 rev 对得上 = 没被外部改过。
        f.write(&library, "shared rule body\n");
        let target = join_native(&join_native(&f.home, ".codex"), "AGENTS.md");
        f.write(
            &target,
            &compose::upsert_block("# Team notes\n\n", "agents", "shared rule body\n"),
        );

        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "codex");
        assert_eq!(c["state"], json!("on"));
        assert_eq!(c["mode"], json!("compose"));
        assert_eq!(c["path"], json!(target));
        assert_eq!(
            row(&payload, "rules", "agents")["label"],
            json!("agents.md")
        );
        // 固定文件名声明只挂同名 slug 行：`CLAUDE.md` 不能出现在 agents 行。
        let cells = row(&payload, "rules", "agents")["cells"]
            .as_object()
            .expect("cells");
        assert!(
            !cells.contains_key("claude-code"),
            "CLAUDE.md must not land on the agents row: {:?}",
            cells.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_rule_file_without_our_block_is_foreign() {
        let f = Fixture::new("rule-foreign");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "---\nname: agents\n---\n\nbody\n",
        );
        let target = join_native(&f.home, ".codex\\AGENTS.md");
        f.write(&target, "# The user's own notes\n");

        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "codex");
        assert_eq!(c["state"], json!("foreign"));
        assert_eq!(c["path"], json!(target));
    }

    #[test]
    fn a_missing_rule_target_is_off_and_still_carries_its_path() {
        let f = Fixture::new("rule-off");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "---\nname: agents\n---\n\nbody\n",
        );
        let expected = join_native(&f.home, ".codex\\AGENTS.md");
        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "codex");
        assert_eq!(c["state"], json!("off"));
        assert_eq!(c["path"], json!(expected), "an off cell must still be clickable");
        assert_eq!(c["mode"], json!("compose"));
        assert!(!Path::new(&expected).exists());
    }

    #[test]
    fn a_directory_style_rule_declaration_targets_a_file_inside_the_directory() {
        let f = Fixture::new("rule-dir");
        let library = join_native(&join_native(&f.library_root(), "rules"), "agents.md");
        f.write(&library, "shared body
");
        // cursor 的全局 rules 里既有固定文件 `rules.mdc`、也有目录 `rules`：
        // 目录型声明在每一行给一个 `<dir>/<slug>.md` 的目标。
        let target = join_native(
            &join_native(&join_native(&f.home, ".cursor"), "rules"),
            "agents.md",
        );
        f.write(
            &target,
            &compose::upsert_block("# cursor rules

", "agents", "shared body
"),
        );

        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "cursor");
        assert_eq!(c["state"], json!("on"));
        assert_eq!(c["mode"], json!("compose"));
        assert_eq!(c["path"], json!(target));
    }

    #[test]
    fn a_fixed_file_declaration_wins_over_a_directory_declaration() {
        let f = Fixture::new("rule-fixed-wins");
        // 行 `rules` 来自 cursor 的固定文件声明 `rules.mdc`（另有目录 `rules`）。
        let payload = f.payload();
        let rules_row = row(&payload, "rules", "rules");
        assert_eq!(rules_row["libraryFile"], Value::Null);
        assert_eq!(rules_row["label"], json!("rules.mdc"));
        let fixed = join_native(&join_native(&f.home, ".cursor"), "rules.mdc");
        let c = &rules_row["cells"]["cursor"];
        assert_eq!(c["path"], json!(fixed));
        assert_eq!(c["mode"], json!("link"), ".mdc 只能 link");
    }

    #[test]
    fn a_missing_directory_style_target_is_a_file_inside_that_directory() {
        let f = Fixture::new("rule-dir-off");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "---\nname: agents\n---\n\nbody\n",
        );
        // cline 的全局 rules 是目录 `rules`，且这个目录还不存在：目标必须拼成
        // 目录里的文件，不能把目录本身当文件交回去。
        let cline = adapters::AGENTS
            .iter()
            .find(|a| a.id == "cline")
            .expect("cline adapter");
        let cline_home =
            store::native_str(Path::new(&resolve_home(cline.home, &f.home).expect("cline home")));
        let expected = join_native(&join_native(&cline_home, "rules"), "agents.md");
        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "cline");
        assert_eq!(c["state"], json!("off"));
        assert_eq!(c["path"], json!(expected));
    }

    #[test]
    fn a_memory_row_uses_the_memory_directory() {
        let f = Fixture::new("memory");
        let library = join_native(&join_native(&f.library_root(), "memory"), "personal.md");
        f.write(&library, "personal memory
");

        let payload = f.payload();
        let r = row(&payload, "memory", "personal");
        assert_eq!(r["libraryFile"], json!(library));
        assert_eq!(r["label"], json!("personal.md"));
        // codex 的 memory 声明是 ["AGENTS.md", "memories"]：固定名 AGENTS.md 不匹配
        // personal，落到目录 memories → 目标 memories/personal.md。
        let target = join_native(
            &join_native(&join_native(&f.home, ".codex"), "memories"),
            "personal.md",
        );
        let c = cell(&payload, "memory", "personal", "codex");
        assert_eq!(c["state"], json!("off"));
        assert_eq!(c["path"], json!(target));
    }

    #[test]
    fn fixed_file_declarations_only_attach_to_their_own_slug_row() {
        let f = Fixture::new("rule-rows");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "shared body
",
        );

        let payload = f.payload();
        // agents 行：codex 声明 AGENTS.md → 有 cell；claude 的 CLAUDE.md 不挂上来。
        let agents_row = row(&payload, "rules", "agents");
        let agents_cells = agents_row["cells"].as_object().expect("cells");
        assert!(agents_cells.contains_key("codex"));
        assert!(!agents_cells.contains_key("claude-code"));
        let codex = &agents_cells["codex"];
        assert_eq!(codex["state"], json!("off"), "库里有主副本、目标还没建 → off");
        assert_eq!(
            codex["path"],
            json!(join_native(&join_native(&f.home, ".codex"), "AGENTS.md"))
        );

        // claude 行由固定文件名 CLAUDE.md 合成：库里没有 claude.md，目标也不存在
        // → noSource（不可点），但 path 仍然返回给前端展示/后续调用。
        let claude_row = row(&payload, "rules", "claude");
        assert_eq!(claude_row["label"], json!("CLAUDE.md"));
        assert_eq!(claude_row["libraryFile"], Value::Null);
        let claude_cells = claude_row["cells"].as_object().expect("cells");
        assert!(!claude_cells.contains_key("codex"));
        let claude = &claude_cells["claude-code"];
        assert_eq!(claude["state"], json!("unsupported"));
        assert_eq!(claude["reason"], json!("noSource"));
        assert_eq!(
            claude["path"],
            json!(join_native(&join_native(&f.home, ".claude"), "CLAUDE.md"))
        );
    }

    #[test]
    fn a_synthesized_row_becomes_clickable_once_the_library_has_its_file() {
        let f = Fixture::new("rule-synth-source");
        // 库里还没有 claude.md：claude 行是合成的，claude 单元格 noSource。
        let payload = f.payload();
        let claude = &row(&payload, "rules", "claude")["cells"]["claude-code"];
        assert_eq!(claude["state"], json!("unsupported"));
        assert_eq!(claude["reason"], json!("noSource"));

        let library = join_native(&join_native(&f.library_root(), "rules"), "claude.md");
        f.write(&library, "shared body
");

        let payload = f.payload();
        let claude_row = row(&payload, "rules", "claude");
        assert_eq!(claude_row["libraryFile"], json!(library));
        let claude = &claude_row["cells"]["claude-code"];
        assert_eq!(claude["state"], json!("off"));
        assert!(
            claude.get("reason").is_none(),
            "off with a library source must be clickable: {}",
            claude
        );
        assert_eq!(
            claude["path"],
            json!(join_native(&join_native(&f.home, ".claude"), "CLAUDE.md"))
        );
    }

    #[test]
    fn directory_declarations_appear_on_every_row() {
        let f = Fixture::new("rule-dir-rows");
        for slug in ["agents", "claude"] {
            f.write(
                &join_native(
                    &join_native(&f.library_root(), "rules"),
                    &format!("{}.md", slug),
                ),
                "shared body
",
            );
        }
        let payload = f.payload();
        let cursor_rules_dir = join_native(&join_native(&f.home, ".cursor"), "rules");
        for slug in ["agents", "claude"] {
            let r = row(&payload, "rules", slug);
            let c = &r["cells"]["cursor"];
            assert_eq!(
                c["path"],
                json!(join_native(&cursor_rules_dir, &format!("{}.md", slug))),
                "目录型声明在每一行都有目标"
            );
            assert_eq!(c["state"], json!("off"), "库里有主副本、目标文件还没建");
        }
    }

    #[test]
    fn a_hand_edited_compose_block_is_foreign() {
        let f = Fixture::new("rule-foreign-edit");
        let library = join_native(&join_native(&f.library_root(), "rules"), "agents.md");
        f.write(&library, "shared body
");
        let target = join_native(&join_native(&f.home, ".codex"), "AGENTS.md");
        // 区块 rev 是库内内容的指纹；盘上区块内被改过 → rev 对不上 → 外部改动。
        let edited = compose::upsert_block("# notes

", "agents", "shared body
")
            .replace("shared body", "hand edited body");
        f.write(&target, &edited);

        let payload = f.payload();
        let c = cell(&payload, "rules", "agents", "codex");
        assert_eq!(c["state"], json!("foreign"));
        assert_eq!(c["path"], json!(target));
    }

    #[test]
    fn a_rule_file_linked_to_its_library_copy_is_on() {
        let f = Fixture::new("rule-link");
        let library = join_native(&join_native(&f.library_root(), "rules"), "claude.md");
        f.write(&library, "shared body
");
        let target = join_native(&join_native(&f.home, ".claude"), "CLAUDE.md");
        linkfs::create_file_link(Path::new(&library), Path::new(&target), None).expect("file link");

        let payload = f.payload();
        let c = cell(&payload, "rules", "claude", "claude-code");
        assert_eq!(c["state"], json!("on"));
        assert_eq!(c["mode"], json!("link"));
        assert_eq!(c["path"], json!(target));
    }

    #[test]
    fn link_cap_serialises_to_camel_case_and_round_trips() {
        let cap = LinkCap {
            symlink_dir: true,
            symlink_file: true,
            junction: true,
            hardlink: false,
            dir_order: vec![LinkMethod::Symlink, LinkMethod::Junction],
            file_order: vec![LinkMethod::Symlink],
            probed_at: "C:\\tmp\\probe".to_string(),
        };
        let v = cap.to_json();
        assert_eq!(v["symlinkDir"], json!(true));
        assert_eq!(v["symlinkFile"], json!(true));
        assert_eq!(v["junction"], json!(true));
        assert_eq!(v["hardlink"], json!(false));
        assert_eq!(v["probedAt"], json!("C:\\tmp\\probe"));
        assert_eq!(v["dirOrder"], json!(["symlink", "junction"]));
        assert_eq!(v["fileOrder"], json!(["symlink"]));
        assert_eq!(v["copyOnly"], json!(false));
        assert!(
            v.get("symlink_dir").is_none(),
            "keys must be camelCase, not snake_case"
        );
        // 反向：JSON 里的每个值与结构体逐项相等（方式字符串能解析回来）。
        assert_eq!(v["symlink_dir"].as_bool(), None);
        assert_eq!(v["junction"].as_bool(), Some(cap.junction));
        let dir_order: Vec<LinkMethod> = v["dirOrder"]
            .as_array()
            .expect("dirOrder")
            .iter()
            .map(|m| LinkMethod::parse(m.as_str().expect("method string")).expect("known method"))
            .collect();
        assert_eq!(dir_order, cap.dir_order);
        assert_eq!(v["copyOnly"].as_bool(), Some(cap.copy_only()));
    }

    /// 只读：视图调用前后整棵夹具树逐字节不变（能力探测只在 store 下留缓存）。
    #[test]
    fn listing_never_writes_into_the_home_or_the_library() {
        let f = Fixture::new("readonly");
        skill_fixture(&f, "pdf");
        f.write(
            &join_native(&join_native(&f.library_root(), "rules"), "agents.md"),
            "---\nname: agents\n---\n\nbody\n",
        );
        let before = snapshot(&f.home);
        let _ = settings::read(&f.store_dir);
        let _ = f.payload();
        let after = snapshot(&f.home);
        assert_eq!(before, after, "list_library must not touch home / library");
    }

    fn snapshot(root: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .map(store::native_str)
                    .unwrap_or_default();
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => stack.push(path),
                    Ok(meta) => {
                        let bytes = std::fs::read(&path).unwrap_or_default();
                        out.push(format!("{}#{}#{}", rel, meta.len(), bytes.len()));
                    }
                    Err(_) => out.push(format!("{}#?", rel)),
                }
            }
        }
        out.sort();
        out
    }
}
