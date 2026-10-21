//! Asset lookup + detail read — the read half of `Keeper` (`core/keeper.js`)
//! that the drawer depends on.
//!
//! ## Why a cache, and what it costs
//!
//! `Keeper` keeps `lastScan` in memory and `readAsset()` looks up
//! `lastScan.effective` by id. The Rust side has no such object between calls,
//! so a process-global cache is populated by the scan command. A miss (first
//! call after launch) triggers one scan — the same fallback as
//! `get assetIndex() { if (!this.lastScan) this.scan(); }`.
//!
//! The cache stores the **payload** form of each asset, where `raw` is null and
//! `effective` is reduced to `{live, shadowedBy, overrides}`. That keeps the
//! cached copy small: the unshrunk form holds the full text of every skill and
//! MCP config (hundreds of MB on a large library), which is exactly the memory
//! the JS build spends and this port avoids. `read()` therefore re-reads the
//! single file it needs and re-attaches it as `raw`.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::model::js_string;
use super::scan;
use super::security::check_command;

/// Insertion order is kept alongside the index: `setBaseline()` writes a JSON
/// object in asset order, and a HashMap would reshuffle it (visible as a
/// needless diff against the file the Electron build produced).
#[derive(Default)]
struct Cache {
    order: Vec<String>,
    by_id: HashMap<String, Value>,
    /// `(id, fingerprint)` over **all** assets, not the effective list:
    /// `ak:baseline-diff` and `ak:set-baseline` both run on `scan.assets`, and
    /// the two lists differ once a project-level asset shadows a global one.
    asset_fps: Vec<(String, String)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

/// Replace the cache with the arrays of a fresh scan payload.
///
/// Called from `scan_payload`, so both the Tauri command and `ak-scan` keep the
/// cache in step with what the renderer just received.
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

/// `Keeper.assetIndex.get(id)` — the cached payload form of one asset.
///
/// Used by the write path (`keeper.rs`) to resolve an id to its file, dir and
/// `meta` before deciding what to mutate. `None` means "not in the last scan",
/// which the caller must report as `asset not found` — never guess a path.
pub fn get(id: &str) -> Option<Value> {
    lookup(id)
}

/// `Keeper._invalidate()` — drop the cached scan after a mutation.
///
/// The next `read_asset` then re-scans instead of serving the content the write
/// just replaced. Kept in the core layer so every caller (IPC, MCP server, CLI)
/// inherits it; relying on each handler to remember meant a write followed by a
/// read served stale text from the old index.
pub fn invalidate() {
    let Ok(mut c) = cache().lock() else { return };
    c.order.clear();
    c.by_id.clear();
    c.asset_fps.clear();
}

pub fn cached_count() -> usize {
    cache().lock().map(|c| c.by_id.len()).unwrap_or(0)
}

/// `(id, fingerprint)` over all assets, in scan order — the exact input
/// `Store.baselineDiff()` / `setBaseline()` take.
pub fn fingerprints() -> Vec<(String, String)> {
    cache().lock().map(|c| c.asset_fps.clone()).unwrap_or_default()
}

/// `readAll()` — via the shared store helper, so the long-path handling (and
/// the `''` on a missing file) is the same one the write path uses.
fn read_all(path: &str) -> String {
    super::store::read_text(std::path::Path::new(path))
}

/// `Keeper.readAsset(id)`.
///
/// Returns `{ ok, asset, content }` on success and `{ ok: false, error }` when
/// the id is unknown — the renderer toasts `error`, so the message is
/// user-visible and mirrors the JS wording.
pub fn read_asset(user_home: &str, store_root: &str, id: &str) -> Value {
    let mut asset = lookup(id);

    if asset.is_none() {
        // Cold cache: scan once. Deliberately the UNCACHED variant — this
        // fallback answers one lookup, and publishing its result replaced the
        // index every other caller (and every other test) was holding, which
        // made results depend on interleaving rather than on the code.
        let projects = scan::read_projects(store_root);
        let (payload, _) = scan::scan_payload_uncached(user_home, &projects);
        if let Some(list) = payload.get("effective").and_then(|v| v.as_array()) {
            asset = list
                .iter()
                .find(|a| a.get("id").map(js_string).as_deref() == Some(id))
                .cloned();
        }
        // A concurrent scan may have published while this one ran.
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
        // skill / rule / memory / mcp: read straight through. A missing file
        // yields "", matching readAll()'s try/catch.
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
        // Plugins never carried `raw` in the JS scanner, and `readAsset` does not
        // pass through `stripHeavy` (which is what turns an absent `raw` into
        // `null` in the scan payload). So the key must be *removed*, not nulled,
        // or the drawer receives a shape the JS never produces.
        if kind == "plugin" {
            m.remove("raw");
        } else {
            m.insert("raw".to_string(), Value::String(content.clone()));
        }
        if kind == "mcp" {
            // MCP liveness: does the configured command exist? Filesystem only —
            // never spawns an unknown binary.
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

/// Serialises tests that mutate the process-global cache.
///
/// The cache is global by design (it stands in for `Keeper.lastScan`), so two
/// test functions that call [`remember`] would race under `cargo test`'s thread
/// pool. Every test that seeds the cache takes this lock first.
#[cfg(test)]
pub fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not two: the cache is process-global and `cargo test` runs
    /// test fns in parallel, so two tests that both call `remember()` race and
    /// fail intermittently (observed once, then only under load).
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
        assert_eq!(fps[0], ("skill:a".to_string(), "f1".to_string()), "scan order is preserved");

        // Unknown id: the JS error envelope, not a panic and not an empty asset.
        let r = read_asset("C:/nohome", "C:/noprofile", "missing");
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"], json!("asset not found: missing"));
    }
}
