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

/// 事务 id 的格式：`tx-${Date.now().toString(36)}-${Math.random().toString(36).slice(2,7)}`
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
/* 内存中的撤销状态                                                        */
/* ------------------------------------------------------------------ */

/// 被捕获目录中的一个条目，相对于该目录。
#[derive(Clone)]
enum DirSnap {
    /// 一个子目录。记录下来是为了让空目录能在撤销后存活 —— 一个存在但为空的资产
    /// 目录是一种真实状态，丢掉它会在 agent 的目录树里留下一个幻影文件夹。
    Dir(String),
    File(String, Vec<u8>),
    /// `target` 加上"目标是一个目录"，正是这一点告诉恢复过程该调用 `symlink_dir`
    /// 而不是 `symlink_file`。
    Link(String, String, bool),
}

/// 一个事务触及了什么，以便失败时能把它放回去。
///
/// 它在内存中只存活事务运行的这段时间，随后被丢弃：产品不保留备份树，也不提供
/// 按 id 回滚，因此唯一的消费者是下面的 [`Store::restore`]。`Snapshot` 只会由
/// [`Store::snapshot`] 构造，而后者是全函数 —— 一个无法被捕获的路径会让整个事务
/// 在第一次写入*之前*拒绝，所以不存在"只能部分恢复"的变体。
enum Snapshot {
    /// 该路径此前不存在：撤销意味着移除本次事务在那里创建的东西。
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
    /// 一个符号链接，或一个悬空的符号链接：目标字符串加上目标是否为目录，正是它
    /// 决定恢复时选 `symlink_dir` 还是 `symlink_file`。链接不是文件 —— 把它当
    /// 文件重建，会把 `a -> b` 换成一个过期的 `b` 副本。
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
/* 存储                                                                  */
/* ------------------------------------------------------------------ */

pub struct Store {
    root: PathBuf,
    root_str: String,
}

impl Store {
    /// 如果存储树还不存在就创建它。
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
    /* 撤销状态                                                          */
    /* -------------------------------------------------------------- */

    /// 把一个路径（文件、目录或链接）捕获进内存。
    ///
    /// 存在性也是状态的一部分，所以撤销一次删除和撤销一次创建是同一段代码配不同的
    /// 数据。遍历遵守 [`SKIP_DIRS`] 的理由与 [`copy_dir`] 相同 —— 一棵
    /// `node_modules` 不属于任何 agent 资产的含义，把它留在 RAM 里不只是没用，
    /// 而是有害。
    ///
    /// `Err` 是拒绝，不是警告：一个超过 [`MAX_SNAPSHOT_BYTES`] 的目录必须让事务
    /// 在第一次写入之前失败，因为"恢复了其中大部分"正是这里绝不能留下的半写状态。
    fn snapshot(&self, original: &str, budget: &mut usize) -> Result<Snapshot, TxError> {
        let path = Path::new(original);
        // `exists()` 会跟随链接，所以悬空链接会被读成不存在 —— 用
        // `symlink_metadata` 来区分"没有这条路径"和"这是一个断链"。
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                return Ok(Snapshot::Absent {
                    path: original.to_string(),
                })
            }
        };

        if meta.is_symlink() {
            // 悬空链接不是错误，而是磁盘上已经存在的一种状态；此时 `metadata`
            // 会失败，于是该链接被记录为指向一个非目录，这是唯一能知道的事。
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

    /// `transaction(ops, meta)` —— 一个文件得以改变的唯一途径。
    ///
    /// `meta` 的键：`txId`、`kind`、`action`、`agentId`、
    /// `agentLabel`、`projectId`、`projectName`、`assetId`、`assetName`。
    ///
    /// 撤销保证有两半，而第一半是承重的那半：每个被触及的路径都在**第一个**操作
    /// 运行**之前**被捕获，所以捕获阶段的拒绝意味着什么都没有写入。第二半是
    /// [`Store::restore`]，它只在失败路径上运行。
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
        // 整个事务共用一份预算，而不是每条路径一份：五个 6 MiB 的目录和一个
        // 30 MiB 的目录是同一个问题，而这个上限是在陈述峰值内存，按路径分配的
        // 预算做不到这一点。
        let mut budget = MAX_SNAPSHOT_BYTES;

        let outcome: Result<(), TxError> = (|| {
            // 1. 在任何东西被移动之前，把事务将要触及的一切都捕获下来。这里的
            //    失败意味着什么都没有写入。
            //
            //    同一条路径被点名两次时只捕获一次。对同一路径的两个操作（distribute
            //    会为每个目标推送 `mkdir <rules dir>` + `write <rule>`，所以同一个
            //    父目录会跨目标反复出现）否则会被重复计入共享预算，并且在
            //    `restore` 里被重复计数 —— 而那里的*第二*份副本会试图放回一个
            //    第一个操作早已改变过的状态。事务前的这一次捕获才是唯一值得保留的。
            let mut captured: HashSet<String> = HashSet::new();
            for op in ops {
                if captured.insert(normalize_path(&op.path)) {
                    touched.push(
                        self.snapshot(&op.path, &mut budget)
                            .map_err(|e| e.naming(&op.path))?,
                    );
                }
            }

            // 2. 应用。
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
            // 一次本身也失败的恢复会留下半写的目录树，所以这些失败被折进应答里，
            // 而不是被吞掉 —— 全部失败都折进去，每条还带着自己的 code，好让调用方
            // 不必读散文就能区分"什么都没写"和"写了，然后部分撤销"。
            let restore_errors = self.restore(&touched);
            let error = err.message;
            let error_code = if restore_errors.is_empty() {
                err.code
            } else {
                // 恢复失败是两者中更可操作的那个：操作错误是导致写入失败的原因，
                // 而这个才是用户需要到磁盘上查看的东西。
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
            // `restoreErrors` 是完整列表；`restoreError` 是为早于它的调用方保留的
            // 拼接形式。空列表会被省略而不是以 `[]` 发出去，这样在旧契约下
            // `get("restoreError")` 仍然能回答"撤销是否干净"。
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
        // 每个**操作**一条记录，而不是每条被捕获的路径一条：这两个列表是刻意不同的
        // （同一路径上的 `mkdir` 和 `write` 是两个操作、一份快照），而应答/审计这一对
        // 一直统计的是操作 —— `keeper::distribute` 读它来构造自己的 `written` 列表。
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

    /// 把捕获到的状态列表放回去。
    ///
    /// **每一份快照都会被尝试，每一处失败都会被收集。** 旧的形式
    /// （`Option<String>`，在第一个错误处 `return`）意味着一个触及 N 条路径的
    /// 事务会在第一条放不回去的路径处停下：它之后的每一条路径都停留在写入之后的
    /// 状态，而调用方只被告知了其中一条。目录树被悄悄撤销了一半，应答却读起来像
    /// 只有一个可修的抱怨。返回完整列表让"只报告第一个失败"变成一个编译错误而
    /// 不是一条策略 —— 没有更短的形状可以返回。
    ///
    /// 单个失败仍不会让其他失败停下，所以一条不可写路径造成的损害被限制在那一
    /// 条路径上。
    fn restore(&self, snaps: &[Snapshot]) -> Vec<RestoreFailure> {
        let mut out = Vec::new();
        for s in snaps {
            let target = Path::new(s.path());
            let outcome: Result<(), RestoreFailure> = match s {
                Snapshot::Absent { .. } => {
                    // 用 `symlink_metadata` 而不是 `exists()`：操作可能创建了一个
                    // *悬空*链接，而 `exists()` 会跟随链接，于是它会报告"那里没
                    // 东西"并把它留在原地。
                    if std::fs::symlink_metadata(target).is_ok() {
                        rmrf(target);
                    }
                    // 否则删掉一个刚创建目录里的最后一个文件（比如新建的 skill
                    // 文件夹）会留下一个幻影条目，而某些 agent 仍会把它枚举出来。
                    if let Some(parent) = target.parent() {
                        prune_empty_parents(&native_str(parent), s.path());
                    }
                    Ok(())
                }
                Snapshot::File { content, .. } => {
                    if target.is_dir() {
                        // 该操作可能把目录换成了文件（或反过来）；恢复意味着移除
                        // 现在那里的东西。
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
                    // 该操作可能在链接该在的位置留下了文件或目录：Windows 上的
                    // `symlink_file` 会以 `ERROR_ALREADY_EXISTS` 失败而不是替换它，
                    // 所以撤销必须自己清掉这条路径，否则它会为一棵其实本可以恢复
                    // 的目录树报告失败。
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

    /// 打开一个只允许读取、**不允许**写入或删除的句柄。
    ///
    /// 这是撤销测试的确定性失败触发器：病毒扫描器、同步客户端或另一个持有资产
    /// 句柄的 AgentKeeper 就是它的真实形态。仅仅 `FILE_SHARE_READ` 会让对该文件
    /// 的每一次写入打开都以共享冲突失败，而读取仍然成功，于是捕获阶段能读到该
    /// 文件、撤销却写不回去。一次探针会在测试依赖它之前证明环境确实是这个行为
    /// —— 没有探针的话，某个将来允许写入的 Windows 会让测试一直是绿的，却什么
    /// 也没测到。
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

    /// **撤销不会在第一条放不回去的路径处停下。**
    ///
    /// 这正是整个 `Vec<RestoreFailure>` 形状所存在的理由。`restore` 过去是
    /// `return Some(<first failure>)`，所以一个触及多条路径的事务会让失败之后的
    /// 每一条路径都停留在写入之后的状态，而应答只提到其中一条：目录树被悄悄
    /// 撤销了一半，报错却描述的是单个问题。
    ///
    /// 独占句柄是在事务**之后**才拿到的，绝不在之前。若跨事务持有，它会让*写入*
    /// 失败，于是事务中止、它自己的撤销已经把所有东西放回去了 —— 那时就没有可
    /// 以断言的恢复问题存在了。
    #[cfg(windows)]
    #[test]
    fn a_failed_restore_does_not_stop_the_paths_after_it() {
        let root = temp_root("restore-collect");
        let store = store_at(&root);

        let locked = root.join("locked.md");
        let untouched = root.join("a.md");
        let early = root.join("b.md");
        let late = root.join("c.md");

        // 一次*成功*的事务才会让多条路径被改动，所以下面的撤销才有好几条路径
        // 可能出错。
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

        // 挡住对 `locked.md` 的写入，让只有*撤销*失败。
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

    /// 每一处失败都被报告，而不只是第一处 —— 一个要展示这个列表的调用方，绝不能
    /// 低估磁盘上处于已改动状态的东西。
    ///
    /// 直接驱动 [`Store::restore`]，理由与上面的测试相同：要有多条路径真正被改动，
    /// 设置步骤必须是一次*成功*的事务。它的锁是在之后才拿到的。
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

    /// 渲染层所依据的应答形状：`errorCode` 加上完整的 `restoreErrors` 列表，
    /// 同时保留 `restoreError` 作为拼接形式。
    ///
    /// 锁是在一次成功的事务*之后*才拿到的，所以被测试应答的那个事务确实走到了
    /// 写入、然后没能把一切放回去 —— 正是 UI 必须能大声说出来的半成功状态。
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

        // 用真实的快照重建失败路径所构建的应答，只让锁在撤销期间生效。
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

        // 这就是 `transaction()` 为这些失败所公布的契约。
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
        // `path` 是独立字段：在 `detail` 里重复它会让渲染层毫无收益地把同一条
        // 路径打印两遍。
        assert!(
            errors[0]["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the failure carries the OS reason: {:?}",
            errors[0]
        );

        // 兼容用的拼接字符串也带着同一条失败。
        assert!(restore_sentence(&failures).contains("locked.md"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// `restore_dir` 会清空目录再重建，所以它失败的条目过去会 `return Err` 并放弃
    /// **其余**条目 —— 留下一棵只包含它恰好走到的那部分的目录树。剩下的每一个条目
    /// 都必须仍然被写入。
    ///
    /// 失败的条目被做成确定性的，而且不需要特权：一个条目创建目录（`blocker/…`），
    /// 之后一个条目想在同一个名字上放一个*文件*。在已经立在那里的目录上写文件会
    /// 失败，而它两侧的条目都是普通写入。这个列表是手工搭出来的，正是为了顺序
    /// 确定 —— 依赖遍历顺序的触发条件会让这个测试变得不稳定而非严格。
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
            // 创建目录 `blocker`……
            DirSnap::File("blocker/inner.md".to_string(), b"original-inner".to_vec()),
            // ……然后想在同一个名字上放一个*文件*，这不可能成功。
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
        // 失败条目**两侧**的条目都在。
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

    /// 一个根本没法往目录里写的 `restore_dir` 必须在销毁捕获到的目录树**之前**
    /// 把这件事说出来。否则 `rmrf` 会先跑，而"无法重建"就会以"你原本的目录没了"
    /// 的形式送达。
    #[test]
    fn a_directory_restore_that_cannot_write_keeps_the_captured_tree() {
        let root = temp_root("restore-dir-probe");
        let dir = root.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "keep-me").unwrap();

        // 探针路径是一个*目录*，所以写探针文件会失败、守卫会被触发 —— 而且
        // 不需要一个只读目录，Windows 对属主本来就不强制这一点。
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

        // 日志记录就是那行审计记录：应用不在磁盘上保留每事务一个文件，所以
        // `txId` 存在的意义仅是把日志的两半彼此、以及与事务自身的应答关联起来。
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

    /// 产品没有备份树。一个事务 —— 无论成功、被拒绝还是失败 —— 除了审计日志之外
    /// 绝不能留下任何东西，因为一个存放旧副本的目录正是这个版本不再保留的东西。
    #[test]
    fn no_transaction_creates_a_backup_directory() {
        let root = temp_root("no-backups");
        let store = store_at(&root);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let ok = store.transaction(&[Op::write(&p, "body")], &json!({ "kind": "skill" }));
        assert_eq!(ok["ok"], json!(true), "{}", ok);

        // 以及失败路径 —— 正是它过去会在写入之前把被触及的路径拷贝出去。
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

    /// *失败*事务的撤销：在失败的操作之前执行的一次删除，必须从内存快照里逐字节
    /// 把文件放回去。
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

    /// 镜像的情形：一条本来不存在的路径最终必须再次不存在，而那个仅仅为了容纳它
    /// 而存在的目录必须被清掉 —— 幻影文件夹仍会被某些 agent 枚举到。
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
        // 存储根目录本身必须存活；它不是这次写入创建出来的。
        assert!(root.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// 目录操作正是旧的磁盘拷贝撤销所覆盖的情形，也是内存快照可能悄悄搞错的
    /// 那个：恢复必须复现*捕获到的*目录树，而不是合并进现在的东西里。
    ///
    /// 关键的是 `CopyDir` 操作 —— 它把**目录**本身作为被触及的路径，所以整棵树
    /// 都必须从同一份快照里回来。一个指名其中某个文件的操作会得到自己的逐文件
    /// 快照，从而掩盖掉一次合并式的恢复。
    #[test]
    fn a_failed_transaction_restores_a_copied_directory_exactly() {
        let root = temp_root("fail-copiedir");
        let store = store_at(&root);
        let dir = root.join("plugin");
        let src = root.join("src");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("keep.md"), "keep").unwrap();
        std::fs::write(dir.join("nested").join("a.md"), "a").unwrap();
        // 一个空子目录同样是一种状态，必须存活下来。
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.md"), "x").unwrap();
        let before = list_files(&dir);

        let tx = store.transaction(
            &[
                // 这个操作是拷贝*进* `dir`，所以 `dir` 本身才是必须被捕获并放回的
                // 东西。（`copy_dir` 的第一个参数是目标位置。）
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

    /// 尺寸守卫是一次拒绝，而不是一次截断：一棵没法在内存中容纳的目录树必须在
    /// 第一次写入*之前*就让事务停下，于是"正确恢复"的替代方案永远不会变成
    /// "恢复了一半"。
    #[test]
    fn an_uncapturable_directory_refuses_before_writing_anything() {
        let root = temp_root("oversize-dir");
        let store = store_at(&root);
        let dir = root.join("huge");
        let src = root.join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.md"), "small").unwrap();
        // 一个就超出整个事务预算的文件。
        std::fs::write(dir.join("blob.bin"), vec![0u8; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let other = root.join("other.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&other, "would have landed"),
                // 目标就是那棵超大的目录树，所以捕获它正是事务必须做的事 ——
                // 也正是它拒绝的原因。
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
        // 是 code 而非那个句子，才是渲染层措辞的依据。
        assert_eq!(tx["errorCode"], json!(STORE_SNAPSHOT_TOO_LARGE), "{}", tx);
        // 什么都没写入，所以既没有东西需要撤销、也没有关于撤销的失败可报告：
        // 这是一次拒绝，不是一次部分写入。
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

    /// 同样的守卫作用在单个文件上：超大的配置和超大的目录树一样被精确地拒绝，
    /// 因为文件同样没有"部分快照"这种东西。
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
        // 消息必须说明该怎么办，而不只是说撞上了一个数字 —— 一个光秃秃的字节数
        // 会让读者毫无下一步可走。
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
