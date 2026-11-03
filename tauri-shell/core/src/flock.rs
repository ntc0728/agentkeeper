//! 跨进程排他锁。
//!
//! ## 为什么需要它
//!
//! 有三处「读-改-写」的元数据文件：`<library>/links.json`（台账）、
//! `<store>/settings.json`（设置）、`<store>/projects.json`（项目注册表）。
//! 桌面端与 `agentkeeper-mcp` 是两个**独立进程**，各自都可能在同一瞬间做
//! 「读出来 → 改一改 → 整份写回」。没有跨进程互斥时，后写的那一份会盖掉前一份
//! 的改动：丢一条链接台账、丢一次设置补丁、丢一个刚注册的项目。
//!
//! 文件本身不会写坏（`settings::atomic_write` 保证不出现半份），丢的是**更新**，
//! 而丢失的更新事后没人能发现 —— 正是这个项目最不能接受的那类静默失败。
//!
//! ## 为什么不用第三方 crate
//!
//! 目标平台只有 Windows，而 `windows-sys` 已经是本 crate 的依赖：`CreateFileW`
//! 用 `dwShareMode = 0` 打开即独占，第二个进程拿到
//! `ERROR_SHARING_VIOLATION`，这就是一把操作系统级的排他锁，不需要再引一个
//! 文件锁 crate（也不用担心它在网络盘上的行为差异）。
//!
//! ## 语义
//!
//! * 锁是**文件句柄级**的：进程崩溃时句柄由内核关闭，锁自动释放，不会留下
//!   需要人工清理的陈旧锁文件（这是相对「自己写 pid 文件」的关键好处）。
//! * 锁文件 `<store>/.lock` 只是句柄的载体，内容永远为空 —— 不要去读它。
//! * 拿不到锁**不是致命错误**：等 `timeout` 之后返回 `store.busy`，调用方把它
//!   翻译成用户能看懂的一句话（§15：宁可失败并给出可读的 code）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 默认等待时长。桌面端的一次写通常 < 100 ms，取 5 s 足够覆盖
/// 「另一个进程正在搬库/重建台账」这种人能感知的情况，又不至于让界面卡住。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 重试间隔。25 ms 足够小（一次写盘之外几乎不占 CPU），也足够大
/// （不会把 5 s 预算烧在空转上）。
const RETRY: Duration = Duration::from_millis(25);

/// 一次持有。`Drop` 时释放 —— 调用方只需把它绑在一个变量上。
pub struct StoreLock {
    /// `CloseHandle` 用的原始句柄；`None` 表示这台机器上锁退化成空操作
    ///（非 Windows 构建：本项目的发布范围只有 Windows，这条分支只为让
    ///  `cargo test` 在别的平台上仍能跑）。
    handle: Option<isize>,
    /// 锁文件路径；可重入的重入计数按它记账。
    path: Option<PathBuf>,
    /// 是不是「同一线程已经持有时」的嵌套获取：嵌套的那一份不关句柄。
    nested: bool,
}

// 同一线程内可重入。
//
// 为什么需要：迁移（`migrate_library`）会整段持有 store 锁，而它中途要调
// `settings::save`（改 `libraryRoot`）；如果那把锁再被同一个线程请求一次，
// Windows 的文件共享是按**句柄**算的 —— 第二次 `CreateFileW` 会撞上共享冲突，
// 于是自己的代码把自己的锁等成了死锁。按线程记重入深度是最小的解：
// 嵌套获取只递增计数，最外层释放时才真正关句柄。
thread_local! {
    static HELD: RefCell<HashMap<PathBuf, usize>> = RefCell::new(HashMap::new());
}

impl StoreLock {
    /// 取锁：`store_root/.lock` 上的独占打开，失败按 [`RETRY`] 退避重试，
    /// 超过 `timeout` 返回 `Err("store.busy")`。
    pub fn acquire(store_root: &str, timeout: Duration) -> Result<StoreLock, String> {
        if store_root.trim().is_empty() {
            // 没有 store 根就没有共享状态可保护（测试里偶发）。
            return Ok(StoreLock {
                handle: None,
                path: None,
                nested: false,
            });
        }
        let path = std::path::Path::new(store_root).join(".lock");
        // 同一线程已持有 → 只记一层，不再打开第二个句柄。
        if HELD.with(|h| h.borrow().get(&path).copied().unwrap_or(0)) > 0 {
            HELD.with(|h| *h.borrow_mut().entry(path.clone()).or_insert(0) += 1);
            return Ok(StoreLock {
                handle: None,
                path: Some(path),
                nested: true,
            });
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let started = Instant::now();
        loop {
            match try_open_exclusive(&path) {
                Ok(handle) => {
                    HELD.with(|h| h.borrow_mut().insert(path.clone(), 1));
                    return Ok(StoreLock {
                        handle: Some(handle),
                        path: Some(path),
                        nested: false,
                    });
                }
                Err(busy) if busy => {
                    if started.elapsed() >= timeout {
                        return Err("store.busy".to_string());
                    }
                    std::thread::sleep(RETRY);
                }
                // 非「被占用」的失败（权限、只读介质）：不重试，直接报出来 ——
                // 重试只会把同一句话重复 200 遍。
                Err(_) => return Err("store.lockFailed".to_string()),
            }
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if self.nested {
            // 嵌套释放：只退一层计数，句柄仍由最外层持有。
            HELD.with(|h| {
                if let Some(depth) = h.borrow_mut().get_mut(&path) {
                    *depth = depth.saturating_sub(1);
                }
            });
            return;
        }
        HELD.with(|h| {
            h.borrow_mut().remove(&path);
        });
        if let Some(h) = self.handle.take() {
            close(h);
        }
    }
}

#[cfg(windows)]
fn try_open_exclusive(p: &std::path::Path) -> Result<isize, bool> {
    // `GENERIC_READ` / `GENERIC_WRITE` 住在 Foundation 而不是 FileSystem
    //（与 junction 那个 `IO_REPARSE_TAG_MOUNT_POINT` 的坑相反方向）。
    use windows_sys::Win32::Foundation::{
        GetLastError, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_ALWAYS,
    };
    // `ERROR_SHARING_VIOLATION`：另一个进程正握着这个文件。
    const ERROR_SHARING_VIOLATION: u32 = 32;
    let wide: Vec<u16> = {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    };
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            // dwShareMode = 0：任何第二个打开都会失败 —— 这就是锁。
            0,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(err == ERROR_SHARING_VIOLATION);
    }
    Ok(h)
}

#[cfg(not(windows))]
fn try_open_exclusive(_p: &std::path::Path) -> Result<isize, bool> {
    // 非 Windows：本项目的发布范围不含它，测试里退化成「总能拿到」。
    Ok(0)
}

#[cfg(windows)]
fn close(h: isize) {
    unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
}

#[cfg(not(windows))]
fn close(_h: isize) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> String {
        let d = std::env::temp_dir().join(format!(
            "ak-flock-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        crate::store::native_str(&d)
    }

    /// 同一线程可重入（迁移里会嵌套调用 settings::save），另一条线程仍然被挡。
    #[test]
    fn same_thread_is_reentrant_while_another_thread_is_blocked() {
        let root = temp("reentrant");
        let first = StoreLock::acquire(&root, DEFAULT_TIMEOUT).expect("first acquire");
        // 同一线程再取一层：允许，且不新开句柄。
        let nested = StoreLock::acquire(&root, Duration::from_millis(50)).expect("nested acquire");
        drop(nested);
        // 另一条线程：文件共享冲突 → busy。这条才证明「跨进程/跨线程互斥」成立。
        let root2 = root.clone();
        let busy = std::thread::spawn(move || {
            StoreLock::acquire(&root2, Duration::from_millis(120)).err()
        })
        .join()
        .expect("thread");
        assert_eq!(busy.as_deref(), Some("store.busy"));
        drop(first);
        let again = StoreLock::acquire(&root, DEFAULT_TIMEOUT).expect("after release");
        drop(again);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_store_root_is_a_no_op_instead_of_an_error() {
        assert!(StoreLock::acquire("", DEFAULT_TIMEOUT).is_ok());
    }

    #[test]
    fn the_lock_file_is_never_written() {
        let root = temp("empty");
        let guard = StoreLock::acquire(&root, DEFAULT_TIMEOUT).expect("acquire");
        let meta = std::fs::metadata(std::path::Path::new(&root).join(".lock")).expect("lock file");
        assert_eq!(meta.len(), 0, "the lock file is only a handle carrier");
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
    }
}
