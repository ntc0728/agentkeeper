//! Settings + store bootstrap — a port of `Store.ensure()`, `readSettings()`,
//! `saveSettings()` and `setSandbox()` from `core/store.js`.
//!
//! This is the first slice of the store layer. It deliberately stops short of
//! the transaction/backup/journal machinery: settings are a single-file JSON
//! write with the same atomic-rename guarantee, so moving them early costs
//! little and removes a real lie — the Stage-2 shell accepted settings changes
//! and dropped them on exit.
//!
//! ## Semantics copied from the JS
//!
//! * Only keys that exist in `defaults` may be written, and only with the same
//!   `typeof` as the default. A hand-edited `settings.json` cannot smuggle in a
//!   string where a number belongs (the update manager would then schedule a
//!   check every `"abc"` minutes).
//! * The file is written through `<path>.agentkeeper-tmp-<pid>-<ms>-<seq>` then
//!   renamed, falling back to a direct write if rename fails (Windows: a
//!   scanner can hold the target briefly). The `seq` half exists because
//!   `<pid>-<ms>` is not unique when two tasks write the same path inside one
//!   millisecond — see [`atomic_write`].
//! * Every save appends one `audit.jsonl` record.
//!
//! ## One deliberate difference
//!
//! `sandbox` is **not** a settings key here nor in the JS: Electron seeds it
//! from `AGENTKEEPER_SANDBOX` (default on) and mirrors later toggles into
//! `meta.json`, which nothing reads back at startup. So the toggle is
//! session-scoped in both builds — `set_sandbox_meta` keeps `meta.json`
//! truthful for external inspectors without pretending it drives startup.

use serde_json::{json, Map, Value};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::path::Path;

use super::model::iso_from_millis;

/// `Store.defaultSettings()` — the type of each default also decides which
/// values may override it.
pub fn defaults() -> Value {
    json!({
        "lang": "auto",
        "theme": "system",
        "updateCheck": true,
        "updateCheckMinutes": 20,
        "updateBeta": false,
        "updateRepo": "ntc0728/agentkeeper",
        "updateApiBase": "",
        "updateProxy": "auto",
        "updateToken": "",
        "updateAssetKeyword": "AgentKeeper",
        "updateInstallSilent": true,
        "notifications": true,
        "showLocalStats": true
    })
}

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_millis(ms)
}

fn settings_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("settings.json")
}

fn meta_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("meta.json")
}

fn audit_path(root: &str) -> std::path::PathBuf {
    Path::new(root).join("audit.jsonl")
}

/// `typeof a === typeof b`, with JS's narrower meaning of "object": arrays and
/// objects are both `object`, null is also `object` (which is why a null in the
/// file never overrides a typed default).
fn same_type(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::String(_), Value::String(_))
            | (Value::Bool(_), Value::Bool(_))
            | (Value::Number(_), Value::Number(_))
            | (Value::Array(_), Value::Array(_))
            | (Value::Object(_), Value::Object(_))
    )
}

/// `Store.ensure()` — create the tree the rest of the app assumes exists.
///
/// Safe to call repeatedly; this runs on every bootstrap and on every settings
/// write, which is what makes "first launch" work without a separate step.
pub fn ensure_store(root: &str, sandbox: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::create_dir_all(Path::new(root).join("sandbox"))?;
    std::fs::create_dir_all(Path::new(root).join("backups"))?;
    let audit = audit_path(root);
    if !audit.exists() {
        std::fs::write(&audit, "")?;
    }
    let meta = meta_path(root);
    if !meta.exists() {
        let body = serde_json::to_string_pretty(&json!({
            "version": 1,
            "sandbox": sandbox,
            "createdAt": now_iso()
        }))
        .unwrap_or_default();
        std::fs::write(&meta, body)?;
    }
    Ok(())
}

/// `readSettings()` — defaults, selectively overridden by the file.
pub fn read(root: &str) -> Value {
    let raw = std::fs::read(settings_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));

    let d = defaults();
    let (Value::Object(d), Value::Object(r)) = (&d, &raw) else {
        return d.clone();
    };
    let mut out = Map::new();
    for (k, dv) in d {
        match r.get(k) {
            Some(v) if same_type(dv, v) => {
                out.insert(k.clone(), v.clone());
            }
            _ => {
                out.insert(k.clone(), dv.clone());
            }
        }
    }
    Value::Object(out)
}

/// `saveSettings(patch)` — filtered merge, atomic write, audit.
///
/// Returns the same `{ ok, settings }` envelope as the JS so the renderer's
/// success path is identical. Errors surface as `{ ok: false, error }` instead
/// of a silent success: a settings change that did not reach disk must not look
/// like it did.
pub fn save(root: &str, patch: &Value) -> Value {
    let current = read(root);
    let d = defaults();
    let mut next = current.as_object().cloned().unwrap_or_default();
    let mut applied: Vec<String> = Vec::new();

    if let (Value::Object(d), Value::Object(p)) = (&d, patch) {
        for (k, v) in p {
            // Unknown keys and type mismatches are ignored, exactly as in JS.
            let Some(dv) = d.get(k) else { continue };
            if !same_type(dv, v) {
                continue;
            }
            next.insert(k.clone(), v.clone());
            applied.push(k.clone());
        }
    }

    let next = Value::Object(next);
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&next).unwrap_or_else(|_| "{}".to_string())
    );
    if let Err(e) = atomic_write(&settings_path(root), &body) {
        return json!({ "ok": false, "error": format!("settings.json write failed: {}", e) });
    }

    audit(
        root,
        &json!({
            "action": "settings-save",
            "status": "ok",
            "kind": "settings",
            "assetName": applied.join(",")
        }),
    );

    json!({ "ok": true, "settings": next })
}

/// `atomicWrite()` — temp file in the same directory, then rename.
///
/// The temp name must be unique **per call**, not per millisecond: two tasks
/// writing the *same* path inside one tick (a double-clicked save, or the
/// renderer saving one asset twice) would otherwise get the same name, and
/// `File::create` truncates — one writer would overwrite the other's temp file
/// and both would then rename the mixture into place, which is exactly what
/// temp+rename exists to prevent. The pid keeps a stale temp file traceable to
/// the process that failed to clean it up, the timestamp makes it readable, and
/// the counter is what actually makes it unique.
fn temp_name(file_name: &str) -> String {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "{}.agentkeeper-tmp-{}-{}-{}",
        file_name,
        std::process::id(),
        stamp,
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let tmp = path.with_file_name(temp_name(&file_name));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all().ok();
    }

    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows: a virus scanner can hold the target open for a moment.
            // Dropping the temp file and writing in place is still better than
            // failing the save.
            //
            // This is the one non-atomic branch in the function: during it a
            // concurrent reader can observe a truncated file. It is not made
            // "safe" by retrying here — a retry loop would only narrow the
            // window, and calling the result atomic is how the guarantee gets
            // lost. Two tests hold the two halves of that apart. The same-path
            // writers in `tests/boundary.rs` assert a reader never observes a
            // torn body, so if this branch ever fires on that path the suite
            // fails instead of the corruption shipping silently. And
            // `a_target_held_without_delete_sharing_makes_atomic_write_fall_back_in_place`
            // holds the target open without `FILE_SHARE_DELETE`, which is what
            // forces *this* branch, and asserts the save still lands. The second
            // proves the fallback works; it does not make the window go away,
            // which is why the first is still the guarantee that matters.
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, content)
        }
    }
}

/// `Store.audit()` — one JSONL record, `ts` first.
pub fn audit(root: &str, rec: &Value) {    let mut map = Map::new();
    map.insert("ts".to_string(), Value::String(now_iso()));
    if let Value::Object(r) = rec {
        for (k, v) in r {
            map.insert(k.clone(), v.clone());
        }
    }
    let line = format!("{}\n", Value::Object(map));
    let path = audit_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// `Store.baselineDiff()` — what changed since the user last anchored.
///
/// Returns the shape the Health card reads: `{ changed, added, gone, hasBaseline }`.
/// With no `baseline.json` yet (the
/// normal state until someone clicks "建立基线") it reports `hasBaseline: false`
/// instead of pretending everything is clean — and, importantly, instead of
/// failing: the card renders "loading…" forever on `ok: false`.
pub fn baseline_diff(root: &str, assets: &[(String, String)]) -> Value {
    let base = read_baseline(root);
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut gone = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for (id, fp) in assets {
        seen.insert(id.as_str());
        match base.get(id) {
            None => added.push(Value::String(id.clone())),
            Some(prev) if prev != fp => changed.push(Value::String(id.clone())),
            Some(_) => {}
        }
    }
    for id in base.keys() {
        if !seen.contains(id.as_str()) {
            gone.push(Value::String(id.clone()));
        }
    }

    json!({
        "ok": true,
        "changed": changed,
        "added": added,
        "gone": gone,
        "hasBaseline": !base.is_empty()
    })
}

/// `Store.setBaseline()` — anchor the current fingerprints.
///
/// This is a single file in the store root with the same atomic-rename
/// guarantee as settings.json; it does not touch agent files, so it does not
/// need the transaction layer.
pub fn set_baseline(root: &str, assets: &[(String, String)]) -> Value {
    let mut map = Map::new();
    for (id, fp) in assets {
        map.insert(id.clone(), Value::String(fp.clone()));
    }
    let body = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
    );
    if let Err(e) = atomic_write(&Path::new(root).join("baseline.json"), &body) {
        return json!({ "ok": false, "error": format!("baseline.json write failed: {}", e) });
    }
    audit(
        root,
        &json!({
            "kind": "settings",
            "action": "set-baseline",
            "status": "ok",
            "assetName": assets.len().to_string()
        }),
    );
    json!({ "ok": true, "count": assets.len() })
}

fn read_baseline(root: &str) -> Map<String, Value> {
    std::fs::read(Path::new(root).join("baseline.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// `setSandbox(on)` — leave a truthful trace in `meta.json`.
///
/// The JS writes the same field through `_patchMeta()`; startup does not read
/// it back, so this is bookkeeping, not persistence.
pub fn set_sandbox_meta(root: &str, on: bool) {
    let _ = ensure_store(root, on);
    let mut meta = std::fs::read(meta_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| json!({}));
    if let Value::Object(m) = &mut meta {
        m.insert("sandbox".to_string(), Value::Bool(on));
    }
    let body = serde_json::to_string_pretty(&meta).unwrap_or_default();
    let _ = std::fs::write(meta_path(root), body);
}

/// `process.env.AGENTKEEPER_SANDBOX !== '0'` — default on, even when unset.
pub fn sandbox_from_env() -> bool {
    std::env::var("AGENTKEEPER_SANDBOX").map(|v| v != "0").unwrap_or(true)
}

/* ------------------------------------------------------------------ */
/* renderer crash channel (`ak:ui-error`)                              */
/* ------------------------------------------------------------------ */

/// How many renderer errors are kept. Small on purpose: this file is a
/// diagnostic tail, not a log archive, and a renderer stuck in an error loop can
/// emit thousands of lines per minute.
pub const UI_ERROR_LIMIT: usize = 200;

/// Per-field cap for `message` / `stack`. A minified stack with a build hash and
/// a dozen frames is comfortably under 4 KB; anything longer is a data URL or a
/// runaway string and would push a single record into the "large file" category.
pub const UI_ERROR_FIELD_MAX: usize = 4096;

/// Diagnostics live next to `audit.jsonl` in the store root, so "open the store
/// folder" surfaces them without a second path convention.
pub const UI_ERROR_FILE: &str = "ui-errors.jsonl";

/// Cut a string to at most `max` bytes **on a UTF-8 character boundary**.
///
/// `s[..n]` panics when `n` splits a character, and `&s.as_bytes()[..n]` returns
/// bytes that `from_utf8` then rejects — a Chinese stack message truncated at
/// an arbitrary byte offset is exactly that case (3 bytes per character, so 4096
/// is not a boundary). Walking back to the nearest boundary is the only
/// truncation that keeps the line parseable as JSON *and* readable as text.
fn cut_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Build the record that gets appended: the five documented fields (empty
/// string when absent, so the renderer never has to test for `null`) plus every
/// unknown key, untouched.
fn ui_error_record(payload: &Value) -> Value {
    let mut out = Map::new();
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    out.insert("kind".into(), Value::String(text("kind")));
    out.insert(
        "message".into(),
        Value::String(cut_utf8(&text("message"), UI_ERROR_FIELD_MAX)),
    );
    out.insert(
        "stack".into(),
        Value::String(cut_utf8(&text("stack"), UI_ERROR_FIELD_MAX)),
    );
    out.insert("view".into(), Value::String(text("view")));
    out.insert("at".into(), Value::String(text("at")));
    // Unknown keys survive verbatim: the renderer will grow this payload and the
    // backend must not have to ship in lockstep to keep the extra fields.
    if let Value::Object(map) = payload {
        for (k, v) in map {
            if !out.contains_key(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// `ak:ui-error` — append one renderer error, keeping at most
/// [`UI_ERROR_LIMIT`] lines.
///
/// Read-trim-write rather than a rotating log: the file is bounded, so the whole
/// thing fits in memory, and the alternative (append + periodic rotate) leaves a
/// second file whose lifecycle nothing owns.
///
/// Never panics and never returns `Err` to the caller — a renderer reporting a
/// crash must not itself crash the app. A failed write is reported as
/// `{"ok": false, "error": ...}` and echoed to stderr, which is where a
/// packaged build's console shows it.
///
/// **Concurrent reports are safe.** The read-trim-write runs under
/// [`UI_ERROR_LOCK`], so two renderer errors arriving together are serialised
/// and both lines survive; the last writer can no longer save a body it read
/// before the other one wrote. Cheap to guarantee here (one small file, writes
/// already bounded by [`UI_ERROR_LIMIT`]) and expensive to live without: the
/// line lost to a race is the one explaining the crash in front of the user.
pub fn ui_error(root: &str, payload: &Value) -> Value {
    match append_ui_error(root, payload) {
        Ok(count) => json!({ "ok": true, "count": count }),
        Err(e) => {
            // stderr as well: the renderer's toast is the only other copy, and
            // it disappears with the page that reported the error.
            eprintln!("[ui-error] could not append to {}: {}", UI_ERROR_FILE, e);
            json!({ "ok": false, "error": e.to_string() })
        }
    }
}

/// Serialises the read-trim-write in [`append_ui_error`].
///
/// Why a lock and not "append and let the OS interleave": every report rewrites
/// the whole bounded file, so two reports that overlap both read the same old
/// body and the second write silently drops the first line. Appending would fix
/// that but loses the cap, which is the reason the file is a rewrite in the
/// first place.
///
/// One process-wide lock, not a per-root map: there is exactly one store root
/// per process, the critical section is one small file, and a map would need a
/// lifecycle (and a cleanup rule) of its own for no gain.
///
/// `unwrap_or_else(|e| e.into_inner())` rather than `unwrap`: a panic in some
/// other thread must not poison this lock permanently — the crash channel going
/// silent after the first panic is precisely the failure it exists to report.
static UI_ERROR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn append_ui_error(root: &str, payload: &Value) -> std::io::Result<usize> {
    let dir = Path::new(root);
    // A missing store root is created here rather than assumed: this command can
    // be the first thing a crashed renderer calls, before `bootstrap` ran.
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
    }
    let path = dir.join(UI_ERROR_FILE);

    // The guard is taken here, around the read *and* the write, because the lock
    // has to cover both halves of the read-modify-write: guarding only the write
    // still lets a second reporter read the pre-append body. Locking at this
    // single entry point (rather than in `ui_error`) also covers any future
    // caller that appends directly, and keeps the invariant next to the code
    // that breaks it. Not `try_lock`: skipping the write under contention trades
    // a lost line for a lost report, which is worse — a refused report is not
    // retried by the renderer.
    let _guard = UI_ERROR_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    // One line, one object — `to_string`, not pretty: a pretty record would
    // occupy several lines and break both the cap and any line-based reader.
    lines.push(ui_error_record(payload).to_string());

    // Drop the oldest, never the newest: the most recent error is the one that
    // explains the crash in front of the user.
    if lines.len() > UI_ERROR_LIMIT {
        let excess = lines.len() - UI_ERROR_LIMIT;
        lines.drain(0..excess);
    }

    let mut body = lines.join("\n");
    body.push('\n');
    // Same atomic-rename guarantee as every other store file: a half-written
    // diagnostics file is worthless exactly when it is needed.
    atomic_write(&path, &body)?;
    Ok(lines.len())
}

/// `Store.readAudit()` — newest first, optionally filtered.
///
/// Returns a **plain array**, not an envelope: `ak:audit` hands this straight
/// to the renderer, which does `audit.filter(...)`. (The stage-1 stub returned
/// `{ok, entries}` and broke the Activity page on open.)
pub fn read_audit(
    root: &str,
    limit: usize,
    kind: Option<&str>,
    agent_id: Option<&str>,
    project_id: Option<&str>,
) -> Vec<Value> {
    let text = std::fs::read_to_string(audit_path(root)).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.split('\n').filter(|l| !l.trim().is_empty()).rev() {
        if out.len() >= limit {
            break;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let matches = |key: &str, want: Option<&str>| match want {
            Some(w) => rec.get(key).and_then(|v| v.as_str()) == Some(w),
            None => true,
        };
        if !matches("kind", kind) || !matches("agentId", agent_id) || !matches("projectId", project_id)
        {
            continue;
        }
        out.push(rec);
    }
    out
}

/// `Store.listTransactions()` — one `backups/<txId>/tx.json` per transaction.
///
/// Read-only; it stays empty until the write path lands, which is truthful:
/// no transaction has run.
pub fn list_transactions(root: &str, limit: usize) -> Vec<Value> {
    let dir = Path::new(root).join("backups");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let tx_file = entry.path().join("tx.json");
        let Ok(body) = std::fs::read(&tx_file) else {
            continue;
        };
        let Ok(mut tx) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        if let Value::Object(m) = &mut tx {
            m.insert("hasBackup".to_string(), Value::Bool(true));
        }
        out.push(tx);
    }
    // Newest first: txIds are `tx-<timestamp>-<rand>`, so reverse string order
    // is chronological order (same trick as the JS localeCompare sort).
    out.sort_by(|a, b| {
        let ka = a.get("txId").map(|v| v.to_string()).unwrap_or_default();
        let kb = b.get("txId").map(|v| v.to_string()).unwrap_or_default();
        kb.cmp(&ka)
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ak-settings-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// Two writes of the same file must never share a temp name, even when they
    /// begin inside the same millisecond: the pid and the clock cannot tell them
    /// apart, so without the sequence counter the second `File::create`
    /// truncates the first writer's temp file and both rename the mixture.
    ///
    /// Fifty calls, not two: a single pair can straddle a millisecond boundary by
    /// luck, which is exactly how this bug would hide. Fifty names inside a few
    /// microseconds cannot.
    #[test]
    fn temp_file_names_are_unique_per_call() {
        let mut names = std::collections::HashSet::new();
        for _ in 0..50 {
            assert!(
                names.insert(temp_name("settings.json")),
                "a temp name repeated inside one burst — the next writer would truncate it"
            );
        }
        let sample = temp_name("settings.json");
        assert!(
            sample.starts_with("settings.json.agentkeeper-tmp-"),
            "the temp file must live next to its target: {}",
            sample
        );
        assert!(
            sample.contains(&std::process::id().to_string()),
            "the pid must stay in the name so a stale temp file is traceable: {}",
            sample
        );
    }

    #[test]
    fn read_returns_defaults_when_file_missing() {
        let root = temp_root("defaults");
        let s = read(&root);
        assert_eq!(s["lang"], json!("auto"));
        assert_eq!(s["updateCheckMinutes"], json!(20));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_ignores_wrong_types_and_unknown_keys() {
        let root = temp_root("types");
        std::fs::write(
            settings_path(&root),
            r#"{ "lang": 42, "updateCheckMinutes": "abc", "evil": true, "theme": "dark" }"#,
        )
        .unwrap();
        let s = read(&root);
        assert_eq!(s["lang"], json!("auto"), "number must not override a string");
        assert_eq!(s["updateCheckMinutes"], json!(20), "string must not override a number");
        assert_eq!(s["theme"], json!("dark"), "matching type is kept");
        assert!(s.get("evil").is_none(), "unknown keys never enter the merged view");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_writes_only_known_keys_with_trailing_newline() {
        let root = temp_root("save");
        let r = save(&root, &json!({ "theme": "dark", "evil": 1, "updateCheckMinutes": "x" }));
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["settings"]["theme"], json!("dark"));
        assert_eq!(r["settings"]["updateCheckMinutes"], json!(20));

        let raw = std::fs::read_to_string(settings_path(&root)).unwrap();
        assert!(raw.ends_with("}\n"), "JS writes a trailing newline");
        assert!(!raw.contains("evil"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_is_a_merge_not_a_replace() {
        let root = temp_root("merge");
        save(&root, &json!({ "theme": "dark" }));
        save(&root, &json!({ "notifications": false }));
        let s = read(&root);
        assert_eq!(s["theme"], json!("dark"));
        assert_eq!(s["notifications"], json!(false));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_appends_an_audit_record() {
        let root = temp_root("audit");
        ensure_store(&root, true).unwrap();
        save(&root, &json!({ "lang": "zh" }));
        let log = std::fs::read_to_string(audit_path(&root)).unwrap();
        assert_eq!(log.lines().count(), 1);
        let rec: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(rec["kind"], json!("settings"));
        assert_eq!(rec["action"], json!("settings-save"));
        assert_eq!(rec["assetName"], json!("lang"));
        assert!(rec["ts"].as_str().unwrap().ends_with('Z'));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_audit_is_newest_first_and_filterable() {
        let root = temp_root("read-audit");
        ensure_store(&root, true).unwrap();
        let lines = [
            r#"{"ts":"1","kind":"scan","action":"scan"}"#,
            r#"{"ts":"2","kind":"settings","action":"settings-save"}"#,
            r#"{"ts":"3","kind":"mcp","action":"upsert","agentId":"a1"}"#,
        ];
        std::fs::write(audit_path(&root), lines.join("\n") + "\n").unwrap();

        let all = read_audit(&root, 10, None, None, None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0]["ts"], json!("3"), "newest first");
        assert_eq!(all[2]["ts"], json!("1"));

        assert_eq!(read_audit(&root, 10, Some("settings"), None, None).len(), 1);
        assert_eq!(read_audit(&root, 10, None, Some("a1"), None).len(), 1);
        assert_eq!(read_audit(&root, 10, None, Some("nope"), None).len(), 0);
        assert_eq!(read_audit(&root, 2, None, None, None).len(), 2, "limit applies");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_transactions_reads_tx_json_and_sorts_desc() {
        let root = temp_root("txs");
        for tx in ["tx-b", "tx-a"] {
            let dir = Path::new(&root).join("backups").join(tx);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("tx.json"), format!(r#"{{"txId":"{}"}}"#, tx)).unwrap();
        }
        let txs = list_transactions(&root, 10);
        assert_eq!(txs.len(), 2);
        assert_eq!(txs[0]["txId"], json!("tx-b"));
        assert_eq!(txs[0]["hasBackup"], json!(true));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn baseline_round_trip_reports_added_changed_gone() {
        let root = temp_root("baseline");
        ensure_store(&root, true).unwrap();
        let first = vec![
            ("a".to_string(), "fp1".to_string()),
            ("b".to_string(), "fp2".to_string()),
        ];

        // No baseline yet: hasBaseline false, everything looks "added".
        let d = baseline_diff(&root, &first);
        assert_eq!(d["hasBaseline"], json!(false));
        assert_eq!(d["added"].as_array().unwrap().len(), 2);

        let r = set_baseline(&root, &first);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["count"], json!(2));

        let d = baseline_diff(&root, &first);
        assert_eq!(d["hasBaseline"], json!(true));
        assert_eq!(d["changed"].as_array().unwrap().len(), 0);

        let second = vec![
            ("a".to_string(), "fp1-CHANGED".to_string()),
            ("c".to_string(), "fp3".to_string()),
        ];
        let d = baseline_diff(&root, &second);
        assert_eq!(d["changed"], json!(["a"]));
        assert_eq!(d["added"], json!(["c"]));
        assert_eq!(d["gone"], json!(["b"]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn atomic_write_replaces_existing_content() {
        let root = temp_root("atomic");
        let p = Path::new(&root).join("x.json");
        atomic_write(&p, "one").unwrap();
        atomic_write(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("agentkeeper-tmp"))
            .collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ensure_store_creates_the_tree_once() {
        let root = temp_root("tree");
        ensure_store(&root, true).unwrap();
        ensure_store(&root, true).unwrap();
        assert!(Path::new(&root).join("sandbox").is_dir());
        assert!(Path::new(&root).join("backups").is_dir());
        assert!(audit_path(&root).is_file());
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(meta_path(&root)).unwrap()).unwrap();
        assert_eq!(meta["version"], json!(1));
        assert_eq!(meta["sandbox"], json!(true));
        std::fs::remove_dir_all(&root).ok();
    }
}
