//! 健康检查（M5）：7 类 finding 的只读检测。
//!
//! ## 只读契约
//!
//! 这里**不做任何写入**：不建目录、不探测链接能力（探测会在 `<store>/probe`
//! 下留文件）、不改台账、不写审计。修复动作由调用方（M6 的
//! `apply_finding_fix`）按 `code` 决定 —— 让检测与修复共用一个函数会在某次
//! 「顺手修一下」里把只读视图变成写入路径，这条边界必须由类型（没有 `&Store`
//! 形参）来守。
//!
//! ## 数据来源
//!
//! - 台账 `links.json`：链接的**应然**状态。我们自己建的每一条链接都在这里，
//!   所以「库内主副本被删了」这类事故只有从台账侧才看得全（链接本身已经不满足
//!   skill 清单条件，扫描器不会把它报成资产）。
//! - 扫描资产 `assets[].meta.link`：链接的**实然**状态（磁盘上现在是什么）。
//!   台账可能过时，磁盘才是事实；两个来源合起来才能区分「悬空」「被顶掉」
//!   「未纳管」「台账漂移」。
//! - `<library>/skills/*`：库内主副本。自定义 `libraryRoot` 时它不一定出现在
//!   扫描结果里，所以重复内容检测自己遍历库目录。
//! - `sharing::plan_drift`：MCP 漂移（M3 已实现的只读路径，不在这里重写）。
//!
//! ## 为什么部分 finding 是只提示
//!
//! `link.fragile` 只提示不修：硬链接会被 agent 的「写临时文件 + rename」打断，
//! 我们能看见它，但没法在它被打断之前把它换成符号链接（那需要一次写盘，而写盘
//! 属于用户点下去的动作）。`mcp.drift` 同理，三种修法（以库为准 / 以 agent 为准 /
//! 标记独立）必须由人在界面上选，`fix` 字段不编码这个选择。

use crate::library::Library;
use crate::linkfs::{self, LinkMethod};
use crate::model::slug;
use crate::sharing;
use crate::store;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/* ------------------------------------------------------------------ */
/* code / severity / fix 常量                                          */
/* ------------------------------------------------------------------ */

/// 七类 finding 的 code。
///
/// 用常量而不是在构造点写字面量：这些是**数据字段**，不是 `zhCode()` 渲染的
/// 错误码；界面按 code 查 `doctor.*` 那组 i18n 文案（§8.4），`CODE_ZH` 不参与。
pub const LINK_DANGLING: &str = "link.dangling";
pub const LINK_UNMANAGED: &str = "link.unmanaged";
pub const LINK_HIJACKED: &str = "link.hijacked";
pub const LINK_FRAGILE: &str = "link.fragile";
pub const ASSET_DUPLICATED: &str = "asset.duplicated";
pub const MCP_DRIFT: &str = "mcp.drift";
pub const LINK_LEDGER_DRIFT: &str = "link.ledgerDrift";

pub const SEV_CRITICAL: &str = "critical";
pub const SEV_WARN: &str = "warn";
pub const SEV_INFO: &str = "info";

/// 修法动词（与 §7.3 的 fix 列一一对应）。
/// `rebuild` 是台账整本以磁盘为准重建，不属于「修一条」，所以单独一个动词。
pub const FIX_RELINK: &str = "relink";
pub const FIX_ADOPT: &str = "adopt";
pub const FIX_CONVERGE: &str = "converge";
pub const FIX_REBUILD: &str = "rebuild";

/* ------------------------------------------------------------------ */
/* 数据结构                                                            */
/* ------------------------------------------------------------------ */

/// 一条 finding。
///
/// `detail` 的形状按 code 不同而不同（见每个检测函数），但共同点是**只有事实**：
/// 路径、目标、内容是否一致。给界面的建议文案不放这里 —— 那句话属于 i18n。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DoctorFinding {
    pub code: &'static str,
    pub severity: &'static str,
    pub path: String,
    pub detail: Value,
    pub fixable: bool,
    /// 一键修的动词；`None` = 没有单一修法（MCP 漂移要三选一，硬链接只能提示）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<&'static str>,
}

/// 一次健康检查的结果。只持有 finding 列表，顺序稳定（先按 code 的固定顺序、
/// 再按路径），这样同一份磁盘状态每次打开看到的顺序都一样。
#[derive(Clone, Debug, Default)]
pub struct DoctorReport {
    findings: Vec<DoctorFinding>,
}

impl DoctorReport {
    pub fn findings(&self) -> &[DoctorFinding] {
        &self.findings
    }

    pub fn count(&self) -> usize {
        self.findings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    pub fn has_critical(&self) -> bool {
        self.findings.iter().any(|f| f.severity == SEV_CRITICAL)
    }

    /// 某个 code 的全部 finding（顺序与报告一致）。测试与 M6 的按类分组用它。
    pub fn by_code(&self, code: &str) -> Vec<&DoctorFinding> {
        self.findings.iter().filter(|f| f.code == code).collect()
    }

    pub fn counts(&self) -> Value {
        let mut critical = 0usize;
        let mut warn = 0usize;
        let mut info = 0usize;
        for f in &self.findings {
            match f.severity {
                SEV_CRITICAL => critical += 1,
                SEV_WARN => warn += 1,
                _ => info += 1,
            }
        }
        json!({
            "total": self.findings.len(),
            "critical": critical,
            "warn": warn,
            "info": info,
        })
    }

    /// `shared_doctor` 命令的返回形态：`{ ok, findings[], counts }`（§8.2）。
    pub fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "findings": self.findings,
            "counts": self.counts(),
        })
    }
}

/// 主副本的**关键内容**还在吗？
///
/// 目录级 `exists()` 捕捉不到「目录在、SKILL.md 被删」——而那正是 agent 读不到
/// 内容的形态（§13 终验 #11）。skill 的关键内容是 `SKILL.md`；rule / memory 的
/// 主副本**本身**是单个文件，目录级判定对它们已经足够（目录在即文件在）。
fn master_unreadable(target: &Path, kind: &str) -> bool {
    kind == "skill" && !target.join("SKILL.md").is_file()
}

/// code 在报告里的固定顺序。按 §7.3 的表格从上到下，方便人对着文档读。
fn code_rank(code: &str) -> u8 {
    match code {
        LINK_DANGLING => 0,
        LINK_UNMANAGED => 1,
        LINK_HIJACKED => 2,
        LINK_FRAGILE => 3,
        ASSET_DUPLICATED => 4,
        MCP_DRIFT => 5,
        LINK_LEDGER_DRIFT => 6,
        _ => 7,
    }
}

/* ------------------------------------------------------------------ */
/* 入口                                                                */
/* ------------------------------------------------------------------ */

/// **只读**健康检查。
///
/// `assets` 是扫描结果里的资产数组（`payload["assets"]`），只用得到
/// `kind` / `name` / `dir` / `meta.link` 四类字段。`project_dir` 为 `Some` 时
/// 额外检查该项目作用域的 MCP 漂移；链接类 finding 只看全局（项目里的链接也会
/// 落在台账里，台账本身没有 scope 概念）。
pub fn diagnose(
    library: &Library,
    assets: &[Value],
    user_home: &str,
    project_dir: Option<&str>,
) -> DoctorReport {
    let mut c = Collector::default();

    let ledger = match library.load_ledger() {
        Ok(l) => Some(l),
        Err(error) => {
            // 台账坏了不等于磁盘坏了：不能把它当空台账（那会让所有链接看起来
            // 都「未纳管」），只能报一条「以磁盘为准重建」的提示。
            c.push(DoctorFinding {
                code: LINK_LEDGER_DRIFT,
                severity: SEV_WARN,
                path: store::native_str(&library.ledger_path()),
                detail: json!({ "reason": "corruptLedger", "error": error }),
                fixable: true,
                fix: Some(FIX_REBUILD),
            });
            None
        }
    };

    if let Some(ledger) = &ledger {
        check_ledger(ledger, &mut c);
    }
    check_scan_links(library, ledger.as_ref(), assets, &mut c);
    check_duplicated_skills(library, assets, &mut c);
    check_mcp_drift(library, user_home, project_dir, &mut c);

    c.finish()
}

/* ------------------------------------------------------------------ */
/* 收集器                                                              */
/* ------------------------------------------------------------------ */

/// 同一个 `(code, path)` 只报一条。台账与扫描两边都可能看见同一条链接，
/// 不去重会让「五条链接悬空」变成十条 finding。
#[derive(Default)]
struct Collector {
    findings: Vec<DoctorFinding>,
    seen: HashSet<(String, String)>,
}

impl Collector {
    fn push(&mut self, f: DoctorFinding) {
        let key = (f.code.to_string(), f.path.to_lowercase());
        if self.seen.insert(key) {
            self.findings.push(f);
        }
    }

    fn already(&self, code: &str, path: &str) -> bool {
        self.seen
            .contains(&(code.to_string(), path.to_lowercase()))
    }

    fn finish(mut self) -> DoctorReport {
        self.findings.sort_by(|a, b| {
            code_rank(a.code)
                .cmp(&code_rank(b.code))
                .then_with(|| a.path.cmp(&b.path))
        });
        DoctorReport {
            findings: self.findings,
        }
    }
}

/* ------------------------------------------------------------------ */
/* 台账侧：应然                                                          */
/* ------------------------------------------------------------------ */

fn check_ledger(ledger: &crate::library::Ledger, c: &mut Collector) {
    for entry in &ledger.entries {
        // `copy` 不是链接：它本来就是一份受管副本，报成「被顶掉」就是误报。
        if entry.mode == "copy" || entry.method == LinkMethod::Copy {
            continue;
        }
        let dst = Path::new(&entry.dst);
        let src = Path::new(&entry.src);
        let path = store::native_str(dst);
        let name = entry.name.as_str();
        let kind = entry.kind.as_str();

        if linkfs::is_link(dst) {
            // 硬链接没有可读的目标路径，先单独接住：它不是悬空、也不是台账漂移，
            // 而是「agent 的写临时文件 + rename 会把它打断」这一条 info。
            if linkfs::link_kind(dst) == Some(LinkMethod::Hardlink) {
                c.push(DoctorFinding {
                    code: LINK_FRAGILE,
                    severity: SEV_INFO,
                    path,
                    detail: json!({
                        "method": LinkMethod::Hardlink.as_str(),
                        "target": entry.src,
                    }),
                    fixable: false,
                    fix: None,
                });
                continue;
            }
            match linkfs::read_target(dst) {
                None => {
                    // 重解析点读不出来（权限或损坏）。它不是悬空 —— 悬空是「目标
                    // 有值但不存在」；这里连值都没有，只能按台账漂移报。
                    c.push(DoctorFinding {
                        code: LINK_LEDGER_DRIFT,
                        severity: SEV_WARN,
                        path,
                        detail: json!({
                            "reason": "unreadableTarget",
                            "kind": kind,
                            "name": name,
                            "src": entry.src,
                        }),
                        fixable: true,
                        fix: Some(FIX_REBUILD),
                    });
                }
                Some(target) if !target.exists() => {
                    c.push(DoctorFinding {
                        code: LINK_DANGLING,
                        severity: SEV_CRITICAL,
                        path,
                        detail: json!({
                            "reason": "targetMissing",
                            "kind": kind,
                            "name": name,
                            "target": store::native_str(&target),
                            "src": entry.src,
                        }),
                        fixable: true,
                        fix: Some(FIX_RELINK),
                    });
                }
                Some(target) if master_unreadable(&target, kind) => {
                    // 目标目录还在，但**主副本的关键内容没了**（skill 没有
                    // SKILL.md）。§13 终验 #11 的字面场景：删库内 SKILL.md，
                    // 各 agent 侧都必须报出来 —— 目录级 `exists()` 捕捉不到它，
                    // 而 agent 读内容确实会失败，静默等于「界面说健康、agent
                    // 实际坏了」。relink 修不了内容缺失（没有可指的内容），
                    // 所以 fixable: false，诊断信息在 detail 里。
                    c.push(DoctorFinding {
                        code: LINK_DANGLING,
                        severity: SEV_CRITICAL,
                        path,
                        detail: json!({
                            "reason": "masterUnreadable",
                            "kind": kind,
                            "name": name,
                            "target": store::native_str(&target),
                            "src": entry.src,
                        }),
                        fixable: false,
                        fix: None,
                    });
                }
                Some(target) if !linkfs::same_target(&target, src) => {
                    // 链接指向了别处：不是悬空（目标存在），也不是被顶掉
                    // （它还是链接），只能按台账与磁盘不符处理。
                    c.push(DoctorFinding {
                        code: LINK_LEDGER_DRIFT,
                        severity: SEV_WARN,
                        path,
                        detail: json!({
                            "reason": "wrongTarget",
                            "kind": kind,
                            "name": name,
                            "expectedSrc": entry.src,
                            "actualTarget": store::native_str(&target),
                        }),
                        fixable: true,
                        fix: Some(FIX_REBUILD),
                    });
                }
                Some(_) => {
                    // 正常的受管链接（目标存在且指向库内主副本）。
                }
            }
        } else if dst.exists() {
            // 台账说这里是我们建的链接，磁盘上却是一个真实目录 / 文件：
            // 典型成因是 agent 把链接换成了自己的副本（§13 终验 #8）。
            let same = same_content(src, dst);
            let found = if dst.is_dir() { "directory" } else { "file" };
            c.push(DoctorFinding {
                code: LINK_HIJACKED,
                severity: SEV_CRITICAL,
                path,
                detail: json!({
                    "kind": kind,
                    "name": name,
                    "src": entry.src,
                    "contentMatches": same,
                    "found": found,
                }),
                // 「内容一致才允许 relink」是 §7.3 的口径：内容不一致时自动重连
                // 会丢掉 agent 侧那份改动，只能人工处理。
                fixable: same,
                fix: if same { Some(FIX_RELINK) } else { None },
            });
        } else {
            // 台账有记录，但那个路径上什么都没有：链接被删掉了（不是被顶掉）。
            c.push(DoctorFinding {
                code: LINK_LEDGER_DRIFT,
                severity: SEV_WARN,
                path,
                detail: json!({
                    "reason": "missingLink",
                    "kind": kind,
                    "name": name,
                    "src": entry.src,
                }),
                fixable: true,
                fix: Some(FIX_REBUILD),
            });
        }
    }
}

/// 两个路径的内容是否相同（目录逐文件摘要，文件逐字节）。
/// 只用于「被顶掉后能不能安全地重连」这一个判断，所以读不出来一律算不同。
fn same_content(src: &Path, dst: &Path) -> bool {
    let Ok(src_meta) = std::fs::metadata(src) else {
        return false;
    };
    let Ok(dst_meta) = std::fs::metadata(dst) else {
        return false;
    };
    if src_meta.is_dir() && dst_meta.is_dir() {
        let a = sharing::digest_dir(src);
        let b = sharing::digest_dir(dst);
        a.unreadable == 0 && b.unreadable == 0 && a.digest == b.digest
    } else if src_meta.is_file() && dst_meta.is_file() {
        match (std::fs::read(src), std::fs::read(dst)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    } else {
        false
    }
}

/* ------------------------------------------------------------------ */
/* 扫描侧：实然                                                          */
/* ------------------------------------------------------------------ */

/// 扫描资产里的链接标注（`meta.link`）。台账可能漏记、也可能过时，这里是
/// 磁盘上的实时状态。台账审计过的那一条不重复报（`Collector` 按 code+path 去重，
/// 但「悬空」与「未纳管」是不同 code，所以这里要显式判一下）。
fn check_scan_links(
    library: &Library,
    ledger: Option<&crate::library::Ledger>,
    assets: &[Value],
    c: &mut Collector,
) {
    for a in assets {
        let Some(link) = a.get("meta").and_then(|m| m.get("link")) else {
            continue;
        };
        if link.is_null() {
            continue;
        }
        let Some(dir) = a
            .get("dir")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        else {
            continue;
        };
        // 以磁盘为准：资产是上一次扫描的快照，可能已经被换掉了。
        let Some(method) = linkfs::entry_link_kind(Path::new(dir)) else {
            continue;
        };
        let path = store::native_str(Path::new(dir));
        let kind = a.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
        // 修复入口用 assetId / scope 做索引（M6 的 `apply_finding_fix`）。它们是
        // 事实字段，不参与任何判定 —— `findings[].detail` 的其余键才是判据。
        let asset_id = a.get("id").cloned().unwrap_or(Value::Null);
        let scope = a.get("scope").cloned().unwrap_or(Value::Null);

        if method == LinkMethod::Hardlink {
            c.push(DoctorFinding {
                code: LINK_FRAGILE,
                severity: SEV_INFO,
                path: path.clone(),
                detail: json!({
                    "method": LinkMethod::Hardlink.as_str(),
                    "target": link.get("src").cloned().unwrap_or(Value::Null),
                    "assetId": asset_id,
                    "scope": scope,
                }),
                fixable: false,
                fix: None,
            });
        }

        let target = linkfs::read_target(Path::new(dir));
        let Some(target) = target else {
            // 读不出目标：台账侧已经会报 unreadableTarget（若登记过）；
            // 没登记的链接没有可靠信息，这里不猜。
            continue;
        };
        if !target.exists() {
            c.push(DoctorFinding {
                code: LINK_DANGLING,
                severity: SEV_CRITICAL,
                path,
                detail: json!({
                    "reason": "targetMissing",
                    "kind": kind,
                    "name": name,
                    "target": store::native_str(&target),
                    "source": "scan",
                    "assetId": asset_id,
                    "scope": scope,
                }),
                fixable: true,
                fix: Some(FIX_RELINK),
            });
            continue;
        }
        if master_unreadable(&target, kind) {
            // 扫描侧同样要接住「目录在、SKILL.md 没了」（§13 终验 #11 的字面
            // 场景）：agent 读内容会失败，静默就是「界面说健康、agent 实际坏」。
            c.push(DoctorFinding {
                code: LINK_DANGLING,
                severity: SEV_CRITICAL,
                path,
                detail: json!({
                    "reason": "masterUnreadable",
                    "kind": kind,
                    "name": name,
                    "target": store::native_str(&target),
                    "source": "scan",
                    "assetId": asset_id,
                    "scope": scope,
                }),
                fixable: false,
                fix: None,
            });
            continue;
        }
        if !library.owns_target(Path::new(dir)) {
            // 指向库外的链接：它可能是用户自己建的，删掉会动到别人的目录。
            // 只提示「可以导入库」，不动它（§5.4 的同一立场）。
            c.push(DoctorFinding {
                code: LINK_UNMANAGED,
                severity: SEV_INFO,
                path,
                detail: json!({
                    "kind": kind,
                    "name": name,
                    "target": store::native_str(&target),
                    "assetId": asset_id,
                    "scope": scope,
                }),
                fixable: true,
                fix: Some(FIX_ADOPT),
            });
            continue;
        }
        if let Some(ledger) = ledger {
            if ledger.find(dir).is_none() && !c.already(LINK_LEDGER_DRIFT, &path) {
                c.push(DoctorFinding {
                    code: LINK_LEDGER_DRIFT,
                    severity: SEV_WARN,
                    path,
                    detail: json!({
                        "reason": "unrecordedLink",
                        "kind": kind,
                        "name": name,
                        "target": store::native_str(&target),
                        "method": method.as_str(),
                        "assetId": asset_id,
                        "scope": scope,
                    }),
                    fixable: true,
                    fix: Some(FIX_REBUILD),
                });
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* 重复内容                                                             */
/* ------------------------------------------------------------------ */

/// 一份真实副本（非链接）。
struct SkillCopy {
    path: String,
    agent_id: String,
    /// 扫描结果里的资产 id / scope（库内主副本没有对应资产，为 `None` / `"library"`）。
    /// M6 的修复入口据此把「收敛」转回一个扫描资产；不参与重复内容判定。
    asset_id: Option<String>,
    scope: String,
}

/// `asset.duplicated`：同一个 slug 有 ≥2 份**真实目录**且内容摘要相同。
///
/// 只做 skill：规则 / 记忆是单文件，重复由 M6 的文件级视图展示；插件不属于共享
/// 四类（§2.1 的 `KINDS`）。库内主副本也参与比较 —— 自定义 `libraryRoot` 时它
/// 不在扫描结果里，只看 assets 会漏掉「库里已有一份 + agent 侧还有一份」。
fn check_duplicated_skills(library: &Library, assets: &[Value], c: &mut Collector) {
    let mut groups: BTreeMap<String, (String, Vec<SkillCopy>)> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();

    for a in assets {
        if a.get("kind").and_then(Value::as_str) != Some("skill") {
            continue;
        }
        // 链接不是副本：它和主副本是同一份内容（收敛之后不该再报重复）。
        if a.get("meta")
            .and_then(|m| m.get("link"))
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(dir) = a
            .get("dir")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        else {
            continue;
        };
        if linkfs::is_link(Path::new(dir)) {
            continue;
        }
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
        let slug_name = slug(name);
        if slug_name.is_empty() {
            continue;
        }
        let key = canonical_key(Path::new(dir));
        if !seen.insert(key) {
            continue;
        }
        let agent_id = a
            .get("agentId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        groups
            .entry(slug_name)
            .or_insert_with(|| (name.to_string(), Vec::new()))
            .1
            .push(SkillCopy {
                path: store::native_str(Path::new(dir)),
                agent_id: agent_id.to_string(),
                asset_id: a.get("id").and_then(Value::as_str).map(|s| s.to_string()),
                scope: a
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
    }

    // 库内主副本。默认库根时它同时也在 assets 里（伪 agent `shared`），
    // 上面已经按物理路径去重，这里只补自定义库根那种情况。
    let lib_skills = library.dir_for("skill");
    if let Ok(entries) = std::fs::read_dir(&lib_skills) {
        for e in entries.flatten() {
            let dir = e.path();
            if !dir.is_dir() || linkfs::is_link(&dir) {
                continue;
            }
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let key = canonical_key(&dir);
            if !seen.insert(key) {
                continue;
            }
            groups
                .entry(name.to_string())
                .or_insert_with(|| (name.to_string(), Vec::new()))
                .1
                .push(SkillCopy {
                    path: store::native_str(&dir),
                    agent_id: "library".to_string(),
                    asset_id: None,
                    scope: "library".to_string(),
                });
        }
    }

    for (slug_name, (label, copies)) in groups {
        if copies.len() < 2 {
            continue;
        }
        // 每份只算一次摘要；读不全（权限 / 占用 / 悬空链接）的副本不参与判定
        // ——「读不全」不等于「相同」。
        let mut by_digest: BTreeMap<(String, u64), Vec<&SkillCopy>> = BTreeMap::new();
        for copy in &copies {
            let d = sharing::digest_dir(Path::new(&copy.path));
            if d.unreadable > 0 {
                continue;
            }
            by_digest
                .entry((d.digest, d.bytes))
                .or_default()
                .push(copy);
        }
        for ((_digest, bytes), hits) in by_digest {
            if hits.len() < 2 {
                continue;
            }
            let paths: Vec<String> = hits.iter().map(|c| c.path.clone()).collect();
            let agents: Vec<String> = hits.iter().map(|c| c.agent_id.clone()).collect();
            let asset_ids: Vec<String> = hits
                .iter()
                .filter_map(|c| c.asset_id.clone())
                .collect();
            let scopes: Vec<String> = hits.iter().map(|c| c.scope.clone()).collect();
            c.push(DoctorFinding {
                code: ASSET_DUPLICATED,
                severity: SEV_INFO,
                path: paths[0].clone(),
                detail: json!({
                    "name": label,
                    "slug": slug_name,
                    "copies": paths,
                    "agents": agents,
                    "assetIds": asset_ids,
                    "scopes": scopes,
                    "bytes": bytes,
                }),
                fixable: true,
                fix: Some(FIX_CONVERGE),
            });
        }
    }
}

/// 判断「同一个物理目录」用的键：canonicalize + 小写 + 去 `\\?\` 前缀。
fn canonical_key(p: &Path) -> String {
    let resolved = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut s = store::native_str(&resolved);
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s.to_lowercase()
}

/* ------------------------------------------------------------------ */
/* MCP 漂移                                                             */
/* ------------------------------------------------------------------ */

fn check_mcp_drift(
    library: &Library,
    user_home: &str,
    project_dir: Option<&str>,
    c: &mut Collector,
) {
    let mut add = |drift: sharing::McpDrift, scope: &str| {
        let mut detail = drift.to_json();
        if let Value::Object(m) = &mut detail {
            m.insert("scope".to_string(), json!(scope));
        }
        c.push(DoctorFinding {
            code: MCP_DRIFT,
            severity: SEV_WARN,
            path: drift.file.clone(),
            detail,
            // 三种修法（以库为准 / 以 agent 为准 / 标记独立）由界面选，
            // 没有单一 fix 字符串；但确实有可执行的修法，所以 fixable。
            fixable: true,
            fix: None,
        });
    };
    for drift in sharing::plan_drift(library, user_home, None, "global") {
        add(drift, "global");
    }
    if let Some(dir) = project_dir {
        for drift in sharing::plan_drift(library, user_home, Some(dir), "project") {
            add(drift, "project");
        }
    }
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Ledger, LinkEntry};
    use serde_json::json;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ak-doctor-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_file(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, body).expect("write");
    }

    #[test]
    fn a_corrupt_ledger_is_a_finding_and_the_file_is_left_alone() {
        let dir = temp("corrupt-ledger");
        let lib = Library::at(dir.join("library"));
        std::fs::create_dir_all(lib.root()).expect("library");
        let ledger_path = lib.ledger_path();
        write_file(&ledger_path, "{");
        let before = std::fs::read(&ledger_path).expect("read ledger");

        let report = diagnose(&lib, &[], &dir.to_string_lossy(), None);
        let drift = report.by_code(LINK_LEDGER_DRIFT);
        assert_eq!(drift.len(), 1, "a corrupt ledger is one finding: {:?}", report);
        assert_eq!(drift[0].detail["reason"], json!("corruptLedger"));
        assert_eq!(drift[0].severity, SEV_WARN);
        assert!(drift[0].fixable);
        // 只读：坏台账一个字节都没动。
        assert_eq!(std::fs::read(&ledger_path).expect("read again"), before);
        crate::store::rmrf(&dir);
    }

    #[test]
    fn a_hardlinked_rule_is_reported_as_fragile_not_as_hijacked() {
        let dir = temp("hardlink-rule");
        let lib = Library::at(dir.join("library"));
        std::fs::create_dir_all(lib.root()).expect("library");
        let src = dir.join("src").join("AGENTS.md");
        write_file(&src, "shared rules\n");
        let dst = dir.join("agent").join("AGENTS.md");
        std::fs::create_dir_all(dst.parent().unwrap()).expect("agent dir");
        std::fs::hard_link(&src, &dst).expect("hard link");
        let mut ledger = Ledger::default();
        ledger.push_raw(LinkEntry::new(
            "rule",
            "agents",
            &store::native_str(&src),
            &store::native_str(&dst),
            LinkMethod::Hardlink,
        ));
        lib.save_ledger(&ledger).expect("save ledger");

        let report = diagnose(&lib, &[], &dir.to_string_lossy(), None);
        assert_eq!(report.by_code(LINK_FRAGILE).len(), 1, "{:?}", report);
        assert_eq!(report.by_code(LINK_HIJACKED).len(), 0, "{:?}", report);
        assert_eq!(report.by_code(LINK_DANGLING).len(), 0, "{:?}", report);
        crate::store::rmrf(&dir);
    }

    #[test]
    fn a_link_to_somewhere_outside_the_library_is_unmanaged() {
        let dir = temp("unmanaged");
        let lib = Library::at(dir.join("library"));
        std::fs::create_dir_all(lib.root()).expect("library");
        // 一个库外的真实 skill 目录，被 agent 侧链接过来。
        let outside = dir.join("elsewhere").join("pdf");
        write_file(&outside.join("SKILL.md"), "---\nname: pdf\n---\n\nbody\n");
        let agent_dir = dir.join("home").join(".claude").join("skills").join("pdf");
        std::fs::create_dir_all(agent_dir.parent().unwrap()).expect("skills dir");
        let Ok(method) = linkfs::create_dir_link(&outside, &agent_dir, None) else {
            eprintln!("skipping: this machine cannot create directory links");
            crate::store::rmrf(&dir);
            return;
        };

        let assets = vec![json!({
            "kind": "skill",
            "name": "pdf",
            "agentId": "claude-code",
            "scope": "global",
            "dir": store::native_str(&agent_dir),
            "meta": { "link": { "method": method.as_str(), "src": store::native_str(&outside) } },
        })];
        let report = diagnose(&lib, &assets, &dir.to_string_lossy(), None);
        let unmanaged = report.by_code(LINK_UNMANAGED);
        assert_eq!(unmanaged.len(), 1, "{:?}", report);
        assert_eq!(unmanaged[0].fix, Some(FIX_ADOPT));
        assert_eq!(report.by_code(LINK_HIJACKED).len(), 0, "{:?}", report);
        crate::store::rmrf(&dir);
    }
}
