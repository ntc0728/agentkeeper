//! 共享页（界面契约）的端到端验收。
//!
//! 覆盖界面契约里界面直接依赖的五块数据面：
//!
//! 1. `list_library` 的键集合与矩阵形状（列 = agent，行 = 资产，单元格状态词表）；
//! 2. `plan_converge` 只读 —— 调用前后整棵夹具逐字节不变；
//! 3. `shared_doctor` 的形状（`ok` / `findings` / `counts`）；
//! 4. `apply_converge` 之后 `list_library` 里对应单元格从 `copy` 变成 `on`；
//! 5. `apply_finding_fix(link.dangling)` 真的把一条悬空链接接回去。
//!
//! 命令层（`src/lib.rs` 的 8 个 `#[tauri::command]`）只是
//! `open_keeper()` + `core::…` 的薄适配，所以这里直接测 core 的入口；命令
//! 注册与桥接形状由 `node scripts/check.js bridge` 把关。
//!
//! 夹具约定与 `tests/doctor.rs` 相同：每个测试在 `temp_dir()` 下拥有自己的
//! 临时根目录与假 home，结束时删掉；会碰进程级资产缓存的测试先持有
//! [`common::serial`] 这把锁。

mod common;

use agentkeeper_shell_lib::core::doctor;
use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::library;
use agentkeeper_shell_lib::core::library_view;
use agentkeeper_shell_lib::core::linkfs;
use agentkeeper_shell_lib::core::registry;
use agentkeeper_shell_lib::core::scan;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::shared_ops;
use agentkeeper_shell_lib::core::store::{self, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use common::{cleanup, join, read, temp_root, write};

/// 单元格状态词表（见 `docs/DESIGN.md` §8.12 界面契约）。渲染层只认这些值。
const CELL_STATES: [&str; 7] = [
    "on",
    "off",
    "copy",
    "foreign",
    "drift",
    "unsupported",
    "error",
];

/// 矩阵里可能出现的四类行。
const ROW_KINDS: [&str; 4] = ["skills", "mcp", "rules", "memory"];

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

    fn agent_skills_dir(&self, agent: &str) -> String {
        agent_skills_dir(&self.home, agent)
    }

    fn agent_skill(&self, agent: &str, slug: &str) -> String {
        join(&self.agent_skills_dir(agent), &[slug])
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

    /// `list_library` 的命令体（`open_keeper()` 在命令层，这里等价）。
    fn list(&self) -> Value {
        library_view::list_library(&self.keeper())
    }

    /// `plan_converge` 的命令体。
    fn plan(&self) -> Value {
        shared_ops::converge_plan(&self.keeper())
    }

    /// `shared_doctor` 的命令体。
    fn doctor_report(&self) -> Value {
        shared_ops::doctor_report(&self.keeper())
    }

    /// `apply_converge` 的命令体。
    fn apply_converge(&self, input: Value) -> Value {
        shared_ops::apply_converge(&self.keeper(), &input)
    }

    /// `apply_finding_fix` 的命令体。
    fn apply_fix(&self, input: Value) -> Value {
        shared_ops::apply_finding_fix(&self.keeper(), &input)
    }

    fn scan(&self) -> Value {
        let projects = scan::read_projects(&self.root);
        let (payload, _timings) = scan::scan_payload(&self.home, &projects);
        payload
    }

    /// 用真实的 `enable_asset` 收敛一个（agent, slug）到库内主副本。
    fn enable(&self, agent: &str, slug: &str) -> Value {
        let payload = self.scan();
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

    fn library(&self) -> library::Library {
        library::Library::resolve(&self.home, &settings::read(&self.root))
    }

    fn ledger(&self) -> library::Ledger {
        library::Library::at(self.library_root())
            .load_ledger()
            .expect("ledger readable")
    }

    /// 预热链接能力探测。探测会往 `<store>/probe` 写文件（`Library::capability`
    /// 的既有行为），所以「只读」断言必须从探测之后开始算。
    fn warm_capability(&self) {
        let _ = self.library().capability(&self.root);
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

fn row<'a>(data: &'a Value, kind: &str, name: &str) -> &'a Value {
    data[kind]
        .as_array()
        .unwrap_or_else(|| panic!("{} must be an array", kind))
        .iter()
        .find(|r| r["name"] == json!(name))
        .unwrap_or_else(|| panic!("no {} row named {}: {}", kind, name, data[kind]))
}

/* ------------------------------------------------------------------ */
/* 1. list_library：键集合与矩阵形状                                     */
/* ------------------------------------------------------------------ */

#[test]
fn list_library_has_the_contract_shape() {
    let _lock = common::serial();
    let fx = Fixture::new("shared-shape");
    fx.seed_skill("claude-code", "pdf", "# PDF\n");

    let data = fx.list();
    assert_eq!(data["ok"], json!(true), "list_library: {}", data);

    // 顶层键就是 `docs/DESIGN.md` §8.12 那 8 个，多了少了都说明数据面走样。
    let obj = data.as_object().expect("list_library returns an object");
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["agents", "cap", "mcp", "memory", "ok", "root", "rules", "skills"],
        "list_library top-level keys: {}",
        data
    );
    assert_eq!(
        data["root"],
        json!(store::native_str(Path::new(&fx.library_root()))),
        "root is the library root, not the store root"
    );

    // 链接能力：五个布尔 + 两个候选顺序 + 探测位置。
    let cap = &data["cap"];
    for k in ["symlinkDir", "symlinkFile", "junction", "hardlink", "copyOnly"] {
        assert!(cap[k].is_boolean(), "cap.{} must be a boolean: {}", k, cap);
    }
    for k in ["dirOrder", "fileOrder"] {
        assert!(cap[k].is_array(), "cap.{} must be an array: {}", k, cap);
    }
    assert!(
        cap["probedAt"].is_string(),
        "cap.probedAt must be a string: {}",
        cap
    );

    // 列：每个已解析出 home 的 agent 一条，字段与 `docs/DESIGN.md` §8.12 对齐。
    let agents = data["agents"].as_array().expect("agents array");
    let agent_ids: Vec<String> = agents
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_string))
        .collect();
    let claude = agents
        .iter()
        .find(|a| a["id"] == json!("claude-code"))
        .unwrap_or_else(|| panic!("claude-code column missing: {}", data["agents"]));
    assert_eq!(claude["installed"], json!(true));
    assert!(claude["label"].is_string(), "agent label: {}", claude);
    assert!(claude["accent"].is_string(), "agent accent: {}", claude);
    assert_eq!(
        claude["skillsDir"],
        json!(store::native_str(Path::new(&fx.agent_skills_dir("claude-code")))),
        "skillsDir comes from the adapter table: {}",
        claude
    );
    for k in ["ruleDirs", "memoryDirs", "mcpFiles"] {
        assert!(claude[k].is_array(), "agent {} must be an array: {}", k, claude);
    }
    assert!(claude["mcpStyle"].is_string(), "mcpStyle: {}", claude);

    // 还没收敛：库里没有主副本，agent 侧那份是真实目录 → 外部行 + copy 单元格。
    let pdf = row(&data, "skills", "pdf");
    assert!(pdf["dir"].is_null(), "no library copy yet: {}", pdf);
    let cell = &pdf["cells"]["claude-code"];
    assert_eq!(cell["state"], json!("copy"), "{}", pdf);
    assert_eq!(
        cell["path"],
        json!(store::native_str(Path::new(&fx.agent_skill("claude-code", "pdf")))),
        "{}",
        pdf
    );
    let external = pdf["external"].as_array().expect("external array");
    assert!(!external.is_empty(), "external row needs external[]: {}", pdf);
    assert!(external[0]["assetId"].is_string(), "{}", external[0]);
    assert_eq!(external[0]["agentId"], json!("claude-code"));
    assert_eq!(external[0]["scope"], json!("global"));

    // 所有类别的行都必须是「name + cells」，单元格的 agent 必须是已知列，
    // state 必须落在词表内；unsupported/error 还要给出 reason。
    let mut rows = 0usize;
    for kind in ROW_KINDS {
        let list = data[kind]
            .as_array()
            .unwrap_or_else(|| panic!("{} must be an array", kind));
        for r in list {
            rows += 1;
            assert!(r["name"].is_string(), "{} row name: {}", kind, r);
            let cells = r["cells"]
                .as_object()
                .unwrap_or_else(|| panic!("{} row cells: {}", kind, r));
            for (agent_id, cell) in cells {
                assert!(
                    agent_ids.contains(agent_id),
                    "{} row has a cell for unknown agent {}: {}",
                    kind,
                    agent_id,
                    r
                );
                let state = cell["state"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{} cell without a state string: {}", kind, cell));
                assert!(
                    CELL_STATES.contains(&state),
                    "{} cell state {:?} is outside the contract vocabulary: {}",
                    kind,
                    state,
                    cell
                );
                if state == "unsupported" || state == "error" {
                    assert!(
                        cell["reason"].is_string(),
                        "{} {} cell needs a reason: {}",
                        kind,
                        state,
                        cell
                    );
                }
            }
        }
    }
    assert!(rows >= 1, "the fixture seeded one row: {}", data);
}

/* ------------------------------------------------------------------ */
/* 2. plan_converge：只读                                                */
/* ------------------------------------------------------------------ */

#[test]
fn plan_converge_is_read_only_and_matches_the_contract() {
    let _lock = common::serial();
    let fx = Fixture::new("shared-plan");
    fx.seed_skill("claude-code", "pdf", "# PDF\n");
    fx.seed_skill("codex", "pdf", "# PDF\n");

    // 能力探测会写 <store>/probe 与 linkcap.txt —— 那不属于 plan_converge，
    // 先预热一次，再把「调用前后逐字节不变」钉在本次动作上。
    fx.warm_capability();
    assert!(
        !Path::new(&fx.library_root()).exists(),
        "the library root must not exist before the plan"
    );
    let tree_before = common::dir_snapshot(&fx.dir.to_string_lossy());
    let audit_before = fx.audit_bytes();

    let plan = fx.plan();
    let plan_again = fx.plan();

    assert_eq!(fx.audit_bytes(), audit_before, "plan writes no audit line");
    assert_eq!(
        common::dir_snapshot(&fx.dir.to_string_lossy()),
        tree_before,
        "plan leaves the fixture byte-identical"
    );
    assert!(
        !Path::new(&fx.library_root()).exists(),
        "plan creates neither the library root nor the ledger"
    );

    assert_eq!(plan["ok"], json!(true), "plan_converge: {}", plan);
    assert!(plan["savedBytes"].is_u64(), "savedBytes: {}", plan);
    assert!(plan["actionable"].is_u64(), "actionable: {}", plan);
    let groups = plan["groups"].as_array().expect("groups array");
    let pdf = groups
        .iter()
        .find(|g| g["name"] == json!("pdf"))
        .unwrap_or_else(|| panic!("no pdf group: {}", plan));
    assert_eq!(pdf["verdict"], json!("identical"), "{}", pdf);
    assert!(pdf["keeper"].is_string(), "identical group needs a keeper: {}", pdf);
    assert!(pdf["label"].is_string(), "{}", pdf);
    assert_eq!(pdf["copies"].as_array().expect("copies").len(), 2);
    assert_eq!(
        pdf["replace"].as_array().expect("replace").len(),
        1,
        "one of the two copies gets replaced: {}",
        pdf
    );
    assert!(pdf["diffs"].as_array().expect("diffs").is_empty());
    assert_eq!(plan_again, plan, "plan is deterministic across calls");
}

/* ------------------------------------------------------------------ */
/* 3. shared_doctor：形状                                                */
/* ------------------------------------------------------------------ */

#[test]
fn shared_doctor_reports_the_contract_shape() {
    let _lock = common::serial();
    let fx = Fixture::new("shared-doctor");
    fx.seed_skill("claude-code", "pdf", "# PDF\n");
    fx.seed_skill("codex", "pdf", "# PDF\n");

    let report = fx.doctor_report();
    assert_eq!(report["ok"], json!(true), "shared_doctor: {}", report);

    let findings = report["findings"].as_array().expect("findings array");
    let counts = report["counts"].as_object().expect("counts object");
    for k in ["total", "critical", "warn", "info"] {
        assert!(counts[k].is_u64(), "counts.{} must be a number: {}", k, report);
    }
    assert_eq!(
        counts["total"].as_u64().unwrap() as usize,
        findings.len(),
        "counts.total must match findings.length: {}",
        report
    );

    // 两份逐字节相同的真实副本：必须报 asset.duplicated（收敛按钮的数据来源）。
    let duplicated = findings
        .iter()
        .filter(|f| f["code"] == json!(doctor::ASSET_DUPLICATED))
        .count();
    assert!(
        duplicated >= 1,
        "two identical copies must be reported as duplicated: {}",
        report
    );

    for f in findings {
        assert!(f["code"].is_string(), "finding code: {}", f);
        let severity = f["severity"].as_str().expect("severity string");
        assert!(
            matches!(severity, "critical" | "warn" | "info"),
            "severity {:?} outside the vocabulary: {}",
            severity,
            f
        );
        assert!(f["path"].is_string(), "finding path: {}", f);
        assert!(f["detail"].is_object(), "finding detail: {}", f);
        assert!(f["fixable"].is_boolean(), "finding fixable: {}", f);
        if f["fixable"] == json!(true) {
            assert!(f["fix"].is_string(), "fixable finding needs a fix: {}", f);
        }
    }
}

/* ------------------------------------------------------------------ */
/* 4. apply_converge → list_library 的单元格变化                         */
/* ------------------------------------------------------------------ */

#[test]
fn applying_converge_turns_the_matrix_cells_on() {
    let _lock = common::serial();
    let fx = Fixture::new("shared-converge");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# PDF\n");
    fx.seed_skill("codex", "pdf", "# PDF\n");

    // 收敛之前：库内没有主副本，两个 agent 各有一份真实目录。
    let before = fx.list();
    let pdf_before = row(&before, "skills", "pdf");
    assert!(pdf_before["dir"].is_null(), "no library copy yet: {}", pdf_before);
    for agent in ["claude-code", "codex"] {
        assert_eq!(
            pdf_before["cells"][agent]["state"],
            json!("copy"),
            "{} before converge: {}",
            agent,
            pdf_before
        );
    }

    let r = fx.apply_converge(json!({ "all": true }));
    assert_eq!(r["ok"], json!(true), "apply_converge: {}", r);
    assert_eq!(r["failed"], json!(0), "{}", r);
    assert!(
        !r["written"].as_array().expect("written array").is_empty(),
        "converge writes at least one path: {}",
        r
    );
    let groups = r["groups"].as_array().expect("groups array");
    let group = groups
        .iter()
        .find(|g| g["name"] == json!("pdf"))
        .unwrap_or_else(|| panic!("no pdf group in apply result: {}", r));
    assert_eq!(group["ok"], json!(true), "{}", group);
    assert_eq!(fx.ledger().entries.len(), 2, "one ledger entry per agent link");

    // 收敛之后：库内主副本出现，两个 agent 位置变成指向它的链接 → on。
    let after = fx.list();
    let pdf = after["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .find(|r| r["name"] == json!("pdf") && !r["dir"].is_null())
        .unwrap_or_else(|| panic!("no converged pdf row: {}", after["skills"]));
    assert_eq!(
        pdf["dir"],
        json!(store::native_str(Path::new(&fx.library_skill("pdf")))),
        "{}",
        pdf
    );
    for agent in ["claude-code", "codex"] {
        let cell = &pdf["cells"][agent];
        assert_eq!(cell["state"], json!("on"), "{} after converge: {}", agent, pdf);
        assert!(!cell["path"].as_str().unwrap_or("").is_empty(), "{}", cell);
        assert!(linkfs::is_link(Path::new(&fx.agent_skill(agent, "pdf"))));
    }
}

/* ------------------------------------------------------------------ */
/* 5. apply_finding_fix(link.dangling) 修好一条悬空链接                 */
/* ------------------------------------------------------------------ */

#[test]
fn apply_finding_fix_relinks_a_dangling_link() {
    let _lock = common::serial();
    let fx = Fixture::new("shared-relink");
    if !links_available(&fx) {
        eprintln!("skipping: this machine cannot create directory links");
        return;
    }
    fx.seed_skill("claude-code", "pdf", "# PDF\n");
    let enabled = fx.enable("claude-code", "pdf");
    assert_eq!(enabled["ok"], json!(true), "enable_asset: {}", enabled);

    let dst = fx.agent_skill("claude-code", "pdf");
    assert!(linkfs::is_link(Path::new(&dst)), "{} is a link", dst);

    // 让这条链接悬空：库内主副本挪到同级另一个名字，台账 src 跟着指过去。
    // agent 侧链接仍指向旧路径（已不存在），而源还在 —— 这正是「能修」的形态。
    let old = fx.library_skill("pdf");
    let new = fx.library_skill("pdf-old");
    std::fs::rename(&old, &new).expect("move the library copy");
    let ledger_path = join(&fx.library_root(), &["links.json"]);
    let mut ledger: Value =
        serde_json::from_str(&read(&ledger_path)).expect("ledger is valid JSON");
    for entry in ledger["entries"].as_array_mut().expect("entries array") {
        entry["src"] = json!(store::native_str(Path::new(&new)));
    }
    write(&ledger_path, &serde_json::to_string_pretty(&ledger).unwrap());

    let report = fx.doctor_report();
    let dangling: Vec<Value> = report["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter(|f| f["code"] == json!(doctor::LINK_DANGLING))
        .cloned()
        .collect();
    assert_eq!(dangling.len(), 1, "one dangling link: {}", report);
    let finding = &dangling[0];
    assert_eq!(
        finding["path"],
        json!(store::native_str(Path::new(&dst))),
        "{}",
        finding
    );
    assert_eq!(finding["fix"], json!(doctor::FIX_RELINK));
    assert_eq!(finding["severity"], json!(doctor::SEV_CRITICAL));

    let r = fx.apply_fix(json!({
        "code": finding["code"],
        "path": finding["path"],
        "fix": finding["fix"],
        "detail": finding["detail"],
    }));
    assert_eq!(r["ok"], json!(true), "apply_finding_fix: {}", r);
    assert!(linkfs::is_link(Path::new(&dst)), "{} is a link again", dst);
    let target = linkfs::read_target(Path::new(&dst)).expect("link target readable");
    assert!(
        linkfs::same_target(&target, Path::new(&new)),
        "relinked to {:?}, expected {}",
        target,
        new
    );

    let after = fx.doctor_report();
    let still_dangling = after["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .filter(|f| f["code"] == json!(doctor::LINK_DANGLING))
        .count();
    assert_eq!(
        still_dangling, 0,
        "the dangling link must be repaired: {}",
        after
    );
}
