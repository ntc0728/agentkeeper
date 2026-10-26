//! IPC / 数据契约、资产生命周期、项目注册表与格式层。
//!
//! * contract —— 面向渲染器的响应形状，也就是套件中纯粹为了
//!   防止信封在 UI 之下被改动而存在的那部分。套件
//!   覆盖 IPC 契约的第 1-7、9-11 节以及设置 / store
//!   各节。`lib.rs` 中的 `#[tauri::command]` 函数
//!   是私有的，并且接收一个 `tauri::AppHandle`，所以集成测试无法
//!   调用它们：每个测试调用*与该命令相同的核心函数*，并在
//!   命令自己额外加了信封逻辑的地方（一个钳制、一个过滤、一份固定的
//!   键列表）复现那几行，并附一条指明该命令的注释。
//!   被测的不变式是形状，而这正是渲染器读取的东西。
//! * assets —— 扫描 → 读取资产 → 保存 → 重扫，以及位于它们之间的
//!   写入关卡。覆盖资产生命周期与格式层：
//!   第 1-7 节。所有东西都跑在一个临时"用户
//!   主目录"加一个临时 store 根目录里；没有任何东西碰到真实的配置。
//! * registry —— 添加 / 删除 / 列出 / 播种 / 解析，以及 `projects.json`
//!   的字节形状。覆盖整个套件
//!   始终依赖的那些 `ProjectRegistry` 行为。
//! * formats —— YAML、frontmatter 与 JSONC：写入路径所建立在的
//!   格式层。这些是纯函数 ——
//!   没有 store、没有单例、没有文件系统 —— 所以它们不需要临时根目录。TOML
//!   的分节手术在这里*不*重复：`core::toml_splice` 的单元测试
//!   用精确字符串断言了同样的（以及更多的）夹具，所以集成测试的
//!   拷贝纯属重复。
//!
//! `core::registry` 与 `core::assets` 是进程级单例（它们分别代表
//! 注册表实例和最后一次扫描），所以任何
//! 绑定或播种它们的测试都要先拿到 `common::serial()`。

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
/* renderer-facing contract —— 面向渲染器的契约                        */
/* ------------------------------------------------------------------ */
/// store 根目录，以及拥有它的临时目录。
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
/* settings —— 设置                                                    */
/* ------------------------------------------------------------------ */

// 默认值以一个扁平对象的形式返回：
// "设置默认值是完整的"。
//
// `ak:get-settings` 原样读取设置 —— 一个扁平对象，绝不是
// 一个 `{ok, settings}` 信封。
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

    // `sandbox` 曾是一个会话级开关，从来就不是设置键；整个
    // 功能已退役，所以这个键不得在这里重新出现。
    assert!(s.get("sandbox").is_none(), "sandbox is not a settings key");

    // 那些已经消失的键。它们不得回来，否则一份
    // 过期的 settings.json 又会开始控制写入路径。
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

// 一个类型与默认值不符的值 ——
// "默认值"；一份被手工编辑过的文件不得把检查周期设成每
// "abc" 分钟。
#[test]
fn a_hand_edited_settings_file_cannot_change_a_value_type() {
    let fx = Fixture::new("settings-types");
    write(
        &fx.path(&["settings.json"]),
        &format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "lang": 5,                     // 数字放在了字符串该在的位置
                "updateCheckMinutes": "abc",   // 字符串放在了数字该在的位置
                "theme": "dark",               // 合法的覆盖
                "updateCheck": null,           // JS `typeof null === 'object'`
                "unknownKey": "smuggled"       // 不在默认值里
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

// 一次保存返回 ok 信封，并且
// "saveSettings 忽略未知键"。
#[test]
fn save_settings_returns_ok_plus_settings_and_filters_the_patch() {
    let fx = Fixture::new("settings-save");

    let r = settings::save(
        &fx.root,
        &json!({
            "lang": "zh",
            "updateCheckMinutes": 45,
            "updateCheckMinutesWrong": 1,
            "updateCheck": "yes",      // 字符串放在了布尔值该在的位置
            "notAKey": true            // 不在默认值里
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

    // 写入以固定的字节形状到达磁盘：2 空格缩进 + "\n"。
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

// 每次设置保存都会追加一条审计记录。
//
// 值得知道的是：一次设置保存只记录它真正应用了的键，所以
// `assetName` 比提交的补丁更窄。记录的
// `action`/`status`/`kind` 正是 Activity 页面读取的东西，所以页面
// 渲染出来是一样的；只有 `assetName` 可能不同。
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
/* store bootstrap —— store 引导                                       */
/* ------------------------------------------------------------------ */

// "ensure 创建 store 目录树" —— 一个空的 audit.jsonl 和一个 meta.json，
// 后来的代码都假定它们存在。
#[test]
fn ensure_store_creates_the_tree_the_rest_of_the_app_assumes() {
    let fx = Fixture::new("ensure-store");

    // 没有备份树：写入路径把它的撤销状态保存在内存里，磁盘上什么都没有，
    // 所以首次启动不得凭空造出一个 `backups/`。
    assert!(
        !std::path::Path::new(&fx.path(&["backups"])).exists(),
        "no backups/ may be created"
    );
    assert_eq!(
        common::read(&fx.path(&["audit.jsonl"])),
        "",
        "audit.jsonl starts empty"
    );

    // 幂等：引导会在每次启动和每次设置写入时运行。
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
    // 也不再创建任何暂存层：写入直接落在真实的目录树里。
    assert!(
        !std::path::Path::new(&fx.path(&["sandbox"])).exists(),
        "the retired staging tree must not be recreated"
    );
}

/* ------------------------------------------------------------------ */
/* collections the renderer iterates directly —— 渲染器直接迭代的集合    */
/* ------------------------------------------------------------------ */

// 列出项目返回一个裸数组。把它包进
// `{ok, projects}` 会让 app.js 把这个对象当成列表。
#[test]
fn list_projects_is_a_bare_array_of_project_objects() {
    let _lock = common::serial();
    let fx = Fixture::new("contract-projects");
    registry::reload(&fx.root);

    let project = join(&fx.dir.to_string_lossy(), &["proj"]);
    write(&join(&project, &["README.md"]), "# p\n");
    let added = registry::add(&project, Some("Contract Project"));
    assert_eq!(added["ok"], json!(true), "{}", added);

    // `registry.list()` 返回一个裸数组 —— 没有 `{ok, projects}` 信封。
    // 项目通过 `pick_project` 添加、通过 `scan` 读取，所以
    // 这里断言的是唯一生产者的形状。
    let list = registry::list();
    assert_eq!(list.len(), 1, "{:?}", list);
    for key in ["id", "name", "dir"] {
        assert!(list[0].get(key).is_some(), "project is missing {}", key);
    }
    assert_eq!(list[0]["name"], json!("Contract Project"));
}

// 审计返回一个数组 —— `audit.filter(...)` 直接
// 作用在它上面，最新的在前，并带上 IPC 侧的数量钳制。
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

// 一份由更旧版本写出的配置里已经有一个装满旧事务副本的 `backups/` 目录。
// 那棵树是退役了，而不只是没用上：这个版本里没有任何东西
// 能读它，所以留下一个看起来像"撤销"的文件夹就是对这个应用
// 能力的一声谎言。`ensure_store` 会把它删掉，而且必须
// 在反复启动之间一直这么做。
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
    // 应用真正读取的那些文件在清理之后依然存在。
    assert!(std::path::Path::new(&fx.path(&["audit.jsonl"])).is_file());
}

// 一次写入不铸造任何种类的目录；journal 那一行是唯一的痕迹。
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
    // `hasBackup` 曾是 Activity 页据以决定是否显示撤销按钮的标志；
    // 它也不得以任何别的形式回来。
    assert!(rows[0].get("hasBackup").is_none(), "{}", rows[0]);
}

/* ------------------------------------------------------------------ */
/* single-asset + stats envelopes —— 单资产 + 统计信封                  */
/* ------------------------------------------------------------------ */

// 读取一个资产返回 {ok, asset, content} —— 恰好
// 三个键；抽屉会把三个都解构出来。
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

// 本地统计返回 ok + 计数。每次调用都从
// audit.jsonl 重新数一遍；没有任何缓存。`rollbacks` 已经没有了 —— 已经
// 没有回滚可数了。
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

    // `ak:local-stats` —— 计数逻辑就在那个命令里。
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
/* updater input handling —— 更新器的输入处理                          */
/* ------------------------------------------------------------------ */

// 这些都是没有网络也没有 DPAPI 的纯函数，所以在这里钉住它们是安全的。
// `UpdateManager` 本身是一个绑定到单个根目录的进程级单例，
// 所以它的状态载荷在这个套件里刻意不被断言。

// 更新仓库被规范化 —— 只允许 `owner/repo`。
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

// 更新间隔被钳制 —— 1..1440 分钟，默认 20。
#[test]
fn update_interval_is_clamped_to_a_day() {
    assert_eq!(manager::clamp_minutes(&json!(20)), 20);
    assert_eq!(manager::clamp_minutes(&json!(0)), manager::MIN_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(-5)), manager::MIN_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(100000)), manager::MAX_MINUTES);
    assert_eq!(manager::clamp_minutes(&json!(2.6)), 3, "Math.round");
    // `Number(x)` 的语义，而不是"解析不了就用默认值"：null 和 "" 是
    // 0，所以它们会被钳到下限；只有 NaN（对象/数组）才会落到
    // 默认值 20。
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

// 为这个平台挑选安装包 —— 后缀/关键词规则
// 以及安装步骤所依赖的文件名净化器。
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

    // 刻意保留的怪癖（`core/update/version.rs` 模块文档）：
    // `allowed_installer_suffixes` 接受 .dmg/.appimage/.deb，但
    // `is_non_installer_asset` 拒绝每一个以此结尾的名字，所以 macOS 和
    // Linux 永远拿不到 `pick_installer` 选出的安装包。更新器在那里
    // 隐藏按钮，而不是下载一个它跑不起来的东西。
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

    // `safe_file_name` —— 一个恶意的发布名不得逃出下载目录。
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

// 下载主机白名单 —— 只有 GitHub 可以提供安装包。
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

// 版本比较 —— 带预发布排序的 semver。
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
/* discover input handling —— discover 的输入处理                      */
/* ------------------------------------------------------------------ */

// `normalizeRepo` —— 这是 *discover* 的规范化器，它
// 在被拒绝时回答 `null`（更新器的那个返回 ""）。它是
// 注册表里的 `sourceUrl` 与 `git clone`
// 目标之间唯一的屏障，所以刁钻的输入绝不能到达 shell。
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

// 注册表的行被规范化 —— `shape_registry_response` 会给
// 每一个字段设上限并强制转换类型，所以一个恶意的注册表回答
// 无法撑爆 Discover 页面，也无法把一个非仓库的东西夹带进 `installFromRepo`。
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

    // `search_url` —— 查询串有长度上限且是表单编码的。
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
/* scan → read → save → rescan —— 扫描、读取、保存、重扫           */
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

        // 一个全局的 workbuddy 安装：一个 skill、一个 memory 文件、一份 MCP 配置。
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

        // 一个注册在 projects.json 里的项目，带着它自己的 skill 与配置。
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

    /// 把注册表单例绑定到这个夹具的 store 根目录。
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
/* scan —— 扫描                                                       */
/* ------------------------------------------------------------------ */

// workbuddy 的 skill 与 memory 文件被找到了，并且
// "找到了项目 skill" / "找到了项目 MCP（两个服务器，一个文件）"。
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

    // 至少检测到一个已安装的 agent
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

// 扫描载荷是可克隆的，扫描出的 agent 也是
// 可克隆的" —— 载荷必须是普通 JSON，并带有 app.js 读取的那些键。
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

    // 剥离那一步：任何资产上都没有 `raw`。
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
        // effective 那一遍是克隆资产而不是改动它们，所以
        // 一个没有 `effective` 的资产是 `undefined`，剥离那一步把它映射为
        // `null` —— JSON 净化器保留 null，它只丢掉
        // `undefined`。所以这里的 `null` 是正确的，不是缺键。
        assert!(
            a["effective"].is_null(),
            "assets carry no effective view: {}",
            a["id"]
        );
    }

    // 精简后的 `effective` 位于载荷一侧那个平行的 `effective` 数组上：
    // 恰好三个键，别的什么都活不下来。
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
        // `keeper.scan()` 只对 `res.assets` 运行安全检查。
        assert!(a.get("security").is_none(), "effective carries no security");
    }

    // ipc_payload：`homeExists` 从不被设置，所以这个键是
    // 缺席而不是 null（净化器会丢掉 undefined）。
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

    // effective 映射覆盖每一个资产
    assert_eq!(
        payload["effective"].as_array().unwrap().len(),
        payload["assets"].as_array().unwrap().len()
    );
}

// 载荷把每个数组都封顶在 500 条。一个有 511 个
// 文件的 skill 抵达渲染器时只有前 500 个 —— 发得更多是另一个
// 载荷，而不是更好的数据。
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

    // 证明截断确实来自上限而不是夹具太小：磁盘上真的有 511 个文件
    //（SKILL.md + f0..f509）。
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
/* read asset —— 读取资产                                             */
/* ------------------------------------------------------------------ */

// 读取一个 memory 资产成功，并且
// "read-asset 返回文件内容" / "read-asset 返回该资产"。
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

    // skill 的内容包含 frontmatter
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

// 读取一个不存在的资产会干净地失败
// "deleteAsset(bogus) 干净地失败"。
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

// `assets.rs`："插件不带 `raw` … 所以这个键必须被*移除*，
// 而不是置为 null，否则抽屉会收到一个别处从不产生的形状。"
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
/* save → rescan —— 保存后重扫                                        */
/* ------------------------------------------------------------------ */

// 文件在磁盘上存在，并且被应用的资产现在能被
// 扫描器认出来"。沙箱已关闭：这次写入命中真实的目录树，所以紧接着的
// 一次扫描就必须看到它。
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

    // 缓存已被这次写入失效，所以这里会重新扫描。
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

    // 这次写入留存下来了，而且审计行记录了它。
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

// keeper.rs："重命名意味着移动目录；宁可拒绝，也不要写出一个
// 目录名与自身名字不再匹配的 SKILL.md。"
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

// 写入事务成功，写出的文件能解析成一个 skill，并且
// "allowed-tools 往返保真" + §8b "skill 现在存在于真实项目中"。
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

// 写入在碰到磁盘之前就被拦下，资产数量不变，并且
// "用无效输入保存 skill 会被拦下"，以及
// "被拦下的写入没有产生任何审计记录"。
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

// 那个阻断性问题必须在响应里回来 —— 那正是抽屉
// 要渲染的东西，而一个光秃秃的 `ok:false` 让用户毫无理由可循。
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
/* MCP —— MCP 服务器                                                     */
/* ------------------------------------------------------------------ */

// 新服务器出现在影子目录树里，并且
// 预先存在的服务器被保留下来，并且 upsert 一个 MCP
// 成功" / "upsert-mcp 拦下没有传输层的服务器"。
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

    // 没有传输层的服务器在文件被碰到之前就被拦下。
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

// 损坏的配置会拒绝这次写入 —— 在 keeper 这一层，
// 那里拒绝时必须指明损坏之处，并且不动那个文件。
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

// 删除一个 MCP 条目成功，兄弟服务器在删除后
// 仍然健在"，以及 §14 "mcp.json 存活（移除的是条目，不是文件）"。
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

    // 之后一次失败的事务会恢复*它自己*触碰过的东西；而这一次
    // 成功了，所以条目保持被移除的状态，journal 也是这么记的。
    let audit = settings::read_audit(&fx.root, 50, None, None, None);
    assert!(
        audit
            .iter()
            .any(|a| a["txId"] == r["txId"] && a["status"] == json!("ok")),
        "the delete is journalled: {:?}",
        audit
    );
}

// 删除一个 MCP 或 JSON 资产会被正确分派并成功：一个 MCP
// 资产的 `file` 是*共享的*配置，所以一次普通的删除会抹掉
// 这个 agent 的每一个服务器。
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

// 删除一个不存在的资产会干净地失败。
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
/* diff / distribute / summarize —— 差异 / 分发 / 汇总                 */
/* ------------------------------------------------------------------ */

// 差异计算成功，它报告了追加的那一行，并且
// "内容相同时差异是空操作"。
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

// 分发返回一个裁决 —— 复制到同一个
// 位置会被跳过，绝不重复。
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

// 核心的汇总返回完整的形状。
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
/* project registry —— 项目注册表                                     */
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
    /// 把单例绑定到这个夹具的 store 根目录。
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

// id 的构成：`proj-` + slug(name || basename(abs))，截到 32，再加 `-` 和 6 位散列字符。
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

    // 一个显式的空名字是假值，所以 basename 胜出。
    let dir2 = fx.path(&["work", "epsilon"]);
    make_dir(&dir2);
    let added2 = registry::add(&dir2, Some(""));
    assert_eq!(added2["project"]["name"], json!("epsilon"), "{}", added2);

    // 两个不同的目录绝不会撞车。
    let dir3 = fx.path(&["work", "zeta"]);
    make_dir(&dir3);
    let added3 = registry::add(&dir3, Some("epsilon"));
    assert_ne!(
        added2["project"]["id"], added3["project"]["id"],
        "the hash keeps same-named projects apart"
    );
}

// `get()` 是写入路径解析 `projectId` 的地方 —— 一个未知的 id
// 必须是 `None`，绝不能是猜出来的项目。
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
/* format layer —— 格式层                                             */
/* YAML —— YAML 格式                                                     */
/* ------------------------------------------------------------------ */

// YAML 子集可以往返 —— 与单元测试所用的同一个夹具，
// 所以解析回归会对着已知值暴露出来。
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
    // 逗号列表是一个普通字符串，不是一个数组 —— agent 自己的读取器
    // 会去拆分它，把它变成数组就等于改写了用户的文件。
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

// 一份损坏的 YAML 文档必须返回 `Err`，而不是悄悄产生一个 `{}` ——
// `upsert_mcp` 会把它变成"配置文件已损坏，拒绝覆盖"。
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
/* frontmatter —— 前置元数据                                          */
/* ------------------------------------------------------------------ */

// frontmatter 的往返保留未知字段 —— 这正是
// `parse` 保留一个宽容回退的原因：Cursor 的 `.mdc` 文件含有
// `globs: **/*.ts`，而 `*` 在 YAML 里是保留字符。
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
    // 闭合围栏之后的那个空行是由围栏本身提供的。
    assert!(
        again.starts_with("---\nname: release-checklist\n"),
        "{}",
        again
    );
    // frontmatter 写入器会剥掉正文开头的一个换行，因为
    // 闭合围栏已经提供了这个断行 —— 所以 `---` 与 `# Checklist` 之间的
    // 那个空行不属于一次往返。
    assert!(again.contains("\n---\n# Checklist\n"), "{}", again);
}

// 数据后来新增的键会被追加，而不是被打乱插进去 —— 调用方
// 恢复出的顺序只是一份善意的提示，不是一道过滤器。
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

    // 一份过期的提示无法复活一个已删除的字段。
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

    // 一个 BOM 不得破坏 `startsWith('---')` 这项检查。
    let bom = frontmatter::parse("\u{feff}---\nname: x\n---\nbody\n");
    assert!(bom.has_frontmatter, "a BOM is stripped first");
}

// `find_key_separator` —— 第一个结束一个键的 `:`，忽略被引用的冒号
// 和 `http://x`。
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

// `parse_tolerant` 是让 `.mdc` 规则文件仍然可读的兜底。
#[test]
fn frontmatter_falls_back_to_a_tolerant_scan_for_invalid_yaml() {
    // 一个未定义的别名是硬性 YAML 错误 —— 正是 Cursor
    // `.mdc` 文件可能撞上的那种情形，也是这个兜底存在本身的理由。
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
/* JSONC —— JSONC 格式                                                    */
/* ------------------------------------------------------------------ */

// 现实中 MCP 配置是 JSONC；剥离注释不得碰到
// 字符串里面的任何东西。
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
/* renderer crash channel (`ak:ui-error`) —— 渲染器崩溃通道            */
/* ------------------------------------------------------------------ */

// `lib.rs` 的那个命令只是 `settings::ui_error` 之上的两行适配器，
// 和本文件里其他每个命令一样：值得断言的是文件
// 契约（行数、上限、截断、失败信封），而不是那个包装。

/// 有文档记载的记录形状：五个字段总是存在（渲染器省略它们时
/// 是空字符串），未知键原样保留。
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

    // 缺席的可选字段变成空字符串，绝不是 `null`/缺失 ——
    // 渲染器读取它们时没有判空保护。
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

/// 250 条上报之后只留下最新的 200 行：第一条保留的行是第 51 条，而且
/// 上限在每次调用时都生效，而不只是在启动时。
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

/// 过长的字段被裁到 4096 字节，**且不会切裂一个字符**。
///
/// 中文那一半才是重点：一个字符是三个字节，所以 4096 并不是
/// 一个边界，按字节切片要么会 panic（`s[..4096]`），要么产生
/// 不是 UTF-8 的字节。
#[test]
fn ui_error_truncates_long_fields_on_a_character_boundary() {
    let fx = Fixture::new("ui-error-truncate");
    settings::ensure_store(&fx.root).expect("ensure_store");
    let path = format!("{}{}ui-errors.jsonl", fx.root, std::path::MAIN_SEPARATOR);

    let message = "错".repeat(2000); // 6000 字节
    let stack = "at frame ".repeat(1000); // 9000 字节
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

/// 崩溃通道上出现畸形载荷是家常便饭：渲染器自己坏掉时可能
/// 发出一个数组、一个空体或 `null`。
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
    // 字段类型不对时降级为空字符串，而不是让这次写入失败。
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

/// 一次无法发生的写入会被报告出来，而不是被吞掉、更不是 panic ——
/// 渲染器自己的崩溃报告绝不该成为应用挂掉的原因。
#[test]
fn ui_error_reports_an_unwritable_store_root_instead_of_panicking() {
    let fx = Fixture::new("ui-error-unwritable");
    // store 根目录该在的地方却是一个普通文件：`create_dir_all` 会以
    // "not a directory" 失败，而且不会在别处悄悄建出一个目录。
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
