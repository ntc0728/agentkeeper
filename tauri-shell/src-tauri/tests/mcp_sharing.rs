//! MCP 单一事实源（M3）的端到端验收。
//!
//! 覆盖 `docs/SHARED-ASSETS-IMPLEMENTATION.md` §5.5 的四条：
//!
//! 1. Codex（TOML）与 Claude Code（JSON）同时开启同一个 server：TOML 里注释与
//!    小节顺序完全不变，JSON 里兄弟 server 与键序保留；
//! 2. 关闭后两个文件除该条外与开启前逐字节相同；
//! 3. `~/.claude/.mcp.json` 里手写一个库里没有的 server → 漂移零报告、零写入；
//! 4. 同一 server 在两处参数不同 → 一条 `mcp.drift`，且**没有**任何写入。
//!
//! 还额外锁住三条边界：`opencode` 走 `enabled: false`（`DisableStyle::Flag`）而
//! 别的 agent 走删除；YAML 配置直接拒绝而不是尽力而为；物化两遍第二遍零写入。
//!
//! 写入全部经 `core::sharing::{open_ops, close_ops}` 产出的 `store::Op`，再用真实
//! 的 `Store::transaction` 落盘 —— 与产品代码走的是同一条路，而不是另写一份
//! 「测试专用的写入」。

mod common;

use agentkeeper_shell_lib::core::adapters::DisableStyle;
use agentkeeper_shell_lib::core::library::Library;
use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::sharing;
use agentkeeper_shell_lib::core::store::{self, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use common::{cleanup, join, read, temp_root, write};

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

    fn library(&self) -> Library {
        library_of(&self.home, &settings::read(&self.root))
    }

    fn store(&self) -> Store {
        Store::new(&self.root)
    }

    /// 库内规范形态的落盘位置（`<root>/mcp/servers/<slug>.json`）。
    fn record_path(&self, name: &str) -> String {
        store::native_str(&self.library().mcp_record_path(name))
    }

    /// 把一份记录写进库（真实文件，不是夹具内存里的）。
    fn seed_record(&self, name: &str, transport: &str) -> Value {
        let record = record_json(name, transport);
        write(
            &self.record_path(name),
            &format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
        );
        record
    }

    fn claude_config(&self) -> String {
        join(&self.home, &[".claude", ".mcp.json"])
    }

    fn codex_config(&self) -> String {
        join(&self.home, &[".codex", "config.toml"])
    }

    fn opencode_config(&self) -> String {
        join(&self.home, &[".opencode", "opencode.json"])
    }

    fn home_path(&self) -> &str {
        &self.home
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

fn library_of(home: &str, s: &Value) -> Library {
    Library::resolve(home, s)
}

/// 一条 stdio 服务器的库内规范形态。
fn record_json(name: &str, transport: &str) -> Value {
    json!({
        "schema": sharing::MCP_SCHEMA,
        "name": name,
        "transport": transport,
        "command": "uvx",
        "args": ["mcp-server-fetch"],
        "env": {},
        "url": null,
        "headers": {},
        "allOf": { "claude-code": true, "codex": true, "opencode": true }
    })
}

/// 跑一次真实事务（与产品代码同一条路：ops 由 sharing 产出，落盘由 store 负责）。
fn run(store: &Store, ops: Vec<store::Op>, meta: Value) -> Value {
    store.transaction(&ops, &meta)
}

/* ------------------------------------------------------------------ */
/* 验收 1 + 2：TOML 逐字节、JSON 键序、关闭后逐字节还原                */
/* ------------------------------------------------------------------ */

#[test]
fn opening_and_closing_keeps_every_other_byte_in_both_formats() {
    let _lock = common::serial();
    let fx = Fixture::new("mcp-roundtrip");
    fx.seed_record("fetch", "stdio");

    // Codex 的配置：带注释、带小节顺序、带一个兄弟 server。
    const CODEX_BEFORE: &str = concat!(
        "# Codex 配置——这一行注释必须活下来\r\n",
        "model = \"gpt-5\"\r\n",
        "\r\n",
        "[mcp_servers.alpha]\r\n",
        "# alpha 的注释\r\n",
        "command = \"a-cmd\"\r\n",
        "\r\n",
        "[other]\r\n",
        "k = 1\r\n",
    );
    write(&fx.codex_config(), CODEX_BEFORE);

    // Claude Code 的配置：兄弟 server 在前，键序要保留。
    const CLAUDE_BEFORE: &str =
        "{\n  \"mcpServers\": {\n    \"keep\": {\n      \"command\": \"a\",\n      \"args\": [\n        \"x\"\n      ]\n    }\n  },\n  \"zeta\": 1\n}\n";
    write(&fx.claude_config(), CLAUDE_BEFORE);

    let store = fx.store();
    let record = record_json("fetch", "stdio");

    for (agent, file) in [
        ("codex", fx.codex_config()),
        ("claude-code", fx.claude_config()),
    ] {
        let ops = sharing::open_ops(&file, agent, &record)
            .expect("openable")
            .unwrap_or_else(|| panic!("{} should need a write", agent));
        let tx = run(&store, ops, json!({ "kind": "mcp", "agentId": agent }));
        assert_eq!(tx["ok"], json!(true), "{} open -> {}", agent, tx);
    }

    // --- 验收 1：TOML 里注释与小节顺序完全不变 ---------------------------
    let codex_after = read(&fx.codex_config());
    assert!(codex_after.contains("# Codex 配置——这一行注释必须活下来"));
    assert!(codex_after.contains("# alpha 的注释"));
    let alpha_at = codex_after.find("[mcp_servers.alpha]").expect("alpha");
    let other_at = codex_after.find("[other]").expect("other");
    let fetch_at = codex_after.find("[mcp_servers.fetch]").expect("fetch");
    assert!(alpha_at < other_at, "sections keep their order");
    assert!(other_at < fetch_at, "the new section is appended last");
    // 非目标部分逐字节相同：把新增的那一段切掉，剩下的必须与原文一致。
    let head = &codex_after[..fetch_at];
    assert!(
        head.starts_with(CODEX_BEFORE.trim_end_matches(['\r', '\n'])),
        "everything before the new section is byte-identical:\n{}",
        head
    );
    // CRLF 风格没有被混写（原文是 CRLF）。
    assert!(!codex_after.replace("\r\n", "").contains('\n'), "no bare LF appears");

    // --- 验收 1：JSON 里兄弟 server 与键序保留 ---------------------------
    let claude_after = read(&fx.claude_config());
    let doc: Value = serde_json::from_str(&claude_after).expect("valid json");
    let keys: Vec<&String> = doc.as_object().expect("object").keys().collect();
    assert_eq!(keys, vec!["mcpServers", "zeta"], "top-level key order kept");
    let servers: Vec<&String> = doc["mcpServers"].as_object().expect("servers").keys().collect();
    assert_eq!(servers, vec!["keep", "fetch"], "sibling first, new one appended");
    assert_eq!(doc["mcpServers"]["keep"]["args"], json!(["x"]));
    assert_eq!(doc["mcpServers"]["fetch"]["command"], json!("uvx"));

    // --- 验收 2：关闭后两个文件与开启前逐字节相同 -----------------------
    for (agent, file, before) in [
        ("codex", fx.codex_config(), CODEX_BEFORE),
        ("claude-code", fx.claude_config(), CLAUDE_BEFORE),
    ] {
        let style = if agent == "opencode" {
            DisableStyle::Flag
        } else {
            DisableStyle::Remove
        };
        let ops = sharing::close_ops(&file, agent, "fetch", style)
            .expect("closable")
            .unwrap_or_else(|| panic!("{} should need a write", agent));
        let tx = run(&store, ops, json!({ "kind": "mcp", "agentId": agent }));
        assert_eq!(tx["ok"], json!(true), "{} close -> {}", agent, tx);
        assert_eq!(read(&file), before, "{} is byte-identical again", agent);
    }
}

/// 打开再关闭，跨两种格式都不留半对状态；「再打开一次」是幂等的零写入。
#[test]
fn reopening_an_already_open_server_writes_nothing() {
    let fx = Fixture::new("mcp-idempotent");
    fx.seed_record("fetch", "stdio");
    write(&fx.codex_config(), "[mcp_servers.alpha]\ncommand = \"a\"\n");
    let file = fx.codex_config();
    let record = record_json("fetch", "stdio");
    let store = fx.store();

    let ops = sharing::open_ops(&file, "codex", &record).expect("ok").expect("write");
    assert_eq!(run(&store, ops, json!({ "kind": "mcp" }))["ok"], json!(true));
    let after_first = read(&file);

    assert!(
        sharing::open_ops(&file, "codex", &record).expect("ok").is_none(),
        "the second open needs no write"
    );
    assert_eq!(read(&file), after_first);

    // 关闭之后再关一次：也没有写入。
    let ops = sharing::close_ops(&file, "codex", "fetch", DisableStyle::Remove)
        .expect("ok")
        .expect("write");
    assert_eq!(run(&store, ops, json!({ "kind": "mcp" }))["ok"], json!(true));
    assert!(sharing::close_ops(&file, "codex", "fetch", DisableStyle::Remove)
        .expect("ok")
        .is_none());
}

/* ------------------------------------------------------------------ */
/* opencode：靠 enabled 标志关闭，而不是删条目                          */
/* ------------------------------------------------------------------ */

#[test]
fn opencode_closes_by_flag_and_keeps_the_entry() {
    let fx = Fixture::new("mcp-flag");
    fx.seed_record("fetch", "stdio");
    let file = fx.opencode_config();
    write(&file, "{ \"mcpServers\": {} }\n");
    let store = fx.store();
    let record = record_json("fetch", "stdio");

    let ops = sharing::open_ops(&file, "opencode", &record).expect("ok").expect("write");
    assert_eq!(run(&store, ops, json!({ "kind": "mcp" }))["ok"], json!(true));

    // 适配器表说 opencode 用 `flag`。
    let style = agentkeeper_shell_lib::core::adapters::mcp_disable("opencode");
    assert_eq!(style, DisableStyle::Flag);
    let ops = sharing::close_ops(&file, "opencode", "fetch", style)
        .expect("ok")
        .expect("write");
    assert_eq!(run(&store, ops, json!({ "kind": "mcp" }))["ok"], json!(true));

    let doc: Value = serde_json::from_str(&read(&file)).expect("json");
    let entry = &doc["mcpServers"]["fetch"];
    assert!(entry.is_object(), "the entry stays: {}", entry);
    assert_eq!(entry["enabled"], json!(false));
    assert_eq!(entry["command"], json!("uvx"), "the rest of the entry is intact");

    // 其余 16 个 agent 都是「删条目」—— 这条是快照测试的另一半，这里只抽一个。
    assert_eq!(
        agentkeeper_shell_lib::core::adapters::mcp_disable("claude-code"),
        DisableStyle::Remove
    );
}

/* ------------------------------------------------------------------ */
/* 验收 3：库外手写的 server → 零报告、零写入                          */
/* ------------------------------------------------------------------ */

#[test]
fn a_server_that_is_not_in_the_library_is_reported_nowhere_and_touched_never() {
    let fx = Fixture::new("mcp-outside");
    // 库是空的：一条记录都没有。
    const HANDWRITTEN: &str =
        "{\n  \"mcpServers\": {\n    \"handwritten\": {\n      \"command\": \"npx\",\n      \"args\": [\n        \"my-server\"\n      ]\n    }\n  }\n}\n";
    write(&fx.claude_config(), HANDWRITTEN);

    let drifts = sharing::plan_drift(&fx.library(), fx.home_path(), None, "global");
    assert!(drifts.is_empty(), "nothing to report: {:?}", drifts);
    assert_eq!(read(&fx.claude_config()), HANDWRITTEN, "no byte changed");

    // 显式请求物化一个库里没有的名字：给出可读的 code，不写。
    let r = sharing::materialize_mcp(
        &fx.library(),
        fx.home_path(),
        None,
        &json!({ "name": "handwritten", "agentId": "claude-code", "scope": "global", "on": true }),
    );
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["code"], json!("mcp.noRecordInLibrary"));
    assert_eq!(read(&fx.claude_config()), HANDWRITTEN);
}

/* ------------------------------------------------------------------ */
/* 验收 4：同名不同参 → 一条 mcp.drift，且零写入                       */
/* ------------------------------------------------------------------ */

#[test]
fn a_diverged_server_is_reported_once_and_nothing_is_written() {
    let fx = Fixture::new("mcp-drift");
    fx.seed_record("fetch", "stdio");
    // 手写一个同名不同参的 server。
    const CLAUDE: &str = "{\n  \"mcpServers\": {\n    \"fetch\": {\n      \"command\": \"other-cmd\",\n      \"args\": [\n        \"different\"\n      ]\n    }\n  }\n}\n";
    write(&fx.claude_config(), CLAUDE);
    // codex 那边根本没有这条：不算漂移（那是「没启用」）。
    write(&fx.codex_config(), "[mcp_servers.alpha]\ncommand = \"a\"\n");

    let drifts = sharing::plan_drift(&fx.library(), fx.home_path(), None, "global");
    assert_eq!(drifts.len(), 1, "{:?}", drifts);
    assert_eq!(drifts[0].name, "fetch");
    assert_eq!(drifts[0].agent_id, "claude-code");
    assert_eq!(drifts[0].field, "command");
    assert_eq!(drifts[0].library_value, json!("uvx"));
    assert_eq!(drifts[0].agent_value, json!("other-cmd"));

    // 零写入：两份配置与库内记录都逐字节不变。
    assert_eq!(read(&fx.claude_config()), CLAUDE);
    assert_eq!(read(&fx.codex_config()), "[mcp_servers.alpha]\ncommand = \"a\"\n");

    // 报告形态里带着一条可以喂给 `doctor` 的 `mcp.drift` code。
    let report = sharing::drift_report(&fx.library(), fx.home_path(), None, "global");
    assert_eq!(report["count"], json!(1));
    assert_eq!(report["drift"][0]["field"], json!("command"));
}

/* ------------------------------------------------------------------ */
/* 边界：YAML 拒绝、名字非法、缺传输方式                                */
/* ------------------------------------------------------------------ */

#[test]
fn a_yaml_config_is_refused_instead_of_rewritten() {
    let fx = Fixture::new("mcp-yaml");
    fx.seed_record("fetch", "stdio");
    let file = join(&fx.home, &[".continue", "config.yaml"]);
    const YAML: &str = "mcpServers:\n  fetch:\n    command: a\n";
    write(&file, YAML);

    // 显式给文件：`open_ops` / `close_ops` 都拒绝，且**不写**。
    assert_eq!(
        sharing::open_ops(&file, "continue", &record_json("fetch", "stdio"))
            .err()
            .expect("refused"),
        "mcp.shareUnsupported"
    );
    assert_eq!(
        sharing::close_ops(&file, "continue", "fetch", DisableStyle::Remove)
            .err()
            .expect("refused"),
        "mcp.shareUnsupported"
    );
    assert_eq!(read(&file), YAML);

    // 走公共入口时，YAML 不是候选 —— `continue` 还有一份 `mcp.json`，会落到它上面。
    let candidates = sharing::mcp_config_candidates("continue", "global", None, fx.home_path());
    assert!(!candidates.iter().any(|c| c.ends_with(".yaml")), "{:?}", candidates);
}

#[test]
fn a_record_without_a_transport_is_refused_with_a_code() {
    let fx = Fixture::new("mcp-shape");
    write(
        &fx.record_path("broken"),
        "{ \"schema\": 1, \"name\": \"broken\" }\n",
    );
    let file = fx.claude_config();
    write(&file, "{ \"mcpServers\": {} }\n");

    let r = sharing::materialize_mcp(
        &fx.library(),
        fx.home_path(),
        None,
        &json!({ "name": "broken", "agentId": "claude-code", "scope": "global", "on": true }),
    );
    assert_eq!(r["ok"], json!(false), "{}", r);
    assert_eq!(r["code"], json!("mcp.notMaterialisable"));
    assert_eq!(read(&file), "{ \"mcpServers\": {} }\n");

    // 名字不合法（TOML 表头会被它破开）同样在写之前就拦住。
    write(
        &fx.record_path("evil"),
        "{ \"schema\": 1, \"name\": \"bad]name\", \"transport\": \"stdio\", \"command\": \"x\" }\n",
    );
    let r = sharing::materialize_mcp(
        &fx.library(),
        fx.home_path(),
        None,
        &json!({ "name": "bad]name", "agentId": "codex", "scope": "global", "on": true }),
    );
    assert_eq!(r["code"], json!("mcp.notMaterialisable"), "{}", r);
}

/// `materialize_mcp` 的「开」是一条完整路径：选文件、算 op、给出写到的路径。
#[test]
fn materialize_mcp_picks_the_agent_config_file() {
    let fx = Fixture::new("mcp-materialize");
    fx.seed_record("fetch", "stdio");
    let plan = sharing::materialize_mcp(
        &fx.library(),
        fx.home_path(),
        None,
        &json!({ "name": "fetch", "agentId": "codex", "scope": "global", "on": true }),
    );
    assert_eq!(plan["ok"], json!(true), "{}", plan);
    assert!(
        store::native_str(Path::new(plan["file"].as_str().expect("file"))).ends_with("config.toml"),
        "codex writes its TOML config: {}",
        plan
    );
    assert_eq!(plan["style"], json!("remove"));
}
