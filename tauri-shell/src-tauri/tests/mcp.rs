//! The MCP tool server, both in-process and as a real child process.
//!
//! Merged from `mcp.rs` and `mcp_stdio.rs`.
//!
//! * protocol + sandbox gate (was `mcp.rs`) — `scripts/selftest.js` sections
//!   15b/15c ("MCP apply_pending refuses while sandboxed" / "works with sandbox
//!   off and stays reversible"), section 14 ("readPending returns an array" — via
//!   `list_pending`) and the `src/mcp` protocol contract. `core::mcp::dispatch`
//!   is public and takes no `AppHandle`, so these run against a real keeper over
//!   a temp store: no network, no stdio loop.
//! * stdio (was `mcp_stdio.rs`) — the same server **as a process**: real
//!   stdin/stdout, real JSON-RPC 2.0. Calling `dispatch` in-process cannot catch
//!   the failures that only exist once the server is a child process of an agent:
//!
//!   * a `println!` anywhere in the tree corrupting the stream — an agent reads
//!     stdout as a line-oriented JSON-RPC channel and one stray line breaks it;
//!   * a response that is buffered and never flushed (the client waits forever);
//!   * a notification that gets a reply, which desynchronises the client;
//!   * `AK_STORE_ROOT` / `USERPROFILE` not reaching the process, so the server
//!     silently works on the wrong store and the human never sees the staged
//!     write the agent made;
//!   * a non-zero exit on EOF, which an agent's supervisor reports as a crash.
//!
//!   The session is deliberately ordered like a real client: initialize →
//!   initialized → tools/list → read tools → one write tool (sandboxed) → the
//!   apply gate → protocol errors → EOF.

mod common;

use agentkeeper_shell_lib::core::keeper::Keeper;
use agentkeeper_shell_lib::core::store::{Op, Store};
use agentkeeper_shell_lib::core::{mcp, settings};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use common::{cleanup, exists, join, temp_root, write};

/* ------------------------------------------------------------------ */
/* protocol + sandbox gate (was tests/mcp.rs)                          */
/* ------------------------------------------------------------------ */
/// A temp home + temp store, and a keeper bound to them.
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
        settings::ensure_store(&root, false).expect("ensure_store");
        write(&join(&home, &[".workbuddy", "MEMORY.md"]), "# memory\n");
        Fixture { dir, home, root }
    }
    fn keeper(&self, sandbox: bool) -> Keeper {
        Keeper::new(Store::new(&self.root, sandbox), &self.home)
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

/// `dispatch` with a JSON-RPC request.
fn rpc(keeper: &Keeper, id: u64, method: &str, params: Value) -> Value {
    mcp::dispatch(
        keeper,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    )
}

/// `tools/call`, with the text payload parsed — every handler answers one text
/// part holding JSON.
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

/// Stage one write through the transaction layer, as the UI would.
///
/// The pending queue is keyed by the real path, so two stages need two names.
fn stage(fx: &Fixture, sandbox: bool, name: &str) -> String {
    let store = Store::new(&fx.root, sandbox);
    let target = fx.path(&["project", name]);
    let r = store.transaction(
        &[Op::write(&target, "gate probe\n")],
        &json!({ "kind": "rule", "action": "update" }),
    );
    assert_eq!(r["ok"], json!(true), "{}", r);
    target
}

/* ------------------------------------------------------------------ */
/* protocol                                                            */
/* ------------------------------------------------------------------ */

// `src/mcp/server.js` `initialize` — the handshake an agent client performs
// before anything else.
#[test]
fn initialize_answers_the_protocol_version_and_server_info() {
    let fx = Fixture::new("mcp-init");
    let keeper = fx.keeper(true);

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
    let keeper = fx.keeper(true);

    assert_eq!(rpc(&keeper, 7, "ping", json!({}))["result"], json!({}));
    // `handle_line` turns a `Null` response into "no reply", and a client that
    // pipelined a notification would otherwise wait forever.
    assert!(
        rpc(&keeper, 7, "notifications/initialized", json!({})).is_null(),
        "notifications are never answered"
    );
}

#[test]
fn an_unknown_method_and_an_unknown_tool_fail_cleanly() {
    let fx = Fixture::new("mcp-errors");
    let keeper = fx.keeper(true);

    let missing = rpc(&keeper, 1, "nope", json!({}));
    assert_eq!(missing["error"]["code"], json!(-32601), "{}", missing);
    assert!(missing["error"]["message"].as_str().unwrap().contains("method not found"));

    let bad_tool = rpc(&keeper, 2, "tools/call", json!({ "name": "no_such_tool" }));
    assert_eq!(bad_tool["error"]["code"], json!(-32602), "{}", bad_tool);
    assert!(bad_tool["error"]["message"].as_str().unwrap().contains("unknown tool"));

    // A malformed line answers a parse error instead of hanging the client.
    let line = mcp::handle_line("not json at all").expect("a parse error is answered");
    assert_eq!(line["error"]["code"], json!(-32700));
    assert!(mcp::handle_line("   ").is_none(), "a blank line is not answered");
}

// `tools/list` — every tool needs the three keys a client renders.
#[test]
fn tools_list_exposes_every_tool_with_a_schema() {
    let fx = Fixture::new("mcp-tools");
    let keeper = fx.keeper(true);

    let resp = rpc(&keeper, 1, "tools/list", json!({}));
    let tools = resp["result"]["tools"].as_array().expect("tools array").clone();
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
        "list_pending",
        "apply_pending",
        "discard_pending",
        "list_transactions",
        "rollback_transaction",
        "baseline_diff",
        "set_baseline",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == json!(expected)),
            "{} is exposed",
            expected
        );
    }
}

/* ------------------------------------------------------------------ */
/* sandbox gate                                                        */
/* ------------------------------------------------------------------ */

// selftest.js §15b "MCP apply_pending refuses while sandboxed". The shadow
// layer exists so an agent can *stage*; letting it apply would bypass the
// human-approval step the whole design rests on.
#[test]
fn apply_pending_is_refused_while_the_sandbox_is_on() {
    let fx = Fixture::new("mcp-gate");
    let keeper = fx.keeper(true);

    let target = stage(&fx, true, ".gate-test.md");
    let before = Store::new(&fx.root, true).read_pending().len();
    assert_eq!(before, 1, "the probe was staged");

    let (resp, body) = call(&keeper, "apply_pending", json!({}));
    assert_eq!(body["ok"], json!(false), "{}", body);
    assert!(
        body["error"].as_str().unwrap().to_lowercase().contains("sandbox"),
        "the refusal names the sandbox: {}",
        body
    );
    // `isError` is set on a failed tool result — see the module header: the JS
    // omitted it, so spec-following clients reported failures as successes.
    assert_eq!(resp["result"]["isError"], json!(true), "{}", resp);

    assert_eq!(
        Store::new(&fx.root, true).read_pending().len(),
        before,
        "the refused apply left the queue intact"
    );
    assert!(!exists(&target), "the refused apply created nothing");
}

// selftest.js §15c "MCP apply_pending works with sandbox off" + "rolled back
// cleanly".
#[test]
fn apply_pending_works_with_the_sandbox_off_and_stays_reversible() {
    let fx = Fixture::new("mcp-apply");
    let keeper = fx.keeper(false);

    // Stage with the sandbox ON (that is what creates a pending queue), then
    // apply with it OFF — the same two steps the UI performs.
    let first = stage(&fx, true, "first.md");
    let second = stage(&fx, true, "second.md");
    assert!(!exists(&first) && !exists(&second), "both writes are staged, not applied");

    let (_resp, body) = call(&keeper, "apply_pending", json!({}));
    assert_eq!(body["ok"], json!(true), "{}", body);
    assert!(body["applied"].as_i64().unwrap_or(0) >= 1, "{}", body);
    assert!(exists(&first) && exists(&second), "both reached the real tree");
    let tx_id = body["txId"].as_str().expect("a txId to roll back").to_string();

    let undone = Store::new(&fx.root, false).rollback(&tx_id);
    assert_eq!(undone["ok"], json!(true), "{}", undone);
    assert!(!exists(&first), "the rollback removed the applied file");
    assert!(!exists(&second), "and the second one");
}

// selftest.js §13 "pending queue emptied" via the MCP tool.
#[test]
fn discard_pending_empties_the_queue_and_the_shadow_tree() {
    let fx = Fixture::new("mcp-discard");
    let keeper = fx.keeper(true);

    let target = stage(&fx, true, "one.md");
    stage(&fx, true, "two.md");
    assert_eq!(Store::new(&fx.root, true).read_pending().len(), 2);

    let (_resp, body) = call(&keeper, "discard_pending", json!({}));
    assert_eq!(body["ok"], json!(true), "{}", body);
    assert_eq!(
        Store::new(&fx.root, true).read_pending().len(),
        0,
        "the queue is empty"
    );
    assert!(!exists(&target), "nothing reached the real tree");
}

// selftest.js §14 "readPending returns an array" — `list_pending` reports the
// array itself, not `{changes:[]}` (which used to report zero forever).
#[test]
fn list_pending_reports_the_count_and_the_changes() {
    let fx = Fixture::new("mcp-list-pending");
    let keeper = fx.keeper(true);

    let (_resp, empty) = call(&keeper, "list_pending", json!({}));
    assert_eq!(empty["count"], json!(0));
    assert_eq!(empty["sandbox"], json!(true));
    assert_eq!(empty["note"], json!("nothing staged"));
    assert_eq!(empty["changes"].as_array().unwrap().len(), 0);

    let target = stage(&fx, true, ".gate-test.md");
    let (_resp, body) = call(&keeper, "list_pending", json!({}));
    assert_eq!(body["count"], json!(1), "{}", body);
    assert_eq!(
        body["note"],
        json!("staged changes await human approval in the AgentKeeper UI")
    );
    let change = &body["changes"][0];
    // The MCP shape renames `real` to `target` and drops `shadow`.
    for key in ["op", "target", "bytes", "at"] {
        assert!(change.get(key).is_some(), "change is missing {}: {}", key, change);
    }
    assert_eq!(change["target"], json!(target));
}

// A tool that resolves with `ok: false` must still be marked `isError` — the
// deliberate difference from `src/mcp/server.js` documented in the module
// header.
#[test]
fn a_failed_tool_result_carries_is_error() {
    let fx = Fixture::new("mcp-iserror");
    let keeper = fx.keeper(true);

    let (resp, body) = call(&keeper, "read_asset", json!({ "assetId": "nope:does:not:exist" }));
    assert_eq!(body["ok"], json!(false), "{}", body);
    assert_eq!(resp["result"]["isError"], json!(true), "{}", resp);

    let (ok_resp, ok_body) = call(&keeper, "rollback_transaction", json!({ "txId": "tx-1" }));
    assert_eq!(ok_body["ok"], json!(false), "{}", ok_body);
    assert_eq!(ok_resp["result"]["isError"], json!(true), "{}", ok_resp);
}

// `list_transactions` answers a plain array (the MCP contract in selftest §14
// and the `ak:transactions` shape).
#[test]
fn list_transactions_answers_a_plain_array() {
    let fx = Fixture::new("mcp-tx");
    let keeper = fx.keeper(false);

    write(
        &fx.path(&["backups", "tx-0000000001-aa", "tx.json"]),
        r#"{"txId":"tx-0000000001-aa","kind":"rule"}"#,
    );
    let (_resp, body) = call(&keeper, "list_transactions", json!({ "limit": 20 }));
    assert!(body.is_array(), "{}", body);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["hasBackup"], json!(true));
}

/* ------------------------------------------------------------------ */
/* the server as a process (was tests/mcp_stdio.rs)                    */
/* ------------------------------------------------------------------ */
/// Every tool name `tools/list` must return, in order. A rename is a breaking
/// change for an agent that already learned the old name, so it is pinned.
const TOOLS: [&str; 20] = [
    "list_assets",
    "read_asset",
    "scan",
    "stats",
    "health_report",
    "create_skill",
    "update_asset",
    "delete_asset",
    "distribute_asset",
    "list_pending",
    "apply_pending",
    "discard_pending",
    "list_transactions",
    "rollback_transaction",
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
    /// Every stdout line ever produced, for the "nothing but JSON-RPC" check.
    seen: Vec<String>,
}

impl Session {
    /// Spawn the shipped binary against a temp home and a temp store.
    fn spawn(tag: &str) -> (Session, std::path::PathBuf) {
        let dir = temp_root(tag);
        let home = join(&dir.to_string_lossy(), &["home"]);
        let root = join(&dir.to_string_lossy(), &["store"]);
        settings::ensure_store(&root, true).expect("ensure_store");

        // Something for the read tools to find, so "empty because the scan
        // found nothing" cannot be mistaken for "the tool works".
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
        self.send(
            id,
            "tools/call",
            json!({ "name": tool, "arguments": args }),
        );
    }

    /// The next stdout line, parsed. Panics on timeout so a hung server fails
    /// the test with a message instead of blocking the suite forever.
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

    /// Assert nothing arrives within `ms` — used after notifications and
    /// blank lines, which must stay silent.
    fn expect_silence(&mut self, ms: u64) {
        match self.out.recv_timeout(Duration::from_millis(ms)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("server closed stdout while a request was pending")
            }
            Ok(line) => panic!("expected silence, got a line: {line}"),
        }
    }

    /// `JSON.parse(result.content[0].text)` — the tool payload, unwrapped the
    /// way a client does.
    fn payload(resp: &Value) -> Value {
        let raw = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {}", resp));
        serde_json::from_str(raw).unwrap_or_else(|e| panic!("tool text is not JSON: {e}\n{raw}"))
    }
}

/// Is there a file under `dir` whose contents mention `needle`? The shadow
/// tree encodes the real path into its own layout, and that encoding is an
/// implementation detail — the content is what the assertion is about.
fn tree_contains(dir: &str, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_contains(&path.to_string_lossy(), needle) {
                return true;
            }
        } else if std::fs::read_to_string(&path)
            .map(|body| body.contains(needle))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

#[test]
fn the_server_speaks_json_rpc_over_stdio_and_keeps_stdout_clean() {
    let (mut s, dir) = Session::spawn("mcpstdio");
    let store_root = join(&dir.to_string_lossy(), &["store"]);

    // --- handshake -----------------------------------------------------
    s.send(1, "initialize", json!({ "protocolVersion": "2024-11-05", "capabilities": {} }));
    let init = s.next();
    assert_eq!(init["id"], json!(1));
    assert!(init["result"]["protocolVersion"].is_string(), "initialize: {init}");
    assert_eq!(init["result"]["serverInfo"]["name"], json!("agentkeeper"));
    assert!(init["result"]["capabilities"]["tools"].is_object(), "initialize: {init}");

    // A notification must never be answered. `initialized` is the one every
    // real client sends, so a reply here would desynchronise all of them.
    s.send_raw(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    s.expect_silence(400);

    // Proof the silence above was silence and not a stalled pipe: this ping
    // answers immediately, and it is the *next* line on the wire.
    s.send(2, "ping", json!({}));
    let pong = s.next();
    assert_eq!(pong["id"], json!(2));
    assert_eq!(pong["result"], json!({}));

    // --- tool catalogue ------------------------------------------------
    s.send(3, "tools/list", json!({}));
    let list = s.next();
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS.to_vec(), "tool list changed");
    for t in tools {
        assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()), "{t}");
        assert_eq!(t["inputSchema"]["type"], json!("object"), "{t}");
    }

    // --- read-only tools ----------------------------------------------
    s.call(4, "list_assets", json!({}));
    let listed = Session::payload(&s.next());
    let assets = listed["assets"].as_array().expect("assets array");
    assert!(listed["total"].as_u64().unwrap() >= 1, "scan found nothing: {listed}");
    assert!(!assets.is_empty());
    let first_id = assets[0]["id"].as_str().expect("asset id").to_string();
    assert!(assets[0]["kind"].is_string() && assets[0]["name"].is_string(), "{:?}", assets[0]);

    // Filtering has to happen server-side; an agent cannot fetch 1000 rows
    // and sift them itself.
    s.call(5, "list_assets", json!({ "kind": "skill" }));
    let only_skills = Session::payload(&s.next());
    for a in only_skills["assets"].as_array().unwrap() {
        assert_eq!(a["kind"], json!("skill"));
    }

    s.call(6, "read_asset", json!({ "assetId": first_id }));
    let read = Session::payload(&s.next());
    assert_eq!(read["ok"], json!(true), "read_asset: {read}");
    assert!(read["asset"]["file"].is_string(), "read_asset: {read}");

    // Missing assetId is a tool error, flagged on the wrapper per the spec.
    s.call(7, "read_asset", json!({}));
    let bad_read = s.next();
    assert_eq!(bad_read["result"]["isError"], json!(true), "{bad_read}");
    assert_eq!(Session::payload(&bad_read)["ok"], json!(false));

    s.call(8, "stats", json!({}));
    let stats = Session::payload(&s.next());
    assert_eq!(stats["sandbox"], json!(true));
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

    // --- a write tool, with the sandbox on -----------------------------
    // The whole security model is that an agent can stage but not commit.
    s.call(
        10,
        "create_skill",
        json!({
            "agentId": "workbuddy",
            "name": "stdio-written",
            "description": "created over the MCP stdio channel",
            "body": "# stdio-written\n\nWritten by the regression test.\n"
        }),
    );
    let created = Session::payload(&s.next());
    assert_eq!(created["ok"], json!(true), "create_skill: {created}");
    assert_eq!(created["staged"], json!(true), "create_skill: {created}");
    assert!(created["txId"].is_string(), "create_skill: {created}");
    let shadow = created["files"]
        .as_array()
        .and_then(|f| f.first())
        .and_then(|f| f.as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(shadow.contains("stdio-written"), "create_skill target: {created}");
    assert!(shadow.ends_with("SKILL.md"), "create_skill target: {created}");
    // Staged means exactly that: the real agent directory is untouched. A
    // staged write that already landed is the failure mode the sandbox exists
    // to prevent, so this asserts on the *absence*.
    assert!(
        !std::path::Path::new(&shadow).exists(),
        "sandbox leaked the write into the real path: {shadow}"
    );
    assert!(
        tree_contains(&join(&store_root, &["sandbox"]), "stdio-written"),
        "no shadow copy under the store sandbox for {shadow}"
    );

    s.call(11, "list_pending", json!({}));
    let pending = Session::payload(&s.next());
    let changes = pending["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 1, "list_pending: {pending}");
    assert_eq!(pending["sandbox"], json!(true));
    assert_eq!(changes[0]["target"], json!(shadow), "list_pending: {pending}");

    // apply_pending must refuse while sandboxed — this is the one call that
    // would let an agent bypass human approval.
    s.call(12, "apply_pending", json!({}));
    let applied = s.next();
    assert_eq!(applied["result"]["isError"], json!(true), "apply_pending: {applied}");
    let gate = Session::payload(&applied);
    assert_eq!(gate["ok"], json!(false));
    assert!(
        gate["error"].as_str().unwrap_or("").to_lowercase().contains("sandbox"),
        "apply_pending refusal should name the sandbox: {gate}"
    );

    // --- protocol errors ----------------------------------------------
    s.send(13, "tools/nope", json!({}));
    let unknown_method = s.next();
    assert_eq!(unknown_method["id"], json!(13));
    assert_eq!(unknown_method["error"]["code"], json!(-32601));

    s.call(14, "no_such_tool", json!({}));
    let unknown_tool = s.next();
    assert_eq!(unknown_tool["error"]["code"], json!(-32602), "{unknown_tool}");

    s.send_raw("{ this is not json");
    let parse_error = s.next();
    assert_eq!(parse_error["error"]["code"], json!(-32700));
    assert_eq!(parse_error["id"], Value::Null, "parse errors carry id:null");

    // A request with no `id` is a notification even when the method is bogus:
    // answering it would put a stray response on the wire.
    s.send_raw(r#"{"jsonrpc":"2.0","method":"tools/nope"}"#);
    s.expect_silence(400);

    // Blank lines are not messages.
    s.send_raw("");
    s.send_raw("   ");
    s.expect_silence(300);

    // --- stdout purity -------------------------------------------------
    // Every line already parsed as JSON-RPC 2.0 inside `next()`. What is left
    // to prove is that there were no *extra* lines: the count must equal the
    // number of requests that must be answered (1,2,3,4,5,6,7,8,9,10,11,12,13,
    // 14 plus the parse error).
    assert_eq!(
        s.seen.len(),
        15,
        "stdout carried unexpected lines: {:#?}",
        s.seen
    );
    for line in &s.seen {
        assert!(line.starts_with('{'), "not a compact JSON object: {line}");
        assert!(!line.contains('\n'));
    }

    // The store diagnostic belongs on stderr, where it cannot corrupt the
    // protocol stream.
    let diag = s.err.recv_timeout(Duration::from_secs(10)).unwrap_or_default();
    assert!(
        diag.contains("[agentkeeper-mcp] store:"),
        "startup diagnostic missing from stderr: {diag:?}"
    );

    // --- EOF -----------------------------------------------------------
    drop(s.stdin);
    let status = s.child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "closing stdin must exit 0");
    assert!(
        s.out.recv_timeout(Duration::from_secs(10)).is_err(),
        "stdout stayed open after exit"
    );

    cleanup(&dir);
}
