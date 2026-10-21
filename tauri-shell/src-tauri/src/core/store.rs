//! `core/store.js` — the write path.
//!
//! Every mutation goes through [`Store::transaction`], which guarantees, in
//! this order:
//!
//! 1. validate — a malformed op is rejected before a single byte moves
//! 2. backup   — the current state of every touched path is copied into
//!               `<store>/backups/<txId>/`
//! 3. atomic write — `<path>.agentkeeper-tmp-<pid>-<ms>` then `rename(2)`
//! 4. journal  — `tx.json` + one `audit.jsonl` line describing before/after
//! 5. rollback — [`Store::rollback`] restores exactly the backup state
//!
//! The shadow layer sits on top: while the sandbox is on, [`Store::real`]
//! reroutes every absolute path into `<store>/sandbox/` (keeping the drive
//! letter so the agent file layout is reproduced faithfully), and each write is
//! recorded in `pending.json` so the UI can offer Apply or Discard.
//!
//! **Any caller — UI, MCP, CLI — must go through `transaction()`.** Nothing may
//! write an agent file directly; that is the only reason "one click rollback"
//! is a true statement.
//!
//! ## Deliberate differences from the JavaScript
//!
//! * `unshadow()` handles both separators. The JS strips a leading `\`/`/` and
//!   then matches `^drive\/` — on Windows, where `real()` produces `\` after
//!   `path.join`, that match never fires, so a sandboxed rollback resolves to
//!   the relative path `drive\C\Users\…` (i.e. writes into the process CWD).
//!   This port reconstructs the drive letter correctly.
//! * Snapshots no longer compute `hashDir()`/`hash()` for the record. Those two
//!   fields were written into an in-memory array that is only ever read for
//!   `path`/`existed`/`type` — `tx.json` stores `targets` alone — so hashing
//!   doubled the I/O of every backup for a value nobody could observe.
//! * JSONC stripping is shared with the scanner ([`super::jsonc`]) instead of a
//!   second, subtly different regex chain living in this module.

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::jsonc::strip_json_comments;
use super::model::iso_from_millis;
use super::settings;

/// Directories that are never copied into a backup, never hashed, and never
/// descended into when measuring a shadow tree. Copying `node_modules` into a
/// backup turns a 40 ms save into a 4 GB one.
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

fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    iso_from_millis(now_millis())
}

/// Node's `path.join` emits `\` on Windows; store paths are compared and
/// journalled as strings, so the separator has to match what the JS produced.
pub fn native_str(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s
    }
}

pub fn to_native_sep(s: &str) -> String {
    if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

/// `norm.replace(/^([A-Za-z]):/, 'drive/$1').replace(/^\/+/g, '')`
///
/// The drive letter is kept as a *directory* name so the shadow tree stays on
/// one volume (`C:` and `D:` never collide).
fn shadow_key(norm_slashed: &str) -> String {
    let b = norm_slashed.as_bytes();
    // `drive/C` + the rest *after* the colon — `C:` is not a legal path
    // component on Windows, so the colon must go with it.
    let mut s = if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        format!("drive/{}{}", &norm_slashed[..1], &norm_slashed[2..])
    } else {
        norm_slashed.to_string()
    };
    while s.starts_with('/') {
        s.remove(0);
    }
    s
}

fn shadow_join(base: &str, original: &str) -> String {
    let norm = original.replace('\\', "/");
    let key = shadow_key(&norm);
    let joined = Path::new(base).join(to_native_sep(&key));
    native_str(&joined)
}

/* ------------------------------------------------------------------ */
/* JSON helpers                                                        */
/* ------------------------------------------------------------------ */

/// `readJsonOrNull()` — `Some` only if the file exists *and* parses.
///
/// The distinction matters: an empty file parses to `null` in JS too, and the
/// `json` op aborts rather than overwriting a config it could not read.
pub fn read_json_or_null(path: &str) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let stripped = strip_json_comments(&text);
    serde_json::from_str(&stripped).ok()
}

/// `readJsonSafe()` — a parse failure looks like an empty object.
pub fn read_json_safe(path: &str) -> Value {
    read_json_or_null(path).unwrap_or_else(|| json!({}))
}

fn is_plain_obj(v: &Value) -> bool {
    v.is_object()
}

/// `JSON.stringify(a) === JSON.stringify(b)`, with JS's number folding
/// (`1.0` and `1` serialize identically).
fn json_canon(v: &Value) -> String {
    fn num(n: &serde_json::Number) -> String {
        if let Some(i) = n.as_i64() {
            return format!("i{}", i);
        }
        if let Some(u) = n.as_u64() {
            return format!("i{}", u);
        }
        let f = n.as_f64().unwrap_or(0.0);
        if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
            format!("i{}", f as i64)
        } else {
            format!("f{}", f)
        }
    }
    match v {
        Value::Null => "n".to_string(),
        Value::Bool(b) => {
            if *b {
                "t".to_string()
            } else {
                "f".to_string()
            }
        }
        Value::Number(n) => num(n),
        Value::String(s) => format!("s{}:{}", s.chars().count(), s),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(json_canon).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("k{}:{}{}", k.chars().count(), k, json_canon(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

fn json_equal_opt(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => json_canon(x) == json_canon(y),
        _ => false,
    }
}

/// `threeWayMerge()` — keys we did not touch keep theirs, so an external edit
/// made between staging and applying survives; keys we touched keep ours.
fn three_way_merge(base: &Value, ours: &Value, theirs: &Value) -> Value {
    if !is_plain_obj(base) || !is_plain_obj(ours) || !is_plain_obj(theirs) {
        return ours.clone();
    }
    let (Value::Object(b), Value::Object(o), Value::Object(t)) = (base, ours, theirs) else {
        return ours.clone();
    };

    // `new Set([...keys(base), ...keys(ours), ...keys(theirs)])` — insertion
    // order is base, then the keys only ours has, then the keys only theirs has.
    let mut keys: Vec<&String> = Vec::new();
    let mut seen: HashSet<&String> = HashSet::new();
    for m in [b, o, t] {
        for k in m.keys() {
            if seen.insert(k) {
                keys.push(k);
            }
        }
    }

    let mut out = Map::new();
    for k in keys {
        let bv = b.get(k);
        let ov = o.get(k);
        let tv = t.get(k);
        if !json_equal_opt(ov, bv) {
            let merged = if let (Some(ov), Some(tv), Some(bv)) = (ov, tv, bv) {
                if is_plain_obj(ov) && is_plain_obj(tv) && is_plain_obj(bv) {
                    three_way_merge(bv, ov, tv)
                } else {
                    ov.clone()
                }
            } else {
                match ov {
                    Some(v) => v.clone(),
                    // `out[k] = undefined` — the key disappears on stringify.
                    None => continue,
                }
            };
            out.insert(k.clone(), merged);
        } else if let Some(v) = tv {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// `{ ...cur, ...patch }` — JS object spread, which also turns an array (or a
/// string) into an index-keyed object rather than merging into it.
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
                let is_dir = std::fs::metadata(&from).map(|m| m.is_dir()).unwrap_or(false);
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
/// `readAll()` in the JS wrapper returns `''` for a missing or unreadable file
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
    JsonMerge,
    Mkdir,
}

impl OpKind {
    fn name(self) -> &'static str {
        match self {
            OpKind::Write => "write",
            OpKind::CopyDir => "copyDir",
            OpKind::Json => "json",
            OpKind::Delete => "delete",
            OpKind::JsonMerge => "jsonmerge",
            OpKind::Mkdir => "mkdir",
        }
    }
}

/// One mutation. Built through the constructors so a missing field is a
/// compile error rather than a runtime `op[i] is missing a "from"`.
pub struct Op {
    pub kind: OpKind,
    /// The **real** path. Sandbox rerouting happens inside `transaction()`.
    pub path: String,
    pub from: Option<String>,
    /// `copyDir` source is read from the real tree even when the write is
    /// sandboxed — otherwise a source that exists only for real looks missing.
    pub from_real: bool,
    pub content: Option<String>,
    /// `json` op: shallow merge source.
    pub patch: Option<Value>,
    /// `json` op: full control. Returning `None` aborts the transaction.
    pub mutate: Option<Box<dyn Fn(Value) -> Option<Value>>>,
    /// `jsonmerge` op: the seed snapshot taken when the change was staged.
    pub base: String,
}

impl Op {
    fn new(kind: OpKind, path: &str) -> Op {
        Op {
            kind,
            path: path.to_string(),
            from: None,
            from_real: false,
            content: None,
            patch: None,
            mutate: None,
            base: String::new(),
        }
    }

    pub fn write(path: &str, content: &str) -> Op {
        let mut op = Op::new(OpKind::Write, path);
        op.content = Some(content.to_string());
        op
    }

    /// `write` whose payload is read from `from` (the shadow tree, when
    /// applying a staged change).
    pub fn write_from(path: &str, from: &str) -> Op {
        let mut op = Op::new(OpKind::Write, path);
        op.from = Some(from.to_string());
        op
    }

    pub fn copy_dir(path: &str, from: &str, from_real: bool) -> Op {
        let mut op = Op::new(OpKind::CopyDir, path);
        op.from = Some(from.to_string());
        op.from_real = from_real;
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

    pub fn json_merge(path: &str, from: &str, base: &str) -> Op {
        let mut op = Op::new(OpKind::JsonMerge, path);
        op.from = Some(from.to_string());
        op.base = base.to_string();
        op
    }
}

/// What a transaction touched, so a failure can put it back.
struct Snapshot {
    path: String,
    existed: bool,
    backup: Option<String>,
    kind: Option<&'static str>,
    /// `rollback()` set to true: `path` is not the path the transaction
    /// wrote. The journal stores shadow paths; `rollback()` maps them back
    /// through `unshadow()` so the undo cannot land in the process CWD. The
    /// `existed` flag, however, still describes the *shadow* copy.
    remapped: bool,
}

/* ------------------------------------------------------------------ */
/* Store                                                               */
/* ------------------------------------------------------------------ */

pub struct Store {
    root: PathBuf,
    root_str: String,
    sandbox_root: String,
    backup_root: String,
    sandbox: bool,
}

impl Store {
    /// `new Store(root, {sandbox})` — creates the tree if it is not there yet.
    pub fn new(root: &str, sandbox: bool) -> Store {
        let root_path = PathBuf::from(root);
        let _ = settings::ensure_store(&native_str(&root_path), sandbox);
        Store {
            root_str: native_str(&root_path),
            sandbox_root: native_str(&root_path.join("sandbox")),
            backup_root: native_str(&root_path.join("backups")),
            root: root_path,
            sandbox,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_str(&self) -> &str {
        &self.root_str
    }

    pub fn sandbox(&self) -> bool {
        self.sandbox
    }

    /// `setSandbox(on)` — session-scoped, mirrored into `meta.json` so an
    /// external inspector still sees a truthful value.
    pub fn set_sandbox(&mut self, on: bool) {
        self.sandbox = on;
        let _ = std::fs::create_dir_all(&self.sandbox_root);
        settings::set_sandbox_meta(&self.root_str, on);
    }

    pub fn audit_file(&self) -> String {
        native_str(&self.root.join("audit.jsonl"))
    }

    pub fn pending_file(&self) -> String {
        native_str(&self.root.join("pending.json"))
    }

    /* -------------------------------------------------------------- */
    /* path routing                                                    */
    /* -------------------------------------------------------------- */

    /// `real()` — the path actually written.
    pub fn real(&self, p: &str) -> String {
        if !self.sandbox {
            return p.to_string();
        }
        if p.starts_with(&self.sandbox_root) {
            return p.to_string();
        }
        shadow_join(&self.sandbox_root, p)
    }

    /// `unshadow()` — recover the real path a shadow path stands for.
    pub fn unshadow(&self, shadow: &str) -> String {
        if !shadow.starts_with(&self.sandbox_root) {
            return shadow.to_string();
        }
        let rest = &shadow[self.sandbox_root.len()..];
        let rel = rest.trim_start_matches(['\\', '/']);
        let rel = rel.replace('\\', "/");
        // `drive/<letter>` → `<letter>:` (see the module note: the JS misses
        // this on Windows because it strips separators before the match).
        let rel = match rel.strip_prefix("drive/") {
            Some(tail) if tail.len() >= 2 && tail.as_bytes()[0].is_ascii_alphabetic() => {
                format!("{}:{}", &tail[..1], &tail[1..])
            }
            _ => rel,
        };
        to_native_sep(&rel)
    }

    /* -------------------------------------------------------------- */
    /* shadow layer                                                    */
    /* -------------------------------------------------------------- */

    /// `readPending()` — the staged changes, or an empty list.
    pub fn read_pending(&self) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(self.pending_file()) else {
            return Vec::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => v
                .get("changes")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn write_pending(&self, changes: &[Value]) -> std::io::Result<()> {
        let file = self.pending_file();
        let tmp = format!("{}.tmp", file);
        let body = pretty(&json!({
            "version": 1,
            "updatedAt": now_iso(),
            "changes": changes,
        }));
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &file)
    }

    /// `_recordPending()` — one entry per written path, keyed by shadow path.
    ///
    /// A path whose shadow no longer exists (a delete) simply drops its entry:
    /// there is nothing to replay, and the delete already happened in the real
    /// tree's future.
    fn record_pending(&self, target: &str, shadow: &str, op: &str, base: Option<String>) {
        if !self.sandbox {
            return;
        }
        let mut changes: Vec<Value> = self
            .read_pending()
            .into_iter()
            .filter(|c| c.get("shadow").and_then(|s| s.as_str()) != Some(shadow))
            .collect();

        let st = std::fs::symlink_metadata(shadow);
        let Ok(st) = st else {
            let _ = self.write_pending(&changes);
            return;
        };

        let mut rec = Map::new();
        rec.insert(
            "op".to_string(),
            Value::String(
                match op {
                    "delete" => "delete",
                    "json" => "json",
                    _ => "write",
                }
                .to_string(),
            ),
        );
        rec.insert("shadow".to_string(), Value::String(shadow.to_string()));
        rec.insert("real".to_string(), Value::String(target.to_string()));
        if op == "json" {
            rec.insert("base".to_string(), Value::String(base.unwrap_or_default()));
        }
        let bytes = if st.is_dir() {
            list_files(Path::new(shadow)).len() as u64
        } else {
            st.len()
        };
        rec.insert("bytes".to_string(), json!(bytes));
        rec.insert("at".to_string(), Value::String(now_iso()));

        changes.push(Value::Object(rec));
        let _ = self.write_pending(&changes);
    }

    /// `applyPending()` — replay every staged change onto the real paths.
    ///
    /// Runs as ONE transaction, so a partial apply cannot happen: either all of
    /// the shadow tree lands or none of it does and `pending.json` survives.
    pub fn apply_pending(&mut self) -> Value {
        let changes = self.read_pending();
        if changes.is_empty() {
            return json!({ "ok": true, "applied": 0, "message": "no pending changes" });
        }

        let mut ops: Vec<Op> = Vec::with_capacity(changes.len());
        for c in &changes {
            let shadow = c.get("shadow").and_then(|v| v.as_str()).unwrap_or_default();
            let real = c.get("real").and_then(|v| v.as_str()).unwrap_or_default();
            match c.get("op").and_then(|v| v.as_str()).unwrap_or("write") {
                "json" => ops.push(Op::json_merge(
                    real,
                    shadow,
                    c.get("base").and_then(|v| v.as_str()).unwrap_or(""),
                )),
                "write" => {
                    // `_recordPending()` files a staged directory under the
                    // very same `write` op as a staged file (store.js:230) —
                    // only `bytes` hints at the difference. Replaying the
                    // directory as a file read is what the JS does, and it is
                    // why a staged `copyDir` can never be applied there:
                    // `readFileSync` on a directory throws EISDIR
                    // (store.js:382) and the whole apply rolls back. The
                    // shadow tree still knows which it is, so replay a
                    // directory as a copy of the staged tree instead.
                    if Path::new(shadow).is_dir() {
                        ops.push(Op::copy_dir(real, shadow, true));
                    } else {
                        ops.push(Op::write_from(real, shadow));
                    }
                }
                _ => ops.push(Op::delete(real)),
            }
        }

        let was = self.sandbox;
        self.sandbox = false;
        let mut tx = self.transaction(
            &ops,
            &json!({
                "kind": "apply",
                "action": "apply-shadow",
                "assetName": format!("{} pending change(s)", changes.len()),
            }),
        );
        self.sandbox = was;

        if tx.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let _ = self.write_pending(&[]);
            settings::audit(
                &self.root_str,
                &json!({
                    "action": "shadow-clear",
                    "status": "ok",
                    "kind": "apply",
                    "count": changes.len(),
                }),
            );
        } else {
            // The replay failed. `transaction()` ran with `sandbox = false`
            // (set just above, so the staged tree is replayed onto real paths),
            // which means its own failure path deliberately did NOT reclaim
            // anything — that guard exists to protect a sandboxed caller's
            // staged work. Here the staged work is exactly what just failed,
            // so drop every shadow this apply consumed plus its pending entry.
            // Without this the entry survives the rollback and the UI keeps
            // offering "Apply" for a change that can never succeed.
            let shadows: Vec<String> = changes
                .iter()
                .filter_map(|c| c.get("shadow").and_then(|v| v.as_str()).map(str::to_string))
                .collect();
            self.reclaim_staged(&shadows);
        }
        if let Value::Object(m) = &mut tx {
            m.insert("applied".to_string(), json!(changes.len()));
        }
        tx
    }

    /// `discardPending()` — drop the mirror tree without touching reality.
    pub fn discard_pending(&self) -> Value {
        let changes = self.read_pending();
        for c in &changes {
            let Some(shadow) = c.get("shadow").and_then(|v| v.as_str()) else {
                continue;
            };
            let p = Path::new(shadow);
            let Ok(st) = std::fs::symlink_metadata(p) else {
                continue;
            };
            if st.is_dir() {
                rmrf(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
        let _ = self.write_pending(&[]);
        settings::audit(
            &self.root_str,
            &json!({
                "action": "shadow-discard",
                "status": "ok",
                "kind": "apply",
                "count": changes.len(),
            }),
        );
        json!({ "ok": true, "discarded": changes.len() })
    }

    /* -------------------------------------------------------------- */
    /* backup / rollback                                               */
    /* -------------------------------------------------------------- */

    fn tx_dir(&self, tx_id: &str) -> PathBuf {
        let dir = Path::new(&self.backup_root).join(tx_id);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Snapshot one path (file or directory). Existence is recorded so a
    /// delete is reversible.
    fn snapshot(&self, original: &str, tx_dir: &Path) -> Snapshot {
        let dest = shadow_join(&native_str(tx_dir), original);
        let existed = Path::new(original).exists();
        if !existed {
            return Snapshot {
                path: original.to_string(),
                existed: false,
                backup: None,
                kind: None,
                remapped: false,
            };
        }
        let dest_path = PathBuf::from(&dest);
        let kind = match std::fs::metadata(original) {
            Ok(m) if m.is_dir() => {
                copy_dir(Path::new(original), &dest_path);
                "dir"
            }
            Ok(_) => {
                if let Some(parent) = dest_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::copy(original, &dest_path);
                "file"
            }
            Err(_) => "file",
        };
        Snapshot {
            path: original.to_string(),
            existed: true,
            backup: Some(dest),
            kind: Some(kind),
            remapped: false,
        }
    }

    /// `transaction(ops, meta)` — the only way a file may change.
    ///
    /// `meta` keys mirror the JS: `txId`, `kind`, `action`, `agentId`,
    /// `agentLabel`, `projectId`, `projectName`, `assetId`, `assetName`.
    pub fn transaction(&self, ops: &[Op], meta: &Value) -> Value {
        for (i, op) in ops.iter().enumerate() {
            if matches!(op.kind, OpKind::CopyDir | OpKind::JsonMerge) && op.from.is_none() {
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
        let tx_dir = self.tx_dir(&tx_id);

        let mut log: Vec<Value> = Vec::new();
        let mut touched: Vec<Snapshot> = Vec::new();

        let outcome: Result<(), String> = (|| {
            // 1. back up everything the transaction will touch, before any of
            //    it moves. A failure here means nothing was written at all.
            for op in ops {
                let real = self.real(&op.path);
                let snap = self.snapshot(&real, &tx_dir);
                touched.push(snap);
            }

            // 2. apply.
            for op in ops {
                let real = self.real(&op.path);
                match op.kind {
                    OpKind::Write => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let content = match op.from.as_deref() {
                            Some(from) if Path::new(from).exists() => {
                                // JS reads an already-resolved source with
                                // `fs.readFileSync(from, 'utf8')`
                                // (store.js:381-383) and lets the throw fail
                                // the whole transaction. The failure is worth
                                // keeping — swallowing it is what used to
                                // write an empty file over the target and
                                // report success — but Node's EISDIR wording
                                // is not: `applyPending` now replays staged
                                // directories as `copyDir`, so this is only
                                // reachable by a caller that named a
                                // directory as a file source.
                                match std::fs::read_to_string(from) {
                                    Ok(c) => c,
                                    Err(_) if Path::new(from).is_dir() => {
                                        return Err(format!(
                                            "write source is a directory, not a file: {}",
                                            from
                                        ));
                                    }
                                    Err(e) => {
                                        return Err(format!("cannot read write source {}: {}", from, e))
                                    }
                                }
                            }
                            _ => match &op.content {
                                Some(c) => c.clone(),
                                None => format!("{}\n", pretty(&Value::Null)),
                            },
                        };
                        if let Err(e) = settings::atomic_write(Path::new(&real), &content) {
                            return Err(format!("write failed for {}: {}", real, e));
                        }
                        log.push(json!({
                            "op": "write",
                            "path": real,
                            "bytes": content.len(),
                        }));
                        self.record_pending(&op.path, &real, "write", None);
                    }

                    OpKind::CopyDir => {
                        let from = op.from.as_deref().unwrap_or_default();
                        let src = if op.from_real {
                            from.to_string()
                        } else {
                            self.real(from)
                        };
                        if !Path::new(&src).exists() {
                            return Err(format!("copy source missing: {}", src));
                        }
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if real != src {
                            copy_dir(Path::new(&src), Path::new(&real));
                        }
                        log.push(json!({ "op": "copyDir", "path": real, "from": src }));
                        self.record_pending(&op.path, &real, "write", None);
                    }

                    OpKind::Json => {
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        // In sandbox mode the shadow tree starts empty; seed the
                        // mutation from the real file so sibling servers and
                        // sibling keys survive.
                        let base = if self.sandbox && op.path != real {
                            op.path.clone()
                        } else {
                            real.clone()
                        };
                        let mut cur = if Path::new(&real).exists() {
                            match read_json_or_null(&real) {
                                Some(v) => v,
                                None => {
                                    return Err(format!(
                                        "config file is corrupt, refusing to overwrite: {}",
                                        real
                                    ))
                                }
                            }
                        } else {
                            json!({})
                        };

                        if self.sandbox && object_keys_len(&cur) == 0 && Path::new(&base).exists() {
                            match read_json_or_null(&base) {
                                Some(v) => cur = v,
                                None => {
                                    return Err(format!(
                                        "config file is corrupt, refusing to overwrite: {}",
                                        base
                                    ))
                                }
                            }
                        }

                        let next = match &op.mutate {
                            Some(f) => match f(cur.clone()) {
                                Some(v) => v,
                                None => return Err(format!("json mutate aborted for {}", real)),
                            },
                            None => spread_merge(&cur, op.patch.as_ref().unwrap_or(&json!({}))),
                        };
                        let body = format!("{}\n", pretty(&next));
                        if let Err(e) = settings::atomic_write(Path::new(&real), &body) {
                            return Err(format!("write failed for {}: {}", real, e));
                        }
                        log.push(json!({
                            "op": "json",
                            "path": real,
                            "keys": object_keys_len(&next),
                        }));
                        // Record the seed snapshot so applyPending can re-merge
                        // if the real file changed between stage and apply.
                        self.record_pending(
                            &op.path,
                            &real,
                            "json",
                            Some(format!("{}\n", pretty(&cur))),
                        );
                    }

                    OpKind::Delete => {
                        let p = Path::new(&real);
                        if p.exists() {
                            if std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) {
                                rmrf(p);
                            } else {
                                let _ = std::fs::remove_file(p);
                            }
                        }
                        log.push(json!({ "op": "delete", "path": real }));
                        self.record_pending(&op.path, &real, "delete", None);
                    }

                    OpKind::JsonMerge => {
                        // Replay a staged json change onto a real file that may
                        // have been edited externally since staging.
                        //   base = seed snapshot at staging time
                        //   ours = the shadow result
                        //   theirs = the current real file
                        if let Some(parent) = Path::new(&real).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let from = op.from.clone().unwrap_or_default();
                        let ours = if Path::new(&from).exists() {
                            read_json_or_null(&from)
                        } else {
                            None
                        };
                        let Some(ours) = ours else {
                            return Err(format!("copy source missing: {}", from));
                        };
                        if !Path::new(&real).exists() {
                            let body = format!("{}\n", pretty(&ours));
                            if let Err(e) = settings::atomic_write(Path::new(&real), &body) {
                                return Err(format!("write failed for {}: {}", real, e));
                            }
                            log.push(json!({ "op": "jsonmerge", "path": real, "mode": "create" }));
                            continue;
                        }
                        let Some(theirs) = read_json_or_null(&real) else {
                            return Err(format!(
                                "config file is corrupt, refusing to overwrite: {}",
                                real
                            ));
                        };
                        let stripped = strip_json_comments(&op.base);
                        let base = serde_json::from_str::<Value>(&stripped).unwrap_or_else(|_| json!({}));
                        let merged = three_way_merge(&base, &ours, &theirs);
                        let body = format!("{}\n", pretty(&merged));
                        if let Err(e) = settings::atomic_write(Path::new(&real), &body) {
                            return Err(format!("write failed for {}: {}", real, e));
                        }
                        log.push(json!({ "op": "jsonmerge", "path": real, "mode": "merge" }));
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
            self.restore(&touched);
            // A failed *sandboxed* replay must also drop the staged copy and
            // its `pending.json` entry. `restore()` only undoes the writes this
            // transaction managed to make; it knows nothing about the shadow
            // tree, so leaving the entry behind kept the UI offering "Apply"
            // for a change that had already been rolled back — the counter
            // stayed at 1 forever and every retry failed the same way.
            //
            // Guarded by `self.sandbox`: a non-sandboxed transaction writes to
            // the real tree and has no pending entry to begin with, so
            // reclaiming there would be a no-op sweep at best and would touch
            // an unrelated caller's staged work at worst. `apply_pending()`
            // clears the flag before replaying (`:824-834`), so its own
            // failure path never reaches this branch.
            if self.sandbox {
                let journalled: Vec<String> = touched.iter().map(|t| t.path.clone()).collect();
                self.reclaim_staged(&journalled);
            }
            settings::audit(
                &self.root_str,
                &json!({
                    "txId": tx_id,
                    "status": "failed",
                    "error": err,
                    "ops": ops
                        .iter()
                        .map(|o| json!({ "op": o.kind.name(), "path": self.real(&o.path) }))
                        .collect::<Vec<Value>>(),
                    "kind": meta.get("kind").cloned().unwrap_or(Value::Null),
                    "agentId": meta.get("agentId").cloned().unwrap_or(Value::Null),
                    "projectId": meta.get("projectId").cloned().unwrap_or(Value::Null),
                    "assetId": meta.get("assetId").cloned().unwrap_or(Value::Null),
                }),
            );
            return json!({ "ok": false, "txId": tx_id, "error": err });
        }

        let ms = now_millis() - t0;
        let targets: Vec<Value> = touched
            .iter()
            .map(|t| Value::String(t.path.clone()))
            .collect();
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
            "sandbox": self.sandbox,
            "targets": targets,
            "log": log,
        });
        let _ = std::fs::write(
            tx_dir.join("tx.json"),
            pretty(&rec),
        );
        settings::audit(&self.root_str, &rec);
        json!({ "ok": true, "txId": tx_id, "ms": ms, "targets": targets })
    }

    /// `_restore()` — put a snapshot list back, best effort per item.
    fn restore(&self, snaps: &[Snapshot]) {
        for s in snaps {
            let target = Path::new(&s.path);
            if s.existed {
                match (s.kind, s.backup.as_deref()) {
                    (Some("dir"), Some(backup)) => {
                        if target.exists() {
                            rmrf(target);
                        }
                        copy_dir(Path::new(backup), target);
                    }
                    (_, Some(backup)) => {
                        if let Some(parent) = target.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::copy(backup, target);
                    }
                    _ => {}
                }
            } else if target.exists() {
                // `existed == false` means "the path the transaction wrote was
                // absent before it ran". When `rollback()` remapped a shadow
                // path onto the real tree, that claim is about the staged
                // copy — the real file was never created by this transaction,
                // and removing it would destroy a file the store's own
                // guarantee ("restore every file touched by the transaction to
                // its state before the change") says must survive. The staged
                // copy is the human-approval staging area: only
                // `discard_pending()` clears it, never a rollback.
                if s.remapped {
                    continue;
                }
                if std::fs::symlink_metadata(target).map(|m| m.is_dir()).unwrap_or(false) {
                    rmrf(target);
                } else {
                    let _ = std::fs::remove_file(target);
                }
                // Deleting the last file of a freshly created directory (a new
                // skill folder) would otherwise leave a phantom entry that some
                // agents still enumerate.
                if let Some(parent) = target.parent() {
                    prune_empty_parents(&native_str(parent), &s.path);
                }
            }
        }
    }

    /// Drop the staged copy of `targets` and remove its `pending.json` entries.
    ///
    /// `targets` are the **journalled** paths — under the sandbox that is the
    /// shadow path, which is exactly the key `record_pending()` files `c.shadow`
    /// under, so the match is per path and never a sweep of the whole
    /// `sandbox/` subtree: `restore()` can undo several paths at once, and
    /// unrelated work may be waiting for approval in the same mirror.
    fn reclaim_staged(&self, targets: &[String]) {
        let all = self.read_pending();
        let staged: Vec<String> = all
            .iter()
            .filter(|c| {
                c.get("shadow")
                    .and_then(|v| v.as_str())
                    .map_or(false, |s| targets.iter().any(|t| t.as_str() == s))
            })
            .filter_map(|c| c.get("shadow").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if staged.is_empty() {
            return;
        }
        for shadow in &staged {
            let p = Path::new(shadow);
            let Ok(st) = std::fs::symlink_metadata(p) else {
                continue;
            };
            // Same two cases `discard_pending()` handles, and the same reason
            // not to prune the empty parents it leaves behind: the skeleton
            // under `sandbox/` is not what the UI reads, and pruning it would
            // reach towards the sandbox root for no observable gain.
            if st.is_dir() {
                rmrf(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
        let kept: Vec<Value> = all
            .into_iter()
            .filter(|c| {
                c.get("shadow")
                    .and_then(|v| v.as_str())
                    .map_or(true, |s| !targets.iter().any(|t| t.as_str() == s))
            })
            .collect();
        let _ = self.write_pending(&kept);
    }

    /// `rollback(txId)` — restore exactly the state the backup captured.
    pub fn rollback(&self, tx_id: &str) -> Value {
        let tx_dir = Path::new(&self.backup_root).join(tx_id);
        let tx_file = tx_dir.join("tx.json");
        if !tx_file.exists() {
            return json!({ "ok": false, "error": format!("transaction {} not found", tx_id) });
        }
        let Ok(body) = std::fs::read_to_string(&tx_file) else {
            return json!({ "ok": false, "error": "corrupt transaction record" });
        };
        let Ok(tx) = serde_json::from_str::<Value>(&body) else {
            return json!({ "ok": false, "error": "corrupt transaction record" });
        };

        // The journal records the path as written. A sandboxed transaction
        // journalled a shadow path; map it back so the undo hits the same tree.
        // Only a path genuinely under the sandbox root counts — a real agent
        // directory that happens to contain a "sandbox" segment must not.
        let sandbox_prefix = format!("{}/", self.sandbox_root.replace('\\', "/"));
        let resolve = |p: &str| -> String {
            let norm = p.replace('\\', "/");
            if norm.starts_with(&sandbox_prefix) || p.starts_with(&self.sandbox_root) {
                self.unshadow(p)
            } else {
                p.to_string()
            }
        };

        let targets: Vec<String> = tx
            .get("targets")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut snaps: Vec<Snapshot> = Vec::with_capacity(targets.len());
        for raw in &targets {
            let p = resolve(raw);
            let remapped = p != *raw;
            let backup = shadow_join(&native_str(&tx_dir), raw);
            let backup_path = Path::new(&backup);
            let existed = backup_path.exists();
            let kind = if existed {
                Some(if std::fs::metadata(backup_path).map(|m| m.is_dir()).unwrap_or(false) {
                    "dir"
                } else {
                    "file"
                })
            } else {
                None
            };
            snaps.push(Snapshot {
                path: p,
                existed,
                backup: if existed { Some(backup) } else { None },
                kind,
                remapped,
            });
        }
        let restored = snaps.len();
        self.restore(&snaps);

        // A shadowed transaction must not leave a ghost behind.
        //
        // `core/store.js:561` filters pending by the *resolved* (real) paths
        // and compares them against `c.shadow`, which holds the *shadow* path.
        // The two can never be equal, so the filter removes nothing and the
        // staged entry survives the rollback. The port used to reproduce that
        // filter verbatim, bug included.
        //
        // The bug was invisible in the JS build and is not here. In JS a
        // staged directory is replayed by `readFileSync` (store.js:382) and
        // throws EISDIR, and a staged file can only be applied by walking the
        // user through Apply — but in this port `apply_pending()` actually
        // works for both, so the residual entry is actionable: the UI still
        // offers "Apply", and pressing it puts the undone change back. Rolling
        // back has to be reversible *and* not re-offerable.
        //
        // So both the staged copy and its `pending.json` entry go. The match
        // is per path and against the journalled target — `c.shadow` is keyed
        // the same way `record_pending()` filters it — never a sweep of the
        // whole `sandbox/` subtree: `restore()` can undo several paths at once,
        // and unrelated work may be waiting for approval in the same mirror.
        if tx.get("sandbox").and_then(|v| v.as_bool()).unwrap_or(false) {
            self.reclaim_staged(&targets);
        }

        settings::audit(
            &self.root_str,
            &json!({
                "txId": tx_id,
                "status": "rolled-back",
                "rolledBackAt": now_iso(),
                "kind": tx.get("kind").cloned().unwrap_or(Value::Null),
                "action": "rollback",
                "assetId": tx.get("assetId").cloned().unwrap_or(Value::Null),
                "agentId": tx.get("agentId").cloned().unwrap_or(Value::Null),
                "projectId": tx.get("projectId").cloned().unwrap_or(Value::Null),
                "restored": restored,
            }),
        );
        json!({ "ok": true, "txId": tx_id, "restored": restored })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn store_at(root: &Path, sandbox: bool) -> Store {
        Store::new(&root.to_string_lossy(), sandbox)
    }

    #[test]
    fn real_and_unshadow_round_trip() {
        let root = temp_root("paths");
        let store = store_at(&root, true);

        let real = "F:\\demo\\proj\\.claude\\skills\\a.md";
        let shadow = store.real(real);
        assert!(shadow.starts_with(&store.sandbox_root), "{}", shadow);
        assert!(
            shadow.contains("drive") && shadow.contains("F"),
            "drive letter is kept as a directory: {}",
            shadow
        );
        assert_eq!(store.unshadow(&shadow), real);
        // Already-shadowed paths are passed through unchanged.
        assert_eq!(store.real(&shadow), shadow);
        // Non-shadow paths are untouched by unshadow.
        assert_eq!(store.unshadow(real), real);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sandbox_off_writes_straight_through() {
        let root = temp_root("noshadow");
        let store = store_at(&root, false);
        let target = root.join("agent").join("x.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::write(&p, "hello")],
            &json!({ "kind": "skill", "action": "save" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        assert!(store.read_pending().is_empty(), "no shadow, no pending");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn transaction_writes_journal_and_audit() {
        let root = temp_root("journal");
        let store = store_at(&root, false);
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

        let rec: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("backups").join(&tx_id).join("tx.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(rec["status"], json!("ok"));
        assert_eq!(rec["assetId"], json!("skill:x"));
        assert_eq!(rec["targets"], json!([p]));
        assert_eq!(rec["log"][0]["op"], json!("write"));
        assert_eq!(rec["log"][0]["bytes"], json!(4));

        let audit = std::fs::read_to_string(store.audit_file()).unwrap();
        assert_eq!(audit.lines().count(), 1);
        let line: Value = serde_json::from_str(audit.lines().next().unwrap()).unwrap();
        assert_eq!(line["txId"], json!(tx_id));
        assert_eq!(line["status"], json!("ok"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn delete_then_rollback_restores_content() {
        let root = temp_root("rollback-delete");
        let store = store_at(&root, false);
        let target = root.join("skill").join("SKILL.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "original").unwrap();
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::delete(&p)], &json!({ "kind": "skill" }));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(!target.exists());

        let rb = store.rollback(tx["txId"].as_str().unwrap());
        assert_eq!(rb["ok"], json!(true), "{}", rb);
        assert_eq!(rb["restored"], json!(1));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rollback_removes_a_created_file_and_prunes_its_dir() {
        let root = temp_root("rollback-create");
        let store = store_at(&root, false);
        let dir = root.join("agents").join("fresh");
        let target = dir.join("SKILL.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::write(&p, "new")], &json!({ "kind": "skill" }));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(target.exists());

        let rb = store.rollback(tx["txId"].as_str().unwrap());
        assert_eq!(rb["ok"], json!(true), "{}", rb);
        assert!(!target.exists());
        assert!(!dir.exists(), "empty parent left behind: {:?}", dir);
        // The transaction root itself must survive; it was not created by us.
        assert!(root.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rollback_unknown_transaction_reports_error() {
        let root = temp_root("rollback-missing");
        let store = store_at(&root, false);
        let rb = store.rollback("tx-nope");
        assert_eq!(rb["ok"], json!(false));
        assert!(rb["error"].as_str().unwrap().contains("not found"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_op_merges_patch_and_keeps_siblings() {
        let root = temp_root("json-merge");
        let store = store_at(&root, false);
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
        let store = store_at(&root, false);
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
        let store = store_at(&root, false);
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
        let store = store_at(&root, false);
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
    fn sandbox_write_lands_in_shadow_and_records_pending() {
        let root = temp_root("shadow");
        let store = store_at(&root, true);
        let real = root.join("agents").join(".claude").join("skills").join("a.md");
        let p = real.to_string_lossy().into_owned();

        let tx = store.transaction(&[Op::write(&p, "staged")], &json!({ "kind": "skill" }));
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(!real.exists(), "sandbox must not touch the real path");

        let changes = store.read_pending();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["op"], json!("write"));
        assert_eq!(changes[0]["real"], json!(p));
        assert_eq!(changes[0]["bytes"], json!(6));
        let shadow = changes[0]["shadow"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read_to_string(&shadow).unwrap(), "staged");
        assert!(changes[0]["at"].as_str().unwrap().ends_with('Z'));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sandbox_json_op_seeds_from_the_real_file() {
        let root = temp_root("shadow-seed");
        let store = store_at(&root, true);
        let cfg = root.join("agents").join("mcp.json");
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
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
        // The real config is untouched; the shadow copy has both servers.
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("keep"));
        assert!(!std::fs::read_to_string(&cfg).unwrap().contains("added"));

        let changes = store.read_pending();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["op"], json!("json"));
        assert!(
            changes[0]["base"].as_str().unwrap().contains("keep"),
            "the seed snapshot must be journalled for the three-way merge"
        );
        let shadow: Value = serde_json::from_str(
            &std::fs::read_to_string(changes[0]["shadow"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(shadow["mcpServers"]["added"]["command"], json!("b"));
        assert_eq!(shadow["mcpServers"]["keep"]["command"], json!("a"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn failed_apply_does_not_leave_a_ghost_pending_entry() {
        // A staged change whose replay fails must not stay in `pending.json`.
        // It used to: the failure path only called `restore()`, which undoes
        // the writes it made but touches neither the shadow tree nor the
        // pending entry — so the UI kept offering "Apply" for a change that
        // had already been rolled back, and the counter stayed at 1 forever.
        let root = temp_root("apply-fail-ghost");
        let mut store = store_at(&root, true);

        // Stage a json change (the op kind `record_pending` files under "json",
        // and the one `apply_pending` replays as a `jsonmerge` that errors with
        // "copy source missing" once its `from` vanishes), so there is
        // something to ghost.
        let real = root.join("agents").join(".claude").join("mcp.json");
        let p = real.to_string_lossy().into_owned();
        store.transaction(
            &[Op::json_patch(&p, json!({ "mcpServers": { "a": { "command": "x" } } }))],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(store.read_pending().len(), 1, "one staged change to begin with");

        // Corrupt the staged copy so the replay fails with "copy source
        // missing" — the same way a vanished shadow tree fails in production.
        let shadow = PathBuf::from(store.read_pending()[0]["shadow"].as_str().unwrap());
        std::fs::remove_file(&shadow).unwrap();

        let applied = store.apply_pending();
        assert_eq!(applied["ok"], json!(false), "{}", applied);
        assert!(
            applied["error"].as_str().unwrap().contains("copy source missing"),
            "{}",
            applied["error"]
        );
        assert!(
            store.read_pending().is_empty(),
            "a failed apply must not leave a ghost pending entry: {:?}",
            store.read_pending()
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn apply_pending_replays_onto_real_paths_and_clears_pending() {
        let root = temp_root("apply");
        let mut store = store_at(&root, true);
        let real = root.join("agents").join(".claude").join("skills").join("a.md");
        let p = real.to_string_lossy().into_owned();

        store.transaction(&[Op::write(&p, "staged")], &json!({ "kind": "skill" }));
        assert!(!real.exists());

        let applied = store.apply_pending();
        assert_eq!(applied["ok"], json!(true), "{}", applied);
        assert_eq!(applied["applied"], json!(1));
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "staged");
        assert!(store.read_pending().is_empty());
        assert!(store.sandbox(), "the sandbox flag is restored after apply");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn apply_pending_replays_a_staged_copy_dir_as_a_directory() {
        let root = temp_root("apply-copydir");
        let mut store = store_at(&root, true);
        let src = root.join("library").join("pdf");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("SKILL.md"), "skill").unwrap();
        std::fs::write(src.join("nested").join("a.md"), "a").unwrap();
        let dest = root.join("home").join(".claude").join("skills").join("pdf");

        let tx = store.transaction(
            &[Op::copy_dir(
                &dest.to_string_lossy(),
                &src.to_string_lossy(),
                true,
            )],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(!dest.exists(), "staging never touches the real path");
        let shadow = PathBuf::from(store.read_pending()[0]["shadow"].as_str().unwrap());

        let applied = store.apply_pending();
        assert_eq!(applied["ok"], json!(true), "{}", applied);
        assert_eq!(applied["applied"], json!(1));
        assert!(dest.is_dir(), "the replayed target is a directory, not a stub file");
        for rel in ["SKILL.md", "nested/a.md"] {
            let rel = rel.replace('/', std::path::MAIN_SEPARATOR.to_string().as_str());
            assert_eq!(
                std::fs::read_to_string(dest.join(&rel)).unwrap(),
                std::fs::read_to_string(shadow.join(&rel)).unwrap(),
                "{} lands byte for byte with the staged copy",
                rel
            );
        }
        assert!(store.read_pending().is_empty(), "a successful apply clears pending");
        assert_eq!(
            applied["targets"],
            json!([dest.to_string_lossy()]),
            "the journal records the real path"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discard_pending_clears_a_staged_directory() {
        let root = temp_root("discard-copydir");
        let store = store_at(&root, true);
        let src = root.join("library").join("pdf");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "skill").unwrap();
        let dest = root.join("home").join(".claude").join("skills").join("pdf");

        store.transaction(
            &[Op::copy_dir(
                &dest.to_string_lossy(),
                &src.to_string_lossy(),
                true,
            )],
            &json!({ "kind": "skill" }),
        );
        let shadow = store.read_pending()[0]["shadow"].as_str().unwrap().to_string();
        assert!(Path::new(&shadow).is_dir());

        let r = store.discard_pending();
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["discarded"], json!(1));
        assert!(!Path::new(&shadow).exists(), "the staged directory is gone");
        assert!(!dest.exists(), "the real path was never written");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn apply_pending_is_a_noop_when_nothing_is_staged() {
        let root = temp_root("apply-empty");
        let mut store = store_at(&root, true);
        let r = store.apply_pending();
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["applied"], json!(0));
        assert_eq!(r["message"], json!("no pending changes"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn discard_pending_removes_the_mirror_only() {
        let root = temp_root("discard");
        let store = store_at(&root, true);
        let real = root.join("agents").join("b.md");
        let p = real.to_string_lossy().into_owned();

        store.transaction(&[Op::write(&p, "staged")], &json!({ "kind": "rule" }));
        let shadow = store.read_pending()[0]["shadow"].as_str().unwrap().to_string();
        assert!(Path::new(&shadow).exists());

        let r = store.discard_pending();
        assert_eq!(r["discarded"], json!(1));
        assert!(!Path::new(&shadow).exists());
        assert!(store.read_pending().is_empty());
        assert!(!real.exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn three_way_merge_keeps_external_edits_on_untouched_keys() {
        let base = json!({ "a": 1, "b": { "x": 1, "y": 2 } });
        let ours = json!({ "a": 9, "b": { "x": 1, "y": 2 } });
        let theirs = json!({ "a": 1, "b": { "x": 1, "y": 42 }, "c": "external" });

        let merged = three_way_merge(&base, &ours, &theirs);
        assert_eq!(merged["a"], json!(9), "our edit wins");
        assert_eq!(merged["b"]["y"], json!(42), "their nested edit survives");
        assert_eq!(merged["b"]["x"], json!(1));
        assert_eq!(merged["c"], json!("external"), "a key we never saw is kept");
    }

    #[test]
    fn three_way_merge_returns_ours_verbatim_for_scalars() {
        let merged = three_way_merge(&json!(1), &json!(2), &json!(3));
        assert_eq!(merged, json!(2));
    }

    #[test]
    fn jsonmerge_replays_a_staged_change_over_an_external_edit() {
        let root = temp_root("jsonmerge");
        let store = store_at(&root, false);
        let real = root.join("mcp.json");
        // `theirs` — the file as it stands at apply time: an external writer
        // re-pointed `b` and added a whole new section after we staged.
        std::fs::write(
            &real,
            r#"{"servers":{"a":{"command":"old"},"b":{"command":"theirs-edit"}},"external":{"x":1}}"#,
        )
        .unwrap();
        let p = real.to_string_lossy().into_owned();

        // `ours` — the shadow result: we only touched `a`.
        let shadow = root.join("shadow.json");
        std::fs::write(
            &shadow,
            r#"{"servers":{"a":{"command":"new"},"b":{"command":"b"}}}"#,
        )
        .unwrap();
        // `base` — the seed snapshot taken when the change was staged.
        let base = r#"{"servers":{"a":{"command":"old"},"b":{"command":"b"}}}"#;

        let tx = store.transaction(
            &[Op::json_merge(
                &p,
                &shadow.to_string_lossy(),
                &format!("{}\n", base),
            )],
            &json!({ "kind": "apply" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        let merged: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(merged["servers"]["a"]["command"], json!("new"), "our edit wins");
        assert_eq!(
            merged["servers"]["b"]["command"],
            json!("theirs-edit"),
            "an external edit to a key we never touched survives"
        );
        assert_eq!(merged["external"]["x"], json!(1), "a key we never saw is kept");

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
        assert!(!dest.join("node_modules").exists(), "build trees are not backed up");

        let mut files = list_files(&src);
        files.sort();
        assert_eq!(files, vec!["b.md".to_string(), "nested/a.md".to_string()]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_dir_via_op_uses_the_real_source_when_sandboxed() {
        let root = temp_root("copydir-sandbox");
        let store = store_at(&root, true);
        let src = root.join("library").join("pdf");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), "content").unwrap();
        let dest = root.join("agents").join(".claude").join("skills").join("pdf");

        let tx = store.transaction(
            &[Op::copy_dir(
                &dest.to_string_lossy(),
                &src.to_string_lossy(),
                true,
            )],
            &json!({ "kind": "skill", "action": "install" }),
        );
        assert_eq!(tx["ok"], json!(true), "{}", tx);
        assert!(!dest.exists(), "sandbox: the real destination is untouched");
        let shadow = store.read_pending()[0]["shadow"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read_to_string(Path::new(&shadow).join("SKILL.md")).unwrap(), "content");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_copy_source_fails_the_whole_transaction() {
        let root = temp_root("copydir-missing");
        let store = store_at(&root, false);
        let target = root.join("written.md");
        let p = target.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[
                Op::write(&p, "should be rolled back"),
                Op::copy_dir(
                    &root.join("dest").to_string_lossy(),
                    &root.join("nope").to_string_lossy(),
                    true,
                ),
            ],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(tx["ok"], json!(false), "{}", tx);
        assert!(
            tx["error"].as_str().unwrap().contains("copy source missing"),
            "{}",
            tx["error"]
        );
        assert!(!target.exists(), "the earlier write must be undone");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn json_mutate_returning_none_aborts() {
        let root = temp_root("mutate-abort");
        let store = store_at(&root, false);
        let cfg = root.join("mcp.json");
        std::fs::write(&cfg, r#"{"a":1}"#).unwrap();
        let p = cfg.to_string_lossy().into_owned();

        let tx = store.transaction(
            &[Op::json_mutate(&p, |_| None)],
            &json!({ "kind": "mcp" }),
        );
        assert_eq!(tx["ok"], json!(false));
        assert!(tx["error"].as_str().unwrap().contains("mutate aborted"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), r#"{"a":1}"#);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mkdir_op_is_idempotent_and_needs_no_backup() {
        let root = temp_root("mkdir");
        let store = store_at(&root, false);
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

    #[test]
    fn rollback_of_a_sandboxed_transaction_hits_the_real_path() {
        let root = temp_root("rollback-shadow");
        let store = store_at(&root, true);
        let real = root.join("agents").join(".claude").join("skills").join("a.md");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "before").unwrap();
        let p = real.to_string_lossy().into_owned();

        // Sandboxed write, then apply (which lands on the real path), then undo.
        let tx = store.transaction(&[Op::write(&p, "after")], &json!({ "kind": "skill" }));
        let tx_id = tx["txId"].as_str().unwrap().to_string();
        let shadow = store.read_pending()[0]["shadow"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read_to_string(&shadow).unwrap(), "after");

        // The journal records the shadow path, so `rollback()` has to map it
        // back through `unshadow()` (core/store.js:539-557) — a path that is
        // not mapped resolves to a relative "drive\<letter>\..." and the undo
        // would land in the process CWD.
        assert_eq!(store.unshadow(&shadow), p, "the undo target is the real path");

        let rb = store.rollback(&tx_id);
        assert_eq!(rb["ok"], json!(true), "{}", rb);
        // JS `rollback()` (store.js:527-567) rebuilds its snapshots from the
        // journalled targets and calls `_restore()` (store.js:504-525), which
        // only writes to or removes `s.path` — the remapped real path.
        //
        // This port additionally reclaims the staged copy, so the assertion
        // that used to live here (`Path::new(&shadow).exists()`) is gone. See
        // `rollback_clears_the_staged_shadow_copy` for why, and note that the
        // divergence this creates against `core/store.js` is asserted on both
        // sides by `scripts/tauri-write-parity.js`.
        assert!(
            !Path::new(&shadow).exists(),
            "the staged copy must not outlive the rollback"
        );
        // A sandboxed transaction never wrote the real file, so the undo must
        // leave it exactly as it was — deleting it here would be data loss.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "before");
        // store.js:561 filters pending by the remapped (real) paths against
        // `c.shadow`, so there the staged entry survives the rollback. Here it
        // must be gone, together with the file it pointed at.
        assert_eq!(store.read_pending().len(), 0, "{}", json!(store.read_pending()));

        std::fs::remove_dir_all(&root).ok();
    }

    /// The rollback has to take the staging area back with it — the ghost entries
    /// `core/store.js:561` leaves behind are only harmless there.
    ///
    /// Constraint worth reading before touching the cleanup it exercises: the
    /// reclaim must be **per path**. A second, unrelated staged change sits in
    /// the same mirror tree below and must survive untouched — sweeping the
    /// whole `sandbox/` subtree instead would be worse than the ghost, because
    /// it destroys work the user is still deciding on.
    #[test]
    fn rollback_clears_the_staged_shadow_copy() {
        let root = temp_root("rollback-reclaim");
        let store = store_at(&root, true);

        // Two independent transactions stage changes into the same mirror.
        let first = root.join("home").join(".claude").join("skills").join("a.md");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, "before").unwrap();
        let t1 = store.transaction(
            &[Op::write(&native_str(&first), "after")],
            &json!({ "kind": "skill" }),
        );
        let id1 = t1["txId"].as_str().unwrap().to_string();

        let second = root.join("home").join(".claude").join("skills").join("b.md");
        let t2 = store.transaction(
            &[Op::write(&native_str(&second), "staged too")],
            &json!({ "kind": "skill" }),
        );
        assert_eq!(t2["ok"], json!(true), "{}", t2);

        let before = store.read_pending();
        assert_eq!(before.len(), 2, "two staged changes: {}", json!(before));
        let shadow1 = before[0]["shadow"].as_str().unwrap().to_string();
        let shadow2 = before[1]["shadow"].as_str().unwrap().to_string();
        assert_ne!(shadow1, shadow2);
        assert!(Path::new(&shadow1).is_file());
        assert!(Path::new(&shadow2).is_file());

        let rb = store.rollback(&id1);
        assert_eq!(rb["ok"], json!(true), "{}", rb);

        // The rolled-back transaction's copy and entry are both gone.
        assert!(!Path::new(&shadow1).exists(), "its staged copy is reclaimed");
        let after = store.read_pending();
        assert_eq!(after.len(), 1, "one entry left: {}", json!(after));
        assert_eq!(
            after[0]["shadow"].as_str(),
            Some(shadow2.as_str()),
            "and it is the *other* transaction's"
        );
        // `pending.json` has to move with the file: an entry pointing at a path
        // that no longer exists is the same ghost wearing a different hat.
        assert!(Path::new(&shadow2).is_file(), "the untouched staged copy survives");

        // Reclaiming the mirror changes nothing about the real tree.
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "before");
        assert!(!second.exists(), "the other change was never applied");

        std::fs::remove_dir_all(&root).ok();
    }

    /// With the sandbox off the new cleanup must not exist at all.
    ///
    /// Read as the "do no harm" counterpart of the test above: nothing in that
    /// path may leak into the unsandboxed one, where every write already lands
    /// on the real file and there is no mirror to reclaim.
    #[test]
    fn non_sandboxed_rollback_still_leaves_no_shadow_copy() {
        let root = temp_root("rollback-noshadow");
        let store = store_at(&root, false);
        let real = root.join("home").join("b.md");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "before").unwrap();
        let p = native_str(&real);

        let tx = store.transaction(&[Op::write(&p, "after")], &json!({ "kind": "rule" }));
        let id = tx["txId"].as_str().unwrap().to_string();

        let rb = store.rollback(&id);
        assert_eq!(rb["ok"], json!(true), "{}", rb);
        assert_eq!(rb["restored"], json!(1));

        // Byte-identical undo — the whole point of the round trip.
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "before");

        // No mirror, no staging: nothing was ever recorded and nothing can be
        // left behind. `record_pending()` returns before touching the file, so
        // `pending.json` is not written at all — were the new cleanup reachable
        // from here it would create it empty, and this assertion would fail.
        assert!(
            list_files(Path::new(&store.sandbox_root)).is_empty(),
            "nothing may be staged under sandbox/"
        );
        assert!(store.read_pending().is_empty());
        assert!(
            !Path::new(&store.pending_file()).exists(),
            "the unsandboxed path never writes pending.json"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The `tx.sandbox` gate is what keeps the cleanup off the unsandboxed path,
    /// and this is the case that would hurt if it ever came loose.
    ///
    /// `read_pending()` is empty here for a structural reason — `record_pending()`
    /// returns before writing anything when the sandbox is off — so a relaxed
    /// gate can look harmless until `pending.json` already has content that was
    /// put there some other way. Hand-planting one is not contrived for its own
    /// sake: with the sandbox off the journalled targets *are* real paths, so a
    /// pending entry naming one of them names a real agent file, and an
    /// ungated cleanup deletes it.
    ///
    /// A mutation that drops the `tx.sandbox` gate turns this test red; without
    /// the planted entry that same mutation passes, which is exactly the hole
    /// this closes.
    #[test]
    fn non_sandboxed_rollback_ignores_a_planted_pending_entry() {
        let root = temp_root("rollback-planted");
        let store = store_at(&root, false);
        let real = root.join("home").join("real.md");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "before").unwrap();
        let p = native_str(&real);

        // An entry whose `shadow` is a real path. Nothing in the unsandboxed
        // flow produces one; if the reclaim ran anyway it would match on it.
        let _ = std::fs::write(
            store.pending_file(),
            pretty(&json!({
                "version": 1,
                "updatedAt": now_iso(),
                "changes": [{
                    "op": "write",
                    "shadow": p,
                    "real": p,
                    "bytes": 6
                }],
            })),
        );

        let tx = store.transaction(&[Op::write(&p, "after")], &json!({ "kind": "rule" }));
        let id = tx["txId"].as_str().unwrap().to_string();
        let rb = store.rollback(&id);
        assert_eq!(rb["ok"], json!(true), "{}", rb);

        // The gate held: the real file is restored, not deleted, and the
        // planted entry was left exactly as it was.
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "before",
            "an ungated reclaim would have deleted this file instead"
        );
        let left = store.read_pending();
        assert_eq!(left.len(), 1, "{}", json!(left));
        assert_eq!(left[0]["shadow"].as_str(), Some(p.as_str()));

        std::fs::remove_dir_all(&root).ok();
    }
}
