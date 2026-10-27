//! MCP 工具服务器：既覆盖进程内调用，也覆盖真实子进程。
//!
//! * protocol —— JSON-RPC 契约与工具信封。`core::mcp::dispatch`
//!   是公开的，且不接收 `AppHandle`，所以这些用例针对临时仓库上的
//!   真实 keeper 运行：没有网络，也没有 stdio 循环。
//! * stdio —— 同一个服务器**作为进程**：真实的
//!   stdin/stdout，真实的 JSON-RPC 2.0。在进程内调用 `dispatch` 抓不到
//!   那些只有服务器变成 agent 的子进程之后才会出现的失败：
//!
//!   * 代码树里任何一处 `println!` 都会污染这条流 —— agent 把
//!     stdout 读作按行分隔的 JSON-RPC 通道，冒出一行杂音就把它弄坏了；
//!   * 响应被缓冲却从不 flush（客户端会一直等下去）；
//!   * 通知被回了消息，客户端因此失去同步；
//!   * `AK_STORE_ROOT` / `USERPROFILE` 没传进该进程，服务器于是
//!     悄悄对着错误的仓库干活，人永远看不到 agent 写下的东西；
//!   * EOF 时以非零码退出，agent 的监管进程会把它报成崩溃。
//!
//!   会话顺序刻意与真实客户端一致：initialize →
//!   initialized → tools/list → 读类工具 → 一个写工具 → 协议错误
//!   → 以 EOF 收尾。

mod common;

use agentkeeper_core::keeper::Keeper;
use agentkeeper_core::store::Store;
use agentkeeper_core::{mcp, settings};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use common::{cleanup, join, temp_root, write};

/* ------------------------------------------------------------------ */
/* 协议                                                                */
/* ------------------------------------------------------------------ */
/// 一个临时 home + 一个临时仓库，以及绑定在它们上面的 keeper。
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
        settings::ensure_store(&root).expect("ensure_store");
        write(&join(&home, &[".workbuddy", "MEMORY.md"]), "# memory\n");
        Fixture { dir, home, root }
    }
    fn keeper(&self) -> Keeper {
        Keeper::new(Store::new(&self.root), &self.home)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        cleanup(&self.dir);
    }
}

/// 用一条 JSON-RPC 请求调用 `dispatch`。
fn rpc(keeper: &Keeper, id: u64, method: &str, params: Value) -> Value {
    mcp::dispatch(
        keeper,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    )
}

/// `tools/call`，并把文本载荷解析出来 —— 每个处理器都只回一个装着 JSON
/// 的 text part。
fn call(keeper: &Keeper, tool: &str, args: Value) -> (Value, Value) {
    let resp = rpc(
        keeper,
        1,
        "tools/call",
        json!({ "name": tool, "arguments": args }),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("{}")
        .to_string();
    let body = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({ "raw": text }));
    (resp, body)
}

/* ------------------------------------------------------------------ */
/* 协议                                                                */
/* ------------------------------------------------------------------ */

// `initialize` —— agent 客户端在做任何其它事情之前
// 都要先完成的那次握手。
#[test]
fn initialize_answers_the_protocol_version_and_server_info() {
    let fx = Fixture::new("mcp-init");
    let keeper = fx.keeper();

    let resp = rpc(&keeper, 1, "initialize", json!({}));
    assert_eq!(resp["jsonrpc"], json!("2.0"));
    assert_eq!(resp["id"], json!(1), "the request id is echoed back");
    assert_eq!(resp["result"]["protocolVersion"], json!("2024-11-05"));
    assert_eq!(resp["result"]["serverInfo"]["name"], json!("agentkeeper"));
    assert_eq!(
        resp["result"]["capabilities"]["tools"]["listChanged"],
        json!(false)
    );
    assert!(resp["result"]["serverInfo"]["version"].is_string());
}

#[test]
fn ping_answers_an_empty_result_and_a_notification_is_not_answered() {
    let fx = Fixture::new("mcp-ping");
    let keeper = fx.keeper();

    assert_eq!(rpc(&keeper, 7, "ping", json!({}))["result"], json!({}));
    // `handle_line` 把 `Null` 响应转成「不回消息」，否则一个把通知
    // 流水线式发送出去的客户端就会一直等下去。
    assert!(
        rpc(&keeper, 7, "notifications/initialized", json!({})).is_null(),
        "notifications are never answered"
    );
}

#[test]
fn an_unknown_method_and_an_unknown_tool_fail_cleanly() {
    let fx = Fixture::new("mcp-errors");
    let keeper = fx.keeper();

    let missing = rpc(&keeper, 1, "nope", json!({}));
    assert_eq!(missing["error"]["code"], json!(-32601), "{}", missing);
    assert!(missing["error"]["message"]
        .as_str()
        .unwrap()
        .contains("method not found"));

    let bad_tool = rpc(&keeper, 2, "tools/call", json!({ "name": "no_such_tool" }));
    assert_eq!(bad_tool["error"]["code"], json!(-32602), "{}", bad_tool);
    assert!(bad_tool["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));

    // 格式错误的行要回一个解析错误，而不是把客户端挂住。
    let line = mcp::handle_line("not json at all").expect("a parse error is answered");
    assert_eq!(line["error"]["code"], json!(-32700));
    assert!(
        mcp::handle_line("   ").is_none(),
        "a blank line is not answered"
    );
}

// `tools/list` —— 每个工具都必须带上客户端渲染所需的三个键。
#[test]
fn tools_list_exposes_every_tool_with_a_schema() {
    let fx = Fixture::new("mcp-tools");
    let keeper = fx.keeper();

    let resp = rpc(&keeper, 1, "tools/list", json!({}));
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    assert!(!tools.is_empty(), "the tool list is not empty");
    for t in &tools {
        assert!(t["name"].is_string(), "{:?}", t);
        assert!(t["description"].is_string(), "{:?}", t);
        assert!(t["inputSchema"]["type"] == json!("object"), "{:?}", t);
    }
    for expected in [
        "list_assets",
        "read_asset",
        "scan",
        "baseline_diff",
        "set_baseline",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == json!(expected)),
            "{} is exposed",
            expected
        );
    }
    // 已退役的工具也必须从*对外公布*的列表里消失：agent 从 `tools/list`
    // 里读到一个名字却调不动，那就是一份坏掉的契约。
    for retired in ["list_transactions", "rollback_transaction"] {
        assert!(
            !tools.iter().any(|t| t["name"] == json!(retired)),
            "{} must no longer be advertised: {:?}",
            retired,
            tools
        );
    }
}

// 以 `ok: false` 解析的工具仍必须标记为 `isError` —— 这就是
// 模块头部所记载的那处刻意差异。
#[test]
fn a_failed_tool_result_carries_is_error() {
    let fx = Fixture::new("mcp-iserror");
    let keeper = fx.keeper();

    let (resp, body) = call(
        &keeper,
        "read_asset",
        json!({ "assetId": "nope:does:not:exist" }),
    );
    assert_eq!(body["ok"], json!(false), "{}", body);
    assert_eq!(resp["result"]["isError"], json!(true), "{}", resp);

    // 已退役的工具属于协议错误（-32602），它*同样*是 `isError`：
    // 区别在于它根本不会解析出任何载荷。
    let (gone_resp, _) = call(&keeper, "rollback_transaction", json!({ "txId": "tx-1" }));
    assert_eq!(gone_resp["error"]["code"], json!(-32602), "{}", gone_resp);
}

// `set_baseline` 回的是一个普通对象，而写入路径只会在身后留下
// 审计日志 —— 现在不再有任何事务目录。
#[test]
fn set_baseline_answers_an_object_and_a_write_leaves_no_backup_tree() {
    let fx = Fixture::new("mcp-tx");
    let keeper = fx.keeper();

    let (_resp, body) = call(&keeper, "set_baseline", json!({}));
    assert_eq!(body["ok"], json!(true), "{}", body);
    assert!(
        !std::path::Path::new(&fx.root).join("backups").exists(),
        "no backup tree: {:?}",
        fx.root
    );
}

/* ------------------------------------------------------------------ */
/* 作为进程的服务器                                                     */
/* ------------------------------------------------------------------ */
/// `tools/list` 必须按顺序返回的每一个工具名。改名对已经记住该名字的 agent
/// 是破坏性变更，所以这里把它钉死。
const TOOLS: [&str; 15] = [
    "list_assets",
    "read_asset",
    "scan",
    "stats",
    "health_report",
    "create_skill",
    "update_asset",
    "delete_asset",
    "distribute_asset",
    "baseline_diff",
    "set_baseline",
    "export_bundle",
    "import_bundle",
    "search_registry",
    "install_skill",
];

struct Session {
    child: Child,
    stdin: ChildStdin,
    out: Receiver<String>,
    err: Receiver<String>,
    /// 曾经产出的每一行 stdout，用于「除了 JSON-RPC 别无他物」的检查。
    seen: Vec<String>,
}

impl Session {
    /// 对着一个临时 home 和一个临时仓库启动正式发布的二进制。
    fn spawn(tag: &str) -> (Session, std::path::PathBuf) {
        let dir = temp_root(tag);
        let home = join(&dir.to_string_lossy(), &["home"]);
        let root = join(&dir.to_string_lossy(), &["store"]);
        settings::ensure_store(&root).expect("ensure_store");

        // 给读类工具放点东西可找，这样「空是因为扫描什么都没扫到」就不会
        // 被误认成「工具正常」。
        write(
            &join(&home, &[".workbuddy", "skills", "pdf", "SKILL.md"]),
            "---\nname: pdf\ndescription: Extract text from PDFs\n---\n\n# pdf\n\nUse pdftotext.\n",
        );
        write(
            &join(&home, &[".workbuddy", "MEMORY.md"]),
            "# memory\n\nRemember the store root.\n",
        );

        let mut child = Command::new(env!("CARGO_BIN_EXE_agentkeeper-mcp"))
            .env("AK_STORE_ROOT", &root)
            .env("USERPROFILE", &home)
            .env("HOME", &home)
            .env("AGENTKEEPER_SANDBOX", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agentkeeper-mcp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let (tx, out) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let (etx, err) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if etx.send(line).is_err() {
                    break;
                }
            }
        });

        (
            Session {
                child,
                stdin,
                out,
                err,
                seen: Vec::new(),
            },
            dir,
        )
    }

    fn send_raw(&mut self, raw: &str) {
        self.stdin
            .write_all(format!("{}\n", raw).as_bytes())
            .expect("write request");
        self.stdin.flush().expect("flush request");
    }

    fn send(&mut self, id: u64, method: &str, params: Value) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_raw(&serde_json::to_string(&line).unwrap());
    }

    fn call(&mut self, id: u64, tool: &str, args: Value) {
        self.send(id, "tools/call", json!({ "name": tool, "arguments": args }));
    }

    /// 解析出的下一行 stdout。超时就 panic，以便卡死的服务器能让用例带着
    /// 信息失败，而不是把整个测试套件永远堵住。
    fn next(&mut self) -> Value {
        let line = self
            .out
            .recv_timeout(Duration::from_secs(120))
            .expect("no response on stdout within 120s");
        self.seen.push(line.clone());
        let parsed: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON: {e}\n{line}"));
        assert_eq!(
            parsed["jsonrpc"],
            json!("2.0"),
            "every stdout line must be a JSON-RPC 2.0 message, got: {line}"
        );
        parsed
    }

    /// 断言在 `ms` 毫秒内没有任何东西到达 —— 用在通知和空行之后，它们
    /// 必须保持沉默。
    fn expect_silence(&mut self, ms: u64) {
        match self.out.recv_timeout(Duration::from_millis(ms)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("server closed stdout while a request was pending")
            }
            Ok(line) => panic!("expected silence, got a line: {line}"),
        }
    }

    /// `JSON.parse(result.content[0].text)` —— 按客户端的方式把工具载荷
    /// 拆出来。
    fn payload(resp: &Value) -> Value {
        let raw = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {}", resp));
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("tool text is not JSON: {e}\n{raw}"))
    }
}

#[test]
fn the_server_speaks_json_rpc_over_stdio_and_keeps_stdout_clean() {
    let (mut s, dir) = Session::spawn("mcpstdio");
    let store_root = join(&dir.to_string_lossy(), &["store"]);

    // --- 握手 -----------------------------------------------------------
    s.send(
        1,
        "initialize",
        json!({ "protocolVersion": "2024-11-05", "capabilities": {} }),
    );
    let init = s.next();
    assert_eq!(init["id"], json!(1));
    assert!(
        init["result"]["protocolVersion"].is_string(),
        "initialize: {init}"
    );
    assert_eq!(init["result"]["serverInfo"]["name"], json!("agentkeeper"));
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "initialize: {init}"
    );

    // 通知永远不能被回消息。`initialized` 是每个真实客户端都会发的，
    // 所以这里回一句就会让它们全部失去同步。
    s.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    s.expect_silence(400);

    // 证明上面的沉默确实是沉默、而不是管道卡住：这个 ping 立即得到回应，
    // 而且它就是线上*下一*行。
    s.send(2, "ping", json!({}));
    let pong = s.next();
    assert_eq!(pong["id"], json!(2));
    assert_eq!(pong["result"], json!({}));

    // --- 工具目录 ------------------------------------------------------
    s.send(3, "tools/list", json!({}));
    let list = s.next();
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS.to_vec(), "tool list changed");
    for t in tools {
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{t}"
        );
        assert_eq!(t["inputSchema"]["type"], json!("object"), "{t}");
    }

    // --- 只读工具 ------------------------------------------------------
    s.call(4, "list_assets", json!({}));
    let listed = Session::payload(&s.next());
    let assets = listed["assets"].as_array().expect("assets array");
    assert!(
        listed["total"].as_u64().unwrap() >= 1,
        "scan found nothing: {listed}"
    );
    assert!(!assets.is_empty());
    let first_id = assets[0]["id"].as_str().expect("asset id").to_string();
    assert!(
        assets[0]["kind"].is_string() && assets[0]["name"].is_string(),
        "{:?}",
        assets[0]
    );

    // 过滤必须发生在服务端；agent 不可能把 1000 行全捞回去自己筛。
    s.call(5, "list_assets", json!({ "kind": "skill" }));
    let only_skills = Session::payload(&s.next());
    for a in only_skills["assets"].as_array().unwrap() {
        assert_eq!(a["kind"], json!("skill"));
    }

    s.call(6, "read_asset", json!({ "assetId": first_id }));
    let read = Session::payload(&s.next());
    assert_eq!(read["ok"], json!(true), "read_asset: {read}");
    assert!(read["asset"]["file"].is_string(), "read_asset: {read}");

    // 缺 assetId 属于工具错误，按规范在外层信封上标记出来。
    s.call(7, "read_asset", json!({}));
    let bad_read = s.next();
    assert_eq!(bad_read["result"]["isError"], json!(true), "{bad_read}");
    assert_eq!(Session::payload(&bad_read)["ok"], json!(false));

    s.call(8, "stats", json!({}));
    let stats = Session::payload(&s.next());
    assert!(
        stats["storeRoot"].as_str().unwrap().contains("store"),
        "stats storeRoot: {stats}"
    );
    assert!(stats["total"].as_u64().unwrap() >= 1, "stats: {stats}");
    assert_eq!(stats["agents"].as_array().unwrap().len(), 17);

    s.call(9, "health_report", json!({}));
    let health = Session::payload(&s.next());
    assert!(health["items"].is_array(), "health_report: {health}");
    assert!(health["findings"].is_u64(), "health_report: {health}");

    // --- 一个写工具 -----------------------------------------------------
    // agent 通过 UI 所用的同一层事务写入真实的 agent home：校验、原子写入，
    // 并记入 audit.jsonl —— 身旁不会留下任何备份目录。
    s.call(
        10,
        "create_skill",
        json!({
            "agentId": "workbuddy",
            "name": "stdio-written",
            "description": "created over the MCP stdio channel",
            "body": "# stdio-written

Written by the regression test.
"
        }),
    );
    let created = Session::payload(&s.next());
    assert_eq!(created["ok"], json!(true), "create_skill: {created}");
    assert!(created["txId"].is_string(), "create_skill: {created}");
    let written = created["files"]
        .as_array()
        .and_then(|f| f.first())
        .and_then(|f| f.as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(
        written.contains("stdio-written"),
        "create_skill target: {created}"
    );
    assert!(
        written.ends_with("SKILL.md"),
        "create_skill target: {created}"
    );
    assert!(
        std::path::Path::new(&written).is_file(),
        "the write must land in the real agent tree: {written}"
    );
    assert!(
        written.starts_with(&store_root) == false,
        "the write must not land in the store: {written}"
    );
    // 已入账：`txId` 把工具应答与它的审计行对应起来，而这次写入除了它
    // 什么都不会留下。
    let audit = std::fs::read_to_string(std::path::Path::new(&store_root).join("audit.jsonl"))
        .unwrap_or_default();
    assert!(
        audit.contains(created["txId"].as_str().unwrap_or("")),
        "the write must be journalled: {audit}"
    );
    assert!(
        !std::path::Path::new(&store_root).join("backups").exists(),
        "the write must not create a backup tree"
    );

    // --- 协议错误 ------------------------------------------------------
    s.send(11, "tools/nope", json!({}));
    let unknown_method = s.next();
    assert_eq!(unknown_method["id"], json!(11));
    assert_eq!(unknown_method["error"]["code"], json!(-32601));

    s.call(12, "no_such_tool", json!({}));
    let unknown_tool = s.next();
    assert_eq!(
        unknown_tool["error"]["code"],
        json!(-32602),
        "{unknown_tool}"
    );

    s.send_raw("{ this is not json");
    let parse_error = s.next();
    assert_eq!(parse_error["error"]["code"], json!(-32700));
    assert_eq!(parse_error["id"], Value::Null, "parse errors carry id:null");

    // 没有 `id` 的请求即使方法名是假的也算通知：回了它就会往线上多塞
    // 一条游离的响应。
    s.send_raw(r#"{"jsonrpc":"2.0","method":"tools/nope"}"#);
    s.expect_silence(400);

    // 空行不是消息。
    s.send_raw("");
    s.send_raw("   ");
    s.expect_silence(300);

    // --- stdout 纯净度 -------------------------------------------------
    // 每一行在 `next()` 里都已经按 JSON-RPC 2.0 解析过了。剩下要证明的是
    // 没有*多余*的行：行数必须等于必须被应答的请求数（1–12 加上那条解析错误）。
    assert_eq!(
        s.seen.len(),
        13,
        "stdout carried unexpected lines: {:#?}",
        s.seen
    );
    for line in &s.seen {
        assert!(line.starts_with('{'), "not a compact JSON object: {line}");
        assert!(!line.contains('\n'));
    }

    // 仓库诊断信息属于 stderr，在那儿它污染不了协议流。
    let diag = s
        .err
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_default();
    assert!(
        diag.contains("[agentkeeper-mcp] store:"),
        "startup diagnostic missing from stderr: {diag:?}"
    );

    // --- EOF（输入结束） -----------------------------------------------
    drop(s.stdin);
    let status = s.child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "closing stdin must exit 0");
    assert!(
        s.out.recv_timeout(Duration::from_secs(10)).is_err(),
        "stdout stayed open after exit"
    );

    cleanup(&dir);
}
