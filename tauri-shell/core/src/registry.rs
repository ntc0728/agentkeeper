//! `projects.json` 注册表 —— 加载 `<userData>/agentkeeper/projects.json`，
//! 把同一个实例交给每一个 IPC handler，并在首次运行时做种子。
//!
//! 每次应用运行创建一个注册表，由 [`ensure_loaded`] 惰性填充，所以每个 handler
//! 看到的是同一个 `OnceLock<Mutex<..>>`。
//!
//! ## 为什么每一个字节都要在意
//!
//! 只有一个 store 目录：`load()` 读文件，`save()` 重写它，所以任何格式差异
//! （键顺序、缩进、一个结尾换行）都会在审计轨迹里表现为无谓的变动、在什么都没
//! 改动时表现为一份没必要的差异。因此 [`render`] 保持一份固定的字节布局：两个
//! 空格的缩进、对象键顺序被保留、**没有结尾换行**。
//!
//! ## 刻意保留的怪癖
//!
//! * 重复检测跑在存在性检查**之前**，并且比较两侧的 `dir.to_lowercase()`，所以
//!   重新添加一个目录已被删除的项目会报告 `project already registered`，
//!   而不是 `directory not found`。
//! * `dir` 是按字面解析的 —— 从不被 canonicalise。刻意不用 `fs::canonicalize`：
//!   它会解析符号链接，并在 Windows 上产出带 `\\?\` 前缀的逐字路径，那会改变
//!   存储的字符串、id 哈希以及 `p.dir` 的每一个消费者。
//! * `id` 是 `proj-` 加上 `name || basename(abs)` 的 slug（截到 32 个 UTF-16
//!   码元）加上 `-` 加上哈希的前 6 个字符；这个上限基于 UTF-16，所以改用
//!   `model::js_slice` 来切。
//! * `load()` 吞掉每一个错误并退回 `[]`。
//! * `remove()` 即使 id 没匹配上也会保存。
//! * `seed()` 按目录列表顺序迭代 —— 操作系统顺序，从不排序 —— 并且对一个
//!   缺失/不可读的根目录返回两个空数组。
//!
//! ## 演示根目录
//!
//! [`seed`] 把演示根目录作为 `root_dir` 接收；`lib.rs` 负责解析它（开发布局、
//! `AK_DEMO_PROJECTS`，或打包的资源目录）。把路径解析挡在 `core/` 之外，正是
//! 让无头的 `ak-scan` 二进制能复用本模块的原因。

use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::model::{hash16, iso_from_millis, js_slice, norm_str, slug};
use super::settings;

/* ------------------------------------------------------------------ */
/* 单例状态                                                             */
/* ------------------------------------------------------------------ */

/// `ProjectRegistry`：一条文件路径加上从它加载出来的那个数组。`file` 在第一次
/// 绑定之前是空的，这就是"还没加载"与"从一个空 store 加载出来"的区分方式。
struct Registry {
    file: PathBuf,
    projects: Vec<Value>,
}

fn cell() -> &'static Mutex<Registry> {
    static CELL: OnceLock<Mutex<Registry>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(Registry {
            file: PathBuf::new(),
            projects: Vec::new(),
        })
    })
}

/// 一把被毒化的锁意味着另一个线程在改动中途 panic 了；存储的数组在结构上仍然
/// 有效，所以继续用它，而不是把之后每一次项目调用都变成 panic。
fn state() -> MutexGuard<'static, Registry> {
    cell().lock().unwrap_or_else(|e| e.into_inner())
}

fn projects_file(store_root: &str) -> PathBuf {
    Path::new(store_root).join("projects.json")
}

fn load_from(file: &Path) -> Vec<Value> {
    // 每一种失败 —— 文件缺失、JSON 无效、一个
    // 不是数组的 `projects` 值 —— 都塌缩成一个空列表。绝不会
    // 向渲染层暴露任何错误。
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|doc| doc.get("projects").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn bind(r: &mut Registry, store_root: &str) {
    r.file = projects_file(store_root);
    r.projects = load_from(&r.file);
}

/// 每个进程只加载一次注册表，在首次使用时。
///
/// 文件在启动时被读取、之后从不重读，
/// 所以两次调用不会与期间发生的内存中 `add()` 产生分歧。带着另一个根目录的
/// 第二次调用被刻意忽略 —— 在生产环境里只有一个 store 根目录；测试用 [`reload`]。
pub fn ensure_loaded(store_root: &str) {
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        bind(&mut r, store_root);
    }
}

/// 丢掉内存中的列表并重新读取 `projects.json`。
///
/// 不是死代码：它是集成测试套件用来把进程级单例重新指向另一个 store 根目录的
/// 测试钩子，也是下面那些内联测试所用的钩子。GUI 本身从不调用它 —— 生产环境
/// 恰好读一次文件，通过 [`ensure_loaded`]。
pub fn reload(store_root: &str) {
    let mut r = state();
    bind(&mut r, store_root);
}

/* ------------------------------------------------------------------ */
/* 路径辅助函数                                                          */
/* ------------------------------------------------------------------ */

/// 纯字面的路径解析，不碰文件系统。
///
/// 先相对当前工作目录转成绝对路径，然后纯粹按文本折叠 `.` / `..`。
/// 既不涉及 `canonicalize` 也不涉及 `metadata`，所以一条并不存在的路径仍然能
/// 被解析（调用方的存在性检查是另一步），而且符号链接永远不会被跟随到同一条
/// 路径的另一种写法上。
fn resolve_path(dir: &str) -> String {
    let raw = Path::new(dir);
    let joined: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else if raw.has_root() {
        // Windows：`/x` 和 `\x` 是 rooted 在*当前驱动器*上的。
        let mut s = String::new();
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(Component::Prefix(p)) = cwd.components().next() {
                s.push_str(&p.as_os_str().to_string_lossy());
            }
        }
        s.push_str(dir);
        PathBuf::from(s)
    } else {
        std::env::current_dir().unwrap_or_default().join(raw)
    };

    let mut prefix: Option<String> = None;
    let mut rooted = false;
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() && !rooted {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
        }
    }
    // `C:foo`（驱动器相对）无法在缺少操作系统保存在环境里的逐驱动器工作目录
    // 时被解析。驱动器根是最接近真相的答案，也让存储的值保持绝对。
    if !rooted && prefix.as_deref().map(|p| p.contains(':')).unwrap_or(false) {
        rooted = true;
    }

    let sep = if cfg!(windows) { '\\' } else { '/' };
    let mut out = String::new();
    if let Some(p) = prefix {
        out.push_str(&p);
    }
    if rooted {
        out.push(sep);
    }
    let mut first = true;
    for part in parts {
        if !first {
            out.push(sep);
        }
        first = false;
        out.push_str(&part);
    }
    if out.is_empty() {
        // 只有在工作目录不可读且 `dir` 为空时才可能走到这里。
        out.push('.');
    }
    out
}

/// basename，保留根目录的行为。
///
/// `model::basename` 在没有最后一个分量时会退回整条路径；驱动器根返回 `''`。
/// 注册表把这个值同时喂给 `name` 和 id 的 slug，所以这个区分是真实存在的。
fn js_basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 目录检查 —— 跟随链接，吞掉错误。
fn exists_dir(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_millis(ms)
}

/* ------------------------------------------------------------------ */
/* 保存 / 加载                                                           */
/* ------------------------------------------------------------------ */

/// 两个空格的缩进，没有结尾换行。
///
/// `serde_json` 是用 `preserve_order` 构建的，所以顶层包装和加载出来的项目对象
/// 都保持存储时的确切键顺序。
pub fn render(projects: &[Value]) -> String {
    serde_json::to_string_pretty(&json!({ "projects": projects }))
        .unwrap_or_else(|_| "{\"projects\":[]}".to_string())
}

/// `save()` —— 建目录、写临时文件、重命名。它共用
/// `settings::atomic_write`，所以两个 store 有同样的持久化说法；
/// 只有字节不同，而那些字节由 `render` 钉住。
fn save(r: &Registry) -> std::io::Result<()> {
    settings::atomic_write(&r.file, &render(&r.projects))
}

/* ------------------------------------------------------------------ */
/* 公开的注册表 API                                                       */
/* ------------------------------------------------------------------ */

/// `list()` —— 一份拷贝，所以调用方无法改动注册表的数组。
pub fn list() -> Vec<Value> {
    state().projects.clone()
}

/// `get(id)` —— 字符串上的同一性比较，没有匹配时为 `null`。
pub fn get(id: &str) -> Option<Value> {
    state()
        .projects
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

/// `add(dir, name)` —— 完整流程。
pub fn add(dir: &str, name: Option<&str>) -> Value {
    let abs = resolve_path(dir);
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        // 只有调用方跳过了 `ensure_loaded` 才可能发生；拒绝比
        // 悄悄在进程的工作目录里创建 `projects.json` 要好。
        return json!({ "ok": false, "error": "registry not initialised" });
    }

    // 大小写不敏感，而且刻意跑在存在性检查之前。
    let needle = abs.to_lowercase();
    let duplicate = r.projects.iter().any(|p| {
        p.get("dir")
            .and_then(Value::as_str)
            .map(|d| d.to_lowercase() == needle)
            .unwrap_or(false)
    });
    if duplicate {
        return json!({ "ok": false, "error": "project already registered" });
    }

    if !exists_dir(&abs) {
        return json!({ "ok": false, "error": format!("directory not found: {}", abs) });
    }

    // `name || path.basename(abs)` —— 缺失*或为空*的 name 是 falsy。
    let base = js_basename(&abs);
    let chosen: &str = match name {
        Some(n) if !n.is_empty() => n,
        _ => &base,
    };
    let id = format!(
        "proj-{}-{}",
        js_slice(&slug(chosen), 32),
        &hash16(&abs)[..6]
    );
    let project = json!({
        "id": id,
        "name": chosen,
        "dir": abs,
        "addedAt": now_iso(),
        "tags": []
    });

    r.projects.push(project.clone());
    // 即使写入失败，这次 push 也会保留：失败发生在
    // 数组已经被扩展之后。
    if let Err(e) = save(&r) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    json!({ "ok": true, "project": project })
}

/// `remove(id)` —— 总是保存，即使什么都没匹配上。
pub fn remove(id: &str) -> Value {
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        return json!({ "ok": false, "error": "registry not initialised" });
    }
    let before = r.projects.len();
    r.projects
        .retain(|p| p.get("id").and_then(Value::as_str) != Some(id));
    let after = r.projects.len();
    if let Err(e) = save(&r) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    json!({ "ok": after != before, "removed": before - after })
}

/// 注册 `root_dir` 下的每一个项目。
///
/// 非目录和以点开头的名字在尝试 `add` 之前就被跳过；
/// 其余一切都走正常的重复/存在性检查，所以第二次运行会把每个项目都报告为
/// `already registered`（已注册）。
pub fn seed(store_root: &str, root_dir: &str) -> Value {
    ensure_loaded(store_root);
    let mut added: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();

    if root_dir.is_empty() {
        return json!({ "added": added, "skipped": skipped });
    }
    let root = Path::new(root_dir);
    if !root.exists() {
        return json!({ "added": added, "skipped": skipped });
    }
    // 这里是一个文件（或者一个权限错误）会产出空数组，而不是错误。
    let Ok(entries) = std::fs::read_dir(root) else {
        return json!({ "added": added, "skipped": skipped });
    };

    // `readdirSync` 的顺序就是操作系统顺序；排序会改变哪个项目先被
    // 添加，从而改变 projects.json 内部的顺序。
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // 对 junction/符号链接而言 Dirent::isDirectory() 为 false，这正是
        // 链接过来的演示项目被跳过的原因。
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir || name.starts_with('.') {
            continue;
        }
        let dir = norm_str(&root.join(&name));
        let r = add(&dir, Some(&name));
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            added.push(r.get("project").cloned().unwrap_or(Value::Null));
        } else {
            skipped.push(json!({
                "dir": dir,
                "reason": r.get("error").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    json!({ "added": added, "skipped": skipped })
}

/* ------------------------------------------------------------------ */
/* 测试                                                                 */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ak-registry-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// 一个固定项目下、字面的两空格缩进形态。
    #[test]
    fn render_uses_the_two_space_indent_shape() {
        let projects = vec![json!({
            "id": "proj-demo-a1b2c3",
            "name": "demo",
            "dir": "C:\\work\\demo",
            "addedAt": "2026-10-19T13:22:24.297Z",
            "tags": []
        })];
        let expected = "{\n  \"projects\": [\n    {\n      \"id\": \"proj-demo-a1b2c3\",\n      \"name\": \"demo\",\n      \"dir\": \"C:\\\\work\\\\demo\",\n      \"addedAt\": \"2026-10-19T13:22:24.297Z\",\n      \"tags\": []\n    }\n  ]\n}";
        assert_eq!(render(&projects), expected);
        assert!(
            !render(&projects).ends_with('\n'),
            "JSON.stringify adds no newline"
        );
        assert_eq!(render(&[]), "{\n  \"projects\": []\n}");
    }

    #[test]
    fn resolve_is_lexical_and_never_verbatim() {
        let abs = resolve_path("C:/work/./agentkeeper/../agentkeeper/src");
        assert!(
            !abs.starts_with(r"\\?\"),
            "must never produce a verbatim path: {}",
            abs
        );
        assert!(!abs.contains(".."));
        assert!(!abs.contains("/."));
        if cfg!(windows) {
            assert_eq!(abs, r"C:\work\agentkeeper\src");
            // 不存在的路径照样能解析；这里完全不碰文件系统。
            assert_eq!(resolve_path(r"C:\nope\nope\..\x"), r"C:\nope\x");
            // 结尾的分隔符会被丢掉，和 Node 的 path.resolve 一样。
            assert_eq!(resolve_path("C:/work/agentkeeper/"), r"C:\work\agentkeeper");
            // 期望值是对着本机词法解析的结果核出来的。
            for (input, want) in [
                (r"C:\work\demo\..\demo\.\sub", r"C:\work\demo\sub"),
                (r"C:\work\demo\", r"C:\work\demo"),
                (r"C:\", r"C:\"),
                (r"\\server\share\a\..\b", r"\\server\share\b"),
            ] {
                assert_eq!(resolve_path(input), want, "resolve {:?}", input);
            }
            // 对盘符根目录做 `path.basename` 得到的是 ''，而不是路径本身。
            assert_eq!(js_basename(r"C:\"), "");
            assert_eq!(js_basename(r"C:\a\b\"), "b");
        }
    }

    /// 所有碰进程级单例的用例都放在这一个测试里：cargo 并行跑各个测试函数，
    /// 两个测试把同一个 `OnceLock` 指向不同的临时根目录就会互相踩。
    #[test]
    fn registry_round_trip_load_add_duplicate_remove_seed() {
        let root = temp_root("roundtrip");
        let store = Path::new(&root).join("store");
        std::fs::create_dir_all(&store).unwrap();
        let store_s = store.to_string_lossy().into_owned();
        let file = store.join("projects.json");

        let demo = store.join("work").join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        let demo_s = demo.to_string_lossy().into_owned();

        // projects.json 缺失时加载成一份空注册表。
        reload(&store_s);
        assert!(list().is_empty());

        // --- 添加 -----------------------------------------------------
        let a = add(&demo_s, None);
        assert_eq!(a["ok"], json!(true), "add must succeed: {}", a);
        let p = a["project"].clone();
        let expected_dir = resolve_path(&demo_s);
        assert_eq!(p["dir"], json!(expected_dir));
        assert_eq!(p["name"], json!("demo"), "basename fallback");
        assert_eq!(p["tags"], json!([]));
        assert_eq!(
            p["id"],
            json!(format!("proj-demo-{}", &hash16(&expected_dir)[..6]))
        );
        let added_at = p["addedAt"].as_str().unwrap().to_string();
        assert!(
            added_at.ends_with('Z') && added_at.len() == 24 && &added_at[19..20] == ".",
            "ISO with millisecond precision: {}",
            added_at
        );
        assert_eq!(list().len(), 1);
        assert_eq!(get(p["id"].as_str().unwrap()).unwrap()["id"], p["id"]);

        // --- 文件字节完全一致 -----------------------------------------
        let body = std::fs::read_to_string(&file).unwrap();
        let q = |v: &Value| serde_json::to_string(v).unwrap();
        let expected = format!(
            "{{\n  \"projects\": [\n    {{\n      \"id\": {},\n      \"name\": {},\n      \"dir\": {},\n      \"addedAt\": {},\n      \"tags\": []\n    }}\n  ]\n}}",
            q(&p["id"]),
            q(&p["name"]),
            q(&p["dir"]),
            q(&p["addedAt"])
        );
        assert_eq!(body, expected);
        assert!(!body.ends_with('\n'), "save() writes no trailing newline");
        assert_eq!(body, render(&list()), "the file is render(list())");

        // --- 重复添加，大小写不敏感 -----------------------------------
        let mixed = demo_s.replace("demo", "DEMO");
        let dup = add(&mixed, Some("other"));
        assert_eq!(dup["ok"], json!(false));
        assert_eq!(dup["error"], json!("project already registered"));
        assert_eq!(list().len(), 1, "a rejected add must not mutate state");

        // --- 重复添加一个已缺失的目录，报重复 -------------------------
        let gone = store.join("work").join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let gone_s = gone.to_string_lossy().into_owned();
        assert_eq!(add(&gone_s, None)["ok"], json!(true));
        std::fs::remove_dir_all(&gone).unwrap();
        let again = add(&gone_s, None);
        assert_eq!(
            again["error"],
            json!("project already registered"),
            "the duplicate check precedes the existence check"
        );

        // --- 添加一个确实不存在的目录 ---------------------------------
        let missing = store.join("work").join("absent");
        let missing_s = missing.to_string_lossy().into_owned();
        let m = add(&missing_s, None);
        assert_eq!(m["ok"], json!(false));
        assert_eq!(
            m["error"],
            json!(format!("directory not found: {}", resolve_path(&missing_s)))
        );

        // --- 显式指定名字添加 -----------------------------------------
        let named = store.join("work").join("folder");
        std::fs::create_dir_all(&named).unwrap();
        let named_s = named.to_string_lossy().into_owned();
        let n = add(&named_s, Some("Custom Name"));
        assert_eq!(n["project"]["name"], json!("Custom Name"));
        assert_eq!(
            n["project"]["id"],
            json!(format!(
                "proj-custom-name-{}",
                &hash16(&resolve_path(&named_s))[..6]
            ))
        );
        // 空名字是假值，会退回用目录名。
        let empty = add(&named_s, Some(""));
        assert_eq!(empty["ok"], json!(false), "same dir, so duplicate");
        assert_eq!(list().len(), 3);

        // --- 删除 -----------------------------------------------------
        let id = p["id"].as_str().unwrap().to_string();
        let rm = remove(&id);
        assert_eq!(rm, json!({ "ok": true, "removed": 1 }));
        assert!(get(&id).is_none());
        assert_eq!(list().len(), 2);

        // Removing an unknown id still writes the file (解析口径).
        let before = std::fs::read_to_string(&file).unwrap();
        let rm2 = remove("proj-nope-000000");
        assert_eq!(rm2, json!({ "ok": false, "removed": 0 }));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert_eq!(before, render(&list()));

        // --- 做种 -----------------------------------------------------
        let seed_root = store.join("demo-projects");
        for name in ["alpha", "beta", ".hidden"] {
            std::fs::create_dir_all(seed_root.join(name)).unwrap();
        }
        std::fs::write(seed_root.join("note.txt"), "not a project").unwrap();
        let seed_s = seed_root.to_string_lossy().into_owned();

        let s = seed(&store_s, &seed_s);
        let added = s["added"].as_array().unwrap();
        let mut names: Vec<String> = added
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(
            s["skipped"],
            json!([]),
            "dot dirs and files are skipped, not reported"
        );
        assert_eq!(list().len(), 4);

        let s2 = seed(&store_s, &seed_s);
        assert_eq!(s2["added"], json!([]));
        assert_eq!(s2["skipped"].as_array().unwrap().len(), 2);
        assert_eq!(
            s2["skipped"][0]["reason"],
            json!("project already registered")
        );
        assert_eq!(
            s2["skipped"][0]["dir"],
            json!(norm_str(&seed_root.join("alpha")))
        );

        // 根目录缺失、根目录参数为空、以及根目录其实是个文件：都只返回两个空
        // 数组，从不报错。
        assert_eq!(
            seed(&store_s, &store.join("nope").to_string_lossy()),
            json!({ "added": [], "skipped": [] })
        );
        assert_eq!(seed(&store_s, ""), json!({ "added": [], "skipped": [] }));
        assert_eq!(
            seed(&store_s, &seed_root.join("note.txt").to_string_lossy()),
            json!({ "added": [], "skipped": [] })
        );

        // --- 重新加载会把文件重新读回来 -------------------------------
        reload(&store_s);
        assert_eq!(list().len(), 4);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), render(&list()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn load_swallows_bad_json_and_non_array_projects() {
        let root = temp_root("load");
        let file = Path::new(&root).join("projects.json");

        std::fs::write(&file, "{not json").unwrap();
        assert!(load_from(&file).is_empty());

        std::fs::write(&file, r#"{"projects":{}}"#).unwrap();
        assert!(load_from(&file).is_empty());

        std::fs::write(&file, r#"{"projects":[]}"#).unwrap();
        assert!(load_from(&file).is_empty());

        // 任意多余字段都会原封不动地活过一次加载。
        std::fs::write(
            &file,
            r#"{"projects":[{"id":"x","dir":"C:\\a","extra":1}]}"#,
        )
        .unwrap();
        let loaded = load_from(&file);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["extra"], json!(1));

        assert!(load_from(&Path::new(&root).join("absent.json")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
