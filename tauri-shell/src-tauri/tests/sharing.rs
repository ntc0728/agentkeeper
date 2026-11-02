//! 共享层（M2 · Skills 共享）的端到端验收。
//!
//! 覆盖 `docs/SHARED-ASSETS-IMPLEMENTATION.md` §4.4 的三条验收，外加「先删后建、
//! 同一事务」与「停用只删链接」这两个安全不变式：
//!
//! 1. 5 个 agent × 同名同内容 skill → 收敛后库内 1 份、5 条链接、`audit.jsonl`
//!    新增记录、改库内 `SKILL.md` 后五个位置同时读到新内容；
//! 2. 内容不同的一组 → 收敛入口置灰（`plan_converge` 返回 `diverged` 且列出差异
//!    文件名）；
//! 3. 停用 → 只删链接，库内主副本一个字节不动；从库里删 → 进 `.trash`、链接全删。
//!
//! 每个夹具都活在 `temp_dir()` 下自己的临时根里，`home` 也是假的：整个套件不碰
//! 真实用户目录，也不依赖本机装没装哪个 agent。

mod common;

use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::linkfs;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::sharing;
use agentkeeper_shell_lib::core::store::{self, Store};
use agentkeeper_shell_lib::core::{library, registry, scan};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use common::{cleanup, join, read, temp_root, write};

/// 参与收敛夹具的五个 agent（都声明了 `skills` 目录，且都是全局作用域）。
const AGENTS: [&str; 5] = ["claude-code", "codex", "cursor", "gemini-cli", "pi"];

/// 每个 agent 的全局 skill 目录（`<home>/<rel>`）。
fn agent_skills_dir(home: &str, agent_id: &str) -> String {
    let (dir, rel) = match agent_id {
        "claude-code" => (".claude", "skills"),
        "codex" => (".codex", "skills"),
        "cursor" => (".cursor", "skills"),
        "gemini-cli" => (".gemini", "skills"),
        "pi" => (".pi/agent", "skills"),
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

    /// 找到某个 agent 上某个 slug 的 skill 资产。
    fn skill_asset(&self, payload: &Value, agent: &str, slug: &str) -> Option<Value> {
        payload["assets"]
            .as_array()?
            .iter()
            .find(|a| {
                a["kind"] == json!("skill")
                    && a["agentId"] == json!(agent)
                    && a["name"] == json!(slug)
            })
            .cloned()
    }

    fn plan(&self, payload: &Value) -> sharing::ConvergePlan {
        let lib = library::Library::resolve(&self.home, &settings::read(&self.root));
        let cap = lib.capability(&self.root);
        let assets = payload["assets"].as_array().cloned().unwrap_or_default();
        sharing::plan_converge(&assets, &lib, &cap)
    }

    fn audit_lines(&self) -> usize {
        read(&join(&self.root, &["audit.jsonl"]))
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    }

    fn ledger(&self) -> library::Ledger {
        library::Library::at(self.library_root())
            .load_ledger()
            .expect("ledger readable")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

/// 本机能不能建目录链接？不能的话收敛夹具没有意义（`cap.dir_order` 里没有链接方式）。
fn links_available(fx: &Fixture) -> bool {
    let lib = library::Library::resolve(&fx.home, &settings::read(&fx.root));
    lib.capability(&fx.root)
        .dir_order
        .iter()
        .any(|m| m.is_link())
}

/* ------------------------------------------------------------------ */
/* 验收 1：5 份同名同内容 → 1 库 + 5 链接，改一处五处生效              */
/* ------------------------------------------------------------------ */

#[test]
fn five_identical_skills_converge_into_one_library_copy() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-converge");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    for agent in AGENTS {
        fx.seed_skill(agent, "pdf", "# PDF\n\nExtract text from PDF files.\n");
    }

    // 收敛前：五份真实副本、同一个 slug，方案判 `identical` 且给出替换清单。
    let before = fx.scan();
    let plan = fx.plan(&before);
    let group = plan
        .groups
        .iter()
        .find(|g| g.name == "pdf")
        .expect("the pdf group is planned");
    assert_eq!(group.verdict, sharing::Verdict::Identical);
    assert_eq!(group.replace.len(), AGENTS.len() - 1);
    assert!(plan.saved_bytes > 0, "converging saves bytes");

    // 记下五份的 id：收敛之后 id 必须一个不少、一个不变（§7.1 的前提）。
    let ids_before: Vec<String> = AGENTS
        .iter()
        .map(|agent| {
            fx.skill_asset(&before, agent, "pdf")
                .unwrap_or_else(|| panic!("no pdf asset for {}", agent))["id"]
                .as_str()
                .expect("id")
                .to_string()
        })
        .collect();
    assert_eq!(ids_before.len(), 5);

    // 逐个启用：同一个事务里「删掉来源那份 + 建一条指向库内主副本的链接」。
    let audit_before = fx.audit_lines();
    let mut first_keeper: Option<String> = None;
    for agent in AGENTS {
        let payload = fx.scan();
        let asset = fx.skill_asset(&payload, agent, "pdf").expect("asset");
        let r = fx.keeper().enable_asset(&json!({
            "assetId": asset["id"],
            "agentId": agent,
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(true), "enable {} -> {}", agent, r);
        match &first_keeper {
            None => first_keeper = r["keeper"].as_str().map(|s| s.to_string()),
            Some(k) => assert_eq!(Some(k.as_str()), r["keeper"].as_str(), "one keeper for all"),
        }
    }

    // 库内恰好一份主副本，五个 agent 侧全是指向它的链接。
    let keeper = first_keeper.expect("keeper path");
    assert!(Path::new(&keeper).is_dir(), "keeper exists: {}", keeper);
    for agent in AGENTS {
        let dst = fx.agent_skill(agent, "pdf");
        assert!(
            linkfs::is_link(Path::new(&dst)),
            "{} should be a link: {}",
            agent,
            dst
        );
        let target = linkfs::read_target(Path::new(&dst)).expect("target readable");
        assert!(
            linkfs::same_target(&target, Path::new(&keeper)),
            "{} points at the keeper",
            agent
        );
    }

    // 审计：每一次事务至少一行；这里只断言「确实写了审计线」，不钉死行数。
    assert!(
        fx.audit_lines() > audit_before,
        "the transactions are audited"
    );

    // 台账：五条，全部指向库内主副本。
    let ledger = fx.ledger();
    let entries: Vec<_> = ledger.links_to(&keeper);
    assert_eq!(entries.len(), AGENTS.len(), "one ledger line per agent");

    // 改库内正文 → 五个位置同时读到新内容。
    let manifest = join(&keeper, &["SKILL.md"]);
    write(&manifest, "---\nname: pdf\n---\n\nNEW BODY\n");
    for agent in AGENTS {
        let seen = read(&join(&fx.agent_skill(agent, "pdf"), &["SKILL.md"]));
        assert!(seen.contains("NEW BODY"), "{} sees the new body", agent);
    }

    // 再扫一次：id 一个不少、一个不变，且资产被标成链接（`meta.link`）。
    // 再扫一次。注意这是**现有扫描器的既有口径**：`dedupe_by_physical_path` 按
    // canonicalize 后的物理路径去重，而收敛之后五个位置就是同一个物理目录，所以
    // 资产列表只剩一条（id 与原来那五条中的第一条相同），其余四个 agent 记在
    // `meta.alsoSeen` 里。收敛前后「同一个物理资产的条数不变」正是这条规则想要的
    // 结果 —— 改它不在本阶段（P5 的 `sameInode` 才处理 `effective` 视图）。
    let after = fx.scan();
    let linked: Vec<Value> = after["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .filter(|a| a["kind"] == json!("skill") && a["name"] == json!("pdf"))
        .cloned()
        .collect();
    assert_eq!(linked.len(), 1, "one physical asset, not five");
    assert_eq!(
        linked[0]["id"].as_str().unwrap(),
        ids_before[0],
        "the surviving id is the first agent's"
    );
    let also = linked[0]["meta"]["alsoSeen"].as_array().cloned().unwrap_or_default();
    let mut seen_agents: Vec<String> = vec![linked[0]["agentId"].as_str().unwrap().to_string()];
    seen_agents.extend(
        also.iter()
            .filter_map(|e| e["agentId"].as_str().map(|s| s.to_string())),
    );
    for agent in AGENTS {
        assert!(
            seen_agents.iter().any(|a| a == agent),
            "{} still appears in the scan",
            agent
        );
    }
    // 留下的那一条带 `meta.link`，且指向库内主副本。
    assert!(linked[0]["meta"]["link"].is_object(), "carries meta.link");
    assert_eq!(linked[0]["meta"]["link"]["managed"], json!(true));
    assert_eq!(
        linked[0]["meta"]["link"]["src"]
            .as_str()
            .map(|s| store::native_str(std::path::Path::new(s))),
        Some(store::native_str(std::path::Path::new(&keeper))),
        "reports the library copy as its link source"
    );

    // 收敛过之后再出方案：这几组已经不该再被提出来。
    let again = fx.plan(&after);
    assert!(
        again.groups.iter().all(|g| g.name != "pdf" || g.replace.is_empty()),
        "nothing left to converge"
    );
}

/// 收敛之后 disk 上只剩一份真实内容：其余四处是链接，库内那一份是真实目录。
#[test]
fn converging_leaves_exactly_one_real_directory() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-one-real");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    for agent in ["claude-code", "codex"] {
        fx.seed_skill(agent, "notes", "# same\n");
    }
    for agent in ["claude-code", "codex"] {
        let payload = fx.scan();
        let asset = fx.skill_asset(&payload, agent, "notes").expect("asset");
        let r = fx.keeper().enable_asset(&json!({
            "assetId": asset["id"],
            "agentId": agent,
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(true), "{}", r);
    }
    let keeper = fx.library_skill("notes");
    assert!(Path::new(&keeper).is_dir());
    assert!(!linkfs::is_link(Path::new(&keeper)), "the keeper is real");
    for agent in ["claude-code", "codex"] {
        assert!(linkfs::is_link(Path::new(&fx.agent_skill(agent, "notes"))));
    }
}

/* ------------------------------------------------------------------ */
/* 验收 2：内容不同 → 不收敛，列出差异文件名                            */
/* ------------------------------------------------------------------ */

#[test]
fn diverged_skills_are_reported_with_diff_file_names() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-diverged");
    fx.seed_skill("claude-code", "report", "# one\n");
    fx.seed_skill("codex", "report", "# two\n");

    let payload = fx.scan();
    let plan = fx.plan(&payload);
    let group = plan
        .groups
        .iter()
        .find(|g| g.name == "report")
        .expect("group planned");
    assert_eq!(group.verdict, sharing::Verdict::Diverged);
    assert!(group.replace.is_empty(), "a diverged group is never replaced");
    assert_eq!(plan.actionable(), 0, "the converge entry is greyed out");
    let rels: Vec<&str> = group.diffs.iter().map(|d| d.rel.as_str()).collect();
    assert!(rels.contains(&"SKILL.md"), "diff names the changed file");

    // 只读：方案不建库目录、不写台账。
    assert!(!Path::new(&fx.library_root()).exists());
    let r = fx.keeper().enable_asset(&json!({
        "assetId": "skill:does-not-exist",
        "agentId": "codex",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["code"], json!("share.unknownAsset"));
}

/* ------------------------------------------------------------------ */
/* 验收 3：停用只删链接；从库里删进 .trash                              */
/* ------------------------------------------------------------------ */

#[test]
fn disabling_removes_only_the_link_and_keeps_the_library_copy() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-disable");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# body\n");
    let payload = fx.scan();
    let asset = fx.skill_asset(&payload, "claude-code", "pdf").expect("asset");
    let enabled = fx.keeper().enable_asset(&json!({
        "assetId": asset["id"],
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(enabled["ok"], json!(true), "{}", enabled);
    let keeper = fx.library_skill("pdf");
    let keeper_body = read(&join(&keeper, &["SKILL.md"]));

    let audit_before = fx.audit_lines();
    let r = fx.keeper().disable_asset(&json!({
        "name": "pdf",
        "kind": "skill",
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(true), "{}", r);

    // agent 侧没了；库内主副本逐字节还在；台账清掉了这一条。
    let dst = fx.agent_skill("claude-code", "pdf");
    assert!(!Path::new(&dst).exists() && !linkfs::is_link(Path::new(&dst)));
    assert_eq!(read(&join(&keeper, &["SKILL.md"])), keeper_body);
    assert!(fx.ledger().links_to(&keeper).is_empty());
    assert!(fx.audit_lines() > audit_before);
}

/// 三条安全闸：不是链接就拒绝；指向库外就拒绝；两者都不许动盘。
#[test]
fn disabling_refuses_paths_that_are_not_managed_links() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-guards");

    // 闸 1：agent 侧是真实目录（用户自己的副本）→ `share.notLinked`，目录原样留着。
    fx.seed_skill("claude-code", "real", "# mine\n");
    let dst = fx.agent_skill("claude-code", "real");
    let r = fx.keeper().disable_asset(&json!({
        "name": "real",
        "kind": "skill",
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["code"], json!("share.notLinked"));
    assert!(Path::new(&dst).is_dir(), "the real directory is untouched");
    assert!(Path::new(&join(&dst, &["SKILL.md"])).is_file());

    // 闸 2：agent 侧是一条指向库外的链接 → `share.foreignLink`。
    if links_available(&fx) {
        let outside = join(&fx.home, &["elsewhere", "shared-skill"]);
        write(&join(&outside, &["SKILL.md"]), "# outside\n");
        let agent_dir = agent_skills_dir(&fx.home, "codex");
        std::fs::create_dir_all(&agent_dir).expect("agent dir");
        let foreign = join(&agent_dir, &["foreign"]);
        linkfs::create_dir_link(Path::new(&outside), Path::new(&foreign), None).expect("link");
        let r = fx.keeper().disable_asset(&json!({
            "name": "foreign",
            "kind": "skill",
            "agentId": "codex",
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["code"], json!("share.foreignLink"));
        assert!(linkfs::is_link(Path::new(&foreign)), "the foreign link stays");
        assert!(Path::new(&join(&outside, &["SKILL.md"])).is_file());
    }
}

#[test]
fn removing_from_the_library_moves_it_to_the_trash_and_unlinks_every_agent() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-trash");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    for agent in ["claude-code", "codex"] {
        fx.seed_skill(agent, "pdf", "# body\n");
    }
    for agent in ["claude-code", "codex"] {
        let payload = fx.scan();
        let asset = fx.skill_asset(&payload, agent, "pdf").expect("asset");
        let r = fx.keeper().enable_asset(&json!({
            "assetId": asset["id"],
            "agentId": agent,
            "scope": "global",
        }));
        assert_eq!(r["ok"], json!(true), "{}", r);
    }
    let keeper = fx.library_skill("pdf");
    assert!(Path::new(&keeper).is_dir());

    let r = fx.keeper().remove_from_library("skill", "pdf");
    assert_eq!(r["ok"], json!(true), "{}", r);
    // 主副本进了回收站，链接全删，库内那条目录不在了。
    let trashed = r["trashed"].as_str().expect("trashed path").to_string();
    assert!(Path::new(&trashed).is_dir(), "kept in .trash: {}", trashed);
    assert!(trashed.replace('/', "\\").contains("\\.trash\\skills\\"));
    assert!(Path::new(&join(&trashed, &["SKILL.md"])).is_file());
    assert_eq!(r["failed"].as_array().map(|a| a.len()), Some(0));
    assert!(!Path::new(&keeper).exists());
    for agent in ["claude-code", "codex"] {
        let dst = fx.agent_skill(agent, "pdf");
        assert!(!linkfs::is_link(Path::new(&dst)), "{} link removed", agent);
    }
    assert!(fx.ledger().links_to(&keeper).is_empty());

    // 回收站里有审计记录（每一行都带 action）。
    let audit = read(&join(&fx.root, &["audit.jsonl"]));
    assert!(audit.contains("library.trash"));
    assert!(audit.contains("library.unlink"));
}

/* ------------------------------------------------------------------ */
/* 事务性：启用失败时不留半对状态                                        */
/* ------------------------------------------------------------------ */

#[test]
fn a_failed_enable_leaves_the_source_untouched() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-atomic");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# body\n");
    let payload = fx.scan();
    let asset = fx.skill_asset(&payload, "claude-code", "pdf").expect("asset");
    // 先在库内造一堵墙：`skills` 被占成**文件**，主副本目录建不出来。
    write(&join(&fx.library_root(), &["skills"]), "not a directory\n");
    let src = fx.agent_skill("claude-code", "pdf");
    let snapshot: Vec<String> = store::list_files(Path::new(&src));

    let r = fx.keeper().enable_asset(&json!({
        "assetId": asset["id"],
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(false), "the transaction must fail: {}", r);
    // 来源目录原样还在，仍是真实目录而不是链接。
    assert!(Path::new(&src).is_dir());
    assert!(!linkfs::is_link(Path::new(&src)));
    assert_eq!(store::list_files(Path::new(&src)), snapshot);
}

/// 已经在库里的主副本可以「补启用」到别的 agent，而不必再走一次收敛。
#[test]
fn enabling_an_asset_already_in_the_library_links_without_copying() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-adopt");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# body\n");
    let payload = fx.scan();
    let asset = fx.skill_asset(&payload, "claude-code", "pdf").expect("asset");
    let first = fx.keeper().enable_asset(&json!({
        "assetId": asset["id"],
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(first["ok"], json!(true), "{}", first);

    // 不带 assetId：凭 name 找到库内已有的主副本。
    let second = fx.keeper().enable_asset(&json!({
        "name": "pdf",
        "kind": "skill",
        "agentId": "codex",
        "scope": "global",
    }));
    assert_eq!(second["ok"], json!(true), "{}", second);
    assert_eq!(second["storedInLibrary"], json!(false));
    let dst = fx.agent_skill("codex", "pdf");
    assert!(linkfs::is_link(Path::new(&dst)));
    assert!(fx.ledger().links_to(&fx.library_skill("pdf")).len() == 2);
}

/// 不认识的 kind 给出可读的 code，而不是静默成功。
///
/// （M2 时代这条测的是 `rule` → `rule.shareUnsupported`；M4 起规则与记忆可以共享，
/// 那条断言已经过期，换成真的不支持的 kind。规则共享的验收在 `rule_sharing.rs`。）
#[test]
fn enabling_an_unsupported_kind_reports_a_code() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-kinds");
    let r = fx.keeper().enable_asset(&json!({
        "name": "AGENTS",
        "kind": "plugin",
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["code"], json!("share.unsupportedKind"));
}

/// 审核发现（M7）：`enable_asset` 在「目标已是链接」时只看 `owns_target` 就返回
/// `already linked`，于是**指错了地方**的链接——比如库迁移后仍指着旧库根、或被别的
/// 资产占位——永远没人修，调用方（尤其经 MCP 进来的 agent，它没有「重指」入口）
/// 拿到的却是一次「成功」。
///
/// 现在必须比对到**本资产的库内主副本**才算空操作；不一致就走同一事务里的
/// 删 + 重指。
#[test]
fn enabling_repoints_a_link_that_points_at_the_wrong_master_copy() {
    let _lock = common::serial();
    let fx = Fixture::new("sharing-repoint");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# PDF\n\nExtract text.\n");
    // 库里先有两份主副本：pdf 与别的（notes），以及一份 pdf 的真身。
    fx.keeper().enable_asset(&json!({
        "assetId": fx.scan()["assets"].as_array().unwrap().iter()
            .find(|a| a["kind"] == json!("skill") && a["name"] == json!("pdf") && a["agentId"] == json!("claude-code"))
            .unwrap()["id"],
        "agentId": "claude-code",
        "scope": "global",
    }));

    // 把 agent 侧那条链接**改指到别的资产**（模拟迁移残留 / 被别处接走）：
    // 先撤掉真链接，再手建一条指向库内 notes 的链接。
    let dst = fx.agent_skill("claude-code", "pdf");
    let dst_path = std::path::Path::new(&dst);
    assert!(linkfs::is_link(dst_path), "fixture must start from a linked state");
    linkfs::unlink(dst_path).expect("unlink fixture link");
    std::fs::create_dir_all(&fx.library_skill("notes")).expect("notes master");
    std::fs::write(join(&fx.library_skill("notes"), &["SKILL.md"]), "# notes\n").expect("write notes");
    linkfs::create_dir_link(
        std::path::Path::new(&fx.library_skill("notes")),
        dst_path,
        None,
    )
    .expect("plant a wrong-target link");

    // 现在 enable：它不能再报 already linked，而要把链接重指回 pdf 的主副本。
    let r = fx.keeper().enable_asset(&json!({
        "name": "pdf",
        "kind": "skill",
        "agentId": "claude-code",
        "scope": "global",
    }));
    assert_eq!(r["ok"], json!(true), "{r}");
    assert!(
        r["written"].as_array().is_some_and(|w| !w.is_empty()),
        "a wrong-target link must be repointed, not skipped: {r}"
    );
    let target = linkfs::read_target(dst_path).unwrap_or_default();
    assert!(
        linkfs::same_target(&target, std::path::Path::new(&fx.library_skill("pdf"))),
        "link must now point at pdf's own master: {:?} -> {}",
        target,
        fx.library_skill("pdf")
    );
}
