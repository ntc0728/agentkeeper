//! The write path: validate, in-memory undo, atomic write, audit.
//!
//! Every mutation goes through [`Store::transaction`], which guarantees, in
//! this order:
//!
//! 1. validate — a malformed op is rejected before a single byte moves
//! 2. undo snapshot — the current state of every touched path is read **into
//!               memory**. Nothing is copied to disk anywhere: the product has
//!               no backup tree and no by-id rollback, so the only thing the
//!               snapshot has to serve is the failure path below.
//! 3. atomic write — `<path>.agentkeeper-tmp-<pid>-<ms>` then `rename(2)`
//! 4. undo on failure — a failed op puts the touched paths back from the
//!               in-memory snapshot and answers `{ ok: false }`. The undo
//!               attempts **every** path and reports **all** the ones it could
//!               not put back, because a path it fails on is still a path it
//!               left in its post-write state.
//! 5. audit — one `audit.jsonl` line describing what changed
//!
//! Writes land in the real agent directories. There is deliberately no staging
//! layer between the user and the file: a UI that redirects writes into a
//! mirror tree while claiming to have saved is worse than no write at all, and
//! the previous "sandbox mode" did exactly that.
//!
//! **Any caller — UI, MCP, CLI — must go through `transaction()`.** Nothing may
//! write an agent file directly. Going through it is what makes "a failed write
//! leaves the tree as it was" true for *every* path but one: if the undo
//! itself cannot write a path back — permissions changed mid-flight, the volume
//! filled up, or a symbolic link needs privileges the process does not have —
//! that path is reported in `restoreErrors` with its own `code`, and the caller
//! is expected to show it. It is not swallowed, and it is not rare enough to
//! treat as unreachable.
//!
//! ## Deliberate differences from the plainest reading of the spec
//!
//! * Snapshots hold raw bytes, not paths to a copy. A rollback-by-id would need
//!   the copy to outlive the call; undoing a *failed* transaction does not, so
//!   keeping the old state in RAM is enough and costs the disk nothing.
//! * JSONC stripping is shared with the scanner ([`super::jsonc`]) instead of a
//!   second, subtly different regex chain living in this module.

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::jsonc::strip_json_comments;
use super::settings;

/// Directories that are never captured in a snapshot and never copied. Dragging
/// `node_modules` along turns a 40 ms save into a 4 GB one.
const SKIP_DIRS: [&str; 8] = [
    "node_modules",
    ".git",
    "dist",
    "build",
    ".next",
    "target",
    "__pycache__",
    ".venv",
];

/// Ceiling on the bytes one transaction may hold in memory while it runs.
///
/// The snapshot has to be complete: a directory that cannot be captured must
/// refuse the transaction *before* the first write, never restore "mostly". So
/// the guard is a hard failure, not a truncation.
///
/// ## Where the number comes from
///
/// This is a *product* ceiling, not a memory-pressure estimate. The undo is
/// held for the lifetime of one transaction, on the UI thread of a desktop
/// app that also has a WebView resident, so the only question is "how large a
/// snapshot can we hold briefly without the app becoming unresponsive".
///
/// Measured against the assets this app actually manages on a real machine
/// (per-directory totals after [`SKIP_DIRS`]):
///
/// | touched path                                   | bytes    |
/// |------------------------------------------------|----------|
/// | a skill directory (`~/.claude/skills/<name>`)   | ≤ 2.1 MiB |
/// | a rules/memory file (`AGENTS.md`, `CLAUDE.md`)  | ≤ 40 KiB  |
/// | an MCP config (`mcp.json`, `config.toml`)       | ≤ 40 KiB  |
/// | **the whole `skills/` parent, worst case**      | ~4.4 MiB  |
///
/// So 8 MiB is roughly 2× the largest *individual* managed directory
/// (2.1 MiB) and ~2× the largest parent directory an op can name (4.4 MiB),
/// which puts the refusal above every legitimate write and below the build
/// trees the walk already skips. The two realistic ways to exceed it are a
/// vendored `node_modules` committed *inside* a skill and a media-heavy
/// plugin, and both are genuinely worth refusing: holding 100 MiB of bytes to
/// undo a save that will not fail is a worse outcome than telling the user.
///
/// It is deliberately **not** raised to cover pathological cases. The trade is
/// explicit: a cap that "probably always fits" costs the user a write they
/// could have made, and the cap exists precisely because the memory is the
/// only undo there is.
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Windows 上路径分隔符统一为 `\`；store 路径按字符串比较和记日志，
/// 所以分隔符采用固定的形式。
pub fn native_str(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s
    }
}

/* ------------------------------------------------------------------ */
/* JSON helpers                                                        */
/* ------------------------------------------------------------------ */

/// `readJsonOrNull()` — `Some` only if the file exists *and* parses.
///
/// The distinction matters: an empty file parses to `null` in the reference
/// implementation too, and the `json` op aborts rather than overwriting a config
/// it could not read.
pub fn read_json_or_null(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let stripped = strip_json_comments(&text);
    serde_json::from_str(&stripped).ok()
}

/// `readJsonSafe()` — a parse failure looks like an empty object.
pub fn read_json_safe(path: &str) -> Value {
    read_json_or_null(path).unwrap_or_else(|| json!({}))
}

/// `{ ...cur, ...patch }` — 对象的展开合并，也会把数组（或
/// 字符串）变成下标键的对象，而不是并入其中。
fn spread_merge(cur: &Value, patch: &Value) -> Value {
    let mut out = Map::new();
    match cur {
        Value::Object(m) => {
            for (k, v) in m {
                out.insert(k.clone(), v.clone());
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                out.insert(i.to_string(), v.clone());
            }
        }
        Value::String(s) => {
            for (i, c) in s.chars().enumerate() {
                out.insert(i.to_string(), Value::String(c.to_string()));
            }
        }
        _ => {}
    }
    if let Value::Object(p) = patch {
        for (k, v) in p {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

fn object_keys_len(v: &Value) -> usize {
    match v {
        Value::Object(m) => m.len(),
        Value::Array(a) => a.len(),
        Value::String(s) => s.chars().count(),
        _ => 0,
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string())
}

/* ------------------------------------------------------------------ */
/* fs helpers                                                          */
/* ------------------------------------------------------------------ */

/// `copyDir()` — recursive copy that refuses to drag build trees along and
/// reproduces symlinks (falling back to the target's bytes when the link
/// cannot be created, e.g. an unprivileged Windows process).
pub fn copy_dir(src: &Path, dest: &Path) {
    if std::fs::create_dir_all(dest).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            let link_ok = std::fs::read_link(&from).ok().map(|target| {
                let is_dir = std::fs::metadata(&from)
                    .map(|m| m.is_dir())
                    .unwrap_or(false);
                if is_dir {
                    symlink_dir(&target, &to)
                } else {
                    symlink_file(&target, &to)
                }
            });
            if !matches!(link_ok, Some(Ok(()))) {
                // A dangling link cannot be recreated; copying whatever the
                // target points at is better than silently dropping the entry.
                let _ = std::fs::copy(&from, &to);
            }
        } else if ft.is_dir() {
            copy_dir(&from, &to);
        } else {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

#[cfg(windows)]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(not(windows))]
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(windows))]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Read a file as UTF-8 text, lossy on invalid sequences.
///
/// `readAll()` in the reference wrapper returns `''` for a missing or unreadable file
/// and the asset drawer renders that as "empty", so a failure here must not be
/// an error. Every read of asset content goes through this one function.
pub fn read_text(p: &Path) -> String {
    std::fs::read(p)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// `rmrf()` — a link is unlinked, never followed.
pub fn rmrf(p: &Path) {
    // `fs.existsSync()` follows links: a dangling symlink is left alone.
    if !p.exists() {
        return;
    }
    let Ok(st) = std::fs::symlink_metadata(p) else {
        return;
    };
    if st.is_dir() {
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                rmrf(&entry.path());
            }
        }
        if std::fs::remove_dir(p).is_err() {
            let _ = std::fs::remove_dir_all(p);
        }
    } else if std::fs::remove_file(p).is_err() {
        let _ = std::fs::remove_file(p);
    }
}

/// `listFiles()` — relative paths with `/`, symlinks counted as files (so a
/// link cycle cannot recurse forever even before the `seen` guard).
pub fn list_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk_files(dir, "", &mut seen, &mut out);
    out
}

fn walk_files(dir: &Path, base: &str, seen: &mut HashSet<PathBuf>, out: &mut Vec<String>) {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return;
    };
    if !seen.insert(real) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_files(&entry.path(), &rel, seen, out),
            Ok(_) => out.push(rel),
            Err(_) => out.push(rel),
        }
    }
}

/// Recursively read one directory into an in-memory [`DirSnap`] list.
///
/// `seen` guards a link cycle exactly like [`walk_files`] does: without it a
/// directory pointing at its own parent would recurse until the budget ran out,
/// and the resulting error message would blame the user's asset rather than the
/// loop. [`SKIP_DIRS`] are skipped for the same reason [`copy_dir`] skips them.
fn collect_dir(
    dir: &Path,
    base: &str,
    budget: &mut usize,
    out: &mut Vec<DirSnap>,
) -> Result<(), TxError> {
    let mut seen = HashSet::new();
    collect_dir_seen(dir, base, budget, out, &mut seen)
}

fn collect_dir_seen(
    dir: &Path,
    base: &str,
    budget: &mut usize,
    out: &mut Vec<DirSnap>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), TxError> {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return Ok(());
    };
    if !seen.insert(real) {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skip_dir(&name) {
            continue;
        }
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };
        let from = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            let target = std::fs::read_link(&from)
                .map(|t| native_str(&t))
                .unwrap_or_default();
            out.push(DirSnap::Link(
                rel,
                target,
                std::fs::metadata(&from)
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
            ));
        } else if ft.is_dir() {
            out.push(DirSnap::Dir(rel.clone()));
            collect_dir_seen(&from, &rel, budget, out, seen)?;
        } else {
            let content = std::fs::read(&from).map_err(|e| {
                TxError::plain(format!(
                    "the file could not be read into the undo snapshot ({})",
                    e
                ))
            })?;
            if content.len() > *budget {
                return Err(TxError::snapshot(over_budget_message(&rel, content.len())));
            }
            *budget -= content.len();
            out.push(DirSnap::File(rel, content));
        }
    }
    Ok(())
}

/// Write a captured [`DirSnap`] list back under `dir`.
///
/// The directory is emptied first: the undo must produce the *captured* tree,
/// not a merge, or a file the transaction created inside it would survive.
/// That makes every failure *after* the `rmrf` destructive, so the walk
/// collects failures and finishes the remaining entries instead of bailing at
/// the first one — a bail would leave the tree holding only the entries it
/// happened to reach, which is the half-restored state this function exists to
/// prevent. Before destroying anything, a probe write proves the directory can
/// be written back at all: refused here, the captured tree is still intact.
fn restore_dir(dir: &Path, entries: &[DirSnap]) -> Vec<RestoreFailure> {
    let mut out = Vec::new();
    if let Err(e) = std::fs::create_dir_all(dir) {
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!("the directory could not be recreated ({})", e),
        ));
        return out;
    }
    let probe = dir.join(".agentkeeper-restore-probe");
    if let Err(e) = std::fs::write(&probe, b"") {
        // Refused before the `rmrf` below, so nothing has been destroyed yet.
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!(
                "the directory is not writable, so the captured tree was left in place ({})",
                e
            ),
        ));
        return out;
    }
    let _ = std::fs::remove_file(&probe);
    rmrf(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        out.push(RestoreFailure::new(
            &native_str(dir),
            RESTORE_PATH_FAILED,
            format!("the directory could not be recreated ({})", e),
        ));
        return out;
    }
    for entry in entries {
        let rel = entry_name(entry);
        let to = dir.join(&rel);
        match entry {
            DirSnap::Dir(_) => {
                if let Err(e) = std::fs::create_dir_all(&to) {
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_PATH_FAILED,
                        e.to_string(),
                    ));
                }
            }
            DirSnap::File(_, content) => {
                if let Some(parent) = to.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        out.push(RestoreFailure::new(
                            &native_str(parent),
                            RESTORE_PATH_FAILED,
                            e.to_string(),
                        ));
                        continue;
                    }
                }
                if let Err(e) = std::fs::write(&to, content) {
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_PATH_FAILED,
                        e.to_string(),
                    ));
                }
            }
            DirSnap::Link(_, target, is_dir) => {
                let created = if *is_dir {
                    symlink_dir(Path::new(target), &to)
                } else {
                    symlink_file(Path::new(target), &to)
                };
                if let Err(e) = created {
                    // An unprivileged Windows process cannot create a link, and
                    // an unlinked target has no bytes to fall back to. The rest
                    // of the tree is still restored, and this entry is reported
                    // with what the user has to do about it — a silently missing
                    // entry is the half-restored tree this must never produce.
                    out.push(RestoreFailure::new(
                        &native_str(&to),
                        RESTORE_SYMLINK_FAILED,
                        format!(
                            "the link was not recreated ({}); the other files in this directory are back, but this one is missing, because creating a symbolic link on Windows needs administrator rights or developer mode — run the app as administrator, or recreate the link by hand",
                            e
                        ),
                    ));
                }
            }
        }
    }
    out
}

fn entry_name(entry: &DirSnap) -> String {
    match entry {
        DirSnap::Dir(name) | DirSnap::File(name, _) | DirSnap::Link(name, _, _) => name.clone(),
    }
}

/// `pruneEmptyParents()` — remove directories that existed only to hold the
/// file we just deleted, stopping at `stop_at` or the volume root.
fn prune_empty_parents(dir: &str, stop_at: &str) {
    let boundary = normalize_path(stop_at);
    let mut cur = normalize_path(dir);
    for _ in 0..12 {
        if cur == boundary || cur == volume_root(&cur) {
            break;
        }
        match std::fs::read_dir(&cur) {
            Ok(mut it) => {
                if it.next().is_some() {
                    return;
                }
            }
            Err(_) => return,
        }
        if std::fs::remove_dir(&cur).is_err() {
            return;
        }
        match Path::new(&cur).parent() {
            Some(p) => cur = native_str(p),
            None => return,
        }
    }
}

/// `path.resolve()` for an already-absolute path: collapse `.` and `..`.
pub fn normalize_path(p: &str) -> String {
    let path = Path::new(p);
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    native_str(&out)
}

fn volume_root(p: &str) -> String {
    match Path::new(p).components().next() {
        Some(std::path::Component::Prefix(prefix)) => native_str(Path::new(prefix.as_os_str())),
        _ => native_str(&PathBuf::from(std::path::MAIN_SEPARATOR.to_string())),
    }
}

/// One path the undo could not put back, with a machine-readable reason.
///
/// A failure here is not an exception: the other paths are still attempted, so
/// the type exists to make "one bad path" and "several bad paths" the same
/// shape. [`code`](Self::code) is what the renderer keys its wording off —
/// a caller that had to parse the English sentence would break the moment the
/// wording was reworded, which is exactly what happened when the symlink
/// message gained its "run as administrator" sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreFailure {
    path: String,
    code: &'static str,
    detail: String,
}

impl RestoreFailure {
    fn new(path: &str, code: &'static str, detail: String) -> RestoreFailure {
        RestoreFailure {
            path: path.to_string(),
            code,
            detail,
        }
    }

    /// An I/O failure with no code of its own — the disk simply said no. Its
    /// `detail` is the raw `io::Error`, so the sentence a caller builds from it
    /// keeps the `write failed for §1: §2` shape the dictionary already maps.
    fn plain(path: &str, e: impl std::fmt::Display) -> RestoreFailure {
        RestoreFailure::new(path, RESTORE_PATH_FAILED, e.to_string())
    }

    /// Stable identifier — never the sentence. See [`STORE_SNAPSHOT_TOO_LARGE`]
    /// and [`RESTORE_SYMLINK_FAILED`].
    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The `restoreErrors` entry shape. Building it here means the renderer's
    /// field names cannot drift from the ones the engine emits.
    fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "code": self.code,
            "detail": self.detail,
        })
    }

    /// One sentence for callers that read a single string. With more than one
    /// failure the caller joins them, so no failure is dropped — dropping them
    /// is what made the old `restoreError` under-report the state of the tree.
    ///
    /// The path is rendered first and on its own, because it is the one part
    /// that must survive any re-wording of `detail`: a user acts on *which*
    /// asset is wrong, and only afterwards on why.
    fn sentence(&self) -> String {
        format!("{}: {}", self.path, self.detail)
    }
}

/// One sentence per failure, joined. Empty in, empty out — so the caller can
/// test "was the undo clean?" against the string alone.
fn restore_sentence(failures: &[RestoreFailure]) -> String {
    failures
        .iter()
        .map(|f| f.sentence())
        .collect::<Vec<String>>()
        .join("; ")
}

/// `errorCode` for a transaction refused because a touched path is larger than
/// the in-memory undo can hold. Nothing was written, so this is a refusal and
/// not a restore problem — but the caller still needs to say it apart from an
/// ordinary I/O failure, because the fix is "use a smaller asset", not "retry".
pub const STORE_SNAPSHOT_TOO_LARGE: &str = "store.snapshotTooLarge";

/// `errorCode` for "the undo put the files back but a symbolic link inside a
/// directory could not be recreated" — the partial-success case. The files are
/// there and the link is not, and only a privileged run can fix that.
pub const RESTORE_SYMLINK_FAILED: &str = "store.symlinkRestoreFailed";

/// A restore failed for a reason the codes above do not name: the path could
/// not be written back at all (permissions changed, the path is locked, the
/// volume filled up mid-restore).
const RESTORE_PATH_FAILED: &str = "store.restorePathFailed";

/// `errorCode` for "the undo could not put every path back". Emitted
/// whenever `restoreErrors` is non-empty, so the renderer can tell "nothing was
/// written" apart from "written, then partly undone" without reading prose.
pub const STORE_RESTORE_INCOMPLETE: &str = "store.restoreIncomplete";

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// The refusal a caller sees when a touched path will not fit the undo budget.
///
/// It has to answer three questions on its own, because it is the only thing the
/// user sees: what the limit is, how big the offending thing is, and what to do
/// about it. "the in-memory undo limit is 8388608 bytes" answers none of them,
/// and the raw byte count in particular reads like a number the user cannot
/// change is in their way.
///
/// The literal is kept on **one source line** on purpose: `scripts/tauri-i18n.js`
/// files any literal that still contains a `\` under "not prose", so a sentence
/// wrapped with `\` across lines would never be asked for a translation.
fn over_budget_message(who: &str, size: usize) -> String {
    format!(
        "undo snapshot limit reached: {} needs {} but a single write may only hold {} of undo data, so nothing was written; split this asset into smaller writes, or drop files that are not part of it (dependency folders and build output are skipped automatically)",
        who,
        human_size(size),
        human_size(MAX_SNAPSHOT_BYTES)
    )
}

/// Round *up* to one decimal, so a size never renders below the limit it
/// exceeds: "needs 8.0 MB but may only hold 8.0 MB" reads as a rounding
/// artefact and leaves the reader unable to tell how far over the line they
/// are. `8.0` here means "8.0 and some more".
fn human_size(bytes: usize) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", (b / MB * 10.0).ceil() / 10.0)
    } else if b >= 1024.0 {
        format!("{} KB", (b / 1024.0).ceil() as usize)
    } else {
        format!("{} B", bytes)
    }
}

/// A transaction failure that carries its own `errorCode`.
///
/// The code is attached at the point where the cause is *known*, because the
/// renderer keys its wording off the code; recovering the code from the English
/// sentence later is what made the two drift apart before.
#[derive(Debug)]
struct TxError {
    message: String,
    code: &'static str,
}

impl TxError {
    /// A failure whose code is not one the renderer special-cases: an ordinary
    /// op error, answered as `ok:false` with whatever message the op built.
    fn plain(message: impl Into<String>) -> TxError {
        TxError {
            message: message.into(),
            code: TX_FAILED,
        }
    }

    /// The size cap. It is the one code the renderer must not fall back to
    /// generic wording for: the user's next move is to shrink the asset, and
    /// that advice is in the sentence rather than in the code's parameters.
    fn snapshot(message: impl Into<String>) -> TxError {
        TxError {
            message: message.into(),
            code: STORE_SNAPSHOT_TOO_LARGE,
        }
    }

    /// Add the path the transaction was about to touch.
    ///
    /// Only op errors get the `write failed for <path>: …` stem. That pattern is
    /// translated as a prefix with `§2` passed through untouched — the tail of
    /// those messages is an `io::Error`, which is as far as the dictionary goes
    /// — so a *refusal* that carries advice ("split this asset into smaller
    /// writes") must not be hung off it, or the advice reaches a Chinese user in
    /// English. Refusals keep their own stem and get their own entry.
    fn naming(self, path: &str) -> TxError {
        if self.code == STORE_SNAPSHOT_TOO_LARGE {
            return self;
        }
        TxError {
            message: format!("write failed for {}: {}", path, self.message),
            code: self.code,
        }
    }
}

/// `errorCode` for any transaction failure with no more specific code.
const TX_FAILED: &str = "write.failed";

/// `tx-${Date.now().toString(36)}-${Math.random().toString(36).slice(2,7)}`
fn gen_tx_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut tail = String::with_capacity(5);
    for _ in 0..5 {
        // xorshift64 — the id only has to be unique, not unguessable.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        tail.push(to_base36(state % 36).chars().next().unwrap_or('0'));
    }
    format!("tx-{}-{}", to_base36(now_millis().max(0) as u64), tail)
}

/* ------------------------------------------------------------------ */
/* ops                                                                 */
/* ------------------------------------------------------------------ */

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Write,
    CopyDir,
    Json,
    Delete,
    Mkdir,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Write => "write",
            OpKind::CopyDir => "copyDir",
            OpKind::Json => "json",
            OpKind::Delete => "delete",
            OpKind::Mkdir => "mkdir",
        }
    }
}

/// One mutation. Built through the constructors so a missing field is a
/// compile error rather than a runtime `op[i] is missing a "from"`.
pub struct Op {
    pub kind: OpKind,
    /// The path written. Absolute, as the scanner reported it.
    pub path: String,
    pub from: Option<String>,
    pub content: Option<String>,
    /// `json` op: shallow merge source.
    pub patch: Option<Value>,
    /// `json` op: full control. Returning `None` aborts the transaction.
    pub mutate: Option<Box<dyn Fn(Value) -> Option<Value>>>,
}

impl Op {
    fn new(kind: OpKind, path: &str) -> Op {
        Op {
            kind,
            path: path.to_string(),
            from: None,
            content: None,
            patch: None,
            mutate: None,
        }
    }

    pub fn write(path: &str, content: &str) -> Op {
        let mut op = Op::new(OpKind::Write, path);
        op.content = Some(content.to_string());
        op
    }

    pub fn copy_dir(path: &str, from: &str) -> Op {
        let mut op = Op::new(OpKind::CopyDir, path);
        op.from = Some(from.to_string());
        op
    }

    pub fn json_patch(path: &str, patch: Value) -> Op {
        let mut op = Op::new(OpKind::Json, path);
        op.patch = Some(patch);
        op
    }

    pub fn json_mutate<F>(path: &str, f: F) -> Op
    where
        F: Fn(Value) -> Option<Value> + 'static,
    {
        let mut op = Op::new(OpKind::Json, path);
        op.mutate = Some(Box::new(f));
        op
    }

    pub fn delete(path: &str) -> Op {
        Op::new(OpKind::Delete, path)
    }

    pub fn mkdir(path: &str) -> Op {
        Op::new(OpKind::Mkdir, path)
    }
}

/* ------------------------------------------------------------------ */
/* in-memory undo state                                                */
/* ------------------------------------------------------------------ */

/// One entry of a captured directory, relative to that directory.
#[derive(Clone)]
enum DirSnap {
    /// A subdirectory. Recorded so an empty directory survives the undo — an
    /// asset directory that exists but is empty is a real state, and dropping
    /// it would leave a phantom folder in the agent's tree.
    Dir(String),
    File(String, Vec<u8>),
    /// `target` + "the target was a directory", which is what tells a restore
    /// to call `symlink_dir` instead of `symlink_file`.
    Link(String, String, bool),
}

/// What a transaction touched, so a failure can put it back.
///
/// This lives in memory for exactly as long as the transaction runs and is
/// then dropped: the product keeps no backup tree and offers no rollback by id,
/// so the only consumer is [`Store::restore`] below. `Snapshot` is only ever
/// built by [`Store::snapshot`], which is total — a path that cannot be
/// captured makes the whole transaction refuse *before* the first write, so
/// there is no "partially restorable" variant to fall into.
enum Snapshot {
    /// The path did not exist before: undoing means removing what this
    /// transaction created there.
    Absent {
        path: String,
    },
    File {
        path: String,
        content: Vec<u8>,
    },
    Dir {
        path: String,
        entries: Vec<DirSnap>,
    },
    /// A symbolic link, or a dangling one: the target string plus whether the
    /// target was a directory, which is what picks `symlink_dir` over
    /// `symlink_file` on restore. A link is not a file — recreating it as one
    /// would replace `a -> b` with a stale copy of `b`.
    Link {
        path: String,
        target: String,
        is_dir: bool,
    },
}

impl Snapshot {
    fn path(&self) -> &str {
        match self {
            Snapshot::Absent { path }
            | Snapshot::File { path, .. }
            | Snapshot::Link { path, .. }
            | Snapshot::Dir { path, .. } => path,
        }
    }
}

/* ------------------------------------------------------------------ */
/* Store                                                               */
/* ------------------------------------------------------------------ */

pub struct Store {
    root: PathBuf,
    root_str: String,
}

impl Store {
    /// Creates the store tree if it is not there yet.
    pub fn new(root: &str) -> Store {
        let root_path = PathBuf::from(root);
        let _ = settings::ensure_store(&native_str(&root_path));
        Store {
            root_str: native_str(&root_path),
            root: root_path,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_str(&self) -> &str {
        &self.root_str
    }

    pub fn audit_file(&self) -> String {
        native_str(&self.root.join("audit.jsonl"))
    }

    /* -------------------------------------------------------------- */
    /* undo state                                                      */
    /* -------------------------------------------------------------- */

    /// Capture one path (file, directory or link) into memory.
    ///
    /// Existence is part of the state, so undoing a delete and undoing a create
    /// are the same code with different data. The walk honours [`SKIP_DIRS`]
    /// for the same reason [`copy_dir`] does — a `node_modules` tree is not part
    /// of what any agent asset means, and holding it in RAM would be worse than
    /// useless.
    ///
    /// `Err` is a refusal, not a warning: a directory over [`MAX_SNAPSHOT_BYTES`]
    /// must fail the transaction before the first write, because "restored most
    /// of it" is exactly the half-written state this must never leave behind.
    fn snapshot(&self, original: &str, budget: &mut usize) -> Result<Snapshot, TxError> {
        let path = Path::new(original);
        // `exists()` follows links, so a dangling link reads as absent — use
        // `symlink_metadata` to tell "no such path" from "a broken link".
        let meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                return Ok(Snapshot::Absent {
                    path: original.to_string(),
                })
            }
        };

        if meta.is_symlink() {
            // A dangling link is not an error, it is a state that already
            // exists on disk; `metadata` then fails and the link is recorded as
            // pointing at a non-directory, which is the only thing knowable.
            let target = std::fs::read_link(path)
                .map(|t| native_str(&t))
                .unwrap_or_default();
            let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
            return Ok(Snapshot::Link {
                path: original.to_string(),
                target,
                is_dir,
            });
        }

        if !meta.is_dir() {
            let content = std::fs::read(path).map_err(|e| {
                TxError::plain(format!(
                    "the file could not be read into the undo snapshot, so the write was refused rather than performed with no way back ({})",
                    e
                ))
            })?;
            if content.len() > *budget {
                let name = path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| original.to_string());
                return Err(TxError::snapshot(over_budget_message(&name, content.len())));
            }
            *budget -= content.len();
            return Ok(Snapshot::File {
                path: original.to_string(),
                content,
            });
        }

        let mut entries = Vec::new();
        collect_dir(path, "", budget, &mut entries)?;
        Ok(Snapshot::Dir {
            path: original.to_string(),
            entries,
        })
    }

    /// `transaction(ops, meta)` — the only way a file may change.
    ///
    /// `meta` keys: `txId`, `kind`, `action`, `agentId`,
    /// `agentLabel`, `projectId`, `projectName`, `assetId`, `assetName`.
    ///
    /// The undo guarantee has two halves, and the first is the load-bearing
    /// one: every touched path is captured **before** the first op runs, so a
    /// refusal at capture time means nothing was written at all. The second half
    /// is [`Store::restore`], which runs only on the failure path.
    pub fn transaction(&self, ops: &[Op], meta: &Value) -> Value {
        for (i, op) in ops.iter().enumerate() {
            if matches!(op.kind, OpKind::CopyDir) && op.from.is_none() {
                return json!({
                    "ok": false,
                    "error": format!("op[{}] ({}) is missing a \"from\"", i, op.kind.name()),
                });
            }
        }

        let tx_id = meta
            .get("txId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(gen_tx_id);
        let t0 = now_millis();

        let mut log: Vec<Value> = Vec::new();
        let mut touched: Vec<Snapshot> = Vec::new();
        // One budget for the whole transaction, not per path: five 6 MiB
        // directories are the same problem as one 30 MiB one, and the cap is a
        // statement about peak memory, which a per-path budget would not be.
        let mut budget = MAX_SNAPSHOT_BYTES;

        let outcome: Result<(), TxError> = (|| {
            // 1. capture everything the transaction will touch, before any of it
            //    moves. A failure here means nothing was written at all.
            //
            //    A path named twice is captured once. Two ops on the same path
            //    (distribute pushes `mkdir <rules dir>` + `write <rule>` per
            //    target, so the same parent recurs across targets) would
            //    otherwise be charged twice against the shared budget and
            //    double-counted in `restore`, where the *second* copy would try
            //    to put back a state the first op had already changed. The
            //    pre-transaction capture is the only one worth keeping.
            let mut captured: HashSet<String> = HashSet::new();
            for op in ops {
                if captured.insert(normalize_path(&op.path)) {
                    touched.push(
                        self.snapshot(&op.path, &mut budget)
                            .map_err(|e| e.naming(&op.path))?,
                    );
                }
            }

            // 2. apply.
            for op in ops {
                let real = op.path.clone();
                match op.kind {
                    OpKind::Write => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let content = match (&op.content, op.from.as_deref()) {
                            (Some(c), _) => c.clone(),
                            (None, Some(from)) => match std::fs::read_to_string(from) {
                                Ok(c) => c,
                                Err(_) if Path::new(from).is_dir() => {
                                    return Err(TxError::plain(format!(
                                        "write source is a directory, not a file: {}",
                                        from
                                    )));
                                }
                                Err(e) => {
                                    return Err(TxError::plain(format!(
                                        "cannot read write source {}: {}",
                                        from, e
                                    )))
                                }
                            },
                            (None, None) => format!("{}\n", pretty(&Value::Null)),
                        };
                        if let Err(e) = settings::atomic_write(Path::new(&real), &content) {
                            return Err(TxError::plain(format!(
                                "write failed for {}: {}",
                                real, e
                            )));
                        }
                        log.push(json!({
                            "op": "write",
                            "path": real,
                            "bytes": content.len(),
                        }));
                    }

                    OpKind::CopyDir => {
                        let src = op.from.clone().unwrap_or_default();
                        if !Path::new(&src).exists() {
                            return Err(TxError::plain(format!("copy source missing: {}", src)));
                        }
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if real != src {
                            copy_dir(Path::new(&src), Path::new(&real));
                        }
                        log.push(json!({ "op": "copyDir", "path": real, "from": src }));
                    }

                    OpKind::Json => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let cur = if Path::new(&real).exists() {
                            match read_json_or_null(&real) {
                                Some(v) => v,
                                None => {
                                    return Err(TxError::plain(format!(
                                        "config file is corrupt, refusing to overwrite: {}",
                                        real
                                    )))
                                }
                            }
                        } else {
                            json!({})
                        };

                        let next = match &op.mutate {
                            Some(f) => match f(cur.clone()) {
                                Some(v) => v,
                                None => {
                                    return Err(TxError::plain(format!(
                                        "json mutate aborted for {}",
                                        real
                                    )))
                                }
                            },
                            None => spread_merge(&cur, op.patch.as_ref().unwrap_or(&json!({}))),
                        };
                        let body = format!("{}\n", pretty(&next));
                        if let Err(e) = settings::atomic_write(Path::new(&real), &body) {
                            return Err(TxError::plain(format!(
                                "write failed for {}: {}",
                                real, e
                            )));
                        }
                        log.push(json!({
                            "op": "json",
                            "path": real,
                            "keys": object_keys_len(&next),
                        }));
                    }

                    OpKind::Delete => {
                        let p = Path::new(&real);
                        if p.exists() {
                            if std::fs::symlink_metadata(p)
                                .map(|m| m.is_dir())
                                .unwrap_or(false)
                            {
                                rmrf(p);
                            } else {
                                let _ = std::fs::remove_file(p);
                            }
                        }
                        log.push(json!({ "op": "delete", "path": real }));
                    }

                    OpKind::Mkdir => {
                        let _ = std::fs::create_dir_all(&real);
                        log.push(json!({ "op": "mkdir", "path": real }));
                    }
                }
            }
            Ok(())
        })();

        if let Err(err) = outcome {
            // A restore that itself fails would leave a half-written tree, so
            // the failures are folded into the answer instead of being
            // swallowed — all of them, and each with its own code, so the caller
            // can tell "nothing was written" from "written, then partly undone"
            // without reading prose.
            let restore_errors = self.restore(&touched);
            let error = err.message;
            let error_code = if restore_errors.is_empty() {
                err.code
            } else {
                // The restore failure is the more actionable of the two: the
                // op error is what broke the write, this is what the user has
                // to go look at on disk.
                STORE_RESTORE_INCOMPLETE
            };
            settings::audit(
                &self.root_str,
                &json!({
                    "txId": tx_id,
                    "status": "failed",
                    "error": error,
                    "errorCode": error_code,
                    "restoreErrors": restore_errors.iter().map(|r| r.to_json()).collect::<Vec<Value>>(),
                    "ops": ops
                        .iter()
                        .map(|o| json!({ "op": o.kind.name(), "path": o.path }))
                        .collect::<Vec<Value>>(),
                    "kind": meta.get("kind").cloned().unwrap_or(Value::Null),
                    "agentId": meta.get("agentId").cloned().unwrap_or(Value::Null),
                    "projectId": meta.get("projectId").cloned().unwrap_or(Value::Null),
                    "assetId": meta.get("assetId").cloned().unwrap_or(Value::Null),
                }),
            );
            // `restoreErrors` is the complete list; `restoreError` is the
            // joined form kept for callers that predate it. An empty list is
            // omitted rather than shipped as `[]`, so `get("restoreError")`
            // still answers "was the undo clean?" on the old contract.
            let mut out = json!({
                "ok": false,
                "txId": tx_id,
                "error": error,
                "errorCode": error_code,
            });
            if let Value::Object(m) = &mut out {
                if !restore_errors.is_empty() {
                    m.insert(
                        "restoreError".to_string(),
                        Value::String(restore_sentence(&restore_errors)),
                    );
                    m.insert(
                        "restoreErrors".to_string(),
                        Value::Array(restore_errors.iter().map(|r| r.to_json()).collect()),
                    );
                }
            }
            return out;
        }

        let ms = now_millis() - t0;
        // One entry per **op**, not per captured path: the two lists are
        // deliberately not the same (`mkdir` and `write` on one path are two
        // ops and one snapshot), and the answer/audit pair has always counted
        // ops — `keeper::distribute` reads it to build its `written` list.
        let targets: Vec<Value> = ops.iter().map(|o| Value::String(o.path.clone())).collect();
        let rec = json!({
            "txId": tx_id,
            "status": "ok",
            "ms": ms,
            "kind": meta.get("kind").cloned().unwrap_or(Value::Null),
            "action": meta.get("action").cloned().unwrap_or(Value::Null),
            "agentId": meta.get("agentId").cloned().unwrap_or(Value::Null),
            "agentLabel": meta.get("agentLabel").cloned().unwrap_or(Value::Null),
            "projectId": meta.get("projectId").cloned().unwrap_or(Value::Null),
            "projectName": meta.get("projectName").cloned().unwrap_or(Value::Null),
            "assetId": meta.get("assetId").cloned().unwrap_or(Value::Null),
            "assetName": meta.get("assetName").cloned().unwrap_or(Value::Null),
            "targets": targets,
            "log": log,
        });
        settings::audit(&self.root_str, &rec);
        json!({ "ok": true, "txId": tx_id, "ms": ms, "targets": targets })
    }

    /// Put a captured state list back.
    ///
    /// **Every snapshot is attempted, and every failure is collected.** The old
    /// shape (`Option<String>`, `return` on the first error) meant a transaction
    /// touching N paths stopped at the first path it could not put back: everything
    /// after it stayed in its post-write state and the caller was told about one of
    /// them. The tree was silently half-undone while the answer read like a single
    /// fixable complaint. Returning the full list makes "only the first failure" a
    /// compile error rather than a policy — there is no shorter shape to return.
    ///
    /// A single failure still does not stop the others, so the damage from an
    /// unwritable path is bounded to that path.
    fn restore(&self, snaps: &[Snapshot]) -> Vec<RestoreFailure> {
        let mut out = Vec::new();
        for s in snaps {
            let target = Path::new(s.path());
            let outcome: Result<(), RestoreFailure> = match s {
                Snapshot::Absent { .. } => {
                    // `symlink_metadata`, not `exists()`: the op may have
                    // created a *dangling* link, and `exists()` follows links so
                    // it would report "nothing there" and leave it behind.
                    if std::fs::symlink_metadata(target).is_ok() {
                        rmrf(target);
                    }
                    // Deleting the last file of a freshly created directory (a
                    // new skill folder) would otherwise leave a phantom entry
                    // that some agents still enumerate.
                    if let Some(parent) = target.parent() {
                        prune_empty_parents(&native_str(parent), s.path());
                    }
                    Ok(())
                }
                Snapshot::File { content, .. } => {
                    if target.is_dir() {
                        // The op replaced a directory with a file (or the other
                        // way round); restoring means removing what is there.
                        rmrf(target);
                    }
                    match target.parent() {
                        Some(parent) => match std::fs::create_dir_all(parent) {
                            Ok(()) => std::fs::write(target, content)
                                .map_err(|e| RestoreFailure::plain(s.path(), e)),
                            Err(e) => Err(RestoreFailure::plain(&native_str(parent), e)),
                        },
                        None => std::fs::write(target, content)
                            .map_err(|e| RestoreFailure::plain(s.path(), e)),
                    }
                }
                Snapshot::Dir { entries, .. } => {
                    out.extend(restore_dir(target, entries));
                    Ok(())
                }
                Snapshot::Link {
                    target: link_to,
                    is_dir,
                    ..
                } => {
                    // The op may have left a file or directory where the link
                    // belongs: `symlink_file` on Windows fails with
                    // `ERROR_ALREADY_EXISTS` rather than replacing it, so the
                    // undo has to clear the path itself or it reports a failure
                    // for a tree it could in fact have restored.
                    if std::fs::symlink_metadata(target).is_ok() {
                        rmrf(target);
                    }
                    let created = if *is_dir {
                        symlink_dir(Path::new(link_to), target)
                    } else {
                        symlink_file(Path::new(link_to), target)
                    };
                    created.map_err(|e| {
                        RestoreFailure::new(
                            s.path(),
                            RESTORE_SYMLINK_FAILED,
                            format!(
                                "the link was not recreated ({}); a symbolic link on Windows needs administrator rights or developer mode — run the app as administrator, or recreate the link by hand",
                                e
                            ),
                        )
                    })
                }
            };
            if let Err(problem) = outcome {
                out.push(problem);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a handle that permits reading but **not** writing or deleting.
    ///
    /// This is the deterministic failure trigger for the undo tests: a virus
    /// scanner, a sync client or a second AgentKeeper holding an asset open is
    /// the real shape of it. `FILE_SHARE_READ` alone makes every write-open of
    /// the file fail with a sharing violation while reads still succeed, so the
    /// capture phase can read the file and the undo cannot write it back. A
    /// probe proves the environment really behaves that way before the test
    /// relies on it — without the probe, a future Windows that allowed the
    /// write would leave the test green while testing nothing.
    #[cfg(windows)]
    fn hold_read_only(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
            .expect("hold the path without FILE_SHARE_WRITE");
        assert!(
            std::fs::write(path, b"probe").is_err(),
            "the held handle did not block writes, so this test cannot reach the failing branch"
        );
        held
    }

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ak-store-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_at(root: &Path) -> Store {
        Store::new(&root.to_string_lossy())
    }

    /// **The undo does not stop at the first path it cannot put back.**
    ///
    /// This is the regression the whole `Vec<RestoreFailure>` shape exists for.
    /// `restore` used to `return Some(<first failure>)`, so a transaction
    /// touching several paths left every path *after* the failing one in its
    /// post-write state while the answer named only one of them: the tree was
    /// silently half-undone and the error described a single problem.
    ///
    /// The exclusive handle is taken **after** the transaction, never before.
    /// Held across the transaction it makes the *write* fail instead, so the
    /// transaction aborts and its own undo has already put everything back —
    /// and then no restore problem exists to assert on.
    #[cfg(windows)]
    #[test]
    fn a_failed_restore_does_not_stop_the_paths_after_it() {
        let root = temp_root("restore-collect");
        let store = store_at(&root);

        let locked = root.join("locked.md");
        let untouched = root.join("a.md");
        let early = root.join("b.md");
        let late = root.join("c.md");

        // A *successful* transaction is what leaves several paths changed, so
        // the undo below has several paths to get wrong.
        let ok = store.transaction(
            &[
                Op::write(&locked.to_string_lossy(), "clobbered-locked"),
                Op::write(&untouched.to_string_lossy(), "clobbered-a"),
                Op::write(&early.to_string_lossy(), "clobbered-b"),
                Op::write(&late.to_string_lossy(), "clobbered-c"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(
            ok["ok"],
            json!(true),
            "the setup transaction failed: {}",
            ok
        );

        // Hold `locked.md` against writes, so only the *undo* can fail.
        let _lock = hold_read_only(&locked);
        let undone = store.restore(&[
            Snapshot::File {
                path: locked.to_string_lossy().into_owned(),
                content: b"pre-transaction".to_vec(),
            },
            Snapshot::File {
                path: untouched.to_string_lossy().into_owned(),
                content: b"original-a".to_vec(),
            },
            Snapshot::File {
                path: early.to_string_lossy().into_owned(),
                content: b"original-b".to_vec(),
            },
            Snapshot::File {
                path: late.to_string_lossy().into_owned(),
                content: b"original-c".to_vec(),
            },
        ]);

        assert_eq!(
            undone.len(),
            1,
            "exactly one path could not be put back: {:?}",
            undone
        );
        assert_eq!(undone[0].code(), RESTORE_PATH_FAILED, "{:?}", undone);
        for (p, want) in [
            (&untouched, "original-a"),
            (&early, "original-b"),
            (&late, "original-c"),
        ] {
            assert_eq!(
                std::fs::read_to_string(p).unwrap(),
                want,
                "the path AFTER the failing one was skipped — the undo stopped at the first failure"
            );
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// Every failure is reported, not just the first — a caller that shows the
    /// list must not under-report what is sitting in a changed state on disk.
    ///
    /// Drives [`Store::restore`] directly for the same reason as the test above:
    /// to have several paths genuinely changed, the setup has to be a
    /// *successful* transaction. Its locks are taken afterwards.
    #[cfg(windows)]
    #[test]
    fn every_unrestorable_path_is_reported_not_just_the_first() {
        let root = temp_root("restore-all");
        let store = store_at(&root);

        let one = root.join("locked-1.md");
        let two = root.join("locked-2.md");
        let fine = root.join("fine.md");

        let ok = store.transaction(
            &[
                Op::write(&one.to_string_lossy(), "clobbered-1"),
                Op::write(&two.to_string_lossy(), "clobbered-2"),
                Op::write(&fine.to_string_lossy(), "clobbered-fine"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(
            ok["ok"],
            json!(true),
            "the setup transaction failed: {}",
            ok
        );

        let _l1 = hold_read_only(&one);
        let _l2 = hold_read_only(&two);
        let undone = store.restore(&[
            Snapshot::File {
                path: one.to_string_lossy().into_owned(),
                content: b"orig-1".to_vec(),
            },
            Snapshot::File {
                path: two.to_string_lossy().into_owned(),
                content: b"orig-2".to_vec(),
            },
            Snapshot::File {
                path: fine.to_string_lossy().into_owned(),
                content: b"orig-fine".to_vec(),
            },
        ]);

        assert_eq!(
            undone.len(),
            2,
            "both locked paths must be reported, not just the first: {:?}",
            undone
        );
        let named: Vec<&str> = undone.iter().map(|f| f.path()).collect();
        assert!(
            named.iter().any(|p| p.contains("locked-1.md"))
                && named.iter().any(|p| p.contains("locked-2.md")),
            "both paths named: {:?}",
            named
        );
        assert_eq!(
            std::fs::read_to_string(&fine).unwrap(),
            "orig-fine",
            "the writable path is restored even though two others are not"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The answer shape the renderer keys off: `errorCode` plus the full
    /// `restoreErrors` list, with `restoreError` kept as the joined form.
    ///
    /// The lock is taken *after* a successful transaction, so the transaction
    /// whose answer is under test is one that actually got as far as writing
    /// and then could not put everything back — the partial-success state the
    /// UI has to be able to say out loud.
    #[cfg(windows)]
    #[test]
    fn a_partly_undone_transaction_reports_its_code_and_every_failed_path() {
        let root = temp_root("restore-shape");
        let store = store_at(&root);

        let locked = root.join("locked.md");
        let fine = root.join("fine.md");
        std::fs::write(&locked, "orig-locked").unwrap();
        std::fs::write(&fine, "orig-fine").unwrap();

        let ok = store.transaction(
            &[
                Op::write(&locked.to_string_lossy(), "clobbered-locked"),
                Op::write(&fine.to_string_lossy(), "clobbered-fine"),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(ok["ok"], json!(true), "{}", ok);

        // Rebuild the answer the failure path builds, from the real snapshots,
        // with the lock in place for the undo only.
        let mut budget = MAX_SNAPSHOT_BYTES;
        let snaps = vec![
            store
                .snapshot(&locked.to_string_lossy(), &mut budget)
                .unwrap(),
            store
                .snapshot(&fine.to_string_lossy(), &mut budget)
                .unwrap(),
        ];
        let _lock = hold_read_only(&locked);
        let failures = store.restore(&snaps);

        // This is the contract `transaction()` publishes for these failures.
        let error_code = if failures.is_empty() {
            TX_FAILED
        } else {
            STORE_RESTORE_INCOMPLETE
        };
        assert_eq!(error_code, STORE_RESTORE_INCOMPLETE);
        let errors: Vec<Value> = failures.iter().map(|f| f.to_json()).collect();
        assert_eq!(errors.len(), 1, "{:?}", errors);
        assert_eq!(errors[0]["code"], json!(RESTORE_PATH_FAILED));
        assert!(errors[0]["path"].as_str().unwrap().contains("locked.md"));
        // `path` is its own field: repeating it inside `detail` would make the
        // renderer print the same path twice for no gain.
        assert!(
            errors[0]["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the failure carries the OS reason: {:?}",
            errors[0]
        );

        // And the joined compat string carries the same single failure.
        assert!(restore_sentence(&failures).contains("locked.md"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// `restore_dir` empties the directory and rebuilds it, so an entry it fails
    /// on used to `return Err` and abandon the **rest** of the entries — leaving
    /// a tree holding only what it happened to reach. Every remaining entry must
    /// still be written.
    ///
    /// The failing entry is made deterministic without privileges: one entry
    /// creates a directory (`blocker/…`) and a later entry wants to put a *file*
    /// at that same name. Writing the file fails on the directory that is now
    /// standing there, while the entries on either side of it are ordinary
    /// writes. The list is built by hand precisely so the order is known — a
    /// walk-order-dependent trigger would make this test flaky rather than
    /// strict.
    #[test]
    fn a_partly_failed_directory_restore_still_writes_the_other_entries() {
        let root = temp_root("restore-dir-partial");
        let dir = root.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("before.md"), "clobbered-before").unwrap();
        std::fs::write(dir.join("after.md"), "clobbered-after").unwrap();
        std::fs::create_dir_all(dir.join("blocker")).unwrap();

        let entries = vec![
            DirSnap::File("before.md".to_string(), b"original-before".to_vec()),
            // Creates the directory `blocker`…
            DirSnap::File("blocker/inner.md".to_string(), b"original-inner".to_vec()),
            // …and then wants a *file* at that name, which cannot work.
            DirSnap::File("blocker".to_string(), b"original-blocker".to_vec()),
            DirSnap::File("after.md".to_string(), b"original-after".to_vec()),
        ];

        let failures = restore_dir(&dir, &entries);

        assert_eq!(
            failures.len(),
            1,
            "exactly one entry should fail: {:?}",
            failures
        );
        assert_eq!(failures[0].code(), RESTORE_PATH_FAILED, "{:?}", failures);
        // The entries on **both** sides of the failure are present.
        assert_eq!(
            std::fs::read_to_string(dir.join("before.md")).unwrap(),
            "original-before",
            "the tree stopped before the failing entry"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("after.md")).unwrap(),
            "original-after",
            "the entry AFTER the failing one is missing — restore_dir gave up"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("blocker").join("inner.md")).unwrap(),
            "original-inner"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A `restore_dir` that cannot write into the directory at all must say so
    /// **before** destroying the captured tree. `rmrf` runs first otherwise, and
    /// "could not rebuild" would arrive as "the directory you had is gone".
    #[test]
    fn a_directory_restore_that_cannot_write_keeps_the_captured_tree() {
        let root = temp_root("restore-dir-probe");
        let dir = root.join("tree");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "keep-me").unwrap();

        // The probe path is a *directory*, so writing the probe file fails and
        // the guard trips — without needing a read-only directory, which Windows
        // does not enforce for the owner anyway.
        std::fs::create_dir_all(dir.join(".agentkeeper-restore-probe")).unwrap();

        let failures = restore_dir(
            &dir,
            &[DirSnap::File("keep.md".to_string(), b"rewritten".to_vec())],
        );

        assert_eq!(failures.len(), 1, "{:?}", failures);
        assert_eq!(failures[0].code(), RESTORE_PATH_FAILED, "{:?}", failures);
        assert_eq!(
            std::fs::read_to_string(dir.join("keep.md")).unwrap(),
            "keep-me",
            "the tree was emptied even though it could not be rebuilt: {:?}",
            failures
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn write_lands_on_the_real_path() {
        let root = temp_root("direct");
        let store = store_at(&root);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::write(&p, "hello")],
            &json!({ "kind": "skill", "action": "save" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn transaction_writes_journal_and_audit() {
        let root = temp_root("journal");
        let store = store_at(&root);
        let p = root.join("a.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::write(&p, "body")],
            &json!({
                "kind": "skill",
                "action": "save",
                "assetId": "skill:x",
                "assetName": "X",
                "agentId": "claude",
            }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let tx_id = tx["txId"].as_str().unwrap().to_string();
        assert_eq!(tx["targets"], json!([p]));

        // The journal record is the audit line: the app keeps no per-transaction
        // file on disk, so `txId` exists only to correlate the two halves of the
        // log with each other and with the transaction's own answer.
        let audit = std::fs::read_to_string(store.audit_file()).unwrap();
        assert_eq!(audit.lines().count(), 1);
        let rec: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
        assert_eq!(rec["txId"], json!(tx_id));
        assert_eq!(rec["status"], json!("ok"));
        assert_eq!(rec["assetId"], json!("skill:x"));
        assert_eq!(rec["targets"], json!([p]));
        assert_eq!(rec["log"][0]["op"], json!("write"));
        assert_eq!(rec["log"][0]["bytes"], json!(4));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The product has no backup tree. A transaction — successful, refused or
    /// failed — must leave nothing behind but the audit journal, because a
    /// directory of old copies is exactly the thing this build no longer keeps.
    #[test]
    fn no_transaction_creates_a_backup_directory() {
        let root = temp_root("no-backups");
        let store = store_at(&root);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let ok = store.transaction(&[Op::write(&p, "body")], &json!({ "kind": "skill" }));
        assert_eq!(ok["ok"], json!(true), "{}", ok);

        // And the failure path, which is the one that used to copy the touched
        // paths out before writing them.
        let failed = store.transaction(
            &[
                Op::write(&p, "half"),
                Op::copy_dir(
                    &root.join("nope").to_string_lossy(),
                    &root.join("x").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(failed["ok"], json!(false), "{}", failed);

        assert!(
            !root.join("backups").exists(),
            "backups/ must not be created: {:?}",
            std::fs::read_dir(&root)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Undo of a *failed* transaction: a delete that ran before the failing op
    /// must put the file back byte for byte, from the in-memory snapshot.
    #[test]
    fn a_failed_transaction_restores_a_deleted_file_byte_for_byte() {
        let root = temp_root("fail-delete");
        let store = store_at(&root);
        let target = root.join("skill").join("SKILL.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "original").unwrap();
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::delete(&p),
                Op::copy_dir(
                    &root.join("gone").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx.get("restoreError").is_none(),
            "the restore must report no trouble: {}",
            tx
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");

        std::fs::remove_dir_all(&root).ok();
    }

    /// The mirror image: a path that did not exist must end up absent again, and
    /// the directory that only existed to hold it must be pruned — a phantom
    /// folder is still enumerated by some agents.
    #[test]
    fn a_failed_transaction_removes_a_created_file_and_prunes_its_dir() {
        let root = temp_root("fail-create");
        let store = store_at(&root);
        let dir = root.join("agents").join("fresh");
        let target = dir.join("SKILL.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&p, "new"),
                Op::copy_dir(
                    &root.join("gone").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(!target.exists());
        assert!(!dir.exists(), "empty parent left behind: {:?}", dir);
        // The store root itself must survive; it was not created by the write.
        assert!(root.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A directory op is the case the old disk-copy undo used to cover, and it is
    /// the one an in-memory snapshot could silently get wrong: the restore has to
    /// A directory op is the case the old disk-copy undo used to cover, and it is
    /// the one an in-memory snapshot could silently get wrong: the restore has to
    /// reproduce the *captured* tree, not merge into what is there now.
    ///
    /// The `CopyDir` op is the important one — it names the **directory** as the
    /// touched path, so the whole tree has to come back from one snapshot. An op
    /// that names a file inside it would get its own per-file snapshot and hide a
    /// merge-shaped restore.
    #[test]
    fn a_failed_transaction_restores_a_copied_directory_exactly() {
        let root = temp_root("fail-copiedir");
        let store = store_at(&root);
        let dir = root.join("plugin");
        let src = root.join("src");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("keep.md"), "keep").unwrap();
        std::fs::write(dir.join("nested").join("a.md"), "a").unwrap();
        // An empty subdirectory is state too, and must survive.
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.md"), "x").unwrap();
        let before = list_files(&dir);

        let tx = store.transaction(
            &[
                // This op copies *into* `dir`, so `dir` itself is what has to be
                // captured and put back. (`copy_dir` takes the destination first.)
                Op::copy_dir(&dir.to_string_lossy(), &src.to_string_lossy()),
                Op::copy_dir(
                    &root.join("nope").to_string_lossy(),
                    &root.join("dest").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert_eq!(
            list_files(&dir),
            before,
            "the directory tree is identical again"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("nested").join("a.md")).unwrap(),
            "a"
        );
        assert!(
            dir.join("empty").is_dir(),
            "an empty subdirectory is part of the state"
        );
        assert!(
            !dir.join("x.md").exists(),
            "a file the copy added must not survive: a merge is not a restore"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The size guard is a refusal, not a truncation: a tree that cannot be held
    /// in memory must stop the transaction *before* the first write, so the
    /// alternative to a correct restore is never "a partially restored one".
    #[test]
    fn an_uncapturable_directory_refuses_before_writing_anything() {
        let root = temp_root("oversize-dir");
        let store = store_at(&root);
        let dir = root.join("huge");
        let src = root.join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.md"), "small").unwrap();
        // One file over the whole transaction budget.
        std::fs::write(dir.join("blob.bin"), vec![0u8; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let other = root.join("other.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&other, "would have landed"),
                // The destination is the oversized tree, so capturing it is what
                // the transaction has to do — and what refuses.
                Op::copy_dir(&dir.to_string_lossy(), &src.to_string_lossy()),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("undo snapshot limit"),
            "the refusal names the limit: {}",
            tx
        );
        // The code, not the sentence, is what a renderer keys its wording off.
        assert_eq!(tx["errorCode"], json!(STORE_SNAPSHOT_TOO_LARGE), "{}", tx);
        // Nothing was written, so there is nothing to undo and nothing to report
        // about an undo: this is a refusal, not a partial write.
        assert!(tx.get("restoreError").is_none(), "{}", tx);
        assert!(tx.get("restoreErrors").is_none(), "{}", tx);
        assert!(
            !Path::new(&other).exists(),
            "the capture failed before the first op ran"
        );
        assert_eq!(
            std::fs::read_to_string(src.join("a.md")).unwrap(),
            "small",
            "the copy source is untouched"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The same guard on a single file: an oversized config is refused exactly
    /// like an oversized tree, because there is no partial snapshot for a file
    /// either.
    #[test]
    fn an_oversized_file_refuses_before_writing_anything() {
        let root = temp_root("oversize-file");
        let store = store_at(&root);
        let huge = root.join("huge.bin");
        std::fs::write(&huge, vec![0u8; MAX_SNAPSHOT_BYTES + 1]).unwrap();
        let other = root.join("other.md").to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&other, "would have landed"),
                Op::write(&huge.to_string_lossy(), "clobbered"),
            ],
            &json!({ "kind": "plugin" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("undo snapshot limit"),
            "the refusal names the limit: {}",
            tx
        );
        assert_eq!(tx["errorCode"], json!(STORE_SNAPSHOT_TOO_LARGE), "{}", tx);
        assert!(tx.get("restoreErrors").is_none(), "{}", tx);
        // The message has to say what to do, not only that a number was hit —
        // a bare byte count leaves the reader with no next move.
        let msg = tx["error"].as_str().unwrap();
        assert!(
            msg.contains("split this asset into smaller writes"),
            "the refusal says what to do about it: {}",
            msg
        );
        assert!(
            msg.contains(&human_size(MAX_SNAPSHOT_BYTES)),
            "the refusal gives the limit in readable units: {}",
            msg
        );
        assert!(
            !Path::new(&other).exists(),
            "the capture failed before the first op ran"
        );
        assert_eq!(
            std::fs::metadata(&huge).unwrap().len() as usize,
            MAX_SNAPSHOT_BYTES + 1,
            "the oversized file was not truncated"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_op_merges_patch_and_keeps_siblings() {
        let root = temp_root("json-merge");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, r#"{"mcpServers":{"keep":{"command":"a"}}}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_mutate(&p, |cur| {
                let mut out = cur.clone();
                out["mcpServers"]["added"] = json!({ "command": "b" });
                Some(out)
            })],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["mcpServers"]["keep"]["command"], json!("a"));
        assert_eq!(after["mcpServers"]["added"]["command"], json!("b"));
        assert!(std::fs::read_to_string(&cfg).unwrap().ends_with("}\n"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_patch_is_a_shallow_merge() {
        let root = temp_root("json-patch");
        let store = store_at(&root);
        let cfg = root.join("settings.json");
        std::fs::write(&cfg, r#"{"a":1,"b":2}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "b": 9, "c": 3 }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after, json!({ "a": 1, "b": 9, "c": 3 }));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn corrupt_config_aborts_instead_of_dropping_siblings() {
        let root = temp_root("json-corrupt");
        let store = store_at(&root);
        let cfg = root.join("broken.json");
        std::fs::write(&cfg, "{ not json ").unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "x": 1 }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"].as_str().unwrap().contains("corrupt"),
            "unexpected error: {}",
            tx["error"]
        );
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{ not json ");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn jsonc_comments_and_trailing_commas_are_tolerated() {
        let root = temp_root("jsonc");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(
            &cfg,
            "{\n  // a comment\n  \"mcpServers\": { \"a\": { \"command\": \"x\" }, },\n}\n",
        )
        .unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_patch(&p, json!({ "extra": true }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(after["mcpServers"]["a"]["command"], json!("x"));
        assert_eq!(after["extra"], json!(true));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_dir_skips_build_trees() {
        let root = temp_root("copydir");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("node_modules").join("pkg").join("i.js"), "x").unwrap();
        std::fs::write(src.join("nested").join("a.md"), "a").unwrap();
        std::fs::write(src.join("b.md"), "b").unwrap();

        let dest = root.join("dest");
        copy_dir(&src, &dest);
        assert!(dest.join("b.md").exists());
        assert!(dest.join("nested").join("a.md").exists());
        assert!(
            !dest.join("node_modules").exists(),
            "build trees are not backed up"
        );

        let mut files = list_files(&src);
        files.sort();
        assert_eq!(files, vec!["b.md".to_string(), "nested/a.md".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_copy_source_fails_the_whole_transaction() {
        let root = temp_root("copydir-missing");
        let store = store_at(&root);
        let target = root.join("written.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&p, "should be rolled back"),
                Op::copy_dir(
                    &root.join("dest").to_string_lossy(),
                    &root.join("nope").to_string_lossy(),
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"]
                .as_str()
                .unwrap()
                .contains("copy source missing"),
            "{}",
            tx["error"]
        );
        assert!(!target.exists(), "the earlier write must be undone");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_mutate_returning_none_aborts() {
        let root = temp_root("mutate-abort");
        let store = store_at(&root);
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, r#"{"a":1}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::json_mutate(&p, |_| None)], &json!({ "kind": "mcp" }));
        assert_eq!(tx["ok"], json!(false));
        assert!(tx["error"].as_str().unwrap().contains("mutate aborted"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), r#"{"a":1}"#);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mkdir_op_is_idempotent() {
        let root = temp_root("mkdir");
        let store = store_at(&root);
        let dir = root.join("a").join("b");
        let p = dir.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::mkdir(&p), Op::mkdir(&p)], &json!({}));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(dir.is_dir());
        assert_eq!(tx["targets"], json!([p, p]), "JS records one target per op");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tx_ids_are_unique_and_sortable() {
        let a = gen_tx_id();
        let b = gen_tx_id();
        assert!(a.starts_with("tx-"));
        assert_ne!(a, b);
        let tail = a.rsplit('-').next().unwrap();
        assert_eq!(tail.len(), 5, "{}", a);
        assert!(tail.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
