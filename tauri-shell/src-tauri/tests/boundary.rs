//! 行为测试套件刻意避开的边界条件：
//! Windows 路径长度、针对同一个 store 的并发事务，以及一个
//! 接受连接后却什么都不说的 HTTP 对端。
//!
//! 为什么单独一个文件：`contract.rs` / `write_path.rs` 断言的是*单个调用方*
//! 逐次调用时 store *记录了什么*。这里的一切都关乎边界 —— 一个
//! 除非你换种方式请求、否则 OS 本身就拒绝的路径，几个写者
//! 共用一份 journal，以及一个永不回话的 socket。因此每个测试都
//! 断言一条无论实现怎么写都必须成立的性质（磁盘上的字节、行数、耗时），
//! 而且每个都有一个能让它变红的变异。

mod common;

use agentkeeper_shell_lib::core::store::{Op, Store};
use agentkeeper_shell_lib::core::{http, settings, store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use common::{cleanup, read, temp_root};

/* ------------------------------------------------------------------ */
/* fixtures —— 夹具                                                   */
/* ------------------------------------------------------------------ */

/// store 根目录，以及测试可以当作 agent 目录树使用的第二个临时目录。
struct Fixture {
    dir: PathBuf,
    root: String,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let dir = temp_root(tag);
        let root = store::native_str(&dir);
        Fixture { dir, root }
    }

    fn store(&self) -> Store {
        Store::new(&self.root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 用 `store::rmrf`，而不是 `remove_dir_all`：后者的回收层
        // 对不是它自己创建的目录树会失败（见 tests/common/mod.rs）。
        cleanup(&self.dir);
    }
}

/* ------------------------------------------------------------------ */
/* long paths (Windows MAX_PATH) —— 长路径                              */
/* ------------------------------------------------------------------ */

/// `base` 之下一个比 Windows 传统 `MAX_PATH` 更长的文件路径。
///
/// `MAX_PATH` 是 260 个字符，*包含* 终止符，并且它计算的是
/// 整条绝对路径，所以唯一能触及它的办法是加深目录。循环追加
/// 49 字符的片段，直到光路径本身就超过 330，从而给文件名留出余地，
/// 也容得下比本机更短或更长的临时根目录。
fn long_target(base: &Path, file: &str) -> String {
    let mut p = base.to_path_buf();
    let mut level = 0;
    while store::native_str(&p).chars().count() < 330 && level < 20 {
        p.push(format!("level-{:02}-{}", level, "x".repeat(40)));
        level += 1;
    }
    p.push(file);
    store::native_str(&p)
}

/// `\\?\` + 路径，独立于生产用的那个辅助函数拼写：下面那些读回断言
/// 的重点是字节确实在磁盘上，而不是同一个辅助函数的两个副本彼此一致。
fn verbatim_join(p: &str) -> String {
    format!(r"\\?\{}", p)
}

/// 超过 `MAX_PATH` 的路径仍然必须可写可读。
///
/// 这个上限在本机是真实存在的 —— 并未选择加入长路径支持的 `cmd.exe`
/// 对同一个文件会回答"系统找不到指定的路径"（`os error 3`）——
/// 但 `std::fs` 能绕过去。有三件事钉住了它的来源：
/// 测试二进制**没有** `longPathAware` 清单，注册表开关是关的
/// （`HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 0`），
/// 并且本工具链的 `libstd` 里含有 `std::sys::path::windows::maybe_verbatim`
/// —— 那个把绝对路径改写成 `\\?\` 形式的辅助函数。所以 store **不会**自己
/// 加这个前缀，也不应该加：第二个前缀是无效的，而且无条件地加还会
/// 改变每一条短路径的传统 `.`/`..` 与结尾点号处理方式。
///
/// 本测试钉住的是 store 继续继承这一行为：等到有人故意缩短路径
/// （8.3 别名、基于 `cmd` 的复制、绕开 std 的直接 Win32 调用）时，
/// 一个签出在长用户名下十层深的项目就会坏掉，而且失败会表现为
/// 一句误导人的"找不到路径"，可那个目录 Explorer 打开却好好的。
#[test]
fn a_path_past_max_path_is_written_and_read_back_byte_for_byte() {
    let fx = Fixture::new("long-path");
    let target = long_target(&fx.dir, "SKILL.md");
    assert!(
        target.chars().count() > 320,
        "the fixture must exceed MAX_PATH, got {} chars: {}",
        target.chars().count(),
        target
    );

    // 刻意用多字节内容：一次截断或有损的路径转换表现为
    // 不同的字节数，而不是一个错误。
    let content = format!(
        "---\nname: far-away\ndescription: {}\n---\n\nbody 远端资产\n",
        "超出路径上限".repeat(4)
    );

    let store = fx.store();
    let tx = store.transaction(
        &[Op::write(&target, &content)],
        &json!({ "kind": "skill", "action": "create", "assetName": "far-away" }),
    );
    assert_eq!(tx["ok"], json!(true), "the write was refused: {}", tx);

    let on_disk = std::fs::read(verbatim_join(&target))
        .unwrap_or_else(|e| panic!("read back {}: {}", target, e));
    assert_eq!(
        on_disk,
        content.as_bytes(),
        "the bytes on disk differ from what was written"
    );
    assert_eq!(
        store::read_text(Path::new(&target)),
        content,
        "the asset read path cannot read back what the write path produced"
    );

    // 不得留下任何写了一半的东西：`atomic_write`
    // 创建并重命名的临时文件就在目标所在的同一个目录里。
    let parent = Path::new(&target).parent().expect("parent");
    let names: Vec<String> = std::fs::read_dir(verbatim_join(&store::native_str(parent)))
        .expect("read the deepest directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["SKILL.md".to_string()],
        "leftover files: {:?}",
        names
    );
}

/* ------------------------------------------------------------------ */
/* concurrency —— 并发                                                 */
/* ------------------------------------------------------------------ */

/// 写者 `t` 在第 `i` 次迭代产生的内容；既用来写入，也用来
/// 核对并发读者看到的东西。
fn writer_body(t: usize, i: usize) -> String {
    format!(
        "writer-{}-iteration-{}\n{}",
        t,
        i,
        format!("{}-", t).repeat(1000)
    )
}

/// 每次调用一个事务，把 `content` 写入 `path`。
fn write_once(store: &Store, path: &str, content: &str) -> Value {
    store.transaction(
        &[Op::write(path, content)],
        &json!({ "kind": "rule", "action": "create", "assetName": "concurrent" }),
    )
}

/// 八个写者，每个二十五个事务，各写不同的文件，共用一个
/// store 根目录。
///
/// 本测试钉住的是 journal 与临时文件，而不是"能不能用"：
/// `audit.jsonl` 是被八个线程同时追加的，所以一行被撕裂或交错
/// 就会破坏 Activity 页面唯一的数据源；而一个共用的临时文件名
/// 会留下 `.agentkeeper-tmp-*` 垃圾。两者在单线程测试里都看不见。
#[test]
fn eight_threads_writing_different_files_keep_the_journal_intact() {
    let fx = Fixture::new("conc-distinct");
    let root = fx.root.clone();
    let store = Arc::new(fx.store());
    let barrier = Arc::new(Barrier::new(8));
    let failures = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for t in 0..8 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let failures = Arc::clone(&failures);
            let root = root.clone();
            scope.spawn(move || {
                barrier.wait();
                for i in 0..25 {
                    let path = format!(
                        "{}{}thread-{}-item-{}.md",
                        root,
                        std::path::MAIN_SEPARATOR,
                        t,
                        i
                    );
                    let content = format!("---\nname: t{}-{}\n---\nthread {}\n", t, i, t);
                    let tx = write_once(&store, &path, &content);
                    if tx["ok"] != json!(true) {
                        failures.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if read(&path) != content {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    assert_eq!(
        failures.load(Ordering::Relaxed),
        0,
        "some writes failed or lost content"
    );

    let audit = read(&store.audit_file());
    let lines: Vec<&str> = audit.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        200,
        "audit.jsonl must hold exactly one line per transaction"
    );
    for (n, line) in lines.iter().enumerate() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("line {} is not valid JSON ({}): {}", n + 1, e, line));
    }

    // 目录树里任何地方都没有残留的临时文件，而且 store 保留的每个状态文件
    // 仍然可解析 —— 一个被重命名了一半的 settings.json 会让
    // 下次启动坏掉。
    let mut leftovers: Vec<String> = Vec::new();
    collect(&fx.dir, "", &mut leftovers);
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {:?}",
        leftovers
    );
    assert!(
        !fx.dir.join("backups").exists(),
        "200 transactions must not create a backup tree"
    );
    for name in ["settings.json", "meta.json", "pending.json"] {
        let p = format!("{}{}{}", fx.root, std::path::MAIN_SEPARATOR, name);
        if let Ok(text) = std::fs::read_to_string(&p) {
            serde_json::from_str::<Value>(&text)
                .unwrap_or_else(|e| panic!("{} is no longer valid JSON: {}", name, e));
        }
    }
}

/// 遍历 store 目录树，按相对路径收集临时文件。之所以递归，
/// 是因为残留可能就在任何目标旁边，不只是在根目录。
fn collect(dir: &Path, base: &str, temp: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect(&entry.path(), &rel, temp);
        } else if name.contains("agentkeeper-tmp") {
            temp.push(rel);
        }
    }
}

/// 四个线程写*同一个*路径：读者能观察到的每一种状态都必须是
/// 某一个写者的完整内容，绝不能是两个写者的混合。
///
/// `atomic_write` 采用先写临时文件再重命名，正是为了让读者永远看不到
/// 半个文件，而本测试就是让这条承诺兑现的那一个。两种失败模式
/// 被分开统计，因为它们含义不同：
/// `failed` = 事务本身被拒绝（共用临时文件名会让重命名撞车），
/// `torn` = 读回来的东西不属于任何一个写者的完整内容
/// （原地写入，或一次输掉竞争的重命名）。
#[test]
fn four_threads_writing_one_path_leave_exactly_one_writers_content() {
    let fx = Fixture::new("conc-same");
    let root = fx.root.clone();
    let store = Arc::new(fx.store());
    let path = format!("{}{}shared.md", root, std::path::MAIN_SEPARATOR);
    let barrier = Arc::new(Barrier::new(4));
    let failed = Arc::new(AtomicUsize::new(0));
    let torn = Arc::new(AtomicUsize::new(0));
    // 第一次出问题的读取被原样保留下来："不属于任何一个写者的内容"
    // 在缺少长度和它坏在哪里的标记时对失败信息毫无用处，
    // 而本测试的全部主题正是*它*是怎么坏的。
    let sample = Arc::new(std::sync::Mutex::new(String::new()));
    // 任何写者可能产生的全部内容，这样并发读取就能被判定，
    // 而不必知道是谁赢下了这场竞争。
    let bodies: Vec<String> = (0..4)
        .flat_map(|t| (0..10).map(move |i| writer_body(t, i)))
        .collect();
    let bodies = Arc::new(bodies);

    std::thread::scope(|scope| {
        for t in 0..4 {
            let store = Arc::clone(&store);
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            let failed = Arc::clone(&failed);
            let torn = Arc::clone(&torn);
            let bodies = Arc::clone(&bodies);
            let sample = Arc::clone(&sample);
            scope.spawn(move || {
                barrier.wait();
                for i in 0..10 {
                    // 约 4 KB 的单一重复标记：两次交错的写入
                    // 不可能碰巧产生这种形状。
                    let content = writer_body(t, i);
                    let tx = write_once(&store, &path, &content);
                    if tx["ok"] != json!(true) {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // 在别人还在写的时候读回来：磁盘上的东西
                    // 必须是那些完整内容之一。
                    let seen = read(&path);
                    if !bodies.iter().any(|b| *b == seen) {
                        torn.fetch_add(1, Ordering::Relaxed);
                        let mut slot = sample.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_empty() {
                            *slot = format!(
                                "len={} head={:?} tail={:?}",
                                seen.len(),
                                &seen.chars().take(60).collect::<String>(),
                                &seen.chars().rev().take(30).collect::<String>()
                            );
                        }
                    }
                }
            });
        }
    });

    assert_eq!(
        failed.load(Ordering::Relaxed),
        0,
        "{} of 40 transactions were refused outright",
        failed.load(Ordering::Relaxed)
    );
    assert_eq!(
        torn.load(Ordering::Relaxed),
        0,
        "a concurrent read returned a body no writer ever wrote: {}",
        sample.lock().unwrap_or_else(|e| e.into_inner())
    );

    let final_body = read(&path);
    assert!(
        bodies.iter().any(|b| *b == final_body),
        "the final file is not any single writer's complete content:\n{}",
        final_body
    );

    let audit = read(&store.audit_file());
    assert_eq!(
        audit.split('\n').filter(|l| !l.is_empty()).count(),
        40,
        "one audit line per transaction"
    );
}

/* ------------------------------------------------------------------ */
/* `atomic_write` 中的重命名失败回退分支                                */
/* ------------------------------------------------------------------ */

/// 持有目标文件的句柄所用的共享标志。
///
/// 读 + 写是共享的（这样普通读者和写者仍能正常工作），但
/// **`FILE_SHARE_DELETE` 被刻意排除在外** —— 正是这个遗漏构成了
/// 全部触发条件，见下面的测试。
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x2;

/// 确定性地强制走 `atomic_write` 的"重命名失败、原地写入"分支。
///
/// 在 Windows 上 `std::fs::rename` 就是 `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`，
/// 它必须**删除**它所替换的目标：它会以 `DELETE` 访问权限打开目标。
/// 一个共享读+写但不共享 `FILE_SHARE_DELETE` 的句柄会让这次打开失败
/// （`ERROR_SHARING_VIOLATION` / 32，或 `ERROR_ACCESS_DENIED` / 5），
/// 而对同一个文件的普通写打开只需要 `FILE_WRITE_DATA` —— 那个是*被*共享的。
/// 所以重命名被拒绝，而原地回退仍然成功，在同一个文件上，且没有时序竞争。
/// 这才是它的真实形态：某个扫描器或同步客户端持有目标几百毫秒，
/// 而不是人为注入的错误。
///
/// 本测试证明的是**回退路径可用**。它并不证明回退是原子的 —— 读者看到
/// 截断文件的那个窗口依然存在，而
/// 那个测试 `four_threads_writing_one_path_leave_exactly_one_writers_content`
/// 仍然是防止任何人观察到它的守卫。
#[cfg(windows)]
#[test]
fn a_target_held_without_delete_sharing_makes_atomic_write_fall_back_in_place() {
    use std::os::windows::fs::OpenOptionsExt;

    let fx = Fixture::new("rename-fallback");
    let target = fx.dir.join("settings.json");
    let old = "{\"lang\":\"auto\"}\n";
    std::fs::write(&target, old).expect("seed the target");

    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&target)
        .expect("hold the target without FILE_SHARE_DELETE");

    // 先证明环境确实拒绝这次重命名，再依赖它。探针不是形式主义：
    // 如果将来的 Windows 或别的文件系统允许了这次重命名，`atomic_write`
    // 就会走它的*成功*路径，而下面每一条断言仍然会通过 —— 测试会变绿，
    // 却什么也没测到。有了探针，它会变红，这才是"触发条件失效了"
    // 唯一诚实的结论。
    let probe = target.with_file_name("rename-probe.tmp");
    std::fs::write(&probe, "probe").expect("write the probe");
    let refused = std::fs::rename(&probe, &target);
    assert!(
        refused.is_err(),
        "the held handle did not block rename (got Ok), so this test cannot reach the fallback \
         branch — the trigger is broken, not the code"
    );
    std::fs::remove_file(&probe).expect("remove the probe");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default(),
        old,
        "the refused probe rename changed the target anyway"
    );

    // 刻意用多字节内容：一次有损或被截断的原地写入表现为
    // 不同的字节数，而不是一个错误。
    let new = "{\"lang\":\"auto\",\"theme\":\"dark\",\"note\":\"回退写入\"}\n";
    let result = settings::atomic_write(&target, new);
    assert!(
        result.is_ok(),
        "the fallback branch reported a failure: {:?}",
        result.err()
    );

    assert_eq!(
        std::fs::read(&target).expect("read the target"),
        new.as_bytes(),
        "the in-place fallback did not land the new content byte for byte"
    );

    // 临时文件在回退路径上也必须被清理 —— 残留的
    // `.agentkeeper-tmp-*` 正是残留物关卡在目录树里搜寻的东西，
    // 而在这里它会在每次这样的保存后被丢到用户的设置旁边。
    let names: Vec<String> = std::fs::read_dir(&fx.dir)
        .expect("read the store root")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["settings.json".to_string()],
        "leftover files: {:?}",
        names
    );

    // 现在松开句柄再读一次：字节必须真的到达了
    // 磁盘，而不只是待在缓存或某个带缓冲的句柄里。
    drop(held);
    assert_eq!(
        std::fs::read_to_string(&target).expect("read after releasing the handle"),
        new,
        "the content is not durable once the holder lets go"
    );
}

/* ------------------------------------------------------------------ */
/* 并发上报下的崩溃通道                                               */
/* ------------------------------------------------------------------ */

/// 八个线程 × 二十五条渲染器错误，每条消息都唯一。
///
/// `append_ui_error` 是"读-裁剪-写"：它读整个文件、追加一行、
/// 裁剪到 [`settings::UI_ERROR_LIMIT`] 然后重写。于是两份重叠的上报
/// 会读到同一份旧内容，第二次写就把第一行抹掉了。这里的条数
/// 被特意选成*恰好*等于上限（8 × 25 = 200 = `UI_ERROR_LIMIT`），
// 只有一个原因：裁剪永远不会执行，所以一份更短的文件
/// 不是"上限在起作用"，而是丢了一行 —— 而缺失的消息会指明
/// 丢的是哪一条。
#[test]
fn concurrent_ui_error_reports_keep_every_line() {
    let fx = Fixture::new("ui-error-concurrent");
    let root = fx.root.clone();
    let barrier = Arc::new(Barrier::new(8));
    let refused = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for t in 0..8usize {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let refused = Arc::clone(&refused);
            scope.spawn(move || {
                barrier.wait();
                for i in 0..25usize {
                    let r = settings::ui_error(
                        &root,
                        &json!({
                            "kind": "error",
                            "message": format!("msg-{}-{}", t, i),
                            "stack": "",
                            "view": "settings",
                            "at": "2026-10-20T00:00:00.000Z"
                        }),
                    );
                    // 一次拒绝与丢一行是不同的缺陷，所以它
                    // 被单独统计，而不是折进行数里。
                    if r["ok"] != json!(true) {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    assert_eq!(
        refused.load(Ordering::Relaxed),
        0,
        "{} of 200 reports were refused instead of appended",
        refused.load(Ordering::Relaxed)
    );

    let path = common::join(&fx.root, &[settings::UI_ERROR_FILE]);
    let body = read(&path);
    let lines: Vec<&str> = body.split('\n').filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        settings::UI_ERROR_LIMIT,
        "expected exactly {} lines (one per report, and the cap is {}), got {}",
        settings::UI_ERROR_LIMIT,
        settings::UI_ERROR_LIMIT,
        lines.len()
    );

    let observed: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(n, line)| {
            let rec: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {} is not valid JSON ({}): {}", n + 1, e, line));
            rec["message"].as_str().unwrap_or_default().to_string()
        })
        .collect();

    let seen: std::collections::HashSet<String> = observed.iter().cloned().collect();
    assert_eq!(
        seen.len(),
        observed.len(),
        "a message appears twice: {} lines, {} distinct",
        observed.len(),
        seen.len()
    );

    let expected: std::collections::HashSet<String> = (0..8usize)
        .flat_map(|t| (0..25usize).map(move |i| format!("msg-{}-{}", t, i)))
        .collect();
    let mut missing: Vec<&String> = expected.difference(&seen).collect();
    let mut unexpected: Vec<&String> = seen.difference(&expected).collect();
    // 排序是为了让失败信息在多次运行之间保持稳定（HashSet 没有
    // 顺序，而不稳定的 diff 会让"丢的是哪一行"变得没法读）。
    missing.sort();
    unexpected.sort();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{} messages were lost and {} unexpected appeared; missing={:?} unexpected={:?}",
        missing.len(),
        unexpected.len(),
        missing,
        unexpected
    );
}

/* ------------------------------------------------------------------ */
/* timeouts —— 超时                                                    */
/* ------------------------------------------------------------------ */

/// 接受一个连接，读完请求，然后完全不说话。
///
/// 返回 base URL。线程是分离的，随进程一起死掉；它
/// 一直握着那个 socket，这正是重点 —— 一个回答很慢的对端
/// 与一个永不回答的对端是两种不同的情况。
fn silent_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        // 被接受的流必须被*持有*：`let _ = accept()` 会立刻把它丢掉，
        // 那会关闭 socket，表现为 WSAECONNABORTED
        // (10053) 而不是本测试要测的读超时 —— 这花了一轮调试才注意到，
        // 因为"连接被中止"读起来像超时，直到你去看耗时。
        match listener.accept() {
            Ok(stream) => {
                // 一个连接就够：被测客户端会放弃它；
                // 睡眠让 socket 保持打开而不作回答。
                std::thread::sleep(std::time::Duration::from_secs(30));
                drop(stream);
            }
            Err(e) => eprintln!("silent_server accept failed: {}", e),
        }
    });
    base
}

/// 注册表查询路径必须对一个沉默的对端放弃，而不是把调用方
/// 永远挂住。
///
/// 超时是让一个死掉的网络不至于变成一个卡死的
/// `search_registry` 的唯一原因：该命令运行在 `spawn_blocking` 中，
/// 所以没有截止时间的话这个请求会把一个阻塞池线程*永远*占住 —— UI
/// 还在继续绘制，但 Discover 永不回答，之后每次搜索都排在
/// 同一个死 socket 后面。这里的 400 ms 截止时间故意比
/// 生产环境的 6 s 短（否则测试要花 6 s 来证明
/// 同一个分支）；策略值本身另有一处断言。
#[test]
fn a_silent_peer_times_out_the_registry_query() {
    let base = silent_server();
    let started = std::time::Instant::now();
    let err = http::get_json(&base, 400).expect_err("a silent peer must not answer");
    let elapsed = started.elapsed();
    assert!(err.is_timeout(), "expected a timeout, got {:?}", err);
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "gave up only after {:?} — the socket deadline did not apply",
        elapsed
    );
}

/// 同一个对端，走更新器的请求路径。`request_with_route` 明确设定各阶段
/// （`connect` / `read` / `write`）而不是给一个总的截止时间，
/// 因为下载必须能扛住一次缓慢的 600 MB 传输；没有 read
/// 阶段，一个沉默的对端会把这次检查阻塞到 OS 的 TCP 超时（约 2 分钟）。
#[test]
fn a_silent_peer_times_out_the_update_request() {
    let base = silent_server();
    let started = std::time::Instant::now();
    let err = agentkeeper_shell_lib::core::update::http::fetch_release(
        &format!("{}/releases/latest", base),
        None,
        400,
    )
    .expect_err("a silent peer must not answer");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "gave up only after {:?} ({}) — the read deadline did not apply",
        elapsed,
        err.message()
    );
}

/* ------------------------------------------------------------------ */
/* 生产环境的截止时间是显式的，不是默认值                               */
/* ------------------------------------------------------------------ */

/// 防止"它能工作是因为 ureq 的默认超时恰好比较短"这种解读：
/// 两个调用点都传了自己的截止时间。
#[test]
fn every_outbound_call_names_its_own_deadline() {
    // 6 s 是固定的策略值；用默认值会悄悄漂移。
    assert_eq!(
        agentkeeper_shell_lib::core::discover::SEARCH_TIMEOUT_MS,
        6000
    );
    // 更新器允许每次 API 调用 30 s、每个下载阶段 60 s；两者
    // 都必须保持显式 —— 否则 `download_to_file` 会继承一个
    // 可能因依赖升级而改变的默认值。
    assert_eq!(
        agentkeeper_shell_lib::core::update::manager::NETWORK_TIMEOUT_MS,
        30_000
    );
    assert_eq!(
        agentkeeper_shell_lib::core::update::manager::DOWNLOAD_TIMEOUT_MS,
        60_000
    );
}

/// `settings::audit` 是让 journal 能被多线程追加的原因：
/// 断言行数是看得见的那一半，这里说的是"为什么"。
#[test]
fn the_journal_is_a_plain_appendable_jsonl_file() {
    let fx = Fixture::new("journal");
    let store = fx.store();
    settings::audit(&fx.root, &json!({ "kind": "scan", "action": "scan" }));
    settings::audit(&fx.root, &json!({ "kind": "scan", "action": "scan" }));
    let audit = read(&store.audit_file());
    assert_eq!(audit.lines().count(), 2, "{}", audit);
}

/* ------------------------------------------------------------------ */
/* IPC 线程预算（哪些命令可以阻塞 UI 线程）                            */
/* ------------------------------------------------------------------ */

/// 那些 IO 不受 store schema 限制的命令必须是
/// `async` + `spawn_blocking`；而那些只碰一份小的 JSON 文档
/// （或一个编辑器已经打开的文件）的命令必须保持同步。
///
/// 这是一次刻意的源码形态断言。所保证的性质 ——
/// "这件事在跑的时候窗口还在继续绘制" —— 从集成测试里是观察不到的
/// （根本没有窗口，而且这些命令是私有的），而它防住的那个失败
/// 本来也是看不见的：一个同步的重命令照样返回正确答案，
/// 只是过程中把应用冻住了。与 `scripts/check.js` 的 bridge 关卡
/// 是同一个道理：断言那个看得见的形态，因为 bug 就住在那里。
///
/// 这份清单不是许愿清单 —— 它是那些会遍历目录树、解析整个 journal、
/// 打开 socket 或启动进程的命令集合（判据写在 `lib.rs` 中 `bootstrap` 之上）。
#[test]
fn heavy_commands_are_async_and_light_commands_are_not() {
    const LIB: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"));
    const HEAVY: &[&str] = &[
        "scan",
        "read_asset",
        "search_registry",
        "install_skill",
        "export_bundle",
        "import_bundle",
        "update_check",
        "update_download",
        "update_install",
        "distribute",
        "baseline_diff",
        "set_baseline",
        "audit",
        "local_stats",
        "update_releases",
    ];
    const LIGHT: &[&str] = &[
        "bootstrap",
        "get_settings",
        "save_settings",
        "update_status",
        "update_progress",
        "update_cancel",
        "update_skip",
        "update_save_settings",
        "update_set_token",
        "ui_error",
    ];

    // `(attribute..fn name)` + 直到下一个 attribute 之前的函数体。
    let block = |name: &str| -> String {
        let marker = format!("fn {}(", name);
        let at = LIB
            .find(&marker)
            .unwrap_or_else(|| panic!("`{}` is not declared in lib.rs", name));
        let attr = LIB[..at]
            .rfind("#[tauri::command]")
            .unwrap_or_else(|| panic!("`{}` has no #[tauri::command] attribute", name));
        let end = LIB[at..]
            .find("\n#[tauri::command]")
            .map(|offset| at + offset)
            .unwrap_or(LIB.len());
        LIB[attr..end].to_string()
    };

    for name in HEAVY {
        let decl = block(name);
        assert!(
            decl.contains("async fn "),
            "`{}` does unbounded IO and must be `async` + spawn_blocking: a synchronous \
             command runs on the IPC thread, so the window freezes for the whole call",
            name
        );
        assert!(
            decl.contains("spawn_blocking"),
            "`{}` is async but does not move its IO off the runtime: the closure must run in \
             `tauri::async_runtime::spawn_blocking`",
            name
        );
    }
    for name in LIGHT {
        let decl = block(name);
        assert!(
            !decl.contains("async fn "),
            "`{}` only touches a schema-bounded document (or is the first command to answer); \
             keeping it synchronous avoids a thread hop for no gain — see the note above `bootstrap`",
            name
        );
    }
}
