//! Scanner.
//!
//! ## Why this is a port, not a redesign
//!
//! The reference scanner defines the *meaning* of the data: which files become
//! assets, what an asset's identity is, how duplicates collapse, what
//! "effective" and "shadowed" mean. Re-deriving any of that here would
//! silently change what the UI reports. So the rules are copied literally,
//! including two deliberate oddities that look like bugs:
//!
//!  1. `describePluginDir` checks `existsFile()` *before* `isDirectory()`, so
//!     the nested-skill branch is unreachable and `skillCount` is always 0.
//!     Reproducing it keeps numbers identical; fixing it would quietly
//!     change every plugin's counters and needs its own decision.
//!  2. `listFiles` treats a symlinked directory as a file (Dirent::isDirectory
//!     is false for links). Skill *roots* do follow links, but a link found
//!     deeper is counted as one file. Copied as-is.
//!
//! ## Where the speed comes from
//!
//! The reference version's cost was structural, not micro-optimisation:
//!   49k `statSync` calls (a second pass just to sum plugin sizes) and 9.4k
//!   `realpathSync` calls (one per directory, for a cycle guard). Both
//!   disappear here: `DirEntry::metadata()` on Windows carries the size from
//!   the directory scan, and the cycle guard only needs a canonicalise when
//!   an entry is actually a link. On top of that the per-agent work runs in
//!   parallel.
//!
//! Output is `serde_json::Value` for the same reason model.rs is: field shape
//! is part of the contract with the renderer.

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use super::adapters::{resolve_home, Agent, AGENTS, SHARED_SKILLS_DIR};
use super::frontmatter;
use super::jsonc::strip_json_comments;
use super::model::*;

const SKILL_MANIFEST: &str = "SKILL.md";
const MAX_TEXT_BYTES: u64 = 400 * 1024;
const MAX_PLUGIN_DEPTH: usize = 4;
const SKIP_DIRS: [&str; 8] = [
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    "target",
    "__pycache__",
    ".venv",
];

/* ------------------------------------------------------------------ */
/* fs helpers                                                          */
/* ------------------------------------------------------------------ */

fn exists_dir(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn exists_file(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

fn read_all(p: &Path) -> String {
    fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// `readHead` — the first 400KB plus the real size. Only memory entries need
/// the text; callers that just want sizes use `walk_files` instead.
fn read_head(p: &Path) -> (String, u64) {
    let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let take = size.min(MAX_TEXT_BYTES) as usize;
    match fs::read(p) {
        Ok(bytes) => {
            let slice = &bytes[..take.min(bytes.len())];
            (String::from_utf8_lossy(slice).into_owned(), size)
        }
        Err(_) => (String::new(), size),
    }
}

struct DirEntryInfo {
    name: String,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

/// `readdirSync(dir, {withFileTypes:true})`, in OS order (the reference code relies
/// on that order for list stability, and matching it keeps diffs meaningful).
fn read_entries(dir: &Path) -> Vec<DirEntryInfo> {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let (is_dir, is_file, is_symlink) = match e.file_type() {
            Ok(ft) => (ft.is_dir(), ft.is_file(), ft.is_symlink()),
            Err(_) => (false, false, false),
        };
        out.push(DirEntryInfo {
            name,
            is_dir,
            is_file,
            is_symlink,
        });
    }
    out
}

/// `listFiles()` — recursive relative paths (forward slashes) plus the byte
/// total, in one pass. The reference version walked to collect names and then
/// stat'ed every file to sum sizes; that second pass was 5.3s of the 11s.
struct Walk {
    files: Vec<String>,
    bytes: u64,
}

fn walk_files(root: &Path) -> Walk {
    let mut w = Walk {
        files: Vec::new(),
        bytes: 0,
    };
    if !exists_dir(root) {
        // listFiles() returns [] when readdir fails (a file, or no access).
        return w;
    }
    walk_into(root, "", &mut w);
    w
}

fn walk_into(dir: &Path, base: &str, w: &mut Walk) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        let ft = match e.file_type() {
            Ok(f) => f,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk_into(&dir.join(&name), &rel, w);
        } else {
            // A symlinked directory lands here too — see the module note.
            let size = e.metadata().map(|m| m.len()).unwrap_or(0);
            w.files.push(rel);
            w.bytes += size;
        }
    }
}

fn mtime_of(p: &Path) -> Option<String> {
    fs::metadata(p)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(iso_from_systemtime)
}

fn file_size(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn has_ext_ci(name: &str, exts: &[&str]) -> bool {
    let low = name.to_lowercase();
    exts.iter().any(|e| low.ends_with(e))
}

fn first_line(s: &str) -> String {
    for raw in s.split('\n') {
        let l = raw.trim_end_matches('\r').trim();
        if !l.is_empty() && !l.starts_with('#') {
            return l.chars().take(120).collect();
        }
    }
    String::new()
}

fn trim_preview(s: &str, n: usize) -> String {
    js_slice(s.trim(), n)
}

/// `str.replace(/\.md$/i, '')` — case-insensitive, unlike `basename_strip`.
fn strip_md_ci(name: &str) -> String {
    if name.len() >= 3 && name.to_lowercase().ends_with(".md") {
        name[..name.len() - 3].to_string()
    } else {
        name.to_string()
    }
}

/* ------------------------------------------------------------------ */
/* scan context                                                        */
/* ------------------------------------------------------------------ */

#[derive(Clone)]
struct Ctx {
    agent_id: String,
    agent_label: String,
    scope: &'static str,
    project_id: Option<String>,
    project_name: Option<String>,
    root: String,
    shared: bool,
}

impl Ctx {
    fn global(agent: &Agent, home: &str) -> Ctx {
        Ctx {
            agent_id: agent.id.to_string(),
            agent_label: agent.label.to_string(),
            scope: "global",
            project_id: None,
            project_name: None,
            root: home.to_string(),
            shared: false,
        }
    }
    fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }
}

/* ------------------------------------------------------------------ */
/* skills                                                             */
/* ------------------------------------------------------------------ */

fn scan_skills(root_dir: &Path, ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    if !exists_dir(root_dir) {
        return out;
    }
    for e in read_entries(root_dir) {
        let dir = root_dir.join(&e.name);
        // A junction/symlink entry does not report is_dir(); stat through it
        // so a linked skill directory is still scanned.
        let mut is_dir = e.is_dir;
        if !is_dir && e.is_symlink {
            is_dir = exists_dir(&dir);
        }
        let manifest: Option<PathBuf> = if is_dir {
            let m = dir.join(SKILL_MANIFEST);
            if exists_file(&m) {
                Some(m)
            } else {
                None
            }
        } else if e.is_file && e.name.to_lowercase().ends_with(".md") {
            Some(dir.clone())
        } else {
            None
        };
        let manifest = match manifest {
            Some(m) => m,
            None => continue,
        };

        let raw = read_all(&manifest);
        let fm = frontmatter::parse(&raw);
        let skill_dir = if is_dir {
            dir.clone()
        } else {
            manifest
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(dir.clone())
        };
        let walk = walk_files(&skill_dir);
        let size = walk.bytes;
        let mtime = mtime_of(&manifest);

        let mut issues: Vec<Value> = Vec::new();
        let name = match fm.data.get("name") {
            Some(v) if js_truthy(v) => js_string(v),
            _ => file_name(&skill_dir),
        };
        let description = fm
            .data
            .get("description")
            .map(js_string)
            .unwrap_or_default();

        let asset = make_asset(NewAsset {
            kind: "skill",
            name,
            agent_id: ctx.agent_id.clone(),
            agent_label: ctx.agent_label.clone(),
            scope: ctx.scope.to_string(),
            project_id: ctx.project_id.clone(),
            project_name: ctx.project_name.clone(),
            root: Some(ctx.root.clone()),
            dir: Some(norm_str(&skill_dir)),
            file: Some(norm_str(&manifest)),
            files: walk
                .files
                .iter()
                .map(|f| norm_str(&join_native(&skill_dir, f)))
                .collect(),
            format: Some("md".to_string()),
            size,
            mtime,
            fingerprint: Some(hash16(&raw)),
            meta: json!({
                "relLabel": e.name,
                "frontmatter": fm.data.clone(),
                "hasFrontmatter": fm.has_frontmatter,
                "bodyPreview": trim_preview(&fm.body, 400),
                "version": or_null(&fm.data, "version"),
                "author": or_null(&fm.data, "author"),
                "license": or_null(&fm.data, "license"),
                "allowedTools": or_null(&fm.data, "allowed-tools"),
                "bodyLines": fm.body.split('\n').count()
            }),
            description,
            tags: tags_of(&fm.data),
            shared: ctx.shared,
            writable: Some(!ctx.shared),
            issues: Vec::new(),
            ..Default::default()
        });

        if !fm.has_frontmatter {
            issues.push(json!({"level":"error","field":"frontmatter","message":"missing YAML frontmatter — the agent cannot load this skill"}));
        } else {
            let no_name = !fm.data.get("name").map(js_truthy).unwrap_or(false);
            let no_desc = !fm.data.get("description").map(js_truthy).unwrap_or(false);
            if no_name {
                issues.push(
                    json!({"level":"error","field":"name","message":"frontmatter.name missing"}),
                );
            }
            if no_desc {
                issues.push(json!({"level":"error","field":"description","message":"frontmatter.description missing — skill will never auto-trigger"}));
            }
            if fm.body.trim().is_empty() {
                issues.push(json!({"level":"warn","field":"body","message":"empty skill body"}));
            }
        }
        out.push(with_issues(asset, issues));
    }
    out
}

/* ------------------------------------------------------------------ */
/* rules                                                              */
/* ------------------------------------------------------------------ */

fn rule_role(rel: &str) -> &'static str {
    let b = basename(rel).to_lowercase();
    let stem_is = |s: &str| b == format!("{}.md", s);
    if [
        "agents",
        "claude",
        "gemini",
        "soul",
        "identity",
        "user",
        "cursorrules",
        "windsurfrules",
        "clinerules",
        "roo",
    ]
    .iter()
    .any(|s| stem_is(s))
    {
        return "instruction";
    }
    if b.contains("copilot-instructions") {
        return "instruction";
    }
    if b.starts_with("boot.md") || b.starts_with("boot.mdc") {
        return "bootstrap";
    }
    if b.ends_with("rule") || b.ends_with("rules") {
        return "rules-dir";
    }
    if b == "goosehints" {
        return "hints";
    }
    if b.ends_with("settings.json") || b.ends_with(".local.md") {
        return "settings";
    }
    "doc"
}

fn scan_rules(patterns: &[&str], ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    let root = ctx.root_path();
    for rel in patterns {
        let abs = join_native(&root, rel);
        if exists_dir(&abs) {
            let rel_dir = abs
                .strip_prefix(&root)
                .map(norm_str)
                .unwrap_or_else(|_| (*rel).to_string());
            for e in read_entries(&abs) {
                if e.is_dir {
                    continue;
                }
                if !has_ext_ci(&e.name, &[".md", ".mdc", ".txt"]) {
                    continue;
                }
                let label = format!("{}/{}", rel_dir, e.name);
                out.push(make_rule(&abs.join(&e.name), ctx, &label));
            }
        } else if exists_file(&abs) {
            if has_ext_ci(rel, &[".json", ".jsonc"]) {
                continue; // settings, handled elsewhere
            }
            out.push(make_rule(&abs, ctx, rel));
        }
    }
    out
}

fn make_rule(file: &Path, ctx: &Ctx, rel_label: &str) -> Value {
    let raw = read_all(file);
    let fm = frontmatter::parse(&raw);
    let size = file_size(file);
    let mtime = mtime_of(file);
    let ext = ext_of(file);
    let body = fm.body.clone();

    let name = match fm.data.get("description") {
        Some(v) if js_truthy(v) => js_slice(&js_string(v), 60),
        _ => file_name(file),
    };
    let description = match fm.data.get("description") {
        Some(v) if js_truthy(v) => js_string(v),
        _ => first_line(&body),
    };
    let always_apply = match fm.data.get("alwaysApply") {
        Some(Value::Null) | None => fm.data.get("always_apply").cloned().unwrap_or(Value::Null),
        Some(v) => v.clone(),
    };

    let asset = make_asset(NewAsset {
        kind: "rule",
        name,
        agent_id: ctx.agent_id.clone(),
        agent_label: ctx.agent_label.clone(),
        scope: ctx.scope.to_string(),
        project_id: ctx.project_id.clone(),
        project_name: ctx.project_name.clone(),
        root: Some(ctx.root.clone()),
        dir: Some(norm_str(file.parent().unwrap_or_else(|| Path::new("")))),
        file: Some(norm_str(file)),
        files: vec![norm_str(file)],
        format: Some(if file_name(file).ends_with(".mdc") {
            // `/\.mdc$/` is case-sensitive: `A.MDC` keeps `MDC`.
            "md".to_string()
        } else if ext.is_empty() {
            "md".to_string() // makeAsset's `|| 'md'` default for extensionless files
        } else {
            ext.clone()
        }),
        size,
        mtime,
        fingerprint: Some(hash16(&raw)),
        meta: json!({
            "relLabel": rel_label,
            "role": rule_role(rel_label),
            "alwaysApply": always_apply,
            "globs": or_null(&fm.data, "globs"),
            "frontmatter": fm.data.clone(),
            "bytes": size,
            "estTokens": (size + 3) / 4,
            "bodyPreview": trim_preview(&body, 400)
        }),
        description,
        tags: Vec::new(),
        shared: false,
        writable: Some(true),
        issues: Vec::new(),
        ..Default::default()
    });

    let mut issues = Vec::new();
    if size > 60_000 {
        issues.push(json!({
            "level":"warn","field":"size",
            "message": format!("{}KB — always-loaded rules this large eat the context window", (size as f64 / 1024.0).round() as u64)
        }));
    }
    if body.trim().is_empty() {
        issues.push(json!({"level":"warn","field":"body","message":"empty rule file"}));
    }
    with_issues(asset, issues)
}

/* ------------------------------------------------------------------ */
/* memory                                                             */
/* ------------------------------------------------------------------ */

fn scan_memory(patterns: &[&str], ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    let root = ctx.root_path();
    for rel in patterns {
        let abs = join_native(&root, rel);
        if exists_file(&abs) && has_ext_ci(rel, &[".md"]) {
            out.push(make_memory(&abs, ctx, rel));
        } else if exists_dir(&abs) {
            for e in read_entries(&abs) {
                if e.is_dir {
                    continue;
                }
                if !has_ext_ci(&e.name, &[".md"]) {
                    continue;
                }
                if has_ext_ci(&e.name, &[".bak"]) {
                    continue; // 同样不可达；保留以对齐
                }
                let label = format!("{}/{}", rel, e.name);
                out.push(make_memory(&abs.join(&e.name), ctx, &label));
            }
        }
    }
    out
}

fn make_memory(file: &Path, ctx: &Ctx, rel_label: &str) -> Value {
    let (head, size) = read_head(file);
    let full = read_all(file);
    let fm = frontmatter::parse(&head);
    let mtime = mtime_of(file);

    let name = match fm.data.get("title") {
        Some(v) if js_truthy(v) => js_string(v),
        _ => strip_md_ci(&basename(&norm_str(file))),
    };
    let is_long_term = norm_str(file).to_lowercase().ends_with("memory.md");

    make_asset(NewAsset {
        kind: "memory",
        name,
        agent_id: ctx.agent_id.clone(),
        agent_label: ctx.agent_label.clone(),
        scope: ctx.scope.to_string(),
        project_id: ctx.project_id.clone(),
        project_name: ctx.project_name.clone(),
        root: Some(ctx.root.clone()),
        dir: Some(norm_str(file.parent().unwrap_or_else(|| Path::new("")))),
        file: Some(norm_str(file)),
        files: vec![norm_str(file)],
        format: Some("md".to_string()),
        size,
        mtime,
        fingerprint: Some(hash16(&full)),
        meta: json!({
            "relLabel": rel_label,
            "role": if is_long_term { "long-term" } else { "daily-log" },
            "preview": js_slice(&head, 500),
            "lines": full.split('\n').count()
        }),
        description: first_line(&head),
        tags: tags_of(&fm.data),
        shared: false,
        writable: Some(true),
        issues: Vec::new(),
        ..Default::default()
    })
}

/* ------------------------------------------------------------------ */
/* MCP                                                                */
/* ------------------------------------------------------------------ */

fn scan_mcp(patterns: &[&str], ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    let root = ctx.root_path();
    for rel in patterns {
        let abs = join_native(&root, rel);
        if !exists_file(&abs) {
            continue;
        }
        let ext = ext_of(&abs).to_lowercase();
        if ext == "toml" || ext == "yaml" || ext == "yml" {
            out.extend(scan_mcp_structured(&abs, ctx, rel));
            continue;
        }
        let raw = read_all(&abs);
        let cleaned = strip_json_comments(&raw);
        let doc: Value = match serde_json::from_str(&cleaned) {
            Ok(v) => v,
            Err(e) => {
                out.push(broken_mcp(&abs, ctx, rel, &format!("invalid JSON: {}", e)));
                continue;
            }
        };

        // Bucket selection mirrors the `||` chain exactly.
        let mut buckets: Vec<(String, Vec<(String, Value)>)> = Vec::new();
        let srv = ["mcpServers", "servers"]
            .iter()
            .find_map(|k| doc.get(*k).filter(|v| js_truthy(v)))
            .or_else(|| {
                doc.get("mcp")
                    .and_then(|m| m.get("servers"))
                    .filter(|v| js_truthy(v))
            });
        if let Some(Value::Object(map)) = srv {
            buckets.push((
                String::new(),
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            ));
        }
        // Top-level `<name>: {command|url: ...}` entries also count as servers.
        // The reference guard tests `json.mcp` but then iterates the top level — kept
        // verbatim so unusual configs bucket the same way.
        if doc.get("mcp").map(|v| v.is_object()).unwrap_or(false) {
            if let Value::Object(top) = &doc {
                for (k, v) in top.iter() {
                    if v.is_object()
                        && (v.get("command").map(js_truthy).unwrap_or(false)
                            || v.get("url").map(js_truthy).unwrap_or(false))
                    {
                        buckets.push((format!("{}.", k), vec![(k.clone(), v.clone())]));
                    }
                }
            }
        }
        if buckets.is_empty() {
            if doc.get("command").map(js_truthy).unwrap_or(false) {
                let key = basename_strip(&norm_str(&abs), &format!(".{}", ext));
                buckets.push((String::new(), vec![(key, doc.clone())]));
            }
        }

        for (prefix, servers) in buckets {
            for (id, cfg) in servers {
                out.push(make_mcp(
                    &abs,
                    ctx,
                    rel,
                    &format!("{}{}", prefix, id),
                    &cfg,
                    &raw,
                    None,
                ));
            }
        }
    }
    out
}

fn scan_mcp_structured(file: &Path, ctx: &Ctx, rel: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let raw = read_all(file);
    let ext = ext_of(file).to_lowercase();

    if ext == "toml" {
        let doc: toml::Value = match toml::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                out.push(broken_mcp(file, ctx, rel, "unparsable config"));
                return out;
            }
        };
        let doc_json = toml_to_json(&doc);
        for bucket in ["mcp_servers", "mcp"] {
            if let Some(Value::Object(map)) = doc_json.get(bucket) {
                for (name, cfg) in map.iter() {
                    if cfg.is_object() {
                        out.push(make_mcp(file, ctx, rel, name, cfg, &raw, Some("toml")));
                    }
                }
            }
        }
    } else {
        let doc: Value = match serde_yaml_ng::from_str::<Value>(&raw) {
            Ok(v) if v.is_object() => v,
            Ok(Value::Null) => json!({}),
            Ok(_) => {
                out.push(broken_mcp(file, ctx, rel, "unparsable config"));
                return out;
            }
            Err(_) => {
                out.push(broken_mcp(file, ctx, rel, "unparsable config"));
                return out;
            }
        };
        for bucket in ["mcpServers", "servers", "mcp_servers"] {
            if let Some(Value::Object(map)) = doc.get(bucket) {
                for (name, cfg) in map.iter() {
                    if !cfg.is_object() {
                        continue;
                    }
                    let mut cfg = cfg.clone();
                    // goose-style keys: `cmd` means `command`.
                    if cfg.get("cmd").map(js_truthy).unwrap_or(false)
                        && !cfg.get("command").map(js_truthy).unwrap_or(false)
                    {
                        if let Value::Object(m) = &mut cfg {
                            let cmd = m.get("cmd").cloned().unwrap_or(Value::Null);
                            m.insert("command".to_string(), cmd);
                        }
                    }
                    out.push(make_mcp(file, ctx, rel, name, &cfg, &raw, Some("yaml")));
                }
            }
        }
    }
    out
}

fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            let mut m = Map::new();
            for (k, val) in t.iter() {
                m.insert(k.clone(), toml_to_json(val));
            }
            Value::Object(m)
        }
    }
}

fn broken_mcp(file: &Path, ctx: &Ctx, rel: &str, message: &str) -> Value {
    let size = file_size(file);
    let mtime = mtime_of(file);
    let ext = ext_of(file);
    let mut asset = make_asset(NewAsset {
        kind: "mcp",
        name: file_name(file),
        agent_id: ctx.agent_id.clone(),
        agent_label: ctx.agent_label.clone(),
        scope: ctx.scope.to_string(),
        project_id: ctx.project_id.clone(),
        project_name: ctx.project_name.clone(),
        root: Some(ctx.root.clone()),
        dir: Some(norm_str(file.parent().unwrap_or_else(|| Path::new("")))),
        file: Some(norm_str(file)),
        files: vec![norm_str(file)],
        format: Some(if ext.is_empty() {
            "json".to_string()
        } else {
            ext
        }),
        size,
        mtime,
        description: "unparsable config".to_string(),
        ..Default::default()
    });
    let _ = rel;
    if let Value::Object(m) = &mut asset {
        m.insert(
            "issues".to_string(),
            json!([{"level":"error","field":"json","message":message}]),
        );
    }
    asset
}

fn make_mcp(
    file: &Path,
    ctx: &Ctx,
    rel: &str,
    id: &str,
    cfg: &Value,
    raw: &str,
    format: Option<&str>,
) -> Value {
    let size = file_size(file);
    let mtime = mtime_of(file);
    let ext = ext_of(file);
    let url = cfg.get("url").cloned().unwrap_or(Value::Null);
    let has_url = js_truthy(&url);

    let description = if has_url {
        js_string(&url)
    } else {
        let mut parts: Vec<String> = Vec::new();
        if cfg.get("command").map(js_truthy).unwrap_or(false) {
            parts.push(js_string(cfg.get("command").unwrap()));
        }
        if let Some(Value::Array(args)) = cfg.get("args") {
            for a in args {
                if js_truthy(a) {
                    parts.push(js_string(a));
                }
            }
        }
        parts.join(" ")
    };

    let disabled = matches!(cfg.get("disabled"), Some(Value::Bool(true)));
    // `Object.keys(cfg.env)` — arrays and strings
    // yield index keys.
    let env = cfg.get("env");
    let env_keys: Vec<String> = match env {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        Some(Value::Array(a)) => (0..a.len()).map(|i| i.to_string()).collect(),
        Some(Value::String(s)) => (0..s.encode_utf16().count())
            .map(|i| i.to_string())
            .collect(),
        _ => Vec::new(),
    };
    let has_env = env.map(js_truthy).unwrap_or(false) && !env_keys.is_empty();
    let header_keys: Vec<String> = cfg
        .get("headers")
        .and_then(|h| h.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let transport = if has_url {
        cfg.get("type")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "http".to_string())
    } else {
        "stdio".to_string()
    };

    let asset_id_str = format!(
        "mcp:{}:{}:{}:{}:{}",
        id,
        ctx.agent_id,
        ctx.scope,
        ctx.project_id.clone().unwrap_or_else(|| "-".to_string()),
        &hash16(&norm_str(file).to_lowercase())[..6]
    );

    make_asset(NewAsset {
        kind: "mcp",
        name: id.to_string(),
        id: Some(asset_id_str),
        agent_id: ctx.agent_id.clone(),
        agent_label: ctx.agent_label.clone(),
        scope: ctx.scope.to_string(),
        project_id: ctx.project_id.clone(),
        project_name: ctx.project_name.clone(),
        root: Some(ctx.root.clone()),
        dir: Some(norm_str(file.parent().unwrap_or_else(|| Path::new("")))),
        file: Some(norm_str(file)),
        files: vec![norm_str(file)],
        format: Some(format.map(|s| s.to_string()).unwrap_or_else(|| {
            if ext.is_empty() {
                "json".to_string()
            } else {
                ext
            }
        })),
        enabled: Some(!disabled),
        size,
        mtime,
        fingerprint: Some(hash16(&format!("{}#{}", raw, id))),
        meta: json!({
            "relLabel": rel,
            "configKey": id,
            "transport": transport,
            "command": or_null(cfg, "command"),
            "args": or_null(cfg, "args"),
            "url": or_null(cfg, "url"),
            "hasEnv": has_env,
            "envKeys": env_keys,
            "headerKeys": header_keys,
            "config": cfg,
            "containerKey": norm_str(file).replace('\\', "/")
        }),
        description,
        tags: Vec::new(),
        shared: false,
        writable: Some(true),
        issues: validate_mcp_server(id, cfg),
        ..Default::default()
    })
}

/// Issue order matters — the UI shows them in the order returned.
/// Delegates to [`super::validate::validate_mcp_server`] — one rule set, not two.
///
/// A non-object config is not special-cased: an array yields the stdio/http
/// error and then indexed unknown-key warnings. Real manifests are always
/// objects, so only a malformed file reaches that branch.
fn validate_mcp_server(id: &str, cfg: &Value) -> Vec<Value> {
    super::validate::validate_mcp_server(id, cfg)
}

/* ------------------------------------------------------------------ */
/* plugins                                                            */
/* ------------------------------------------------------------------ */

fn manifest_candidates(dir: &Path) -> Vec<PathBuf> {
    [
        ".codebuddy-plugin/plugin.json",
        ".claude-plugin/plugin.json",
        ".workbuddy-plugin/plugin.json",
        ".gemini-extension/plugin.json",
        "plugin.json",
    ]
    .iter()
    .map(|rel| join_native(dir, rel))
    .filter(|p| exists_file(p))
    .collect()
}

fn entry_named<'a>(entries: &'a [DirEntryInfo], name: &str) -> Option<&'a DirEntryInfo> {
    entries.iter().find(|e| e.name == name)
}

/// `existsFile` / `existsDir` for a direct child, answered from the already
/// read directory listing. A symlink still needs one stat because the
/// listing does not say what it points at.
fn child_is_dir(dir: &Path, entries: &[DirEntryInfo], name: &str) -> bool {
    match entry_named(entries, name) {
        Some(e) if e.is_dir => true,
        Some(e) if e.is_symlink => exists_dir(&dir.join(name)),
        _ => false,
    }
}

fn child_is_file(dir: &Path, entries: &[DirEntryInfo], name: &str) -> bool {
    match entry_named(entries, name) {
        Some(e) if e.is_file => true,
        Some(e) if e.is_symlink => exists_file(&dir.join(name)),
        _ => false,
    }
}

/// `manifestCandidates(dir)` from a listing: the nested `plugin.json` still
/// costs one stat, but the five unconditional probes done per
/// directory collapse into name lookups. Order is preserved because
/// `manifests[0]` decides which manifest wins.
fn manifest_candidates_in(dir: &Path, entries: &[DirEntryInfo]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for container in [
        ".codebuddy-plugin",
        ".claude-plugin",
        ".workbuddy-plugin",
        ".gemini-extension",
    ] {
        if child_is_dir(dir, entries, container) {
            let candidate = dir.join(container).join("plugin.json");
            if exists_file(&candidate) {
                out.push(candidate);
            }
        }
    }
    if child_is_file(dir, entries, "plugin.json") {
        out.push(dir.join("plugin.json"));
    }
    out
}

fn is_marketplace_in(dir: &Path, entries: &[DirEntryInfo]) -> bool {
    if child_is_file(dir, entries, "marketplace.json") {
        return true;
    }
    for container in [".claude-plugin", ".codebuddy-plugin"] {
        if child_is_dir(dir, entries, container)
            && exists_file(&dir.join(container).join("marketplace.json"))
        {
            return true;
        }
    }
    false
}

fn collect_plugin_targets(
    dir: &Path,
    label: &str,
    depth: usize,
    out: &mut Vec<(PathBuf, String, Vec<DirEntryInfo>)>,
) {
    if depth > MAX_PLUGIN_DEPTH {
        return;
    }
    // One listing per directory; every decision below is answered from it.
    let entries = read_entries(dir);
    if is_marketplace_in(dir, &entries) {
        for e in entries.iter() {
            if !e.is_dir || e.name.starts_with('.') {
                continue;
            }
            collect_plugin_targets(
                &dir.join(&e.name),
                &format!("{}/{}", label, e.name),
                depth + 1,
                out,
            );
        }
        return;
    }
    if !manifest_candidates_in(dir, &entries).is_empty() {
        out.push((dir.to_path_buf(), label.to_string(), entries));
        return;
    }
    for e in entries.iter() {
        if !e.is_dir {
            continue;
        }
        if e.name.starts_with('.') && e.name != ".claude-plugin" {
            continue;
        }
        collect_plugin_targets(
            &dir.join(&e.name),
            &format!("{}/{}", label, e.name),
            depth + 1,
            out,
        );
    }
}

fn scan_plugins(patterns: &[&str], ctx: &Ctx) -> Vec<Value> {
    let root = ctx.root_path();
    let mut targets: Vec<(PathBuf, String, Vec<DirEntryInfo>)> = Vec::new();
    for rel in patterns {
        let abs = join_native(&root, rel);
        if !exists_dir(&abs) {
            continue;
        }
        collect_plugin_targets(&abs, rel, 0, &mut targets);
    }
    if targets.is_empty() {
        return Vec::new();
    }

    // The expensive half: one recursive walk per plugin directory. Parallel,
    // order-preserving (collect on an IndexedParallelIterator keeps input
    // order, which dedupe depends on).
    let described: Vec<Vec<Value>> = targets
        .par_iter()
        .map(|(dir, label, entries)| describe_plugin_dir(dir, ctx, label, entries))
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for group in described {
        for asset in group {
            let key = asset
                .get("meta")
                .and_then(|m| m.get("manifest"))
                .filter(|v| js_truthy(v))
                .map(js_string)
                .or_else(|| asset.get("dir").map(js_string))
                .unwrap_or_default()
                .to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            out.push(asset);
        }
    }
    out
}

fn describe_plugin_dir(
    dir: &Path,
    ctx: &Ctx,
    rel_label: &str,
    entries: &[DirEntryInfo],
) -> Vec<Value> {
    let mut out = Vec::new();
    let manifests = manifest_candidates_in(dir, entries);

    let components: Vec<&str> = [
        "skills",
        "agents",
        "hooks",
        "mcp",
        ".codebuddy-plugin",
        ".claude-plugin",
        "commands",
    ]
    .iter()
    .filter(|s| child_is_dir(dir, entries, s))
    .copied()
    .collect();

    let version_dirs: Vec<String> = entries
        .iter()
        .filter(|e| {
            e.is_dir
                && e.name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
        })
        .map(|e| e.name.clone())
        .collect();

    struct Target {
        dir: PathBuf,
        manifest: Option<PathBuf>,
        version: Option<String>,
    }
    let targets: Vec<Target> = if !manifests.is_empty() {
        vec![Target {
            dir: dir.to_path_buf(),
            manifest: Some(manifests[0].clone()),
            version: None,
        }]
    } else {
        version_dirs
            .iter()
            .map(|v| {
                let vd = dir.join(v);
                Target {
                    dir: vd.clone(),
                    manifest: manifest_candidates(&vd).into_iter().next(),
                    version: Some(v.clone()),
                }
            })
            .collect()
    };

    for t in targets {
        let data = match &t.manifest {
            Some(m) => read_json_safe(m),
            None => json!({}),
        };
        let name = match data.get("name") {
            Some(v) if js_truthy(v) => js_string(v),
            _ => file_name(&t.dir),
        };
        let walk = walk_files(&t.dir);
        let size = walk.bytes;
        let mtime = mtime_of(&t.dir);

        // Nested components. The `exists_file` guard mirrors the reference code,
        // which means directories never reach the isDirectory branch.
        let mut nested: Vec<Value> = Vec::new();
        for sd in ["skills", "agents"] {
            let sp = t.dir.join(sd);
            if !exists_dir(&sp) {
                continue;
            }
            for e in read_entries(&sp) {
                let f = sp.join(&e.name);
                if !exists_file(&f) {
                    continue;
                }
                if e.is_dir {
                    let mf = f.join(SKILL_MANIFEST);
                    if exists_file(&mf) {
                        nested.push(json!({
                            "name": file_name(&f),
                            "file": norm_str(&mf),
                            "type": "skill"
                        }));
                    }
                } else if e.name.to_lowercase().ends_with(".md") {
                    nested.push(json!({
                        "name": basename_strip(&e.name, ".md"),
                        "file": norm_str(&f),
                        "type": if sd == "agents" { "agent" } else { "prompt" }
                    }));
                }
            }
        }

        let version = data
            .get("version")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .or_else(|| t.version.clone());

        let is_marketplace = {
            let flag = ["plugins", "owner", "metadata"]
                .iter()
                .any(|k| data.get(*k).map(js_truthy).unwrap_or(false));
            flag || data.get("name").map(js_truthy).unwrap_or(false)
                && data
                    .get("name")
                    .map(js_string)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("marketplace")
        };
        let listings: Value = match data.get("plugins") {
            Some(Value::Array(a)) => json!(a.len()),
            _ => Value::Null,
        };

        let origin = plugin_origin(rel_label, is_marketplace);
        // `meta.listings` only exists on marketplace entries (the reference code
        // assigns it inside the isMarketplace branch).
        let mut meta = json!({
            "version": version.clone().map(Value::from).unwrap_or(Value::Null),
            "relLabel": rel_label,
            "manifest": t.manifest.as_ref().map(|p| Value::from(norm_str(p))).unwrap_or(Value::Null),
            "manifestData": data,
            "components": components,
            "skillCount": nested.iter().filter(|n| n.get("type").and_then(|t| t.as_str()) == Some("skill")).count(),
            "agentCount": nested.iter().filter(|n| n.get("type").and_then(|t| t.as_str()) == Some("agent")).count(),
            "nested": nested,
            "marketplace": rel_label
        });
        if let Value::Object(m) = &mut meta {
            // `isMarketplace` is always present; `listings` only on marketplace
            // entries — that asymmetry is in the reference and the renderer relies on it.
            m.insert("isMarketplace".to_string(), json!(is_marketplace));
            if is_marketplace {
                m.insert("listings".to_string(), listings.clone());
            }
        }
        let description = if is_marketplace {
            format!(
                "Marketplace · {} listings",
                listings
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )
        } else {
            data.get("description").map(js_string).unwrap_or_default()
        };

        out.push(make_asset(NewAsset {
            kind: "plugin",
            name: name.clone(),
            agent_id: ctx.agent_id.clone(),
            agent_label: ctx.agent_label.clone(),
            scope: ctx.scope.to_string(),
            project_id: ctx.project_id.clone(),
            project_name: ctx.project_name.clone(),
            root: Some(ctx.root.clone()),
            dir: Some(norm_str(&t.dir)),
            file: Some(match &t.manifest {
                Some(m) => norm_str(m),
                None => norm_str(&t.dir.join(SKILL_MANIFEST)),
            }),
            files: walk
                .files
                .iter()
                .map(|f| norm_str(&join_native(&t.dir, f)))
                .collect(),
            format: Some("dir".to_string()),
            size,
            mtime,
            fingerprint: Some(hash16(&format!(
                "{}|{}|{}",
                name,
                version.unwrap_or_default(),
                walk.files.len()
            ))),
            meta,
            description,
            tags: match data.get("keywords").filter(|v| js_truthy(v)) {
                Some(_) => tags_from(data.get("keywords")),
                None => tags_of(&data),
            },
            shared: false,
            writable: Some(false), // plugin cache is host-owned
            origin: Some(origin.to_string()),
            issues: Vec::new(),
            ..Default::default()
        }));
    }
    out
}

fn read_json_safe(path: &Path) -> Value {
    let raw = read_all(path);
    serde_json::from_str(&strip_json_comments(&raw)).unwrap_or(json!({}))
}

/* ------------------------------------------------------------------ */
/* dedupe + effective view                                            */
/* ------------------------------------------------------------------ */

fn str_field(a: &Value, key: &str) -> Option<String> {
    a.get(key)
        .filter(|v| js_truthy(v))
        .map(js_string)
        .filter(|s| !s.is_empty())
}

fn dedupe_by_physical_path(assets: Vec<Value>) -> Vec<Value> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Value> = Vec::new();
    let mut real_cache: HashMap<String, String> = HashMap::new();

    fn physical(p: &str, cache: &mut HashMap<String, String>) -> String {
        if let Some(hit) = cache.get(p) {
            return hit.clone();
        }
        let rp = fs::canonicalize(p)
            .map(|pb| pb.to_string_lossy().to_string())
            .unwrap_or_else(|_| p.to_string());
        cache.insert(p.to_string(), rp.clone());
        rp
    }

    for a in assets {
        let kind = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let primary = str_field(&a, "file")
            .or_else(|| str_field(&a, "dir"))
            .or_else(|| str_field(&a, "id"))
            .unwrap_or_default();
        let key = if kind == "mcp" {
            let config_key = a
                .get("meta")
                .and_then(|m| m.get("configKey"))
                .filter(|v| js_truthy(v))
                .map(js_string)
                .or_else(|| str_field(&a, "name"))
                .unwrap_or_default();
            format!(
                "{}#{}",
                physical(&primary, &mut real_cache)
                    .replace('\\', "/")
                    .to_lowercase(),
                config_key.to_lowercase()
            )
        } else if kind == "plugin" {
            let d = str_field(&a, "dir")
                .or_else(|| str_field(&a, "file"))
                .unwrap_or_default();
            format!(
                "dir:{}",
                physical(&d, &mut real_cache)
                    .replace('\\', "/")
                    .to_lowercase()
            )
        } else {
            format!(
                "file:{}",
                physical(&primary, &mut real_cache)
                    .replace('\\', "/")
                    .to_lowercase()
            )
        };

        if let Some(&idx) = seen.get(&key) {
            let entry = json!({
                "agentId": a.get("agentId").cloned().unwrap_or(Value::Null),
                "rel": a.get("meta").and_then(|m| m.get("relLabel")).cloned().unwrap_or(Value::Null)
            });
            if let Value::Object(outer) = &mut out[idx] {
                let meta = outer.entry("meta".to_string()).or_insert_with(|| json!({}));
                if let Value::Object(m) = meta {
                    let also = m.entry("alsoSeen".to_string()).or_insert_with(|| json!([]));
                    if let Value::Array(arr) = also {
                        arr.push(entry);
                    }
                }
            }
            continue;
        }
        seen.insert(key, out.len());
        out.push(a);
    }
    out
}

/// `computeEffective` — project scope shadows global scope for the same
/// agent; copies belonging to different agents all stay live.
fn compute_effective(assets: &[Value]) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Value> = HashMap::new();

    for a in assets {
        let id = a.get("id").map(js_string).unwrap_or_default();
        let mut cloned = a.clone();
        if let Value::Object(m) = &mut cloned {
            m.insert(
                "effective".to_string(),
                json!({"shadowedBy": [], "overrides": [], "live": true}),
            );
        }
        if !map.contains_key(&id) {
            order.push(id.clone());
        }
        map.insert(id, cloned);
    }

    fn get<'a>(map: &'a HashMap<String, Value>, id: &str) -> &'a Value {
        map.get(id).expect("id came from the map")
    }
    fn agent_of(map: &HashMap<String, Value>, id: &str) -> Option<String> {
        get(map, id).get("agentId").map(js_string)
    }
    fn scope_of(map: &HashMap<String, Value>, id: &str) -> String {
        get(map, id)
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    // Group by `kind:slug(name)`, keeping first-sighting order.
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut group_index: HashMap<String, usize> = HashMap::new();
    for id in &order {
        let a = get(&map, id);
        let kind = a.get("kind").map(js_string).unwrap_or_default();
        let name = a.get("name").map(js_string).unwrap_or_default();
        let key = format!("{}:{}", kind, slug(&name));
        match group_index.get(&key) {
            Some(&i) => groups[i].1.push(id.clone()),
            None => {
                group_index.insert(key.clone(), groups.len());
                groups.push((key, vec![id.clone()]));
            }
        }
    }

    for (_, ids) in groups {
        if ids.len() < 2 {
            continue;
        }
        let projects: Vec<String> = ids
            .iter()
            .filter(|id| scope_of(&map, id) == "project")
            .cloned()
            .collect();
        let globals: Vec<String> = ids
            .iter()
            .filter(|id| scope_of(&map, id) == "global")
            .cloned()
            .collect();

        for g in &globals {
            let agent = agent_of(&map, g);
            let shadows: Vec<String> = projects
                .iter()
                .filter(|p| agent_of(&map, p) == agent)
                .cloned()
                .collect();
            if !shadows.is_empty() {
                if let Some(Value::Object(m)) = map.get_mut(g) {
                    m.insert(
                        "effective".to_string(),
                        json!({"shadowedBy": shadows, "overrides": [], "live": false}),
                    );
                }
            }
        }
        for p in &projects {
            let agent = agent_of(&map, p);
            let overrides: Vec<String> = globals
                .iter()
                .filter(|g| agent_of(&map, g) == agent)
                .cloned()
                .collect();
            if let Some(Value::Object(m)) = map.get_mut(p) {
                m.insert(
                    "effective".to_string(),
                    json!({"shadowedBy": [], "overrides": overrides, "live": true}),
                );
            }
        }
    }

    order
        .iter()
        .map(|id| map.get(id).cloned().unwrap_or(Value::Null))
        .collect()
}

/* ------------------------------------------------------------------ */
/* summary + payload                                                  */
/* ------------------------------------------------------------------ */

pub fn summarize(assets: &[Value]) -> Value {
    let mut by: Map<String, Value> = Map::new();
    for k in ["skill", "mcp", "plugin", "rule", "memory"] {
        by.insert(k.to_string(), json!(0));
    }
    let mut by_origin: Map<String, Value> = Map::new();
    for k in ["user", "managed", "cached", "marketplace"] {
        by_origin.insert(k.to_string(), json!(0));
    }
    let mut issues = 0u64;
    let mut shadowed = 0u64;
    let mut bytes = 0u64;
    let mut tokens = 0u64;

    for a in assets {
        let kind = a.get("kind").map(js_string).unwrap_or_default();
        let cur = by.get(&kind).and_then(|v| v.as_u64()).unwrap_or(0);
        by.insert(kind, json!(cur + 1));

        let origin = a
            .get("origin")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "user".to_string());
        let cur = by_origin.get(&origin).and_then(|v| v.as_u64()).unwrap_or(0);
        by_origin.insert(origin, json!(cur + 1));

        if a.get("issues")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .any(|i| i.get("level").and_then(|l| l.as_str()) == Some("error"))
            })
            .unwrap_or(false)
        {
            issues += 1;
        }
        if a.get("effective")
            .and_then(|e| e.get("live"))
            .and_then(|l| l.as_bool())
            == Some(false)
        {
            shadowed += 1;
        }
        let size = a.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        bytes += size;
        tokens += (size + 3) / 4;
    }

    json!({
        "by": by,
        "byOrigin": by_origin,
        "total": assets.len(),
        "issues": issues,
        "shadowed": shadowed,
        "bytes": bytes,
        "estTokens": tokens
    })
}

fn with_issues(mut asset: Value, issues: Vec<Value>) -> Value {
    if let Value::Object(m) = &mut asset {
        m.insert("issues".to_string(), Value::Array(issues));
    }
    asset
}

fn or_null(data: &Value, key: &str) -> Value {
    match data.get(key) {
        Some(v) if js_truthy(v) => v.clone(),
        _ => Value::Null,
    }
}

/// `stripHeavy` — the renderer never sees `raw`, and `effective` is reduced
/// to the three fields the UI reads. Done in place: this runs over ~1000
/// assets holding megabytes of metadata, and the reference version's object spread
/// is exactly the kind of copy this implementation is trying to avoid.
fn shrink_effective(a: &mut Value) {
    let reduced = match a.get("effective") {
        Some(Value::Object(e)) => json!({
            "live": e.get("live").cloned().unwrap_or(json!(true)),
            "shadowedBy": e.get("shadowedBy").cloned().unwrap_or(json!([])),
            "overrides": e.get("overrides").cloned().unwrap_or(json!([]))
        }),
        _ => Value::Null,
    };
    if let Value::Object(m) = a {
        // `stripHeavy` sets `raw: undefined`, which `jsonSafe` then
        // *drops* — so the renderer receives no `raw` key at all. Sending
        // `null` here is not equivalent: it is a different payload. The key
        // must be absent.
        m.remove("raw");
        m.insert("effective".to_string(), reduced);
    }
}

/// Consumes the scan result: the payload is built by moving values into one
/// map, so the 11MB of asset JSON is never copied. (`json!` on a `Value`
/// re-serialises it, which cost ~70ms and a second full copy.)
fn ipc_payload(result: ScanResult) -> Value {
    let mut assets = result.assets;
    // `keeper.scan()` runs the security pass on `res.assets` only, and
    // `computeEffective()` has already cloned them by then — so
    // payload `effective[i].security` is *absent*. Mirroring that exactly is
    // the point: the UI reads `security` from `assets` (Health view) and never
    // from `effective`, and inventing the field here would make the two
    // payloads disagree.
    super::security::security_pass(&mut assets);
    for a in &mut assets {
        shrink_effective(a);
    }
    let mut effective = result.effective;
    for a in &mut effective {
        shrink_effective(a);
    }

    let agents: Vec<Value> = result
        .agents
        .into_iter()
        .map(|a| {
            let mut m = Map::new();
            m.insert("id".to_string(), Value::from(a.id));
            m.insert("label".to_string(), Value::from(a.label));
            m.insert("vendor".to_string(), Value::from(a.vendor));
            m.insert("accent".to_string(), Value::from(a.accent));
            m.insert("home".to_string(), Value::from(a.home));
            m.insert("installed".to_string(), Value::Bool(a.installed));
            // `scanAll` never sets `homeExists` on its agent status, so
            // `jsonSafe` drops the key entirely (`undefined` is skipped, not
            // nulled). Emitting `null` here was a difference the hand-copied
            // mirror in the parity script could not see.
            m.insert(
                "notes".to_string(),
                a.notes.map(Value::from).unwrap_or(Value::Null),
            );
            m.insert("pseudo".to_string(), Value::Bool(a.pseudo));
            Value::Object(m)
        })
        .collect();

    let mut out = Map::new();
    out.insert("assets".to_string(), Value::Array(assets));
    out.insert("effective".to_string(), Value::Array(effective));
    out.insert("agents".to_string(), Value::Array(agents));
    out.insert("projects".to_string(), Value::Array(result.projects));
    out.insert("scannedAt".to_string(), Value::from(result.scanned_at));
    out.insert("summary".to_string(), result.summary);
    let mut out = Value::Object(out);
    // `jsonSafe` caps every array at 500 entries (`value.slice(0, 500)`), which
    // is why a plugin with 718 files reaches the renderer with the first 500.
    // Sending the full list is not "better data" — it is a different payload.
    //
    // The cap applies *per element*: the reference runs `stripHeavy` on each asset
    // separately and the top-level `assets` array never goes through jsonSafe,
    // so capping the whole payload at once would drop 487 of 987 assets.
    if let Value::Object(m) = &mut out {
        for key in ["assets", "effective", "agents", "projects"] {
            if let Some(Value::Array(items)) = m.get_mut(key) {
                for it in items.iter_mut() {
                    cap_arrays(it, 500);
                }
            }
        }
    }
    out
}

/// Recursively truncate every array to `cap` entries, mutating in place.
fn cap_arrays(v: &mut Value, cap: usize) {
    match v {
        Value::Array(items) => {
            if items.len() > cap {
                items.truncate(cap);
            }
            for it in items.iter_mut() {
                cap_arrays(it, cap);
            }
        }
        Value::Object(map) => {
            for it in map.values_mut() {
                cap_arrays(it, cap);
            }
        }
        _ => {}
    }
}

/// Same scan as [`scan_payload`] but WITHOUT publishing the result to the
/// process-global read cache.
///
/// `Keeper::index_get()` uses this for its cold-cache fallback. That fallback
/// exists to answer "what does this id point at" and nothing else; publishing
/// its result globally made a lookup silently overwrite the cache every other
/// caller relies on — which is how a keeper test could pass alone and fail
/// under `cargo test` depending on interleaving.
pub fn scan_payload_uncached(
    user_home: &str,
    projects: &[Value],
) -> (Value, Vec<(&'static str, u128)>) {
    let result = scan_all(user_home, projects);
    let timings = result.timings.clone();
    (ipc_payload(result), timings)
}

/// Public entry point used by both the Tauri command and the parity CLI.
pub fn scan_payload(user_home: &str, projects: &[Value]) -> (Value, Vec<(&'static str, u128)>) {
    let result = scan_all(user_home, projects);
    let timings = result.timings.clone();
    let payload = ipc_payload(result);
    // Feed the read-side cache with exactly what the renderer is about to get,
    // so `read_asset` / `baseline_diff` do not need a second scan.
    if let (Some(all), Some(effective)) = (
        payload.get("assets").and_then(|v| v.as_array()),
        payload.get("effective").and_then(|v| v.as_array()),
    ) {
        super::assets::remember(all, effective);
    }
    (payload, timings)
}

/* ------------------------------------------------------------------ */
/* orchestration                                                      */
/* ------------------------------------------------------------------ */

fn scan_agent_globals(agent: &Agent, home: &str) -> Vec<Value> {
    let ctx = Ctx::global(agent, home);
    let root = PathBuf::from(home);
    let mut out = Vec::new();

    let first = agent.globals.skills.first().copied().unwrap_or("skills");
    out.extend(scan_skills(&join_native(&root, first), &ctx));
    for p in agent.globals.skills {
        if p.contains('/') {
            out.extend(scan_skills(&join_native(&root, p), &ctx));
        }
    }
    if !agent.globals.rules.is_empty() {
        out.extend(scan_rules(agent.globals.rules, &ctx));
    }
    if !agent.globals.memory.is_empty() {
        out.extend(scan_memory(agent.globals.memory, &ctx));
    }
    if !agent.globals.mcp.is_empty() {
        out.extend(scan_mcp(agent.globals.mcp, &ctx));
    }
    if !agent.globals.plugins.is_empty() {
        out.extend(scan_plugins(agent.globals.plugins, &ctx));
    }
    out
}

pub struct AgentStatus {
    id: String,
    label: String,
    vendor: String,
    accent: String,
    home: String,
    installed: bool,
    notes: Option<String>,
    pseudo: bool,
}

pub struct ScanResult {
    pub assets: Vec<Value>,
    pub effective: Vec<Value>,
    pub agents: Vec<AgentStatus>,
    pub projects: Vec<Value>,
    pub scanned_at: String,
    pub summary: Value,
    pub timings: Vec<(&'static str, u128)>,
}

/// `scanAll({store, projects})`. `user_home` replaces `os.homedir()`, and the
/// project list comes from the same `projects.json` the reference registry writes.
pub fn scan_all(user_home: &str, projects: &[Value]) -> ScanResult {
    let mut timings: Vec<(&'static str, u128)> = Vec::new();
    let t_agents = Instant::now();

    // Status is cheap (17 stats) and must stay in declaration order.
    let mut statuses: Vec<AgentStatus> = Vec::new();
    let mut homes: Vec<Option<String>> = Vec::new();
    for agent in AGENTS {
        let home = resolve_home(agent.home, user_home);
        let installed = home
            .as_ref()
            .map(|h| exists_dir(Path::new(h)))
            .unwrap_or(false);
        statuses.push(AgentStatus {
            id: agent.id.to_string(),
            label: agent.label.to_string(),
            vendor: agent.vendor.to_string(),
            accent: agent.accent.to_string(),
            home: home.clone().unwrap_or_default(),
            installed,
            notes: agent.notes.map(|s| s.to_string()),
            pseudo: false,
        });
        homes.push(home);
    }
    timings.push(("agents", t_agents.elapsed().as_millis()));

    // Per-agent scan in parallel; `collect` preserves declaration order so
    // dedupe keeps the same "first sighting wins" result as the reference build.
    let t_scan = Instant::now();
    let per_agent: Vec<Vec<Value>> = AGENTS
        .par_iter()
        .zip(homes.par_iter())
        .map(|(agent, home)| match home {
            Some(h) if exists_dir(Path::new(h)) => scan_agent_globals(agent, h),
            _ => Vec::new(),
        })
        .collect();
    let mut raw: Vec<Value> = Vec::new();
    for group in per_agent {
        raw.extend(group);
    }
    timings.push(("scan_agents", t_scan.elapsed().as_millis()));

    // Shared Agent Skills standard directory, reported as a pseudo-agent.
    let t_shared = Instant::now();
    if let Some(shared_skills) = resolve_home(SHARED_SKILLS_DIR, user_home) {
        let shared_home = resolve_home("~/.agents", user_home).unwrap_or_default();
        if exists_dir(Path::new(&shared_skills)) {
            let ctx = Ctx {
                agent_id: "shared".to_string(),
                agent_label: "Shared (~/.agents)".to_string(),
                scope: "global",
                project_id: None,
                project_name: None,
                root: shared_home.clone(),
                shared: true,
            };
            raw.extend(scan_skills(Path::new(&shared_skills), &ctx));
            statuses.push(AgentStatus {
                id: "shared".to_string(),
                label: "Shared Agent Skills".to_string(),
                vendor: "agents.md standard".to_string(),
                accent: "#64748b".to_string(),
                home: shared_home,
                installed: true,
                notes: None,
                pseudo: true,
            });
        }
    }
    timings.push(("shared", t_shared.elapsed().as_millis()));

    // Projects.
    let t_projects = Instant::now();
    let mut projects_out: Vec<Value> = Vec::new();
    for proj in projects {
        let dir = proj.get("dir").map(js_string).unwrap_or_default();
        if dir.is_empty() || !exists_dir(Path::new(&dir)) {
            continue;
        }
        let mut counts: HashMap<&str, u64> = HashMap::new();
        let mut matched: Vec<String> = Vec::new();
        let mut project_raw: Vec<Value> = Vec::new();

        for agent in AGENTS {
            let proj_root = PathBuf::from(&dir);
            let any_hit = exists_dir(&join_native(&proj_root, agent.projects.marker))
                || agent
                    .projects
                    .skills
                    .iter()
                    .any(|p| exists_dir(&join_native(&proj_root, p)))
                || agent
                    .projects
                    .rules
                    .iter()
                    .any(|p| exists_file(&join_native(&proj_root, p)));
            if !any_hit {
                continue;
            }
            matched.push(agent.id.to_string());
            let ctx = Ctx {
                agent_id: agent.id.to_string(),
                agent_label: agent.label.to_string(),
                scope: "project",
                project_id: proj.get("id").map(js_string),
                project_name: proj.get("name").map(js_string),
                root: dir.clone(),
                shared: false,
            };
            let mut found: Vec<Value> = Vec::new();
            for p in agent.projects.skills {
                found.extend(scan_skills(&join_native(&proj_root, p), &ctx));
            }
            if !agent.projects.rules.is_empty() {
                found.extend(scan_rules(agent.projects.rules, &ctx));
            }
            if !agent.projects.memory.is_empty() {
                found.extend(scan_memory(agent.projects.memory, &ctx));
            }
            if !agent.projects.mcp.is_empty() {
                found.extend(scan_mcp(agent.projects.mcp, &ctx));
            }
            if let Some(plugins) = agent.projects.plugins {
                found.extend(scan_plugins(plugins, &ctx));
            }
            project_raw.extend(found);
        }

        let assets = dedupe_by_physical_path(project_raw);
        for a in &assets {
            let k = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            *counts
                .entry(match k {
                    "skill" => "skill",
                    "mcp" => "mcp",
                    "plugin" => "plugin",
                    "rule" => "rule",
                    "memory" => "memory",
                    _ => "other",
                })
                .or_insert(0) += 1;
        }
        raw.extend(assets);

        let mut out = proj.clone();
        if let Value::Object(m) = &mut out {
            m.insert(
                "counts".to_string(),
                json!({
                    "skill": counts.get("skill").copied().unwrap_or(0),
                    "mcp": counts.get("mcp").copied().unwrap_or(0),
                    "plugin": counts.get("plugin").copied().unwrap_or(0),
                    "rule": counts.get("rule").copied().unwrap_or(0),
                    "memory": counts.get("memory").copied().unwrap_or(0)
                }),
            );
            m.insert("agents".to_string(), json!(matched));
        }
        projects_out.push(out);
    }
    timings.push(("projects", t_projects.elapsed().as_millis()));

    let t_dedupe = Instant::now();
    let assets = dedupe_by_physical_path(raw);
    timings.push(("dedupe", t_dedupe.elapsed().as_millis()));

    let t_eff = Instant::now();
    let effective = compute_effective(&assets);
    let summary = summarize(&effective);
    timings.push(("effective", t_eff.elapsed().as_millis()));

    ScanResult {
        assets,
        effective,
        agents: statuses,
        projects: projects_out,
        scanned_at: iso_from_millis(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        ),
        summary,
        timings,
    }
}

/// Read the project registry the reference store writes (`projects.json`).
pub fn read_projects(store_root: &str) -> Vec<Value> {
    let path = Path::new(store_root).join("projects.json");
    let raw = read_all(&path);
    let doc: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    doc.get("projects")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}
