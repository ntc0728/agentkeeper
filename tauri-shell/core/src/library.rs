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

    /// `p`（或其目标）是否位于库内。
    ///
    /// `disable_asset` 的安全闸与 `link.unmanaged` 的判定共用这一个函数：一个链接
    /// 指向库外时，删掉它会动到用户自己的目录，那是本功能绝不做的事。
    pub fn owns_target(&self, p: &Path) -> bool {
        let root = normalize(&self.root);
        for candidate in [p.to_path_buf(), linkfs::read_target(p).unwrap_or_default()] {
            let s = normalize(&candidate);
            if !s.is_empty() && (s == root || s.starts_with(&format!("{}\\", root))) {
                return true;
            }
        }
        false
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

/// 比较用的小写化绝对路径（`\\?\` 前缀已去掉、末尾分隔符已抹平）。
fn normalize(p: &Path) -> String {
    let abs = match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(_) => p.to_path_buf(),
    };
    let mut s = store::native_str(&abs);
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    let s = s.trim_end_matches('\\').to_lowercase();
    s
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
}

impl Migrating {
    fn from_json(v: &Value) -> Option<Migrating> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
        let done = v
            .get("done")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|x| x.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Some(Migrating {
            from: s("from")?,
            to: s("to")?,
            done,
        })
    }
}

impl Ledger {
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
                json!({ "from": m.from, "to": m.to, "done": m.done }),
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

        // 这是 M1 验收 4：写个 `{` 进去，必须 Err，而且不碰磁盘。
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
