//! Shared scaffolding for the integration tests.
//!
//! Two rules the whole suite depends on:
//!
//! * Every test owns a temp root under `std::env::temp_dir()` (pid + nanos, so
//!   two concurrent runs cannot collide) and removes it with
//!   `core::store::rmrf`. `std::fs::remove_dir_all` is unusable on this host —
//!   its trash layer fails on trees it did not create.
//! * `core::registry` and `core::assets` are process-global singletons (they
//!   stand in for the JS `ProjectRegistry` instance and `Keeper.lastScan`).
//!   `cargo test` runs test fns on a thread pool, so any test that binds or
//!   seeds them must hold [`serial`] first.

#![allow(dead_code)]

use agentkeeper_shell_lib::core::store;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// Serialises tests that touch the process-global registry / asset cache.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fresh, empty directory that this test owns and removes afterwards.
pub fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("ak-it-{}-{}-{}", tag, std::process::id(), nanos));
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

/// `fs.rmSync(dir, {recursive:true, force:true})` — via the store's helper.
pub fn cleanup(p: &Path) {
    store::rmrf(p);
}

pub fn write(path: &str, content: &str) {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, content).unwrap_or_else(|e| panic!("write {}: {}", path, e));
}

pub fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path, e))
}

pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `readDirSnapshot()` from `scripts/selftest.js` — relative path + content for
/// every file, so "restored byte-for-byte" is checkable in one comparison.
pub fn dir_snapshot(dir: &str) -> String {
    let mut files = list_all(Path::new(dir), String::new());
    files.sort();
    files
        .iter()
        .map(|rel| {
            let full = Path::new(dir).join(rel.replace('/', &std::path::MAIN_SEPARATOR.to_string()));
            format!("{}#{}", rel, std::fs::read_to_string(&full).unwrap_or_default())
        })
        .collect::<Vec<String>>()
        .join("|")
}

fn list_all(dir: &Path, base: String) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.extend(list_all(&e.path(), rel));
        } else {
            out.push(rel);
        }
    }
    out
}

/// `path.join(base, ...parts)` with the native separator, matching what the JS
/// build stores in every path field.
pub fn join(base: &str, parts: &[&str]) -> String {
    let mut p = PathBuf::from(base);
    for part in parts {
        p.push(part);
    }
    store::native_str(&p)
}
