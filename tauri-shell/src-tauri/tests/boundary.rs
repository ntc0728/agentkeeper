//! Boundary conditions the behavioural suites deliberately stay away from:
//! Windows path length, concurrent transactions against one store, and an
//! HTTP peer that accepts a connection and then says nothing.
//!
//! Why a separate file: `contract.rs` / `write_path.rs` assert *what* the store
//! records for one caller at a time. Everything here is about the edges — a
//! path the OS itself refuses unless you ask differently, several writers
//! sharing one journal, and a socket that never answers. Each test therefore
//! asserts a property that must hold no matter how the implementation is
//! written (bytes on disk, line count, elapsed time), and each has a mutation
//! that makes it red.

mod common;

use agentkeeper_shell_lib::core::store::{Op, Store};
use agentkeeper_shell_lib::core::{http, settings, store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use common::{cleanup, read, temp_root};

/* ------------------------------------------------------------------ */
/* fixtures                                                            */
/* ------------------------------------------------------------------ */

/// A store root plus a second temp dir the test may use as an agent tree.
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
        // `store::rmrf`, not `remove_dir_all`: the latter's trash layer fails on
        // trees it did not create (see tests/common/mod.rs).
        cleanup(&self.dir);
    }
}

/* ------------------------------------------------------------------ */
/* long paths (Windows MAX_PATH)                                       */
/* ------------------------------------------------------------------ */

/// A file path under `base` that is longer than Windows' legacy `MAX_PATH`.
///
/// `MAX_PATH` is 260 characters *including* the terminator and it counts the
/// whole absolute path, so the only way to reach it is depth. The loop appends
/// 49-character segments until the path alone is over 330, which leaves room
/// for the file name and for a temp root shorter or longer than this host's.
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

/// `\\?\` + path, spelled independently of the production helper: the point of
/// the read-back assertions below is that the bytes are on disk, not that two
/// copies of the same helper agree.
fn verbatim_join(p: &str) -> String {
    format!(r"\\?\{}", p)
}

/// A path past `MAX_PATH` must still be writable and readable.
///
/// The ceiling is real on this host — `cmd.exe`, which does not opt in, answers
/// "the system cannot find the path specified" (`os error 3`) for the very same
/// file — but `std::fs` gets past it. Three facts pin down where that comes
/// from: the test binary carries **no** `longPathAware` manifest, the registry
/// switch is off (`HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\
/// LongPathsEnabled = 0`), and this toolchain's `libstd` contains
/// `std::sys::path::windows::maybe_verbatim` — the helper that rewrites an
/// absolute path into its `\\?\` form. So the store does **not** prepend the
/// prefix itself, and should not: a second prefix is invalid, and applying one
/// unconditionally would also change the legacy `.`/`..` and trailing-dot
/// handling for every short path.
///
/// What this test pins down is that the store keeps inheriting that behaviour:
/// the day someone shortens paths on purpose (an 8.3 alias, a `cmd`-based copy,
/// a direct Win32 call that bypasses std), a project checked out ten levels deep
/// under a long user name breaks, and the failure surfaces as a misleading
/// "cannot find the path" on a directory Explorer opens fine.
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

    // Multibyte content on purpose: a truncating or lossy path conversion shows
    // up as a different byte count, not as an error.
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

    // Nothing half-written may be left behind: the temp file `atomic_write`
    // creates and renames sits in the same directory as the target.
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
/* concurrency                                                         */
/* ------------------------------------------------------------------ */

/// The body writer `t` produces on iteration `i`; used both to write and to
/// check what a concurrent reader saw.
fn writer_body(t: usize, i: usize) -> String {
    format!(
        "writer-{}-iteration-{}\n{}",
        t,
        i,
        format!("{}-", t).repeat(1000)
    )
}

/// One transaction per call, writing `content` to `path`.
fn write_once(store: &Store, path: &str, content: &str) -> Value {
    store.transaction(
        &[Op::write(path, content)],
        &json!({ "kind": "rule", "action": "create", "assetName": "concurrent" }),
    )
}

/// Eight writers, twenty-five transactions each, all different files, one
/// store root.
///
/// What this pins down is the journal and the temp files, not "does it work":
/// `audit.jsonl` is appended to from eight threads at once, so a torn or
/// interleaved line would corrupt the Activity page's only data source, and a
/// shared temp name would leave `.agentkeeper-tmp-*` garbage behind. Both are
/// invisible in single-threaded tests.
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

    // No temp file survived anywhere in the tree, and every state file the
    // store keeps is still parseable — a half-renamed settings.json would break
    // the next launch.
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

/// Walk the store tree, collecting temp files by relative path. Recursive
/// because a leftover can sit next to any target, not just at the root.
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

/// Four threads writing the *same* path: every state a reader can observe must
/// be one writer's complete body, never a mixture of two.
///
/// `atomic_write` is temp-then-rename precisely so a reader can never observe a
/// half-written file, and this is the test that holds it to that. Two failure
/// modes are counted separately because they mean different things:
/// `failed` = the transaction itself refused (a shared temp name makes the
/// rename collide), `torn` = a read returned something that is not any writer's
/// complete body (an in-place write, or a rename that lost a race).
#[test]
fn four_threads_writing_one_path_leave_exactly_one_writers_content() {
    let fx = Fixture::new("conc-same");
    let root = fx.root.clone();
    let store = Arc::new(fx.store());
    let path = format!("{}{}shared.md", root, std::path::MAIN_SEPARATOR);
    let barrier = Arc::new(Barrier::new(4));
    let failed = Arc::new(AtomicUsize::new(0));
    let torn = Arc::new(AtomicUsize::new(0));
    // The first offending read is kept verbatim: "not one writer's body" is
    // useless in a failure message without the length and the marker it broke
    // on, and this test's whole subject is *how* it broke.
    let sample = Arc::new(std::sync::Mutex::new(String::new()));
    // Every body any writer can produce, so a concurrent read can be judged
    // without knowing who won the race.
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
                    // ~4 KB of a single repeated marker: two interleaved writes
                    // cannot accidentally produce this shape.
                    let content = writer_body(t, i);
                    let tx = write_once(&store, &path, &content);
                    if tx["ok"] != json!(true) {
                        failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    // Read back while others are still writing: whatever is on
                    // disk must be one of the complete bodies.
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
/* the rename-failure fallback in `atomic_write`                       */
/* ------------------------------------------------------------------ */

/// Sharing flags for the handle that holds the target file.
///
/// Read + write are shared (so ordinary readers and writers keep working), but
/// **`FILE_SHARE_DELETE` is deliberately left out** — that omission is the whole
/// trigger, see the test below.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x2;

/// `atomic_write`'s "rename failed, write in place" branch, forced
/// deterministically.
///
/// `std::fs::rename` is `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)` on Windows,
/// which must **delete** the target it replaces: it opens the target for
/// `DELETE` access. A handle that shares read+write but not
/// `FILE_SHARE_DELETE` makes that open fail (`ERROR_SHARING_VIOLATION` / 32, or
/// `ERROR_ACCESS_DENIED` / 5), while a plain write-open of the same file needs
/// only `FILE_WRITE_DATA` — which *is* shared. So rename is refused and the
/// in-place fallback still succeeds, on the same file, with no timing race.
/// This is the real shape of it: a scanner or a sync client holding the target
/// for a few hundred milliseconds, not a fabricated error injection.
///
/// What this test proves is that **the fallback path works**. It does not prove
/// the fallback is atomic — the window where a reader sees a truncated file is
/// still there, and `four_threads_writing_one_path_leave_exactly_one_writers_content`
/// stays the guard against anyone observing it.
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

    // Prove the environment really refuses the rename before relying on it. A
    // probe is not ceremony: if a future Windows or a different filesystem ever
    // allows this rename, `atomic_write` takes its *success* path and every
    // assertion below would still pass — the test would go green while testing
    // nothing. With the probe it goes red instead, which is the only honest
    // outcome for "the trigger stopped working".
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

    // Multibyte body on purpose: a lossy or truncated in-place write shows up as
    // a different byte count, not as an error.
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

    // The temp file must be cleaned up on the fallback path too — a leftover
    // `.agentkeeper-tmp-*` is what the residue gate hunts in the tree, and here
    // it would be dropped next to the user's settings on every such save.
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

    // Now let go of the handle and read again: the bytes must have reached the
    // disk, not merely a cache or a buffered handle.
    drop(held);
    assert_eq!(
        std::fs::read_to_string(&target).expect("read after releasing the handle"),
        new,
        "the content is not durable once the holder lets go"
    );
}

/* ------------------------------------------------------------------ */
/* the crash channel under concurrent reports                          */
/* ------------------------------------------------------------------ */

/// Eight threads × twenty-five renderer errors, every message unique.
///
/// `append_ui_error` is a read-trim-write: it reads the whole file, appends a
/// line, trims to [`settings::UI_ERROR_LIMIT`] and rewrites. Two reports that
/// overlap therefore both read the same old body and the second write erases the
/// first line. The count here is chosen to be *exactly* the cap (8 × 25 = 200 =
/// `UI_ERROR_LIMIT`) for one reason: trim can never run, so a shorter file is
/// not "the cap working", it is a dropped line — and the missing message names
/// which one.
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
                    // A refusal is a different defect from a lost line, so it is
                    // counted separately rather than folded into the line count.
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
    // Sorted so a failure message is stable between runs (a HashSet has no
    // order, and an unstable diff makes "which line was lost" unreadable).
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
/* timeouts                                                            */
/* ------------------------------------------------------------------ */

/// Accept one connection, read the request, then say nothing at all.
///
/// Returns the base URL. The thread is detached and dies with the process; it
/// holds the socket open, which is the whole point — a peer that answers
/// slowly is a different case from a peer that never answers.
fn silent_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        // The accepted stream must be *held*: `let _ = accept()` drops it
        // immediately, which closes the socket and shows up as WSAECONNABORTED
        // (10053) instead of the read timeout this test is about — it cost one
        // debugging round to notice, because "connection aborted" reads like a
        // timeout until you look at the elapsed time.
        match listener.accept() {
            Ok(stream) => {
                // One connection is enough: the client under test gives up on
                // it; sleeping holds the socket open without answering.
                std::thread::sleep(std::time::Duration::from_secs(30));
                drop(stream);
            }
            Err(e) => eprintln!("silent_server accept failed: {}", e),
        }
    });
    base
}

/// The registry query path must give up on a silent peer instead of hanging the
/// caller forever.
///
/// Timeouts are the only reason a dead network cannot turn into a frozen
/// `search_registry`: the command runs in `spawn_blocking`, so without a
/// deadline the request would park one blocking-pool thread *forever* — the UI
/// keeps painting, but Discover never answers and every later search queues
/// behind the same dead socket. The 400 ms deadline here is shorter than the
/// production 6 s on purpose (the test would otherwise spend 6 s proving the
/// same branch); the policy value itself is asserted separately.
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

/// Same peer, the updater's request path. `request_with_route` sets the phases
/// (`connect` / `read` / `write`) explicitly rather than one total deadline,
/// because the download must survive a slow 600 MB transfer; without the read
/// phase a silent peer blocks the check until the OS TCP timeout (~2 min).
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
/* the production deadlines are explicit, not defaults                  */
/* ------------------------------------------------------------------ */

/// Guard against the "it works because ureq's default happens to be short"
/// reading: both call sites pass a deadline of their own.
#[test]
fn every_outbound_call_names_its_own_deadline() {
    // 6 s is the fixed policy value; a default would drift silently.
    assert_eq!(
        agentkeeper_shell_lib::core::discover::SEARCH_TIMEOUT_MS,
        6000
    );
    // The updater allows 30 s per API call and 60 s per download phase; both
    // must stay explicit — `download_to_file` would otherwise inherit a default
    // that a dependency bump can change.
    assert_eq!(
        agentkeeper_shell_lib::core::update::manager::NETWORK_TIMEOUT_MS,
        30_000
    );
    assert_eq!(
        agentkeeper_shell_lib::core::update::manager::DOWNLOAD_TIMEOUT_MS,
        60_000
    );
}

/// `settings::audit` is what keeps the journal appendable from many threads:
/// asserting the line count is the visible half, this is the why.
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
/* IPC thread budget (which commands may block the UI thread)          */
/* ------------------------------------------------------------------ */

/// The commands whose IO is *not* bounded by the store schema must be
/// `async` + `spawn_blocking`; the ones that only touch one small JSON document
/// (or one file the editor already holds) must stay synchronous.
///
/// This is a source-shape assertion on purpose. The property being guaranteed —
/// "the window keeps painting while this runs" — cannot be observed from an
/// integration test (there is no window, and the commands are private), and the
/// failure it prevents is invisible anyway: a synchronous heavy command still
/// returns the right answer, it just freezes the app while doing it. The same
/// reasoning as `scripts/check.js`'s bridge gate: assert the shape that is
/// visible, because that is where the bug lives.
///
/// The list is not a wish list — it is the set of commands that walk a directory
/// tree, parse the whole journal, open a socket or spawn a process (the
/// criterion is written out above `bootstrap` in `lib.rs`).
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

    // `(attribute..fn name)` + the body up to the next attribute.
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
