//! Minimal blocking HTTP for the two places that need one: registry search
//! (`core/discover.js`) and the updater (`core/update/http.js`).
//!
//! Kept in one file so there is a single timeout policy, a single user-agent,
//! and a single place to change if the transport must move (a proxy setting,
//! or reusing WebView2's network stack). `core/` must stay free of async
//! runtimes, so this is synchronous.
//!
//! ## Egress policy
//!
//! `resolve_routes` reproduces `core/update/http.js:56-69`: `auto` tries direct
//! then the local Clash mixed port, `direct` is one route, and a pinned proxy
//! URL is exactly one route (a pinned route must not silently fall back).
//! `https://` proxy URLs and percent-encoded proxy credentials are not
//! supported by `ureq`; those settings degrade to `auto`, as the JS does for an
//! unparseable value. `TODO(stage6)` covers both gaps.

use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub enum FetchError {
    /// Non-2xx response.
    Http(u16),
    /// The deadline elapsed before the response arrived.
    Timeout,
    /// DNS failure, refused connection, TLS error, malformed body, ...
    Transport(String),
}

impl FetchError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, FetchError::Timeout)
    }

    /// JS `searchRegistry` maps the abort to `'registry timeout'` and any
    /// other failure to `` `registry unreachable: ${e.message}` ``. Message
    /// text differs between runtimes by nature; the *classification* is what
    /// the UI branches on, and that is preserved.
    pub fn describe(&self) -> String {
        match self {
            FetchError::Http(code) => format!("http {}", code),
            FetchError::Timeout => "timeout".to_string(),
            FetchError::Transport(msg) => msg.clone(),
        }
    }
}

/// `ureq` has no public timeout predicate, so this walks the error chain for
/// the `io::ErrorKind` a socket deadline produces, and only then falls back to
/// the message text.
///
/// The text check alone is locale-dependent: on a non-English Windows a read
/// timeout arrives as a localized OS message ending in `(os error 10060)` — no
/// "timed out", no "timeout" — so the registry query reported *registry
/// unreachable: …* instead of *registry timeout*. Found by
/// `tests/boundary.rs::a_silent_peer_times_out_the_registry_query`; the kind
/// check is what makes the classification independent of the display language.
pub(crate) fn is_timeout_error(error: &ureq::Error) -> bool {
    let ureq::Error::Transport(transport) = error else {
        return false;
    };
    let mut source = std::error::Error::source(transport);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
        source = err.source();
    }
    let lower = transport.to_string().to_lowercase();
    lower.contains("timed out") || lower.contains("timeout")
}

fn is_timeout(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Status(_, _) => false,
        ureq::Error::Transport(_) => is_timeout_error(e),
    }
}

/// GET a JSON document with a hard deadline covering connect, TLS and body.
pub fn get_json(url: &str, timeout_ms: u64) -> Result<Value, FetchError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(timeout_ms.max(1)))
        // Deliberately no redirect following: an allow-list check on the
        // *original* host is worthless if the client silently follows a 302
        // somewhere else. A 3xx surfaces as `FetchError::Http(3xx)`.
        .redirects(0)
        .user_agent(concat!("AgentKeeper/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(url)
        .set("accept", "application/json")
        .call()
        .map_err(|e| {
            if is_timeout(&e) {
                FetchError::Timeout
            } else {
                match e {
                    ureq::Error::Status(code, _) => FetchError::Http(code),
                    other => FetchError::Transport(other.to_string()),
                }
            }
        })?;

    let text = resp
        .into_string()
        .map_err(|e| FetchError::Transport(e.to_string()))?;
    serde_json::from_str::<Value>(&text)
        .map_err(|e| FetchError::Transport(format!("bad json: {}", e)))
}

/* ------------------------------------------------------------------ */
/* shared request plumbing                                             */
/* ------------------------------------------------------------------ */

/// The UA the updater sends (`USER_AGENT` in `core/update/manager.js:49`). Not
/// the versioned one above: GitHub sees the same string from both builds.
pub(crate) const UPDATER_USER_AGENT: &str = "AgentKeeper-updater";

/// `depth < 5` in `core/update/http.js:198`: five redirects are followed, a
/// sixth response is returned to the caller with its 3xx status.
const MAX_REDIRECTS: u32 = 5;

fn is_redirect(status: u16) -> bool {
    // `REDIRECTS` in `core/update/http.js:187`.
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn parse_content_length(raw: Option<&str>) -> Option<u64> {
    // `Number(headers['content-length']) || 0` (manager.js:482): an unparseable
    // or empty header behaves like 0, i.e. "no declared length".
    raw.and_then(|value| value.trim().parse::<u64>().ok())
}

/// Resolve a `Location` header against the request URL.
///
/// The JS uses `new URL(location, url).href` (`core/update/http.js:200`). This
/// covers absolute URLs, scheme-relative URLs, absolute paths and path-relative
/// URLs — everything GitHub and its CDN send. Dot segments are *not* folded and
/// there is no percent-encoding normalization.
fn resolve_redirect(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if location.starts_with("//") {
        let scheme = base.split(':').next()?;
        return Some(format!("{}:{}", scheme, location));
    }
    // "https:", "http:", or any other scheme: already absolute.
    if let Some(colon) = location.find(':') {
        let scheme = &location[..colon];
        let is_scheme = !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
        if is_scheme {
            return Some(location.to_string());
        }
    }

    let (scheme, rest) = base.split_once("://")?;
    let authority_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path_and_more = &rest[authority_end..];

    if location.starts_with('/') {
        return Some(format!("{}://{}{}", scheme, authority, location));
    }
    if location.starts_with('?') {
        let path = path_and_more.split(['?', '#']).next().unwrap_or("/");
        return Some(format!("{}://{}{}{}", scheme, authority, path, location));
    }
    if location.starts_with('#') {
        return Some(format!(
            "{}://{}{}{}",
            scheme, authority, path_and_more, location
        ));
    }

    let path = path_and_more.split(['?', '#']).next().unwrap_or("/");
    let dir = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    Some(format!("{}://{}{}{}", scheme, authority, dir, location))
}

/// `describeError` (`core/update/http.js:80`) for a ureq transport failure.
/// Message text is the contract here: the settings panel renders it verbatim.
pub(crate) fn describe_ureq_error(error: &ureq::Error) -> String {
    let ureq::Error::Transport(transport) = error else {
        return error.to_string();
    };
    let message = transport.to_string();
    let lower = message.to_lowercase();

    if matches!(transport.kind(), ureq::ErrorKind::Dns)
        || lower.contains("dns")
        || lower.contains("no such host")
        || lower.contains("name or service not known")
    {
        return "DNS lookup failed".to_string();
    }
    if is_timeout_error(error) {
        return "connection timed out".to_string();
    }
    if lower.contains("refused") {
        return "connection refused (is the proxy running?)".to_string();
    }
    if lower.contains("reset by peer") || lower.contains("connection reset") {
        return "connection was reset".to_string();
    }
    if lower.contains("certificate")
        || lower.contains("tls")
        || lower.contains("handshake")
        || lower.contains("ssl")
    {
        // JS renders Node's code (`TLS 失败（EPROTO）`); ureq has no code, so
        // the detail goes inside the parentheses instead.
        return format!("TLS failed ({})", message);
    }
    message
}

fn describe_io_error(error: &std::io::Error) -> String {
    // Same phrases as `describeError`; the UI renders them verbatim.
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "connection timed out".to_string(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            "connection was reset".to_string()
        }
        std::io::ErrorKind::ConnectionRefused => "connection refused (is the proxy running?)".to_string(),
        _ => {
            let text = error.to_string();
            if text.to_lowercase().contains("timed out") {
                "connection timed out".to_string()
            } else {
                text
            }
        }
    }
}

/// One egress candidate (`resolveCandidates` in `core/update/http.js:56`).
pub(crate) struct Route {
    /// Shown in the attempt log, e.g. `直连` or `127.0.0.1:7897`. Unused until
    /// the manager's `onRouteFailure` logging lands (it has no logger hook in
    /// the stage-6 contract).
    #[allow(dead_code)]
    pub label: String,
    pub proxy: Option<ureq::Proxy>,
}

/// `CLASH_MIXED` (`core/update/http.js:29`): the default local mixed port.
const CLASH_HOST: &str = "127.0.0.1";
const CLASH_PORT: u16 = 7897;

fn direct_route() -> Route {
    Route {
        label: "direct connection".to_string(),
        proxy: None,
    }
}

fn clash_route() -> Route {
    Route {
        label: format!("{}:{}", CLASH_HOST, CLASH_PORT),
        proxy: ureq::Proxy::new(format!("http://{}:{}", CLASH_HOST, CLASH_PORT)).ok(),
    }
}

/// `parseProxy` (`core/update/http.js:35`). `None` means "not a usable proxy
/// setting", which the caller turns into the default route.
fn parse_proxy(value: &str) -> Option<Route> {
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    // TODO(stage6): ureq can speak an HTTP proxy (CONNECT) but not TLS *to the
    // proxy*, so an `https://` proxy falls into the same "unparseable" bucket
    // as the JS `catch` branch and degrades to auto.
    if lower.starts_with("https://") {
        return None;
    }
    // TODO(stage6): the JS percent-decodes proxy credentials
    // (`decodeURIComponent`); ureq takes them verbatim.
    let proxy = ureq::Proxy::new(value.trim_end_matches('/')).ok()?;
    Some(Route {
        label: proxy_label(value),
        proxy: Some(proxy),
    })
}

/// `${hostname}:${port}` from the proxy URL, defaulting the port like
/// `parseProxy` does (`core/update/http.js:44`).
fn proxy_label(value: &str) -> String {
    let rest = value.split_once("://").map(|(_, tail)| tail).unwrap_or(value);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, tail)| tail)
        .unwrap_or(authority);
    if host_port.contains(':') {
        host_port.to_string()
    } else if value.to_ascii_lowercase().starts_with("https://") {
        format!("{}:443", host_port)
    } else {
        format!("{}:80", host_port)
    }
}

/// Ordered candidates for a settings value: `auto` is [direct, clash], a pinned
/// route is exactly one entry, and an unusable value degrades to `auto`
/// (`core/update/http.js:56-69`).
fn resolve_routes(setting: &str) -> Vec<Route> {
    let value = setting.trim();
    if value.is_empty() || value == "auto" {
        return vec![direct_route(), clash_route()];
    }
    if value == "direct" {
        return vec![direct_route()];
    }
    if let Some(route) = parse_proxy(value) {
        return vec![route];
    }
    vec![direct_route(), clash_route()]
}

/// One GET with `Location` following, through one route.
///
/// A non-2xx status is a *response*: `Err` is reserved for transport problems
/// (and an unparseable `Location`), which is what `requestWithEgress` treats as
/// a route failure in the JS. Returns the error text already run through
/// [`describe_ureq_error`].
fn request_with_route(
    url: &str,
    route: &Route,
    timeout_ms: u64,
    user_agent: &str,
    headers: &[(&str, String)],
) -> Result<ureq::Response, String> {
    // JS `req.setTimeout(ms)` is a *socket idle* timeout, not a deadline for
    // the whole transfer: a 600 MB installer must not be aborted at 60 s.
    // ureq exposes the phases separately, so no total `.timeout()` here.
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .redirects(0)
        .user_agent(user_agent);
    if let Some(proxy) = &route.proxy {
        builder = builder.proxy(proxy.clone());
    }
    let agent = builder.build();

    let mut current = url.to_string();
    let mut hops = 0u32;
    loop {
        let mut request = agent.get(&current);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        // ureq reports >=400 as `Err(Status(_, response))`; recover the
        // response so the status is handled like any other answer.
        let response = match request.call() {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(other) => return Err(describe_ureq_error(&other)),
        };

        let status = response.status();
        if !is_redirect(status) || hops >= MAX_REDIRECTS {
            return Ok(response);
        }
        let Some(location) = response.header("location") else {
            return Ok(response);
        };
        // JS quirk (security-relevant): `requestWithRoute` follows the hop and
        // the caller only ever re-validates the *original* URL through
        // `assertDownloadable` (manager.js:426). A redirect target is therefore
        // not host-checked in the JS. Reproduced as-is; see the report.
        current = resolve_redirect(&current, location)
            .ok_or_else(|| format!("cannot resolve the redirect target: {}", location))?;
        hops += 1;
    }
}

/// One GET on the direct route. Used by the update API, whose contract has no
/// proxy setting.
pub(crate) fn request_with_redirects(
    url: &str,
    timeout_ms: u64,
    user_agent: &str,
    headers: &[(&str, String)],
) -> Result<ureq::Response, String> {
    request_with_route(url, &direct_route(), timeout_ms, user_agent, headers)
}


/* ------------------------------------------------------------------ */
/* streaming download                                                  */
/* ------------------------------------------------------------------ */

/// Streaming GET straight to `dest`, with the updater's transport policy.
///
/// `proxy` is the raw `updateProxy` setting (`"auto"` / `"direct"` / a URL),
/// matching `requestWithEgress`: transport failures before a response move to
/// the next candidate, anything that produced a response does not.
///
/// * `on_progress(0, Some(content_length))` fires once after the header checks
///   and before the file is created (`updateProgress(0, declared)`), then after
///   every successful read with the running byte count. `received` counts bytes
///   handed to the file, not chunks; the second element is `None` when
///   `Content-Length` is absent.
/// * Returning `false` from `on_progress` aborts (cancellation and the size
///   ceiling both arrive this way from the manager). The partial file is left
///   on disk: `runDownload`'s `catch` decides what to delete, and its cancel
///   path deliberately keeps it.
/// * `max_bytes` is also enforced here, exactly as `manager.js:482-500` does:
///   the declared `Content-Length` is rejected before any byte is written
///   (`` 安装包体积异常（N MB），已中止 ``) and the stream is cut off on the
///   first byte past the limit (`` 安装包超过体积上限，已中止 ``).
/// * Redirects are followed for up to five hops, like the JS.
/// * The status must be exactly 200 (`manager.js:479`), not merely 2xx.
///
/// Transport failures are reported as `` 无法连接服务器：… ``, matching
/// `http.js:225`; a failure after the response arrived is `` 下载中断: … ``.
pub fn download_to_file(
    url: &str,
    dest: &Path,
    proxy: &str,
    max_bytes: u64,
    timeout_ms: u64,
    on_progress: &mut dyn FnMut(u64, Option<u64>) -> bool,
) -> Result<u64, FetchError> {
    let headers = [
        ("accept", "application/octet-stream".to_string()),
        // Node's `http` sends no Accept-Encoding, so the installer arrives as
        // stored and `received` counts exactly what `Content-Length` declares.
        // ureq would otherwise advertise gzip and decompress transparently,
        // which would break both size checks.
        ("accept-encoding", "identity".to_string()),
    ];
    let routes = resolve_routes(proxy);
    let mut last_error = None;
    let mut response = None;
    for route in &routes {
        match request_with_route(url, route, timeout_ms, UPDATER_USER_AGENT, &headers) {
            Ok(found) => {
                response = Some(found);
                break;
            }
            Err(message) => last_error = Some(message),
        }
    }
    let Some(response) = response else {
        return Err(FetchError::Transport(format!(
            "cannot reach the server: {}",
            last_error.unwrap_or_else(|| "unknown error".to_string())
        )));
    };

    let status = response.status();
    if status != 200 {
        // `response.status !== 200` (manager.js:479) — not "non-2xx".
        return Err(FetchError::Http(status));
    }

    let declared = parse_content_length(response.header("content-length"));
    if let Some(length) = declared {
        if length > max_bytes {
            return Err(FetchError::Transport(format!(
                "installer size looks wrong ({} MB); download aborted",
                (length as f64 / 1_048_576.0).round()
            )));
        }
    }

    if !on_progress(0, declared) {
        return Err(FetchError::Transport("download was aborted".to_string()));
    }

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;
        }
    }
    let mut file = std::fs::File::create(dest)
        .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;

    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut received: u64 = 0;
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(FetchError::Transport(format!(
                    "download interrupted: {}",
                    describe_io_error(&e)
                )))
            }
        };
        received += read as u64;
        if received > max_bytes {
            return Err(FetchError::Transport(
                "installer exceeds the size cap; download aborted".to_string(),
            ));
        }
        // Progress is reported before the bytes reach the file, matching the
        // `updateProgress(...)` call that precedes `file.write(chunk)`.
        if !on_progress(received, declared) {
            return Err(FetchError::Transport("download was aborted".to_string()));
        }
        file.write_all(&buffer[..read])
            .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;
    }

    file.flush()
        .map_err(|e| FetchError::Transport(format!("failed to write the installer: {}", e)))?;
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classification_is_stable() {
        assert!(FetchError::Timeout.is_timeout());
        assert!(!FetchError::Http(404).is_timeout());
        assert_eq!(FetchError::Http(404).describe(), "http 404");
    }

    #[test]
    fn refused_connection_is_transport_not_http() {
        // Port 1 on loopback: connection refused, no DNS involved.
        match get_json("http://127.0.0.1:1/", 1000) {
            Err(FetchError::Transport(_)) => {}
            other => panic!("expected transport error, got {:?}", other),
        }
    }

    /* ---- download_to_file ------------------------------------------- */

    use std::net::TcpListener;

    /// Serve `responses` in order, one per accepted connection, on a loopback
    /// port. Never joined: the thread stops after the responses or 10 s.
    ///
    /// Two details are load-bearing on Windows, both of which used to make
    /// these tests intermittently fail (roughly 1 run in 5 under a full
    /// parallel `cargo test`, never in isolation):
    ///
    /// * **Read the whole request first.** ureq sends the request line and the
    ///   headers in whatever segmentation the socket gives it, and the client
    ///   keeps writing after the server replies. Closing a socket that still
    ///   has unread inbound bytes makes Windows send RST instead of FIN, which
    ///   discards the response the client has not read yet — surfacing as
    ///   `os error 10053` / "Network Error" rather than as a wrong response.
    /// * **Half-close, then drain.** `shutdown(Write)` emits FIN so a client
    ///   reading to EOF (the no-`Content-Length` test) sees a clean end,
    ///   and the subsequent drain absorbs whatever the client is still
    ///   sending before the socket is dropped.
    fn serve(responses: Vec<String>) -> String {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut index = 0;
            while index < responses.len() && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // On Windows an accepted socket INHERITS the listener's
                        // nonblocking mode, and a nonblocking socket ignores
                        // SO_RCVTIMEO: `read` returns WouldBlock at once. Under a
                        // loaded `cargo test` the client is descheduled between
                        // connect() and write(), so the stub saw WouldBlock,
                        // broke out of the read loop with the request still
                        // unwritten, and closed. The client's bytes then landed
                        // on a socket being torn down, Windows answered with RST
                        // instead of FIN, and the client's status-line read died
                        // as `os error 10053` — the intermittent failure these
                        // tests used to show only in the full suite, never alone.
                        // Put the socket back to blocking so the timeout below
                        // genuinely waits.
                        stream.set_nonblocking(false).ok();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .ok();
                        // Read until the request head is complete. A single
                        // `read` can return only the request line when the
                        // client segments its write; replying then leaves the
                        // rest of the head inbound, and closing a Windows
                        // socket with unread inbound bytes sends RST, which
                        // discards the reply the client has not read yet.
                        // That is the `os error 10053` / "Error encountered in
                        // the status line" seen intermittently under a parallel
                        // `cargo test`.
                        //
                        // A timeout or a closed peer breaks the loop and the
                        // reply is still written: this is a stub server, and a
                        // truncated GET head does not change what it answers.
                        let mut request = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => request.extend_from_slice(&buf[..n]),
                                Err(ref e)
                                    if e.kind() == std::io::ErrorKind::WouldBlock
                                        || e.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    break
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = stream.write_all(responses[index].as_bytes());
                        let _ = stream.flush();
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                        // Absorb whatever the client is still sending before the
                        // socket is dropped, so the close is a FIN and not a RST.
                        // Bounded to one short pass: the client has already had
                        // its reply, so anything left is its own close dance.
                        stream
                            .set_read_timeout(Some(Duration::from_millis(50)))
                            .ok();
                        let mut sink = [0u8; 1024];
                        let _ = stream.read(&mut sink);
                        index += 1;
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        format!("http://{}", addr)
    }

    fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
        let mut out = format!("HTTP/1.1 {} X\r\n", status);
        for (name, value) in headers {
            out.push_str(&format!("{}: {}\r\n", name, value));
        }
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("Connection: close\r\n\r\n");
        out.push_str(body);
        out
    }

    /// No `Content-Length`: the body ends at EOF (`Connection: close`).
    fn http_response_streamed(status: u16, body: &str) -> String {
        format!("HTTP/1.1 {} X\r\nConnection: close\r\n\r\n{}", status, body)
    }

    /// A throwaway destination whose parent directory belongs to this call only.
    ///
    /// The stamp alone was not enough: the six download tests start on the same
    /// tick, and two `SystemTime::now()` calls inside one process can resolve to
    /// the same 100 ns tick on Windows, which hands two tests the same parent —
    /// then one test's `remove_dir_all(parent)` cleanup deletes the other's
    /// destination between `create_dir_all` and `File::create`, surfacing as
    /// `os error 3` ("the system cannot find the path specified"). Seen once in
    /// 20 concurrent full-suite runs, never alone. The counter removes the
    /// collision instead of widening the timestamp.
    fn temp_dest(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir()
            .join(format!("ak-http-test-{}-{}-{}", std::process::id(), stamp, seq))
            .join(name)
    }

    #[test]
    fn download_writes_bytes_and_reports_progress() {
        let url = serve(vec![http_response(
            200,
            &[("Content-Type", "application/octet-stream")],
            "hello installer",
        )]);
        let dest = temp_dest("a.exe");
        let mut events: Vec<(u64, Option<u64>)> = Vec::new();

        let written = download_to_file(&url, &dest, "direct", 1024, 5000, &mut |received, total| {
            events.push((received, total));
            true
        })
        .unwrap();

        assert_eq!(written, 15);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello installer");
        // First event is the pre-write `updateProgress(0, declared)`; the last
        // one carries the full size.
        assert_eq!(events.first(), Some(&(0, Some(15))));
        assert_eq!(events.last(), Some(&(15, Some(15))));
        let mut previous = 0;
        for (received, total) in &events {
            assert_eq!(*total, Some(15));
            assert!(*received >= previous);
            previous = *received;
        }
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_rejects_declared_oversize_before_writing() {
        let url = serve(vec![http_response(200, &[], "0123456789")]);
        let dest = temp_dest("declared.exe");

        let error = download_to_file(&url, &dest, "direct", 5, 5000, &mut |_, _| true).unwrap_err();
        match error {
            // manager.js:484 — `Math.round(bytes / 1048576)`.
            FetchError::Transport(message) => {
                assert!(message.contains("size looks wrong"), "{}", message);
                assert!(message.contains("0 MB"), "{}", message);
            }
            other => panic!("expected transport error, got {:?}", other),
        }
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_cuts_the_stream_off_without_content_length() {
        let url = serve(vec![http_response_streamed(200, "0123456789")]);
        let dest = temp_dest("streamed.exe");

        let error = download_to_file(&url, &dest, "direct", 5, 5000, &mut |_, _| true).unwrap_err();
        match error {
            FetchError::Transport(message) => {
                assert!(message.contains("exceeds the size cap"), "{}", message)
            }
            other => panic!("expected transport error, got {:?}", other),
        }
        // The transport does not delete: `runDownload`'s catch decides, and its
        // cancel path deliberately keeps the partial file.
        assert!(dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn progress_returning_false_aborts_before_the_file_is_created() {
        let url = serve(vec![http_response(200, &[], "payload")]);
        let dest = temp_dest("cancelled.exe");

        let error = download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| false).unwrap_err();
        assert!(matches!(error, FetchError::Transport(_)));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn routes_follow_the_js_policy() {
        assert_eq!(resolve_routes("auto").len(), 2);
        assert_eq!(resolve_routes("").len(), 2);
        assert_eq!(resolve_routes("direct").len(), 1);
        assert_eq!(resolve_routes("direct")[0].label, "direct connection");
        assert!(resolve_routes("direct")[0].proxy.is_none());

        let pinned = resolve_routes("http://127.0.0.1:8888");
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].label, "127.0.0.1:8888");
        assert!(pinned[0].proxy.is_some());

        // An unusable value degrades to auto rather than bricking updates.
        assert_eq!(resolve_routes("not a url").len(), 2);
        // Documented limitation: ureq cannot do TLS-to-proxy.
        assert_eq!(resolve_routes("https://proxy.local:8443").len(), 2);
    }

    #[test]
    fn download_follows_a_relative_redirect() {
        let url = serve(vec![
            http_response(302, &[("Location", "/installer")], ""),
            http_response(200, &[], "payload"),
        ]);
        let dest = temp_dest("redirected.exe");

        let written = download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| true).unwrap();
        assert_eq!(written, 7);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "payload");
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn download_surfaces_non_200_as_http() {
        let url = serve(vec![http_response(404, &[], "no")]);
        let dest = temp_dest("missing.exe");

        match download_to_file(&url, &dest, "direct", 1024, 5000, &mut |_, _| true) {
            Err(FetchError::Http(404)) => {}
            other => panic!("expected http 404, got {:?}", other),
        }
        let _ = std::fs::remove_dir_all(dest.parent().unwrap());
    }

    #[test]
    fn redirect_locations_resolve_like_new_url() {
        let base = "https://github.com/owner/repo/releases/download/v1/setup.exe";
        assert_eq!(
            resolve_redirect(base, "https://objects.githubusercontent.com/x").as_deref(),
            Some("https://objects.githubusercontent.com/x")
        );
        assert_eq!(
            resolve_redirect(base, "//cdn.example.com/x").as_deref(),
            Some("https://cdn.example.com/x")
        );
        assert_eq!(
            resolve_redirect(base, "/absolute").as_deref(),
            Some("https://github.com/absolute")
        );
        assert_eq!(
            resolve_redirect(base, "sibling.exe").as_deref(),
            Some("https://github.com/owner/repo/releases/download/v1/sibling.exe")
        );
        assert_eq!(
            resolve_redirect("https://github.com/a?x=1", "?y=2").as_deref(),
            Some("https://github.com/a?y=2")
        );
        assert_eq!(resolve_redirect(base, ""), None);
    }
}
