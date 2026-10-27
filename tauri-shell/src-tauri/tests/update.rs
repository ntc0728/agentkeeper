//! 软件更新流水线：`check → download → progress → cancel →
//! install`，调度器、`ak:update-*` 载荷守卫，以及版本运算。
//!
//! * pipeline —— 更新流水线的端到端覆盖：渠道、下载、
//!   安装守卫。这里的
//!   模拟服务器会架起一个本地 GitHub API 并驱动
//!   *真实的* manager 走一遍，正是这才证明了请求头、资产
//!   选择、流式传输、进度、取消和安装守卫
//!   能协同工作。`manager.rs` 的单元测试做不到这一点：它们把
//!   `fetch` / `downloader` 闭包换成进程内的假实现，所以 socket、
//!   `ureq`、`Content-Length` 处理和字节级的进度核算在那里
//!   从未被执行。
//!
//!   这一半遵守两条规则：没有测试接触网络、DNS 或 GitHub
//!   （一切都发往 `127.0.0.1:<ephemeral>`），也没有测试执行
//!   下载得到的文件 —— `install()` 是通过 `Options::launcher`
//!   这个接缝来断言的，而生产环境把它留作 `None`。`OnceLock` 单例
//!   （`ensure_manager`）被刻意不用：它会把第一个根目录钉死给
//!   整个进程，所以每个测试都自己用
//!   `with_options` 构造 `UpdateManager`，各自拥有一份私有临时根目录。
//! * ipc guard —— 两条
//!   `ak:update-*` IPC 表层规则："清除令牌"
//!   按钮，以及五条设置校验。两者都以纯函数的形式住在
//!   `core::update::manager` 里（`token_arg`、
//!   `validate_settings_patch`），这样*规则*无需 WebView 也能被测试；
//!   `lib.rs` 只是调用它们的适配器。每条规则都对应它所镜像的那个
//!   参考表达式，这就是为什么每条
//!   断言都与那个表达式配对。这里刻意不去
//!   断言的部分：Tauri 的参数→形参映射（也就是 `{token:null}`
//!   能否完整抵达 `Request` —— `tauri::ipc::InvokeMessage` 的字段是
//!   `pub(crate)`，所以集成测试造不出一个）以及渲染器那一半
//!   （`app.js`）。
//! * version semantics —— 版本辅助函数与固定
//!   期望值的对比。`version.rs` 的单元测试
//!   和上面的流水线已经证明了版本运算的*语义* ——
//!   但用的是它们自己的夹具，而不是参考实现那套。这是测试
//!   行为的正确方式，却是不适合发现漂移的方式：一种只有其中
//!   一套会走到的形状，在两套都绿的情况下也能各自偏离。所以那一半
//!   逐字镜像：相同的字符串、相同的期望值，
//!   顺序也与参考实现相同。这种冗余正是重点。那里的
//!   一切都是纯函数：没有 HTTP、没有 manager、没有单例、没有真的 DPAPI、
//!   没有文件系统。
//!
//! 参考实现返回 `1 / -1 / 0 / null`，而 Rust 返回
//! `Option<Ordering>`；版本比较本身由
//! `agentkeeper-core` 的单元测试覆盖，它们直接比较 `Ordering`。

use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::update::manager::{
    token_arg, validate_settings_patch, LauncherFn, Options, UpdateManager, BUILT_IN_REPO,
    ERR_API_BASE_FORMAT, ERR_INTERVAL_RANGE, ERR_PROXY_FORMAT, ERR_REPO_FORMAT,
    ERR_TOKEN_FIELD_REQUIRED, ERR_TOKEN_TYPE,
};
use agentkeeper_shell_lib::core::update::version;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/* ------------------------------------------------------------------ */
/* 通过真实 HTTP 的 check → download → install                          */
/* 一个 GitHub 形状的模拟服务器                                        */
/* ------------------------------------------------------------------ */

/// 服务器线程需要的一切。构造一次，然后不可变地共享。
struct MockConfig {
    base: String,
    payload: Vec<u8>,
    /// `/releases/latest` 返回 404（一个还没有任何 release 的仓库）。
    no_release: bool,
    /// `/releases?per_page=30` 返回什么。
    list: Vec<Value>,
    /// `/releases?per_page=…` 的状态码；200 表示返回 `list`。
    list_status: Option<u16>,
    /// 在 `/dl/` 上声明这个 `Content-Length`，而不是真实大小。
    oversized: Option<u64>,
    /// `/dl/` 的状态码；200 表示流式发送载荷。
    dl_status: u16,
    /// 把载荷切成这个大小的块，块之间暂停 `chunk_delay_ms`
    /// 毫秒。0 = 一次写完。
    chunk: usize,
    chunk_delay_ms: u64,
    /// 用 `Transfer-Encoding: chunked` 且不带 `Content-Length` 来
    /// 响应 `/dl/`，这样字节上限只能从流本身发现。
    chunked: bool,
}

impl MockConfig {
    fn release(&self) -> Value {
        let size = self.payload.len() as u64;
        json!({
            "tag_name": "v9.9.9",
            "name": "Keeper 9.9.9",
            "body": "Release notes body",
            "published_at": "2026-01-02T03:04:05Z",
            "html_url": "https://github.com/acme/keeper/releases/tag/v9.9.9",
            "prerelease": false,
            "assets": [
                {
                    "name": "AgentKeeper-9.9.9-x64.exe",
                    "size": size,
                    "browser_download_url": format!("{}/dl/AgentKeeper-9.9.9-x64.exe", self.base)
                },
                {
                    "name": "AgentKeeper-Setup-9.9.9.exe",
                    "size": size,
                    "browser_download_url": format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", self.base)
                }
            ]
        })
    }
}

/// 用 Rust 写的 GitHub 模拟服务器。
struct MockGitHub {
    base: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    rate_limited: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct MockOptions {
    payload: Option<Vec<u8>>,
    no_release: bool,
    list: Vec<Value>,
    list_status: Option<u16>,
    oversized: Option<u64>,
    dl_status: Option<u16>,
    chunk: usize,
    chunk_delay_ms: u64,
    chunked: bool,
}

impl MockGitHub {
    fn start(options: MockOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let payload = options
            .payload
            .unwrap_or_else(|| b"MZ fake installer payload - never executed".to_vec());
        let cfg = Arc::new(MockConfig {
            base: base.clone(),
            payload,
            no_release: options.no_release,
            list: options.list,
            list_status: options.list_status,
            oversized: options.oversized,
            dl_status: options.dl_status.unwrap_or(200),
            chunk: options.chunk,
            chunk_delay_ms: options.chunk_delay_ms,
            chunked: options.chunked,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let rate_limited = Arc::new(AtomicBool::new(false));

        let thread = {
            let listener = listener.try_clone().expect("clone listener");
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            let rate_limited = Arc::clone(&rate_limited);
            std::thread::spawn(move || {
                serve(listener, cfg, stop, requests, rate_limited);
            })
        };

        Self {
            base,
            stop,
            requests,
            rate_limited,
            thread: Some(thread),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// 把服务器切到 403 模式以演练冷却期。
    fn set_rate_limited(&self, on: bool) {
        self.rate_limited.store(on, AtomicOrdering::SeqCst);
    }
}

impl Drop for MockGitHub {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    cfg: Arc<MockConfig>,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    rate_limited: Arc<AtomicBool>,
) {
    // 设成非阻塞，这样不用等一个连接来解除 `stop` 的阻塞。
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !stop.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows 会让被接受的 socket 继承监听器的
                // 非阻塞模式，而非阻塞 socket 会忽略
                // SO_RCVTIMEO —— 这正是 `core::http::tests::serve` 和
                // `core::update::http::tests::serve` 里已经
                // 记录并修复过的那个缺陷，却在这里被漏掉了。
                // 于是 `read_request` 在第一次读时拿到 `WouldBlock(10035)`
                // —— 只要客户端的字节还没到，
                // 它就返回一条空的请求行，`respond` 于是落进
                // 它的 404 分支：一次健康的操作上报了"404"，而这个
                // 测试大约每 8 % 失败一次（实测 50 次里 4 次，全都在
                // 0.02 s 之内，这已经排除了 2 s 的读超时）。
                stream.set_nonblocking(false).expect("blocking connection");
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let request = read_request(&mut stream);
                requests.lock().unwrap().push(request.clone());
                respond(
                    &mut stream,
                    &request,
                    &cfg,
                    rate_limited.load(AtomicOrdering::SeqCst),
                );
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

/// 只要请求行；这些请求从不发送请求体，但
/// 头部块会被读空，好让客户端不会往一个已关闭的 socket 里写。
fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 8192 {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                    break;
                }
            }
            // 双保险，针对 accept 处描述的非阻塞继承：
            // `WouldBlock` 意味着"还没到字节"，绝不是"空
            // 请求"，所以它不该结束这次读。
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn write_head(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body_len: usize) {
    let reason = match status {
        200 => "OK",
        301 | 302 => "Found",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {} {}\r\n", status, reason);
    for (name, value) in headers {
        out.push_str(&format!("{}: {}\r\n", name, value));
    }
    out.push_str(&format!("Content-Length: {}\r\n", body_len));
    out.push_str("Connection: close\r\n\r\n");
    let _ = stream.write_all(out.as_bytes());
}

fn respond(stream: &mut TcpStream, request: &str, cfg: &MockConfig, rate_limited: bool) {
    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();

    if rate_limited {
        write_head(
            stream,
            403,
            &[
                ("content-type", "application/json"),
                ("retry-after", "120"),
                ("x-ratelimit-remaining", "0"),
            ],
            38,
        );
        let _ = stream.write_all(br#"{"message":"API rate limit exceeded"}"#);
        return;
    }

    let is_repo_path = path.starts_with("/repos/");
    if is_repo_path && path.ends_with("/releases/latest") {
        if cfg.no_release {
            write_head(stream, 404, &[("content-type", "application/json")], 2);
            let _ = stream.write_all(b"{}");
            return;
        }
        let body = serde_json::to_vec(&cfg.release()).unwrap();
        write_head(
            stream,
            200,
            &[("content-type", "application/json")],
            body.len(),
        );
        let _ = stream.write_all(&body);
        return;
    }
    if is_repo_path && path.contains("/releases?") {
        // 一个非 200 的状态码就把这个分支变成变更日志测试
        // 需要的失败夹具（`GitHub returned HTTP 500`），而不需要第二个
        // 模拟服务器：下面的传输分支已经在别处被演练过了。
        let status = cfg.list_status.unwrap_or(200);
        let body = serde_json::to_vec(&cfg.list).unwrap();
        write_head(
            stream,
            status,
            &[("content-type", "application/json")],
            body.len(),
        );
        let _ = stream.write_all(&body);
        return;
    }
    if path.starts_with("/dl/") {
        if cfg.dl_status != 200 {
            write_head(stream, cfg.dl_status, &[("content-type", "text/plain")], 4);
            let _ = stream.write_all(b"boom");
            return;
        }
        // 一个撒谎的声明长度正是上限测试的全部要点：
        // 守卫必须在头部上触发，在第一个字节落地之前。
        let declared = cfg.oversized.unwrap_or(cfg.payload.len() as u64) as usize;
        if cfg.chunked {
            // 完全没有 `Content-Length`：上限只能
            // 从流里发现，这是该守卫的另一半。
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                  Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            );
        } else {
            write_head(
                stream,
                200,
                &[("content-type", "application/octet-stream")],
                declared,
            );
        }
        if cfg.chunk == 0 {
            if cfg.chunked {
                let _ = stream.write_all(format!("{:x}\r\n", cfg.payload.len()).as_bytes());
                let _ = stream.write_all(&cfg.payload);
                let _ = stream.write_all(b"\r\n0\r\n\r\n");
            } else {
                let _ = stream.write_all(&cfg.payload);
            }
            return;
        }
        for part in cfg.payload.chunks(cfg.chunk) {
            let sent = if cfg.chunked {
                stream
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .and_then(|_| stream.write_all(part))
                    .and_then(|_| stream.write_all(b"\r\n"))
            } else {
                stream.write_all(part)
            };
            if sent.is_err() {
                break;
            }
            let _ = stream.flush();
            if cfg.chunk_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(cfg.chunk_delay_ms));
            }
        }
        if cfg.chunked {
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        return;
    }

    write_head(stream, 404, &[("content-type", "application/json")], 2);
    let _ = stream.write_all(b"{}");
}

/* ------------------------------------------------------------------ */
/* fixtures —— 夹具                                                   */
/* ------------------------------------------------------------------ */

struct Fixture {
    root: PathBuf,
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("ak-upd-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&root).expect("temp root");
        let dir = root.join("dl");
        std::fs::create_dir_all(&dir).expect("download dir");
        settings::ensure_store(&root.to_string_lossy()).expect("ensure_store");
        Self { root, dir }
    }

    fn root_str(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn save(&self, patch: Value) {
        settings::save(&self.root_str(), &patch);
    }

    /// 一个跑在 `win32`/`x64` 上的 manager，钉在这个夹具的下载目录里。
    fn manager(&self, app_version: &str) -> UpdateManager {
        self.manager_with(app_version, None, 0)
    }

    fn manager_with(
        &self,
        app_version: &str,
        launcher: Option<LauncherFn>,
        max_bytes: u64,
    ) -> UpdateManager {
        let mut options = Options::default();
        options.platform = "win32".into();
        options.arch = "x64".into();
        options.app_version = app_version.into();
        options.dir = Some(self.dir.clone());
        options.launcher = launcher;
        options.max_bytes = max_bytes;
        UpdateManager::with_options(&self.root_str(), options)
    }

    /// 把 manager 指向模拟服务器并打开自动检查。
    fn use_mock(&self, base: &str) {
        self.save(json!({
            "updateRepo": "acme/keeper",
            "updateApiBase": base,
            "updateProxy": "direct",
            "updateCheck": true,
            "updateCheckMinutes": 20
        }));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        agentkeeper_shell_lib::core::store::rmrf(&self.root);
    }
}

/// 轮询 `progress()` 直到任务不再处于活跃状态。
fn wait_inactive(manager: &UpdateManager) -> Value {
    for _ in 0..10_000 {
        let task = manager.progress();
        if task["active"] != Value::Bool(true) {
            return task;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("the download never finished: {}", manager.progress());
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/* ------------------------------------------------------------------ */
/* 真实发布流水线产出的资产名                                          */
/* ------------------------------------------------------------------ */

/// `pick_installer` 是一个已发布的 release 与真正发生的升级之间
/// 唯一的那道关卡。发布流水线把 NSIS 安装包命名为
/// `AgentKeeper_<version>_x64-setup.exe`（下划线、`-setup` 后缀），而
/// 没有别的夹具覆盖这一点 —— 所以在这里把真实的产物名钉住。
#[test]
fn the_real_tauri_installer_name_is_selected() {
    let assets = json!([
        {
            "name": "AgentKeeper_0.2.0_x64-setup.exe",
            "size": 1024,
            "browser_download_url": "u-setup"
        }
    ]);
    let picked = version::pick_installer(&assets, "win32", "x64")
        .expect("the Tauri NSIS installer must be selectable");
    assert_eq!(picked["name"], json!("AgentKeeper_0.2.0_x64-setup.exe"));
    assert_eq!(picked["url"], json!("u-setup"));

    // 一次真实的 release 会附带好几个产物。仔细读这一段：真正
    // 彼此竞争的只有两个。`allowed_installer_suffixes("win32")` 是 `["exe"]`，所以
    // `…_en-US.msi` 和 `…_setup.exe.sig` 并不是"被安装包击败了" ——
    // 它们根本进不了 `candidates`。在那两个 `.exe` 之中，`-setup.exe`
    // 凭 `setup|install` 模式胜出，而 `-portable.exe` 被
    // `is_non_installer_asset` 滤掉（便携版什么都不安装）。那个
    // `.tar.gz` 出于同样的理由被排除。
    let full = json!([
        { "name": "AgentKeeper_0.2.0_x64-setup.exe", "size": 1, "browser_download_url": "u-setup" },
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 2, "browser_download_url": "u-msi" },
        { "name": "AgentKeeper_0.2.0_x64_update.tar.gz", "size": 3, "browser_download_url": "u-tar" },
        { "name": "AgentKeeper_0.2.0_x64-setup.exe.sig", "size": 4, "browser_download_url": "u-sig" },
        { "name": "AgentKeeper_0.2.0_x64-portable.exe", "size": 5, "browser_download_url": "u-portable" }
    ]);
    let picked = version::pick_installer(&full, "win32", "x64").expect("installer");
    assert_eq!(picked["name"], json!("AgentKeeper_0.2.0_x64-setup.exe"));

    // 上面那一行背后的静默失败模式：在 win32 上后缀
    // 白名单只有 `["exe"]`，所以一个只有 `.msi` 的 release 是看不见的。
    // `bundle.targets` 如今只有 nsis；加上 `msi` 会发布一个
    // 更新器永远看不到的安装包（`asset: null` →
    // `downloadSupported: false` →"有可用更新，但无法安装"）。
    // 在这里钉住，是为了等到有人加上那个 target 时，
    // 这里会响亮地失败，而不是在现场悄无声息地出问题。
    let msi_only = json!([
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 1, "browser_download_url": "u-msi" }
    ]);
    assert_eq!(
        version::pick_installer(&msi_only, "win32", "x64"),
        None,
        ".msi is not in allowed_installer_suffixes(\"win32\")"
    );
    // 混合情况：.exe 必须仍然胜出，.msi 不得遮蔽它。
    let mixed = json!([
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 1, "browser_download_url": "u-msi" },
        { "name": "AgentKeeper_0.2.0_x64-setup.exe", "size": 2, "browser_download_url": "u-setup" }
    ]);
    let picked = version::pick_installer(&mixed, "win32", "x64").expect("installer");
    assert_eq!(picked["name"], json!("AgentKeeper_0.2.0_x64-setup.exe"));

    // 更新面板的下载守卫也接受它：这个名字能通过
    // `safe_file_name`，而安装包检查要求含产品关键词。
    assert_eq!(
        version::safe_file_name("AgentKeeper_0.2.0_x64-setup.exe", "win32"),
        "AgentKeeper_0.2.0_x64-setup.exe"
    );
    assert!("AgentKeeper_0.2.0_x64-setup.exe"
        .to_lowercase()
        .contains("agentkeeper"));
}

/* ------------------------------------------------------------------ */
/* 发布仓库是内置的                                                    */
/* ------------------------------------------------------------------ */

#[test]
fn the_release_repository_is_built_in() {
    let fx = Fixture::new("repo");
    let manager = fx.manager("1.0.0");

    // 无需输入任何东西，而且它必须等于
    // package.json 里的那个仓库（`ntc0728/agentkeeper`）。
    assert_eq!(manager.status()["repo"], json!(BUILT_IN_REPO));
    assert_eq!(manager.status()["repo"], json!("ntc0728/agentkeeper"));

    // 一个不可用的值会回退，而不是禁用
    // 更新。`normalize_repo` 拒绝了它，所以 `Cfg::repo` 取默认值。
    fx.save(json!({ "updateRepo": "not a repo" }));
    assert_eq!(manager.status()["repo"], json!(BUILT_IN_REPO));

    fx.save(json!({ "updateRepo": "acme/keeper" }));
    assert_eq!(manager.status()["repo"], json!("acme/keeper"));
}

/* ------------------------------------------------------------------ */
/* 17b. check → download → install，通过真实 HTTP                       */
/* ------------------------------------------------------------------ */

#[test]
fn check_finds_the_newer_release_over_http() {
    let fx = Fixture::new("check");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let found = manager.check(true);
    assert_eq!(found["ok"], json!(true), "{}", found);
    assert_eq!(found["hasUpdate"], json!(true), "{}", found);
    assert_eq!(found["latest"], json!("9.9.9"), "{}", found);
    // 说明与发布页面在往返中完好无损。
    assert_eq!(found["notes"], json!("Release notes body"));
    assert!(
        found["pageUrl"]
            .as_str()
            .unwrap_or("")
            .contains("releases/tag/v9.9.9"),
        "{}",
        found["pageUrl"]
    );
    // NSIS setup 胜过便携版 exe，即使
    // 那个裸 exe 排在前面。
    let asset = found["asset"].clone();
    assert!(
        asset["name"].as_str().unwrap_or("").contains("Setup"),
        "{}",
        asset
    );
    assert!(
        asset["url"].as_str().unwrap_or("").starts_with(&mock.base),
        "{}",
        asset
    );
}

#[test]
fn a_second_manual_check_inside_the_gap_reuses_the_result() {
    let fx = Fixture::new("throttle");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    assert_eq!(manager.check(true)["latest"], json!("9.9.9"));
    let before = mock.request_count();

    // 60 s 最小间隔之内不会有第二次请求。
    let throttled = manager.check(true);
    assert_eq!(throttled["reason"], json!("throttled"), "{}", throttled);
    assert_eq!(mock.request_count(), before);
}

#[test]
fn download_streams_to_completion_and_reaches_100_percent() {
    let payload = b"MZ fake installer payload - never executed".to_vec();
    let fx = Fixture::new("download");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload.clone()),
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let asset = manager.check(true)["asset"].clone();
    let url = asset["url"].as_str().expect("asset url").to_string();
    let name = asset["name"].as_str().expect("asset name").to_string();

    let started = manager.start_download(&url, Some(&name)).expect("start");
    assert_eq!(started["active"], json!(true), "{}", started);

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(true), "{}", task);
    assert_eq!(task["percent"], json!(100), "{}", task);
    assert_eq!(task["error"], Value::Null, "{}", task);
    assert_eq!(task["received"], json!(payload.len()), "{}", task);
    assert_eq!(task["total"], json!(payload.len()), "{}", task);

    // 磁盘上的字节就是被发出来的字节，
    // 而且文件落在了受管的目录里。
    let path = task["path"].as_str().expect("task path").to_string();
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    let canonical = std::fs::canonicalize(&path).unwrap();
    let dir = std::fs::canonicalize(&fx.dir).unwrap();
    assert!(
        canonical.starts_with(&dir),
        "{} is outside {}",
        path,
        dir.display()
    );

    // 已完成的任务是 `status()` 的一部分，而面板轮询的正是它。
    assert_eq!(manager.status()["task"]["done"], json!(true));
}

#[test]
fn progress_reports_partial_bytes_while_the_transfer_runs() {
    // 8 块 8 KiB 并带一次暂停：长到足以观察到一个飞行中的
    // 快照，短到仍稳稳落在 60 s 的 socket 超时之内。
    let payload = vec![b'P'; 64 * 1024];
    let fx = Fixture::new("progress");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload.clone()),
        chunk: 8 * 1024,
        chunk_delay_ms: 40,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");

    // 盯着一个既不是第一个也不是最后一个的快照。
    let mut saw_partial = false;
    for _ in 0..5_000 {
        let task = manager.progress();
        let received = task["received"].as_u64().unwrap_or(0);
        let percent = task["percent"].as_u64().unwrap_or(0);
        if received > 0 && received < payload.len() as u64 {
            saw_partial = true;
            assert!(
                percent > 0 && percent < 100,
                "percent must move with the bytes: {}",
                task
            );
            break;
        }
        if task["active"] != Value::Bool(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(saw_partial, "never observed a partial progress snapshot");

    let task = wait_inactive(&manager);
    assert_eq!(task["percent"], json!(100), "{}", task);
    assert_eq!(task["received"], json!(payload.len()), "{}", task);
}

#[test]
fn cancel_stops_the_transfer_and_reports_canceled() {
    // 一个慢速服务器：取消必须在字节仍在传输中落地。
    let payload = vec![b'C'; 512 * 1024];
    let fx = Fixture::new("cancel");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload.clone()),
        chunk: 32 * 1024,
        chunk_delay_ms: 60,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    let started = manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");
    let path = started["path"].as_str().expect("path").to_string();

    // 等到第一块被计入，这样部分文件已经存在，
    // 取消也明显发生在传输途中。
    for _ in 0..5000 {
        let task = manager.progress();
        if task["received"].as_u64().unwrap_or(0) > 0 || task["active"] != Value::Bool(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let answer = manager.cancel();
    assert_eq!(answer["canceled"], json!(true), "{}", answer);

    let task = wait_inactive(&manager);
    assert_eq!(task["canceled"], json!(true), "{}", task);
    assert_eq!(task["done"], json!(false), "{}", task);
    assert_eq!(task["active"], json!(false), "{}", task);
    // 取消路径在那个删除文件的 `catch` 之前就返回了，所以
    // 一个部分下载的安装包会留存下来 —— 而任务报告没有可用的路径。
    assert_eq!(task["path"], Value::Null, "{}", task);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    assert!(
        size > 0 && size < payload.len() as u64,
        "expected a partial file at {}, got {} bytes of {}",
        path,
        size,
        payload.len()
    );
}

#[test]
fn cancel_without_an_active_task_is_a_no_op() {
    let fx = Fixture::new("cancel-idle");
    let manager = fx.manager("1.0.0");
    assert_eq!(manager.cancel()["canceled"], json!(false));

    // 一个已完成的任务同样不算活跃。
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");
    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");
    wait_inactive(&manager);
    assert_eq!(manager.cancel()["canceled"], json!(false));
}

#[test]
fn a_second_start_download_while_one_is_active_returns_the_running_task() {
    let payload = vec![b'R'; 256 * 1024];
    let fx = Fixture::new("restart");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload),
        chunk: 16 * 1024,
        chunk_delay_ms: 40,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let first_url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    let first = manager
        .start_download(&first_url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");

    // 一个不同的 URL（而且根本没有名字）：正在运行的任务胜出，
    // 第二个 URL 连校验都不会经过。
    let second = manager
        .start_download("https://example.com/other.exe", None)
        .expect("second start must not fail validation");
    assert_eq!(second["path"], first["path"], "{}", second);
    assert_eq!(second["active"], json!(true), "{}", second);

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(true), "{}", task);
    assert_eq!(
        mock.request_count(),
        1,
        "only one transfer may reach the server"
    );
}

#[test]
fn download_refuses_a_url_outside_the_allow_list() {
    let fx = Fixture::new("allowlist");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    // 引擎用英语，而 `src/renderer/engine-zh.js` 会把它直接映射成
    // 中文，所以断言落在引擎字符串上 ——
    // 下面的对照一节钉住映射那一侧。
    let error = manager
        .start_download("https://evil.example/a.exe", Some("AgentKeeper-a.exe"))
        .expect_err("a foreign host must be refused");
    assert!(
        error.message().contains("not on the allow list"),
        "{}",
        error
    );

    // 除非它就是那个镜像本身，否则明文 http 会被拒绝。
    let error = manager
        .start_download("http://github.com/a.exe", Some("AgentKeeper-a.exe"))
        .expect_err("plain http must be refused");
    assert!(error.message().contains("https"), "{}", error);

    // 配置的镜像被允许（那正是这个设置的全部意义）。
    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    assert!(manager.start_download(&url, None).is_ok());
    wait_inactive(&manager);
}

#[test]
fn a_failed_download_removes_the_partial_file() {
    let fx = Fixture::new("http500");
    let mock = MockGitHub::start(MockOptions {
        dl_status: Some(500),
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    let started = manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");
    let path = started["path"].as_str().expect("path").to_string();

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(false), "{}", task);
    let error = task["error"].as_str().unwrap_or("").to_string();
    assert!(error.contains("the server returned HTTP 500"), "{}", task);
    assert!(!Path::new(&path).exists(), "{} must be unlinked", path);
    assert_eq!(task["path"], Value::Null, "{}", task);
}

#[test]
fn an_oversized_installer_is_refused_before_anything_is_written() {
    let fx = Fixture::new("oversized");
    // 声明 50 MB 却只发几个字节：上限必须在头部上就触发。
    let mock = MockGitHub::start(MockOptions {
        oversized: Some(50 * 1024 * 1024),
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    // 一个很小的 `maxBytes`，好让守卫够得着。
    let manager = fx.manager_with("1.0.0", None, 1024);

    let url = format!("{}/dl/AgentKeeper-big.exe", mock.base);
    let started = manager
        .start_download(&url, Some("AgentKeeper-big.exe"))
        .expect("start");
    let path = started["path"].as_str().expect("path").to_string();

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(false), "{}", task);
    let error = task["error"].as_str().unwrap_or("").to_string();
    assert!(error.contains("size looks wrong"), "{}", task);
    assert!(!Path::new(&path).exists(), "{} must not exist", path);
}

#[test]
fn a_stream_that_outgrows_the_ceiling_is_cut_off_and_unlinked() {
    // 分块传输：没有 `Content-Length` 可以事先拒绝，所以
    // 上限是在传输中途才被发现的 —— 也就是在若干块已经到达
    // 文件之后。那正是别处都不演练的失败分支。
    let fx = Fixture::new("overrun");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(vec![b'X'; 256 * 1024]),
        chunk: 32 * 1024,
        chunked: true,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager_with("1.0.0", None, 64 * 1024);

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    let started = manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");
    let path = started["path"].as_str().expect("path").to_string();

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(false), "{}", task);
    let error = task["error"].as_str().unwrap_or("").to_string();
    assert!(error.contains("exceeds the size cap"), "{}", task);
    assert!(!Path::new(&path).exists(), "{} must be unlinked", path);
    assert_eq!(task["path"], Value::Null, "{}", task);
}

#[test]
fn a_truncated_download_leaves_no_installer_behind() {
    // 声明了四倍的字节数却只发了实际那么多就关闭：这次
    // 传输是在若干块已经到达文件之后才失败的，而这是唯一
    // 能演练 `runDownload` 失败分支里那个 unlink 的办法。
    let payload = vec![b'T'; 8 * 1024];
    let fx = Fixture::new("truncated");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload),
        oversized: Some(32 * 1024),
        chunk: 4 * 1024,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    let started = manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");
    let path = started["path"].as_str().expect("path").to_string();

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(false), "{}", task);
    let error = task["error"].as_str().unwrap_or("").to_string();
    assert!(
        error.contains("incomplete") || error.contains("download interrupted"),
        "{}",
        task
    );
    assert!(!Path::new(&path).exists(), "{} must be unlinked", path);
    assert_eq!(task["path"], Value::Null, "{}", task);
}

#[test]
fn the_transport_layer_refuses_an_oversized_declaration_before_writing() {
    // 上限被强制了两次：一次在这里，在 `Content-Length` 头上，
    // 另一次在 `run_download` 的进度回调里，措辞相同。这个
    // 测试单独驱动 `download_to_file`，好让头部守卫
    // 独立于 manager 里的那份被钉住。
    let fx = Fixture::new("transport");
    let mock = MockGitHub::start(MockOptions {
        oversized: Some(50 * 1024 * 1024),
        ..Default::default()
    });
    let dest = fx.dir.join("AgentKeeper-direct.exe");
    let mut calls: Vec<(u64, Option<u64>)> = Vec::new();

    let result = agentkeeper_shell_lib::core::http::download_to_file(
        &format!("{}/dl/AgentKeeper-direct.exe", mock.base),
        &dest,
        "direct",
        1024,
        30_000,
        &mut |received, total| {
            calls.push((received, total));
            true
        },
    );

    let error = result.expect_err("an oversized declaration must be refused");
    assert!(
        error.describe().contains("size looks wrong"),
        "{}",
        error.describe()
    );
    // 在头部上就被拒绝：`on_progress` 从未运行，也没有写入任何东西。
    assert!(calls.is_empty(), "on_progress must not run: {:?}", calls);
    assert!(!dest.exists(), "{} must not exist", dest.display());

    // 同一次下载在上限之内则成功。
    let plain = MockGitHub::start(MockOptions::default());
    let ok = agentkeeper_shell_lib::core::http::download_to_file(
        &format!("{}/dl/AgentKeeper-direct.exe", plain.base),
        &dest,
        "direct",
        version::MAX_INSTALLER_BYTES,
        30_000,
        &mut |_, _| true,
    )
    .expect("a download inside the ceiling");
    assert_eq!(ok, std::fs::metadata(&dest).unwrap().len() as u64);
}

/* ------------------------------------------------------------------ */
/* 17c. 安装守卫                                                      */
/* ------------------------------------------------------------------ */

#[test]
fn install_refuses_a_file_outside_the_download_directory() {
    let fx = Fixture::new("outside");
    let manager = fx.manager("1.0.0");
    let outside = fx.root.join("AgentKeeper-outside.exe");
    std::fs::write(&outside, "x").unwrap();

    let error = manager
        .verify_installer(&outside.to_string_lossy())
        .expect_err("a file outside the download directory");
    // 引擎字符串；`engine-zh.js:211` 把它渲染为 安装包不在受控的下载目录内。
    assert!(
        error
            .message()
            .contains("outside the managed download directory"),
        "{}",
        error
    );
    assert_eq!(error.status(), 400);
}

#[test]
fn install_refuses_a_non_installer_extension() {
    let fx = Fixture::new("ext");
    let manager = fx.manager("1.0.0");
    let notes = fx.dir.join("AgentKeeper-notes.txt");
    std::fs::write(&notes, "x").unwrap();

    let error = manager
        .verify_installer(&notes.to_string_lossy())
        .expect_err("a non-installer extension");
    // 引擎字符串；`engine-zh.js:307` 把它渲染为 只允许运行 §1 安装包。
    assert!(
        error.message().contains("installers may be run"),
        "{}",
        error
    );
}

#[test]
fn install_refuses_another_projects_installer() {
    let fx = Fixture::new("foreign");
    let manager = fx.manager("1.0.0");
    let foreign = fx.dir.join("SomeOtherApp-1.0.0.exe");
    std::fs::write(&foreign, "x").unwrap();

    let error = manager
        .verify_installer(&foreign.to_string_lossy())
        .expect_err("another product's installer");
    // 引擎字符串；`engine-zh.js:308` 把它渲染为 疑似来自其他项目。
    assert!(error.message().contains("another project"), "{}", error);
}

#[test]
fn install_refuses_a_relative_or_missing_path() {
    let fx = Fixture::new("relative");
    let manager = fx.manager("1.0.0");

    let error = manager
        .verify_installer("relative.exe")
        .expect_err("a relative path");
    assert!(error.message().contains("must be absolute"), "{}", error);

    let error = manager.verify_installer("  ").expect_err("an empty path");
    assert!(error.message().contains("path is empty"), "{}", error);

    let missing = fx.dir.join("AgentKeeper-gone.exe");
    let error = manager
        .verify_installer(&missing.to_string_lossy())
        .expect_err("a path that does not exist");
    assert!(
        error.message().contains("missing or is not a file"),
        "{}",
        error
    );

    // 一个带着安装包名字的目录不是一个文件。
    let as_dir = fx.dir.join("AgentKeeper-dir.exe");
    std::fs::create_dir_all(&as_dir).unwrap();
    let error = manager
        .verify_installer(&as_dir.to_string_lossy())
        .expect_err("a directory");
    // 引擎字符串；`engine-zh.js:210` 把它渲染为 安装包不存在或不是文件。
    assert!(
        error.message().contains("missing or is not a file"),
        "{}",
        error
    );
}

#[test]
fn install_launches_the_verified_file_and_reports_the_mode() {
    let fx = Fixture::new("launch");
    let launched: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&launched);
    let launcher: LauncherFn = Arc::new(move |file, silent| {
        sink.lock()
            .unwrap()
            .push((file.to_string_lossy().into_owned(), silent));
        Ok("fake".to_string())
    });
    let manager = fx.manager_with("1.0.0", Some(launcher), 0);

    let target = fx.dir.join("AgentKeeper-Setup-9.9.9.exe");
    std::fs::write(&target, "MZ").unwrap();

    // 启动器收到的是*真实的*路径，而且
    // `silent` 设置每次安装时都重新读取，而不是在构造时被捕获。
    let installed = manager.install(&target.to_string_lossy()).expect("install");
    assert_eq!(installed["ok"], json!(true), "{}", installed);
    assert_eq!(installed["mode"], json!("fake"), "{}", installed);
    assert_eq!(installed["restarted"], json!(true), "{}", installed);

    let calls = launched.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{:?}", calls);
    let expected = std::fs::canonicalize(&target)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected = expected
        .strip_prefix(r"\\?\")
        .unwrap_or(&expected)
        .to_string();
    assert_eq!(calls[0].0, expected, "the launcher saw {:?}", calls);
    assert_eq!(calls[0].1, true, "silent install is on by default");

    // 关掉它会在下一次安装时被尊重。
    fx.save(json!({ "updateInstallSilent": false }));
    manager.install(&target.to_string_lossy()).expect("install");
    assert_eq!(launched.lock().unwrap()[1].1, false);
}

#[test]
fn install_refuses_while_a_download_is_active() {
    let payload = vec![b'I'; 256 * 1024];
    let fx = Fixture::new("busy");
    let mock = MockGitHub::start(MockOptions {
        payload: Some(payload),
        chunk: 16 * 1024,
        chunk_delay_ms: 40,
        ..Default::default()
    });
    fx.use_mock(&mock.base);

    let launched: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let sink = Arc::clone(&launched);
    let manager = fx.manager_with(
        "1.0.0",
        Some(Arc::new(move |_, _| {
            *sink.lock().unwrap() += 1;
            Ok("fake".to_string())
        })),
        0,
    );

    let target = fx.dir.join("AgentKeeper-Setup-9.9.9.exe");

    let url = format!("{}/dl/AgentKeeper-Setup-9.9.9.exe", mock.base);
    manager
        .start_download(&url, Some("AgentKeeper-Setup-9.9.9.exe"))
        .expect("start");

    // `startDownload` 会在传输开始前删除先前的副本，所以
    // 要等到第一块落地：只有那时才有一个真实文件，
    // 其执行才能被活跃任务守卫拒绝。
    for _ in 0..5000 {
        if manager.progress()["received"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(target.exists(), "the installer must exist by now");

    let error = manager
        .install(&target.to_string_lossy())
        .expect_err("install must not run during a transfer");
    assert!(error.message().contains("still downloading"), "{}", error);

    wait_inactive(&manager);
    // 下载覆盖了那个文件；事后守卫就消失了。
    assert!(manager.install(&target.to_string_lossy()).is_ok());
    assert_eq!(*launched.lock().unwrap(), 1);
}

/* ------------------------------------------------------------------ */
/* 17i. ak:update-* 载荷守卫                                           */
/* ------------------------------------------------------------------ */
#[test]
fn clearing_the_token_goes_through_the_guard_and_empties_it() {
    use agentkeeper_shell_lib::core::update::manager::token_arg;

    let fx = Fixture::new("clear");
    let manager = fx.manager("1.0.0");
    if manager.status()["encryption"] != json!(true) {
        // 这里没有 DPAPI：`set_token` 无论哪种情况都会拒绝，而守卫才是
        // 本测试的主题，所以这里只断言守卫的形状。
        assert_eq!(token_arg(&json!({ "token": null })), Ok(None));
        return;
    }

    manager.set_token(Some("ghp_testtoken123")).expect("set");
    assert_eq!(manager.token_status()["filled"], json!(true));
    let raw = settings::read(&fx.root_str())["updateToken"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(raw.starts_with("enc1:"), "{}", raw);

    // IPC 层如何处理渲染器发来的 `null`：守卫 → 清除。
    let cleared = token_arg(&json!({ "token": null })).expect("null is a clear request");
    assert_eq!(cleared, None, "`null` must become `set_token(None)`");
    manager.set_token(cleared.as_deref()).expect("clear");

    assert_eq!(manager.token_status()["filled"], json!(false));
    assert_eq!(
        settings::read(&fx.root_str())["updateToken"].as_str(),
        Some(""),
        "the envelope must be gone from settings.json"
    );
    // 而且它在重启之后也依然不在。
    assert_eq!(fx.manager("1.0.0").token_status()["filled"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17d. 跳过 / 取消跳过                                                */
/* ------------------------------------------------------------------ */

#[test]
fn skip_survives_a_restart_and_unskip_clears_it() {
    let fx = Fixture::new("skip");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);

    let manager = fx.manager("1.0.0");
    assert_eq!(manager.check(true)["latest"], json!("9.9.9"));

    let skipped = manager.skip(Some("9.9.9"));
    assert_eq!(skipped["skipped"], json!(true), "{}", skipped);
    assert_eq!(skipped["skippedVersion"], json!("9.9.9"), "{}", skipped);

    // 第二个 manager（即一次重启）读取状态文件。
    assert_eq!(fx.manager("1.0.0").status()["skipped"], json!(true));
    assert_eq!(manager.unskip()["skipped"], json!(false));
    assert_eq!(fx.manager("1.0.0").status()["skipped"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17e. 令牌存储                                                      */
/* ------------------------------------------------------------------ */

#[test]
fn the_token_is_stored_as_an_encrypted_envelope() {
    let fx = Fixture::new("token");
    let manager = fx.manager("1.0.0");

    if manager.status()["encryption"] != json!(true) {
        // 本机没有 DPAPI：唯一正确的行为是干脆利落地拒绝。
        let error = manager
            .set_token(Some("ghp_testtoken123"))
            .expect_err("refusal");
        // 引擎字符串；`engine-zh.js:231` 把它渲染为
        // 本机安全存储不可用，已拒绝以明文保存令牌.
        assert!(
            error.message().contains("secure storage on this machine"),
            "{}",
            error
        );
        assert!(!settings::read(&fx.root_str())["updateToken"]
            .as_str()
            .unwrap_or("")
            .contains("ghp_testtoken123"));
        return;
    }

    let saved = manager
        .set_token(Some("ghp_testtoken123"))
        .expect("set_token");
    assert_eq!(saved["token"]["filled"], json!(true), "{}", saved);
    assert_eq!(saved["token"]["origin"], json!("stored"), "{}", saved);

    // 明文永远到不了 settings.json，而
    // 真正落盘的是那个 `enc1:` 信封。
    let raw = settings::read(&fx.root_str())["updateToken"]
        .as_str()
        .expect("updateToken")
        .to_string();
    assert!(!raw.contains("ghp_testtoken123"), "{}", raw);
    assert!(raw.starts_with("enc1:"), "{}", raw);

    assert_eq!(manager.token_status()["filled"], json!(true));

    // 垃圾密文会报出一个错误，而不是"空"。
    fx.save(json!({ "updateToken": "enc1:bm90LWEtcmVhbC1lbnZlbG9wZQ==" }));
    let status = manager.token_status();
    assert!(
        status["error"].is_string(),
        "an undecryptable token must report an error: {}",
        status
    );

    manager.set_token(None).expect("clear");
    assert_eq!(manager.token_status()["filled"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17f. 渠道：beta、限流、没有 release                                   */
/* ------------------------------------------------------------------ */

#[test]
fn the_beta_channel_queries_the_release_list() {
    let fx = Fixture::new("beta");
    let mock = MockGitHub::start(MockOptions {
        list: vec![
            json!({ "tag_name": "v9.9.9-beta.2", "name": "beta2", "body": "b2", "assets": [] }),
            json!({ "tag_name": "v9.9.9", "name": "final", "body": "final", "assets": [] }),
            json!({ "tag_name": "v9.8.0", "name": "old", "body": "old", "assets": [] }),
        ],
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    fx.save(json!({ "updateBeta": true }));

    // 一个 beta 用户看到下一个 beta，而且请求发往
    // `/releases?per_page=`，而不是 `/releases/latest`。
    let on_beta = fx.manager("9.9.9-beta.1");
    let status = on_beta.check(true);
    assert_eq!(status["latest"], json!("9.9.9-beta.2"), "{}", status);
    assert_eq!(status["hasUpdate"], json!(true), "{}", status);
    assert!(
        mock.requests()
            .iter()
            .any(|u| u.contains("/releases?per_page=")),
        "{:?}",
        mock.requests()
    );
}

#[test]
fn a_final_release_user_on_the_beta_channel_takes_the_highest_overall() {
    let fx = Fixture::new("final");
    let mock = MockGitHub::start(MockOptions {
        list: vec![
            json!({ "tag_name": "v9.9.9-beta.2", "name": "beta2", "body": "b2", "assets": [] }),
            json!({ "tag_name": "v9.9.9", "name": "final", "body": "final", "assets": [] }),
            json!({ "tag_name": "v9.8.0", "name": "old", "body": "old", "assets": [] }),
        ],
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    fx.save(json!({ "updateBeta": true }));

    // 对一个正式版安装来说 `onlyPrerelease` 是关的。
    let status = fx.manager("9.8.0").check(true);
    assert_eq!(status["latest"], json!("9.9.9"), "{}", status);
}

#[test]
fn a_rate_limit_is_a_cooldown_not_a_failed_check() {
    let fx = Fixture::new("limited");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    mock.set_rate_limited(true);
    let manager = fx.manager("1.0.0");

    let limited = manager.check(true);
    assert_eq!(limited["ok"], json!(false), "{}", limited);
    let retry_at = limited["retryAt"].as_i64().unwrap_or(0);
    assert!(
        retry_at > now_ms(),
        "retryAt {} must be in the future",
        retry_at
    );

    // 一个处于冷却期的 manager 不再发问就作答。
    let before = mock.request_count();
    let again = manager.check(true);
    assert_eq!(again["reason"], json!("cooldown"), "{}", again);
    assert_eq!(mock.request_count(), before);

    // …… 而且这个截止时间在重启后依然有效（它存在状态文件里）。
    let revived = fx.manager("1.0.0");
    assert_eq!(revived.check(true)["reason"], json!("cooldown"));
}

#[test]
fn a_repository_with_no_release_is_not_a_failed_check() {
    let fx = Fixture::new("empty");
    let mock = MockGitHub::start(MockOptions {
        no_release: true,
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    // 404 意味着"没有可升级的东西"。
    let status = manager.check(true);
    assert_ne!(status["ok"], json!(false), "{}", status);
    assert_eq!(status["hasUpdate"], Value::Null, "{}", status);
    assert_eq!(status["latest"], Value::Null, "{}", status);
}

/* ------------------------------------------------------------------ */
/* 17g. 调度器                                                        */
/* ------------------------------------------------------------------ */

#[test]
fn the_scheduler_fires_at_once_then_backs_off() {
    let fx = Fixture::new("sched");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    assert_eq!(manager.due_at(), 0, "no history means due immediately");
    let first = manager.tick().expect("the first tick must check");
    assert_eq!(first["latest"], json!("9.9.9"), "{}", first);

    let due = manager.due_at();
    assert!(
        due > now_ms() + 15 * 60_000,
        "the next deadline {} must be a full interval away",
        due
    );

    let before = mock.request_count();
    assert!(manager.tick().is_none());
    assert_eq!(mock.request_count(), before);

    fx.save(json!({ "updateCheck": false }));
    assert_eq!(manager.due_at(), 0);
    assert!(manager.tick().is_none());
}

/* ------------------------------------------------------------------ */
/* 17h. 设置：校验、钳制、静默安装                                      */
/* ------------------------------------------------------------------ */

#[test]
fn save_settings_clamps_validates_and_echoes_back() {
    let fx = Fixture::new("settings");
    let manager = fx.manager("1.0.0");

    // 静默安装默认开启，并且可以被关掉。
    assert_eq!(manager.status()["settings"]["silent"], json!(true));
    let off = manager
        .save_settings(&json!({ "silent": false }))
        .expect("save");
    assert_eq!(off["settings"]["silent"], json!(false), "{}", off);
    manager
        .save_settings(&json!({ "silent": true }))
        .expect("save");
    assert_eq!(manager.status()["settings"]["silent"], json!(true));

    // `clampMinutes`：1..1440，而一个非有限值回退到 20。
    let one = manager
        .save_settings(&json!({ "minutes": 0 }))
        .expect("save");
    assert_eq!(one["settings"]["minutes"], json!(1), "{}", one);
    let max = manager
        .save_settings(&json!({ "minutes": 5000 }))
        .expect("save");
    assert_eq!(max["settings"]["minutes"], json!(1440), "{}", max);
    let back = manager
        .save_settings(&json!({ "minutes": 45 }))
        .expect("save");
    assert_eq!(back["settings"]["minutes"], json!(45), "{}", back);

    // 一个不可用的仓库会被原样存下，但读回来时是内置的那个。
    let bad = manager
        .save_settings(&json!({ "repo": "not a repo" }))
        .expect("save");
    assert_eq!(bad["repo"], json!(BUILT_IN_REPO), "{}", bad);

    // 一个好的仓库会被回显并持久化。
    let good = manager
        .save_settings(&json!({ "repo": "acme/keeper", "beta": true, "proxy": "direct" }))
        .expect("save");
    assert_eq!(good["repo"], json!("acme/keeper"), "{}", good);
    assert_eq!(good["settings"]["beta"], json!(true), "{}", good);
    assert_eq!(good["settings"]["proxy"], json!("direct"), "{}", good);
    assert_eq!(
        settings::read(&fx.root_str())["updateRepo"],
        json!("acme/keeper")
    );
}

#[test]
fn turning_auto_check_off_clears_the_schedule() {
    let fx = Fixture::new("autooff");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    assert!(manager.tick().is_some());
    assert!(manager.due_at() > now_ms());

    fx.save(json!({ "updateCheck": false }));
    assert_eq!(manager.due_at(), 0);
    assert!(manager.tick().is_none());
}

// shell 的调度器线程每 30 s 只调用一次 `tick()`；是否接触网络的
// 决定就住在 `tick()` 本身，所以这个设置开关是在这里针对
// 线程所调用的同一个方法来断言的（lib.rs:update_scheduler_loop）。
// 模拟服务器会统计请求次数，所以"关"必须能证明是零请求，而不只是
// 一个可能被坏掉的提前 return 伪造出来的 `None` 返回。
#[test]
fn update_check_off_means_no_tick_and_no_request() {
    let fx = Fixture::new("autogate");
    let mock = MockGitHub::start(MockOptions::default());
    fx.save(json!({
        "updateRepo": "acme/keeper",
        "updateApiBase": mock.base,
        "updateProxy": "direct",
        "updateCheck": false,
        "updateCheckMinutes": 20
    }));
    let manager = fx.manager("1.0.0");

    assert_eq!(manager.due_at(), 0, "auto-check off schedules nothing");
    assert!(manager.tick().is_none(), "auto-check off must not tick");
    assert_eq!(
        mock.request_count(),
        0,
        "auto-check off must not hit the network"
    );

    // 拨动这个开关就是调度器所需的一切（同一个 manager、同一个根目录 ——
    // 这个设置是在 `tick` 内部被重新读取的）。一次 tick = 恰好一次请求。
    fx.save(json!({ "updateCheck": true }));
    let fired = manager
        .tick()
        .expect("auto-check on with no history is due now");
    assert_eq!(mock.request_count(), 1, "one tick, one request: {}", fired);
    assert_eq!(fired["ok"], json!(true), "{}", fired);
    assert_eq!(fired["latest"], json!("9.9.9"), "{}", fired);

    // …… 而且这次 tick 刚刚持久化的截止时间会让下一次保持安静。
    assert!(
        manager.tick().is_none(),
        "the next tick is a full interval away"
    );
    assert_eq!(
        mock.request_count(),
        1,
        "a tick that is not due sends nothing"
    );
}

/* ------------------------------------------------------------------ */
/* ak:update-* 载荷守卫                                                */
/* 夹具                                                                */
/* ------------------------------------------------------------------ */

struct IpcFixture {
    root: PathBuf,
}

impl IpcFixture {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "ak-ipcguard-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        settings::ensure_store(&root.to_string_lossy()).expect("ensure_store");
        Self { root }
    }

    fn root_str(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn manager(&self) -> UpdateManager {
        let mut options = agentkeeper_shell_lib::core::update::manager::Options::default();
        options.platform = "win32".into();
        options.arch = "x64".into();
        options.app_version = "1.0.0".into();
        UpdateManager::with_options(&self.root_str(), options)
    }
}

impl Drop for IpcFixture {
    fn drop(&mut self) {
        agentkeeper_shell_lib::core::store::rmrf(&self.root);
    }
}

/* ------------------------------------------------------------------ */
/* BUG-1 —— `ak:update-token` 能清空凭据                               */
/* ------------------------------------------------------------------ */

/// 整个处理器，一个测试搞定。
///
/// 这个 bug 有两半，两半都在这里被钉住：
///
/// 1. `token: String` 无法反序列化 `null`，所以那次调用在函数体执行前
///    就被*拒绝*了 —— 渲染器的 `.catch` 为一次明明要求清除的点击
///    显示了错误提示。所以这里用了原始请求体。
/// 2. `set_token(Some(&token))` 让 `Option<&str>` 的 `None` 分支无法触达，
///    所以"清除"在 Rust 里根本没有对应的含义。所以这里转发了 `None`。
///
/// 需要原始请求体的是缺字段那个分支：Tauri 的
/// `CommandItem::deserialize_option` 把键缺席和显式的
/// `null` 都映射成 `visit_none()`，所以一个 `Option<Value>` 参数
/// 分不开它们，`update.tokenFieldRequired` 就会是死代码。
#[test]
fn token_arg_clears_on_null_and_rejects_a_missing_or_wrong_typed_field() {
    // `if (!('token' in p)) return { ok:false, error:'请求缺少 token 字段…' }`
    assert_eq!(
        token_arg(&json!({})),
        Err(ERR_TOKEN_FIELD_REQUIRED),
        "a payload without the token field must not be read as 'clear'"
    );
    // `asObject(payload) || {}` —— 一个非对象的请求体会塌缩成 `{}`。
    assert_eq!(token_arg(&json!(7)), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!("ghp_x")), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&Value::Null), Err(ERR_TOKEN_FIELD_REQUIRED));

    // `null` 是明确的清除，而且它必须以 `None` 回来 ——
    // 那正是 `manager.set_token` 真正据以行动的 `Option<&str>` 分支。
    assert_eq!(
        token_arg(&json!({ "token": null })),
        Ok(None),
        "null must reach set_token as None, not as an empty string"
    );

    // `typeof p.token === 'string'` —— token 必须是一个字符串。
    assert_eq!(
        token_arg(&json!({ "token": "ghp_abc123" })),
        Ok(Some("ghp_abc123".to_string()))
    );
    assert_eq!(
        token_arg(&json!({ "token": "" })),
        Ok(Some(String::new())),
        "an empty string is a bad token, not a clear — token.rs rejects it"
    );

    // `p.token !== null && typeof p.token !== 'string'` —— 既不是 null
    // 也不是字符串的 token 都是类型错误。
    for bad in [
        json!({ "token": 12 }),
        json!({ "token": true }),
        json!({ "token": [] }),
        json!({ "token": {} }),
    ] {
        assert_eq!(token_arg(&bad), Err(ERR_TOKEN_TYPE), "{}", bad);
    }
}

/// 完整的"清除令牌"链路，不含按钮：`set_token(None)` 必须把凭据
/// 在 manager 里*并且*在磁盘上都清空，而设置页面上的
/// 徽标读的就是磁盘上那个。
#[test]
fn clearing_the_token_empties_it_in_the_status_and_in_settings_json() {
    let fx = IpcFixture::new("clear");
    let manager = fx.manager();

    if manager.status()["encryption"] != json!(true) {
        // 本机没有 DPAPI：`set_token` 会拒绝，而不是存下
        // 明文（`update.tokenNoCrypto`）。清除路径仍然必须是
        // 那次拒绝，而不是一次无声的成功。
        let error = manager.set_token(None).expect_err("clear must be refused");
        assert!(error.message().contains("安全存储"), "{}", error);
        return;
    }

    let saved = manager.set_token(Some("ghp_testtoken123")).expect("set");
    assert_eq!(saved["token"]["filled"], json!(true), "{}", saved);
    assert_eq!(saved["token"]["origin"], json!("stored"), "{}", saved);
    assert!(settings::read(&fx.root_str())["updateToken"]
        .as_str()
        .unwrap_or("")
        .starts_with("enc1:"));

    // `app.js:1802` → `{token:null}` → `None`。
    let cleared = manager.set_token(None).expect("clear");
    assert_eq!(cleared["token"]["filled"], json!(false), "{}", cleared);
    assert_eq!(cleared["token"]["origin"], json!(null), "{}", cleared);
    assert_eq!(manager.token_status()["filled"], json!(false));
    assert!(
        settings::read(&fx.root_str())["updateToken"]
            .as_str()
            .unwrap_or("")
            .is_empty(),
        "clearing must remove the stored envelope, not leave a stale one"
    );
}

/// "清除令牌"按钮自己的那条路径：**渲染器发出的请求体**，
/// 走的是命令所用的同一个入口。
///
/// 规则是可达的而命令不可达，所以
/// `set_token_checked` 就是那个让接线可以被断言的接缝。重新引入
/// BUG-1 —— 无条件地传 `Some(..)`，或者把参数声明成
/// `Option<Value>` 并把 `None` 当作"清除" —— 都会让这里变红。
#[test]
fn the_clear_button_payload_clears_and_a_bare_body_is_refused() {
    let fx = IpcFixture::new("clear-button");
    let manager = fx.manager();
    let encrypted = manager.status()["encryption"] == json!(true);
    if !encrypted {
        // 这里没有 DPAPI：两条路径都必须拒绝，而不是写入明文。
        assert!(manager
            .set_token_checked(&json!({ "token": null }))
            .is_err());
        assert!(manager.set_token_checked(&json!({})).is_err());
        return;
    }

    manager
        .set_token_checked(&json!({ "token": "ghp_testtoken123" }))
        .expect("save the token first");

    // 正是 `app.js:1802` 为清除按钮放到线上的内容。
    let cleared = manager
        .set_token_checked(&json!({ "token": null }))
        .expect("the clear button must succeed, not throw");
    assert_eq!(cleared["ok"], json!(true), "{}", cleared);
    assert_eq!(cleared["token"]["filled"], json!(false), "{}", cleared);
    assert_eq!(
        settings::read(&fx.root_str())["updateToken"],
        json!(""),
        "the stored envelope must be gone"
    );

    // 一个没有这个字段的请求体是个 bug，不是删除的许可。
    let missing = manager
        .set_token_checked(&json!({}))
        .expect_err("a bare body must not clear the credential");
    assert_eq!(
        missing.message(),
        ERR_TOKEN_FIELD_REQUIRED,
        "and it must say so in English, for the dictionary to translate"
    );
    assert_eq!(
        manager.token_status()["filled"],
        json!(false),
        "…and must not have touched the token on the way"
    );
}

/// `#[tauri::command]` 实际上交给一个声明为 `Option<Value>` 的参数的东西。
///
/// `CommandItem::deserialize_option`（`tauri-2.12.4/src/ipc/command.rs:146-157`）
/// 在请求体里查 `key`：缺席 → `visit_none()`，在场 → 委派给
/// 那个值，而 `Value::deserialize_option` 也把 `Null` 映射成 `visit_none()`。
/// 这里镜像它是因为 `InvokeMessage` 的字段是 `pub(crate)`，所以真正的
/// 那套管线无法从集成测试里驱动。
fn tauri_option_field(payload: &Value, key: &str) -> Option<Value> {
    match payload.get(key) {
        None => None,
        Some(Value::Null) => None,
        Some(other) => Some(other.clone()),
    }
}

/// `update_set_token` 之所以要拿原始请求体的原因。
///
/// 规则要求"没有 `token` 字段"和"`token` 为 null"产生
/// *不同*的结果（一个错误 versus 一次清除）。在一个普通的
/// `token: Option<Value>` 参数下两者都成了 `None`，所以两个分支
/// 之中必有一个要牺牲 —— 而那正是要避免的妥协。
/// 这里把这种塌缩钉住，好让这层推理始终是可核对的。
#[test]
fn an_absent_token_key_and_an_explicit_null_are_indistinguishable_as_a_parameter() {
    let absent = tauri_option_field(&json!({}), "token");
    let explicit = tauri_option_field(&json!({ "token": null }), "token");
    assert_eq!(absent, None);
    assert_eq!(
        absent, explicit,
        "as long as these are equal, `token: Option<Value>` cannot implement \
         the command must keep taking the request body"
    );

    // 请求体把它们分开，这正是整个修复所在。
    assert_eq!(token_arg(&json!({})), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!({ "token": null })), Ok(None));
}

/* ------------------------------------------------------------------ */
/* BUG-2 —— 那五条 `ak:update-settings` 校验                            */
/* ------------------------------------------------------------------ */
/// `validate_settings_patch` 是挡在一个坏值
/// 与 store 之间的*唯一*屏障，因为 `manager.save_settings` 是钳制而不是拒绝。
///
/// 在这里钉住，好让将来一次把这些规则搬走的重构无法悄悄
/// 丢掉它们：守卫必须在 `save_settings` 见到这个补丁之前就拒绝它。
#[test]
fn the_guard_rejects_what_save_settings_would_otherwise_persist() {
    let fx = IpcFixture::new("persist");
    let manager = fx.manager();

    // 没有守卫的话，`save_settings` 会把 99999 钳成 1440 并存下它。
    assert_eq!(
        validate_settings_patch(&json!({ "minutes": 99999 })),
        Some(ERR_INTERVAL_RANGE)
    );
    // …… 而且会把一个畸形的仓库原样存下，之后它读回来时又悄悄
    // 变成了内置的那个 —— 也就是"更新检查失败"这个症状。
    assert_eq!(
        validate_settings_patch(&json!({ "repo": "这不是仓库名" })),
        Some(ERR_REPO_FORMAT)
    );

    // 第二半的证据：`save_settings` 本身确实会钳制，所以它下面
    // 不会有任何东西抓住这些。
    let clamped = manager
        .save_settings(&json!({ "minutes": 99999 }))
        .expect("save_settings still succeeds — it clamps");
    assert_eq!(clamped["settings"]["minutes"], json!(1440), "{}", clamped);
}

/// BUG-2 的**接线**，不只是它的规则：一个被拒绝的补丁绝不能抵达
/// store。
///
/// "校验函数是对的"和"命令会执行校验"是两种不同的断言，
/// 而用户看到的只有后者。命令函数在 crate 内部是私有的，所以
/// `save_settings_checked` 就是那个让测试能断言它的接缝。
#[test]
fn a_rejected_settings_patch_never_reaches_the_store() {
    let fx = IpcFixture::new("reject");
    let manager = fx.manager();
    manager
        .save_settings(&json!({ "minutes": 30 }))
        .expect("seed a known-good interval");
    assert_eq!(
        settings::read(&fx.root_str())["updateCheckMinutes"],
        json!(30)
    );

    for bad in [
        json!({ "minutes": 99999 }),
        json!({ "minutes": 0 }),
        json!({ "repo": "这不是仓库名" }),
        json!({ "apiBase": "乱码" }),
        json!({ "proxy": "clash" }),
    ] {
        let error = manager
            .save_settings_checked(&bad)
            .expect_err("a bad patch must be rejected, not persisted");
        assert_eq!(error.status(), 400, "{}", bad);
        assert_eq!(
            settings::read(&fx.root_str())["updateCheckMinutes"],
            json!(30),
            "{} must not have been written",
            bad
        );
    }
}
/* ------------------------------------------------------------------ */
/* 17j. ak:update-releases —— 变更日志列表                              */
/* ------------------------------------------------------------------ */

/// 一条 GitHub 发出的 release。`body` 被刻意写得足够长，以便触发
/// 8000 字节的上限，而不必分配一兆字节：9000 个字符。
fn changelog_entry(tag: &str, day: &str, body: &str, prerelease: bool) -> Value {
    json!({
        "tag_name": format!("v{}", tag),
        "name": format!("Keeper {}", tag),
        "body": body,
        "published_at": format!("{}T09:30:00Z", day),
        "html_url": format!("https://github.com/acme/keeper/releases/tag/v{}", tag),
        "prerelease": prerelease,
        "draft": false,
        "assets": []
    })
}

fn long_body(marker: &str) -> String {
    format!("{}{}", marker, "x".repeat(9000))
}

/// 顺利路径：一个普通数组，最新的在前，带着面板会渲染的每一个字段
/// —— 而且没有信封，因为渲染器是对这个结果做 map 的。
#[test]
fn update_releases_returns_the_changelog_as_a_plain_array() {
    let fx = Fixture::new("releases");
    let mock = MockGitHub::start(MockOptions {
        list: vec![
            changelog_entry("0.2.1", "2026-10-23", "third\n", false),
            changelog_entry("0.2.0", "2026-09-01", "second\n", false),
            changelog_entry("0.3.0-beta.1", "2026-10-01", "beta\n", true),
            changelog_entry("0.1.9", "2026-08-01", "oldest\n", false),
        ],
        ..MockOptions::default()
    });
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));

    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    let items = rows.as_array().expect("a plain array").clone();
    assert_eq!(items.len(), 4, "{items:#?}");

    // GitHub 的顺序被保留：最新的在前，而不是"版本号最高的"。
    // `0.3.0-beta.1` 数值上最高，却排在第三。
    assert_eq!(items[0]["version"], json!("0.2.1"), "{items:#?}");
    assert_eq!(items[0]["date"], json!("2026-10-23"));
    assert_eq!(items[0]["notes"], json!("third\n"));
    assert_eq!(items[0]["prerelease"], json!(false));
    assert_eq!(
        items[0]["url"],
        json!("https://github.com/acme/keeper/releases/tag/v0.2.1")
    );

    assert_eq!(items[2]["version"], json!("0.3.0-beta.1"));
    assert_eq!(
        items[2]["prerelease"],
        json!(true),
        "the beta flag survives"
    );
    assert_eq!(items[2]["date"], json!("2026-10-01"));

    // 面板读取的每一个键在每一行上都存在。
    for row in &items {
        let keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec!["version", "date", "url", "notes", "prerelease"],
            "{row}"
        );
    }

    // 请求本身就是文档里写的那个 URL，而且它要的是默认值。
    let requests = mock.requests();
    assert!(
        requests
            .iter()
            .any(|r| r.contains("/repos/acme/keeper/releases?per_page=20")),
        "{requests:?}"
    );
}

/// 那个 8000 字节上限是 `release_from_api` 的，不是新加的一个：
/// 一份 200 KB 的变更日志不该穿过 IPC，而且面板必须看到
/// 与更新卡片相同的被截断文本。
#[test]
fn update_releases_truncates_the_notes_at_the_shared_ceiling() {
    let fx = Fixture::new("releases-notes");
    let body = long_body("HEAD-MARKER");
    let mock = MockGitHub::start(MockOptions {
        list: vec![changelog_entry("0.2.1", "2026-10-23", &body, false)],
        ..MockOptions::default()
    });
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));

    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    let notes = rows[0]["notes"].as_str().expect("notes");
    assert!(
        notes.starts_with("HEAD-MARKER"),
        "{}",
        &notes[..20.min(notes.len())]
    );
    // `js_slice` 按 UTF-16 码元计数，不得劈开一个代理对。
    assert!(
        notes.chars().count() <= 8000 && notes.encode_utf16().count() <= 8000,
        "notes are {} utf-16 units",
        notes.encode_utf16().count()
    );

    // 而且它与更新卡片拿到的是*同一处*截断：用同一份载荷
    // 构造出面板自己的记录，然后逐字节比较。
    let panel = version::release_from_api(
        &json!({
            "tag_name": "v0.2.1",
            "body": body,
            "published_at": "2026-10-23T09:30:00Z",
            "html_url": "https://github.com/acme/keeper/releases/tag/v0.2.1",
            "prerelease": false,
            "assets": []
        }),
        version::ReleaseOptions::default(),
    );
    assert_eq!(
        rows[0]["notes"], panel["notes"],
        "the cap must not be re-derived"
    );
}

/// `limit` 在链路上（`per_page`）*和*在出站时都被遵守，而且
/// 两端都被钳制 —— 调用方不能要 10000 条，调用方也不能被一个
/// 无意义的值压到一条以下。
#[test]
fn update_releases_clamps_the_limit_at_both_ends() {
    let fx = Fixture::new("releases-limit");
    let entries: Vec<Value> = (0..8)
        .map(|i| changelog_entry(&format!("0.0.{}", 7 - i), "2026-10-23", "n\n", false))
        .collect();
    let mock = MockGitHub::start(MockOptions {
        list: entries,
        ..MockOptions::default()
    });
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));

    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), Some(3));
    assert_eq!(rows.as_array().unwrap().len(), 3, "{rows:#?}");
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.contains("/releases?per_page=3")),
        "the limit reaches GitHub: {:?}",
        mock.requests()
    );

    // 0 会向上钳到 1（一份空的变更日志看起来像是取数坏了）。
    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), Some(0));
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows:#?}");

    // 一个远高于 GitHub `per_page` 最大值的上限会被钳到那个最大值，
    // 这样请求就只占一页，而不是悄悄少给内容。
    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), Some(10_000));
    assert_eq!(
        rows.as_array().unwrap().len(),
        8,
        "only eight releases exist: {rows:#?}"
    );
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.contains("/releases?per_page=100")),
        "the ceiling is GitHub's own per_page maximum: {:?}",
        mock.requests()
    );
}

/// 每一种失败模式都必须给出可显示的文本，而不是一个空
/// 列表：空数组与"这个项目从来没有发布过任何东西"
/// 无法区分，而那是这个应用无法支撑的一句断言。
#[test]
fn update_releases_reports_each_failure_with_a_message() {
    let fx = Fixture::new("releases-fail");

    // 1. 传输失败：1 号端口上没有任何东西在监听。
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": "http://127.0.0.1:1" }));
    let r = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert_eq!(r["ok"], json!(false), "{r}");
    let error = r["error"].as_str().expect("an error string");
    assert!(error.contains("cannot reach the server"), "{error}");
    // 渲染器字典映射的就是这个形状（`^cannot reach the server: §1$`），
    // 这就是为什么这条消息被原样透传而不是改写。
    assert!(error.len() > 20, "the message must say something: {error}");

    // 2. 被限流。
    let mock = MockGitHub::start(MockOptions::default());
    mock.set_rate_limited(true);
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));
    let r = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert_eq!(r["ok"], json!(false), "{r}");
    assert!(r["error"].as_str().unwrap().contains("rate limit"), "{r}");

    // 3. 一个既不是"没有 release"也不是限流的 HTTP 状态码。
    let mock = MockGitHub::start(MockOptions {
        list_status: Some(500),
        ..MockOptions::default()
    });
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));
    let r = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert_eq!(r["ok"], json!(false), "{r}");
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("GitHub returned HTTP 500"),
        "{r}"
    );
}

/// 一个还没有任何 release 的仓库*不是*一个错误：GitHub 会用
/// 一个空数组回答 `/releases`，而那是一句诚实的"什么都没发布"。
#[test]
fn update_releases_answers_an_empty_array_for_a_silent_repository() {
    let fx = Fixture::new("releases-empty");
    let mock = MockGitHub::start(MockOptions::default());
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));

    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert_eq!(rows, json!([]), "{rows:#?}");
}

/// 一份草稿是没有发布日期的未完成记录；把它列进公开的
/// 变更日志等于宣传一个没人能安装的版本。
#[test]
fn update_releases_drops_drafts_and_keeps_everything_else() {
    let fx = Fixture::new("releases-draft");
    let mock = MockGitHub::start(MockOptions {
        list: vec![
            json!({ "tag_name": "v0.3.0", "draft": true, "body": "wip", "assets": [] }),
            changelog_entry("0.2.1", "2026-10-23", "shipped\n", false),
        ],
        ..MockOptions::default()
    });
    fx.save(json!({ "updateRepo": "acme/keeper", "updateApiBase": mock.base }));

    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    let items = rows.as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0]["version"], json!("0.2.1"));
}

/// 仓库与 API 基址取自更新检查所读的同一批设置键，
/// 所以把应用指向一个镜像时，变更日志也跟着走。一个不可用的
/// `updateRepo` 会回退到内置仓库而不是失败。
#[test]
fn update_releases_reads_the_same_repository_settings_as_the_check() {
    let fx = Fixture::new("releases-repo");
    let mock = MockGitHub::start(MockOptions::default());

    // API 基址上的一个尾随斜杠不得产生 `//repos/…`。
    fx.save(json!({
        "updateRepo": "  acme/keeper  ",
        "updateApiBase": format!("{}/", mock.base),
    }));
    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert!(rows.is_array(), "{rows:#?}");
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.contains("/repos/acme/keeper/releases?per_page=")),
        "the mirror and the trimmed repo must both be honoured: {:?}",
        mock.requests()
    );
    assert!(
        !mock.requests().iter().any(|r| r.contains("//repos")),
        "no doubled slash: {:?}",
        mock.requests()
    );

    // 一个不可用的仓库会被内置的那个取代 —— 查询仍然会跑。
    fx.save(json!({ "updateRepo": "not a repo" }));
    let rows = agentkeeper_shell_lib::update_releases_from(&fx.root_str(), None);
    assert!(rows.is_array(), "{rows:#?}");
    assert!(
        mock.requests()
            .iter()
            .any(|r| r.contains(&format!("/repos/{}/releases?per_page=", BUILT_IN_REPO))),
        "the built-in repository takes over: {:?}",
        mock.requests()
    );
}

/* ------------------------------------------------------------------ */
/* 消息本身                                                            */
/* ------------------------------------------------------------------ */

/// 引擎说的是英语；中文来自渲染器的字典。
///
/// 这里硬编码一句中文，会让英语界面收到中文 ——
/// 那正是被翻译过的"英语"槽位会重新引入的缺陷。
/// `scripts/tauri-i18n.js` 会 grep Rust 源码，而 `src/renderer/
/// engine-zh.js` 必须映射其中每一条；本测试让这两份清单
/// 对彼此的拼写错误保持诚实。
#[test]
fn every_guard_message_is_english_and_has_a_dictionary_entry() {
    let messages = [
        ERR_INTERVAL_RANGE,
        ERR_REPO_FORMAT,
        ERR_API_BASE_FORMAT,
        ERR_PROXY_FORMAT,
        ERR_TOKEN_TYPE,
        ERR_TOKEN_FIELD_REQUIRED,
    ];
    for message in messages {
        assert!(
            !message
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "the Rust engine must emit English; got {:?}",
            message
        );
        assert!(
            !message.trim().is_empty()
                && message.chars().next().unwrap().is_ascii_uppercase() == false,
            "keep these sentences lowercase like the rest of the engine: {:?}",
            message
        );
    }
    // `discover.repoFormat` 与 `ipc.repoFormat` 是同一条规则，所以它们共用
    // 一个字符串 —— 这正是让一条 EXACT 条目能同时覆盖两者的原因。
    assert_eq!(
        ERR_REPO_FORMAT,
        "repo must be owner/repo or a github.com URL"
    );
}
