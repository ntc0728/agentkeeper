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

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::library::Library;
use crate::linkfs::{self, LinkCap};
use crate::store;

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

    /// 可以一键收敛的组数（`Diverged` 不算）。
    pub fn actionable(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| g.verdict != Verdict::Diverged)
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
        all.extend(copies);

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
}
