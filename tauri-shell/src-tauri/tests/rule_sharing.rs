//! 共享层中规则与记忆共享的端到端验收。
//!
//! 覆盖 `docs/DESIGN.md` §8.10 规则与记忆：两种模式 的四条验收，外加三条安全
//! 不变式（`.mdc` 只能 link、停用只拿走我们的区块、外来编辑不改盘）。
//!
//! 每个夹具都活在 `temp_dir()` 下自己的临时根里，`home` 也是假的：整个套件不碰
//! 真实用户目录，也不依赖本机装没装哪个 agent。

mod common;

use agentkeeper_shell_lib::core::compose;
use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::linkfs;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::store::{self, Store};
use agentkeeper_shell_lib::core::sharing;
use agentkeeper_shell_lib::core::{library, registry, scan};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use common::{cleanup, join, read, temp_root, write};

/// 全局作用域下声明了 `AGENTS.md` 的 agent（`adapters.rs` 的实际声明）。
///
/// 取四个用于验收 1：它们各自的家目录不同，但规则文件名相同 —— 这正是「同源
/// 映射」要收敛的那种情形。
const AGENTS: [&str; 4] = ["codex", "opencode", "amp", "pi"];

/// 每个 agent 的 home（`~` 之后的相对路径）。分隔符统一走 `join`。
fn agent_home(home: &str, agent_id: &str) -> String {
    let parts: &[&str] = match agent_id {
        "codex" => &[".codex"],
        "opencode" => &[".opencode"],
        "amp" => &[".config", "amp"],
        "pi" => &[".pi", "agent"],
        other => panic!("fixture does not know agent {}", other),
    };
    join(home, parts)
}

/// 某个 agent 的全局 `AGENTS.md`。
fn agent_agents_md(home: &str, agent_id: &str) -> String {
    join(&agent_home(home, agent_id), &["AGENTS.md"])
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

    fn library_rule(&self, slug: &str) -> String {
        join(&self.library_root(), &["rules", &format!("{}.md", slug)])
    }

    fn scan(&self) -> Value {
        let projects = scan::read_projects(&self.root);
        let (payload, _timings) = scan::scan_payload(&self.home, &projects);
        payload
    }

    /// 上一次扫描里某个 agent 的某个规则资产（按 `file` 精确定位）。
    fn rule_asset(&self, payload: &Value, file: &str) -> Option<Value> {
        payload["assets"].as_array()?.iter().find(|a| {
            a["kind"] == json!("rule") && a["file"].as_str() == Some(file)
        }).cloned()
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

/// 本机能不能建**文件**链接？（不能时链接类断言退化成「至少 compose 可用」）
fn file_links_available(fx: &Fixture) -> bool {
    let lib = library::Library::resolve(&fx.home, &settings::read(&fx.root));
    lib.capability(&fx.root)
        .file_order
        .iter()
        .any(|m| m.is_link())
}

/* ------------------------------------------------------------------ */
/* 验收 1：AGENTS.md 被 4 个 agent 引用（link），改库内版本 → 4 处同时生效 */
/* ------------------------------------------------------------------ */

#[test]
fn one_agents_md_shared_by_four_agents_takes_effect_everywhere_at_once() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-four-agents");
    if !file_links_available(&fx) {
        eprintln!("skipping: this machine cannot create file links");
        return;
    }
    // 四个 agent 各自有一份**不同**的 AGENTS.md（各自的本地习惯），
    // 第一份进库、其余变成指向库内那一条的链接。
    for (i, agent) in AGENTS.iter().enumerate() {
        write(
            &agent_agents_md(&fx.home, agent),
            &format!("# rules from {}\n\nline {}\n", agent, i),
        );
    }

    // 逐个启用（link 模式）。第一个把内容搬进库，其余链接到它。
    for agent in AGENTS {
        let file = agent_agents_md(&fx.home, agent);
        let r = fx.keeper().enable_asset(&json!({
            "kind": "rule",
            "file": file,
            "agentId": agent,
            "scope": "global",
            "mode": "link",
        }));
        // 已经指向库内（第二个起）时返回 ok + skipped，同样是成功。
        assert_eq!(r["ok"], json!(true), "{} enabling {}: {}", agent, agent, r);
    }

    // 库内主副本一份；四处都是**指向它**的链接。
    let keeper = fx.library_rule("agents");
    assert!(Path::new(&keeper).is_file(), "库内主副本应在 {}", keeper);
    for agent in AGENTS {
        let file = agent_agents_md(&fx.home, agent);
        assert!(
            linkfs::is_link(Path::new(&file)),
            "{} 的 AGENTS.md 应该是指向库内的链接",
            agent
        );
        assert_eq!(
            linkfs::read_target(Path::new(&file)).map(|t| store::native_str(&t)),
            Some(keeper.clone()),
            "{} 的链接应指向库内主副本",
            agent
        );
    }

    // 台账记了四条。
    let ledger = fx.ledger();
    assert_eq!(
        ledger.links_to(&keeper).len(),
        AGENTS.len(),
        "四条链接都要进台账"
    );

    // 改库内版本 → 四处**同时**读到新内容（这就是「一份事实源」）。
    write(&keeper, "# rules from the library\n\nunified line\n");
    for agent in AGENTS {
        let file = agent_agents_md(&fx.home, agent);
        assert_eq!(read(&file), "# rules from the library\n\nunified line\n", "{}", agent);
    }

    // 复用一次：全部再启用一遍是零写入（幂等），台账也不会重复记。
    let again = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": agent_agents_md(&fx.home, "codex"),
        "agentId": "codex",
        "scope": "global",
        "mode": "link",
    }));
    assert_eq!(again["ok"], json!(true), "{}", again);
    assert_eq!(again["written"], json!([]), "{}", again);
    assert_eq!(fx.ledger().links_to(&keeper).len(), AGENTS.len());
}

/* ------------------------------------------------------------------ */
/* 验收 2：compose 模式下在标记**外**追加一行 → 下次同步保留该行        */
/* ------------------------------------------------------------------ */

#[test]
fn a_line_appended_outside_the_block_survives_the_next_sync() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-compose-outside");
    let file = agent_agents_md(&fx.home, "codex");
    write(&file, "# my own header\n\nkeep this paragraph\n");
    // 库里先放一份共享内容。
    write(&fx.library_rule("agents"), "shared rule one\nshared rule two\n");

    let first = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(first["ok"], json!(true), "{}", first);
    assert_eq!(first["mode"], json!("compose"));

    // 用户在**区块之外**追加一行。
    let with_extra = format!("{}my own extra line\n", read(&file));
    write(&file, &with_extra);

    // 再同步一次：区块内容刷新，用户那一行原样保留。
    let second = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(second["ok"], json!(true), "{}", second);

    let out = read(&file);
    assert!(
        out.contains("my own extra line"),
        "块外追加的行必须保留：{:?}",
        out
    );
    assert!(out.starts_with("# my own header\n\nkeep this paragraph\n"), "{:?}", out);
    // 区块还在，且内容仍是库内那一份。
    let block = compose::find_block(&out, "agents").expect("区块");
    assert!(block.body.contains("shared rule one"), "{:?}", block.body);
}

/* ------------------------------------------------------------------ */
/* 验收 3：compose 模式下改标记**内**一行 → rule.foreignEdit，盘上未动   */
/* ------------------------------------------------------------------ */

#[test]
fn a_line_edited_inside_the_block_is_refused_and_the_disk_is_untouched() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-compose-inside");
    let file = agent_agents_md(&fx.home, "codex");
    write(&file, "# mine\n");
    write(&fx.library_rule("agents"), "generated line one\ngenerated line two\n");

    let first = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(first["ok"], json!(true), "{}", first);

    // 用户在**区块之内**改一行。
    let edited = read(&file).replace("generated line one", "I rewrote this by hand");
    write(&file, &edited);

    let r = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["code"], json!("rule.foreignEdit"), "{}", r);
    // 三方 diff 要的两份内容都在 detail 里。
    assert!(r["detail"]["onDisk"].as_str().unwrap().contains("I rewrote this by hand"));
    assert!(r["detail"]["want"].as_str().unwrap().contains("generated line one"));

    // 盘上文件一个字节都没动。
    assert_eq!(read(&file), edited, "闸门只判不写");
}

/* ------------------------------------------------------------------ */
/* 验收 4：CRLF 目标文件 compose 后仍是纯 CRLF（逐字节断言）            */
/* ------------------------------------------------------------------ */

#[test]
fn a_crlf_rule_file_stays_pure_crlf_after_compose() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-compose-crlf");
    let file = agent_agents_md(&fx.home, "codex");
    let original = "# title\r\n\r\nmy words\r\n";
    write(&file, original);
    // 库内主副本用 LF 写：正是最容易混写的那种组合。
    write(&fx.library_rule("agents"), "shared a\nshared b\n");

    let r = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(r["ok"], json!(true), "{}", r);

    let out = read(&file);
    // 前缀逐字节不动。
    assert!(out.starts_with(original), "前缀变了：{:?}", out);
    // 没有任何裸 LF。
    let bytes = out.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            assert!(i > 0 && bytes[i - 1] == b'\r', "位置 {} 出现裸 LF", i);
        }
    }
    // 也没有 `\r\r\n`（重复 CR）。
    assert!(!out.contains("\r\r\n"), "出现重复 CR：{:?}", out);
    // 区块内容被归一成 CRLF。
    assert!(out.contains("shared a\r\nshared b\r\n"), "{:?}", out);
}

/* ------------------------------------------------------------------ */
/* 附加：.mdc 只能 link，compose 被拒绝并给出原因                        */
/* ------------------------------------------------------------------ */

#[test]
fn an_mdc_rule_refuses_compose_instead_of_rewriting_the_file() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-mdc");
    let file = join(&join(&fx.home, &[".cursor", "rules"]), &["a.mdc"]);
    let original = "---\nalwaysApply: true\n---\n\nbody line\n";
    write(&file, original);
    write(&fx.library_rule("a"), "shared\n");

    let r = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "cursor",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["code"], json!("rule.mdcLinkOnly"), "{}", r);
    assert_eq!(read(&file), original, "被拒绝时文件不动");
}

/* ------------------------------------------------------------------ */
/* 附加：停用 compose 只拿走我们的区块，用户内容留着                     */
/* ------------------------------------------------------------------ */

#[test]
fn disabling_a_composed_rule_leaves_the_users_own_text() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-disable-compose");
    let file = agent_agents_md(&fx.home, "codex");
    let original = "# mine\n\ntail line\n";
    write(&file, original);
    write(&fx.library_rule("agents"), "shared\n");

    let on = fx.keeper().enable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
        "mode": "compose",
    }));
    assert_eq!(on["ok"], json!(true), "{}", on);
    assert!(read(&file).contains("agentkeeper:begin"));

    let off = fx.keeper().disable_asset(&json!({
        "kind": "rule",
        "file": file,
        "agentId": "codex",
        "scope": "global",
    }));
    assert_eq!(off["ok"], json!(true), "{}", off);
    assert_eq!(read(&file), original, "停用后回到用户原文");
}

/* ------------------------------------------------------------------ */
/* 附加：扫描结果里的规则资产可以直接拿来共享（assetId 路径）            */
/* ------------------------------------------------------------------ */

#[test]
fn a_scanned_rule_asset_can_be_shared_by_its_id() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-by-asset-id");
    if !file_links_available(&fx) {
        eprintln!("skipping: this machine cannot create file links");
        return;
    }
    let file = agent_agents_md(&fx.home, "codex");
    write(&file, "# team conventions\n\nalways run the tests\n");

    // 扫描一次，拿到这条规则资产。
    let payload = fx.scan();
    let asset = fx
        .rule_asset(&payload, &file)
        .unwrap_or_else(|| panic!("扫描结果里应有 {} 这条规则", file));

    let r = fx.keeper().enable_asset(&json!({
        "assetId": asset["id"],
        "kind": "rule",
        "agentId": "codex",
        "scope": "global",
        "mode": "link",
    }));
    assert_eq!(r["ok"], json!(true), "{}", r);
    assert!(linkfs::is_link(Path::new(&file)));
    assert_eq!(read(&fx.library_rule("agents")), "# team conventions\n\nalways run the tests\n");

    // 拿规则的 id 去当 skill 共享：种类对不上就拒绝，不猜。
    let wrong = fx.keeper().enable_asset(&json!({
        "assetId": asset["id"],
        "kind": "memory",
        "agentId": "codex",
        "scope": "global",
    }));
    assert_eq!(wrong["ok"], json!(false), "{}", wrong);
    assert_eq!(wrong["code"], json!("share.unsupportedKind"), "{}", wrong);
}

/// 已知缺陷：硬链接没有可读的目标（`read_target` 返回 None），
/// `owns_target` 因此认不出「库内主副本的硬链接」，停用一条**我们自己建的**
/// 硬链接会被误报 `share.foreignLink` —— 而 file 链接的降级链恰好在
/// symlink 不可用的机器上产出硬链接（§3.4 的 file 链）。
///
/// 现在归属由台账作证：`owner=library` 且 `src` 在库内的记录就是我们的链接；
/// 删掉那一个目录项只少一个名字，主副本的字节一个都不会动。
#[test]
fn a_managed_hardlink_can_be_disabled_and_unlinked() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-hardlink");
    // 库内主副本
    write(&fx.library_rule("shared-rules"), "# shared rules\n\nbody\n");
    // agent 侧硬链接（同卷，std::fs::hard_link 不需要特权）
    let dst = join(&fx.home, &[".codex", "AGENTS.md"]);
    std::fs::create_dir_all(join(&fx.home, &[".codex"])).expect("agent dir");
    std::fs::hard_link(fx.library_rule("shared-rules"), &dst)
        .expect("hard link within the same temp volume");
    let dst_path = std::path::Path::new(&dst);
    assert!(
        linkfs::link_kind(dst_path) == Some(linkfs::LinkMethod::Hardlink),
        "fixture must be a hardlink"
    );

    // 台账里登记这条硬链接（owner=library、src 在库内）—— enable 在真实链路上
    // 会写这条记录，这里直接落台账以覆盖闸口本身。
    let lib = library::Library::at(fx.library_root());
    let mut ledger = fx.ledger();
    lib.record(
        &mut ledger,
        library::LinkEntry::new(
            "rule",
            "shared-rules",
            &fx.library_rule("shared-rules"),
            &dst,
            linkfs::LinkMethod::Hardlink,
        ),
    );
    lib.save_ledger(&ledger).expect("save ledger");

    // 停用：必须是成功的一次事务（旧实现在这里报 share.foreignLink）。
    let r = fx.keeper().disable_asset(&json!({
        "name": "shared-rules",
        "kind": "rule",
        "agentId": "codex",
        "scope": "global",
        "file": &dst,
    }));
    assert_eq!(r["ok"], json!(true), "{r}");
    // 目录项没了，库内主副本原封不动。
    assert!(!dst_path.exists(), "the hardlink name must be gone");
    assert!(
        std::path::Path::new(&fx.library_rule("shared-rules")).is_file(),
        "the library master copy must stay"
    );
    // 台账里那条也清掉了。
    assert!(fx.ledger().find(&dst).is_none(), "ledger entry must be removed");
}

/// 反向不变式：台账里没有记录的外来硬链接，照旧拒绝（foreignLink）。
#[test]
fn a_foreign_hardlink_is_still_refused() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-hardlink-foreign");
    write(&fx.library_rule("theirs"), "# theirs\n");
    let dst = join(&fx.home, &[".codex", "AGENTS.md"]);
    std::fs::create_dir_all(join(&fx.home, &[".codex"])).expect("agent dir");
    std::fs::hard_link(fx.library_rule("theirs"), &dst).expect("hard link");
    let r = fx.keeper().disable_asset(&json!({
        "name": "theirs",
        "kind": "rule",
        "agentId": "codex",
        "scope": "global",
        "file": &dst,
    }));
    assert_eq!(r["ok"], json!(false), "{r}");
    assert_eq!(r["code"], json!("share.foreignLink"), "{r}");
    assert!(std::path::Path::new(&dst).exists(), "nothing must be deleted");
}

/// 两方 diff 出口：区块被外部改过时，`force: true`（用户对着 diff 显式
/// 选了「以库内为准」）是 foreignEdit 闸门的唯一绕过方式；不带 force 的第二次
/// 尝试仍然被拒——它是人对着 diff 做出的决定，不是调用方图省事的开关。
#[test]
fn force_overrides_a_foreign_edit_and_the_gate_stays_closed_without_it() {
    let _lock = common::serial();
    let fx = Fixture::new("rule-force-diff");
    let agents_md = agent_agents_md(&fx.home, "codex");
    write(&agents_md, "# mine

local edit
");

    // 真实启用路径：compose 把原文件圈成受管区块。
    let k = fx.keeper();
    let r = k.enable_asset(&json!({
        "name": "AGENTS",
        "kind": "rule",
        "agentId": "codex",
        "scope": "global",
        "file": agents_md,
    }));
    assert_eq!(r["ok"], json!(true), "{r}");
    let after_first = read(&agents_md);
    assert!(after_first.contains("agentkeeper:begin"), "{after_first:?}");

    // 用户手改区块内一行 → 外部改动。
    write(&agents_md, after_first.replace("local edit", "hacked edit").as_str());

    let lib = library::Library::at(fx.library_root());
    let target = sharing::RuleTarget {
        base: String::new(),
        rel: None,
        file: Some(agents_md.clone()),
    };

    // 不带 force：拒绝，盘上保持被改的样子。
    let refused = sharing::rule_plan(&lib, &target, None, false);
    assert_eq!(refused.code.as_deref(), Some("rule.foreignEdit"), "{:?}", refused.code);
    assert!(
        refused.detail["onDisk"].as_str().unwrap_or_default().contains("hacked edit"),
        "{:?}",
        refused.detail
    );

    // force：库内版本写回（diff 对话框里「以库内为准」的动作）。
    let forced = sharing::rule_plan(&lib, &target, None, true);
    assert_eq!(forced.code, None, "{:?}", forced.code);
    let tx2 = k.store.transaction(&forced.ops, &json!({ "action": "compose", "force": true }));
    assert_eq!(tx2["ok"], json!(true), "{tx2}");
    // force 只恢复**区块内**的内容（那是共享的部分）；区块外是用户自己的
    // 原文，compose 从来不动它（§6.1）。区块内恢复为库内版本、区块外保留
    // 用户的行，两者同时成立才算对。
    let after = read(&agents_md);
    assert!(
        after.contains("agentkeeper:begin") && after.contains("local edit"),
        "the block must be restored to the library version: {after:?}"
    );
    let block_body = compose::find_block(&after, "agents").expect("block").body;
    assert!(
        !block_body.contains("hacked"),
        "block body must be the library copy: {block_body:?}"
    );
}
