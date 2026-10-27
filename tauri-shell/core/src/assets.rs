//! 资产查找 + 详情读取 —— `Keeper` 的读半边，
//! 抽屉（drawer）依赖的就是它。
//!
//! ## 为什么要缓存，以及代价是什么
//!
//! 核心层在两次调用之间不保留任何 scan 对象，所以用一份进程级的全局缓存，
//! 由 scan 命令来填充。未命中时（启动后的第一次调用）就触发一次扫描。
//!
//! 缓存里存的是每个资产的 **payload** 形态：`raw` 为 null，`effective` 被压缩成
//! `{live, shadowedBy, overrides}`。这样缓存副本才能保持小：未经压缩的形态要装下
//! 每个 skill 和每份 MCP 配置的全文（大一点的库能到几百 MB），而这恰恰就是
//! 本缓存要避免占用的内存。因此 `read()` 只把自己需要的那一个文件重新读出来，
//! 再把它作为 `raw` 挂回去。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::model::js_string;
use super::scan;
use super::security::check_command;

/// 索引之外还单独保留插入顺序：`set_baseline()` 按资产顺序写一个 JSON 对象，
/// 换成一个 HashMap 就会把它重新打乱（表现为每次重写基线都多出一份毫无意义的
/// 差异）。
#[derive(Default)]
struct Cache {
    order: Vec<String>,
    by_id: HashMap<String, Value>,
    /// `(id, fingerprint)`，覆盖**全部**资产，而不是只覆盖有效列表：
    /// `ak:baseline-diff` 和 `ak:set-baseline` 都是跑在 `scan.assets` 上的，而一旦
    /// 有项目级资产遮蔽了全局资产，这两个列表就不一样了。
    asset_fps: Vec<(String, String)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// 用一份全新 scan payload 里的数组替换掉缓存。
///
/// 由 `scan_payload` 调用，所以 Tauri 命令和 `ak-scan` 都能让缓存与渲染层刚
/// 收到的东西保持同步。
pub fn remember(assets: &[Value], effective: &[Value]) {
    let Ok(mut c) = cache().lock() else { return };
    c.order.clear();
    c.by_id.clear();
    c.asset_fps.clear();

    c.order.reserve(effective.len());
    c.by_id.reserve(effective.len());
    for a in effective {
        if let Some(id) = a.get("id") {
            let id = js_string(id);
            c.order.push(id.clone());
            c.by_id.insert(id, a.clone());
        }
    }

    c.asset_fps.reserve(assets.len());
    for a in assets {
        if let Some(id) = a.get("id") {
            c.asset_fps.push((
                js_string(id),
                a.get("fingerprint").map(js_string).unwrap_or_default(),
            ));
        }
    }
}

fn lookup(id: &str) -> Option<Value> {
    cache().lock().ok()?.by_id.get(id).cloned()
}

/// 按 id 取某个资产缓存中的 payload 形态。
///
/// 写入路径（`keeper.rs`）用它把一个 id 解析成对应的文件、目录和 `meta`，然后
/// 再决定改什么。`None` 的意思是「不在上一次扫描的结果里」，调用方必须把它报成
/// `asset not found` —— 绝不要去猜一个路径。
pub fn get(id: &str) -> Option<Value> {
    lookup(id)
}

/// 变更之后把缓存的扫描结果丢掉。
///
/// 这样下一次 `read_asset` 会重新扫描，而不是把刚被写掉的内容又端出来。把它放在
/// 核心层，是为了让每个调用方（IPC、MCP server、CLI）都自动继承这个行为；哪个
/// 处理函数忘了这一句，写入之后就会端出过期的文本。
pub fn invalidate() {
    let Ok(mut c) = cache().lock() else { return };
    c.order.clear();
    c.by_id.clear();
    c.asset_fps.clear();
}

pub fn cached_count() -> usize {
    cache().lock().map(|c| c.by_id.len()).unwrap_or(0)
}

/// 覆盖所有资产的 `(id, fingerprint)`，按扫描顺序 —— 这正是
/// `baseline_diff` / `set_baseline` 接收的输入。
pub fn fingerprints() -> Vec<(String, String)> {
    cache()
        .lock()
        .map(|c| c.asset_fps.clone())
        .unwrap_or_default()
}

/// 读整个文件 —— 走共享的 store 辅助函数，这样长路径的处理（以及文件不存在时
/// 返回 `''`）就和写入路径用的是同一套。
fn read_all(path: &str) -> String {
    super::store::read_text(std::path::Path::new(path))
}

/// 读一个资产的详情。
///
/// 成功时返回 `{ ok, asset, content }`，id 未知时返回 `{ ok: false, error }` ——
/// 渲染层会把 `error` 弹成 toast，所以这条消息是用户能看见的，措辞是定死的。
pub fn read_asset(user_home: &str, store_root: &str, id: &str) -> Value {
    let mut asset = lookup(id);

    if asset.is_none() {
        // 冷缓存：扫一次。这里刻意用不带缓存的那个变体 —— 这条兜底只回答一次
        // 查找，而把它的结果发布出去会替掉其他每一个调用方（以及其他每一个测试）
        // 手里正握着的索引，结果就是结果取决于交错顺序，而不是取决于代码。
        let projects = scan::read_projects(store_root);
        let (payload, _) = scan::scan_payload_uncached(user_home, &projects);
        if let Some(list) = payload.get("effective").and_then(|v| v.as_array()) {
            asset = list
                .iter()
                .find(|a| a.get("id").map(js_string).as_deref() == Some(id))
                .cloned();
        }
        // 并发进行的另一次扫描可能在此期间已经把结果发布了。
        if asset.is_none() {
            asset = lookup(id);
        }
    }

    let Some(mut asset) = asset else {
        return json!({ "ok": false, "error": format!("asset not found: {}", id) });
    };

    let kind = asset.get("kind").map(js_string).unwrap_or_default();
    let file = asset.get("file").map(js_string).unwrap_or_default();

    let content = match kind.as_str() {
        // skill / rule / memory / mcp：直读到底。文件不存在时得到 ""，与共享的
        // 读取辅助函数一致。
        "skill" | "rule" | "memory" | "mcp" => read_all(&file),
        "plugin" => {
            if !file.is_empty() && std::path::Path::new(&file).is_file() {
                read_all(&file)
            } else {
                "(directory asset)".to_string()
            }
        }
        _ => String::new(),
    };

    if let Value::Object(m) = &mut asset {
        // 插件不带 `raw` 字段，而这条路径不经过裁剪那一步（在 scan payload 里
        // 正是它把一个缺失的 `raw` 变成 `null` 的）。所以这个键必须*删掉*，
        // 而不是置成 null，否则抽屉会收到一个别处都不会产出的形状。
        if kind == "plugin" {
            m.remove("raw");
        } else {
            m.insert("raw".to_string(), Value::String(content.clone()));
        }
        if kind == "mcp" {
            // MCP 存活检查：配置里的命令到底存不存在？只查文件系统 ——
            // 绝不去启动一个来路不明的二进制。
            let health = m
                .get("meta")
                .map(check_command)
                .unwrap_or_else(|| json!({ "ok": false, "detail": "no meta" }));
            if let Some(Value::Object(meta)) = m.get_mut("meta") {
                meta.insert("health".to_string(), health);
            }
        }
    }

    json!({ "ok": true, "asset": asset, "content": content })
}

/// 把会改动进程级全局缓存的测试串行化。
///
/// 缓存按设计就是全局的（它代表的是上一次扫描的结果），所以在 `cargo test` 的
/// 线程池下，两个都调用 [`remember`] 的测试函数会互相竞争。每个会往缓存里灌数据
/// 的测试都必须先拿这把锁。
#[cfg(test)]
pub fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个测试，而不是拆成两个：缓存是进程级的，而 `cargo test` 是并行跑测试
    /// 函数的，所以两个都调用 `remember()` 的测试会竞争并间歇性失败（实际观察到
    /// 过一次，之后只在负载高时复现）。
    #[test]
    fn cache_round_trip_and_error_shape() {
        let _guard = lock_for_test();
        let all = vec![
            json!({ "id": "skill:a", "kind": "skill", "file": "", "fingerprint": "f1" }),
            json!({ "id": "rule:b", "kind": "rule", "file": "", "fingerprint": "f2" }),
        ];
        remember(&all, &all);
        assert_eq!(cached_count(), 2);
        assert!(lookup("skill:a").is_some());
        assert!(lookup("nope").is_none());
        let fps = fingerprints();
        assert_eq!(fps.len(), 2);
        assert_eq!(
            fps[0],
            ("skill:a".to_string(), "f1".to_string()),
            "scan order is preserved"
        );

        // 未知 id：走错误信封，既不 panic，也不返回一个空资产。
        let r = read_asset("C:/nohome", "C:/noprofile", "missing");
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"], json!("asset not found: missing"));
    }
}
