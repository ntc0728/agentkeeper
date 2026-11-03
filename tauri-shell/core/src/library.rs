//! 库根与链接台账（`links.json`）。
//!
//! 「库」是一份主副本的家：skill 目录、规则片段、记忆目录、MCP 规范形态都放在
//! 这里，agent 侧只留链接（或受管副本）。默认库根是 `~/.agents` ——
//! 它与扫描里那个 `shared` 伪 agent（`~/.agents/skills`）是同一个位置，
//! 所以收敛之后不会凭空多出一份「库目录里的资产」。
//!
//! ## 台账不是真相
//!
//! `links.json` 是**索引**：真相永远是磁盘（链接实际指向哪里）。启动时 `doctor`
//! 以磁盘为准修正台账，任何「台账说有、盘上没有」的条目都会被降级成 finding。
//! 这条与 `store.rs` 的整体立场一致：写入直落真实路径，界面显示的就是实际路径。
//!
//! 正因为台账可以被重建，它损坏时**不能当空**：返回 `Err` 让调用方去重建，
//! 而「空台账」会静默地把每一条链接变成「未纳管」，那是另一种数据丢失。

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::linkfs::{self, LinkCap, LinkMethod};
use crate::store;

/// 库内四类主副本的子目录。顺序即 UI 里的标签顺序。
pub const KINDS: [&str; 4] = ["skills", "mcp", "rules", "memory"];

/// 台账文件的相对位置（相对库根）。
const LEDGER_FILE: &str = "links.json";

/// 一次解析出来的库句柄。
///
/// 只持有 `root`：库位置可以被设置切换（`library_migrate`），所以每次用到时都从
/// 设置里重新解析，而不是把它缓存进一个长生命周期对象。
#[derive(Clone, Debug)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    pub fn at(root: impl Into<PathBuf>) -> Library {
        Library { root: root.into() }
    }

    /// 库根：设置里的 `libraryRoot`，未设置（空串）时默认 `~/.agents`。
    ///
    /// **不创建目录**，只解析 —— 解析是纯函数，创建目录是写入，两者不能混。
    /// `libraryRoot` 里的 `~` 按 `user_home` 展开，其余按原样（绝对路径）使用。
    pub fn resolve(user_home: &str, settings: &Value) -> Library {
        let configured = settings
            .get("libraryRoot")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let root = if configured.is_empty() {
            join_home(user_home, ".agents")
        } else {
            expand_home(configured, user_home)
        };
        Library { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 四类主副本目录之一。未知 kind 退回库根本身，让调用方的 `join` 仍然落在库内。
    pub fn dir_for(&self, kind: &str) -> PathBuf {
        let name = match kind {
            "skill" => "skills",
            "rule" => "rules",
            "memory" => "memory",
            "mcp" => "mcp",
            other => other,
        };
        self.root.join(name)
    }

    /// 一个 skill 的主副本目录（`<root>/skills/<slug>`）。
    pub fn skill_dir(&self, slug: &str) -> PathBuf {
        self.dir_for("skill").join(slug)
    }

    /// 一个 MCP 规范形态文件（`<root>/mcp/servers/<slug>.json`）。
    pub fn mcp_record_path(&self, slug: &str) -> PathBuf {
        self.dir_for("mcp").join("servers").join(format!("{}.json", slug))
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.root.join(LEDGER_FILE)
    }

    /// 读台账。文件不存在 = 空台账；文件存在但读不懂 = `Err`，**不是**空台账。
    ///
    /// 损坏的判定包含两种情况：不是合法 JSON，或者 `entries` 不是数组 ——
    /// 后者是「有人把结构改坏了」的同一种事故，静默丢掉会让所有链接变成未纳管。
    pub fn load_ledger(&self) -> Result<Ledger, String> {
        let path = self.ledger_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
            Err(e) => {
                return Err(format!(
                    "the link ledger could not be read ({}): {}",
                    path.display(),
                    e
                ))
            }
        };
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
            format!(
                "the link ledger is not valid JSON, so it was left untouched ({}): {}",
                path.display(),
                e
            )
        })?;
        let entries = parsed
            .get("entries")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                format!(
                    "the link ledger has no \"entries\" array, so it was left untouched ({})",
                    path.display()
                )
            })?;
        let mut out = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            let entry = LinkEntry::from_json(e).ok_or_else(|| {
                format!(
                    "the link ledger entry #{} is malformed, so it was left untouched ({})",
                    i,
                    path.display()
                )
            })?;
            out.push(entry);
        }
        // 迁移标记坏了不该让整本台账读不出来：它是进度记录，丢了最多是重跑一遍
        // （迁移本身必须幂等），而台账读不出来会让所有链接变成「未纳管」。
        let migrating = parsed.get("migrating").and_then(Migrating::from_json);
        Ok(Ledger {
            entries: out,
            migrating,
        })
    }

    /// 写台账。不存在时创建；写入是原子的（`settings::atomic_write`），
    /// 半写的台账比没有台账更糟。
    pub fn save_ledger(&self, l: &Ledger) -> Result<(), String> {
        let path = self.ledger_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                format!("the library directory could not be created ({}): {}", dir.display(), e)
            })?;
        }
        let body = serde_json::to_string_pretty(&l.to_json())
            .map_err(|e| format!("the link ledger could not be serialised: {}", e))?;
        crate::settings::atomic_write(&path, &format!("{}\n", body))
            .map_err(|e| format!("the link ledger could not be written ({}): {}", path.display(), e))
    }

    /// 登记一条链接（只记账，不落盘）。真正的落盘由调用方组织成 Op 走 transaction。
    pub fn record(&self, l: &mut Ledger, entry: LinkEntry) {
        l.upsert(entry);
    }

    /// **跨进程**的台账读-改-写：持有 `<store_root>/.lock` 期间完成
    /// 「读 → 改 → 写」三件事。
    ///
    /// 为什么必须是一个整体：`load_ledger` + `save_ledger` 分开调用时，两个进程
    /// 可以交替成「A 读 → B 读 → A 写 → B 写」，B 的那次写会把 A 刚登记的条目
    /// 盖掉，而两边都返回成功。桌面端与 `agentkeeper-mcp` 同时启用不同资产就是
    /// 这个形状（`keeper::record_link`、`disable_asset` 等）。
    ///
    /// 台账损坏时**照旧报错**、不做任何写入（与 `load_ledger` 的立场一致）：
    /// 覆盖一本读不出来的台账等于静默丢光全部链接记录。
    pub fn update_ledger<F>(&self, store_root: &str, f: F) -> Result<(), String>
    where
        F: FnOnce(&mut Ledger) -> Result<(), String>,
    {
        let _guard = crate::flock::StoreLock::acquire(store_root, crate::flock::DEFAULT_TIMEOUT)?;
        let mut ledger = self.load_ledger()?;
        f(&mut ledger)?;
        self.save_ledger(&ledger)
    }

    /// **跨进程**的台账整本替换：持锁期间把 `ledger` 原样写盘，**不先读**。
    ///
    /// 只有一个调用方：`doctor` 的 `link.ledgerDrift(rebuild)`。重建的目的是
    /// 换掉一本**读不出来**的台账 —— [`Self::update_ledger`] 会先 `load_ledger`
    /// 并在损坏时返回 `Err`，那样重建永远开不了头。这里持锁直接写：磁盘事实
    /// 已经算好（`rebuild_ledger`），锁只保证「写回的瞬间没有别人在动它」。
    pub fn replace_ledger(&self, store_root: &str, ledger: &Ledger) -> Result<(), String> {
        let _guard = crate::flock::StoreLock::acquire(store_root, crate::flock::DEFAULT_TIMEOUT)?;
        self.save_ledger(ledger)
    }

    /// 库内全部 MCP 规范形态记录（`<root>/mcp/servers/*.json`），按文件名排序。
    ///
    /// **只读**：目录不存在 = 空列表（还没有任何 MCP 进库，是正常状态，不是错）。
    /// 一条读不懂的记录**跳过但不猜** —— 它不参与物化与漂移判定；把读不懂的记录
    /// 当成空记录去写，等于把用户库里那条内容抹掉。
    ///
    /// 排序是稳定输出的一部分：漂移列表与物化顺序都跟着它，界面上同一份磁盘
    /// 状态每次打开看到的顺序必须一样，否则「刚才那条呢」会成为一个问题。
    pub fn mcp_records(&self) -> Vec<Value> {
        let dir = self.dir_for("mcp").join("servers");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("json")).unwrap_or(false))
            .collect();
        files.sort();
        let mut out = Vec::new();
        for f in files {
            let Ok(raw) = std::fs::read_to_string(&f) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if v.is_object() {
                out.push(v);
            }
        }
        out
    }

    /// `p`（或其目标）是否位于库内。
    ///
    /// `disable_asset` 的安全闸与 `link.unmanaged` 的判定共用这一个函数：一个链接
    /// 指向库外时，删掉它会动到用户自己的目录，那是本功能绝不做的事。
    pub fn owns_target(&self, p: &Path) -> bool {
        // 链接看指向（`read_target` 不跟随、只读重解析点），真实路径看自身。
        // 不能把两者都丢进 `normalize` 比较：`normalize` 现在不跟随链接，而这里
        // 恰恰需要「这条链接指向哪里」。
        let target = linkfs::read_target(p).unwrap_or_default();
        is_under(&target, &self.root) || is_under(p, &self.root)
    }

    /// `p` 是否是库内某个 skill 的主副本目录（`<root>/skills/<slug>` 或更深）。
    pub fn owns_skill_dir(&self, p: &Path) -> bool {
        let skills = normalize(&self.skill_dir(""));
        let s = normalize(p);
        s != skills && s.starts_with(&format!("{}\\", skills))
    }

    /// 这台机器上库能不能用来建链接。返回 `(cap, 说明)`，说明在 `None` 时必定有值。
    pub fn capability(&self, store_root: &str) -> LinkCap {
        // 探测写在 store 下而不是库内：库可能位于只读介质或网络盘，而能力探测
        // 本来就不该在用户的共享目录里留下任何东西。
        let probe_root = Path::new(store_root).join("probe");
        let cache = Path::new(store_root).join("linkcap.txt");
        linkfs::capability(&probe_root, Some(&cache))
    }
}

/// 库内全部 MCP 规范形态记录。与 [`Library::mcp_records`] 同一份实现，给「手上只有
/// `&Library`、不需要方法调用风格」的调用方（`sharing::plan_drift`）用。
pub fn mcp_records(library: &Library) -> Vec<Value> {
    library.mcp_records()
}

fn join_home(user_home: &str, rel: &str) -> PathBuf {
    Path::new(user_home).join(rel)
}

/// `~/x` / `~\x` → `<user_home>\x`；其它原样返回（`libraryRoot` 允许绝对路径）。
fn expand_home(raw: &str, user_home: &str) -> PathBuf {
    let rest = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| if raw == "~" { Some("") } else { None });
    match rest {
        Some(rest) => {
            let mut p = Path::new(user_home).to_path_buf();
            if !rest.is_empty() {
                p.push(rest);
            }
            p
        }
        None => PathBuf::from(raw),
    }
}

/// 比较用的小写化路径：**只做词法归一，绝不跟随链接**。
///
/// 跟随链接（`canonicalize`）在这里是一个数据错误：agent 侧每一条链接都指回库内
/// 主副本，用 canonicalize 归一后它们**全部坍缩成同一个键**，`Ledger::upsert` 的
/// 去重就会把五条链接当成同一条，最后只剩最后一条。同样地，`links_to(src)` 会
/// 「找到」每一条链向那个主副本的链接。链接的身份是它**自己的路径**，不是它现在
/// 指向哪里 —— 这一条与 `linkfs::same_target`（判断「是不是同一个物理对象」，
/// 必须跟随）正好相反，两者不能互相替代。
///
/// 规则：反斜杠统一、去掉 `\\?\` 前缀、抹平 `.` / `..` 与重复分隔符、去尾分隔符、
/// 小写。`..` 只在能抵掉一个普通段时才折叠，UNC 的 `\\server\share` 前缀原样保留。
fn normalize(p: &Path) -> String {
    let raw = store::native_str(p);
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string();
    let (prefix, rest) = split_unc(&raw);
    let mut out: Vec<&str> = Vec::new();
    for seg in rest.split('\\').filter(|s| !s.is_empty() && *s != ".") {
        if seg == ".." {
            if out.last().map(|s| *s != "..").unwrap_or(false) {
                out.pop();
            }
        } else {
            out.push(seg);
        }
    }
    let joined = if prefix.is_empty() {
        out.join("\\")
    } else {
        format!("{}{}", prefix, out.join("\\"))
    };
    joined.trim_end_matches('\\').to_lowercase()
}

/// 把 `\\server\share\...` 切成（`\\server\share\`，其余）。
fn split_unc(raw: &str) -> (String, String) {
    if let Some(rest) = raw.strip_prefix(r"\\") {
        let mut it = rest.splitn(3, '\\');
        let server = it.next().unwrap_or_default();
        let share = it.next().unwrap_or_default();
        let tail = it.next().unwrap_or_default();
        return (format!(r"\\{}\{}\", server, share), tail.to_string());
    }
    (String::new(), raw.to_string())
}

/// `p` 的**字面路径**是否落在 `root` 之下（含相等，词法归一、不跟随链接）。
///
/// 「目标是否在库内」要看链接的指向，那是 [`Library::owns_target`]；这里只用来看
/// 「这条路径本身是不是库里的东西」。两者共用同一份 `normalize`，所以不会出现
/// 「安全闸说在库内、扫描说不在」的分歧。
pub fn is_under(p: &Path, root: &Path) -> bool {
    let r = normalize(root);
    if r.is_empty() {
        return false;
    }
    let s = normalize(p);
    s == r || s.starts_with(&format!("{}\\", r))
}

/* ------------------------------------------------------------------ */
/* 台账                                                                */
/* ------------------------------------------------------------------ */

/// 台账：一个扁平的链接列表。没有嵌套、没有索引表 —— 它会被人工读、被 doctor 修。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    pub entries: Vec<LinkEntry>,
    /// 迁移中的标记：库位置切换做到一半时写在这里，重跑时从它继续。
    pub migrating: Option<Migrating>,
}

/// 库位置迁移的进度。`from`/`to` 都是库根，`done` 是已搬完的 dst 列表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migrating {
    pub from: String,
    pub to: String,
    pub done: Vec<String>,
    /// 迁移开始时算出的顶层子项数（进度分母）。`from_json` 对旧 journal 缺省 0 ——
    /// 读到 0 时调用方按「未知总数」呈现，不显示百分比。
    pub total: usize,
    /// 已重指到新库根的链接 dst 列表。
    ///
    /// 顶层子项靠「磁盘事实」（源侧不在了）就能续跑，链接不行：一条链接在磁盘上
    /// 看不出它已经被指到新库根还是旧库根 —— 旧库根本身还要等第 4 步才删，所以
    /// 中断之后「链接还指着旧根」和「已经指对了」长得一模一样。不记下来，每次
    /// 重跑都会把全部链接再解一次、再建一次：几百次无谓写入，而且中途任何一个
    /// 失败窗口都可能留下一条悬空链接。
    pub relinked: Vec<String>,
}

impl Migrating {
    fn from_json(v: &Value) -> Option<Migrating> {
        let list = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|x| x.to_string()))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        };
        Some(Migrating {
            from: v.get("from").and_then(|x| x.as_str())?.to_string(),
            to: v.get("to").and_then(|x| x.as_str())?.to_string(),
            done: list("done"),
            total: v.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
            relinked: list("relinked"),
        })
    }
}

impl Ledger {
    /// 把台账里某个 `dst` 的 `src` 换掉，返回是否真的改了。
    ///
    /// 迁移用它把「这条链接现在指向新库根」写成一行台账记录 —— 否则重跑时
    /// 只能从磁盘上「这条链接还指着哪」反推，而旧库根在中途还在，反推不出
    /// 「已经指对了」。
    pub fn repoint(&mut self, dst: &str, new_src: &str) -> bool {
        let key = normalize(Path::new(dst));
        match self.entries.iter_mut().find(|e| normalize(Path::new(&e.dst)) == key) {
            Some(e) => {
                if normalize(Path::new(&e.src)) == normalize(Path::new(new_src)) {
                    return false;
                }
                e.src = new_src.to_string();
                true
            }
            None => false,
        }
    }

    /// 同一个 `dst` 只保留一条：链接是唯一的，重复条目会让「谁建的这一条」变成
    /// 竞猜。重新登记等于更新。
    pub fn upsert(&mut self, entry: LinkEntry) {
        let key = normalize(Path::new(&entry.dst));
        match self.entries.iter_mut().find(|e| normalize(Path::new(&e.dst)) == key) {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    /// 直接推入一条（不做去重）。只给测试与迁移用 —— 生产路径一律走 `upsert`。
    pub fn push_raw(&mut self, entry: LinkEntry) {
        self.entries.push(entry);
    }

    /// 删掉一个 `dst` 的登记，返回被删掉的那条。
    pub fn remove(&mut self, dst: &str) -> Option<LinkEntry> {
        let key = normalize(Path::new(dst));
        let idx = self.entries.iter().position(|e| normalize(Path::new(&e.dst)) == key)?;
        Some(self.entries.remove(idx))
    }

    /// 某个 `src` 的全部链接（删库内主副本时要一并解除）。
    pub fn links_to(&self, src: &str) -> Vec<&LinkEntry> {
        let key = normalize(Path::new(src));
        self.entries
            .iter()
            .filter(|e| normalize(Path::new(&e.src)) == key)
            .collect()
    }

    pub fn find(&self, dst: &str) -> Option<&LinkEntry> {
        let key = normalize(Path::new(dst));
        self.entries.iter().find(|e| normalize(Path::new(&e.dst)) == key)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("schema".to_string(), json!(1));
        map.insert(
            "entries".to_string(),
            Value::Array(self.entries.iter().map(LinkEntry::to_json).collect()),
        );
        if let Some(m) = &self.migrating {
            map.insert(
                "migrating".to_string(),
                json!({
                    "from": m.from,
                    "to": m.to,
                    "done": m.done,
                    "total": m.total,
                    "relinked": m.relinked,
                }),
            );
        }
        Value::Object(map)
    }
}

/// 台账里的一条。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkEntry {
    /// 资产类别：`skill` | `rule` | `memory` | `mcp`
    pub kind: String,
    /// 名称的 slug 形态（与库内目录名一致）
    pub name: String,
    /// 库内主副本绝对路径
    pub src: String,
    /// agent 侧路径
    pub dst: String,
    pub method: LinkMethod,
    /// `link` | `copy`（copy 模式下 dst 是受管副本，method 记 `Copy`）
    pub mode: String,
    /// `library`（本应用建的）| `external`（迁移时发现的外部链接）
    pub owner: String,
    pub created_at: String,
    pub last_sync_at: String,
}

impl LinkEntry {
    /// 新建一条，`created_at`/`last_sync_at` 取当前时间。
    pub fn new(kind: &str, name: &str, src: &str, dst: &str, method: LinkMethod) -> LinkEntry {
        let now = now_iso();
        LinkEntry {
            kind: kind.to_string(),
            name: name.to_string(),
            src: src.to_string(),
            dst: dst.to_string(),
            method,
            mode: if method == LinkMethod::Copy { "copy" } else { "link" }.to_string(),
            owner: "library".to_string(),
            created_at: now.clone(),
            last_sync_at: now,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "name": self.name,
            "src": self.src,
            "dst": self.dst,
            "method": self.method.as_str(),
            "mode": self.mode,
            "owner": self.owner,
            "createdAt": self.created_at,
            "lastSyncAt": self.last_sync_at,
        })
    }

    fn from_json(v: &Value) -> Option<LinkEntry> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
        let method_raw = s("method")?;
        // 未知 method 视为损坏：猜一个方式会让 doctor 修错方向。
        let method = match LinkMethod::parse(&method_raw) {
            Some(m) => m,
            None if method_raw == "copy" => LinkMethod::Copy,
            None => {
                // `copy` 已经被上面接住；走到这里说明是真正未知的值。
                return None;
            }
        };
        let now = now_iso();
        Some(LinkEntry {
            kind: s("kind")?,
            name: s("name").unwrap_or_default(),
            src: s("src")?,
            dst: s("dst")?,
            method,
            mode: s("mode").unwrap_or_else(|| {
                if method == LinkMethod::Copy { "copy" } else { "link" }.to_string()
            }),
            owner: s("owner").unwrap_or_else(|| "library".to_string()),
            created_at: s("createdAt").unwrap_or_else(|| now.clone()),
            last_sync_at: s("lastSyncAt").unwrap_or(now),
        })
    }
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::model::iso_from_millis(ms)
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ak-library-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn default_root_is_dot_agents_under_the_home() {
        let home = r"C:\Users\me";
        let lib = Library::resolve(home, &json!({}));
        assert_eq!(store::native_str(lib.root()), r"C:\Users\me\.agents");
        // 未设置与显式空串必须一致：前端把「没填」存成空串。
        let lib2 = Library::resolve(home, &json!({ "libraryRoot": "" }));
        assert_eq!(lib.root(), lib2.root());
        assert_eq!(
            store::native_str(&lib.skill_dir("pdf")),
            r"C:\Users\me\.agents\skills\pdf"
        );
        assert_eq!(
            store::native_str(&lib.mcp_record_path("fetch")),
            r"C:\Users\me\.agents\mcp\servers\fetch.json"
        );
    }

    #[test]
    fn a_configured_root_wins_and_expands_a_tilde() {
        let home = r"C:\Users\me";
        let lib = Library::resolve(home, &json!({ "libraryRoot": r"~\keeper" }));
        assert_eq!(store::native_str(lib.root()), r"C:\Users\me\keeper");
        let abs = Library::resolve(home, &json!({ "libraryRoot": r"D:\shared\agents" }));
        assert_eq!(store::native_str(abs.root()), r"D:\shared\agents");
    }

    #[test]
    fn resolve_does_not_create_anything() {
        let home = temp("resolve");
        let lib = Library::resolve(&store::native_str(&home), &json!({}));
        assert!(!lib.root().exists(), "resolving the root must not create it");
        store::rmrf(&home);
    }

    #[test]
    fn a_missing_ledger_is_empty_but_a_corrupt_one_is_an_error() {
        let root = temp("ledger");
        let lib = Library::at(&root);
        assert_eq!(lib.load_ledger().expect("absent ledger is empty"), Ledger::default());

        // 这是链接内核的一条验收：写个 `{` 进去，必须 Err，而且不碰磁盘。
        std::fs::write(lib.ledger_path(), "{").expect("seed garbage");
        let err = lib.load_ledger().expect_err("corrupt ledger must be an error");
        assert!(err.contains("not valid JSON"), "{err}");
        assert_eq!(
            std::fs::read_to_string(lib.ledger_path()).expect("read back"),
            "{",
            "a refused load must not rewrite the file"
        );

        // 结构坏了同样算损坏，不能当成空。
        std::fs::write(lib.ledger_path(), r#"{"schema":1,"entries":{}}"#).expect("seed shape");
        assert!(lib.load_ledger().is_err(), "a non-array entries field is corruption too");

        // 条目缺字段也算损坏。
        std::fs::write(lib.ledger_path(), r#"{"schema":1,"entries":[{"kind":"skill"}]}"#).expect("seed entry");
        assert!(lib.load_ledger().is_err(), "an entry without src/dst is corruption too");
        store::rmrf(&root);
    }

    #[test]
    fn an_unknown_method_is_treated_as_corruption() {
        let root = temp("method");
        let lib = Library::at(&root);
        std::fs::write(
            lib.ledger_path(),
            r#"{"schema":1,"entries":[{"kind":"skill","name":"pdf","src":"C:\\l","dst":"C:\\d","method":"teleport"}]}"#,
        )
        .expect("seed");
        assert!(lib.load_ledger().is_err(), "guessing a method would make doctor repair the wrong way");
        store::rmrf(&root);
    }

    /// 两条**指向同一目标**的链接是台账里的两条，不是一条。
    ///
    /// 这是 Skills 收敛时踩到的问题：`normalize` 曾经用 `canonicalize` 归一，于是每条
    /// agent 侧链接的键都变成库内主副本的路径，五条链接被 `upsert` 折叠成一条。
    #[test]
    fn two_links_to_one_target_stay_two_entries() {
        let root = temp("two-links");
        let lib = Library::at(&root);
        let keeper = root.join("skills").join("pdf");
        std::fs::create_dir_all(&keeper).expect("keeper");
        std::fs::write(keeper.join("SKILL.md"), "x").expect("file");
        let mut l = lib.load_ledger().expect("empty");
        for agent in ["a", "b"] {
            let dst = root.join(agent).join("skills").join("pdf");
            std::fs::create_dir_all(dst.parent().unwrap()).expect("dir");
            crate::linkfs::create_dir_link(&keeper, &dst, None).expect("link");
            lib.record(
                &mut l,
                LinkEntry::new(
                    "skill",
                    "pdf",
                    &store::native_str(&keeper),
                    &store::native_str(&dst),
                    LinkMethod::Symlink,
                ),
            );
        }
        assert_eq!(l.entries.len(), 2, "links are identified by their own path");
        assert_eq!(l.links_to(&store::native_str(&keeper)).len(), 2);
        lib.save_ledger(&l).expect("save");
        assert_eq!(lib.load_ledger().expect("reload").entries.len(), 2);
        store::rmrf(&root);
    }

    #[test]
    fn ledger_round_trips_and_dedupes_by_dst() {
        let root = temp("roundtrip");
        let lib = Library::at(&root);
        let mut l = lib.load_ledger().expect("empty");
        lib.record(&mut l, LinkEntry::new("skill", "pdf", r"C:\lib\skills\pdf", r"C:\a\skills\pdf", LinkMethod::Junction));
        lib.record(&mut l, LinkEntry::new("skill", "pdf", r"C:\lib\skills\pdf", r"C:\b\skills\pdf", LinkMethod::Symlink));
        assert_eq!(l.entries.len(), 2);
        // 同一个 dst 重新登记 = 更新，不新增。
        lib.record(&mut l, LinkEntry::new("skill", "pdf", r"C:\lib\skills\pdf", r"C:\a\skills\pdf", LinkMethod::Hardlink));
        assert_eq!(l.entries.len(), 2);
        assert_eq!(l.find(r"C:\a\skills\pdf").map(|e| e.method), Some(LinkMethod::Hardlink));
        assert_eq!(l.links_to(r"C:\lib\skills\pdf").len(), 2);

        lib.save_ledger(&l).expect("save");
        let back = lib.load_ledger().expect("reload");
        assert_eq!(back.entries.len(), 2);
        let e = back.find(r"C:\b\skills\pdf").expect("entry");
        assert_eq!(e.method, LinkMethod::Symlink);
        assert_eq!(e.mode, "link");
        assert_eq!(e.owner, "library");
        assert!(!e.created_at.is_empty());
        store::rmrf(&root);
    }

    #[test]
    fn migrating_progress_survives_a_reload() {
        let root = temp("migrating");
        let lib = Library::at(&root);
        let mut l = Ledger::default();
        l.push_raw(LinkEntry::new(
            "skill",
            "pdf",
            r"C:\old\skills\pdf",
            r"C:\a\skills\pdf",
            LinkMethod::Junction,
        ));
        l.migrating = Some(Migrating {
            from: r"C:\old".to_string(),
            to: r"C:\new".to_string(),
            done: vec![r"C:\a\skills\pdf".to_string()],
            total: 1,
            relinked: Vec::new(),
        });
        lib.save_ledger(&l).expect("save");
        let back = lib.load_ledger().expect("reload");
        let m = back.migrating.expect("migrating marker");
        assert_eq!(m.to, r"C:\new");
        assert_eq!(m.done, vec![r"C:\a\skills\pdf".to_string()]);
        store::rmrf(&root);
    }

    #[test]
    fn owns_target_covers_the_library_and_nothing_outside_it() {
        let root = temp("owns");
        let lib = Library::at(root.join("lib"));
        std::fs::create_dir_all(lib.skill_dir("pdf")).expect("skill dir");
        std::fs::create_dir_all(root.join("outside")).expect("outside");
        assert!(lib.owns_target(&lib.skill_dir("pdf")));
        assert!(lib.owns_skill_dir(&lib.skill_dir("pdf")));
        assert!(!lib.owns_skill_dir(lib.root()));
        assert!(!lib.owns_target(&root.join("outside")));
        // 指向库外的链接不算自己的。
        let outside = root.join("outside");
        let link = root.join("foreign-link");
        if linkfs::create_dir_link(&outside, &link, None).is_ok() {
            assert!(!lib.owns_target(&link));
        }
        store::rmrf(&root);
    }

    #[test]
    fn capability_probe_lands_under_the_store_not_the_library() {
        let root = temp("cap");
        let lib = Library::at(root.join("lib"));
        let store_root = root.join("store");
        std::fs::create_dir_all(&store_root).expect("store");
        let cap = lib.capability(&store::native_str(&store_root));
        assert!(cap.dir_order.iter().any(|m| m.is_link()), "no dir link method: {:?}", cap.dir_order);
        assert!(!lib.root().exists(), "the probe must not touch the library");
        store::rmrf(&root);
    }
}
