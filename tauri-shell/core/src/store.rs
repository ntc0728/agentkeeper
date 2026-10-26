//! 写入路径：校验、内存撤销、原子写入、审计。
//!
//! 每次变更都必须经过 [`Store::transaction`]，它按以下顺序保证：
//!
//! 1. 校验 —— 格式错误的操作在任何一个字节被移动之前就会被拒绝
//! 2. 撤销快照 —— 每个被触及路径的当前状态被读取**到内存中**。任何地方都不会
//!               把内容拷贝到磁盘：产品没有备份树，也没有按 id 回滚，因此快照唯一
//!               要服务的对象就是下面这条失败路径。
//! 3. 原子写入 —— `<path>.agentkeeper-tmp-<pid>-<ms>`，然后 `rename(2)`
//! 4. 失败时撤销 —— 失败的操作会从内存快照把被触及的路径恢复回去，并返回
//!               `{ ok: false }`。撤销会尝试**每一个**路径，并报告**所有**它
//!               无法恢复的路径，因为在某个路径上失败意味着那个路径仍停留在
//!               写入之后的状态。
//! 5. 审计 —— 一行 `audit.jsonl` 描述改动内容
//!
//! 写入落在真实的 agent 目录里。用户和文件之间刻意不设暂存层：一个把写入重定向
//! 到镜像树、却声称已经保存的 UI，比根本不写更糟糕，而之前的 "sandbox mode"
//! 做的正是这件事。
//!
//! **任何调用方 —— UI、MCP、CLI —— 都必须走 `transaction()`。** 任何东西都不得
//! 直接写 agent 文件。走这条路径，正是"一次失败的写入会原样留下目录树"对*除一个
//! 路径之外*的每个路径都成立的原因：如果撤销本身无法把某个路径写回去 —— 飞行
//! 中途权限变了、卷写满了，或者符号链接需要进程没有的权限 —— 该路径会带着自己的
//! `code` 出现在 `restoreErrors` 里，调用方应当把它展示出来。它不会被吞掉，而且
//! 它出现得并不罕见到可以当作不可达。
//!
//! ## 与规格最朴素读法之间的刻意差异
//!
//! * 快照保存的是原始字节，不是指向副本的路径。按 id 回滚需要副本在调用结束之后
//!   仍然存在；而撤销一次*失败的*事务不需要，所以把旧状态留在 RAM 里就够了，
//!   而且不花磁盘一分钱。
//! * JSONC 剥离与扫描器共用（[`super::jsonc`]），而不是在本模块里另起一套
//!   微妙的、略有差异的正则链。

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::jsonc::strip_json_comments;
use super::settings;

/// 从不纳入快照、也从不拷贝的目录。把 `node_modules` 一起拖进来，会让一次
/// 40 ms 的保存变成 4 GB 的保存。
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

/// 单个事务运行期间可在内存中持有的字节上限。
///
/// 快照必须完整：一个无法被完整捕获的目录应当在第一次写入*之前*就让事务失败，
/// 绝不能"大致恢复"。所以这个守卫是硬失败，而不是截断。
///
/// ## 这个数字的来源
///
/// 这是一个*产品*上限，而不是内存压力估算。撤销数据在一次事务的生命周期内一直
/// 被持有，而且是在一个同时驻留了 WebView 的桌面应用的 UI 线程上，因此唯一的问题
/// 是"我们可以短暂持有多大的快照而不让应用失去响应"。
///
/// 针对本应用在一台真实机器上实际管理的资产实测（按 [`SKIP_DIRS`] 排除之后的
/// 各目录合计）：
///
/// | 被触及的路径                                   | 字节数    |
/// |------------------------------------------------|----------|
/// | 一个 skill 目录（`~/.claude/skills/<name>`）   | ≤ 2.1 MiB |
/// | 一个 rules/memory 文件（`AGENTS.md`、`CLAUDE.md`）| ≤ 40 KiB  |
/// | 一个 MCP 配置（`mcp.json`、`config.toml`）       | ≤ 40 KiB  |
/// | **整个 `skills/` 父目录，最坏情况**             | ~4.4 MiB  |
///
/// 所以 8 MiB 大约是最大的*单个*受管目录（2.1 MiB）的 2 倍，也是操作所能指名的
/// 最大父目录（4.4 MiB）的约 2 倍，这把拒绝线放在了所有合法写入之上、也放在了
/// 遍历本来就跳过的构建树之下。真正会超出的两种现实情形是：提交在某个 skill
/// *内部*的 vendored `node_modules`，以及媒体资源繁重的插件，而这两者都确实
/// 值得拒绝：为了撤销一次根本不会失败的保存而持有 100 MiB 字节，比告诉用户更糟。
///
/// 它被刻意**不**提高到能覆盖病态情形。这笔交易是明摆着的：一个"大概总是放得下"
/// 的上限，代价是让用户失去一次本来能做的写入，而这个上限之所以存在，恰恰因为
/// 内存就是唯一的撤销手段。
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Windows 上路径分隔符统一为 `\`；store 路径按字符串比较和记日志，
/// 所以分隔符采用固定的形式。
pub fn native_str(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s
    }
}

/* ------------------------------------------------------------------ */
/* JSON 辅助函数                                                        */
/* ------------------------------------------------------------------ */

/// `readJsonOrNull()` —— 只有文件存在*且*能解析时才返回 `Some`。
///
/// 这个区分很重要：在参考实现里空文件同样解析成 `null`，而 `json` 操作应当
/// 中止，而不是覆盖一份它读不懂的配置。
pub fn read_json_or_null(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let stripped = strip_json_comments(&text);
    serde_json::from_str(&stripped).ok()
}

/// `readJsonSafe()` —— 解析失败看起来就像一个空对象。
pub fn read_json_safe(path: &str) -> Value {
    read_json_or_null(path).unwrap_or_else(|| json!({}))
}

/// `{ ...cur, ...patch }` — 对象的展开合并，也会把数组（或
/// 字符串）变成下标键的对象，而不是并入其中。
fn spread_merge(cur: &Value, patch: &Value) -> Value {
    let mut out = Map::new();
    match cur {
        Value::Object(m) => {
            for (k, v) in m {
                out.insert(k.clone(), v.clone());
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                out.insert(i.to_string(), v.clone());
            }
        }
        Value::String(s) => {
            for (i, c) in s.chars().enumerate() {
                out.insert(i.to_string(), Value::String(c.to_string()));
            }
        }
        _ => {}
    }
    if let Value::Object(p) = patch {
        for (k, v) in p {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn object_keys_len(v: &Value) -> usize {
    match v {
        Value::Object(m) => m.len(),
        Value::Array(a) => a.len(),
        Value::String(s) => s.chars().count(),
        _ => 0,
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}

/* ------------------------------------------------------------------ */
/* 文件系统辅助函数                                                        */
/* ------------------------------------------------------------------ */

/// `copyDir()` —— 递归拷贝，拒绝顺带拖走构建树，并复现符号链接（当链接无法
/// 创建时退回目标的内容，例如没有特权的 Windows 进程）。
pub fn copy_dir(src: &Path, dest: &Path) {
    if std::fs::create_dir_all(dest).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            let link_ok = std::fs::read_link(&from).ok().map(|target| {
                let is_dir = std::fs::metadata(&from)
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if is_dir {
                    symlink_dir(&target, &to)
                } else {
                    symlink_file(&target, &to)
                }
            });
            if !matches!(link_ok, Some(Ok(()))) {
                // 悬空链接无法被重建；把目标指向的内容拷贝过来，总好过悄悄丢掉这个条目。
                let _ = std::fs::copy(&from, &to);
            }
        } else if ft.is_dir() {
            copy_dir(&from, &to);
        } else {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(not(windows))]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(windows))]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// 以 UTF-8 文本读取文件，遇到非法序列则宽松处理。
///
/// 参考封装里的 `readAll()` 对缺失或不可读的文件返回 `''`，而资产抽屉会把它
/// 渲染成"空"，因此这里的失败绝不能算作错误。所有资产内容的读取都走这一个函数。
pub fn read_text(p: &Path) -> String {
    std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// `rmrf()` —— 链接是被解除的，绝不跟随。
pub fn rmrf(p: &Path) {
    // `fs.existsSync()` 会跟随链接：悬空的符号链接会被原样留下。
    if !p.exists() {
        return;
    }
    let Ok(st) = std::fs::symlink_metadata(p) else {
        return;
    };
    if st.is_dir() {
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                rmrf(&entry.path());
            }
        }
        if std::fs::remove_dir(p).is_err() {
            let _ = std::fs::remove_dir_all(p);
        }
    } else if std::fs::remove_file(p).is_err() {
        let _ = std::fs::remove_file(p);
    }
}

/// `listFiles()` —— 相对路径用 `/`，符号链接按文件计数（这样即使在 `seen`
/// 守卫之前，链接环也无法无限递归）。
pub fn list_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk_files(dir, "", &mut seen, &mut out);
    out
}

fn walk_files(dir: &Path, base: &str, seen: &mut HashSet<PathBuf>, out: &mut Vec<String>) {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return;
    };
    if !seen.insert(real) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_files(&entry.path(), &rel, seen, out),
            Ok(_) => out.push(rel),
            Err(_) => out.push(rel),
        }
    }
}

/// 把一个目录递归读进内存中的 [`DirSnap`] 列表。
///
/// `seen` 对链接环的防护与 [`walk_files`] 完全一致：没有它，一个指向自身父目录
/// 的目录会一直递归到预算耗尽，而由此产生的报错会归咎于用户的资产而不是那个
/// 环。跳过 [`SKIP_DIRS`] 的理由也同 [`copy_dir`] 跳过它们一样。
fn collect_dir(
    dir: &Path,
    base: &str,
    budget: &mut usize,
    out: &mut Vec<DirSnap>,
) -> Result<(), TxError> {
    let mut seen = HashSet::new();
    collect_dir_seen(dir, base, budget, out, &mut seen)
}

fn collect_dir_seen(
    dir: &Path,
    base: &str,
    budget: &mut usize,
    out: &mut Vec<DirSnap>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), TxError> {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return Ok(());
    };
    if !seen.insert(real) {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        let from = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)
                .map(|t| native_str(&t))
                .unwrap_or_default();
            out.push(DirSnap::Link(
                rel,
                target,
                std::fs::metadata(&from)
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
            ));
        } else if ft.is_dir() {
            out.push(DirSnap::Dir(rel.clone()));
            collect_dir_seen(&from, &rel, budget, out, seen)?;
        } else {
            let content = std::fs::read(&from).map_err(|e| {
                TxError::plain(format!(
                    "the file could not be read into the undo snapshot ({})",
                    e
                ))
            })?;
            if content.len() > *budget {
                return Err(TxError::snapshot(over_budget_message(&rel, content.len())));
            }
            *budget -= content.len();
            out.push(DirSnap::File(rel, content));
        }
    }
    Ok(())
}

/// 把捕获到的 [`DirSnap`] 列表写回 `dir` 之下。
///
/// 目录会先被清空：撤销必须产出*捕获到的*那棵树，而不是合并，否则事务在其中
/// 创建的文件会残留下来。这使得 `rmrf` 之后的每一次失败都具有破坏性，所以遍历
/// 会收集失败并把剩下的条目处理完，而不是在第一个失败处直接退出 —— 直接退出会
/// 让目录树只剩下恰好走到的那部分条目，那正是本函数要防止的"恢复了一半"的
/// 状态。在销毁任何东西之前，一次探针写入会证明该目录究竟能不能被写回：在这里
/// 被拒绝的话，捕获到的树仍然完好。
fn restore_dir(dir: &Path, entries: &[DirSnap]) -> Vec<RestoreFailure> {
    let mut out = Vec::new();
    if let Err(e) = std::fs::create_dir_all(dir) {
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!("the directory could not be recreated ({})", e),
        ));
        return out;
    }
    let probe = dir.join(".agentkeeper-restore-probe");
    if let Err(e) = std::fs::write(&probe, b"") {
        // 在下面的 `rmrf` 之前就被拒绝，因此此刻还没有任何东西被销毁。
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!(
                "the directory is not writable, so the captured tree was left in place ({})",
                e
            ),
        ));
        return out;
    }
    let _ = std::fs::remove_file(&probe);
    rmrf(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!("the directory could not be recreated ({})", e),
        ));
        return out;
    }
    for entry in entries {
        let rel = entry_name(entry);
        let to = dir.join(&rel);
        match entry {
            DirSnap::Dir(_) => {
                if let Err(e) = std::fs::create_dir_all(&to) {
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_PATH_FAILED,
                        e.to_string(),
                    ));
                }
            }
            DirSnap::File(_, content) => {
                if let Some(parent) = to.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        out.push(RestoreFailure::new(
                            &native_str(parent),
                            RESTORE_PATH_FAILED,
                            e.to_string(),
                        ));
                        continue;
                    }
                }
                if let Err(e) = std::fs::write(&to, content) {
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_PATH_FAILED,
                        e.to_string(),
                    ));
                }
            }
            DirSnap::Link(_, target, is_dir) => {
                let created = if *is_dir {
                    symlink_dir(Path::new(target), &to)
                } else {
                    symlink_file(Path::new(target), &to)
                };
                if let Err(e) = created {
                    // 没有特权的 Windows 进程无法创建链接，而未链接的目标也没有
                    // 字节可以退回。目录树的其余部分仍然被恢复了，而该条目会带着
                    // 用户需要做什么一起被报告 —— 悄悄缺失一个条目，正是这里
                    // 绝不能产出的"恢复了一半"的目录树。
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_SYMLINK_FAILED,
                        format!(
                            "the link was not recreated ({}); the other files in this directory are back, but this one is missing, because creating a symbolic link on Windows needs administrator rights or developer mode — run the app as administrator, or recreate the link by hand",
                            e
                        ),
                    ));
                }
            }
        }
    }
    out
}

fn entry_name(entry: &DirSnap) -> String {
    match entry {
        DirSnap::Dir(name) | DirSnap::File(name, _) | DirSnap::Link(name, _, _) => name.clone(),
    }
}

/// `pruneEmptyParents()` —— 移除那些仅仅为了存放我们刚删掉的文件而存在的
/// 目录，遇到 `stop_at` 或卷根就停下。
fn prune_empty_parents(dir: &str, stop_at: &str) {
    let boundary = normalize_path(stop_at);
    let mut cur = normalize_path(dir);
    for _ in 0..12 {
        if cur == boundary || cur == volume_root(&cur) {
            break;
        }
        match std::fs::read_dir(&cur) {
            Ok(mut it) => {
                if it.next().is_some() {
                    return;
                }
            }
            Err(_) => return,
        }
        if std::fs::remove_dir(&cur).is_err() {
            return;
        }
        match Path::new(&cur).parent() {
            Some(p) => cur = native_str(p),
            None => return,
        }
    }
}

/// 对已经是绝对路径的路径做 `path.resolve()`：折叠 `.` 和 `..`。
pub fn normalize_path(p: &str) -> String {
    let path = Path::new(p);
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    native_str(&out)
}

fn volume_root(p: &str) -> String {
    match Path::new(p).components().next() {
        Some(std::path::Component::Prefix(prefix)) => native_str(Path::new(prefix.as_os_str())),
        _ => native_str(&PathBuf::from(std::path::MAIN_SEPARATOR.to_string())),
    }
}

/// 撤销没能放回去的一个路径，带着机器可读的原因。
///
/// 这里的失败不是异常：其他路径仍会被尝试，所以这个类型的存在是为了让"一个坏
/// 路径"和"几个坏路径"具有相同的形状。[`code`](Self::code) 正是渲染层措辞的
/// 依据 —— 一个不得不去解析英文句子的调用方，会在措辞被改写的瞬间就崩掉，而这正是
/// 符号链接消息加上"以管理员身份运行"那句时发生过的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreFailure {
    path: String,
    code: &'static str,
    detail: String,
}

impl RestoreFailure {
    fn new(path: &str, code: &'static str, detail: String) -> RestoreFailure {
        RestoreFailure {
            path: path.to_string(),
            code,
            detail,
        }
    }

    /// 一个自身不带 code 的 I/O 失败 —— 磁盘只是说了"不"。它的 `detail` 是原始的
    /// `io::Error`，所以调用方据此拼出的句子仍保持字典里已映射的
    /// `write failed for §1: §2` 形状。
    fn plain(path: &str, e: impl std::fmt::Display) -> RestoreFailure {
        RestoreFailure::new(path, RESTORE_PATH_FAILED, e.to_string())
    }

    /// 稳定标识 —— 绝不是那个句子。见 [`STORE_SNAPSHOT_TOO_LARGE`] 和
    /// [`RESTORE_SYMLINK_FAILED`]。
    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// `restoreErrors` 的条目形状。在这里构造它，意味着渲染层的字段名不会与引擎
    /// 实际发出的字段名发生漂移。
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "code": self.code,
            "detail": self.detail,
        })
    }

    /// 给读取单个字符串的调用方的一句话。失败多于一条时由调用方把它们拼接起来，
    /// 因此没有哪条失败会被丢掉 —— 正是丢掉它们让旧的 `restoreError` 低估了
    /// 目录树的真实状态。
    ///
    /// 路径排在最前面且独占一行，因为它是唯一必须在 `detail` 被改写时依然存活的
    /// 部分：用户是基于*哪个*资产出了问题采取行动，其次才是为什么。
    fn sentence(&self) -> String {
        format!("{}: {}", self.path, self.detail)
    }
}

/// 每条失败一句话，拼接在一起。空进空出 —— 这样调用方就可以仅凭这个字符串来
/// 判断"撤销是否干净"。
fn restore_sentence(failures: &[RestoreFailure]) -> String {
    failures
        .iter()
        .map(|f| f.sentence())
        .collect::<Vec<String>>()
        .join("; ")
}

/// 当某个被触及的路径比内存撤销能容纳的还大时，事务被拒绝所对应的 `errorCode`。
/// 什么都没有写入，因此这是"拒绝"而不是"恢复"问题 —— 但调用方仍需要把它与普通的
/// I/O 失败区分开，因为解决办法是"换一个更小的资产"，而不是"重试"。
pub const STORE_SNAPSHOT_TOO_LARGE: &str = "store.snapshotTooLarge";

/// 针对"撤销把文件放了回去，但目录里的某个符号链接没能重建"这一半成功情形的
/// `errorCode`。文件都在、链接不在，而只有带特权的运行才能修复它。
pub const RESTORE_SYMLINK_FAILED: &str = "store.symlinkRestoreFailed";

/// 因上述 code 没有涵盖的原因而失败的恢复：该路径根本写不回去（权限变了、路径被
/// 锁住、恢复途中卷被写满）。
const RESTORE_PATH_FAILED: &str = "store.restorePathFailed";

/// 针对"撤销没能把每一条路径都放回去"的 `errorCode`。只要 `restoreErrors` 非空
/// 就会发出，好让渲染层不必读散文就能区分"什么都没写"和"写了，然后部分撤销"。
pub const STORE_RESTORE_INCOMPLETE: &str = "store.restoreIncomplete";

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// 调用方在某个被触及的路径装不进撤销预算时看到的拒绝信息。
///
/// 它必须自己回答三个问题，因为它是用户唯一能看到的东西：上限是多少、越界的那个
/// 东西有多大、该怎么办。"the in-memory undo limit is 8388608 bytes" 三个都没回答，
/// 尤其是原始字节数，读起来像是一个用户改不了的数字挡在路上。
///
/// 字面量刻意保持在**一行源码**上：`scripts/tauri-i18n.js` 会把仍含 `\` 的
/// 字面量归到"不是散文"一类，所以一个用 `\` 折行的句子永远不会被要求翻译。
fn over_budget_message(who: &str, size: usize) -> String {
    format!(
        "undo snapshot limit reached: {} needs {} but a single write may only hold {} of undo data, so nothing was written; split this asset into smaller writes, or drop files that are not part of it (dependency folders and build output are skipped automatically)",
        who,
        human_size(size),
        human_size(MAX_SNAPSHOT_BYTES)
    )
}

/// *向上*取整到一位小数，这样一个尺寸永远不会显示得比它超出的上限更低：
/// "needs 8.0 MB but may only hold 8.0 MB" 读起来像取整的假象，让读者无法判断
/// 自己超了多少。这里的 `8.0` 意味着"8.0 还多一点"。
fn human_size(bytes: usize) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", (b / MB * 10.0).ceil() / 10.0)
    } else if b >= 1024.0 {
        format!("{} KB", (b / 1024.0).ceil() as usize)
    } else {
        format!("{} B", bytes)
    }
}

/// 携带自身 `errorCode` 的事务失败。
///
/// code 是在原因*已知*的地方挂上去的，因为渲染层的措辞就是以 code 为依据的；
/// 之后再从英文句子里去反推 code，正是此前两者产生漂移的原因。
#[derive(Debug)]
struct TxError {
    message: String,
    code: &'static str,
}

impl TxError {
    /// 一个 code 不属于渲染层特例处理的失败：普通的操作错误，以 `ok:false` 应答，
    /// 消息就是该操作构造出来的那条。
    fn plain(message: impl Into<String>) -> TxError {
        TxError {
            message: message.into(),
            code: TX_FAILED,
        }
    }

    /// 尺寸上限。它是渲染层唯一不许退回通用措辞的 code：用户的下一步是把资产
    /// 缩小，而这条建议写在句子里，而不是放在 code 的参数里。
    fn snapshot(message: impl Into<String>) -> TxError {
        TxError {
            message: message.into(),
            code: STORE_SNAPSHOT_TOO_LARGE,
        }
    }

    /// 补上事务即将触及的路径。
    ///
    /// 只有操作错误会带上 `write failed for <path>: …` 这个前缀骨架。该模式是作为
    /// 带 `§2` 原样透传的前缀被翻译的 —— 那些消息的尾部是一个 `io::Error`，
    /// 字典能覆盖的也就到此为止 —— 所以一条携带建议的*拒绝*（"把这个资产拆成
    /// 更小的写入"）绝不能挂在它下面，否则建议会以英文抵达中文用户。拒绝有自己的
    /// 骨架，也有自己的词条。
    fn naming(self, path: &str) -> TxError {
        if self.code == STORE_SNAPSHOT_TOO_LARGE {
            return self;
        }
        TxError {
            message: format!("write failed for {}: {}", path, self.message),
            code: self.code,
        }
    }
}

/// 针对没有更具体 code 的任何事务失败的 `errorCode`。
const TX_FAILED: &str = "write.failed";

/// `tx-${Date.now().toString(36)}-${Math.random().toString(36).slice(2,7)}`
fn gen_tx_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut tail = String::with_capacity(5);
    for _ in 0..5 {
        // xorshift64 —— 这个 id 只需要唯一，不需要不可猜测。
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        tail.push(to_base36(state % 36).chars().next().unwrap_or('0'));
    }
    format!("tx-{}-{}", to_base36(now_millis().max(0) as u64), tail)
}

/* ------------------------------------------------------------------ */
/* 操作                                                                 */
/* ------------------------------------------------------------------ */

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Write,
    CopyDir,
    Json,
    Delete,
    Mkdir,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Write => "write",
            OpKind::CopyDir => "copyDir",
            OpKind::Json => "json",
            OpKind::Delete => "delete",
            OpKind::Mkdir => "mkdir",
        }
    }
}

/// 一次变更。通过这些构造函数来构造，这样漏掉某个字段就是编译错误，而不是运行时的
/// `op[i] is missing a "from"`。
pub struct Op {
    pub kind: OpKind,
    /// 被写入的路径。绝对路径，与扫描器报告的一致。
    pub path: String,
    pub from: Option<String>,
    pub content: Option<String>,
    /// `json` 操作：浅合并的来源。
    pub patch: Option<Value>,
    /// `json` 操作：完全控制。返回 `None` 会中止事务。
    pub mutate: Option<Box<dyn Fn(Value) -> Option<Value>>>,
}

impl Op {
    fn new(kind: OpKind, path: &str) -> Op {
        Op {
            kind,
            path: path.to_string(),
            from: None,
            content: None,
            patch: None,
            mutate: None,
        }
    }

    pub fn write(path: &str, content: &str) -> Op {
        let mut op = Op::new(OpKind::Write, path);
        op.content = Some(content.to_string());
        op
    }

    pub fn copy_dir(path: &str, from: &str) -> Op {
        let mut op = Op::new(OpKind::CopyDir, path);
        op.from = Some(from.to_string());
        op
    }

    pub fn json_patch(path: &str, patch: Value) -> Op {
        let mut op = Op::new(OpKind::Json, path);
        op.patch = Some(patch);
        op
    }

    pub fn json_mutate<F>(path: &str, f: F) -> Op
    where
        F: Fn(Value) -> Option<Value> + 'static,
    {
        let mut op = Op::new(OpKind::Json, path);
        op.mutate = Some(Box::new(f));
        op
    }

    pub fn delete(path: &str) -> Op {
        Op::new(OpKind::Delete, path)
    }

    pub fn mkdir(path: &str) -> Op {
        Op::new(OpKind::Mkdir, path)
    }
}

/* ------------------------------------------------------------------ */
/* in-memory undo state                                                */
/* ------------------------------------------------------------------ */

/// One entry of a captured directory, relative to that directory.
#[derive(Clone)]
enum DirSnap {
    /// A subdirectory. Recorded so an empty directory survives the undo — an
    /// asset directory that exists but is empty is a real state, and dropping
    /// it would leave a phantom folder in the agent's tree.
    Dir(String),
    File(String, Vec<u8>),
    /// `target` + "the target was a directory", which is what tells a restore
    /// to call `symlink_dir` instead of `symlink_file`.
    Link(String, String, bool),
}

/// What a transaction touched, so a failure can put it back.
///
/// This lives in memory for exactly as long as the transaction runs and is
/// then dropped: the product keeps no backup tree and offers no rollback by id,
/// so the only consumer is [`Store::restore`] below. `Snapshot` is only ever
/// built by [`Store::snapshot`], which is total — a path that cannot be
/// captured makes the whole transaction refuse *before* the first write, so
/// there is no "partially restorable" variant to fall into.
enum Snapshot {
    /// The path did not exist before: undoing means removing what this
    /// transaction created there.
    Absent {
        path: String,
    },
    File {
        path: String,
        content: Vec<u8>,
    },
    Dir {
        path: String,
        entries: Vec<DirSnap>,
    },
    /// A symbolic link, or a dangling one: the target string plus whether the
    /// target was a directory, which is what picks `symlink_dir` over
    /// `symlink_file` on restore. A link is not a file — recreating it as one
    /// would replace `a -> b` with a stale copy of `b`.
    Link {
        path: String,
        target: String,
        is_dir: bool,
    },
}

impl Snapshot {
    fn path(&self) -> &str {
        match self {
            Snapshot::Absent { path }
            | Snapshot::File { path, .. }
            | Snapshot::Link { path, .. }
            | Snapshot::Dir { path, .. } => path,
        }
    }
}

/* ------------------------------------------------------------------ */
/* Store                                                               */
/* ------------------------------------------------------------------ */

pub struct Store {
    root: PathBuf,
    root_str: String,
}

impl Store {
    /// Creates the store tree if it is not there yet.
    pub fn new(root: &str) -> Store {
        let root_path = PathBuf::from(root);
        let _ = settings::ensure_store(&native_str(&root_path));
        Store {
            root_str: native_str(&root_path),
            root: root_path,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_str(&self) -> &str {
        &self.root_str
    }

    pub fn audit_file(&self) -> String {
        native_str(&self.root.join("audit.jsonl"))
    }

    /* -------------------------------------------------------------- */
    /* undo state                                                      */
    /* -------------------------------------------------------------- */

    /// Capture one path (file, directory or link) into memory.
    ///
    /// Existence is part of the state, so undoing a delete and undoing a create
    /// are the same code with different data. The walk honours [`SKIP_DIRS`]
    /// for the same reason [`copy_dir`] does — a `node_modules` tree is not part
    /// of what any agent asset means, and holding it in RAM would be worse than
    /// useless.
    ///
    /// `Err` is a refusal, not a warning: a directory over [`MAX_SNAPSHOT_BYTES`]
    /// must fail the transaction before the first write, because "restored most
    /// of it" is exactly the half-written state this must never leave behind.
    fn snapshot(&self, original: &str, budget: &mut usize) -> Result<Snapshot, TxError> {
        let path = Path::new(original);
        // `exists()` follows links, so a dangling link reads as absent — use
        // `symlink_metadata` to tell "no such path" from "a broken link".
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                return Ok(Snapshot::Absent {
                    path: original.to_string(),
                })
            }
        };

        if meta.is_symlink() {
            // A dangling link is not an error, it is a state that already
            // exists on disk; `metadata` then fails and the link is recorded as
            // pointing at a non-directory, which is the only thing knowable.
            let target = std::fs::read_link(path)
                .map(|t| native_str(&t))
                .unwrap_or_default();
            let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
            return Ok(Snapshot::Link {
                path: original.to_string(),
                target,
                is_dir,
            });
        }

        if !meta.is_dir() {
            let content = std::fs::read(path).map_err(|e| {
                TxError::plain(format!(
                    "the file could not be read into the undo snapshot, so the write was refused rather than performed with no way back ({})",
                    e
                ))
            })?;
            if content.len() > *budget {
                let name = path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| original.to_string());
                return Err(TxError::snapshot(over_budget_message(&name, content.len())));
            }
            *budget -= content.len();
            return Ok(Snapshot::File {
                path: original.to_string(),
                content,
            });
        }

        let mut entries = Vec::new();
        collect_dir(path, "", budget, &mut entries)?;
        Ok(Snapshot::Dir {
            path: original.to_string(),
            entries,
        })
    }

    /// `transaction(ops, meta)` — the only way a file may change.
    ///
    /// `meta` keys: `txId`, `kind`, `action`, `agentId`,
    /// `agentLabel`, `projectId`, `projectName`, `assetId`, `assetName`.
    ///
    /// The undo guarantee has two halves, and the first is the load-bearing
    /// one: every touched path is captured **before** the first op runs, so a
    /// refusal at capture time means nothing was written at all. The second half
    /// is [`Store::restore`], which runs only on the failure path.
    pub fn transaction(&self, ops: &[Op], meta: &Value) -> Value {
        for (i, op) in ops.iter().enumerate() {
            if matches!(op.kind, OpKind::CopyDir) && op.from.is_none() {
                return json!({
                    "ok": false,
                    "error": format!("op[{}] ({}) is missing a \"from\"", i, op.kind.name()),
                });
            }
        }

        let tx_id = meta
            .get("txId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(gen_tx_id);
        let t0 = now_millis();

        let mut log: Vec<Value> = Vec::new();
        let mut touched: Vec<Snapshot> = Vec::new();
        // One budget for the whole transaction, not per path: five 6 MiB
        // directories are the same problem as one 30 MiB one, and the cap is a
        // statement about peak memory, which a per-path budget would not be.
        let mut budget = MAX_SNAPSHOT_BYTES;

        let outcome: Result<(), TxError> = (|| {
            // 1. capture everything the transaction will touch, before any of it
            //    moves. A failure here means nothing was written at all.
            //
            //    A path named twice is captured once. Two ops on the same path
            //    (distribute pushes `mkdir <rules dir>` + `write <rule>` per
            //    target, so the same parent recurs across targets) would
            //    otherwise be charged twice against the shared budget and
            //    double-counted in `restore`, where the *second* copy would try
            //    to put back a state the first op had already changed. The
            //    pre-transaction capture is the only one worth keeping.
            let mut captured: HashSet<String> = HashSet::new();
            for op in ops {
                if captured.insert(normalize_path(&op.path)) {
                    touched.push(
                        self.snapshot(&op.path, &mut budget)
                            .map_err(|e| e.naming(&op.path))?,
                    );
                }
            }

            // 2. apply.
            for op in ops {
                let real = op.path.clone();
                match op.kind {
                    OpKind::Write => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let content = match (&op.content, op.from.as_deref()) {
                            (Some(c), _) => c.clone(),
                            (None, Some(from)) => match std::fs::read_to_string(from) {
                                Ok(c) => c,
                                Err(_) if Path::new(from).is_dir() => {
                                    return Err(TxError::plain(format!(
                                        "write source is a directory, not a file: {}",
                                        from
                                    )));
                                }
                                Err(e) => {
                                    return Err(TxError::plain(format!(
                                        "cannot read write source {}: {}",
                                        from, e
                                    )))
                                }
                            },
                            (None, None) => format!("{}\n", pretty(&Value::Null)),
                        };
                        if let Err(e) = settings::atomic_write(Path::new(&real), &content) {
                            return Err(TxError::plain(format!(
                                "write failed for {}: {}",
                                real, e
                            )));
                        }
                        log.push(json!({
                            "op": "write",
                            "path": real,
                            "bytes": content.len(),
                        }));
                    }

                    OpKind::CopyDir => {
                        let src = op.from.clone().unwrap_or_default();
                        if !Path::new(&src).exists() {
                            return Err(TxError::plain(format!("copy source missing: {}", src)));
                        }
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if real != src {
                            copy_dir(Path::new(&src), Path::new(&real));
                        }
                        log.push(json!({ "op": "copyDir", "path": real, "from": src }));
                    }

                    OpKind::Json => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let cur = if Path::new(&real).exists() {
                            match read_json_or_null(&real) {
                                Some(v) => v,
                                None => {
                                    return Err(TxError::plain(format!(
                                        "config file is corrupt, refusing to overwrite: {}",
                                        real
                                    )))
                                }
                            }
                        } else {
                            json!({})
                        };

                        let next = match &op.mutate {
                            Some(f) => match f(cur.clone()) {
                                Some(v) => v,
                                None => {
                                    return Err(TxError::plain(format!(
                                        "json mutate aborted for {}",
                                        real
                                    )))
                                }
                            },
                            None => spread_merge(&cur, op.patch.as_ref().unwrap_or(&json!({}))),
                        };
                        let body = format!("{}\n", pretty(&next));
                        if let Err(e) = settings::atomic_write(Path::new(&real), &body) {
                            return Err(TxError::plain(format!(
                                "write failed for {}: {}",
                                real, e
                            )));
                        }
                        log.push(json!({
                            "op": "json",
                            "path": real,
                            "keys": object_keys_len(&next),
                        }));
                    }

                    OpKind::Delete => {
                        let p = Path::new(&real);
                        if p.exists() {
                            if std::fs::symlink_metadata(p)
                                .map(|m| m.is_dir())
                                .unwrap_or(false)
                            {
                                rmrf(p);
                            } else {
                                let _ = std::fs::remove_file(p);
                            }
                        }
                        log.push(json!({ "op": "delete", "path": real }));
                    }

                    OpKind::Mkdir => {
                        let _ = std::fs::create_dir_all(&real);
                        log.push(json!({ "op": "mkdir", "path": real }));
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = outcome {
            // A restore that itself fails would leave a half-written tree, so
            // the failures are folded into the answer instead of being
            // swallowed — all of them, and each with its own code, so the caller
            // can tell "nothing was written" from "written, then partly undone"
            // without reading prose.
            let restore_errors = self.restore(&touched);
            let error = err.message;
            let error_code = if restore_errors.is_empty() {
                err.code
            } else {
                // The restore failure is the more actionable of the two: the
                // op error is what broke the write, this is what the user has
                // to go look at on disk.
                STORE_RESTORE_INCOMPLETE
            };
            settings::audit(
                &self.root_str,
                &json!({
                    "txId": tx_id,
                    "status": "failed",
                    "error": error,
                    "errorCode": error_code,
                    "restoreErrors": restore_errors.iter().map(|r| r.to_json()).collect::<Vec<Value>>(),
                    "ops": ops
                        .iter()
                        .map(|o| json!({ "op": o.kind.name(), "path": o.path }))
                        .collect::<Vec<Value>>(),
                    "kind": meta.get("kind").cloned().unwrap_or(Value::Null),
                    "agentId": meta.get("agentId").cloned().unwrap_or(Value::Null),
                    "projectId": meta.get("projectId").cloned().unwrap_or(Value::Null),
                    "assetId": meta.get("assetId").cloned().unwrap_or(Value::Null),
                }),
            );
            // `restoreErrors` is the complete list; `restoreError` is the
            // joined form kept for callers that predate it. An empty list is
            // omitted rather than shipped as `[]`, so `get("restoreError")`
            // still answers "was the undo clean?" on the old contract.
            let mut out = json!({
                "ok": false,
                "txId": tx_id,
                "error": error,
                "errorCode": error_code,
            });
            if let Value::Object(m) = &mut out {
                if !restore_errors.is_empty() {
                    m.insert(
                        "restoreError".to_string(),
                        Value::String(restore_sentence(&restore_errors)),
                    );
                    m.insert(
                        "restoreErrors".to_string(),
                        Value::Array(restore_errors.iter().map(|r| r.to_json()).collect()),
                    );
                }
            }
            return out;
        }

        let ms = now_millis() - t0;
        // One entry per **op**, not per captured path: the two lists are
        // deliberately not the same (`mkdir` and `write` on one path are two
        // ops and one snapshot), and the answer/audit pair has always counted
        // ops — `keeper::distribute` reads it to build its `written` list.
        let targets: Vec<Value> = ops.iter().map(|o| Value::String(o.path.clone())).collect();
        let rec = json!({
            "txId": tx_id,
            "status": "ok",
            "ms": ms,
            "kind": meta.get("kind").cloned().unwrap_or(Value::Null),
            "action": meta.get("action").cloned().unwrap_or(Value::Null),
            "agentId": meta.get("agentId").cloned().unwrap_or(Value::Null),
            "agentLabel": meta.get("agentLabel").cloned().unwrap_or(Value::Null),
            "projectId": meta.get("projectId").cloned().unwrap_or(Value::Null),
            "projectName": meta.get("projectName").cloned().unwrap_or(Value::Null),
            "assetId": meta.get("assetId").cloned().unwrap_or(Value::Null),
            "assetName": meta.get("assetName").cloned().unwrap_or(Value::Null),
            "targets": targets,
            "log": log,
        });
        settings::audit(&self.root_str, &rec);
        json!({ "ok": true, "txId": tx_id, "ms": ms, "targets": targets })
    }

    /// Put a captured state list back.
    ///
    /// **Every snapshot is attempted, and every failure is collected.** The old
    /// shape (`Option<String>`, `return` on the first error) meant a transaction
    /// touching N paths stopped at the first path it could not put back: everything
    /// after it stayed in its post-write state and the caller was told about one of
    /// them. The tree was silently half-undone while the answer read like a single
    /// fixable complaint. Returning the full list makes "only the first failure" a
    /// compile error rather than a policy — there is no shorter shape to return.
    ///
    /// A single failure still does not stop the others, so the damage from an
    /// unwritable path is bounded to that path.
    fn restore(&self, snaps: &[Snapshot]) -> Vec<RestoreFailure> {
        let mut out = Vec::new();
        for s in snaps {
            let target = Path::new(s.path());
            let outcome: Result<(), RestoreFailure> = match s {
                Snapshot::Absent { .. } => {
                    // `symlink_metadata`, not `exists()`: the op may have
                    // created a *dangling* link, and `exists()` follows links so
                    // it would report "nothing there" and leave it behind.
                    if std::fs::symlink_metadata(target).is_ok() {
                        rmrf(target);
                    }
                    // Deleting the last file of a freshly created directory (a
                    // new skill folder) would otherwise leave a phantom entry
                    // that some agents still enumerate.
                    if let Some(parent) = target.parent() {
                        prune_empty_parents(&native_str(parent), s.path());
                    }
                    Ok(())
                }
                Snapshot::File { content, .. } => {
                    if target.is_dir() {
                        // The op replaced a directory with a file (or the other
                        // way round); restoring means removing what is there.
                        rmrf(target);
                    }
                    match target.parent() {
                        Some(parent) => match std::fs::create_dir_all(parent) {
                            Ok(()) => std::fs::write(target, content)
                                .map_err(|e| RestoreFailure::plain(s.path(), e)),
                            Err(e) => Err(RestoreFailure::plain(&native_str(parent), e)),
                        },
                        None => std::fs::write(target, content)
                            .map_err(|e| RestoreFailure::plain(s.path(), e)),
                    }
                }
                Snapshot::Dir { entries, .. } => {
                    out.extend(restore_dir(target, entries));
                    Ok(())
                }
                Snapshot::Link {
                    target: link_to,
                    is_dir,
                    ..
                } => {
                    // The op may have left a file or directory where the link
                    // belongs: `symlink_file` on Windows fails with
                    // `ERROR_ALREADY_EXISTS` rather than replacing it, so the
                    // undo has to clear the path itself or it reports a failure
                    // for a tree it could in fact have restored.
                    if std::fs::symlink_metadata(target).is_ok() {
                        rmrf(target);
                    }
                    let created = if *is_dir {
                        symlink_dir(Path::new(link_to), target)
                    } else {
                        symlink_file(Path::new(link_to), target)
                    };
                    created.map_err(|e| {
                        RestoreFailure::new(
                            s.path(),
                            RESTORE_SYMLINK_FAILED,
                            format!(
                                "the link was not recreated ({}); a symbolic link on Windows needs administrator rights or developer mode — run the app as administrator, or recreate the link by hand",
                                e
                            ),
                        )
                    })
                }
            };
            if let Err(problem) = outcome {
                out.push(problem);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a handle that permits reading but **not** writing or deleting.
    ///
    /// This is the deterministic failure trigger for the undo tests: a virus
    /// scanner, a sync client or a second AgentKeeper holding an asset open is
    /// the real shape of it. `FILE_SHARE_READ` alone makes every write-open of
    /// the file fail with a sharing violation while reads still succeed, so the
    /// capture phase can read the file and the undo cannot write it back. A
    /// probe proves the environment really behaves that way before the test
    /// relies on it — without the probe, a future Windows that allowed the
    /// write would leave the test green while testing nothing.
    #[cfg(windows)]
    fn hold_read_only(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .expect("hold the path without FILE_SHARE_WRITE");
        assert!(
            std::fs::write(path, b"probe").is_err(),
            "the held handle did not block writes, so this test cannot reach the failing branch"
        );
        held
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ak-store-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_at(root: &Path) -> Store {
        Store::new(&root.to_string_lossy())
    }

    /// **The undo does not stop at the first path it cannot put back.**
    ///
    /// This is the regression the whole `Vec<RestoreFailure>` shape exists for.
    /// `restore` used to `return Some(<first failure>)`, so a transaction
    /// touching several paths left every path *after* the failing one in its
    /// post-write state while the answer named only one of them: the tree was
    /// silently half-undone and the error described a single problem.
    ///
    /// The exclusive handle is taken **after** the transaction, never before.
    /// Held across the transaction it makes the *write* fail instead, so the
    /// transaction aborts and its own undo has already put everything back —
    /// and then no restore problem exists to assert on.
    #[cfg(windows)]
    #[test]
    fn a_failed_restore_does_not_stop_the_paths_after_it() {
        let root = temp_root("restore-collect");
        let store = store_at(&root);

        let locked = root.join("locked.md");
        let untouched = root.join("a.md");
        let early = root.join("b.md");
        let late = root.join("c.md");

        // A *successful* transaction is what leaves several paths changed, so
        // the undo below has several paths to get wrong.
        let ok = store.transaction(
            &[
                Op::write(&locked.to_string_lossy(), "clobbered-locked"),
                Op::write(&untouched.to_string_lossy(), "clobbered-a"),
                Op::write(&early.to_string_lossy(), "clobbered-b"),
                Op::write(&late.to_string_lossy(), "clobbered-c"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(
            ok["ok"],
            json!(true),
            "the setup transaction failed: {}",
            ok
        );

        // Hold `locked.md` against writes, so only the *undo* can fail.
        let _lock = hold_read_only(&locked);
        let undone = store.restore(&[
            Snapshot::File {
                path: locked.to_string_lossy().into_owned(),
                content: b"pre-transaction".to_vec(),
            },
            Snapshot::File {
                path: untouched.to_string_lossy().into_owned(),
                content: b"original-a".to_vec(),
            },
            Snapshot::File {
                path: early.to_string_lossy().into_owned(),
                content: b"original-b".to_vec(),
            },
            Snapshot::File {
                path: late.to_string_lossy().into_owned(),
                content: b"original-c".to_vec(),
            },
        ]);

        assert_eq!(
            undone.len(),
            1,
            "exactly one path could not be put back: {:?}",
            undone
        );
        assert_eq!(undone[0].code(), RESTORE_PATH_FAILED, "{:?}", undone);
        for (p, want) in [
            (&untouched, "original-a"),
            (&early, "original-b"),
            (&late, "original-c"),
        ] {
            assert_eq!(
                std::fs::read_to_string(p).unwrap(),
                want,
                "the path AFTER the failing one was skipped — the undo stopped at the first failure"
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// Every failure is reported, not just the first — a caller that shows the
    /// list must not under-report what is sitting in a changed state on disk.
    ///
    /// Drives [`Store::restore`] directly for the same reason as the test above:
    /// to have several paths genuinely changed, the setup has to be a
    /// *successful* transaction. Its locks are taken afterwards.
    #[cfg(windows)]
    #[test]
    fn every_unrestorable_path_is_reported_not_just_the_first() {
        let root = temp_root("restore-all");
        let store = store_at(&root);

        let one = root.join("locked-1.md");
        let two = root.join("locked-2.md");
        let fine = root.join("fine.md");

        let ok = store.transaction(
            &[
                Op::write(&one.to_string_lossy(), "clobbered-1"),
                Op::write(&two.to_string_lossy(), "clobbered-2"),
                Op::write(&fine.to_string_lossy(), "clobbered-fine"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(
            ok["ok"],
            json!(true),
            "the setup transaction failed: {}",
            ok
        );

        let _l1 = hold_read_only(&one);
        let _l2 = hold_read_only(&two);
        let undone = store.restore(&[
            Snapshot::File {
                path: one.to_string_lossy().into_owned(),
                content: b"orig-1".to_vec(),
            },
            Snapshot::File {
                path: two.to_string_lossy().into_owned(),
                content: b"orig-2".to_vec(),
            },
            Snapshot::File {
                path: fine.to_string_lossy().into_owned(),
                content: b"orig-fine".to_vec(),
            },
        ]);

        assert_eq!(
            undone.len(),
            2,
            "both locked paths must be reported, not just the first: {:?}",
            undone
        );
        let named: Vec<&str> = undone.iter().map(|f| f.path()).collect();
        assert!(
            named.iter().any(|p| p.contains("locked-1.md"))
                && named.iter().any(|p| p.contains("locked-2.md")),
            "both paths named: {:?}",
            named
        );
        assert_eq!(
            std::fs::read_to_string(&fine).unwrap(),
            "orig-fine",
            "the writable path is restored even though two others are not"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The answer shape the renderer keys off: `errorCode` plus the full
    /// `restoreErrors` list, with `restoreError` kept as the joined form.
    ///
    /// The lock is taken *after* a successful transaction, so the transaction
    /// whose answer is under test is one that actually got as far as writing
    /// and then could not put everything back — the partial-success state the
    /// UI has to be able to say out loud.
    #[cfg(windows)]
    #[test]
    fn a_partly_undone_transaction_reports_its_code_and_every_failed_path() {
        let root = temp_root("restore-shape");
        let store = store_at(&root);

        let locked = root.join("locked.md");
        let fine = root.join("fine.md");
        std::fs::write(&locked, "orig-locked").unwrap();
        std::fs::write(&fine, "orig-fine").unwrap();

        let ok = store.transaction(
            &[
                Op::write(&locked.to_string_lossy(), "clobbered-locked"),
                Op::write(&fine.to_string_lossy(), "clobbered-fine"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(ok["ok"], json!(true), "{}", ok);

        // Rebuild the answer the failure path builds, from the real snapshots,
        // with the lock in place for the undo only.
        let mut budget = MAX_SNAPSHOT_BYTES;
        let snaps = vec![
            store
                .snapshot(&locked.to_string_lossy(), &mut budget)
                .unwrap(),
            store
                .snapshot(&fine.to_string_lossy(), &mut budget)
                .unwrap(),
        ];
        let _lock = hold_read_only(&locked);
        let failures = store.restore(&snaps);

        // This is the contract `transaction()` publishes for these failures.
        let error_code = if failures.is_empty() {
            TX_FAILED
        } else {
            STORE_RESTORE_INCOMPLETE
        };
        assert_eq!(error_code, STORE_RESTORE_INCOMPLETE);
        let errors: Vec<Value> = failures.iter().map(|f| f.to_json()).collect();
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert_eq!(errors[0]["code"], json!(RESTORE_PATH_FAILED));
        assert!(errors[0]["path"].as_str().unwrap().contains("locked.md"));
        // `path` is its own field: repeating it inside `detail` would make the
        // renderer print the same path twice for no gain.
        assert!(
            errors[0]["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the failure carries the OS reason: {:?}",
            errors[0]
        );

        // And the joined compat string carries the same single failure.
        assert!(restore_sentence(&failures).contains("locked.md"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// `restore_dir` empties the directory and rebuilds it, so an entry it fails
    /// on used to `return Err` and abandon the **rest** of the entries — leaving
    /// a tree holding only what it happened to reach. Every remaining entry must
    /// still be written.
    ///
    /// The failing entry is made deterministic without privileges: one entry
    /// creates a directory (`blocker/…`) and a later entry wants to put a *file*
    /// at that same name. Writing the file fails on the directory that is now
    /// standing there, while the entries on either side of it are ordinary
    /// writes. The list is built by hand precisely so the order is known — a
    /// walk-order-dependent trigger would make this test flaky rather than
    /// strict.
    #[test]
    fn a_partly_failed_directory_restore_still_writes_the_other_entries() {
        let root = temp_root("restore-dir-partial");
        let dir = root.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("before.md"), "clobbered-before").unwrap();
        std::fs::write(dir.join("after.md"), "clobbered-after").unwrap();
        std::fs::create_dir_all(dir.join("blocker")).unwrap();

        let entries = vec![
            DirSnap::File("before.md".to_string(), b"original-before".to_vec()),
            // Creates the directory `blocker`…
            DirSnap::File("blocker/inner.md".to_string(), b"original-inner".to_vec()),
            // …and then wants a *file* at that name, which cannot work.
            DirSnap::File("blocker".to_string(), b"original-blocker".to_vec()),
            DirSnap::File("after.md".to_string(), b"original-after".to_vec()),
        ];

        let failures = restore_dir(&dir, &entries);

        assert_eq!(
            failures.len(),
            1,
            "exactly one entry should fail: {:?}",
            failures
        );
        assert_eq!(failures[0].code(), RESTORE_PATH_FAILED, "{:?}", failures);
        // The entries on **both** sides of the failure are present.
        assert_eq!(
            std::fs::read_to_string(dir.join("before.md")).unwrap(),
            "original-before",
            "the tree stopped before the failing entry"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("after.md")).unwrap(),
            "original-after",
            "the entry AFTER the failing one is missing — restore_dir gave up"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("blocker").join("inner.md")).unwrap(),
            "original-inner"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A `restore_dir` that cannot write into the directory at all must say so
    /// **before** destroying the captured tree. `rmrf` runs first otherwise, and
    /// "could not rebuild" would arrive as "the directory you had is gone".
    #[test]
    fn a_directory_restore_that_cannot_write_keeps_the_captured_tree() {
        let root = temp_root("restore-dir-probe");
        let dir = root.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "keep-me").unwrap();

        // The probe path is a *directory*, so writing the probe file fails and
        // the guard trips — without needing a read-only directory, which Windows
        // does not enforce for the owner anyway.
        std::fs::create_dir_all(dir.join(".agentkeeper-restore-probe")).unwrap();

        let failures = restore_dir(
            &dir,
            &[DirSnap::File("keep.md".to_string(), b"rewritten".to_vec())],
        );

        assert_eq!(failures.len(), 1, "{:?}", failures);
        assert_eq!(failures[0].code(), RESTORE_PATH_FAILED, "{:?}", failures);
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.md")).unwrap(),
            "keep-me",
            "the tree was emptied even though it could not be rebuilt: {:?}",
            failures
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_lands_on_the_real_path() {
        let root = temp_root("direct");
        let store = store_at(&root);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::write(&p, "hello")],
            &json!({ "kind": "skill", "action": "save" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn transaction_writes_journal_and_audit() {
        let root = temp_root("journal");
        let store = store_at(&root);
        let p = root.join("a.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::write(&p, "body")],
            &json!({
                "kind": "skill",
                "action": "save",
                "assetId": "skill:x",
                "assetName": "X",
                "agentId": "claude",
            }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let tx_id = tx["txId"].as_str().unwrap().to_string();
        assert_eq!(tx["targets"], json!([p]));

        // The journal record is the audit line: the app keeps no per-transaction
        // file on disk, so `txId` exists only to correlate the two halves of the
        // log with each other and with the transaction's own answer.
        let audit = std::fs::read_to_string(store.audit_file()).unwrap();
        assert_eq!(audit.lines().count(), 1);
        let rec: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
        assert_eq!(rec["txId"], json!(tx_id));
        assert_eq!(rec["status"], json!("ok"));
        assert_eq!(rec["assetId"], json!("skill:x"));
        assert_eq!(rec["targets"], json!([p]));
        assert_eq!(rec["log"][0]["op"], json!("write"));
        assert_eq!(rec["log"][0]["bytes"], json!(4));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The product has no backup tree. A transaction — successful, refused or
    /// failed — must leave nothing behind but the audit journal, because a
    /// directory of old copies is exactly the thing this build no longer keeps.
    #[test]
    fn no_transaction_creates_a_backup_directory() {
        let root = temp_root("no-backups");
        let store = store_at(&root);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let ok = store.transaction(&[Op::write(&p, "body")], &json!({ "kind": "skill" }));
        assert_eq!(ok["ok"], json!(true), "{}", ok);

        // And the failure path, which is the one that used to copy the touched
        // paths out before writing them.
        let failed = store.transaction(
            &[
                Op::write(&p, "half"),
                Op::copy_dir(
                    &root.join("nope").to_string_lossy(),
                    &root.join("x").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(failed["ok"], json!(false), "{}", failed);

        assert!(
            !root.join("backups").exists(),
            "backups/ must not be created: {:?}",
            std::fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Undo of a *failed* transaction: a delete that ran before the failing op
    /// must put the file back byte for byte, from the in-memory snapshot.
    #[test]
    fn a_failed_transaction_restores_a_deleted_file_byte_for_byte() {
        let root = temp_root("fail-delete");
        let store = store_at(&root);
        let target = root.join("skill").join("SKILL.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "original").unwrap();
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::delete(&p),
                Op::copy_dir(
                    &root.join("gone").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx.get("restoreError").is_none(),
            "the restore must report no trouble: {}",
            tx
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");

        std::fs::remove_dir_all(&root).ok();
    }

    /// The mirror image: a path that did not exist must end up absent again, and
    /// the directory that only existed to hold it must be pruned — a phantom
    /// folder is still enumerated by some agents.
    #[test]
    fn a_failed_transaction_removes_a_created_file_and_prunes_its_dir() {
        let root = temp_root("fail-create");
        let store = store_at(&root);
        let dir = root.join("agents").join("fresh");
        let target = dir.join("SKILL.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&p, "new"),
                Op::copy_dir(
                    &root.join("gone").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(!target.exists());
        assert!(!dir.exists(), "empty parent left behind: {:?}", dir);
        // The store root itself must survive; it was not created by the write.
        assert!(root.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A directory op is the case the old disk-copy undo used to cover, and it is
    /// the one an in-memory snapshot could silently get wrong: the restore has to
    /// A directory op is the case the old disk-copy undo used to cover, and it is
    /// the one an in-memory snapshot could silently get wrong: the restore has to
    /// reproduce the *captured* tree, not merge into what is there now.
    ///
    /// The `CopyDir` op is the important one — it names the **directory** as the
    /// touched path, so the whole tree has to come back from one snapshot. An op
    /// that names a file inside it would get its own per-file snapshot and hide a
    /// merge-shaped restore.
    #[test]
    fn a_failed_transaction_restores_a_copied_directory_exactly() {
        let root = temp_root("fail-copiedir");
        let store = store_at(&root);
        let dir = root.join("plugin");
        let src = root.join("src");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("keep.md"), "keep").unwrap();
        std::fs::write(dir.join("nested").join("a.md"), "a").unwrap();
        // An empty subdirectory is state too, and must survive.
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.md"), "x").unwrap();
        let before = list_files(&dir);

        let tx = store.transaction(
            &[
                // This op copies *into* `dir`, so `dir` itself is what has to be
                // captured and put back. (`copy_dir` takes the destination first.)
                Op::copy_dir(&dir.to_string_lossy(), &src.to_string_lossy()),
                Op::copy_dir(
                    &root.join("nope").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert_eq!(
            list_files(&dir),
            before,
            "the directory tree is identical again"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("nested").join("a.md")).unwrap(),
            "a"
        );
        assert!(
            dir.join("empty").is_dir(),
            "an empty subdirectory is part of the state"
        );
        assert!(
            !dir.join("x.md").exists(),
            "a file the copy added must not survive: a merge is not a restore"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The size guard is a refusal, not a truncation: a tree that cannot be held
    /// in memory must stop the transaction *before* the first write, so the
    /// alternative to a correct restore is never "a partially restored one".
    #[test]
    fn an_uncapturable_directory_refuses_before_writing_anything() {
        let root = temp_root("oversize-dir");
        let store = store_at(&root);
        let dir = root.join("huge");
        let src = root.join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.md"), "small").unwrap();
        // One file over the whole transaction budget.
        std::fs::write(dir.join("blob.bin"), vec![0u8; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let other = root.join("other.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&other, "would have landed"),
                // The destination is the oversized tree, so capturing it is what
                // the transaction has to do — and what refuses.
                Op::copy_dir(&dir.to_string_lossy(), &src.to_string_lossy()),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("undo snapshot limit"),
            "the refusal names the limit: {}",
            tx
        );
        // The code, not the sentence, is what a renderer keys its wording off.
        assert_eq!(tx["errorCode"], json!(STORE_SNAPSHOT_TOO_LARGE), "{}", tx);
        // Nothing was written, so there is nothing to undo and nothing to report
        // about an undo: this is a refusal, not a partial write.
        assert!(tx.get("restoreError").is_none(), "{}", tx);
        assert!(tx.get("restoreErrors").is_none(), "{}", tx);
        assert!(
            !Path::new(&other).exists(),
            "the capture failed before the first op ran"
        );
        assert_eq!(
            std::fs::read_to_string(src.join("a.md")).unwrap(),
            "small",
            "the copy source is untouched"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The same guard on a single file: an oversized config is refused exactly
    /// like an oversized tree, because there is no partial snapshot for a file
    /// either.
    #[test]
    fn an_oversized_file_refuses_before_writing_anything() {
        let root = temp_root("oversize-file");
        let store = store_at(&root);
        let huge = root.join("huge.bin");
        std::fs::write(&huge, vec![0u8; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let other = root.join("other.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&other, "would have landed"),
                Op::write(&huge.to_string_lossy(), "clobbered"),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("undo snapshot limit"),
            "the refusal names the limit: {}",
            tx
        );
        assert_eq!(tx["errorCode"], json!(STORE_SNAPSHOT_TOO_LARGE), "{}", tx);
        assert!(tx.get("restoreErrors").is_none(), "{}", tx);
        // The message has to say what to do, not only that a number was hit —
        // a bare byte count leaves the reader with no next move.
        let msg = tx["error"].as_str().unwrap();
        assert!(
            msg.contains("split this asset into smaller writes"),
            "the refusal says what to do about it: {}",
            msg
        );
        assert!(
            msg.contains(&human_size(MAX_SNAPSHOT_BYTES)),
            "the refusal gives the limit in readable units: {}",
            msg
        );
        assert!(
            !Path::new(&other).exists(),
            "the capture failed before the first op ran"
        );
        assert_eq!(
            std::fs::metadata(&huge).unwrap().len() as usize,
            MAX_SNAPSHOT_BYTES + 1,
            "the oversized file was not truncated"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_op_merges_patch_and_keeps_siblings() {
        let root = temp_root("json-merge");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, r#"{"mcpServers":{"keep":{"command":"a"}}}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_mutate(&p, |cur| {
                let mut out = cur.clone();
                out["mcpServers"]["added"] = json!({ "command": "b" });
                Some(out)
            })],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["mcpServers"]["keep"]["command"], json!("a"));
        assert_eq!(after["mcpServers"]["added"]["command"], json!("b"));
        assert!(std::fs::read_to_string(&cfg).unwrap().ends_with("}\n"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_patch_is_a_shallow_merge() {
        let root = temp_root("json-patch");
        let store = store_at(&root);
        let cfg = root.join("settings.json");
        std::fs::write(&cfg, r#"{"a":1,"b":2}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "b": 9, "c": 3 }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after, json!({ "a": 1, "b": 9, "c": 3 }));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn corrupt_config_aborts_instead_of_dropping_siblings() {
        let root = temp_root("json-corrupt");
        let store = store_at(&root);
        let cfg = root.join("broken.json");
        std::fs::write(&cfg, "{ not json ").unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "x": 1 }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"].as_str().unwrap().contains("corrupt"),
            "unexpected error: {}",
            tx["error"]
        );
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ not json ");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_tolerated() {
        let root = temp_root("jsonc");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(
            &cfg,
            "{\n  // a comment\n  \"mcpServers\": { \"a\": { \"command\": \"x\" }, },\n}\n",
        )
        .unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "extra": true }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["mcpServers"]["a"]["command"], json!("x"));
        assert_eq!(after["extra"], json!(true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_dir_skips_build_trees() {
        let root = temp_root("copydir");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("node_modules").join("pkg").join("i.js"), "x").unwrap();
        std::fs::write(src.join("nested").join("a.md"), "a").unwrap();
        std::fs::write(src.join("b.md"), "b").unwrap();

        let dest = root.join("dest");
        copy_dir(&src, &dest);
        assert!(dest.join("b.md").exists());
        assert!(dest.join("nested").join("a.md").exists());
        assert!(
            !dest.join("node_modules").exists(),
            "build trees are not backed up"
        );

        let mut files = list_files(&src);
        files.sort();
        assert_eq!(files, vec!["b.md".to_string(), "nested/a.md".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_copy_source_fails_the_whole_transaction() {
        let root = temp_root("copydir-missing");
        let store = store_at(&root);
        let target = root.join("written.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&p, "should be rolled back"),
                Op::copy_dir(
                    &root.join("dest").to_string_lossy(),
                    &root.join("nope").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("copy source missing"),
            "{}",
            tx["error"]
        );
        assert!(!target.exists(), "the earlier write must be undone");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_mutate_returning_none_aborts() {
        let root = temp_root("mutate-abort");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, r#"{"a":1}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::json_mutate(&p, |_| None)], &json!({ "kind": "mcp" }));
        assert_eq!(tx["ok"], json!(false));
        assert!(tx["error"].as_str().unwrap().contains("mutate aborted"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), r#"{"a":1}"#);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mkdir_op_is_idempotent() {
        let root = temp_root("mkdir");
        let store = store_at(&root);
        let dir = root.join("a").join("b");
        let p = dir.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::mkdir(&p), Op::mkdir(&p)], &json!({}));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(dir.is_dir());
        assert_eq!(tx["targets"], json!([p, p]), "JS records one target per op");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tx_ids_are_unique_and_sortable() {
        let a = gen_tx_id();
        let b = gen_tx_id();
        assert!(a.starts_with("tx-"));
        assert_ne!(a, b);
        let tail = a.rsplit('-').next().unwrap();
        assert_eq!(tail.len(), 5, "{}", a);
        assert!(tail.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
