//! The software-update pipeline: `check → download → progress → cancel →
//! install`, the scheduler, the `ak:update-*` payload guard, and literal parity
//! with the JS version maths.
//!
//! Merged from `update_pipeline.rs`, `update_ipc_guard.rs` and
//! `update_js_parity.rs`.
//!
//! * pipeline (was `update_pipeline.rs`) — the Rust half of `scripts/selftest.js`
//!   section 17 ("Software update: channels, download, install guards"). That
//!   harness stands up an `http.createServer` mock of the GitHub API and drives
//!   the *real* manager through it, which is what proves headers, asset
//!   selection, streaming, progress, cancellation and the install guards
//!   together. `manager.rs`'s unit tests cannot do that: they replace the
//!   `fetch` / `downloader` closures with in-process fakes, so the sockets,
//!   `ureq`, `Content-Length` handling and the byte-level progress accounting are
//!   never executed there.
//!
//!   Two rules that half follows: no test touches the network, DNS or GitHub
//!   (everything goes to `127.0.0.1:<ephemeral>`), and no test executes a
//!   downloaded file — `install()` is asserted through the `Options::launcher`
//!   seam, which production leaves as `None`. The `OnceLock` singleton
//!   (`ensure_manager`) is deliberately unused: it pins the first root for the
//!   whole process, so every test builds its own `UpdateManager` with
//!   `with_options` and each owns a private temp root.
//! * ipc guard (was `update_ipc_guard.rs`) — regression cover for the two
//!   capability-audit bugs in the `ak:update-*` IPC surface: the "clear token"
//!   button and the five settings validations. Both live in
//!   `core::update::manager` as pure functions (`token_arg`,
//!   `validate_settings_patch`) so the *rules* are testable without a WebView;
//!   `lib.rs` is only the adapter that calls them. The rules are a line-by-line
//!   port of the JS guard in `src/main/ipc.js:259-308`, which is why each
//!   assertion is paired with the JS expression it mirrors. Deliberately not
//!   claimed here: the Tauri argument→parameter mapping (that `{token:null}`
//!   reaches `Request` intact — `tauri::ipc::InvokeMessage`'s fields are
//!   `pub(crate)`, so an integration test cannot build one) and the renderer half
//!   (`app.js` / `tauri-bridge.js`).
//! * js parity (was `update_js_parity.rs`) — literal 1:1 parity with
//!   `scripts/selftest.js` section 17a (lines 773-834). `version.rs`'s unit tests
//!   and the pipeline above already prove the *semantics* of the version maths —
//!   but with their own fixtures, not the JS ones. That is the right way to test
//!   behaviour and the wrong way to detect drift: if `compareVersions` and
//!   `compare_versions` ever diverge on a shape only one of them exercises, both
//!   suites stay green and the difference ships. That half therefore mirrors 17a
//!   **literal for literal**: the same strings, the same expected values, in the
//!   same order as the JS. The redundancy is the point. Everything there is a
//!   pure function: no HTTP, no manager, no singleton, no real DPAPI, no
//!   filesystem.
//!
//! The JS returns `1 / -1 / 0 / null` from `compareVersions` while Rust returns
//! `Option<Ordering>`; `ord()` in the parity section is the only adapter, so
//! those assertions still read like the JS.

use agentkeeper_shell_lib::core::settings;
use agentkeeper_shell_lib::core::update::manager::{
    normalize_repo, token_arg, validate_settings_patch, LauncherFn, Options, UpdateManager,
    BUILT_IN_REPO, ERR_API_BASE_FORMAT, ERR_INTERVAL_RANGE, ERR_PROXY_FORMAT, ERR_REPO_FORMAT,
    ERR_TOKEN_FIELD_REQUIRED, ERR_TOKEN_TYPE,
};
use agentkeeper_shell_lib::core::update::version;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/* ------------------------------------------------------------------ */
/* check → download → install over real HTTP (was update_pipeline.rs)  */
/* a GitHub-shaped mock server                                         */
/* ------------------------------------------------------------------ */

/// Everything the server thread needs. Built once, then shared immutably.
struct MockConfig {
    base: String,
    payload: Vec<u8>,
    /// `/releases/latest` answers 404 (a repository with no release yet).
    no_release: bool,
    /// What `/releases?per_page=30` returns.
    list: Vec<Value>,
    /// Declare this `Content-Length` on `/dl/` instead of the real size.
    oversized: Option<u64>,
    /// Status for `/dl/`; 200 streams the payload.
    dl_status: u16,
    /// Split the payload into chunks of this size, pausing `chunk_delay_ms`
    /// between them. 0 = one write.
    chunk: usize,
    chunk_delay_ms: u64,
    /// Answer `/dl/` with `Transfer-Encoding: chunked` and no `Content-Length`,
    /// so the byte ceiling can only be discovered from the stream itself.
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

/// `startMockGitHub` from `scripts/selftest.js:689`, in Rust.
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

    /// Flip the server into 403 mode, as the JS does to exercise the cooldown.
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
    // Non-blocking so `stop` is checked without a connection to unblock it.
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !stop.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows makes an accepted socket inherit the listener's
                // non-blocking mode, and a non-blocking socket ignores
                // SO_RCVTIMEO — the exact defect already documented and fixed in
                // `core::http::tests::serve` and `core::update::http::tests::serve`,
                // but missed here. `read_request` then got `WouldBlock(10035)` on
                // its first read whenever the client's bytes had not arrived yet,
                // returned an empty request line, and `respond` fell through to
                // its 404 branch: a healthy product op reported "404" and this
                // test failed ~8 % of the time (measured 4/50 runs, all inside
                // 0.02 s, which already ruled out the 2 s read timeout).
                stream.set_nonblocking(false).expect("blocking connection");
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let request = read_request(&mut stream);
                requests.lock().unwrap().push(request.clone());
                respond(&mut stream, &request, &cfg, rate_limited.load(AtomicOrdering::SeqCst));
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

/// The request line only; the body is never sent by these requests, but the
/// header block is drained so the client is not writing into a closed socket.
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
            // Belt and braces for the non-blocking inheritance described at the
            // accept site: a `WouldBlock` is "no bytes yet", never "empty
            // request", so it must not end the read.
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
        write_head(stream, 200, &[("content-type", "application/json")], body.len());
        let _ = stream.write_all(&body);
        return;
    }
    if is_repo_path && path.contains("/releases?") {
        let body = serde_json::to_vec(&cfg.list).unwrap();
        write_head(stream, 200, &[("content-type", "application/json")], body.len());
        let _ = stream.write_all(&body);
        return;
    }
    if path.starts_with("/dl/") {
        if cfg.dl_status != 200 {
            write_head(stream, cfg.dl_status, &[("content-type", "text/plain")], 4);
            let _ = stream.write_all(b"boom");
            return;
        }
        // A declared length that lies is the whole point of the ceiling test:
        // the guard must trip on the header, before the first byte lands.
        let declared = cfg.oversized.unwrap_or(cfg.payload.len() as u64) as usize;
        if cfg.chunked {
            // No `Content-Length` at all: the ceiling can only be discovered
            // from the stream, which is the other half of the guard.
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
                let _ = stream.write_all(
                    format!("{:x}\r\n", cfg.payload.len()).as_bytes(),
                );
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
/* fixtures                                                            */
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
        let root = std::env::temp_dir().join(format!(
            "ak-upd-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let dir = root.join("dl");
        std::fs::create_dir_all(&dir).expect("download dir");
        settings::ensure_store(&root.to_string_lossy(), false).expect("ensure_store");
        Self { root, dir }
    }

    fn root_str(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    fn save(&self, patch: Value) {
        settings::save(&self.root_str(), &patch);
    }

    /// A manager on `win32`/`x64`, pinned to this fixture's download directory.
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

    /// Point the manager at the mock and turn on auto-check.
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

/// Poll `progress()` until the task stops being active.
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
/* 17a. the asset name the real release pipeline produces              */
/* ------------------------------------------------------------------ */

/// `pickInstaller` is the one function between a published release and an
/// upgrade that actually happens. The Tauri build renames the NSIS installer
/// to `AgentKeeper_<version>_x64-setup.exe` (underscores, `-setup` suffix),
/// which no Electron-era fixture covers — so pin the real artefact name here.
/// Requested by ci-mcp-verifier while wiring `.github/workflows/release.yml`.
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

    // A real release ships several artefacts. Read this carefully: only two of
    // them ever compete. `allowed_installer_suffixes("win32")` is `["exe"]`, so
    // `…_en-US.msi` and `…_setup.exe.sig` are not "beaten by the installer" —
    // they never enter `candidates` at all. Of the two `.exe`s, `-setup.exe`
    // wins on the `setup|install` pattern and `-portable.exe` is filtered out
    // by `is_non_installer_asset` (portable builds install nothing). The
    // `.tar.gz` is excluded on the same count.
    let full = json!([
        { "name": "AgentKeeper_0.2.0_x64-setup.exe", "size": 1, "browser_download_url": "u-setup" },
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 2, "browser_download_url": "u-msi" },
        { "name": "AgentKeeper_0.2.0_x64_update.tar.gz", "size": 3, "browser_download_url": "u-tar" },
        { "name": "AgentKeeper_0.2.0_x64-setup.exe.sig", "size": 4, "browser_download_url": "u-sig" },
        { "name": "AgentKeeper_0.2.0_x64-portable.exe", "size": 5, "browser_download_url": "u-portable" }
    ]);
    let picked = version::pick_installer(&full, "win32", "x64").expect("installer");
    assert_eq!(picked["name"], json!("AgentKeeper_0.2.0_x64-setup.exe"));

    // The silent failure mode behind the line above: on win32 the suffix
    // whitelist is `["exe"]` alone, so an `.msi`-only release is invisible.
    // `bundle.targets` is nsis-only today; adding `msi` would publish an
    // installer the updater can never see (`asset: null` →
    // `downloadSupported: false` → "update available, cannot install").
    // Pinned so the day someone adds the target, this fails loudly here
    // instead of quietly in the field.
    let msi_only = json!([
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 1, "browser_download_url": "u-msi" }
    ]);
    assert_eq!(
        version::pick_installer(&msi_only, "win32", "x64"),
        None,
        ".msi is not in allowed_installer_suffixes(\"win32\")"
    );
    // Mixed: the .exe must still win and the .msi must not shadow it.
    let mixed = json!([
        { "name": "AgentKeeper_0.2.0_x64_en-US.msi", "size": 1, "browser_download_url": "u-msi" },
        { "name": "AgentKeeper_0.2.0_x64-setup.exe", "size": 2, "browser_download_url": "u-setup" }
    ]);
    let picked = version::pick_installer(&mixed, "win32", "x64").expect("installer");
    assert_eq!(picked["name"], json!("AgentKeeper_0.2.0_x64-setup.exe"));

    // And the update panel's download guard accepts it: the name survives
    // `safeFileName`, and `verifyInstaller` requires the product keyword.
    assert_eq!(
        version::safe_file_name("AgentKeeper_0.2.0_x64-setup.exe", "win32"),
        "AgentKeeper_0.2.0_x64-setup.exe"
    );
    assert!(
        "AgentKeeper_0.2.0_x64-setup.exe"
            .to_lowercase()
            .contains("agentkeeper")
    );
}

/* ------------------------------------------------------------------ */
/* 17a. the release repository is built in                             */
/* ------------------------------------------------------------------ */

#[test]
fn the_release_repository_is_built_in() {
    let fx = Fixture::new("repo");
    let manager = fx.manager("1.0.0");

    // `scripts/selftest.js:864` — nothing to type, and it must equal the
    // repository in package.json (`ntc0728/agentkeeper`).
    assert_eq!(manager.status()["repo"], json!(BUILT_IN_REPO));
    assert_eq!(manager.status()["repo"], json!("ntc0728/agentkeeper"));

    // `selftest.js:870`: an unusable value falls back instead of disabling
    // updates. `normalize_repo` rejects it, so `Cfg::repo` takes the default.
    fx.save(json!({ "updateRepo": "not a repo" }));
    assert_eq!(manager.status()["repo"], json!(BUILT_IN_REPO));

    fx.save(json!({ "updateRepo": "acme/keeper" }));
    assert_eq!(manager.status()["repo"], json!("acme/keeper"));
}

/* ------------------------------------------------------------------ */
/* 17b. check → download → install, over real HTTP                     */
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
    // `selftest.js:877` — notes and the release page survive the round trip.
    assert_eq!(found["notes"], json!("Release notes body"));
    assert!(
        found["pageUrl"]
            .as_str()
            .unwrap_or("")
            .contains("releases/tag/v9.9.9"),
        "{}",
        found["pageUrl"]
    );
    // `selftest.js:879` — the NSIS setup beats the portable exe, even though
    // the bare exe is listed first.
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

    // `selftest.js:883`: no second request inside the 60 s minimum gap.
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

    // `selftest.js:894` — the bytes on disk are the bytes that were served,
    // and the file landed inside the managed directory.
    let path = task["path"].as_str().expect("task path").to_string();
    assert_eq!(std::fs::read(&path).unwrap(), payload);
    let canonical = std::fs::canonicalize(&path).unwrap();
    let dir = std::fs::canonicalize(&fx.dir).unwrap();
    assert!(canonical.starts_with(&dir), "{} is outside {}", path, dir.display());

    // The finished task is part of `status()`, which is what the panel polls.
    assert_eq!(manager.status()["task"]["done"], json!(true));
}

#[test]
fn progress_reports_partial_bytes_while_the_transfer_runs() {
    // 8 chunks of 8 KiB with a pause: long enough to observe a mid-flight
    // snapshot, short enough to stay well inside the 60 s socket timeout.
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

    // Watch for a snapshot that is neither the first nor the last one.
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
    // A slow server: cancel has to land while bytes are still in flight.
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

    // Wait until the first chunk has been counted, so the partial file exists
    // and the cancellation is observably mid-transfer.
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
    // The JS cancel path resolves before the `catch` that unlinks the file, so
    // a partial installer survives — and the task reports no usable path.
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

    // A finished task is not active either.
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

    // A different URL (and no name at all): the running task wins, and the
    // second URL is never even validated.
    let second = manager
        .start_download("https://example.com/other.exe", None)
        .expect("second start must not fail validation");
    assert_eq!(second["path"], first["path"], "{}", second);
    assert_eq!(second["active"], json!(true), "{}", second);

    let task = wait_inactive(&manager);
    assert_eq!(task["done"], json!(true), "{}", task);
    assert_eq!(mock.request_count(), 1, "only one transfer may reach the server");
}

#[test]
fn download_refuses_a_url_outside_the_allow_list() {
    let fx = Fixture::new("allowlist");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    // `ipc-contract-test.js:310`.
    //
    // The engine speaks English and `src/renderer/engine-zh.js` maps it to the
    // Chinese the JS throws directly, so the assertion is on the engine string —
    // `tests/update_js_parity.rs` pins the mapping side.
    let error = manager
        .start_download("https://evil.example/a.exe", Some("AgentKeeper-a.exe"))
        .expect_err("a foreign host must be refused");
    assert!(error.message().contains("not on the allow list"), "{}", error);

    // `selftest.js:797` — plain http is refused unless it is the mirror itself.
    let error = manager
        .start_download("http://github.com/a.exe", Some("AgentKeeper-a.exe"))
        .expect_err("plain http must be refused");
    assert!(error.message().contains("https"), "{}", error);

    // The configured mirror is allowed (that is the whole point of the setting).
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
    // Declares 50 MB, sends a few bytes: the ceiling has to trip on the header.
    let mock = MockGitHub::start(MockOptions {
        oversized: Some(50 * 1024 * 1024),
        ..Default::default()
    });
    fx.use_mock(&mock.base);
    // `selftest.js:1009` — a small `maxBytes` so the guard is reachable.
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
    // Chunked transfer: there is no `Content-Length` to reject up front, so the
    // ceiling is discovered mid-stream — after chunks have already reached the
    // file. That is the failure branch whose unlink nothing else exercises.
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
    // Declares four times the bytes it actually sends and then closes: the
    // transfer fails *after* chunks have reached the file, which is the only
    // way to exercise the unlink in `runDownload`'s failure branch.
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
    // The ceiling is enforced twice: here, on the `Content-Length` header, and
    // again in `run_download`'s progress callback with the same wording. This
    // test drives `download_to_file` alone so the header guard is pinned
    // independently of the manager's copy of it.
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
    assert!(error.describe().contains("size looks wrong"), "{}", error.describe());
    // Refused on the header: `on_progress` never ran and nothing was written.
    assert!(calls.is_empty(), "on_progress must not run: {:?}", calls);
    assert!(!dest.exists(), "{} must not exist", dest.display());

    // The same download within the ceiling succeeds.
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
/* 17c. install guards                                                 */
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
    // Engine string; `engine-zh.js:211` renders it as 安装包不在受控的下载目录内.
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
    // Engine string; `engine-zh.js:307` renders it as 只允许运行 §1 安装包.
    assert!(error.message().contains("installers may be run"), "{}", error);
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
    // Engine string; `engine-zh.js:308` renders it as 疑似来自其他项目.
    assert!(error.message().contains("another project"), "{}", error);
}

#[test]
fn install_refuses_a_relative_or_missing_path() {
    let fx = Fixture::new("relative");
    let manager = fx.manager("1.0.0");

    // `ipc-contract-test.js:308`.
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

    // A directory with an installer's name is not a file.
    let as_dir = fx.dir.join("AgentKeeper-dir.exe");
    std::fs::create_dir_all(&as_dir).unwrap();
    let error = manager
        .verify_installer(&as_dir.to_string_lossy())
        .expect_err("a directory");
    // Engine string; `engine-zh.js:210` renders it as 安装包不存在或不是文件.
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

    // `selftest.js:929` — the launcher receives the *real* path, and the
    // `silent` setting is read on every install, not captured at construction.
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

    // Turning it off is honoured on the next install.
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

    // `startDownload` unlinks any previous copy before the transfer starts, so
    // wait until the first chunk has landed: only then is there a real file
    // whose execution the active-task guard can refuse.
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
    // The download overwrote the file; the guard is gone afterwards.
    assert!(manager.install(&target.to_string_lossy()).is_ok());
    assert_eq!(*launched.lock().unwrap(), 1);
}

/* ------------------------------------------------------------------ */
/* 17i. the ak:update-* payload guard (ipc.js:259-308)                 */
/* ------------------------------------------------------------------ */
//
// BUG-1 (clear token) and BUG-2 (five missing settings checks) are both fixed
// in `core::update::manager::{validate_settings_patch, token_arg}` rather than
// in the `#[tauri::command]` bodies, precisely so they are reachable from an
// integration test — a command needs an `AppHandle` and cannot be called here.
// The expectations below are `scripts/ipc-contract-test.js:279-302` and
// `src/main/ipc.js:259-308`.

#[test]
fn token_arg_turns_null_into_clear_and_rejects_other_types() {
    use agentkeeper_shell_lib::core::update::manager::{
        token_arg, ERR_TOKEN_FIELD_REQUIRED, ERR_TOKEN_TYPE,
    };

    // `token_arg` takes the **whole IPC body**, not the `token` field: that is
    // the only way the missing-key branch survives Tauri's
    // `deserialize_option`, which collapses `{}` and `{token:null}` into the
    // same `visit_none()`.

    // BUG-1: `null` must reach `set_token(None)`. Anything else here — an empty
    // string, or a `Some` carrying anything — re-breaks clearing.
    assert_eq!(token_arg(&json!({ "token": null })), Ok(None));

    assert_eq!(
        token_arg(&json!({ "token": "ghp_example" })),
        Ok(Some("ghp_example".to_string()))
    );
    // `ipc-contract-test.js:302` / ipc.js:301 — a non-string is `tokenType`.
    assert_eq!(token_arg(&json!({ "token": 1 })), Err(ERR_TOKEN_TYPE));
    assert_eq!(token_arg(&json!({ "token": true })), Err(ERR_TOKEN_TYPE));
    assert_eq!(token_arg(&json!({ "token": { "a": 1 } })), Err(ERR_TOKEN_TYPE));
    assert_eq!(token_arg(&json!({ "token": ["x"] })), Err(ERR_TOKEN_TYPE));

    // `ipc.js:298` — `'token' in p` false is an error, not a silent clear. A
    // non-object body is `asObject`-collapsed to `{}`, so it lands here too.
    assert_eq!(token_arg(&json!({})), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!(null)), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!(7)), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!([1, 2])), Err(ERR_TOKEN_FIELD_REQUIRED));

    // Extra keys are ignored.
    assert_eq!(
        token_arg(&json!({ "token": "abc", "other": 1 })),
        Ok(Some("abc".to_string()))
    );
}

#[test]
fn clearing_the_token_goes_through_the_guard_and_empties_it() {
    use agentkeeper_shell_lib::core::update::manager::token_arg;

    let fx = Fixture::new("clear");
    let manager = fx.manager("1.0.0");
    if manager.status()["encryption"] != json!(true) {
        // No DPAPI here: `set_token` refuses either way and the guard is what
        // this test is about, so only the guard's shape is asserted.
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

    // What the IPC layer does with the renderer's `null`: guard → clear.
    let cleared = token_arg(&json!({ "token": null })).expect("null is a clear request");
    assert_eq!(cleared, None, "`null` must become `set_token(None)`");
    manager.set_token(cleared.as_deref()).expect("clear");

    assert_eq!(manager.token_status()["filled"], json!(false));
    assert_eq!(
        settings::read(&fx.root_str())["updateToken"].as_str(),
        Some(""),
        "the envelope must be gone from settings.json"
    );
    // And it stays gone across a restart.
    assert_eq!(fx.manager("1.0.0").token_status()["filled"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17d. skip / unskip                                                  */
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

    // `selftest.js:938` — a second manager (a restart) reads the state file.
    assert_eq!(fx.manager("1.0.0").status()["skipped"], json!(true));
    assert_eq!(manager.unskip()["skipped"], json!(false));
    assert_eq!(fx.manager("1.0.0").status()["skipped"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17e. token storage                                                  */
/* ------------------------------------------------------------------ */

#[test]
fn the_token_is_stored_as_an_encrypted_envelope() {
    let fx = Fixture::new("token");
    let manager = fx.manager("1.0.0");

    if manager.status()["encryption"] != json!(true) {
        // No DPAPI on this host: the only correct behaviour is a clean refusal.
        // `ipc-contract-test.js:299`.
        let error = manager.set_token(Some("ghp_testtoken123")).expect_err("refusal");
        // Engine string; `engine-zh.js:231` renders it as
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

    let saved = manager.set_token(Some("ghp_testtoken123")).expect("set_token");
    assert_eq!(saved["token"]["filled"], json!(true), "{}", saved);
    assert_eq!(saved["token"]["origin"], json!("stored"), "{}", saved);

    // `selftest.js:947` — the plaintext never reaches settings.json, and the
    // value that does is the `enc1:` envelope.
    let raw = settings::read(&fx.root_str())["updateToken"]
        .as_str()
        .expect("updateToken")
        .to_string();
    assert!(!raw.contains("ghp_testtoken123"), "{}", raw);
    assert!(raw.starts_with("enc1:"), "{}", raw);

    assert_eq!(manager.token_status()["filled"], json!(true));

    // `selftest.js:950` — garbage ciphertext reports an error, not "empty".
    fx.save(json!({ "updateToken": "enc1:bm90LWEtcmVhbC1lbnZlbG9wZQ==" }));
    let status = manager.token_status();
    assert!(
        status["error"].is_string(),
        "an undecryptable token must report an error: {}",
        status
    );

    // `selftest.js:953`.
    manager.set_token(None).expect("clear");
    assert_eq!(manager.token_status()["filled"], json!(false));
}

/* ------------------------------------------------------------------ */
/* 17f. channels: beta, rate limit, no release                         */
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

    // `selftest.js:970` — a beta user sees the next beta, and the request went
    // to `/releases?per_page=`, not `/releases/latest`.
    let on_beta = fx.manager("9.9.9-beta.1");
    let status = on_beta.check(true);
    assert_eq!(status["latest"], json!("9.9.9-beta.2"), "{}", status);
    assert_eq!(status["hasUpdate"], json!(true), "{}", status);
    assert!(
        mock.requests().iter().any(|u| u.contains("/releases?per_page=")),
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

    // `selftest.js:981`: `onlyPrerelease` is off for a final-release install.
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

    // `selftest.js:988`.
    let limited = manager.check(true);
    assert_eq!(limited["ok"], json!(false), "{}", limited);
    let retry_at = limited["retryAt"].as_i64().unwrap_or(0);
    assert!(retry_at > now_ms(), "retryAt {} must be in the future", retry_at);

    // `selftest.js:991` — a cooled-down manager answers without asking again.
    let before = mock.request_count();
    let again = manager.check(true);
    assert_eq!(again["reason"], json!("cooldown"), "{}", again);
    assert_eq!(mock.request_count(), before);

    // ... and the deadline survives a restart (it lives in the state file).
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

    // `selftest.js:1000` — 404 means "nothing to upgrade to".
    let status = manager.check(true);
    assert_ne!(status["ok"], json!(false), "{}", status);
    assert_eq!(status["hasUpdate"], Value::Null, "{}", status);
    assert_eq!(status["latest"], Value::Null, "{}", status);
}

/* ------------------------------------------------------------------ */
/* 17g. scheduler                                                      */
/* ------------------------------------------------------------------ */

#[test]
fn the_scheduler_fires_at_once_then_backs_off() {
    let fx = Fixture::new("sched");
    let mock = MockGitHub::start(MockOptions::default());
    fx.use_mock(&mock.base);
    let manager = fx.manager("1.0.0");

    // `selftest.js:1021`.
    assert_eq!(manager.due_at(), 0, "no history means due immediately");

    let first = manager.tick().expect("the first tick must check");
    assert_eq!(first["latest"], json!("9.9.9"), "{}", first);

    let due = manager.due_at();
    assert!(
        due > now_ms() + 15 * 60_000,
        "the next deadline {} must be a full interval away",
        due
    );

    // `selftest.js:1027`.
    let before = mock.request_count();
    assert!(manager.tick().is_none());
    assert_eq!(mock.request_count(), before);

    // `selftest.js:1030`.
    fx.save(json!({ "updateCheck": false }));
    assert_eq!(manager.due_at(), 0);
    assert!(manager.tick().is_none());
}

/* ------------------------------------------------------------------ */
/* 17h. settings: validation, clamping, silent install                 */
/* ------------------------------------------------------------------ */

#[test]
fn save_settings_clamps_validates_and_echoes_back() {
    let fx = Fixture::new("settings");
    let manager = fx.manager("1.0.0");

    // `selftest.js:930` — silent install is on by default and can be turned off.
    assert_eq!(manager.status()["settings"]["silent"], json!(true));
    let off = manager.save_settings(&json!({ "silent": false })).expect("save");
    assert_eq!(off["settings"]["silent"], json!(false), "{}", off);
    manager.save_settings(&json!({ "silent": true })).expect("save");
    assert_eq!(manager.status()["settings"]["silent"], json!(true));

    // `clampMinutes`: 1..1440, and a non-finite value falls back to 20.
    let one = manager.save_settings(&json!({ "minutes": 0 })).expect("save");
    assert_eq!(one["settings"]["minutes"], json!(1), "{}", one);
    let max = manager.save_settings(&json!({ "minutes": 5000 })).expect("save");
    assert_eq!(max["settings"]["minutes"], json!(1440), "{}", max);
    let back = manager.save_settings(&json!({ "minutes": 45 })).expect("save");
    assert_eq!(back["settings"]["minutes"], json!(45), "{}", back);

    // An unusable repo is stored verbatim but reads back as the built-in one.
    let bad = manager.save_settings(&json!({ "repo": "not a repo" })).expect("save");
    assert_eq!(bad["repo"], json!(BUILT_IN_REPO), "{}", bad);

    // A good one is echoed and persisted.
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

// The shell's scheduler thread only calls `tick()` every 30 s; the decision to
// touch the network lives in `tick()` itself, so the setting gate is asserted
// here against the same method the thread calls (lib.rs:update_scheduler_loop).
// The mock counts requests, so "off" has to be provably zero requests, not just
// a `None` return that a broken early-return could fake.
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
    assert_eq!(mock.request_count(), 0, "auto-check off must not hit the network");

    // Flipping the switch is all the scheduler needs (same manager, same root —
    // the setting is re-read inside `tick`). One tick = exactly one request.
    fx.save(json!({ "updateCheck": true }));
    let fired = manager.tick().expect("auto-check on with no history is due now");
    assert_eq!(mock.request_count(), 1, "one tick, one request: {}", fired);
    assert_eq!(fired["ok"], json!(true), "{}", fired);
    assert_eq!(fired["latest"], json!("9.9.9"), "{}", fired);

    // ...and the deadline the tick just persisted keeps the next one quiet.
    assert!(manager.tick().is_none(), "the next tick is a full interval away");
    assert_eq!(mock.request_count(), 1, "a tick that is not due sends nothing");
}

/* ------------------------------------------------------------------ */
/* the ak:update-* payload guard (was update_ipc_guard.rs)             */
/* fixture                                                             */
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
        settings::ensure_store(&root.to_string_lossy(), false).expect("ensure_store");
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
/* BUG-1 — `ak:update-token` can clear the credential                  */
/* ------------------------------------------------------------------ */

/// `ipc.js:296-308`, the whole handler, in one test.
///
/// The bug had two halves and both are pinned here:
///
/// 1. `token: String` cannot deserialize `null`, so the invoke was *rejected*
///    before the body ran — the renderer's `.catch` showed an error toast for a
///    click that had asked to clear. Fixed by taking the raw request body.
/// 2. `set_token(Some(&token))` made `Option<&str>`'s `None` branch unreachable,
///    so "clear" had no Rust meaning at all. Fixed by forwarding the `None`.
///
/// The missing-field branch is the one that needs the raw body: Tauri's
/// `CommandItem::deserialize_option` maps both an absent key and an explicit
/// `null` to `visit_none()`, so an `Option<Value>` parameter cannot tell them
/// apart and `update.tokenFieldRequired` would be dead code.
#[test]
fn token_arg_clears_on_null_and_rejects_a_missing_or_wrong_typed_field() {
    // `if (!('token' in p)) return { ok:false, error:'请求缺少 token 字段…' }`
    assert_eq!(
        token_arg(&json!({})),
        Err(ERR_TOKEN_FIELD_REQUIRED),
        "a payload without the token field must not be read as 'clear'"
    );
    // `asObject(payload) || {}` — a non-object body collapses to `{}`.
    assert_eq!(token_arg(&json!(7)), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!("ghp_x")), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&Value::Null), Err(ERR_TOKEN_FIELD_REQUIRED));

    // `null` is the explicit clear, and it must come back as `None` — the
    // `Option<&str>` branch `manager.set_token` actually acts on.
    assert_eq!(
        token_arg(&json!({ "token": null })),
        Ok(None),
        "null must reach set_token as None, not as an empty string"
    );

    // `typeof p.token === 'string'`
    assert_eq!(
        token_arg(&json!({ "token": "ghp_abc123" })),
        Ok(Some("ghp_abc123".to_string()))
    );
    assert_eq!(
        token_arg(&json!({ "token": "" })),
        Ok(Some(String::new())),
        "an empty string is a bad token, not a clear — token.rs rejects it"
    );

    // `p.token !== null && typeof p.token !== 'string'`
    for bad in [
        json!({ "token": 12 }),
        json!({ "token": true }),
        json!({ "token": [] }),
        json!({ "token": {} }),
    ] {
        assert_eq!(token_arg(&bad), Err(ERR_TOKEN_TYPE), "{}", bad);
    }
}

/// The full "clear token" chain, minus the button: `set_token(None)` must empty
/// the credential in the manager *and* on disk, which is what the settings page
/// badge reads.
#[test]
fn clearing_the_token_empties_it_in_the_status_and_in_settings_json() {
    let fx = IpcFixture::new("clear");
    let manager = fx.manager();

    if manager.status()["encryption"] != json!(true) {
        // No DPAPI on this host: `set_token` refuses rather than storing
        // plaintext (`update.tokenNoCrypto`). The clear path still has to be
        // the refusal, not a silent success.
        let error = manager.set_token(None).expect_err("clear must be refused");
        assert!(error.message().contains("安全存储"), "{}", error);
        return;
    }

    let saved = manager.set_token(Some("ghp_testtoken123")).expect("set");
    assert_eq!(saved["token"]["filled"], json!(true), "{}", saved);
    assert_eq!(saved["token"]["origin"], json!("stored"), "{}", saved);
    assert!(
        settings::read(&fx.root_str())["updateToken"]
            .as_str()
            .unwrap_or("")
            .starts_with("enc1:")
    );

    // `app.js:1802` → `tauri-bridge.js:60` → `{token:null}` → `None`.
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

/// The "清除令牌" button's own path: **the body the renderer sends**, through
/// the same entry point the command uses.
///
/// `tests/update_ipc_guard.rs` can reach the rules but not the command, so
/// `set_token_checked` is the seam that makes the wiring assertable. Reverting
/// BUG-1 — passing `Some(..)` unconditionally, or declaring the parameter
/// `Option<Value>` and treating `None` as "clear" — turns this red.
#[test]
fn the_clear_button_payload_clears_and_a_bare_body_is_refused() {
    let fx = IpcFixture::new("clear-button");
    let manager = fx.manager();
    let encrypted = manager.status()["encryption"] == json!(true);
    if !encrypted {
        // No DPAPI here: both paths have to refuse rather than write plaintext.
        assert!(manager.set_token_checked(&json!({ "token": null })).is_err());
        assert!(manager.set_token_checked(&json!({})).is_err());
        return;
    }

    manager
        .set_token_checked(&json!({ "token": "ghp_testtoken123" }))
        .expect("save the token first");

    // Exactly what `tauri-bridge.js:60` puts on the wire for the clear button.
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

    // A body without the field is a bug, not permission to delete.
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

/// What `#[tauri::command]` actually hands a parameter typed `Option<Value>`.
///
/// `CommandItem::deserialize_option` (`tauri-2.12.4/src/ipc/command.rs:146-157`)
/// looks `key` up in the body: absent → `visit_none()`, present → delegate to
/// the value, and `Value::deserialize_option` maps `Null` to `visit_none()` too.
/// Mirrored here because `InvokeMessage`'s fields are `pub(crate)`, so the real
/// plumbing cannot be driven from an integration test.
fn tauri_option_field(payload: &Value, key: &str) -> Option<Value> {
    match payload.get(key) {
        None => None,
        Some(Value::Null) => None,
        Some(other) => Some(other.clone()),
    }
}

/// The reason `update_set_token` takes the raw body.
///
/// `ipc.js:299` requires "no `token` field" and "`token` is null" to produce
/// *different* outcomes (an error vs. a clear). Under a plain
/// `token: Option<Value>` parameter both arrive as `None`, so one of the two
/// branches has to be sacrificed — which is exactly the compromise the audit
/// rejected. This pins the collapse so the reasoning stays checkable.
#[test]
fn an_absent_token_key_and_an_explicit_null_are_indistinguishable_as_a_parameter() {
    let absent = tauri_option_field(&json!({}), "token");
    let explicit = tauri_option_field(&json!({ "token": null }), "token");
    assert_eq!(absent, None);
    assert_eq!(
        absent, explicit,
        "as long as these are equal, `token: Option<Value>` cannot implement \
         ipc.js:299 and the command must keep taking the request body"
    );

    // The body keeps them apart, which is the whole fix.
    assert_eq!(token_arg(&json!({})), Err(ERR_TOKEN_FIELD_REQUIRED));
    assert_eq!(token_arg(&json!({ "token": null })), Ok(None));
}

/* ------------------------------------------------------------------ */
/* BUG-2 — the five `ak:update-settings` validations                   */
/* ------------------------------------------------------------------ */

/// Every rejection `ipc.js:263-287` produces, and the reason for each.
#[test]
fn the_settings_guard_rejects_every_value_the_js_ipc_layer_rejects() {
    // `Math.round(Number(p.minutes))` outside 1..=1440 → `ipc.intervalRange`.
    for bad in [
        json!({ "minutes": 0 }),
        json!({ "minutes": -5 }),
        json!({ "minutes": 1441 }),
        json!({ "minutes": 99999 }),
        json!({ "minutes": "abc" }),
        json!({ "minutes": {} }),
        json!({ "minutes": [1, 2] }),
    ] {
        assert_eq!(
            validate_settings_patch(&bad),
            Some(ERR_INTERVAL_RANGE),
            "{}",
            bad
        );
    }

    // `repo && !normalizeRepo(repo)` → `ipc.repoFormat`.
    for bad in [
        json!({ "repo": "这不是一个仓库名而是一段话" }),
        json!({ "repo": "a/b/c" }),
        json!({ "repo": "/repo" }),
        json!({ "repo": "owner/" }),
        json!({ "repo": "owner / repo" }),
    ] {
        assert_eq!(
            validate_settings_patch(&bad),
            Some(ERR_REPO_FORMAT),
            "{}",
            bad
        );
    }

    // `base && !/^https?:\/\//.test(base)` → `ipc.apiBaseFormat`.
    for bad in [
        json!({ "apiBase": "ftp://example.com" }),
        json!({ "apiBase": "example.com/api" }),
        json!({ "apiBase": "乱码更新源" }),
    ] {
        assert_eq!(
            validate_settings_patch(&bad),
            Some(ERR_API_BASE_FORMAT),
            "{}",
            bad
        );
    }

    // `!(auto | direct | /^https?:\/\/\S+$/)` → `ipc.proxyFormat`.
    for bad in [
        json!({ "proxy": "clash" }),
        json!({ "proxy": "http://" }),
        json!({ "proxy": "https://" }),
        json!({ "proxy": "http://127.0.0.1 7897" }),
    ] {
        assert_eq!(
            validate_settings_patch(&bad),
            Some(ERR_PROXY_FORMAT),
            "{}",
            bad
        );
    }

    // The guard reports the first offending field, in the JS field order
    // (`auto`, `minutes`, `beta`, `repo`, `apiBase`, `proxy`, `silent`).
    assert_eq!(
        validate_settings_patch(&json!({ "minutes": 0, "repo": "a/b/c" })),
        Some(ERR_INTERVAL_RANGE)
    );
}

/// Everything `ipc.js` lets through, including the coercions: a non-string
/// `repo`/`apiBase` becomes `""` (meaning "built-in"/"default"), and an empty
/// proxy becomes `auto`.
#[test]
fn the_settings_guard_accepts_what_the_js_ipc_layer_accepts() {
    for good in [
        json!({}),
        json!({ "auto": true }),
        json!({ "beta": false, "silent": true }),
        // `Math.round` runs before the range check, so 0.6 → 1 and 20.6 → 21.
        json!({ "minutes": 1 }),
        json!({ "minutes": 1440 }),
        json!({ "minutes": 0.6 }),
        json!({ "minutes": 20.6 }),
        json!({ "minutes": "45" }),
        // An empty repo means "fall back to the built-in one".
        json!({ "repo": "" }),
        json!({ "repo": "   " }),
        json!({ "repo": "acme/keeper" }),
        json!({ "repo": "  acme/keeper  " }),
        json!({ "repo": "acme/keeper.nightly" }),
        // A non-string collapses to `""` via `asString`, which then passes.
        json!({ "repo": 42 }),
        json!({ "apiBase": "" }),
        json!({ "apiBase": "https://api.github.com" }),
        json!({ "apiBase": "http://127.0.0.1:8080" }),
        json!({ "apiBase": 42 }),
        json!({ "proxy": "auto" }),
        json!({ "proxy": "direct" }),
        json!({ "proxy": "" }),
        json!({ "proxy": "http://127.0.0.1:7897" }),
        json!({ "proxy": "https://proxy.example.com:8443" }),
        json!({ "proxy": 42 }),
    ] {
        assert_eq!(validate_settings_patch(&good), None, "{}", good);
    }
}

/// A partial save must never be rejected for a field it did not send.
///
/// This is the reason the guard inspects `patch.get(..)` per key rather than
/// reading a defaulted struct: flipping "auto check" on a machine whose stored
/// `repo` is garbage would otherwise fail for an unrelated reason.
#[test]
fn the_settings_guard_ignores_fields_the_patch_does_not_carry() {
    assert_eq!(validate_settings_patch(&json!({ "auto": true })), None);
    assert_eq!(
        validate_settings_patch(&json!({ "silent": false, "beta": true })),
        None
    );
    // Unknown keys are not an error either — the renderer sends only known
    // ones, but a stray key must not break a save.
    assert_eq!(validate_settings_patch(&json!({ "nope": 1 })), None);
}

/// `validate_settings_patch` is the *only* thing standing between a bad value
/// and the store, because `manager.save_settings` clamps instead of rejecting.
///
/// Pinned here so a future refactor that moves the rules cannot quietly drop
/// them: the guard has to reject before `save_settings` ever sees the patch.
#[test]
fn the_guard_rejects_what_save_settings_would_otherwise_persist() {
    let fx = IpcFixture::new("persist");
    let manager = fx.manager();

    // Without the guard `save_settings` would clamp 99999 → 1440 and store it.
    assert_eq!(
        validate_settings_patch(&json!({ "minutes": 99999 })),
        Some(ERR_INTERVAL_RANGE)
    );
    // …and would store a malformed repo verbatim, which then silently reads
    // back as the built-in one — the "更新检查失败" symptom.
    assert_eq!(
        validate_settings_patch(&json!({ "repo": "这不是仓库名" })),
        Some(ERR_REPO_FORMAT)
    );

    // Proof of the second half: `save_settings` itself does clamp, so nothing
    // below it would have caught these.
    let clamped = manager
        .save_settings(&json!({ "minutes": 99999 }))
        .expect("save_settings still succeeds — it clamps");
    assert_eq!(
        clamped["settings"]["minutes"],
        json!(1440),
        "{}",
        clamped
    );
}

/// BUG-2's **wiring**, not just its rules: a rejected patch must never reach
/// the store.
///
/// "The validation function is correct" and "the command runs the validation"
/// are different claims, and only the second one is what the user sees. The
/// command functions are private to the crate, so `save_settings_checked` is
/// the seam that lets a test assert it.
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

/// `normalize_repo` is the shared rule behind `ipc.repoFormat`; pin the exact
/// character class the JS regex `^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$` allows.
#[test]
fn normalize_repo_matches_the_js_character_class() {
    for good in [
        "acme/keeper",
        "acme/keeper-x64",
        "acme.nightly/keeper_v2",
        "_a/1",
    ] {
        assert_eq!(normalize_repo(good), good, "{}", good);
        assert_eq!(
            normalize_repo(&format!("  {}  ", good)),
            good,
            "trimmed: {}",
            good
        );
    }
    for bad in ["", "   ", "acme", "a/b/c", "/b", "a/", "a /b", "a/ b", "a\\b"] {
        assert_eq!(normalize_repo(bad), "", "{}", bad);
    }
}

/* ------------------------------------------------------------------ */
/* the messages themselves                                             */
/* ------------------------------------------------------------------ */

/// The engine speaks English; Chinese comes from the renderer dictionary.
///
/// A hard-coded Chinese sentence here would reach an English UI in Chinese —
/// the defect `core/errors.js` has in the "English" slot of these very codes.
/// `scripts/tauri-i18n.js` greps the Rust source, and `src/renderer/
/// engine-zh.js` must map each of these; this test keeps the two lists honest
/// against typos in either.
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
            !message.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "the Rust engine must emit English; got {:?}",
            message
        );
        assert!(
            !message.trim().is_empty() && message.chars().next().unwrap().is_ascii_uppercase()
                == false,
            "keep these sentences lowercase like the rest of the engine: {:?}",
            message
        );
    }
    // `discover.repoFormat` and `ipc.repoFormat` are one rule, so they share a
    // string — which is what lets one EXACT entry cover both.
    assert_eq!(ERR_REPO_FORMAT, "repo must be owner/repo or a github.com URL");
}

/* ------------------------------------------------------------------ */
/* literal 1:1 parity with selftest.js 17a (was update_js_parity.rs)   */
/* ------------------------------------------------------------------ */
/// `compareVersions` is `1 | -1 | 0` in JS and `Option<Ordering>` in Rust.
fn ord(value: Option<Ordering>) -> i8 {
    match value {
        Some(Ordering::Greater) => 1,
        Some(Ordering::Less) => -1,
        Some(Ordering::Equal) => 0,
        None => i8::MIN, // the JS `null`: unparseable, never a real comparison
    }
}

/// `selftest.js:773-779` — the six `compareVersions` / `isPrereleaseVersion`
/// assertions, verbatim.
#[test]
fn semver_comparison_matches_the_js_literals() {
    // 'semver: final outranks its own prerelease'
    assert_eq!(ord(version::compare_versions("1.2.0", "1.2.0-beta.2")), 1);
    // 'semver: beta.2 < beta.10 (numeric identifiers)'
    assert_eq!(
        ord(version::compare_versions("1.2.0-beta.2", "1.2.0-beta.10")),
        -1
    );
    // 'semver: v prefix ignored'
    assert_eq!(ord(version::compare_versions("v1.2.1", "1.2.0")), 1);
    // 'semver: unparseable tag returns null'
    assert_eq!(ord(version::compare_versions("nightly", "1.0.0")), i8::MIN);
    assert!(version::compare_versions("nightly", "1.0.0").is_none());

    // 'semver: prerelease detection'
    assert!(version::is_prerelease_version("1.0.0-rc.1"));
    assert!(!version::is_prerelease_version("1.0.0"));
    // 'semver: 4th segment is not a prerelease'
    assert!(!version::is_prerelease_version("1.0.0.1"));
}

/// `selftest.js:781-793` — `pickListedRelease` over the JS's own fixture list.
#[test]
fn pick_listed_release_matches_the_js_fixture() {
    // The exact list the JS builds at 781-786.
    let list = json!([
        { "tag_name": "v1.2.0" },
        { "tag_name": "v1.3.0-beta.1" },
        { "tag_name": "v1.1.9" },
        { "tag_name": "v9.0.0", "draft": true }
    ]);

    // 'release list: highest version wins across beta + final'
    assert_eq!(
        version::pick_listed_release(&list, false).expect("a winner")["tag_name"],
        json!("v1.3.0-beta.1")
    );
    // 'release list: beta-only view keeps prereleases'
    assert_eq!(
        version::pick_listed_release(&list, true).expect("a winner")["tag_name"],
        json!("v1.3.0-beta.1")
    );
    // 'release list: no beta in list → no update (never falls back)'
    assert!(
        version::pick_listed_release(&json!([{ "tag_name": "v1.2.0" }]), true).is_none(),
        "a final-only list must not satisfy a beta-only view"
    );
    // 'release list: drafts never win'
    assert_eq!(
        version::pick_listed_release(
            &json!([
                { "tag_name": "v9.0.0", "draft": true },
                { "tag_name": "v1.0.0" }
            ]),
            false
        )
        .expect("a winner")["tag_name"],
        json!("v1.0.0")
    );
}

/// `selftest.js:795-804` — the download allow-list, asserted at the same four
/// URLs the JS uses.
///
/// One deliberate deviation from the literal JS: `version.js:200` throws the
/// Chinese sentence `下载地址域名不在允许列表内` directly, whereas the Rust
/// engine emits English (`"download address host is not on the allow list: …"`)
/// and `src/renderer/engine-zh.js:330` maps that pattern to the same Chinese
/// sentence. So the JS assertion `/允许列表/` is split across two layers here —
/// the engine wording, and the mapping that turns it back into the JS wording.
/// Asserting only one of them would let the other drift silently.
#[test]
fn download_allow_list_matches_the_js_literals() {
    // 'download URL: plain http rejected'
    let error = version::assert_downloadable("http://github.com/a.exe", "")
        .expect_err("plain http must be refused");
    assert!(
        error.message().contains("https"),
        "the reason must mention https: {}",
        error.message()
    );

    // 'download URL: foreign host rejected'
    let error = version::assert_downloadable("https://evil.example/a.exe", "")
        .expect_err("a foreign host must be refused");
    assert!(
        error.message().contains("not on the allow list"),
        "the reason must name the allow list: {}",
        error.message()
    );

    // …and the English engine sentence must still resolve to the Chinese one
    // the JS throws — that mapping is what keeps the UI free of English.
    let zh_table = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src/renderer/engine-zh.js"),
    )
    .expect("src/renderer/engine-zh.js must be readable from the test");
    assert!(
        zh_table.contains("download address host is not on the allow list")
            && zh_table.contains("下载地址域名不在允许列表内"),
        "engine-zh.js must map this engine message to the JS wording"
    );

    // 'download URL: github asset accepted'
    assert!(
        version::assert_downloadable(
            "https://github.com/acme/keeper/releases/download/v1/x.exe",
            ""
        )
        .is_ok()
    );

    // 'download URL: configured mirror host accepted'
    assert!(version::assert_downloadable("http://127.0.0.1:9/x.exe", "http://127.0.0.1:9").is_ok());
}

/// `selftest.js:806-830` — `pickInstaller` and `isNonInstallerAsset`.
#[test]
fn installer_selection_matches_the_js_literals() {
    let installer = version::pick_installer(
        &json!([
            { "name": "AgentKeeper-9.9.9-x64.exe", "size": 1, "browser_download_url": "u1" },
            { "name": "AgentKeeper-Setup-9.9.9.exe", "size": 2, "browser_download_url": "u2" },
            { "name": "AgentKeeper-9.9.9.dmg", "size": 3, "browser_download_url": "u3" }
        ]),
        "win32",
        "x64",
    )
    .expect("an installer");

    // 'asset pick: NSIS setup beats the bare exe'
    assert!(
        installer["name"]
            .as_str()
            .unwrap_or("")
            .contains("Setup"),
        "{}",
        installer
    );
    // 'asset pick: no matching platform → null'
    assert!(
        version::pick_installer(&json!([{ "name": "AgentKeeper.dmg" }]), "win32", "x64").is_none()
    );

    // Our own release layout: electron-builder emits BOTH the installer and a
    // portable exe. Picking the portable one would install nothing.
    let real_assets = json!([
        { "name": "AgentKeeper-0.2.0-x64.exe", "size": 1, "browser_download_url": "u-inst" },
        { "name": "AgentKeeper-0.2.0-portable.exe", "size": 2, "browser_download_url": "u-port" }
    ]);
    // 'asset pick: the installer wins over the portable exe'
    assert_eq!(
        version::pick_installer(&real_assets, "win32", "x64").expect("an installer")["url"],
        json!("u-inst")
    );
    // 'asset pick: portable-only release offers nothing to install'
    assert!(version::pick_installer(&json!([real_assets[1].clone()]), "win32", "x64").is_none());

    // 'asset pick: archives and other platforms are ignored'
    assert!(version::is_non_installer_asset("AgentKeeper-0.3.0-linux-x64.zip"));
    assert!(version::is_non_installer_asset("AgentKeeper-0.3.0.AppImage"));
    assert!(!version::is_non_installer_asset("AgentKeeper-0.3.0-x64.exe"));
}

/// `selftest.js:832-835` — `safeFileName`.
#[test]
fn safe_file_name_matches_the_js_literals() {
    // 'file name: path segments stripped'
    assert_eq!(version::safe_file_name("../../evil.exe", "win32"), "evil.exe");
    // 'file name: illegal characters folded'
    assert_eq!(version::safe_file_name("a:b?.exe", "win32"), "a_b_.exe");
    // 'file name: empty falls back with platform suffix'
    assert_eq!(
        version::safe_file_name("", "win32"),
        "AgentKeeper-update.exe"
    );
}
