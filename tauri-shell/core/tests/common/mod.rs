//! 集成测试共用的脚手架。
//!
//! 整套用例依赖两条规矩：
//!
//! * 每个用例都在 `std::env::temp_dir()` 下拥有一份自己的临时根目录（pid + 纳秒，
//!   所以两次并发运行不会撞车），并用 `core::store::rmrf` 把它删掉。
//!   `std::fs::remove_dir_all` 在这台机器上不能用 ——
//!   它的回收站层遇到不是自己创建的目录树就会失败。
//! * `core::registry` 和 `core::assets` 是进程级单例（它们
//!   代替了注册表实例和最近一次扫描的结果）。
//!   `cargo test` 在线程池上跑测试函数，所以任何要绑定或
//!   播种它们的用例都必须先拿到 [`serial`]。

#![allow(dead_code)]

use agentkeeper_core::store;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// 给那些会碰到进程级 registry / asset 缓存的用例加锁，让它们串行执行。
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一个全新的空目录，归本用例所有，事后由它负责删除。
pub fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("ak-it-{}-{}-{}", tag, std::process::id(), nanos));
    std::fs::create_dir_all(&dir).expect("temp root");
    dir
}

/// 递归强制删除 —— 走 store 提供的辅助函数。
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

/// 每个文件的相对路径 + 内容，这样「逐字节还原」在一次比较里
/// 就能验完。
pub fn dir_snapshot(dir: &str) -> String {
    let mut files = list_all(Path::new(dir), String::new());
    files.sort();
    files
        .iter()
        .map(|rel| {
            let full =
                Path::new(dir).join(rel.replace('/', &std::path::MAIN_SEPARATOR.to_string()));
            format!(
                "{}#{}",
                rel,
                std::fs::read_to_string(&full).unwrap_or_default()
            )
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

/// 用本机原生分隔符拼接，好让结果与每个
/// 持久化的路径字段长出来的样子一致。
pub fn join(base: &str, parts: &[&str]) -> String {
    let mut p = PathBuf::from(base);
    for part in parts {
        p.push(part);
    }
    store::native_str(&p)
}
