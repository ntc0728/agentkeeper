//! 共享层（Skills 共享，M2）—— 收敛探测与「库内主副本 + agent 侧链接」的判定。
//!
//! 本模块只做**判定**：盘上现有的 skill 副本哪些其实是一份、哪一份该进库、哪些
//! 位置该被换成链接。真正的写入由 `keeper::enable_asset` / `keeper::disable_asset`
//! 组织成 `store::Op` 走事务，`remove_from_library` 负责卸载。
//!
//! ## 为什么探测与写入分成两处
//!
//! 收敛是**不可逆感最强**的一步：它会把用户看得见的真实目录换成链接。所以先出
//! 一份只读方案（`plan_converge`），让用户看见「保留哪一份、替换哪几处、差异在
//! 哪些文件」，再由事务执行。方案本身不建目录、不写台账、不改任何字节 ——
//! 它的可用范围是「界面上显示什么」，而界面上显示的必须就是盘上的事实
//! （与 `store.rs` 的整体立场一致）。
//!
//! ## 判定规则（写死，不做启发式）
//!
//! | 情况 | 裁决 | 动作 |
//! |---|---|---|
//! | ≥2 份，逐文件内容与种类全等 | `Identical` | 保留一份入库存，其余替换为链接 |
//! | ≥2 份，任一文件内容或种类不同 | `Diverged` | **不动**，只给差异清单 |
//! | 只有 1 份 | `SingleSource` | 并入库，原位置替换为链接 |
//!
//! ## 口径里的两个陷阱
//!
//! 1. **哈希必须逐文件流式算**：`store::list_files` 给出目录下全部文件，skill
//!    里常有几十 MB 的二进制，整份读进内存会让一次探测吃掉应用的堆。
//! 2. **「文件」与「指向同内容的链接」不能算同一种**：按内容比，一个真实文件与
//!    一条指向同内容的链接会被判成「相同」，可收敛成链接之后语义已经变了
//!    （删掉库内主副本那天，前者是副本、后者会断）。所以每一条都要带
//!    `kind`（`file` | `link`），**kind 不同一律算 `Diverged`**。
//!
//! ## MCP 的部分（M3）
//!
//! 一个 MCP 服务器在不同 agent 的配置里是**同一份事实的几种写法**，而配置文件
//! 本身是用户手写的：注释、小节顺序、兄弟服务器、键序都是用户的东西。所以这里
//! 的写盘规则只有一条 —— **只动那一条服务器，其余逐字节不动**：
//!
//! | 目标格式 | 开 | 关（`Remove`） | 关（`Flag`） |
//! |---|---|---|---|
//! | JSON | 桶里插入/覆盖该 key | 从桶里删掉该 key | 该条上写 `enabled: false` |
//! | TOML | `toml_splice::splice_server` 换掉那一段 | `toml_splice::remove_server` 删掉那一段 | 写 `enabled: false` 到那一段里 |
//! | YAML | 拒绝（`mcp.shareUnsupported`） | 拒绝 | 拒绝 |
//!
//! 三条不发写入的情形，各给一个可读的 code：
//! `mcp.shareUnsupported`（这一份配置的语言表达不了「只动一条」）、
//! `mcp.noRecordInLibrary`（库里没有这份规范形态，无从物化）、
//! `mcp.noConfigFile`（这个 agent / 作用域没有声明配置文件）。
//!
//! **漂移只报告、不覆盖**（`plan_drift`）：同名不同参的服务器在多个 agent 上
//! 并存时，悄悄挑一份覆盖另一份就是静默丢数据；和 cc-switch 的差别正在这里。

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::adapters;
use crate::compose;
use crate::library::Library;
use crate::linkfs::{self, LinkCap};
use crate::registry;
use crate::store::{self, Op};

/// 两类裁决：一份（`SingleSource`）、多份全等（`Identical`）、多份不等（`Diverged`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Identical,
    Diverged,
    SingleSource,
}

impl Verdict {
    /// 写进 IPC 与审计日志的字符串。渲染层只认这三个值。
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Identical => "identical",
            Verdict::Diverged => "diverged",
            Verdict::SingleSource => "single",
        }
    }
}

/// 逐文件差异：只给相对路径、状态与种类，**不给全文** —— 全文是编辑器的事，
/// 方案里带上它会让一次探测的负载跟着资产大小涨。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// 相对 skill 根的路径（`/` 分隔，与 `store::list_files` 同口径）。
    pub rel: String,
    /// 差异状态：`only-a`（只在保留方有）、`only-b`（只在对比方有）、
    /// `different`（两边内容不同）、`kind`（内容相同但一边是文件、一边是链接）。
    pub state: String,
}

impl FileDiff {
    fn to_json(&self) -> Value {
        json!({ "rel": self.rel, "state": self.state })
    }
}

/// 某一处在盘上的 skill 副本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyNode {
    /// 绝对路径（盘上真实位置，可能是链接，也可能是真实目录）
    pub path: String,
    pub agent_id: String,
    pub agent_label: String,
    /// 作用域：`global`（agent 全局目录）或 `project`（某个项目的目录）。
    pub scope: String,
    pub project_id: Option<String>,
    /// 这一处本身是不是链接（链接进来的副本不该再被当成独立来源）
    pub link: Option<String>,
}

impl CopyNode {
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "agentId": self.agent_id,
            "agentLabel": self.agent_label,
            "scope": self.scope,
            "projectId": self.project_id,
            "link": self.link,
        })
    }
}

/// 一个「同名 skill 在多个位置」的分组。
#[derive(Clone, Debug)]
pub struct ConvergeGroup {
    /// slug（与库内目录名、agent 侧目录名一致）
    pub name: String,
    /// frontmatter 里的原名（展示用）。没有 frontmatter 时等于 `name`。
    pub label: String,
    pub verdict: Verdict,
    /// 保留哪一份（绝对路径）。`Diverged` 时也给出建议值，但因为整组不动，
    /// 界面上这一列要按裁决置灰。
    pub keeper: Option<String>,
    /// 要替换成链接的路径（`Diverged` 时为空 —— 那一组整体不动）
    pub replace: Vec<CopyNode>,
    pub copies: Vec<CopyNode>,
    /// `Diverged` 时的逐文件差异（`Identical` / `SingleSource` 时为空）
    pub diffs: Vec<FileDiff>,
}

impl ConvergeGroup {
    /// 收敛之后能省下的字节：被替换掉的每一份的大小之和。
    ///
    /// 只算真实目录；链接本身几乎不占空间，把它算进去会让「省下的字节」虚高。
    pub fn saved_bytes(&self) -> u64 {
        self.replace
            .iter()
            .filter(|c| c.link.is_none())
            .map(|c| dir_bytes(Path::new(&c.path)))
            .sum()
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "label": self.label,
            "verdict": self.verdict.as_str(),
            "keeper": self.keeper,
            "replace": self.replace.iter().map(CopyNode::to_json).collect::<Vec<_>>(),
            "copies": self.copies.iter().map(CopyNode::to_json).collect::<Vec<_>>(),
            "diffs": self.diffs.iter().map(FileDiff::to_json).collect::<Vec<_>>(),
            "savedBytes": self.saved_bytes(),
        })
    }
}

/* ------------------------------------------------------------------ */
/* 内容摘要                                                            */
/* ------------------------------------------------------------------ */

/// 一条目录项的内容指纹（逐文件流式 SHA-256，相对路径参与排序因此也参与摘要）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeDigest {
    /// 整棵树的摘要：`rel\0kind\0sha256` 逐行拼起来再算一次 SHA-256。
    pub digest: String,
    /// 每个文件的 `(rel, kind, 文件 sha256)`，按 `rel` 升序。
    pub files: Vec<(String, &'static str, String)>,
    pub bytes: u64,
    /// 有文件读不出来（权限、被占用、悬空链接）。此时摘要只反映「读得出来的部分」，
    /// 调用方必须把它当成「无法判定」，不能当成「相同」。
    pub unreadable: usize,
}

/// 算一个 skill 目录的内容指纹。**只读**。
///
/// 排除规则不在这里另抄一份：`store::list_files` 已经跳过 `SKIP_DIRS`、按
/// canonicalize 去环，并且把链接按文件计数。直接在它的输出上读文件，口径就与
/// 「事务会把哪些文件当成内容」一致。
pub fn digest_dir(dir: &Path) -> TreeDigest {
    let mut files: Vec<(String, &'static str, String)> = Vec::new();
    let mut bytes = 0u64;
    let mut unreadable = 0usize;
    for rel in store::list_files(dir) {
        let full = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        // 种类先判：一条链接与一个文件即使内容相同，语义也不同（见模块注释）。
        let kind = if linkfs::is_link(&full) { "link" } else { "file" };
        match sha256_file(&full) {
            Ok((sha, len)) => {
                bytes += len;
                files.push((rel, kind, sha));
            }
            Err(_) => unreadable += 1,
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut outer = Sha256::new();
    for (rel, kind, sha) in &files {
        outer.update(rel.as_bytes());
        outer.update(b"\0");
        outer.update(kind.as_bytes());
        outer.update(b"\0");
        outer.update(sha.as_bytes());
        outer.update(b"\n");
    }
    TreeDigest {
        digest: hex(&outer.finalize()),
        files,
        bytes,
        unreadable,
    }
}

/// 流式算一个文件的 SHA-256（64 KiB 一块），返回 `(十六进制摘要, 字节数)`。
fn sha256_file(p: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        len += n as u64;
    }
    Ok((hex(&h.finalize()), len))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 一棵树的字节总数（链接不跟随）。
fn dir_bytes(dir: &Path) -> u64 {
    store::list_files(dir)
        .iter()
        .map(|rel| {
            let full = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            std::fs::symlink_metadata(&full).map(|m| m.len()).unwrap_or(0)
        })
        .sum()
}

/// 两份副本的逐文件差异。`a` 是保留方，所以状态是相对 `a` 说的
/// （`only-a` = 只在保留方有）。
fn diff_trees(a: &TreeDigest, b: &TreeDigest) -> Vec<FileDiff> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.files.len() || j < b.files.len() {
        let take_a = match (a.files.get(i), b.files.get(j)) {
            (Some(x), Some(y)) => x.0 <= y.0,
            (Some(_), None) => true,
            _ => false,
        };
        if take_a {
            let x = &a.files[i];
            match b.files.get(j) {
                Some(y) if y.0 == x.0 => {
                    if x.2 != y.2 {
                        out.push(FileDiff {
                            rel: x.0.clone(),
                            state: "different".to_string(),
                        });
                    } else if x.1 != y.1 {
                        // 内容一样、种类不同：这一条如果不报出来，收敛之后
                        // 「文件」会悄悄变成「链接」，或者反过来。
                        out.push(FileDiff {
                            rel: x.0.clone(),
                            state: "kind".to_string(),
                        });
                    }
                    i += 1;
                    j += 1;
                }
                _ => {
                    out.push(FileDiff {
                        rel: x.0.clone(),
                        state: "only-a".to_string(),
                    });
                    i += 1;
                }
            }
        } else {
            let y = &b.files[j];
            out.push(FileDiff {
                rel: y.0.clone(),
                state: "only-b".to_string(),
            });
            j += 1;
        }
    }
    out
}

/* ------------------------------------------------------------------ */
/* 收敛探测                                                            */
/* ------------------------------------------------------------------ */

/// 收敛方案：分组 + 汇总数字。UI 一次拿全，不需要每组再问一次。
#[derive(Clone, Debug)]
pub struct ConvergePlan {
    pub groups: Vec<ConvergeGroup>,
    pub saved_bytes: u64,
}

impl ConvergePlan {
    pub fn to_json(&self) -> Value {
        json!({
            "groups": self.groups.iter().map(ConvergeGroup::to_json).collect::<Vec<_>>(),
            "savedBytes": self.saved_bytes,
        })
    }

    /// 可以一键收敛的组数。
    ///
    /// 两条排除：`Diverged` 组不能动；`replace` 为空的组没有可替换的位置
    /// （典型是「库内只有一份」的 `SingleSource`，它只是被列出来让界面说明现状，
    /// 点一下也不会有任何写入）。
    pub fn actionable(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| g.verdict != Verdict::Diverged && !g.replace.is_empty())
            .count()
    }
}

/// **纯只读**：不写盘、不建目录、不改台账。
///
/// `assets` 是扫描结果里的资产数组，只取 `kind == "skill"` 的那些；分组键是
/// **slug**（`model::slug(name)`），因为 agent 侧目录名与库内目录名都是 slug，
/// 而 `name` 本身可以是 `PDF Tools` 这种带空格与大小写的写法。
///
/// `library` 里已有的主副本会作为一组的第一份参与比较（这样「库里已经有了、
/// agent 侧还有一份真实副本」也能被收敛），`cap` 用来判断本机有没有链接能力：
/// 连链接都建不出来时不做任何替换 —— 把真实目录换成副本毫无意义。
pub fn plan_converge(assets: &[Value], library: &Library, cap: &LinkCap) -> ConvergePlan {
    let mut groups: Vec<(&str, Vec<CopyNode>, String)> = Vec::new();
    for a in assets {
        let kind = a.get("kind").and_then(Value::as_str).unwrap_or_default();
        if kind != "skill" {
            continue;
        }
        let Some(dir) = a.get("dir").and_then(Value::as_str) else {
            continue;
        };
        if dir.is_empty() || !Path::new(dir).is_dir() {
            continue;
        }
        let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
        let slug_name = crate::model::slug(name);
        let node = CopyNode {
            path: dir.to_string(),
            agent_id: a
                .get("agentId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            agent_label: a
                .get("agentLabel")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            scope: a
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            project_id: a
                .get("projectId")
                .and_then(Value::as_str)
                .map(|s| s.to_string()),
            link: linkfs::is_link(Path::new(dir)).then(|| {
                linkfs::link_kind(Path::new(dir))
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            }),
        };
        // 同一个 slug 已经出现过：并进那一组。扫描结果里同一物理目录可能被两个
        // scope 各报一次（全局链接 + 项目链接），按路径去重，否则「两份」是假的。
        match groups.iter_mut().find(|g| g.0 == slug_name) {
            Some(g) => {
                if !g.1.iter().any(|c| same_dir(&c.path, dir)) {
                    g.1.push(node);
                }
            }
            None => groups.push((
                Box::leak(slug_name.into_boxed_str()) as &str,
                vec![node],
                name.to_string(),
            )),
        }
    }

    let can_link = cap.dir_order.iter().any(|m| m.is_link());
    let mut out: Vec<ConvergeGroup> = Vec::new();
    let mut saved = 0u64;
    for (slug_name, copies, label) in groups {
        // 库内主副本作为「一份」参与比较，但它永远是保留方的首选。
        let lib_dir = library.skill_dir(slug_name);
        let lib_node = if lib_dir.is_dir() {
            Some(CopyNode {
                path: store::native_str(&lib_dir),
                agent_id: "library".to_string(),
                agent_label: "Library".to_string(),
                scope: "library".to_string(),
                project_id: None,
                link: None,
            })
        } else {
            None
        };
        let mut all: Vec<CopyNode> = Vec::new();
        if let Some(n) = lib_node {
            all.push(n);
        }
        // 库内主副本只参与一次：默认库根是 `~/.agents`，它同时被扫描器报成伪 agent
        // `shared` 的资产 —— 同一物理目录会以「副本」的身份再出现一次。把它当成
        // 第二份，就会算出一组「保留库内路径、把它替换成指向自己的链接」的假
        // `identical`（replace 与 keeper 是同一条路径）。
        //
        // **但链接要留着**：一条指向库内的链接不是重复副本，它说明这个位置已经
        // 纳管（「一份真实 + 若干链接」的组仍然不进方案，见下面的 `all.len() > 1`）。
        let lib_path = store::native_str(&lib_dir);
        let mut deduped: Vec<CopyNode> = Vec::new();
        for node in copies {
            if node.link.is_none() && same_dir(&node.path, &lib_path) {
                continue;
            }
            deduped.push(node);
        }
        all.extend(deduped);

        // 已经是纯链接、且指向同一个库内主副本的组：没有收敛价值，不列进方案。
        let real: Vec<&CopyNode> = all.iter().filter(|c| c.link.is_none()).collect();
        if real.len() < 2 {
            if real.len() == 1 && can_link && all.len() > 1 {
                // 一份真实内容 + 若干链接：状态已经正确，只是没有被纳管。
                // 纳管是 `doctor` 的活，不是收敛的活（收敛只处理「多份真实副本」）。
                continue;
            }
            if real.len() == 1 && all.len() == 1 {
                out.push(ConvergeGroup {
                    name: slug_name.to_string(),
                    label,
                    verdict: Verdict::SingleSource,
                    keeper: Some(real[0].path.clone()),
                    replace: if can_link && all[0].agent_id != "library" {
                        vec![all[0].clone()]
                    } else {
                        Vec::new()
                    },
                    copies: all,
                    diffs: Vec::new(),
                });
            }
            continue;
        }

        if !can_link {
            // 本机连目录链接都建不出来：给出分组让界面解释原因，但绝不替换。
            out.push(ConvergeGroup {
                name: slug_name.to_string(),
                label,
                verdict: Verdict::Diverged,
                keeper: None,
                replace: Vec::new(),
                copies: all,
                diffs: Vec::new(),
            });
            continue;
        }

        let digests: Vec<TreeDigest> = all.iter().map(|c| digest_dir(Path::new(&c.path))).collect();
        // 读不全的树不能被判成「相同」：那会让用户在不知情的情况下丢掉读不出来的
        // 那一部分。一律按 `Diverged` 处理，差异清单留空，交给界面提示「无法比对」。
        let unreadable = digests.iter().any(|d| d.unreadable > 0);
        let base = &digests[0];
        let mut diffs: Vec<FileDiff> = Vec::new();
        let mut identical = !unreadable;
        for d in digests.iter().skip(1) {
            // 摘要相同而字节数不同只可能是哈希碰撞；宁可判不同也不要覆盖。
            if d.digest != base.digest || d.bytes != base.bytes {
                identical = false;
                if diffs.is_empty() {
                    diffs = diff_trees(base, d);
                }
            }
        }

        let keeper = all[0].path.clone();
        let replace: Vec<CopyNode> = if identical {
            all.iter()
                .skip(1)
                .filter(|c| c.link.is_none())
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let group = ConvergeGroup {
            name: slug_name.to_string(),
            label,
            verdict: if identical {
                Verdict::Identical
            } else {
                Verdict::Diverged
            },
            keeper: Some(keeper),
            replace,
            copies: all,
            diffs,
        };
        saved += group.saved_bytes();
        out.push(group);
    }

    // 可收敛的组排在前面：用户打开页面第一眼要看到能动的那几条。
    out.sort_by(|a, b| {
        let rank = |g: &ConvergeGroup| match g.verdict {
            Verdict::Identical => 0,
            Verdict::SingleSource => 1,
            Verdict::Diverged => 2,
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    ConvergePlan {
        groups: out,
        saved_bytes: saved,
    }
}

/// 两个路径是不是同一个物理目录（符号链接 / junction / 大小写 / 盘符差异都算同一）。
fn same_dir(a: &str, b: &str) -> bool {
    linkfs::same_target(Path::new(a), Path::new(b))
}

/// 一个 skill 目录的绝对路径列表，便于调用方在不拿完整扫描结果时也能探测。
pub fn skill_dirs_of(assets: &[Value]) -> Vec<PathBuf> {
    assets
        .iter()
        .filter(|a| a.get("kind").and_then(Value::as_str) == Some("skill"))
        .filter_map(|a| a.get("dir").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect()
}

/* ------------------------------------------------------------------ */
/* MCP 单一事实源（M3）                                                 */
/* ------------------------------------------------------------------ */

/// 库内规范形态的 `schema` 版本。改结构就改它；读到更大的版本一律拒绝 ——
/// 一份「看起来像我们的、其实来自更新版本」的记录，按旧口径物化会丢字段。
pub const MCP_SCHEMA: u64 = 1;

/// 一条 MCP 服务器在**某个 agent 的配置文件里**长什么样。
///
/// 只由规范形态 + 该 agent 的写入风格生成，不由扫描结果反推：扫描出来的是
/// agent 自己的写法（`copilot` 的 `servers`、`goose` 的 `cmd`），拿它去写另一个
/// agent 会把两家的私有字段互相传染。
pub struct McpMaterial {
    /// 库内规范形态里的名字（也是台账里的 `name`）。
    pub name: String,
    /// 配置文件里的键名。目前与 `name` 一致：名字一律由外层 key 表达，
    /// 记进条目里会让某些 agent 把 `name` 当成未知键告警。
    pub key: String,
    /// 落盘的那一条（不含 `allOf`，那是记账，不是 agent 的配置）。
    pub entry: Value,
    /// 记录里有值、但被 transport 挑选**丢掉**的字段名（stdio 丢 url/headers，
    /// 非 stdio 丢 command/args/env/cwd）。空 = 什么都没丢。调用方应把它带进
    /// 应答的 `skipped`：静默丢弃正是 §23.3 记过的那类「看不见的取舍」。
    pub dropped: Vec<String>,
}

/// 把库内规范形态转成某个 agent 的写盘形态。
///
/// 只做三件事：按 `transport` 挑字段、去掉空的 `args`/`env`、把显式写过的
/// `timeout`/`enabled` 原样带上。**不补默认值** —— 记录里没有的字段不会凭空出现，
/// 否则用户会在 agent 的配置里看到一个自己没写过的 `timeout`。
pub fn materialize_entry(agent_id: &str, record: &Value) -> Option<McpMaterial> {
    let _ = agent_id;
    let name = record
        .get("name")
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    let transport = record
        .get("transport")
        .and_then(Value::as_str)
        .unwrap_or("stdio")
        .to_string();
    let mut entry = serde_json::Map::new();
    // `transport` 写在最前：它是这一条里唯一能区分两类服务器的字段，也让
    // 「这条记录到底缺不缺传输方式」在配置文件里一眼可见。
    entry.insert("transport".to_string(), Value::String(transport.clone()));
    let keys: &[&str] = match transport.as_str() {
        "stdio" => &["command", "args", "env", "cwd"],
        _ => &["url", "headers"],
    };
    // 这个 transport 用不上的字段：记录里有值却没被带走 → 显式记下来，让应答
    // 能说清「哪些内容没有物化」。空值不算丢（没什么可丢的）。
    let mut dropped: Vec<String> = Vec::new();
    let other_side: &[&str] = if transport == "stdio" {
        &["url", "headers"]
    } else {
        &["command", "args", "env", "cwd"]
    };
    for k in other_side {
        let Some(v) = record.get(*k) else { continue };
        let has_value = !v.is_null()
            && !v.as_array().map(|a| a.is_empty()).unwrap_or(false)
            && !v.as_object().map(|m| m.is_empty()).unwrap_or(false)
            && !v.as_str().map(|s| s.is_empty()).unwrap_or(false);
        if has_value {
            dropped.push((*k).to_string());
        }
    }
    for k in keys {
        let Some(v) = record.get(*k) else { continue };
        if v.is_null() {
            continue;
        }
        if v.as_array().map(|a| a.is_empty()).unwrap_or(false) {
            continue;
        }
        if v.as_object().map(|m| m.is_empty()).unwrap_or(false) {
            continue;
        }
        entry.insert((*k).to_string(), v.clone());
    }
    for k in ["timeout", "enabled"] {
        let Some(v) = record.get(k) else { continue };
        if v.is_null() {
            continue;
        }
        entry.insert((*k).to_string(), v.clone());
    }
    Some(McpMaterial {
        key: name.clone(),
        name,
        entry: Value::Object(entry),
        dropped,
    })
}

/// 某个 agent / 作用域的 MCP 配置文件的**候选列表**（不是单值）。
///
/// 为什么不是单值：`continue` 声明的是 `config.yaml`，而「只动一条服务器」在
/// YAML 上做不到（重新序列化会丢掉全部注释），与它并列的 `mcp.json` 却能做。
/// 所以调用方要能看见「第一份写不了、第二份能写」这一层，而不是拿到一个路径
/// 然后失败。
///
/// 顺序即适配器声明的顺序，与 `keeper::mcp_target_for` 同口径 ——
/// 它排除 `mcp-approvals.json` / `mcp-tool-list.json`，那两类是记账文件。
pub fn mcp_config_candidates(
    agent_id: &str,
    scope: &str,
    project_dir: Option<&str>,
    user_home: &str,
) -> Vec<String> {
    let Some(agent) = crate::adapters::by_id(agent_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if scope == "global" {
        let Some(home) = crate::adapters::resolve_home(agent.home, user_home) else {
            return out;
        };
        for rel in agent.globals.mcp {
            if is_materializable(rel) {
                out.push(store::native_str(&Path::new(&home).join(rel)));
            }
        }
        return out;
    }
    let Some(dir) = project_dir else {
        return out;
    };
    for rel in agent.projects.mcp {
        if is_materializable(rel) {
            out.push(store::native_str(&Path::new(dir).join(rel)));
        }
    }
    out
}

/// 这个相对路径是不是一份**可以只动一条**的 MCP 配置。
///
/// 排除两类：记账文件（`approvals` / `tool-list`，写进去会被 agent 当成一条决策），
/// 以及 YAML（重新序列化会丢注释，见 `mcp.shareUnsupported`）。
fn is_materializable(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    if lower.contains("approvals") || lower.contains("tool-list") {
        return false;
    }
    lower.ends_with(".json") || lower.ends_with(".jsonc") || lower.ends_with(".toml")
}

/// 某一份配置的格式。只看扩展名，与 `keeper::upsert_mcp` 同一判据 ——
/// 两处对「这是什么文件」给出不同答案，是那种最难查的错。
pub fn config_format(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".toml") {
        "toml"
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        "yaml"
    } else {
        "json"
    }
}

/// JSON 配置里该往哪个桶里写。与 `keeper::mcp_bucket_for` 同一份规则：
/// `copilot` 把 server 放在 `servers` 之下，其余都在 `mcpServers` 之下。
fn bucket_for(agent_id: &str) -> &'static str {
    match crate::adapters::family(agent_id) {
        "copilot" => "servers",
        _ => "mcpServers",
    }
}

/// 打开一份配置：把某个服务器写进去。
///
/// 返回 `Ok(None)` 表示「这份文件里的这一条已经一模一样」—— 同一份记录物化两遍，
/// 第二遍不产生任何写入，也不产生第二份撤销快照（幂等）。
///
/// `Err(code)` 是**能到达界面的 code**，不是散文：`mcp.noRecordInLibrary`
/// （记录不完整，连名字都没有）、`mcp.shareUnsupported`（这份语言表达不了）。
pub fn open_ops(
    file: &str,
    agent_id: &str,
    record: &Value,
) -> Result<Option<Vec<store::Op>>, &'static str> {
    let Some(m) = materialize_entry(agent_id, record) else {
        return Err("mcp.noRecordInLibrary");
    };
    let raw = read_text(file);
    match config_format(file) {
        "yaml" => Err("mcp.shareUnsupported"),
        "toml" => {
            if toml_entry(&raw, &m.key).as_ref() == Some(&m.entry) {
                return Ok(None);
            }
            let section = toml_section(&m.key, &m.entry)?;
            Ok(Some(vec![store::Op::write(
                file,
                &crate::toml_splice::splice_server(&raw, &m.key, &section),
            )]))
        }
        _ => {
            let bucket = bucket_for(agent_id);
            let key = m.key.clone();
            let entry = m.entry.clone();
            if json_entry(&raw, bucket, &key).as_ref() == Some(&entry) {
                return Ok(None);
            }
            Ok(Some(vec![store::Op::json_mutate(file, move |cur| {
                let mut next = as_object(cur);
                let current = next.get(bucket).cloned().unwrap_or_else(|| json!({}));
                let mut servers = as_object(current);
                servers.insert(key.clone(), entry.clone());
                next.insert(bucket.to_string(), Value::Object(servers));
                Some(Value::Object(next))
            })]))
        }
    }
}

/// 关掉一份配置里的某个服务器。`style` 来自适配器表（`adapters::DisableStyle`）。
///
/// `Remove` 是**删掉那一条**，不是「删掉它所在的桶」：`mcpServers` 里其余的
/// 服务器一条都不许少。`Flag` 是「留着、写个 `enabled: false`」—— 只在适配器表
/// 核实过这个 agent 认这个字段时才用（见 `adapters::mcp_disable`）。
///
/// 文件里本来就没有这条服务器 → `Ok(None)`：已经满足，零写入。
pub fn close_ops(
    file: &str,
    agent_id: &str,
    name: &str,
    style: crate::adapters::DisableStyle,
) -> Result<Option<Vec<store::Op>>, &'static str> {
    use crate::adapters::DisableStyle;
    let raw = read_text(file);
    match config_format(file) {
        "yaml" => Err("mcp.shareUnsupported"),
        "toml" => {
            let Some(existing) = toml_entry(&raw, name) else {
                return Ok(None);
            };
            match style {
                DisableStyle::Remove => Ok(Some(vec![store::Op::write(
                    file,
                    &crate::toml_splice::remove_server(&raw, name),
                )])),
                DisableStyle::Flag => {
                    if existing.get("enabled") == Some(&json!(false)) {
                        return Ok(None);
                    }
                    let mut flagged = existing.clone();
                    if let Value::Object(m) = &mut flagged {
                        m.insert("enabled".to_string(), json!(false));
                    }
                    let section = toml_section(name, &flagged)?;
                    Ok(Some(vec![store::Op::write(
                        file,
                        &crate::toml_splice::splice_server(&raw, name, &section),
                    )]))
                }
            }
        }
        _ => {
            let bucket = bucket_for(agent_id);
            let Some(existing) = json_entry(&raw, bucket, name) else {
                return Ok(None);
            };
            let key = name.to_string();
            let bucket = bucket.to_string();
            match style {
                DisableStyle::Remove => Ok(Some(vec![store::Op::json_mutate(file, move |cur| {
                    let mut next = as_object(cur);
                    let current = next.get(&bucket).cloned().unwrap_or_else(|| json!({}));
                    let mut servers = as_object(current);
                    servers.shift_remove(&key);
                    next.insert(bucket.clone(), Value::Object(servers));
                    Some(Value::Object(next))
                })])),
                DisableStyle::Flag => {
                    if existing.get("enabled") == Some(&json!(false)) {
                        return Ok(None);
                    }
                    Ok(Some(vec![store::Op::json_mutate(file, move |cur| {
                        let mut next = as_object(cur);
                        let current = next.get(&bucket).cloned().unwrap_or_else(|| json!({}));
                        let mut servers = as_object(current);
                        let entry = servers.get(&key).cloned().unwrap_or_else(|| json!({}));
                        let mut entry = as_object(entry);
                        entry.insert("enabled".to_string(), json!(false));
                        servers.insert(key.clone(), Value::Object(entry));
                        next.insert(bucket.clone(), Value::Object(servers));
                        Some(Value::Object(next))
                    })]))
                }
            }
        }
    }
}

/// 从一份配置里读出某个服务器。读不到返回 `None`。
///
/// **读出来的就是 agent 自己的写法**（多出来的 `cwd`、`type`、`disabled` 都在）：
/// 这是 `plan_drift` 比较的依据 —— 用规范形态去比会永远「一致」，等于没比。
pub fn read_entry(file: &str, agent_id: &str, name: &str) -> Option<Value> {
    let raw = read_text(file);
    match config_format(file) {
        "toml" => toml_entry(&raw, name),
        "yaml" => yaml_entry(&raw, name),
        _ => json_entry(&raw, bucket_for(agent_id), name),
    }
}

/// 一份配置里的全部服务器名（按文件里的顺序）。文件不存在或读不懂 → 空。
pub fn server_names(file: &str, agent_id: &str) -> Vec<String> {
    let raw = read_text(file);
    match config_format(file) {
        "toml" => crate::toml_splice::parse(&raw)
            .ok()
            .and_then(|v| v.get("mcp_servers").and_then(Value::as_object).cloned())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
        "yaml" => crate::yaml::parse(&raw)
            .ok()
            .and_then(|v| yaml_bucket(&v).map(|m| m.keys().cloned().collect()))
            .unwrap_or_default(),
        _ => json_doc(&raw)
            .and_then(|d| {
                d.get(bucket_for(agent_id))
                    .and_then(Value::as_object)
                    .cloned()
            })
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default(),
    }
}

/// 写进一份配置前的共同校验：库内记录的**名字必须是一个合法的 MCP 服务器名**。
///
/// 为什么不能省：名字既是 JSON 的键，也是 TOML 的 `[mcp_servers.<name>]` 表头。
/// 一个带 `]` 或换行的名字会让 `splice_server` 的正则匹配不到 / 拓出畸形段落，
/// 于是「打开」这一步会把一份好配置写坏。
fn validate_record(record: &Value) -> Result<McpMaterial, &'static str> {
    let Some(m) = materialize_entry("", record) else {
        return Err("mcp.noRecordInLibrary");
    };
    if crate::validate::is_blocking(&crate::validate::validate_by_kind(
        "mcp",
        &json!({ "name": m.name.clone(), "config": m.entry.clone() }),
    )) {
        return Err("mcp.notMaterialisable");
    }
    Ok(m)
}

/// 打开 / 关掉一个 MCP 服务器要走的 op 列表。
///
/// `on == true` 走 `open_ops`，`false` 走 `close_ops`；`file` 省略时按适配器表
/// 声明顺序挑第一份**能只动一条**的配置（§5.3）。
///
/// 三件事必须同时成立，否则返回 `{ ok:false, code }` 而不写任何字节：
/// 1. 库内有这条记录（`mcp.noRecordInLibrary`）；
/// 2. 记录能物化成一份合法的服务器（`mcp.notMaterialisable`）；
/// 3. 目标文件是 JSON / TOML（YAML 给 mcp.shareUnsupported），且能算出路径
///    （算不出来给 mcp.noConfigFile）。
///
/// 返回 `written: []` 加 `skipped[{reason}]` 表示「已经是一模一样的状态」——
/// 那是一个**成功**，不是失败：幂等才让「全部打开一次」可以反复点。
pub fn materialize_mcp(
    library: &Library,
    user_home: &str,
    project_dir: Option<&str>,
    input: &Value,
) -> Value {
    match materialize_plan(library, user_home, project_dir, input) {
        Ok(Ok(Some(plan))) => plan.to_json(),
        Ok(Ok(None)) => json!({
            "ok": true,
            "written": [],
            "skipped": [{ "target": input.get("file").cloned().unwrap_or(Value::Null),
                          "reason": if input_on(input) { "already open" } else { "already closed" } }],
        }),
        Ok(Err(code)) => json!({ "ok": false, "code": code }),
        Err(code) => json!({ "ok": false, "code": code }),
    }
}

fn input_on(input: &Value) -> bool {
    input.get("on").and_then(Value::as_bool).unwrap_or(true)
}

/// 计划本身：写哪份文件、关掉的方式、以及要进事务的那几条 op。
///
/// 单独成结构体而不是直接回 `Value`：`Op` 里有闭包，序列化不了 —— 回 `Value`
/// 就等于逼调用方把 op 丢掉，然后由它自己去猜「这次到底写了什么」。
pub struct McpPlan {
    pub file: String,
    pub server: String,
    pub style: crate::adapters::DisableStyle,
    pub ops: Vec<store::Op>,
    /// 被 transport 挑选丢弃的非空字段（`materialize_entry` 的 `dropped`）；
    /// 应答把它带进 `skipped`，让「没有物化的内容」可见。关闭动作不丢字段（空）。
    pub dropped: Vec<String>,
}

impl McpPlan {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": true,
            "file": self.file,
            "server": self.server,
            "written": [self.file],
            "skipped": [],
            "style": self.style.as_str(),
        })
    }
}

/// 算「把库内记录物化到某个 agent 配置」要走的 op，**不落盘**。
///
/// M6 起 `pub`：`keeper::enable_asset` 的 `mcp` 分支与 `shared_ops` 的修复动作
/// 都要拿这份 op 列表自己送进 `store::transaction`，并附上各自的审计元数据。
/// 返回 `Ok(Ok(None))` 表示「这里已经一模一样」——零 op，不是失败。
#[allow(clippy::type_complexity)]
pub fn materialize_plan(
    library: &Library,
    user_home: &str,
    project_dir: Option<&str>,
    input: &Value,
) -> Result<Result<Option<McpPlan>, &'static str>, &'static str> {
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("mcp.noRecordInLibrary");
    }
    let agent_id = input
        .get("agentId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let scope = input
        .get("scope")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("global");
    let on = input_on(input);

    // 记录：库内的规范形态。显式给了 `record` 就先用它（用于「刚导入、还没落库」
    // 的一次性物化），否则从库里读 —— 库里读不到就不写，绝不凭名字造一份空的。
    let record = match input.get("record").filter(|v| v.is_object()) {
        Some(r) => r.clone(),
        None => match library
            .mcp_records()
            .into_iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
        {
            Some(r) => r,
            None => return Err("mcp.noRecordInLibrary"),
        },
    };
    let m = validate_record(&record)?;

    let file = match input.get("file").and_then(Value::as_str) {
        Some(f) if !f.is_empty() => f.to_string(),
        _ => match mcp_config_candidates(&agent_id, scope, project_dir, user_home)
            .first()
            .cloned()
        {
            Some(f) => f,
            None => return Err("mcp.noConfigFile"),
        },
    };

    let style = crate::adapters::mcp_disable(&agent_id);
    let ops = if on {
        open_ops(&file, &agent_id, &record)?
    } else {
        close_ops(&file, &agent_id, &m.name, style)?
    };
    let Some(ops) = ops else {
        return Ok(Ok(None));
    };
    Ok(Ok(Some(McpPlan {
        file,
        server: m.name,
        style,
        ops,
        dropped: m.dropped,
    })))
}

/// 漂移的报告形态：`{ ok, drift: [...] }`（`doctor` 拿它转成 `mcp.drift` finding）。
pub fn drift_report(
    library: &Library,
    user_home: &str,
    project_dir: Option<&str>,
    scope: &str,
) -> Value {
    let drifts = plan_drift(library, user_home, project_dir, scope);
    json!({
        "ok": true,
        "drift": drifts.iter().map(McpDrift::to_json).collect::<Vec<_>>(),
        "count": drifts.len(),
    })
}

/* -------------------------- 内部小工具 -------------------------- */

fn read_text(path: &str) -> String {
    std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// 保序地把一个 `Value` 当成对象；不是对象时给空对象。
fn as_object(v: Value) -> serde_json::Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

/// 读 JSON / JSONC 配置。读不懂 → `None`（**不是空对象**：把一份损坏的配置
/// 当成空的再写回去，就是把它整个抹掉）。
fn json_doc(raw: &str) -> Option<Value> {
    let cleaned = crate::jsonc::strip_json_comments(raw);
    serde_json::from_str(&cleaned).ok()
}

fn json_entry(raw: &str, bucket: &str, name: &str) -> Option<Value> {
    json_doc(raw)?
        .get(bucket)?
        .get(name)
        .filter(|v| v.is_object())
        .cloned()
}

fn yaml_bucket(doc: &Value) -> Option<&serde_json::Map<String, Value>> {
    for b in ["mcpServers", "servers", "mcp_servers"] {
        if let Some(m) = doc.get(b).and_then(Value::as_object) {
            return Some(m);
        }
    }
    None
}

fn yaml_entry(raw: &str, name: &str) -> Option<Value> {
    let doc = crate::yaml::parse(raw).ok()?;
    yaml_bucket(&doc)?.get(name).filter(|v| v.is_object()).cloned()
}

/// TOML 里某个服务器的整段，转回 `Value` 对象；`cmd` 归一成 `command`
/// （`goose` 的写法），与 `scan::scan_mcp_structured` 同一口径。
fn toml_entry(raw: &str, name: &str) -> Option<Value> {
    let doc = crate::toml_splice::parse(raw).ok()?;
    let mut entry = doc.get("mcp_servers")?.get(name)?.clone();
    if !entry.is_object() {
        return None;
    }
    if let Value::Object(m) = &mut entry {
        if let Some(cmd) = m.get("cmd").cloned() {
            m.entry("command".to_string()).or_insert(cmd);
        }
    }
    Some(entry)
}

/// 把一条服务器序列化成一份 TOML 小节文本（`[mcp_servers.<name>]` 那一整段）。
///
/// `toml_splice::stringify` 输出的是整份文档；这里把「一条服务器」装进一个只含
/// 它自己的最小文档再序列化，得到的正好是 `splice_server` 要的那一段。
fn toml_section(name: &str, entry: &Value) -> Result<String, &'static str> {
    let mut servers = serde_json::Map::new();
    servers.insert(name.to_string(), entry.clone());
    let mut root = serde_json::Map::new();
    root.insert("mcp_servers".to_string(), Value::Object(servers));
    crate::toml_splice::stringify(&Value::Object(root)).map_err(|_| "mcp.shareUnsupported")
}

/* ------------------------------------------------------------------ */
/* 漂移（只报告，不写盘）                                               */
/* ------------------------------------------------------------------ */

/// 一处漂移：同一条服务器，库里是一份事实，某个 agent 的配置里是另一份。
///
/// `field` 只报**第一个**不同的字段，`libraryValue` / `agentValue` 给的是那一个
/// 字段的完整值 —— 界面上「以库为准」与「以该 agent 为准」两个动作都只需要知道
/// 这一个字段，整条服务器的全文留给 diff 视图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpDrift {
    pub name: String,
    pub agent_id: String,
    pub file: String,
    pub field: String,
    pub library_value: Value,
    pub agent_value: Value,
}

impl McpDrift {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "agentId": self.agent_id,
            "file": self.file,
            "field": self.field,
            "libraryValue": self.library_value,
            "agentValue": self.agent_value,
        })
    }
}

/// **纯只读**：只读库内记录与各 agent 的配置文件，不写一个字节。
///
/// 只比对「库里**和**这个 agent 上都有」的服务器：
/// - 库里没有的（agent 手写的、从别处拷来的）：一律不报告、不触碰；
/// - 库里有、这个 agent 上没有：这是「没启用」，不是漂移。
///
/// `allOf` 里显式写了 `false` 的 agent 是「用户把它标记为独立」，不比 ——
/// 报了只会让人以为该去改它。
pub fn plan_drift(
    library: &Library,
    user_home: &str,
    project_dir: Option<&str>,
    scope: &str,
) -> Vec<McpDrift> {
    let mut out = Vec::new();
    for rec in crate::library::mcp_records(library) {
        let Some(name) = rec.get("name").and_then(Value::as_str) else {
            continue;
        };
        let all_of = rec.get("allOf").and_then(Value::as_object);
        for agent in crate::adapters::AGENTS.iter() {
            if let Some(map) = all_of {
                if map.get(agent.id) == Some(&json!(false)) {
                    continue;
                }
            }
            let Some(want) = materialize_entry(agent.id, &rec) else {
                continue;
            };
            // **每一份**含这条服务器的配置都独立比对（不再只比第一份）：
            // codex 与 opencode 都声明了两份可物化配置（config.toml + mcp.json），
            // 只比第一份意味着第二份里的漂移**永远不会被发现**——用户在
            // mcp.json 里手改过参数这件事会静默存在，直到某次启用把它覆盖。
            // finding 按 (code, path) 去重，不同文件各报各的，互不吞并。
            for file in mcp_config_candidates(agent.id, scope, project_dir, user_home) {
                let Some(agent_value) = read_entry(&file, agent.id, name) else {
                    continue;
                };
                if let Some((field, library_value, agent_value)) =
                    first_field_diff(&want.entry, &agent_value)
                {
                    out.push(McpDrift {
                        name: name.to_string(),
                        agent_id: agent.id.to_string(),
                        file,
                        field,
                        library_value,
                        agent_value,
                    });
                }
            }
        }
    }
    out
}

/// 两份服务器配置里第一个不同的字段。
///
/// 只比这几类：传输方式、启动方式、开关。agent 自己多出来的调优字段
/// （`timeout` / `cwd` / `type` / `oauth`）不参与 —— 那不是「我们写的和库不一样」，
/// 而是「它多写了些别的东西」，覆盖它等于删除。键序不参与比较。
fn first_field_diff(want: &Value, got: &Value) -> Option<(String, Value, Value)> {
    const FIELDS: [&str; 7] = [
        "transport",
        "command",
        "args",
        "env",
        "url",
        "headers",
        "enabled",
    ];
    let empty = serde_json::Map::new();
    let w = want.as_object().unwrap_or(&empty);
    let g = got.as_object().unwrap_or(&empty);
    let implied = implied_transport(g);
    for k in FIELDS {
        match (w.get(k), g.get(k)) {
            (Some(x), Some(y)) if x == y => continue,
            (None, None) => continue,
            // `transport` 的缺省与显式写法等价：stdio 的服务器不写 `transport`
            // 也是 stdio，http/sse 靠 `url` 就能认出来。不归一这一层，每一份手写
            // 配置都会报一条假漂移 —— 而假漂移会让人去改一份本来正确的文件。
            (Some(x), None) if k == "transport" && implied.as_deref() == x.as_str() => continue,
            (None, Some(y)) if k == "transport" && implied.as_deref() == y.as_str() => continue,
            (a, b) => {
                return Some((
                    k.to_string(),
                    a.cloned().unwrap_or(Value::Null),
                    b.cloned().unwrap_or(Value::Null),
                ))
            }
        }
    }
    None
}

/// 一份服务器条目**隐含**的传输方式：写了 `url` 就是 http/sse（具体哪一种它自己
/// 说了算），否则 stdio。返回 `None` 表示「看不出来」—— 那时缺省就不是等价的，
/// 该报漂移就报。
fn implied_transport(entry: &serde_json::Map<String, Value>) -> Option<String> {
    let str_of = |k: &str| entry.get(k).and_then(Value::as_str).map(|s| s.to_string());
    if let Some(declared) = str_of("transport") {
        return Some(declared);
    }
    if let Some(kind) = str_of("type") {
        // opencode 的写法：`type` 就是传输方式（`local` != stdio 的拼写）。
        if kind == "local" {
            return Some("stdio".to_string());
        }
        if kind == "remote" || kind == "http" || kind == "sse" {
            return Some(kind);
        }
    }
    if entry
        .get("url")
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        // 有 `url` 却没有 `transport`：可能是 http，也可能是 sse。库里写的是哪一个
        // 就按哪一个 —— 但 `first_field_diff` 拿不到库里那一边，所以这里返回
        // `None`，让比较落回「报漂移」，宁可让人看一眼也不要静默认为一致。
        return None;
    }
    Some("stdio".to_string())
}

/* ------------------------------------------------------------------ */
/* 规则与记忆共享（M4）                                                  */
/* ------------------------------------------------------------------ */

/// 规则共享的两种模式（`docs/SHARED-ASSETS-IMPLEMENTATION.md` §6.2）。
///
/// | 模式 | 盘上是什么 | 适用 |
/// |---|---|---|
/// | `Link` | 一条指向库内主副本的链接 | 想让各处读同一份 |
/// | `Compose` | 文件里多一个带标记的区块 | 文件是用户自己写的，只能往里插 |
///
/// 两者的取舍不是风格问题：`Link` 会让 agent 侧那个路径**不再是用户的文件**
/// （用户在自己项目里编辑它，编辑的是库内那份）；`Compose` 保住用户的文件，
/// 但只在一个区块里生效。所以偏好由调用方显式给，本模块不猜。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RuleMode {
    Link,
    Compose,
}

impl RuleMode {
    /// 写进 IPC 与审计日志的字符串，渲染层只认这两个值。
    pub fn as_str(self) -> &'static str {
        match self {
            RuleMode::Link => "link",
            RuleMode::Compose => "compose",
        }
    }

    /// 严格匹配，未知值返回 `None` —— 与 `LinkMethod::parse` 同一立场。
    pub fn parse(s: &str) -> Option<RuleMode> {
        match s {
            "link" => Some(RuleMode::Link),
            "compose" => Some(RuleMode::Compose),
            _ => None,
        }
    }
}

/// 一个 agent 上某个 scope 下某一类（rule / memory）声明的全部路径，按声明顺序。
///
/// 返回 `None` 表示这个 agent 根本没声明这一类（`pi` 的 `rules` 是空的）：调用方
/// 要报可读的 code，而不是静默跳过。
pub fn kind_patterns(agent_id: &str, kind: &str, scope: &str) -> Option<Vec<String>> {
    let agent = adapters::by_id(agent_id)?;
    let rels: &[&str] = match (scope == "project", kind) {
        (true, "rule") => agent.projects.rules,
        (true, _) => agent.projects.memory,
        (false, "rule") => agent.globals.rules,
        (false, _) => agent.globals.memory,
    };
    Some(rels.iter().map(|s| s.to_string()).collect())
}

/// 一个 agent / scope 的基准目录（全局是 agent 的 home，项目是项目根）。
///
/// `None` 有两种原因，调用方分不清也不必分清：agent 不在表里，或者项目没注册。
pub fn scope_base(user_home: &str, agent_id: &str, scope: &str, project_id: Option<&str>) -> Option<String> {
    if scope != "project" {
        let agent = adapters::by_id(agent_id)?;
        return adapters::resolve_home(agent.home, user_home).or_else(|| Some(agent.home.to_string()));
    }
    let id = project_id?;
    registry::get(id)
        .and_then(|p| p.get("dir").and_then(Value::as_str).map(|s| s.to_string()))
}

/// 一个**已经决定了目标文件**的规则共享请求。
///
/// 「目标文件」有两种给法：`rel` + 从哪个基准目录取（`base`），或者直接给
/// `file` 绝对路径。后者是 M6 的矩阵用的 —— 它从扫描结果里拿到的是 `file`。
pub struct RuleTarget {
    pub base: String,
    pub rel: Option<String>,
    pub file: Option<String>,
}

impl RuleTarget {
    /// 解析成绝对路径。`rel` 优先，否则用 `file`。
    pub fn resolve(&self) -> Option<String> {
        if let Some(rel) = self.rel.as_deref() {
            if !rel.trim().is_empty() {
                return Some(store::native_str(&Path::new(&self.base).join(rel)));
            }
        }
        self.file
            .as_deref()
            .filter(|f| !f.trim().is_empty())
            .map(|f| store::native_str(Path::new(f)))
    }
}

/// 一次规则共享的规划结果。
///
/// `ops` 为空且 `code` 为 `None` 是不可能的：每条分支要么给 op，要么给 code。
pub struct RulePlan {
    /// 目标文件的绝对路径。
    pub file: String,
    pub mode: RuleMode,
    pub ops: Vec<Op>,
    /// 不发写入时的可读 code（`rule.foreignEdit` / `rule.noTarget` / …）。
    pub code: Option<String>,
    /// 与 code 一起回给渲染层的细节（三方 diff 用）。
    pub detail: Value,
}

impl RulePlan {
    fn refused(file: &str, mode: RuleMode, code: &str, detail: Value) -> RulePlan {
        RulePlan {
            file: file.to_string(),
            mode,
            ops: Vec::new(),
            code: Some(code.to_string()),
            detail,
        }
    }
}

/// 哪些扩展名的规则文件只能 `link`。
///
/// `.mdc`（Cursor）带 `alwaysApply` / `globs` frontmatter，区块拼接会让 frontmatter
/// 不再是文件的第一段 —— 而 agent 读的正是那一段。所以本期只支持 `link`，
/// `compose` 由调用方置灰并给出原因（§6.3）。
pub fn compose_unsupported(file: &str) -> Option<&'static str> {
    let low = file.to_lowercase();
    if low.ends_with(".mdc") {
        // 目录（`.cursor/rules`）本身没有扩展名问题，只有落到具体 `.mdc` 文件才算。
        Some("rule.mdcLinkOnly")
    } else {
        None
    }
}

/// 规则共享的两种模式，落到 op 上是什么。
///
/// `mode` 为空时按「能不能 compose」选：能 compose 就 compose —— 因为 `link` 会
/// 把用户的文件变成链接，那是更重的一步，不该是缺省。`.mdc` 没有 compose，于是
/// 缺省只能是 link。
pub fn rule_plan(
    library: &Library,
    target: &RuleTarget,
    mode: Option<RuleMode>,
    force: bool,
) -> RulePlan {
    let _ = force;
    let Some(file) = target.resolve() else {
        let m = mode.unwrap_or(RuleMode::Compose);
        return RulePlan::refused("", m, "rule.noTarget", json!({}));
    };
    // 模式没给时的缺省：能 compose 就 compose，不能（`.mdc`）就只能 link。
    //
    // 缺省**不走** `rule.mdcLinkOnly`：那个 code 的意思是「你**要求**了一件做不到
    // 的事」，是给 UI 把一个选项置灰用的；调用方根本没指定模式时，正确动作是挑一个
    // 做得到的做法，而不是报错。
    let mode = match (mode, compose_unsupported(&file)) {
        (None, None) => RuleMode::Compose,
        (None, Some(_)) => RuleMode::Link,
        (Some(RuleMode::Compose), Some(code)) => {
            return RulePlan::refused(&file, RuleMode::Compose, code, json!({ "file": file }))
        }
        (Some(m), _) => m,
    };

    // 库内主副本：`<root>/rules/<slug>.md`（记忆是 `<root>/memory/<slug>.md`）。
    // 名字取自目标文件的文件名 —— 一个 agent 的 `AGENTS.md` 与另一个 agent 的
    // `AGENTS.md` 是**同一件事的两种写法**，按文件名归一是让它们真的收敛成一份的
    // 前提（§12 第 7 条：文件名映射按 adapter-table 的实际声明，不硬编码）。
    let name = Path::new(&file)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| name.clone());
    let kind = if file.to_lowercase().contains("\\memory\\") || file.to_lowercase().contains("/memory/") {
        "memory"
    } else {
        "rule"
    };
    let slug_name = crate::model::slug(&stem);
    let keeper = store::native_str(&library.dir_for(kind).join(format!("{}.md", slug_name)));
    let keeper_path = Path::new(&keeper);
    let file_path = Path::new(&file);

    match mode {
        RuleMode::Link => {
            // 目标已经是指向库内主副本的链接：零写入（幂等）。
            if linkfs::is_link(file_path) && library.owns_target(file_path) {
                return RulePlan {
                    file,
                    mode,
                    ops: Vec::new(),
                    code: None,
                    detail: json!({ "alreadyLinked": true }),
                };
            }
            let mut ops = Vec::new();
            if let Some(parent) = file_path.parent() {
                ops.push(Op::mkdir(&store::native_str(parent)));
            }
            // 库内还没有主副本：让**目标文件本身**成为主副本。
            //
            // 两件事必须同时做到，否则会静默写坏（实测踩到）：
            //
            // 1. 用 `Op::write` 而不是 `copy_dir` —— 后者只遍历目录，拿它拷一个
            //    **文件**会得到一个空目录，随后 `link_file` 报 "link source is not a file"。
            // 2. 内容在**规划期**读好、随 op 传下去（`Op::write(path, content)` 的
            //    字面形式），**不能**用 `from` 让事务执行时再去读。原因：本事务的
            //    第 3 条 op 就是 `linkFile(dst = 这个文件)`，而 op 是按顺序执行的 ——
            //    等到执行 `write` 那一刻，目标已经是一条指向库内主副本的链接了。
            //    「从 from 读」于是读回链接**自己**，主副本里落的是一串路径文本。
            if !keeper_path.exists() {
                if !file_path.exists() {
                    // 目标还不存在、库里也没有：没有内容可 link。
                    return RulePlan::refused(&file, mode, "rule.noTarget", json!({ "file": file }));
                }
                // 读失败（权限、被占用）绝不能当成「文件是空的」：那会让主副本
                // 里落进一份空内容，随后启用把目标换成指向这份空主副本的链接
                // —— 用户的原文一个字节都找不回来。宁可拒绝，让人先解决占用。
                let Ok(body) = std::fs::read_to_string(file_path) else {
                    return RulePlan::refused(&file, mode, "rule.targetUnreadable", json!({ "file": file }));
                };
                ops.push(Op::write(&keeper, &body));
            }
            // 目标位置：真实文件先删，再建链接（顺序不能反，§11.3）。
            if file_path.exists() || linkfs::is_link(file_path) {
                ops.push(Op::delete(&file));
            }
            ops.push(Op::link_file(&file, &keeper));
            RulePlan {
                file,
                mode,
                ops,
                code: None,
                detail: json!({ "keeper": keeper, "slug": slug_name }),
            }
        }
        RuleMode::Compose => {
            // 目标文件与库内主副本**任一**读失败都拒绝（权限、被占用）：把失败
            // 当成空文件会让「圈区块」变成「清空文件」，那正是这里绝不做的事。
            let Ok(raw) = std::fs::read_to_string(file_path) else {
                return RulePlan::refused(&file, mode, "rule.targetUnreadable", json!({ "file": file }));
            };
            // 区块内容 = 库内主副本的内容。库里没有主副本时退回目标文件自己，
            // 于是第一次 compose 得到一个「原有内容被圈成受管区块」的文件
            // （一个字节都没被改写），用户看得见它成了受管区块。
            let body = if keeper_path.exists() {
                let Ok(keeper_body) = std::fs::read_to_string(keeper_path) else {
                    return RulePlan::refused(&file, mode, "rule.targetUnreadable", json!({ "file": keeper_path.to_string_lossy() }));
                };
                keeper_body
            } else {
                raw.clone()
            };
            // 写回闸门。
            //
            // 两个条件合起来才叫「我们以前生成过」，缺一不可：
            //   ① 盘上有一个同名区块；② 它的 `rev` 既是它自己内容的指纹，又是
            //     **库里这一份**的指纹。
            //
            // 只看 ① 会把「首次 compose 而库内主副本早已存在」误判成外部改动
            // （区块当时还不存在，`is_foreign_edit` 一律回 true），于是 M4 永远
            // 开不了头；只看 ② 又会让「用户改了区块内」蒙混过关。两条分开问，
            // 两种情形才不会互相掩护。
            //
            // 区块整个不见了（① 假、② 真）**不算**外部改动：那时用户只是把标记
            // 删了，把共享内容重新放回去正是他点这一下的意思；真正的数据风险在
            // 「区块还在、内容被改过」，那一种必须拒。
            let existing = compose::find_block(&raw, &slug_name);
            if let Some(b) = &existing {
                if compose::is_foreign_edit(&raw, &slug_name, &body) && !force {
                    // 闸门：区块被外部改过时拒绝。`force`（用户在两方 diff 里显式
                    // 选了「以库内为准」）是唯一绕过方式 —— 它是人对着 diff 做的
                    // 决定，不是调用方图省事的开关。
                    return RulePlan::refused(
                        &file,
                        mode,
                        "rule.foreignEdit",
                        json!({
                            "file": file,
                            "slug": slug_name,
                            "onDisk": b.body,
                            "want": body,
                        }),
                    );
                }
            }
            let next = compose::upsert_block(&raw, &slug_name, &body);
            if next == raw && keeper_path.exists() {
                // 区块已经在位、内容一致、库里也有主副本：零写入、零撤销快照
                //（幂等的前提）。
                //
                // `keeper_path.exists()` 是 §26.2 补的条件：compose 分支此前
                // **从不写库内主副本**——首次 compose 后库里什么都没有，于是
                // 每次同步都退回「拿盘上自己的内容当库内版本」，区块被外部
                // 改过后 force 重建会得到嵌套区块（区块里套区块）。「单一
                // 事实源」对 compose 也必须成立：第一次启用就把主副本落进库。
                return RulePlan {
                    file,
                    mode,
                    ops: Vec::new(),
                    code: None,
                    detail: json!({ "unchanged": true, "slug": slug_name }),
                };
            }
            let mut ops = vec![Op::write(&file, &next)];
            if !keeper_path.exists() {
                // 首次 compose：库内主副本 = 区块 body（此刻与用户原文一致）。
                // 与目标文件的写入在**同一个事务**里，要么都落、要么都不落。
                ops.push(Op::write(
                    &keeper,
                    &compose::find_block(&next, &slug_name)
                        .map(|b| b.body)
                        .unwrap_or_else(|| body.clone()),
                ));
            }
            RulePlan {
                file: file.clone(),
                mode,
                ops,
                code: None,
                detail: json!({ "keeper": keeper, "slug": slug_name }),
            }
        }
    }
}

/// 去掉一个文件里的库内区块（`compose` 的「停用」）。
///
/// 只删区块，文件本身留着 —— `compose` 从来没有把文件变成链接，停用它就是把
/// 我们放进去的那一截拿走。区块之外的内容一个字节都不动。
pub fn uncompose_plan(file: &str, slug: &str) -> RulePlan {
    // 读失败不能当成空文件：`remove_block("") == ""` 会让这次停用被当成
    // 「本来就没有区块」而静默成功，用户以为已停用、盘上却什么都没动。
    let Ok(raw) = std::fs::read_to_string(file) else {
        return RulePlan::refused(file, RuleMode::Compose, "rule.targetUnreadable", json!({ "file": file, "slug": slug }));
    };
    let next = compose::remove_block(&raw, slug);
    RulePlan {
        file: file.to_string(),
        mode: RuleMode::Compose,
        ops: if next == raw {
            Vec::new()
        } else {
            vec![Op::write(file, &next)]
        },
        code: None,
        detail: json!({ "slug": slug }),
    }
}

/* ------------------------------------------------------------------ */
/* 单元测试                                                            */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            store::rmrf(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ak-sharing-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Tmp(dir)
    }

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).expect("parent");
        }
        std::fs::write(path, body).expect("write");
    }

    fn cap_with_links() -> LinkCap {
        LinkCap {
            symlink_dir: true,
            symlink_file: true,
            junction: true,
            hardlink: true,
            dir_order: vec![LinkMethod::Symlink, LinkMethod::Junction],
            file_order: vec![LinkMethod::Symlink, LinkMethod::Hardlink],
            probed_at: String::new(),
        }
    }

    fn cap_without_links() -> LinkCap {
        LinkCap {
            symlink_dir: false,
            symlink_file: false,
            junction: false,
            hardlink: false,
            dir_order: vec![LinkMethod::Copy],
            file_order: vec![LinkMethod::Copy],
            probed_at: String::new(),
        }
    }

    use crate::linkfs::LinkMethod;

    /// 摘要口径：同一内容三份 → 同一个 digest；改一个字节 → 变。
    #[test]
    fn a_tree_digest_tracks_content_and_paths() {
        let t = tmp("digest");
        let a = t.0.join("a");
        let b = t.0.join("b");
        write(&a.join("SKILL.md"), "# A\n");
        write(&a.join("sub/NOTES.md"), "notes\n");
        write(&b.join("SKILL.md"), "# A\n");
        write(&b.join("sub/NOTES.md"), "notes\n");
        assert_eq!(digest_dir(&a).digest, digest_dir(&b).digest);

        // 内容变一个字节
        write(&b.join("sub/NOTES.md"), "notes!\n");
        assert_ne!(digest_dir(&a).digest, digest_dir(&b).digest);

        // 只改文件名：摘要也必须变（否则「同一个 rel 集合」这条前提就形同虚设）
        let c = t.0.join("c");
        write(&c.join("SKILL.md"), "# A\n");
        write(&c.join("sub/OTHER.md"), "notes\n");
        assert_ne!(digest_dir(&a).digest, digest_dir(&c).digest);
    }

    /// 摘要不把构建树算进去（`SKIP_DIRS` 是同一份常量）。
    #[test]
    fn a_tree_digest_skips_build_trees() {
        let t = tmp("skip");
        let a = t.0.join("a");
        write(&a.join("SKILL.md"), "# A\n");
        let plain = digest_dir(&a).digest;
        write(&a.join("node_modules/x/index.js"), "build\n");
        assert_eq!(digest_dir(&a).digest, plain);
    }

    /// 逐文件差异的三种状态，以及「内容相同、种类不同」的 `kind` 状态。
    #[test]
    fn diff_reports_only_a_only_b_different_and_kind() {
        let t = tmp("diff");
        let a = t.0.join("a");
        let b = t.0.join("b");
        write(&a.join("SKILL.md"), "same\n");
        write(&b.join("SKILL.md"), "same\n");
        write(&a.join("only-a.md"), "a\n");
        write(&b.join("only-b.md"), "b\n");
        write(&a.join("both.md"), "one\n");
        write(&b.join("both.md"), "two\n");
        // b 里把 both2.md 做成一条指向同内容的链接：内容一样、种类不同。
        write(&a.join("both2.md"), "x\n");
        write(&b.join("tgt.md"), "x\n");
        std::fs::create_dir_all(&b).expect("b");
        // 硬链接不需要任何特权，所以这条「同内容、不同种类」的夹具在哪台机器上都能造出来。
        let linked = crate::linkfs::create_file_link(
            &b.join("tgt.md"),
            &b.join("both2.md"),
            Some(LinkMethod::Hardlink),
        );
        if linked.is_ok() {
            let la = digest_dir(&a);
            let lb = digest_dir(&b);
            let states: Vec<(String, String)> = diff_trees(&la, &lb)
                .into_iter()
                .map(|d| (d.rel, d.state))
                .collect();
            assert!(states.contains(&("only-a.md".into(), "only-a".into())));
            assert!(states.contains(&("only-b.md".into(), "only-b".into())));
            assert!(states.contains(&("both.md".into(), "different".into())));
            assert!(states.contains(&("both2.md".into(), "kind".into())));
        }
    }

    /// 三份全等的 skill → `Identical`，保留一份、替换两处，省下的字节是两份之和。
    #[test]
    fn identical_copies_converge_to_one_keeper() {
        let t = tmp("identical");
        let home = t.0.join("home");
        let mut assets = Vec::new();
        for agent in ["a", "b", "c"] {
            let dir = home.join(agent).join("skills").join("pdf");
            write(&dir.join("SKILL.md"), "---\nname: pdf\n---\nbody\n");
            write(&dir.join("assets/ref.txt"), "reference\n");
            assets.push(json!({
                "kind": "skill",
                "name": "pdf",
                "dir": store::native_str(&dir),
                "agentId": agent,
                "agentLabel": agent,
                "scope": "global",
                "projectId": null
            }));
        }
        let lib = Library::at(t.0.join("lib"));
        let plan = plan_converge(&assets, &lib, &cap_with_links());
        assert_eq!(plan.groups.len(), 1);
        let g = &plan.groups[0];
        assert_eq!(g.verdict, Verdict::Identical);
        assert_eq!(g.replace.len(), 2);
        assert_eq!(g.keeper.as_deref(), Some(assets[0]["dir"].as_str().unwrap()));
        assert!(plan.saved_bytes > 0);
        assert_eq!(plan.actionable(), 1);
    }

    /// 一份内容不同 → `Diverged`，**不动**，并列出差异文件名。
    #[test]
    fn diverged_copies_are_reported_and_left_alone() {
        let t = tmp("diverged");
        let home = t.0.join("home");
        let a = home.join("a/skills/pdf");
        let b = home.join("b/skills/pdf");
        write(&a.join("SKILL.md"), "one\n");
        write(&b.join("SKILL.md"), "two\n");
        let assets = vec![
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&a),"agentId":"a","agentLabel":"a","scope":"global"}),
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&b),"agentId":"b","agentLabel":"b","scope":"global"}),
        ];
        let plan = plan_converge(&assets, &Library::at(t.0.join("lib")), &cap_with_links());
        let g = &plan.groups[0];
        assert_eq!(g.verdict, Verdict::Diverged);
        assert!(g.replace.is_empty());
        assert_eq!(plan.saved_bytes, 0);
        assert_eq!(plan.actionable(), 0);
        assert_eq!(g.diffs.len(), 1);
        assert_eq!(g.diffs[0].rel, "SKILL.md");
        assert_eq!(g.diffs[0].state, "different");
    }

    /// 只有一份 → `SingleSource`：并入库、原位置换成链接。
    #[test]
    fn a_single_copy_is_a_single_source() {
        let t = tmp("single");
        let a = t.0.join("home/a/skills/pdf");
        write(&a.join("SKILL.md"), "only\n");
        let assets = vec![
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&a),"agentId":"a","agentLabel":"a","scope":"global"}),
        ];
        let plan = plan_converge(&assets, &Library::at(t.0.join("lib")), &cap_with_links());
        let g = &plan.groups[0];
        assert_eq!(g.verdict, Verdict::SingleSource);
        assert_eq!(g.replace.len(), 1);
    }

    /// 默认库根（`~/.agents`）下，库内主副本会**同时**被扫描成伪 agent `shared`
    /// 的资产。回归：它只能算一份 —— 组是 `single`、`copies` 只有一条、`replace`
    /// 为空，绝不出现「保留库内路径、再把库内路径替换成指向自己的链接」。
    #[test]
    fn the_library_copy_is_counted_once_when_the_shared_agent_reports_it() {
        let t = tmp("libdup");
        let lib = Library::at(t.0.join("home/.agents"));
        let keeper = lib.skill_dir("team-wiki");
        write(&keeper.join("SKILL.md"), "body\n");
        let assets = vec![json!({
            "kind": "skill",
            "name": "team-wiki",
            "dir": store::native_str(&keeper),
            "agentId": "shared",
            "agentLabel": "Shared (~/.agents)",
            "scope": "global",
        })];
        let plan = plan_converge(&assets, &lib, &cap_with_links());
        assert_eq!(plan.groups.len(), 1);
        let g = &plan.groups[0];
        assert_eq!(g.verdict, Verdict::SingleSource);
        assert_eq!(g.copies.len(), 1);
        assert_eq!(g.copies[0].path, store::native_str(&keeper));
        assert!(g.replace.is_empty());
        assert_eq!(g.keeper.as_deref(), Some(g.copies[0].path.as_str()));
        assert_eq!(plan.actionable(), 0, "没有可替换的位置，不计入可一键收敛");
    }

    /// 库内一份 + 另一个 agent 一份全等（且伪 agent `shared` 也报了库内那份）：
    /// 结果是 `identical`，保留方是库内路径，`replace` 只含 agent 那份。
    #[test]
    fn a_library_copy_and_an_agent_copy_converge_without_replacing_the_library() {
        let t = tmp("libagent");
        let lib = Library::at(t.0.join("home/.agents"));
        let keeper = lib.skill_dir("team-wiki");
        let agent_copy = t.0.join("home/.claude/skills/team-wiki");
        write(&keeper.join("SKILL.md"), "body\n");
        write(&agent_copy.join("SKILL.md"), "body\n");
        let assets = vec![
            json!({
                "kind": "skill", "name": "team-wiki",
                "dir": store::native_str(&agent_copy),
                "agentId": "claude-code", "agentLabel": "Claude Code", "scope": "global",
            }),
            json!({
                "kind": "skill", "name": "team-wiki",
                "dir": store::native_str(&keeper),
                "agentId": "shared", "agentLabel": "Shared (~/.agents)", "scope": "global",
            }),
        ];
        let plan = plan_converge(&assets, &lib, &cap_with_links());
        assert_eq!(plan.groups.len(), 1);
        let g = &plan.groups[0];
        let keeper_str = store::native_str(&keeper);
        let agent_str = store::native_str(&agent_copy);
        assert_eq!(g.verdict, Verdict::Identical);
        assert_eq!(g.keeper.as_deref(), Some(keeper_str.as_str()));
        assert_eq!(g.replace.len(), 1);
        assert_eq!(g.replace[0].path, agent_str);
        assert_eq!(g.copies.len(), 2);
        assert_eq!(plan.actionable(), 1);
    }

    /// 本机没有链接能力时：给出分组解释原因，但**绝不**建议替换。
    #[test]
    fn no_link_capability_never_proposes_replacement() {
        let t = tmp("nocap");
        let a = t.0.join("home/a/skills/pdf");
        let b = t.0.join("home/b/skills/pdf");
        write(&a.join("SKILL.md"), "x\n");
        write(&b.join("SKILL.md"), "x\n");
        let assets = vec![
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&a),"agentId":"a","agentLabel":"a","scope":"global"}),
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&b),"agentId":"b","agentLabel":"b","scope":"global"}),
        ];
        let plan = plan_converge(&assets, &Library::at(t.0.join("lib")), &cap_without_links());
        assert!(plan.groups.iter().all(|g| g.replace.is_empty()));
        assert_eq!(plan.saved_bytes, 0);
    }

    /// 非 skill 的资产不参与收敛。
    #[test]
    fn only_skills_are_considered() {
        let t = tmp("kinds");
        let a = t.0.join("home/a/rules");
        write(&a.join("AGENTS.md"), "x\n");
        let assets = vec![json!({"kind":"rule","name":"AGENTS","dir":store::native_str(&a),"agentId":"a","scope":"global"})];
        let plan = plan_converge(&assets, &Library::at(t.0.join("lib")), &cap_with_links());
        assert!(plan.groups.is_empty());
    }

    /// 已经收敛过的组（一份真实副本 + 若干链接）不再出现在方案里。
    #[test]
    fn an_already_converged_group_is_not_proposed_again() {
        let t = tmp("converged");
        let lib = Library::at(t.0.join("lib"));
        let keeper = lib.skill_dir("pdf");
        write(&keeper.join("SKILL.md"), "x\n");
        let link_dir = t.0.join("home/a/skills/pdf");
        std::fs::create_dir_all(link_dir.parent().unwrap()).expect("parent");
        let method = crate::linkfs::create_dir_link(&keeper, &link_dir, None).expect("link");
        assert!(method.is_link());
        let assets = vec![
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&link_dir),"agentId":"a","agentLabel":"a","scope":"global"}),
            json!({"kind":"skill","name":"pdf","dir":store::native_str(&keeper),"agentId":"library","agentLabel":"Library","scope":"library"}),
        ];
        let plan = plan_converge(&assets, &lib, &cap_with_links());
        assert!(plan.groups.is_empty(), "already converged");
    }
    /* ------------------------ MCP（M3 · 单一事实源） ------------------------ */

    /// 一条库内规范形态的记录（stdio）。
    fn record(name: &str) -> Value {
        json!({
            "schema": MCP_SCHEMA,
            "name": name,
            "transport": "stdio",
            "command": "uvx",
            "args": ["mcp-server-fetch"],
            "env": {},
            "url": null,
            "headers": {},
            "allOf": { "claude-code": true, "codex": true, "opencode": true }
        })
    }

    /// 规范形态 → 写盘形态：只带这一种传输方式该有的字段，空的 `args`/`env` 不落盘。
    #[test]
    fn materialize_drops_empty_fields_and_keeps_the_transport() {
        let m = materialize_entry("claude-code", &record("fetch")).expect("materialised");
        assert_eq!(m.key, "fetch");
        let e = m.entry.as_object().expect("object");
        assert_eq!(e.get("transport"), Some(&json!("stdio")));
        assert_eq!(e.get("command"), Some(&json!("uvx")));
        assert_eq!(e.get("args"), Some(&json!(["mcp-server-fetch"])));
        assert!(e.get("env").is_none(), "an empty env is not written");
        assert!(e.get("url").is_none(), "a stdio server has no url");
        assert!(e.get("allOf").is_none(), "allOf is bookkeeping");
    }

    /// http 只带 url/headers；`command` 不会跟着过去。
    #[test]
    fn materialize_an_http_record_keeps_only_url_and_headers() {
        let rec = json!({
            "name": "web", "transport": "http", "url": "http://localhost:3000/mcp",
            "headers": {"Authorization": "Bearer x"}, "command": "npx", "args": ["ignored"]
        });
        let m = materialize_entry("codex", &rec).expect("materialised");
        let e = m.entry.as_object().expect("object");
        assert_eq!(e.get("url"), Some(&json!("http://localhost:3000/mcp")));
        assert!(e.get("headers").is_some());
        assert!(e.get("command").is_none());
        assert!(e.get("args").is_none());
    }

    /// 一条没有名字的记录物化不了 —— 调用方要拿到一个**可读的 code**，而不是空对象。
    #[test]
    fn a_record_without_a_name_does_not_materialise() {
        assert!(materialize_entry("codex", &json!({"transport": "stdio"})).is_none());
        let err = open_ops("nope.json", "codex", &json!({"transport": "stdio"}))
            .err()
            .expect("refused");
        assert_eq!(err, "mcp.noRecordInLibrary");
    }

    /// JSON：打开时只插入/覆盖这一条，兄弟服务器与键序保持。
    #[test]
    fn open_ops_inserts_one_server_and_keeps_the_siblings() {
        let t = tmp("mcp-open-json");
        let file = t.0.join("mcp.json");
        write(
            &file,
            "{\n  \"mcpServers\": {\n    \"keep\": {\n      \"command\": \"a\"\n    }\n  },\n  \"other\": 1\n}\n",
        );
        let p = store::native_str(&file);
        let ops = open_ops(&p, "claude-code", &record("fetch")).expect("ops").expect("write");
        assert_eq!(ops.len(), 1);
        // 直接跑 op 的变更函数（不落盘，只验证形状）。
        let cur: Value = serde_json::from_str(&read_text(&p)).expect("json");
        let f = ops[0].mutate.as_ref().expect("mutate");
        let next = f(cur).expect("mutated");
        let keys: Vec<&String> = next.as_object().expect("object").keys().collect();
        assert_eq!(keys, vec!["mcpServers", "other"], "the surviving key order is kept");
        let servers: Vec<&String> = next["mcpServers"].as_object().expect("servers").keys().collect();
        assert_eq!(servers, vec!["keep", "fetch"], "the new server is appended");
        assert_eq!(next["mcpServers"]["keep"]["command"], json!("a"));
    }

    /// 同一份记录物化两遍：第二遍零写入（幂等）。
    #[test]
    fn opening_the_same_record_twice_writes_nothing_the_second_time() {
        let t = tmp("mcp-idempotent");
        let file = t.0.join("mcp.json");
        let rec = record("fetch");
        let m = materialize_entry("claude-code", &rec).expect("materialised");
        let body = json!({ "mcpServers": { "fetch": m.entry } });
        write(&file, &format!("{}\n", serde_json::to_string(&body).unwrap()));
        let p = store::native_str(&file);
        assert!(open_ops(&p, "claude-code", &rec).expect("ok").is_none());
    }

    /// TOML：打开只替换该服务器那一段，注释与小节顺序逐字节保留。
    #[test]
    fn open_ops_keeps_toml_comments_and_section_order() {
        let t = tmp("mcp-open-toml");
        let file = t.0.join("config.toml");
        let raw = "# top comment\nmodel = \"gpt\"\n\n[mcp_servers.alpha]\n# keep me\ncommand = \"a\"\n\n[other]\nk = 1\n";
        write(&file, raw);
        let p = store::native_str(&file);
        let ops = open_ops(&p, "codex", &record("fetch")).expect("ops").expect("write");
        let body = ops[0].content.as_deref().expect("content");
        assert!(body.contains("# top comment"), "the header comment survives");
        assert!(body.contains("# keep me"), "the sibling section's comment survives");
        assert!(body.contains("[other]"), "the sibling section survives");
        assert!(body.contains("[mcp_servers.fetch]"), "the new section is appended");
        // 段落之外的部分逐字节不变：原文去掉末尾空白后是它的前缀。
        assert!(body.starts_with(raw.trim_end()));
    }

    /// TOML 关闭：只删那一段，其余字节不变。
    #[test]
    fn close_ops_removes_only_the_toml_section() {
        let t = tmp("mcp-close-toml");
        let file = t.0.join("config.toml");
        let raw = "# head\n\n[mcp_servers.alpha]\ncommand = \"a\"\n\n[mcp_servers.beta]\n# beta note\ncommand = \"b\"\n";
        write(&file, raw);
        let p = store::native_str(&file);
        let ops = close_ops(&p, "codex", "alpha", crate::adapters::DisableStyle::Remove)
            .expect("ops")
            .expect("write");
        let body = ops[0].content.as_deref().expect("content");
        assert!(!body.contains("mcp_servers.alpha"));
        assert!(body.contains("# head"));
        assert!(body.contains("# beta note"), "the other section is untouched");
    }

    /// `Flag` 风格：条目留着，只写 `enabled = false`。
    #[test]
    fn close_ops_with_flag_keeps_the_entry_and_only_sets_enabled_false() {
        let t = tmp("mcp-close-flag");
        let file = t.0.join("opencode.json");
        write(
            &file,
            "{\n  \"mcp\": {\n    \"fetch\": {\n      \"command\": \"a\"\n    }\n  }\n}\n",
        );
        let p = store::native_str(&file);
        // opencode 用的是第二个桶名？这里只验证 JSON 路径按 `mcpServers` 工作，
        // 桶名由 `bucket_for` 决定 —— 所以测试里显式造 `mcpServers`。
        let file2 = t.0.join("opencode2.json");
        write(&file2, "{ \"mcpServers\": { \"fetch\": { \"command\": \"a\" } } }\n");
        let p2 = store::native_str(&file2);
        let ops = close_ops(&p2, "opencode", "fetch", crate::adapters::DisableStyle::Flag)
            .expect("ops")
            .expect("write");
        let cur: Value = serde_json::from_str(&read_text(&p2)).expect("json");
        let next = (ops[0].mutate.as_ref().expect("mutate"))(cur).expect("mutated");
        assert_eq!(next["mcpServers"]["fetch"]["command"], json!("a"));
        assert_eq!(next["mcpServers"]["fetch"]["enabled"], json!(false));
        let _ = p;
    }

    /// 关掉一条本来就不存在的服务器：零写入（幂等，不是「写一个空桶」）。
    #[test]
    fn closing_a_missing_server_writes_nothing() {
        let t = tmp("mcp-close-missing");
        let file = t.0.join("mcp.json");
        write(&file, "{ \"mcpServers\": { \"keep\": { \"command\": \"a\" } } }\n");
        let p = store::native_str(&file);
        assert!(close_ops(&p, "claude-code", "gone", crate::adapters::DisableStyle::Remove)
            .expect("ok")
            .is_none());
        let toml = t.0.join("c.toml");
        write(&toml, "[mcp_servers.keep]\ncommand = \"a\"\n");
        let tp = store::native_str(&toml);
        assert!(close_ops(&tp, "codex", "gone", crate::adapters::DisableStyle::Remove)
            .expect("ok")
            .is_none());
    }

    /// YAML 里的「只动一条」这份语言表达不了 → 一个可读的 code，而不是尽力而为。
    #[test]
    fn yaml_configs_are_refused_with_a_code() {
        let t = tmp("mcp-yaml");
        let file = t.0.join("config.yaml");
        write(&file, "mcpServers:\n  fetch:\n    command: a\n");
        let p = store::native_str(&file);
        assert_eq!(
            open_ops(&p, "continue", &record("fetch")).err().expect("refused"),
            "mcp.shareUnsupported"
        );
        assert_eq!(
            close_ops(&p, "continue", "fetch", crate::adapters::DisableStyle::Remove)
                .err()
                .expect("refused"),
            "mcp.shareUnsupported"
        );
    }

    /// 候选文件：记账文件与 YAML 都不是候选；全局与项目各按声明顺序给。
    #[test]
    fn mcp_config_candidates_skip_bookkeeping_files() {
        let g = mcp_config_candidates("workbuddy", "global", None, "C:\\home");
        assert_eq!(g.len(), 1, "{:?}", g);
        assert!(g[0].ends_with("mcp.json"));
        let c = mcp_config_candidates("codex", "global", None, "C:\\home");
        assert!(c[0].ends_with("config.toml"), "{:?}", c);
        let p = mcp_config_candidates("codex", "project", Some("C:\\proj"), "C:\\home");
        assert!(p[0].ends_with("config.toml") && p[0].starts_with("C:\\proj"), "{:?}", p);
        assert!(mcp_config_candidates("not-an-agent", "global", None, "C:\\home").is_empty());
    }

    /// 漂移只看「库里和 agent 上都有」的那一条：库里没有的（手写的）零报告。
    #[test]
    fn drift_ignores_servers_that_are_not_in_the_library() {
        let t = tmp("mcp-drift-outside");
        let lib = Library::at(t.0.join("lib"));
        let file = lib.mcp_record_path("fetch");
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("dir");
        }
        write(&file, &format!("{}\n", serde_json::to_string(&record("fetch")).unwrap()));
        // 一份手写配置：库里没有 `handwritten`；库里有的 `fetch` 参数不同。
        let home = t.0.join("home");
        write(
            &home.join(".claude").join(".mcp.json"),
            r#"{"mcpServers":{"handwritten":{"command":"z"},"fetch":{"command":"other"}}}"#,
        );
        let drifts = plan_drift(&lib, &store::native_str(&home), None, "global");
        assert_eq!(drifts.len(), 1, "{:?}", drifts);
        assert_eq!(drifts[0].name, "fetch");
        assert_eq!(drifts[0].agent_id, "claude-code");
        assert_eq!(drifts[0].field, "command");
        assert_eq!(drifts[0].library_value, json!("uvx"));
        assert_eq!(drifts[0].agent_value, json!("other"));
    }

    /// agent 自己多写的字段不算漂移（覆盖它等于删掉用户写的东西）。
    #[test]
    fn drift_ignores_extra_fields_the_agent_added() {
        let t = tmp("mcp-drift-extra");
        let lib = Library::at(t.0.join("lib"));
        let file = lib.mcp_record_path("fetch");
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("dir");
        }
        write(&file, &format!("{}\n", serde_json::to_string(&record("fetch")).unwrap()));
        let home = t.0.join("home");
        let mut entry = materialize_entry("claude-code", &record("fetch")).unwrap().entry;
        entry["timeout"] = json!(5000);
        entry["type"] = json!("stdio");
        write(
            &home.join(".claude").join(".mcp.json"),
            &format!(
                "{}\n",
                serde_json::to_string(&json!({ "mcpServers": { "fetch": entry } })).unwrap()
            ),
        );
        let drifts = plan_drift(&lib, &store::native_str(&home), None, "global");
        assert!(drifts.is_empty(), "{:?}", drifts);
    }

    /// 库里有、agent 上没有：这是「没启用」，不是漂移。
    #[test]
    fn drift_ignores_a_server_that_is_not_enabled_on_that_agent() {
        let t = tmp("mcp-drift-absent");
        let lib = Library::at(t.0.join("lib"));
        let file = lib.mcp_record_path("fetch");
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("dir");
        }
        write(&file, &format!("{}\n", serde_json::to_string(&record("fetch")).unwrap()));
        let home = t.0.join("home");
        write(&home.join(".claude").join(".mcp.json"), "{ \"mcpServers\": {} }\n");
        assert!(plan_drift(&lib, &store::native_str(&home), None, "global").is_empty());
    }

    /// `allOf` 里标了 `false` 的 agent 是「用户标记为独立」，不比。
    #[test]
    fn drift_skips_agents_marked_independent() {
        let t = tmp("mcp-drift-independent");
        let lib = Library::at(t.0.join("lib"));
        let mut rec = record("fetch");
        rec["allOf"] = json!({ "claude-code": false });
        let file = lib.mcp_record_path("fetch");
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("dir");
        }
        write(&file, &format!("{}\n", serde_json::to_string(&rec).unwrap()));
        let home = t.0.join("home");
        write(
            &home.join(".claude").join(".mcp.json"),
            r#"{"mcpServers":{"fetch":{"command":"other"}}}"#,
        );
        assert!(plan_drift(&lib, &store::native_str(&home), None, "global").is_empty());
    }

    /// 漂移是**只读**的：报告完，盘上一个字节都没变。
    #[test]
    fn drift_never_writes() {
        let t = tmp("mcp-drift-readonly");
        let lib = Library::at(t.0.join("lib"));
        let file = lib.mcp_record_path("fetch");
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("dir");
        }
        let rec_body = format!("{}\n", serde_json::to_string(&record("fetch")).unwrap());
        write(&file, &rec_body);
        let home = t.0.join("home");
        let cfg = home.join(".claude").join(".mcp.json");
        let cfg_body = "{\n  \"mcpServers\": {\n    \"fetch\": {\n      \"command\": \"other\"\n    }\n  }\n}\n";
        write(&cfg, cfg_body);
        let before = list_all(&t.0);
        let drifts = plan_drift(&lib, &store::native_str(&home), None, "global");
        assert!(!drifts.is_empty());
        assert_eq!(list_all(&t.0), before, "reading drift writes nothing");
        assert_eq!(read_text(&store::native_str(&cfg)), cfg_body);
        assert_eq!(read_text(&store::native_str(&file)), rec_body);
    }

    /// 一份临时树里所有文件的 (路径, 字节数, 内容) 列表 —— 「零写入」的判据。
    fn list_all(root: &Path) -> Vec<(String, usize, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let body = std::fs::read(&p).unwrap_or_default();
                    out.push((
                        store::native_str(&p),
                        body.len(),
                        String::from_utf8_lossy(&body).into_owned(),
                    ));
                }
            }
        }
        out.sort();
        out
    }

    /* -------------------------------------------------------------- */
    /* 规则共享（M4）                                                   */
    /* -------------------------------------------------------------- */

    /// 一个库 + 一个 agent 侧的规则文件。
    fn rule_fixture(tag: &str, file_name: &str, content: &str) -> (Tmp, Library, String) {
        let t = tmp(tag);
        let lib = Library::at(t.0.join("library"));
        let file = t.0.join("home").join(file_name);
        if let Some(p) = file.parent() {
            std::fs::create_dir_all(p).expect("home");
        }
        if !content.is_empty() {
            std::fs::write(&file, content).expect("seed");
        }
        let path = store::native_str(&file);
        (t, lib, path)
    }

    fn at(file: &str) -> RuleTarget {
        RuleTarget {
            base: String::new(),
            rel: None,
            file: Some(file.to_string()),
        }
    }

    /// 模式字符串严格解析：未知值不是「猜一个」，是 `None`。
    #[test]
    fn rule_mode_parses_strictly() {
        assert_eq!(RuleMode::parse("link"), Some(RuleMode::Link));
        assert_eq!(RuleMode::parse("compose"), Some(RuleMode::Compose));
        assert_eq!(RuleMode::parse("Link"), None);
        assert_eq!(RuleMode::parse(""), None);
        assert_eq!(RuleMode::Link.as_str(), "link");
        assert_eq!(RuleMode::Compose.as_str(), "compose");
    }

    /// `rel` 优先于 `file`；两者都没有时解析不出来（调用方要报 `rule.noTarget`）。
    #[test]
    fn a_rule_target_prefers_rel_then_file() {
        let t = RuleTarget {
            base: "C:\\home".to_string(),
            rel: Some("AGENTS.md".to_string()),
            file: Some("C:\\other\\X.md".to_string()),
        };
        assert_eq!(t.resolve(), Some("C:\\home\\AGENTS.md".to_string()));
        let only_file = RuleTarget {
            base: String::new(),
            rel: Some("  ".to_string()),
            file: Some("C:\\other\\X.md".to_string()),
        };
        assert_eq!(only_file.resolve(), Some("C:\\other\\X.md".to_string()));
        let none = RuleTarget {
            base: String::new(),
            rel: None,
            file: None,
        };
        assert_eq!(none.resolve(), None);
    }

    /// 目标解析不出来时报 `rule.noTarget`，且不产生任何 op。
    #[test]
    fn a_target_without_a_path_is_refused_with_a_code() {
        let t = tmp("rule-notarget");
        let lib = Library::at(t.0.join("library"));
        let plan = rule_plan(
            &lib,
            &RuleTarget {
                base: String::new(),
                rel: None,
                file: None,
            },
            None,
            false,
        );
        assert_eq!(plan.code.as_deref(), Some("rule.noTarget"));
        assert!(plan.ops.is_empty());
    }

    /// `.mdc` 只能 link：compose 被拒绝，且理由是一个 code（UI 置灰要用它）。
    #[test]
    fn an_mdc_file_refuses_compose_and_says_why() {
        let (t, lib, file) = rule_fixture("rule-mdc", "a.mdc", "---\nalwaysApply: true\n---\nbody\n");
        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(plan.code.as_deref(), Some("rule.mdcLinkOnly"));
        assert!(plan.ops.is_empty());
        // 文件一个字节都不动。
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "---\nalwaysApply: true\n---\nbody\n"
        );
        drop(t);
    }

    /// link 模式：目标变成指向库内主副本的链接，内容逐字节保留。
    #[test]
    fn link_mode_stores_the_file_in_the_library_and_links_to_it() {
        let (t, lib, file) = rule_fixture("rule-link", "AGENTS.md", "# team rules\n");
        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Link), false);
        assert_eq!(plan.code, None, "{:?}", plan.detail);
        assert_eq!(plan.mode.as_str(), "link");
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let tx = store.transaction(&plan.ops, &json!({ "kind": "rule" }));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(linkfs::is_link(Path::new(&file)), "目标应该是链接");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# team rules\n");
        // 库内主副本按文件名归一：`AGENTS.md` → `<root>/rules/agents.md`。
        let keeper = lib.dir_for("rule").join("agents.md");
        assert!(keeper.exists(), "库内主副本应在 {:?}", keeper);
        // 主副本里是**内容**，不是一条指向自己的路径文本。
        //
        // 这一条钉住的是一个实测踩到的缺陷：`Op::write` 若用 `from` 让事务在执行
        // 时去读来源，而同一个事务里随后那条 `linkFile` 又把这个来源换成了指向
        // 主副本的链接，那么写进主副本的就是「链接自己」这个路径字符串。整个
        // 事务成功、界面全绿、内容却已经没了。
        assert_eq!(
            std::fs::read_to_string(&keeper).unwrap(),
            "# team rules\n",
            "库内主副本必须是原文，不能是指向自己的路径"
        );
        drop(t);
    }

    /// link 模式幂等：已经是指向库内的链接时零写入（零撤销快照）。
    #[test]
    fn link_mode_writes_nothing_when_already_linked() {
        let (t, lib, file) = rule_fixture("rule-link-twice", "AGENTS.md", "# x\n");
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let first = rule_plan(&lib, &at(&file), Some(RuleMode::Link), false);
        assert_eq!(store.transaction(&first.ops, &json!({}))["ok"], json!(true));
        let second = rule_plan(&lib, &at(&file), Some(RuleMode::Link), false);
        assert!(second.ops.is_empty(), "第二遍不该有 op");
        assert_eq!(second.detail["alreadyLinked"], json!(true));
        drop(t);
    }

    /// compose 模式：文件里出现一个带标记的区块，区块之外逐字节不动。
    #[test]
    fn compose_mode_wraps_the_body_and_keeps_the_users_bytes() {
        let original = "# my own rules\r\n\r\nkeep me\r\n";
        let (t, lib, file) = rule_fixture("rule-compose", "CLAUDE.md", original);
        // 先放一份库内主副本：compose 的区块内容从它来。
        let keeper = lib.dir_for("rule").join("claude.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "shared rule one\nshared rule two\n").unwrap();

        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(plan.code, None);
        assert_eq!(plan.ops.len(), 1);
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        assert_eq!(store.transaction(&plan.ops, &json!({}))["ok"], json!(true));

        let out = std::fs::read_to_string(&file).unwrap();
        assert!(out.starts_with(original), "前缀变了: {:?}", out);
        let block = compose::find_block(&out, "claude").expect("区块");
        assert!(block.body.contains("shared rule one"));
        // CRLF 目标：区块内也是 CRLF，不出现裸 LF。
        let bytes = out.as_bytes();
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                assert!(i > 0 && bytes[i - 1] == b'\r', "裸 LF 在 {}", i);
            }
        }
        drop(t);
    }

    /// compose 幂等：同一份内容跑两遍，第二遍零写入。
    #[test]
    fn compose_mode_is_idempotent() {
        let (t, lib, file) = rule_fixture("rule-compose-twice", "CLAUDE.md", "# t\n");
        let keeper = lib.dir_for("rule").join("claude.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "body\n").unwrap();
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let first = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(store.transaction(&first.ops, &json!({}))["ok"], json!(true));
        let before = std::fs::read(&file).unwrap();
        let second = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert!(second.ops.is_empty(), "第二遍不该有 op: {:?}", second.detail);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        drop(t);
    }

    /// 区块被用户改过 → `rule.foreignEdit`，且盘上文件零改动（这就是闸门的意义）。
    #[test]
    fn a_hand_edited_block_is_refused_and_the_file_is_left_alone() {
        let (t, lib, file) = rule_fixture("rule-foreign", "CLAUDE.md", "# t\n");
        let keeper = lib.dir_for("rule").join("claude.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "generated body\n").unwrap();
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let first = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(store.transaction(&first.ops, &json!({}))["ok"], json!(true));

        // 用户手改区块内一行。
        let edited = std::fs::read_to_string(&file)
            .unwrap()
            .replace("generated body", "my own wording");
        std::fs::write(&file, &edited).unwrap();

        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(plan.code.as_deref(), Some("rule.foreignEdit"));
        assert!(plan.ops.is_empty(), "闸门只判不写");
        // 三方 diff 需要的那两份内容都在 detail 里。
        assert_eq!(plan.detail["slug"], json!("claude"));
        assert!(plan.detail["onDisk"].as_str().unwrap().contains("my own wording"));
        assert!(plan.detail["want"].as_str().unwrap().contains("generated body"));
        // 盘上文件没被改动。
        assert_eq!(std::fs::read_to_string(&file).unwrap(), edited);
        drop(t);
    }

    /// 库内没有主副本时，第一次 compose 取目标文件自己的内容 —— 不改一个字，
    /// 只是把现有内容圈成受管区块。
    #[test]
    fn the_first_compose_wraps_the_existing_text_verbatim() {
        let original = "# existing\nline two\n";
        let (t, lib, file) = rule_fixture("rule-adopt", "AGENTS.md", original);
        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(plan.code, None, "首次 compose 不该被闸门拒绝: {:?}", plan.detail);
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        assert_eq!(store.transaction(&plan.ops, &json!({}))["ok"], json!(true));
        let out = std::fs::read_to_string(&file).unwrap();
        let block = compose::find_block(&out, "agents").expect("区块");
        assert_eq!(block.body, original, "区块内容应当就是原文");
        // 去掉区块后回到原文：一个字都没被改写。
        assert_eq!(compose::remove_block(&out, "agents"), original);
        drop(t);
    }

    /// 区块整个被删掉 → 不算外部改动，共享内容回到区块里（同步就是这个意思）。
    ///
    /// 与「区块还在、内容被改过」分开判：前者用户只是删了标记，后者用户改的是
    /// 内容。把前者也拒掉会让「删掉标记想重新纳入」变成一条死路。
    #[test]
    fn a_deleted_block_comes_back_because_that_is_what_sync_means() {
        let (t, lib, file) = rule_fixture("rule-deleted-block", "CLAUDE.md", "# mine\n");
        let keeper = lib.dir_for("rule").join("claude.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "shared\n").unwrap();
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let first = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(store.transaction(&first.ops, &json!({}))["ok"], json!(true));

        // 用户把整个区块删掉，只留自己的内容。
        std::fs::write(&file, "# mine\n").unwrap();
        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(plan.code, None, "区块被删不等于区块被改: {:?}", plan.detail);
        assert_eq!(store.transaction(&plan.ops, &json!({}))["ok"], json!(true));
        // 用户自己的内容原样保留，共享内容重新回到区块里。
        let out = std::fs::read_to_string(&file).unwrap();
        assert!(out.starts_with("# mine\n"), "{:?}", out);
        assert_eq!(
            compose::find_block(&out, "claude").map(|b| b.body),
            Some("shared\n".to_string())
        );
        drop(t);
    }

    /// 停用 compose：只拿走我们放进去的区块，文件本身留着。
    #[test]
    fn uncompose_removes_only_the_block() {
        let original = "# mine\ntail\n";
        let (t, lib, file) = rule_fixture("rule-uncompose", "AGENTS.md", original);
        let keeper = lib.dir_for("rule").join("agents.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "shared\n").unwrap();
        let store = store::Store::new(&store::native_str(&t.0.join("store")));
        let plan = rule_plan(&lib, &at(&file), Some(RuleMode::Compose), false);
        assert_eq!(store.transaction(&plan.ops, &json!({}))["ok"], json!(true));

        let back = uncompose_plan(&file, "agents");
        assert_eq!(store.transaction(&back.ops, &json!({}))["ok"], json!(true));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        // 再停用一次：零写入。
        assert!(uncompose_plan(&file, "agents").ops.is_empty());
        drop(t);
    }

    /// 模式没给时缺省 compose（link 会把用户的文件变成链接，不该是缺省），
    /// 但 `.mdc` 缺省只能是 link。
    #[test]
    fn the_default_mode_is_compose_except_for_mdc() {
        let (t, lib, file) = rule_fixture("rule-default", "AGENTS.md", "# t\n");
        let keeper = lib.dir_for("rule").join("agents.md");
        std::fs::create_dir_all(keeper.parent().unwrap()).unwrap();
        std::fs::write(&keeper, "b\n").unwrap();
        assert_eq!(rule_plan(&lib, &at(&file), None, false).mode, RuleMode::Compose);
        drop(t);

        let (t2, lib2, mdc) = rule_fixture("rule-default-mdc", "a.mdc", "x\n");
        let keeper2 = lib2.dir_for("rule").join("a.md");
        std::fs::create_dir_all(keeper2.parent().unwrap()).unwrap();
        std::fs::write(&keeper2, "b\n").unwrap();
        let plan = rule_plan(&lib2, &at(&mdc), None, false);
        assert_eq!(plan.code, None);
        assert_eq!(plan.mode, RuleMode::Link, ".mdc 只能是 link");
        drop(t2);
    }

    /// `kind_patterns` 按声明顺序给出路径；没声明这一类的 agent 返回空表而不是 `None`。
    #[test]
    fn kind_patterns_follow_the_adapter_table() {
        assert_eq!(
            kind_patterns("claude-code", "rule", "global"),
            Some(vec!["CLAUDE.md".to_string(), "settings.json".to_string()])
        );
        // pi 声明了 rules（`AGENTS.md`）—— 表里是什么就是什么，不凭印象。
        assert_eq!(
            kind_patterns("pi", "rule", "global"),
            Some(vec!["AGENTS.md".to_string()])
        );
        // 未知 agent：`None`。
        assert_eq!(kind_patterns("nope", "rule", "global"), None);
    }

    /// 全局作用域的基准目录就是 agent 的 home（`~` 已展开）。
    #[test]
    fn scope_base_expands_the_agent_home() {
        let base = scope_base("C:\\Users\\tester", "claude-code", "global", None).unwrap();
        assert!(base.ends_with(".claude"), "{}", base);
        // 项目作用域没有 projectId → 解析不出来。
        assert_eq!(scope_base("C:\\Users\\tester", "claude-code", "project", None), None);
    }
}
