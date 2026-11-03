//! 命令行与共享层的端到端验收：命令行 `--shared`、库位置迁移的幂等、首次启用提示。
//!
//! 三块各对应 §9 的一小节：
//!
//! 1. `ak-scan --shared` 打出 `{library, ledger, findings}`，且**无参数时的输出
//!    完全不变**（CI 依赖现有扫描载荷逐字段 diff）；
//! 2. 200 份 skill 的库做一次真实迁移，「在第 100 个上被强杀」由
//!    `shared_ops` 的续跑用例覆盖（那里真的把 journal 停在中间）；这里验的是
//!    端到端的最终状态与「重跑一次一个字节都不动」；
//! 3. `bootstrap.shared` 只放 `{enabled}`，候选数不回溯进启动路径。
//!
//! 夹具约定与 `tests/shared_ui.rs` 相同：每个用例在 `temp_dir()` 下拥有自己的
//! 临时根与假 home，结束时删掉。

mod common;

use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::library;
use agentkeeper_shell_lib::core::linkfs;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::shared_ops;
use agentkeeper_shell_lib::core::store::{self, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{cleanup, join, temp_root, write};

struct Fixture {
    dir: PathBuf,
    home: String,
    root: String,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let dir = temp_root(tag);
        let home = join(&dir.to_string_lossy(), &["home"]);
        let root = join(&dir.to_string_lossy(), &["store"]);
        std::fs::create_dir_all(&home).expect("home");
        settings::ensure_store(&root).expect("ensure_store");
        Fixture { dir, home, root }
    }

    fn keeper(&self) -> Keeper {
        Keeper::new(Store::new(&self.root), &self.home)
    }

    fn library_root(&self) -> String {
        join(&self.home, &[".agents"])
    }

    fn library(&self) -> library::Library {
        library::Library::at(self.library_root())
    }

    /// 造一份 agent 侧 skill（真实目录）。
    fn seed_skill(&self, agent_dir: &str, slug: &str) -> String {
        write(
            &join(&self.home, &[agent_dir, "skills", slug, "SKILL.md"]),
            &format!(
                "---\nname: {}\ndescription: Fixture skill.\nversion: 1.0.0\n---\n\nBody of {}.\n",
                slug, slug
            ),
        );
        join(&self.home, &[agent_dir, "skills", slug])
    }

    fn scan_payload(&self) -> Value {
        let projects = agentkeeper_shell_lib::core::scan::read_projects(&self.root);
        let (payload, _t) =
            agentkeeper_shell_lib::core::scan::scan_payload_uncached(&self.home, &projects);
        payload
    }

    /// 把某个 agent 侧的 skill 启用到库内主副本（真实写盘，含链接）。
    fn enable(&self, agent: &str, slug: &str) -> Value {
        let payload = self.scan_payload();
        let asset = payload["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| {
                a["kind"] == json!("skill")
                    && a["agentId"] == json!(agent)
                    && a["name"] == json!(slug)
            })
            .unwrap_or_else(|| panic!("no {} asset for {}", slug, agent));
        self.keeper().enable_asset(&json!({
            "assetId": asset["id"],
            "agentId": agent,
            "scope": "global",
        }))
    }

    fn links_available(&self) -> bool {
        self.library()
            .capability(&self.root)
            .dir_order
            .iter()
            .any(|m| m.is_link())
    }

    /// `ak-scan` 的路径。它不在本 crate 里（`agentkeeper-core` 的二进制），
    /// 所以没有 `CARGO_BIN_EXE_*`；按「谁是它现在的工作目录」往上找
    /// `target/<profile>/` —— 测试二进制的邻居就是同一次构建的产物。
    fn ak_scan() -> PathBuf {
        let exe = std::env::current_exe().expect("current exe");
        let dir = exe.parent().expect("exe has a parent");
        for name in ["ak-scan.exe", "ak-scan"] {
            let p = dir.join(name);
            if p.is_file() {
                return p;
            }
        }
        // `target/<profile>/deps/` 是测试二进制常驻的位置。
        if let Some(up) = dir.parent() {
            for name in ["ak-scan.exe", "ak-scan"] {
                let p = up.join(name);
                if p.is_file() {
                    return p;
                }
            }
        }
        panic!(
            "ak-scan binary not found next to {} — run `cargo build -p agentkeeper-core --bins`",
            exe.display()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

/* ------------------------------------------------------------------ */
/* 1. ak-scan --shared                                                  */
/* ------------------------------------------------------------------ */

/// `--shared` 的三块必须同时在场，且 `library` 就是矩阵视图本尊
/// （键集合与 `list_library` 完全一致，不多也不少）。
#[test]
fn ak_scan_shared_prints_library_ledger_and_findings() {
    let fx = Fixture::new("shared-scan");
    fx.seed_skill(".claude", "pdf");

    let out = Command::new(Fixture::ak_scan())
        .arg("--shared")
        .arg(&fx.home)
        .arg(&fx.root)
        .output()
        .expect("run ak-scan --shared");
    assert!(
        out.status.success(),
        "ak-scan --shared failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON: {}\n{}",
            e,
            String::from_utf8_lossy(&out.stdout)
        )
    });

    // 顶层恰好三块 —— 多出来的键会让 diff 夹具每次都红。
    let keys: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["library", "ledger", "findings"], "{doc}");

    // library：与桌面端矩阵同源，同一个键集合。
    let direct = agentkeeper_shell_lib::core::library_view::list_library(&fx.keeper());
    let mut from_bin: Vec<&str> = doc["library"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let mut from_core: Vec<&str> = direct
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    from_bin.sort_unstable();
    from_core.sort_unstable();
    assert_eq!(
        from_bin, from_core,
        "library payload drifted from list_library"
    );
    assert_eq!(doc["library"]["ok"], json!(true));

    // ledger：文件不存在时是空台账，且**报 ok:true + 路径**，不是「读失败」。
    assert_eq!(doc["ledger"]["ok"], json!(true), "{}", doc["ledger"]);
    assert!(
        doc["ledger"]["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("links.json"),
        "{}",
        doc["ledger"]
    );
    assert_eq!(doc["ledger"]["entries"], json!([]));

    // findings：七类发现的固定外壳。
    assert_eq!(doc["findings"]["ok"], json!(true));
    assert!(doc["findings"]["findings"].is_array());
    assert!(doc["findings"]["counts"]["total"].is_u64());
}

/// 台账损坏时 `--shared` **不能**把损坏读成空：`ok:false` + 原因。
#[test]
fn ak_scan_shared_reports_a_corrupt_ledger_instead_of_an_empty_one() {
    let fx = Fixture::new("shared-scan-corrupt");
    write(&join(&fx.library_root(), &["links.json"]), "{");

    let out = Command::new(Fixture::ak_scan())
        .arg("--shared")
        .arg(&fx.home)
        .arg(&fx.root)
        .output()
        .expect("run ak-scan --shared");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(doc["ledger"]["ok"], json!(false), "{}", doc["ledger"]);
    assert!(
        doc["ledger"]["error"]
            .as_str()
            .is_some_and(|e| !e.is_empty()),
        "{}",
        doc["ledger"]
    );
}

/// **无参数时行为完全不变**：还是那份扫描载荷（`assets` / `projects` …），
/// 而不是 `--shared` 的三块。CI 依赖这一点。
#[test]
fn ak_scan_without_arguments_still_prints_the_scan_payload() {
    let fx = Fixture::new("shared-scan-plain");
    fx.seed_skill(".claude", "pdf");

    let out = Command::new(Fixture::ak_scan())
        .arg(&fx.home)
        .arg(&fx.root)
        .output()
        .expect("run ak-scan");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert!(doc["assets"].is_array(), "{doc}");
    assert!(doc.get("projects").is_some(), "{doc}");
    assert!(
        doc.get("library").is_none(),
        "the plain payload must not smuggle the shared blocks in: {doc}"
    );
    assert!(
        doc["assets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == json!("skill") && a["name"] == json!("pdf")),
        "the seeded skill must be scanned: {doc}"
    );
}

/* ------------------------------------------------------------------ */
/* 2. 库位置迁移：200 份 skill，杀在第 100 个                            */
/* ------------------------------------------------------------------ */

/// 300 个文件/目录的指纹：路径 → 内容长度 + 字节和。
///
/// 链接单独记（`usize::MAX`）：它的「内容」在迁移前后指向不同的库根，
/// 所以只判断它还在、且读得回目标。
fn tree_digest(root: &Path) -> Vec<(String, usize, u64)> {
    fn walk(dir: &Path, out: &mut Vec<(String, usize, u64)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if linkfs::is_link(&p) {
                out.push((p.to_string_lossy().to_string(), usize::MAX, 0));
                continue;
            }
            if p.is_dir() {
                walk(&p, out);
            } else if let Ok(bytes) = std::fs::read(&p) {
                let sum = bytes.iter().map(|b| *b as u64).sum::<u64>();
                out.push((p.to_string_lossy().to_string(), bytes.len(), sum));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// 端到端幂等验收的默认规模。发布前用 `AK_MIGRATE_SKILLS=200` 跑一次
/// 完整规模；默认取小值的原因见 [`migrating_a_large_library_is_end_to_end_idempotent`]。
const DEFAULT_SKILLS: usize = 20;

/// **幂等验收（§9.3）的端到端那一半**：`n` 份 skill × 2 个 agent，迁到
/// `<store>/library` 之后：
///
/// * 全部链接指到新库根、全部读得回内容；
/// * 顶层类别目录搬完、旧库根里不留 `links.json`；
/// * **再跑一次一个字节都不动**（`moved=0`，两棵树指纹相同）。
///
/// 规模由 `AK_MIGRATE_SKILLS` 决定，默认 [`DEFAULT_SKILLS`]。为什么默认不是 200：
/// 每次 `enable_asset` 都要全量重扫，200 份要跑二十多分钟，那会让整条门禁变成
/// 没人跑的东西；默认规模走的是**完全相同的代码路径**。
///
/// 「在第 100 个上强杀」由
/// `shared_ops::tests::migrate_resumes_the_relink_phase_without_redoing_finished_links`
/// 覆盖 —— 那里真的把 journal 停在链接阶段的正中间。
#[test]
fn migrating_a_large_library_is_end_to_end_idempotent() {
    let fx = Fixture::new("shared-migrate");
    if !fx.links_available() {
        eprintln!("skipped: this machine cannot create directory links");
        return;
    }
    let n = std::env::var("AK_MIGRATE_SKILLS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SKILLS);
    for i in 0..n {
        let slug = format!("skill-{:03}", i);
        for agent in [".claude", ".codex"] {
            fx.seed_skill(agent, &slug);
        }
    }
    for i in 0..n {
        let slug = format!("skill-{:03}", i);
        for agent in ["claude-code", "codex"] {
            let r = fx.enable(agent, &slug);
            assert_eq!(r["ok"], json!(true), "enable {} / {}: {}", agent, slug, r);
        }
    }
    let seeded = fx.library().load_ledger().expect("ledger");
    assert_eq!(
        seeded.entries.len(),
        n * 2,
        "fixture must create 2 links per skill"
    );

    let to = store::native_str(&Path::new(&fx.root).join("library"));
    let first = shared_ops::migrate_library(&fx.keeper(), &json!({ "to": to }));
    assert_eq!(first["ok"], json!(true), "{first}");
    // 库的顶层子项是**类别**（`skills` / `rules` / …），不是每个 skill 一项：
    // 搬迁阶段只搬一个 `skills`，重指阶段才是 `2 * n` 条链接。
    assert_eq!(first["total"], json!(1), "{first}");
    assert_eq!(first["links"], json!(n * 2), "{first}");
    assert_eq!(first["resumed"], json!(false), "{first}");

    let ledger = library::Library::at(&to).load_ledger().expect("new ledger");
    assert!(ledger.migrating.is_none(), "journal must be cleared");
    assert_eq!(ledger.entries.len(), n * 2);
    for e in &ledger.entries {
        assert!(e.src.starts_with(&to), "stale src: {}", e.src);
        assert!(
            linkfs::is_link(Path::new(&e.dst)),
            "dst is no longer a link: {}",
            e.dst
        );
        assert!(
            Path::new(&e.dst).join("SKILL.md").is_file(),
            "dst no longer resolves: {}",
            e.dst
        );
    }
    assert!(
        !Path::new(&fx.library_root()).join("links.json").exists(),
        "the old ledger must be gone"
    );

    // 重跑：零写入、树逐字节不变。
    let digest_after_first = tree_digest(Path::new(&fx.library_root()));
    let again = shared_ops::migrate_library(&fx.keeper(), &json!({ "to": to }));
    assert_eq!(again["ok"], json!(true), "{again}");
    assert_eq!(again["moved"], json!(0), "{again}");
    assert_eq!(again["links"], json!(0), "{again}");
    assert_eq!(
        tree_digest(Path::new(&fx.library_root())),
        digest_after_first,
        "re-running a finished migration must not touch a byte"
    );
}

/* ------------------------------------------------------------------ */
/* 3. bootstrap 的 shared                                                */
/* ------------------------------------------------------------------ */

/// `bootstrap.shared` 只有布尔与库根：**没有**、也不该有候选组数
/// （后者由共享页打开时的 `converge_plan` 现算）。
#[test]
fn bootstrap_shared_carries_no_scan_result() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shared-bootstrap-store");
    let _ = std::fs::remove_dir_all(&root);
    settings::ensure_store(&root.to_string_lossy()).expect("ensure_store");

    let shared = agentkeeper_shell_lib::shared_status(&store::native_str(&root));
    let keys: Vec<&str> = shared
        .as_object()
        .expect("shared payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["enabled", "root"], "{shared}");
    assert!(shared["enabled"].is_boolean(), "{shared}");
    assert!(
        shared["root"].as_str().unwrap_or_default().ends_with(".agents"),
        "the default library root is ~/.agents: {shared}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
