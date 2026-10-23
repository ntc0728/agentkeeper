//! IPC / data contract, asset lifecycle, project registry and the format layer.
//!
//! * contract — renderer-facing response shapes, i.e. the part of the suite that
//!   exists purely to stop an envelope from changing under the UI. The suite
//!   covers the IPC contract sections 1-7, 9-11 and the settings / store
//!   sections. The `#[tauri::command]` fns in `lib.rs`
//!   are private and take a `tauri::AppHandle`, so an integration test cannot
//!   call them: each test calls *the same core functions the command calls* and,
//!   where the command adds envelope logic of its own (a clamp, a filter, a fixed
//!   key list), reproduces those few lines with a comment naming the command.
//!   The invariant under test is the shape, which is what the renderer reads.
//! * assets — scan → read asset → save → rescan, and the write gates that sit
//!   between them. Covers the asset lifecycle and the format layer:
//!   sections 1-7. Everything runs in a temp "user
//!   home" plus a temp store root; nothing touches the real profile.
//! * registry — add / remove / list / seed / resolve and the `projects.json`
//! byte shape. Covers the
//! `ProjectRegistry` behaviours the rest of the suite relies on throughout.
//! * formats — YAML, frontmatter and JSONC: the format layer the write path is
//! built on. These are pure functions —
//!   no store, no singleton, no filesystem — so they need no temp root. TOML
//!   section surgery is *not* repeated here: `core::toml_splice`'s unit tests
//!   assert the same fixtures (and more) as exact strings, so the integration
//!   copies were pure duplication.
//!
//! `core::registry` and `core::assets` are process-global singletons (they stand
//! in for the registry instance and the last scan), so any test
//! that binds or seeds them holds `common::serial()` first.

mod common;

use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::store::Store;
use agentkeeper_shell_lib::core::update::{manager, version};
use agentkeeper_shell_lib::core::{
    assets, discover, frontmatter, jsonc, registry, scan, settings, store, yaml,
};
use serde_json::{json, Value};
use std::path::PathBuf;

use common::{cleanup, exists, join, read, temp_root, write};

/* ------------------------------------------------------------------ */
/* renderer-facing contract                                            */
/* ------------------------------------------------------------------ */
/// A store root plus the temp dir that owns it.
struct Fixture {
    dir: PathBuf,
    root: String,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let dir = temp_root(tag);
        let root = store::native_str(&dir);
        settings::ensure_store(&root).expect("ensure_store");
        Fixture { dir, root }
    }
    fn path(&self, parts: &[&str]) -> String {
        join(&self.root, parts)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

/* ------------------------------------------------------------------ */
/* settings                                                            */
/* ------------------------------------------------------------------ */

// The defaults come back as a flat object:
// "settings defaults are complete".
//
// `ak:get-settings` reads settings verbatim — a flat object, never
// an `{ok, settings}` envelope.
#[test]
fn get_settings_is_a_flat_defaults_object() {
    let fx = Fixture::new("settings-defaults");

    let s = settings::read(&fx.root);
    assert!(s.is_object(), "settings must be a plain object, not {}", s);

    let mut keys: Vec<&str> = s.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "lang",
            "notifications",
            "showLocalStats",
            "theme",
            "updateApiBase",
            "updateAssetKeyword",
            "updateBeta",
            "updateCheck",
            "updateCheckMinutes",
            "updateInstallSilent",
            "updateProxy",
            "updateRepo",
            "updateToken"
        ],
        "{:?}",
        keys
    );

    // `sandbox` was a session-scoped flag, never a settings key; the whole
    // feature is retired, so the key must not reappear here.
    assert!(s.get("sandbox").is_none(), "sandbox is not a settings key");

    // Keys that are gone. They must not come back, or a
    // stale settings.json starts controlling the write path again.
    for gone in [
        "keepBackups",
        "autoPrune",
        "gitBackup",
        "updateManifestUrl",
        "marketplaces",
    ] {
        assert!(
            s.get(gone).is_none(),
            "{} was removed and must stay removed",
            gone
        );
    }

    assert_eq!(s["showLocalStats"], json!(true));
    assert_eq!(s["updateRepo"], json!(manager::BUILT_IN_REPO));
    assert_eq!(s["updateCheckMinutes"], json!(20));
}

// A value whose type does not match the
// default" — a hand-edited file must not be able to schedule a check every
// "abc" minutes.
#[test]
fn a_hand_edited_settings_file_cannot_change_a_value_type() {
    let fx = Fixture::new("settings-types");
    write(
        &fx.path(&["settings.json"]),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "lang": 5,                     // number where a string belongs
                "updateCheckMinutes": "abc",   // string where a number belongs
                "theme": "dark",               // valid override
                "updateCheck": null,           // JS `typeof null === 'object'`
                "unknownKey": "smuggled"       // not in defaults
            }))
            .unwrap()
        ),
    );

    let s = settings::read(&fx.root);
    assert_eq!(
        s["lang"],
        json!("auto"),
        "type mismatch falls back to the default"
    );
    assert_eq!(s["updateCheckMinutes"], json!(20));
    assert_eq!(
        s["theme"],
        json!("dark"),
        "same-type overrides are honoured"
    );
    assert_eq!(
        s["updateCheck"],
        json!(true),
        "null never overrides a boolean"
    );
    assert!(
        s.get("unknownKey").is_none(),
        "unknown keys never reach the renderer"
    );
}

// A save returns the ok envelope, and
// "saveSettings ignores unknown keys".
#[test]
fn save_settings_returns_ok_plus_settings_and_filters_the_patch() {
    let fx = Fixture::new("settings-save");

    let r = settings::save(
        &fx.root,
        &json!({
            "lang": "zh",
            "updateCheckMinutes": 45,
            "updateCheckMinutesWrong": 1,
            "updateCheck": "yes",      // string where a bool belongs
            "notAKey": true            // not in defaults
        }),
    );

    assert_eq!(r["ok"], json!(true), "{}", r);
    let saved = &r["settings"];
    assert!(saved.is_object(), "the envelope carries the new settings");
    assert!(
        r.get("error").is_none(),
        "a successful save has no error field"
    );

    assert_eq!(saved["lang"], json!("zh"));
    assert_eq!(saved["updateCheckMinutes"], json!(45));
    assert_eq!(
        saved["updateCheck"],
        json!(true),
        "the type mismatch was ignored"
    );
    assert!(
        saved.get("notAKey").is_none(),
        "unknown keys are dropped, not stored"
    );

    // The write reached disk with the fixed byte shape: 2-space pretty + "\n".
    let on_disk = common::read(&fx.path(&["settings.json"]));
    assert!(on_disk.ends_with("\n"), "settings.json ends with a newline");
    assert!(
        on_disk.contains("\n  \"lang\": \"zh\""),
        "two-space indent: {}",
        on_disk
    );
    assert_eq!(
        settings::read(&fx.root)["lang"],
        json!("zh"),
        "and reads back"
    );
}

// Every settings save appends one audit record.
//
// Worth knowing: a settings save logs only the keys it actually applied, so
// `assetName` is narrower than the submitted patch. The record's
// `action`/`status`/`kind` are what the Activity page reads, so the page
// renders the same; only `assetName` can differ.
#[test]
fn save_settings_appends_one_audit_record() {
    let fx = Fixture::new("settings-audit");

    let before = settings::read_audit(&fx.root, 100, None, None, None).len();
    settings::save(&fx.root, &json!({ "lang": "zh", "updateCheck": false }));

    let rows = settings::read_audit(&fx.root, 100, None, None, None);
    assert_eq!(rows.len(), before + 1, "exactly one line per save");
    let rec = &rows[0];
    assert_eq!(rec["action"], json!("settings-save"));
    assert_eq!(rec["status"], json!("ok"));
    assert_eq!(rec["kind"], json!("settings"));
}

/* ------------------------------------------------------------------ */
/* store bootstrap                                                     */
/* ------------------------------------------------------------------ */

// "ensure creates the store tree" — an empty audit.jsonl and a meta.json,
// all of which later code assumes exist.
#[test]
fn ensure_store_creates_the_tree_the_rest_of_the_app_assumes() {
    let fx = Fixture::new("ensure-store");

    // No backup tree: the write path keeps its undo state in memory and nothing
    // on disk, so a first launch must not manufacture a `backups/`.
    assert!(
        !std::path::Path::new(&fx.path(&["backups"])).exists(),
        "no backups/ may be created"
    );
    assert_eq!(
        common::read(&fx.path(&["audit.jsonl"])),
        "",
        "audit.jsonl starts empty"
    );

    // Idempotent: bootstrap runs on every startup and on every settings write.
    settings::ensure_store(&fx.root).expect("second ensure_store");
    assert_eq!(
        common::read(&fx.path(&["audit.jsonl"])),
        "",
        "and does not truncate it"
    );
    write(
        &fx.path(&["audit.jsonl"]),
        "{}
",
    );
    settings::ensure_store(&fx.root).expect("third ensure_store");
    assert_eq!(
        common::read(&fx.path(&["audit.jsonl"])),
        "{}
"
    );

    let meta: Value =
        serde_json::from_str(&common::read(&fx.path(&["meta.json"]))).expect("meta.json");
    assert_eq!(meta["version"], json!(1));
    assert!(
        meta.get("createdAt").is_some(),
        "meta.json keeps its createdAt"
    );
    // No staging layer is created any more: writes land in the real tree.
    assert!(
        !std::path::Path::new(&fx.path(&["sandbox"])).exists(),
        "the retired staging tree must not be recreated"
    );
}

/* ------------------------------------------------------------------ */
/* collections the renderer iterates directly                          */
/* ------------------------------------------------------------------ */

// Listing projects returns a bare array. Wrapping it in
// `{ok, projects}` made app.js treat the object as the list.
#[test]
fn list_projects_is_a_bare_array_of_project_objects() {
    let _lock = common::serial();
    let fx = Fixture::new("contract-projects");
    registry::reload(&fx.root);

    let project = join(&fx.dir.to_string_lossy(), &["proj"]);
    write(&join(&project, &["README.md"]), "# p\n");
    let added = registry::add(&project, Some("Contract Project"));
    assert_eq!(added["ok"], json!(true), "{}", added);

    // `registry.list()` returns a bare array — no `{ok, projects}` envelope.
    // Projects are added through `pick_project` and read through `scan`, so
    // this asserts the shape of the only producer.
    let list = registry::list();
    assert_eq!(list.len(), 1, "{:?}", list);
    for key in ["id", "name", "dir"] {
        assert!(list[0].get(key).is_some(), "project is missing {}", key);
    }
    assert_eq!(list[0]["name"], json!("Contract Project"));
}

// The audit returns an array — `audit.filter(...)` runs
// straight on it, newest first, with the IPC-side limit clamp.
#[test]
fn audit_is_a_bare_array_newest_first_with_filters() {
    let fx = Fixture::new("contract-audit");
    write(
        &fx.path(&["audit.jsonl"]),
        &[
            r#"{"at":"a","action":"scan","kind":"skill","agentId":"workbuddy","projectId":"p1"}"#,
            r#"{"at":"b","action":"apply","kind":"mcp","agentId":"claude","projectId":"p2"}"#,
            r#"{"at":"c","action":"scan","kind":"skill","agentId":"workbuddy","projectId":"p1"}"#,
            "not json",
        ]
        .join("\n"),
    );

    let rows = settings::read_audit(&fx.root, 300, None, None, None);
    assert_eq!(rows.len(), 3, "malformed lines are skipped: {:?}", rows);
    assert_eq!(rows[0]["at"], json!("c"), "newest first");
    assert_eq!(rows[2]["at"], json!("a"));

    assert_eq!(
        settings::read_audit(&fx.root, 300, Some("mcp"), None, None).len(),
        1
    );
    assert_eq!(
        settings::read_audit(&fx.root, 300, None, Some("claude"), None).len(),
        1
    );
    assert_eq!(
        settings::read_audit(&fx.root, 300, None, None, Some("p1")).len(),
        2
    );
    assert_eq!(
        settings::read_audit(&fx.root, 300, Some("skill"), None, Some("p2")).len(),
        0,
        "filters intersect"
    );
    assert_eq!(
        settings::read_audit(&fx.root, 2, None, None, None).len(),
        2,
        "limit applies"
    );
}

// A profile written by an older build already has a `backups/` directory full
// of old transaction copies. That tree is retired, not merely unused: nothing in
// this build can read it, so leaving a folder that looks like "undo" would be a
// lie about what the app can do. `ensure_store` removes it, and must keep doing
// so across repeated launches.
#[test]
fn ensure_store_removes_a_legacy_backups_directory() {
    let fx = Fixture::new("ensure-legacy");
    let legacy = fx.path(&["backups", "tx-0000000001-aa", "tx.json"]);
    write(&legacy, r#"{"txId":"tx-0000000001-aa"}"#);
    assert!(std::path::Path::new(&legacy).is_file());

    settings::ensure_store(&fx.root).expect("ensure_store over a legacy tree");
    assert!(
        !std::path::Path::new(&fx.path(&["backups"])).exists(),
        "the retired backup tree must be removed"
    );

    settings::ensure_store(&fx.root).expect("and again");
    assert!(
        !std::path::Path::new(&fx.path(&["backups"])).exists(),
        "and must not come back"
    );
    // The files the app actually reads survive the cleanup.
    assert!(std::path::Path::new(&fx.path(&["audit.jsonl"])).is_file());
}

// A write mints no directory of any kind; the journal line is the only trace.
#[test]
fn a_write_leaves_only_an_audit_line_and_no_transaction_directory() {
    let fx = Fixture::new("contract-tx");
    let file = fx.path(&["project", "a.md"]);

    let tx = store::Store::new(&fx.root).transaction(
        &[store::Op::write(&file, "x\n")],
        &json!({ "kind": "skill", "action": "create", "assetName": "a" }),
    );
    assert_eq!(tx["ok"], json!(true), "{}", tx);

    assert!(
        !std::path::Path::new(&fx.path(&["backups"])).exists(),
        "no backups/ may be created by a write"
    );
    let rows = settings::read_audit(&fx.root, 300, None, None, None);
    assert_eq!(rows.len(), 1, "{:?}", rows);
    assert_eq!(rows[0]["status"], json!("ok"));
    assert_eq!(rows[0]["targets"], json!([file]));
    // `hasBackup` was the flag the Activity page keyed its undo button off; it
    // must not come back in any other form either.
    assert!(rows[0].get("hasBackup").is_none(), "{}", rows[0]);
}

/* ------------------------------------------------------------------ */
/* single-asset + stats envelopes                                      */
/* ------------------------------------------------------------------ */

// Reading an asset returns {ok, asset, content} — exactly
// three keys; the drawer destructures all three.
#[test]
fn read_asset_returns_exactly_ok_asset_and_content() {
    let _lock = common::serial();
    let fx = Fixture::new("contract-read-asset");
    registry::reload(&fx.root);

    let home = join(&fx.dir.to_string_lossy(), &["home"]);
    write(
        &join(&home, &[".workbuddy", "memory", "MEMORY.md"]),
        "# memory\nseed\n",
    );
    let payload = {
        let projects = agentkeeper_shell_lib::core::scan::read_projects(&fx.root);
        let (payload, _timings) = agentkeeper_shell_lib::core::scan::scan_payload(&home, &projects);
        payload
    };
    let asset = payload["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["kind"] == "memory"))
        .cloned()
        .expect("a memory asset");
    let id = asset["id"].as_str().unwrap().to_string();

    let r = assets::read_asset(&home, &fx.root, &id);
    let mut keys: Vec<&str> = r.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["asset", "content", "ok"], "{:?}", keys);
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["content"], json!("# memory\nseed\n"));
    assert_eq!(r["asset"]["id"], json!(id));
}

// The local stats return ok + counts. Counted from
// audit.jsonl on every call; nothing is cached. `rollbacks` is gone — there is
// no rollback left to count.
#[test]
fn local_stats_counts_the_journal() {
    let fx = Fixture::new("contract-stats");
    write(
        &fx.path(&["audit.jsonl"]),
        &[
            r#"{"action":"scan","status":"ok"}"#,
            r#"{"action":"scan","status":"ok"}"#,
            r#"{"action":"apply","status":"ok"}"#,
            r#"{"action":"rollback","status":"ok"}"#,
            r#"{"action":"write","status":"failed"}"#,
            "{ malformed",
        ]
        .join("\n"),
    );

    // `ak:local-stats` — the counting lives in the command.
    let raw = common::read(&fx.path(&["audit.jsonl"]));
    let (mut scans, mut writes, mut failed) = (0u64, 0u64, 0u64);
    for line in raw.split('\n').filter(|l| !l.is_empty()) {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match rec.get("action").and_then(|v| v.as_str()) {
            Some("scan") => scans += 1,
            Some("apply") => writes += 1,
            _ => {}
        }
        if rec.get("status").and_then(|v| v.as_str()) == Some("failed") {
            failed += 1;
        }
    }
    let payload = json!({
        "ok": true,
        "counts": { "scans": scans, "writes": writes, "failed": failed }
    });

    assert_eq!(payload["ok"], json!(true));
    assert_eq!(payload["counts"]["scans"], json!(2));
    assert_eq!(
        payload["counts"]["writes"],
        json!(1),
        "only 'apply' counts as a write"
    );
    assert_eq!(payload["counts"]["failed"], json!(1));
    assert_eq!(
        payload["counts"].as_object().unwrap().len(),
        3,
        "exactly three counters: a rollback counter would be a lie about the feature"
    );
}

/* ------------------------------------------------------------------ */
/* updater input handling                                              */
/* ------------------------------------------------------------------ */

// These are pure functions with no network and no DPAPI, so they are safe to
// pin here. `UpdateManager` itself is a process-global singleton bound to one
// root, so its status payload is deliberately not asserted in this suite.

// The update repo is normalised — `owner/repo` only.
#[test]
fn update_repo_is_normalised_to_owner_slash_repo() {
    assert_eq!(
        manager::normalize_repo("ntc0728/agentkeeper"),
        "ntc0728/agentkeeper"
    );
    assert_eq!(manager::normalize_repo("  a/b  "), "a/b", "trimmed");
    assert_eq!(manager::normalize_repo("a/b/c"), "");
    assert_eq!(manager::normalize_repo("a"), "");
    assert_eq!(manager::normalize_repo("a/"), "");
    assert_eq!(
        manager::normalize_repo("a b/c"),
        "",
        "whitespace inside is rejected"
    );
    assert_eq!(manager::normalize_repo("https://github.com/a/b"), "");
}

// The update interval is clamped — 1..1440 minutes, default 20.
#[test]
fn update_interval_is_clamped_to_a_day() {
    assert_eq!(manager::clamp_minutes(&json!(20)), 20);
    assert_eq!(manager::clamp_minutes(&json!(0)), manager::MIN_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(-5)), manager::MIN_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(100000)), manager::MAX_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(2.6)), 3, "Math.round");
    // `Number(x)` semantics, not "unparseable means default": null and "" are
    // 0, so they clamp to the floor; only NaN (an object/array) hits the
    // default of 20.
    assert_eq!(manager::clamp_minutes(&json!(null)), manager::MIN_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!("")), manager::MIN_MINUTES);
    assert_eq!(
        manager::clamp_minutes(&json!("abc")),
        manager::DEFAULT_MINUTES
    );
    assert_eq!(manager::clamp_minutes(&json!({})), manager::DEFAULT_MINUTES);
    assert_eq!(
        manager::clamp_minutes(&json!("45")),
        45,
        "a numeric string parses"
    );
}

// The installer is chosen for this platform — suffix/keyword rules
// and the filename sanitiser the install step relies on.
#[test]
fn update_installer_selection_and_file_name_rules() {
    let assets = json!([
        { "name": "AgentKeeper-1.2.3.msi", "size": 10 },
        { "name": "AgentKeeper-1.2.3.exe", "size": 10 },
        { "name": "AgentKeeper-1.2.3.dmg", "size": 10 },
        { "name": "latest.yml", "size": 1 },
        { "name": "AgentKeeper-1.2.3.exe.blockmap", "size": 1 }
    ]);
    let win = version::pick_installer(&assets, "win32", "x64").expect("a Windows installer");
    assert_eq!(win["name"], json!("AgentKeeper-1.2.3.exe"));
    assert_eq!(win["size"], json!(10), "the asset size is carried through");
    assert!(
        win.get("url").is_some(),
        "the response always has a url key, even when empty"
    );

    // Quirk deliberately preserved (`core/update/version.rs` module docs):
    // `allowed_installer_suffixes` admits .dmg/.appimage/.deb, but
    // `is_non_installer_asset` rejects every name ending in one, so macOS and
    // Linux never get an installer from `pick_installer`. The updater hides the
    // button there instead of downloading something it cannot run.
    assert!(
        version::pick_installer(&assets, "darwin", "arm64").is_none(),
        "macOS never resolves an installer"
    );
    assert!(
        version::pick_installer(&assets, "linux", "x64").is_none(),
        "Linux never resolves an installer"
    );

    assert!(
        version::is_non_installer_asset("AgentKeeper-portable.exe"),
        "portable installs nothing"
    );
    assert!(
        version::is_non_installer_asset("AgentKeeper-win.exe"),
        "another platform's exe"
    );
    assert!(version::is_non_installer_asset("AgentKeeper-1.2.3.zip"));
    assert!(!version::is_non_installer_asset("AgentKeeper-1.2.3.exe"));

    // `safe_file_name` — a hostile release name must not escape the download dir.
    assert_eq!(
        version::safe_file_name("AgentKeeper-1.2.3.exe", "win32"),
        "AgentKeeper-1.2.3.exe"
    );
    let evil = version::safe_file_name("..\\..\\evil.exe", "win32");
    assert!(!evil.contains(".."), "path traversal is stripped: {}", evil);
    assert!(
        evil.ends_with("evil.exe"),
        "the basename survives: {}",
        evil
    );
}

// The download host allowlist — only GitHub may serve an installer.
#[test]
fn update_download_hosts_are_restricted() {
    let api = version::GITHUB_API;
    assert!(
        version::assert_downloadable(
            "https://github.com/ntc0728/agentkeeper/releases/download/v1/AgentKeeper-1.2.3.exe",
            api
        )
        .is_ok(),
        "a GitHub release asset is allowed"
    );
    assert!(
        version::assert_downloadable("https://objects.githubusercontent.com/x", api).is_ok(),
        "the release CDN is allowed"
    );
    assert!(
        version::assert_downloadable("https://evil.example/AgentKeeper-1.2.3.exe", api).is_err(),
        "a third-party mirror is refused"
    );
    assert!(
        version::assert_downloadable("http://github.com/a/b", api).is_err(),
        "plain http is refused"
    );
}

// The version compare — semver with prerelease ordering.
#[test]
fn update_versions_compare_semver_with_prereleases() {
    assert_eq!(
        version::compare_versions("1.2.3", "1.2.3"),
        Some(std::cmp::Ordering::Equal)
    );
    assert_eq!(
        version::compare_versions("1.2.4", "1.2.3"),
        Some(std::cmp::Ordering::Greater)
    );
    assert_eq!(
        version::compare_versions("1.2.3", "1.10.0"),
        Some(std::cmp::Ordering::Less)
    );
    assert_eq!(
        version::compare_versions("v1.2.3", "1.2.3"),
        Some(std::cmp::Ordering::Equal),
        "v-prefix"
    );
    assert_eq!(
        version::compare_versions("1.2.3-beta.1", "1.2.3"),
        Some(std::cmp::Ordering::Less),
        "a prerelease is older than the release"
    );
    assert_eq!(version::compare_versions("not-a-version", "1.2.3"), None);
    assert!(version::is_prerelease_version("1.2.3-beta.1"));
    assert!(!version::is_prerelease_version("1.2.3"));
}

/* ------------------------------------------------------------------ */
/* discover input handling                                             */
/* ------------------------------------------------------------------ */

// `normalizeRepo` — this is the *discover* normaliser, which
// answers `null` on rejection (the update manager's returns ""). It is the
// only thing standing between a registry `sourceUrl` and a `git clone`
// target, so tricky input must not reach the shell.
#[test]
fn discover_repo_normalisation_accepts_only_owner_slash_repo() {
    assert_eq!(
        discover::normalize_repo("vercel-labs/skills"),
        Some("vercel-labs/skills".to_string()),
        "bare owner/repo"
    );
    assert_eq!(
        discover::normalize_repo("https://github.com/anthropics/skills.git"),
        Some("anthropics/skills".to_string()),
        "a full URL with .git"
    );
    assert_eq!(
        discover::normalize_repo("HTTPS://GitHub.com/A/B"),
        Some("A/B".to_string()),
        "host case is normalised away, path case is not"
    );
    assert_eq!(discover::normalize_repo("../../etc/passwd"), None);
    assert_eq!(
        discover::normalize_repo("https://evil.com/a/b"),
        None,
        "not a GitHub host"
    );
    assert_eq!(
        discover::normalize_repo("a/b/c"),
        None,
        "a third segment is rejected"
    );
    assert_eq!(
        discover::normalize_repo(""),
        None,
        "empty input is null, not ''"
    );
    assert_eq!(
        discover::normalize_repo("a/b.git.git"),
        Some("a/b.git".to_string()),
        "only one trailing .git is stripped (no /g flag)"
    );
}

// The registry rows are normalised — `shape_registry_response` caps
// and coerces every field, so a hostile registry answer cannot overflow the
// Discover page or smuggle a non-repo into `installFromRepo`.
#[test]
fn discover_shapes_registry_rows_into_the_renderer_contract() {
    let body = json!({
        "total": 1,
        "skills": [
            {
                "name": "x".repeat(300),
                "slug": "a-skill",
                "author": "",
                "description": "d".repeat(900),
                "category": "c",
                "installs": "12",
                "sourceUrl": "https://github.com/o/r.git"
            },
            { "name": "second", "sourceUrl": "https://evil.com/a/b" }
        ]
    });
    let out = discover::shape_registry_response(&body);
    assert_eq!(out["ok"], json!(true));
    assert_eq!(
        out["total"],
        json!(1),
        "total comes from the body, not the row count"
    );
    let rows = out["skills"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["slug"], json!("a-skill"));
    assert_eq!(
        rows[0]["name"].as_str().unwrap().len(),
        120,
        "name is capped"
    );
    assert_eq!(
        rows[0]["description"].as_str().unwrap().len(),
        500,
        "description is capped"
    );
    assert_eq!(
        rows[0]["installs"],
        json!(12),
        "a numeric string is coerced"
    );
    assert_eq!(rows[0]["repo"], json!("o/r"), "sourceUrl is normalised");
    assert_eq!(
        rows[1]["repo"],
        Value::Null,
        "a non-GitHub source yields null"
    );
    assert_eq!(rows[1]["installs"], json!(0), "a missing count is 0");

    // `search_url` — the query is length-capped and form-encoded.
    let url =
        discover::search_url(&json!({ "q": "a b&c", "cat": "x", "limit": 1000, "offset": -5 }));
    assert!(url.contains("q=a+b%26c"), "{}", url);
    assert!(
        url.contains("limit=100"),
        "limit is clamped to 100: {}",
        url
    );
    assert!(
        url.contains("offset=0"),
        "a negative offset clamps to 0: {}",
        url
    );
}

/* ------------------------------------------------------------------ */
/* scan → read → save → rescan                                         */
/* ------------------------------------------------------------------ */
const PROJECT_ID: &str = "proj-demo-aaaaaa";

struct AssetsFixture {
    dir: PathBuf,
    home: String,
    root: String,
    project_dir: String,
}

impl AssetsFixture {
    fn new(tag: &str) -> AssetsFixture {
        let dir = temp_root(tag);
        let home = join(&dir.to_string_lossy(), &["home"]);
        let root = join(&dir.to_string_lossy(), &["store"]);
        let project_dir = join(&dir.to_string_lossy(), &["proj"]);

        // A global workbuddy install: one skill, one memory file, one MCP config.
        write(
            &join(&home, &[".workbuddy", "skills", "home-skill", "SKILL.md"]),
            "---\nname: home-skill\ndescription: Global fixture skill.\nversion: 1.0.0\n---\n\n# Home\n",
        );
        write(
            &join(&home, &[".workbuddy", "MEMORY.md"]),
            "# Memory\n\nseed line\n",
        );
        write(
            &join(&home, &[".workbuddy", "mcp.json"]),
            "{\n  \"mcpServers\": {\n    \"home-db\": {\n      \"command\": \"node\"\n    }\n  }\n}\n",
        );

        // A project registered in projects.json, with its own skill and config.
        write(
            &join(
                &project_dir,
                &[".workbuddy", "skills", "proj-skill", "SKILL.md"],
            ),
            "---\nname: proj-skill\ndescription: Project fixture skill.\n---\n\n# Project\n",
        );
        write(
            &join(&project_dir, &[".workbuddy", "mcp.json"]),
            "{\n  \"mcpServers\": {\n    \"saas-db\": {\n      \"command\": \"node\"\n    },\n    \"saas-logs\": {\n      \"command\": \"node\"\n    }\n  }\n}\n",
        );

        std::fs::create_dir_all(&root).expect("store root");
        write(
            &join(&root, &["projects.json"]),
            &format!(
                "{{\n  \"projects\": [\n    {{\n      \"id\": \"{}\",\n      \"name\": \"demo\",\n      \"dir\": {},\n      \"addedAt\": \"2026-01-01T00:00:00.000Z\",\n      \"tags\": []\n    }}\n  ]\n}}",
                PROJECT_ID,
                serde_json::to_string(&project_dir).unwrap()
            ),
        );

        AssetsFixture {
            dir,
            home,
            root,
            project_dir,
        }
    }

    /// Bind the registry singleton to this fixture's store root.
    fn bind(&self) {
        registry::reload(&self.root);
    }

    fn store(&self) -> Store {
        Store::new(&self.root)
    }

    fn keeper(&self) -> Keeper {
        Keeper::new(self.store(), &self.home)
    }

    fn scan(&self) -> Value {
        let projects = scan::read_projects(&self.root);
        let (payload, _timings) = scan::scan_payload(&self.home, &projects);
        payload
    }

    fn find(&self, payload: &Value, pred: impl Fn(&Value) -> bool) -> Option<Value> {
        payload["assets"]
            .as_array()
            .and_then(|a| a.iter().find(|x| pred(x)).cloned())
    }
}

impl Drop for AssetsFixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

/* ------------------------------------------------------------------ */
/* scan                                                                */
/* ------------------------------------------------------------------ */

// The workbuddy skills and memory files are found, and
// "project skill found" / "project MCP found (both servers, one file)".
#[test]
fn scan_finds_global_and_project_assets() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("scan-basic");
    fx.bind();

    let payload = fx.scan();
    let list = payload["assets"].as_array().expect("assets");
    assert!(!list.is_empty(), "the scanner produced assets");

    let global_skill = fx.find(&payload, |a| {
        a["kind"] == "skill" && a["name"] == "home-skill"
    });
    assert!(global_skill.is_some(), "global skill found");
    assert_eq!(global_skill.unwrap()["scope"], json!("global"));

    let proj_skill = fx.find(&payload, |a| {
        a["kind"] == "skill" && a["name"] == "proj-skill"
    });
    assert!(proj_skill.is_some(), "project skill found");
    assert_eq!(proj_skill.unwrap()["scope"], json!("project"));

    assert!(
        fx.find(&payload, |a| a["kind"] == "memory").is_some(),
        "memory file found"
    );

    let project_mcps: Vec<String> = payload["assets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "mcp" && a["projectId"] == json!(PROJECT_ID))
        .filter_map(|a| a["name"].as_str().map(String::from))
        .collect();
    assert_eq!(project_mcps.len(), 2, "{:?}", project_mcps);
    assert!(project_mcps.iter().any(|n| n == "saas-db"));
    assert!(project_mcps.iter().any(|n| n == "saas-logs"));

    // At least one installed agent is detected
    let installed: Vec<&Value> = payload["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["installed"] == json!(true))
        .collect();
    assert!(!installed.is_empty(), "at least one agent is installed");
    assert!(
        installed.iter().any(|a| a["id"] == json!("workbuddy")),
        "workbuddy is detected"
    );
}

// The scan payload is cloneable, and the scan agents are
// cloneable" — the payload must be plain JSON with the keys app.js reads.
#[test]
fn scan_payload_carries_exactly_the_keys_the_renderer_reads() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("scan-shape");
    fx.bind();

    let payload = fx.scan();
    for key in [
        "assets",
        "effective",
        "agents",
        "projects",
        "summary",
        "scannedAt",
    ] {
        assert!(payload.get(key).is_some(), "payload is missing {}", key);
    }
    for key in ["assets", "effective", "agents", "projects"] {
        assert!(payload[key].is_array(), "{} must be an array", key);
    }
    assert!(payload["scannedAt"].is_string());
    assert_eq!(
        payload["summary"]["total"],
        json!(payload["assets"].as_array().unwrap().len())
    );

    // The strip step: no `raw` on any asset.
    for a in payload["assets"].as_array().unwrap() {
        assert!(
            a.get("raw").is_none(),
            "raw must be dropped, not nulled: {}",
            a["id"]
        );
        assert!(
            a["security"].is_array(),
            "security is always present: {}",
            a["id"]
        );
        // The effective pass clones the assets instead of mutating them, so
        // an asset with no `effective` is `undefined` and the strip step maps it
        // to `null` — the JSON sanitiser keeps nulls, it only drops
        // `undefined`. So `null` here is correct, not a missing key.
        assert!(
            a["effective"].is_null(),
            "assets carry no effective view: {}",
            a["id"]
        );
    }

    // The reduced `effective` lives on the parallel `effective` array
    // payload side: exactly three keys, nothing else survives.
    for a in payload["effective"].as_array().unwrap() {
        assert!(
            a.get("raw").is_none(),
            "effective drops raw too: {}",
            a["id"]
        );
        let eff = a["effective"].as_object().expect("effective object");
        let mut keys: Vec<&str> = eff.keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["live", "overrides", "shadowedBy"], "{:?}", keys);
        // `keeper.scan()` runs the security pass on `res.assets` only.
        assert!(a.get("security").is_none(), "effective carries no security");
    }

    // ipc_payload: `homeExists` is never set, so the key is
    // absent rather than null (the sanitiser drops undefined).
    for a in payload["agents"].as_array().unwrap() {
        for key in [
            "id",
            "label",
            "vendor",
            "accent",
            "home",
            "installed",
            "pseudo",
        ] {
            assert!(a.get(key).is_some(), "agent is missing {}", key);
        }
        assert!(
            a.get("homeExists").is_none(),
            "homeExists must not be emitted"
        );
    }

    // The effective map covers every asset
    assert_eq!(
        payload["effective"].as_array().unwrap().len(),
        payload["assets"].as_array().unwrap().len()
    );
}

// The payload caps every array at 500 entries. A skill with 511
// files reaches the renderer with the first 500 — sending more is a different
// payload, not better data.
#[test]
fn asset_file_lists_are_capped_at_500() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("scan-cap");
    fx.bind();

    let skill = join(&fx.project_dir, &[".workbuddy", "skills", "bulk"]);
    write(
        &join(&skill, &["SKILL.md"]),
        "---\nname: bulk\ndescription: Bulk fixture.\n---\n\n# Bulk\n",
    );
    for i in 0..510 {
        write(&join(&skill, &[&format!("f{}.txt", i)]), "x");
    }

    // Prove the truncation is the cap and not a small fixture: 511 files exist
    // on disk (SKILL.md + f0..f509).
    let on_disk = std::fs::read_dir(&skill)
        .expect("skill dir")
        .filter(|e| e.as_ref().map(|x| x.path().is_file()).unwrap_or(false))
        .count();
    assert_eq!(on_disk, 511, "the fixture really has more than 500 files");

    let payload = fx.scan();
    let bulk = fx
        .find(&payload, |a| a["kind"] == "skill" && a["name"] == "bulk")
        .expect("bulk skill");
    let files = bulk["files"].as_array().expect("files");
    assert_eq!(
        files.len(),
        500,
        "the renderer receives exactly the first 500"
    );
    assert!(
        files.iter().all(|f| f.is_string()),
        "the capped list is still relative path strings"
    );
}

/* ------------------------------------------------------------------ */
/* read asset                                                          */
/* ------------------------------------------------------------------ */

// Reading a memory asset succeeds, and
// "read-asset returns the file content" / "read-asset returns the asset".
#[test]
fn read_asset_returns_ok_asset_and_content() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("read");
    fx.bind();
    let payload = fx.scan();

    let mem = fx
        .find(&payload, |a| a["kind"] == "memory")
        .expect("a memory asset");
    let id = mem["id"].as_str().unwrap().to_string();
    let r = assets::read_asset(&fx.home, &fx.root, &id);
    assert_eq!(r["ok"], json!(true), "{}", r);
    assert_eq!(r["asset"]["id"], json!(id));
    assert_eq!(r["content"], json!("# Memory\n\nseed line\n"));
    assert_eq!(r["asset"]["raw"], r["content"], "raw is re-attached");

    // The skill content includes the frontmatter
    let skill = fx
        .find(&payload, |a| {
            a["kind"] == "skill" && a["name"] == "home-skill"
        })
        .unwrap();
    let rs = assets::read_asset(&fx.home, &fx.root, skill["id"].as_str().unwrap());
    assert_eq!(rs["ok"], json!(true), "{}", rs);
    assert!(
        rs["content"].as_str().unwrap().starts_with("---"),
        "{}",
        rs["content"]
    );
}

// Reading a bogus asset fails cleanly
// "deleteAsset(bogus) fails cleanly".
#[test]
fn read_asset_of_an_unknown_id_fails_cleanly() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("read-bogus");
    fx.bind();
    let r = assets::read_asset(&fx.home, &fx.root, "nope:nope:nope");
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(
        r["error"],
        json!("asset not found: nope:nope:nope"),
        "{}",
        r
    );
}

// `assets.rs`: "Plugins carry no `raw` … so the key must be *removed*, not
// nulled, or the drawer receives a shape nothing else produces."
#[test]
fn read_asset_drops_raw_for_a_plugin_asset() {
    let _lock = common::serial();
    let plugin = json!({
        "id": "plugin:demo",
        "kind": "plugin",
        "name": "demo",
        "dir": "C:/no/such/plugin",
        "file": "C:/no/such/plugin/manifest.json",
        "effective": { "live": true, "shadowedBy": [], "overrides": [] }
    });
    assets::remember(std::slice::from_ref(&plugin), std::slice::from_ref(&plugin));
    let r = assets::read_asset("C:/nohome", "C:/nostore", "plugin:demo");
    assert_eq!(r["ok"], json!(true), "{}", r);
    assert!(
        r["asset"].get("raw").is_none(),
        "no raw key at all: {}",
        r["asset"]
    );
    assert_eq!(r["content"], json!("(directory asset)"));
    assets::invalidate();
}

/* ------------------------------------------------------------------ */
/* save → rescan                                                       */
/* ------------------------------------------------------------------ */

// The file exists on disk and the applied asset is now recognised
// by the scanner". Sandbox off: the write hits the real tree, so the very next
// scan must see it.
#[test]
fn save_markdown_then_rescan_sees_the_change() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("save-md");
    fx.bind();
    let payload = fx.scan();
    let mem = fx
        .find(&payload, |a| a["kind"] == "memory")
        .expect("memory");
    let id = mem["id"].as_str().unwrap().to_string();

    let keeper = fx.keeper();
    let saved = keeper.save_markdown(&id, "# Memory\n\nseed line\nappended by the test\n");
    assert_eq!(saved["ok"], json!(true), "{}", saved);

    assert_eq!(
        read(&join(&fx.home, &[".workbuddy", "MEMORY.md"])),
        "# Memory\n\nseed line\nappended by the test\n"
    );

    // The cache was invalidated by the write, so this re-scans.
    let after = assets::read_asset(&fx.home, &fx.root, &id);
    assert_eq!(after["ok"], json!(true), "{}", after);
    assert!(
        after["content"]
            .as_str()
            .unwrap()
            .contains("appended by the test"),
        "the rescan sees the change: {}",
        after["content"]
    );

    // The write survives, and the audit line records it.
    assert_eq!(
        read(&join(&fx.home, &[".workbuddy", "MEMORY.md"])),
        "# Memory\n\nseed line\nappended by the test\n"
    );
    let audit = settings::read_audit(&fx.root, 100, None, None, None);
    assert!(
        audit
            .iter()
            .any(|a| a["txId"] == saved["txId"] && a["status"] == json!("ok")),
        "the write is journalled: {:?}",
        audit
    );
}

// keeper.rs: "Renaming means moving the directory; refuse rather than write a
// SKILL.md whose folder no longer matches its name."
#[test]
fn save_markdown_refuses_to_rename_a_skill_in_place() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("save-rename");
    fx.bind();
    let payload = fx.scan();
    let skill = fx
        .find(&payload, |a| {
            a["kind"] == "skill" && a["name"] == "home-skill"
        })
        .unwrap();
    let id = skill["id"].as_str().unwrap().to_string();

    let keeper = fx.keeper();
    let r = keeper.save_markdown(
        &id,
        "---\nname: renamed-skill\ndescription: still fine.\n---\n\n# body\n",
    );
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["blocked"], json!(true), "{}", r);
    assert!(
        r["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["field"] == json!("name")),
        "{}",
        r
    );
    assert!(
        read(&join(
            &fx.home,
            &[".workbuddy", "skills", "home-skill", "SKILL.md"]
        ))
        .contains("name: home-skill"),
        "nothing was written"
    );
}

// The write transaction succeeds, the written file parses as a skill, and
// "allowed-tools round-tripped" + §8b "skill now exists in the real project".
#[test]
fn save_skill_writes_a_parseable_skill_and_rescan_sees_it() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("save-skill");
    fx.bind();
    let keeper = fx.keeper();

    let r = keeper.save_skill(&json!({
        "data": {
            "name": "selftest-skill",
            "description": "Written by the Rust regression suite. Use when verifying the write pipeline.",
            "version": "0.1.0",
            "allowed-tools": "Read, Grep"
        },
        "body": "# Self Test Skill\n\nproduced by tests/assets.rs\n",
        "targets": [{ "agentId": "workbuddy", "scope": "project", "projectId": PROJECT_ID }]
    }));
    assert_eq!(r["ok"], json!(true), "{}", r);

    let file = join(
        &fx.project_dir,
        &[".workbuddy", "skills", "selftest-skill", "SKILL.md"],
    );
    assert!(exists(&file), "the skill landed in the real project");
    let text = read(&file);
    assert!(text.starts_with("---"), "{}", text);
    assert!(text.contains("name: selftest-skill"), "{}", text);
    assert!(
        text.contains("allowed-tools: Read, Grep"),
        "allowed-tools round-tripped: {}",
        text
    );

    let payload = fx.scan();
    let found = fx.find(&payload, |a| a["name"] == "selftest-skill");
    assert!(found.is_some(), "the scanner recognises the new skill");
    assert_eq!(found.unwrap()["scope"], json!("project"));
    assert!(r["written"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w == &json!(file)));
}

// The write is blocked before touching disk, the asset count is unchanged, and
// Saving a skill with invalid input is blocked, and
// "blocked write produced no audit record".
#[test]
fn validation_blocks_a_bad_skill_write_and_journals_nothing() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("save-blocked");
    fx.bind();
    let keeper = fx.keeper();
    let before = fx.scan()["assets"].as_array().unwrap().len();
    let audit_before = settings::read_audit(&fx.root, 999, None, None, None).len();

    let r = keeper.save_skill(&json!({
        "data": { "name": "Bad Name!", "description": "" },
        "body": "",
        "targets": [{ "agentId": "workbuddy", "scope": "project", "projectId": PROJECT_ID }]
    }));
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["blocked"], json!(true), "{}", r);
    assert!(r["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["level"] == json!("error")));

    assert!(
        !exists(&join(
            &fx.project_dir,
            &[".workbuddy", "skills", "bad-name"]
        )),
        "nothing was created on disk"
    );
    assert_eq!(
        fx.scan()["assets"].as_array().unwrap().len(),
        before,
        "asset count unchanged"
    );
    assert_eq!(
        settings::read_audit(&fx.root, 999, None, None, None).len(),
        audit_before,
        "no transaction was journalled"
    );
}

// The blocking issue must come back in the response — that is what the drawer
// renders, and a bare `ok:false` leaves the user with no reason.
#[test]
fn a_blocking_write_returns_the_issues() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("save-issues");
    fx.bind();
    let keeper = fx.keeper();

    let r = keeper.save_skill(&json!({
        "data": { "name": "Bad Name", "description": "x" },
        "body": "b",
        "targets": [{ "agentId": "workbuddy", "scope": "project", "projectId": PROJECT_ID }]
    }));
    assert_eq!(r["ok"], json!(false), "{}", r);
    let issues = r["issues"].as_array().expect("issues");
    assert!(!issues.is_empty(), "{}", r);
    assert!(issues
        .iter()
        .any(|i| i["level"] == json!("error") && i["field"] == json!("name")));
}

/* ------------------------------------------------------------------ */
/* MCP                                                                 */
/* ------------------------------------------------------------------ */

// The new server is present in the shadow tree, and
// The pre-existing servers are preserved, and upserting an MCP
// succeeds" / "upsert-mcp blocks a transport-less server".
#[test]
fn upsert_mcp_preserves_sibling_servers() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("mcp-upsert");
    fx.bind();
    let keeper = fx.keeper();
    let cfg = join(&fx.project_dir, &[".workbuddy", "mcp.json"]);
    let before = read(&cfg);

    let r = keeper.upsert_mcp(&json!({
        "name": "selftest-server",
        "config": { "command": "npx", "args": ["-y", "@example/pkg"] },
        "agentId": "workbuddy",
        "scope": "project",
        "projectId": PROJECT_ID
    }));
    assert_eq!(r["ok"], json!(true), "{}", r);

    let after: Value = serde_json::from_str(&read(&cfg)).unwrap();
    assert!(
        after["mcpServers"].get("selftest-server").is_some(),
        "{}",
        after
    );
    assert!(
        after["mcpServers"].get("saas-db").is_some(),
        "siblings survived: {}",
        after
    );
    assert!(
        after["mcpServers"].get("saas-logs").is_some(),
        "siblings survived: {}",
        after
    );
    assert!(read(&cfg).len() > before.len());

    // A transport-less server is blocked before the file is touched.
    let bad = keeper.upsert_mcp(&json!({
        "name": "no-transport",
        "config": { "foo": 1 },
        "agentId": "workbuddy",
        "scope": "project",
        "projectId": PROJECT_ID
    }));
    assert_eq!(bad["ok"], json!(false), "{}", bad);
    assert_eq!(bad["blocked"], json!(true), "{}", bad);
    assert_eq!(
        read(&cfg).contains("no-transport"),
        false,
        "nothing was written"
    );
}

// A corrupt config refuses the write — at the keeper level,
// where the refusal has to name the corruption and leave the file alone.
#[test]
fn upsert_mcp_refuses_a_corrupt_config() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("mcp-corrupt");
    fx.bind();
    let cfg = join(&fx.project_dir, &[".workbuddy", "mcp.json"]);
    const CORRUPT: &str = "{ \"mcpServers\": { \"saas-db\": TRUNCATED";
    write(&cfg, CORRUPT);

    let keeper = fx.keeper();
    let r = keeper.upsert_mcp(&json!({
        "name": "should-not-land",
        "config": { "command": "npx", "args": ["-y", "@example/pkg"] },
        "agentId": "workbuddy",
        "scope": "project",
        "projectId": PROJECT_ID
    }));
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert!(
        r["error"]
            .as_str()
            .map(|e| e.contains("corrupt"))
            .unwrap_or(false),
        "the refusal names the corruption: {}",
        r
    );
    assert_eq!(read(&cfg), CORRUPT, "the corrupt file was not overwritten");
}

// Deleting an MCP entry succeeds, the sibling servers survived the
// delete" and §14 "mcp.json survived (entry removed, not the file)".
#[test]
fn delete_mcp_removes_only_the_entry_and_the_file_survives() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("mcp-delete");
    fx.bind();
    let cfg = join(&fx.project_dir, &[".workbuddy", "mcp.json"]);
    let before = read(&cfg);

    let payload = fx.scan();
    let target = fx
        .find(&payload, |a| a["kind"] == "mcp" && a["name"] == "saas-db")
        .expect("saas-db asset");
    let id = target["id"].as_str().unwrap().to_string();

    let keeper = fx.keeper();
    let r = keeper.delete_mcp(&id);
    assert_eq!(r["ok"], json!(true), "{}", r);

    assert!(exists(&cfg), "mcp.json was deleted — catastrophic");
    let after: Value = serde_json::from_str(&read(&cfg)).unwrap();
    assert!(after["mcpServers"].get("saas-db").is_none(), "{}", after);
    assert!(
        after["mcpServers"].get("saas-logs").is_some(),
        "sibling survived: {}",
        after
    );
    assert!(read(&cfg).len() < before.len(), "the entry was removed");

    // A later transaction that fails restores what *it* touched; this one
    // succeeded, so it stays removed and the journal says so.
    let audit = settings::read_audit(&fx.root, 50, None, None, None);
    assert!(
        audit
            .iter()
            .any(|a| a["txId"] == r["txId"] && a["status"] == json!("ok")),
        "the delete is journalled: {:?}",
        audit
    );
}

// Deleting an MCP or JSON asset dispatches and succeeds: an MCP
// asset's `file` is the *shared* config, so a plain delete would wipe every
// server the agent has.
#[test]
fn delete_asset_of_an_mcp_asset_dispatches_to_delete_mcp() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("mcp-dispatch");
    fx.bind();
    let cfg = join(&fx.project_dir, &[".workbuddy", "mcp.json"]);
    let before = read(&cfg);

    let payload = fx.scan();
    let target = fx
        .find(&payload, |a| a["kind"] == "mcp" && a["name"] == "saas-logs")
        .expect("saas-logs asset");

    let keeper = fx.keeper();
    let r = keeper.delete_asset(target["id"].as_str().unwrap());
    assert_eq!(r["ok"], json!(true), "{}", r);
    assert!(exists(&cfg), "the shared config must survive");

    let after: Value = serde_json::from_str(&read(&cfg)).unwrap();
    assert!(after["mcpServers"].get("saas-logs").is_none(), "{}", after);
    assert!(
        after["mcpServers"].get("saas-db").is_some(),
        "sibling survived: {}",
        after
    );
    assert!(read(&cfg).len() < before.len());
}

// Deleting a bogus asset fails cleanly.
#[test]
fn delete_asset_of_an_unknown_id_fails_cleanly() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("mcp-bogus");
    fx.bind();
    let r = fx.keeper().delete_asset("nope:does:not:exist");
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["error"], json!("asset not found"), "{}", r);
}

/* ------------------------------------------------------------------ */
/* diff / distribute / summarize                                       */
/* ------------------------------------------------------------------ */

// The diff succeeds, it reports the appended line, and
// "diff is a no-op for identical content".
#[test]
fn diff_reports_appended_lines_and_is_a_noop_for_identical_content() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("diff");
    fx.bind();
    let payload = fx.scan();
    let mem = fx.find(&payload, |a| a["kind"] == "memory").unwrap();
    let id = mem["id"].as_str().unwrap().to_string();
    let keeper = fx.keeper();

    let before = assets::read_asset(&fx.home, &fx.root, &id)["content"]
        .as_str()
        .unwrap()
        .to_string();
    let edited = format!("{}\nappended by the test\n", before);

    let d = keeper.diff(&id, &edited);
    assert_eq!(d["ok"], json!(true), "{}", d);
    assert!(!d["lines"].as_array().unwrap().is_empty(), "{}", d);

    let same = keeper.diff(&id, &before);
    assert_eq!(same["lines"].as_array().unwrap().len(), 0, "{}", same);

    let missing = keeper.diff("nope:nope:nope", "x");
    assert_eq!(missing["ok"], json!(false), "{}", missing);
}

// Distributing returns a verdict — a copy to the same
// location is skipped, never duplicated.
#[test]
fn distribute_skips_the_same_location() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("distribute");
    fx.bind();
    let payload = fx.scan();
    let skill = fx
        .find(&payload, |a| {
            a["kind"] == "skill" && a["name"] == "proj-skill"
        })
        .unwrap();
    let id = skill["id"].as_str().unwrap().to_string();

    let keeper = fx.keeper();
    let r = keeper.distribute(
        &id,
        &[json!({ "agentId": "workbuddy", "scope": "project", "projectId": PROJECT_ID })],
    );
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["error"], json!("nothing to write"), "{}", r);
    let skipped = r["skipped"].as_array().expect("skipped");
    assert!(
        skipped
            .iter()
            .any(|s| s["reason"] == json!("same location")),
        "no 'same location' skip in {}",
        serde_json::to_string(&skipped).unwrap_or_default()
    );
}

// The core summary returns the full shape.
#[test]
fn summarize_returns_the_full_shape() {
    let _lock = common::serial();
    let fx = AssetsFixture::new("summarize");
    fx.bind();
    let payload = fx.scan();
    let effective = payload["effective"].as_array().unwrap().clone();

    let s = scan::summarize(&effective);
    assert_eq!(s["total"], json!(effective.len()), "{}", s);
    assert!(s["estTokens"].is_number(), "{}", s);
    assert!(s["by"]["skill"].as_u64().unwrap() > 0, "{}", s);
    for kind in ["skill", "mcp", "plugin", "rule", "memory"] {
        assert!(s["by"].get(kind).is_some(), "by.{} is present", kind);
    }
    for origin in ["user", "managed", "cached", "marketplace"] {
        assert!(
            s["byOrigin"].get(origin).is_some(),
            "byOrigin.{} is present",
            origin
        );
    }
    assert!(s["bytes"].is_number() && s["issues"].is_number() && s["shadowed"].is_number());
}

/* ------------------------------------------------------------------ */
/* project registry                                                    */
/* ------------------------------------------------------------------ */
struct RegistryFixture {
    dir: PathBuf,
    root: String,
}

impl RegistryFixture {
    fn new(tag: &str) -> RegistryFixture {
        let dir = temp_root(tag);
        let root = agentkeeper_shell_lib::core::store::native_str(&dir);
        RegistryFixture { dir, root }
    }
    fn path(&self, parts: &[&str]) -> String {
        join(&self.root, parts)
    }
    /// Bind the singleton to this fixture's store root.
    fn bind(&self) {
        registry::reload(&self.root);
    }
}

impl Drop for RegistryFixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

fn make_dir(path: &str) {
    std::fs::create_dir_all(path).expect("mkdir");
}

/* ------------------------------------------------------------------ */

// The id: `proj-` + slug(name || basename(abs)) capped at 32 + `-` + 6 hash chars.
#[test]
fn the_id_is_the_slug_plus_a_six_char_hash() {
    let _lock = common::serial();
    let fx = RegistryFixture::new("reg-id");
    fx.bind();

    let dir = fx.path(&["work", "My Project"]);
    make_dir(&dir);
    let added = registry::add(&dir, None);
    let id = added["project"]["id"].as_str().expect("id").to_string();
    assert!(
        id.starts_with("proj-my-project-"),
        "name falls back to the basename: {}",
        id
    );
    let tail = id.rsplit('-').next().unwrap_or_default();
    assert_eq!(tail.len(), 6, "hash is six chars: {}", id);

    // An explicit empty name is falsy, so the basename wins.
    let dir2 = fx.path(&["work", "epsilon"]);
    make_dir(&dir2);
    let added2 = registry::add(&dir2, Some(""));
    assert_eq!(added2["project"]["name"], json!("epsilon"), "{}", added2);

    // Two different directories never collide.
    let dir3 = fx.path(&["work", "zeta"]);
    make_dir(&dir3);
    let added3 = registry::add(&dir3, Some("epsilon"));
    assert_ne!(
        added2["project"]["id"], added3["project"]["id"],
        "the hash keeps same-named projects apart"
    );
}

// `get()` is what the write path resolves `projectId` through — an unknown id
// must be `None`, never a guessed project.
#[test]
fn get_resolves_by_id_and_returns_none_for_an_unknown_id() {
    let _lock = common::serial();
    let fx = RegistryFixture::new("reg-get");
    fx.bind();

    let dir = fx.path(&["work", "eta"]);
    make_dir(&dir);
    let added = registry::add(&dir, Some("eta"));
    let id = added["project"]["id"].as_str().unwrap();

    assert_eq!(
        registry::get(id).and_then(|p| p["name"].as_str().map(String::from)),
        Some("eta".to_string())
    );
    assert!(registry::get("proj-nope").is_none());
    assert!(registry::get("").is_none());
    assert!(exists(&dir));
}

/* ------------------------------------------------------------------ */
/* format layer                                                        */
/* YAML                                                                */
/* ------------------------------------------------------------------ */

// The YAML subset round-trips — the same fixture the unit
// uses, so a parse regression shows up against known values.
#[test]
fn yaml_parses_the_subset_the_writers_emit() {
    let src = [
        "name: my-skill",
        "description: Does a thing. Use when asked.",
        "version: 1.0.0",
        "allowed-tools: Read, Grep, Glob",
        "tags: [a, b, c]",
        "nested:",
        "  key: value",
        "  num: 42",
        "  flag: true",
        "list:",
        "  - one",
        "  - two",
        "quoted: \"has: colon\"",
        "empty:",
    ]
    .join("\n");

    let o = yaml::parse(&src).expect("parses");
    assert_eq!(o["name"], json!("my-skill"), "parses scalars");
    assert_eq!(o["version"], json!("1.0.0"));
    // A comma list is a plain string, not an array — the agents' own readers
    // split it, and turning it into an array would rewrite the user's file.
    assert_eq!(
        o["allowed-tools"],
        json!("Read, Grep, Glob"),
        "parses comma list as string"
    );
    assert_eq!(
        o["tags"].as_array().unwrap().len(),
        3,
        "parses inline array"
    );
    assert_eq!(o["nested"]["key"], json!("value"), "parses nested map");
    assert_eq!(
        o["nested"]["num"],
        json!(42),
        "a bare number parses as a number"
    );
    assert_eq!(o["list"][1], json!("two"), "parses block array");
    assert_eq!(o["quoted"], json!("has: colon"), "preserves quoted colon");
    assert_eq!(o["empty"], Value::Null, "an empty value is null");
}

// A corrupt YAML document must `Err`, not silently produce `{}` — `upsert_mcp`
// turns that into "config file is corrupt, refusing to overwrite".
#[test]
fn yaml_reports_a_syntax_error_instead_of_an_empty_document() {
    assert!(
        yaml::parse("a:\n- b\n  c: [\n").is_err(),
        "unbalanced flow seq"
    );
    assert!(
        yaml::parse("\ta: 1\n").is_err(),
        "a tab indent is invalid YAML"
    );
    let empty = yaml::parse("").expect("an empty document is not an error");
    assert_eq!(
        empty,
        Value::Null,
        "and arrives as null, which callers spell as an empty object"
    );
}

/* ------------------------------------------------------------------ */
/* frontmatter                                                         */
/* ------------------------------------------------------------------ */

// The frontmatter round-trip preserves unknown fields — the
// reason `parse` keeps a tolerant fallback: Cursor `.mdc` files contain
// `globs: **/*.ts`, and `*` is reserved in YAML.
#[test]
fn frontmatter_round_trip_preserves_unknown_fields_and_the_body() {
    let raw = "---\nname: release-checklist\ndescription: Ships safely.\nversion: 1.2.0\nallowed-tools: Read, Grep, Glob, Bash\n---\n\n# Checklist\n\n- [ ] tag\n";

    let fm = frontmatter::parse(raw);
    assert!(fm.has_frontmatter);
    assert_eq!(fm.data["name"], json!("release-checklist"));
    assert_eq!(
        fm.data["allowed-tools"],
        json!("Read, Grep, Glob, Bash"),
        "keeps allowed-tools"
    );

    let order: Vec<&str> = fm.order.iter().map(|s| s.as_str()).collect();
    let again = frontmatter::serialize(&fm.data, &fm.body, &order);
    let p2 = frontmatter::parse(&again);
    assert_eq!(p2.body.trim(), fm.body.trim(), "body preserved");
    assert_eq!(p2.data["name"], fm.data["name"], "fields preserved");
    assert_eq!(p2.data["version"], fm.data["version"]);
    assert_eq!(p2.data["description"], fm.data["description"]);
    // The blank line after the closing fence is supplied by the fence itself.
    assert!(
        again.starts_with("---\nname: release-checklist\n"),
        "{}",
        again
    );
    // The frontmatter writer strips one leading newline from the body because
    // the closing fence already supplies the break — the
    // blank line between `---` and `# Checklist` is not part of a round-trip.
    assert!(again.contains("\n---\n# Checklist\n"), "{}", again);
}

// A key the data gained later is appended, never shuffled in — the caller's
// recovered order is a courtesy hint, not a filter.
#[test]
fn frontmatter_serialize_appends_keys_missing_from_the_order_hint() {
    let data = json!({ "name": "x", "extra": 1 });
    let out = frontmatter::serialize(&data, "body\n", &["name"]);
    assert!(out.contains("name: x"), "{}", out);
    assert!(
        out.contains("extra: 1"),
        "the new key is appended, not dropped: {}",
        out
    );

    // A stale hint cannot resurrect a deleted field.
    let out = frontmatter::serialize(&json!({ "name": "x" }), "body\n", &["name", "gone"]);
    assert!(!out.contains("gone"), "{}", out);
}

#[test]
fn frontmatter_treats_an_unclosed_fence_as_no_frontmatter() {
    let fm = frontmatter::parse("---\nname: x\n\n# heading only, never closed\n");
    assert!(
        !fm.has_frontmatter,
        "the frontmatter regex requires a closing fence"
    );
    assert_eq!(fm.data, json!({}));
    assert!(fm.body.starts_with("---"), "and the text is the body");

    let plain = frontmatter::parse("# no frontmatter\n");
    assert!(!plain.has_frontmatter);
    assert_eq!(plain.body, "# no frontmatter\n");

    // A BOM must not defeat the `startsWith('---')` check.
    let bom = frontmatter::parse("\u{feff}---\nname: x\n---\nbody\n");
    assert!(bom.has_frontmatter, "a BOM is stripped first");
}

// `find_key_separator` — the first `:` that ends a key, ignoring quoted colons
// and `http://x`.
#[test]
fn frontmatter_finds_the_real_key_separator() {
    assert_eq!(frontmatter::find_key_separator("name: x"), Some(4));
    assert_eq!(
        frontmatter::find_key_separator("url: http://x"),
        Some(3),
        "the colon in http:// is not it"
    );
    assert_eq!(
        frontmatter::find_key_separator("q: \"has: colon\""),
        Some(1),
        "a quoted colon is not it"
    );
    assert_eq!(frontmatter::find_key_separator("no-separator"), None);
}

// `parse_tolerant` is the fallback that keeps `.mdc` rule files readable.
#[test]
fn frontmatter_falls_back_to_a_tolerant_scan_for_invalid_yaml() {
    // An undefined alias is a hard YAML error — exactly the case a Cursor
    // `.mdc` file can hit, and the reason the fallback exists at all.
    let src = "---\nname: mdc-rule\nglobs: [*undefined]\n---\nbody\n";
    assert!(
        yaml::parse("name: mdc-rule\nglobs: [*undefined]\n").is_err(),
        "strict parse fails"
    );
    let v = frontmatter::parse_tolerant(src);
    assert_eq!(
        v["name"],
        json!("mdc-rule"),
        "the scalar scan still reads plain keys: {}",
        v
    );
}

/* ------------------------------------------------------------------ */
/* JSONC                                                               */
/* ------------------------------------------------------------------ */

// MCP configs are JSONC in the wild; stripping comments must not touch
// anything inside a string.
#[test]
fn jsonc_strips_comments_but_not_string_contents() {
    let out = jsonc::strip_json_comments(
        "{\n  // a comment\n  \"a\": \"keep // this\",\n  /* block\n     comment */\n  \"b\": 1\n}\n",
    );
    assert!(!out.contains("// a comment"), "{}", out);
    assert!(!out.contains("/* block"), "{}", out);
    assert!(
        out.contains("\"keep // this\""),
        "a '//' inside a string survives: {}",
        out
    );

    let v: Value = serde_json::from_str(&out).expect("the stripped text is valid JSON");
    assert_eq!(v["a"], json!("keep // this"));
    assert_eq!(v["b"], json!(1));
}

/* ------------------------------------------------------------------ */
/* renderer crash channel (`ak:ui-error`)                              */
/* ------------------------------------------------------------------ */

// `lib.rs`'s command is a two-line adapter over `settings::ui_error`, exactly
// like every other command in this file: what is worth asserting is the file
// contract (line count, cap, truncation, failure envelope), not the wrapper.

/// The documented record shape: the five fields always present (empty string
/// when the renderer omitted them) and unknown keys preserved verbatim.
#[test]
fn ui_error_appends_one_parseable_line_with_the_documented_shape() {
    let fx = Fixture::new("ui-error-shape");
    settings::ensure_store(&fx.root).expect("ensure_store");

    let r = settings::ui_error(
        &fx.root,
        &json!({
            "kind": "unhandledrejection",
            "message": "Cannot read properties of null",
            "stack": "at render (app.js:1)",
            "view": "dashboard",
            "at": "2026-10-20T12:00:00.000Z",
            "futureField": { "nested": 1 }
        }),
    );
    assert_eq!(r["ok"], json!(true), "{}", r);
    assert_eq!(r["count"], json!(1), "{}", r);

    let text = read(&format!(
        "{}{}ui-errors.jsonl",
        fx.root,
        std::path::MAIN_SEPARATOR
    ));
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "one call, one line: {}", text);
    let rec: Value = serde_json::from_str(lines[0]).expect("the line is JSON");
    assert_eq!(rec["kind"], json!("unhandledrejection"));
    assert_eq!(rec["message"], json!("Cannot read properties of null"));
    assert_eq!(rec["view"], json!("dashboard"));
    assert_eq!(rec["at"], json!("2026-10-20T12:00:00.000Z"));
    assert_eq!(
        rec["futureField"],
        json!({ "nested": 1 }),
        "unknown fields must survive for forward compatibility: {}",
        rec
    );

    // Absent optional fields become empty strings, never `null`/missing — the
    // renderer reads them without a guard.
    settings::ui_error(&fx.root, &json!({ "kind": "error" }));
    let text = read(&format!(
        "{}{}ui-errors.jsonl",
        fx.root,
        std::path::MAIN_SEPARATOR
    ));
    let second: Value = serde_json::from_str(text.split('\n').nth(1).expect("second line"))
        .expect("the line is JSON");
    assert_eq!(second["message"], json!(""));
    assert_eq!(second["stack"], json!(""));
    assert_eq!(second["view"], json!(""));
}

/// 250 reports leave the newest 200 lines: the first kept line is #51 and the
/// cap is enforced on every call, not only at startup.
#[test]
fn ui_error_keeps_only_the_newest_two_hundred_lines() {
    let fx = Fixture::new("ui-error-cap");
    settings::ensure_store(&fx.root).expect("ensure_store");
    let path = format!("{}{}ui-errors.jsonl", fx.root, std::path::MAIN_SEPARATOR);

    let mut last = Value::Null;
    for i in 0..250 {
        last = settings::ui_error(
            &fx.root,
            &json!({ "kind": "error", "message": format!("msg-{}", i) }),
        );
        assert_eq!(last["ok"], json!(true), "{}", last);
        assert_eq!(last["count"], json!(std::cmp::min(i + 1, 200)), "{}", last);
    }
    assert_eq!(last["count"], json!(200), "the cap holds after 250 reports");

    let text = read(&path);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 200, "the file must be capped at 200 lines");
    for (n, line) in lines.iter().enumerate() {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("line {} is not JSON ({}): {}", n + 1, e, line));
    }
    let first: Value = serde_json::from_str(lines[0]).expect("first line");
    let last_kept: Value = serde_json::from_str(lines[199]).expect("last line");
    assert_eq!(
        first["message"],
        json!("msg-50"),
        "the oldest kept line must be report #51, i.e. the 50th index"
    );
    assert_eq!(
        last_kept["message"],
        json!("msg-249"),
        "and the newest one is kept"
    );
}

/// Over-long fields are cut to 4096 bytes **without splitting a character**.
///
/// The Chinese half is the point: one character is three bytes, so 4096 is not
/// a boundary and a byte-slice would either panic (`s[..4096]`) or produce bytes
/// that are not UTF-8.
#[test]
fn ui_error_truncates_long_fields_on_a_character_boundary() {
    let fx = Fixture::new("ui-error-truncate");
    settings::ensure_store(&fx.root).expect("ensure_store");
    let path = format!("{}{}ui-errors.jsonl", fx.root, std::path::MAIN_SEPARATOR);

    let message = "错".repeat(2000); // 6000 bytes
    let stack = "at frame ".repeat(1000); // 9000 bytes
    let r = settings::ui_error(
        &fx.root,
        &json!({ "kind": "error", "message": message, "stack": stack }),
    );
    assert_eq!(r["ok"], json!(true), "{}", r);

    let text = read(&path);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "a truncated payload still fits one line: {}",
        text
    );
    let rec: Value = serde_json::from_str(lines[0]).expect("the line is still JSON");

    let kept = rec["message"].as_str().expect("message is a string");
    assert!(
        kept.len() <= settings::UI_ERROR_FIELD_MAX,
        "kept {} bytes, cap is {}",
        kept.len(),
        settings::UI_ERROR_FIELD_MAX
    );
    assert_eq!(
        kept.len(),
        4095,
        "3-byte characters stop one byte short of 4096"
    );
    assert_eq!(kept.chars().count(), 1365);
    assert!(
        kept.chars().all(|c| c == '错'),
        "truncation must not introduce U+FFFD: {:?}",
        &kept[kept.len() - 6..]
    );
    assert_eq!(
        rec["stack"].as_str().expect("stack").len(),
        settings::UI_ERROR_FIELD_MAX,
        "an ASCII stack can be cut at exactly the cap"
    );
}

/// A malformed payload is a fact of life on a crash channel: the renderer may
/// send an array, an empty body or `null` while it is itself broken.
#[test]
fn ui_error_never_panics_on_a_malformed_payload() {
    let fx = Fixture::new("ui-error-malformed");
    settings::ensure_store(&fx.root).expect("ensure_store");
    let path = format!("{}{}ui-errors.jsonl", fx.root, std::path::MAIN_SEPARATOR);

    for payload in [json!([1, 2, 3]), json!({}), Value::Null, json!("a string")] {
        let r = settings::ui_error(&fx.root, &payload);
        assert_eq!(
            r["ok"],
            json!(true),
            "payload {} was rejected: {}",
            payload,
            r
        );
    }
    // Wrong field types degrade to empty strings rather than failing the write.
    let r = settings::ui_error(&fx.root, &json!({ "kind": 7, "message": { "oops": true } }));
    assert_eq!(r["ok"], json!(true), "{}", r);

    let text = read(&path);
    let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        5,
        "one line per report, whatever the payload was: {}",
        text
    );
    for line in &lines {
        let rec: Value = serde_json::from_str(line).expect("every line is JSON");
        assert!(rec["message"].is_string(), "{}", rec);
    }
    let last: Value = serde_json::from_str(lines[4]).expect("last line");
    assert_eq!(
        last["kind"],
        json!(""),
        "a non-string kind becomes an empty string"
    );
    assert_eq!(last["message"], json!(""));
}

/// A write that cannot happen is reported, not swallowed and not a panic — the
/// renderer's own crash report must never be the reason the app dies.
#[test]
fn ui_error_reports_an_unwritable_store_root_instead_of_panicking() {
    let fx = Fixture::new("ui-error-unwritable");
    // A regular file where the store root should be: `create_dir_all` fails with
    // "not a directory" and no directory is silently created elsewhere.
    let blocker = join(&fx.root, &["blocker"]);
    write(&blocker, "not a directory\n");
    let blocked_root = join(&fx.root, &["blocker", "child"]);

    let r = settings::ui_error(
        &blocked_root,
        &json!({ "kind": "error", "message": "boom" }),
    );
    assert_eq!(
        r["ok"],
        json!(false),
        "an unwritable root must not report success: {}",
        r
    );
    let error = r["error"].as_str().expect("error text");
    assert!(!error.is_empty(), "the failure must carry a reason: {}", r);
    assert!(
        !exists(&blocked_root),
        "nothing may be created under a file"
    );
}
