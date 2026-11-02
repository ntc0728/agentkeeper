//! 共享页（M6）的写入编排：收敛应用、健康发现的一键修、库位置迁移。
//!
//! 本模块只做「把已有原语编排成一次用户动作」：
//! * `apply_converge` 复用 M2 的 `keeper.enable_asset`（每个 skill 一个事务）；
//! * `apply_finding_fix` 按 `doctor` 的 code 分派到已有的链接 / 台账 / MCP 原语；
//! * `migrate_library` 按 §9.3 的台账 journal 做可断点续跑的库根迁移。
//!
//! 所有会写盘的函数都返回 `{ ok, written, … }` 或 `{ ok:false, code }`，
//! 不在 Rust 侧拼英文长句（中文由 `engine-zh.js` 渲染）。
//!
//! ## 服务端复核
//!
//! `apply_finding_fix` 拿到的 `detail` 只当**索引**用（例如 `name` / `slug`）：
//! 来源路径、内容一致性、目标是否还是链接等一律重新从磁盘算。客户端上一次扫描
//! 的结果可能已经过期，而按过期事实删掉一个真实目录是不可逆的。
//!
//! ## 写入边界
//!
//! 用户可见的目录 / 文件（链接、副本、MCP 配置、库内记录）一律走
//! [`crate::store::Store::transaction`]；台账 `links.json` 与迁移 journal 走
//! [`crate::library::Library::save_ledger`] 的原子写 —— 它是元数据，不是用户内容，
//! 且必须在事务成功**之后**才更新（见 `keeper::record_link` 的同一立场）。
//! `migrate_library` 是唯一绕过事务的共享写路径，理由写在函数注释里。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapters;
use crate::doctor;
use crate::keeper::Keeper;
use crate::library::{self, Ledger, Library, LinkEntry, Migrating};
use crate::linkfs::{self, LinkMethod};
use crate::model::{basename, slug};
use crate::scan;
use crate::settings;
use crate::sharing::{self, Verdict};
use crate::store::{self, Op};

/* ------------------------------------------------------------------ */
/* 小工具                                                              */
/* ------------------------------------------------------------------ */

/// 取一个非空字符串字段。
fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 统一的失败信封：只给稳定 code，不拼英文句子。
fn fail(code: &str) -> Value {
    json!({ "ok": false, "code": code })
}

/// 带 detail 的失败信封。
fn fail_with(code: &str, detail: Value) -> Value {
    json!({ "ok": false, "code": code, "detail": detail })
}

/// 绝对路径的原生分隔符形式。
fn native(p: &str) -> String {
    store::native_str(Path::new(p))
}

/// 两个路径是否同一个字面路径（大小写不敏感，词法归一）。
fn paths_equal(a: &str, b: &str) -> bool {
    store::normalize_path(a).eq_ignore_ascii_case(&store::normalize_path(b))
}

/// 事务是否成功。
fn tx_ok(tx: &Value) -> bool {
    tx.get("ok").and_then(Value::as_bool).unwrap_or(false)
}

/// 事务失败时补一个 `code`（取自 `errorCode`），让共享页可以像其它命令一样按
/// `code` 查中文；事务自己的 `error` / `restoreErrors` 原样保留。
fn tx_failure(mut tx: Value) -> Value {
    if let Value::Object(m) = &mut tx {
        if !m.contains_key("code") {
            if let Some(code) = m.get("errorCode").and_then(Value::as_str) {
                let code = code.to_string();
                m.insert("code".to_string(), json!(code));
            }
        }
    }
    tx
}

/// 事务应答里的 `written`（字符串数组）；没有就空。
fn tx_written(tx: &Value) -> Vec<String> {
    tx.get("written")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 按小写去重，保持首次出现的顺序。
fn unique(items: Vec<String>) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    items
        .into_iter()
        .filter(|s| seen.insert(s.to_lowercase()))
        .collect()
}

/// 给目录链接选偏好（hardlink 不能用于目录）。
fn preferred_dir(keeper: &Keeper) -> Option<LinkMethod> {
    match keeper.link_prefer() {
        Some(LinkMethod::Symlink) => Some(LinkMethod::Symlink),
        Some(LinkMethod::Junction) => Some(LinkMethod::Junction),
        _ => None,
    }
}

/// 给 `Op::link_*` 选一种适配目标的偏好：目录不接受 hardlink，文件不接受 junction。
fn preferred_for(keeper: &Keeper, target: &Path) -> Option<LinkMethod> {
    let is_dir = target.is_dir();
    if is_dir {
        return preferred_dir(keeper);
    }
    match keeper.link_prefer() {
        Some(LinkMethod::Symlink) => Some(LinkMethod::Symlink),
        Some(LinkMethod::Hardlink) => Some(LinkMethod::Hardlink),
        _ => None,
    }
}

/// 建一条指向 `src` 的链接（目录走 linkDir，文件走 linkFile）。
fn link_op(dst: &str, src: &str, prefer: Option<LinkMethod>) -> Op {
    if Path::new(src).is_dir() {
        Op::link_dir_with(dst, src, prefer)
    } else {
        Op::link_file_with(dst, src, prefer)
    }
}

/// 两个路径内容是否相同：目录逐文件摘要，文件逐字节。
///
/// 「读不全」一律算不同 —— 自动删掉一个读不全的真实副本，可能把用户唯一的一份
/// 内容删掉。
fn same_content(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.is_dir() != mb.is_dir() {
        return false;
    }
    if ma.is_dir() {
        let da = sharing::digest_dir(a);
        let db = sharing::digest_dir(b);
        da.unreadable == 0 && db.unreadable == 0 && da.digest == db.digest && da.bytes == db.bytes
    } else {
        match (std::fs::read(a), std::fs::read(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// 把修复结果记进台账。失败只作为 `ledgerWarning` 返回，不让一次已成功的磁盘
/// 修复看起来失败（台账是索引，可以重建）。
fn ledger_warning(
    lib: &Library,
    kind: &str,
    name: &str,
    src: &str,
    dst: &str,
) -> Option<String> {
    let method = linkfs::link_kind(Path::new(dst)).unwrap_or(LinkMethod::Symlink);
    let mut ledger = match lib.load_ledger() {
        Ok(l) => l,
        Err(e) => return Some(e),
    };
    lib.record(&mut ledger, LinkEntry::new(kind, name, src, dst, method));
    lib.save_ledger(&ledger).err()
}

/* ------------------------------------------------------------------ */
/* 只读命令                                                            */
/* ------------------------------------------------------------------ */

/// 扫描一次并返回资产数组（`payload["assets"]`）。M6 的只读预览与写入编排共用。
///
/// 每次调用都现扫：写入编排必须基于盘上的**当前**事实，不能复用渲染层手里的
/// 上一份方案。扫描结果同时喂给 `assets` 缓存（`scan::scan_payload` 的既有行为），
/// 所以后续 `enable_asset(assetId)` 能解析到同一批 id。
pub fn assets_of(keeper: &Keeper) -> Vec<Value> {
    let projects = scan::read_projects(keeper.store.root_str());
    let (payload, _timings) = scan::scan_payload(&keeper.user_home, &projects);
    payload
        .get("assets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `plan_converge` 命令：只读的收敛方案。
///
/// 外层补契约要求的 `ok` / `actionable`；组内字段原样来自
/// [`sharing::plan_converge`]（名字一旦漂移，`view-shared.js` 就会读不到）。
pub fn converge_plan(keeper: &Keeper) -> Value {
    let lib = keeper.library();
    let cap = lib.capability(keeper.store.root_str());
    let assets = assets_of(keeper);
    let plan = sharing::plan_converge(&assets, &lib, &cap);
    let mut out = plan.to_json();
    if let Value::Object(m) = &mut out {
        m.insert("ok".to_string(), json!(true));
        m.insert("actionable".to_string(), json!(plan.actionable()));
    }
    out
}

/// `shared_doctor` 命令：只读健康检查。
pub fn doctor_report(keeper: &Keeper) -> Value {
    let lib = keeper.library();
    let assets = assets_of(keeper);
    doctor::diagnose(&lib, &assets, &keeper.user_home, None).to_json()
}

/* ------------------------------------------------------------------ */
/* 收敛应用                                                            */
/* ------------------------------------------------------------------ */

/// `apply_converge` 命令：按组应用收敛方案。
///
/// 入参：`{ groups?: [slug], only?: [slug], all?: bool }`。
/// * 明确点名（`groups` / `only`）时按 slug 过滤，允许 `single` 组（把唯一真实
///   副本纳入库）；
/// * 没点名且 `all:true` 时只取 `identical` 组 —— 收敛向导的「全选可动项」；
/// * 什么都没给 = 零写入（写命令不该自己猜用户想动哪一组）。
///
/// `diverged` 组一律拒绝（组级 `converge.diverged`，该组零写入）。每个组独立走
/// [`Keeper::enable_asset`]：一组失败不阻断其它组。
pub fn apply_converge(keeper: &Keeper, input: &Value) -> Value {
    let lib = keeper.library();
    let cap = lib.capability(keeper.store.root_str());
    let assets = assets_of(keeper);
    // 服务端重算：客户端的方案只用来选组，路径与裁决一律以这次扫描为准。
    let plan = sharing::plan_converge(&assets, &lib, &cap);

    let named: Vec<String> = {
        let mut out: Vec<String> = Vec::new();
        for key in ["groups", "only"] {
            if let Some(items) = input.get(key).and_then(Value::as_array) {
                for item in items {
                    if let Some(s) = item.as_str().map(|s| s.trim().to_string()) {
                        if !s.is_empty() && !out.contains(&s) {
                            out.push(s);
                        }
                    }
                }
            }
        }
        out
    };
    let all = input.get("all").and_then(Value::as_bool).unwrap_or(false);
    if named.is_empty() && !all {
        return json!({ "ok": true, "written": [], "groups": [], "failed": 0 });
    }

    let mut written: Vec<String> = Vec::new();
    let mut groups_out: Vec<Value> = Vec::new();
    let mut failed = 0usize;
    let mut handled: HashSet<String> = HashSet::new();

    for group in &plan.groups {
        let selected = if named.is_empty() {
            all && group.verdict == Verdict::Identical
        } else {
            named.iter().any(|n| n == &group.name)
        };
        if !selected {
            continue;
        }
        handled.insert(group.name.clone());
        let result = apply_group(keeper, &lib, &assets, group);
        if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            written.extend(
                result
                    .get("written")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default(),
            );
        } else {
            failed += 1;
        }
        groups_out.push(result);
    }

    // 点名了但方案里没有这一组：可能是并行写入后它已经不是「多份真实副本」，
    // 也可能是客户端拿了个旧名字。两者都只能报「不可收敛」，不猜路径。
    for name in &named {
        if handled.contains(name) {
            continue;
        }
        failed += 1;
        groups_out.push(json!({
            "name": name,
            "ok": false,
            "code": "converge.notConvergeable",
            "written": [],
        }));
    }

    json!({
        "ok": failed == 0,
        "written": unique(written),
        "groups": groups_out,
        "failed": failed,
    })
}

/// 收敛一组。组级结果包含：组名、是否成功、写过的路径、失败时的稳定 code 或
/// 事务原文。
fn apply_group(keeper: &Keeper, lib: &Library, assets: &[Value], group: &sharing::ConvergeGroup) -> Value {
    let group_name = group.name.clone();
    if group.verdict == Verdict::Diverged {
        return json!({
            "name": group_name,
            "ok": false,
            "code": "converge.diverged",
            "written": [],
        });
    }
    let missing_writes = || {
        json!({
            "name": group_name.clone(),
            "ok": false,
            "code": "converge.notConvergeable",
            "written": [],
        })
    };

    let mut written: Vec<String> = Vec::new();
    let mut kept: Option<String> = None;

    // 库内还没有这个 slug：第一份必须按扫描资产 id 搬进库 —— 只有它知道来源路径，
    // 而 `enable_asset` 的名字路径要求「库里已经有一份」。
    if !lib.skill_dir(&group.name).is_dir() {
        let Some(keeper_path) = group.keeper.clone() else {
            return missing_writes();
        };
        let Some(node) = group
            .copies
            .iter()
            .find(|c| paths_equal(&c.path, &keeper_path))
        else {
            return missing_writes();
        };
        let Some(asset_id) = asset_id_for(assets, &group.name, &keeper_path) else {
            // 方案与资产表对不上（扫描后又被写过）。不猜 assetId。
            return missing_writes();
        };
        let tx = keeper.enable_asset(&json!({
            "assetId": asset_id,
            "agentId": node.agent_id,
            "scope": node.scope,
            "projectId": node.project_id,
        }));
        if !tx_ok(&tx) {
            return group_error(&group_name, &tx, written);
        }
        written.extend(tx_written(&tx));
        kept = Some(keeper_path);
    }

    // 其余副本：库内已有主副本，按 `{name, kind:"skill", agentId, scope}` 补链接。
    for node in &group.replace {
        if let Some(k) = &kept {
            if paths_equal(&node.path, k) {
                // `single` 组的 replace 就是保留方自己：搬库那一步已经处理过它。
                continue;
            }
        }
        let tx = keeper.enable_asset(&json!({
            "name": group.name,
            "kind": "skill",
            "agentId": node.agent_id,
            "scope": node.scope,
            "projectId": node.project_id,
        }));
        if !tx_ok(&tx) {
            return group_error(&group_name, &tx, written);
        }
        written.extend(tx_written(&tx));
    }

    // 「重算后这一组没有任何可写的位置」不是成功：没有写入的收敛等于什么都没做，
    // 必须给一个明确 code（`converge.notConvergeable`），而不是一个既无 error 也
    // 无 code 的 `ok:false` / 静默的 `ok:true`。典型触发：默认库根下库内只有一份，
    // 它被扫描成伪 agent `shared` 的资产；该组是 `single` 且 `replace` 为空。
    if written.is_empty() {
        return missing_writes();
    }

    json!({
        "name": group_name,
        "ok": true,
        "written": unique(written),
        "error": Value::Null,
    })
}

/// 组失败：带上事务的稳定 `errorCode`；事务只给了英文 `error` 时，code 回落成
/// `converge.notConvergeable` —— 界面按 code 找中文，不能靠读英文句子，所以这里
/// **不把英文原文**塞进应答（它已经在 `audit.jsonl` 里）。
fn group_error(name: &str, tx: &Value, written: Vec<String>) -> Value {
    let code = tx
        .get("errorCode")
        .and_then(Value::as_str)
        .unwrap_or("converge.notConvergeable");
    json!({
        "name": name,
        "ok": false,
        "code": code,
        "written": unique(written),
    })
}

/// 在扫描资产里按 `(slug, dir)` 找一条 skill 的 assetId。
fn asset_id_for(assets: &[Value], slug_name: &str, path: &str) -> Option<String> {
    assets.iter().find_map(|a| {
        if a.get("kind").and_then(Value::as_str) != Some("skill") {
            return None;
        }
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
        if slug(name) != slug_name {
            return None;
        }
        let dir = a.get("dir").and_then(Value::as_str)?;
        if !paths_equal(dir, path) {
            return None;
        }
        a.get("id").and_then(Value::as_str).map(|s| s.to_string())
    })
}

/* ------------------------------------------------------------------ */
/* 健康发现的一键修                                                     */
/* ------------------------------------------------------------------ */

/// `apply_finding_fix` 命令：修一条 finding。
///
/// 入参：`{ code, path, fix?, detail?, choice? }`。`detail` 只作索引；所有判据都
/// 重新从磁盘 / 台账算。`fix` 给了但与该 code 的修法不符时回
/// `doctor.unsupportedFix`，而不是按一个不认识的动作去写盘。
pub fn apply_finding_fix(keeper: &Keeper, input: &Value) -> Value {
    let code = text(input, "code").unwrap_or_default();
    let path = text(input, "path").unwrap_or_default();
    let fix = text(input, "fix").unwrap_or_default();
    let detail = input.get("detail").cloned().unwrap_or_else(|| json!({}));
    match code.as_str() {
        doctor::LINK_DANGLING => fix_dangling(keeper, &path, &detail, &fix),
        doctor::LINK_HIJACKED => fix_hijacked(keeper, &path, &detail, &fix),
        doctor::LINK_LEDGER_DRIFT => fix_ledger_drift(keeper, &fix),
        doctor::LINK_UNMANAGED => fix_unmanaged(keeper, &path, &detail, &fix),
        doctor::ASSET_DUPLICATED => fix_duplicated(keeper, &detail, &fix),
        doctor::MCP_DRIFT => fix_mcp_drift(keeper, &detail, input),
        doctor::LINK_FRAGILE => fail("doctor.notFixable"),
        _ => fail("doctor.unsupportedFix"),
    }
}

/// `fix` 缺省时按 code 自身的修法执行；给了就必须对上。
fn fix_is(fix: &str, want: &str) -> bool {
    fix.is_empty() || fix == want
}

/// `link.dangling`：按台账（找不到再用 `detail.src`）重新链接。
///
/// 前提是**库内主副本还在**。目标不存在（主副本被删）时唯一诚实的答复是
/// `doctor.sourceMissing` —— 建一条指向不存在路径的链接只会制造第二条 finding。
fn fix_dangling(keeper: &Keeper, path: &str, detail: &Value, fix: &str) -> Value {
    if !fix_is(fix, doctor::FIX_RELINK) {
        return fail("doctor.unsupportedFix");
    }
    let lib = keeper.library();
    let dst = native(path);
    let ledger_entry = lib
        .load_ledger()
        .ok()
        .and_then(|l| l.find(&dst).cloned());
    let src = ledger_entry
        .as_ref()
        .map(|e| e.src.clone())
        .or_else(|| text(detail, "src"))
        .map(|s| native(&s));
    let Some(src) = src else {
        return fail("doctor.sourceMissing");
    };
    if !Path::new(&src).exists() {
        return fail_with("doctor.sourceMissing", json!({ "path": src }));
    }
    let dst_path = Path::new(&dst);
    let is_link = linkfs::entry_link_kind(dst_path).is_some();
    if !is_link && dst_path.exists() {
        // 路径上是真实目录 / 文件：这不是「悬空链接」，删掉它就是删用户内容。
        return fail_with("share.notLinked", json!({ "path": dst }));
    }
    if is_link {
        if let Some(target) = linkfs::read_target(dst_path) {
            if linkfs::same_target(&target, Path::new(&src)) {
                return json!({
                    "ok": true,
                    "written": [],
                    "skipped": [{ "path": dst, "reason": "already linked" }],
                });
            }
        }
    }

    let kind = ledger_entry
        .as_ref()
        .map(|e| e.kind.clone())
        .or_else(|| text(detail, "kind"))
        .unwrap_or_else(|| "skill".to_string());
    let name = ledger_entry
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| text(detail, "name"))
        .unwrap_or_else(|| basename(&src));
    let prefer = preferred_for(keeper, Path::new(&src));

    let mut ops: Vec<Op> = Vec::new();
    if is_link {
        ops.push(Op::delete(&dst));
    }
    ops.push(link_op(&dst, &src, prefer));
    let tx = keeper.store.transaction(
        &ops,
        &json!({
            "kind": kind,
            "action": "repair",
            "assetName": name,
            "file": dst,
            "source": src,
        }),
    );
    if !tx_ok(&tx) {
        return tx_failure(tx);
    }
    let warning = ledger_warning(&lib, &kind, &name, &src, &dst);
    keeper.invalidate();
    let mut out = json!({
        "ok": true,
        "written": tx_written(&tx),
        "path": dst,
        "src": src,
    });
    if let Some(w) = warning {
        out["ledgerWarning"] = json!(w);
    }
    out
}

/// `link.hijacked`：盘上换成了真实目录 / 文件，内容与库内主副本一致才允许重链。
///
/// 内容不一致时自动重链会丢掉 agent 侧那份改动（§7.3 的口径），所以零写入回
/// `doctor.contentChanged`，交给人工处理。
fn fix_hijacked(keeper: &Keeper, path: &str, detail: &Value, fix: &str) -> Value {
    if !fix_is(fix, doctor::FIX_RELINK) {
        return fail("doctor.unsupportedFix");
    }
    let lib = keeper.library();
    let dst = native(path);
    let ledger_entry = lib
        .load_ledger()
        .ok()
        .and_then(|l| l.find(&dst).cloned());
    let src = ledger_entry
        .as_ref()
        .map(|e| e.src.clone())
        .or_else(|| text(detail, "src"))
        .map(|s| native(&s));
    let Some(src) = src else {
        return fail("doctor.sourceMissing");
    };
    if !Path::new(&src).exists() {
        return fail_with("doctor.sourceMissing", json!({ "path": src }));
    }
    let dst_path = Path::new(&dst);
    if linkfs::entry_link_kind(dst_path).is_some() {
        // 已经被修好（或本来就还是一条正常链接）：零写入，别把它删了重建。
        if let Some(target) = linkfs::read_target(dst_path) {
            if linkfs::same_target(&target, Path::new(&src)) {
                return json!({
                    "ok": true,
                    "written": [],
                    "skipped": [{ "path": dst, "reason": "already linked" }],
                });
            }
        }
        return fail("doctor.contentChanged");
    }
    if !dst_path.exists() {
        return fail("doctor.contentChanged");
    }
    // 服务端重算内容一致性：detail.contentMatches 是上一次扫描的结论，不可信。
    if !same_content(Path::new(&src), dst_path) {
        return fail_with(
            "doctor.contentChanged",
            json!({ "path": dst, "src": src }),
        );
    }

    let kind = ledger_entry
        .as_ref()
        .map(|e| e.kind.clone())
        .or_else(|| text(detail, "kind"))
        .unwrap_or_else(|| "skill".to_string());
    let name = ledger_entry
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| text(detail, "name"))
        .unwrap_or_else(|| basename(&src));
    let prefer = preferred_for(keeper, Path::new(&src));
    let tx = keeper.store.transaction(
        &[Op::delete(&dst), link_op(&dst, &src, prefer)],
        &json!({
            "kind": kind,
            "action": "relink",
            "assetName": name,
            "file": dst,
            "source": src,
        }),
    );
    if !tx_ok(&tx) {
        return tx_failure(tx);
    }
    let warning = ledger_warning(&lib, &kind, &name, &src, &dst);
    keeper.invalidate();
    let mut out = json!({
        "ok": true,
        "written": tx_written(&tx),
        "path": dst,
        "src": src,
        "contentMatches": true,
    });
    if let Some(w) = warning {
        out["ledgerWarning"] = json!(w);
    }
    out
}

/// `link.ledgerDrift` + `fix:"rebuild"`：以磁盘为准重建整本台账。
///
/// 坏台账**只在这个显式动作里**被替换（其余路径一律拒绝覆盖，见
/// `library::load_ledger` 的立场）。
fn fix_ledger_drift(keeper: &Keeper, fix: &str) -> Value {
    if !fix_is(fix, doctor::FIX_REBUILD) {
        return fail("doctor.unsupportedFix");
    }
    let lib = keeper.library();
    let ledger = rebuild_ledger(keeper, &lib);
    if let Err(e) = lib.save_ledger(&ledger) {
        return fail_with("write.failed", json!({ "error": e }));
    }
    keeper.invalidate();
    let written = store::native_str(&lib.ledger_path());
    json!({
        "ok": true,
        "entries": ledger.entries.len(),
        "written": [written],
        "path": written,
    })
}

/// 一条待重建的台账（路径已解析成绝对路径的字符串）。
struct PendingLink {
    kind: String,
    name: String,
    src: String,
    dst: String,
    method: LinkMethod,
}

/// 以磁盘为准重建台账：扫描各 agent 声明的 skill 目录与规则 / 记忆路径，把指向
/// 库内的链接重新登记。
///
/// 只收**指向库内**的链接：库外链接属于 `link.unmanaged`，不是我们建的，写进
/// 台账会让「谁建的这一条」变成假的。硬链接没有可读目标，无法登记诚实的 `src`，
/// 留给 `link.fragile` 报告。
fn rebuild_ledger(keeper: &Keeper, lib: &Library) -> Ledger {
    let mut pending: Vec<PendingLink> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let projects = scan::read_projects(keeper.store.root_str());
    for agent in adapters::AGENTS {
        let Some(home) = adapters::resolve_home(agent.home, &keeper.user_home) else {
            continue;
        };
        let home_path = Path::new(&home);
        // 全局：skills 是目录（条目本身是链接）；rules / memory 既可能是目录，
        // 也可能是单个文件。
        for rel in agent.globals.skills {
            if rel.is_empty() {
                continue;
            }
            collect_links_in("skill", &home_path.join(rel), lib, &mut pending, &mut seen);
        }
        for rel in agent.globals.rules {
            collect_declared("rule", home_path, rel, lib, &mut pending, &mut seen);
        }
        for rel in agent.globals.memory {
            collect_declared("memory", home_path, rel, lib, &mut pending, &mut seen);
        }
        // 项目作用域：项目目录来自 `projects.json`（与扫描同一份）。
        for project in &projects {
            let Some(dir) = project.get("dir").and_then(Value::as_str) else {
                continue;
            };
            let base = Path::new(dir);
            for rel in agent.projects.skills {
                if rel.is_empty() {
                    continue;
                }
                collect_links_in("skill", &base.join(rel), lib, &mut pending, &mut seen);
            }
            for rel in agent.projects.rules {
                collect_declared("rule", base, rel, lib, &mut pending, &mut seen);
            }
            for rel in agent.projects.memory {
                collect_declared("memory", base, rel, lib, &mut pending, &mut seen);
            }
        }
    }

    let entries = pending
        .into_iter()
        .map(|p| LinkEntry::new(&p.kind, &p.name, &p.src, &p.dst, p.method))
        .collect();
    Ledger {
        entries,
        migrating: None,
    }
}

/// 处理一条「声明路径」：是目录就逐个文件找链接，是文件就看它本身。
fn collect_declared(
    kind: &str,
    base: &Path,
    rel: &str,
    lib: &Library,
    out: &mut Vec<PendingLink>,
    seen: &mut HashSet<String>,
) {
    let path = base.join(rel);
    let is_link = linkfs::entry_link_kind(&path).is_some();
    if !is_link && path.is_dir() {
        collect_links_in(kind, &path, lib, out, seen);
        return;
    }
    collect_one(kind, &path, lib, out, seen);
}

/// 扫描一个目录里的**条目**（skill 目录 / 规则目录）。
fn collect_links_in(
    kind: &str,
    dir: &Path,
    lib: &Library,
    out: &mut Vec<PendingLink>,
    seen: &mut HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        collect_one(kind, &entry.path(), lib, out, seen);
    }
}

/// 一条路径若是「指向库内的链接」就登记。
fn collect_one(
    kind: &str,
    path: &Path,
    lib: &Library,
    out: &mut Vec<PendingLink>,
    seen: &mut HashSet<String>,
) {
    let Some(method) = linkfs::entry_link_kind(path) else {
        return;
    };
    if method == LinkMethod::Hardlink {
        // 硬链接读不出目标：登记一条 src 为空的记录只会制造假数据。
        return;
    }
    let Some(target) = linkfs::read_target(path) else {
        return;
    };
    // 只收指向库内的链接；库外的是 link.unmanaged 的事。
    if !library::is_under(&target, lib.root()) {
        return;
    }
    let dst = store::native_str(path);
    if !seen.insert(store::normalize_path(&dst).to_lowercase()) {
        return;
    }
    let name = match kind {
        "skill" => basename(&dst),
        _ => {
            let stem = Path::new(&dst)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| basename(&dst));
            slug(&stem)
        }
    };
    if name.is_empty() {
        return;
    }
    out.push(PendingLink {
        kind: kind.to_string(),
        name,
        src: store::native_str(&target),
        dst,
        method,
    });
}

/// `link.unmanaged` + `fix:"adopt"`：把链接目标拷进库、原链接改指库内。
///
/// **绝不删除库外原目录**：这个链接可能是用户自己建的，目标属于用户。库内已有
/// 同 slug 且内容不同时按 `converge.diverged` 拒绝（覆盖就等于静默丢数据）。
fn fix_unmanaged(keeper: &Keeper, path: &str, detail: &Value, fix: &str) -> Value {
    if !fix_is(fix, doctor::FIX_ADOPT) {
        return fail("doctor.unsupportedFix");
    }
    let lib = keeper.library();
    let dst = native(path);
    let dst_path = Path::new(&dst);
    if linkfs::entry_link_kind(dst_path).is_none() {
        return fail_with("share.notLinked", json!({ "path": dst }));
    }
    let Some(target) = linkfs::read_target(dst_path) else {
        return fail("doctor.contentChanged");
    };
    let target = store::native_str(&target);
    if !Path::new(&target).exists() {
        return fail_with("doctor.sourceMissing", json!({ "src": target }));
    }
    if lib.owns_target(dst_path) {
        // 已经指向库内：没什么可导入的（可能是别人刚修过）。
        return json!({
            "ok": true,
            "written": [],
            "skipped": [{ "path": dst, "reason": "already in the library" }],
        });
    }
    let kind = text(detail, "kind").unwrap_or_else(|| "skill".to_string());
    if kind != "skill" || !Path::new(&target).is_dir() {
        return fail_with(
            "doctor.unsupportedFix",
            json!({ "kind": kind, "target": target }),
        );
    }
    let name = text(detail, "name")
        .or_else(|| text(detail, "slug"))
        .unwrap_or_else(|| basename(&target));
    let slug_name = slug(&name);
    if slug_name.is_empty() {
        return fail_with("doctor.unsupportedFix", json!({ "target": target }));
    }
    let keeper_dir = lib.skill_dir(&slug_name);
    let keeper_str = store::native_str(&keeper_dir);
    let mut ops: Vec<Op> = Vec::new();
    let mut written: Vec<String> = Vec::new();
    if keeper_dir.exists() {
        // 库内已有一份：内容必须一致，否则「导入」会变成覆盖库内主副本。
        if !same_content(&keeper_dir, Path::new(&target)) {
            return fail_with(
                "converge.diverged",
                json!({ "slug": slug_name, "library": keeper_str, "target": target }),
            );
        }
    } else {
        // 只拷贝，不移动：`copyDir` 的来源是库外那份，原目录一个字节都不动。
        ops.push(Op::copy_dir(&keeper_str, &target));
        written.push(keeper_str.clone());
    }
    ops.push(Op::delete(&dst));
    // 建链接时库里那份可能还没落盘（`copyDir` 在同一个事务里先执行），所以不能
    // 用 `Path::is_dir()` 去猜类型 —— skill 就是目录，直接用 linkDir。
    ops.push(Op::link_dir_with(&dst, &keeper_str, preferred_dir(keeper)));
    written.push(dst.clone());
    let tx = keeper.store.transaction(
        &ops,
        &json!({
            "kind": "skill",
            "action": "adopt",
            "assetName": name,
            "library": keeper_str,
            "source": target,
        }),
    );
    if !tx_ok(&tx) {
        return tx_failure(tx);
    }
    let warning = ledger_warning(&lib, "skill", &slug_name, &keeper_str, &dst);
    keeper.invalidate();
    let mut out = json!({
        "ok": true,
        "written": unique(written),
        "path": dst,
        "library": keeper_str,
        "source": target,
    });
    if let Some(w) = warning {
        out["ledgerWarning"] = json!(w);
    }
    out
}

/// `asset.duplicated` + `fix:"converge"`：转调 `apply_converge`（只点名这一组）。
fn fix_duplicated(keeper: &Keeper, detail: &Value, fix: &str) -> Value {
    if !fix_is(fix, doctor::FIX_CONVERGE) {
        return fail("doctor.unsupportedFix");
    }
    let slug_name = text(detail, "slug")
        .or_else(|| text(detail, "name").map(|n| slug(&n)))
        .unwrap_or_default();
    if slug_name.is_empty() {
        return fail("doctor.unsupportedFix");
    }
    apply_converge(keeper, &json!({ "groups": [slug_name] }))
}

/// `mcp.drift`：三选一 —— 以库为准覆盖 agent / 以 agent 为准回写库 / 标记独立。
///
/// `detail` 里的 `file` / `field` / 值都不可信；只拿 `name` / `agentId` 当索引，
/// 配置路径从适配器表重新解析，配置条目从磁盘重新读。
fn fix_mcp_drift(keeper: &Keeper, detail: &Value, input: &Value) -> Value {
    let choice = text(input, "choice").unwrap_or_default();
    let lib = keeper.library();
    let name = text(detail, "name").unwrap_or_default();
    if name.is_empty() {
        return fail("mcp.noRecordInLibrary");
    }
    let agent_id = text(detail, "agentId").unwrap_or_default();
    let scope = text(detail, "scope").unwrap_or_else(|| "global".to_string());
    // 找「这份配置里确实有这条服务器」的候选文件；不信任客户端给的 path / file。
    let files = sharing::mcp_config_candidates(&agent_id, &scope, None, &keeper.user_home);
    let file = files
        .iter()
        .find(|f| sharing::read_entry(f, &agent_id, &name).is_some())
        .cloned();
    match choice.as_str() {
        "library" => {
            let Some(file) = file else {
                return fail("mcp.noConfigFile");
            };
            let req = json!({
                "name": name,
                "agentId": agent_id,
                "scope": scope,
                "file": file,
                "on": true,
            });
            let plan = match sharing::materialize_plan(
                &lib,
                &keeper.user_home,
                None,
                &req,
            ) {
                Ok(Ok(Some(plan))) => plan,
                Ok(Ok(None)) => {
                    return json!({
                        "ok": true,
                        "written": [],
                        "skipped": [{ "path": file, "reason": "already open" }],
                    })
                }
                Ok(Err(code)) => return fail(code),
                Err(code) => return fail(code),
            };
            let tx = keeper.store.transaction(
                &plan.ops,
                &json!({
                    "kind": "mcp",
                    "action": "repair",
                    "assetName": name,
                    "server": plan.server,
                    "file": plan.file,
                }),
            );
            if !tx_ok(&tx) {
                return tx_failure(tx);
            }
            keeper.invalidate();
            json!({
                "ok": true,
                "written": [plan.file],
                "file": plan.file,
                "server": plan.server,
                "choice": "library",
            })
        }
        "agent" => {
            let Some(file) = file else {
                return fail("mcp.noConfigFile");
            };
            let Some(entry) = sharing::read_entry(&file, &agent_id, &name) else {
                return fail("doctor.contentChanged");
            };
            let existing = lib
                .mcp_records()
                .into_iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()));
            let record = canonical_record(&name, &agent_id, &entry, existing.as_ref());
            let path = store::native_str(&lib.mcp_record_path(&slug(&name)));
            let body = format!(
                "{}\n",
                serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".to_string())
            );
            // 写的是**库内规范形态**，不是 agent 配置；仍走 `store.transaction`：
            // 它会在写入前快照旧记录（可撤销）+ 写 `audit.jsonl`，而 `settings::atomic_write`
            // 只保证不出现半份文件、不产生审计线。共享库记录同样是用户数据，
            // 「能回滚」比「单文件原子」更符合这里的要求。
            let tx = keeper.store.transaction(
                &[Op::write(&path, &body)],
                &json!({
                    "kind": "mcp",
                    "action": "repair",
                    "assetName": name,
                    "file": path,
                    "source": file,
                }),
            );
            if !tx_ok(&tx) {
                return tx_failure(tx);
            }
            keeper.invalidate();
            json!({
                "ok": true,
                "written": [path],
                "file": path,
                "source": file,
                "choice": "agent",
            })
        }
        "independent" => {
            let Some(mut record) = lib
                .mcp_records()
                .into_iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
            else {
                return fail("mcp.noRecordInLibrary");
            };
            let mut all_of = record
                .get("allOf")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            all_of.insert(agent_id.clone(), json!(false));
            if let Value::Object(m) = &mut record {
                m.insert("allOf".to_string(), Value::Object(all_of));
            }
            let path = store::native_str(&lib.mcp_record_path(&slug(&name)));
            let body = format!(
                "{}\n",
                serde_json::to_string_pretty(&record).unwrap_or_else(|_| "{}".to_string())
            );
            let tx = keeper.store.transaction(
                &[Op::write(&path, &body)],
                &json!({
                    "kind": "mcp",
                    "action": "repair",
                    "assetName": name,
                    "file": path,
                }),
            );
            if !tx_ok(&tx) {
                return tx_failure(tx);
            }
            keeper.invalidate();
            json!({
                "ok": true,
                "written": [path],
                "file": path,
                "choice": "independent",
                "agentId": agent_id,
            })
        }
        _ => fail("doctor.unsupportedFix"),
    }
}

/// 把 agent 配置里的一条服务器转回库内规范形态。
///
/// 只搬规范字段（transport / command / args / env / url / headers / cwd），
/// `timeout` / `enabled` / `type` 这类 agent 私有调优不跟着走 —— 它们属于这个
/// agent 的写法，写进共享记录会被物化到别的 agent 上。`allOf` 原样保留（它是
/// 「谁启用过」的记账，不是服务器参数）。
fn canonical_record(name: &str, agent_id: &str, entry: &Value, existing: Option<&Value>) -> Value {
    let transport = transport_of(entry);
    let keys: &[&str] = if transport == "stdio" {
        &["command", "args", "env", "cwd"]
    } else {
        &["url", "headers"]
    };
    let mut record = serde_json::Map::new();
    record.insert("schema".to_string(), json!(1));
    record.insert("name".to_string(), json!(name));
    record.insert("transport".to_string(), json!(transport));
    for key in keys {
        if let Some(value) = entry.get(*key) {
            if !value.is_null() {
                record.insert((*key).to_string(), value.clone());
            }
        }
    }
    let all_of = existing
        .and_then(|r| r.get("allOf"))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({ agent_id: true }));
    record.insert("allOf".to_string(), all_of);
    Value::Object(record)
}

/// 一条 agent 配置条目隐含的传输方式。
fn transport_of(entry: &Value) -> String {
    let declared = entry
        .get("transport")
        .and_then(Value::as_str)
        .or_else(|| entry.get("type").and_then(Value::as_str));
    match declared {
        // opencode 的写法：`type: "local"` 就是 stdio，`remote` 是远程。
        Some("local") => "stdio".to_string(),
        Some("remote") => "http".to_string(),
        Some(t @ ("stdio" | "http" | "sse")) => t.to_string(),
        _ => {
            let has_url = entry
                .get("url")
                .and_then(Value::as_str)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if has_url {
                // 有 url 但没写传输方式：http 与 sse 无从区分，按 http 记。
                "http".to_string()
            } else {
                "stdio".to_string()
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* 库位置迁移                                                          */
/* ------------------------------------------------------------------ */

/// `library_migrate` 命令：迁移库根（幂等、可断点续跑）。
///
/// 入参：`{ to: "default" | "store" | "<绝对路径>" }`。`default` = `~/.agents`，
/// `store` = `<store>/library`，其余按绝对路径（`~` 展开）。
///
/// 流程（§9.3）：
/// 1. 台账里写 `migrating:{from,to}`（journal）；
/// 2. 逐个搬**顶层子项**：`rename`，跨卷失败退回 `copy + rm`；
/// 3. 逐条重指链接（`unlink` + 重建到新 src），成功一条就更新台账 `src`；
/// 4. 清 journal，把台账写到新库根，删掉旧台账；
/// 5. `settings::save{libraryRoot}` 让后续所有解析都跟到新位置。
///
/// ## 为什么可以绕过 `store.transaction`
///
/// 迁移是**唯一**允许绕过事务的共享写路径：它要搬的是整棵库（可能几百个目录、
/// 跨卷），一次事务要么撑爆撤销预算（8 MiB），要么把几百次重命名绑成一个
/// 无法完成的原子操作。取而代之的是 journal：每一步都先记进度、再做动作，
/// 动作本身是幂等的（已搬过的看磁盘就知道、已重指过的链接读目标就知道），
/// 所以中断后重跑会从磁盘事实继续，最终状态与一次跑完一致。事务的「失败即回滚」
/// 在这里由「失败即留 journal、重跑收敛」替代。
pub fn migrate_library(keeper: &Keeper, input: &Value) -> Value {
    let lib = keeper.library();
    let from = store::native_str(lib.root());
    let Some(to) = migrate_target(keeper, input) else {
        return fail_with("library.migrateTarget", json!({ "from": from }));
    };
    if paths_equal(&from, &to) {
        // 已经就在目标位置：幂等返回，不产生任何写入。
        let total = top_level_names(Path::new(&from)).len();
        return json!({
            "ok": true,
            "from": from,
            "to": to,
            "moved": 0,
            "total": total,
            "links": 0,
            "resumed": false,
        });
    }
    // 嵌套（目标在库内 / 库在目标内）会让「搬库」变成递归自我覆盖。
    if library::is_under(Path::new(&to), Path::new(&from))
        || library::is_under(Path::new(&from), Path::new(&to))
    {
        return fail_with("library.migrateTarget", json!({ "from": from, "to": to }));
    }
    let mut ledger = match lib.load_ledger() {
        Ok(l) => l,
        Err(e) => {
            // 读不出台账就不知道有哪些链接要重指；硬迁会留下一批悬空链接。
            return fail_with("link.ledgerDrift", json!({ "error": e }));
        }
    };
    // 续跑：journal 的目标一致才继续；指向别处的 journal 说明另一个迁移没做完，
    // 覆盖它会让那批链接永久指错。
    let (resumed, mut done) = match &ledger.migrating {
        Some(m) if paths_equal(&m.to, &to) => (true, m.done.clone()),
        Some(_) => {
            return fail_with(
                "library.migrateTarget",
                json!({ "from": from, "to": to, "reason": "anotherMigrationInFlight" }),
            )
        }
        None => (false, Vec::new()),
    };
    // 1) journal 落盘（写在当前库根的台账里）。
    ledger.migrating = Some(Migrating {
        from: from.clone(),
        to: to.clone(),
        done: done.clone(),
    });
    if let Err(e) = lib.save_ledger(&ledger) {
        return fail_with("write.failed", json!({ "error": e }));
    }
    if let Err(e) = std::fs::create_dir_all(Path::new(&to)) {
        return fail_with("library.migrateFailed", json!({ "to": to, "error": e.to_string() }));
    }

    // 2) 顶层子项。名字取两边的并集：续跑时已搬走的那部分只在目标侧存在。
    let mut names = top_level_names(Path::new(&from));
    for name in top_level_names(Path::new(&to)) {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    let total = names.len();
    let mut moved = 0usize;
    for name in &names {
        let src = store::native_str(&Path::new(&from).join(name));
        let dst = store::native_str(&Path::new(&to).join(name));
        let src_path = Path::new(&src);
        let dst_path = Path::new(&dst);
        let src_present = src_path.exists() || linkfs::is_link(src_path);
        let dst_present = dst_path.exists() || linkfs::is_link(dst_path);
        if !src_present {
            // 上一次已经搬完（续跑）：不再计入本次 moved。
            continue;
        }
        if dst_present {
            // 两侧都有：只可能是跨卷 copy + rm 中断在中间。内容一致才收尾，
            // 不一致就停下 —— 猜哪边是对的会丢数据。
            if !same_content(src_path, dst_path) {
                return fail_with(
                    "library.migrateTarget",
                    json!({
                        "from": from,
                        "to": to,
                        "name": name,
                        "reason": "bothSidesDiffer",
                    }),
                );
            }
            store::rmrf(src_path);
        } else if let Err(e) = move_entry(src_path, dst_path) {
            // 停在 journal 上：下次重跑会从磁盘事实继续。
            return json!({
                "ok": false,
                "code": "library.migrateFailed",
                "from": from,
                "to": to,
                "moved": moved,
                "total": total,
                "links": 0,
                "resumed": resumed,
                "error": e,
            });
        }
        moved += 1;
        if !done.iter().any(|d| d.eq_ignore_ascii_case(name)) {
            done.push(name.clone());
        }
        // 每搬一项就刷新一次进度。写失败不中断：磁盘状态本身已经能回答
        // 「这一项在哪」，journal 只是让人更快看出来的记录。
        ledger.migrating = Some(Migrating {
            from: from.clone(),
            to: to.clone(),
            done: done.clone(),
        });
        let _ = lib.save_ledger(&ledger);
        settings::audit(
            keeper.store.root_str(),
            &json!({ "action": "library.migrate", "name": name, "from": src, "to": dst }),
        );
    }

    // 3) 逐条重指链接，并更新台账里的 src。
    let mut links = 0usize;
    for entry in &mut ledger.entries {
        let new_src = relocate_path(&entry.src, &from, &to);
        let dst_path = Path::new(&entry.dst);
        if entry.mode == "copy" || entry.method == LinkMethod::Copy {
            // 受管副本不是链接，搬库不改变它的位置；只把台账里的 src 换到新库。
            entry.src = new_src;
            continue;
        }
        if linkfs::entry_link_kind(dst_path).is_none() {
            // 不是链接（被顶掉 / 已删）：没有可重指的对象，只更新台账 src，
            // 让下一次 doctor 以正确的 src 判定这一条。
            entry.src = new_src;
            continue;
        }
        let old_target = linkfs::read_target(dst_path);
        if old_target
            .as_ref()
            .map(|t| linkfs::same_target(t, Path::new(&new_src)))
            .unwrap_or(false)
        {
            entry.src = new_src;
            continue;
        }
        if linkfs::unlink(dst_path).is_err() {
            // 解不掉就保留原链接与旧台账：doctor 会把它报成需要修的链接，
            // 比留下一条「台账说新、磁盘指旧」的假记录好。
            continue;
        }
        let made = if Path::new(&new_src).is_dir() {
            linkfs::create_dir_link(Path::new(&new_src), dst_path, Some(entry.method))
        } else {
            linkfs::create_file_link(Path::new(&new_src), dst_path, Some(entry.method))
        };
        match made {
            Ok(_) => {
                entry.src = new_src;
                links += 1;
                settings::audit(
                    keeper.store.root_str(),
                    &json!({
                        "action": "library.relink",
                        "path": entry.dst,
                        "from": old_target.map(|t| store::native_str(&t)).unwrap_or_default(),
                        "to": entry.src,
                    }),
                );
            }
            Err(_) => {
                // 重建失败：把旧链接按原目标补回去（尽力而为），台账保持旧值。
                if let Some(old_target) = &old_target {
                    let _ = if old_target.is_dir() {
                        linkfs::create_dir_link(old_target, dst_path, None)
                    } else {
                        linkfs::create_file_link(old_target, dst_path, None)
                    };
                }
            }
        }
    }

    // 4) 清 journal、把台账写到新库根、删旧台账。
    ledger.migrating = None;
    if let Err(e) = Library::at(Path::new(&to)).save_ledger(&ledger) {
        return json!({
            "ok": false,
            "code": "library.migrateFailed",
            "from": from,
            "to": to,
            "moved": moved,
            "total": total,
            "links": links,
            "resumed": resumed,
            "error": e,
        });
    }
    let old_ledger = Path::new(&from).join("links.json");
    if old_ledger.exists() {
        let _ = std::fs::remove_file(&old_ledger);
    }

    // 5) 让所有后续解析跟到新位置。新库根恰好是默认位置时写空串（跟随 `~`），
    // 否则写绝对路径 —— 与 `Library::resolve` 的读法一一对应。
    let default_root = store::native_str(&Path::new(&keeper.user_home).join(".agents"));
    let setting = if paths_equal(&to, &default_root) {
        String::new()
    } else {
        to.clone()
    };
    let saved = settings::save(keeper.store.root_str(), &json!({ "libraryRoot": setting }));
    if !tx_ok(&saved) {
        return json!({
            "ok": false,
            "code": "write.failed",
            "from": from,
            "to": to,
            "moved": moved,
            "total": total,
            "links": links,
            "resumed": resumed,
            "error": saved.get("error").cloned().unwrap_or(Value::Null),
        });
    }
    keeper.invalidate();
    json!({
        "ok": true,
        "from": from,
        "to": to,
        "moved": moved,
        "total": total,
        "links": links,
        "resumed": resumed,
    })
}

/// 解析迁移目标：`default` / `store` / 绝对路径（`~` 展开）。相对路径返回 `None`。
fn migrate_target(keeper: &Keeper, input: &Value) -> Option<String> {
    let raw = text(input, "to")?;
    let expanded = match raw.as_str() {
        "default" => Path::new(&keeper.user_home).join(".agents"),
        "store" => keeper.store.root().join("library"),
        other => {
            if let Some(rest) = other.strip_prefix("~/").or_else(|| other.strip_prefix("~\\")) {
                Path::new(&keeper.user_home).join(rest)
            } else if other == "~" {
                PathBuf::from(&keeper.user_home)
            } else {
                PathBuf::from(other)
            }
        }
    };
    if !expanded.is_absolute() {
        return None;
    }
    Some(store::normalize_path(&store::native_str(&expanded)))
}

/// 库根下的顶层子项名（不含 `links.json`，它是元数据不是资产）。
fn top_level_names(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("links.json") {
                continue;
            }
            out.push(name);
        }
    }
    out.sort();
    out
}

/// 搬一个顶层条目：同卷 `rename`，跨卷退回 `copy + rm`。
///
/// 失败时只返回机器可读的短标记（`copyFailed` / `removeFailed`），不带英文句子：
/// 用户看到的是 `library.migrateFailed` 的中文，具体是哪一步由这个标记区分。
fn move_entry(src: &Path, dst: &Path) -> Result<(), &'static str> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // 跨卷：先完整拷过去，再删来源。复制失败时保留来源（宁可两边都有，
    // 也不要留下半棵目标树 —— 重跑会按内容一致性收尾）。
    if src.is_dir() {
        store::copy_dir(src, dst);
        if !dst.is_dir() {
            return Err("copyFailed");
        }
        store::rmrf(src);
    } else {
        std::fs::copy(src, dst).map_err(|_| "copyFailed")?;
        std::fs::remove_file(src).map_err(|_| "removeFailed")?;
    }
    Ok(())
}

/// 把旧库根下的路径映射到新库根；不在旧库根下原样返回。
fn relocate_path(src: &str, from: &str, to: &str) -> String {
    let from_norm = store::normalize_path(from);
    let src_norm = store::normalize_path(src);
    if src_norm.eq_ignore_ascii_case(&from_norm) {
        return store::native_str(Path::new(to));
    }
    let prefix = format!("{}\\", from_norm);
    if src_norm.len() > prefix.len()
        && src_norm[..prefix.len()].eq_ignore_ascii_case(&prefix)
    {
        let rest = &src_norm[prefix.len()..];
        return store::normalize_path(&store::native_str(&Path::new(to).join(rest)));
    }
    store::native_str(Path::new(src))
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets;
    use std::time::SystemTime;

    /// 一个全新的临时目录。
    fn temp(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ak-shared-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn keeper_at(root: &Path, home: &Path) -> Keeper {
        Keeper::new(store::Store::new(&root.to_string_lossy()), &home.to_string_lossy())
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// 整棵树的「相对路径#内容」快照，用来断言「零写入」。
    fn snapshot(dir: &Path) -> String {
        fn walk(dir: &Path, base: &str, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let rel = if base.is_empty() {
                    name
                } else {
                    format!("{}/{}", base, name)
                };
                let p = e.path();
                if p.is_dir() && !linkfs::is_link(&p) {
                    walk(&p, &rel, out);
                } else {
                    out.push(format!("{}#{}", rel, read(&p)));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, "", &mut out);
        out.sort();
        out.join("|")
    }

    fn cleanup(root: &Path, home: &Path) {
        store::rmrf(root);
        store::rmrf(home);
    }

    /// 造一个 skill 目录。
    fn skill(home: &Path, agent_home: &str, slug_name: &str, body: &str) -> PathBuf {
        let dir = home.join(agent_home).join("skills").join(slug_name);
        write(&dir.join("SKILL.md"), body);
        dir
    }

    /// 把一条已经存在的链接登记到台账（链接由调用方先建好）。
    fn record_link(lib: &Library, slug_name: &str, src: &Path, dst: &Path) {
        let method = linkfs::link_kind(dst).unwrap_or(LinkMethod::Junction);
        let mut ledger = lib.load_ledger().unwrap();
        lib.record(
            &mut ledger,
            LinkEntry::new(
                "skill",
                slug_name,
                &store::native_str(src),
                &store::native_str(dst),
                method,
            ),
        );
        lib.save_ledger(&ledger).unwrap();
    }

    #[test]
    fn apply_converge_merges_identical_copies_into_the_library() {
        let _guard = assets::lock_for_test();
        let root = temp("converge-root");
        let home = temp("converge-home");
        let k = keeper_at(&root, &home);
        let a = skill(&home, ".claude", "pdf", "SAME BODY");
        let b = skill(&home, ".codex", "pdf", "SAME BODY");

        let out = apply_converge(&k, &json!({ "all": true }));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["failed"], json!(0));
        assert_eq!(out["groups"].as_array().unwrap().len(), 1);
        assert!(out["written"].as_array().unwrap().len() >= 3);

        let lib = k.library();
        let keeper = lib.skill_dir("pdf");
        assert_eq!(read(&keeper.join("SKILL.md")), "SAME BODY");
        assert!(linkfs::link_kind(&a).is_some(), "a 应换成链接");
        assert!(linkfs::link_kind(&b).is_some(), "b 应换成链接");
        assert!(linkfs::same_target(Path::new(&stores(&a)), &keeper));
        cleanup(&root, &home);
    }

    /// 链接的读回目标（测试里断言「指到库里」）。
    fn stores(link: &Path) -> String {
        linkfs::read_target(link)
            .map(|p| store::native_str(&p))
            .unwrap_or_default()
    }

    #[test]
    fn apply_converge_refuses_a_diverged_group_without_any_write() {
        let _guard = assets::lock_for_test();
        let root = temp("diverged-root");
        let home = temp("diverged-home");
        let k = keeper_at(&root, &home);
        skill(&home, ".claude", "pdf", "AAA");
        skill(&home, ".codex", "pdf", "BBB");
        let before = snapshot(&home);

        let out = apply_converge(&k, &json!({ "groups": ["pdf"] }));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["failed"], json!(1));
        assert_eq!(out["groups"][0]["code"], json!("converge.diverged"));
        assert_eq!(out["written"], json!([]));
        assert_eq!(snapshot(&home), before, "diverged 组必须零写入");
        assert!(!k.library().skill_dir("pdf").exists());
        cleanup(&root, &home);
    }

    #[test]
    fn apply_converge_can_name_a_single_group_explicitly() {
        let _guard = assets::lock_for_test();
        let root = temp("single-root");
        let home = temp("single-home");
        let k = keeper_at(&root, &home);
        let only = skill(&home, ".claude", "pdf", "ONLY ONE");

        // 没有 all、没有点名：零写入。
        let empty = apply_converge(&k, &json!({}));
        assert_eq!(empty["written"], json!([]));
        assert!(!k.library().skill_dir("pdf").exists());

        let out = apply_converge(&k, &json!({ "only": ["pdf"] }));
        assert_eq!(out["ok"], json!(true), "{}", out);
        let lib_copy = k.library().skill_dir("pdf");
        assert_eq!(read(&lib_copy.join("SKILL.md")), "ONLY ONE");
        assert!(linkfs::same_target(
            Path::new(&stores(&only)),
            &lib_copy
        ));
        cleanup(&root, &home);
    }

    #[test]
    fn relinking_a_dangling_link_points_it_back_at_the_library_copy() {
        let _guard = assets::lock_for_test();
        let root = temp("relink-root");
        let home = temp("relink-home");
        let k = keeper_at(&root, &home);
        let lib = k.library();
        let keeper = lib.skill_dir("pdf");
        write(&keeper.join("SKILL.md"), "BODY");
        // 一个「指向别处」的链接：目标随后被删 → 悬空，而台账里的 src 是好的。
        let gone = home.join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&gone, &dst, None).unwrap();
        store::rmrf(&gone);
        // 台账里的 src 是好的（库内主副本还在），只是链接指向了一个已消失的目标。
        record_link(&lib, "pdf", &keeper, &dst);

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.dangling",
                "path": store::native_str(&dst),
                "fix": "relink",
                "detail": { "kind": "skill", "name": "pdf" },
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(linkfs::same_target(Path::new(&stores(&dst)), &keeper));
        cleanup(&root, &home);
    }

    #[test]
    fn relinking_refuses_when_the_library_copy_is_gone() {
        let _guard = assets::lock_for_test();
        let root = temp("missing-root");
        let home = temp("missing-home");
        let k = keeper_at(&root, &home);
        let lib = k.library();
        let keeper = lib.skill_dir("pdf");
        write(&keeper.join("SKILL.md"), "BODY");
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&keeper, &dst, None).unwrap();
        record_link(&lib, "pdf", &keeper, &dst);
        store::rmrf(&keeper);

        let before = snapshot(&home);
        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.dangling",
                "path": store::native_str(&dst),
                "fix": "relink",
            }),
        );
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["code"], json!("doctor.sourceMissing"));
        assert_eq!(snapshot(&home), before);
        cleanup(&root, &home);
    }

    #[test]
    fn hijacked_with_matching_content_is_relinked() {
        let _guard = assets::lock_for_test();
        let root = temp("hijack2-root");
        let home = temp("hijack2-home");
        let k = keeper_at(&root, &home);
        let lib = k.library();
        let keeper = lib.skill_dir("pdf");
        write(&keeper.join("SKILL.md"), "BODY");
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&keeper, &dst, None).unwrap();
        record_link(&lib, "pdf", &keeper, &dst);
        // 顶掉：删链接，放一份内容相同的真实目录。
        linkfs::unlink(&dst).unwrap();
        write(&dst.join("SKILL.md"), "BODY");

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.hijacked",
                "path": store::native_str(&dst),
                "fix": "relink",
                "detail": { "kind": "skill", "name": "pdf", "contentMatches": true },
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(linkfs::same_target(Path::new(&stores(&dst)), &keeper));
        cleanup(&root, &home);
    }

    #[test]
    fn hijacked_with_changed_content_is_refused_without_writing() {
        let _guard = assets::lock_for_test();
        let root = temp("hijack3-root");
        let home = temp("hijack3-home");
        let k = keeper_at(&root, &home);
        let lib = k.library();
        let keeper = lib.skill_dir("pdf");
        write(&keeper.join("SKILL.md"), "LIBRARY");
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&keeper, &dst, None).unwrap();
        record_link(&lib, "pdf", &keeper, &dst);
        linkfs::unlink(&dst).unwrap();
        write(&dst.join("SKILL.md"), "AGENT EDIT");

        let before = snapshot(&home);
        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.hijacked",
                "path": store::native_str(&dst),
                "fix": "relink",
            }),
        );
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["code"], json!("doctor.contentChanged"));
        assert_eq!(snapshot(&home), before, "内容不一致必须零写入");
        assert_eq!(read(&dst.join("SKILL.md")), "AGENT EDIT");
        cleanup(&root, &home);
    }

    #[test]
    fn rebuilding_the_ledger_uses_the_disk_as_truth() {
        let _guard = assets::lock_for_test();
        let root = temp("rebuild-root");
        let home = temp("rebuild-home");
        let k = keeper_at(&root, &home);
        let lib = k.library();
        // 一个 skill 链接 + 一个规则文件链接。
        let skill_src = lib.skill_dir("pdf");
        write(&skill_src.join("SKILL.md"), "BODY");
        let skill_dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(skill_dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&skill_src, &skill_dst, None).unwrap();
        let rule_src = lib.dir_for("rule").join("agents.md");
        write(&rule_src, "# AGENTS\n");
        let rule_dst = home.join(".codex").join("AGENTS.md");
        std::fs::create_dir_all(rule_dst.parent().unwrap()).unwrap();
        linkfs::create_file_link(&rule_src, &rule_dst, None).unwrap();
        // 坏台账（不是合法 JSON）。
        write(&lib.ledger_path(), "{ not json");

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.ledgerDrift",
                "path": store::native_str(&lib.ledger_path()),
                "fix": "rebuild",
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["entries"].as_u64().unwrap() >= 2);
        let ledger = lib.load_ledger().expect("重建后的台账必须可读");
        assert!(ledger.entries.iter().any(|e| e.kind == "skill"
            && e.name == "pdf"
            && e.dst.eq_ignore_ascii_case(&store::native_str(&skill_dst))));
        assert!(ledger.entries.iter().any(|e| (e.kind == "rule")
            && e.dst.eq_ignore_ascii_case(&store::native_str(&rule_dst))));
        cleanup(&root, &home);
    }

    #[test]
    fn adopting_an_unmanaged_link_copies_into_the_library_and_keeps_the_source() {
        let _guard = assets::lock_for_test();
        let root = temp("adopt-root");
        let home = temp("adopt-home");
        let k = keeper_at(&root, &home);
        let outside = home.join("outside").join("pdf");
        write(&outside.join("SKILL.md"), "USER OWNED");
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&outside, &dst, None).unwrap();

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "link.unmanaged",
                "path": store::native_str(&dst),
                "fix": "adopt",
                "detail": { "kind": "skill", "name": "pdf" },
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        let lib_copy = k.library().skill_dir("pdf");
        assert_eq!(read(&lib_copy.join("SKILL.md")), "USER OWNED");
        assert!(linkfs::same_target(Path::new(&stores(&dst)), &lib_copy));
        // 库外原目录必须一个字节不少地留着。
        assert_eq!(read(&outside.join("SKILL.md")), "USER OWNED");
        assert!(!linkfs::is_link(&outside));
        cleanup(&root, &home);
    }

    /// 准备一份库内 MCP 记录 + 一份 claude-code 的 `.mcp.json`。
    fn mcp_fixture(_root: &Path, home: &Path, k: &Keeper) -> (PathBuf, PathBuf) {
        let lib = k.library();
        let record = lib.mcp_record_path("github");
        write(
            &record,
            "{\n  \"schema\": 1,\n  \"name\": \"github\",\n  \"transport\": \"stdio\",\n  \"command\": \"npx\",\n  \"args\": [\"-y\", \"server-github\"],\n  \"allOf\": { \"codex\": true }\n}\n",
        );
        let config = home.join(".claude").join(".mcp.json");
        write(
            &config,
            "{\n  \"mcpServers\": {\n    \"github\": { \"transport\": \"stdio\", \"command\": \"custom\", \"args\": [] }\n  }\n}\n",
        );
        (record, config)
    }

    #[test]
    fn enable_and_disable_asset_support_the_mcp_kind() {
        let _guard = assets::lock_for_test();
        let root = temp("mcp-root");
        let home = temp("mcp-home");
        let k = keeper_at(&root, &home);
        let (_record, config) = mcp_fixture(&root, &home, &k);
        let config_str = store::native_str(&config);

        let on = k.enable_asset(&json!({
            "kind": "mcp",
            "name": "github",
            "agentId": "claude-code",
            "scope": "global",
            "file": config_str,
        }));
        assert_eq!(on["ok"], json!(true), "{}", on);
        assert_eq!(on["written"], json!([config_str]));
        let entry = sharing::read_entry(&config_str, "claude-code", "github").expect("config entry");
        assert_eq!(entry["command"], json!("npx"));

        let off = k.disable_asset(&json!({
            "kind": "mcp",
            "name": "github",
            "agentId": "claude-code",
            "scope": "global",
            "file": config_str,
        }));
        assert_eq!(off["ok"], json!(true), "{}", off);
        assert!(sharing::read_entry(&config_str, "claude-code", "github").is_none());
        cleanup(&root, &home);
    }

    #[test]
    fn mcp_drift_fix_library_overwrites_the_agent_config() {
        let _guard = assets::lock_for_test();
        let root = temp("mcp-lib-root");
        let home = temp("mcp-lib-home");
        let k = keeper_at(&root, &home);
        let (_record, config) = mcp_fixture(&root, &home, &k);
        let config_str = store::native_str(&config);

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "mcp.drift",
                "path": config_str,
                "detail": { "name": "github", "agentId": "claude-code", "file": config_str, "scope": "global" },
                "choice": "library",
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        let entry = sharing::read_entry(&config_str, "claude-code", "github").unwrap();
        assert_eq!(entry["command"], json!("npx"));
        // 库内记录不变。
        let record = k.library().mcp_record_path("github");
        assert!(read(&record).contains("\"npx\""));
        cleanup(&root, &home);
    }

    #[test]
    fn mcp_drift_fix_agent_writes_the_record_from_the_agent() {
        let _guard = assets::lock_for_test();
        let root = temp("mcp-agent-root");
        let home = temp("mcp-agent-home");
        let k = keeper_at(&root, &home);
        let (record, config) = mcp_fixture(&root, &home, &k);
        let config_str = store::native_str(&config);

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "mcp.drift",
                "path": config_str,
                "detail": { "name": "github", "agentId": "claude-code", "file": config_str, "scope": "global" },
                "choice": "agent",
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        let raw = read(&record);
        assert!(raw.contains("\"custom\""), "{}", raw);
        // allOf 原样保留（这是「谁启用过」的记账，不是服务器参数）。
        assert!(raw.contains("\"codex\": true"), "{}", raw);
        cleanup(&root, &home);
    }

    #[test]
    fn mcp_drift_fix_independent_flags_the_record() {
        let _guard = assets::lock_for_test();
        let root = temp("mcp-ind-root");
        let home = temp("mcp-ind-home");
        let k = keeper_at(&root, &home);
        let (record, config) = mcp_fixture(&root, &home, &k);
        let config_str = store::native_str(&config);
        let lib = k.library();
        assert_eq!(
            sharing::plan_drift(&lib, &k.user_home, None, "global").len(),
            1
        );

        let out = apply_finding_fix(
            &k,
            &json!({
                "code": "mcp.drift",
                "path": config_str,
                "detail": { "name": "github", "agentId": "claude-code", "file": config_str, "scope": "global" },
                "choice": "independent",
            }),
        );
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(read(&record).contains("\"claude-code\": false"));
        assert!(
            sharing::plan_drift(&lib, &k.user_home, None, "global").is_empty(),
            "标记独立之后不再报漂移"
        );
        cleanup(&root, &home);
    }

    /// 默认库根（`~/.agents`）回归：库内只有一份、且被扫描成伪 agent `shared` 的
    /// 资产时，`converge_plan` 不能给出「替换自己」的假 identical 组；点名应用它
    /// 时也没有可写位置，必须给明确 code 且零写入。
    #[test]
    fn default_library_root_does_not_plan_a_self_replacement() {
        let _guard = assets::lock_for_test();
        let root = temp("libdup-root");
        let home = temp("libdup-home");
        // 默认库根：`~/.agents`。这份 skill 会被扫描器报成伪 agent `shared`。
        let keeper = home.join(".agents").join("skills").join("team-wiki");
        write(&keeper.join("SKILL.md"), "WIKI BODY");
        // 一个已安装的 agent，保证扫描不是空的（链接能力探测落在 store，不在 home）。
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let k = keeper_at(&root, &home);

        let plan = converge_plan(&k);
        assert_eq!(plan["ok"], json!(true), "{}", plan);
        let groups = plan["groups"].as_array().unwrap();
        let group = groups
            .iter()
            .find(|g| g["name"] == json!("team-wiki"))
            .unwrap_or_else(|| panic!("team-wiki 必须出现在方案里：{}", plan));
        assert_eq!(group["verdict"], json!("single"));
        assert_eq!(group["replace"], json!([]));
        assert_eq!(group["copies"].as_array().unwrap().len(), 1);
        assert_eq!(group["keeper"], json!(store::native_str(&keeper)));
        assert_eq!(plan["actionable"], json!(0));

        // 点名「应用」它：重算后 replace 为空 → 无写入，必须给 code 而不是裸 ok:false。
        let before = snapshot(&home);
        let out = apply_converge(&k, &json!({ "groups": ["team-wiki"] }));
        assert_eq!(out["ok"], json!(false), "{}", out);
        assert_eq!(out["written"], json!([]));
        assert_eq!(
            out["groups"][0]["code"],
            json!("converge.notConvergeable"),
            "{}",
            out
        );
        assert_eq!(snapshot(&home), before, "没有可写位置时必须零写入");

        // 不存在的组同样要给 code。
        let missing = apply_converge(&k, &json!({ "groups": ["not-there"] }));
        assert_eq!(missing["ok"], json!(false));
        assert_eq!(missing["groups"][0]["code"], json!("converge.notConvergeable"));
        cleanup(&root, &home);
    }

    /// 默认库根 + 另一个 agent 有一份全等副本：方案是 identical，保留方是库内路径，
    /// replace 只含 agent 那份；应用后 agent 侧变成链接，库内主副本仍是真实目录。
    #[test]
    fn default_library_root_and_an_agent_copy_converge_without_self_replacement() {
        let _guard = assets::lock_for_test();
        let root = temp("libagent-root");
        let home = temp("libagent-home");
        let keeper = home.join(".agents").join("skills").join("team-wiki");
        write(&keeper.join("SKILL.md"), "WIKI BODY");
        let agent_copy = home.join(".claude").join("skills").join("team-wiki");
        write(&agent_copy.join("SKILL.md"), "WIKI BODY");
        let k = keeper_at(&root, &home);

        let plan = converge_plan(&k);
        let groups = plan["groups"].as_array().unwrap();
        let group = groups
            .iter()
            .find(|g| g["name"] == json!("team-wiki"))
            .unwrap_or_else(|| panic!("team-wiki 必须出现在方案里：{}", plan));
        assert_eq!(group["verdict"], json!("identical"), "{}", group);
        assert_eq!(group["keeper"], json!(store::native_str(&keeper)));
        assert_eq!(group["replace"].as_array().unwrap().len(), 1);
        assert_eq!(
            group["replace"][0]["path"],
            json!(store::native_str(&agent_copy))
        );
        assert_eq!(group["copies"].as_array().unwrap().len(), 2);
        assert_eq!(plan["actionable"], json!(1));

        let out = apply_converge(&k, &json!({ "groups": ["team-wiki"] }));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert!(out["written"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().map(|s| s.eq_ignore_ascii_case(&store::native_str(&agent_copy))).unwrap_or(false)));
        // 库内主副本还是真实目录（没有被替换成指向自己的链接）。
        assert!(!linkfs::is_link(&keeper));
        assert_eq!(read(&keeper.join("SKILL.md")), "WIKI BODY");
        // agent 侧变成指向库内的链接。
        assert!(linkfs::is_link(&agent_copy));
        assert!(linkfs::same_target(
            &agent_copy,
            &k.library().skill_dir("team-wiki")
        ));
        cleanup(&root, &home);
    }

    /// 一份可迁移的库：skills/pdf + rules/agents.md + 一条 skill 链接。
    fn migrate_fixture(root: &Path, home: &Path) -> (Keeper, PathBuf, PathBuf) {
        let k = keeper_at(root, home);
        let lib = k.library();
        let skill_src = lib.skill_dir("pdf");
        write(&skill_src.join("SKILL.md"), "BODY");
        write(&lib.dir_for("rule").join("agents.md"), "# AGENTS\n");
        let dst = home.join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        linkfs::create_dir_link(&skill_src, &dst, None).unwrap();
        record_link(&lib, "pdf", &skill_src, &dst);
        (k, skill_src, dst)
    }

    #[test]
    fn migrate_moves_the_library_and_repoints_links_idempotently() {
        let _guard = assets::lock_for_test();
        let root = temp("migrate-root");
        let home = temp("migrate-home");
        let (k, _skill_src, dst) = migrate_fixture(&root, &home);
        let old_root = k.library().root().to_path_buf();
        let new_root = root.join("library");
        let new_str = store::native_str(&new_root);

        let out = migrate_library(&k, &json!({ "to": new_str }));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["moved"], json!(2));
        assert_eq!(out["total"], json!(2));
        assert_eq!(out["links"], json!(1));
        assert_eq!(out["resumed"], json!(false));
        assert!(new_root.join("skills").join("pdf").join("SKILL.md").exists());
        assert!(new_root.join("rules").join("agents.md").exists());
        assert!(!old_root.join("skills").exists());
        assert!(!old_root.join("links.json").exists());
        assert!(linkfs::same_target(
            Path::new(&stores(&dst)),
            &new_root.join("skills").join("pdf")
        ));
        let ledger = Library::at(&new_root).load_ledger().unwrap();
        assert!(ledger.migrating.is_none());
        assert_eq!(ledger.entries.len(), 1);
        assert!(ledger.entries[0].src.eq_ignore_ascii_case(&store::native_str(
            &new_root.join("skills").join("pdf")
        )));
        assert_eq!(settings::read(k.store.root_str())["libraryRoot"], json!(new_str));

        // 幂等：再跑一遍，moved=0，状态不变。
        let again = migrate_library(&k, &json!({ "to": new_str }));
        assert_eq!(again["ok"], json!(true), "{}", again);
        assert_eq!(again["moved"], json!(0));
        assert_eq!(again["total"], json!(2));
        assert!(linkfs::same_target(
            Path::new(&stores(&dst)),
            &new_root.join("skills").join("pdf")
        ));
        cleanup(&root, &home);
    }

    #[test]
    fn migrate_resumes_from_the_journal_after_a_partial_move() {
        let _guard = assets::lock_for_test();
        let root = temp("resume-root");
        let home = temp("resume-home");
        let (k, _skill_src, dst) = migrate_fixture(&root, &home);
        let old_root = k.library().root().to_path_buf();
        let new_root = root.join("resumed");
        let new_str = store::native_str(&new_root);
        let lib = k.library();

        // 模拟「搬到一半被杀」：skills 已经搬过去，journal 留在旧库根的台账里。
        std::fs::create_dir_all(&new_root).unwrap();
        std::fs::rename(
            old_root.join("skills"),
            new_root.join("skills"),
        )
        .unwrap();
        let mut ledger = lib.load_ledger().unwrap();
        ledger.migrating = Some(Migrating {
            from: store::native_str(&old_root),
            to: new_str.clone(),
            done: vec!["skills".to_string()],
        });
        lib.save_ledger(&ledger).unwrap();

        let out = migrate_library(&k, &json!({ "to": new_str }));
        assert_eq!(out["ok"], json!(true), "{}", out);
        assert_eq!(out["resumed"], json!(true));
        assert_eq!(out["moved"], json!(1), "只剩 rules 要搬");
        assert_eq!(out["total"], json!(2));
        assert!(new_root.join("rules").join("agents.md").exists());
        assert!(linkfs::same_target(
            Path::new(&stores(&dst)),
            &new_root.join("skills").join("pdf")
        ));
        let final_ledger = Library::at(&new_root).load_ledger().unwrap();
        assert!(final_ledger.migrating.is_none());
        assert_eq!(final_ledger.entries.len(), 1);
        cleanup(&root, &home);
    }
}
