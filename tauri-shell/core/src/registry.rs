//! `projects.json` registry — loads `<userData>/agentkeeper/projects.json`,
//! hands the same instance to every IPC handler, and seeds on first run.
//!
//! One registry is created per app run and populated lazily by
//! [`ensure_loaded`], so every handler sees the same `OnceLock<Mutex<..>>`.
//!
//! ## Why every byte matters
//!
//! One store directory: `load()` reads the file, `save()` rewrites it, so any
//! formatting difference (key order, indent, a trailing newline) would show up
//! as churn in the audit trail and as a needless diff even when nothing
//! changed. [`render`] therefore keeps one fixed byte layout: two-space
//! indent, object key order preserved, **no trailing newline**.
//!
//! ## Quirks kept on purpose
//!
//! * Duplicate detection runs **before** the existence check and compares
//!   `dir.to_lowercase()` on both sides, so re-adding a
//!   project whose directory was deleted reports `project already registered`,
//!   not `directory not found`.
//! * `dir` is resolved lexically — never canonicalised. `fs::canonicalize` is
//!   deliberately not used: it resolves symlinks and yields `\\?\`-prefixed
//!   verbatim paths on Windows, which would change the stored string, the id
//!   hash and every consumer of `p.dir`.
//! * `id` is `proj-` + the slug of `name || basename(abs)` capped at 32
//!   UTF-16 code units + `-` + the first 6 hash chars; the cap is UTF-16
//!   based, hence `model::js_slice`.
//! * `load()` swallows every error and falls back to `[]`.
//! * `remove()` saves even when the id did not match.
//! * `seed()` iterates in directory-listing order — OS order, never sorted —
//!   and returns two empty arrays for a missing/unreadable root.
//!
//! ## Demo root
//!
//! [`seed`] takes the demo root as `root_dir`; `lib.rs` resolves it (dev
//! layout, `AK_DEMO_PROJECTS`, or the bundled resource dir). Keeping path
//! resolution out of `core/` is what lets the headless `ak-scan` binary reuse
//! this module.

use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::model::{hash16, iso_from_millis, js_slice, norm_str, slug};
use super::settings;

/* ------------------------------------------------------------------ */
/* singleton state                                                     */
/* ------------------------------------------------------------------ */

/// The `ProjectRegistry`: one file path plus the array that was loaded from
/// it. `file` is empty until the first bind, which is how "not loaded yet" is
/// told apart from "loaded from an empty store".
struct Registry {
    file: PathBuf,
    projects: Vec<Value>,
}

fn cell() -> &'static Mutex<Registry> {
    static CELL: OnceLock<Mutex<Registry>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(Registry {
            file: PathBuf::new(),
            projects: Vec::new(),
        })
    })
}

/// A poisoned lock means another thread panicked mid-mutation; the stored
/// array is still structurally valid, so continue with it rather than turning
/// every later project call into a panic.
fn state() -> MutexGuard<'static, Registry> {
    cell().lock().unwrap_or_else(|e| e.into_inner())
}

fn projects_file(store_root: &str) -> PathBuf {
    Path::new(store_root).join("projects.json")
}

fn load_from(file: &Path) -> Vec<Value> {
    // Every failure — missing file, invalid JSON, a
    // `projects` value that is not an array — collapses to an empty list. No
    // error is ever surfaced to the renderer.
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|doc| doc.get("projects").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn bind(r: &mut Registry, store_root: &str) {
    r.file = projects_file(store_root);
    r.projects = load_from(&r.file);
}

/// Load the registry on first use, exactly once per process.
///
/// The file is read at startup and never re-read,
/// so two calls cannot disagree with an in-memory `add()` that happened in
/// between. A second call with a different root is ignored on purpose — in
/// production there is only one store root; tests use [`reload`].
pub fn ensure_loaded(store_root: &str) {
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        bind(&mut r, store_root);
    }
}

/// Drop the in-memory list and re-read `projects.json`.
///
/// Not dead code: it is the test hook the integration suites use to re-point
/// the process-wide singleton at another store root, plus the inline tests
/// below. The GUI itself never calls it — production reads the file exactly
/// once, via [`ensure_loaded`].
pub fn reload(store_root: &str) {
    let mut r = state();
    bind(&mut r, store_root);
}

/* ------------------------------------------------------------------ */
/* path helpers                                                        */
/* ------------------------------------------------------------------ */

/// Lexical resolution, filesystem-free.
///
/// Absolutise against the cwd, then collapse `.` / `..` purely as text.
/// Neither `canonicalize` nor `metadata` is involved, so a path that does not
/// exist still resolves (the caller's existence check is a separate step), and
/// symlinks are never followed into a different spelling of the same path.
fn resolve_path(dir: &str) -> String {
    let raw = Path::new(dir);
    let joined: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else if raw.has_root() {
        // Windows: `/x` and `\x` are rooted on the *current drive*.
        let mut s = String::new();
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(Component::Prefix(p)) = cwd.components().next() {
                s.push_str(&p.as_os_str().to_string_lossy());
            }
        }
        s.push_str(dir);
        PathBuf::from(s)
    } else {
        std::env::current_dir().unwrap_or_default().join(raw)
    };

    let mut prefix: Option<String> = None;
    let mut rooted = false;
    let mut parts: Vec<String> = Vec::new();
    for c in joined.components() {
        match c {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() && !rooted {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
        }
    }
    // `C:foo` (drive-relative) cannot be resolved without the per-drive cwd
    // the OS keeps in the environment. The drive root is the closest truthful
    // answer and keeps the stored value absolute.
    if !rooted && prefix.as_deref().map(|p| p.contains(':')).unwrap_or(false) {
        rooted = true;
    }

    let sep = if cfg!(windows) { '\\' } else { '/' };
    let mut out = String::new();
    if let Some(p) = prefix {
        out.push_str(&p);
    }
    if rooted {
        out.push(sep);
    }
    let mut first = true;
    for part in parts {
        if !first {
            out.push(sep);
        }
        first = false;
        out.push_str(&part);
    }
    if out.is_empty() {
        // Only reachable if the cwd is unreadable and `dir` was empty.
        out.push('.');
    }
    out
}

/// Basename, with the root behaviour preserved.
///
/// `model::basename` falls back to the whole path when there is no final
/// component; a drive root returns `''`. The registry feeds this
/// value into both `name` and the id slug, so the distinction is real.
fn js_basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Directory check — follows links, swallows errors.
fn exists_dir(p: &str) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    iso_from_millis(ms)
}

/* ------------------------------------------------------------------ */
/* save / load                                                         */
/* ------------------------------------------------------------------ */

/// Two-space indent, no trailing newline.
///
/// `serde_json` is built with `preserve_order`, so both the top-level wrapper
/// and the loaded project objects keep the exact stored key order.
pub fn render(projects: &[Value]) -> String {
    serde_json::to_string_pretty(&json!({ "projects": projects }))
        .unwrap_or_else(|_| "{\"projects\":[]}".to_string())
}

/// `save()` — mkdir, write temp, rename. Shares
/// `settings::atomic_write` so both stores have the same durability story;
/// only the bytes differ, and those are pinned by `render`.
fn save(r: &Registry) -> std::io::Result<()> {
    settings::atomic_write(&r.file, &render(&r.projects))
}

/* ------------------------------------------------------------------ */
/* public registry API                                                 */
/* ------------------------------------------------------------------ */

/// `list()` — a copy, so a caller cannot mutate the registry's array.
pub fn list() -> Vec<Value> {
    state().projects.clone()
}

/// `get(id)` — identity comparison on strings, `null` when nothing matches.
pub fn get(id: &str) -> Option<Value> {
    state()
        .projects
        .iter()
        .find(|p| p.get("id").and_then(Value::as_str) == Some(id))
        .cloned()
}

/// `add(dir, name)` — the full sequence.
pub fn add(dir: &str, name: Option<&str>) -> Value {
    let abs = resolve_path(dir);
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        // Only possible if a caller skipped `ensure_loaded`; refusing beats
        // silently creating `projects.json` in the process's cwd.
        return json!({ "ok": false, "error": "registry not initialised" });
    }

    // Case-insensitive, and deliberately ahead of the existence check.
    let needle = abs.to_lowercase();
    let duplicate = r.projects.iter().any(|p| {
        p.get("dir")
            .and_then(Value::as_str)
            .map(|d| d.to_lowercase() == needle)
            .unwrap_or(false)
    });
    if duplicate {
        return json!({ "ok": false, "error": "project already registered" });
    }

    if !exists_dir(&abs) {
        return json!({ "ok": false, "error": format!("directory not found: {}", abs) });
    }

    // `name || path.basename(abs)` — an absent *or empty* name is falsy.
    let base = js_basename(&abs);
    let chosen: &str = match name {
        Some(n) if !n.is_empty() => n,
        _ => &base,
    };
    let id = format!(
        "proj-{}-{}",
        js_slice(&slug(chosen), 32),
        &hash16(&abs)[..6]
    );
    let project = json!({
        "id": id,
        "name": chosen,
        "dir": abs,
        "addedAt": now_iso(),
        "tags": []
    });

    r.projects.push(project.clone());
    // The push stays even if the write fails: the failure happens
    // after the array was already extended.
    if let Err(e) = save(&r) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    json!({ "ok": true, "project": project })
}

/// `remove(id)` — always saves, even when nothing matched.
pub fn remove(id: &str) -> Value {
    let mut r = state();
    if r.file.as_os_str().is_empty() {
        return json!({ "ok": false, "error": "registry not initialised" });
    }
    let before = r.projects.len();
    r.projects
        .retain(|p| p.get("id").and_then(Value::as_str) != Some(id));
    let after = r.projects.len();
    if let Err(e) = save(&r) {
        return json!({ "ok": false, "error": e.to_string() });
    }
    json!({ "ok": after != before, "removed": before - after })
}

/// Register every project under `root_dir`.
///
/// Non-directories and dot-prefixed names are skipped before `add` is tried;
/// everything else goes through the normal duplicate/existence checks, so a
/// second run reports each project as `already registered`.
pub fn seed(store_root: &str, root_dir: &str) -> Value {
    ensure_loaded(store_root);
    let mut added: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();

    if root_dir.is_empty() {
        return json!({ "added": added, "skipped": skipped });
    }
    let root = Path::new(root_dir);
    if !root.exists() {
        return json!({ "added": added, "skipped": skipped });
    }
    // A file here (or a permission error) yields empty arrays, not an error.
    let Ok(entries) = std::fs::read_dir(root) else {
        return json!({ "added": added, "skipped": skipped });
    };

    // `readdirSync` order is OS order; sorting would change which project is
    // added first and therefore the order inside projects.json.
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Dirent::isDirectory() is false for a junction/symlink, which is why
        // a linked demo project is skipped.
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir || name.starts_with('.') {
            continue;
        }
        let dir = norm_str(&root.join(&name));
        let r = add(&dir, Some(&name));
        if r.get("ok").and_then(Value::as_bool) == Some(true) {
            added.push(r.get("project").cloned().unwrap_or(Value::Null));
        } else {
            skipped.push(json!({
                "dir": dir,
                "reason": r.get("error").cloned().unwrap_or(Value::Null)
            }));
        }
    }
    json!({ "added": added, "skipped": skipped })
}

/* ------------------------------------------------------------------ */
/* tests                                                               */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ak-registry-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// The literal two-space-indent shape for a fixed project.
    #[test]
    fn render_uses_the_two_space_indent_shape() {
        let projects = vec![json!({
            "id": "proj-demo-a1b2c3",
            "name": "demo",
            "dir": "C:\\work\\demo",
            "addedAt": "2026-10-19T13:22:24.297Z",
            "tags": []
        })];
        let expected = "{\n  \"projects\": [\n    {\n      \"id\": \"proj-demo-a1b2c3\",\n      \"name\": \"demo\",\n      \"dir\": \"C:\\\\work\\\\demo\",\n      \"addedAt\": \"2026-10-19T13:22:24.297Z\",\n      \"tags\": []\n    }\n  ]\n}";
        assert_eq!(render(&projects), expected);
        assert!(
            !render(&projects).ends_with('\n'),
            "JSON.stringify adds no newline"
        );
        assert_eq!(render(&[]), "{\n  \"projects\": []\n}");
    }

    #[test]
    fn resolve_is_lexical_and_never_verbatim() {
        let abs = resolve_path("C:/work/./agentkeeper/../agentkeeper/src");
        assert!(
            !abs.starts_with(r"\\?\"),
            "must never produce a verbatim path: {}",
            abs
        );
        assert!(!abs.contains(".."));
        assert!(!abs.contains("/."));
        if cfg!(windows) {
            assert_eq!(abs, r"C:\work\agentkeeper\src");
            // A path that does not exist still resolves; no filesystem access.
            assert_eq!(resolve_path(r"C:\nope\nope\..\x"), r"C:\nope\x");
            // Trailing separators are dropped, like Node's path.resolve.
            assert_eq!(resolve_path("C:/work/agentkeeper/"), r"C:\work\agentkeeper");
            // Expected values checked against this host's lexical resolution.
            for (input, want) in [
                (r"C:\work\demo\..\demo\.\sub", r"C:\work\demo\sub"),
                (r"C:\work\demo\", r"C:\work\demo"),
                (r"C:\", r"C:\"),
                (r"\\server\share\a\..\b", r"\\server\share\b"),
            ] {
                assert_eq!(resolve_path(input), want, "resolve {:?}", input);
            }
            // `path.basename` on a drive root is '', not the path itself.
            assert_eq!(js_basename(r"C:\"), "");
            assert_eq!(js_basename(r"C:\a\b\"), "b");
        }
    }

    /// Everything that touches the process-wide singleton lives in this one
    /// test: cargo runs test functions in parallel, and two of them pointing
    /// the same `OnceLock` at different temp roots would race.
    #[test]
    fn registry_round_trip_load_add_duplicate_remove_seed() {
        let root = temp_root("roundtrip");
        let store = Path::new(&root).join("store");
        std::fs::create_dir_all(&store).unwrap();
        let store_s = store.to_string_lossy().into_owned();
        let file = store.join("projects.json");

        let demo = store.join("work").join("demo");
        std::fs::create_dir_all(&demo).unwrap();
        let demo_s = demo.to_string_lossy().into_owned();

        // A missing projects.json loads as an empty registry.
        reload(&store_s);
        assert!(list().is_empty());

        // --- add ------------------------------------------------------
        let a = add(&demo_s, None);
        assert_eq!(a["ok"], json!(true), "add must succeed: {}", a);
        let p = a["project"].clone();
        let expected_dir = resolve_path(&demo_s);
        assert_eq!(p["dir"], json!(expected_dir));
        assert_eq!(p["name"], json!("demo"), "basename fallback");
        assert_eq!(p["tags"], json!([]));
        assert_eq!(
            p["id"],
            json!(format!("proj-demo-{}", &hash16(&expected_dir)[..6]))
        );
        let added_at = p["addedAt"].as_str().unwrap().to_string();
        assert!(
            added_at.ends_with('Z') && added_at.len() == 24 && &added_at[19..20] == ".",
            "ISO with millisecond precision: {}",
            added_at
        );
        assert_eq!(list().len(), 1);
        assert_eq!(get(p["id"].as_str().unwrap()).unwrap()["id"], p["id"]);

        // --- byte-identical file -------------------------------------
        let body = std::fs::read_to_string(&file).unwrap();
        let q = |v: &Value| serde_json::to_string(v).unwrap();
        let expected = format!(
            "{{\n  \"projects\": [\n    {{\n      \"id\": {},\n      \"name\": {},\n      \"dir\": {},\n      \"addedAt\": {},\n      \"tags\": []\n    }}\n  ]\n}}",
            q(&p["id"]),
            q(&p["name"]),
            q(&p["dir"]),
            q(&p["addedAt"])
        );
        assert_eq!(body, expected);
        assert!(!body.ends_with('\n'), "save() writes no trailing newline");
        assert_eq!(body, render(&list()), "the file is render(list())");

        // --- duplicate, case-insensitive ------------------------------
        let mixed = demo_s.replace("demo", "DEMO");
        let dup = add(&mixed, Some("other"));
        assert_eq!(dup["ok"], json!(false));
        assert_eq!(dup["error"], json!("project already registered"));
        assert_eq!(list().len(), 1, "a rejected add must not mutate state");

        // --- duplicate of a missing directory reports duplicate ------
        let gone = store.join("work").join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        let gone_s = gone.to_string_lossy().into_owned();
        assert_eq!(add(&gone_s, None)["ok"], json!(true));
        std::fs::remove_dir_all(&gone).unwrap();
        let again = add(&gone_s, None);
        assert_eq!(
            again["error"],
            json!("project already registered"),
            "the duplicate check precedes the existence check"
        );

        // --- add of a genuinely missing directory --------------------
        let missing = store.join("work").join("absent");
        let missing_s = missing.to_string_lossy().into_owned();
        let m = add(&missing_s, None);
        assert_eq!(m["ok"], json!(false));
        assert_eq!(
            m["error"],
            json!(format!("directory not found: {}", resolve_path(&missing_s)))
        );

        // --- add with an explicit name -------------------------------
        let named = store.join("work").join("folder");
        std::fs::create_dir_all(&named).unwrap();
        let named_s = named.to_string_lossy().into_owned();
        let n = add(&named_s, Some("Custom Name"));
        assert_eq!(n["project"]["name"], json!("Custom Name"));
        assert_eq!(
            n["project"]["id"],
            json!(format!(
                "proj-custom-name-{}",
                &hash16(&resolve_path(&named_s))[..6]
            ))
        );
        // An empty name is falsy and falls back to the basename.
        let empty = add(&named_s, Some(""));
        assert_eq!(empty["ok"], json!(false), "same dir, so duplicate");
        assert_eq!(list().len(), 3);

        // --- remove ---------------------------------------------------
        let id = p["id"].as_str().unwrap().to_string();
        let rm = remove(&id);
        assert_eq!(rm, json!({ "ok": true, "removed": 1 }));
        assert!(get(&id).is_none());
        assert_eq!(list().len(), 2);

        // Removing an unknown id still writes the file (解析口径).
        let before = std::fs::read_to_string(&file).unwrap();
        let rm2 = remove("proj-nope-000000");
        assert_eq!(rm2, json!({ "ok": false, "removed": 0 }));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert_eq!(before, render(&list()));

        // --- seed -----------------------------------------------------
        let seed_root = store.join("demo-projects");
        for name in ["alpha", "beta", ".hidden"] {
            std::fs::create_dir_all(seed_root.join(name)).unwrap();
        }
        std::fs::write(seed_root.join("note.txt"), "not a project").unwrap();
        let seed_s = seed_root.to_string_lossy().into_owned();

        let s = seed(&store_s, &seed_s);
        let added = s["added"].as_array().unwrap();
        let mut names: Vec<String> = added
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(
            s["skipped"],
            json!([]),
            "dot dirs and files are skipped, not reported"
        );
        assert_eq!(list().len(), 4);

        let s2 = seed(&store_s, &seed_s);
        assert_eq!(s2["added"], json!([]));
        assert_eq!(s2["skipped"].as_array().unwrap().len(), 2);
        assert_eq!(
            s2["skipped"][0]["reason"],
            json!("project already registered")
        );
        assert_eq!(
            s2["skipped"][0]["dir"],
            json!(norm_str(&seed_root.join("alpha")))
        );

        // Missing root, empty root argument, and a file as root: two empty
        // arrays, never an error.
        assert_eq!(
            seed(&store_s, &store.join("nope").to_string_lossy()),
            json!({ "added": [], "skipped": [] })
        );
        assert_eq!(seed(&store_s, ""), json!({ "added": [], "skipped": [] }));
        assert_eq!(
            seed(&store_s, &seed_root.join("note.txt").to_string_lossy()),
            json!({ "added": [], "skipped": [] })
        );

        // --- reload picks the file back up ----------------------------
        reload(&store_s);
        assert_eq!(list().len(), 4);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), render(&list()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn load_swallows_bad_json_and_non_array_projects() {
        let root = temp_root("load");
        let file = Path::new(&root).join("projects.json");

        std::fs::write(&file, "{not json").unwrap();
        assert!(load_from(&file).is_empty());

        std::fs::write(&file, r#"{"projects":{}}"#).unwrap();
        assert!(load_from(&file).is_empty());

        std::fs::write(&file, r#"{"projects":[]}"#).unwrap();
        assert!(load_from(&file).is_empty());

        // Arbitrary extra fields survive a load untouched.
        std::fs::write(
            &file,
            r#"{"projects":[{"id":"x","dir":"C:\\a","extra":1}]}"#,
        )
        .unwrap();
        let loaded = load_from(&file);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["extra"], json!(1));

        assert!(load_from(&Path::new(&root).join("absent.json")).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
