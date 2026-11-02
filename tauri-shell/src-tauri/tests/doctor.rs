//! 健康检查（M5）的端到端验收。
//!
//! 覆盖 `docs/SHARED-ASSETS-IMPLEMENTATION.md` §7.4 的三条验收：
//!
//! 1. 把一条受管链接手工换成真实目录 → `link.hijacked` 出现且 `fixable: true`
//!    （内容一致才允许 relink）；
//! 2. 删除库内主副本 → 所有指向它的链接报 `link.dangling`；
//! 3. 收敛 30 个 skill 前后，资产条数与「Curated + managed」计数按台账精确对上，
//!    且 30 个 id 不变。
//!
//! 另外钉住「doctor 只读」：每一次检查之后，台账与磁盘内容都逐字节不变。
//! 每个夹具都活在 `temp_dir()` 下自己的临时根里，`home` 也是假的，不碰真实用户目录。

mod common;

use agentkeeper_shell_lib::core::doctor;
use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::library;
use agentkeeper_shell_lib::core::linkfs;
use agentkeeper_shell_lib::core::registry;
use agentkeeper_shell_lib::core::scan;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::store::{self, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use common::{cleanup, join, read, temp_root, write};

/// 参与夹具的 agent（都声明了全局 `skills` 目录）。
fn agent_skills_dir(home: &str, agent_id: &str) -> String {
    let (dir, rel) = match agent_id {
        "claude-code" => (".claude", "skills"),
        "codex" => (".codex", "skills"),
        "cursor" => (".cursor", "skills"),
        other => panic!("fixture does not know agent {}", other),
    };
    join(home, &[dir, rel])
}

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
        registry::reload(&root);
        Fixture { dir, home, root }
    }

    fn keeper(&self) -> Keeper {
        Keeper::new(Store::new(&self.root), &self.home)
    }

    fn library_root(&self) -> String {
        join(&self.home, &[".agents"])
    }

    fn library_skill(&self, slug: &str) -> String {
        join(&self.library_root(), &["skills", slug])
    }

    fn agent_skill(&self, agent: &str, slug: &str) -> String {
        join(&agent_skills_dir(&self.home, agent), &[slug])
    }

    /// 造一份 skill 副本（真实目录，不是链接）。
    fn seed_skill(&self, agent: &str, slug: &str, body: &str) {
        write(
            &join(&self.agent_skill(agent, slug), &["SKILL.md"]),
            &format!(
                "---\nname: {}\ndescription: Fixture skill.\nversion: 1.0.0\n---\n\n{}\n",
                slug, body
            ),
        );
        write(
            &join(&self.agent_skill(agent, slug), &["assets/ref.txt"]),
            "reference material\n",
        );
    }

    fn scan(&self) -> Value {
        let projects = scan::read_projects(&self.root);
        let (payload, _timings) = scan::scan_payload(&self.home, &projects);
        payload
    }

    fn library(&self) -> library::Library {
        library::Library::resolve(&self.home, &settings::read(&self.root))
    }

    /// 真实的 doctor 入口：扫描 → 只读检查。返回报告与检查前的磁盘快照。
    fn diagnose(&self) -> doctor::DoctorReport {
        let payload = self.scan();
        let assets = payload["assets"].as_array().cloned().unwrap_or_default();
        doctor::diagnose(&self.library(), &assets, &self.home, None)
    }

    fn ledger(&self) -> library::Ledger {
        library::Library::at(self.library_root())
            .load_ledger()
            .expect("ledger readable")
    }

    fn skill_asset(&self, payload: &Value, agent: &str, slug: &str) -> Option<Value> {
        payload["assets"].as_array()?.iter().find_map(|a| {
            (a["kind"] == json!("skill")
                && a["agentId"] == json!(agent)
                && a["name"] == json!(slug))
            .then(|| a.clone())
        })
    }

    /// 用真实的 `enable_asset` 收敛一组（agent, slug）到库内主副本。
    /// 每做一次都重新扫描：`assetId` 只在这一刻的扫描结果里有效。
    fn enable(&self, agent: &str, slug: &str) -> Value {
        let payload = self.scan();
        let asset = self
            .skill_asset(&payload, agent, slug)
            .unwrap_or_else(|| panic!("no {} asset for {}", slug, agent));
        let r = self.keeper().enable_asset(&json!({
            "assetId": asset["id"],
            "agentId": agent,
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(true), "enable {}/{} -> {}", agent, slug, r);
        r
    }

    fn audit_bytes(&self) -> Vec<u8> {
        std::fs::read(join(&self.root, &["audit.jsonl"])).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

fn links_available(fx: &Fixture) -> bool {
    fx.library()
        .capability(&fx.root)
        .dir_order
        .iter()
        .any(|m| m.is_link())
}

/* ------------------------------------------------------------------ */
/* 验收 1：链接被换成真实目录 → link.hijacked（内容一致才可修）         */
/* ------------------------------------------------------------------ */

#[test]
fn replacing_a_managed_link_with_a_real_directory_is_reported_as_hijacked() {
    let _lock = common::serial();
    let fx = Fixture::new("doctor-hijacked");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# PDF\n");
    let r = fx.enable("claude-code", "pdf");
    let keeper = r["keeper"].as_str().expect("keeper").to_string();
    let dst = fx.agent_skill("claude-code", "pdf");
    assert!(linkfs::is_link(Path::new(&dst)), "{} is a link", dst);

    // 手工把链接换成一份内容相同的真实目录（模拟 agent 自己重建了副本）。
    let target = linkfs::read_target(Path::new(&dst)).expect("target");
    linkfs::unlink(Path::new(&dst)).expect("unlink");
    std::fs::create_dir_all(&dst).expect("real dir");
    write(
        &join(&dst, &["SKILL.md"]),
        &read(&join(&store::native_str(&target), &["SKILL.md"])),
    );
    write(
        &join(&dst, &["assets/ref.txt"]),
        &read(&join(&store::native_str(&target), &["assets/ref.txt"])),
    );
    assert!(!linkfs::is_link(Path::new(&dst)), "now it is a real dir");
    assert!(Path::new(&keeper).is_dir(), "library copy still there");

    // doctor 只读：检查前后台账逐字节不变。
    let ledger_before = read(&join(&fx.library_root(), &["links.json"]));
    let report = fx.diagnose();
    assert_eq!(
        read(&join(&fx.library_root(), &["links.json"])),
        ledger_before,
        "doctor never writes the ledger"
    );

    let hijacked = report.by_code(doctor::LINK_HIJACKED);
    assert_eq!(hijacked.len(), 1, "one hijacked link: {:?}", report);
    assert_eq!(hijacked[0].severity, doctor::SEV_CRITICAL);
    assert_eq!(hijacked[0].path, store::native_str(Path::new(&dst)));
    assert_eq!(hijacked[0].detail["contentMatches"], json!(true));
    assert_eq!(hijacked[0].fix, Some(doctor::FIX_RELINK));
    assert!(hijacked[0].fixable, "same content -> relink is allowed");
    assert_eq!(report.by_code(doctor::LINK_DANGLING).len(), 0, "{:?}", report);
}

/* ------------------------------------------------------------------ */
/* 验收 2：删掉库内主副本 → 所有指向它的链接报 link.dangling            */
/* ------------------------------------------------------------------ */

#[test]
fn deleting_the_library_copy_makes_every_link_dangle() {
    let _lock = common::serial();
    let fx = Fixture::new("doctor-dangling");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    for agent in ["claude-code", "codex", "cursor"] {
        fx.seed_skill(agent, "notes", "# shared notes\n");
    }
    for agent in ["claude-code", "codex", "cursor"] {
        fx.enable(agent, "notes");
    }
    let keeper = fx.library_skill("notes");
    assert!(Path::new(&keeper).is_dir());
    assert_eq!(fx.ledger().entries.len(), 3, "three managed links");

    // 删掉库内主副本：链接全部悬空。
    store::rmrf(Path::new(&keeper));
    assert!(!Path::new(&keeper).exists());

    let report = fx.diagnose();
    let dangling = report.by_code(doctor::LINK_DANGLING);
    assert_eq!(dangling.len(), 3, "every link dangles: {:?}", report);
    for f in &dangling {
        assert_eq!(f.severity, doctor::SEV_CRITICAL);
        assert_eq!(f.fix, Some(doctor::FIX_RELINK));
        assert!(f.fixable);
        assert_eq!(f.detail["target"], json!(store::native_str(Path::new(&keeper))));
    }
    let paths: Vec<String> = dangling.iter().map(|f| f.path.clone()).collect();
    for agent in ["claude-code", "codex", "cursor"] {
        let dst = store::native_str(Path::new(&fx.agent_skill(agent, "notes")));
        assert!(paths.contains(&dst), "{} reported: {:?}", agent, paths);
    }
    // 悬空的链接仍然是链接，不能被误判成「被顶掉」。
    assert_eq!(report.by_code(doctor::LINK_HIJACKED).len(), 0, "{:?}", report);
}

/* ------------------------------------------------------------------ */
/* 验收 3：30 个 skill 收敛前后，计数与台账对得上、id 不变              */
/* ------------------------------------------------------------------ */

#[test]
fn thirty_converged_skills_keep_their_ids_and_match_the_ledger() {
    let _lock = common::serial();
    let fx = Fixture::new("doctor-thirty");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    let agents = ["claude-code", "codex"];
    let slugs: Vec<String> = (0..30).map(|i| format!("skill-{:02}", i)).collect();
    for slug in &slugs {
        for agent in agents {
            fx.seed_skill(agent, slug, &format!("# {}\n", slug));
        }
    }

    // 收敛前：60 份真实副本，每份一个 id。
    let before = fx.scan();
    assert_eq!(before["summary"]["by"]["skill"], json!(60));
    let mut ids_before: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    for slug in &slugs {
        for agent in agents {
            let asset = fx
                .skill_asset(&before, agent, slug)
                .unwrap_or_else(|| panic!("no {} asset before converge", slug));
            assert!(asset["meta"]["link"].is_null(), "seed copy is real");
            ids_before.insert(
                (agent.to_string(), slug.clone()),
                asset["id"].as_str().expect("id").to_string(),
            );
        }
    }
    assert_eq!(ids_before.len(), 60);

    // 真实写入路径：逐个 enable，库内主副本 = 第一个 agent 的那一份。
    for slug in &slugs {
        for agent in agents {
            fx.enable(agent, slug);
        }
    }

    let after = fx.scan();
    let ledger = fx.ledger();
    assert_eq!(ledger.entries.len(), 60, "one ledger line per agent link");

    // 资产条数：同一物理内容在资产表里只出现一次（§17.7 的既有口径）。
    let linked: Vec<Value> = after["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .filter(|a| a["kind"] == json!("skill"))
        .cloned()
        .collect();
    assert_eq!(linked.len(), 30, "one physical asset per skill");
    assert_eq!(after["summary"]["by"]["skill"], json!(30));
    // 「Curated + managed」= origin 为 user 或 managed 的资产；30 个 skill 都是 user。
    let curated = after["summary"]["byOrigin"]["user"].as_u64().unwrap_or(0)
        + after["summary"]["byOrigin"]["managed"].as_u64().unwrap_or(0);
    assert_eq!(curated, 30, "Curated + managed counts the 30 assets");

    // 台账与「扫描里出现的受管位置」精确对上：每个 agent 上一条链接进台账；
    // 库内主副本也在扫描结果里（伪 agent `shared`），但它不是链接、不进台账。
    let mut managed_positions = 0usize;
    let mut total_positions = 0usize;
    for a in &linked {
        assert_eq!(a["meta"]["link"]["managed"], json!(true), "{}", a["name"]);
        let also = a["meta"]["alsoSeen"].as_array().cloned().unwrap_or_default();
        total_positions += 1 + also.len();
        if a["agentId"] != json!("shared") {
            managed_positions += 1;
        }
        for e in &also {
            if e["agentId"] == json!("shared") {
                continue; // 库内主副本：真实的，不是链接
            }
            managed_positions += 1;
        }
        // 30 个 id 不变：留下的 id 必须是收敛前第一个 agent 的那一个。
        let slug = a["name"].as_str().expect("name");
        let agent = a["agentId"].as_str().expect("agentId");
        let want = ids_before
            .get(&(agent.to_string(), slug.to_string()))
            .unwrap_or_else(|| panic!("no pre-converge id for {}/{}", agent, slug));
        assert_eq!(a["id"].as_str().expect("id"), want, "{}/{} keeps its id", agent, slug);
    }
    assert_eq!(
        managed_positions,
        ledger.entries.len(),
        "every managed link position is in the ledger"
    );
    assert_eq!(
        total_positions,
        30 + ledger.entries.len(),
        "30 library copies + 60 links"
    );

    // 收敛干净之后，健康检查应当一条 finding 都没有。
    let report = fx.diagnose();
    assert!(
        report.is_empty(),
        "a clean converge leaves no findings: {:?}",
        report
    );
    assert_eq!(report.counts()["total"], json!(0));
}

/* ------------------------------------------------------------------ */
/* 只读：doctor 不写审计、不改磁盘                                      */
/* ------------------------------------------------------------------ */

#[test]
fn doctor_never_writes_anything() {
    let _lock = common::serial();
    let fx = Fixture::new("doctor-readonly");
    fx.seed_skill("claude-code", "pdf", "# PDF\n");

    let audit_before = fx.audit_bytes();
    let tree_before = common::dir_snapshot(&fx.dir.to_string_lossy());
    let _ = fx.diagnose();
    let _ = fx.diagnose();
    assert_eq!(fx.audit_bytes(), audit_before, "doctor writes no audit line");
    assert_eq!(
        common::dir_snapshot(&fx.dir.to_string_lossy()),
        tree_before,
        "doctor leaves the tree byte-identical"
    );
}
